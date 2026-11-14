//! Bounded regular-expression execution with SQL line-boundary assertions.

use regex_automata::PatternID;
use regex_automata::nfa::thompson::{NFA, State};
use regex_automata::util::{
    look::{Look, LookMatcher},
    primitives::StateID,
    syntax,
};

use super::{ExecError, MAX_COMPILED_REGEX_BYTES};
use crate::execution::{InterruptCounter, RegexError};

pub(super) const WORKSPACE_LIMIT: usize = 4 << 20;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum LineEndings {
    Unicode,
    Unix,
}

pub(super) fn needs_boundaries(pattern: &str, multiline: bool) -> bool {
    pattern.contains('$')
        || pattern.contains("\\Z")
        || (pattern.contains('^') && (multiline || pattern.contains("(?")))
}

fn single_line(text: &str) -> bool {
    !text.contains(['\n', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
}

pub(super) enum Program {
    Fast(regex::Regex),
    Boundary(BoundaryProgram),
    /// A pattern no automaton expresses (see [`super::sql_backtrack`]).
    Backtrack(super::sql_backtrack::Program),
}

pub(super) struct Match<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

impl<'a> Match<'a> {
    pub(super) fn as_str(&self) -> &'a str {
        &self.text[self.start..self.end]
    }
    pub(super) fn start(&self) -> usize {
        self.start
    }
    pub(super) fn end(&self) -> usize {
        self.end
    }
}

impl Program {
    /// Compiles `pattern` for the automaton, or for the backtracking
    /// machine when it uses what no automaton expresses.
    pub(super) fn new(
        pattern: &str,
        case: bool,
        multiline: bool,
        dotall: bool,
        line_endings: LineEndings,
    ) -> Result<Self, ExecError> {
        let backtrack = || {
            super::sql_backtrack::Program::new(pattern, case, multiline, dotall, line_endings)
                .map(Self::Backtrack)
        };
        // A count no pattern may name is refused whichever matcher would
        // have run it.
        let largest = largest_count(pattern);
        if largest > super::sql_backtrack::MAX_COUNT {
            return Err(ExecError::Regex(crate::execution::RegexError::NumberTooBig));
        }
        if needs_backtracking(pattern) {
            return backtrack();
        }
        // The automaton copies an operand once per count and refuses a
        // pattern that grows past its size; the other machine counts, so a
        // pattern refused for its counts runs there.
        // Without regard to case a run of literals matches across a case
        // folding of several characters. The other machine reads its
        // patterns that way; for the automaton the pattern is rewritten.
        let folded = if case {
            with_full_case_folds(pattern)
        } else {
            std::borrow::Cow::Borrowed(pattern)
        };
        match Self::automaton(&folded, case, multiline, dotall, line_endings) {
            Err(_) if largest > AUTOMATON_COUNT => backtrack(),
            compiled => compiled,
        }
    }

    fn automaton(
        pattern: &str,
        case: bool,
        multiline: bool,
        dotall: bool,
        line_endings: LineEndings,
    ) -> Result<Self, ExecError> {
        if !needs_boundaries(pattern, multiline) {
            return regex::RegexBuilder::new(pattern)
                .case_insensitive(case)
                .multi_line(multiline)
                .dot_matches_new_line(dotall)
                .size_limit(MAX_COMPILED_REGEX_BYTES)
                .build()
                .map(Self::Fast)
                .map_err(|_| ExecError::InvalidExpressionType);
        }
        let nfa = NFA::compiler()
            .configure(NFA::config().nfa_size_limit(Some(MAX_COMPILED_REGEX_BYTES)))
            .syntax(
                syntax::Config::new()
                    .case_insensitive(case)
                    .multi_line(multiline)
                    .dot_matches_new_line(dotall),
            )
            .build(&strict_end_markers(pattern)?)
            .map_err(|_| ExecError::InvalidExpressionType)?;
        let slots = nfa.group_info().slot_len();
        let edges = nfa
            .states()
            .iter()
            .map(|state| match state {
                State::Union { alternates } => alternates.len(),
                State::BinaryUnion { .. } => 2,
                _ => 1,
            })
            .sum::<usize>();
        // Two active sets and an epsilon stack can each retain captures.
        // Include vector growth and both visited-state maps in the ceiling.
        let thread_bytes =
            size_of::<Thread>().saturating_add(slots.saturating_mul(size_of::<usize>()));
        let workspace = (2 * nfa.states().len() + edges + 2)
            .saturating_mul(thread_bytes)
            .saturating_mul(2)
            .saturating_add(2 * nfa.states().len());
        if workspace > WORKSPACE_LIMIT {
            return Err(ExecError::InvalidExpressionType);
        }
        let fast = regex::RegexBuilder::new(&normalize_end_spelling(pattern))
            .case_insensitive(case)
            .multi_line(multiline)
            .dot_matches_new_line(dotall)
            .size_limit(MAX_COMPILED_REGEX_BYTES)
            .build()
            .map_err(|_| ExecError::InvalidExpressionType)?;
        Ok(Self::Boundary(BoundaryProgram {
            nfa,
            fast,
            slots,
            line_endings,
            workspace,
        }))
    }

    pub(super) fn workspace_upper_bound(&self) -> usize {
        match self {
            // The backtracking machine is charged to the statement before
            // it runs (see `bound_regex_memory_upper_bound`), not again for
            // every row.
            Self::Fast(_) | Self::Backtrack(_) => 0,
            Self::Boundary(program) => program.workspace,
        }
    }

    /// Whether `text` holds a match. The work is counted on `steps`, which
    /// ends the search with the statement's interruption.
    pub(super) fn is_match(
        &self,
        text: &str,
        steps: &mut InterruptCounter,
    ) -> Result<bool, ExecError> {
        match self {
            Self::Fast(program) => {
                steps.steps(text.len())?;
                Ok(program.is_match(text))
            }
            Self::Boundary(program) if single_line(text) => {
                steps.steps(text.len())?;
                Ok(program.fast.is_match(text))
            }
            Self::Boundary(program) => Ok(program.captures_at(text, 0, steps)?.is_some()),
            Self::Backtrack(program) => Ok(program.captures_at(text, 0, steps)?.is_some()),
        }
    }

    /// Capture slots of the first match starting at or after byte `from`,
    /// with the text before `from` still visible to anchors and boundaries.
    fn captures_from(
        &self,
        text: &str,
        from: usize,
        steps: &mut InterruptCounter,
    ) -> Result<Option<Box<[usize]>>, ExecError> {
        let fast = match self {
            Self::Fast(program) => program,
            Self::Boundary(program) if single_line(text) => &program.fast,
            Self::Boundary(program) => return program.captures_at(text, from, steps),
            Self::Backtrack(program) => return program.captures_at(text, from, steps),
        };
        let Some(captures) = fast.captures_at(text, from) else {
            steps.steps(text.len() - from)?;
            return Ok(None);
        };
        // A search reads up to its match; one that finds a match a byte on,
        // millions of times, is as much work as one long read.
        steps.steps(captures.get(0).map_or(0, |found| found.end() - from) + 1)?;
        Ok(Some(
            (0..captures.len())
                .flat_map(|group| {
                    captures
                        .get(group)
                        .map_or([usize::MAX, usize::MAX], |found| {
                            [found.start(), found.end()]
                        })
                })
                .collect(),
        ))
    }

    /// The match after the one that started the search at `next`, in the
    /// order a search reports them: after a non-empty match the next search
    /// starts at its end, where an empty match may follow; after an empty
    /// match it starts one character on. `next` is `None` once the text is
    /// exhausted.
    fn next_match(
        &self,
        text: &str,
        next: &mut Option<usize>,
        steps: &mut InterruptCounter,
    ) -> Result<Option<Box<[usize]>>, ExecError> {
        let Some(from) = *next else {
            return Ok(None);
        };
        let Some(slots) = self.captures_from(text, from, steps)? else {
            *next = None;
            return Ok(None);
        };
        let end = slots[1];
        *next = if slots[0] == end {
            text[end..]
                .chars()
                .next()
                .map(|character| end + character.len_utf8())
        } else {
            Some(end)
        };
        Ok(Some(slots))
    }

    /// The `occurrence`-th match (1-based) from byte `from`.
    pub(super) fn nth_match<'a>(
        &self,
        text: &'a str,
        from: usize,
        occurrence: usize,
        steps: &mut InterruptCounter,
    ) -> Result<Option<Match<'a>>, ExecError> {
        let Some(mut remaining) = occurrence.checked_sub(1) else {
            return Ok(None);
        };
        let mut next = Some(from);
        while let Some(slots) = self.next_match(text, &mut next, steps)? {
            if remaining == 0 {
                return Ok(Some(Match {
                    text,
                    start: slots[0],
                    end: slots[1],
                }));
            }
            remaining -= 1;
        }
        Ok(None)
    }

    /// Replaces the `occurrence`-th match from byte `from`, or every match
    /// from there when `occurrence` is zero; text before `from` is kept.
    pub(super) fn replace_from(
        &self,
        text: &str,
        from: usize,
        occurrence: usize,
        replacement: &str,
        steps: &mut InterruptCounter,
    ) -> Result<String, ExecError> {
        let mut result = String::with_capacity(text.len());
        let mut copied = 0;
        let mut next = Some(from);
        let mut index = 0;
        while let Some(slots) = self.next_match(text, &mut next, steps)? {
            index += 1;
            if occurrence != 0 && index != occurrence {
                continue;
            }
            result.push_str(&text[copied..slots[0]]);
            self.expand_replacement(replacement, text, &slots, &mut result)?;
            copied = slots[1];
            if occurrence != 0 {
                break;
            }
        }
        result.push_str(&text[copied..]);
        Ok(result)
    }

    /// Writes one match's replacement. `$n` is the text of group `n`, read
    /// with as many digits as still name a group; `${name}` is a named
    /// group; a backslash makes the next character literal, with `\uXXXX`
    /// and `\UXXXXXXXX` spelling a code point. A `$` that starts neither
    /// form and a group the pattern does not have are errors, as they are
    /// in `MySQL`.
    fn expand_replacement(
        &self,
        replacement: &str,
        text: &str,
        slots: &[usize],
        out: &mut String,
    ) -> Result<(), ExecError> {
        let groups = slots.len() / 2;
        let push_group = |group: usize, out: &mut String| {
            if let (Some(&start), Some(&end)) = (slots.get(group * 2), slots.get(group * 2 + 1))
                && start != usize::MAX
                && end != usize::MAX
            {
                out.push_str(&text[start..end]);
            }
        };
        let mut chars = replacement.chars().peekable();
        while let Some(character) = chars.next() {
            match character {
                '\\' => {
                    let Some(escaped) = chars.next() else {
                        break;
                    };
                    let width = match escaped {
                        'u' => 4,
                        'U' => 8,
                        _ => {
                            out.push(escaped);
                            continue;
                        }
                    };
                    let mut code = 0_u32;
                    for _ in 0..width {
                        let digit = chars
                            .next()
                            .and_then(|digit| digit.to_digit(16))
                            .ok_or(ExecError::InvalidExpressionType)?;
                        code = code * 16 + digit;
                    }
                    out.push(char::from_u32(code).ok_or(ExecError::InvalidExpressionType)?);
                }
                '$' => match chars.peek().copied() {
                    Some(first) if first.is_ascii_digit() => {
                        let mut group = 0_usize;
                        let mut read = 0;
                        while let Some(digit) = chars.peek().and_then(|digit| digit.to_digit(10)) {
                            let wider = group * 10 + digit as usize;
                            if read > 0 && wider >= groups {
                                break;
                            }
                            group = wider;
                            read += 1;
                            chars.next();
                        }
                        if group >= groups {
                            return Err(ExecError::Regex(RegexError::IndexOutOfBounds));
                        }
                        push_group(group, out);
                    }
                    Some('{') => {
                        chars.next();
                        let mut name = String::new();
                        loop {
                            match chars.next() {
                                Some('}') => break,
                                Some(letter) => name.push(letter),
                                None => {
                                    return Err(ExecError::Regex(
                                        RegexError::InvalidCaptureGroupName,
                                    ));
                                }
                            }
                        }
                        let named = !name.is_empty()
                            && !name.starts_with(|first: char| first.is_ascii_digit())
                            && name.chars().all(|letter| letter.is_ascii_alphanumeric());
                        let group = named
                            .then(|| self.group_index(&name))
                            .flatten()
                            .ok_or(ExecError::Regex(RegexError::InvalidCaptureGroupName))?;
                        push_group(group, out);
                    }
                    _ => return Err(ExecError::Regex(RegexError::InvalidCaptureGroupName)),
                },
                other => out.push(other),
            }
        }
        Ok(())
    }

    fn group_index(&self, name: &str) -> Option<usize> {
        match self {
            Self::Fast(program) => program
                .capture_names()
                .position(|group| group == Some(name)),
            Self::Boundary(program) => program.nfa.group_info().to_index(PatternID::ZERO, name),
            Self::Backtrack(program) => program.group_index(name),
        }
    }
}

/// A count above which a pattern the automaton refuses is tried on the
/// machine that counts.
pub(super) const AUTOMATON_COUNT: u64 = 255;

/// The largest number a `{n}`, `{n,}` or `{n,m}` of `pattern` names, zero
/// when it has none. Braces in a bracketed set, after a backslash or inside
/// `\Q...\E` are not counts.
pub(super) fn largest_count(pattern: &str) -> u64 {
    let bytes = pattern.as_bytes();
    let mut largest = 0_u64;
    let mut depth = 0_usize;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' if bytes.get(at + 1) == Some(&b'Q') => {
                at = pattern[at..]
                    .find("\\E")
                    .map_or(bytes.len(), |end| at + end + 2);
                continue;
            }
            b'\\' => at += 1,
            b'[' => depth += 1,
            b']' => depth = depth.saturating_sub(1),
            b'{' if depth == 0 => {
                let mut numbers = Vec::with_capacity(2);
                let mut number = None::<u64>;
                let mut next = at + 1;
                while let Some(&byte) = bytes.get(next) {
                    if byte.is_ascii_digit() {
                        number = Some(
                            number
                                .unwrap_or(0)
                                .saturating_mul(10)
                                .saturating_add(u64::from(byte - b'0')),
                        );
                    } else if byte == b',' && numbers.is_empty() && number.is_some() {
                        numbers.extend(number.take());
                    } else {
                        break;
                    }
                    next += 1;
                }
                numbers.extend(number);
                if bytes.get(next) == Some(&b'}') {
                    largest = numbers.into_iter().fold(largest, u64::max);
                }
            }
            _ => {}
        }
        at += 1;
    }
    largest
}

/// Whether `pattern` runs on the backtracking machine. Only a pattern the
/// automaton's syntax cannot write, or reads as something else, goes
/// there: one it can write and refuses for its size stays refused.
///
/// A pattern that switches case sensitivity for part of itself and holds a
/// literal that folds across several characters is one of them: which
/// literals fold depends on where the switch reaches, which the rewrite
/// for the automaton cannot say and the other machine's reading can.
pub(super) fn needs_backtracking(pattern: &str) -> bool {
    if possessive(pattern) {
        return true;
    }
    let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(pattern) else {
        return true;
    };
    sets_case(&ast) && !fold_edits(&ast).is_empty()
}

/// Whether the pattern turns case sensitivity on or off for itself or for
/// a group: `(?i)`, `(?-i)`, `(?i:...)`.
fn sets_case(ast: &regex_syntax::ast::Ast) -> bool {
    use regex_syntax::ast::{Ast, Flag, Flags, FlagsItemKind, GroupKind};
    let names_case = |flags: &Flags| {
        flags
            .items
            .iter()
            .any(|item| matches!(item.kind, FlagsItemKind::Flag(Flag::CaseInsensitive)))
    };
    let mut pending = vec![ast];
    while let Some(node) = pending.pop() {
        match node {
            Ast::Flags(set) if names_case(&set.flags) => return true,
            Ast::Group(group) => {
                if let GroupKind::NonCapturing(flags) = &group.kind
                    && names_case(flags)
                {
                    return true;
                }
                pending.push(&group.ast);
            }
            Ast::Repetition(repetition) => pending.push(&repetition.ast),
            Ast::Alternation(alternation) => pending.extend(alternation.asts.iter()),
            Ast::Concat(concat) => pending.extend(concat.asts.iter()),
            _ => {}
        }
    }
    false
}

/// Whether `pattern` holds a possessive quantifier: a `+` written straight
/// after another quantifier. The automaton's syntax reads that as a
/// repetition of a repetition, which matches more than the possessive does.
fn possessive(pattern: &str) -> bool {
    use regex_syntax::ast::{Ast, RepetitionKind};
    let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(pattern) else {
        return false;
    };
    let mut pending = vec![&ast];
    while let Some(node) = pending.pop() {
        match node {
            Ast::Repetition(repetition) => {
                if matches!(repetition.op.kind, RepetitionKind::OneOrMore)
                    && repetition.greedy
                    && matches!(*repetition.ast, Ast::Repetition(_))
                {
                    return true;
                }
                pending.push(&repetition.ast);
            }
            Ast::Group(group) => pending.push(&group.ast),
            Ast::Alternation(alternation) => pending.extend(alternation.asts.iter()),
            Ast::Concat(concat) => pending.extend(concat.asts.iter()),
            _ => {}
        }
    }
    false
}

/// Normalize spelling without changing byte spans or escaped backslashes.
fn normalize_end_spelling(pattern: &str) -> String {
    let mut normalized = String::with_capacity(pattern.len());
    let mut chars = pattern.chars();
    while let Some(character) = chars.next() {
        normalized.push(character);
        if character == '\\'
            && let Some(next) = chars.next()
        {
            normalized.push(if next == 'Z' { 'z' } else { next });
        }
    }
    normalized
}

/// The characters whose case folding is more than one character, with that
/// folding: `ß` folds to `ss`, the ligature `ﬁ` to `fi`. Read off the
/// case mappings rather than tabulated: a character folds this way when it
/// lowers to several characters, or when its capital form is several.
pub(super) fn full_folds() -> &'static [(char, String)] {
    static FOLDS: std::sync::OnceLock<Vec<(char, String)>> = std::sync::OnceLock::new();
    FOLDS.get_or_init(|| {
        let lowered = |text: &str| {
            text.chars()
                .flat_map(char::to_lowercase)
                .collect::<String>()
        };
        ('\u{80}'..='\u{ffff}')
            .filter_map(|character| {
                let lower = character.to_lowercase().collect::<String>();
                if lower.chars().count() > 1 {
                    return Some((character, lower));
                }
                let upper = lower
                    .chars()
                    .flat_map(char::to_uppercase)
                    .collect::<String>();
                (upper.chars().count() > 1).then(|| (character, lowered(&upper)))
            })
            .collect()
    })
}

/// What one run of literal characters matches, from `at` on: each
/// character as written, or a character whose folding the run spells.
pub(super) fn spelled(
    run: &[char],
    at: usize,
    folds: &[(char, String)],
    budget: &mut usize,
) -> String {
    let Some(&character) = run.get(at) else {
        return String::new();
    };
    let mut written = String::new();
    written.push(character);
    let mut options = Vec::new();
    if let Ok(index) = folds.binary_search_by_key(&character, |(folded, _)| *folded) {
        options.push(regex_syntax::escape(&folds[index].1));
    }
    let mut branches = vec![(regex_syntax::escape(&written), at + 1)];
    for (folded, spelling) in folds {
        let length = spelling.chars().count();
        let spells = run.get(at..at + length).is_some_and(|slice| {
            slice
                .iter()
                .flat_map(|character| character.to_lowercase())
                .eq(spelling.chars())
        });
        if spells {
            let mut folded_text = String::new();
            folded_text.push(*folded);
            branches.push((regex_syntax::escape(&folded_text), at + length));
        }
    }
    if branches.len() == 1 && options.is_empty() {
        return format!("{}{}", branches[0].0, spelled(run, at + 1, folds, budget));
    }
    *budget = budget.saturating_sub(branches.len() + options.len());
    if *budget == 0 {
        return run[at..].iter().fold(String::new(), |mut text, character| {
            let mut one = String::new();
            one.push(*character);
            text.push_str(&regex_syntax::escape(&one));
            text
        });
    }
    let mut alternatives = branches
        .into_iter()
        .map(|(head, next)| format!("{head}{}", spelled(run, next, folds, budget)))
        .collect::<Vec<_>>();
    let rest = spelled(run, at + 1, folds, budget);
    alternatives.extend(options.into_iter().map(|option| format!("{option}{rest}")));
    format!("(?:{})", alternatives.join("|"))
}

/// A case-insensitive pattern in which a literal matches across a folding
/// of more than one character, as the reference engine's literals do:
/// `ss` written out also matches `ß`, and `ß` written out also matches
/// `ss`. Only what is written as a run of literal characters does, in a
/// group or out of one: `s{2}`, `[s]s`, `s.` and `s(?:s)` still do not
/// match `ß`, and `[ß]` does not match `ss`.
///
/// The pattern comes back unchanged when it has no such literal, when it
/// does not parse here, or when it switches case sensitivity for part of
/// itself - that pattern runs on the other machine (see
/// [`needs_backtracking`]).
pub(super) fn with_full_case_folds(pattern: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let Ok(ast) = regex_syntax::ast::parse::Parser::new().parse(pattern) else {
        return Cow::Borrowed(pattern);
    };
    if sets_case(&ast) {
        return Cow::Borrowed(pattern);
    }
    let mut edits = fold_edits(&ast);
    if edits.is_empty() {
        return Cow::Borrowed(pattern);
    }
    edits.sort_unstable_by_key(|(start, _, _)| *start);
    let mut result = String::with_capacity(pattern.len() * 2);
    let mut copied = 0;
    for (start, end, replacement) in edits {
        result.push_str(&pattern[copied..start]);
        result.push_str(&replacement);
        copied = end;
    }
    result.push_str(&pattern[copied..]);
    Cow::Owned(result)
}

/// The runs of literal characters of a pattern that match across a case
/// folding of several characters, each with the span it is written over
/// and what it is spelled out as.
fn fold_edits(ast: &regex_syntax::ast::Ast) -> Vec<(usize, usize, String)> {
    use regex_syntax::ast::Ast;
    let folds = full_folds();
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut budget = 256_usize;
    let mut pending = vec![ast];
    let mut run_of = |literals: &[&regex_syntax::ast::Literal], edits: &mut Vec<_>| {
        let run = literals.iter().map(|literal| literal.c).collect::<Vec<_>>();
        let replaced = spelled(&run, 0, folds, &mut budget);
        if replaced.contains("(?:") {
            edits.push((
                literals[0].span.start.offset,
                literals[literals.len() - 1].span.end.offset,
                replaced,
            ));
        }
    };
    while let Some(node) = pending.pop() {
        match node {
            Ast::Literal(literal) => run_of(&[literal.as_ref()], &mut edits),
            Ast::Concat(concat) => {
                let mut run: Vec<&regex_syntax::ast::Literal> = Vec::new();
                for part in &concat.asts {
                    if let Ast::Literal(literal) = part {
                        run.push(literal.as_ref());
                    } else {
                        if !run.is_empty() {
                            run_of(&run, &mut edits);
                            run.clear();
                        }
                        pending.push(part);
                    }
                }
                if !run.is_empty() {
                    run_of(&run, &mut edits);
                }
            }
            Ast::Repetition(repetition) => pending.push(&repetition.ast),
            Ast::Group(group) => pending.push(&group.ast),
            Ast::Alternation(alternation) => pending.extend(alternation.asts.iter()),
            _ => {}
        }
    }
    edits
}

/// Keep strict end-of-input separate from the dollar assertion in the NFA.
/// Capturing-group indexes are unchanged because the marker is non-capturing.
fn strict_end_markers(pattern: &str) -> Result<String, ExecError> {
    use regex_syntax::ast::{AssertionKind, Ast};
    let normalized = normalize_end_spelling(pattern);
    let ast = regex_syntax::ast::parse::Parser::new()
        .parse(&normalized)
        .map_err(|_| ExecError::InvalidExpressionType)?;
    let mut pending = vec![&ast];
    let mut spans = Vec::new();
    while let Some(node) = pending.pop() {
        match node {
            Ast::Assertion(assertion) if assertion.kind == AssertionKind::EndText => {
                spans.push((assertion.span.start.offset, assertion.span.end.offset));
            }
            Ast::Repetition(repetition) => pending.push(&repetition.ast),
            Ast::Group(group) => pending.push(&group.ast),
            Ast::Alternation(alternation) => pending.extend(alternation.asts.iter()),
            Ast::Concat(concat) => pending.extend(concat.asts.iter()),
            _ => {}
        }
    }
    spans.sort_unstable();
    let mut result = String::with_capacity(pattern.len());
    let mut copied = 0;
    for (start, end) in spans {
        result.push_str(&pattern[copied..start]);
        result.push_str(if pattern.as_bytes()[end - 1] == b'Z' {
            "(?-m:$)"
        } else {
            "(?mR:$)"
        });
        copied = end;
    }
    result.push_str(&pattern[copied..]);
    Ok(result)
}

pub(super) struct BoundaryProgram {
    nfa: NFA,
    fast: regex::Regex,
    slots: usize,
    line_endings: LineEndings,
    workspace: usize,
}

#[derive(Clone)]
struct Thread {
    state: StateID,
    slots: Box<[usize]>,
}

impl BoundaryProgram {
    /// The first match at or after `from`. Each position costs a step per
    /// live thread, counted on `steps`.
    fn captures_at(
        &self,
        text: &str,
        from: usize,
        steps: &mut InterruptCounter,
    ) -> Result<Option<Box<[usize]>>, ExecError> {
        let mut current = Vec::new();
        let mut next = Vec::new();
        let mut seen = vec![false; self.nfa.states().len()];
        let mut next_seen = seen.clone();
        let mut stack = Vec::new();
        let mut found = None;
        for at in from..=text.len() {
            steps.steps(current.len() + 1)?;
            if found.is_none() && text.is_char_boundary(at) {
                let thread = Thread {
                    state: self.nfa.start_anchored(),
                    slots: vec![usize::MAX; self.slots].into_boxed_slice(),
                };
                self.expand(text, at, thread, &mut current, &mut seen, &mut stack);
            }
            next_seen.fill(false);
            for mut thread in current.drain(..) {
                let state = self.nfa.state(thread.state);
                if matches!(state, State::Match { .. }) {
                    found = Some(thread.slots);
                    break;
                }
                let Some(&byte) = text.as_bytes().get(at) else {
                    continue;
                };
                let destination = match state {
                    State::ByteRange { trans } => trans.matches_byte(byte).then_some(trans.next),
                    State::Sparse(transitions) => transitions.matches_byte(byte),
                    State::Dense(transitions) => transitions.matches_byte(byte),
                    _ => None,
                };
                if let Some(destination) = destination {
                    thread.state = destination;
                    self.expand(text, at + 1, thread, &mut next, &mut next_seen, &mut stack);
                }
            }
            if found.is_some() && next.is_empty() {
                return Ok(found);
            }
            std::mem::swap(&mut current, &mut next);
            std::mem::swap(&mut seen, &mut next_seen);
        }
        Ok(found)
    }

    fn expand(
        &self,
        text: &str,
        at: usize,
        thread: Thread,
        output: &mut Vec<Thread>,
        seen: &mut [bool],
        stack: &mut Vec<Thread>,
    ) {
        stack.push(thread);
        while let Some(mut thread) = stack.pop() {
            if std::mem::replace(&mut seen[thread.state.as_usize()], true) {
                continue;
            }
            match self.nfa.state(thread.state) {
                State::Look { look, next } => {
                    if self.assertion(*look, text, at) {
                        thread.state = *next;
                        stack.push(thread);
                    }
                }
                State::Capture { next, slot, .. } => {
                    thread.slots[slot.as_usize()] = at;
                    thread.state = *next;
                    stack.push(thread);
                }
                State::Union { alternates } => {
                    for state in alternates.iter().rev() {
                        let mut branch = thread.clone();
                        branch.state = *state;
                        stack.push(branch);
                    }
                }
                State::BinaryUnion { alt1, alt2 } => {
                    let mut branch = thread.clone();
                    branch.state = *alt2;
                    stack.push(branch);
                    thread.state = *alt1;
                    stack.push(thread);
                }
                State::Fail => {}
                _ => output.push(thread),
            }
        }
    }

    fn assertion(&self, look: Look, text: &str, at: usize) -> bool {
        match look {
            Look::End => {
                at == text.len()
                    || self
                        .line_end(text, at)
                        .is_some_and(|width| at + width == text.len())
            }
            Look::EndCRLF => at == text.len(),
            Look::EndLF => at == text.len() || self.line_end(text, at).is_some(),
            Look::StartLF | Look::StartCRLF => {
                if at == 0 {
                    return true;
                }
                if at == text.len() {
                    return false;
                }
                let Some(prefix) = text.get(..at) else {
                    return false;
                };
                prefix.ends_with('\n')
                    || (self.line_endings != LineEndings::Unix
                        && ((prefix.ends_with('\r') && !text[at..].starts_with('\n'))
                            || prefix.ends_with(['\u{85}', '\u{2028}', '\u{2029}'])))
            }
            _ => LookMatcher::new().matches(look, text.as_bytes(), at),
        }
    }

    fn line_end(&self, text: &str, at: usize) -> Option<usize> {
        let suffix = text.get(at..)?;
        if self.line_endings == LineEndings::Unix {
            return suffix.starts_with('\n').then_some(1);
        }
        if suffix.starts_with("\r\n") {
            return Some(2);
        }
        if suffix.starts_with('\n') && at > 0 && text.as_bytes()[at - 1] == b'\r' {
            return None;
        }
        let first = suffix.chars().next()?;
        matches!(first, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
            .then_some(first.len_utf8())
    }

    /// Every match replaced, skipping an empty match that ends where the
    /// previous match did - the reference the tests compare spans against.
    #[cfg(test)]
    fn replace_all(&self, text: &str, replacement: &str) -> String {
        let mut result = String::new();
        let mut from = 0;
        let mut copied = 0;
        let mut previous_end = None;
        let mut steps = InterruptCounter::unchecked();
        while let Some(slots) = self
            .captures_at(text, from, &mut steps)
            .expect("an unchecked search is never interrupted")
        {
            let (start, end) = (slots[0], slots[1]);
            if start != end || previous_end != Some(end) {
                result.push_str(&text[copied..start]);
                regex_automata::util::interpolate::string(
                    replacement,
                    |index, out| {
                        let Some(slot) = index.checked_mul(2) else {
                            return;
                        };
                        if let (Some(&start), Some(&end)) = (slots.get(slot), slots.get(slot + 1))
                            && start != usize::MAX
                            && end != usize::MAX
                        {
                            out.push_str(&text[start..end]);
                        }
                    },
                    |name| self.nfa.group_info().to_index(PatternID::ZERO, name),
                    &mut result,
                );
                copied = end;
                previous_end = Some(end);
            }
            if start == end {
                let Some(character) = text[end..].chars().next() else {
                    break;
                };
                from = end + character.len_utf8();
            } else {
                from = end;
            }
        }
        result.push_str(&text[copied..]);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecError, InterruptCounter, LineEndings, Program};

    /// The two matchers answer alike. Every pattern the automaton can
    /// express is run through both, forced, over every subject: whether it
    /// matches, and where. Which matcher a pattern's syntax routes it to
    /// must not show in the answer.
    #[test]
    fn both_matchers_give_the_same_answers() {
        const PATTERNS: &[&str] = &[
            "sse",
            "(?:ss)e",
            "(ss)e",
            "stra(?:ss|x)e",
            "(?:ß)e",
            "(ß)e",
            "s(?:s)e",
            "(?:s)(?:s)e",
            "ss?e",
            "s{2}e",
            "(?:ss)+e",
            "a(?:ss|tt)e",
            "a(?:ß|tt)e",
            "(?:fi)n",
            "(?:ﬁ)n",
            "(?m)sse",
            "(?s:ss)e",
            "(?x) s s e",
            "ss|x",
            "^ss$",
            "a[ß]",
            "sss?",
            "[s]s",
            "s.",
            "(s)(s)",
            "s+",
            "ß+",
            "ß{2}",
            "i",
            "ffi",
            "straße",
            r"s\x73e",
            "(?:a|b)+c",
            "a{2,3}b",
            "(a|ab)(c|bcd)",
            "x*",
            "(x)*y",
            "^(?:ab)*$",
            "a.c",
            r"\bss\b",
            "[^s]+",
            "(?:ß|s)s",
            "s?ß",
        ];
        const SUBJECTS: &[&str] = &[
            "straße", "STRASSE", "strasse", "ß", "ss", "ﬁn", "FIN", "fin", "aßb", "assb", "ẞ",
            "sss", "ﬃ", "İ", "xße", "", "abab", "aab", "abcd", "xxy", "a\nc", "ß ss ß", "sßs",
        ];
        let mut compared = 0;
        for insensitive in [true, false] {
            for pattern in PATTERNS {
                let folded = if insensitive {
                    super::with_full_case_folds(pattern)
                } else {
                    std::borrow::Cow::Borrowed(*pattern)
                };
                let Ok(automaton) =
                    Program::automaton(&folded, insensitive, false, false, LineEndings::Unicode)
                else {
                    continue;
                };
                let machine = Program::Backtrack(
                    crate::expression::sql_backtrack::Program::new(
                        pattern,
                        insensitive,
                        false,
                        false,
                        LineEndings::Unicode,
                    )
                    .unwrap_or_else(|error| panic!("{pattern}: {error}")),
                );
                for subject in SUBJECTS {
                    let span = |program: &Program| {
                        program
                            .nth_match(subject, 0, 1, &mut InterruptCounter::unchecked())
                            .unwrap_or_else(|error| panic!("{pattern} over {subject}: {error}"))
                            .map(|found| (found.start(), found.end()))
                    };
                    assert_eq!(
                        span(&machine),
                        span(&automaton),
                        "{pattern} over {subject:?}, insensitive: {insensitive}"
                    );
                    compared += 1;
                }
            }
        }
        assert!(compared > 1_500, "{compared} comparisons");
    }

    /// A pattern that switches case sensitivity for part of itself and
    /// holds a literal that folds across several characters runs on the
    /// machine that reads the switch. Answers measured against MySQL 8.4.
    #[test]
    fn a_pattern_that_switches_case_folds_where_the_switch_reaches() {
        for (pattern, insensitive, text, expected) in [
            ("(?i)sse", true, "straße", true),
            ("(?i:ss)e", true, "straße", true),
            ("(?-i)sse", true, "straße", false),
            ("(?i)sse", false, "straße", true),
            ("(?:ss)e", true, "straße", true),
            ("(?:ss)e", false, "straße", false),
        ] {
            let program = Program::new(pattern, insensitive, false, false, LineEndings::Unicode)
                .unwrap_or_else(|error| panic!("{pattern}: {error}"));
            assert_eq!(
                matches!(program, Program::Backtrack(_)),
                pattern.contains('i'),
                "{pattern}"
            );
            assert_eq!(is_match(&program, text), expected, "{pattern} over {text}");
        }
    }

    #[test]
    fn a_literal_matches_across_a_folding_of_several_characters() {
        let matches = |pattern: &str, text: &str| {
            let folded = super::with_full_case_folds(pattern);
            let program = Program::new(&folded, true, false, false, LineEndings::Unicode).unwrap();
            is_match(&program, text)
        };
        for (pattern, text, expected) in [
            ("ss", "stra\u{df}e", true),
            ("SS", "stra\u{df}e", true),
            ("\u{df}", "STRASSE", true),
            ("a\u{df}b", "assb", true),
            ("assb", "a\u{df}b", true),
            ("^ss$", "\u{df}", true),
            ("ss|x", "\u{df}", true),
            ("a[\u{df}]", "ass", false),
            ("sss?", "\u{df}", true),
            ("ss?", "\u{df}", false),
            ("fi", "\u{fb01}", true),
            ("ffi", "\u{fb03}", true),
            ("ss", "\u{1e9e}", true),
            ("^s", "\u{df}", false),
            ("s{2}", "\u{df}", false),
            ("[s]s", "\u{df}", false),
            ("s.", "\u{df}", false),
            ("(s)(s)", "\u{df}", false),
            ("s+", "\u{df}", false),
            ("as", "a\u{df}b", false),
            ("i", "\u{130}", false),
        ] {
            assert_eq!(matches(pattern, text), expected, "{pattern} against {text}");
        }
        assert!(matches!(
            super::with_full_case_folds("abc[d-f]+"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn boundary_matching_preserves_greediness_and_capture_expansion() {
        let mut subjects = vec![String::new(), "éaé".to_owned(), "éé".to_owned()];
        for width in 1..=6 {
            for bits in 0..(1 << width) {
                subjects.push(
                    (0..width)
                        .map(|bit| if bits & (1 << bit) == 0 { 'a' } else { 'b' })
                        .collect(),
                );
            }
        }
        for pattern in [
            "(a|ab)$",
            "(ab|a)$",
            "(a*)$",
            "(a+?)$",
            "((a)|(b))*$",
            "(?P<lead>a+)(b*)$",
            "(a?)*$",
            "(?:a|aa)*b$",
            "^$",
            "(a|)b?$",
            "(é*)$",
        ] {
            let program = Program::new(pattern, false, false, false, LineEndings::Unicode).unwrap();
            let Program::Boundary(boundary) = &program else {
                panic!("boundary pattern");
            };
            let reference = regex::Regex::new(pattern).unwrap();
            for text in &subjects {
                assert_eq!(
                    boundary
                        .captures_at(text, 0, &mut InterruptCounter::unchecked())
                        .unwrap()
                        .map(|slots| (slots[0], text[slots[0]..slots[1]].to_owned())),
                    reference
                        .find(text)
                        .map(|found| (found.start(), found.as_str().to_owned())),
                    "{pattern}: {text:?}"
                );
                for replacement in [
                    "<$0>",
                    "$1:$2",
                    "${lead}",
                    "$$",
                    "$999999999999999999999",
                    "$18446744073709551615",
                ] {
                    assert_eq!(
                        boundary.replace_all(text, replacement),
                        reference.replace_all(text, replacement),
                        "{pattern}: {text:?}, {replacement}"
                    );
                }
            }
        }
    }

    fn is_match(program: &Program, text: &str) -> bool {
        program
            .is_match(text, &mut InterruptCounter::unchecked())
            .expect("an unchecked search is never interrupted")
    }

    #[test]
    fn multiline_start_accepts_empty_input_but_not_a_trailing_empty_line() {
        let program = Program::new("(?m)^", false, false, false, LineEndings::Unicode).unwrap();
        assert!(is_match(&program, ""));
        let program = Program::new(r"(?m)b\s^", false, false, false, LineEndings::Unicode).unwrap();
        assert!(!is_match(&program, "a\nb\n"));
    }

    #[test]
    fn strict_end_markers_follow_syntax_in_extended_patterns() {
        let program =
            Program::new("(?x)# [\na\\z$", false, false, false, LineEndings::Unicode).unwrap();
        assert!(is_match(&program, "a"));
        assert!(!is_match(&program, "a\n"));
        let program =
            Program::new("(?x)# [\na\\Z", false, false, false, LineEndings::Unicode).unwrap();
        assert!(is_match(&program, "a\n"));
    }

    /// A search over a long subject, through the boundary machine and the
    /// fast one, ends with the statement's interruption: cancelled, or out
    /// of time. The same searches unchecked still answer.
    #[test]
    fn a_long_search_stops_when_its_statement_is_interrupted() {
        let text = format!("{}\n", "ab".repeat(200_000));
        let boundary = Program::new("b$", false, true, false, LineEndings::Unicode).unwrap();
        let fast = Program::new("b", false, false, false, LineEndings::Unicode).unwrap();
        let replace = |program: &Program, steps: &mut InterruptCounter| {
            program.replace_from(&text, 0, 0, "c", steps)
        };
        assert!(replace(&boundary, &mut InterruptCounter::unchecked()).is_ok());
        assert!(replace(&fast, &mut InterruptCounter::unchecked()).is_ok());
        let cancelled = crate::ExecutionCancellation::new();
        cancelled.cancel();
        crate::with_execution_cancellation(cancelled, || {
            for program in [&boundary, &fast] {
                assert!(matches!(
                    replace(program, &mut InterruptCounter::checked()),
                    Err(ExecError::QueryCancelled)
                ));
            }
            assert!(matches!(
                boundary.is_match(&text, &mut InterruptCounter::checked()),
                Err(ExecError::QueryCancelled)
            ));
        });
        let late = crate::ExecutionCancellation::new();
        late.limit_to(std::time::Instant::now());
        crate::with_execution_cancellation(late, || {
            assert!(matches!(
                boundary.nth_match(&text, 0, 200_000, &mut InterruptCounter::checked()),
                Err(ExecError::QueryTimedOut)
            ));
        });
    }

    #[test]
    fn a_pattern_the_automaton_cannot_write_runs_on_the_backtracking_machine() {
        for (pattern, backtracks) in [
            (r"(a)\1", true),
            ("a(?=b)", true),
            ("(?>a+)b", true),
            ("a++b", true),
            ("(a)b+$", false),
            ("a|b", false),
        ] {
            let program = Program::new(pattern, false, false, false, LineEndings::Unicode)
                .unwrap_or_else(|error| panic!("{pattern} compiles: {error}"));
            assert_eq!(
                matches!(program, Program::Backtrack(_)),
                backtracks,
                "{pattern}"
            );
        }
    }

    #[test]
    fn capture_workspace_is_bounded_before_matching() {
        let pattern = format!("{}$", "(a)".repeat(2000));
        assert!(Program::new(&pattern, false, false, false, LineEndings::Unicode).is_err());
    }
}

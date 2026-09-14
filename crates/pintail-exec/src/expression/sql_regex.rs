//! Bounded regular-expression execution with SQL line-boundary assertions.
use std::borrow::Cow;

use regex_automata::PatternID;
use regex_automata::nfa::thompson::{NFA, State};
use regex_automata::util::{
    look::{Look, LookMatcher},
    primitives::StateID,
    syntax,
};

use super::{ExecError, MAX_COMPILED_REGEX_BYTES};

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
}

impl Program {
    pub(super) fn new(
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
            Self::Fast(_) => 0,
            Self::Boundary(program) => program.workspace,
        }
    }

    pub(super) fn is_match(&self, text: &str) -> bool {
        match self {
            Self::Fast(program) => program.is_match(text),
            Self::Boundary(program) if single_line(text) => program.fast.is_match(text),
            Self::Boundary(program) => program.captures_at(text, 0).is_some(),
        }
    }

    pub(super) fn find<'a>(&self, text: &'a str) -> Option<Match<'a>> {
        let (start, end) = match self {
            Self::Fast(program) => {
                let found = program.find(text)?;
                (found.start(), found.end())
            }
            Self::Boundary(program) if single_line(text) => {
                let found = program.fast.find(text)?;
                (found.start(), found.end())
            }
            Self::Boundary(program) => {
                let slots = program.captures_at(text, 0)?;
                (slots[0], slots[1])
            }
        };
        Some(Match { text, start, end })
    }

    pub(super) fn replace_all<'a>(&self, text: &'a str, replacement: &str) -> Cow<'a, str> {
        match self {
            Self::Fast(program) => program.replace_all(text, replacement),
            Self::Boundary(program) if single_line(text) => {
                program.fast.replace_all(text, replacement)
            }
            Self::Boundary(program) => Cow::Owned(program.replace_all(text, replacement)),
        }
    }
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
    fn captures_at(&self, text: &str, from: usize) -> Option<Box<[usize]>> {
        let mut current = Vec::new();
        let mut next = Vec::new();
        let mut seen = vec![false; self.nfa.states().len()];
        let mut next_seen = seen.clone();
        let mut stack = Vec::new();
        let mut found = None;
        for at in from..=text.len() {
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
                return found;
            }
            std::mem::swap(&mut current, &mut next);
            std::mem::swap(&mut seen, &mut next_seen);
        }
        found
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

    fn replace_all(&self, text: &str, replacement: &str) -> String {
        let mut result = String::new();
        let mut from = 0;
        let mut copied = 0;
        let mut previous_end = None;
        while let Some(slots) = self.captures_at(text, from) {
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
    use super::{LineEndings, Program};

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
                        .captures_at(text, 0)
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

    #[test]
    fn multiline_start_accepts_empty_input_but_not_a_trailing_empty_line() {
        let program = Program::new("(?m)^", false, false, false, LineEndings::Unicode).unwrap();
        assert!(program.is_match(""));
        let program = Program::new(r"(?m)b\s^", false, false, false, LineEndings::Unicode).unwrap();
        assert!(!program.is_match("a\nb\n"));
    }

    #[test]
    fn strict_end_markers_follow_syntax_in_extended_patterns() {
        let program =
            Program::new("(?x)# [\na\\z$", false, false, false, LineEndings::Unicode).unwrap();
        assert!(program.is_match("a"));
        assert!(!program.is_match("a\n"));
        let program =
            Program::new("(?x)# [\na\\Z", false, false, false, LineEndings::Unicode).unwrap();
        assert!(program.is_match("a\n"));
    }

    #[test]
    fn capture_workspace_is_bounded_before_matching() {
        let pattern = format!("{}$", "(a)".repeat(2000));
        assert!(Program::new(&pattern, false, false, false, LineEndings::Unicode).is_err());
    }
}

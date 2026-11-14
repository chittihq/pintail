//! Backtracking regular-expression matching.
//!
//! The automaton in [`super::sql_regex`] cannot express a pattern whose
//! match depends on what an earlier part of it matched or on text it does
//! not consume: back-references, look-ahead and look-behind, atomic groups
//! and possessive quantifiers, `\G`. `MySQL` accepts all of them. A pattern
//! the automaton refuses is compiled here instead, to a small program run by
//! a backtracking machine that tries alternatives in the order the pattern
//! writes them.
//!
//! Backtracking can take time exponential in the subject. The machine
//! counts the alternatives it saves and the depth of its stack and stops
//! with an error at a fixed ceiling of each, as `MySQL` does with its own
//! step and stack limits; the statement's interruption is checked as well.

use std::collections::HashMap;

use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, HirKind};

use super::sql_regex::LineEndings;
use super::{ExecError, MAX_COMPILED_REGEX_BYTES};
use crate::execution::{InterruptCounter, RegexError};

/// How deeply groups may nest.
const MAX_DEPTH: usize = 64;
/// The most instructions a pattern may compile to; counted repetition
/// copies its operand.
const MAX_PROGRAM: usize = 16_000;
/// The most character ranges the sets of one pattern may hold between them.
const MAX_SET_RANGES: usize = 48_000;
/// The most groups a pattern may capture.
const MAX_GROUPS: usize = 1_000;
/// The largest count a `{n,m}` repetition may name; `MySQL` refuses the
/// next one up.
pub(super) const MAX_COUNT: u64 = 0x00ff_ffff;
/// The most operands compilation may visit. A counted repetition compiles
/// to a loop around one copy of its operand, so a pattern is visited about
/// once; the ceiling is a guard, not a budget patterns come near.
const MAX_COMPILE_WORK: usize = 1 << 20;
/// What reading or compiling one operand counts for on the statement's
/// interrupt counter, in steps of a search: it builds sets and tables where
/// a search step reads a character, and a pattern is far shorter than the
/// stretch between two checks, so counted singly a pattern would never
/// reach one.
const COMPILE_STEP: usize = 64;
/// The most saved alternatives the stack may hold at once. A loop over one
/// character, or over a fixed run of them, holds one however long the text
/// it takes; a loop over anything else holds one for every turn, so this
/// is how many turns such a loop may take.
const MAX_STACK: usize = 8_000;
/// The most assignments the capture log may hold to undo. A turn of a loop
/// around a capturing group logs about five.
const MAX_TRAIL: usize = 40_000;
/// The most registers the loops of a pattern may count and mark in.
const MAX_REGISTERS: usize = 2_000;
/// The longest fixed run of characters a loop is taken over as one unit.
const MAX_RUN: usize = 64;
/// The most alternatives one search may save in total.
const MAX_SAVES: usize = 4_000_000;
/// The longest text a look-behind may need to read back over.
const MAX_LOOK_BEHIND: usize = 1_000;

/// Bytes one search can hold at its ceilings: the stack and the capture
/// log, which never grow past them, and the capture slots and loop
/// registers with the stamp each carries. A statement is charged this for
/// every program before it runs one.
pub(in crate::expression) const WORKSPACE: usize = MAX_STACK * size_of::<Frame>()
    + MAX_TRAIL * size_of::<Undo>()
    + (3 * (MAX_GROUPS + 1) + MAX_REGISTERS) * 2 * size_of::<u32>();

// A pattern that is not a literal is charged the automaton's ceilings, so
// this machine has to fit inside them: its search in the workspace, its
// program in the compiled size.
const _: () = assert!(WORKSPACE <= super::sql_regex::WORKSPACE_LIMIT);
const _: () = assert!(
    MAX_PROGRAM * size_of::<Inst>() + MAX_SET_RANGES * size_of::<(char, char)>()
        <= MAX_COMPILED_REGEX_BYTES
);

const TIMEOUT: ExecError = ExecError::Regex(RegexError::Timeout);
const STACK: ExecError = ExecError::Regex(RegexError::StackOverflow);
const SYNTAX: ExecError = ExecError::Regex(RegexError::IllegalArgument);
const LOOK_BEHIND: ExecError = ExecError::Regex(RegexError::LookBehindLimit);
const BACK_REFERENCE: ExecError = ExecError::Regex(RegexError::InvalidBackReference);
const NUMBER_TOO_BIG: ExecError = ExecError::Regex(RegexError::NumberTooBig);

/// The switches a pattern can set for itself, each on its own.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy)]
struct Flags {
    case_insensitive: bool,
    multi_line: bool,
    dot_all: bool,
    extended: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Assertion {
    /// `\A`, and `^` outside multi-line mode.
    TextStart,
    /// `^` in multi-line mode.
    LineStart,
    /// `\z`.
    TextEnd,
    /// `\Z`, and `$` outside multi-line mode: the end, or before a final
    /// line ending.
    LastLineEnd,
    /// `$` in multi-line mode.
    LineEnd,
    WordBoundary,
    NotWordBoundary,
    /// `\G`: where this search began.
    SearchStart,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Greed {
    Greedy,
    Lazy,
    Possessive,
}

enum Node {
    Empty,
    /// One character out of the set with this number in the parser's table.
    Set(u32),
    /// `.`: any character, or any but a line ending.
    Any {
        dot_all: bool,
    },
    Assert(Assertion),
    Backref {
        group: usize,
        case_insensitive: bool,
    },
    NamedBackref {
        name: String,
        case_insensitive: bool,
    },
    Group(Option<usize>, Box<Node>),
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
        greed: Greed,
    },
    Look {
        behind: bool,
        negate: bool,
        node: Box<Node>,
    },
    Atomic(Box<Node>),
}

/// The distinct character sets of a pattern, each kept once however often
/// it is written, and what was computed to find them.
#[derive(Default)]
struct Sets {
    table: Vec<Ranges>,
    numbers: HashMap<Ranges, u32>,
    /// The ranges held by `sets` between them.
    ranges: usize,
    /// The ranges an escape or a named class stands for, by its spelling.
    fragments: HashMap<String, Vec<(char, char)>>,
    /// The finished form of raw ranges: folded and negated as asked.
    finished: HashMap<Unfinished, Ranges>,
}

impl Sets {
    fn class_ranges(&mut self, fragment: &str) -> Result<Vec<(char, char)>, ExecError> {
        if let Some(ranges) = self.fragments.get(fragment) {
            return Ok(ranges.clone());
        }
        let ranges = class_ranges(fragment)?;
        self.fragments.insert(fragment.to_owned(), ranges.clone());
        Ok(ranges)
    }

    fn finish(
        &mut self,
        ranges: Vec<(char, char)>,
        case_insensitive: bool,
        negate: bool,
    ) -> Box<[(char, char)]> {
        let key = (ranges, case_insensitive, negate);
        if let Some(finished) = self.finished.get(&key) {
            return finished.clone();
        }
        let finished = finish_set(key.0.clone(), case_insensitive, negate);
        self.finished.insert(key, finished.clone());
        finished
    }

    /// The number of `set` in the table, adding it when it is new. A
    /// pattern whose sets hold more ranges between them than the compiled
    /// size allows is refused here, before it holds them.
    fn number(&mut self, set: Box<[(char, char)]>) -> Result<u32, ExecError> {
        if let Some(number) = self.numbers.get(&set) {
            return Ok(*number);
        }
        self.ranges += set.len();
        if self.ranges > MAX_SET_RANGES {
            return Err(SYNTAX);
        }
        let number = u32::try_from(self.table.len()).map_err(|_| SYNTAX)?;
        self.table.push(set.clone());
        self.numbers.insert(set, number);
        Ok(number)
    }
}

struct Parser<'steps> {
    chars: Vec<char>,
    at: usize,
    groups: usize,
    names: Vec<(String, usize)>,
    depth: usize,
    sets: Sets,
    /// The statement's interruption, asked as the pattern is read.
    steps: &'steps mut InterruptCounter,
    /// The character the last operand was written as, when it was one
    /// literal character.
    literal: Option<char>,
    /// How many alternatives literals may still be spelled out into across
    /// a folding of more than one character; none while such a spelling is
    /// itself being read.
    fold_budget: usize,
}

fn class_ranges(fragment: &str) -> Result<Vec<(char, char)>, ExecError> {
    let hir = regex_syntax::ParserBuilder::new()
        .build()
        .parse(fragment)
        .map_err(|_| SYNTAX)?;
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => Ok(class
            .ranges()
            .iter()
            .map(|range| (range.start(), range.end()))
            .collect()),
        HirKind::Class(Class::Bytes(class)) => Ok(class
            .ranges()
            .iter()
            .map(|range| (char::from(range.start()), char::from(range.end())))
            .collect()),
        HirKind::Literal(literal) => {
            let text = std::str::from_utf8(&literal.0).map_err(|_| SYNTAX)?;
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(only), None) => Ok(vec![(only, only)]),
                _ => Err(SYNTAX),
            }
        }
        _ => Err(SYNTAX),
    }
}

/// A set of characters in the machine's form: folded for case when asked,
/// negated when asked, sorted and merged.
fn finish_set(
    ranges: Vec<(char, char)>,
    case_insensitive: bool,
    negate: bool,
) -> Box<[(char, char)]> {
    let mut class = ClassUnicode::new(
        ranges
            .into_iter()
            .map(|(start, end)| ClassUnicodeRange::new(start, end)),
    );
    if case_insensitive {
        class.case_fold_simple();
    }
    if negate {
        class.negate();
    }
    class
        .ranges()
        .iter()
        .map(|range| (range.start(), range.end()))
        .collect()
}

/// What one escape or class item stands for.
enum Item {
    Char(char),
    Ranges(Vec<(char, char)>),
}

impl Parser<'_> {
    /// The operand matching one character of `ranges`.
    fn set(
        &mut self,
        ranges: Vec<(char, char)>,
        case_insensitive: bool,
        negate: bool,
    ) -> Result<Node, ExecError> {
        let finished = self.sets.finish(ranges, case_insensitive, negate);
        self.sets.number(finished).map(Node::Set)
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn looking_at(&self, text: &str) -> bool {
        text.chars()
            .enumerate()
            .all(|(index, expected)| self.peek_at(index) == Some(expected))
    }

    /// Skips `(?#...)` comments and, in extended mode, white space and
    /// `#` comments: none of them is an operand or separates one from its
    /// quantifier.
    fn skip_trivia(&mut self, flags: Flags) -> Result<(), ExecError> {
        loop {
            if self.looking_at("(?#") {
                while let Some(character) = self.peek() {
                    self.at += 1;
                    if character == ')' {
                        break;
                    }
                }
                if self.chars.get(self.at.wrapping_sub(1)) != Some(&')') {
                    return Err(SYNTAX);
                }
            } else if flags.extended && self.peek().is_some_and(char::is_whitespace) {
                self.at += 1;
            } else if flags.extended && self.peek() == Some('#') {
                while self.peek().is_some_and(|character| character != '\n') {
                    self.at += 1;
                }
            } else {
                return Ok(());
            }
        }
    }

    fn alternation(&mut self, flags: &mut Flags) -> Result<Node, ExecError> {
        let mut branches = vec![self.concatenation(flags)?];
        while self.eat('|') {
            branches.push(self.concatenation(flags)?);
        }
        if branches.len() == 1 {
            return Ok(branches.pop().unwrap_or(Node::Empty));
        }
        // Alternatives that each take one character are one set: the same
        // characters match, with no alternative to save and come back to.
        let sets = branches
            .iter()
            .map(|branch| match branch {
                Node::Set(set) => Some(*set),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
        if let Some(sets) = sets {
            let ranges = sets
                .into_iter()
                .flat_map(|set| self.sets.table[set as usize].iter().copied())
                .collect::<Vec<_>>();
            self.literal = None;
            return self.set(ranges, false, false);
        }
        Ok(Node::Alternate(branches))
    }

    fn concatenation(&mut self, flags: &mut Flags) -> Result<Node, ExecError> {
        let mut items = Vec::new();
        // The literal characters written one after another, waiting to be
        // read as one run.
        let mut run: Vec<(char, Node)> = Vec::new();
        loop {
            self.skip_trivia(*flags)?;
            if matches!(self.peek(), None | Some('|' | ')')) {
                break;
            }
            let Some(atom) = self.atom(flags)? else {
                continue;
            };
            let literal = self.literal.take().filter(|_| flags.case_insensitive);
            self.skip_trivia(*flags)?;
            let before = self.at;
            let quantified = self.quantified(atom)?;
            match literal {
                // A literal alone under a quantifier is a run of one.
                Some(character) if self.at != before => {
                    self.folded_run(&mut run, &mut items)?;
                    let Node::Repeat {
                        node,
                        min,
                        max,
                        greed,
                    } = quantified
                    else {
                        items.push(quantified);
                        continue;
                    };
                    let mut single = Vec::new();
                    self.folded_run(&mut vec![(character, *node)], &mut single)?;
                    items.push(Node::Repeat {
                        node: Box::new(Node::Group(None, Box::new(Node::Concat(single)))),
                        min,
                        max,
                        greed,
                    });
                }
                Some(character) => run.push((character, quantified)),
                None => {
                    self.folded_run(&mut run, &mut items)?;
                    items.push(quantified);
                }
            }
        }
        self.folded_run(&mut run, &mut items)?;
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    /// Moves a run of literal characters into `items`. Under a
    /// case-insensitive match a literal matches across a case folding of
    /// more than one character - `ss` written out matches `ß`, and `ß`
    /// matches `ss` - exactly as the automaton's patterns are rewritten to:
    /// the run is spelled out by the same rule and read back.
    fn folded_run(
        &mut self,
        run: &mut Vec<(char, Node)>,
        items: &mut Vec<Node>,
    ) -> Result<(), ExecError> {
        if run.is_empty() {
            return Ok(());
        }
        let characters = run
            .iter()
            .map(|(character, _)| *character)
            .collect::<Vec<_>>();
        let spelled = if self.fold_budget == 0 {
            String::new()
        } else {
            super::sql_regex::spelled(
                &characters,
                0,
                super::sql_regex::full_folds(),
                &mut self.fold_budget,
            )
        };
        if !spelled.contains("(?:") {
            items.extend(run.drain(..).map(|(_, node)| node));
            return Ok(());
        }
        run.clear();
        // The spelling is read by this parser in place of the pattern, with
        // no further spelling out, and holds no groups of its own.
        let pattern = std::mem::replace(&mut self.chars, spelled.chars().collect());
        let at = std::mem::replace(&mut self.at, 0);
        let budget = std::mem::replace(&mut self.fold_budget, 0);
        let mut flags = Flags {
            case_insensitive: true,
            multi_line: false,
            dot_all: false,
            extended: false,
        };
        let node = self.alternation(&mut flags);
        let read = self.at == self.chars.len();
        self.chars = pattern;
        self.at = at;
        self.fold_budget = budget;
        self.literal = None;
        let node = node?;
        if !read {
            return Err(SYNTAX);
        }
        items.push(Node::Group(None, Box::new(node)));
        Ok(())
    }

    fn quantified(&mut self, atom: Node) -> Result<Node, ExecError> {
        let saved = self.at;
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                self.at += 1;
                let Some(bounds) = self.bounds()? else {
                    self.at = saved;
                    return Ok(atom);
                };
                self.at -= 1;
                bounds
            }
            _ => return Ok(atom),
        };
        self.at += 1;
        if max.is_some_and(|max| max < min) {
            return Err(SYNTAX);
        }
        let greed = if self.eat('?') {
            Greed::Lazy
        } else if self.eat('+') {
            Greed::Possessive
        } else {
            Greed::Greedy
        };
        if matches!(atom, Node::Assert(_) | Node::Look { .. }) && min == 0 {
            // Zero repetitions of something that consumes nothing.
            return Ok(Node::Empty);
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greed,
        })
    }

    /// `n}`, `n,}` or `n,m}` after a `{`, leaving the position past the
    /// brace; `None` when the brace does not open a count.
    ///
    /// A count past [`MAX_COUNT`] is refused, as `MySQL` refuses it: no
    /// loop anywhere runs a number of times the pattern could not have
    /// been matched against.
    fn bounds(&mut self) -> Result<Option<(u32, Option<u32>)>, ExecError> {
        let number = |parser: &mut Self| {
            let start = parser.at;
            let mut value = 0_u64;
            while let Some(digit) = parser.peek().and_then(|character| character.to_digit(10)) {
                value = value.saturating_mul(10).saturating_add(u64::from(digit));
                parser.at += 1;
            }
            (parser.at > start).then_some(value)
        };
        let Some(min) = number(self) else {
            return Ok(None);
        };
        let max = if self.eat(',') {
            if self.peek() == Some('}') {
                None
            } else {
                let Some(max) = number(self) else {
                    return Ok(None);
                };
                Some(max)
            }
        } else {
            Some(min)
        };
        if !self.eat('}') {
            return Ok(None);
        }
        let count = |value: u64| {
            u32::try_from(value)
                .ok()
                .filter(|_| value <= MAX_COUNT)
                .ok_or(NUMBER_TOO_BIG)
        };
        Ok(Some((count(min)?, max.map(count).transpose()?)))
    }

    /// One operand. `None` when what was read only changed the flags.
    fn atom(&mut self, flags: &mut Flags) -> Result<Option<Node>, ExecError> {
        self.steps.steps(COMPILE_STEP)?;
        let Some(character) = self.peek() else {
            return Err(SYNTAX);
        };
        self.at += 1;
        Ok(Some(match character {
            '(' => {
                let group = self.group(flags);
                // What the group held is not this operand's character.
                self.literal = None;
                return group;
            }
            '[' => {
                let set = self.class(*flags)?;
                Node::Set(self.sets.number(set)?)
            }
            '.' => Node::Any {
                dot_all: flags.dot_all,
            },
            '^' => Node::Assert(if flags.multi_line {
                Assertion::LineStart
            } else {
                Assertion::TextStart
            }),
            '$' => Node::Assert(if flags.multi_line {
                Assertion::LineEnd
            } else {
                Assertion::LastLineEnd
            }),
            '\\' => return self.escape(*flags).map(Some),
            '*' | '+' | '?' => return Err(SYNTAX),
            literal => {
                self.literal = Some(literal);
                self.set(vec![(literal, literal)], flags.case_insensitive, false)?
            }
        }))
    }

    fn group(&mut self, flags: &mut Flags) -> Result<Option<Node>, ExecError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SYNTAX);
        }
        let mut inner = *flags;
        let node = if self.eat('?') {
            if self.eat(':') {
                Node::Group(None, Box::new(self.alternation(&mut inner)?))
            } else if self.eat('=') {
                Node::Look {
                    behind: false,
                    negate: false,
                    node: Box::new(self.alternation(&mut inner)?),
                }
            } else if self.eat('!') {
                Node::Look {
                    behind: false,
                    negate: true,
                    node: Box::new(self.alternation(&mut inner)?),
                }
            } else if self.looking_at("<=") || self.looking_at("<!") {
                let negate = self.peek_at(1) == Some('!');
                self.at += 2;
                Node::Look {
                    behind: true,
                    negate,
                    node: Box::new(self.alternation(&mut inner)?),
                }
            } else if self.eat('>') {
                Node::Atomic(Box::new(self.alternation(&mut inner)?))
            } else if self.eat('<') {
                let mut name = String::new();
                while let Some(character) = self.peek().filter(|character| *character != '>') {
                    name.push(character);
                    self.at += 1;
                }
                if !self.eat('>')
                    || name.is_empty()
                    || !name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric())
                {
                    return Err(SYNTAX);
                }
                self.groups += 1;
                let index = self.groups;
                self.names.push((name, index));
                Node::Group(Some(index), Box::new(self.alternation(&mut inner)?))
            } else {
                // Flags: for the rest of the enclosing group, or with a
                // colon for the group they open.
                let mut on = true;
                loop {
                    let Some(flag) = self.peek() else {
                        return Err(SYNTAX);
                    };
                    self.at += 1;
                    match flag {
                        '-' => on = false,
                        'i' => inner.case_insensitive = on,
                        'm' => inner.multi_line = on,
                        's' => inner.dot_all = on,
                        'x' => inner.extended = on,
                        // Word-boundary and Unix-line modes are the
                        // statement's, not the pattern's, to choose.
                        'w' | 'u' => {}
                        ')' => {
                            *flags = inner;
                            self.depth -= 1;
                            return Ok(None);
                        }
                        ':' => break,
                        _ => return Err(SYNTAX),
                    }
                }
                Node::Group(None, Box::new(self.alternation(&mut inner)?))
            }
        } else {
            self.groups += 1;
            let index = self.groups;
            Node::Group(Some(index), Box::new(self.alternation(&mut inner)?))
        };
        if !self.eat(')') {
            return Err(SYNTAX);
        }
        self.depth -= 1;
        Ok(Some(node))
    }

    fn hex(&mut self, digits: usize) -> Result<char, ExecError> {
        let mut value = 0_u32;
        for _ in 0..digits {
            let digit = self
                .peek()
                .and_then(|character| character.to_digit(16))
                .ok_or(SYNTAX)?;
            value = value * 16 + digit;
            self.at += 1;
        }
        char::from_u32(value).ok_or(SYNTAX)
    }

    /// The escape after a backslash that both a pattern and a bracketed set
    /// read the same way: one character, or a class of them.
    fn escaped_item(&mut self, escape: char) -> Result<Item, ExecError> {
        Ok(match escape {
            'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                Item::Ranges(self.sets.class_ranges(&format!("\\{escape}"))?)
            }
            'p' | 'P' => {
                let mut fragment = format!("\\{escape}");
                if self.eat('{') {
                    fragment.push('{');
                    loop {
                        let character = self.peek().ok_or(SYNTAX)?;
                        self.at += 1;
                        fragment.push(character);
                        if character == '}' {
                            break;
                        }
                    }
                } else {
                    fragment.push(self.peek().ok_or(SYNTAX)?);
                    self.at += 1;
                }
                Item::Ranges(self.sets.class_ranges(&fragment)?)
            }
            'x' => Item::Char(if self.eat('{') {
                let mut value = 0_u32;
                while let Some(digit) = self.peek().and_then(|character| character.to_digit(16)) {
                    value = value.checked_mul(16).ok_or(SYNTAX)? + digit;
                    self.at += 1;
                }
                if !self.eat('}') {
                    return Err(SYNTAX);
                }
                char::from_u32(value).ok_or(SYNTAX)?
            } else {
                self.hex(2)?
            }),
            'u' => Item::Char(self.hex(4)?),
            'U' => Item::Char(self.hex(8)?),
            '0' => {
                let mut value = 0_u32;
                let mut digits = 0;
                while digits < 3
                    && let Some(digit) = self.peek().and_then(|character| character.to_digit(8))
                {
                    value = value * 8 + digit;
                    self.at += 1;
                    digits += 1;
                }
                if digits == 0 {
                    return Err(SYNTAX);
                }
                Item::Char(char::from_u32(value).ok_or(SYNTAX)?)
            }
            'c' => match self.peek() {
                Some(control) => {
                    self.at += 1;
                    Item::Char(char::from_u32(u32::from(control) & 0x1f).ok_or(SYNTAX)?)
                }
                None => Item::Char('c'),
            },
            // Horizontal and vertical white space, and a line break.
            'h' | 'H' => Item::Ranges(
                finish_set(self.sets.class_ranges(r"[\t\p{Zs}]")?, false, escape == 'H').into_vec(),
            ),
            'v' | 'V' | 'R' => Item::Ranges(
                finish_set(
                    vec![('\n', '\r'), ('\u{85}', '\u{85}'), ('\u{2028}', '\u{2029}')],
                    false,
                    escape == 'V',
                )
                .into_vec(),
            ),
            'a' => Item::Char('\u{7}'),
            'e' => Item::Char('\u{1b}'),
            'f' => Item::Char('\u{c}'),
            'n' => Item::Char('\n'),
            'r' => Item::Char('\r'),
            't' => Item::Char('\t'),
            // A grapheme cluster and a named character are not read here.
            'X' | 'N' => return Err(SYNTAX),
            // Any other escaped character is itself.
            other => Item::Char(other),
        })
    }

    fn escape(&mut self, flags: Flags) -> Result<Node, ExecError> {
        let Some(escape) = self.peek() else {
            return Err(SYNTAX);
        };
        self.at += 1;
        Ok(match escape {
            '1'..='9' => {
                // Digits join the group number while it still names a
                // group the pattern has opened by this point.
                let mut group = escape.to_digit(10).ok_or(SYNTAX)? as usize;
                while group < self.groups
                    && let Some(digit) = self.peek().and_then(|character| character.to_digit(10))
                {
                    group = group * 10 + digit as usize;
                    self.at += 1;
                }
                Node::Backref {
                    group,
                    case_insensitive: flags.case_insensitive,
                }
            }
            'k' => {
                if !self.eat('<') {
                    return Err(SYNTAX);
                }
                let mut name = String::new();
                while let Some(character) = self.peek().filter(|character| *character != '>') {
                    name.push(character);
                    self.at += 1;
                }
                if !self.eat('>') {
                    return Err(SYNTAX);
                }
                Node::NamedBackref {
                    name,
                    case_insensitive: flags.case_insensitive,
                }
            }
            'b' => Node::Assert(Assertion::WordBoundary),
            'B' => Node::Assert(Assertion::NotWordBoundary),
            'A' => Node::Assert(Assertion::TextStart),
            'z' => Node::Assert(Assertion::TextEnd),
            'Z' => Node::Assert(Assertion::LastLineEnd),
            'G' => Node::Assert(Assertion::SearchStart),
            'Q' => {
                let mut quoted = Vec::new();
                while self.peek().is_some() && !self.looking_at("\\E") {
                    let literal = self.chars[self.at];
                    self.at += 1;
                    self.steps.steps(COMPILE_STEP)?;
                    quoted.push(self.set(
                        vec![(literal, literal)],
                        flags.case_insensitive,
                        false,
                    )?);
                }
                if self.looking_at("\\E") {
                    self.at += 2;
                }
                Node::Group(None, Box::new(Node::Concat(quoted)))
            }
            'E' => Node::Empty,
            other => match self.escaped_item(other)? {
                Item::Char(literal) => {
                    self.set(vec![(literal, literal)], flags.case_insensitive, false)?
                }
                Item::Ranges(ranges) => self.set(ranges, flags.case_insensitive, false)?,
            },
        })
    }

    /// A bracketed set, after its `[`. A hyphen between two characters is a
    /// range; beside a class it is itself.
    fn class(&mut self, flags: Flags) -> Result<Box<[(char, char)]>, ExecError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(SYNTAX);
        }
        let negate = self.eat('^');
        let mut ranges = Vec::new();
        let mut first = true;
        loop {
            let Some(character) = self.peek() else {
                return Err(SYNTAX);
            };
            self.at += 1;
            let item = match character {
                ']' if !first => break,
                '[' if self.peek() == Some(':') => {
                    let mut fragment = String::from("[[");
                    loop {
                        let character = self.peek().ok_or(SYNTAX)?;
                        self.at += 1;
                        fragment.push(character);
                        if character == ']' {
                            break;
                        }
                    }
                    fragment.push(']');
                    Item::Ranges(self.sets.class_ranges(&fragment)?)
                }
                '[' => Item::Ranges(
                    self.class(Flags {
                        case_insensitive: false,
                        ..flags
                    })?
                    .into_vec(),
                ),
                '\\' => {
                    let escape = self.peek().ok_or(SYNTAX)?;
                    self.at += 1;
                    self.escaped_item(escape)?
                }
                literal => Item::Char(literal),
            };
            first = false;
            match item {
                Item::Ranges(more) => ranges.extend(more),
                Item::Char(start) => {
                    // A range needs a single character on its far side.
                    let end = if self.peek() == Some('-')
                        && !matches!(self.peek_at(1), None | Some(']' | '['))
                    {
                        let saved = self.at;
                        self.at += 1;
                        let next = self.peek().ok_or(SYNTAX)?;
                        self.at += 1;
                        let far = if next == '\\' {
                            let escape = self.peek().ok_or(SYNTAX)?;
                            self.at += 1;
                            self.escaped_item(escape)?
                        } else {
                            Item::Char(next)
                        };
                        match far {
                            Item::Char(end) => Some(end),
                            Item::Ranges(_) => {
                                self.at = saved;
                                None
                            }
                        }
                    } else {
                        None
                    };
                    match end {
                        Some(end) if end < start => return Err(SYNTAX),
                        Some(end) => ranges.push((start, end)),
                        None => ranges.push((start, start)),
                    }
                }
            }
        }
        self.depth -= 1;
        Ok(self.sets.finish(ranges, flags.case_insensitive, negate))
    }
}

impl Node {
    /// Whether the node can match without consuming a character.
    fn nullable(&self) -> bool {
        match self {
            // A back-reference may repeat a group that matched nothing.
            Self::Empty
            | Self::Assert(_)
            | Self::Look { .. }
            | Self::Backref { .. }
            | Self::NamedBackref { .. } => true,
            Self::Set(_) | Self::Any { .. } => false,
            Self::Group(_, node) | Self::Atomic(node) => node.nullable(),
            Self::Concat(nodes) => nodes.iter().all(Self::nullable),
            Self::Alternate(nodes) => nodes.iter().any(Self::nullable),
            Self::Repeat { node, min, .. } => *min == 0 || node.nullable(),
        }
    }

    /// The fewest and most characters the node can match; `None` for the
    /// most when it has no bound.
    fn width(&self) -> (usize, Option<usize>) {
        match self {
            Self::Empty | Self::Assert(_) | Self::Look { .. } => (0, Some(0)),
            Self::Backref { .. } | Self::NamedBackref { .. } => (0, None),
            Self::Set(_) | Self::Any { .. } => (1, Some(1)),
            Self::Group(_, node) | Self::Atomic(node) => node.width(),
            Self::Concat(nodes) => nodes.iter().fold((0, Some(0)), |(low, high), node| {
                let (min, max) = node.width();
                (
                    low.saturating_add(min),
                    high.zip(max)
                        .map(|(left, right)| left.saturating_add(right)),
                )
            }),
            Self::Alternate(nodes) => {
                nodes
                    .iter()
                    .fold((usize::MAX, Some(0)), |(low, high), node| {
                        let (min, max) = node.width();
                        (
                            low.min(min),
                            high.zip(max).map(|(left, right)| left.max(right)),
                        )
                    })
            }
            Self::Repeat { node, min, max, .. } => {
                let (low, high) = node.width();
                (
                    low.saturating_mul(*min as usize),
                    high.zip(*max)
                        .map(|(width, count)| width.saturating_mul(count as usize)),
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Inst {
    /// One character in set `.0`.
    Set(u32),
    Any {
        dot_all: bool,
    },
    Assert(Assertion),
    /// Try `.0`; on failure resume at `.1`.
    Split(u32, u32),
    Jump(u32),
    Open(u32),
    Close(u32),
    Backref {
        group: u32,
        case_insensitive: bool,
    },
    /// Remembers the position in register `.0` as a loop body starts.
    Mark(u32),
    /// Leaves the loop for `exit` when its body consumed nothing.
    Progress {
        register: u32,
        exit: u32,
    },
    /// Starts a counted loop: no turn taken yet.
    CountStart {
        counter: u32,
    },
    /// The head of a counted loop, reached before every turn: leaves for
    /// `exit` when the count is full or the last turn consumed nothing,
    /// takes another turn while the count is short of `min`, and between
    /// the two prefers another turn, or leaving when `lazy`.
    CountCheck {
        counter: u32,
        mark: u32,
        min: u32,
        /// `u32::MAX` for no upper bound.
        max: u32,
        exit: u32,
        lazy: bool,
    },
    /// Enters a turn of a counted loop, remembering where it began.
    CountEnter {
        mark: u32,
    },
    /// Ends a turn of a counted loop.
    CountTurn {
        counter: u32,
    },
    /// A loop over a fixed run of characters, taken whole: as many turns
    /// of the `len` sets from `seq` in the run table as there are, `max` at
    /// most, failing under `min`, and given back a turn at a time. One
    /// saved alternative serves the whole loop.
    Run {
        seq: u32,
        len: u32,
        min: u32,
        /// `u32::MAX` for no upper bound.
        max: u32,
    },
    /// The instructions up to the matching `End` as an assertion.
    Look {
        behind: bool,
        negate: bool,
        min: u32,
        max: u32,
        after: u32,
    },
    /// The instructions up to the matching `End`, their first match final.
    Atomic {
        after: u32,
    },
    End,
}

const ANY_BUT_LINE_END: u32 = u32::MAX;
const ANY: u32 = u32::MAX - 1;

/// What a position that is not one is written as.
const NONE: u32 = u32::MAX;

pub(super) struct Program {
    /// Operands compilation visited, kept for the tests that bound it.
    #[cfg_attr(not(test), allow(dead_code))]
    work: usize,
    insts: Vec<Inst>,
    sets: Vec<Box<[(char, char)]>>,
    /// The sets of every [`Inst::Run`], one after another.
    runs: Vec<u32>,
    word: Box<[(char, char)]>,
    groups: usize,
    registers: usize,
    names: Vec<(String, usize)>,
    line_endings: LineEndings,
}

struct Compiler<'steps> {
    insts: Vec<Inst>,
    runs: Vec<u32>,
    registers: usize,
    groups: usize,
    names: Vec<(String, usize)>,
    /// Operands visited so far.
    work: usize,
    /// The statement's interruption, asked at every operand.
    steps: &'steps mut InterruptCounter,
}

impl Compiler<'_> {
    fn push(&mut self, inst: Inst) -> Result<u32, ExecError> {
        if self.insts.len() >= MAX_PROGRAM {
            return Err(SYNTAX);
        }
        self.insts.push(inst);
        Ok(self.here() - 1)
    }

    fn here(&self) -> u32 {
        u32::try_from(self.insts.len()).unwrap_or(u32::MAX)
    }

    fn register(&mut self) -> Result<u32, ExecError> {
        if self.registers >= MAX_REGISTERS {
            return Err(SYNTAX);
        }
        let register = u32::try_from(self.registers).map_err(|_| SYNTAX)?;
        self.registers += 1;
        Ok(register)
    }

    /// The sets `node` takes one character of each from, in order, when it
    /// is a fixed run of characters and nothing else: no group that
    /// captures, no choice, nothing of variable length.
    fn run_of(node: &Node, out: &mut Vec<u32>) -> bool {
        match node {
            Node::Set(set) => out.push(*set),
            Node::Any { dot_all: true } => out.push(ANY),
            Node::Any { dot_all: false } => out.push(ANY_BUT_LINE_END),
            Node::Group(None, node) => return Self::run_of(node, out),
            Node::Concat(nodes) => return nodes.iter().all(|node| Self::run_of(node, out)),
            Node::Repeat {
                node,
                min,
                max: Some(max),
                ..
            } if min == max && (*min as usize) <= MAX_RUN => {
                for _ in 0..*min {
                    if !Self::run_of(node, out) || out.len() > MAX_RUN {
                        return false;
                    }
                }
            }
            _ => return false,
        }
        out.len() <= MAX_RUN
    }

    fn group(&self, group: usize) -> Result<u32, ExecError> {
        if group == 0 || group > self.groups {
            return Err(BACK_REFERENCE);
        }
        u32::try_from(group).map_err(|_| BACK_REFERENCE)
    }

    #[allow(clippy::too_many_lines)]
    fn emit(&mut self, node: &Node) -> Result<(), ExecError> {
        // Every operand is visited a bounded number of times - a counted
        // repetition does not copy its operand - so this ceiling is not
        // reached by a pattern of the size the engine accepts; it and the
        // interruption check make the bound hold whatever is compiled.
        self.work += 1;
        if self.work > MAX_COMPILE_WORK {
            return Err(SYNTAX);
        }
        self.steps.steps(COMPILE_STEP)?;
        match node {
            Node::Empty => {}
            Node::Set(set) => {
                self.push(Inst::Set(*set))?;
            }
            Node::Any { dot_all } => {
                self.push(Inst::Any { dot_all: *dot_all })?;
            }
            Node::Assert(assertion) => {
                self.push(Inst::Assert(*assertion))?;
            }
            Node::Backref {
                group,
                case_insensitive,
            } => {
                let group = self.group(*group)?;
                self.push(Inst::Backref {
                    group,
                    case_insensitive: *case_insensitive,
                })?;
            }
            Node::NamedBackref {
                name,
                case_insensitive,
            } => {
                let group = self
                    .names
                    .iter()
                    .find(|(known, _)| known == name)
                    .map(|(_, group)| *group)
                    .ok_or(BACK_REFERENCE)?;
                let group = self.group(group)?;
                self.push(Inst::Backref {
                    group,
                    case_insensitive: *case_insensitive,
                })?;
            }
            Node::Group(None, node) => self.emit(node)?,
            Node::Group(Some(group), node) => {
                let group = u32::try_from(*group).map_err(|_| SYNTAX)?;
                self.push(Inst::Open(group))?;
                self.emit(node)?;
                self.push(Inst::Close(group))?;
            }
            Node::Concat(nodes) => {
                for node in nodes {
                    self.emit(node)?;
                }
            }
            Node::Alternate(nodes) => {
                let mut exits = Vec::new();
                for (index, node) in nodes.iter().enumerate() {
                    if index + 1 < nodes.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit(node)?;
                        exits.push(self.push(Inst::Jump(0))?);
                        let next = self.here();
                        self.insts[split as usize] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(node)?;
                    }
                }
                let end = self.here();
                for exit in exits {
                    self.insts[exit as usize] = Inst::Jump(end);
                }
            }
            Node::Repeat {
                node,
                min,
                max,
                greed,
            } => self.repeat(node, *min, *max, *greed)?,
            Node::Look {
                behind,
                negate,
                node,
            } => {
                let (min, max) = node.width();
                let (min, max) = if *behind {
                    let max = max
                        .filter(|max| *max <= MAX_LOOK_BEHIND)
                        .ok_or(LOOK_BEHIND)?;
                    (
                        u32::try_from(min).map_err(|_| LOOK_BEHIND)?,
                        u32::try_from(max).map_err(|_| LOOK_BEHIND)?,
                    )
                } else {
                    (0, 0)
                };
                let look = self.push(Inst::End)?;
                self.emit(node)?;
                self.push(Inst::End)?;
                self.insts[look as usize] = Inst::Look {
                    behind: *behind,
                    negate: *negate,
                    min,
                    max,
                    after: self.here(),
                };
            }
            Node::Atomic(node) => {
                let atomic = self.push(Inst::End)?;
                self.emit(node)?;
                self.push(Inst::End)?;
                self.insts[atomic as usize] = Inst::Atomic { after: self.here() };
            }
        }
        Ok(())
    }

    fn repeat(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        greed: Greed,
    ) -> Result<(), ExecError> {
        if greed == Greed::Possessive {
            let atomic = self.push(Inst::End)?;
            self.repeat(node, min, max, Greed::Greedy)?;
            self.push(Inst::End)?;
            self.insts[atomic as usize] = Inst::Atomic { after: self.here() };
            return Ok(());
        }
        // A greedy loop over a fixed run of characters is one instruction:
        // it takes its turns without saving an alternative for each, and
        // gives them back from the end.
        let mut run = Vec::new();
        if greed == Greed::Greedy && Self::run_of(node, &mut run) && !run.is_empty() {
            let seq = u32::try_from(self.runs.len()).map_err(|_| SYNTAX)?;
            let len = u32::try_from(run.len()).map_err(|_| SYNTAX)?;
            if self.runs.len() + run.len() > MAX_PROGRAM {
                return Err(SYNTAX);
            }
            self.runs.extend(run);
            self.push(Inst::Run {
                seq,
                len,
                min,
                max: max.unwrap_or(u32::MAX),
            })?;
            return Ok(());
        }
        // A count is a loop around one copy of the operand, so compiling
        // costs the same whatever the count is.
        match (min, max) {
            (0, Some(0)) => return Ok(()),
            (1, Some(1)) => return self.emit(node),
            (0 | 1, None) | (0, Some(1)) => {}
            _ => return self.counted(node, min, max, greed == Greed::Lazy),
        }
        if min == 1 {
            self.emit(node)?;
        }
        let split = |first: u32, second: u32| {
            if greed == Greed::Greedy {
                Inst::Split(first, second)
            } else {
                Inst::Split(second, first)
            }
        };
        let Some(max) = max else {
            let start = self.push(Inst::Split(0, 0))?;
            let nullable = node.nullable();
            let register = if nullable { self.register()? } else { 0 };
            if nullable {
                self.push(Inst::Mark(register))?;
            }
            self.emit(node)?;
            let progress = if nullable {
                Some(self.push(Inst::End)?)
            } else {
                None
            };
            self.push(Inst::Jump(start))?;
            let exit = self.here();
            self.insts[start as usize] = split(start + 1, exit);
            if let Some(progress) = progress {
                self.insts[progress as usize] = Inst::Progress { register, exit };
            }
            return Ok(());
        };
        // What is left is `{0,1}`: the operand, or nothing.
        let _ = max;
        let optional = self.push(Inst::Split(0, 0))?;
        self.emit(node)?;
        let exit = self.here();
        self.insts[optional as usize] = split(optional + 1, exit);
        Ok(())
    }

    /// `node{min,max}` as a loop that counts its turns: the operand is
    /// compiled once, and the count lives in a register of the search.
    fn counted(
        &mut self,
        node: &Node,
        min: u32,
        max: Option<u32>,
        lazy: bool,
    ) -> Result<(), ExecError> {
        let before = self.insts.len();
        let (counter, mark) = (self.register()?, self.register()?);
        self.push(Inst::CountStart { counter })?;
        let head = self.push(Inst::End)?;
        self.push(Inst::CountEnter { mark })?;
        let body = self.insts.len();
        self.emit(node)?;
        if self.insts.len() == body {
            // Nothing to repeat: any number of turns of nothing is nothing.
            self.insts.truncate(before);
            return Ok(());
        }
        self.push(Inst::CountTurn { counter })?;
        self.push(Inst::Jump(head))?;
        self.insts[head as usize] = Inst::CountCheck {
            counter,
            mark,
            min,
            max: max.unwrap_or(u32::MAX),
            exit: self.here(),
            lazy,
        };
        Ok(())
    }
}

impl Program {
    pub(super) fn new(
        pattern: &str,
        case_insensitive: bool,
        multi_line: bool,
        dot_all: bool,
        line_endings: LineEndings,
    ) -> Result<Self, ExecError> {
        // Reading and compiling a pattern is work of the statement, and
        // stops when the statement does.
        let mut steps = InterruptCounter::checked();
        let word = finish_set(class_ranges("\\w")?, false, false);
        let mut parser = Parser {
            chars: pattern.chars().collect(),
            at: 0,
            groups: 0,
            names: Vec::new(),
            depth: 0,
            sets: Sets::default(),
            steps: &mut steps,
            literal: None,
            fold_budget: 256,
        };
        let mut flags = Flags {
            case_insensitive,
            multi_line,
            dot_all,
            extended: false,
        };
        let tree = parser.alternation(&mut flags)?;
        if parser.at != parser.chars.len() || parser.groups > MAX_GROUPS {
            return Err(SYNTAX);
        }
        let Parser {
            groups,
            names,
            sets,
            ..
        } = parser;
        let mut compiler = Compiler {
            insts: Vec::new(),
            runs: Vec::new(),
            registers: 0,
            groups,
            names,
            work: 0,
            steps: &mut steps,
        };
        compiler.emit(&Node::Group(Some(0), Box::new(tree)))?;
        compiler.push(Inst::End)?;
        Ok(Self {
            work: compiler.work,
            insts: compiler.insts,
            runs: compiler.runs,
            sets: sets.table,
            word,
            groups: compiler.groups + 1,
            registers: compiler.registers,
            names: compiler.names,
            line_endings,
        })
    }

    pub(super) fn group_index(&self, name: &str) -> Option<usize> {
        self.names
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, group)| *group)
    }

    /// Capture slots of the first match starting at or after byte `from`:
    /// a start and an end per group, `usize::MAX` for a group that took no
    /// part.
    pub(super) fn captures_at(
        &self,
        text: &str,
        from: usize,
        steps: &mut InterruptCounter,
    ) -> Result<Option<Box<[usize]>>, ExecError> {
        // Positions are held in thirty-two bits; no text the engine passes
        // here is longer.
        if u32::try_from(text.len()).map_or(true, |length| length == NONE) {
            return Err(SYNTAX);
        }
        let variables = 3 * self.groups + self.registers;
        let mut machine = Machine {
            program: self,
            text,
            from,
            vars: vec![NONE; variables],
            stamps: vec![0; variables],
            clock: 0,
            floor: 0,
            trail: Vec::new(),
            stack: Vec::new(),
            saves: 0,
            steps,
        };
        let mut start = from;
        loop {
            machine.vars.fill(NONE);
            machine.stamps.fill(0);
            machine.clock = 0;
            machine.floor = 0;
            machine.trail.clear();
            machine.stack.clear();
            if machine.run(0, start, None)?.is_some() {
                return Ok(Some(
                    machine.vars[..2 * self.groups]
                        .iter()
                        .map(|slot| {
                            if *slot == NONE {
                                usize::MAX
                            } else {
                                *slot as usize
                            }
                        })
                        .collect(),
                ));
            }
            let Some(character) = text[start..].chars().next() else {
                return Ok(None);
            };
            start += character.len_utf8();
        }
    }
}

/// A saved alternative: where to resume, and how much of the capture log
/// was written when it was saved.
#[derive(Clone, Copy)]
struct Frame {
    pc: u32,
    pos: u32,
    trail: u32,
    /// The clock when it was saved; what was assigned since need not be
    /// logged again.
    born: u32,
    /// For a loop taken whole: the characters one turn takes, and the
    /// position its fewest turns end at. Resuming gives one turn back and
    /// keeps the alternative while more can be given. Zero for a plain
    /// alternative, which resumes once.
    step: u32,
    least: u32,
}

/// One assignment to undo: the variable, and the value and stamp it had.
#[derive(Clone, Copy)]
struct Undo {
    index: u32,
    value: u32,
    stamp: u32,
}

struct Machine<'a, 'steps> {
    program: &'a Program,
    text: &'a str,
    from: usize,
    /// Two capture slots per group, then the position each open group
    /// started at, then the loop registers.
    vars: Vec<u32>,
    /// When each variable was last assigned, by the clock.
    stamps: Vec<u32>,
    /// Counts the alternatives saved and the nested searches begun.
    clock: u32,
    /// The clock when the innermost nested search began: it undoes what it
    /// assigned when it fails, as a saved alternative does.
    floor: u32,
    /// What assignments replaced, undone on backtracking. An assignment is
    /// logged only when an alternative saved since the last one could come
    /// back to the old value.
    trail: Vec<Undo>,
    stack: Vec<Frame>,
    saves: usize,
    steps: &'steps mut InterruptCounter,
}

/// Appends the case folding of `character`: several characters where its
/// folding is, its lower case otherwise.
fn fold_into(out: &mut String, character: char) {
    let folds = super::sql_regex::full_folds();
    match folds.binary_search_by_key(&character, |(folded, _)| *folded) {
        Ok(index) => out.push_str(&folds[index].1),
        Err(_) => out.extend(character.to_lowercase()),
    }
}

fn full_fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for character in text.chars() {
        fold_into(&mut folded, character);
    }
    folded
}

fn in_set(set: &[(char, char)], character: char) -> bool {
    set.binary_search_by(|(start, end)| {
        if character < *start {
            std::cmp::Ordering::Greater
        } else if character > *end {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    })
    .is_ok()
}

/// Appends to a vector that must not hold more than `ceiling` items, and
/// so must not reserve room for more either.
fn push_within<T>(items: &mut Vec<T>, item: T, ceiling: usize) -> Result<(), ExecError> {
    if items.len() >= ceiling {
        return Err(STACK);
    }
    if items.len() == items.capacity() {
        items.reserve_exact(items.len().clamp(64, ceiling - items.len()));
    }
    items.push(item);
    Ok(())
}

impl Machine<'_, '_> {
    fn assign(&mut self, index: usize, value: usize) -> Result<(), ExecError> {
        let value = u32::try_from(value).map_err(|_| SYNTAX)?;
        // Nothing saved since the variable was last assigned can come back
        // to the value it has now: only then is it left unlogged.
        let newest = self
            .stack
            .last()
            .map_or(0, |frame| frame.born)
            .max(self.floor);
        if self.stamps[index] < newest {
            push_within(
                &mut self.trail,
                Undo {
                    index: u32::try_from(index).map_err(|_| SYNTAX)?,
                    value: self.vars[index],
                    stamp: self.stamps[index],
                },
                MAX_TRAIL,
            )?;
        }
        self.vars[index] = value;
        self.stamps[index] = self.clock;
        Ok(())
    }

    fn var(&self, index: usize) -> Option<usize> {
        let value = self.vars[index];
        (value != NONE).then_some(value as usize)
    }

    fn undo(&mut self, mark: usize) {
        while self.trail.len() > mark {
            if let Some(undo) = self.trail.pop() {
                self.vars[undo.index as usize] = undo.value;
                self.stamps[undo.index as usize] = undo.stamp;
            }
        }
    }

    /// Saves an alternative to resume at `pc` and `pos`. `step` and
    /// `least` are those of a loop taken whole, zero otherwise.
    fn save(&mut self, pc: u32, pos: usize, step: u32, least: usize) -> Result<(), ExecError> {
        self.saves += 1;
        if self.saves > MAX_SAVES {
            return Err(TIMEOUT);
        }
        self.clock = self.clock.checked_add(1).ok_or(TIMEOUT)?;
        let position = |at: usize| u32::try_from(at).map_err(|_| SYNTAX);
        push_within(
            &mut self.stack,
            Frame {
                pc,
                pos: position(pos)?,
                trail: position(self.trail.len())?,
                born: self.clock,
                step,
                least: position(least)?,
            },
            MAX_STACK,
        )
    }

    /// Where the fixed run `seq` ends when it starts at `pos`, if every
    /// one of its sets finds its character.
    fn run_end(&self, seq: u32, len: u32, pos: usize) -> Option<usize> {
        let sets = &self.program.runs[seq as usize..(seq + len) as usize];
        let mut characters = self.text[pos..].chars();
        let mut end = pos;
        for set in sets {
            let character = characters.next()?;
            if !self.single(*set, character) {
                return None;
            }
            end += character.len_utf8();
        }
        Some(end)
    }

    fn line_ending(&self, character: char) -> bool {
        if self.program.line_endings == LineEndings::Unix {
            character == '\n'
        } else {
            matches!(character, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
        }
    }

    /// The width of the line ending at `at`, a CR LF pair counting once
    /// and the LF of one not counting on its own.
    fn line_end(&self, at: usize) -> Option<usize> {
        let suffix = &self.text[at..];
        if self.program.line_endings == LineEndings::Unix {
            return suffix.starts_with('\n').then_some(1);
        }
        if suffix.starts_with("\r\n") {
            return Some(2);
        }
        if suffix.starts_with('\n') && self.text[..at].ends_with('\r') {
            return None;
        }
        let first = suffix.chars().next()?;
        self.line_ending(first).then_some(first.len_utf8())
    }

    fn word_at(&self, character: Option<char>) -> bool {
        character.is_some_and(|character| in_set(&self.program.word, character))
    }

    fn holds(&self, assertion: Assertion, at: usize) -> bool {
        let text = self.text;
        match assertion {
            Assertion::TextStart => at == 0,
            Assertion::TextEnd => at == text.len(),
            Assertion::SearchStart => at == self.from,
            Assertion::LastLineEnd => {
                at == text.len()
                    || self
                        .line_end(at)
                        .is_some_and(|width| at + width == text.len())
            }
            Assertion::LineEnd => at == text.len() || self.line_end(at).is_some(),
            Assertion::LineStart => {
                if at == 0 {
                    return true;
                }
                if at == text.len() {
                    return false;
                }
                let before = &text[..at];
                before.ends_with('\n')
                    || (self.program.line_endings != LineEndings::Unix
                        && ((before.ends_with('\r') && !text[at..].starts_with('\n'))
                            || before.ends_with(['\u{85}', '\u{2028}', '\u{2029}'])))
            }
            Assertion::WordBoundary | Assertion::NotWordBoundary => {
                let before = self.word_at(text[..at].chars().next_back());
                let after = self.word_at(text[at..].chars().next());
                (before != after) == (assertion == Assertion::WordBoundary)
            }
        }
    }

    fn single(&self, set: u32, character: char) -> bool {
        match set {
            ANY => true,
            ANY_BUT_LINE_END => !self.line_ending(character),
            set => in_set(&self.program.sets[set as usize], character),
        }
    }

    /// The end of what group `group` captured, matched again at `pos`.
    fn repeated(&self, group: u32, case_insensitive: bool, pos: usize) -> Option<usize> {
        let (start, end) = (
            self.var(2 * group as usize)?,
            self.var(2 * group as usize + 1)?,
        );
        let captured = &self.text[start..end];
        let rest = &self.text[pos..];
        if !case_insensitive {
            return rest.starts_with(captured).then_some(pos + captured.len());
        }
        // Without regard to case the capture matches whatever folds to
        // the same characters, across a folding of more than one: a
        // captured `ss` is repeated by `ß`.
        let wanted = full_fold(captured);
        let mut folded = String::with_capacity(wanted.len());
        let mut length = 0;
        let mut subject = rest.chars();
        while folded.len() < wanted.len() {
            let actual = subject.next()?;
            fold_into(&mut folded, actual);
            length += actual.len_utf8();
        }
        (folded == wanted).then_some(pos + length)
    }

    /// Runs from `pc` at `pos` to the `End` that closes this stretch of the
    /// program, answering where it ended. With `target`, only an end at
    /// that position counts. Alternatives saved on the way are dropped once
    /// an end is reached: the caller takes this first answer or none. What
    /// a run that finds no end assigned is undone.
    fn run(
        &mut self,
        pc: u32,
        pos: usize,
        target: Option<usize>,
    ) -> Result<Option<usize>, ExecError> {
        let base = self.stack.len();
        let entry = self.trail.len();
        self.clock = self.clock.checked_add(1).ok_or(TIMEOUT)?;
        let floor = std::mem::replace(&mut self.floor, self.clock);
        let found = self.run_from(pc, pos, target, base);
        self.floor = floor;
        match found {
            Ok(Some(end)) => {
                self.stack.truncate(base);
                Ok(Some(end))
            }
            Ok(None) => {
                self.undo(entry);
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run_from(
        &mut self,
        pc: u32,
        pos: usize,
        target: Option<usize>,
        base: usize,
    ) -> Result<Option<usize>, ExecError> {
        let groups = self.program.groups;
        let (mut pc, mut pos) = (pc, pos);
        loop {
            self.steps.steps(1)?;
            let next = self.text[pos..].chars().next();
            let advanced = match self.program.insts[pc as usize] {
                Inst::Set(set) => match next {
                    Some(character) if in_set(&self.program.sets[set as usize], character) => {
                        pos += character.len_utf8();
                        pc += 1;
                        true
                    }
                    _ => false,
                },
                Inst::Any { dot_all } => match next {
                    Some(character) if dot_all || !self.line_ending(character) => {
                        pos += character.len_utf8();
                        pc += 1;
                        true
                    }
                    _ => false,
                },
                Inst::Assert(assertion) => {
                    pc += 1;
                    self.holds(assertion, pos)
                }
                Inst::Split(first, second) => {
                    self.save(second, pos, 0, 0)?;
                    pc = first;
                    true
                }
                Inst::Jump(to) => {
                    pc = to;
                    true
                }
                Inst::Open(group) => {
                    self.assign(2 * groups + group as usize, pos)?;
                    pc += 1;
                    true
                }
                Inst::Close(group) => {
                    let start = self.vars[2 * groups + group as usize] as usize;
                    self.assign(2 * group as usize, start)?;
                    self.assign(2 * group as usize + 1, pos)?;
                    pc += 1;
                    true
                }
                Inst::Backref {
                    group,
                    case_insensitive,
                } => match self.repeated(group, case_insensitive, pos) {
                    Some(end) => {
                        pos = end;
                        pc += 1;
                        true
                    }
                    None => false,
                },
                Inst::Mark(register) => {
                    self.assign(3 * groups + register as usize, pos)?;
                    pc += 1;
                    true
                }
                Inst::Progress { register, exit } => {
                    pc = if self.var(3 * groups + register as usize) == Some(pos) {
                        exit
                    } else {
                        pc + 1
                    };
                    true
                }
                Inst::CountStart { counter } => {
                    self.assign(3 * groups + counter as usize, 0)?;
                    pc += 1;
                    true
                }
                Inst::CountCheck {
                    counter,
                    mark,
                    min,
                    max,
                    exit,
                    lazy,
                } => {
                    let turns = self.vars[3 * groups + counter as usize];
                    // A turn that consumed nothing leaves every later turn
                    // nothing to consume either: the loop is over.
                    let stalled = turns > 0 && self.var(3 * groups + mark as usize) == Some(pos);
                    if stalled || turns >= max {
                        pc = exit;
                    } else if turns < min {
                        pc += 1;
                    } else if lazy {
                        self.save(pc + 1, pos, 0, 0)?;
                        pc = exit;
                    } else {
                        self.save(exit, pos, 0, 0)?;
                        pc += 1;
                    }
                    true
                }
                Inst::CountEnter { mark } => {
                    self.assign(3 * groups + mark as usize, pos)?;
                    pc += 1;
                    true
                }
                Inst::CountTurn { counter } => {
                    let turns = self.vars[3 * groups + counter as usize];
                    self.assign(
                        3 * groups + counter as usize,
                        turns.saturating_add(1) as usize,
                    )?;
                    pc += 1;
                    true
                }
                Inst::Run { seq, len, min, max } => {
                    let start = pos;
                    let mut turns = 0_u32;
                    // Where the fewest turns end: nothing before it is
                    // given back.
                    let mut least = (min == 0).then_some(pos);
                    while turns < max
                        && let Some(end) = self.run_end(seq, len, pos)
                    {
                        pos = end;
                        turns += 1;
                        if turns == min {
                            least = Some(pos);
                        }
                    }
                    self.steps.steps(pos - start)?;
                    pc += 1;
                    match least {
                        None => false,
                        Some(least) => {
                            if pos > least {
                                self.save(pc, pos, len, least)?;
                            }
                            true
                        }
                    }
                }
                Inst::Look {
                    behind,
                    negate,
                    min,
                    max,
                    after,
                } => {
                    let mark = self.trail.len();
                    let found = if behind {
                        let mut found = false;
                        let mut start = pos;
                        let mut back = 0_u32;
                        loop {
                            if back >= min && self.run(pc + 1, start, Some(pos))?.is_some() {
                                found = true;
                                break;
                            }
                            let Some(character) = self.text[..start].chars().next_back() else {
                                break;
                            };
                            if back >= max {
                                break;
                            }
                            start -= character.len_utf8();
                            back += 1;
                        }
                        found
                    } else {
                        self.run(pc + 1, pos, None)?.is_some()
                    };
                    if negate {
                        self.undo(mark);
                    }
                    pc = after;
                    found != negate
                }
                Inst::Atomic { after } => match self.run(pc + 1, pos, None)? {
                    Some(end) => {
                        pos = end;
                        pc = after;
                        true
                    }
                    None => false,
                },
                Inst::End => {
                    if target.is_none_or(|target| target == pos) {
                        return Ok(Some(pos));
                    }
                    false
                }
            };
            if advanced {
                continue;
            }
            if self.stack.len() <= base {
                return Ok(None);
            }
            let Some(frame) = self.stack.pop() else {
                return Ok(None);
            };
            self.undo(frame.trail as usize);
            pc = frame.pc;
            pos = frame.pos as usize;
            if frame.step > 0 {
                // One turn of the loop is given back, and the alternative
                // stays while another can be.
                for _ in 0..frame.step {
                    pos -= self.text[..pos]
                        .chars()
                        .next_back()
                        .map_or(0, char::len_utf8);
                }
                if pos > frame.least as usize {
                    self.stack.push(Frame {
                        pos: u32::try_from(pos).map_err(|_| SYNTAX)?,
                        ..frame
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExecError, LineEndings, Program, RegexError};
    use crate::execution::InterruptCounter;

    fn find(pattern: &str, text: &str) -> Option<(usize, usize)> {
        let program = Program::new(pattern, false, false, false, LineEndings::Unicode)
            .unwrap_or_else(|error| panic!("{pattern} compiles: {error}"));
        program
            .captures_at(text, 0, &mut InterruptCounter::unchecked())
            .expect("an unchecked search is never interrupted")
            .map(|slots| (slots[0], slots[1]))
    }

    #[test]
    fn back_references_match_what_the_group_captured() {
        assert_eq!(find(r"(abc)\1", "xabcabc"), Some((1, 7)));
        assert_eq!(find(r"(a)|\1", "x"), None);
        assert_eq!(find(r"^(a\1?){4}$", "aaaaaaaaaa"), Some((0, 10)));
        assert_eq!(find(r"^(a\1?){4}$", "aaaaaaaaa"), None);
        assert_eq!(find(r"(?i)(ab)\d\1", "Ab4aB"), Some((0, 5)));
        assert_eq!(find(r"(?<word>o+)x\k<word>", "fooxoo"), Some((1, 6)));
        assert_eq!(find(r"((\3|b)\2(a)x)+", "aaxabxbaxbbx"), None);
        assert_eq!(find(r"((\3|b)\2(a)x)+", "aaaxabaxbaaxbbax"), Some((12, 16)));
    }

    #[test]
    fn assertions_look_without_consuming() {
        assert_eq!(find(r"a(?!b).", "abad"), Some((2, 4)));
        assert_eq!(find(r"a(?=c|d).", "abad"), Some((2, 4)));
        assert_eq!(find(r"(?<=a)b", "cb ab"), Some((4, 5)));
        assert_eq!(find(r"(?<![cd])[ab]", "dbcb"), None);
        assert_eq!(find(r"(?<!cd)[ab]", "cdaccb"), Some((5, 6)));
        assert_eq!(find(r"^(?:b|a(?=(.)))*\1", "abc"), Some((0, 2)));
        assert_eq!(find(r"$(?<=^(a))", "a"), Some((1, 1)));
    }

    /// The same folding rule as the automaton's rewritten patterns: a run
    /// of literals matches across a folding of more than one character,
    /// and nothing else does.
    #[test]
    fn literals_and_back_references_fold_across_several_characters() {
        let matches = |pattern: &str, text: &str| {
            Program::new(pattern, true, false, false, LineEndings::Unicode)
                .unwrap_or_else(|error| panic!("{pattern} compiles: {error}"))
                .captures_at(text, 0, &mut InterruptCounter::unchecked())
                .expect("an unchecked search is never interrupted")
                .is_some()
        };
        assert!(matches("(?=s)straße", "STRASSE"));
        assert!(matches("(?=s)strasse", "Straße"));
        assert!(matches("stra(?!x)sse", "Straße"));
        assert!(matches(r"(ss)\1", "ssß"));
        assert!(matches(r"(ß)\1", "ßss"));
        assert!(matches("(?=.)ß+s", "sss"));
        assert!(!matches("(?=.)s{2}", "ß"));
        assert!(!matches("(?=.)[s]s", "ß"));
        assert!(!matches("(?=.)[ß]", "ss"));
        assert_eq!(find("(?i)(?!x)ss", "xß"), Some((1, 3)));
    }

    #[test]
    fn atomic_groups_keep_their_first_match() {
        assert_eq!(find(r"a(?>bc|b)c", "abcc"), Some((0, 4)));
        assert_eq!(find(r"a(?>bc|b)c", "abc"), None);
        assert_eq!(find(r"(?>a+)b", "aaab"), Some((0, 4)));
        assert_eq!(find(r"a++a", "aaa"), None);
        assert_eq!(find(r"a*+b", "aaab"), Some((0, 4)));
    }

    #[test]
    fn comments_and_sets_read_as_written() {
        assert_eq!(find(r"^a(?#xxx){3}c", "aaac"), Some((0, 4)));
        assert_eq!(find(r"(?x)^a (?#xxx) (?#yyy) {3}c", "aaac"), Some((0, 4)));
        assert_eq!(find(r"([\d-z]+)", "a0-za"), Some((1, 4)));
        assert_eq!(find(r"([\d-\s]+)", "a0- z"), Some((1, 4)));
        assert_eq!(find(r"\GX.*X", "aaaXbX"), None);
        assert_eq!(find(r"(a*)*b\1", "aab"), Some((0, 3)));
    }

    /// A pattern whose backtracking does not end stops with a named error
    /// at the machine's own ceiling, and sooner when its statement is
    /// cancelled or past its deadline: the search asks as it counts steps.
    #[test]
    fn runaway_backtracking_stops_with_a_named_error_or_its_statement() {
        let program =
            Program::new(r"(a+)+\1b", false, false, false, LineEndings::Unicode).expect("compiles");
        let text = "a".repeat(40);
        assert_eq!(
            program.captures_at(&text, 0, &mut InterruptCounter::unchecked()),
            Err(ExecError::Regex(RegexError::Timeout))
        );
        let cancelled = crate::ExecutionCancellation::new();
        cancelled.cancel();
        crate::with_execution_cancellation(cancelled, || {
            assert_eq!(
                program.captures_at(&text, 0, &mut InterruptCounter::checked()),
                Err(ExecError::QueryCancelled)
            );
        });
        let late = crate::ExecutionCancellation::new();
        late.limit_to(std::time::Instant::now());
        crate::with_execution_cancellation(late, || {
            assert_eq!(
                program.captures_at(&text, 0, &mut InterruptCounter::checked()),
                Err(ExecError::QueryTimedOut)
            );
        });
    }

    /// A repetition that saves an alternative for every turn stops at the
    /// stack ceiling the workspace charge is sized by.
    #[test]
    fn a_stack_past_its_ceiling_is_a_named_error() {
        let program = Program::new(r"(?:(a)|b)*\1c", false, false, false, LineEndings::Unicode)
            .expect("compiles");
        let text = "b".repeat(3 * super::MAX_STACK);
        assert_eq!(
            program.captures_at(&text, 0, &mut InterruptCounter::unchecked()),
            Err(ExecError::Regex(RegexError::StackOverflow))
        );
    }

    /// A loop that takes a character, or a fixed run of them, each turn
    /// holds one saved alternative however long the text: a match that
    /// only reads forward is never short of stack.
    #[test]
    fn a_loop_over_a_fixed_run_holds_one_alternative() {
        for (pattern, unit, insensitive) in [
            ("(?=a)a+", "a", true),
            ("(?=a)a+", "a", false),
            ("(?=a)(?:a)+$", "a", true),
            ("(?=a)(?:a|b)+$", "ab", true),
            ("(?=a)(?:ab)+$", "ab", true),
            ("(?=a)(?:(?:a[b-c]){2})*$", "abac", false),
            ("(?=a)[a-c]{3,}$", "abc", false),
            ("(?=a).*c$", "abc", false),
            ("(?=a)(?:..c)++$", "abc", false),
            ("(?=a)a{2,2000000}$", "a", true),
        ] {
            let program = Program::new(pattern, insensitive, false, false, LineEndings::Unicode)
                .unwrap_or_else(|error| panic!("{pattern}: {error}"));
            assert!(
                program
                    .insts
                    .iter()
                    .any(|inst| matches!(inst, super::Inst::Run { .. })),
                "{pattern} compiles to a loop taken whole"
            );
            for length in [5_000, 100_000, 1_000_000] {
                let text = unit.repeat(length / unit.len());
                let found = program
                    .captures_at(&text, 0, &mut InterruptCounter::unchecked())
                    .unwrap_or_else(|error| panic!("{pattern} over {length}: {error}"));
                assert_eq!(
                    found.map(|slots| (slots[0], slots[1])),
                    Some((0, text.len())),
                    "{pattern} over {length}"
                );
            }
        }
    }

    /// Giving a loop back turn by turn finds what a loop that saved every
    /// turn found, captures included.
    #[test]
    fn a_loop_taken_whole_gives_its_turns_back() {
        assert_eq!(find("(?=a)a+ab", "aaaab"), Some((0, 5)));
        assert_eq!(find("(?=a)(?:ab)+abc", "abababc"), Some((0, 7)));
        assert_eq!(find("(?=a)(?:ab){2,}ab$", "abab"), None);
        assert_eq!(find("(?=a)(?:ab){2,}ab$", "ababab"), Some((0, 6)));
        assert_eq!(find("(?=a)a{2,3}a", "aaa"), Some((0, 3)));
        assert_eq!(find("(?=a)a{2,3}a", "aa"), None);
        assert_eq!(find("(?=a)a{2,3}+a", "aaa"), None);
        assert_eq!(find("(?=x)(x*)(x*)y", "xxxy"), Some((0, 4)));
        assert_eq!(find(r"(?=a)(a+)b\1", "aabaa"), Some((0, 5)));
        assert_eq!(find(r"(?=a)(a+)\1$", "aaaa"), Some((0, 4)));
        assert_eq!(find(r"(?!(a)b)a\1", "aca"), None);
        assert_eq!(find(r"(?:(?=(a))a|b)\1", "ba"), None);
        assert_eq!(find("(?=é)(?:é.)+é$", "éaébé"), Some((0, 8)));
    }

    #[test]
    fn a_pattern_past_the_compiled_ceilings_is_refused() {
        let refused = |pattern: &str| {
            matches!(
                Program::new(pattern, false, false, false, LineEndings::Unicode),
                Err(ExecError::Regex(RegexError::IllegalArgument))
            )
        };
        assert!(refused(&format!(
            "(?=a){}",
            "(a)".repeat(super::MAX_GROUPS + 1)
        )));
        // Distinct large sets, each kept once.
        let mut sets = String::new();
        for index in 0..200 {
            use std::fmt::Write as _;
            write!(sets, r"[\p{{L}}\x{{{:x}}}]", 0x2190 + index).expect("writes to a string");
        }
        assert!(refused(&format!("(?=a){sets}")));
        // The same large set written often is one set.
        assert!(!refused(&format!("(?=a){}", r"\w".repeat(5_000))));
    }

    /// Compiling costs what the pattern's text costs, whatever counts it
    /// names: a count is a loop around one copy of its operand, and a
    /// count `MySQL` refuses is refused before anything is compiled.
    #[test]
    fn compiling_a_count_does_not_run_it() {
        let compile =
            |pattern: &str| Program::new(pattern, false, false, false, LineEndings::Unicode);
        for pattern in [
            "(?=a)(?:(?:){2000000000}){2000000000}",
            "(?=a)a{16777216}",
            "(?=a)a{0,2000000000}",
            "(?=a)(?:b|(?:a{1,99999999999999999999})*)",
        ] {
            assert!(
                matches!(
                    compile(pattern),
                    Err(ExecError::Regex(RegexError::NumberTooBig))
                ),
                "{pattern}"
            );
        }
        for (pattern, text, matches) in [
            ("(?=a)(?:(?:){16777215}){16777215}", "a", true),
            ("(?=a)(?:(?:a?){16777215}){16777215}b", "ab", true),
            (
                "(?=a)(?:(?:(?:(?:a{1000}){1000}){1000}){1000}){1000}",
                "a",
                false,
            ),
            ("(?=a)(?:a{3,5}){2,}$", "aaaaaaa", true),
            ("(?=a)(?:a{3,5}){2,}$", "aaaaa", false),
            ("(?=a)a{1,100000}", "a", true),
            ("(?=a)(?:a|b){0,50000}?c", "abbac", true),
            ("(?=a)(a\\1?){4}$", "aaaaaaaaaa", true),
            ("(?=a)(?:ab){2}+a", "ababa", true),
            ("(?=a)(?:ab){2,3}+ab", "ababab", false),
        ] {
            let program = compile(pattern).unwrap_or_else(|error| panic!("{pattern}: {error}"));
            assert!(
                program.work <= 64 && program.insts.len() <= 64,
                "{pattern} visited {} operands for {} instructions",
                program.work,
                program.insts.len()
            );
            let found = program
                .captures_at(text, 0, &mut InterruptCounter::unchecked())
                .unwrap_or_else(|error| panic!("{pattern} over {text}: {error}"));
            assert_eq!(found.is_some(), matches, "{pattern} over {text}");
        }
    }

    /// A statement that is cancelled or past its deadline stops while its
    /// pattern is still being read and compiled.
    #[test]
    fn an_interrupted_statement_stops_inside_compilation() {
        let pattern = format!("(?=a){}", "a".repeat(10_000));
        let compile =
            || Program::new(&pattern, false, false, false, LineEndings::Unicode).map(|_| ());
        assert_eq!(compile(), Ok(()));
        let cancelled = crate::ExecutionCancellation::new();
        cancelled.cancel();
        crate::with_execution_cancellation(cancelled, || {
            assert_eq!(compile(), Err(ExecError::QueryCancelled));
        });
        let late = crate::ExecutionCancellation::new();
        late.limit_to(std::time::Instant::now());
        crate::with_execution_cancellation(late, || {
            assert_eq!(compile(), Err(ExecError::QueryTimedOut));
        });
    }
}

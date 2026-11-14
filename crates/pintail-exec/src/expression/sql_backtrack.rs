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
/// The most saved alternatives the stack may hold at once. Every program
/// is charged this much of its query's memory before it runs, so the
/// ceiling is what a query of many patterns can afford; a run of single
/// characters takes one entry however long it is.
const MAX_STACK: usize = 4_096;
/// The most alternatives one search may save in total.
const MAX_SAVES: usize = 4_000_000;
/// The longest text a look-behind may need to read back over.
const MAX_LOOK_BEHIND: usize = 1_000;

/// Bytes one search can hold at its ceilings: the stack and the capture
/// log, each at twice its ceiling for the room a growing vector takes, and
/// the capture slots and loop registers. A statement is charged this for
/// every program before it runs one.
pub(in crate::expression) const WORKSPACE: usize =
    2 * MAX_STACK * (size_of::<Frame>() + 2 * size_of::<(u32, usize)>())
        + (3 * (MAX_GROUPS + 1) + MAX_PROGRAM) * size_of::<usize>();

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
    /// One character out of a set, as sorted ranges.
    Set(Box<[(char, char)]>),
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

struct Parser {
    chars: Vec<char>,
    at: usize,
    groups: usize,
    names: Vec<(String, usize)>,
    depth: usize,
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

impl Parser {
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
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alternate(branches)
        })
    }

    fn concatenation(&mut self, flags: &mut Flags) -> Result<Node, ExecError> {
        let mut items = Vec::new();
        loop {
            self.skip_trivia(*flags)?;
            if matches!(self.peek(), None | Some('|' | ')')) {
                break;
            }
            let Some(atom) = self.atom(flags)? else {
                continue;
            };
            self.skip_trivia(*flags)?;
            items.push(self.quantified(atom)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    fn quantified(&mut self, atom: Node) -> Result<Node, ExecError> {
        let saved = self.at;
        let (min, max) = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') => {
                self.at += 1;
                let Some(bounds) = self.bounds() else {
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
    fn bounds(&mut self) -> Option<(u32, Option<u32>)> {
        let number = |parser: &mut Self| {
            let start = parser.at;
            let mut value = 0_u32;
            while let Some(digit) = parser.peek().and_then(|character| character.to_digit(10)) {
                value = value.checked_mul(10)?.checked_add(digit)?;
                parser.at += 1;
            }
            (parser.at > start).then_some(value)
        };
        let min = number(self)?;
        let max = if self.eat(',') {
            if self.peek() == Some('}') {
                None
            } else {
                Some(number(self)?)
            }
        } else {
            Some(min)
        };
        self.eat('}').then_some((min, max))
    }

    /// One operand. `None` when what was read only changed the flags.
    fn atom(&mut self, flags: &mut Flags) -> Result<Option<Node>, ExecError> {
        let Some(character) = self.peek() else {
            return Err(SYNTAX);
        };
        self.at += 1;
        Ok(Some(match character {
            '(' => return self.group(flags),
            '[' => Node::Set(self.class(*flags)?),
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
            literal => Node::Set(finish_set(
                vec![(literal, literal)],
                flags.case_insensitive,
                false,
            )),
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
                Item::Ranges(class_ranges(&format!("\\{escape}"))?)
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
                Item::Ranges(class_ranges(&fragment)?)
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
                finish_set(class_ranges(r"[\t\p{Zs}]")?, false, escape == 'H').into_vec(),
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
                    quoted.push(Node::Set(finish_set(
                        vec![(literal, literal)],
                        flags.case_insensitive,
                        false,
                    )));
                }
                if self.looking_at("\\E") {
                    self.at += 2;
                }
                Node::Group(None, Box::new(Node::Concat(quoted)))
            }
            'E' => Node::Empty,
            other => match self.escaped_item(other)? {
                Item::Char(literal) => Node::Set(finish_set(
                    vec![(literal, literal)],
                    flags.case_insensitive,
                    false,
                )),
                Item::Ranges(ranges) => {
                    Node::Set(finish_set(ranges, flags.case_insensitive, false))
                }
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
                    Item::Ranges(class_ranges(&fragment)?)
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
        Ok(finish_set(ranges, flags.case_insensitive, negate))
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
    /// As many characters of set `.0` as there are, giving them back one at
    /// a time; `u32::MAX` stands for any character but a line ending and
    /// `u32::MAX - 1` for any character.
    Star(u32),
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

pub(super) struct Program {
    insts: Vec<Inst>,
    sets: Vec<Box<[(char, char)]>>,
    word: Box<[(char, char)]>,
    groups: usize,
    registers: usize,
    names: Vec<(String, usize)>,
    line_endings: LineEndings,
}

struct Compiler {
    insts: Vec<Inst>,
    sets: Vec<Box<[(char, char)]>>,
    set_ranges: usize,
    registers: usize,
    groups: usize,
    names: Vec<(String, usize)>,
}

impl Compiler {
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

    fn set(&mut self, ranges: &[(char, char)]) -> Result<u32, ExecError> {
        if let Some(index) = self.sets.iter().position(|set| **set == *ranges) {
            return u32::try_from(index).map_err(|_| SYNTAX);
        }
        self.set_ranges += ranges.len();
        if self.set_ranges > MAX_SET_RANGES {
            return Err(SYNTAX);
        }
        self.sets.push(ranges.into());
        u32::try_from(self.sets.len() - 1).map_err(|_| SYNTAX)
    }

    fn group(&self, group: usize) -> Result<u32, ExecError> {
        if group == 0 || group > self.groups {
            return Err(BACK_REFERENCE);
        }
        u32::try_from(group).map_err(|_| BACK_REFERENCE)
    }

    #[allow(clippy::too_many_lines)]
    fn emit(&mut self, node: &Node) -> Result<(), ExecError> {
        match node {
            Node::Empty => {}
            Node::Set(ranges) => {
                let set = self.set(ranges)?;
                self.push(Inst::Set(set))?;
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
        for _ in 0..min {
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
            // One character at a time needs no saved alternative per
            // character: the run is taken whole and given back from its end.
            if greed == Greed::Greedy {
                let single = match node {
                    Node::Set(ranges) => Some(self.set(ranges)?),
                    Node::Any { dot_all: true } => Some(ANY),
                    Node::Any { dot_all: false } => Some(ANY_BUT_LINE_END),
                    _ => None,
                };
                if let Some(single) = single {
                    self.push(Inst::Star(single))?;
                    return Ok(());
                }
            }
            let start = self.push(Inst::Split(0, 0))?;
            let nullable = node.nullable();
            let register = u32::try_from(self.registers).map_err(|_| SYNTAX)?;
            if nullable {
                self.registers += 1;
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
        let mut splits = Vec::new();
        for _ in min..max {
            splits.push(self.push(Inst::Split(0, 0))?);
            self.emit(node)?;
        }
        let exit = self.here();
        for at in splits {
            self.insts[at as usize] = split(at + 1, exit);
        }
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
        let word = finish_set(class_ranges("\\w")?, false, false);
        let mut parser = Parser {
            chars: pattern.chars().collect(),
            at: 0,
            groups: 0,
            names: Vec::new(),
            depth: 0,
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
        let mut compiler = Compiler {
            insts: Vec::new(),
            sets: Vec::new(),
            set_ranges: 0,
            registers: 0,
            groups: parser.groups,
            names: parser.names,
        };
        compiler.emit(&Node::Group(Some(0), Box::new(tree)))?;
        compiler.push(Inst::End)?;
        Ok(Self {
            insts: compiler.insts,
            sets: compiler.sets,
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
        let mut machine = Machine {
            program: self,
            text,
            from,
            vars: vec![usize::MAX; 3 * self.groups + self.registers],
            trail: Vec::new(),
            stack: Vec::new(),
            saves: 0,
            steps,
        };
        let mut start = from;
        loop {
            machine.vars.fill(usize::MAX);
            machine.trail.clear();
            machine.stack.clear();
            if machine.run(0, start, None)?.is_some() {
                machine.vars.truncate(2 * self.groups);
                return Ok(Some(machine.vars.into_boxed_slice()));
            }
            let Some(character) = text[start..].chars().next() else {
                return Ok(None);
            };
            start += character.len_utf8();
        }
    }
}

#[derive(Clone, Copy)]
enum Frame {
    /// Resume at `pc` and `pos`.
    Retry { pc: u32, pos: usize, trail: usize },
    /// Resume at `pc` one character before `pos`, down to `min`.
    Shrink {
        pc: u32,
        min: usize,
        pos: usize,
        trail: usize,
    },
}

struct Machine<'a, 'steps> {
    program: &'a Program,
    text: &'a str,
    from: usize,
    /// Two capture slots per group, then the position each open group
    /// started at, then the loop registers.
    vars: Vec<usize>,
    /// What each assignment to `vars` replaced, undone on backtracking.
    trail: Vec<(u32, usize)>,
    stack: Vec<Frame>,
    saves: usize,
    steps: &'steps mut InterruptCounter,
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

impl Machine<'_, '_> {
    fn assign(&mut self, index: usize, value: usize) {
        self.trail
            .push((u32::try_from(index).unwrap_or(u32::MAX), self.vars[index]));
        self.vars[index] = value;
    }

    fn undo(&mut self, mark: usize) {
        while self.trail.len() > mark {
            if let Some((index, value)) = self.trail.pop() {
                self.vars[index as usize] = value;
            }
        }
    }

    fn save(&mut self, frame: Frame) -> Result<(), ExecError> {
        self.saves += 1;
        if self.saves > MAX_SAVES {
            return Err(TIMEOUT);
        }
        if self.stack.len() >= MAX_STACK || self.trail.len() >= 2 * MAX_STACK {
            return Err(STACK);
        }
        self.stack.push(frame);
        Ok(())
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
            self.vars[2 * group as usize],
            self.vars[2 * group as usize + 1],
        );
        if start == usize::MAX || end == usize::MAX {
            return None;
        }
        let captured = &self.text[start..end];
        let rest = &self.text[pos..];
        if !case_insensitive {
            return rest.starts_with(captured).then_some(pos + captured.len());
        }
        let mut subject = rest.chars();
        let mut length = 0;
        for expected in captured.chars() {
            let actual = subject.next()?;
            if actual != expected && !actual.to_lowercase().eq(expected.to_lowercase()) {
                return None;
            }
            length += actual.len_utf8();
        }
        Some(pos + length)
    }

    /// Runs from `pc` at `pos` to the `End` that closes this stretch of the
    /// program, answering where it ended. With `target`, only an end at
    /// that position counts. Alternatives saved on the way are dropped once
    /// an end is reached: the caller takes this first answer or none.
    #[allow(clippy::too_many_lines)]
    fn run(
        &mut self,
        pc: u32,
        pos: usize,
        target: Option<usize>,
    ) -> Result<Option<usize>, ExecError> {
        let base = self.stack.len();
        let entry = self.trail.len();
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
                    self.save(Frame::Retry {
                        pc: second,
                        pos,
                        trail: self.trail.len(),
                    })?;
                    pc = first;
                    true
                }
                Inst::Jump(to) => {
                    pc = to;
                    true
                }
                Inst::Open(group) => {
                    self.assign(2 * groups + group as usize, pos);
                    pc += 1;
                    true
                }
                Inst::Close(group) => {
                    let start = self.vars[2 * groups + group as usize];
                    self.assign(2 * group as usize, start);
                    self.assign(2 * group as usize + 1, pos);
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
                    self.assign(3 * groups + register as usize, pos);
                    pc += 1;
                    true
                }
                Inst::Progress { register, exit } => {
                    pc = if self.vars[3 * groups + register as usize] == pos {
                        exit
                    } else {
                        pc + 1
                    };
                    true
                }
                Inst::Star(set) => {
                    let start = pos;
                    for character in self.text[start..].chars() {
                        if !self.single(set, character) {
                            break;
                        }
                        pos += character.len_utf8();
                    }
                    self.steps.steps(pos - start)?;
                    pc += 1;
                    if pos > start {
                        self.save(Frame::Shrink {
                            pc,
                            min: start,
                            pos,
                            trail: self.trail.len(),
                        })?;
                    }
                    true
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
                        self.stack.truncate(base);
                        return Ok(Some(pos));
                    }
                    false
                }
            };
            if advanced {
                continue;
            }
            if self.stack.len() <= base {
                self.undo(entry);
                return Ok(None);
            }
            match self.stack.pop() {
                Some(Frame::Retry {
                    pc: saved,
                    pos: at,
                    trail,
                }) => {
                    self.undo(trail);
                    pc = saved;
                    pos = at;
                }
                Some(Frame::Shrink {
                    pc: saved,
                    min,
                    pos: at,
                    trail,
                }) => {
                    self.undo(trail);
                    let width = self.text[..at]
                        .chars()
                        .next_back()
                        .map_or(0, char::len_utf8);
                    let at = at - width;
                    if at > min {
                        self.stack.push(Frame::Shrink {
                            pc: saved,
                            min,
                            pos: at,
                            trail,
                        });
                    }
                    pc = saved;
                    pos = at;
                }
                None => return Ok(None),
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

    #[test]
    fn a_pattern_past_the_compiled_ceilings_is_refused() {
        let refused = |pattern: &str| {
            matches!(
                Program::new(pattern, false, false, false, LineEndings::Unicode),
                Err(ExecError::Regex(RegexError::IllegalArgument))
            )
        };
        // Counted repetition copies its operand.
        assert!(refused(r"(?=a)(?:abcdefgh){5000}"));
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
        assert!(!refused(r"(?=a)(?:abcdefgh){50}"));
    }
}

//! Respell FROM-clause groupings the grammar does not read, before parsing.
//!
//! - A parenthesized list, `a LEFT JOIN (b, c) ON ...`, is the cross join
//!   of its members: `(b CROSS JOIN c)`. A member that is itself a chain of
//!   joins keeps its own grouping.
//! - Parentheses around one table, `FROM (t AS x)`, group nothing and go.
//! - A join whose condition is written after a later join's,
//!   `a LEFT JOIN b LEFT JOIN c ON p ON q`, has everything up to its own
//!   condition as its right side: `a LEFT JOIN (b LEFT JOIN c ON p) ON q`.
//!   Each `ON` or `USING` closes the nearest join still waiting for one.
//!
//! Only the FROM clause of a SELECT is read; every other parenthesis and
//! keyword is passed over.
use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::call_forms::{Edits, is_word};
use crate::interval::{significant, word_token};

/// A join still waiting for its condition.
struct Waiting {
    /// The join's number among the joins of its clause.
    number: usize,
    /// Token position where its right side starts.
    right: usize,
}

/// The joins of one FROM clause, or of one member of a parenthesized list.
#[derive(Default)]
struct Chain {
    waiting: Vec<Waiting>,
    joins: usize,
    /// A table reference is expected next.
    expects_table: bool,
}

/// A parenthesized group of table references.
struct Group {
    open: usize,
    commas: Vec<usize>,
    /// Token position where the current member starts.
    member: usize,
    /// (first token, token after the last) of each earlier member that is
    /// a chain of joins, the first member excepted.
    chains: Vec<(usize, usize)>,
    /// Whether any member so far joins.
    joined: bool,
}

enum Frame {
    /// A statement, a subquery or an expression in parentheses.
    Other {
        select: bool,
        from: Option<Chain>,
    },
    Tables(Group, Chain),
}

impl Frame {
    fn chain(&mut self) -> Option<&mut Chain> {
        match self {
            Self::Other { from, .. } => from.as_mut(),
            Self::Tables(_, chain) => Some(chain),
        }
    }
}

pub(crate) fn rewrite(tokens: &mut Vec<TokenWithSpan>) {
    if !tokens.iter().any(|token| is_word(&token.token, "FROM")) {
        return;
    }
    let significant = significant(tokens);
    let edits = groupings(tokens, &significant);
    if !edits.is_empty() {
        edits.apply(tokens);
    }
}

/// What is known about the run of opening parentheses ending at `end`:
/// how many of them, counted outward from the innermost, were asked about,
/// and how many of those hold a query (`None` when the innermost does not
/// start one).
#[derive(Default)]
struct QueryRun {
    start: usize,
    end: usize,
    asked: usize,
    held: Option<usize>,
}

/// Whether the parentheses opening at `open` hold a query rather than
/// table references: a query starts with its keyword, or with a
/// parenthesized query that the rest of the group continues.
///
/// A run of opening parentheses is settled in one pass from its innermost
/// outward and remembered, so a statement that nests them is read once and
/// no call stands on another.
fn holds_query(
    tokens: &[TokenWithSpan],
    significant: &[usize],
    open: usize,
    run: &mut QueryRun,
) -> bool {
    let token = |position: usize| significant.get(position).map(|&index| &tokens[index].token);
    let end = if (run.start..run.end).contains(&open) {
        run.end
    } else {
        let mut end = open + 1;
        while token(end) == Some(&Token::LParen) {
            end += 1;
        }
        crate::count_preparse_work(end - open);
        end
    };
    // The innermost parenthesis is level 0; `open` is this many outside it.
    let level = end - 1 - open;
    if run.end != end || run.asked < level {
        let starts_query = matches!(
            token(end),
            Some(Token::Word(word)) if word.quote_style.is_none()
                && ["SELECT", "WITH", "VALUES", "TABLE"]
                    .iter()
                    .any(|keyword| word.value.eq_ignore_ascii_case(keyword))
        );
        let mut held = 0_usize;
        if starts_query && level > 0 {
            // Each level holds a query when the one inside it does and is
            // followed by its closing parenthesis or by more of a query.
            let mut depth = 1_usize;
            let mut at = end;
            while let Some(inner) = token(at) {
                match inner {
                    Token::LParen => depth += 1,
                    Token::RParen => {
                        depth -= 1;
                        if depth == 0 {
                            let continues = token(at + 1).is_some_and(|next| {
                                next == &Token::RParen
                                    || ["UNION", "EXCEPT", "INTERSECT", "ORDER", "LIMIT"]
                                        .iter()
                                        .any(|keyword| is_word(next, keyword))
                            });
                            if !continues {
                                break;
                            }
                            held += 1;
                            if held >= level {
                                break;
                            }
                            depth = 1;
                        }
                    }
                    _ => {}
                }
                at += 1;
            }
            crate::count_preparse_work(at - end);
        }
        *run = QueryRun {
            start: open,
            end,
            asked: level,
            held: starts_query.then_some(held),
        };
    }
    run.held.is_some_and(|held| level <= held)
}

#[allow(clippy::too_many_lines)] // one pass over the tokens, one arm per token kind
fn groupings(tokens: &[TokenWithSpan], significant: &[usize]) -> Edits {
    const CLAUSE_ENDS: [&str; 12] = [
        "WHERE",
        "GROUP",
        "HAVING",
        "ORDER",
        "LIMIT",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "WINDOW",
        "INTO",
        "PROCEDURE",
        "LOCK",
    ];
    let mut edits = Edits::default();
    let mut run = QueryRun::default();
    let mut frames = vec![Frame::Other {
        select: false,
        from: None,
    }];
    for (at, &index) in significant.iter().enumerate() {
        let token = &tokens[index].token;
        let previous = at.checked_sub(1).map(|at| &tokens[significant[at]].token);
        let next = significant.get(at + 1).map(|&index| &tokens[index].token);
        let frame = frames
            .last_mut()
            .expect("the statement frame is never popped");
        let expects_table = frame
            .chain()
            .is_some_and(|chain| std::mem::take(&mut chain.expects_table));
        match token {
            Token::LParen => {
                if expects_table && !holds_query(tokens, significant, at, &mut run) {
                    let member = significant.get(at + 1).copied().unwrap_or(index);
                    frames.push(Frame::Tables(
                        Group {
                            open: index,
                            commas: Vec::new(),
                            member,
                            chains: Vec::new(),
                            joined: false,
                        },
                        Chain {
                            expects_table: true,
                            ..Chain::default()
                        },
                    ));
                } else {
                    frames.push(Frame::Other {
                        select: false,
                        from: None,
                    });
                }
            }
            Token::RParen => {
                if frames.len() == 1 {
                    // Unbalanced: leave the statement to the grammar.
                    return Edits::default();
                }
                if let Some(Frame::Tables(mut group, chain)) = frames.pop() {
                    if chain.joins > 0 && !group.commas.is_empty() {
                        group.chains.push((group.member, index));
                    }
                    group.joined |= chain.joins > 0;
                    if group.commas.is_empty() {
                        if !group.joined {
                            edits.remove(group.open);
                            edits.remove(index);
                        }
                    } else {
                        for comma in group.commas {
                            edits.replace(comma, [word_token("CROSS"), word_token("JOIN")]);
                        }
                        for (first, end) in group.chains {
                            edits.before(first, [Token::LParen]);
                            edits.before(end, [Token::RParen]);
                        }
                    }
                }
            }
            Token::SemiColon => {
                frames.truncate(1);
                frames[0] = Frame::Other {
                    select: false,
                    from: None,
                };
            }
            Token::Comma => match frame {
                Frame::Other {
                    from: Some(chain), ..
                } => {
                    *chain = Chain {
                        expects_table: true,
                        ..Chain::default()
                    };
                }
                Frame::Tables(group, chain) => {
                    if chain.joins > 0 && !group.commas.is_empty() {
                        group.chains.push((group.member, index));
                    }
                    group.joined |= chain.joins > 0;
                    group.commas.push(index);
                    group.member = significant.get(at + 1).copied().unwrap_or(index);
                    *chain = Chain {
                        expects_table: true,
                        ..Chain::default()
                    };
                }
                Frame::Other { from: None, .. } => {}
            },
            Token::Word(word) if word.quote_style.is_none() => {
                let keyword = word.value.to_ascii_uppercase();
                if let Frame::Other { select, from } = frame {
                    match keyword.as_str() {
                        "SELECT" => {
                            *select = true;
                            *from = None;
                            continue;
                        }
                        "FROM" if *select && from.is_none() => {
                            *from = Some(Chain {
                                expects_table: true,
                                ..Chain::default()
                            });
                            continue;
                        }
                        ended if CLAUSE_ENDS.contains(&ended) => {
                            *from = None;
                            continue;
                        }
                        _ => {}
                    }
                }
                let Some(chain) = frame.chain() else {
                    continue;
                };
                match keyword.as_str() {
                    // `USE INDEX FOR JOIN (...)` names what an index is for.
                    "JOIN" | "STRAIGHT_JOIN"
                        if !previous.is_some_and(|previous| is_word(previous, "FOR")) =>
                    {
                        chain.joins += 1;
                        chain.expects_table = true;
                        let mut back = at;
                        let natural = loop {
                            let Some(before) = back.checked_sub(1) else {
                                break false;
                            };
                            back = before;
                            let word = &tokens[significant[back]].token;
                            if is_word(word, "NATURAL") {
                                break true;
                            }
                            if !["INNER", "LEFT", "RIGHT", "OUTER"]
                                .iter()
                                .any(|side| is_word(word, side))
                            {
                                break false;
                            }
                        };
                        if !natural && let Some(&right) = significant.get(at + 1) {
                            chain.waiting.push(Waiting {
                                number: chain.joins,
                                right,
                            });
                        }
                    }
                    "ON" | "USING" => {
                        if next.is_some_and(|next| is_word(next, "DUPLICATE")) {
                            if let Frame::Other { from, .. } = frame {
                                *from = None;
                            }
                            continue;
                        }
                        if let Some(join) = chain.waiting.pop()
                            && join.number != chain.joins
                        {
                            edits.before(join.right, [Token::LParen]);
                            edits.before(index, [Token::RParen]);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if frames.len() == 1 {
        edits
    } else {
        Edits::default()
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_statement;

    fn respelled(sql: &str) -> String {
        parse_statement(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error}"))
            .to_string()
    }

    #[test]
    fn a_parenthesized_list_is_a_cross_join() {
        for (sql, expected) in [
            (
                "SELECT * FROM a LEFT JOIN (b, c) ON a.x = b.x AND b.x = c.x",
                "SELECT * FROM a LEFT JOIN (b CROSS JOIN c) ON a.x = b.x AND b.x = c.x",
            ),
            (
                "SELECT * FROM a JOIN (b, LATERAL (SELECT b.x AS y) AS d) ON a.x = d.y",
                "SELECT * FROM a JOIN (b CROSS JOIN LATERAL (SELECT b.x AS y) AS d) ON a.x = d.y",
            ),
            (
                "SELECT * FROM (a, b) JOIN (c, d) ON a.x = c.x",
                "SELECT * FROM (a CROSS JOIN b) JOIN (c CROSS JOIN d) ON a.x = c.x",
            ),
            (
                "SELECT * FROM a NATURAL JOIN (b JOIN (c NATURAL JOIN d, e NATURAL JOIN f) ON c.x = b.x)",
                "SELECT * FROM a NATURAL JOIN (b JOIN (c NATURAL JOIN d CROSS JOIN (e NATURAL JOIN f)) ON c.x = b.x)",
            ),
            (
                "SELECT * FROM a, (b, (c, d)) WHERE a.x IN (1, 2)",
                "SELECT * FROM a, (b CROSS JOIN (c CROSS JOIN d)) WHERE a.x IN (1, 2)",
            ),
            (
                "SELECT (SELECT COUNT(*) FROM a JOIN (b, c) ON a.x = b.x) FROM d",
                "SELECT (SELECT COUNT(*) FROM a JOIN (b CROSS JOIN c) ON a.x = b.x) FROM d",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }

    #[test]
    fn parentheses_around_one_table_go() {
        for (sql, expected) in [
            ("SELECT * FROM (a AS x)", "SELECT * FROM a AS x"),
            (
                "SELECT * FROM a JOIN ((a AS y)) ON 1",
                "SELECT * FROM a JOIN a AS y ON 1",
            ),
            (
                "SELECT * FROM ((SELECT 1 AS v) AS d)",
                "SELECT * FROM (SELECT 1 AS v) AS d",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }

    #[test]
    fn a_late_condition_groups_the_joins_before_it() {
        for (sql, expected) in [
            (
                "SELECT * FROM a LEFT JOIN b LEFT JOIN c ON b.x = c.x ON a.x = c.x",
                "SELECT * FROM a LEFT JOIN (b LEFT JOIN c ON b.x = c.x) ON a.x = c.x",
            ),
            (
                "SELECT * FROM a RIGHT JOIN b STRAIGHT_JOIN c ON b.x = c.x ON a.x = b.x WHERE a.x > 0",
                "SELECT * FROM a RIGHT JOIN (b STRAIGHT_JOIN c ON b.x = c.x) ON a.x = b.x WHERE a.x > 0",
            ),
            (
                "SELECT * FROM a JOIN b JOIN c JOIN d ON c.x = d.x ON b.x = c.x ON a.x = b.x",
                "SELECT * FROM a JOIN (b JOIN (c JOIN d ON c.x = d.x) ON b.x = c.x) ON a.x = b.x",
            ),
            (
                "SELECT * FROM a LEFT JOIN b NATURAL JOIN c USING (x)",
                "SELECT * FROM a LEFT JOIN (b NATURAL JOIN c) USING(x)",
            ),
        ] {
            assert_eq!(respelled(sql), expected, "{sql}");
        }
    }

    #[test]
    fn other_parentheses_and_clauses_are_left_alone() {
        for sql in [
            "SELECT * FROM a JOIN b ON a.x = b.x JOIN c ON b.x = c.x",
            "SELECT * FROM a JOIN b JOIN c ON b.x = c.x",
            "SELECT * FROM a JOIN (b JOIN c ON b.x = c.x) ON a.x = b.x",
            "SELECT * FROM (SELECT 1 AS v) AS d JOIN ((SELECT 2 AS w) UNION (SELECT 3)) AS e ON d.v < e.w",
            "SELECT EXTRACT(YEAR FROM (a)), SUBSTRING(b FROM (1 + 1) * 2) FROM t WHERE (a, b) IN ((1, 2))",
            "SELECT * FROM a USE INDEX FOR JOIN (i) JOIN b ON LEFT(a.x, 1) = b.x, c",
            "SELECT * FROM a JOIN b USING (x, y), c",
        ] {
            let parsed = respelled(sql);
            assert_eq!(parsed.replace("USING(", "USING ("), sql, "{sql}");
        }
    }
}

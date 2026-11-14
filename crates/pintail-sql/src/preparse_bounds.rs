//! The passes that respell a statement before it is parsed, held to work
//! in proportion to the statement and to a clean refusal of hostile input.
//!
//! Those passes run ahead of the grammar, so its depth limit does not
//! protect them: each test here feeds a shape that once made one of them
//! recurse per parenthesis, reread the statement per form, or stop partway.
//! Work is counted in tokens looked at, never timed.

use sqlparser::parser::ParserError;

use crate::{MAX_PAREN_DEPTH, ParseError, parse_statement, parse_statements, take_preparse_work};

/// Tokens a respelling pass may look at per token of the statement.
const WORK_PER_TOKEN: u64 = 64;

fn nested(open: &str, count: usize, inner: &str, close: &str) -> String {
    format!("{}{inner}{}", open.repeat(count), close.repeat(count))
}

/// Parses `sql`, returning the outcome and the respelling work it took.
fn parsed(sql: &str) -> (Result<usize, ParseError>, u64) {
    take_preparse_work();
    let outcome = parse_statements(sql).map(|statements| statements.len());
    (outcome, take_preparse_work())
}

fn assert_refused_as_too_deep(position: &str, sql: &str) {
    let (outcome, work) = parsed(sql);
    assert!(
        matches!(
            outcome,
            Err(ParseError::InvalidSql(ParserError::RecursionLimitExceeded))
        ),
        "{position}: {outcome:?}"
    );
    // Refused on the first reading: no pass looked at the statement.
    assert_eq!(work, 0, "{position}");
}

/// A hundred thousand nested parentheses, wherever they stand, are refused
/// with the error the grammar gives for nesting past its limit - before
/// any pass reads them, so nothing recurses or rereads on their account.
#[test]
fn a_hundred_thousand_nested_parentheses_are_refused_in_every_position() {
    const DEPTH: usize = 100_000;
    for (position, sql) in [
        (
            "table",
            format!("SELECT * FROM {}", nested("(", DEPTH, "t", ")")),
        ),
        (
            "table, unclosed",
            format!("SELECT * FROM {}t", "(".repeat(DEPTH)),
        ),
        (
            "joined table",
            format!(
                "SELECT * FROM a JOIN {} ON 1",
                nested("(", DEPTH, "b, c", ")")
            ),
        ),
        (
            "derived table",
            format!("SELECT * FROM {} AS d", nested("(", DEPTH, "SELECT 1", ")")),
        ),
        (
            "expression",
            format!("SELECT {}", nested("(", DEPTH, "1", ")")),
        ),
        (
            "function argument",
            format!("SELECT ABS{}", nested("(", DEPTH, "1", ")")),
        ),
        (
            "cast argument",
            format!(
                "SELECT CAST({} AS SIGNED INT)",
                nested("(", DEPTH, "1", ")")
            ),
        ),
        (
            "nested casts",
            format!("SELECT {}", nested("CAST(", DEPTH, "1", " AS SIGNED INT)")),
        ),
        (
            "interval amount",
            format!(
                "SELECT NOW() + INTERVAL {} DAY_HOUR",
                nested("(", DEPTH, "1", ")")
            ),
        ),
        (
            "nested interval functions",
            format!("SELECT {}", nested("INTERVAL(", DEPTH, "1, 2", ")")),
        ),
    ] {
        assert_refused_as_too_deep(position, &sql);
    }
}

/// The deepest nesting the passes read is still far past what the grammar
/// accepts, so the bound turns away no statement that used to parse.
#[test]
fn the_nesting_bound_sits_above_the_grammars_own() {
    let accepted = (1..=MAX_PAREN_DEPTH)
        .take_while(|&depth| {
            parse_statement(&format!("SELECT {}", nested("(", depth, "1", ")"))).is_ok()
        })
        .count();
    assert!(
        accepted > 8,
        "the grammar reads ordinary nesting: {accepted}"
    );
    assert!(
        accepted < MAX_PAREN_DEPTH / 2,
        "the grammar gives up at {accepted}, well inside the bound of {MAX_PAREN_DEPTH}"
    );
    // Parentheses around a table are removed before the grammar counts
    // them, up to the bound.
    let wrapped = format!("SELECT * FROM {}", nested("(", MAX_PAREN_DEPTH, "t", ")"));
    assert_eq!(
        parse_statement(&wrapped)
            .map(|statement| statement.to_string())
            .ok(),
        Some("SELECT * FROM t".to_owned())
    );
}

/// Inside the bound, shapes that made a pass reread or rescan are read in
/// work proportional to the statement, whether or not they then parse.
#[test]
fn respelling_work_is_proportional_to_the_statement() {
    let deep = MAX_PAREN_DEPTH - 2;
    let repeat = |unit: &str, count: usize| vec![unit; count].join(", ");
    for (shape, sql) in [
        (
            "deep table groups, many times",
            format!(
                "SELECT * FROM {}",
                repeat(&nested("(", deep, "t", ")"), 300)
            ),
        ),
        (
            "deep derived tables, many times",
            format!(
                "SELECT * FROM {}",
                repeat(&format!("{} AS d", nested("(", deep, "SELECT 1", ")")), 300)
            ),
        ),
        (
            "a deep query group with a long body",
            format!(
                "SELECT * FROM {} AS d",
                nested("(", deep, &format!("SELECT {}", repeat("1", 40_000)), ")")
            ),
        ),
        (
            "deep expression parentheses, many times",
            format!("SELECT {}", repeat(&nested("(", deep, "1", ")"), 300)),
        ),
        (
            "deep call parentheses, many times",
            format!(
                "SELECT {}",
                repeat(&format!("ABS{}", nested("(", deep, "1", ")")), 300)
            ),
        ),
        (
            "many cast forms",
            format!("SELECT {}", repeat("CAST(a AS SIGNED INT)", 20_000)),
        ),
        (
            "many intervals with no qualifier",
            format!("SELECT {}", "INTERVAL a ".repeat(40_000)),
        ),
        (
            "many interval functions",
            format!("SELECT {}", repeat("INTERVAL(1, 2)", 20_000)),
        ),
        (
            "many joins with late conditions",
            format!("SELECT * FROM a {}", "LEFT JOIN b ".repeat(20_000)),
        ),
    ] {
        let tokens =
            u64::try_from(sql.split_whitespace().count() + sql.matches(['(', ')', ',']).count())
                .expect("a count fits");
        let (_, work) = parsed(&sql);
        assert!(
            work <= tokens * WORK_PER_TOKEN,
            "{shape}: looked at {work} tokens for a statement of about {tokens}"
        );
    }
}

/// Every form in a statement is respelled, however many there are: none
/// is left as written past some count.
#[test]
fn every_call_form_in_a_long_statement_is_respelled() {
    const FORMS: usize = 3_000;
    let sql = format!("SELECT {}", vec!["CAST(a AS SIGNED INT)"; FORMS].join(", "));
    let respelled = parse_statement(&sql).expect("every form reads").to_string();
    assert_eq!(respelled.matches("CAST(a AS SIGNED)").count(), FORMS);

    // Forms inside forms take one more reading each, not a reading per form.
    let inner = "CAST(CAST(CAST(a AS SIGNED INT) AS SIGNED INT) AS SIGNED INT)";
    let sql = format!("SELECT {}", vec![inner; 1_000].join(", "));
    take_preparse_work();
    let respelled = parse_statement(&sql)
        .expect("nested forms read")
        .to_string();
    assert_eq!(respelled.matches("AS SIGNED)").count(), 3_000);
    let tokens = u64::try_from(sql.len() / 3).expect("a count fits");
    assert!(take_preparse_work() <= tokens * WORK_PER_TOKEN);

    // Many introduced, adjacent string literals are joined in one pass.
    let sql = format!("SELECT {}", vec!["_utf8mb4 'a' 'b' 'c'"; 20_000].join(", "));
    assert_eq!(
        parse_statements(&sql)
            .map(|statements| statements.len())
            .ok(),
        Some(1)
    );
}

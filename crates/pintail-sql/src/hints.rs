//! `MySQL` optimizer hints Pintail understands.
//!
//! Hints arrive as raw text between the `/*+` and `*/` markers, so they are
//! read lexically rather than parsed - sqlparser deliberately does not
//! interpret them.

use sqlparser::ast::{Query, Select, SetExpr, Statement};

/// The per-statement execution budget from `/*+ MAX_EXECUTION_TIME(ms) */`.
///
/// `MySQL` scopes this hint to the statement and lets it override the session
/// variable, which is the whole reason to write it: one expensive report gets
/// a shorter leash without changing the connection everything else shares.
///
/// A statement carrying several of them - one per SELECT block in a union or
/// subquery - takes the smallest, because the hint is a ceiling and the
/// tightest ceiling is the one the author meant to hold.
#[must_use]
pub fn max_execution_time_hint(statement: &Statement) -> Option<u64> {
    let mut budget = None;
    if let Statement::Query(query) = statement {
        collect_from_query(query, &mut budget);
    }
    budget
}

/// Whether every hint on this block is one Pintail implements.
///
/// A hint it does not implement still rejects the statement. Silently
/// ignoring an optimizer hint is worse than refusing it: the author wrote it
/// believing it does something, and a query that runs without its stated
/// ceiling is exactly the failure the ceiling was there to prevent.
pub(crate) fn select_hints_are_supported(select: &Select) -> bool {
    select
        .optimizer_hints
        .iter()
        .all(|hint| parse_max_execution_time(&hint.text).is_some())
}

fn collect_from_query(query: &Query, budget: &mut Option<u64>) {
    collect_from_set_expr(&query.body, budget);
}

fn collect_from_set_expr(body: &SetExpr, budget: &mut Option<u64>) {
    match body {
        SetExpr::Select(select) => {
            for hint in &select.optimizer_hints {
                if let Some(milliseconds) = parse_max_execution_time(&hint.text) {
                    *budget = Some(budget.map_or(milliseconds, |held: u64| held.min(milliseconds)));
                }
            }
        }
        SetExpr::Query(query) => collect_from_query(query, budget),
        SetExpr::SetOperation { left, right, .. } => {
            collect_from_set_expr(left, budget);
            collect_from_set_expr(right, budget);
        }
        _ => {}
    }
}

/// Reads `MAX_EXECUTION_TIME(1234)` from one hint's raw text.
///
/// Returns `None` for anything else, including a malformed budget: an
/// unreadable hint is an unsupported hint, and the caller refuses rather than
/// running without the ceiling.
fn parse_max_execution_time(text: &str) -> Option<u64> {
    let trimmed = text.trim();
    let rest = trimmed
        .get(..MAX_EXECUTION_TIME.len())
        .filter(|prefix| prefix.eq_ignore_ascii_case(MAX_EXECUTION_TIME))
        .and_then(|_| trimmed.get(MAX_EXECUTION_TIME.len()..))?
        .trim_start();
    let inner = rest.strip_prefix('(')?.strip_suffix(')')?.trim();
    // `MySQL` takes an unsigned millisecond count; 0 means "no ceiling", which
    // is the same reading the session variable gets.
    inner.parse::<u64>().ok()
}

const MAX_EXECUTION_TIME: &str = "MAX_EXECUTION_TIME";

#[cfg(test)]
mod tests {
    use super::max_execution_time_hint;
    use crate::parse_statement;

    fn hint_of(sql: &str) -> Option<u64> {
        max_execution_time_hint(&parse_statement(sql).expect("parses"))
    }

    #[test]
    fn reads_the_budget_from_a_select() {
        assert_eq!(
            hint_of("SELECT /*+ MAX_EXECUTION_TIME(5000) */ COUNT(*) FROM orders"),
            Some(5_000)
        );
    }

    #[test]
    fn is_case_and_space_insensitive() {
        for sql in [
            "SELECT /*+ max_execution_time(250) */ 1",
            "SELECT /*+   MAX_EXECUTION_TIME ( 250 )   */ 1",
        ] {
            assert_eq!(hint_of(sql), Some(250), "{sql}");
        }
    }

    #[test]
    fn a_statement_without_the_hint_has_no_budget() {
        assert_eq!(hint_of("SELECT COUNT(*) FROM orders"), None);
    }

    #[test]
    fn the_tightest_ceiling_wins_across_a_union() {
        assert_eq!(
            hint_of(
                "SELECT /*+ MAX_EXECUTION_TIME(9000) */ id FROM orders \
                 UNION ALL \
                 SELECT /*+ MAX_EXECUTION_TIME(1500) */ id FROM orders"
            ),
            Some(1_500)
        );
    }

    #[test]
    fn a_malformed_budget_is_not_a_budget() {
        // And the statement then rejects, rather than running uncapped.
        for sql in [
            "SELECT /*+ MAX_EXECUTION_TIME(abc) */ 1",
            "SELECT /*+ MAX_EXECUTION_TIME() */ 1",
            "SELECT /*+ MAX_EXECUTION_TIME 500 */ 1",
        ] {
            assert_eq!(hint_of(sql), None, "{sql}");
        }
    }
}

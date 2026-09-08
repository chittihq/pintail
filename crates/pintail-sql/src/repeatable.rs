//! Whether two executions of one statement must agree.
//!
//! A caller that serves one execution's rows to a second request needs more
//! than identical text: it needs the guarantee that running the statement
//! twice over the same data would have produced the same answer. Most
//! statements have it. The ones that do not read something outside the
//! data - the wall clock, a random source, the connection's own identity,
//! a user variable - and this is the syntax gate that keeps them out.
//!
//! The gate is deliberately blunt. It refuses a name it does not recognise
//! as safe rather than reasoning about where the name appears, so a column
//! called `version` costs a sharing opportunity and never costs a wrong
//! answer.

use std::ops::ControlFlow;

use sqlparser::ast::{Expr, Statement, visit_expressions};

/// Names whose value comes from somewhere other than the queried data.
///
/// Clock and randomness change between two executions of the same text.
/// The session group names the connection, its user or its database, which
/// two requests sharing one execution do not share. The locking and
/// row-count group reads or mutates server state that belongs to one
/// connection's history.
const VOLATILE_NAMES: &[&str] = &[
    // Clock.
    "NOW",
    "SYSDATE",
    "CURDATE",
    "CURTIME",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "UNIX_TIMESTAMP",
    "UTC_DATE",
    "UTC_TIME",
    "UTC_TIMESTAMP",
    // Randomness.
    "RAND",
    "RANDOM",
    "RANDOM_BYTES",
    "UUID",
    "UUID_SHORT",
    // The connection asking.
    "CONNECTION_ID",
    "USER",
    "CURRENT_USER",
    "SESSION_USER",
    "SYSTEM_USER",
    "CURRENT_ROLE",
    "DATABASE",
    "SCHEMA",
    // Per-connection server state.
    "LAST_INSERT_ID",
    "FOUND_ROWS",
    "ROW_COUNT",
    "GET_LOCK",
    "RELEASE_LOCK",
    "RELEASE_ALL_LOCKS",
    "IS_FREE_LOCK",
    "IS_USED_LOCK",
    // Deliberately slow, so two callers must not be collapsed into one wait.
    "SLEEP",
    "BENCHMARK",
    "MASTER_POS_WAIT",
    "SOURCE_POS_WAIT",
    // Reads the host rather than the replica.
    "LOAD_FILE",
];

/// Whether running `statement` twice over one unchanged snapshot must
/// produce the same rows in the same order.
///
/// Only a `SELECT` qualifies. Everything else - a write, `EXPLAIN`, session
/// control - either changes state or reports on the execution that produced
/// it, and neither survives being answered from another request's run.
#[must_use]
pub fn is_repeatable_statement(statement: &Statement) -> bool {
    if !matches!(statement, Statement::Query(_)) {
        return false;
    }
    visit_expressions(statement, |expr| {
        let refused = match expr {
            Expr::Function(function) => is_volatile_name(&function.name.to_string()),
            // A bare `CURRENT_TIMESTAMP` carries no argument list, so it
            // reaches the walk as a name rather than as a call.
            Expr::Identifier(ident) => is_volatile_name(&ident.value),
            Expr::CompoundIdentifier(parts) => {
                parts.iter().any(|part| is_volatile_name(&part.value))
            }
            // A parameter that survived substitution has no value to hash,
            // and a session variable belongs to one connection.
            Expr::Value(value) => {
                let text = value.to_string();
                text.starts_with('?') || text.starts_with('@')
            }
            _ => false,
        };
        if refused {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_continue()
}

/// A name is volatile when it matches the list, or when it is a user or
/// system variable - `@total`, `@@time_zone` - whichever syntax the parser
/// chose to represent it with.
fn is_volatile_name(name: &str) -> bool {
    let name = name.trim_matches('`');
    if name.starts_with('@') {
        return true;
    }
    let upper = name.to_ascii_uppercase();
    VOLATILE_NAMES.contains(&upper.as_str())
}

#[cfg(test)]
mod tests {
    use super::is_repeatable_statement;
    use crate::parse_statement;

    fn repeatable(sql: &str) -> bool {
        is_repeatable_statement(&parse_statement(sql).unwrap())
    }

    #[test]
    fn ordinary_reads_repeat() {
        for sql in [
            "SELECT 1",
            "SELECT id, total FROM orders WHERE region = 'west'",
            "SELECT region, SUM(total) FROM orders GROUP BY region ORDER BY 2 DESC LIMIT 10",
            "SELECT o.id FROM orders o JOIN customers c ON c.id = o.customer_id",
            "WITH recent AS (SELECT * FROM orders LIMIT 5) SELECT COUNT(*) FROM recent",
            "SELECT DATE_ADD(placed_at, INTERVAL 1 DAY) FROM orders",
        ] {
            assert!(repeatable(sql), "{sql}");
        }
    }

    #[test]
    fn the_clock_and_the_connection_do_not_repeat() {
        for sql in [
            "SELECT NOW()",
            "SELECT CURRENT_TIMESTAMP",
            "SELECT id FROM orders WHERE placed_at > NOW() - INTERVAL 1 DAY",
            "SELECT RAND()",
            "SELECT UUID()",
            "SELECT CONNECTION_ID()",
            "SELECT USER()",
            "SELECT DATABASE()",
            "SELECT SLEEP(1)",
            "SELECT FOUND_ROWS()",
            "SELECT @total",
            "SELECT id FROM orders WHERE region = @@time_zone",
        ] {
            assert!(!repeatable(sql), "{sql}");
        }
    }

    #[test]
    fn a_volatile_call_nested_anywhere_refuses() {
        for sql in [
            "SELECT id FROM orders WHERE total > (SELECT AVG(total) * RAND() FROM orders)",
            "SELECT id, CASE WHEN placed_at < CURDATE() THEN 1 ELSE 0 END FROM orders",
            "SELECT id FROM orders ORDER BY RAND() LIMIT 1",
            "SELECT id FROM orders GROUP BY id HAVING COUNT(*) > RAND()",
        ] {
            assert!(!repeatable(sql), "{sql}");
        }
    }

    #[test]
    fn only_a_select_repeats() {
        for sql in [
            "INSERT INTO orders (id) VALUES (1)",
            "CREATE TABLE t (id INT)",
            "EXPLAIN SELECT * FROM orders",
        ] {
            assert!(!repeatable(sql), "{sql}");
        }
    }
}

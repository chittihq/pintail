//! Conservative syntax eligibility for reserved execution capacity.
use std::ops::ControlFlow;

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, SetExpr, Statement, TableFactor, UnaryOperator,
    visit_expressions,
};

/// Caps planning work before a query takes an admission permit. Physical
/// costing still rejects unsupported expressions and unbounded operators.
#[must_use]
pub fn has_bounded_planning_shape(statement: &Statement) -> bool {
    let Statement::Query(query) = statement else {
        return false;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if query.with.is_some()
        || select.from.len() > 1
        || select.from.iter().any(|from| {
            from.joins.len() > 1
                || !matches!(from.relation, TableFactor::Table { .. })
                || from
                    .joins
                    .iter()
                    .any(|join| !matches!(join.relation, TableFactor::Table { .. }))
        })
    {
        return false;
    }
    let mut count = 0;
    visit_expressions(statement, |expr| {
        count += 1;
        if count > 128
            || matches!(
                expr,
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
            )
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_continue()
}

/// Whether a statement is one `SELECT` that reads no table and is small
/// enough for its work to be bounded by its own text: no `FROM`, no common
/// table expression, no subquery, and at most a few dozen expressions. Its
/// functions take what the text spells out and return values of capped
/// size, so nothing in it can grow with stored data; pattern matching and
/// the functions whose result can be a multiple of their operands are
/// left out, since neither is bounded by the text.
#[must_use]
pub fn has_bounded_table_less_shape(statement: &Statement) -> bool {
    let Statement::Query(query) = statement else {
        return false;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if query.with.is_some()
        || !select.from.is_empty()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
    {
        return false;
    }
    let mut count = 0;
    visit_expressions(statement, |expr| {
        count += 1;
        // Three things here have a cost the length of the text does not
        // bound. Matching a pattern: a LIKE wildcard retries from every
        // position, so its work is the product of its operands' lengths,
        // and an operand can be a user variable as long as a packet or a
        // value built from the text many times over. A function whose
        // result can be a multiple of its operands - REPLACE writes its
        // replacement once per match, HEX and QUOTE can double their input,
        // TO_BASE64 and JSON_QUOTE grow it too - so that nesting one
        // multiplies: ten nested HEX calls turn a 4096-byte REPEAT into
        // four megabytes. (REPEAT, SPACE and the pads are capped.) And
        // a function whose purpose is to take time or to wait - for a
        // clock, a lock, a replication position, a file. None of the
        // waiting ones is implemented, and they are named so that
        // implementing one cannot put a wait on a thread that serves other
        // connections.
        let pattern = match expr {
            Expr::Like { .. }
            | Expr::ILike { .. }
            | Expr::RLike { .. }
            | Expr::SimilarTo { .. } => true,
            Expr::Function(function) => {
                let name = function.name.to_string().to_ascii_lowercase();
                let name = name.trim_matches('`');
                name.contains("regexp")
                    || name.contains("rlike")
                    || name.contains("like")
                    || name.contains("lock")
                    || name.contains("wait")
                    || matches!(
                        name,
                        "sleep"
                            | "benchmark"
                            | "load_file"
                            | "json_search"
                            | "replace"
                            | "hex"
                            | "quote"
                            | "to_base64"
                            | "json_quote"
                    )
            }
            _ => false,
        };
        if count > 32
            || pattern
            || matches!(
                expr,
                Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. }
            )
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_continue()
}

/// Whether a statement has a small, predictable operator shape. This is only
/// syntax eligibility: callers must also bound the actual pinned input size.
/// Functions, joins, subqueries and unknown syntax always use general capacity.
#[must_use]
pub fn has_bounded_admission_shape(statement: &Statement) -> bool {
    let Statement::Query(query) = statement else {
        return false;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    if query.with.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
        || select.from.len() > 1
        || select.top.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || !select.connect_by.is_empty()
        || !select.named_window.is_empty()
        || select.having.is_some()
        || select.qualify.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty())
    {
        return false;
    }
    for table in &select.from {
        if !table.joins.is_empty() {
            return false;
        }
        match &table.relation {
            TableFactor::Table {
                args: None,
                with_hints,
                version: None,
                partitions,
                ..
            } if with_hints.is_empty() && partitions.is_empty() => {}
            _ => return false,
        }
    }
    let mut count = 0;
    visit_expressions(statement, |expr| {
        count += 1;
        if matches!(expr, Expr::BinaryOp { op, .. } if !matches!(op,
            BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
            | BinaryOperator::Divide | BinaryOperator::Modulo | BinaryOperator::MyIntegerDivide
            | BinaryOperator::Gt | BinaryOperator::Lt | BinaryOperator::GtEq | BinaryOperator::LtEq
            | BinaryOperator::Eq | BinaryOperator::NotEq | BinaryOperator::Spaceship
            | BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Xor
        )) || matches!(expr, Expr::UnaryOp { op, .. } if !matches!(op,
            UnaryOperator::Plus | UnaryOperator::Minus | UnaryOperator::Not | UnaryOperator::BangNot
        )) {
            return ControlFlow::Break(());
        }
        if count > 128
            || !matches!(
                expr,
                Expr::Identifier(_)
                    | Expr::CompoundIdentifier(_)
                    | Expr::Value(_)
                    | Expr::BinaryOp { .. }
                    | Expr::UnaryOp { .. }
                    | Expr::Nested(_)
                    | Expr::IsNull(_)
                    | Expr::IsNotNull(_)
                    | Expr::IsTrue(_)
                    | Expr::IsFalse(_)
                    | Expr::IsUnknown(_)
                    | Expr::IsNotTrue(_)
                    | Expr::IsNotFalse(_)
                    | Expr::IsNotUnknown(_)
            )
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .is_continue()
}

#[cfg(test)]
mod tests {
    use super::has_bounded_admission_shape;
    use crate::parse_statement;

    #[test]
    fn only_a_small_select_without_tables_is_table_less() {
        use super::has_bounded_table_less_shape;
        for sql in [
            "SELECT 1 + 1",
            "SELECT UPPER('abc'), CONCAT('a', 'b') AS joined",
            "SELECT NOW(), @v := 3 ORDER BY 1 LIMIT 1",
        ] {
            assert!(
                has_bounded_table_less_shape(&parse_statement(sql).unwrap()),
                "{sql}"
            );
        }
        let wide = format!("SELECT {}", vec!["1"; 40].join(" + "));
        for sql in [
            "SELECT id FROM t",
            "SELECT 1 FROM DUAL",
            "SELECT (SELECT 1)",
            "SELECT 1 IN (SELECT id FROM t)",
            "WITH c AS (SELECT 1) SELECT 2",
            "SELECT 1 UNION SELECT 2",
            "SELECT 'aaa' REGEXP '(a+)+$'",
            "SELECT REGEXP_REPLACE('abc', 'b', 'x')",
            "SELECT REPEAT('a', 4096) LIKE CONCAT('%', REPEAT('a', 2048), 'b')",
            "SELECT @payload NOT LIKE '%b'",
            "SELECT 'a' LIKE 'a' ESCAPE '!'",
            "SELECT JSON_SEARCH('[\"a\"]', 'one', 'a%')",
            "SELECT REPLACE(REPLACE(REPEAT('a', 1024), 'a', REPEAT('a', 1024)), 'a', 'aa')",
            "SELECT HEX(HEX(HEX(REPEAT('a', 4096))))",
            "SELECT QUOTE(QUOTE(''''))",
            "SELECT SLEEP(5)",
            "SELECT 1 + sleep(0.5)",
            "SELECT BENCHMARK(100000000, MD5('a'))",
            "SELECT GET_LOCK('a', 10)",
            "SELECT IS_FREE_LOCK('a')",
            "SELECT SOURCE_POS_WAIT('f', 4)",
            "SELECT WAIT_FOR_EXECUTED_GTID_SET('x', 10)",
            "SELECT LOAD_FILE('/etc/hostname')",
            "SHOW TABLES",
            wide.as_str(),
        ] {
            assert!(
                !has_bounded_table_less_shape(&parse_statement(sql).unwrap()),
                "{sql}"
            );
        }
    }

    #[test]
    fn only_bounded_operator_shapes_are_eligible() {
        for sql in [
            "SELECT 1",
            "SELECT id, value + 1 FROM t WHERE id = 7",
            "SELECT * FROM t ORDER BY id LIMIT 10",
        ] {
            assert!(
                has_bounded_admission_shape(&parse_statement(sql).unwrap()),
                "{sql}"
            );
        }
        for sql in [
            "SELECT SLEEP(1)",
            "SELECT REPEAT('x', 1000000)",
            "SELECT COUNT(*) FROM t",
            "SELECT * FROM t JOIN u ON t.id = u.id",
            "SELECT (SELECT id FROM t)",
            "WITH t AS (SELECT 1) SELECT * FROM t",
            "SELECT id FROM t UNION ALL SELECT id FROM u",
        ] {
            assert!(
                !has_bounded_admission_shape(&parse_statement(sql).unwrap()),
                "{sql}"
            );
        }
        let many = format!("SELECT {}", vec!["1"; 129].join(","));
        assert!(!has_bounded_admission_shape(
            &parse_statement(&many).unwrap()
        ));
    }
}

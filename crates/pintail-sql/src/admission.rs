//! Conservative syntax eligibility for reserved execution capacity.
use std::ops::ControlFlow;

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, SetExpr, Statement, TableFactor, UnaryOperator,
    visit_expressions,
};

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

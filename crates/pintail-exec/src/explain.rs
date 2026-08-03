use std::fmt::{self, Write};

use pintail_catalog::CatalogSnapshot;
use pintail_sql::{BindError, Binder, BoundJoinKind, Statement};

use crate::{
    ExecError, Execution, LogicalPlanner, Optimizer, PhysicalPlan, PhysicalPlanner,
    SnapshotScanProvider,
};

/// Binds, optimizes, and formats one non-analyzing `EXPLAIN` statement.
///
/// # Errors
///
/// Returns an explicit statement-shape, binding, or physical-planning error.
pub fn explain_statement(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    current_database: Option<&str>,
) -> Result<String, ExplainError> {
    let Statement::Explain {
        analyze: false,
        verbose: false,
        query_plan: false,
        estimate: false,
        statement,
        format: None,
        options: None,
        ..
    } = statement
    else {
        return Err(ExplainError::Unsupported(statement.to_string()));
    };
    let bound = Binder::new(catalog, current_database).bind(statement)?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical = PhysicalPlanner::plan(logical)?;
    Ok(format_physical_plan(&physical))
}

/// Executes an `EXPLAIN ANALYZE` query and includes actual storage-pruning
/// counters in its physical plan.
///
/// # Errors
///
/// Returns an explicit statement-shape, binding, planning, scan, or execution
/// error.
pub fn explain_analyze_statement(
    statement: &Statement,
    catalog: &CatalogSnapshot,
    current_database: Option<&str>,
    provider: &SnapshotScanProvider<'_>,
    memory_limit: usize,
) -> Result<String, ExplainError> {
    let Statement::Explain {
        analyze: true,
        verbose: false,
        query_plan: false,
        estimate: false,
        statement,
        format: None,
        options: None,
        ..
    } = statement
    else {
        return Err(ExplainError::Unsupported(statement.to_string()));
    };
    let bound = Binder::new(catalog, current_database).bind(statement)?;
    let logical = Optimizer::optimize(LogicalPlanner::plan(bound));
    let physical = PhysicalPlanner::plan(logical)?;
    let mut execution = Execution::start(physical.clone(), provider, memory_limit)?;
    while execution.next_batch()?.is_some() {}
    Ok(format_physical_plan_with_stats(&physical, provider))
}

/// Produces a stable, indented physical-plan representation.
#[must_use]
pub fn format_physical_plan(plan: &PhysicalPlan) -> String {
    let mut output = String::new();
    let _ = write_plan(plan, 0, &mut output, None);
    output
}

/// Produces a stable physical plan with accumulated storage scan counters.
#[must_use]
pub fn format_physical_plan_with_stats(
    plan: &PhysicalPlan,
    provider: &SnapshotScanProvider<'_>,
) -> String {
    let mut output = String::new();
    let _ = write_plan(plan, 0, &mut output, Some(provider));
    output
}

#[allow(clippy::too_many_lines)]
fn write_plan(
    plan: &PhysicalPlan,
    depth: usize,
    output: &mut String,
    provider: Option<&SnapshotScanProvider<'_>>,
) -> fmt::Result {
    for _ in 0..depth {
        output.push_str("  ");
    }
    match plan {
        PhysicalPlan::Empty => writeln!(output, "Empty"),
        PhysicalPlan::OneRow => writeln!(output, "OneRow"),
        PhysicalPlan::Scan(scan) => {
            write!(
                output,
                "Scan table={}.{} rows={:?} columns={:?} predicates={} limit={:?}",
                scan.table.database_name,
                scan.table.table_name,
                scan.estimated_rows(),
                scan.projected_column_ids,
                scan.predicates.len(),
                scan.limit
            )?;
            if let Some(stats) = provider.and_then(|provider| {
                provider.scan_stats(scan.table.database_id, scan.table.table_id)
            }) {
                write!(
                    output,
                    " actual_segments={}/{} actual_blocks={}/{} decoded_blocks={}",
                    stats.segments_read,
                    stats.segments_total(),
                    stats.blocks_read,
                    stats.blocks_total(),
                    stats.blocks_decoded
                )?;
            }
            writeln!(output)
        }
        PhysicalPlan::Derived { input, columns } => {
            writeln!(
                output,
                "Derived columns=[{}]",
                columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::CrossJoin {
            inputs,
            estimated_rows,
        } => {
            writeln!(output, "CrossJoin estimated_rows={estimated_rows}")?;
            write_inputs(inputs, depth, output, provider)
        }
        PhysicalPlan::UnionAll { inputs } => {
            writeln!(output, "UnionAll inputs={}", inputs.len())?;
            write_inputs(inputs, depth, output, provider)
        }
        PhysicalPlan::Recursive {
            distinct,
            anchor,
            member,
            ..
        } => {
            writeln!(
                output,
                "Recursive union={}",
                if *distinct { "distinct" } else { "all" }
            )?;
            write_plan(anchor, depth + 1, output, provider)?;
            write_plan(member, depth + 1, output, provider)
        }
        PhysicalPlan::SetOp {
            keep_matching,
            all,
            left,
            right,
        } => {
            writeln!(
                output,
                "SetOp kind={}{}",
                if *keep_matching {
                    "intersect"
                } else {
                    "except"
                },
                if *all { " all" } else { "" }
            )?;
            write_plan(left, depth + 1, output, provider)?;
            write_plan(right, depth + 1, output, provider)
        }
        PhysicalPlan::HashJoin {
            left, right, kind, ..
        } => {
            writeln!(output, "HashJoin kind={}", join_name(*kind))?;
            write_plan(left, depth + 1, output, provider)?;
            write_plan(right, depth + 1, output, provider)
        }
        PhysicalPlan::Filter { input, .. } => {
            writeln!(output, "Filter")?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::HashAggregate {
            input,
            group_by,
            aggregates,
        } => {
            writeln!(
                output,
                "HashAggregate groups={} aggregates={}",
                group_by.len(),
                aggregates.len()
            )?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::Project { input, expressions } => {
            writeln!(
                output,
                "Project outputs=[{}]",
                expressions
                    .iter()
                    .map(|expression| expression.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::Distinct { input } => {
            writeln!(output, "Distinct")?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::Window { input, windows, .. } => {
            writeln!(output, "Window functions={}", windows.len())?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::Sort {
            input,
            keys,
            top_k,
            trim,
        } => {
            writeln!(output, "Sort keys={keys:?} top_k={top_k:?} trim={trim}")?;
            write_plan(input, depth + 1, output, provider)
        }
        PhysicalPlan::Limit {
            input,
            offset,
            count,
        } => {
            writeln!(output, "Limit offset={offset} count={count}")?;
            write_plan(input, depth + 1, output, provider)
        }
    }
}

fn write_inputs(
    inputs: &[PhysicalPlan],
    depth: usize,
    output: &mut String,
    provider: Option<&SnapshotScanProvider<'_>>,
) -> fmt::Result {
    for input in inputs {
        write_plan(input, depth + 1, output, provider)?;
    }
    Ok(())
}

const fn join_name(kind: BoundJoinKind) -> &'static str {
    match kind {
        BoundJoinKind::Inner => "inner",
        BoundJoinKind::Left => "left",
        BoundJoinKind::Semi => "semi",
        BoundJoinKind::Anti => "anti",
        BoundJoinKind::Cross => "cross",
    }
}

/// Failure to produce a physical explanation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExplainError {
    /// The statement requests an unsupported `EXPLAIN` extension.
    Unsupported(String),
    /// SQL binding failed.
    Bind(BindError),
    /// Physical planning failed.
    Exec(ExecError),
}

impl fmt::Display for ExplainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(statement) => {
                write!(formatter, "unsupported EXPLAIN statement: {statement}")
            }
            Self::Bind(error) => error.fmt(formatter),
            Self::Exec(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ExplainError {}

impl From<BindError> for ExplainError {
    fn from(error: BindError) -> Self {
        Self::Bind(error)
    }
}

impl From<ExecError> for ExplainError {
    fn from(error: ExecError) -> Self {
        Self::Exec(error)
    }
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::parse_statement;
    use pintail_types::{Column, DataType, TableSchema};

    use super::explain_statement;

    #[test]
    fn explains_optimized_physical_scan_and_top_k_shape() {
        let table = TableEntry::new(
            TableId::new(2),
            "events",
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(100),
        )
        .expect("table");
        let database = DatabaseEntry::new(DatabaseId::new(1), "app", [table]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement =
            parse_statement("EXPLAIN SELECT name FROM events WHERE id > 10 ORDER BY name LIMIT 5")
                .expect("parse");

        let explanation =
            explain_statement(&statement, &catalog, Some("app")).expect("explanation");
        assert!(explanation.contains("Limit offset=0 count=5"));
        assert!(explanation.contains("Sort keys="));
        assert!(explanation.contains("top_k=Some(5)"));
        assert!(explanation.contains("Scan table=app.events"));
        assert!(explanation.contains("columns=[1, 2]"));
        assert!(explanation.contains("predicates=1"));
    }
}

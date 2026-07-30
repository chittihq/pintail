use pintail_sql::{
    BoundAggregate, BoundExpr, BoundFrom, BoundJoinKind, BoundLimit, BoundProjection, BoundQuery,
    BoundTable,
};

/// A logical table scan with optimizer-controlled storage inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scan {
    /// Bound catalog table and its snapshot statistics.
    pub table: BoundTable,
    /// Stable column IDs requested from storage, in physical schema order.
    pub projected_column_ids: Vec<u32>,
    /// Conjunctive predicates eligible for storage pruning.
    pub predicates: Vec<BoundExpr>,
    /// Maximum rows the scan needs to produce, when safely bounded.
    pub limit: Option<u64>,
}

impl Scan {
    /// Returns the planner's current cardinality estimate.
    #[must_use]
    pub fn estimated_rows(&self) -> Option<u64> {
        match (self.table.row_count, self.limit) {
            (Some(rows), Some(limit)) => Some(rows.min(limit)),
            (Some(rows), None) => Some(rows),
            (None, _) => None,
        }
    }
}

/// Logical relational operators before physical implementation choices.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogicalPlan {
    /// A relation known to produce no rows.
    Empty,
    /// A single row with no input columns, used by queries such as `SELECT 1`.
    OneRow,
    /// A storage-backed relation.
    Scan(Scan),
    /// Cartesian product of two or more inputs.
    CrossJoin {
        /// Inputs in semantic source order.
        inputs: Vec<LogicalPlan>,
    },
    /// Predicate-constrained binary join.
    Join {
        /// Left input.
        left: Box<LogicalPlan>,
        /// Right input.
        right: Box<LogicalPlan>,
        /// Join semantics.
        kind: BoundJoinKind,
        /// Optional predicate. An unconstrained inner join is Cartesian.
        condition: Option<BoundExpr>,
    },
    /// Row predicate.
    Filter {
        /// Input relation.
        input: Box<LogicalPlan>,
        /// `MySQL` truth-valued expression.
        predicate: BoundExpr,
    },
    /// Hash grouping and aggregate computation.
    Aggregate {
        /// Input relation.
        input: Box<LogicalPlan>,
        /// Ordered grouping expressions.
        group_by: Vec<BoundExpr>,
        /// Deduplicated aggregate computations.
        aggregates: Vec<BoundAggregate>,
    },
    /// Ordered client-visible expressions.
    Project {
        /// Input relation.
        input: Box<LogicalPlan>,
        /// Named output expressions.
        expressions: Vec<BoundProjection>,
    },
    /// Duplicate removal.
    Distinct {
        /// Input relation.
        input: Box<LogicalPlan>,
    },
    /// Offset and count.
    Limit {
        /// Input relation.
        input: Box<LogicalPlan>,
        /// Normalized non-negative limit.
        limit: BoundLimit,
    },
}

impl LogicalPlan {
    /// Returns a conservative row-count estimate when catalog statistics make
    /// one available.
    #[must_use]
    pub fn estimated_rows(&self) -> Option<u64> {
        match self {
            Self::Empty => Some(0),
            Self::OneRow => Some(1),
            Self::Scan(scan) => scan.estimated_rows(),
            Self::CrossJoin { inputs } => inputs.iter().try_fold(1_u64, |rows, input| {
                rows.checked_mul(input.estimated_rows()?)
            }),
            Self::Join {
                left, right, kind, ..
            } => match kind {
                BoundJoinKind::Inner | BoundJoinKind::Cross => {
                    left.estimated_rows()?.checked_mul(right.estimated_rows()?)
                }
                BoundJoinKind::Left => left
                    .estimated_rows()?
                    .checked_mul(right.estimated_rows()?.max(1)),
                BoundJoinKind::Semi | BoundJoinKind::Anti => left.estimated_rows(),
            },
            Self::Filter { input, .. }
            | Self::Distinct { input }
            | Self::Project { input, .. }
            | Self::Aggregate { input, .. } => input.estimated_rows(),
            Self::Limit { input, limit } => input
                .estimated_rows()
                .map(|rows| rows.saturating_sub(limit.offset).min(limit.count)),
        }
    }
}

/// Lowers typed bound queries into explicit relational operators.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogicalPlanner;

impl LogicalPlanner {
    /// Builds an unoptimized logical plan.
    #[must_use]
    pub fn plan(query: BoundQuery) -> LogicalPlan {
        let BoundQuery {
            from,
            tables,
            projection,
            filter,
            group_by,
            aggregates,
            having,
            distinct,
            limit,
        } = query;

        debug_assert_eq!(
            tables.len(),
            from.iter()
                .map(|source| 1 + source.joins.len())
                .sum::<usize>()
        );
        let mut plan = source_plan(from);
        if let Some(predicate) = filter {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }
        if !group_by.is_empty() || !aggregates.is_empty() {
            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                group_by,
                aggregates,
            };
        }
        if let Some(predicate) = having {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }
        plan = LogicalPlan::Project {
            input: Box::new(plan),
            expressions: projection,
        };
        if distinct {
            plan = LogicalPlan::Distinct {
                input: Box::new(plan),
            };
        }
        if let Some(limit) = limit {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                limit,
            };
        }
        plan
    }
}

fn source_plan(from: Vec<BoundFrom>) -> LogicalPlan {
    let mut inputs = from
        .into_iter()
        .map(|source| {
            let mut plan = scan_plan(source.base);
            for join in source.joins {
                plan = LogicalPlan::Join {
                    left: Box::new(plan),
                    right: Box::new(scan_plan(join.table)),
                    kind: join.kind,
                    condition: join.condition,
                };
            }
            plan
        })
        .collect::<Vec<_>>();

    match inputs.len() {
        0 => LogicalPlan::OneRow,
        1 => inputs.pop().unwrap_or(LogicalPlan::OneRow),
        _ => LogicalPlan::CrossJoin { inputs },
    }
}

fn scan_plan(table: BoundTable) -> LogicalPlan {
    let projected_column_ids = table
        .columns
        .iter()
        .map(|column| column.column_id)
        .collect();
    LogicalPlan::Scan(Scan {
        table,
        projected_column_ids,
        predicates: Vec::new(),
        limit: None,
    })
}

#[cfg(test)]
mod tests {
    use pintail_catalog::{
        CatalogSnapshot, DatabaseEntry, DatabaseId, TableEntry, TableId, TableStatistics,
    };
    use pintail_sql::{Binder, BoundJoinKind, BoundLimit, parse_statement};
    use pintail_types::{Column, DataType, TableSchema};

    use super::{LogicalPlan, LogicalPlanner};

    fn plan(sql: &str) -> LogicalPlan {
        let events = table(1, "events", 100);
        let users = table(2, "users", 20);
        let database =
            DatabaseEntry::new(DatabaseId::new(9), "app", [events, users]).expect("database");
        let catalog = CatalogSnapshot::new([database]).expect("catalog");
        let statement = parse_statement(sql).expect("parse");
        let query = Binder::new(&catalog, Some("app"))
            .bind(&statement)
            .expect("bind");
        LogicalPlanner::plan(query)
    }

    fn table(id: u64, name: &str, row_count: u64) -> TableEntry {
        TableEntry::new(
            TableId::new(id),
            name,
            TableSchema::new(
                1,
                vec![
                    Column::new(1, "id", DataType::UInt64, false),
                    Column::new(2, "name", DataType::Utf8, true),
                ],
            )
            .expect("schema"),
            TableStatistics::with_row_count(row_count),
        )
        .expect("table")
    }

    #[test]
    fn lowers_select_filter_project_and_limit_in_semantic_order() {
        let plan = plan("SELECT name FROM events WHERE id > 10 LIMIT 5, 12");

        let LogicalPlan::Limit { input, limit } = plan else {
            panic!("limit root");
        };
        assert_eq!(
            limit,
            BoundLimit {
                offset: 5,
                count: 12
            }
        );
        let LogicalPlan::Project { input, expressions } = *input else {
            panic!("project below limit");
        };
        assert_eq!(expressions[0].name, "name");
        let LogicalPlan::Filter { input, .. } = *input else {
            panic!("filter below project");
        };
        let LogicalPlan::Scan(scan) = *input else {
            panic!("scan leaf");
        };
        assert_eq!(scan.projected_column_ids, [1, 2]);
        assert_eq!(scan.estimated_rows(), Some(100));
    }

    #[test]
    fn represents_constant_queries_with_one_row_input() {
        let plan = plan("SELECT 1");
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("project root");
        };
        assert_eq!(*input, LogicalPlan::OneRow);
        assert_eq!(
            LogicalPlan::Limit {
                input,
                limit: BoundLimit {
                    offset: 0,
                    count: 4
                }
            }
            .estimated_rows(),
            Some(1)
        );
    }

    #[test]
    fn keeps_cross_join_inputs_and_cardinality_visible() {
        let plan = plan("SELECT events.id FROM events, users");
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("project root");
        };
        let LogicalPlan::CrossJoin { inputs } = *input else {
            panic!("cross join");
        };
        assert_eq!(inputs.len(), 2);
        assert_eq!(
            LogicalPlan::CrossJoin { inputs }.estimated_rows(),
            Some(2_000)
        );
    }

    #[test]
    fn places_distinct_before_limit() {
        let plan = plan("SELECT DISTINCT name FROM events LIMIT 3");
        let LogicalPlan::Limit { input, .. } = plan else {
            panic!("limit root");
        };
        assert!(matches!(*input, LogicalPlan::Distinct { .. }));
    }

    #[test]
    fn preserves_explicit_join_kind_and_condition() {
        let plan = plan(
            "SELECT events.name FROM events \
             LEFT JOIN users ON events.id = users.id",
        );
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("project root");
        };
        let LogicalPlan::Join {
            kind,
            condition,
            left,
            right,
        } = *input
        else {
            panic!("join");
        };
        assert_eq!(kind, BoundJoinKind::Left);
        assert!(condition.is_some());
        assert!(matches!(*left, LogicalPlan::Scan(_)));
        assert!(matches!(*right, LogicalPlan::Scan(_)));
    }

    #[test]
    fn places_having_between_aggregate_and_projection() {
        let plan =
            plan("SELECT name, COUNT(*) AS rows FROM events GROUP BY name HAVING COUNT(*) > 1");
        let LogicalPlan::Project { input, .. } = plan else {
            panic!("project root");
        };
        let LogicalPlan::Filter { input, .. } = *input else {
            panic!("having filter");
        };
        let LogicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } = *input
        else {
            panic!("aggregate");
        };
        assert_eq!(group_by.len(), 1);
        assert_eq!(aggregates.len(), 1);
        assert!(matches!(*input, LogicalPlan::Scan(_)));
    }
}

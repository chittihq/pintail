//! Metadata-only work bounds for reserved query admission.
use pintail_sql::{AggregateFunction, BoundExpr, BoundExprKind};

use crate::{PhysicalPlan, SnapshotScanProvider};

/// Physical input upper bounds, before residual predicate selectivity.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AdmissionCost {
    /// Physical rows in overlapping segments and the pinned WAL tail.
    pub rows: u64,
    /// Projected fixed-width bytes, or an unknown bound for variable values.
    pub bytes: u64,
}

impl AdmissionCost {
    /// Fixed admission budget. An unknown byte width can still qualify by rows.
    #[must_use]
    pub const fn within_budget(self) -> bool {
        self.rows <= 256 * 1024 || self.bytes <= 32 * 1024 * 1024
    }

    fn plus(self, other: Self) -> Self {
        Self {
            rows: self.rows.saturating_add(other.rows),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }
}

struct Work {
    cost: AdmissionCost,
    outputs: u64,
    scans: usize,
    filtered: bool,
}

impl SnapshotScanProvider<'_> {
    /// Bounds a simple physical plan without reading or executing storage.
    /// Unknown expressions and materializing operators use general capacity.
    #[must_use]
    pub fn admission_cost(&self, plan: &PhysicalPlan) -> Option<AdmissionCost> {
        let work = self.admission_work(plan)?;
        (work.outputs <= 1000 && work.scans <= 2 && work.cost.within_budget()).then_some(work.cost)
    }

    #[allow(clippy::too_many_lines)] // one arm per eligible physical operator
    fn admission_work(&self, plan: &PhysicalPlan) -> Option<Work> {
        match plan {
            PhysicalPlan::Empty | PhysicalPlan::OneRow => Some(Work {
                cost: AdmissionCost::default(),
                outputs: 1,
                scans: 0,
                filtered: false,
            }),
            PhysicalPlan::Scan(scan) => {
                if !scan.predicates.iter().all(cheap_expression) {
                    return None;
                }
                let (cost, point) = self.scan_admission_cost(scan)?;
                Some(Work {
                    cost,
                    outputs: if point { 1 } else { cost.rows },
                    scans: 1,
                    filtered: !scan.predicates.is_empty(),
                })
            }
            PhysicalPlan::Project { input, expressions } => {
                if !expressions.iter().all(|expr| cheap_expression(&expr.expr)) {
                    return None;
                }
                self.admission_work(input)
            }
            PhysicalPlan::Filter { input, predicate } => {
                if !cheap_expression(predicate) {
                    return None;
                }
                let mut work = self.admission_work(input)?;
                work.filtered = true;
                Some(work)
            }
            PhysicalPlan::Limit { input, count, .. } => {
                if *count > 1000 {
                    return None;
                }
                let mut work = self.admission_work(input)?;
                work.outputs = work.outputs.min(*count);
                Some(work)
            }
            PhysicalPlan::HashAggregate {
                input,
                group_by,
                aggregates,
            } => {
                if !group_by.is_empty()
                    || !aggregates.iter().all(|aggregate| {
                        !aggregate.distinct
                            && matches!(
                                aggregate.function,
                                AggregateFunction::Count
                                    | AggregateFunction::Sum
                                    | AggregateFunction::Average
                                    | AggregateFunction::Minimum
                                    | AggregateFunction::Maximum
                            )
                            && aggregate.expr.as_ref().is_none_or(cheap_expression)
                    })
                {
                    return None;
                }
                let mut work = self.admission_work(input)?;
                if !work.filtered {
                    return None;
                }
                work.outputs = 1;
                Some(work)
            }
            PhysicalPlan::Sort { input, keys, .. } => {
                let PhysicalPlan::Project {
                    input: source,
                    expressions,
                } = input.as_ref()
                else {
                    return None;
                };
                let [key] = keys.as_slice() else {
                    return None;
                };
                let expression = expressions.get(key.index)?;
                if !key.ascending || !storage_key(source, &expression.expr) {
                    return None;
                }
                self.admission_work(input)
            }
            PhysicalPlan::HashJoin {
                left,
                right,
                left_key,
                right_key,
                extra_keys,
                residual,
                ..
            } => {
                if !extra_keys.is_empty()
                    || residual.is_some()
                    || !storage_key(left, left_key)
                    || !storage_key(right, right_key)
                {
                    return None;
                }
                let left = self.admission_work(left)?;
                let right = self.admission_work(right)?;
                if left.scans != 1
                    || right.scans != 1
                    || !left.cost.within_budget()
                    || !right.cost.within_budget()
                {
                    return None;
                }
                Some(Work {
                    cost: left.cost.plus(right.cost),
                    outputs: left.outputs.saturating_add(right.outputs),
                    scans: 2,
                    filtered: left.filtered || right.filtered,
                })
            }
            // A remaining sort really materializes input; LIMIT alone does
            // not turn it into a bounded scan. Windows and dependent plans
            // likewise cannot borrow the reserve.
            _ => None,
        }
    }
}

fn storage_key(plan: &PhysicalPlan, expr: &BoundExpr) -> bool {
    let BoundExprKind::Column(column) = &expr.kind else {
        return false;
    };
    match plan {
        PhysicalPlan::Scan(scan) => {
            column.database_id == scan.table.database_id
                && column.table_id == scan.table.table_id
                && scan.table.key_column_ids.as_slice() == [column.column_id]
        }
        PhysicalPlan::Filter { input, .. } => storage_key(input, expr),
        _ => false,
    }
}

fn cheap_expression(expr: &BoundExpr) -> bool {
    match &expr.kind {
        BoundExprKind::Column(_)
        | BoundExprKind::Literal(_)
        | BoundExprKind::GroupKey(_)
        | BoundExprKind::Aggregate(_) => true,
        BoundExprKind::Binary { left, right, .. } => {
            cheap_expression(left) && cheap_expression(right)
        }
        BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
            cheap_expression(expr)
        }
        _ => false,
    }
}

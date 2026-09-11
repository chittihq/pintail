//! Row order as a property of the physical plan.
//!
//! A table that declares key columns is scanned in their order: storage
//! keeps its rows sorted by key and hands every scan over in that order,
//! parallel decoding included, and a provider that cannot keep that order
//! declares no key columns (see [`ScanProvider::open_scan`]). A filter
//! passes on the rows it keeps in the order it read them. So rows that come
//! from one such scan through at most one filter arrive ordered by the
//! table's leading key columns, and a sort by those columns, ascending, has
//! nothing left to do.
//!
//! The order is claimed only where the stored order is the order `MySQL`
//! sorts by: integer key columns that hold no NULL, stored by numeric
//! value. A text key is stored in byte order, which a collation need not
//! follow, and is never claimed.
//!
//! [`ScanProvider::open_scan`]: super::ScanProvider::open_scan

use pintail_sql::{BoundColumn, BoundExprKind, BoundOrderKey, BoundProjection};
use pintail_types::DataType;

use super::{PhysicalPlan, Scan};

pub(super) fn integer_type(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
        Some(
            DataType::Int8
                | DataType::Int16
                | DataType::Int32
                | DataType::Int64
                | DataType::UInt8
                | DataType::UInt16
                | DataType::UInt32
                | DataType::UInt64
        )
    )
}

/// The scan under at most one filter: either way its rows arrive in the
/// table's key order.
pub(super) fn base_scan(plan: &PhysicalPlan) -> Option<&Scan> {
    match plan {
        PhysicalPlan::Scan(scan) => Some(scan),
        PhysicalPlan::Filter { input, .. } => match input.as_ref() {
            PhysicalPlan::Scan(scan) => Some(scan),
            _ => None,
        },
        _ => None,
    }
}

/// Whether `column` is the scan's own column `column_id`, not a reference
/// to an enclosing query's row.
pub(super) fn names_column(scan: &Scan, column: &BoundColumn, column_id: u32) -> bool {
    !column.outer
        && column.database_id == scan.table.database_id
        && column.table_id == scan.table.table_id
        && column.column_id == column_id
        && column
            .relation_name
            .eq_ignore_ascii_case(&scan.table.relation_name)
}

/// Whether the scan's key order is the order of `columns`: they name its
/// leading key columns in turn, and each is an integer that is never NULL,
/// whose stored order is its numeric order.
pub(super) fn ordered_by(scan: &Scan, columns: &[&BoundColumn]) -> bool {
    columns.len() <= scan.table.key_column_ids.len()
        && columns
            .iter()
            .zip(&scan.table.key_column_ids)
            .all(|(column, key)| {
                names_column(scan, column, *key)
                    && !column.nullable
                    && integer_type(Some(column.data_type))
            })
}

/// The columns an ascending sort by `keys` orders a projection by, when
/// every key is one of its bare columns. The projection's last `trim`
/// columns are the sort's own and are dropped once it is done.
pub(super) fn key_columns<'plan>(
    expressions: &'plan [BoundProjection],
    keys: &[BoundOrderKey],
    trim: usize,
) -> Option<Vec<&'plan BoundColumn>> {
    if keys.is_empty() || trim > expressions.len() {
        return None;
    }
    keys.iter()
        .map(|key| match &expressions.get(key.index)?.expr.kind {
            BoundExprKind::Column(column) if key.ascending => Some(column),
            _ => None,
        })
        .collect()
}

/// The sort input with the sort's own columns dropped, and `true`, when it
/// is a projection over a scan already in the order of `keys`. Any other
/// input comes back unchanged, with `false`.
pub(super) fn scan_ordered_input(
    input: PhysicalPlan,
    keys: &[BoundOrderKey],
    trim: usize,
) -> (PhysicalPlan, bool) {
    match input {
        PhysicalPlan::Project {
            input,
            mut expressions,
        } if key_columns(&expressions, keys, trim).is_some_and(|columns| {
            base_scan(&input).is_some_and(|scan| ordered_by(scan, &columns))
        }) =>
        {
            expressions.truncate(expressions.len() - trim);
            (PhysicalPlan::Project { input, expressions }, true)
        }
        other => (other, false),
    }
}

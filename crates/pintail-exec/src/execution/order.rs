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

pub(crate) fn integer_type(data_type: Option<DataType>) -> bool {
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

/// `plan`, already in the order a limit of `rows` rows wants, with its scan
/// told to stop after that many when nothing between the two drops a row:
/// a projection over a scan. The scan's predicates are its own filter, so
/// its first `rows` rows that pass them, in key order, are the limit's
/// rows. A virtual relation is bounded only without predicates.
pub(super) fn limited_scan(plan: PhysicalPlan, rows: u64) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Project { input, expressions } => match *input {
            PhysicalPlan::Scan(mut scan)
                if scan.predicates.is_empty()
                    || scan.table.database_id != pintail_catalog::DatabaseId::new(u64::MAX) =>
            {
                scan.limit = Some(scan.limit.map_or(rows, |limit| limit.min(rows)));
                PhysicalPlan::Project {
                    input: Box::new(PhysicalPlan::Scan(scan)),
                    expressions,
                }
            }
            input => PhysicalPlan::Project {
                input: Box::new(input),
                expressions,
            },
        },
        other => other,
    }
}

/// A sort's input with its scan told that only the table's last `rows`
/// rows are wanted, when the sort is by the whole key, descending, and a
/// limit above it takes `rows` rows: a projection over a scan, the keys
/// its bare key columns in key order, each a non-NULL integer.
///
/// The key is whole so no two rows tie: the last `rows` rows in key order
/// are then exactly the rows the sort would put first, and it orders them
/// as it orders the table. The scan's predicates are its own filter, so
/// the rows counted are rows that pass them. Any other input comes back
/// unchanged.
pub(super) fn end_limited_scan(
    plan: PhysicalPlan,
    keys: &[BoundOrderKey],
    rows: u64,
) -> PhysicalPlan {
    let PhysicalPlan::Project { input, expressions } = plan else {
        return plan;
    };
    let columns = keys
        .iter()
        .map(|key| match &expressions.get(key.index)?.expr.kind {
            BoundExprKind::Column(column)
                if !key.ascending && key.value_kind == pintail_sql::OrderValueKind::Ordinary =>
            {
                Some(column)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    let input = match (*input, columns) {
        (PhysicalPlan::Scan(mut scan), Some(columns))
            if scan.limit.is_none()
                && scan.table.database_id != pintail_catalog::DatabaseId::new(u64::MAX)
                && !columns.is_empty()
                && columns.len() == scan.table.key_column_ids.len()
                && ordered_by(&scan, &columns) =>
        {
            scan.limit = Some(rows);
            scan.from_end = true;
            PhysicalPlan::Scan(scan)
        }
        (input, _) => input,
    };
    PhysicalPlan::Project {
        input: Box::new(input),
        expressions,
    }
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

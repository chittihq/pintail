//! Grouping a TIMESTAMP column that `MySQL` reads through a source index.
//!
//! Under a date-validation mode `MySQL` groups and deduplicates a
//! `TIMESTAMP` by its wall clock because it copies the value into an
//! intermediate result. When an index on the source table leads with the
//! column and holds every other column the query reads, it walks that index
//! in order instead - a covering index scan for `GROUP BY`, a skip scan for
//! `DISTINCT` and `COUNT(DISTINCT)` - and compares the stored instants.
//!
//! That choice belongs to `MySQL`'s optimizer, so only the narrow shape its
//! plans were observed to take is followed here: one source table with no
//! `WHERE` and no join, whose grouping or distinct column leads a source
//! index, and whose every other column read is in that index or the
//! table's key (which every secondary index carries). Any other shape keeps
//! the copy rule.

use std::cell::RefCell;

use pintail_catalog::{CatalogSnapshot, DatabaseId, TableId};

use crate::bound::{BoundColumn, BoundExpr, BoundExprKind, BoundTable};
use crate::metadata::IndexFacts;

/// One source index, by the catalog's identifiers.
struct SourceIndex {
    database_id: DatabaseId,
    table_id: TableId,
    columns: Vec<u32>,
}

std::thread_local! {
    static SOURCE_INDEXES: RefCell<Vec<SourceIndex>> = const { RefCell::new(Vec::new()) };
}

/// Runs `work` with the source tables' index definitions in scope of the
/// binder, restoring the previous ones afterwards. Indexes on tables or
/// columns `catalog` does not hold are dropped.
pub fn with_source_indexes<T>(
    catalog: &CatalogSnapshot,
    indexes: &[IndexFacts],
    work: impl FnOnce() -> T,
) -> T {
    struct Restore(Vec<SourceIndex>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SOURCE_INDEXES.with(|cell| *cell.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let resolved = indexes
        .iter()
        .filter_map(|index| {
            let database = catalog.database(&index.database)?;
            let table = database.table(&index.table)?;
            let columns = index
                .columns
                .iter()
                .map(|name| table.column(name).map(pintail_types::Column::id))
                .collect::<Option<Vec<_>>>()?;
            Some(SourceIndex {
                database_id: database.id(),
                table_id: table.id(),
                columns,
            })
        })
        .collect();
    let previous = SOURCE_INDEXES.with(|cell| cell.replace(resolved));
    let _restore = Restore(previous);
    work()
}

/// The columns of a source index that `column` leads.
fn leading_index(column: &BoundColumn) -> Option<Vec<u32>> {
    SOURCE_INDEXES.with(|cell| {
        cell.borrow()
            .iter()
            .find(|index| {
                index.database_id == column.database_id
                    && index.table_id == column.table_id
                    && index.columns.first() == Some(&column.column_id)
            })
            .map(|index| index.columns.clone())
    })
}

/// Whether `MySQL` matches two stored TIMESTAMP columns by looking one up
/// in a source index it leads, which compares the stored instants. Without
/// such an index it hashes or compares their session-zone readings.
pub(super) fn looks_up_instants(left: &BoundColumn, right: &BoundColumn) -> bool {
    leading_index(left).is_some() || leading_index(right).is_some()
}

/// Whether `MySQL` reads the TIMESTAMP column `key` of the single source
/// table in `tables` through an index that covers every column `reads`
/// touches, keeping its instants apart.
pub(super) fn reads_instants_through_index<'a>(
    tables: &[BoundTable],
    filtered: bool,
    key: &BoundExpr,
    reads: impl IntoIterator<Item = &'a BoundExpr>,
) -> bool {
    let [table] = tables else {
        return false;
    };
    if filtered || table.input.is_some() {
        return false;
    }
    let Some(BoundExprKind::Column(column)) =
        key.session_timestamp_source().map(|source| &source.kind)
    else {
        return false;
    };
    column.timestamp && reads_column_through_index(tables, filtered, column, reads)
}

/// Whether `MySQL` reads `column` of the single source table in `tables`
/// in the order of a source index it leads, an index that also holds every
/// column `reads` touches: it then compares the stored values as they are,
/// with no intermediate copy.
pub(super) fn reads_column_through_index<'a>(
    tables: &[BoundTable],
    filtered: bool,
    column: &BoundColumn,
    reads: impl IntoIterator<Item = &'a BoundExpr>,
) -> bool {
    let [table] = tables else {
        return false;
    };
    if filtered || table.input.is_some() || column.table_id != table.table_id {
        return false;
    }
    let Some(index) = leading_index(column) else {
        return false;
    };
    let covered = |id: u32| table.key_column_ids.contains(&id) || index.contains(&id);
    reads
        .into_iter()
        .all(|expr| columns_read(expr).is_some_and(|ids| ids.into_iter().all(covered)))
}

/// The source column IDs an expression reads, or `None` for one that reads
/// more than columns and constants (a subquery, a window).
fn columns_read(expr: &BoundExpr) -> Option<Vec<u32>> {
    fn walk(expr: &BoundExpr, out: &mut Vec<u32>) -> bool {
        match &expr.kind {
            BoundExprKind::Column(column) => {
                out.push(column.column_id);
                true
            }
            // Aggregates are checked on their own arguments.
            BoundExprKind::Literal(_)
            | BoundExprKind::Aggregate(_)
            | BoundExprKind::GroupKey(_) => true,
            BoundExprKind::Unary { expr, .. } | BoundExprKind::IsNull { expr, .. } => {
                walk(expr, out)
            }
            BoundExprKind::Binary { left, right, .. } => walk(left, out) && walk(right, out),
            BoundExprKind::Scalar { args, .. } => args.iter().all(|argument| walk(argument, out)),
            _ => false,
        }
    }
    let mut out = Vec::new();
    walk(expr, &mut out).then_some(out)
}

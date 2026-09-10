//! Joins that keep their driving input's key order.
//!
//! `ORDER BY a.id LIMIT n` over `a JOIN b ON b.id = a.x` needs only the
//! first n rows of `a` in key order and the `b` row each of them names. A
//! hash join builds all of `b`, and the top-k sort above it reads all of
//! `a`, before the limit applies. This join streams `a` in the order its
//! scan already produces, reads the `b` rows each slice of `a` names by
//! `b`'s primary key, and emits in `a`'s order, so the limit above it stops
//! both inputs after a few rows.

use std::collections::{HashMap, VecDeque};

use pintail_sql::{BinaryOp, BoundColumn, BoundExpr, BoundExprKind, BoundJoinKind, BoundOrderKey};
use pintail_types::{DataType, Value};

use super::{
    Collation, CompiledExpr, ExecError, MemoryTracker, PhysicalPlan, PullOperator, RecordBatch,
    Scan, ScanProvider, build_operator, build_operator_inner, estimated_record_batch_bytes,
    estimated_row_payload_bytes, filtered, predicate_truth, rows_to_columns,
};

/// Driving rows joined in the first lookup round. Each round doubles it up
/// to `LAST_SLICE`, so a small limit reads a few keys and a long run still
/// spreads each round's reads over many rows.
const FIRST_SLICE: usize = 16;
const LAST_SLICE: usize = 4096;

/// Keys this close share one ranged read rather than one read each.
const RANGE_GAP: i128 = 256;

/// Keys falling in more ranges than this are scattered over the table. A
/// read costs about a block whatever it returns, so the table is read once
/// rather than piecemeal.
const MAX_RANGES: usize = 4;

/// Lookup rows the join may read by range before reading the whole table
/// once is the cheaper plan, when the table's size is unknown.
const UNKNOWN_TABLE_ROWS: u64 = 1 << 20;

type Matches = HashMap<i128, Vec<Vec<Value>>>;

fn integer_type(data_type: Option<DataType>) -> bool {
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

fn unsigned_type(data_type: Option<DataType>) -> bool {
    matches!(
        data_type,
        Some(DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64)
    )
}

fn integer_key(value: &Value) -> Option<i128> {
    match value {
        Value::Int64(value) => Some(i128::from(*value)),
        Value::UInt64(value) => Some(i128::from(*value)),
        _ => None,
    }
}

/// The scan under at most one filter: either way its rows arrive in the
/// table's key order.
fn base_scan(plan: &PhysicalPlan) -> Option<&Scan> {
    match plan {
        PhysicalPlan::Scan(scan) => Some(scan),
        PhysicalPlan::Filter { input, .. } => match input.as_ref() {
            PhysicalPlan::Scan(scan) => Some(scan),
            _ => None,
        },
        _ => None,
    }
}

fn names_column(scan: &Scan, column: &BoundColumn, column_id: u32) -> bool {
    column.database_id == scan.table.database_id
        && column.table_id == scan.table.table_id
        && column.column_id == column_id
        && column
            .relation_name
            .eq_ignore_ascii_case(&scan.table.relation_name)
}

/// Whether the scan's key order is the order of `columns`: they name its
/// leading key columns in turn, and each is an integer, whose stored order
/// is its numeric order. Text keys are stored in byte order, which a
/// collation need not follow.
fn ordered_by(scan: &Scan, columns: &[&BoundColumn]) -> bool {
    columns.len() <= scan.table.key_column_ids.len()
        && columns
            .iter()
            .zip(&scan.table.key_column_ids)
            .all(|(column, key)| {
                names_column(scan, column, *key) && integer_type(Some(column.data_type))
            })
}

/// Whether `key` is the scan's whole primary key, as one integer column.
fn found_by_key(scan: &Scan, key: &BoundExpr) -> bool {
    let [key_column] = scan.table.key_column_ids.as_slice() else {
        return false;
    };
    integer_type(key.data_type)
        && matches!(
            &key.kind,
            BoundExprKind::Column(column) if names_column(scan, column, *key_column)
        )
}

/// Which input of the join under `plan` can drive a key lookup that yields
/// the order of `keys`: `Some(true)` for the left.
fn driving_side(plan: &PhysicalPlan, keys: &[BoundOrderKey], trim: usize) -> Option<bool> {
    let PhysicalPlan::Project { input, expressions } = plan else {
        return None;
    };
    let PhysicalPlan::HashJoin {
        left,
        right,
        kind,
        left_key,
        extra_keys,
        right_key,
        ..
    } = input.as_ref()
    else {
        return None;
    };
    if keys.is_empty() || !extra_keys.is_empty() || trim > expressions.len() {
        return None;
    }
    let columns = keys
        .iter()
        .map(|key| match &expressions.get(key.index)?.expr.kind {
            BoundExprKind::Column(column) if key.ascending => Some(column),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    [
        (true, left, left_key, right, right_key),
        (false, right, right_key, left, left_key),
    ]
    .into_iter()
    .find_map(|(driving_left, driving, driving_key, lookup, lookup_key)| {
        // A left join keeps every left row, so only the left can drive it.
        let kind_allows = match kind {
            BoundJoinKind::Inner => true,
            BoundJoinKind::Left => driving_left,
            _ => false,
        };
        let (driving, lookup) = (base_scan(driving)?, base_scan(lookup)?);
        (kind_allows
            && ordered_by(driving, &columns)
            && integer_type(driving_key.data_type)
            && found_by_key(lookup, lookup_key))
        .then_some(driving_left)
    })
}

/// The sort input as a plan already in the order of `keys`, and `true`,
/// when it is a projection over a join whose driving side is a scan ordered
/// by those keys and whose other side is found by its whole primary key.
/// The projection drops the `trim` columns only the sort read. Any other
/// input comes back unchanged, with `false`.
pub(super) fn ordered_input(
    input: PhysicalPlan,
    keys: &[BoundOrderKey],
    trim: usize,
) -> (PhysicalPlan, bool) {
    let Some(driving_left) = driving_side(&input, keys, trim) else {
        return (input, false);
    };
    match input {
        PhysicalPlan::Project {
            input: join,
            mut expressions,
        } => match *join {
            PhysicalPlan::HashJoin {
                left,
                right,
                kind,
                left_key,
                right_key,
                residual,
                ..
            } => {
                let (driving_key, lookup_key) = if driving_left {
                    (left_key, right_key)
                } else {
                    (right_key, left_key)
                };
                expressions.truncate(expressions.len().saturating_sub(trim));
                let join = PhysicalPlan::KeyLookupJoin {
                    left,
                    right,
                    kind,
                    driving_left,
                    driving_key,
                    lookup_key,
                    residual,
                };
                (
                    PhysicalPlan::Project {
                        input: Box::new(join),
                        expressions,
                    },
                    true,
                )
            }
            other => (
                PhysicalPlan::Project {
                    input: Box::new(other),
                    expressions,
                },
                false,
            ),
        },
        other => (other, false),
    }
}

/// A key lookup join's parts, as the physical plan carries them.
pub(super) struct Inputs {
    pub(super) left: PhysicalPlan,
    pub(super) right: PhysicalPlan,
    pub(super) kind: BoundJoinKind,
    pub(super) driving_left: bool,
    pub(super) driving_key: BoundExpr,
    pub(super) lookup_key: BoundExpr,
    pub(super) residual: Option<BoundExpr>,
}

pub(super) fn build(
    inputs: Inputs,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<(PullOperator, Vec<BoundColumn>), ExecError> {
    const NOT_A_SCAN: ExecError = ExecError::InvalidPhysicalPlan("a key lookup reads a table scan");
    let Inputs {
        left,
        right,
        kind,
        driving_left,
        driving_key,
        lookup_key,
        residual,
    } = inputs;
    let (driving_plan, lookup_plan) = if driving_left {
        (left, right)
    } else {
        (right, left)
    };
    let (scan, filter) = match lookup_plan {
        PhysicalPlan::Scan(scan) => (scan, None),
        PhysicalPlan::Filter { input, predicate } => match *input {
            PhysicalPlan::Scan(scan) => (scan, Some(predicate)),
            _ => return Err(NOT_A_SCAN),
        },
        _ => return Err(NOT_A_SCAN),
    };
    let lookup_columns = scan
        .projected_column_ids
        .iter()
        .map(|id| {
            scan.table
                .columns
                .iter()
                .find(|column| column.column_id == *id)
                .cloned()
                .ok_or(ExecError::InvalidPhysicalPlan(
                    "scan projection references an unknown stable column ID",
                ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let lookup_position = CompiledExpr::compile(&lookup_key, &lookup_columns, collation)?
        .column_index()
        .ok_or(ExecError::InvalidPhysicalPlan(
            "a key lookup finds rows by a key column",
        ))?;
    let (driving, driving_columns) = build_operator(driving_plan, provider, memory, collation)?;
    let driving_key = CompiledExpr::compile(&driving_key, &driving_columns, collation)?;
    let lookup_width = lookup_columns.len();
    let output_columns = if driving_left {
        driving_columns
            .into_iter()
            .chain(lookup_columns)
            .collect::<Vec<_>>()
    } else {
        lookup_columns
            .into_iter()
            .chain(driving_columns)
            .collect::<Vec<_>>()
    };
    let residual = residual
        .map(|predicate| CompiledExpr::compile(&predicate, &output_columns, collation))
        .transpose()?;
    let column_types = output_columns
        .iter()
        .map(|column| column.data_type)
        .collect();
    let lookup = lookup_source(scan, filter, lookup_key, provider, memory, collation)?;
    Ok((
        PullOperator::KeyLookupJoin(Box::new(KeyLookupJoin {
            driving: Box::new(driving),
            driving_key,
            pending: VecDeque::new(),
            driving_done: false,
            lookup,
            lookup_position,
            lookup_width,
            kind,
            driving_left,
            residual,
            column_types,
            slice: FIRST_SLICE,
            collation,
        })),
        output_columns,
    ))
}

pub(super) struct KeyLookupJoin {
    driving: Box<PullOperator>,
    driving_key: CompiledExpr,
    /// Driving rows pulled but not yet joined, each with its lookup key.
    pending: VecDeque<(Option<i128>, Vec<Value>)>,
    driving_done: bool,
    lookup: Lookup,
    /// Where the key sits among the lookup side's columns.
    lookup_position: usize,
    lookup_width: usize,
    kind: BoundJoinKind,
    driving_left: bool,
    residual: Option<CompiledExpr>,
    column_types: Vec<DataType>,
    /// Driving rows the next round joins.
    slice: usize,
    collation: Collation,
}

impl KeyLookupJoin {
    pub(super) fn next_batch(
        &mut self,
        memory: &MemoryTracker,
    ) -> Result<Option<RecordBatch>, ExecError> {
        loop {
            self.fill(memory)?;
            if self.pending.is_empty() {
                return Ok(None);
            }
            let take = self.slice.min(self.pending.len());
            self.slice = self.slice.saturating_mul(2).min(LAST_SLICE);
            let slice = self.pending.drain(..take).collect::<Vec<_>>();
            let mut keys = slice.iter().filter_map(|(key, _)| *key).collect::<Vec<_>>();
            keys.sort_unstable();
            keys.dedup();
            let found = self
                .lookup
                .find(&keys, self.lookup_position, memory, self.collation)?;
            let shape = Shape {
                kind: self.kind,
                driving_left: self.driving_left,
                lookup_width: self.lookup_width,
                residual: self.residual.as_ref(),
                column_types: &self.column_types,
            };
            let rows = join_slice(&slice, &found, &shape)?;
            if rows.is_empty() {
                continue;
            }
            memory
                .ensure_transient(estimated_record_batch_bytes(&rows, self.column_types.len()))?;
            let columns = rows_to_columns(&rows, &self.column_types)?;
            return Ok(Some(RecordBatch::new(rows.len(), columns)?));
        }
    }

    /// Pulls driving batches until the next round's rows are pending or the
    /// input has ended.
    fn fill(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        while !self.driving_done && self.pending.len() < self.slice {
            let Some(batch) = self.driving.next_batch(memory)? else {
                self.driving_done = true;
                break;
            };
            for row in batch.selection().selected_rows() {
                let key = integer_key(&self.driving_key.evaluate(&batch, row)?);
                self.pending.push_back((key, row_values(&batch, row)?));
            }
        }
        memory.ensure_transient(
            self.pending
                .iter()
                .map(|(_, row)| estimated_row_payload_bytes(row))
                .fold(0_usize, usize::saturating_add),
        )
    }
}

fn row_values(batch: &RecordBatch, row: usize) -> Result<Vec<Value>, ExecError> {
    batch
        .columns()
        .iter()
        .map(|column| {
            column.value_owned(row).ok_or(ExecError::InvalidBatch(
                "join input row is outside its batch",
            ))
        })
        .collect()
}

fn row_key(batch: &RecordBatch, row: usize, position: usize) -> Option<i128> {
    batch
        .column(position)
        .and_then(|column| column.value(row))
        .and_then(integer_key)
}

enum Lookup {
    Ranged(Box<RangedLookup>),
    /// The lookup side read in full on first use: when the provider cannot
    /// open ranged reads, or once they have read as many rows as the table
    /// holds.
    Built {
        input: Option<Box<PullOperator>>,
        rows: Matches,
    },
}

/// The lookup rows a round found: read for it, or kept from the full read.
enum Found<'a> {
    Read(Matches),
    Built(&'a Matches),
}

impl Found<'_> {
    fn get(&self, key: i128) -> &[Vec<Value>] {
        let rows = match self {
            Self::Read(rows) => rows.get(&key),
            Self::Built(rows) => rows.get(&key),
        };
        rows.map_or(&[], Vec::as_slice)
    }
}

impl Lookup {
    fn find(
        &mut self,
        keys: &[i128],
        position: usize,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<Found<'_>, ExecError> {
        if let Self::Ranged(ranged) = self {
            if ranged.read <= ranged.budget && key_ranges(keys, ranged.unsigned).len() <= MAX_RANGES
            {
                return ranged
                    .read_keys(keys, position, memory, collation)
                    .map(Found::Read);
            }
            // Scattered keys, or ranged reads that have cost a full read of
            // the table already: read it once and answer from that.
            let input = ranged.read_all(memory, collation)?;
            *self = Self::Built {
                input: Some(Box::new(input)),
                rows: Matches::new(),
            };
        }
        let Self::Built { input, rows } = self else {
            return Err(ExecError::InvalidPhysicalPlan(
                "a key lookup lost its table",
            ));
        };
        if let Some(mut input) = input.take() {
            *rows = read_table(&mut input, position, memory)?;
        }
        Ok(Found::Built(rows))
    }
}

struct RangedLookup {
    provider: Box<dyn ScanProvider + Send + Sync>,
    scan: Scan,
    filter: Option<BoundExpr>,
    key: BoundExpr,
    unsigned: bool,
    /// Lookup rows read so far, against the table's size.
    read: u64,
    budget: u64,
}

impl RangedLookup {
    fn plan(&self, scan: Scan) -> PhysicalPlan {
        filtered(PhysicalPlan::Scan(scan), self.filter.clone())
    }

    /// The rows whose key is in `keys`, read by ranges of the table's key.
    fn read_keys(
        &mut self,
        keys: &[i128],
        position: usize,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<Matches, ExecError> {
        let mut found = Matches::new();
        for (low, high) in key_ranges(keys, self.unsigned) {
            let mut scan = self.scan.clone();
            scan.predicates.extend([
                bound(&self.key, BinaryOp::GreaterOrEqual, low),
                bound(&self.key, BinaryOp::LessOrEqual, high),
            ]);
            // Each read's scan is dropped before the next one opens, and
            // what it still holds goes back to the budget with it.
            let held = memory.used();
            let (mut input, _) =
                build_operator_inner(self.plan(scan), self.provider.as_ref(), memory, collation)?;
            while let Some(batch) = input.next_batch(memory)? {
                for row in batch.selection().selected_rows() {
                    self.read = self.read.saturating_add(1);
                    if let Some(key) = row_key(&batch, row, position)
                        && keys.binary_search(&key).is_ok()
                    {
                        found.entry(key).or_default().push(row_values(&batch, row)?);
                    }
                }
            }
            drop(input);
            memory.release(memory.used().saturating_sub(held));
        }
        Ok(found)
    }

    fn read_all(
        &self,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<PullOperator, ExecError> {
        Ok(build_operator_inner(
            self.plan(self.scan.clone()),
            self.provider.as_ref(),
            memory,
            collation,
        )?
        .0)
    }
}

fn read_table(
    input: &mut PullOperator,
    position: usize,
    memory: &MemoryTracker,
) -> Result<Matches, ExecError> {
    let mut rows = Matches::new();
    while let Some(batch) = input.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            let Some(key) = row_key(&batch, row, position) else {
                continue;
            };
            let values = row_values(&batch, row)?;
            memory.reserve(estimated_row_payload_bytes(&values))?;
            rows.entry(key).or_default().push(values);
        }
    }
    Ok(rows)
}

/// Sorted, distinct keys grouped into the ranges one read each covers: keys
/// closer than `RANGE_GAP` share a read. A key the column cannot hold
/// matches nothing and is dropped.
fn key_ranges(keys: &[i128], unsigned: bool) -> Vec<(i128, i128)> {
    let held = if unsigned {
        0..=i128::from(u64::MAX)
    } else {
        i128::from(i64::MIN)..=i128::from(i64::MAX)
    };
    let mut ranges: Vec<(i128, i128)> = Vec::new();
    for &key in keys.iter().filter(|key| held.contains(*key)) {
        match ranges.last_mut() {
            Some((_, high)) if key - *high <= RANGE_GAP => *high = key,
            _ => ranges.push((key, key)),
        }
    }
    ranges
}

/// `key <op> value`, as a scan predicate storage prunes its key range by.
fn bound(key: &BoundExpr, op: BinaryOp, value: i128) -> BoundExpr {
    let (value, data_type) = match i64::try_from(value) {
        Ok(value) => (Value::Int64(value), DataType::Int64),
        Err(_) => (
            Value::UInt64(u64::try_from(value).unwrap_or(u64::MAX)),
            DataType::UInt64,
        ),
    };
    BoundExpr {
        data_type: Some(DataType::Boolean),
        nullable: false,
        kind: BoundExprKind::Binary {
            op,
            left: Box::new(key.clone()),
            right: Box::new(BoundExpr {
                data_type: Some(data_type),
                nullable: false,
                kind: BoundExprKind::Literal(value),
            }),
        },
    }
}

/// What joining a slice needs from the operator.
struct Shape<'a> {
    kind: BoundJoinKind,
    driving_left: bool,
    lookup_width: usize,
    residual: Option<&'a CompiledExpr>,
    column_types: &'a [DataType],
}

fn combine(driving: &[Value], lookup: Option<&[Value]>, shape: &Shape<'_>) -> Vec<Value> {
    let nulls = if lookup.is_none() {
        vec![Value::Null; shape.lookup_width]
    } else {
        Vec::new()
    };
    let lookup = lookup.unwrap_or(&nulls);
    let (first, second) = if shape.driving_left {
        (driving, lookup)
    } else {
        (lookup, driving)
    };
    first.iter().chain(second).cloned().collect()
}

/// Joins a slice of driving rows, in their order, to the lookup rows found
/// for their keys.
fn join_slice(
    slice: &[(Option<i128>, Vec<Value>)],
    found: &Found<'_>,
    shape: &Shape<'_>,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut owners = Vec::new();
    let mut rows = Vec::new();
    for (index, (key, driving)) in slice.iter().enumerate() {
        for lookup in key.map_or(&[][..], |key| found.get(key)) {
            owners.push(index);
            rows.push(combine(driving, Some(lookup), shape));
        }
    }
    let passed = match shape.residual {
        Some(residual) if !rows.is_empty() => {
            let batch = RecordBatch::new(rows.len(), rows_to_columns(&rows, shape.column_types)?)?;
            (0..rows.len())
                .map(|row| predicate_truth(&residual.evaluate(&batch, row)?))
                .collect::<Result<Vec<_>, ExecError>>()?
        }
        _ => vec![true; rows.len()],
    };
    let mut output = Vec::with_capacity(rows.len());
    let mut candidates = owners.into_iter().zip(passed).zip(rows).peekable();
    for (index, (_, driving)) in slice.iter().enumerate() {
        let mut matched = false;
        while let Some(((_, passed), row)) = candidates.next_if(|((owner, _), _)| *owner == index) {
            if passed {
                output.push(row);
                matched = true;
            }
        }
        if !matched && shape.kind == BoundJoinKind::Left {
            output.push(combine(driving, None, shape));
        }
    }
    Ok(output)
}

/// Where the lookup rows come from: ranged reads through a provider the
/// operator keeps, or one read of the whole lookup side when the provider
/// cannot hand one out.
fn lookup_source(
    scan: Scan,
    filter: Option<BoundExpr>,
    key: BoundExpr,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Lookup, ExecError> {
    Ok(
        match provider.table_provider(scan.table.database_id, scan.table.table_id) {
            Some(tables) => Lookup::Ranged(Box::new(RangedLookup {
                budget: scan
                    .table
                    .row_count
                    .or(scan.table.estimated_rows)
                    .unwrap_or(UNKNOWN_TABLE_ROWS),
                unsigned: unsigned_type(key.data_type),
                provider: tables,
                scan,
                filter,
                key,
                read: 0,
            })),
            None => Lookup::Built {
                input: Some(Box::new(
                    build_operator(
                        filtered(PhysicalPlan::Scan(scan), filter),
                        provider,
                        memory,
                        collation,
                    )?
                    .0,
                )),
                rows: Matches::new(),
            },
        },
    )
}

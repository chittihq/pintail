//! Query-owned external membership, installed after physical planning.

use super::{
    Collation, ExecError, Execution, LogicalPlanner, MemoryTracker, Optimizer, PhysicalPlanner,
    ScanProvider, spill,
};
use pintail_sql::{BoundQuery, MembershipError, MembershipLookup, PreparedMembership};
use pintail_types::{DataType, Value};
use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};

const PARTITIONS: usize = 64;

pub(super) enum MaterializedMembership {
    Memory(Vec<Value>),
    /// A lookup the query owns, and the bytes it holds for the query's
    /// lifetime: a spilled set read back by partition, or a hashed index
    /// over a set collected in memory.
    Prepared(PreparedMembership, usize),
}

#[derive(Debug, Clone, Copy)]
enum BucketMode {
    Integer,
    Text,
    Common,
}

/// One partition, either still on disk or already decoded into memory.
/// A partition is a 64th of a set that spilled at a quarter of the query
/// ceiling, so the resident form is bounded by construction; the file scan
/// stays for the partition that still refuses to fit.
enum Partition {
    Absent,
    Resident(Vec<Value>),
    OnDisk(spill::ClosedRun),
}

struct ExternalMembership {
    /// Each partition behind its own lock: probes on different partitions
    /// never wait for each other, and the first probe on one loads it while
    /// later probes on it read the decoded values.
    runs: Vec<std::sync::RwLock<Partition>>,
    /// One promoter per partition. Without this every probe that arrives
    /// before the first one finishes builds its own copy of the same
    /// partition, so a scan's worth of threads holds that many copies at
    /// once; the rest answer from the file and keep nothing.
    promoting: Vec<std::sync::atomic::AtomicBool>,
    mode: BucketMode,
    collation: Collation,
    exact_decimal: bool,
    saw_null: bool,
    /// Bytes the resident partitions hold, against the budget reserved for
    /// this set at construction.
    resident_bytes: std::sync::atomic::AtomicUsize,
    resident_budget: usize,
    interruption: MemoryTracker,
}

impl std::fmt::Debug for ExternalMembership {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalMembership")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// A member's equality key, for the modes whose comparison it decides:
/// integers by value whatever their signedness, text by its collation
/// weights. Values outside the mode have none and are compared one by one.
#[derive(PartialEq, Eq, Hash)]
enum MemberKey {
    Integer(i128),
    Text(String),
}

fn member_key(value: &Value, mode: BucketMode, collation: Collation) -> Option<MemberKey> {
    match (mode, value) {
        (BucketMode::Integer, Value::Int64(value)) => Some(MemberKey::Integer(i128::from(*value))),
        (BucketMode::Integer, Value::UInt64(value)) => Some(MemberKey::Integer(i128::from(*value))),
        (BucketMode::Text, Value::Utf8(value) | Value::Enum { label: value, .. }) => Some(
            MemberKey::Text(super::join::normalized_collation_text(value, collation)),
        ),
        (BucketMode::Text, Value::DecimalAverage(value)) => Some(MemberKey::Text(
            super::join::normalized_collation_text(&value.label, collation),
        )),
        _ => None,
    }
}

fn bucket(value: &Value, mode: BucketMode, collation: Collation) -> usize {
    let Some(key) = member_key(value, mode, collation) else {
        return 0;
    };
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hash);
    usize::try_from(hash.finish() % PARTITIONS as u64).expect("partition fits usize")
}

fn bucket_mode(needle_type: Option<DataType>, value_type: Option<DataType>) -> BucketMode {
    if integer_type(needle_type) && integer_type(value_type) {
        BucketMode::Integer
    } else if needle_type == Some(DataType::Utf8) && value_type == Some(DataType::Utf8) {
        BucketMode::Text
    } else {
        BucketMode::Common
    }
}

/// Compares the needle against one member exactly as the literal list does.
fn member_matches(
    needle: &Value,
    value: Value,
    exact_decimal: bool,
    collation: Collation,
) -> Result<bool, MembershipError> {
    crate::expression::evaluate_in_list(&[needle.clone(), value], false, exact_decimal, collation)
        .map_err(lookup_error)
        .map(|outcome| outcome == Value::Boolean(true))
}

impl ExternalMembership {
    /// Compares the needle against one already-decoded value.
    fn matches(&self, needle: &Value, value: Value) -> Result<bool, MembershipError> {
        member_matches(needle, value, self.exact_decimal, self.collation)
    }

    /// Reads one partition's file, answering the probe as it goes and,
    /// when `promote` is set, keeping the values for the probes that
    /// follow. Every kept value is charged before it is kept, so a
    /// partition larger than the budget is never built in memory just to
    /// be thrown away; such a partition hands back what it claimed and
    /// keeps answering from its file. Correctness never depends on a
    /// promotion happening.
    fn scan_file(
        &self,
        run: &spill::ClosedRun,
        needle: &Value,
        promote: bool,
    ) -> Result<(bool, Option<Vec<Value>>), MembershipError> {
        let mut reader = run
            .open()
            .map_err(|error| MembershipError::Failed(error.to_string()))?;
        let mut found = false;
        let mut collected: Option<Vec<Value>> = promote.then(Vec::new);
        let mut claimed = 0_usize;
        while let Some(payload) = reader
            .next()
            .map_err(|error| MembershipError::Failed(error.to_string()))?
        {
            if let Err(error) = self.interruption.check_interruption() {
                self.release_resident(claimed);
                return Err(lookup_error(error));
            }
            let value = spill::Decoder::new(payload)
                .value()
                .map_err(MembershipError::Failed)?;
            if !found {
                match self.matches(needle, value.clone()) {
                    Ok(true) => found = true,
                    Ok(false) => {}
                    Err(error) => {
                        self.release_resident(claimed);
                        return Err(error);
                    }
                }
                // Nothing left to learn from the rest of the file unless a
                // promotion still wants it.
                if found && collected.is_none() {
                    return Ok((true, None));
                }
            }
            if let Some(values) = collected.as_mut() {
                let bytes = size_of::<Value>().saturating_add(value.heap_bytes());
                if self.claim_resident(bytes) {
                    claimed = claimed.saturating_add(bytes);
                    values.push(value);
                } else {
                    // The budget is spent: this partition stays on disk.
                    self.release_resident(claimed);
                    claimed = 0;
                    collected = None;
                    if found {
                        return Ok((true, None));
                    }
                }
            }
        }
        Ok((found, collected))
    }

    /// Claims `bytes` of the budget left for resident partitions.
    fn claim_resident(&self, bytes: usize) -> bool {
        self.resident_bytes
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |used| {
                    let wanted = used.saturating_add(bytes);
                    (wanted <= self.resident_budget).then_some(wanted)
                },
            )
            .is_ok()
    }

    fn release_resident(&self, bytes: usize) {
        if bytes > 0 {
            self.resident_bytes
                .fetch_sub(bytes, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl MembershipLookup for ExternalMembership {
    fn lookup(&self, needle: &Value) -> Result<Value, MembershipError> {
        self.interruption
            .check_interruption()
            .map_err(lookup_error)?;
        if matches!(needle, Value::Null) {
            return Ok(Value::Null);
        }
        let index = bucket(needle, self.mode, self.collation);
        let slot = &self.runs[index];
        let found = {
            let partition = slot
                .read()
                .map_err(|_| MembershipError::Failed("membership lock poisoned".to_owned()))?;
            match &*partition {
                Partition::Absent => false,
                // The common case after the first probe on this partition:
                // a memory scan of a 64th of the set, with no lock held by
                // any other partition's probes.
                Partition::Resident(values) => {
                    let mut found = false;
                    for value in values {
                        self.interruption
                            .check_interruption()
                            .map_err(lookup_error)?;
                        if self.matches(needle, value.clone())? {
                            found = true;
                            break;
                        }
                    }
                    found
                }
                // First probe on this partition: answer from the file, and
                // decode the rest of it for the probes that follow. The
                // whole set was written under a quarter of the ceiling, so
                // one partition of it is small; a partition that still does
                // not fit keeps answering from disk.
                Partition::OnDisk(run) => {
                    // One probe per partition builds the resident copy.
                    let promote =
                        !self.promoting[index].swap(true, std::sync::atomic::Ordering::Relaxed);
                    let (found, values) = self.scan_file(run, needle, promote)?;
                    if let Some(values) = values {
                        let bytes = values
                            .iter()
                            .map(|value| size_of::<Value>().saturating_add(value.heap_bytes()))
                            .fold(0_usize, usize::saturating_add);
                        drop(partition);
                        let mut partition = slot.write().map_err(|_| {
                            MembershipError::Failed("membership lock poisoned".to_owned())
                        })?;
                        if matches!(&*partition, Partition::OnDisk(_)) {
                            *partition = Partition::Resident(values);
                        } else {
                            // Nothing to install after all; the bytes this
                            // copy claimed go back rather than leaking out
                            // of the budget for the query's lifetime.
                            self.release_resident(bytes);
                        }
                    }
                    found
                }
            }
        };
        Ok(if found {
            Value::Boolean(true)
        } else if self.saw_null {
            Value::Null
        } else {
            Value::Boolean(false)
        })
    }
}

/// Marks the end of a key's chain of members.
const CHAIN_END: usize = usize::MAX;

/// Sets up to this size stay a literal list: comparing a probe against a
/// few members costs less than hashing it, and a literal list keeps the
/// expression's signature for the settled-result memo.
const LITERAL_MEMBERS: usize = 64;

/// A set collected in memory, indexed by its members' equality keys.
///
/// A literal list compares every probe against every member, so a scan of
/// many rows against a large subquery result costs rows times members. The
/// index answers a probe from the members that share its key. A key hit is
/// still confirmed by the list's own comparison, and members or probes
/// without a key are compared one by one, so the answer is always the
/// list's.
struct HashedMembership {
    values: Vec<Value>,
    /// The latest member per key; `chain` links it to the earlier ones.
    heads: std::collections::HashMap<MemberKey, usize>,
    chain: Vec<usize>,
    unkeyed: Vec<usize>,
    mode: BucketMode,
    collation: Collation,
    saw_null: bool,
}

impl std::fmt::Debug for HashedMembership {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HashedMembership")
            .field("mode", &self.mode)
            .field("members", &self.values.len())
            .finish_non_exhaustive()
    }
}

impl HashedMembership {
    fn any_of(
        &self,
        needle: &Value,
        members: impl IntoIterator<Item = usize>,
    ) -> Result<bool, MembershipError> {
        for index in members {
            // Both indexed modes compare integers or text, never an exact
            // decimal.
            if member_matches(needle, self.values[index].clone(), false, self.collation)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn keyed(&self, key: &MemberKey) -> impl Iterator<Item = usize> + '_ {
        std::iter::successors(self.heads.get(key).copied(), |&index| {
            Some(self.chain[index]).filter(|&next| next != CHAIN_END)
        })
    }
}

impl MembershipLookup for HashedMembership {
    fn lookup(&self, needle: &Value) -> Result<Value, MembershipError> {
        if matches!(needle, Value::Null) {
            return Ok(Value::Null);
        }
        let found = match member_key(needle, self.mode, self.collation) {
            Some(key) => {
                self.any_of(needle, self.keyed(&key))?
                    || self.any_of(needle, self.unkeyed.iter().copied())?
            }
            None => self.any_of(needle, 0..self.values.len())?,
        };
        Ok(if found {
            Value::Boolean(true)
        } else if self.saw_null {
            Value::Null
        } else {
            Value::Boolean(false)
        })
    }
}

/// Indexes a set collected in memory when its members have an equality key
/// and there are enough of them to be worth hashing; any other set comes
/// back as the literal list it was. The bytes returned are what the index
/// holds for the query's lifetime.
pub(super) fn index_members(
    mut values: Vec<Value>,
    collation: Collation,
    needle_type: Option<DataType>,
    value_type: Option<DataType>,
) -> MaterializedMembership {
    let mode = bucket_mode(needle_type, value_type);
    if values.len() <= LITERAL_MEMBERS || matches!(mode, BucketMode::Common) {
        return MaterializedMembership::Memory(values);
    }
    // NULL is never a member; it only turns a miss into NULL.
    let collected = values.len();
    values.retain(|value| !matches!(value, Value::Null));
    let saw_null = values.len() != collected;
    let mut heads = std::collections::HashMap::with_capacity(values.len());
    let mut chain = vec![CHAIN_END; values.len()];
    let mut unkeyed = Vec::new();
    let mut key_bytes = 0_usize;
    for (index, value) in values.iter().enumerate() {
        let Some(key) = member_key(value, mode, collation) else {
            unkeyed.push(index);
            continue;
        };
        match heads.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut head) => {
                chain[index] = head.insert(index);
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                if let MemberKey::Text(text) = slot.key() {
                    key_bytes = key_bytes.saturating_add(text.capacity());
                }
                slot.insert(index);
            }
        }
    }
    let bytes = values
        .capacity()
        .saturating_mul(size_of::<Value>())
        .saturating_add(
            values
                .iter()
                .map(Value::heap_bytes)
                .fold(0_usize, usize::saturating_add),
        )
        .saturating_add(
            heads
                .capacity()
                .saturating_mul(size_of::<(MemberKey, usize)>().saturating_add(1)),
        )
        .saturating_add(key_bytes)
        .saturating_add(
            chain
                .len()
                .saturating_add(unkeyed.capacity())
                .saturating_mul(size_of::<usize>()),
        );
    MaterializedMembership::Prepared(
        PreparedMembership(Arc::new(HashedMembership {
            values,
            heads,
            chain,
            unkeyed,
            mode,
            collation,
            saw_null,
        })),
        bytes,
    )
}

fn lookup_error(error: ExecError) -> MembershipError {
    match error {
        ExecError::QueryCancelled => MembershipError::Cancelled,
        ExecError::QueryTimedOut => MembershipError::TimedOut,
        other => MembershipError::Failed(other.to_string()),
    }
}

pub(crate) fn execution_error(error: MembershipError) -> ExecError {
    match error {
        MembershipError::Cancelled => ExecError::QueryCancelled,
        MembershipError::TimedOut => ExecError::QueryTimedOut,
        MembershipError::Failed(message) => ExecError::Source(message),
    }
}

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
                | DataType::Year
        )
    )
}

fn append_value(
    runs: &mut [Option<spill::AppendRun>],
    value: &Value,
    mode: BucketMode,
    collation: Collation,
    query_spill: &spill::QuerySpill,
) -> Result<(), ExecError> {
    if matches!(value, Value::Null) {
        return Ok(());
    }
    let slot = &mut runs[bucket(value, mode, collation)];
    if slot.is_none() {
        *slot = Some(
            spill::AppendRun::create("pintail-membership-", query_spill)
                .map_err(|error| ExecError::Source(error.to_string()))?,
        );
    }
    let mut encoder = spill::Encoder::new();
    encoder.value(value);
    let mut framed = Vec::new();
    spill::write_record(&mut framed, &encoder.finish())
        .map_err(|error| ExecError::Source(error.to_string()))?;
    slot.as_mut()
        .expect("created above")
        .flush(&framed, 1)
        .map_err(|error| ExecError::Source(error.to_string()))
}

// The bound query and its comparison types travel with the query's storage
// budget and spill scope; callers retain either literals or one owned index.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn materialize_membership(
    query: BoundQuery,
    provider: &dyn ScanProvider,
    limit: usize,
    deadline: Option<std::time::Instant>,
    collation: Collation,
    query_spill: &spill::QuerySpill,
    needle_type: Option<DataType>,
    value_type: Option<DataType>,
) -> Result<MaterializedMembership, ExecError> {
    super::DEPENDENT_SUBQUERY_EXECUTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let plan = PhysicalPlanner::plan(Optimizer::optimize(LogicalPlanner::plan(query)), collation)?;
    if plan.output_fields().len() != 1 {
        return Err(ExecError::InvalidPhysicalPlan(
            "IN subquery must produce exactly one column",
        ));
    }
    let mut execution =
        Execution::start_with_deadline(plan, provider, limit / 2, deadline, collation)?;
    let memory = MemoryTracker::with_deadline(limit, deadline);
    let mode = bucket_mode(needle_type, value_type);
    let exact_type = |value| {
        integer_type(value) || matches!(value, Some(DataType::Boolean | DataType::Decimal { .. }))
    };
    let exact_decimal = exact_type(needle_type)
        && exact_type(value_type)
        && [needle_type, value_type]
            .iter()
            .any(|value| matches!(value, Some(DataType::Decimal { .. })));
    let mut values = Vec::new();
    let mut reserved = 0_usize;
    let mut runs = (0..PARTITIONS)
        .map(|_| None)
        .collect::<Vec<Option<spill::AppendRun>>>();
    let mut external = false;
    let mut saw_null = false;
    let mut largest = 0;
    while let Some(batch) = execution.next_batch()? {
        // Charged once per batch: the batch's value cache is built by the
        // first read, and summing it again for every row made collecting a
        // set that arrives in one batch quadratic in its size.
        let mut batch_bytes = None;
        for row in batch.selection().selected_rows() {
            let value = batch
                .column(0)
                .and_then(|column| column.value(row))
                .ok_or(ExecError::InvalidBatch("membership value missing"))?;
            let bytes = size_of::<Value>().saturating_add(value.heap_bytes());
            largest = largest.max(bytes);
            saw_null |= matches!(value, Value::Null);
            if !external && reserved.saturating_add(bytes) > limit / 4 {
                for value in &values {
                    append_value(&mut runs, value, mode, collation, query_spill)?;
                }
                values = Vec::new();
                memory.release(reserved);
                reserved = 0;
                external = true;
            }
            let batch_bytes = *batch_bytes.get_or_insert_with(|| batch.estimated_bytes());
            memory.ensure_transient(
                batch_bytes
                    .saturating_add(execution.memory().used())
                    .saturating_add(bytes.saturating_mul(4)),
            )?;
            if external {
                append_value(&mut runs, value, mode, collation, query_spill)?;
            } else {
                reserved += super::reserve_vec_elements(&mut values, 1, 0, &memory)?;
                memory.reserve(value.heap_bytes())?;
                reserved += value.heap_bytes();
                values.push(value.clone());
            }
        }
    }
    if !external {
        return Ok(MaterializedMembership::Memory(values));
    }
    // Reserve the largest decoded candidate and comparison scratch for the
    // index's lifetime; lookups retain only one candidate at a time.
    // The set spilled at a quarter of the limit, so holding its decoded
    // partitions costs about that again; probes read a resident partition
    // instead of reopening its file per row.
    let resident_budget = limit / 4;
    let retained = PARTITIONS
        .saturating_mul(size_of::<spill::ClosedRun>().saturating_add(256))
        .saturating_add(largest.saturating_mul(16))
        .saturating_add(resident_budget);
    memory.ensure_transient(retained)?;
    Ok(MaterializedMembership::Prepared(
        PreparedMembership(Arc::new(ExternalMembership {
            runs: runs
                .into_iter()
                .map(|run| {
                    std::sync::RwLock::new(
                        run.map_or(Partition::Absent, |run| Partition::OnDisk(run.seal())),
                    )
                })
                .collect(),
            promoting: (0..PARTITIONS)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            mode,
            collation,
            exact_decimal,
            saw_null,
            resident_bytes: std::sync::atomic::AtomicUsize::new(0),
            resident_budget,
            interruption: memory.unbounded_worker(),
        })),
        retained,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_enum_labels_and_signed_integers_share_a_partition() {
        let collation = Collation::default();
        assert_eq!(
            bucket(&Value::Int64(7), BucketMode::Integer, collation),
            bucket(&Value::UInt64(7), BucketMode::Integer, collation)
        );
        assert_eq!(
            bucket(
                &Value::Enum {
                    label: "A".to_owned(),
                    index: 1
                },
                BucketMode::Text,
                collation
            ),
            bucket(&Value::Utf8("a".to_owned()), BucketMode::Text, collation)
        );
    }

    #[test]
    fn a_probed_partition_is_read_from_disk_once() {
        // The file scan is the fallback, not the probe path: a set that
        // spilled must not reopen a file per probe row.
        let spill = spill::QuerySpill::new();
        let memory = MemoryTracker::new(64 * 1024);
        let collation = Collation::default();
        let mut runs = (0..PARTITIONS).map(|_| None).collect::<Vec<_>>();
        for id in 0..256_i64 {
            append_value(
                &mut runs,
                &Value::Int64(id),
                BucketMode::Integer,
                collation,
                &spill,
            )
            .expect("append");
        }
        let set = ExternalMembership {
            runs: runs
                .into_iter()
                .map(|run| {
                    std::sync::RwLock::new(
                        run.map_or(Partition::Absent, |run| Partition::OnDisk(run.seal())),
                    )
                })
                .collect(),
            promoting: (0..PARTITIONS)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            mode: BucketMode::Integer,
            collation,
            exact_decimal: false,
            saw_null: false,
            resident_bytes: std::sync::atomic::AtomicUsize::new(0),
            resident_budget: 64 * 1024,
            interruption: memory.unbounded_worker(),
        };
        for id in 0..256_i64 {
            assert_eq!(
                set.lookup(&Value::Int64(id)).expect("probe"),
                Value::Boolean(true)
            );
        }
        assert_eq!(
            set.lookup(&Value::Int64(-1)).expect("probe"),
            Value::Boolean(false)
        );
        // Every partition that held a value answered from memory after its
        // first probe, so 257 probes touched at most one file each.
        let on_disk = set
            .runs
            .iter()
            .filter(|slot| matches!(&*slot.read().expect("lock"), Partition::OnDisk(_)))
            .count();
        assert_eq!(on_disk, 0, "a probed partition stayed on disk");
        let resident = set
            .runs
            .iter()
            .filter(|slot| matches!(&*slot.read().expect("lock"), Partition::Resident(_)))
            .count();
        assert!(resident > 0);
    }

    #[test]
    fn a_partition_too_large_for_the_budget_keeps_answering_from_disk() {
        let spill = spill::QuerySpill::new();
        let memory = MemoryTracker::new(64 * 1024);
        let collation = Collation::default();
        let mut runs = (0..PARTITIONS).map(|_| None).collect::<Vec<_>>();
        for id in 0..256_i64 {
            append_value(
                &mut runs,
                &Value::Int64(id),
                BucketMode::Integer,
                collation,
                &spill,
            )
            .expect("append");
        }
        let set = ExternalMembership {
            runs: runs
                .into_iter()
                .map(|run| {
                    std::sync::RwLock::new(
                        run.map_or(Partition::Absent, |run| Partition::OnDisk(run.seal())),
                    )
                })
                .collect(),
            promoting: (0..PARTITIONS)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            mode: BucketMode::Integer,
            collation,
            exact_decimal: false,
            saw_null: true,
            resident_bytes: std::sync::atomic::AtomicUsize::new(0),
            resident_budget: 0,
            interruption: memory.unbounded_worker(),
        };
        for id in 0..256_i64 {
            assert_eq!(
                set.lookup(&Value::Int64(id)).expect("probe"),
                Value::Boolean(true)
            );
        }
        // A NULL in the set turns a miss into NULL, from disk as in memory.
        assert_eq!(set.lookup(&Value::Int64(-1)).expect("probe"), Value::Null);
        assert!(
            set.runs
                .iter()
                .any(|slot| matches!(&*slot.read().expect("lock"), Partition::OnDisk(_))),
            "no partition may be promoted past the budget"
        );
    }

    #[test]
    fn external_lookup_observes_cancellation_and_deadlines() {
        let memory = MemoryTracker::new(64 * 1024);
        let mut set = ExternalMembership {
            runs: (0..PARTITIONS)
                .map(|_| std::sync::RwLock::new(Partition::Absent))
                .collect(),
            promoting: (0..PARTITIONS)
                .map(|_| std::sync::atomic::AtomicBool::new(false))
                .collect(),
            mode: BucketMode::Common,
            collation: Collation::default(),
            exact_decimal: false,
            saw_null: false,
            resident_bytes: std::sync::atomic::AtomicUsize::new(0),
            resident_budget: 0,
            interruption: memory.unbounded_worker(),
        };
        memory.cancellation.as_ref().expect("token").cancel();
        assert!(matches!(
            set.lookup(&Value::UInt64(1)),
            Err(MembershipError::Cancelled)
        ));
        set.interruption = MemoryTracker::with_deadline(64 * 1024, Some(std::time::Instant::now()));
        assert!(matches!(
            set.lookup(&Value::UInt64(1)),
            Err(MembershipError::TimedOut)
        ));
    }

    fn assert_answers_as_list(
        membership: &PreparedMembership,
        members: &[Value],
        needle: &Value,
        collation: Collation,
    ) {
        let mut list = vec![needle.clone()];
        list.extend_from_slice(members);
        assert_eq!(
            membership.0.lookup(needle).expect("lookup"),
            crate::expression::evaluate_in_list(&list, false, false, collation).expect("list"),
            "{needle:?}"
        );
    }

    #[test]
    fn an_indexed_set_answers_as_its_literal_list_would() {
        let mut members = (1..=100).map(Value::Int64).collect::<Vec<_>>();
        members.push(Value::UInt64(u64::MAX));
        for with_null in [false, true] {
            if with_null {
                members.push(Value::Null);
            }
            let MaterializedMembership::Prepared(integers, _) = index_members(
                members.clone(),
                Collation::default(),
                Some(DataType::Int64),
                Some(DataType::UInt64),
            ) else {
                panic!("a hundred integers are indexed");
            };
            for needle in [
                Value::Int64(50),
                Value::UInt64(100),
                Value::UInt64(u64::MAX),
                Value::Int64(0),
                Value::Int64(-1),
                Value::Null,
            ] {
                assert_answers_as_list(&integers, &members, &needle, Collation::default());
            }
        }
        let words = (0..100)
            .map(|word| Value::Utf8(format!("Word {word}")))
            .collect::<Vec<_>>();
        for collation in [Collation::Utf8mb4GeneralCi, Collation::default()] {
            let MaterializedMembership::Prepared(text, _) = index_members(
                words.clone(),
                collation,
                Some(DataType::Utf8),
                Some(DataType::Utf8),
            ) else {
                panic!("a hundred words are indexed");
            };
            for needle in ["word 7", "WORD 99 ", "word 100", "Word 7x", "Wörd 8"] {
                assert_answers_as_list(&text, &words, &Value::Utf8(needle.to_owned()), collation);
            }
        }
        assert!(matches!(
            index_members(
                vec![Value::Int64(1)],
                Collation::default(),
                Some(DataType::Int64),
                Some(DataType::Int64),
            ),
            MaterializedMembership::Memory(_)
        ));
    }
}

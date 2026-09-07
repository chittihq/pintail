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
    External(PreparedMembership, usize),
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

fn bucket(value: &Value, mode: BucketMode, collation: Collation) -> usize {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    match (mode, value) {
        (BucketMode::Integer, Value::Int64(value)) => i128::from(*value).hash(&mut hash),
        (BucketMode::Integer, Value::UInt64(value)) => i128::from(*value).hash(&mut hash),
        (BucketMode::Text, Value::Utf8(value) | Value::Enum { label: value, .. }) => {
            super::join::normalized_collation_text(value, collation).hash(&mut hash);
        }
        _ => return 0,
    }
    usize::try_from(hash.finish() % PARTITIONS as u64).expect("partition fits usize")
}

impl ExternalMembership {
    /// Compares the needle against one already-decoded value.
    fn matches(&self, needle: &Value, value: Value) -> Result<bool, MembershipError> {
        crate::expression::evaluate_in_list(
            &[needle.clone(), value],
            false,
            self.exact_decimal,
            self.collation,
        )
        .map_err(lookup_error)
        .map(|outcome| outcome == Value::Boolean(true))
    }

    /// Reads one partition's file, comparing as it goes. Used while the
    /// partition is being promoted, and for good on a partition too large
    /// to hold: correctness never depends on the promotion succeeding.
    fn scan_file(
        &self,
        run: &spill::ClosedRun,
        needle: &Value,
        mut collect: Option<&mut Vec<Value>>,
    ) -> Result<bool, MembershipError> {
        let mut reader = run
            .open()
            .map_err(|error| MembershipError::Failed(error.to_string()))?;
        let mut found = false;
        while let Some(payload) = reader
            .next()
            .map_err(|error| MembershipError::Failed(error.to_string()))?
        {
            self.interruption
                .check_interruption()
                .map_err(lookup_error)?;
            let value = spill::Decoder::new(payload)
                .value()
                .map_err(MembershipError::Failed)?;
            if !found && self.matches(needle, value.clone())? {
                found = true;
                // Keep reading only while a promotion still wants the rest.
                if collect.is_none() {
                    return Ok(true);
                }
            }
            if let Some(values) = collect.as_mut() {
                values.push(value);
            }
        }
        Ok(found)
    }

    /// Whether this partition's values fit the budget left for resident
    /// partitions. Charged once, when the partition is promoted.
    fn claim_resident(&self, values: &[Value]) -> bool {
        let bytes = values
            .iter()
            .map(|value| size_of::<Value>().saturating_add(value.heap_bytes()))
            .fold(0_usize, usize::saturating_add);
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
}

impl MembershipLookup for ExternalMembership {
    fn lookup(&self, needle: &Value) -> Result<Value, MembershipError> {
        self.interruption
            .check_interruption()
            .map_err(lookup_error)?;
        if matches!(needle, Value::Null) {
            return Ok(Value::Null);
        }
        let slot = &self.runs[bucket(needle, self.mode, self.collation)];
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
                    let mut values = Vec::new();
                    let found = self.scan_file(run, needle, Some(&mut values))?;
                    if self.claim_resident(&values) {
                        drop(partition);
                        let mut partition = slot.write().map_err(|_| {
                            MembershipError::Failed("membership lock poisoned".to_owned())
                        })?;
                        // Another probe may have promoted it first; its
                        // values are the same, so either copy will do.
                        if matches!(&*partition, Partition::OnDisk(_)) {
                            *partition = Partition::Resident(values);
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
    let mode = if integer_type(needle_type) && integer_type(value_type) {
        BucketMode::Integer
    } else if needle_type == Some(DataType::Utf8) && value_type == Some(DataType::Utf8) {
        BucketMode::Text
    } else {
        BucketMode::Common
    };
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
            memory.ensure_transient(
                batch
                    .estimated_bytes()
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
    Ok(MaterializedMembership::External(
        PreparedMembership(Arc::new(ExternalMembership {
            runs: runs
                .into_iter()
                .map(|run| {
                    std::sync::RwLock::new(
                        run.map_or(Partition::Absent, |run| Partition::OnDisk(run.seal())),
                    )
                })
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
}

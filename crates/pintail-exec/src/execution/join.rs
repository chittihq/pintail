//! Hash join, grace-partitioned join with spill, join key
//! normalization, and the nested-loop fallback.

use std::{cmp::Ordering, collections::HashMap};

use pintail_sql::{BoundColumn, BoundExpr, BoundJoinKind, BoundOrderKey};
use pintail_types::{DataType, Value};

use crate::collation::Collation;

use super::{
    ExecError, HASH_ENTRY_OVERHEAD, JoinKeyMode, MemoryTracker, PullOperator, ScanProvider,
    batch_row, compare_sort_values, estimated_batch_row_bytes, estimated_record_batch_bytes,
    estimated_row_payload_bytes, reserve_hash_map_entries, reserve_vec_elements,
    resolve_dependent_expr_subqueries, rows_to_columns,
};
use crate::{
    RecordBatch, SPILL_SERVE_BATCH_ROWS,
    expression::{CompiledExpr, mysql_f64, predicate_truth},
    spill,
};

/// Hash tables the build side is split across.
///
/// Sized so a partition's table stays inside L2 for a build side of a few
/// hundred thousand rows, which is where the cache cost was measured.
pub(super) const BUILD_PARTITIONS: usize = 64;

/// The build side, split into independently-sized hash tables.
///
/// One table holding every key is what the profile objected to: joining on a
/// unique key puts a quarter of a million entries in it, and each insert lands
/// on a random slot in a structure far larger than L2. Holding everything else
/// fixed and varying only the number of distinct keys, the build phase read
/// 44.4ms against 28.9ms - 35% of it is the table's size rather than the work
/// per row.
///
/// Partitioning alone does not fix that: rows arrive in source order, so they
/// scatter across the partitions and miss just as often. The locality comes
/// from filling the partitions first and building their tables one at a time,
/// each small enough to stay in cache while it is written.
pub(super) struct PartitionedBuild {
    partitions: Vec<HashMap<JoinHashKey, Vec<Vec<Value>>>>,
}

impl PartitionedBuild {
    fn with_partitions(count: usize) -> Self {
        Self {
            partitions: (0..count.max(1)).map(|_| HashMap::new()).collect(),
        }
    }

    pub(super) fn slot(&self, key: &JoinHashKey) -> usize {
        use std::hash::{Hash as _, Hasher as _};
        if self.partitions.len() == 1 {
            return 0;
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        #[allow(clippy::cast_possible_truncation)] // modulo the count keeps any width
        {
            (hasher.finish() as usize) % self.partitions.len()
        }
    }

    pub(super) fn get(&self, key: &JoinHashKey) -> Option<&Vec<Vec<Value>>> {
        self.partitions[self.slot(key)].get(key)
    }

    pub(super) fn partitions(&self) -> usize {
        self.partitions.len()
    }

    fn contains_key(&self, key: &JoinHashKey) -> bool {
        self.partitions[self.slot(key)].contains_key(key)
    }

    fn entry_or_default(&mut self, key: JoinHashKey) -> &mut Vec<Vec<Value>> {
        let slot = self.slot(&key);
        self.partitions[slot].entry(key).or_default()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.partitions.iter().all(HashMap::is_empty)
    }

    /// Distinct keys across every partition.
    pub(super) fn len(&self) -> usize {
        self.partitions.iter().map(HashMap::len).sum()
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &JoinHashKey> {
        self.partitions.iter().flat_map(HashMap::keys)
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &Vec<Vec<Value>>> {
        self.partitions.iter().flat_map(HashMap::values)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (&JoinHashKey, &Vec<Vec<Value>>)> {
        self.partitions.iter().flat_map(HashMap::iter)
    }

    fn drain(&mut self) -> impl Iterator<Item = (JoinHashKey, Vec<Vec<Value>>)> + '_ {
        self.partitions.iter_mut().flat_map(HashMap::drain)
    }

    /// Reserves room for one more key in the partition it will land in.
    ///
    /// Splitting a batch's rows evenly across the partitions was wrong: keys
    /// are not evenly distributed, and a batch whose rows all hash to one
    /// partition reserved a sixty-fourth of what that partition then grew by.
    /// The excess came from the allocator without passing the tracker, so a
    /// skewed build could exhaust memory instead of spilling at its ceiling.
    fn reserve_for_key(
        &mut self,
        key: &JoinHashKey,
        entry_bytes: usize,
        transient_bytes: usize,
        memory: &MemoryTracker,
    ) -> Result<usize, ExecError> {
        let slot = self.slot(key);
        let partition = &mut self.partitions[slot];
        if partition.len() < partition.capacity() {
            return Ok(0);
        }
        reserve_hash_map_entries(
            partition,
            partition.capacity().max(64),
            entry_bytes,
            transient_bytes,
            memory,
        )
    }

    fn clear(&mut self) {
        for partition in &mut self.partitions {
            partition.clear();
        }
    }
}

/// Hashes the bucket addresses the probe loop looks up.
///
/// The fused probe asks "which groups does this bucket hold?" once per probe
/// row - two million times for this query - against a map keyed by the
/// bucket's address. `SipHash` earns its keep against attacker-chosen keys in
/// a persistent table; these are addresses this process just produced, living
/// for one query, so its cost is pure overhead on the hottest lookup in the
/// join.
#[derive(Default)]
pub(super) struct AddressHasher(u64);

impl std::hash::Hasher for AddressHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("bucket addresses hash through write_usize");
    }

    fn write_usize(&mut self, value: usize) {
        self.0 = crate::batch::mix64(value as u64);
    }
}

pub(super) type AddressMap<V> = HashMap<usize, V, std::hash::BuildHasherDefault<AddressHasher>>;

/// Group identity resolved ONCE from the build side. Group columns of a
/// fused join are build-side by construction, so the complete group set is
/// known before probing: workers then index groups directly instead of
/// hashing and comparing group values per probe row (the Q8 profile's
/// dominant cost, 2026-08-02).
pub(super) struct JoinGroupPlan {
    /// Group key values in index order.
    pub(super) values: Vec<Vec<Value>>,
    /// Per build bucket (keyed by its address), the group index of each row.
    pub(super) buckets: AddressMap<Vec<usize>>,
}

/// Appends a value's collation sort key, without the hex detour.
///
/// `normalized_collation_text` renders the key as hexadecimal so it can live
/// in a `Value::Utf8`. Nothing reads it - it is compared and hashed - so the
/// text form doubles the bytes and allocates a `String` per cell for no
/// purpose beyond fitting the row-shaped key. Writing the raw bytes into a
/// caller's buffer avoids both.
fn append_collation_key(text: &str, collation: Collation, out: &mut Vec<u8>) {
    match collation {
        Collation::Utf8mb4Bin => {
            out.extend_from_slice(&crate::collation::bin_sort_key(text));
        }
        Collation::Utf8mb4GeneralCi => {
            out.extend_from_slice(&crate::collation::general_ci_sort_key(text));
        }
        Collation::Utf8mb40900AiCi => MYSQL_DEFAULT_COLLATOR.with(|collator| {
            collator
                .write_sort_key_to(text, out)
                .expect("Vec-backed collation keys cannot fail");
        }),
    }
}

/// FNV-1a over bytes, for the two byte-keyed maps plan resolution builds.
///
/// Both hold data this query just produced and neither outlives it, so
/// `SipHash`'s resistance to attacker-chosen keys buys nothing - while its
/// cost lands once per build row, on the group key and again on the collation
/// cache lookup. Together they were 3.7% of the profile in `SipHash` alone,
/// before the `memcmp` each collision costs.
#[derive(Default)]
struct ByteKeyHasher(u64);

impl std::hash::Hasher for ByteKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        // Zero is the offset basis rather than a state, so a fresh hasher and
        // one that has consumed nothing agree.
        let mut hash = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = hash;
    }
}

type ByteKeyMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<ByteKeyHasher>>;

/// Collation keys already computed for this plan, by their text.
///
/// A group column is usually low-cardinality - eight regions, a dozen order
/// statuses - while the build side it is read from is hundreds of thousands
/// of rows, so the same handful of strings get collated over and over.
/// Generating those sort keys was 26% of the fused join-aggregate profile,
/// almost all of it recomputing an answer already held.
///
/// Capped: a group column with a distinct value per row - a customer name, an
/// e-mail - would otherwise hold every string in the build side twice, and
/// caching cannot help there anyway. Past the cap this degrades to computing
/// each key as it did before.
#[derive(Default)]
struct CollationKeyCache {
    keys: ByteKeyMap<String, Vec<u8>>,
}

/// Entries the collation cache will hold before it stops growing.
const COLLATION_KEY_CACHE_LIMIT: usize = 1 << 16;

impl CollationKeyCache {
    fn append(&mut self, text: &str, collation: Collation, out: &mut Vec<u8>) {
        if let Some(key) = self.keys.get(text) {
            out.extend_from_slice(key);
            return;
        }
        let mut key = Vec::new();
        append_collation_key(text, collation, &mut key);
        out.extend_from_slice(&key);
        if self.keys.len() < COLLATION_KEY_CACHE_LIMIT {
            self.keys.insert(text.to_owned(), key);
        }
    }
}

/// Encodes one group value into `out`, injectively.
///
/// Injective is the whole requirement: two group values must produce the same
/// bytes exactly when they belong in the same group. A tag separates the
/// variants, and anything variable-length carries its length AFTER its bytes -
/// nothing decodes this, so a suffix distinguishes `("ab", "c")` from
/// `("a", "bc")` as well as a prefix would, without a second pass to measure
/// first.
fn encode_group_value(
    value: &Value,
    collation: Collation,
    keys: &mut CollationKeyCache,
    out: &mut Vec<u8>,
) {
    match value {
        Value::Null => out.push(0),
        Value::Boolean(flag) => {
            out.push(1);
            out.push(u8::from(*flag));
        }
        Value::Int64(number) => {
            out.push(2);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::UInt64(number) => {
            out.push(3);
            out.extend_from_slice(&number.to_le_bytes());
        }
        Value::Float64(number) => {
            out.push(4);
            out.extend_from_slice(&number.get().to_bits().to_le_bytes());
        }
        // An ENUM groups and joins by its label, under the SAME tag as a
        // plain string: MySQL compares an ENUM to a text column by string,
        // and a distinct tag would keep equal labels from ever colliding.
        Value::Utf8(text) | Value::Enum { label: text, .. } => {
            out.push(5);
            let start = out.len();
            keys.append(text, collation, out);
            let length = u32::try_from(out.len() - start).unwrap_or(u32::MAX);
            out.extend_from_slice(&length.to_le_bytes());
        }
        Value::Binary(bytes) => {
            out.push(6);
            out.extend_from_slice(bytes);
            let length = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            out.extend_from_slice(&length.to_le_bytes());
        }
    }
}

pub(super) fn resolve_join_group_plan(
    build: &PartitionedBuild,
    right_group_columns: &[usize],
    collation: Collation,
) -> Result<JoinGroupPlan, ExecError> {
    let mut values = Vec::new();
    let mut index = ByteKeyMap::<Vec<u8>, usize>::default();
    let mut buckets =
        AddressMap::with_capacity_and_hasher(build.len(), std::hash::BuildHasherDefault::default());
    // One scratch buffer for the whole plan. The key used to be a
    // `Vec<Value>`: a heap vector per row, each cell a 32-byte tagged enum,
    // and every text cell an owned hexadecimal `String`. For a build side of
    // a hundred thousand rows that is a hundred thousand allocations to ask
    // a question - which group is this? - whose answer is almost always one
    // we already have. Packed bytes in a reused buffer allocate only when the
    // group is genuinely new.
    let mut key = Vec::<u8>::with_capacity(64);
    let mut keys = CollationKeyCache::default();
    for bucket in build.values() {
        let mut indexes = Vec::with_capacity(bucket.len());
        for row in bucket {
            key.clear();
            for column in right_group_columns {
                let value = row.get(*column).ok_or(ExecError::InvalidPhysicalPlan(
                    "join aggregate group is outside the build-side layout",
                ))?;
                encode_group_value(value, collation, &mut keys, &mut key);
            }
            // Borrowed lookup: `Vec<u8>` keys probe by slice, so the hit path
            // - the common one - neither allocates nor copies.
            let position = if let Some(position) = index.get(key.as_slice()) {
                *position
            } else {
                let group_values = right_group_columns
                    .iter()
                    .map(|column| row[*column].clone())
                    .collect::<Vec<_>>();
                values.push(group_values);
                index.insert(key.clone(), values.len() - 1);
                values.len() - 1
            };
            indexes.push(position);
        }
        buckets.insert(std::ptr::from_ref(bucket) as usize, indexes);
    }
    Ok(JoinGroupPlan { values, buckets })
}

/// Widest key span the dense join table will materialize (~4M slots).
pub(super) const MAX_DENSE_SPAN: i128 = 1 << 22;

pub(super) struct HashJoinState {
    pub(super) build: PartitionedBuild,
    /// Engaged when the build side overflowed: partitioned files replace
    /// the resident map and probing runs partition by partition.
    grace: Option<GraceJoin>,
    /// Min/max of non-null build keys, for probe-side scan restriction.
    pub(super) key_bounds: Option<(Value, Value)>,
    batch: Option<RecordBatch>,
    batch_reserved: usize,
    row: usize,
    match_index: usize,
    left_values: Option<Vec<Value>>,
    left_key: Option<JoinHashKey>,
    left_reserved: usize,
    /// Build-side rows for the current left row that survived the residual ON
    /// predicate, when there is one.
    residual_matches: Option<Vec<Vec<Value>>>,
}

impl HashJoinState {
    /// Whether the build side outgrew the ceiling and moved to grace
    /// partitions. `build` is drained when that happens, so anything reading
    /// it directly has to ask first.
    pub(super) const fn spilled(&self) -> bool {
        self.grace.is_some()
    }

    fn clear_left(&mut self, memory: &MemoryTracker) {
        self.left_values = None;
        self.left_key = None;
        self.match_index = 0;
        memory.release(self.left_reserved);
        self.left_reserved = 0;
    }

    fn clear_batch(&mut self, memory: &MemoryTracker) {
        self.clear_left(memory);
        self.batch = None;
        self.row = 0;
        memory.release(self.batch_reserved);
        self.batch_reserved = 0;
    }
}

#[allow(clippy::too_many_lines)] // one linear build walk with the spill valve
pub(super) fn build_hash_join_state(
    right: &mut PullOperator,
    right_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<HashJoinState, ExecError> {
    let mut build = PartitionedBuild::with_partitions(BUILD_PARTITIONS);
    let mut grace: Option<GraceJoin> = None;
    // Bytes reserved for the resident map, measured through used()
    // snapshots so entry, bucket, and payload reserves all count.
    let mut build_reserved = 0_usize;
    let mut key_bounds: Option<(Value, Value)> = None;
    let bound_order = BoundOrderKey {
        index: 0,
        ascending: true,
        nulls_first: true,
        decimal: false,
        collation: None,
    };
    while let Some(batch) = right.next_batch(memory)? {
        let used_before_batch = memory.used();
        let batch_bytes = batch.estimated_bytes();
        // Keys first, binned by the partition each will land in; the inserts
        // follow one partition at a time.
        let mut binned: Vec<Vec<(JoinHashKey, usize)>> = vec![Vec::new(); build.partitions()];
        // Keys are held for the whole batch before any of them are inserted, so
        // they are charged as they accumulate. Without this a batch of long
        // text keys allocates every normalized key at once and passes the
        // query's ceiling before a single per-row check runs.
        let mut binned_bytes = 0_usize;
        for row in batch.selection().selected_rows() {
            let value = right_key.evaluate(&batch, row)?;
            if !matches!(value, Value::Null) {
                match &mut key_bounds {
                    None => {
                        memory.reserve(value.heap_bytes().saturating_mul(2))?;
                        key_bounds = Some((value.clone(), value.clone()));
                    }
                    Some((minimum, maximum)) => {
                        if compare_sort_values(&value, minimum, bound_order, collation)
                            == Ordering::Less
                        {
                            *minimum = value.clone();
                        }
                        if compare_sort_values(&value, maximum, bound_order, collation)
                            == Ordering::Greater
                        {
                            *maximum = value.clone();
                        }
                    }
                }
            }
            let Some(key) = normalized_join_key(value, key_mode)? else {
                continue;
            };
            let Some(key) = composite_join_key(key, &batch, row, extra_keys, JoinSide::Build)?
            else {
                continue;
            };
            let retained = key
                .heap_bytes()
                .saturating_add(size_of::<(JoinHashKey, usize)>());
            memory.ensure_transient(batch_bytes.saturating_add(binned_bytes))?;
            memory.reserve(retained)?;
            binned_bytes = binned_bytes.saturating_add(retained);
            binned[build.slot(&key)].push((key, row));
        }
        for (key, row) in binned.into_iter().flatten() {
            let row_bytes = estimated_batch_row_bytes(&batch, row)?;
            let key_memory = right_key
                .allocation_upper_bound(&batch, row)
                .saturating_mul(12);
            memory.ensure_transient(
                batch_bytes
                    .saturating_add(row_bytes)
                    .saturating_add(key_memory),
            )?;
            let key_bytes = if build.contains_key(&key) {
                0
            } else {
                key.heap_bytes()
            };
            let row_payload = row_bytes.saturating_sub(size_of::<Vec<Value>>());
            memory.ensure_transient(
                batch_bytes
                    .saturating_add(key_memory)
                    .saturating_add(row_payload)
                    .saturating_add(64_usize.saturating_mul(size_of::<Vec<Value>>()))
                    .saturating_add(key_bytes),
            )?;
            if let Some(grace) = grace.as_mut() {
                let values = batch_row(&batch, row)?;
                grace.build_files[grace_partition(&key, 0)].append(&key, &values)?;
                continue;
            }
            build.reserve_for_key(
                &key,
                size_of::<JoinHashKey>()
                    .saturating_add(size_of::<Vec<Vec<Value>>>())
                    .saturating_add(HASH_ENTRY_OVERHEAD),
                batch_bytes,
                memory,
            )?;
            memory.reserve(key_bytes)?;
            let bucket = build.entry_or_default(key);
            reserve_vec_elements(bucket, 1, 64, memory)?;
            memory.reserve(row_payload)?;
            let values = batch_row(&batch, row)?;
            bucket.push(values);
        }
        memory.release(binned_bytes);
        build_reserved =
            build_reserved.saturating_add(memory.used().saturating_sub(used_before_batch));
        // Proactive spill at half the ceiling, like sort and aggregation:
        // drain the resident map into partition files and route the rest
        // of the build (and later the probe) through them.
        if grace.is_none() && build_reserved > memory.limit() / 2 && !build.is_empty() {
            let mut partitions = GraceJoin::create(memory)?;
            for (key, bucket) in build.drain() {
                let target = grace_partition(&key, 0);
                for values in bucket {
                    partitions.build_files[target].append(&key, &values)?;
                }
            }
            memory.release(build_reserved);
            build_reserved = 0;
            grace = Some(partitions);
        }
    }
    Ok(HashJoinState {
        build,
        grace,
        key_bounds,
        batch: None,
        batch_reserved: 0,
        row: 0,
        match_index: 0,
        left_values: None,
        left_key: None,
        left_reserved: 0,
        residual_matches: None,
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
/// Number of grace-join partitions; key hashes route rows uniformly, so
/// each partition holds roughly build-bytes / 16.
pub(super) const GRACE_PARTITIONS: usize = 16;

/// One append-mode spill file of `(join key, row values)` pairs.
pub(super) struct GraceRun {
    writer: Option<std::io::BufWriter<std::fs::File>>,
    path: tempfile::TempPath,
    reservation: spill::SpillReservation,
    entries: u64,
}

impl GraceRun {
    fn create(memory: &MemoryTracker) -> Result<Self, ExecError> {
        let file = spill::spill_file("pintail-join-spill-", memory.spill())
            .map_err(|error| ExecError::Source(format!("join spill create: {error}")))?;
        let (file, path, reservation) = file.into_parts();
        Ok(Self {
            writer: Some(std::io::BufWriter::new(file)),
            path,
            reservation,
            entries: 0,
        })
    }

    fn append(&mut self, key: &JoinHashKey, row: &[Value]) -> Result<(), ExecError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or(ExecError::InvalidPhysicalPlan("grace run already sealed"))?;
        let mut encoder = spill::Encoder::with_capacity(64);
        encode_join_key(&mut encoder, key);
        encoder.values(row);
        spill::write_record_quota(writer, &encoder.finish(), &mut self.reservation)
            .map_err(|error| ExecError::Source(format!("join spill write: {error}")))?;
        self.entries += 1;
        Ok(())
    }

    fn reader(&mut self) -> Result<GraceRunReader, ExecError> {
        use std::io::Seek as _;
        let writer = self
            .writer
            .take()
            .ok_or(ExecError::InvalidPhysicalPlan("grace run read twice"))?;
        let mut file = writer
            .into_inner()
            .map_err(|error| ExecError::Source(format!("join spill flush: {error}")))?;
        file.rewind()
            .map_err(|error| ExecError::Source(format!("join spill rewind: {error}")))?;
        let _ = &self.path;
        Ok(GraceRunReader {
            reader: std::io::BufReader::new(file),
            payload: Vec::new(),
        })
    }
}

/// Streams back one grace-join spill file.
struct GraceRunReader {
    reader: std::io::BufReader<std::fs::File>,
    payload: Vec<u8>,
}

impl GraceRunReader {
    fn next_entry(&mut self) -> Result<Option<(JoinHashKey, Vec<Value>)>, ExecError> {
        if !spill::read_record(&mut self.reader, &mut self.payload)
            .map_err(|error| ExecError::Source(format!("join spill read: {error}")))?
        {
            return Ok(None);
        }
        let mut decoder = spill::Decoder::new(&self.payload);
        let entry = decode_join_key(&mut decoder)
            .and_then(|key| Ok((key, decoder.values()?)))
            .map_err(|error| ExecError::Source(format!("join spill decode: {error}")))?;
        Ok(Some(entry))
    }
}

const JOIN_KEY_NEGATIVE: u8 = 0;
const JOIN_KEY_NON_NEGATIVE: u8 = 1;
const JOIN_KEY_MYSQL_NUMBER: u8 = 2;
const JOIN_KEY_SCALAR: u8 = 3;
const JOIN_KEY_COMPOSITE: u8 = 4;

fn encode_join_key(encoder: &mut spill::Encoder, key: &JoinHashKey) {
    match key {
        JoinHashKey::NegativeInteger(value) => {
            encoder.u8(JOIN_KEY_NEGATIVE);
            encoder.i64(*value);
        }
        JoinHashKey::NonNegativeInteger(value) => {
            encoder.u8(JOIN_KEY_NON_NEGATIVE);
            encoder.u64(*value);
        }
        JoinHashKey::MysqlNumber(value) => {
            encoder.u8(JOIN_KEY_MYSQL_NUMBER);
            encoder.f64(value.get());
        }
        JoinHashKey::Scalar(value) => {
            encoder.u8(JOIN_KEY_SCALAR);
            encoder.value(value);
        }
        JoinHashKey::Composite(parts) => {
            encoder.u8(JOIN_KEY_COMPOSITE);
            encoder.count(parts.len());
            for part in parts {
                encode_join_key(encoder, part);
            }
        }
    }
}

fn decode_join_key(decoder: &mut spill::Decoder<'_>) -> Result<JoinHashKey, String> {
    match decoder.u8()? {
        JOIN_KEY_NEGATIVE => Ok(JoinHashKey::NegativeInteger(decoder.i64()?)),
        JOIN_KEY_NON_NEGATIVE => Ok(JoinHashKey::NonNegativeInteger(decoder.u64()?)),
        JOIN_KEY_MYSQL_NUMBER => Ok(JoinHashKey::MysqlNumber(pintail_types::Float64::new(
            decoder.f64()?,
        ))),
        JOIN_KEY_SCALAR => Ok(JoinHashKey::Scalar(decoder.value()?)),
        JOIN_KEY_COMPOSITE => {
            let count = decoder.count()?;
            let mut parts = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                parts.push(decode_join_key(decoder)?);
            }
            Ok(JoinHashKey::Composite(parts))
        }
        other => Err(format!("spilled join key holds unknown tag {other}")),
    }
}

/// How many times one partition may be split again before a build side that
/// still will not fit is reported as unjoinable skew.
pub(super) const MAX_GRACE_DEPTH: usize = 3;

fn grace_partition(key: &JoinHashKey, seed: u64) -> usize {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    seed.hash(&mut hasher);
    key.hash(&mut hasher);
    #[allow(clippy::cast_possible_truncation)] // modulo 16 keeps any width
    {
        (hasher.finish() as usize) % GRACE_PARTITIONS
    }
}

/// Partitioned join state once the build side overflowed the ceiling.
pub(super) struct GraceJoin {
    build_files: Vec<GraceRun>,
    probe_files: Vec<GraceRun>,
    /// How many times each partition has been re-partitioned. Parallel to
    /// the file vectors, which grow as oversized partitions are split.
    depths: Vec<usize>,
    /// Probe routing finished; partitions are being served.
    probing_done: bool,
    /// Next partition to load in the serve phase.
    current: usize,
    /// The loaded partition's probe entries being replayed.
    replay: Option<GraceRunReader>,
    /// Bytes reserved for the loaded partition's build map.
    partition_reserved: usize,
}

impl GraceJoin {
    fn create(memory: &MemoryTracker) -> Result<Self, ExecError> {
        let mut build_files = Vec::with_capacity(GRACE_PARTITIONS);
        let mut probe_files = Vec::with_capacity(GRACE_PARTITIONS);
        for _ in 0..GRACE_PARTITIONS {
            build_files.push(GraceRun::create(memory)?);
            probe_files.push(GraceRun::create(memory)?);
        }
        Ok(Self {
            build_files,
            probe_files,
            depths: vec![0; GRACE_PARTITIONS],
            probing_done: false,
            current: 0,
            replay: None,
            partition_reserved: 0,
        })
    }
}

/// Splits one partition whose build side did not fit into a fresh round of
/// partitions under a different hash seed, and appends them to the work
/// list. A different seed is the point: rows that collided under the
/// previous one are spread by this one, so a partition that was merely
/// unlucky becomes joinable. Rows sharing a single key follow each other
/// into the same piece no matter the seed, which is why the depth bound
/// exists to end the recursion.
pub(super) fn split_grace_partition(
    grace: &mut GraceJoin,
    index: usize,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let depth = grace.depths[index];
    if depth >= MAX_GRACE_DEPTH {
        return Err(ExecError::Source(
            "grace join partition still exceeds the memory ceiling after re-partitioning \
             (one join key holds more rows than the ceiling); raise the limit"
                .to_owned(),
        ));
    }
    let first = grace.build_files.len();
    for _ in 0..GRACE_PARTITIONS {
        grace.build_files.push(GraceRun::create(memory)?);
        grace.probe_files.push(GraceRun::create(memory)?);
        grace.depths.push(depth + 1);
    }
    let seed = u64::try_from(depth).unwrap_or(0).saturating_add(1);
    // Move each source file out so its replacement can be written to while
    // the original is read; the emptied slot is never served again.
    let mut build = std::mem::replace(&mut grace.build_files[index], GraceRun::create(memory)?);
    let mut entries = build.reader()?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.build_files[target].append(&key, &values)?;
    }
    let mut probe = std::mem::replace(&mut grace.probe_files[index], GraceRun::create(memory)?);
    let mut entries = probe.reader()?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.probe_files[target].append(&key, &values)?;
    }
    Ok(())
}

/// Keeps only the build-side rows a residual ON predicate accepts.
///
/// A hash join matches on equality alone, so an ON clause that also compares
/// the two sides with something else - `lc.scheduledAt >= us.effectiveFrom` -
/// needs its remaining conjuncts applied to each candidate pair. Filtering the
/// bucket here rather than above the join is what preserves outer semantics:
/// a left row whose every candidate fails the residual arrives at `join_emit`
/// with an empty bucket and is NULL-extended, exactly as `MySQL` does. Moving
/// the same predicate into WHERE drops that row instead, which is why it is
/// not a workaround.
fn apply_join_residual(
    residual: Option<&CompiledExpr>,
    columns: &[BoundColumn],
    left_values: &[Value],
    matches: Option<&Vec<Vec<Value>>>,
) -> Result<Option<Vec<Vec<Value>>>, ExecError> {
    let (Some(residual), Some(matches)) = (residual, matches) else {
        return Ok(matches.cloned());
    };
    let column_types = columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<_>>();
    let mut kept = Vec::with_capacity(matches.len());
    for right_values in matches {
        let mut candidate = left_values.to_vec();
        candidate.extend(right_values.iter().cloned());
        let vectors = rows_to_columns(std::slice::from_ref(&candidate), &column_types)?;
        let batch = RecordBatch::new(1, vectors)?;
        if predicate_truth(&residual.evaluate(&batch, 0)?)? {
            kept.push(right_values.clone());
        }
    }
    // An empty bucket must read as "no match", not as a match producing
    // nothing - the two differ for LEFT and ANTI.
    Ok((!kept.is_empty()).then_some(kept))
}

/// One join-emit step shared by the in-memory probe loop and the grace
/// replay: produces at most one output row and reports whether this left
/// row is finished.
pub(super) fn join_emit(
    kind: BoundJoinKind,
    left_values: &[Value],
    matches: Option<&Vec<Vec<Value>>>,
    match_index: &mut usize,
    right_width: usize,
) -> Result<(Option<Vec<Value>>, bool), ExecError> {
    if kind == BoundJoinKind::Scalar && matches.is_some_and(|rows| rows.len() > 1) {
        return Err(ExecError::ScalarSubqueryRows {
            rows: matches.map_or(0, Vec::len),
        });
    }
    let output = match kind {
        BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Scalar => {
            if let Some(right_values) = matches.and_then(|matches| matches.get(*match_index)) {
                *match_index += 1;
                let mut output = left_values.to_vec();
                output.extend(right_values.iter().cloned());
                Some(output)
            } else if matches!(kind, BoundJoinKind::Left | BoundJoinKind::Scalar)
                && *match_index == 0
            {
                *match_index = 1;
                let mut output = left_values.to_vec();
                output.extend(std::iter::repeat_n(Value::Null, right_width));
                Some(output)
            } else {
                None
            }
        }
        BoundJoinKind::Semi if matches.is_some() => Some(left_values.to_vec()),
        BoundJoinKind::Anti if matches.is_none() => Some(left_values.to_vec()),
        BoundJoinKind::Semi | BoundJoinKind::Anti => None,
        BoundJoinKind::Cross => {
            return Err(ExecError::InvalidPhysicalPlan(
                "cross semantics reached hash join",
            ));
        }
    };
    let complete = match kind {
        BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Scalar => {
            *match_index >= matches.map_or(1, Vec::len)
        }
        BoundJoinKind::Semi | BoundJoinKind::Anti => true,
        BoundJoinKind::Cross => unreachable!("handled above"),
    };
    Ok((output, complete))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn next_hash_join_batch(
    left: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    right_width: usize,
    column_types: &[DataType],
    residual: Option<&CompiledExpr>,
    residual_columns: &[BoundColumn],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    if state.grace.is_some() {
        return next_grace_join_batch(
            left,
            kind,
            left_key,
            key_mode,
            extra_keys,
            right_width,
            column_types,
            residual,
            residual_columns,
            state,
            memory,
        );
    }
    let mut rows = Vec::<Vec<Value>>::with_capacity(SPILL_SERVE_BATCH_ROWS);
    let mut buffered_bytes = 0_usize;
    while rows.len() < SPILL_SERVE_BATCH_ROWS {
        if state.left_values.is_none()
            && !prepare_hash_join_left(left, left_key, key_mode, extra_keys, state, memory)?
        {
            break;
        }
        let left_values = state
            .left_values
            .as_ref()
            .expect("prepared join row is present");
        let matches = state.left_key.as_ref().and_then(|key| state.build.get(key));
        // Recomputed only when this left row is first seen; match_index walks
        // the filtered bucket across successive emits for the same row.
        if state.match_index == 0 {
            state.residual_matches =
                apply_join_residual(residual, residual_columns, left_values, matches)?;
        }
        let filtered = if residual.is_some() {
            state.residual_matches.as_ref()
        } else {
            matches
        };
        let (output, complete) = join_emit(
            kind,
            left_values,
            filtered,
            &mut state.match_index,
            right_width,
        )?;
        let emitted = output.is_some();
        if let Some(output) = output {
            let output_bytes = estimated_row_payload_bytes(&output);
            memory.ensure_transient(
                buffered_bytes
                    .saturating_add(output_bytes)
                    .saturating_add(size_of::<Vec<Value>>()),
            )?;
            buffered_bytes = buffered_bytes
                .saturating_add(output_bytes)
                .saturating_add(size_of::<Vec<Value>>());
            rows.push(output);
        }
        if complete || !emitted {
            state.clear_left(memory);
        }
    }
    if rows.is_empty() {
        state.clear_batch(memory);
        return Ok(None);
    }
    memory.ensure_transient(
        buffered_bytes.saturating_add(estimated_record_batch_bytes(&rows, column_types.len())),
    )?;
    let columns = rows_to_columns(&rows, column_types)?;
    Ok(Some(RecordBatch::new(rows.len(), columns)?))
}

/// Serves a grace-partitioned join: routes remaining probe rows to their
/// partition files (NULL-key rows resolve immediately), then loads each
/// build partition and replays its probe file through the shared emit
/// logic.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn next_grace_join_batch(
    left: &mut PullOperator,
    kind: BoundJoinKind,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    right_width: usize,
    column_types: &[DataType],
    residual: Option<&CompiledExpr>,
    residual_columns: &[BoundColumn],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<Option<RecordBatch>, ExecError> {
    let mut rows = Vec::<Vec<Value>>::with_capacity(SPILL_SERVE_BATCH_ROWS);
    let mut buffered_bytes = 0_usize;
    let push = |rows: &mut Vec<Vec<Value>>,
                buffered_bytes: &mut usize,
                output: Vec<Value>|
     -> Result<(), ExecError> {
        let output_bytes = estimated_row_payload_bytes(&output);
        memory.ensure_transient(
            buffered_bytes
                .saturating_add(output_bytes)
                .saturating_add(size_of::<Vec<Value>>()),
        )?;
        *buffered_bytes = buffered_bytes
            .saturating_add(output_bytes)
            .saturating_add(size_of::<Vec<Value>>());
        rows.push(output);
        Ok(())
    };

    // Phase B: route the probe side to partition files.
    loop {
        let probing_done = state.grace.as_ref().is_some_and(|grace| grace.probing_done);
        if probing_done {
            break;
        }
        if rows.len() >= SPILL_SERVE_BATCH_ROWS {
            break;
        }
        if state.left_values.is_none()
            && !prepare_hash_join_left(left, left_key, key_mode, extra_keys, state, memory)?
        {
            let grace = state.grace.as_mut().expect("grace state engaged");
            grace.probing_done = true;
            break;
        }
        let left_values = state
            .left_values
            .take()
            .expect("prepared join row is present");
        let key = state.left_key.take();
        match key {
            Some(key) => {
                let grace = state.grace.as_mut().expect("grace state engaged");
                grace.probe_files[grace_partition(&key, 0)].append(&key, &left_values)?;
            }
            None => match kind {
                // NULL keys never match: inner/semi drop the row, left
                // emits it null-extended, anti passes it through.
                BoundJoinKind::Inner | BoundJoinKind::Semi => {}
                BoundJoinKind::Left | BoundJoinKind::Scalar => {
                    let mut output = left_values.clone();
                    output.extend(std::iter::repeat_n(Value::Null, right_width));
                    push(&mut rows, &mut buffered_bytes, output)?;
                }
                BoundJoinKind::Anti => {
                    push(&mut rows, &mut buffered_bytes, left_values.clone())?;
                }
                BoundJoinKind::Cross => {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "cross semantics reached hash join",
                    ));
                }
            },
        }
        state.match_index = 0;
        memory.release(state.left_reserved);
        state.left_reserved = 0;
    }

    // Phase C: serve partitions.
    while rows.len() < SPILL_SERVE_BATCH_ROWS {
        let grace = state.grace.as_mut().expect("grace state engaged");
        if !grace.probing_done {
            break;
        }
        if grace.replay.is_none() {
            if grace.current >= grace.build_files.len() {
                break;
            }
            let index = grace.current;
            grace.current += 1;
            // Load this partition's build rows into the resident map.
            state.build.clear();
            memory.release(grace.partition_reserved);
            grace.partition_reserved = 0;
            let used_before = memory.used();
            let mut overflowed = false;
            let mut entries = grace.build_files[index].reader()?;
            while let Some((key, values)) = entries.next_entry()? {
                // The partition this key lands in decides whether anything
                // grows; comparing the summed length against the summed
                // capacity let a full partition grow untracked while the others
                // still had room.
                if state
                    .build
                    .reserve_for_key(
                        &key,
                        size_of::<JoinHashKey>()
                            .saturating_add(size_of::<Vec<Vec<Value>>>())
                            .saturating_add(HASH_ENTRY_OVERHEAD),
                        0,
                        memory,
                    )
                    .is_err()
                {
                    overflowed = true;
                    break;
                }
                if memory
                    .reserve(
                        key.heap_bytes()
                            .saturating_add(estimated_row_payload_bytes(&values)),
                    )
                    .is_err()
                {
                    overflowed = true;
                    break;
                }
                state.build.entry_or_default(key).push(values);
            }
            if overflowed {
                // This partition's build side does not fit. Give back what it
                // took and split it again rather than failing the query.
                drop(entries);
                state.build.clear();
                memory.release(memory.used().saturating_sub(used_before));
                split_grace_partition(grace, index, memory)?;
                continue;
            }
            grace.partition_reserved = memory.used().saturating_sub(used_before);
            grace.replay = Some(grace.probe_files[index].reader()?);
        }
        let Some(replay) = grace.replay.as_mut() else {
            break;
        };
        let Some((key, left_values)) = replay.next_entry()? else {
            grace.replay = None;
            continue;
        };
        let matches = state.build.get(&key);
        let filtered = apply_join_residual(residual, residual_columns, &left_values, matches)?;
        let matches = if residual.is_some() {
            filtered.as_ref()
        } else {
            matches
        };
        let mut match_index = 0_usize;
        loop {
            let (output, complete) =
                join_emit(kind, &left_values, matches, &mut match_index, right_width)?;
            let emitted = output.is_some();
            if let Some(output) = output {
                push(&mut rows, &mut buffered_bytes, output)?;
            }
            // Mirrors the resident loop: an unmatched row is finished even
            // when the completion test says otherwise (inner, no matches).
            if complete || !emitted {
                break;
            }
        }
    }

    if rows.is_empty() {
        let grace = state.grace.as_mut().expect("grace state engaged");
        memory.release(grace.partition_reserved);
        grace.partition_reserved = 0;
        state.build.clear();
        state.clear_batch(memory);
        return Ok(None);
    }
    memory.ensure_transient(
        buffered_bytes.saturating_add(estimated_record_batch_bytes(&rows, column_types.len())),
    )?;
    let columns = rows_to_columns(&rows, column_types)?;
    Ok(Some(RecordBatch::new(rows.len(), columns)?))
}

fn prepare_hash_join_left(
    left: &mut PullOperator,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
) -> Result<bool, ExecError> {
    loop {
        let exhausted = state
            .batch
            .as_ref()
            .is_some_and(|batch| state.row >= batch.row_count());
        if state.batch.is_none() || exhausted {
            state.clear_batch(memory);
            let Some(batch) = left.next_batch(memory)? else {
                return Ok(false);
            };
            let batch_bytes = batch.estimated_bytes();
            memory.reserve(batch_bytes)?;
            state.batch_reserved = batch_bytes;
            state.batch = Some(batch);
        }
        let batch = state.batch.as_ref().expect("left batch initialized");
        let row = state.row;
        state.row += 1;
        if !batch.selection().is_selected(row) {
            continue;
        }
        let row_bytes = estimated_batch_row_bytes(batch, row)?;
        let key_memory = left_key
            .allocation_upper_bound(batch, row)
            .saturating_mul(12);
        memory.ensure_transient(row_bytes.saturating_add(key_memory))?;
        state.left_key = match normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? {
            Some(primary) => composite_join_key(primary, batch, row, extra_keys, JoinSide::Probe)?,
            None => None,
        };
        state.left_reserved = row_bytes.saturating_sub(size_of::<Vec<Value>>());
        memory.reserve(state.left_reserved)?;
        state.left_values = Some(batch_row(batch, row)?);
        state.match_index = 0;
        return Ok(true);
    }
}

pub(super) fn normalized_hash_key(value: Value, collation: Collation) -> Option<Value> {
    (!matches!(value, Value::Null)).then(|| normalized_collation_value(value, collation))
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub(super) enum JoinHashKey {
    NegativeInteger(i64),
    NonNegativeInteger(u64),
    MysqlNumber(pintail_types::Float64),
    Scalar(Value),
    /// Multi-key equality: primary key first, extras in declaration order.
    Composite(Vec<JoinHashKey>),
}

impl JoinHashKey {
    fn heap_bytes(&self) -> usize {
        match self {
            Self::Scalar(value) => value.heap_bytes(),
            Self::NegativeInteger(_) | Self::NonNegativeInteger(_) | Self::MysqlNumber(_) => 0,
            Self::Composite(parts) => parts
                .iter()
                .map(|part| size_of::<Self>().saturating_add(part.heap_bytes()))
                .fold(0, usize::saturating_add),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum JoinSide {
    Build,
    Probe,
}

/// Extends a primary join key with the extra equality keys, or returns
/// `None` when any component is NULL (the row can never match). Single-key
/// joins pass through untouched, keeping their hash shape identical.
pub(super) fn composite_join_key(
    primary: JoinHashKey,
    batch: &RecordBatch,
    row: usize,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    side: JoinSide,
) -> Result<Option<JoinHashKey>, ExecError> {
    if extra_keys.is_empty() {
        return Ok(Some(primary));
    }
    let mut parts = Vec::with_capacity(1 + extra_keys.len());
    parts.push(primary);
    for (probe_key, build_key, mode) in extra_keys {
        let expr = match side {
            JoinSide::Probe => probe_key,
            JoinSide::Build => build_key,
        };
        let Some(part) = normalized_join_key(expr.evaluate(batch, row)?, *mode)? else {
            return Ok(None);
        };
        parts.push(part);
    }
    Ok(Some(JoinHashKey::Composite(parts)))
}

pub(super) fn normalized_join_key(
    value: Value,
    mode: JoinKeyMode,
) -> Result<Option<JoinHashKey>, ExecError> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let key = match mode {
        JoinKeyMode::CollatedText(collation) => {
            JoinHashKey::Scalar(normalized_collation_value(value, collation))
        }
        JoinKeyMode::Binary | JoinKeyMode::Boolean => JoinHashKey::Scalar(value),
        JoinKeyMode::Integer => match value {
            Value::Int64(value) if value < 0 => JoinHashKey::NegativeInteger(value),
            Value::Int64(value) => JoinHashKey::NonNegativeInteger(
                u64::try_from(value).expect("nonnegative i64 fits u64"),
            ),
            Value::UInt64(value) => JoinHashKey::NonNegativeInteger(value),
            _ => return Err(ExecError::InvalidExpressionType),
        },
        JoinKeyMode::MysqlNumber => {
            let value = mysql_f64(&value)?;
            let value = if value == 0.0 { 0.0 } else { value };
            if !value.is_finite() {
                return Err(ExecError::InvalidExpressionType);
            }
            JoinHashKey::MysqlNumber(pintail_types::Float64::new(value))
        }
    };
    Ok(Some(key))
}

pub(super) fn normalized_collation_value(value: Value, collation: Collation) -> Value {
    match value {
        Value::Utf8(value) => Value::Utf8(normalized_collation_text(&value, collation)),
        // An ENUM hashes and matches as its label: MySQL compares an ENUM to
        // a string column by string, so `enum_col = varchar_col` keys must
        // collide with the plain-text side.
        Value::Enum { label, .. } => Value::Utf8(normalized_collation_text(&label, collation)),
        value => value,
    }
}

/// Text normalization for grouping, hashing, DISTINCT, and set membership.
/// The returned hexadecimal ICU primary sort key compares bytewise in the
/// same order as [`compare_collated_text`], so every hash-based and ordered
/// operator shares one case- and accent-insensitive equivalence relation.
pub(crate) fn normalized_collation_text(text: &str, collation: Collation) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    // general_ci has its own flat weight table, and crucially its own PAD
    // SPACE rule; running it through the ICU collator would silently answer
    // with the other collation's semantics.
    let mut key = Vec::new();
    match collation {
        Collation::Utf8mb4GeneralCi => key = crate::collation::general_ci_sort_key(text),
        Collation::Utf8mb4Bin => key = crate::collation::bin_sort_key(text),
        Collation::Utf8mb40900AiCi => MYSQL_DEFAULT_COLLATOR.with(|collator| {
            collator
                .write_sort_key_to(text, &mut key)
                .expect("Vec-backed collation keys cannot fail");
        }),
    }
    let mut encoded = String::with_capacity(key.len().saturating_mul(2));
    for byte in key {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// Compares text under the collation the plan resolved at bind time.
#[must_use]
pub fn compare_collated_text(left: &str, right: &str, collation: Collation) -> std::cmp::Ordering {
    match collation {
        Collation::Utf8mb4GeneralCi => crate::collation::compare_general_ci(left, right),
        Collation::Utf8mb4Bin => crate::collation::compare_bin(left, right),
        Collation::Utf8mb40900AiCi => {
            MYSQL_DEFAULT_COLLATOR.with(|collator| collator.compare(left, right))
        }
    }
}

thread_local! {
    static MYSQL_DEFAULT_COLLATOR: icu_collator::CollatorBorrowed<'static> = {
        let mut options = icu_collator::options::CollatorOptions::default();
        options.strength = Some(icu_collator::options::Strength::Primary);
        icu_collator::Collator::try_new(icu_collator::CollatorPreferences::default(), options)
            .expect("compiled ICU root collation data is available")
    };
}

#[allow(clippy::too_many_arguments)]
pub(super) fn execute_nested_loop_join(
    left_rows: &[Vec<Value>],
    right_rows: &[Vec<Value>],
    left_columns: &[BoundColumn],
    right_columns: &[BoundColumn],
    kind: BoundJoinKind,
    condition: &BoundExpr,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut columns = left_columns.to_vec();
    columns.extend_from_slice(right_columns);
    let column_types = columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<_>>();
    let mut output = Vec::new();
    for left in left_rows {
        memory.check_interruption()?;
        let mut matches = 0_usize;
        for right in right_rows {
            memory.ensure_transient(
                estimated_row_payload_bytes(left)
                    .saturating_add(estimated_row_payload_bytes(right)),
            )?;
            let mut candidate = left.clone();
            candidate.extend(right.iter().cloned());
            let vectors = rows_to_columns(std::slice::from_ref(&candidate), &column_types)?;
            let batch = RecordBatch::new(1, vectors)?;
            let mut predicate = condition.clone();
            resolve_dependent_expr_subqueries(
                &mut predicate,
                &batch,
                0,
                &columns,
                provider,
                memory,
                collation,
            )?;
            let predicate = CompiledExpr::compile(&predicate, &columns, collation)?;
            if !predicate_truth(&predicate.evaluate(&batch, 0)?)? {
                continue;
            }
            matches = matches.saturating_add(1);
            match kind {
                BoundJoinKind::Inner | BoundJoinKind::Left => {
                    push_nested_join_row(&mut output, candidate, memory)?;
                }
                BoundJoinKind::Scalar => {
                    if matches > 1 {
                        return Err(ExecError::ScalarSubqueryRows { rows: matches });
                    }
                    push_nested_join_row(&mut output, candidate, memory)?;
                }
                BoundJoinKind::Semi => break,
                BoundJoinKind::Anti => {}
                BoundJoinKind::Cross => {
                    return Err(ExecError::InvalidPhysicalPlan(
                        "nested-loop ON evaluation cannot represent a cross join",
                    ));
                }
            }
        }
        match kind {
            BoundJoinKind::Left | BoundJoinKind::Scalar if matches == 0 => {
                let mut row = left.clone();
                row.extend(std::iter::repeat_n(Value::Null, right_columns.len()));
                push_nested_join_row(&mut output, row, memory)?;
            }
            BoundJoinKind::Semi if matches > 0 => {
                push_nested_join_row(&mut output, left.clone(), memory)?;
            }
            BoundJoinKind::Anti if matches == 0 => {
                push_nested_join_row(&mut output, left.clone(), memory)?;
            }
            BoundJoinKind::Inner
            | BoundJoinKind::Left
            | BoundJoinKind::Scalar
            | BoundJoinKind::Semi
            | BoundJoinKind::Anti => {}
            BoundJoinKind::Cross => unreachable!("cross joins return above"),
        }
    }
    Ok(output)
}

fn push_nested_join_row(
    output: &mut Vec<Vec<Value>>,
    row: Vec<Value>,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    reserve_vec_elements(output, 1, 0, memory)?;
    memory.reserve(estimated_row_payload_bytes(&row))?;
    output.push(row);
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::collation::Collation;

    fn drain_partitions(runs: &mut [super::GraceRun]) -> Vec<u64> {
        let mut ids = Vec::new();
        for run in runs {
            let mut reader = run.reader().expect("partition reader");
            while let Some((_, values)) = reader.next_entry().expect("partition entry") {
                match values.first() {
                    Some(pintail_types::Value::UInt64(id)) => ids.push(*id),
                    other => panic!("unexpected spilled row {other:?}"),
                }
            }
        }
        ids
    }

    #[test]
    fn scalar_join_null_extends_zero_matches_and_errors_on_two() {
        use pintail_sql::BoundJoinKind;
        use pintail_types::Value;

        let left = vec![Value::UInt64(7)];
        let mut match_index = 0;
        let (row, complete) =
            super::join_emit(BoundJoinKind::Scalar, &left, None, &mut match_index, 1)
                .expect("zero matches");
        assert_eq!(row, Some(vec![Value::UInt64(7), Value::Null]));
        assert!(complete);

        let matches = vec![
            vec![Value::Utf8("first".to_owned())],
            vec![Value::Utf8("second".to_owned())],
        ];
        assert!(matches!(
            super::join_emit(BoundJoinKind::Scalar, &left, Some(&matches), &mut 0, 1,),
            Err(super::ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    #[test]
    fn splitting_an_oversized_partition_keeps_every_row_and_spreads_the_keys() {
        use super::MemoryTracker;
        use super::{GRACE_PARTITIONS, GraceJoin, JoinHashKey, split_grace_partition};
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create(&memory).expect("grace state");
        let ids = (0..500_u64).collect::<Vec<_>>();
        for id in &ids {
            let key = JoinHashKey::NonNegativeInteger(*id);
            let row = vec![pintail_types::Value::UInt64(*id)];
            grace.build_files[0].append(&key, &row).expect("build");
            grace.probe_files[0].append(&key, &row).expect("probe");
        }

        split_grace_partition(&mut grace, 0, &memory).expect("split");
        assert_eq!(grace.build_files.len(), GRACE_PARTITIONS * 2);
        assert_eq!(grace.depths[GRACE_PARTITIONS], 1);

        let mut build = drain_partitions(&mut grace.build_files[GRACE_PARTITIONS..]);
        let mut probe = drain_partitions(&mut grace.probe_files[GRACE_PARTITIONS..]);
        build.sort_unstable();
        probe.sort_unstable();
        assert_eq!(build, ids, "no build row may be lost in a split");
        assert_eq!(probe, ids, "probe rows follow their keys");

        // The emptied original must never be served again.
        assert!(
            drain_partitions(&mut grace.build_files[0..1]).is_empty(),
            "the split partition is left empty"
        );
    }

    #[test]
    fn a_single_key_that_never_fits_reports_skew_at_the_depth_bound() {
        use super::{ExecError, MemoryTracker};
        use super::{GraceJoin, MAX_GRACE_DEPTH, split_grace_partition};
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create(&memory).expect("grace state");
        grace.depths[0] = MAX_GRACE_DEPTH;
        let error = split_grace_partition(&mut grace, 0, &memory).expect_err("depth bound");
        let ExecError::Source(message) = error else {
            panic!("expected a source error at the depth bound");
        };
        assert!(
            message.contains("one join key holds more rows than the ceiling"),
            "the message must name the cause, saw {message}"
        );
    }

    #[test]
    fn collation_keys_match_comparison_for_case_accents_and_expansions() {
        use std::cmp::Ordering;

        for (left, right) in [
            ("CaFé", "cafe"),
            ("é", "e\u{301}"),
            ("Straße", "STRASSE"),
            ("Ａ", "a"),
        ] {
            assert_eq!(
                super::compare_collated_text(left, right, Collation::Utf8mb40900AiCi),
                Ordering::Equal
            );
            assert_eq!(
                super::normalized_collation_text(left, Collation::Utf8mb40900AiCi),
                super::normalized_collation_text(right, Collation::Utf8mb40900AiCi)
            );
        }
        assert_eq!(
            super::compare_collated_text("Émile", "Ernie", Collation::Utf8mb40900AiCi),
            Ordering::Less
        );
        assert!(
            super::normalized_collation_text("Émile", Collation::Utf8mb40900AiCi)
                < super::normalized_collation_text("Ernie", Collation::Utf8mb40900AiCi)
        );

        // utf8mb4_0900_ai_ci is a NO PAD collation: unlike older PAD SPACE
        // collations, a trailing space participates in comparison and keys.
        assert_ne!(
            super::compare_collated_text("a", "a ", Collation::Utf8mb40900AiCi),
            Ordering::Equal
        );
        assert_ne!(
            super::normalized_collation_text("a", Collation::Utf8mb40900AiCi),
            super::normalized_collation_text("a ", Collation::Utf8mb40900AiCi)
        );
    }
}

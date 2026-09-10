//! Hash join, grace-partitioned join with spill, join key
//! normalization, and the nested-loop fallback.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

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
    /// Set once, after every build row was inserted, when the keys are a
    /// plain integer set spanning fewer than [`MAX_DENSE_SPAN`] values:
    /// (minimum key, per-offset index into `dense_buckets`). `get` and the
    /// other read accessors consult this first, trading a probe row's
    /// hash-and-compare for one bounds-checked array index. `partitions`
    /// is left as an emptied skeleton rather than cleared away, since
    /// nothing reads it again once this is `Some` - only the build phase
    /// (`entry_or_default`, `reserve_for_key`, `slot`, `drain`, `clear`)
    /// touches it, and that phase is over by the time this is set.
    dense_index: Option<(i128, Vec<Option<usize>>)>,
    dense_buckets: Vec<Vec<Vec<Value>>>,
}

impl PartitionedBuild {
    fn with_partitions(count: usize) -> Self {
        Self {
            partitions: (0..count.max(1)).map(|_| HashMap::new()).collect(),
            dense_index: None,
            dense_buckets: Vec::new(),
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

    /// The dense slot a key resolves to, when this build finalized to a
    /// dense table and the key is the plain integer variant that mode
    /// requires. `Some` only while dense; the general (hashed) path never
    /// calls this directly - `get` already dispatches to it.
    fn dense_offset(&self, key: &JoinHashKey) -> Option<usize> {
        let (min, index) = self.dense_index.as_ref()?;
        let value = match key {
            JoinHashKey::NegativeInteger(value) => i128::from(*value),
            JoinHashKey::NonNegativeInteger(value) => i128::from(*value),
            _ => return None,
        };
        *index.get(usize::try_from(value.checked_sub(*min)?).ok()?)?
    }

    pub(super) fn get(&self, key: &JoinHashKey) -> Option<&Vec<Vec<Value>>> {
        if self.dense_index.is_some() {
            return self
                .dense_offset(key)
                .map(|offset| &self.dense_buckets[offset]);
        }
        self.partitions[self.slot(key)].get(key)
    }

    /// Like [`Self::get`], but also returns the flat index into
    /// [`Self::dense_buckets`] a caller can use to keep its own array (one
    /// entry per distinct key, built once) aligned to this bucket - the
    /// fused join-aggregate's precomputed group indexes, in particular.
    /// `None` whenever `get` would return through the hashed path instead.
    pub(super) fn dense_get(&self, key: &JoinHashKey) -> Option<(usize, &Vec<Vec<Value>>)> {
        let offset = self.dense_offset(key)?;
        Some((offset, &self.dense_buckets[offset]))
    }

    pub(super) const fn is_dense(&self) -> bool {
        self.dense_index.is_some()
    }

    /// Every distinct bucket, in the order [`Self::dense_get`]'s flat index
    /// addresses - only meaningful once [`Self::is_dense`].
    pub(super) fn dense_buckets(&self) -> &[Vec<Vec<Value>>] {
        &self.dense_buckets
    }

    /// Moves every bucket into a flat, densely-addressable array when the
    /// build key is a plain integer whose span fits [`MAX_DENSE_SPAN`] -
    /// `MySQL` auto-increment keys make this the common case, not the
    /// exception. Idempotent; a no-op once already dense. Must run only
    /// after every insert for this build is done: nothing re-populates
    /// `partitions` afterward.
    pub(super) fn finalize_dense(&mut self) {
        if self.dense_index.is_some() || self.is_empty() {
            return;
        }
        let mut min = i128::MAX;
        let mut max = i128::MIN;
        for key in self.keys() {
            match key {
                JoinHashKey::NegativeInteger(value) => {
                    min = min.min(i128::from(*value));
                    max = max.max(i128::from(*value));
                }
                JoinHashKey::NonNegativeInteger(value) => {
                    min = min.min(i128::from(*value));
                    max = max.max(i128::from(*value));
                }
                _ => return,
            }
        }
        if max - min >= MAX_DENSE_SPAN {
            return;
        }
        let span = usize::try_from(max - min).expect("bounded span") + 1;
        let mut index: Vec<Option<usize>> = vec![None; span];
        let mut buckets = Vec::with_capacity(self.len());
        for partition in &mut self.partitions {
            for (key, bucket) in partition.drain() {
                let value = match key {
                    JoinHashKey::NegativeInteger(value) => i128::from(value),
                    JoinHashKey::NonNegativeInteger(value) => i128::from(value),
                    _ => unreachable!("verified integer keys above"),
                };
                index[usize::try_from(value - min).expect("within span")] = Some(buckets.len());
                buckets.push(bucket);
            }
        }
        self.dense_index = Some((min, index));
        self.dense_buckets = buckets;
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
        if self.dense_index.is_some() {
            return self.dense_buckets.is_empty();
        }
        self.partitions.iter().all(HashMap::is_empty)
    }

    /// Distinct keys across every partition.
    pub(super) fn len(&self) -> usize {
        if self.dense_index.is_some() {
            return self.dense_buckets.len();
        }
        self.partitions.iter().map(HashMap::len).sum()
    }

    pub(super) fn values(&self) -> Box<dyn Iterator<Item = &Vec<Vec<Value>>> + '_> {
        if self.dense_index.is_some() {
            Box::new(self.dense_buckets.iter())
        } else {
            Box::new(self.partitions.iter().flat_map(HashMap::values))
        }
    }

    fn keys(&self) -> impl Iterator<Item = &JoinHashKey> {
        self.partitions.iter().flat_map(HashMap::keys)
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
        Collation::Json => out.extend_from_slice(
            &crate::json_order::json_sort_key(text)
                .unwrap_or_else(|| crate::collation::bin_sort_key(text)),
        ),
        Collation::Utf8mb4GeneralCi => {
            out.extend_from_slice(&crate::collation::general_ci_sort_key(text));
        }
        Collation::Utf8mb4UnicodeCi => {
            out.extend_from_slice(&crate::collation::unicode_ci_sort_key(text));
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
        Value::DecimalAverage(average) => {
            let text = &average.label;
            {
                out.push(5);
                let start = out.len();
                keys.append(text, collation, out);
                let length = u32::try_from(out.len() - start).unwrap_or(u32::MAX);
                out.extend_from_slice(&length.to_le_bytes());
            }
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
    /// Probe batches read ahead of the build, each with the bytes it holds
    /// against the ceiling; served before the probe input is pulled again.
    prefetched: VecDeque<(RecordBatch, usize)>,
    /// Bytes the build-side key filter's set holds, released once the
    /// probe is exhausted.
    filter_reserved: usize,
}

impl HashJoinState {
    /// Whether the build side outgrew the ceiling and moved to grace
    /// partitions. `build` is drained when that happens, so anything reading
    /// it directly has to ask first.
    pub(super) const fn spilled(&self) -> bool {
        self.grace.is_some()
    }

    /// Takes over the probe batches read ahead of the build and the bytes
    /// the build-side key filter holds.
    pub(super) fn adopt_prefetch(&mut self, prefetch: ProbePrefetch) {
        self.prefetched = prefetch.batches;
        self.filter_reserved = prefetch.keys_reserved;
    }

    /// A complete primary-key membership set for a composite resident build.
    /// The final join still checks every component. This set only rejects
    /// rows that cannot possibly match, before an intermediate join copies them.
    pub(super) fn primary_membership(
        &mut self,
        memory: &MemoryTracker,
    ) -> Option<Arc<HashSet<JoinHashKey>>> {
        if self.spilled() || self.build.dense_index.is_some() {
            return None;
        }
        // Include hash capacity, the packed filter and its temporary integer
        // vector. Refuse the optimization as a whole when it cannot fit.
        let bytes = self.build.len().checked_mul(256)?.checked_add(2 << 20)?;
        if bytes > (64 << 20) || bytes > memory.remaining() / 4 {
            return None;
        }
        memory.reserve(bytes).ok()?;
        let mut keys = HashSet::with_capacity(self.build.len());
        for key in self.build.keys() {
            let JoinHashKey::Composite(parts) = key else {
                memory.release(bytes);
                return None;
            };
            let Some(key @ (JoinHashKey::NegativeInteger(_) | JoinHashKey::NonNegativeInteger(_))) =
                parts.first()
            else {
                memory.release(bytes);
                return None;
            };
            keys.insert(key.clone());
        }
        self.filter_reserved = self.filter_reserved.saturating_add(bytes);
        Some(Arc::new(keys))
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

/// Reserves for and inserts one build row into the resident map. Every
/// reservation happens here so a failure can be matched on by the caller,
/// which decides whether the map has anything to spill.
#[allow(clippy::too_many_arguments)]
fn insert_resident_row(
    build: &mut PartitionedBuild,
    key: JoinHashKey,
    batch: &RecordBatch,
    row: usize,
    batch_bytes: usize,
    right_key: &CompiledExpr,
    memory: &MemoryTracker,
) -> Result<(), ExecError> {
    let row_bytes = estimated_batch_row_bytes(batch, row)?;
    let key_memory = right_key
        .allocation_upper_bound(batch, row)
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
    let values = batch_row(batch, row)?;
    bucket.push(values);
    Ok(())
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
        let batch_bytes = batch.estimated_bytes();
        // A wide upstream join can return more than a scan-sized batch.
        // Bin it in bounded pieces when it occupies most of the headroom,
        // so resident insertion reaches its spill valve before keys alone
        // exhaust the ceiling. Ordinary batches retain their partition walk.
        let bin_rows = if batch_bytes > memory.remaining() / 2 {
            1024
        } else {
            batch.row_count().max(1)
        };
        let mut rows = batch.selection().selected_rows().peekable();
        while rows.peek().is_some() {
            let mut used_before_batch = memory.used();
            // Keys first, binned by the partition each will land in; the inserts
            // follow one partition at a time.
            let mut binned: Vec<Vec<(JoinHashKey, usize)>> = vec![Vec::new(); build.partitions()];
            // Keys are held for the whole batch before any of them are inserted, so
            // they are charged as they accumulate. Without this a batch of long
            // text keys allocates every normalized key at once and passes the
            // query's ceiling before a single per-row check runs.
            let mut binned_bytes = 0_usize;
            for row in rows.by_ref().take(bin_rows) {
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
                // Previously binned keys are already reserved. Only the incoming
                // key and the live batch are additional to the tracker here.
                memory.ensure_transient(batch_bytes.saturating_add(retained))?;
                memory.reserve(retained)?;
                binned_bytes = binned_bytes.saturating_add(retained);
                binned[build.slot(&key)].push((key, row));
            }
            for (key, row) in binned.into_iter().flatten() {
                if let Some(grace) = grace.as_mut() {
                    let values = batch_row(&batch, row)?;
                    grace.build_files[grace_partition(&key, 0)].append(&key, &values, memory)?;
                    continue;
                }
                match insert_resident_row(
                    &mut build,
                    key,
                    &batch,
                    row,
                    batch_bytes,
                    right_key,
                    memory,
                ) {
                    Ok(()) => {}
                    Err(ExecError::MemoryLimitExceeded { .. }) if !build.is_empty() => {
                        // Out of memory with rows to spill: either the query's
                        // own ceiling landed inside one batch, past the
                        // proactive half-ceiling valve below, or the process
                        // budget refused what the query ceiling allowed. Both
                        // used to fail the query, and under load the second was
                        // the common one: every admitted query is entitled to
                        // its own ceiling, but their sum is not, so the budget
                        // is a spill signal here, not a verdict. Drain the map
                        // to partitions and route this row there.
                        let mut partitions = GraceJoin::create();
                        for (key, bucket) in build.drain() {
                            let target = grace_partition(&key, 0);
                            for values in bucket {
                                partitions.build_files[target].append(&key, &values, memory)?;
                            }
                        }
                        // Everything this batch reserved beyond its binned keys,
                        // plus the map from earlier batches; a partial insert's
                        // reservations are included because the map is empty now.
                        let this_batch = memory
                            .used()
                            .saturating_sub(used_before_batch)
                            .saturating_sub(binned_bytes);
                        memory.release(build_reserved.saturating_add(this_batch));
                        build_reserved = 0;
                        used_before_batch = memory.used().saturating_sub(binned_bytes);
                        let value = right_key.evaluate(&batch, row)?;
                        let key = normalized_join_key(value, key_mode)?
                            .and_then(|key| {
                                composite_join_key(key, &batch, row, extra_keys, JoinSide::Build)
                                    .transpose()
                            })
                            .transpose()?
                            .expect("the key was binned, so it normalizes");
                        let values = batch_row(&batch, row)?;
                        partitions.build_files[grace_partition(&key, 0)]
                            .append(&key, &values, memory)?;
                        grace = Some(partitions);
                    }
                    Err(error) => return Err(error),
                }
            }
            memory.release(binned_bytes);
            build_reserved =
                build_reserved.saturating_add(memory.used().saturating_sub(used_before_batch));
            // Proactive spill at half the ceiling, like sort and aggregation:
            // drain the resident map into partition files and route the rest
            // of the build (and later the probe) through them.
            if grace.is_none() && build_reserved > memory.limit() / 2 && !build.is_empty() {
                let mut partitions = GraceJoin::create();
                for (key, bucket) in build.drain() {
                    let target = grace_partition(&key, 0);
                    for values in bucket {
                        partitions.build_files[target].append(&key, &values, memory)?;
                    }
                }
                memory.release(build_reserved);
                build_reserved = 0;
                grace = Some(partitions);
            }
        }
    }
    // A build that stayed resident (no grace spill) is never mutated again:
    // every remaining reader only probes it. Dense direct-address probe
    // (experiments/RESULTS.md e04, 2.4-4.2x; e85 extends it to every reader
    // of `get`, not only the fused join-aggregate).
    if grace.is_none() {
        build.finalize_dense();
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
        prefetched: VecDeque::new(),
        filter_reserved: 0,
    })
}

/// Probe rows the join reads before building, and, when the probe side
/// ended within the caps, the set of keys it carries.
#[derive(Default)]
pub(super) struct ProbePrefetch {
    pub(super) batches: VecDeque<(RecordBatch, usize)>,
    pub(super) keys: Option<Arc<HashSet<JoinHashKey>>>,
    pub(super) keys_reserved: usize,
}

/// Probe rows read ahead of the build before the join gives up on
/// filtering the build side. A probe side that ends within this many rows
/// is small enough that reading it twice over would cost nothing worth
/// having; one that does not is served from the read-ahead first and
/// streamed after, and the build side is taken whole.
pub(super) const PROBE_PREFETCH_ROWS: u64 = 65_536;

/// Distinct probe keys the build-side filter will hold.
const PROBE_FILTER_KEYS: usize = 65_536;

/// The build side must be estimated at least this many times the probe's
/// rows before the probe is read ahead to filter it. Reading ahead costs a
/// key per probe row and a test per build row; a probe near the build's
/// size can drop few build rows for that, and a probe the build's equal
/// drops none. The instruction gate's 4,096-row self-join measured the
/// read-ahead at a seventh of the whole query while filtering nothing.
pub(super) const PROBE_PREFETCH_BUILD_RATIO: u64 = 4;

/// Whether the probe side is read ahead of the build.
///
/// A left or anti join keeps every probe row and never restricts the probe
/// scan, so reading ahead costs it nothing beyond the bounded read-ahead
/// itself. An inner or semi join restricts its probe scan to the build's
/// key span once the build exists, and a probe scan that has started
/// cannot be restricted, so those read ahead only when statistics say the
/// probe side is small enough to end within the read-ahead. When both
/// sides are estimated, every kind reads ahead only if the build is at
/// least [`PROBE_PREFETCH_BUILD_RATIO`] times the probe.
pub(super) const fn probe_prefetch_applies(
    kind: BoundJoinKind,
    probe_estimate: Option<u64>,
    build_estimate: Option<u64>,
) -> bool {
    if let (Some(probe), Some(build)) = (probe_estimate, build_estimate)
        && probe.saturating_mul(PROBE_PREFETCH_BUILD_RATIO) > build
    {
        return false;
    }
    match kind {
        BoundJoinKind::Left | BoundJoinKind::Anti => true,
        BoundJoinKind::Inner | BoundJoinKind::Semi => {
            matches!(probe_estimate, Some(rows) if rows <= PROBE_PREFETCH_ROWS)
        }
        BoundJoinKind::Scalar | BoundJoinKind::Cross => false,
    }
}

/// Reads the probe side ahead of the build, up to the row, key and memory
/// caps, collecting the normalized join key of every row. The batches are
/// kept for the probe in their original order; the key set exists only if
/// the probe side ended within the caps, since a partial set would drop
/// build rows that later probe rows need.
pub(super) fn prefetch_probe(
    left: &mut PullOperator,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    memory: &MemoryTracker,
) -> Result<ProbePrefetch, ExecError> {
    let mut prefetch = ProbePrefetch::default();
    let mut keys: HashSet<JoinHashKey> = HashSet::new();
    let mut keys_reserved = 0_usize;
    let mut rows = 0_u64;
    let mut held = 0_usize;
    let mut last_bytes = 0_usize;
    let complete = loop {
        // The read-ahead never takes more than a quarter of the ceiling,
        // nor more than half of what is free right now: the build side and
        // the probe's own working set still have to fit, and in a chain of
        // joins each level's read-ahead pulls the level below it, so the
        // nested holdings have to converge rather than each take a quarter.
        let ceiling = (memory.limit() / 4).min(memory.remaining() / 2);
        if rows >= PROBE_PREFETCH_ROWS
            || keys.len() >= PROBE_FILTER_KEYS
            || held.saturating_add(last_bytes) > ceiling
        {
            break false;
        }
        let Some(batch) = left.next_batch(memory)? else {
            break true;
        };
        let bytes = batch.estimated_bytes();
        memory.reserve(bytes)?;
        held = held.saturating_add(bytes);
        last_bytes = bytes;
        rows = rows.saturating_add(u64::try_from(batch.visible_row_count()).unwrap_or(u64::MAX));
        for row in batch.selection().selected_rows() {
            memory.ensure_transient(
                bytes.saturating_add(left_key.allocation_upper_bound(&batch, row)),
            )?;
            let Some(key) = normalized_join_key(left_key.evaluate(&batch, row)?, key_mode)? else {
                continue;
            };
            if keys.contains(&key) {
                continue;
            }
            let cost = key
                .heap_bytes()
                .saturating_add(size_of::<JoinHashKey>())
                .saturating_add(HASH_ENTRY_OVERHEAD);
            memory.reserve(cost)?;
            keys_reserved = keys_reserved.saturating_add(cost);
            keys.insert(key);
        }
        prefetch.batches.push_back((batch, bytes));
    };
    if complete {
        prefetch.keys = Some(Arc::new(keys));
        prefetch.keys_reserved = keys_reserved;
    } else {
        memory.release(keys_reserved);
    }
    Ok(prefetch)
}

/// The probe side's integer keys as a membership structure the build-side
/// filter tests straight from a packed column, without building a `Value`
/// and a hash key per build row. That per-row path cost about 1,800
/// instructions a row on the instruction gate's 4,096-row join - a fifth of
/// the whole query - for a test that is one bit lookup.
#[derive(Debug)]
pub(super) enum IntegerKeySet {
    /// One bit per value from `low` upward, when the keys span a narrow range.
    Bitmap { low: i128, words: Vec<u64> },
    /// The keys themselves when the span is too wide for a bitmap.
    Sparse(HashSet<i128>),
}

impl IntegerKeySet {
    /// Widest key span the bitmap form covers: sixteen million values, two
    /// mebibytes of bits.
    const BITMAP_SPAN: i128 = 1 << 24;

    /// Builds the set from the probe's keys; `None` when any key is not an
    /// integer, since the packed test has nothing to compare then.
    pub(super) fn from_keys(keys: &HashSet<JoinHashKey>) -> Option<Self> {
        let mut values = Vec::with_capacity(keys.len());
        for key in keys {
            match key {
                JoinHashKey::NegativeInteger(value) => values.push(i128::from(*value)),
                JoinHashKey::NonNegativeInteger(value) => values.push(i128::from(*value)),
                _ => return None,
            }
        }
        let (Some(low), Some(high)) = (values.iter().min().copied(), values.iter().max().copied())
        else {
            return Some(Self::Sparse(HashSet::new()));
        };
        if high - low < Self::BITMAP_SPAN {
            let span = usize::try_from(high - low + 1).ok()?;
            let mut words = vec![0_u64; span.div_ceil(64)];
            for value in values {
                let offset = usize::try_from(value - low).ok()?;
                words[offset / 64] |= 1 << (offset % 64);
            }
            Some(Self::Bitmap { low, words })
        } else {
            Some(Self::Sparse(values.into_iter().collect()))
        }
    }

    pub(super) fn contains(&self, value: i128) -> bool {
        match self {
            Self::Bitmap { low, words } => {
                let Some(offset) = value
                    .checked_sub(*low)
                    .and_then(|delta| usize::try_from(delta).ok())
                else {
                    return false;
                };
                words
                    .get(offset / 64)
                    .is_some_and(|word| word & (1 << (offset % 64)) != 0)
            }
            Self::Sparse(values) => values.contains(&value),
        }
    }
}

/// The smallest and largest integer key in the set, as values a scan can
/// restrict on; `None` when any key is not an integer.
pub(super) fn integer_key_span(keys: &HashSet<JoinHashKey>) -> Option<(Value, Value)> {
    let mut negatives: Option<(i64, i64)> = None;
    let mut naturals: Option<(u64, u64)> = None;
    for key in keys {
        match key {
            JoinHashKey::NegativeInteger(value) => {
                negatives = Some(negatives.map_or((*value, *value), |(low, high)| {
                    (low.min(*value), high.max(*value))
                }));
            }
            JoinHashKey::NonNegativeInteger(value) => {
                naturals = Some(naturals.map_or((*value, *value), |(low, high)| {
                    (low.min(*value), high.max(*value))
                }));
            }
            _ => return None,
        }
    }
    let minimum = match (negatives, naturals) {
        (Some((low, _)), _) => Value::Int64(low),
        (None, Some((low, _))) => Value::UInt64(low),
        (None, None) => return None,
    };
    let maximum = match (naturals, negatives) {
        (Some((_, high)), _) => Value::UInt64(high),
        (None, Some((_, high))) => Value::Int64(high),
        (None, None) => return None,
    };
    Some((minimum, maximum))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
/// Number of grace-join partitions; key hashes route rows uniformly, so
/// each partition holds roughly build-bytes / 16.
pub(super) const GRACE_PARTITIONS: usize = 16;

/// One append-mode spill file of `(join key, row values)` pairs.
///
/// Rows are framed into a small in-memory buffer and reach the file in
/// bursts, each an open, append and close, so the thirty-two partitions a
/// grace join feeds round-robin hold one descriptor between them rather
/// than one each. The file is created on the first flush, so a partition
/// that never receives a row costs nothing.
pub(super) struct GraceRun {
    run: Option<spill::AppendRun>,
    /// Framed records not yet on disk.
    buffer: Vec<u8>,
    buffered: u64,
    /// Whether the buffer's capacity is charged to the query. A partition
    /// the ceiling could not afford a buffer for flushes every append.
    charged: bool,
    closed: Option<spill::ClosedRun>,
    sealed: bool,
    entries: u64,
}

/// Bytes a partition buffers before it flushes. Small enough that a
/// join's two sets of sixteen partitions buffer one megabyte between
/// them, large enough that a flush writes whole rows by the hundred.
const GRACE_BUFFER: usize = 32 * 1024;

impl GraceRun {
    const fn create() -> Self {
        Self {
            run: None,
            buffer: Vec::new(),
            buffered: 0,
            charged: false,
            closed: None,
            sealed: false,
            entries: 0,
        }
    }

    fn append(
        &mut self,
        key: &JoinHashKey,
        row: &[Value],
        memory: &MemoryTracker,
    ) -> Result<(), ExecError> {
        if self.sealed {
            return Err(ExecError::InvalidPhysicalPlan("grace run already sealed"));
        }
        if !self.charged && self.buffer.capacity() == 0 {
            match memory.reserve(GRACE_BUFFER) {
                Ok(()) => {
                    self.buffer.reserve_exact(GRACE_BUFFER);
                    self.charged = true;
                }
                // No room for a buffer: every append goes straight to disk,
                // slower and still correct, rather than failing a join
                // that is spilling precisely because memory is short.
                Err(ExecError::MemoryLimitExceeded { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let mut encoder = spill::Encoder::with_capacity(64);
        encode_join_key(&mut encoder, key);
        encoder.values(row);
        spill::write_record(&mut self.buffer, &encoder.finish())
            .map_err(|error| ExecError::Source(format!("join spill frame: {error}")))?;
        self.buffered += 1;
        self.entries += 1;
        if !self.charged || self.buffer.len() >= GRACE_BUFFER {
            self.flush(memory)?;
        }
        Ok(())
    }

    /// Writes the buffered rows in one open-append-close.
    fn flush(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        if self.run.is_none() {
            self.run = Some(
                spill::AppendRun::create("pintail-join-spill-", memory.spill())
                    .map_err(|error| ExecError::Source(format!("join spill create: {error}")))?,
            );
        }
        self.run
            .as_mut()
            .expect("created above")
            .flush(&self.buffer, self.buffered)
            .map_err(|error| ExecError::Source(format!("join spill write: {error}")))?;
        self.buffer.clear();
        self.buffered = 0;
        Ok(())
    }

    /// Seals the run on first use and opens a fresh reader over its file.
    ///
    /// Every call reopens. The serve loop reads a partition's build side to
    /// find out whether it fits and, when it does not, splits that same
    /// partition by reading it again; a single-use reader turned the second
    /// read into "grace run read twice", so a build side that overflowed
    /// while being served could never be re-partitioned and the depth bound
    /// was unreachable from the path that needs it. A sealed run holds no
    /// descriptor of its own: the closed run keeps the file alive, and the
    /// reader is the only handle for as long as it lives.
    fn reader(&mut self, memory: &MemoryTracker) -> Result<GraceRunReader, ExecError> {
        self.seal(memory)?;
        let reader = match &self.closed {
            Some(closed) => Some(
                closed
                    .open()
                    .map_err(|error| ExecError::Source(format!("join spill reopen: {error}")))?,
            ),
            None => None,
        };
        Ok(GraceRunReader { reader })
    }

    /// Flushes what is buffered and closes the run. Idempotent; a sealed
    /// run refuses further appends.
    fn seal(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        self.sealed = true;
        self.flush(memory)?;
        if let Some(run) = self.run.take() {
            self.closed = Some(run.seal());
        }
        self.buffer = Vec::new();
        if self.charged {
            memory.release(GRACE_BUFFER);
            self.charged = false;
        }
        Ok(())
    }
}

/// Streams back one grace-join spill file; `None` is a run nothing was
/// ever appended to.
struct GraceRunReader {
    reader: Option<spill::RunReader>,
}

impl GraceRunReader {
    fn next_entry(&mut self) -> Result<Option<(JoinHashKey, Vec<Value>)>, ExecError> {
        let Some(reader) = self.reader.as_mut() else {
            return Ok(None);
        };
        let Some(payload) = reader
            .next()
            .map_err(|error| ExecError::Source(format!("join spill read: {error}")))?
        else {
            return Ok(None);
        };
        let mut decoder = spill::Decoder::new(payload);
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

/// A partition that hashing cannot shrink is replayed one build row at a
/// time for each probe. Keeping the probe's match count across the replay
/// preserves outer, scalar, semi, and anti semantics without duplicating
/// unmatched output across sub-partitions.
struct SkewReplay {
    build: GraceRun,
    probes: GraceRunReader,
    current: Option<(JoinHashKey, Vec<Value>)>,
    entries: Option<GraceRunReader>,
    matches: usize,
    reserved: usize,
    scalar: Option<Vec<Value>>,
    /// The build rows a budget lets this replay keep, in file order. Every
    /// probe walks these from memory before the file continues where they
    /// stop, so the rows that fit are read from disk once rather than once
    /// per probe.
    resident: Vec<(JoinHashKey, Vec<Value>)>,
    resident_bytes: usize,
    /// Build rows past the resident prefix, in file order.
    tail: Option<GraceRun>,
    prepared: bool,
    /// Position of the current probe within the resident prefix.
    cursor: usize,
}

impl SkewReplay {
    fn new(build: GraceRun, probes: GraceRunReader) -> Self {
        Self {
            build,
            probes,
            current: None,
            entries: None,
            matches: 0,
            reserved: 0,
            scalar: None,
            resident: Vec::new(),
            resident_bytes: 0,
            tail: None,
            prepared: false,
            cursor: 0,
        }
    }

    /// Reads the partition once, keeping what a quarter of the ceiling
    /// affords and writing the rest to its own run. A partition that fits
    /// entirely leaves no tail and is never read from disk again.
    fn prepare(&mut self, memory: &MemoryTracker) -> Result<(), ExecError> {
        if self.prepared {
            return Ok(());
        }
        self.prepared = true;
        let budget = (memory.limit() / 4).min(memory.remaining() / 2);
        let mut source = self.build.reader(memory)?;
        let mut tail = GraceRun::create();
        let mut spilling = false;
        while let Some((key, row)) = source.next_entry()? {
            if !spilling {
                let bytes = estimated_row_payload_bytes(&row)
                    .saturating_add(key.heap_bytes())
                    .saturating_add(size_of::<(JoinHashKey, Vec<Value>)>());
                if self.resident_bytes.saturating_add(bytes) <= budget
                    && memory.reserve(bytes).is_ok()
                {
                    self.resident_bytes = self.resident_bytes.saturating_add(bytes);
                    self.resident.push((key, row));
                    continue;
                }
                spilling = true;
            }
            tail.append(&key, &row, memory)?;
        }
        if tail.entries > 0 {
            self.tail = Some(tail);
        }
        Ok(())
    }

    /// The next build row for the current probe: the resident prefix first,
    /// then the tail file from where the prefix stops.
    fn next_build(
        &mut self,
        memory: &MemoryTracker,
    ) -> Result<Option<(JoinHashKey, Vec<Value>)>, ExecError> {
        if self.cursor < self.resident.len() {
            let entry = self.resident[self.cursor].clone();
            self.cursor += 1;
            return Ok(Some(entry));
        }
        if self.entries.is_none() {
            let Some(tail) = self.tail.as_mut() else {
                return Ok(None);
            };
            self.entries = Some(tail.reader(memory)?);
        }
        self.entries.as_mut().expect("tail opened").next_entry()
    }

    /// Hands back the resident prefix once the partition is served.
    fn release(&mut self, memory: &MemoryTracker) {
        self.clear_probe(memory);
        memory.release(self.resident_bytes);
        self.resident = Vec::new();
        self.resident_bytes = 0;
    }

    fn clear_probe(&mut self, memory: &MemoryTracker) {
        self.current = None;
        self.scalar = None;
        self.entries = None;
        self.cursor = 0;
        memory.release(self.reserved);
        self.reserved = 0;
        self.matches = 0;
    }

    fn next_row(
        &mut self,
        kind: BoundJoinKind,
        right_width: usize,
        residual: Option<&CompiledExpr>,
        columns: &[BoundColumn],
        memory: &MemoryTracker,
    ) -> Result<Option<Vec<Value>>, ExecError> {
        self.prepare(memory)?;
        loop {
            memory.check_interruption()?;
            if self.current.is_none() {
                let Some((key, row)) = self.probes.next_entry()? else {
                    return Ok(None);
                };
                self.reserved = estimated_row_payload_bytes(&row).saturating_add(key.heap_bytes());
                memory.reserve(self.reserved)?;
                self.current = Some((key, row));
                self.cursor = 0;
                self.entries = None;
            }
            let next = self.next_build(memory)?;
            let (probe_key, left) = self.current.as_ref().expect("probe loaded");
            if let Some((key, right)) = next {
                memory.ensure_transient(
                    estimated_row_payload_bytes(&right).saturating_add(key.heap_bytes()),
                )?;
                if &key != probe_key {
                    continue;
                }
                if residual.is_some() {
                    let candidates = vec![right.clone()];
                    if apply_join_residual(residual, columns, left, Some(&candidates))?
                        .is_none_or(|rows| rows.is_empty())
                    {
                        continue;
                    }
                }
                self.matches = self.matches.saturating_add(1);
                match kind {
                    BoundJoinKind::Inner | BoundJoinKind::Left | BoundJoinKind::Scalar => {
                        if kind == BoundJoinKind::Scalar && self.matches > 1 {
                            return Err(ExecError::ScalarSubqueryRows { rows: self.matches });
                        }
                        let mut output = left.clone();
                        output.extend(right);
                        let bytes = estimated_row_payload_bytes(&output);
                        memory.ensure_transient(bytes)?;
                        if kind == BoundJoinKind::Scalar {
                            memory.reserve(bytes)?;
                            self.reserved += bytes;
                            self.scalar = Some(output);
                            continue;
                        }
                        return Ok(Some(output));
                    }
                    BoundJoinKind::Semi => {
                        let output = left.clone();
                        self.clear_probe(memory);
                        return Ok(Some(output));
                    }
                    BoundJoinKind::Anti => {
                        self.clear_probe(memory);
                    }
                    BoundJoinKind::Cross => {
                        return Err(ExecError::InvalidPhysicalPlan(
                            "cross join reached skew replay",
                        ));
                    }
                }
            } else {
                let output = if self.matches == 0 {
                    match kind {
                        BoundJoinKind::Left | BoundJoinKind::Scalar => {
                            let mut row = left.clone();
                            row.extend(std::iter::repeat_n(Value::Null, right_width));
                            Some(row)
                        }
                        BoundJoinKind::Anti => Some(left.clone()),
                        _ => None,
                    }
                } else {
                    self.scalar.take()
                };
                self.clear_probe(memory);
                if output.is_some() {
                    return Ok(output);
                }
            }
        }
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
    skew: Option<SkewReplay>,
}

impl GraceJoin {
    fn create() -> Self {
        let mut build_files = Vec::with_capacity(GRACE_PARTITIONS);
        let mut probe_files = Vec::with_capacity(GRACE_PARTITIONS);
        for _ in 0..GRACE_PARTITIONS {
            build_files.push(GraceRun::create());
            probe_files.push(GraceRun::create());
        }
        Self {
            build_files,
            probe_files,
            depths: vec![0; GRACE_PARTITIONS],
            probing_done: false,
            current: 0,
            replay: None,
            partition_reserved: 0,
            skew: None,
        }
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
        grace.build_files.push(GraceRun::create());
        grace.probe_files.push(GraceRun::create());
        grace.depths.push(depth + 1);
    }
    let seed = u64::try_from(depth).unwrap_or(0).saturating_add(1);
    // Move each source file out so its replacement can be written to while
    // the original is read; the emptied slot is never served again.
    let mut build = std::mem::replace(&mut grace.build_files[index], GraceRun::create());
    let mut entries = build.reader(memory)?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.build_files[target].append(&key, &values, memory)?;
    }
    let mut probe = std::mem::replace(&mut grace.probe_files[index], GraceRun::create());
    let mut entries = probe.reader(memory)?;
    while let Some((key, values)) = entries.next_entry()? {
        let target = first + grace_partition(&key, seed);
        grace.probe_files[target].append(&key, &values, memory)?;
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
        // Turning the buffered rows into a batch needs about as much again
        // for the columns, so under a tight ceiling the batch is cut where
        // that copy still fits rather than refused once it is buffered.
        if !rows.is_empty() && buffered_bytes.saturating_mul(2) > memory.remaining() {
            break;
        }
        if state.left_values.is_none()
            && !prepare_hash_join_left(
                left,
                left_key,
                key_mode,
                extra_keys,
                state,
                memory,
                matches!(kind, BoundJoinKind::Inner | BoundJoinKind::Semi),
            )?
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
            && !prepare_hash_join_left(left, left_key, key_mode, extra_keys, state, memory, false)?
        {
            let grace = state.grace.as_mut().expect("grace state engaged");
            grace.probing_done = true;
            // Routing is over, so no run receives another row. Closing every
            // writer now drops the join's largest descriptor holding - two
            // per partition - before serving opens them again one at a time.
            for run in grace
                .build_files
                .iter_mut()
                .chain(grace.probe_files.iter_mut())
            {
                run.seal(memory)?;
            }
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
                grace.probe_files[grace_partition(&key, 0)].append(&key, &left_values, memory)?;
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
        // Turning the buffered rows into a batch needs about as much again
        // for the columns, so under a tight ceiling the batch is cut where
        // that copy still fits rather than refused once it is buffered.
        if !rows.is_empty() && buffered_bytes.saturating_mul(4) > memory.remaining() {
            break;
        }
        let grace = state.grace.as_mut().expect("grace state engaged");
        if !grace.probing_done {
            break;
        }
        if let Some(skew) = &mut grace.skew {
            if let Some(output) =
                skew.next_row(kind, right_width, residual, residual_columns, memory)?
            {
                push(&mut rows, &mut buffered_bytes, output)?;
                continue;
            }
            skew.release(memory);
            grace.skew = None;
        }
        if grace.replay.is_none() {
            if grace.current >= grace.build_files.len() {
                // Served: the partition files go now rather than when the
                // operator is dropped, which for a streamed join is later.
                let held = state
                    .grace
                    .take()
                    .map_or(0, |grace| grace.partition_reserved);
                memory.release(held);
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
            let mut entries = grace.build_files[index].reader(memory)?;
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
                if grace.depths[index] >= MAX_GRACE_DEPTH {
                    let build =
                        std::mem::replace(&mut grace.build_files[index], GraceRun::create());
                    let probes = grace.probe_files[index].reader(memory)?;
                    grace.skew = Some(SkewReplay::new(build, probes));
                    continue;
                }
                split_grace_partition(grace, index, memory)?;
                continue;
            }
            grace.partition_reserved = memory.used().saturating_sub(used_before);
            grace.replay = Some(grace.probe_files[index].reader(memory)?);
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
        // Served to the end, or nothing to serve: the grace state is gone
        // once every partition has been read.
        if let Some(grace) = state.grace.as_mut() {
            memory.release(grace.partition_reserved);
            grace.partition_reserved = 0;
        }
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

#[allow(clippy::too_many_arguments)] // resident and spilled probes share row preparation
fn prepare_hash_join_left(
    left: &mut PullOperator,
    left_key: &CompiledExpr,
    key_mode: JoinKeyMode,
    extra_keys: &[(CompiledExpr, CompiledExpr, JoinKeyMode)],
    state: &mut HashJoinState,
    memory: &MemoryTracker,
    discard_unmatched: bool,
) -> Result<bool, ExecError> {
    loop {
        let exhausted = state
            .batch
            .as_ref()
            .is_some_and(|batch| state.row >= batch.row_count());
        if state.batch.is_none() || exhausted {
            state.clear_batch(memory);
            // Batches read ahead of the build come first, in order; their
            // read-ahead reservation is handed back before the probe takes
            // its own, so the bytes are charged once.
            let next = match state.prefetched.pop_front() {
                Some((batch, bytes)) => {
                    memory.release(bytes);
                    Some(batch)
                }
                None => left.next_batch(memory)?,
            };
            let Some(batch) = next else {
                memory.release(state.filter_reserved);
                state.filter_reserved = 0;
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
        let key_memory = left_key
            .allocation_upper_bound(batch, row)
            .saturating_add(
                extra_keys
                    .iter()
                    .map(|(key, _, _)| key.allocation_upper_bound(batch, row))
                    .fold(0_usize, usize::saturating_add),
            )
            .saturating_mul(12)
            .saturating_add(if extra_keys.is_empty() {
                0
            } else {
                (extra_keys.len() + 1).saturating_mul(size_of::<JoinHashKey>())
            });
        memory.ensure_transient(key_memory)?;
        state.left_key = match normalized_join_key(left_key.evaluate(batch, row)?, key_mode)? {
            Some(primary) => composite_join_key(primary, batch, row, extra_keys, JoinSide::Probe)?,
            None => None,
        };
        // Inner/semi probes with no resident bucket cannot produce a row.
        // Test the complete normalized key before touching projected values:
        // decoding decimal/text payloads for rejected probes dominates sparse
        // joins. Spilled, outer, anti and scalar joins retain their row path.
        if discard_unmatched
            && state
                .left_key
                .as_ref()
                .is_none_or(|key| state.build.get(key).is_none())
        {
            state.left_key = None;
            continue;
        }
        let row_bytes = estimated_batch_row_bytes(batch, row)?;
        memory.ensure_transient(row_bytes.saturating_add(key_memory))?;
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
        Value::DecimalAverage(value) => {
            Value::Utf8(normalized_collation_text(&value.label, collation))
        }
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
        Collation::Utf8mb4UnicodeCi => key = crate::collation::unicode_ci_sort_key(text),
        Collation::Utf8mb4Bin => key = crate::collation::bin_sort_key(text),
        Collation::Json => {
            key = crate::json_order::json_sort_key(text)
                .unwrap_or_else(|| crate::collation::bin_sort_key(text));
        }
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
        Collation::Utf8mb4UnicodeCi => crate::collation::compare_unicode_ci(left, right),
        Collation::Utf8mb4Bin => crate::collation::compare_bin(left, right),
        // Text that is not JSON (possible only through casts and mixed
        // sources) falls back to the byte comparison rather than erroring
        // out of a comparator that cannot return errors.
        Collation::Json => crate::json_order::compare_json_text(left, right)
            .unwrap_or_else(|| crate::collation::compare_bin(left, right)),
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

/// A replayable nested-loop side or result. Its resident prefix is replaced
/// by one append-only run as soon as its share of the query budget fills.
struct LoopRows {
    rows: Vec<Vec<Value>>,
    writer: Option<spill::RunWriter>,
    run: Option<spill::ClosedRun>,
    reserved: usize,
}

impl LoopRows {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            writer: None,
            run: None,
            reserved: 0,
        }
    }

    fn push(&mut self, row: Vec<Value>, memory: &MemoryTracker) -> Result<(), ExecError> {
        let bytes = estimated_row_payload_bytes(&row);
        if self.writer.is_none()
            && (self.reserved + bytes > memory.limit() / 8
                || bytes.saturating_mul(2) > memory.remaining())
        {
            let mut writer = spill::RunWriter::create("pintail-loop-", memory.spill())
                .map_err(|error| ExecError::Source(error.to_string()))?;
            for row in &self.rows {
                write_loop_row(&mut writer, row)?;
            }
            self.rows = Vec::new();
            memory.release(self.reserved);
            self.reserved = 0;
            self.writer = Some(writer);
        }
        if let Some(writer) = &mut self.writer {
            memory.ensure_transient(bytes)?;
            write_loop_row(writer, &row)?;
        } else {
            self.reserved += reserve_vec_elements(&mut self.rows, 1, 0, memory)?;
            memory.reserve(bytes)?;
            self.reserved += bytes;
            self.rows.push(row);
        }
        Ok(())
    }

    fn seal(&mut self) -> Result<(), ExecError> {
        if let Some(writer) = self.writer.take() {
            self.run = Some(
                writer
                    .finish()
                    .map_err(|error| ExecError::Source(error.to_string()))?,
            );
        }
        Ok(())
    }

    fn reader(&self) -> Result<LoopReader<'_>, ExecError> {
        if let Some(run) = &self.run {
            Ok(LoopReader::Disk(
                run.open()
                    .map_err(|error| ExecError::Source(error.to_string()))?,
            ))
        } else {
            Ok(LoopReader::Memory(self.rows.iter()))
        }
    }

    fn finish(
        mut self,
        memory: &MemoryTracker,
        collation: Collation,
    ) -> Result<super::SortedRows, ExecError> {
        self.seal()?;
        if let Some(run) = self.run {
            super::sort::SpilledMerge::new(vec![run], &[], Vec::new(), None, collation, memory)
                .map(super::SortedRows::Spilled)
        } else {
            Ok(super::SortedRows::Memory(super::MaterializedRows {
                rows: self.rows,
                position: 0,
                spilled: None,
            }))
        }
    }
}

fn write_loop_row(writer: &mut spill::RunWriter, row: &[Value]) -> Result<(), ExecError> {
    let mut encoder = spill::Encoder::new();
    encoder.values(row);
    writer
        .write(&encoder.finish())
        .map_err(|error| ExecError::Source(error.to_string()))
}

enum LoopReader<'a> {
    Memory(std::slice::Iter<'a, Vec<Value>>),
    Disk(spill::RunReader),
}

impl LoopReader<'_> {
    fn next_row(&mut self) -> Result<Option<Vec<Value>>, ExecError> {
        match self {
            Self::Memory(rows) => Ok(rows.next().cloned()),
            Self::Disk(reader) => reader
                .next()
                .map_err(|error| ExecError::Source(error.to_string()))?
                .map(|payload| {
                    spill::Decoder::new(payload)
                        .values()
                        .map_err(ExecError::Source)
                })
                .transpose(),
        }
    }
}

// Keep the candidate ownership and each join kind in one evaluation loop.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn execute_nested_loop_join(
    left_input: &mut PullOperator,
    right_input: &mut PullOperator,
    left_columns: &[BoundColumn],
    right_columns: &[BoundColumn],
    kind: BoundJoinKind,
    keys: &[(BoundExpr, BoundExpr)],
    condition: &BoundExpr,
    provider: &dyn ScanProvider,
    memory: &MemoryTracker,
    collation: Collation,
) -> Result<super::SortedRows, ExecError> {
    let mut columns = left_columns.to_vec();
    columns.extend_from_slice(right_columns);
    let column_types = columns
        .iter()
        .map(|column| column.data_type)
        .collect::<Vec<_>>();
    let loop_keys = LoopKeys::compile(keys, left_columns, right_columns, collation)?;
    let mut right_rows = LoopRows::new();
    // Each in-memory right row's key, in row order. A right side that
    // spills is replayed whole instead, so its keys are dropped.
    let mut right_keys: Vec<Option<JoinHashKey>> = Vec::new();
    let mut keys_reserved = 0_usize;
    while let Some(batch) = right_input.next_batch(memory)? {
        for row in batch.selection().selected_rows() {
            right_rows.push(batch_row(&batch, row)?, memory)?;
            let Some(loop_keys) = &loop_keys else {
                continue;
            };
            if right_rows.writer.is_some() {
                memory.release(keys_reserved);
                keys_reserved = 0;
                right_keys = Vec::new();
                continue;
            }
            let key = LoopKeys::key(&loop_keys.right, &batch, row)?;
            let bytes = std::mem::size_of::<Option<JoinHashKey>>()
                .saturating_add(key.as_ref().map_or(0, JoinHashKey::heap_bytes));
            memory.reserve(bytes)?;
            keys_reserved = keys_reserved.saturating_add(bytes);
            right_keys.push(key);
        }
    }
    right_rows.seal()?;
    let buckets = match &loop_keys {
        Some(_) if right_rows.run.is_none() => {
            let mut buckets: HashMap<JoinHashKey, Vec<usize>> = HashMap::new();
            for (index, key) in right_keys.into_iter().enumerate() {
                if let Some(key) = key {
                    buckets.entry(key).or_default().push(index);
                }
            }
            Some(buckets)
        }
        _ => None,
    };
    let mut output = LoopRows::new();
    // A condition with no subquery reads only the pair's own values, so it
    // compiles once. Cloning, resolving and compiling it for every
    // candidate pair cost far more than testing the pair.
    let fixed = if super::expression_has_subquery(condition) {
        None
    } else {
        Some(CompiledExpr::compile(condition, &columns, collation)?)
    };
    // One memo for the whole join: the ON condition's subqueries are keyed
    // by the (left, right) values they substitute, and a nested loop
    // revisits the same right row once per left row.
    let mut memo = super::memo::DependentMemo::for_expressions(std::iter::once(condition));
    while let Some(left_batch) = left_input.next_batch(memory)? {
        let batch_bytes = left_batch.estimated_bytes();
        memory.reserve(batch_bytes)?;
        for row in left_batch.selection().selected_rows() {
            let left = batch_row(&left_batch, row)?;
            let left_bytes = estimated_row_payload_bytes(&left);
            memory.reserve(left_bytes)?;
            memory.check_interruption()?;
            let mut matches = 0_usize;
            // With buckets, only the right rows sharing this row's key are
            // candidates, and a NULL key reaches none; without, every row.
            let bucket = match (&loop_keys, &buckets) {
                (Some(loop_keys), Some(buckets)) => Some(
                    LoopKeys::key(&loop_keys.left, &left_batch, row)?
                        .and_then(|key| buckets.get(&key))
                        .map_or(&[][..], Vec::as_slice),
                ),
                _ => None,
            };
            let mut replay = match bucket {
                Some(_) => None,
                None => Some(right_rows.reader()?),
            };
            let mut position = 0_usize;
            loop {
                let right = match (bucket, &mut replay) {
                    (Some(bucket), _) => {
                        let Some(index) = bucket.get(position) else {
                            break;
                        };
                        position += 1;
                        right_rows.rows[*index].clone()
                    }
                    (None, Some(replay)) => {
                        let Some(right) = replay.next_row()? else {
                            break;
                        };
                        right
                    }
                    (None, None) => unreachable!("an unbucketed row replays the right side"),
                };
                memory.ensure_transient(
                    estimated_row_payload_bytes(&left)
                        .saturating_add(estimated_row_payload_bytes(&right)),
                )?;
                let right_bytes = estimated_row_payload_bytes(&right);
                let candidate_bytes = left_bytes.saturating_add(right_bytes);
                memory.reserve(right_bytes.saturating_add(candidate_bytes))?;
                let mut candidate = left.clone();
                candidate.extend(right.iter().cloned());
                let candidate_batch_bytes = estimated_record_batch_bytes(
                    std::slice::from_ref(&candidate),
                    column_types.len(),
                );
                memory.reserve(candidate_batch_bytes)?;
                let vectors = rows_to_columns(std::slice::from_ref(&candidate), &column_types)?;
                let batch = RecordBatch::new(1, vectors)?;
                let accepted = if let Some(fixed) = &fixed {
                    predicate_truth(&fixed.evaluate(&batch, 0)?)?
                } else {
                    let mut predicate = condition.clone();
                    let context = super::DependentRow {
                        batch: &batch,
                        row: 0,
                        columns: &columns,
                        provider,
                        memory,
                        collation,
                    };
                    if memory.remaining() < memory.limit() / 2 {
                        super::record_dependent_memo(memo.finish(memory));
                        memo =
                            super::memo::DependentMemo::for_expressions(std::iter::once(condition));
                    }
                    memo.begin_row();
                    resolve_dependent_expr_subqueries(&mut predicate, &context, &mut memo)?;
                    let predicate = CompiledExpr::compile(&predicate, &columns, collation)?;
                    predicate_truth(&predicate.evaluate(&batch, 0)?)?
                };
                drop(batch);
                drop(right);
                memory.release(
                    right_bytes
                        .saturating_add(candidate_bytes)
                        .saturating_add(candidate_batch_bytes),
                );
                if !accepted {
                    continue;
                }
                matches = matches.saturating_add(1);
                match kind {
                    BoundJoinKind::Inner | BoundJoinKind::Left => {
                        output.push(candidate, memory)?;
                    }
                    BoundJoinKind::Scalar => {
                        if matches > 1 {
                            return Err(ExecError::ScalarSubqueryRows { rows: matches });
                        }
                        output.push(candidate, memory)?;
                    }
                    // One match decides both: the row is kept by a semi join and
                    // dropped by an anti join, whatever else would match.
                    BoundJoinKind::Semi | BoundJoinKind::Anti => break,
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
                    output.push(row, memory)?;
                }
                BoundJoinKind::Semi if matches > 0 => {
                    output.push(left.clone(), memory)?;
                }
                BoundJoinKind::Anti if matches == 0 => {
                    output.push(left.clone(), memory)?;
                }
                BoundJoinKind::Inner
                | BoundJoinKind::Left
                | BoundJoinKind::Scalar
                | BoundJoinKind::Semi
                | BoundJoinKind::Anti => {}
                BoundJoinKind::Cross => unreachable!("cross joins return above"),
            }
            drop(left);
            memory.release(left_bytes);
        }
        memory.release(batch_bytes);
    }
    super::record_dependent_memo(memo.finish(memory));
    let retained = right_rows.reserved;
    drop(buckets);
    drop(right_rows);
    memory.release(retained.saturating_add(keys_reserved));
    output.finish(memory, collation)
}

/// The compiled equality keys a dependent join buckets its right input by,
/// each side with the key mode its pair hashes under - the same
/// normalization a hash join applies, so two values the equality calls equal
/// land in one bucket.
struct LoopKeys {
    left: Vec<(CompiledExpr, JoinKeyMode)>,
    right: Vec<(CompiledExpr, JoinKeyMode)>,
}

impl LoopKeys {
    /// `None` when there are no keys, or a pair has no hashable mode - the
    /// join then tests every pair, which is always correct.
    fn compile(
        keys: &[(BoundExpr, BoundExpr)],
        left_columns: &[BoundColumn],
        right_columns: &[BoundColumn],
        collation: Collation,
    ) -> Result<Option<Self>, ExecError> {
        if keys.is_empty() {
            return Ok(None);
        }
        let mut left = Vec::with_capacity(keys.len());
        let mut right = Vec::with_capacity(keys.len());
        for (left_key, right_key) in keys {
            let Some(mode) = super::hash_join_key_mode(
                left_key.data_type,
                right_key.data_type,
                super::key_collation_of(left_key, collation),
            ) else {
                return Ok(None);
            };
            left.push((
                CompiledExpr::compile(left_key, left_columns, collation)?,
                mode,
            ));
            right.push((
                CompiledExpr::compile(right_key, right_columns, collation)?,
                mode,
            ));
        }
        Ok(Some(Self { left, right }))
    }

    /// One side's key for a row, or `None` when any part is NULL: an
    /// equality with NULL is never true, so such a row meets no candidate.
    fn key(
        side: &[(CompiledExpr, JoinKeyMode)],
        batch: &RecordBatch,
        row: usize,
    ) -> Result<Option<JoinHashKey>, ExecError> {
        let mut parts = Vec::with_capacity(side.len());
        for (expr, mode) in side {
            let Some(part) = normalized_join_key(expr.evaluate(batch, row)?, *mode)? else {
                return Ok(None);
            };
            parts.push(part);
        }
        Ok(Some(JoinHashKey::Composite(parts)))
    }
}

#[cfg(test)]
mod tests {
    use crate::collation::Collation;

    #[test]
    fn skew_scalar_checks_every_match_before_returning_a_row() {
        use super::{GraceRun, JoinHashKey, SkewReplay};
        use crate::execution::{ExecError, MemoryTracker};
        use pintail_sql::BoundJoinKind;
        use pintail_types::Value;
        let memory = MemoryTracker::new(2 * 1024 * 1024);
        let key = JoinHashKey::NonNegativeInteger(1);
        let mut build = GraceRun::create();
        build
            .append(&key, &[Value::UInt64(10)], &memory)
            .expect("build");
        build
            .append(&key, &[Value::UInt64(20)], &memory)
            .expect("build");
        build.seal(&memory).expect("seal");
        let mut probes = GraceRun::create();
        probes
            .append(&key, &[Value::UInt64(1)], &memory)
            .expect("probe");
        let mut replay = SkewReplay::new(build, probes.reader(&memory).expect("reader"));
        assert!(matches!(
            replay.next_row(BoundJoinKind::Scalar, 1, None, &[], &memory),
            Err(ExecError::ScalarSubqueryRows { rows: 2 })
        ));
    }

    fn drain_partitions(runs: &mut [super::GraceRun], memory: &super::MemoryTracker) -> Vec<u64> {
        let mut ids = Vec::new();
        for run in runs {
            let mut reader = run.reader(memory).expect("partition reader");
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
        let mut grace = GraceJoin::create();
        let ids = (0..500_u64).collect::<Vec<_>>();
        for id in &ids {
            let key = JoinHashKey::NonNegativeInteger(*id);
            let row = vec![pintail_types::Value::UInt64(*id)];
            grace.build_files[0]
                .append(&key, &row, &memory)
                .expect("build");
            grace.probe_files[0]
                .append(&key, &row, &memory)
                .expect("probe");
        }

        split_grace_partition(&mut grace, 0, &memory).expect("split");
        assert_eq!(grace.build_files.len(), GRACE_PARTITIONS * 2);
        assert_eq!(grace.depths[GRACE_PARTITIONS], 1);

        let mut build = drain_partitions(&mut grace.build_files[GRACE_PARTITIONS..], &memory);
        let mut probe = drain_partitions(&mut grace.probe_files[GRACE_PARTITIONS..], &memory);
        build.sort_unstable();
        probe.sort_unstable();
        assert_eq!(build, ids, "no build row may be lost in a split");
        assert_eq!(probe, ids, "probe rows follow their keys");

        // The emptied original must never be served again.
        assert!(
            drain_partitions(&mut grace.build_files[0..1], &memory).is_empty(),
            "the split partition is left empty"
        );
    }

    #[test]
    fn the_probe_reads_ahead_for_left_and_anti_joins_and_for_small_inner_probes() {
        use super::{PROBE_PREFETCH_BUILD_RATIO, PROBE_PREFETCH_ROWS, probe_prefetch_applies};
        use pintail_sql::BoundJoinKind;
        assert!(probe_prefetch_applies(BoundJoinKind::Left, None, None));
        assert!(probe_prefetch_applies(
            BoundJoinKind::Anti,
            Some(u64::MAX),
            None
        ));
        assert!(probe_prefetch_applies(
            BoundJoinKind::Inner,
            Some(PROBE_PREFETCH_ROWS),
            None
        ));
        assert!(probe_prefetch_applies(BoundJoinKind::Semi, Some(1), None));
        // An inner probe with no statistics, or too many rows, keeps its
        // scan unstarted so the build's key span can still restrict it.
        assert!(!probe_prefetch_applies(BoundJoinKind::Inner, None, None));
        assert!(!probe_prefetch_applies(
            BoundJoinKind::Inner,
            Some(PROBE_PREFETCH_ROWS + 1),
            None
        ));
        assert!(!probe_prefetch_applies(
            BoundJoinKind::Scalar,
            Some(1),
            None
        ));
        // With both sides estimated, a probe the build's size filters
        // nothing and is not read ahead, whatever the join kind; a probe
        // the ratio smaller is.
        assert!(!probe_prefetch_applies(
            BoundJoinKind::Inner,
            Some(4_096),
            Some(4_096)
        ));
        assert!(!probe_prefetch_applies(
            BoundJoinKind::Left,
            Some(4_096),
            Some(4_096)
        ));
        assert!(!probe_prefetch_applies(
            BoundJoinKind::Inner,
            Some(1_025),
            Some(1_024 * PROBE_PREFETCH_BUILD_RATIO)
        ));
        assert!(probe_prefetch_applies(
            BoundJoinKind::Inner,
            Some(1_024),
            Some(1_024 * PROBE_PREFETCH_BUILD_RATIO)
        ));
        assert!(probe_prefetch_applies(
            BoundJoinKind::Semi,
            Some(128),
            Some(20_000_000)
        ));
    }

    #[test]
    fn an_integer_key_span_covers_both_signs_and_refuses_other_keys() {
        use super::{JoinHashKey, integer_key_span};
        use pintail_types::Value;
        let keys = [
            JoinHashKey::NonNegativeInteger(7),
            JoinHashKey::NegativeInteger(-3),
            JoinHashKey::NonNegativeInteger(42),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            integer_key_span(&keys),
            Some((Value::Int64(-3), Value::UInt64(42)))
        );
        let naturals = [
            JoinHashKey::NonNegativeInteger(9),
            JoinHashKey::NonNegativeInteger(2),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            integer_key_span(&naturals),
            Some((Value::UInt64(2), Value::UInt64(9)))
        );
        let negatives = [
            JoinHashKey::NegativeInteger(-9),
            JoinHashKey::NegativeInteger(-2),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            integer_key_span(&negatives),
            Some((Value::Int64(-9), Value::Int64(-2)))
        );
        let mixed = [
            JoinHashKey::NonNegativeInteger(1),
            JoinHashKey::MysqlNumber(pintail_types::Float64::new(1.5)),
        ]
        .into_iter()
        .collect();
        assert_eq!(integer_key_span(&mixed), None);
        assert_eq!(integer_key_span(&std::collections::HashSet::new()), None);
    }

    #[test]
    fn a_partition_read_once_by_the_serve_loop_can_still_be_split() {
        use super::{
            GRACE_PARTITIONS, GraceJoin, JoinHashKey, MemoryTracker, split_grace_partition,
        };
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create();
        let ids = (0..300_u64).collect::<Vec<_>>();
        for id in &ids {
            let key = JoinHashKey::NonNegativeInteger(*id);
            let row = vec![pintail_types::Value::UInt64(*id)];
            grace.build_files[0]
                .append(&key, &row, &memory)
                .expect("build");
            grace.probe_files[0]
                .append(&key, &row, &memory)
                .expect("probe");
        }
        // Exactly the serve loop's sequence: read the build side, decide it
        // does not fit, drop the reader, ask for a split of the same slot.
        let mut first = grace.build_files[0].reader(&memory).expect("first read");
        let mut seen = 0;
        while first.next_entry().expect("entry").is_some() {
            seen += 1;
        }
        assert_eq!(seen, ids.len());
        drop(first);
        split_grace_partition(&mut grace, 0, &memory)
            .expect("a partition the serve loop already read must still split");
        let mut build = drain_partitions(&mut grace.build_files[GRACE_PARTITIONS..], &memory);
        let mut probe = drain_partitions(&mut grace.probe_files[GRACE_PARTITIONS..], &memory);
        build.sort_unstable();
        probe.sort_unstable();
        assert_eq!(build, ids, "the second read must see every build row");
        assert_eq!(probe, ids, "probe rows follow their keys through the split");
    }

    #[test]
    fn a_sealed_run_reads_repeatably_holds_no_writer_and_refuses_appends() {
        use super::{ExecError, GraceRun, JoinHashKey, MemoryTracker};
        let memory = MemoryTracker::new(usize::MAX);
        let mut run = GraceRun::create();
        for id in 0..10_u64 {
            run.append(
                &JoinHashKey::NonNegativeInteger(id),
                &[pintail_types::Value::UInt64(id)],
                &memory,
            )
            .expect("append");
        }
        assert!(
            !run.buffer.is_empty() || run.run.is_some(),
            "open for appends until first read"
        );
        for _ in 0..3 {
            let mut reader = run.reader(&memory).expect("reader");
            let mut count = 0;
            while reader.next_entry().expect("entry").is_some() {
                count += 1;
            }
            assert_eq!(count, 10, "each read starts from the first record");
            assert!(
                run.run.is_none() && run.buffer.is_empty(),
                "sealed: no writer, no buffer, no descriptor of its own"
            );
        }
        let refused = run
            .append(
                &JoinHashKey::NonNegativeInteger(99),
                &[pintail_types::Value::UInt64(99)],
                &memory,
            )
            .expect_err("a sealed run takes no more rows");
        assert!(matches!(
            refused,
            ExecError::InvalidPhysicalPlan("grace run already sealed")
        ));
    }

    #[test]
    fn hash_repartitioning_stops_at_its_depth_bound() {
        use super::{ExecError, MemoryTracker};
        use super::{GraceJoin, MAX_GRACE_DEPTH, split_grace_partition};
        let memory = MemoryTracker::new(usize::MAX);
        let mut grace = GraceJoin::create();
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

#[cfg(test)]
mod dense_join_table_tests {
    use super::{JoinHashKey, PartitionedBuild};
    use pintail_types::Value;

    /// Inserts one row per `(key, payload)` pair without going through
    /// `build_hash_join_state`'s batch/memory machinery - a bare
    /// `PartitionedBuild` is enough to test `finalize_dense` and its
    /// accessors directly. A key mentioned more than once produces a bucket
    /// with more than one row, covering duplicates.
    fn build(rows: &[(JoinHashKey, u64)]) -> PartitionedBuild {
        let mut build = PartitionedBuild::with_partitions(4);
        for (key, payload) in rows {
            build
                .entry_or_default(key.clone())
                .push(vec![Value::UInt64(*payload)]);
        }
        build
    }

    fn payloads(bucket: &[Vec<Value>]) -> Vec<u64> {
        let mut values: Vec<u64> = bucket
            .iter()
            .map(|row| match row.first() {
                Some(Value::UInt64(value)) => *value,
                other => panic!("unexpected row {other:?}"),
            })
            .collect();
        values.sort_unstable();
        values
    }

    #[test]
    fn a_narrow_integer_span_with_duplicates_finalizes_dense_and_matches_the_hashed_result() {
        let rows = [
            (JoinHashKey::NonNegativeInteger(10), 1),
            (JoinHashKey::NonNegativeInteger(10), 2), // duplicate key, second row
            (JoinHashKey::NonNegativeInteger(11), 3),
            (JoinHashKey::NegativeInteger(-5), 4),
        ];
        let mut hashed = build(&rows);
        // Captured before finalizing: the answer the general (hashed) path
        // gives, which the dense path below must reproduce exactly.
        let expected: Vec<(JoinHashKey, Vec<u64>)> = rows
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .map(|key| {
                let expected = payloads(hashed.get(&key).expect("row was inserted"));
                (key, expected)
            })
            .collect();
        assert!(!hashed.is_dense());

        hashed.finalize_dense();
        assert!(hashed.is_dense(), "a span of 16 fits MAX_DENSE_SPAN");
        assert_eq!(hashed.len(), 3, "three distinct keys, one with two rows");
        for (key, expected) in &expected {
            let dense = payloads(hashed.get(key).expect("dense get finds the same key"));
            assert_eq!(&dense, expected, "dense and hashed paths must agree");
            let (flat_index, via_dense_get) = hashed
                .dense_get(key)
                .expect("dense_get mirrors get once dense");
            assert_eq!(&payloads(via_dense_get), expected);
            assert!(flat_index < hashed.dense_buckets().len());
        }
        assert!(
            hashed.get(&JoinHashKey::NonNegativeInteger(999)).is_none(),
            "a key never inserted must miss on the dense path too"
        );
    }

    #[test]
    fn a_span_past_max_dense_span_never_finalizes() {
        let mut build = build(&[
            (JoinHashKey::NonNegativeInteger(0), 1),
            // MAX_DENSE_SPAN is 1 << 22; this key alone puts the span past it.
            (JoinHashKey::NonNegativeInteger(1 << 23), 2),
        ]);
        build.finalize_dense();
        assert!(
            !build.is_dense(),
            "a span this wide must stay on the hashed path rather than allocate a huge table"
        );
        assert_eq!(
            payloads(build.get(&JoinHashKey::NonNegativeInteger(0)).expect("row")),
            vec![1]
        );
        assert_eq!(
            payloads(
                build
                    .get(&JoinHashKey::NonNegativeInteger(1 << 23))
                    .expect("row")
            ),
            vec![2]
        );
    }

    #[test]
    fn a_non_integer_key_never_finalizes() {
        let mut build = build(&[]);
        build
            .entry_or_default(JoinHashKey::Scalar(Value::Utf8("a".to_owned())))
            .push(vec![Value::UInt64(1)]);
        build.finalize_dense();
        assert!(!build.is_dense());
        assert_eq!(
            payloads(
                build
                    .get(&JoinHashKey::Scalar(Value::Utf8("a".to_owned())))
                    .expect("row")
            ),
            vec![1]
        );
    }
}

#[cfg(test)]
mod integer_key_set_tests {
    use super::{IntegerKeySet, JoinHashKey};
    use std::collections::HashSet;

    fn keys(values: &[i64]) -> HashSet<JoinHashKey> {
        values
            .iter()
            .map(|value| {
                if *value < 0 {
                    JoinHashKey::NegativeInteger(*value)
                } else {
                    JoinHashKey::NonNegativeInteger(u64::try_from(*value).expect("non-negative"))
                }
            })
            .collect()
    }

    #[test]
    fn a_narrow_span_is_a_bitmap_that_answers_both_signs() {
        let set = IntegerKeySet::from_keys(&keys(&[-3, 0, 5, 127])).expect("integers");
        assert!(matches!(set, IntegerKeySet::Bitmap { low: -3, .. }));
        for value in [-3, 0, 5, 127] {
            assert!(set.contains(value), "{value}");
        }
        for value in [
            -4,
            -1,
            1,
            6,
            126,
            128,
            i128::from(i64::MAX),
            i128::from(i64::MIN),
        ] {
            assert!(!set.contains(value), "{value}");
        }
    }

    #[test]
    fn a_wide_span_falls_back_to_the_sparse_set() {
        let set = IntegerKeySet::from_keys(&keys(&[0, i64::MAX])).expect("integers");
        assert!(matches!(set, IntegerKeySet::Sparse(_)));
        assert!(set.contains(0));
        assert!(set.contains(i128::from(i64::MAX)));
        assert!(!set.contains(1));
    }

    #[test]
    fn non_integer_keys_and_empty_sets_are_handled() {
        let mut mixed = keys(&[1]);
        mixed.insert(JoinHashKey::Scalar(pintail_types::Value::Utf8(
            "x".to_owned(),
        )));
        assert!(IntegerKeySet::from_keys(&mixed).is_none());
        let empty = IntegerKeySet::from_keys(&HashSet::new()).expect("empty is a set");
        assert!(!empty.contains(0));
    }
}

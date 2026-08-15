# Architecture decisions

### Bun is the dashboard package manager

The goal specification originally named pnpm in the repository layout. The
owner subsequently directed Pintail to use Bun instead. Bun now owns dependency
installation, the lockfile, local scripts, CI, and container dashboard builds;
Yarn and pnpm artifacts are not accepted. This changes build tooling only and
does not change the Nuxt 4 plus shadcn-vue dashboard decision.

### Dashboard output is generated, not versioned

Nuxt's static output contains per-build identifiers and timestamps, so
committing `.output/public` creates unrelated diffs after every verification
run. `pintail-api` instead generates the dashboard when its source inputs
change and embeds that exact output. CI and the container build generate once,
then set `PINTAIL_DASHBOARD_PREBUILT=1` for the immediately following Cargo
build. The generated directory stays ignored.

### SQL parsing may use sqlparser-rs

Pintail will use `sqlparser-rs` with its MySQL dialect when the query and
replication milestones introduce SQL parsing. This is the specification's
pre-approved engine-adjacent exception: Pintail will still own its bound AST,
type semantics, planner, optimizer, and vectorized executor.

### SQLite is control-plane storage only

The embedded SQLite database stores metadata, settings, checkpoints, jobs, and
other operational state. It is never part of the analytical data path; table
data, indexes, WAL records, and execution remain Pintail-owned formats and
implementations.

### Storage data uses Pintail-owned binary formats

The M1 data path uses no serialization or database engine crate. Pintail owns
the WAL, manifest, PTSEG column layout, adaptive encodings, checksums,
merge-on-read, snapshots, and compaction rules. `xxhash-rust`, `lz4_flex`, and
`zstd` provide only the utility checksum/compression primitives permitted by
the specification. Format version 1 is documented byte-for-byte in
`docs/format.md`.

### Compaction scheduling stays outside the table core

The storage core performs at most one size-tier compaction and reclamation
step when an ingest crosses its memtable budget, reports compaction debt, and
yields between input segments. It does not create an unsupervised thread per
open table. The replication supervisor will own background scheduling when
the ingestion task tree is introduced, calling the bounded per-table
maintenance API from its managed worker. This is a deliberate staging
deviation from §5.1's final “background, per table” topology: M1 has no
executor, query scheduler, or supervisor lifetime to own and stop those
threads safely.

### Codec chunks are independently checksummed blocks

PTSEG treats each target-sized, independently checksummed block as the codec
chunk. Dictionary selection and dictionary bytes therefore belong to one
block rather than being shared across every block of a physical column. This
narrows corruption and decode scope, keeps block pruning independently
seekable, and bounds dictionary memory. It is the storage format's explicit
interpretation of §5.2's “dict per chunk”; `docs/format.md` uses “block”
consistently for that unit.

### Metadata queries use a catalog-backed compatibility path

`SHOW`, `DESCRIBE`, and the initial `information_schema` tables are immutable
catalog views rather than analytical storage tables. They share the MySQL
parser and typed result model, but do not create PTSEG snapshots merely to
answer control metadata. The compatibility path is deliberately restricted
to deterministic metadata query shapes and rejects unsupported expressions;
ordinary table queries still pass through Pintail's binder, logical planner,
optimizer, physical planner, and vectorized executor.

### Uncorrelated subqueries materialize once

M2 binds table-reading scalar and `IN` subqueries as typed nested queries and
executes each once before compiling the containing physical expression.
Materialized values count against the parent query's hard memory cap. This
keeps uncorrelated semantics deterministic without introducing a hidden
nested-loop executor. Correlated subqueries remain explicit errors until a
decorrelation or parameterized-plan design can preserve both correctness and
bounded execution.

### Projected scans parallelize independent segment work

Projected scans use a Pintail-owned Rayon thread pool, a data-path CPU utility
explicitly permitted by the goal specification. Header/zone-map reads and late
column fetches run in parallel per segment; max-version and tombstone winner
resolution remains deterministic and single-owner after the parallel results
return. This preserves merge-on-read correctness, isolates scan scheduling
from Rayon's process-global pool, and avoids exposing storage internals to the
async runtime.

### Key pruning requires explicit catalog provenance

Catalog table entries carry stable source-column IDs for each physical key
component. The executor never infers that the first visible column produced
the first storage-key component: append-row IDs, reordered schemas, and source
index choices make that inference unsafe. M2 range pruning is therefore
restricted to an explicitly declared single `Int64` or `UInt64` key and an
exact or losslessly convertible integer literal. Every other predicate falls
back to a correct full physical-key range.

### Aggregate pushdown requires a proof

The v1 optimizer pushes an aggregate through a cross join only when the
discarded input is unreferenced, predicate-free, and has exact catalog
cardinality one. Pintail does not yet catalog foreign-key or uniqueness
relationships rich enough to prove broader join/aggregate rewrites safe.
Rule coverage stays narrow rather than relying on optimistic cardinality
assumptions that could change results.

### M3 logical types reuse PTSEG version-one carriers

M3 records exact source widths, decimal precision/scale, and temporal
fractional precision in Pintail schemas and schema fingerprints. PTSEG and WAL
version one continue to encode the six physical scalar carriers established in
M1: narrow signed and unsigned integers use their 64-bit carrier, `Float32`
uses the 64-bit float carrier, and decimal, date, date-time, time, and JSON use
canonical UTF-8. Probe-time range validation prevents a narrow source value
from escaping its logical width; decimal text is lossless.

This deliberately avoids an after-M2 format rewrite or an unversioned tag
reinterpretation. It deviates from the goal's proposed i128/date integer
physical representations, so the limitation is explicit: exact decimal query
arithmetic remains future work even though snapshot and storage fidelity are
lossless. A future native representation requires a new storage format
version and migration design.

### Resumes retain the first snapshot handoff position

The control plane inserts the snapshot-to-stream checkpoint once. A restarted
snapshot opens new consistent source transactions and skips durable chunks,
but it never advances that checkpoint. M4 can therefore replay every source
change since the first attempt and make mixed-attempt chunks converge.
Replacing the position on restart would silently lose changes made to a chunk
that had already completed.

Direct snapshot chunks publish immutable segments before atomically marking
their SQLite journal entry complete. A crash in that narrow interval may
publish the same version-zero keys twice on retry; mandatory merge-on-read
makes the replay invisible, and compaction later removes the redundant
version. Snapshot traffic consequently bypasses both the mutable table and
WAL without weakening recovery.

### mysql_async uses its minimal Rust feature set

The source client is the specification-approved `mysql_async` crate. Pintail
enables its `minimal-rust`, Rustls, and ring features instead of the default
feature set so the workspace keeps its declared Rust 1.85 MSRV and does not
pull a native TLS dependency. Snapshot tests exercise the resulting client
against MySQL 5.7, MySQL 8.4, and MariaDB 11.

### CDC checkpoints follow every synchronized table WAL

M4 buffers one source transaction and groups its mutations by target. Each
table accepts its deterministic batch into the WAL, then every touched WAL is
synchronized. Only after all those calls succeed does one SQLite transaction
advance the source coordinate and table state. A failure before SQLite commit
replays the transaction; a failure after it cannot lose a WAL-backed row.

Independent table WALs mean a storage failure can leave a prefix of table
batches accepted without advancing the global coordinate. Replay is safe
because keyed rows carry the same source version. Append-mode INSERTs use that
version as their storage key through the CDC-specific ingest path, rather than
allocating a new local row ID.

### File/position versions allocate non-overlapping fields

The prose design describes the binlog file index, end position, and
intra-event counter conceptually. M4 encodes them as 16 file-index bits, 32
event-position bits, and 16 ordinal bits. GTID versions use 48 sequence bits
and the same ordinal field. This makes ordering and replay deterministic
without overlapping bit ranges; exceeding a field returns an explicit decode
error.

### MariaDB resumes from its captured classic coordinate

The selected protocol client parses MariaDB row events but does not encode the
MariaDB GTID dump request. Snapshot capture therefore retains both
`gtid_binlog_pos` and `SHOW MASTER STATUS`; M4 uses the latter for MariaDB and
persists file/position after its first commit. Live tests cover MariaDB 11,
including its extra query/metadata commit boundaries.

MariaDB rotate events decoded by this dependency may expose trailing
non-filename bytes. Pintail accepts only the ASCII binlog filename prefix
before persisting that event, while file availability is independently
validated with `SHOW BINARY LOGS`.

### Purge recovery replaces a table generation

A missing checkpoint cannot be repaired incrementally. M4 marks the database
and all included tables `needs_resync`, publishes an empty manifest generation
for every target, clears the old snapshot journal and coordinate, and runs one
new consistent snapshot. Existing readers retain the previous manifest and
segments until their snapshots release; new readers see only the replacement
generation. One automatic attempt per runner prevents an unbounded resnapshot
loop on a source whose retention policy is still unsafe.

### Cheap polling tokens are advisory

Count plus maximum cursor/key is a low-cost activity signal, not a correctness
proof. A delete and insert can leave both values unchanged, and a new row can
reuse the current maximum timestamp. Pintail therefore records and reports the
cheap token but does not use an unchanged value to skip the strategy-specific
check: cursor tables reread their inclusive boundary, keyed cursor-less tables
compare aggregate chunks, and append tables compare their complete generation.
No-op suppression keeps these checks from adding row-storage writes when the
source is unchanged.

This deliberately strengthens §9's “only then sync” wording. Treating the token
as a hard gate would reproduce the count-neutral blind spot that Pintail exists
to close.

### Live DDL evolution preserves source-column identity

Pure ADD COLUMN and DROP COLUMN events re-probe the source, retain stable IDs
for unchanged columns, allocate new IDs monotonically, and publish a new table
schema generation. Old segments resolve an added nullable column as NULL and
retain dropped bytes until normal rewriting. Pinned readers continue using the
schema generation they opened.

Any ALTER operation other than a pure add/drop, a physical-key change, or a
same-name physical-type change quarantines only that table and records the DDL
as durable schema history. This is more conservative than trying to infer
compatible index/default-only changes from SQL text, but preserves the
per-database stream for unrelated tables and cannot silently reinterpret stored
bytes.

### Reconciliation is paginated but currently materializes keysets

M5 reads source keys with composite-safe keyset pagination, then compares the
complete source and visible-replica keysets in memory before emitting
tombstones. This provides exact delete repair and avoids OFFSET instability,
but does not yet implement the bloom-assisted streaming anti-join suggested in
§9. The later operations milestone may bound memory by partitioning or a
disk-backed/bloom-assisted comparison without changing reconciliation
semantics.

### Poll cadence belongs to the replication supervisor

`pintail-poll` executes one deterministic cycle and accepts explicit requests
for full reconciliation or CDC-side cascade repair. It does not spawn timers.
The M8 per-database supervised task tree owns the 1-second probe, 5-second sync,
10-minute delete-reconcile, and 10-minute CDC-cascade defaults so pause,
shutdown, backoff, and blast-radius behavior have one lifetime owner. This
entry said "hourly" for the cascade repair while the code shipped
`reconcile_interval_seconds` at 600 seconds; the code is what runs, so the
sentence is corrected to it rather than the other way round.

### Operator resync preserves the database-wide handoff

The table-action REST surface accepts a table name so operators can act from
the table that exposed drift or schema quarantine. Its `resync` operation
nevertheless starts a force resnapshot of every included table. Pintail owns
one source checkpoint per database: independently capturing one table while
retaining the older shared checkpoint could replay pre-snapshot binlog events
over that table and replace fresh values with stale ones.

The dashboard calls this behavior out in the action tooltip and acceptance
toast. `reconcile` remains genuinely table-local because it assigns versions
above the table's visible rows, synchronizes storage before metadata, and does
not advance the CDC source checkpoint.

### Wire authentication stores a protocol verifier

The MySQL native password handshake cannot be validated from the API key's
SHA-256 lookup digest: it requires `SHA1(SHA1(secret))` to recover and verify
the challenge response. New API keys therefore persist that 20-byte
double-SHA-1 verifier in addition to the existing SHA-256 digest. The plaintext
secret is still shown once and never stored, and HTTP bearer authentication
continues to use SHA-256.

Keys created before metadata schema version 6 have no recoverable native
verifier and must be rotated before use on the wire endpoint. This avoids a
cleartext-password plugin, which would require client-specific opt-ins or TLS
just to complete the standard compatibility gate.

### HTTP and wire share one replica query facade

Opening table stores, pinning snapshots, rebuilding the catalog, enforcing
query ceilings, and translating execution counters are client-independent
operations. M7 extracts them into `ReplicaEngine`; HTTP and MySQL protocol
handlers only authorize a database and encode the same typed `QueryOutput`.
This prevents metadata statements, read-only enforcement, visibility policy,
and pruning statistics from drifting between client surfaces.

The facade currently lives in `pintail-wire` because that milestone introduced
the second client and owns the protocol-facing result model. It delegates all
SQL semantics to `pintail-sql` and `pintail-exec`; it is not a dialect
translation layer. If another native client arrives, the facade can move to a
small client-neutral crate without changing either engine.

### Release memory bounds favor merge-on-read over forced compaction

The release soak exposed that whole-segment verification, overlapping scan
materialization, and large size-tier merges could each create an RSS spike
despite the executor's query budget. M9 changes these paths independently:
segment readers verify and seek through checksummed structures, overlapping
queries choose winners from system-column headers before late materializing
projected values, and compaction merges block-wise into bounded output
segments.

Default maintenance admits at most 50,000 input rows to one compaction pass.
An oversized candidate is deliberately left as immutable segments and remains
correct through merge-on-read. This is a throughput and segment-count tradeoff,
not a semantic relaxation: forcing every eligible size tier to compact would
make the maintenance path exceed the memory behavior enforced by the release
gate. The limit can remain an engine option in v1 because exposing it without
a scheduler/debt model for deferred windows would imply an operational
contract Pintail does not yet provide.

## Decisions from the 2026-07-31 experiment lab (experiments/RESULTS.md)

The following were settled empirically on both reference machines (Apple M2 Pro
and the x86 docker host under pinned limits); see `experiments/RESULTS.md` for
numbers and `docs/engine-research/` for the source research. Per the lab's
rules, a technique is adopted only when it wins on both machines; sub-15%
margins prefer the simpler implementation.

### Executor moves to typed packed arrays

The `Vec<Value>` executor is replaced by typed columnar arrays with
Flat/Constant/Dictionary physical forms, selection vectors, validity masks
carrying an all-valid fast path, and ~1024-row vectors. `Value` remains only at
API, wire, and final-output boundaries. This completes the GOAL specification's
X100 design rather than amending it; e01 measured fused typed kernels at
memory bandwidth while every materialized intermediate cost 2-8x.

### Decimals and dates execute natively

DECIMAL becomes i128-scaled Decimal128 and dates become Date32/DateTime64 in
execution, populated by scan-time conversion from the current text carriers.
A PTSEG v2 with fixed-width encodings **was approved by the owner on
2026-07-31**: format-version bump, old segments stay readable, migration via
compaction rewrite. In the same ruling the owner **confirmed
`unsafe_code = "forbid"` stands**: kernels are autovectorizable scalar code
plus safe SIMD crates only; any per-kernel exception requires new profiling
evidence and a fresh ruling.

PTSEG v3 adds only adaptive block compression, not a new integer layout. e27
reproduced production payload shapes on Apple and Linux: FOR-packed and random
Float64 blocks expand under LZ4, while delta-packed keys and dictionary/text
blocks save 41-99.5%. Normal flushes therefore try LZ4 and keep it only at 5%
or greater savings; raw blocks carry compression tag `0`. Existing LZ4/zstd
segments remain readable and cold full-merge output remains zstd. This is not a
claim that the sub-15% decode differences beat the lab's performance threshold;
they are ties. The adoption instead enforces a storage invariant: normal-tier
compression may not expand a block, while retaining the 41-99.5% reductions on
compressible blocks. The 5% hysteresis keeps marginal size wins from adding a
decode step. The decision is identical on both reference targets.

### Merge-on-read uses granule-level sweep-line classification

Scans classify granule ranges against newer segments and the memtable using
the sparse index; non-overlapping granules take the direct fast path, only
overlapping granules pay a versioned merge, and fully-compacted segments skip
classification via `unique_keys`. Measured 29-35x under the realistic CDC
update pattern (recent-hot keys) and 8-11x under adversarial uniform updates
(e05, e11). Composite merge keys pack into u128 integers when they fit;
normalized memcmp byte keys are rejected for heap merges (e12: 17-61% slower
on both machines).

### Temporal reads use one stable replica policy

Source `DATE` and `DATETIME` values with zero components or an impossible
calendar date normalize to SQL `NULL` during both snapshot and CDC decoding.
The two ingestion paths share the same mapper, so an existing row and a later
change event cannot disagree. `YEAR 0` remains a valid year value, and invalid
`TIME` encodings fail decoding rather than becoming a plausible clock value.
This policy is independent of the querying connection's `sql_mode`: Pintail is
a read replica and does not reinterpret already-ingested source bytes when a
client changes modes. `SET sql_mode` is retained and echoed for client
compatibility but does not alter expression or temporal semantics.

`SYSTEM` means the process host timezone. Explicit session `time_zone` names
use the embedded IANA database, numeric offsets are fixed, fall-back folds take
the earlier offset, and nonexistent spring-forward local times produce `NULL`.
`CONVERT_TZ` always uses its explicit source and destination arguments, not the
session default. Operators that need host-independent behavior should set a
named zone or offset; a separate server-timezone setting would duplicate that
control without improving determinism.

Compound interval qualifiers remain deferred until captured workload demand
justifies maintaining a parser fork. sqlparser currently rejects those tokens
before binding, so the deferral is an explicit error rather than guessed date
arithmetic.

### Operator choices

Low-cardinality aggregation runs on dictionary codes with direct-array
accumulators and thread-local partials merged at finalize; radix two-phase
aggregation is reserved for beyond ~10k groups (e02). Joins use dense
direct-address tables when the build-key domain is dense (the common case for
MySQL auto-increment PKs), hashbrown otherwise, always with build-side min/max
join-filter pushdown into probe scans; semi-joins use dense bitmaps for dense
domains and blocked Bloom filters otherwise (e04). Top-K uses cutoff-guarded
per-thread heaps with the k-th value pushed into granule pruning (e03).
Scan morsels are 64K rows; scans saturate memory bandwidth well below core
count, so parallelism budget goes to aggregation and joins (e10).

### Techniques measured and rejected

Shared-atomic aggregation tables (ISA-inconsistent: 5x better than
thread-local hashmaps on x86, 3x worse on Apple Silicon; never beat
thread-local dense arrays). Hand-rolled FOR+bit-packed scanning (loses 2.2-6x
to plain scans; only the FastLanes transposed layout can change this and is
untested — e13). Length-classed string hash tables (tie with hashbrown).
The simplified Umbra unchained join table (lost to hashbrown on all-hit inner
probes on both machines; revisit only for miss-heavy workloads). Normalized
memcmp keys in heap merges (see above).

### String columns execute as 16-byte views

German-string views (4-byte prefix, 12-byte inline, heap offset beyond) win
every equality workload on both machines and halve Vec<String> memory (e07).
The ordering-comparison kernel stays eligible for flat chars+offsets
specialization, which won ordering on x86.

### Clustering determines pruning value

e09 showed zone maps deliver 10-18x on clustered layouts and exactly nothing on
scattered ones. e29 isolated the remaining condition-cache niche: a repeated
non-zone-map predicate was 4.5-7.4x faster when its first scan covered only
1-15% of blocks, but 5% slower when scattered matches covered every block. The
production-shaped workload contains no qualifying repeated predicate, so a
cache stays deferred until observed reuse and block coverage satisfy that gate.
Optional clustering/ordering keys remain the broader pruning lever.

### Window functions and recursive CTEs are in scope after all

The goal specification listed "window functions and CTE recursion in SQL" as
out of scope for v1, and the query-engine section repeated them as explicitly
deferred. Both are now implemented in the forms real analytical workloads ask
for: `ROW_NUMBER`/`RANK`/`DENSE_RANK` and the standard aggregates over
`PARTITION BY` / `ORDER BY` with MySQL's default frames, and `WITH RECURSIVE`
in its canonical `anchor UNION [ALL] member` shape.

The reason for the change is that the deferral assumed these were conveniences.
They are not: BI tools generate windowed queries for ranking and running
totals by default, and a MySQL-dialect endpoint that rejects them fails on the
first dashboard a user points at it. Recursive CTEs carry the same weight for
hierarchy queries over self-referencing tables, which are ordinary in the
mirrored schemas Pintail targets.

`GOAL.md` is updated rather than contradicted, and `docs/limitations.md`
remains the authority on which shapes within these features are supported —
explicit frames, named windows, the positional window functions, and several
recursive-member restrictions are still absent.

### Clustering and replica reads are deferred to v2

Pintail v1 is a single node: one process holds the WAL, memtables, segments and
control plane for every mirrored database, and a restart or host failure takes
analytics offline for the length of its recovery. That boundary is deliberate,
not an oversight, and it is recorded in `docs/limitations.md` alongside the
`pintail_startup_milliseconds` metric that measures the resulting window.

The reason to defer rather than build is that the failure it protects against is
cheap here in a way it is not for a system of record. Pintail is derived: MySQL
holds the truth, and every byte in a replica is re-derivable by re-snapshotting.
The disaster-recovery story for the analytics tier is "re-add the database",
which needs no consensus protocol, no quorum, and no split-brain reasoning.
Adding clustering in v1 would buy availability during a restart at the cost of
the single-writer-per-table concurrency contract that keeps a from-scratch
storage engine tractable.

v2 direction, when it is justified by an operator who cannot absorb the restart
window: replica reads served from pinned manifest generations, which the
existing snapshot isolation already makes safe, before any attempt at
multi-writer clustering.

### The JOIN is the analytical gap, not the scan or the aggregate

With the result memo disabled so both engines compute, ClickHouse answers the
benchmark's join-and-group-by roughly 8.7x faster. Profiling and four
experiments locate the cause, and rule out the two explanations that look
obvious first.

It is not one slow function. A sampling profile attributes the query as
`build_hash_aggregate_scan` 34.2%, `resolve_join_group_plan` 22.3%,
`build_fused_inner_join_aggregate` 21.5% (of which the ICU collation key is
10.3%), `build_hash_join_state` 6.7%. Nothing dominates, which is itself the
finding: a gap of this size spread that evenly is a property of the shape, not
of a hotspot.

It is not parallelism either, though the profile invites that reading - rayon
workers sample overwhelmingly as idle. Measured at 2M rows on the same dataset:
1 thread 249ms, 2 threads 191ms, 4 threads 189ms, 8 threads 175ms, 10 threads
174ms. Ten cores buy 1.43x, which by Amdahl puts ~67% of the query on the
serial path and caps perfect parallelism at 1.50x. The workers are idle because
two thirds of the work cannot be handed to them. Partitioning harder buys at
most 4%.

What remains is the serial path's cost per row, and that is a representation
question. The scan is columnar, but the join materializes its build side as
`Vec<Vec<Value>>` - `crates/pintail-exec/src/execution/join.rs` contains ten
such types and zero uses of `ColumnVector` or `TypedValues` - and the group
plan keys a `HashMap` on `Vec<Value>`. `Value` is a 32-byte tagged enum, so a
two-column build row costs a 24-byte `Vec` header plus 64 bytes of cells, and
one heap allocation, where typed columns would carry eight bytes for the
integer key and roughly one for a dictionary-coded region. That is about ten
times the memory traffic, plus an allocation per row and a tag branch per cell
access - against a gap that needs 8.7x.

Two micro-optimisations were tried and measured at nothing: removing a
redundant per-row clone (175ms against 178ms) and memoising the collation key
(170-179ms against 175-178ms, reverted). Neither changes the representation,
which is consistent with the representation being the cost.

A later experiment narrowed this considerably, and the earlier framing above -
that the executor is broadly row-shaped - was too broad. Run on the same 2M
rows with the same aggregates, `q3` groups and aggregates WITHOUT a join in
24ms; `q8` adds the join and takes 172ms. The scan and aggregate machinery is
already competitive - ClickHouse answers the joined query in about 20ms - and
the join adds 148ms, 86% of the query. Nothing needs rewriting except the
join.

Within that, the build side is the surprise. It holds 100,000 users against
2,000,000 probe rows, yet `resolve_join_group_plan` and
`build_hash_join_state` together take roughly 40% of the query: about 690ns
per build row, against 50ns per probe row. Profiling inside the build shows
over half of it in `Clone::clone` and `memmove` - materialising each row as a
`Vec<Value>` and copying every cell, including an owned `String` per row for a
`region` column holding eight distinct values. Memory accounting, which looked
like a plausible culprit, is under 1%.

So the direction is to keep the columnar form through the join
rather than materialising rows: typed key columns, packed fixed-width join
keys instead of hashing a heap structure, and dictionary encoding for the text
that grouping repeats. That is a substantial piece of work on the executor's
core, not a tuning pass, and it should be entered deliberately with the
engine-speed track as its measure.

One caveat on the evidence: a row-count scaling sweep was run and discarded.
Measuring 0.5M through 4M back to back let the largest dataset evict the
smaller ones from page cache, and it reported 830ms for a 2M query that
measures 173-183ms warm. The thread-scaling numbers above avoid that - one
dataset, consecutive runs - which is why they are quoted and the sweep is not.

Dictionary-encoding the build side was then implemented and measured, and it
is slower. Storing build rows as interned cells - a per-bucket dictionary of
distinct text, a `Vec` of tagged cells, and rows addressed by offset - removed
the owned `String` per row that the profile pointed at, and cost 15-30% on
`q8` (214-239ms against 162-192ms, measured order-reversed in two interleaved
pairs). The reason is that the old path MOVED a whole `Vec<Value>` per row,
one memory operation, while interning walks every cell, hashes each text
value against the dictionary, and pushes cells one at a time. It then repays
that on the probe with a tag branch and a dictionary indirection per access.
The clone the profile attributed to the build side is real, but it is cheaper
than the bookkeeping that removes it. Reverted rather than kept.

Sampling the same query then found the actual cost, and it was not the row
representation at all: `resolve_join_group_plan` was 26% of the query, and
ICU sort-key generation inside it was 12.6% of non-idle samples. The plan
resolves group identity from the build side, collating the group column of
every build row - 250,000 of them - for a `region` column with eight distinct
values. Memoising the sort key by its text drops ICU to 0.3% of samples and
takes about 7% off the query (min 124-128ms against 135-137ms, two
interleaved pairs, n~150 per arm). The cache is capped, so a group column
with a distinct value per row degrades to the previous behaviour rather than
holding every string twice.

This supersedes the earlier note that "memoising the collation key" measured
at nothing. That attempt memoised a different key on a different path, and
was measured best-of-three, where the noise floor is wider than the effect.

The group column arrives dictionary-coded, and the plan throws that away.
Instrumenting the build side of `q8` shows the join key as a plain `UInt64`
projection and the `region` column as `DICT codes=4096 distinct=8` - storage
has already done the work of reducing it to eight values, and the batch still
carries the codes. `resolve_join_group_plan` never sees them: it runs after
`batch_row` has materialised each row into a `Vec<Value>`, by which point the
coding is gone and every row's group must be re-derived from its text -
encode a byte key, hash it, look it up, 250,000 times to answer a question
with eight possible answers.

That is what remains of the cost after the collation memo and FNV took it from
26% of the serial path to 12.3%. Resolving group identity during the build,
where the codes are still in hand, turns the per-row work into one array
index and leaves eight encodes per batch.

The obstacle is association, not arithmetic. The plan maps each bucket to its
rows' group indexes BY ADDRESS, which is only sound because the build has
finished and the map has stopped rehashing; resolving during the build would
take addresses that later move. The fix is for a bucket to carry its group
indexes beside its rows rather than in a separate map keyed by where it
happens to live - a mechanical change to the bucket type, and explicitly NOT
a change to the row representation, which is the thing that failed when it was
tried.

Resolving the group during the build was implemented and is 5% SLOWER. The
previous entry argued for it: the group column arrives dictionary-coded, plan
resolution re-derives group identity from text per row, and moving the work
into the build would keep the codes in reach. It also removed the per-probe-row
lookup that found a bucket's group indexes by its address. Measured on an idle
sixteen-core host with four interleaved rounds, it read 169-173ms against
160-167ms - no overlap between the arms. The likely mechanism is that the
second pass it replaced was a tight loop over already-materialised rows with
exact-sized allocations, while resolving inline adds a branch and a growing
per-bucket vector to a hot loop that runs once per build row. Reverted.

That is the second measured failure at the same target, and the two together
say something the profile could not: the per-row costs in the join are not
where the time is. So the question was put differently - not "what is slow"
but "how much of this runs on one core" - and the answer changes the program.

Speedup from one thread to sixteen, on an idle sixteen-core host, 2M rows,
minimum of a twelve-second window, forward and reversed:

  q3 (no join) 75ms -> 44 -> 29 -> 26 -> 24 -> 25   peak 3.12x at 12 threads
  q8 (join)   323ms -> 228 -> 180 -> 169 -> 165 -> 168  peak 1.96x at 12 threads

Fitting Amdahl: q3 is about 26% serial, ceiling 3.9x. q8 is about 47% serial,
ceiling 2.1x. THE JOIN IS HALF SERIAL - no number of cores takes it past
twice one core's speed.

Extrapolating the one-thread cost to the benchmark's 20M rows says the rest.
q3 at perfect sixteen-core scaling would answer in 47ms against ClickHouse's
69ms - ahead. q8 would be 202ms against 169ms - near parity. Our per-row cost
is already competitive; what we do not do is use the machine. The measured
4.5x execution gap and the 8.4x novel-query gap are, to a first approximation,
the difference between 3x scaling and 12x scaling.

This supersedes the framing of the earlier entries. "The join is the
analytical gap, not the scan or the aggregate" was drawn from a 2M-row local
profile; the benchmark's join-free `Q3` is 4.1x behind, so the gap is not
join-specific. And per-row tuning - cheaper hashing, memoised collation keys,
packed keys - helped precisely because those costs sat on the SERIAL path,
not because per-row work is expensive in general. The next work is to find
what runs serially and make it run in parallel: batch pulling and decode ahead
of the parallel aggregation are the first suspects, since the scan hands
batches out one at a time.

The join's serial 44% decomposes into a serial build and a serial gather.
Timing the phases directly, on an idle sixteen-core host, 2M rows:

                    1 thread     16 threads
  join build          44.4ms        45.0ms     does not scale at all
  probe: gather       34ms          28ms       does not scale at all
  probe: parallel    239ms          52ms       4.6x
  total              349ms         165ms       2.12x

Serial work is 73ms of the 165ms sixteen-thread query - 44%, which reproduces
the Amdahl fit from an unrelated method and is the reason to believe both.

The build does not scale because it is one thread walking batches, computing
keys and inserting into one map. The gather does not scale because a round of
batches is pulled - and decompressed - one at a time before being handed to
`par_iter`. The parallel remainder reaching only 4.6x is a separate question
and may be memory bandwidth rather than removable serial work; it should not
be assumed fixable.

Ranked by what is actually on the table: the build is 45ms, the gather 28ms.
An earlier reading of this that put the probe first confused "does not scale
perfectly" with "is serial" - the probe's serial part is the smaller of the
two.

Partitioning the build by key hash, so each thread owns a partition and the
probe routes through the same function, removes the larger block without a
merge step. Decoding a round's batches in parallel removes the other, and that
one also lands on every scan-and-aggregate query, not just joins.

Parallelising the build's inputs does nothing, which says what the build
actually costs. Key evaluation and row materialisation were moved across the
pool, leaving accounting, bounds, the map insert and the spill decision
serial and in order. The build phase read 47ms on sixteen threads against
45ms before, and 52ms on one thread against 44ms - slightly worse both ways,
from the pool's overhead and one extra vector. Reverted.

So the 45ms is not the per-row work fanned out; it is what remains. At 250,000
rows that is 180ns per row, and `q8` joins on `user_id`, which is UNIQUE -
every row is its own bucket. The build therefore does a quarter of a million
inserts into a map growing to a quarter of a million entries, cache-missing on
most of them, and a heap allocation per bucket for a row vector holding one
row. That is serial by nature and untouched by parallelising the inputs.

It is also, in hindsight, why dictionary-encoding the build side lost: it
added per-cell bookkeeping to a phase whose cost is map growth and allocation,
not cell handling.

Three measured failures now share a shape: each predicted a win from a phase's
share of the profile without first establishing WHICH operation inside that
phase dominates. The build's remaining 45ms and the gather's 28ms should be
decomposed - allocation against hashing against decode - before any further
code is written against them.

The build's cost is a cache problem, and a third of it is measurable as one.
Holding the build side, the key type and the probe size fixed and varying ONLY
the number of distinct keys - `q9` joins on a unique integer, `q10` on an
integer with eight values - the build phase reads 44.4ms against 28.9ms. So
35% of it is the large hash table: a quarter of a million inserts scattered
across a table far bigger than L2, each one a likely miss.

What it is NOT was established first, and each of those was a measurement:
fanning key evaluation and row materialisation across sixteen threads changed
the build by nothing; the map is already pre-sized per batch, so growth is
amortised; and swapping the system allocator for jemalloc moved it 1ms, which
rules out allocation despite two allocations per row.

That points at the standard answer rather than another guess. Radix-partition
the build by key hash so each partition's table is cache-resident, and give
each partition to a thread: the same change removes the cache misses AND the
serial fraction, which is why it is worth more than either alone. The grace
join already partitions by key hash for spilling, so the routing function and
its probe-side counterpart exist.

The remaining 28.9ms is per-row work independent of table size, and is NOT yet
decomposed. It should be, before anything is written against it - the same
discipline that turned three failures into this.

Parallelising the build's inserts across partitions changed the build by
nothing, and the reason narrows what is left. Each bin belongs to one
partition, so the inserts touch disjoint maps and can run at once; the build
phase read 45.9-50.6ms against 45.2-45.4ms, which is no gain and some loss.
The granularity is why: a batch is 4,096 rows spread over 64 bins, so each
task carries about 64 rows and the pool's per-task cost exceeds the work.

Taken with the two earlier negatives - fanning key evaluation and row
materialisation across the pool did nothing, and swapping the allocator moved
1ms - the build's 45ms is now attributable by elimination rather than by
guess. It is not the inserts, not the key work, not allocation, and only about
15ms of it is hash-table size (44.4ms against 28.9ms when only key cardinality
varies). What remains is pulling and decoding the build side's own batches:
`next_batch` is called serially and does the zstd decompression, and nothing
tried so far has touched it.

That also explains why the aggregate's round-width fix helped the join and the
join-free query differently. Widening the round gave the PROBE more parallel
width; neither the build's decode nor the probe's serial gather moved, and
those are what the scaling curve is measuring.

The next thing worth trying is therefore overlap rather than more parallelism:
decode the next round while the current one is being processed, so the serial
decode leaves the critical path instead of being divided. If bigger work units
are wanted for the inserts as well, they should be accumulated across a round
of batches rather than dispatched per batch.

Overlapping the build's decode with its own work is slower too, and the number
that explains it also explains the three failures before it. Pulling the next
batch on a scoped thread while binning and inserting the current one read
55.5-58.2ms against 44.6-45.6ms. `std::thread::scope` spawns an OS thread per
batch, and at roughly twenty-five batches that is about 10ms - almost exactly
the regression.

The number underneath is that the build handles about twenty-five batches in
45ms: 1.8ms each. Every attempt so far has been a coordination scheme applied
at that granularity - fanning out key evaluation, fanning out row
materialisation, partitioned inserts, and now one-batch lookahead - and each
was defeated by its own overhead against a 1.8ms unit. There is no dominant
phase inside the build to attack; the cost is spread thinly across many small
units, which is why eliminating candidates one at a time kept finding nothing.

That rules out the whole family rather than another member of it. What is left
is coarser granularity - accumulate the build side and dispatch ONE parallel
pass over the partitions instead of twenty-five - or making the per-row work
itself cheaper, of which the measured part is the hash table: 44.4ms against
28.9ms when only key cardinality varies, everything else held fixed.

A prefetching decorator around `BatchStream` was considered and rejected before
measuring. The trait also carries synchronous memory planning -
`retained_bytes` and `next_batch_memory_upper_bound(budget)` - which the
executor uses to decide whether it can afford a pull. Moving the inner stream
onto a worker leaves those unanswerable, and guessing a memory bound is how a
query OOMs instead of spilling.

The batch size is the largest lever found, and it is one constant. Executor
batches target 4,096 rows; ClickHouse's `max_block_size` defaults to 65,536.
Raising ours to 65,536 measured 135-136ms against 151-159ms on the join and
23ms against 25-26ms on the join-free group-by - 11-14% and 8-12% - which is
more than every hand-written change in this program put together.

It also explains the failures that preceded it. The build handles about
twenty-five batches in 45ms, so a unit is 1.8ms, and four separate coordination
schemes were each defeated by their own overhead against that unit. The units
were not small because the work was small; they were small because the constant
said so.

It does not ship as a constant. Two spill tests fail at 65,536 AND at 16,384,
so the exposure is not the size: a query with a tight ceiling asks for one
batch, is quoted 2.1MB against a 1MB limit, and fails on the first pull instead
of receiving a smaller batch. The storage scan already does the right thing -
`planned_batch_rows` caps the target by what the budget affords - but the
join's own output batching treats `DEFAULT_BATCH_ROWS` as a hard size and holds
sixteen times more when the constant grows.

So the work is to make the remaining producers size to the budget the way the
scan already does, after which the constant can rise. That is also what the
`BatchStream` contract already asks for in words: a small ceiling is a reason
to produce a smaller batch, not to fail.

The batch-size win is downstream of the scan, not in it. Raising only the
scan's target - the one producer that already sizes to its budget - measured
153-156ms against 155ms on the join and 26-28ms against 26ms on the join-free
group-by. Nothing. The 11-14% from raising `DEFAULT_BATCH_ROWS` globally
therefore comes from the producers that BUFFER rows and emit a fixed count:
the join's output batching and the materialized-row paths, not from how much
storage hands over at once. The likely reason the scan cannot use a larger
target is that a segment's `block_rows` already bounds what a read yields, so
asking for more changes nothing; that is the next thing to verify.

Making those producers size to their budget was attempted and abandoned in the
same session. Three separate attempts each fixed one site and broke another -
`next_materialized_batch`, the cross-join producer, and top-k - because the
per-row figure has to come from the same function that reserves. Payload alone
reads about 33 bytes per row where `estimated_record_batch_bytes` charges 433
with column and validity overhead, so a cap computed from the wrong estimate
does not bind. Several operators assume a fixed batch size in their accounting,
and changing one at a time moves the failure rather than removing it.

So this is a deliberate slice rather than a patch: give the materialized
producers one shared budget-aware sizing helper that uses the reserving
function for its estimate, convert them together, and only then raise the
constant. It is worth the care - 11-14% is larger than every hand-written
change in this program combined - and it is worth NOT shipping half of it,
because the failure mode is a query with a tight ceiling failing on its first
pull instead of returning a smaller batch.

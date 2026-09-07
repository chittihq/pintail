# System optimization opportunities from engineering blogs

Research date: 2026-09-07. Code inspected across `ce240a0`–`b72db77` while
independent work continued in this checkout. These are proposals, not engine
changes or measured Pintail speedups. This expands beyond allocation layout.

## Recommendation

Prioritize **shared in-flight execution**, **dependency-aware reuse during CDC**,
and **memory decisions based on whether state is cheaper to retain, rebuild,
or spill**. These could remove entire scans, duplicated operator state, and
unnecessary spill cycles. They potentially change cost per completed query more
than another small improvement to a scalar loop.

The mechanisms are established systems ideas. The proposed combinations and
integration boundaries are specific to Pintail; no claim of research originality
or guaranteed speedup is intended. Existing simulations are identified explicitly.

| Priority | Opportunity | Main value | Current evidence / uncertainty |
| --- | --- | --- | --- |
| 1 | Share identical in-flight queries at one exact snapshot | Burst throughput; duplicate memory | Strong external mechanism; Pintail duplicate-query frequency unknown |
| 2 | Keep results valid across changes to unused columns | Throughput during CDC | Strong product fit; substantial invalidation correctness work |
| 3 | Reclaim state by reconstruction cost | Fewer spills and failures under concurrency | Existing policy simulations; real cost curves unmeasured |
| 4 | Compile logical expressions into reusable physical work | Less per-row conversion; more reuse | Promising SQL-specific extension; need expression census |
| 5 | Admit work by its current phase | Query tails and CDC freshness under load | Existing admission and simulations; cross-layer integration needed |
| 6 | Compact ranges according to read work avoided | Lower merge-on-read tax during updates | Existing overlap and heat ideas; benefit must exceed rewrite cost |
| 7 | Share bounded decode batches across related queries | Fewer reads/decodes during dashboard bursts | Related simulation exists; slow-consumer and snapshot costs unknown |
| 8 | Maintain only the hot aggregate groups under updates/deletes | Repeated analytical reads with live CDC | Largest potential extension; highest semantic and maintenance cost |

## 1. Share identical work while it is happening

Meta's gateway combines requests across clients and coalesces simultaneous
requests for one key. Discord also coalesces image transformations. Both provide
examples where concurrency creates an opportunity to avoid repeated work.
[Meta](https://engineering.fb.com/2026/09/03/core-infra/zgateway-proxy-zippydb-meta/),
[Discord](https://discord.com/blog/how-discord-resizes-150-million-images-every-day-with-go-and-c).

**Pintail proposal:** several refreshes requesting the same deterministic query
at the same pinned snapshot join one execution. Share immutable result batches
or a bounded result; release it after its last consumer. Start with bounded
results and already-identical snapshots, with no artificial waiting window.
Twenty identical requests are an illustrative workload, not a 20× prediction:
response encoding and transmission still happen for each client.

**Difference from today:** `ReplicaCache` coalesces loading the replica; that is
not coalescing execution of the query. e48 explores aligned scans, e73 retained
intermediates, and the settled aggregate memo reuses a narrow class of completed
results. This proposal targets concurrent duplication before a result exists.

Key by database identity, every participating table's pinned snapshot,
parameters, bound plan, schema/semantic version, relevant session settings,
output limits, and authorization scope. Do not key on SQL text alone. Volatile
expressions, differing clocks, incompatible snapshots, and unsupported shapes
run independently. A request arriving after a commit cannot join an older
snapshot merely because the SQL matches. Do not make CDC wait to improve reuse.
A cancelled follower must not cancel other readers; a slow reader must not
retain unbounded batches. Query accounting and audit events remain per request.

**Experiment:** HTTP and wire clients at concurrency 1/4/16/64, with 0/25/100%
identical requests, exact snapshot controls, cancellation, and slow readers.
Compare engine execution count, CPU per completed response, peak shared and
per-client memory, goodput, and p99. Reject if a non-sharing workload regresses
materially or live CDC makes overlap too rare. Initially propose a 20% burst
CPU reduction with under 5% no-overlap p99 regression as a decision threshold.

## 2. Make cache validity depend on what a query actually reads

Streaming query systems map data changes to dependent computation rather than
blindly rebuilding complete results. Readyset describes on-demand partial state;
Materialize explains why retained computation state has a real memory cost.
[Readyset](https://readyset.io/blog/behind-the-magic-how-readyset-speeds-up-queries-with-streaming-dataflow),
[Materialize](https://materialize.com/blog/materialize-and-memory/).

**Pintail proposal:** a report reading four columns of a wide table should not
lose its reusable result solely because an update changes a different column.
Maintain column-change generations plus a row-membership generation. A bound
query records its complete dependency set, including predicates, join keys,
ordering, grouping, generated expressions, and correlated expressions. Updates
can preserve eligibility only when all dependencies and row membership are
unchanged. Inserts/deletes invalidate conservatively. Incomplete row images,
DDL, uncertain dependencies, or unknown changes fall back to recomputation.

**Difference from today:** the settled memo keys by manifest generation;
insert-only delta and SMA paths already cover specific live-ingest aggregates.
This is a validity extension for unrelated updates, not a proposal to add those
existing mechanisms again. #34's metadata-bookkeeping fix is related but does
not establish column-sensitive query validity. A targeted source search did not
find column-generation accounting; this is a candidate, not an exhaustive
absence proof.

Generations must advance atomically with the visible commit. Derived validity
metadata must be conservative after restart, resnapshot, and polling repair.
Dropping cache entries on restart is acceptable; retaining a falsely valid
entry is not. This remains replica-snapshot consistency, not synchronous
freshness with the upstream database.

**Experiment:** invented wide tables, unchanged read columns, then progressively
more updates touching read columns. Compare at fenced CDC positions. Include
predicate-only columns, updates moving join/group keys, inserts/deletes, minimal
row images, DDL, restart, and repair. Measure useful cache hits and CPU saved
minus dependency tracking overhead and extra CDC lag. Reject if full-row images
or dependency fan-out cost more than avoided computation. #31's live-tail
benchmark is the important system test.

## 3. Choose between eviction, compression, recomputation, and spill

Materialize's dictionary-compression work shows that retained computation state
can be compressed, while its memory article discusses moving rebuildable state
to cheaper storage. Compression has construction and access costs; it is not a
free reduction in RAM.
[Dictionary compression](https://materialize.com/blog/dictionary-compression-in-materialize/),
[Memory](https://materialize.com/blog/materialize-and-memory/).

**Pintail proposal:** distinguish optional derived state from indispensable
unfinished state. A cheap memo can be evicted and recomputed, a reused
low-cardinality intermediate can be compressed, and expensive unfinished join
or aggregate state can spill. Select using measured recent rebuild time,
reuse, retained bytes, compression cost, and spill I/O. Apply bounded,
infrequent decisions with hysteresis; never let the controller violate caps.
Do not discard state unless a pinned replay source proves it reconstructible.

**Difference from today:** #12 already provides bounded spilling. e56/e59/e70
simulate memory allocation and marginal spill savings, e60 recycles buffers,
and e72 considers decoded mirrors. The new integration question is the choice
of *state representation or reconstruction*, not merely who receives memory.
The earlier row-layout experiment is one input: it demonstrated both savings
and conversion costs, not an answer for all state classes.

**Experiment:** simultaneous repeated subqueries, large joins, and output-heavy
aggregations under 0.5/1/2× their unconstrained working set. Compare current
policy, eviction-only, compression-only, spill-only, and a measured chooser.
Charge transition peaks, dictionary memory, CPU, and additional reads. Include
unique strings and reuse reversal as negative controls. Accept only a whole
workload improvement in goodput, disk traffic, or peak memory with exact results
and bounded p99; reject policies that win by deferring work beyond measurement.

## 4. Preserve SQL semantics while eliminating redundant physical work

Materialize's representation-type article shows how bookkeeping casts can
prevent common-expression and retained-state sharing even when physical values
do not change. Uber's Preon demonstrates workload analysis of query structure
and predicate use.
[Representation types](https://materialize.com/blog/no-classification-without-representation/),
[Preon](https://www.uber.com/en-NL/blog/preon/).

**Pintail proposal:** retain precise logical types in binding and output metadata,
but attach a separate, explicit physical-operation identity where equivalence
has been proved. Compute a physical expression once per batch and share it
between filters, grouping, sorting, and joins when their semantics agree.
Extend this to immutable dictionary entries: compute each required collation
key once per dictionary generation and collation, rather than independently
per operator and per query. Never compare dictionary codes from unrelated
dictionaries without a checked translation.

**Difference from today:** packed kernels and dictionary execution already
exist. `CollationKeyCache` already caches repeated text within the join path.
The extension is a safe shared identity across compatible consumers, not a new
local string-key cache. e73 is related but studies broader retained intermediates.

MySQL signedness, overflow, decimal scale, timezone, collation/coercibility,
ENUM/SET ordinal semantics, warnings, and error/short-circuit behavior can make
apparently identical expressions different. Same storage carrier does not mean
same semantics. Begin with one proven identity, not broad cast elimination.

**Experiment:** generated BI-shaped expressions with repeated compatible work,
plus counterexamples covering the distinctions above. Compare complete
parse/bind/execute time and memory, record physical evaluation counts, and use
the differential oracle. Reject if hashing/sharing overhead dominates simple
queries. Collect workload frequencies locally without publishing raw SQL or
customer identifiers. This also makes it easier to rank optimization effort.

## 5. Schedule phases, not just whole queries

ScyllaDB's connection-storm work separates CPU-active setup from network waits
and favors progress of partially completed work. Its asymmetric I/O report is
also a useful negative result: moving I/O work did not produce a universal disk
throughput improvement.
[Connection storms](https://www.scylladb.com/2026/07/01/cutting-p99-during-connection-storms/),
[I/O offload](https://www.scylladb.com/2026/07/22/asymmetric-io_uring-backend-seastar/).

**Pintail proposal:** retain hard query admission and memory bounds, but divide
runnable CPU, resident state, outstanding spill I/O, and finalization capacity.
A query waiting for disk need not monopolize a CPU execution slot; many queries
must not simultaneously enter a large finalization allocation. Protect CDC
progress and bounded short queries while guaranteeing eventual progress for
large ones. This is an in-process scheduler, not a new proxy or service.

**Difference from today:** `QueryAdmission` already distinguishes general and
short queries, and HTTP execution is offloaded to blocking workers. e38/e49/e63
already simulate overload, runnable budgets, and completion reserves. The
opportunity is to verify and connect those policies to real execution phases;
renaming them would not be innovative work.

**Experiment:** connection bursts plus short queries, large spills, and CDC.
Compare with the existing reserved-short admission policy, not an unbounded
baseline. Record runnable/blocked worker time, allocated threads, queue wait,
completed-query goodput, per-class p99, and CDC lag. Include long-query starvation
checks. Keep durability and total resource limits identical. Reject if the new
queues merely move tail latency or shift an unbounded backlog into another layer.

## 6. Spend compaction effort where queries repay it

Redpanda separates compaction scheduling from unrelated placement decisions and
prioritizes eligible work. Its transaction/tombstone article also demonstrates
that removing old records requires a correctness argument spanning more than
one file.
[Compaction scheduling](https://www.redpanda.com/blog/how-redpanda-cloud-topics-rethinks-kafka-compaction),
[Compaction correctness](https://www.redpanda.com/blog/kafka-log-compaction-bug-fix-streaming).

**Pintail proposal:** rank safe overlapping ranges by estimated future query
work avoided per byte rewritten. Observed merge comparisons, overlap depth,
query frequency, and expected near-term reads contribute to the numerator;
actual read/write/compression work contributes to the denominator. Retain a
mandatory debt/file-pressure floor so cold ranges cannot accumulate indefinitely.
Use feedback from completed compactions to correct estimates.

**Difference from today:** bounded compaction, debt reporting, and overlap
selection already exist. e46/e50/e54 study compaction control and tombstone heat.
This proposal combines actual query read amplification with maintenance cost;
it should begin as a shadow ranking of the existing safe candidates.

**Experiment:** two equally sized synthetic ranges with different read and
update frequencies, followed by a hot-range shift and uniformly scattered
updates. Same total compaction budget in every arm. Count *all* maintenance
work, final debt, write amplification, query CPU, p99, and CDC lag. Verify
update/delete visibility and restart behavior. Reject an apparent speedup that
merely borrows from future compaction or evicts useful cache pages. #31 is a
better discriminator than the fully compacted static benchmark.

## 7. Share decoding without retaining complete intermediate results

RocksDB's asynchronous-I/O article distinguishes overlapping independent reads
from waiting for each read in sequence. Meta demonstrates the value of batching
at a point that can see multiple callers.
[Overlapping reads](https://rocksdb.org/blog/2022/10/07/asynchronous-io-in-rocksdb.html),
[Cross-client batching](https://engineering.fb.com/2026/09/03/core-infra/zgateway-proxy-zippydb-meta/).

**Pintail proposal:** queries scanning the same immutable segment and compatible
columns can consume a shared decoded batch while applying separate predicates
and aggregates. Bound sharing to a small live window. Detach slow consumers;
do not pin an entire scan or wait indefinitely for a possible partner.
Request-local memtable/version resolution stays separate unless snapshot
identity proves it shareable. Permit immediate independent execution.

**Difference from today:** e48 already simulates aligned scan frontiers. The new
experiment must use real decoding, `RecordBatch` ownership, memory charging,
pruning, and cancellation. This is an escalation in evidence, not a new claim
that shared scanning has just been invented. Existing prefetch is the baseline.

**Experiment:** simultaneous related queries with 0/25/75/100% column and segment
overlap, staggered arrival, selective predicates, and a deliberately slow
consumer. Measure physical bytes read, decode calls, peak pinned bytes, and
completed-query throughput. Reject if a wider shared projection decodes more
than the separate selective scans, or a fast query waits for an unrelated one.

## 8. Repair only hot aggregate groups when rows change

Readyset describes partial materialization on demand; Materialize's
self-correcting-view work highlights that persisted derived results may become
wrong across semantic changes even if source data do not change.
[Partial state](https://readyset.io/blog/behind-the-magic-how-readyset-speeds-up-queries-with-streaming-dataflow),
[Semantic drift](https://materialize.com/blog/self-correcting-materialized-views/).

**Pintail proposal:** for a small admitted set of repeated group aggregates,
retain exact state only for hot groups. Apply before/after-row changes to
supported invertible aggregates, and mark affected groups dirty for selective
recomputation when an exact inverse is unavailable. For example, removing the
current minimum needs additional state or recomputation; subtracting the old
minimum is invalid. A group-key change affects both groups. Preserve exact
floating-point behavior rather than assuming arithmetic reassociation is safe.

**Difference from today:** insert-only memo merging, SMA folding, and e53's
immutable aggregate composition already exist. Updates, deletes, partial hot
group state, and bounded repair are the proposed extension. Start with narrow
integer/decimal/count shapes whose arithmetic and error behavior are proved;
fall back for unsupported aggregates and expensive join fan-out.

**Experiment:** refresh frequency versus update frequency, with hot-key skew,
group migration, deletes of extrema, cold groups, and semantic-version changes.
Measure total query plus CDC plus repair CPU and retained state, not read latency
alone. Fence updates for exact oracle comparisons. Evict optional state when
maintenance costs exceed avoided scans. Invalidate derived state on semantic
version changes; persistent self-repair is not required for the first prototype.
This is the highest-risk candidate and should follow simpler dependency reuse.

## Experiment sequence and evidence standard

1. Add local shape/counter instrumentation to measure duplicated simultaneous
   work, repeated physical expressions, unused-column updates, spill/rebuild
   costs, and overlap-related read work. Existing telemetry is the starting
   point. Do not export customer queries, identifiers, literals, or row counts
   into the public repository; reproductions use invented fixtures.
2. Prototype in-flight sharing first: a narrow switch, real query execution,
   identical pinned inputs, no new external dependency. In parallel work is
   not required; the next candidate is selected from measured opportunity.
3. Prototype dependency-aware reuse next if source changes show the required
   pattern; otherwise prioritize phase scheduling or state reclamation based
   on counters. Avoid building a global adaptive controller before measuring
   the resource costs it would control.
4. Every A/B reports goodput (successful complete SQL responses), CPU/response,
   allocator live/active/resident bytes, RSS, peak query charge, per-class
   p50/p95/p99, spill I/O, CDC lag, and residual maintenance debt when relevant.
   Use the shipped allocator, the same total CPU/memory/I/O budgets, randomized
   run order, at least five independent repeats, and uncertainty across runs.
   Account for precomputation, construction, output, cleanup, and repair.
5. Include a workload with no opportunity for the optimization. No optimization
   earns adoption solely by outperforming a deliberately weak baseline.
   Illustrative thresholds above are proposals, not policy changes.
6. Real engine tests precede claims about SQL throughput. Live differential
   tests cover snapshot + memtable + schema-history states. #31 remains the
   open measurement gap for a timed live update tail. #12 spill correctness,
   #27 adopted adaptive compression, and #34 cache eligibility remain baseline
   capabilities, not tasks to reopen.

No new performance measurements were run for this broader survey. The earlier
allocation experiment remains narrow evidence and is not used to substantiate
these system-level hypotheses. No engine dependency, storage format, release
gate, or production configuration is changed by this document.

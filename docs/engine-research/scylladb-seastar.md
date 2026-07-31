# ScyllaDB / Seastar systems techniques — extraction report for pintail

ScyllaDB is not analytical, but it is the masterclass in single-node resource efficiency. This
report extracts the systems-level novelty relevant to pintail's compaction-vs-query contention,
ingestion backpressure, and latency isolation.

## 1. Shard-per-core, shared-nothing execution

**How.** Seastar runs one OS thread per core ("shard"), pinned, with per-shard memory, data
structures, connections, and schedulers — no shared mutable state, no locks on the hot path.
Cross-shard communication is exclusively `smp::submit_to(shard, lambda)` over lock-free SPSC
message queues, batched and polled by the reactor. Refs: https://seastar.io/shared-nothing/,
https://www.scylladb.com/product/technology/shard-per-core-architecture/,
https://github.com/scylladb/seastar/blob/master/doc/tutorial.md,
https://www.scylladb.com/2024/10/21/why-scylladbs-shard-per-core-architecture-matters/.

**Why.** Eliminates lock contention, cache-line bouncing, coherence traffic; makes per-core
resource accounting *possible* — the foundation everything below builds on.

**Applicability: MEDIUM-HIGH (architectural), pragmatic Rust caveat.** Full shard-per-core
conflicts with tokio-control-plane + worker-pool, and for analytical scans work-stealing across
cores is often what a single query wants. Transplantable middle ground: (a) pin worker threads;
(b) strictly thread-local per-thread state (arenas, buffer pools, metrics); (c) shard *ingestion*
state (memtable/WAL segments) per worker so writes never take locks, even if reads fan out.
Rust: glommio (closest Seastar port: thread-per-core, io_uring, proportional-share task queues),
monoio (fastest raw throughput, FIFO only — useless for compaction isolation),
tokio-with-pinned-single-threaded-runtimes as the boring option. Case study: Apache Iggy's
migration https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/.

## 2. Userspace CPU scheduler: scheduling groups + shares

**How.** Every task belongs to a *scheduling group* (statements, compaction, memtable-flush,
streaming…). Each group has *shares*; the reactor runs a proportional-share scheduler over group
runqueues, accounting actual vCPU-time per group. Shares only bite under contention — an idle
system lets compaction use 100% CPU. Workload Prioritization exposes this as CQL
`SERVICE_LEVEL ... WITH SHARES=n`. Measured: OLAP at 260K ops/s co-located with OLTP; with shares
1000:10, OLTP p99 stayed 4–7 ms and lost only 3–10% throughput vs 5–6× p99 blowup unmanaged.
Refs: https://www.scylladb.com/2019/05/23/workload-prioritization-running-oltp-and-olap-traffic-on-the-same-superhighway/,
https://www.scylladb.com/2026/01/28/can-database-workloads-coexist/.

**Why.** The only known-good answer to "compaction vs queries" that doesn't involve crude
concurrency caps or pausing compaction (which builds debt). Background work is never *paused*,
it's *metered*.

**Applicability: HIGH.** Tag every unit of data-path work (query morsel, compaction chunk, flush
batch) with a scheduling class; per worker thread keep per-class deficit counters of consumed
CPU-time and pick the next task from the class with lowest weighted virtual runtime. A per-worker
multi-queue with stride/deficit scheduling over the existing thread pool gets 90% of the benefit.

## 3. Fine-grained preemption: task quotas, `maybe_yield`, stall detector

**How.** Cooperative scheduling with a *task quota* (default 500 µs): every loop and `co_await`
checks a preemption flag; long loops insert `seastar::maybe_yield()`. A *stall detector* fires
when any task runs >N ms without yielding and logs a backtrace. Refs:
https://docs.seastar.io/master/tutorial.html,
https://docs.seastar.io/master/classseastar_1_1coroutine_1_1maybe__yield.html.

**Why.** Shares are meaningless if a compaction merge loop holds the CPU for 50 ms. Preemption
granularity, not scheduling policy, bounds p99.

**Applicability: HIGH.** Process compaction and scans in fixed-size morsels (≲500 µs worst-case),
returning to the scheduler between morsels. Add a watchdog thread sampling per-worker "last yield
timestamp", logging a backtrace when a worker exceeds ~20 ms.

## 4. Userspace I/O scheduler: priority classes, disk model, token bucket

**How.** All file I/O is direct I/O (O_DIRECT, linux-aio or io_uring); kernel page cache and
kernel I/O queues bypassed so queuing happens in userspace where priority is known:
- **Fair queue with priority classes** (~6: query, commitlog, compaction, flush, streaming…),
  weighted cost per request, exponential-decay accounting.
  https://www.scylladb.com/2016/04/29/io-scheduler-2/
- **Bounded disk concurrency**: beyond a disk's "maximum useful concurrency" you only add latency
  variance. Keep excess queued in userspace where it can be reordered by priority.
- **iotune / diskplorer**: benchmark the disk at install to get 4 numbers (read/write bandwidth,
  read/write IOPS); the scheduler builds a disk model from them.
  https://github.com/scylladb/diskplorer
- **The 2021.2 disk model**: normalize requests to *tokens* via
  `Br/Or·reads + Bw/Ow·writes ≤ K`; rate-limit dispatch with a **token bucket whose refill is fed
  by actual disk completions**, not just the model — FTL garbage-collection slowdowns
  automatically throttle dispatch. Result: commitlog latency 1.5 ms → 0.5 ms, query latency −50%
  under compaction.
  https://www.scylladb.com/2022/08/03/implementing-a-new-io-scheduler-algorithm-for-mixed-read-write-workloads/
- **IO groups + capacity rovers**: shards negotiate capacity via two atomic counters (tail rover
  on submit, head on completion; dispatch while `tail − head < capacity`).
  https://www.scylladb.com/2021/04/06/scyllas-new-io-scheduler/,
  https://thenewstack.io/lessons-learned-from-6-years-of-io-scheduling-at-scylladb/

**Why.** For an LSM engine, compaction-vs-query I/O contention is *the* p99 driver. Two
non-obvious insights: (1) priority scheduling requires the queue in userspace, which forces
bounded device concurrency and usually direct I/O; (2) a static disk model lies — SSDs degrade
dynamically, so dispatch must be capped by observed completions too.

**Applicability: HIGH.** Route all data-path I/O through a per-device userspace queue with
classes {query, WAL, flush, compaction}; benchmark the disk once at first startup; token bucket
with completion-fed refill. Staged path: priority-classed queue + bounded concurrency over
buffered I/O first, move compaction/flush (sequential, cache-polluting) to O_DIRECT, queries
last.

## 5. Memory: per-shard pools, log-structured allocator, own cache

**How.** Memory statically split into per-shard pools. The **log-structured allocator (LSA)** for
memtable + row cache makes objects movable/compactable and *evictable* (the allocator reclaims by
evicting LRU rows). The row cache replaces the kernel page cache: object granularity, one
consolidated post-merge row version, range-continuity markers, `BYPASS CACHE` for analytic scans,
SSTable index caching. Refs: https://www.scylladb.com/2024/01/08/inside-scylladbs-internal-cache/,
https://www.scylladb.com/2018/07/26/how-scylla-data-cache-works/,
https://www.scylladb.com/2024/11/04/database-internals-optimizing-memory-management/.

**Applicability: MEDIUM.** A movable allocator is Rust-hostile (relocation breaks references).
What transplants cheaply: (a) per-worker bump/arena allocators for query execution (`bumpalo`);
(b) a *unified memory broker*: one budget spanning memtables + column-chunk cache + query
scratch, with a reclamation hierarchy (evict cache → force flush → queue queries); (c) cache
*decompressed column chunks* (object granularity) rather than relying on the page cache, and let
big scans bypass the cache by default.

## 6. Compaction backlog controller — feedback control sets compaction shares

**How.** Compaction's scheduling-group shares are set by a **proportional controller on a
"backlog" signal**: backlog = estimated future bytes-to-be-written to make the compaction
strategy content. For STCS, per-SSTable backlog `B_i = (S_i − C_i) · log₄(T / S_i)`; the
aggregate has a closed form maintained incrementally. Shares ∝ backlog. Negative feedback: writes
grow backlog → shares rise → compaction gets more CPU/IO → foreground writes slow slightly →
backlog stabilizes where compaction rate = ingest rate. No operator tuning, no oscillation. The
memtable-flush controller works the same way (dirty-memory ratio → flush shares).
Refs: https://www.scylladb.com/2018/06/12/scylla-leverages-control-theory/,
https://github.com/scylladb/scylladb/blob/master/docs/dev/compaction_controller.md.

**Why.** Converts "compaction competes with queries" from a static-tuning problem into a
self-regulating system. Works *because* §2 and §4 exist — the controller needs a shares knob to
actuate.

**Applicability: HIGH — the single most novel and directly relevant idea for pintail.** Define
backlog for the LSM (size-tiered formula ports verbatim; log base = fan-in). Every N ms:
`compaction_shares = clamp(k · backlog / capacity)` feeding the CPU class scheduler (§2) and I/O
class (§4). Same pattern for flush. Start proportional-only (ScyllaDB deliberately avoided
integral terms — windup).

## 7. Incremental Compaction Strategy (ICS)

**How.** Replace monolithic SSTables with *runs* of fixed 1 GB non-overlapping fragments in
STCS-like size tiers. Compacting two 100 GB inputs needs ~2 GB temporary space instead of 200 GB:
input fragments are released as output fragments are sealed. ICS 2.0 adds a space-amplification
goal for the largest tier. Refs:
https://www.scylladb.com/2020/01/16/maximizing-disk-utilization-with-incremental-compaction/,
https://www.scylladb.com/2021/04/28/incremental-compaction-2-0-a-revolutionary-space-and-write-optimized-compaction-strategy/.

**Applicability: HIGH — arguably easier for columnar.** Split sorted runs into fixed-size segment
files from day one; compaction becomes an incremental streaming merge that (a) needs constant
temp space (users can run at 80–90% disk), (b) yields naturally at fragment boundaries (synergy
with §3), (c) is checkpointable/abortable mid-run.

## 8. Ingestion flow control (admission control on writes)

**How.** Writes go "background" (ack early) until in-flight background writes reach 10% of shard
memory, then the coordinator stops early-acking, throttling clients. For deferred work, a
controller *delays responses*: `delay = f(backlog / backlog₀) · delay₀` — with bounded client
concurrency, delay converges to exactly the sustainable rate.
Ref: https://www.scylladb.com/2018/12/04/worry-free-ingestion-flow-control/.

**Applicability: HIGH.** Cap dirty (unflushed memtable + un-synced WAL) bytes per worker; past
threshold, delay ingest acks proportionally to (dirty bytes + compaction backlog) — smooth
backpressure instead of a cliff.

## 9. Grab bag

- **Per-partition rate limiting**: decaying counters (halved every second), reject with
  probability `P = L/(x·ln 2)` — O(1) state, no sliding windows.
  https://www.scylladb.com/2024/04/17/per-partition-query-rate-limiting/. LOW-MEDIUM.
- **Heat-weighted load balancing**: cluster feature, LOW — but motivates cache warmup after
  restart. https://www.scylladb.com/2017/09/21/scylla-heat-weighted-load-balancing/
- **io_uring**: Seastar's finding — coming from linux-aio the gains are modest. For Rust the
  calculus differs: io_uring (glommio/monoio/tokio-uring or the `io-uring` crate for just the
  storage path) is the way to get bounded-depth async direct I/O.
  https://www.scylladb.com/2020/05/05/how-io_uring-and-ebpf-will-revolutionize-programming-in-linux/

## Top 5 most transplantable (ranked)

1. **Compaction backlog controller (§6)** — small code, huge operational payoff; STCS math ports
   as-is.
2. **CPU scheduling classes with shares + morsel-level preemption (§2+§3)** — prerequisite
   actuator for #1; implementable on the existing thread pool.
3. **Userspace I/O scheduler with completion-fed token bucket (§4)** — priority-classed queue,
   bounded device concurrency, measured disk model.
4. **Incremental compaction via fixed-size fragments (§7)** — constant temp space, checkpointable,
   natural preemption boundaries; cheap if adopted before the format ossifies.
5. **Ingestion flow control via proportional ack-delay (§8)** — closes the loop with #1.

Synthesis: not five independent tricks — one architecture: *measure capacity (iotune) → schedule
everything in userspace against explicit shares (CPU + I/O classes) → set shares by feedback from
backlog signals → backpressure the edges when backlog grows*. Any subset transplants; the payoff
compounds when actuators (shares) and sensors (backlogs) are built as one system.

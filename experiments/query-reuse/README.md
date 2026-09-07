# Query reuse experiments

Experimental code only. Neither mechanism is connected to HTTP, wire, or the
production CDC publisher. The lab links the engine, binder, catalog and table
store from the archived commit in `BASE_COMMIT`. It is a separate workspace.

## Questions

1. Does collapsing identical in-flight execution at one pinned snapshot reduce
   complete query work under concurrent demand?
2. Can a bound query's result survive updates to unused columns, and is the
   cost of dependency accounting lower than the avoided execution?

The intended result is a decision about further engine integration, not a
production-ready cache or a release performance claim.

## Mechanisms and boundaries

`Flights` keeps at most 32 active keys; overflow executes independently.
A key includes an exact snapshot token, exact parameterized-as-literals SQL,
execution scope, settings identity, and output limit. This lab has one
fixture database; production needs explicit database identity and actual
bound/session/authorization identity, not caller-invented numeric tokens.
Only the lab's deterministic query family is passed to the coordinator.
The general-purpose coordinator itself does not determine SQL eligibility.

Followers receive an `Arc` to the same complete result; each serializes its own
response. An entry is removed when execution finishes, so later requests
execute again. There is no persistent result cache, linger window, or delayed
snapshot selection. Follower cancellation is local. Execution failures and
unwinding panics wake waiters and allow retry. Leader execution is not
interruptible by the leader client's cancellation in this experiment. A
production implementation needs an execution-owned cancellation policy and
last-reader cleanup, plus bounded follower counts/wait times and response
memory charging. Returning errors for oversized results is a lab ceiling,
not a proposed change to server behavior.

`Epochs` tracks row membership, conservative global invalidation, and changed
column IDs. The dependency walker reads the actual bound query, including
hidden ORDER BY projections, predicates, grouping, aggregate inputs and
HAVING. It accepts only a single base table; no joins, derived tables, windows,
subqueries, recursive/set queries, scalar functions, or aggregates other than
COUNT/SUM/MIN/MAX. Unknown expressions refuse eligibility. This conservative
refusal also excludes volatile functions and time-dependent expressions.

Updates compare typed before/after values; differing bytes or ENUM ordinals
invalidate dependent columns, even if a collation might consider them equal.
Insert, delete, missing-before-image, and key-change events invalidate row
membership. Incomplete-width updates invalidate globally. Unknown changes and
schema changes invalidate globally. Versions use checked increments. Even
COUNT(*) depends on row membership. Results are a single optional entry with
a one-MiB payload admission bound. This is not a complete production memory
tracker: outer row headers, key strings, and allocation rounding are outside
that experiment bound.

Publication is single-owner in the fixture: `&mut Fixture` excludes another
publisher/reader capturing ledger state during the store write. Captured table
snapshots remain usable concurrently afterward. Production must atomically
publish the validity token with the actual transaction snapshot; simply
putting this ledger beside an asynchronous CDC callback is unsafe. Restart
must discard cache state or assign a fresh namespace. The lab uses full
before-images kept in a fixture vector; recovering them from minimal binlog
images is not measured or claimed implemented.

## Correctness checks

The tests cover:

- identical followers share one allocation, later requests execute anew;
- different snapshots, SQL, scopes, settings, and output limits execute apart;
- follower cancellation, leader error, leader panic, and retry after cleanup;
- bound predicate-only and hidden-sort dependencies; refusal of volatile,
  joined, and UNION query shapes;
- actual `TableStore::ingest_cdc` updates to unused versus used columns;
- membership changes, unknown events, an empty dependency set for COUNT(*),
  and NULL transitions;
- an old pinned snapshot remains unchanged after a relevant update;
- an old execution finishing after an update cannot validate a new token;
- schema evolution invalidates eligibility; the engine rejects an old bound
  schema, then accepts a freshly bound catalog;
- flush/reopen preserves the same query result after update/delete history.

These are storage/execution-path checks, not tests of native binlog decoding,
HTTP/wire session settings, or source transaction fences. Results are compared
as complete typed rows/serialized bytes, not checksums. The independent arm
uses the current engine as the behavioral reference; no new MySQL differential
oracle run is claimed.

## Measurement protocol

Run `python3 run.py` after a release build on Linux; `--flights-only` runs
only #1 and was used for the corrected measurement. Processes run sequentially
in a seeded shuffled order. Five independent processes are run for every arm.
The native build machine is used with four-CPU affinity and two Rayon workers,
not a Docker measurement host. It is not exclusively reserved. The allocator
is tikv-jemallocator 0.6, matching the shipped allocator family. Crate versions
are fixed by this experiment's Cargo.lock; the standalone workspace resolves
its own utility dependencies and is not a bit-identical release binary.

In-flight experiment: 50,000 invented rows, a filtered grouped SUM/COUNT query,
1/4/16 simultaneous clients, and 0/25/100% duplicate requests (with redundant
one-client cases removed). The measured live-update arm first changes an unused column on an existing
key, leaving an overlapping memtable version. This prevents settled-state
reuse from answering the benchmark without scanning. A first settled-only
run unexpectedly hit the existing aggregate memo despite the predicate; its
readings and exact sources are retained under `evidence/settled/`. Unique requests use different
literal predicates. All threads are created before a start barrier releases
them. The measured batch includes thread creation, execution/coordination,
each client's JSON serialization, and cleanup. Sampler shutdown is excluded from the final live-update batch
timer; the archived settled run included it and has a roughly 0.5 ms timing
floor. Request latencies begin after the barrier. Reference queries warm the fixture before timing; each measured
response is checked against its reference. No sleep forces followers to join;
late arrivals may become additional leaders, and actual counts are recorded.

CPU time is the process user+system tick delta over the batch and sampler
shutdown (including sampler work), with tick frequency recorded. At short durations, tick resolution
limits precision. Shared query charge was sampled every 500 microseconds, but the lab leaves
the server-wide budget disabled: `reserve` skips accounting when its limit is
zero. Consequently every `sampled_query_bytes` reading is unavailable (zero),
not evidence of zero memory use. Per-query limits still apply. Memory findings
below use operating-system process peaks only; a bounded shared-budget
experiment remains necessary before integration. Process VmHWM includes
fixture construction and reference work, so it is not query-only RSS.

Dependency experiment: 10,000 invented rows, 41 refreshes, 40 single-row updates.
Every update changes the unused text column; approximately 0/25/100% also
change the numeric dependency. A deterministic permutation spreads the relevant
changes through the run; actual counts are recorded. Real WAL/memtable writes
are timed separately. Cached requests pay parsing/binding and dependency lookup;
misses also pay normal execution. The uncached arm executes normally. Tracker
work is enabled only in the cached arm. Every response is serialized, then
checked against an uncached execution on the same snapshot outside the timer.
Verification warms engine/OS state; both arms perform it. No background writer
or continuous live CDC is part of these timing runs. The 25% schedule contains
9 relevant updates out of 40 (22.5%); the exact count is recorded.
The dependency measurements are preserved in `evidence/settled/` because they
were completed before the corrected in-flight workload; their code and workload
were not changed by that correction.

Retain raw timings, medians/min/max and all independent-run readings.
Pooled p95/p99 are descriptive; small burst samples do not support a production
SLO claim. No timing excludes result serialization to make reuse look free.

## Reproduction

Archive the base commit on the authorized Linux build machine, then copy this
experiment directory into it. From this directory:

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo fmt --check
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo clippy --all-targets -- -D warnings
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test --locked
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --release --locked
python3 run.py
```

Long runs should use the repository's detached build-server workflow.
The runner prints `QUERY-REUSE-DONE` only after every subprocess succeeds and
summary evidence is written. No Docker or production resources are used.

## Results and decision

Five independent process repetitions per arm; all numbers below are medians.
The corrected in-flight matrix has 80 processes. The retained initial matrix
has 110, including the 30 dependency measurements: 190 total successful
processes. Every measured response passed exact reference comparison. Seven
unit/integration tests and strict all-target clippy passed on the Linux build
machine. This standalone experiment did not run the full release gate.

### 1. Share identical in-flight executions

| Burst | Independent batch | Shared batch | Batch speedup | Executions, independent → shared |
| --- | ---: | ---: | ---: | ---: |
| 4 identical requests | 32.77 ms | 22.99 ms | 1.43× | 4 → 1 |
| 16 identical requests | 129.60 ms | 23.71 ms | 5.47× | 16 → 1 |
| 16 requests, 4 identical | 129.07 ms | 105.80 ms | 1.22× | 16 → 13 |
| 16 distinct requests | 132.42 ms | 131.18 ms | No demonstrated benefit | 16 → 16 |

For the 16-identical burst, independent runs ranged 126.58–133.53 ms;
shared runs ranged 23.41–24.40 ms. Decoded blocks fell from 336 to 21 in
all five repetitions. Median process CPU time fell from approximately
500 to 30 ms, with coarse 10 ms tick resolution. Whole-process peak RSS
fell from 155.19 to 57.11 MiB (63%); this includes fixture construction
and reference queries, so it is not an isolated query-allocation claim.
For distinct requests, peak RSS was essentially unchanged (160.26 versus
160.41 MiB), and timing ranges overlapped. The small median difference is
not evidence of a distinct-query optimization.

This supports adopting shared execution for overlapping, eligible requests.
It establishes higher throughput for this fixed concurrent burst, not a
steady-state server capacity figure. Four available CPUs also explain why
removing fifteen executions does not produce a sixteenfold wall-time gain.
The settled-only control demonstrates why eligibility/admission should
consider existing cheap engine reuse: coordinating an already memoized
answer has much less work to save.

### 2. Preserve results across irrelevant column updates

Times are the sum of 41 refreshes, including result serialization, excluding
separately recorded ingestion and validation queries.

| Updates changing a dependency | Uncached refresh time | Reuse refresh time | Ratio | Executions, uncached → reuse |
| --- | ---: | ---: | ---: | ---: |
| 0 / 40 | 177.40 ms | 2.51 ms | 70.7× faster | 41 → 1 |
| 9 / 40 (22.5%) | 180.89 ms | 42.64 ms | 4.24× faster | 41 → 10 |
| 40 / 40 | 178.32 ms | 181.14 ms | 1.6% slower median | 41 → 41 |

The no-hit ranges overlap (175.12–181.01 versus 175.25–184.48 ms), so the
1.6% difference is descriptive, not a precise overhead estimate. It does
show that invalidating on every update removes the benefit. Ledger tracking
was approximately 10–11 microseconds across 40 updates, with full before-images
already available; this excludes their acquisition/storage cost. These runs
do not establish a meaningful process-memory reduction for dependency reuse.

This supports a narrowly eligible dependency cache for repeated reads under
updates to unused columns. The large best-case ratio depends on that update
pattern and the existing settled-state memo on the first refresh. It is not
a general query speedup or a claim that all CDC workloads benefit.

### Integration order

Implement #1 first behind explicit eligibility and resource bounds. Bind keys
to actual database, authorization, session settings, schema, and pinned snapshot
identity. Charge retained responses and bound followers; define cancellation
ownership before connecting HTTP/wire clients. Verify under a sustained mixed
workload and a finite process memory budget.

Then add #2 for the conservative single-table expression subset. Publish column
and membership epochs atomically with committed transaction snapshots; use
conservative invalidation whenever before-images or dependency information are
incomplete. Test real binlog events, concurrent commits, DDL, restart, eviction,
and protocol result parity. A later experiment can test composing both ideas:
coalesce misses for one validity token and reuse completed results while that
token remains valid. That combination has not been measured here.

The engine has evolved after the archived base. Rebase any integration onto
current behavior and remeasure: these numbers belong to `73303d2` plus the
recorded standalone sources, not subsequent engine commits.

Raw corrected readings: [raw.jsonl](evidence/raw.jsonl),
[summary.json](evidence/summary.json), [environment](evidence/environment.json).
Earlier control and dependency readings: [raw.jsonl](evidence/settled/raw.jsonl),
[summary.json](evidence/settled/summary.json),
[environment](evidence/settled/environment.json). Both source-hash manifests
were checked against their corresponding preserved sources.

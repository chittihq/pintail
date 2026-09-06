# Production hardening todo

Opened 2026-09-06. The date-predicate rewrite in 0.1.2-rc4 did what it
promised: a reporting query that had taken tens of seconds now answers in
about a second. Running that release then surfaced three further failures
within ten minutes, none of which any gate could have caught. This list is
those failures, their fixes, and the test gaps that let them through.

The triggering workload, described generically because that is all the
engine needs to know: several multi-CTE reports, each joining about ten
tables with `LEFT JOIN` chains, grouping per entity with `COUNT(DISTINCT)`
aggregates, issued concurrently one per entity. Each ran 10-25 seconds.
Reproductions use invented schemas of the same shape.

Ordering rule: a production failure outranks the test that would have
caught it, but every fix lands with the gate that proves it, in the same
slice. No item is done until something fails when the product breaks.

## A. The three failures

Observed on a four-core container with a 12 GiB memory cap, a 1.5 GiB
per-query ceiling and a 9 GiB shared budget.

| # | Client error | Cause established |
|---|---|---|
| A1 | `HY000 ... aggregate spill create: Too many open files (os error 24)` | 1024 descriptor soft limit; a spilling grouped aggregation holds one run file open per run until its final merge |
| A2 | `ER_CON_COUNT_ERROR 1040 08004: too many concurrent queries` | admission saturated by concurrent long reports |
| A3 | `HY000 ... server memory limit exceeded: 9662624953 bytes used, 1052560 requested, 9663676416 limit` | the shared budget read full while the process held 325 MiB resident |

## B. Configuration the deployment set and the product ignored

- [x] **B1. The container shipped a desktop descriptor limit.** Docker's
  default soft limit is 1024. `docker-compose.yml` set no `ulimits`, so the
  shipped deployment ran a columnar engine under it. Fixed in `f394111`
  and recorded in `docs/limitations.md`. Reaches a deployment only on its
  next redeploy.
- [ ] **B2. `PINTAIL_MAX_CONCURRENT_QUERIES` never reached the server.**
  The compose file passes ten other `PINTAIL_*` keys through and silently
  dropped this one, so a deployment that configured it ran the default
  (`cores * 4`, minimum 16) instead. Every other knob is honoured; this one
  read as configured and was not. Pass it through, and add a compose/env
  consistency check so a dropped knob fails a gate rather than a dashboard.
- [ ] **B3. Nothing reported the settings actually in force.** A dropped
  environment variable is invisible today. Log one line at startup naming
  the effective admission limit, per-query ceiling, shared budget, process
  memory ceiling, descriptor soft and hard limits, and spill directory and
  its ceilings. That line makes B1 and B2 self-evident in the first minute.

## C. Engine: bound what a spilling query holds

Blocked on the design review requested 2026-09-06: bounded fan-in
multi-pass merge versus hash-partitioned spill; whether merging already
merged partial aggregate states stays associative; spill reservation
accounting across passes.

- [ ] **C1. Bound descriptors across every spilling operator.** Confirmed
  unbounded in `merge_spilled_aggregate_groups`, which opens every run at
  once. `sort.rs`, `join.rs` grace partitions and `two_pass.rs` are
  unaudited and assumed to share the shape until shown otherwise. Whatever
  design lands, the invariant is the same: peak open descriptors must be a
  constant, not a function of run or partition count.
- [ ] **C2. Replace the linear k-way merge scan with a heap** while that
  code is open, if it is free to do so.
- [ ] **C3. Understand the 30x reservation overestimate (A3).** The shared
  budget read within 0.02% of its ceiling across eight seconds of
  consecutive refusals while the process held 325 MiB resident. Refusals
  later stopped on their own, so the budget drains: this is
  over-reservation, not a permanent leak. Establish where a ten-way
  `LEFT JOIN` chain with no useful statistics inflates its estimate, and
  note that the arithmetic is structurally unsatisfiable regardless: the
  admission limit times the per-query ceiling can exceed the shared budget
  several times over, so under load the budget is always the binding
  constraint and refuses work the box could do. Either admission and the
  budget agree on a number, or the budget stops hard-refusing reservations
  the process is not actually holding.

## D. Test gaps that let all of this through

Approved by the owner 2026-09-06. Each converts a known-but-dismissed
observation into a gate.

- [ ] **D1. Assert a descriptor bound where spilling is already forced.**
  `docs/limitations.md` has recorded the "Too many open files" failure
  since before this incident and dismissed it as a tight-ceiling test
  artifact. The spill tests already force spilling and assert answers;
  nothing asserts resources. Add a peak open-descriptor measurement and
  assert it does not grow with run count. Open question: counting
  `/dev/fd` entries versus lowering `RLIMIT_NOFILE` inside the test
  process, and whether that needs a `libc` dependency the workspace does
  not currently have.
- [ ] **D2. Run one gate inside the shipped compose file.** Today
  `docker-compose.yml` gets `config --quiet`, `up --wait` and a curl of
  `/health`. Every functional gate launches the bare binary on the host, so
  the file that defines a deployment's resource envelope is never under
  test. Running an existing gate through the composed container catches
  both B1 and B2 by construction.
- [ ] **D3. Gate production-shaped SQL at production scale.**
  `tests/e2e/bi-dogfood.ts` and `tests/corpus/bi-captured` exist for
  exactly this and are in no profile. The oracle's generated families stop
  at three-table joins with no CTE chains; the E2E corpus is about 1,300
  rows; the benchmark runs eight hand-picked queries. Nothing runs many
  query shapes at scale, which is the same gap the rc4 date-predicate
  slowness fell through. Add invented-schema cases of the shape described
  at the top of this file, run them against a large replica under the
  shipped container's limits, and assert both answers and completion.

## E. Sequence

1. B2 and B3, with D2 as their gate. Small, and D2 proves both.
2. Redeploy so B1 takes effect.
3. C1 and C2 behind the design review, with D1 as their gate.
4. C3, which needs its own measurement before any change.
5. D3 last: it is the broadest and will surface more of the same class.

## F. Still open from earlier reviews

- The rename orphan left in `snapshotting` (owner decision 2026-09-05:
  retire it to the retained-data state at probe time).
- Capped exponential backoff with jitter for pending keyless copy retries.
- `docs/design/quality-performance-todo.md` item 4: the instruction gate
  never passed in CI until `3b12ba5`.
- A source whose binlog retention is shorter than a full snapshot takes
  will resnapshot in a loop. Unrelated to these three failures, and a
  deployment note rather than an engine defect.

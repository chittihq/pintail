# Performance experiments

## Live-update requirement — owner direction, 2026-09-08

A settled-only speedup does not demonstrate value for this MySQL accelerator.
Evaluate optimizations while the source changes: inserts, updates, key/group
changes, NULL transitions and deletes, including memtable tails, overlapping
segments, flushes, compaction and replay/restart boundaries.

Verify complete answers at the committed state and preserve pinned-snapshot
semantics. Charge query preparation and any index/cache build, invalidation,
repair, ingestion and maintenance work. Record query latency, update/cycle cost,
memory, and actual writer/query overlap. A faster query that transfers its cost
to updates is not automatically a system improvement.

Keep settled controls, but select candidates from changing-data measurements.
Distinguish external algorithm screens, actual SQL execution and native CDC
end-to-end measurements. Document untested boundaries and failures explicitly;
never claim a production breakthrough from a microbenchmark alone.

[Ten core workloads × ten approaches: results](core-engine-100/RESULTS.md)

## Execution-path evidence — owner direction, 2026-09-08

Before attributing a result to an optimization, prove the harness enables it at
its required lifecycle point and record actual path counters. Treat zero phase
time or zero optimized slices as a possible path mismatch, not free work.
Include an explicit fallback control and fixtures that actually carry each
claimed key type. An integer fixture with opt-in omitted is not evidence of a
text-key workload's performance. Preserve update, snapshot and maintenance
correctness checks when exploring a replacement.

[Controlled join proof](f2-proof/RESULTS.md) · [Typed-key overlay investigation](overlay-proof/README.md)

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

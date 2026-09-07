# Correctness and integration findings

## F1: replay after a full compaction has no retained deletion marker

The first changing-data runner failed at its post-compaction stale-replay phase.
A two-row-event reproducer isolates it:

1. Snapshot a key at version 1.
2. Delete it at version 2 and flush.
3. Optionally compact the complete manifest.
4. Submit version 1 again directly through `TableStore::ingest_cdc`.

Without compaction, the key stays deleted. With full compaction, it reappears.
`src/bin/replay_contract.rs` prints the two outcomes and is kept runnable.
The current store explicitly documents that full compaction drops tombstones.
This is a demonstrated **store API replay boundary**, not proof that the native
CDC checkpoint/reconnect path can deliver such an event. CDC reachability still
needs a separate investigation. No production fix is claimed or made here.

The valid-state performance trajectory therefore replays stale/duplicate events
**before** compaction retires their deletion markers, and records compaction and
restart afterward. The adversarial post-retirement case is retained separately,
not silently passed or used to produce performance numbers on incorrect data.
This uncovered boundary remains an open result of the 100-experiment program.


## F2: SQL resource boundaries and join shape at 256 MiB

At 100,000 invented rows, the ordinary SQL anchors produced 200 exact answers and
40 memory refusals across three distributions and eight changing states. The
join refused in 16 of 24 states (uniform and hot-key distributions), and nullable
membership refused in all 24. The guard returned an explicit error, never a
wrong answer. These are results for this harness and cap, not a claim about every
deployment's configured ceiling. See `engine-evidence/` for every outcome.

Moving the dimension filter and projection into a derived table allowed the join
to return all 24 exact answers at the same cap. Factoring the fact-side aggregate
before the join also returned all 24 exact answers, and improved the hot-key
query timings while slightly regressing the other distributions. This experiment
does not isolate the contribution of filtering versus projection versus plan
shape, and does not install an optimizer rule. `engine.py --prefilter-join` and
`--factorized-join` reproduce those two SQL variants against changing snapshots.

The native SQL checks run between mutation batches. The independently measured
factorized prototype supplies the concurrent writer and full-cycle comparison.
The ordinary SQL and factorized SQL were checked against transactional MySQL 8.4
on the smaller invented fixture (240 + 24 byte-exact checks). No native CDC
transport, production memory configuration, or arbitrary-type rewrite is certified.

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

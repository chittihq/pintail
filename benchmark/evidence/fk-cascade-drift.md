# Foreign-key action drift: how far, how long, and what the repair costs

MySQL applies `ON DELETE/UPDATE CASCADE` and `SET NULL` inside InnoDB and
writes no row events for the child rows they change. Pintail flags those
child tables and repairs them with a cascade reconciliation pass every
`reconcile_interval_seconds` (600 by default). This is the measurement
issue #29 asked for before choosing between a targeted repair and a
shorter interval for the flagged tables.

Host: 8-vCPU cloud host (AMD Ryzen 9 9950X), 16 GB. MySQL 8.4 in a
container on the same host (row binlog, GTID). Pintail release build of
this branch, production supervisor cadence (5 s), info log.

## Method

Two instruments, both in the repository:

- **MySQL's own tests through change capture.**
  `MTR_MODE=replica MTR_CASCADE=1 bun run tests/mtr/run.ts`, once for the
  main suite and once with `MTR_SUITE=mysql-innodb` (the storage engine's
  suite, same pinned commit). It replays every file that declares
  `ON DELETE/UPDATE CASCADE` or `SET NULL` (17 main, 28 engine files) with
  the cascade pass effectively off. A compared SELECT on a table such a key
  acts on that differs right after catch-up forces one scheduled pass and
  is compared again.
- **A drift harness on an invented schema.** `tests/e2e/fk-cascade-drift.ts`:
  `vendor -> catalog_item -> item_tag` (two-level `ON DELETE CASCADE`, the
  first level also `ON UPDATE CASCADE`) and `depot -> stock_slot` on a
  two-column key (`ON DELETE SET NULL ON UPDATE CASCADE`). Four writers
  insert, delete and re-key parents and update children (about 160 child
  rows a second changed by an invisible action). Every 10 s of writing it
  diffs both sides as they stand, then pauses the writers, waits for a
  marker row and diffs again: what still differs after catch-up is the
  invisible action alone. A cost phase times one pass on an idle server
  from the server's own log line (`cascade reconcile ... elapsed_ms=`), with
  process CPU and peak RSS read from `/proc`; the 10,000,000-item size ran
  single-level only (`DRIFT_COST_TAGS=0`), since its million tags would
  have taken about half an hour a pass. A checks phase runs shapes the
  repair must get right.

## Results

### MySQL's tests (101 compared SELECTs on cascade-affected tables)

| Outcome | Engine suite | Main suite |
|---|---:|---:|
| Exact right after catch-up | 88 | 6 |
| Stale until one pass, then exact | 2 | 0 |
| Still wrong after the pass | 2 | 0 |
| Pass never completed | 2 | 1 |

Most of the suite's FK statements do not reach a compared SELECT on a
changed child (errors expected by the test, debug-only hooks, restarts).
The five that do not converge are all repair defects, not latency:

- **wrong** (`cascade_lock_wait`): `ON UPDATE CASCADE` into a child whose
  primary key is the referencing column. The pass sees the child's old key
  without a parent, asks the source for that key, finds it gone and
  tombstones it - and never fetches the row under its new key. The child
  loses the row for good.
- **not run** (`innodb-index`, `virtual_fk`, main `foreign_key`): the pass
  failed every time it was due. The refusals logged over both runs: a child
  table without a primary or unique key, "has no source key for CDC
  reconciliation" (364 times); a flagged table not among the opened targets
  (4); a referencing column absent from the replica after an ALTER (1).
  `run_cdc_reconciliation` fails the whole scope on its first error, so a
  keyless flagged child also stops the repair of the keyed tables flagged
  beside it, and since no pass completes, the attempt repeats every cycle.

### Drift under concurrent writers, by interval

| Interval | Stale child rows after catch-up: mean / max | Stale-row lifetime p50 / p90 / max | Replica reads AHEAD (rows, mean) | Reads BEHIND (rows, mean) | Catch-up median / max |
|---:|---:|---:|---:|---:|---:|
| 600 s | 41,348 / 90,156 | 375 / 580 / 640 s | 18,780 | 17 | 4.7 / 49.7 s |
| 60 s | 3,649 / 5,612 | 70 / 100 / 170 s | 1,160 | 77 | 4.7 / 44.8 s |
| 15 s | 2,560 / 3,376 | 70 / 125 / 125 s | 1,104 | 414 | 34.9 / 44.7 s |

600 s ran 16 minutes; 60 s and 15 s ran 3 minutes each, so their maxima
rest on fewer samples (9 and 5). "Ahead" and "behind" are catalog item rows
in the unpaused diff: an unrepaired cascade reads ahead of the source by
two orders of magnitude more than lag reads behind it. After catch-up
nothing ever read behind (0 rows in every run): every difference left was
an invisible action.

The interval is not the floor. One pass over the 62,000-row `item_tag`
took 42-53 s in the 60 s and 15 s runs, against 0.2-0.5 s for the
80,000-125,000-row `catalog_item`. At 15 s passes run back to back, so the
lifetime of a stale row is the pass length plus a cycle, the same 70 s
median as at 60 s.

The pass runs inside the database's replication cycle. While it runs no
row event is applied to ANY table of that database: the samples that fell
on a pass waited 35-50 s to catch up instead of the 4.7 s cadence. At 15 s
that is most samples.

### Cost of one pass, idle server

| Child rows (single level: `catalog_item`, parent `vendor`) | Parent rows | Pass |
|---:|---:|---:|
| 100,000 | 1,000 | 0.13 s |
| 1,000,000 | 10,000 | 8.5 s |
| 10,000,000 | 100,000 | 135 s |

| Child rows (second level: `item_tag`, parent `catalog_item`) | Parent rows | Pass |
|---:|---:|---:|
| 10,000 | 100,000 | 16.4 s |
| 100,000 | 1,000,000 | 172 s |

Whole pass over all flagged tables (the unit the supervisor schedules),
with 1% of the vendors deleted beforehand and then with nothing to repair:

| Items | Repairs 1% | Nothing to repair | Process CPU | Peak RSS growth |
|---:|---:|---:|---:|---:|
| 100,000 (+10,000 tags) | 16.7 s | 16.5 s | 17.1 CPU-s | +21 MB |
| 1,000,000 (+100,000 tags) | 181 s | 178 s | 195 CPU-s | +28 MB |
| 10,000,000 (no tags) | 135.5 s | 134.5 s | 139-143 CPU-s | +164 to +207 MB |

The pass costs the same whether it repairs anything or not, uses one core
for its whole length, and holds little memory. Its time is the number of
distinct parent keys the children reference times one point lookup in the
parent replica (repeats hit a presence cache): about 0.13 ms a lookup in a
1,000-row parent, 0.85 ms at 10,000 rows, 1.35 ms at 100,000 and 1.7 ms at
1,000,000. So ten million children of 100,000 vendors cost 135 s, and
100,000 tags of 100,000 distinct items cost 172 s. Scanning the child is
cheap by comparison, and so is verifying candidates against the source:
52,000 candidates were verified in a pass of under 0.5 s.

### Shapes the repair must get right (checks phase)

| Shape | After one pass | After two |
|---|---|---|
| `SET foreign_key_checks = 0`, then delete parents (no cascade at the source) | exact - orphans kept | exact |
| Orphans through later passes | exact (re-verified each pass) | exact |
| Plain cascade, both levels, and `SET NULL` | exact | exact |
| Parent deleted and inserted again with the same key before a pass | **wrong**: 20 items, 4 tags kept | **wrong** |
| `REPLACE` on a parent (InnoDB deletes and cascades, then inserts) | **wrong**: 20 items, 4 tags kept | **wrong** |
| Two-column parent key moved by `ON UPDATE CASCADE`, then reused | **wrong**: 10 slots keep the old key | **wrong** |
| Two-column parent key moved, not reused | exact | exact |

The current repair finds a damaged child only by its parent being absent
from the replica. Whenever the parent key exists again - reinserted,
replaced, or reused after a move - the damaged children are never
candidates, and no later pass fixes them. `REPLACE` does this in a single
statement, so no interval is short enough. The `foreign_key_checks = 0`
case the issue worried about is handled correctly today, because the source
arbitrates every candidate.

## Recommendation

A shorter interval on the flagged tables buys most of the latency, but only
for flagged tables whose parents are small, and it does nothing for the
wrong-forever shapes. Measured here: 600 s to 60 s cut stale rows elevenfold
(41,348 to 3,649 mean) and the median stale lifetime from 375 s to 70 s;
15 s bought almost nothing more, because the pass itself took 45 s.

Options, in the order they pay back:

1. **Fix the repair's correctness first, whatever the interval.**
   (a) Fail per table, not per scope: a keyless or unopened flagged table
   must not stop its siblings' repair. (b) For `ON UPDATE CASCADE` into
   child key columns, fetch the source rows by the new parent key instead
   of only tombstoning the old one. (c) Parent key reuse and `REPLACE`: the
   parent-absent heuristic cannot see them; candidates have to come from
   the parent's captured delete and update events (the issue's step 1) or
   from a periodic full compare of flagged tables.
2. **Make the pass cheap and keep it off the replication cycle**, then
   shorten. The pass is bound by one point lookup per distinct parent key
   (1.35-1.7 ms each in a parent of 100,000 rows or more) and blocks every
   table's CDC while it runs. Looking parents up in sorted batches, or
   merging the child's and the parent's key streams, should take the 172 s
   pass over 100,000 second-level rows toward the cost of a scan. Only then is a per-table interval of
   15-60 s safe; until then, an interval shorter than a few times the last
   pass's duration only converts cascade staleness into replication lag
   for the whole database.
3. **Per-flagged-table interval, adaptive.** Once 2 lands, run flagged
   tables at max(30 s, 4 x last pass) instead of the database's 600 s.
   On this workload that keeps stale rows near the 60 s figures above.
4. **The targeted repair the issue proposes** closes the remaining
   seconds-versus-tens-of-seconds gap and, built from parent events, also
   fixes 1(c). Worth it only if the latency left after 2 and 3 matters.

Raw results (per-sample series, per-pass log lines) are written by the
harness to `validate-out/fk-cascade-drift/<run>/` and are not committed.

## After: repair from parent events, planned beside the stream

Options 1(a)-(c), 2 and 3 above, implemented (`docs/decisions.md`, "Cascade
repair follows the parent's events, beside the stream"). The stream queues
the parent keys it deletes or moves per flagged child; a pass plans its
candidates from them on a blocking thread beside the stream, and applies
the repairs inside a later cycle. Flagged tables run at max(30 s, 4 x the
last pass), capped by `reconcile_interval_seconds`. Same host and harness;
the harness gained the shapes below and reports the part of each pass the
replication cycle waited for (`apply_ms`).

### Shapes (checks phase)

Every shape is exact after one pass and after a second:

| Shape | Before | After one pass |
|---|---|---|
| `foreign_key_checks = 0` delete, orphans through later passes | exact | exact |
| Plain cascade, both levels, and `SET NULL` | exact | exact |
| Parent deleted and inserted again with the same key | wrong | exact |
| `REPLACE` on a parent, and through two levels | wrong | exact |
| Two-column parent key moved then reused | wrong | exact |
| `ON UPDATE CASCADE` into a child's primary key | wrong | exact |
| The same, then the old parent key reused | - | exact |
| A child without a key beside keyed ones | pass failed for all | keyed exact in the pass; keyless recopied (`keyless_policy` `auto_resync`) |

### MySQL's tests (the same 101 compared SELECTs)

| Outcome | Engine suite before / after | Main suite before / after |
|---|---:|---:|
| Exact right after catch-up | 88 / 89 | 6 / 7 |
| Stale until one pass, then exact | 2 / 5 | 0 / 0 |
| Still wrong after the pass | 2 / 0 | 0 / 0 |
| Pass never completed | 2 / 0 | 1 / 0 |

### Drift under concurrent writers

`reconcile_interval_seconds` 600, which now caps the flagged tables'
adaptive interval rather than setting it; their passes ran every 30 s.
16 minutes, 65 samples, the same writer mix.

| | Before (600 s) | Before (60 s) | After (600 s cap, adaptive) |
|---|---:|---:|---:|
| Stale child rows after catch-up: mean / max | 41,348 / 90,156 | 3,649 / 5,612 | 3,148 / 7,864 |
| Stale-row lifetime p50 / p90 / max | 375 / 580 / 640 s | 70 / 100 / 170 s | 25 / 30 / 75 s |
| Catch-up median / max | 4.7 / 49.7 s | 4.7 / 44.8 s | 4.7 / 5.4 s |
| One pass over all three children | - | 42-53 s | 82-153 ms (697 ms the first, which sweeps) |
| ... of which the replication cycle waited | all of it | all of it | 23-88 ms (632 ms the first) |

Nothing read behind after catch-up (0 rows). No sample waited longer than
5.4 s to catch up. What staleness remains is the 30-second floor of the
interval, not the pass.

### Cost of one pass, idle server

100 items per vendor, a tenth of them tagged, as in the first table.
"Work" is planning plus applying; "in the cycle" is the part replication
waited for.

| Items | First pass (sweep): work / in the cycle | Repairs 1%: work / in the cycle | Nothing queued |
|---:|---:|---:|---:|
| 100,000 (+10,000 tags) | 39 / 11 ms | 23 / 9 ms | 1 ms |
| 1,000,000 (+100,000 tags) | 323 / 126 ms | 267 / 48 ms | 0 ms |
| 10,000,000 (no tags) | 1,259 / 2 ms | 1,482 / 175 ms | 0 ms |

Before, the same passes took 16.5-16.7 s, 178-181 s and 134.5-135.5 s,
all of it inside the cycle, whether or not anything was repaired. A pass
with nothing queued reads no rows. At 10,000,000 items the 1% repair
(100,000 tombstones) used 1.4 CPU-s and 180 MB of peak RSS growth, against
139 CPU-s before.

### What a pass still does not repair

- A parent change the stream did not follow (the parent quarantined,
  paused or being copied, or the change older than the queue) is found only
  by a sweep, which sees absent parents, not reused keys.
- A child without a key is recopied, not repaired in place, and only under
  the `auto_resync` keyless policy; otherwise it stays quarantined.
- A cascading key on something other than a replicated parent's primary
  key still takes the full compare, in the cycle, at the database's
  interval.

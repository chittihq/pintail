# Reconciliation at scale

Measured 2026-09-02 with the e2e `reconcile-memory` phase run alone at ten
times the gate's size. The server ran on the development Mac; the source
was a fresh `mysql:8.4` container on the docker host with a 12 GB tmpfs.

```sh
cd tests/e2e
E2E_PHASES=reconcile-memory E2E_RECONCILE_CHILDREN=20000000 \
  E2E_RECONCILE_MARGIN_MB=1024 PINTAIL_E2E_MYSQL_TMPFS_GB=12 \
  [E2E_RECONCILE_VIA=supervisor] [E2E_RECONCILE_DELETE_EVERY=100] bun run run.ts
```

The child table has an `INT` key, an `INT` foreign key with `ON DELETE
CASCADE`, and a 40-byte payload: about 100 bytes a row in `InnoDB`, so
twenty million rows are roughly 2 GB of source data.

| Run | Rows | Cascaded | Pass | Repair | Peak RSS | Baseline |
|---|---|---|---|---|---|---|
| gate (banked) | 2,000,000 | 200,000 | operator's full compare | 342.5 s | 183 MB | 29 MB |
| scale | 20,000,000 | 2,000,000 | operator's full compare | 195.0 s | 440 MB | 23 MB |
| scale | 20,000,000 | 2,000,000 | supervisor's targeted pass | 63.8 s | 416 MB | 30 MB |
| scale | 20,000,000 | 200,000 | supervisor's targeted pass | 29.3 s | 200 MB | 37 MB |

"Repair" is wall-clock from the parent deletes to the replica matching the
source; the supervisor rows include waiting for its next five-second
cycle. The gate row predates the single-stream merge and the plain-ingest
repair (commits b199dfb and 630d6dd); the first attempt at twenty million
rows with the per-page reopen and the scan ingest had not converged after
34 minutes. Seeding and snapshotting the twenty million rows took under
five minutes of each run.

Two bugs surfaced on the way and are fixed in the same series: a
compacted segment larger than the scan budget was refused instead of
sliced (8702b15, 05ad1dc), and creating a database dropped the reconcile
interval it was given, so the supervisor's second pass waited the default
ten minutes and every early measurement read as six hundred seconds
(da492ef).

## What the numbers say

- Memory is flat in the table size. Both passes hold one streamed chunk
  plus one source page or one candidate slice, and the peak barely moves
  between two and twenty million rows. The 0.1.0 code peaked at 1,089 MB
  over a 192 MB baseline on the two-million-row gate alone.
- The full compare is bound by reading the source once, in pages, and
  the replica once, in a stream: about 100,000 rows a second here. A
  100 GB table of these rows, a billion of them, would take around three
  hours; wider rows cost proportionally more transfer.
- The targeted pass costs one local scan of the child's key and
  foreign-key columns plus a verification query per five thousand
  candidates, so it scales with the cascaded rows rather than the table:
  ten deleted parents over twenty million rows repair in half a minute,
  and cascading a tenth of the table, its worst case, in about a minute.
  On a 100 GB child of these rows the scan alone is the floor, around
  twenty-five minutes at this rate, every reconcile interval that finds
  the table flagged; the source is only asked about the affected rows.

## Limits of this measurement

The docker host had 16 GB of disk free at the time and the staging host
carries other staging services, so a synthetic 100 GB table was not
generated on either. The extrapolations above are linear in rows from
the twenty-million-row runs; the snapshot of such a table is bound by
the link to the source, and a timed run needs a host with the disk to
hold it.

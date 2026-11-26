# Memtable copy-on-write: chunked rows

Measured 2026-11-27 on an 8-vCPU box (AMD EPYC, KVM), release builds, each
arm built in its own target directory, runs interleaved with the order
rotating per round.

## What a pinned write cost

The writer's memtable held its rows as one `Arc<BTreeMap>`. A snapshot of
the writer shares that map; the next write while the snapshot is alive
copies every row (`Arc::make_mut`) before it changes one. A held snapshot
pays at most one copy for the life of the pin, but that copy is the whole
memtable, up to the 64 MiB flush threshold, and it doubles the memtable's
memory until the snapshot is dropped.

The SQL engine does not share the writer's map: a reader opens its own
snapshot and replays the log into its own memtable. In-process holders that
keep a writer snapshot across a write are:

- cascade planning, which pins a snapshot of every due child table and the
  tables they reference before a cycle's stream runs, and plans on a
  blocking thread while the stream applies events (at most one pass per
  database every 30 s, only for tables with cascading foreign keys);
- reconciliation and cascade repair, which hold a snapshot while they
  ingest repairs (only when the replica diverged from the source).

Write-path statements that take a snapshot (`INSERT` key checks, version
reads, narrowing proofs, cascade apply's plan) drop it before they write.
So the copy is rare: at most one per pinned table per cascade pass or
repair. Its cost when it happens is the table below.

## Probe

`cargo run --release --example memtable_cow_probe -- --fill-mb 60 --batch N`:
a five-column table (two integers, two short strings, a key) filled in
4,096-row batches of ascending keys to 63,249,919 estimated bytes (253,952
rows), then 100 batches of N updates to random existing keys, applied with
no snapshot held (`unpinned`, before and after the pinned phases), with a
fresh snapshot pinned across every batch (`pin-each`), and with one
snapshot pinned across all 100 (`pin-once`). The time is the
`ingest_cdc` call alone (log append included, log sync off). Six rounds;
each cell is the median over rounds of the per-run median, with the range of
per-run medians, and the median over rounds of the per-run maximum.

| batch | mode | before (us) | after (us) |
|---:|---|---|---|
| 1 | pin-each | 60,008 [58,861-60,903] | 76 [72-94] |
| 1 | pin-once, worst batch | 30,014 | 413 |
| 64 | pin-each | 31,760 [31,369-32,163] | 2,020 [1,903-2,083] |
| 64 | pin-once, worst batch | 32,946 | 3,455 |
| 1,024 | pin-each | 48,798 [47,041-52,561] | 20,294 [19,732-21,116] |
| 1,024 | pin-once, worst batch | 61,511 | 24,509 |
| 1 | unpinned, first | 8 [8-10] | 12 [11-16] |
| 1 | unpinned, last | 14 [12-18] | 10 [7-13] |
| 64 | unpinned, first | 78 [70-81] | 87 [84-90] |
| 64 | unpinned, last | 72 [65-81] | 80 [76-87] |
| 1,024 | unpinned, first | 676 [629-836] | 802 [756-911] |
| 1,024 | unpinned, last | 925 [892-968] | 846 [819-879] |

The ascending-key fill (62 batches of 4,096 rows): 84.6 ms median before,
83.4 ms after (minimum 78.3 and 74.4). Counted with `perf stat` over the
fill alone (process start and exit included), 1,378.9 M instructions
before and 1,230.3 M after; cycles 387-436 M before and 386-436 M after
over six runs each.

A pinned write now copies the list of chunk heads (one key and one `Arc`
per 256-512 rows) and each chunk it lands in, so its cost follows the
number of chunks the batch touches: about 800x less for one row, 16x for 64
scattered rows, 2.4x for 1,024 scattered rows. `pin-once` spreads the copy
over the batches that first touch each chunk instead of paying it all on
the first one, which is why its median rises while its worst batch falls.

Unpinned writes to random existing keys read 12-19% slower in the state
right after the ascending fill and 9% faster after the pinned phases; the
before arm itself moves 37% between those two states, so the probe does not
settle it. Ascending-key appends, the common shape of a change stream, are
level.

## Chunk size

Chunks of 128, 256 and 512 rows (median us per `pin-each` batch, four
rounds): one row 74 / 71 / 94, 64 rows 992 / 2,010 / 4,290, 1,024 rows
16,770 / 20,710 / 25,115. Smaller chunks copy less per touched chunk but a
longer head list per pin, and `pin-once` at 128 rows rose to 490 us per
64-row batch. 256 was kept.

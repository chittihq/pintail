# Typed-key overlay proof

An optimization benchmark must first establish that the measured path ran.
This fixture calls `enable_memtable_overlay` before the first chunk, records
eligibility, and asserts overlay-slice versus merge-part counters. It includes
an explicit omitted-opt-in control. Counters are injected temporarily by
`run.py`; original storage source is restored in `finally`. The production
engine is not modified by this experiment.

The paired fixtures have 100,000 initial rows, integer or fixed-width UTF-8
primary keys, one or four projected columns, and 0/1/1,000/20,000 changed keys.
Changes use `ingest_cdc` and checkpoint, including tombstones every nineteenth
changed key. The one-change case is a delete. Four scans per configuration
include a warm-up; every projected value and row order is checked outside the
timer. These are finite pending-update snapshots, **not continuous replication
or concurrent reader/writer evidence**. Decimal, temporal and composite keys
are not measured. Text uses synthetic binary ordering, not SQL collation tests.

Run only on the build host with no other measurements or builds active:
`python3 experiments/overlay-proof/run.py`. The runner compiles an experimental
binary using temporary counters in the real storage implementation. It writes
raw timings, path counts and source/binary hashes. Do not run a production gate
against the temporarily instrumented tree. An interruption that bypasses Python
cleanup requires restoring the original scan source before continuing.

## Candidate directions after confirming the boundary

The most useful abstraction to investigate is a snapshot-owned map from changed
storage keys to immutable segment positions. Key comparison should retain the
storage key's exact semantics; scans should consume a position mask and ordered
replacement rows. Construction, invalidation and retention costs belong in the
measurement. This is a design hypothesis, not a demonstrated speedup.

| Approach | Question the experiment must answer |
|---|---|
| 1. Typed overlay key comparison | Can existing masking support UTF-8/binary keys without materializing every row? |
| 2. Sparse-index lookup per changed key | Does locating only affected blocks make sparse updates proportional to change count? |
| 3. Block-local sorted merge | Where is the crossover between probing keys and merging a block's changed keys? |
| 4. Snapshot-owned position mask | Can repeated scans amortize discovery without leaking new changes into pinned snapshots? |
| 5. CDC-maintained position map | Does charging maintenance to writes still improve query-plus-ingestion throughput? |
| 6. Adaptive dense masks | When should changed positions use a bitmap instead of a sorted vector? |
| 7. Segment-local key ordinals | Can stable local ordinals remove repeated string comparisons while surviving compaction? |
| 8. Hash lookup with full-key verification | Can collision-safe probes beat ordering searches within an affected block? |
| 9. Affected-block materialization | Can rebuilding only dirty blocks bound memory without penalizing narrow projections? |
| 10. Adaptive merge/overlay choice | Can observed changed-key density and key width select a plan without repeated preparation? |

All candidates need fresh fixtures with varying table size, text length, composite
keys, key changes, inserts, deletes, replay, NULL policy where applicable,
overlapping segments, flush, compaction and pinned/restarted snapshots. Repeated
query readers must race sustained writers, reporting lag, writer latency, query
latency, memory and total cycle cost. A table-size sweep is necessary before
claiming change-proportional complexity; one table size cannot establish it.

## Measured result

All 128 scans returned exact projected values in storage-key order. Each table
below uses the median of three warm scans after one warm-up, on the same idle
build machine with four scan workers and CPU affinity 0–7. Results are a small
screen at one table size, without randomized ordering or confidence intervals.

| Key | Projected columns | Settled | 1 changed | 1,000 changed | 20,000 changed |
|---|---:|---:|---:|---:|---:|
| Integer | 1 | 1.48 ms | 1.63 ms | 1.88 ms | 2.21 ms |
| Integer | 4 | 1.90 ms | 2.21 ms | 3.18 ms | 3.60 ms |
| UTF-8 | 1 | 1.48 ms | 16.62 ms | 16.71 ms | 22.34 ms |
| UTF-8 | 4 | 3.08 ms | 35.44 ms | 35.56 ms | 38.23 ms |

These are the opt-in arms. Every dirty integer scan recorded one overlay slice
and zero merge parts. Every dirty UTF-8 scan recorded zero overlay slices and
one merge part despite opt-in. Omitted opt-in recorded zero overlay slices and
one merge part for both key types. Settled scans recorded neither path.

The actual text-key penalty is 11.2–15.1× for one column and 11.5–12.4× for four
columns versus each fixture's own settled baseline. This independently confirms
the eligibility boundary and a material cost on these fixtures. It does not
confirm a universal 63×/84× penalty, a completely flat curve, or scaling only
with changed rows. The integer fallback control must never be described as the
production integer scan path.

Timing includes stream construction, opt-in, decoding and retaining returned
chunks. It excludes fixture load, change ingestion/checkpoint and subsequent
value validation. These exclusions are why this is a boundary confirmation,
not a measured system improvement. No alternative overlay has been implemented
or selected yet; the candidate table above specifies the next investigations.

# Projected block addressing and predicate reuse

> Historical measurements before the qualification rebase. See
> [the rebased qualification](storage-scan-qualification.md) for the adversarial
> fixture, complete rc gate, and controlled performance comparison.

This experiment compares baseline `40486955` against the storage changes in
`2b8175a8` and `d421aba9`. The benchmark programs are identical in both
binaries. All fixtures are invented.

The first change caches an immutable segment's block offsets in a bounded,
process-local metadata directory. Projected reads seek to selected payloads
instead of traversing every on-disk block header for each scan slice. The
cache uses file identity and schema generation; payload checksums remain
mandatory when decoding. The on-disk format is unchanged.

The second change extends the existing all-predicate integer buffer reuse to
all decoded column representations, including text arenas and dictionary
codes. When the predicate and output projections are identical, selection
compacts the existing buffers instead of decoding the output again. Mixed
projections with additional output columns retain their previous behavior.

## Method

- Linux x86_64 build machine, release optimization, local segment files.
- One persisted segment: 524,288 rows, 23 unsigned integer columns and one
  eight-value text column. Both binaries read the very same files.
- Three independent processes per variant, alternating baseline/candidate
  order, with seven measured iterations after two warmups per case.
- Warm OS page cache. These numbers do not establish cold-disk performance.
- The scan probe checks every output value in its unmeasured warmup. The SQL
  probe checks scalar answers against independently calculated expectations.
- `PINTAIL_DISABLE_SETTLED_MEMO=1` is required for the SQL probe, and every
  query asserts that physical blocks were decoded. Result memo hits are
  excluded from the experiment.
- SQL measurements include parse, bind, optimization, physical planning and
  execution. They exclude HTTP/wire serialization and network latency.
- The selective storage case retains 256 rows in each 4,096-row interval:
  6.25% of rows, scattered across every block. It deliberately cannot win by
  skipping payload blocks. The actual SQL cases use ordinary predicates.

An initial scan measurement made in the seeding process was discarded:
seeding changes allocator state, so comparing it with a fresh candidate
process gave a misleading wide-scan regression. The reported comparison
uses fresh processes and the persisted fixture for both variants.

## Results

Each entry below is the median of the three process medians, in milliseconds.
The adjacent JSON file retains every process median and minimum.

| Workload | Baseline ms | Candidate ms | Ratio |
|---|---:|---:|---:|
| Narrow projected scan | 1.366 | 0.868 | 1.57x |
| Wide projected scan | 38.095 | 33.531 | 1.14x |
| Text predicate, all rows retained | 2.045 | 0.466 | 4.39x |
| Text predicate, 6.25% retained | 2.531 | 0.466 | 5.43x |
| Mixed projection, 6.25% retained | 4.413 | 3.346 | 1.32x |
| SQL numeric filter + count | 1.624 | 1.182 | 1.37x |
| SQL text equality + count | 1.150 | 0.987 | 1.17x |
| SQL text inequality + count | 1.455 | 1.226 | 1.19x |
| SQL wide arithmetic + sum | 427.290 | 414.785 | 1.03x |

The text-only storage cases decode **32 blocks instead of 64**. The mixed
projection still decodes 96 blocks in both variants: its gain comes from
addressing, not predicate-buffer reuse. Narrow and wide unfiltered scans
also decode the same payload block counts as before.

Wide scans varied substantially between processes (baseline 34.430–40.769 ms,
candidate 33.525–39.502 ms); no reliable wide-scan improvement is established.
The wide arithmetic query likewise stays within noise. SQL text-filter gains
are smaller and noisier than the isolated scan gains. These measurements
support reduced storage work and a numeric-filter SQL improvement on this
fixture, not a universal query speedup.

The storage suite and touched-crate clippy pass for both implementation
slices. The predicate-reuse regression covers nullable signed/unsigned
integers, floats, dictionary text, variable-width text, native dates,
booleans and binary values, with whole and sliced scans. It checks exact
values for all/empty/disjoint selections and asserts one decode per block.
The directory regression checks reuse and invalidation on file changes.

## Final validation

The complete `development` profile passed on clean commit `46a9780c`:
`bun run scripts/validate.ts --profile development` (run
`2026-09-09T16-41-44-526Z-development`). This includes workspace clippy with
warnings denied, formatting and README checks, dashboard typechecking,
workspace unit/integration tests (1,033 passed, 43 skipped), and the parser
corpus. The build used
rustc/cargo 1.97.0 and bun 1.3.14. The full Pintail executable also built
successfully under the recovery profile.

A dedicated disposable MySQL 8.4 source and a fresh Pintail process passed
14 additional differential queries through the real HTTP query and replica
loading path: six after snapshot and eight after ADD COLUMN, INSERT, UPDATE
and DELETE. The invented source had 100,000 rows with nullable text, decimal
and datetime values. Checks included counts, filtered aggregates, grouped
counts and exact selected rows, including the newly added column. The source
container, Pintail process and tunnel were cleaned up afterwards.

This is a complete development-profile validation plus a targeted live
replication check. It is not an rc/stable release gate or the full analytical
benchmark, and makes no claim about cold-cache I/O or unrelated query shapes.

The [20-million-row follow-up](storage-scan-20m.md) records the same comparison
at larger row counts, including its shared-server timing limitations.

## Reproduce

Build both examples at each revision, retaining the resulting binaries under
different names. Copy the same benchmark source files into the baseline
checkout before building; only the engine implementation should differ.

```sh
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --release \
  -p pintail-store --example storage_scan_probe \
  -p pintail-exec --example storage_query_probe
./target/release/examples/storage_scan_probe /tmp/storage-probe-data 524288
PINTAIL_DISABLE_SETTLED_MEMO=1 \
  ./target/release/examples/storage_query_probe /tmp/storage-probe-data 524288
```

The first scan invocation seeds the directory when it is empty. Discard that
process's timings and rerun from fresh processes, alternating binaries.

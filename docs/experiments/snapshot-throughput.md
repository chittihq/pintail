# Snapshot throughput experiment

An isolated experiment based on commit `d267f05fead0af30d50a7f16162d3088c329be68`.
No deployment or production dataset was used. All edits, builds, MySQL queries,
and measurements ran on one Linux machine with 16 logical CPUs, about 61 GiB RAM,
and NVMe storage. Rust 1.97.0, release profile, MySQL 8.4.11, 4 GiB source buffer pool.
The source connection is loopback; this does not measure a remote deployment link.

## Findings

Three repetitions per case, alternating case order. Each case copies one million
synthetic rows. The row has an unsigned key, grouping key, decimal, datetime,
512-character payload derived from eight different hashes, binary token, ENUM,
and nullable text. Neither caches nor the source buffer pool were flushed.
Timings cover snapshot coordination through durable chunk publication, excluding
setup, source probing and subsequent verification. CDC catch-up is not timed.

| Case | Median seconds | Comparison |
| --- | ---: | --- |
| One table, existing path | 6.778 | Reference for range experiment |
| Four physical tables, existing workers | 6.445 | Reference for worker experiment |
| Four physical tables, separate worker threads | 2.220 | 2.90x faster |
| Four disjoint ranges of one physical table, separate threads | 2.392 | 2.83x faster than the one-table reference |
| Composite key, tuple predicate | 23.575 | Reference for pagination experiment |
| Composite key, expanded predicate | 6.766 | 3.48x faster |

The first four cases use the default 100,000-row chunks. The composite cases use
10,000-row chunks to exercise 100 successive pages on this modest dataset.
The latter speedup is not a measurement of default chunk sizing on a 200 GB source.
The gains must not be multiplied: these are independent experiments.

An individual late-page `EXPLAIN ANALYZE` gives the causal evidence for pagination:
`(bucket,id) > (900,0)` read 910,000 rows to produce 10,000, taking about 331 ms.
`bucket > 900 OR (bucket = 900 AND id > 0)` read 10,000 rows and took about 4.23 ms.
These are invented identifiers. The existing tuple predicate caused an index scan
from the beginning; the equivalent expanded predicate enabled an index range scan.
Successive pages can therefore repeatedly visit previously copied rows. This
mechanism merits checking on slow composite-key sources, but it does not establish
that it caused any particular deployment's delay.

In the initial one-table baseline, accumulated fetch-await time was 0.635 seconds,
conversion 0.622 seconds, and bulk writing 5.670 seconds over 6.948 seconds total.
Fetch-await includes client scheduling and protocol work; it is not pure network
latency. For this local workload, eliminating all non-overlapped fetch time would
save only about nine percent. The experiment consequently prioritized parallel
workers and indexed paging over a more invasive fetch/write pipeline.

## What the prototype changes

- `PINTAIL_SNAPSHOT_THREADS=1`: execute each existing snapshot worker on a dedicated
  thread with its own current-thread runtime. Worker transactions still originate
  under the coordinator's global lock. This allows CPU encoding and durable writes
  for different workers to run concurrently.
- `PINTAIL_SNAPSHOT_EXPAND_KEYS=1`: expand composite-key seek predicates into ordered
  equality prefixes and a greater-than comparison, preserving parameter order and
  source comparison semantics.
- `PINTAIL_SNAPSHOT_PROFILE=1`: emit per-chunk fetch-await, conversion and bulk-write
  durations. Concurrent durations overlap and must not be added as wall time.

All candidate behavior is opt-in on this experiment branch. The default algorithm
is retained as an A/B control. These flags are not deployment recommendations.
Dedicated-thread shutdown, cancellation, cross-database resource budgeting and
operational defaults still require production design and review.

The range case uses four disjoint MySQL views over one physical table and four
independent Pintail stores. It proves the benefit of parallel reads/encoding of
ranges without requiring four physical source copies. It does NOT implement a
range planner, one-table segment assembly, or a resumable range journal. A shipping
implementation needs those pieces while retaining one table manifest publisher.

## Correctness and evidence

All 18 timed copies passed source row-count, key-sum and payload CRC-sum comparisons.
Two additional candidate runs deliberately paused after two durable chunks, reopened
stores, and resumed. The multi-table and expanded-composite cases both passed
checksums across every column, including decimal, datetime, binary, ENUM and NULL.
They also asserted that the original handoff checkpoint was unchanged. Source data
was quiescent; concurrent DDL, live CDC overlap and process-kill recovery were not
exercised by these experiments.

The six snapshot unit tests and touched-crate clippy passed. Full development
validation passed at code commit `a2b28a9`: formatting, workspace clippy, dashboard
type checking, and workspace unit tests. The full result is banked in
`benchmark/snapshot-throughput/validation.md`. This is not an rc/stable gate.

The fresh environment initially lacked Node; adding Node 24 resolved the dashboard
type-check failure. A storage test assumes its temporary directory shares the root
filesystem; setting TMPDIR to a directory on that filesystem resolved the other
environment failure. No application or test changes were made for either issue.
The complete development profile was then rerun successfully.

Evidence: `benchmark/snapshot-throughput/matrix-results.json`, `matrix-summary.json`,
`composite-explain.log`, and `resume-checks.json`. The `estimated_row_bytes` fields
include in-memory row accounting: they are NOT source disk size or measured wire
bytes. Resumed-run rates include already copied rows in their numerator and must
not be used as performance results.

## Reproduce

Use an isolated MySQL 8.4 instance with ROW binlogging, full row images and metadata,
GTID enabled, UTC timezone, and permission to establish the global read lock.
The setup creates a new `snapshot_lab` database and intentionally refuses an
existing one. The source DSN should initially omit a database name.

```sh
export SNAPSHOT_BENCH_DSN='mysql://USER:PASSWORD@127.0.0.1:PORT'
CARGO_TARGET_DIR=target ~/.cargo/bin/cargo build --release -p pintail-snapshot --example throughput
./target/release/examples/throughput setup 1000000
export SNAPSHOT_BENCH_OUTPUT=/path/to/new/experiment-output
python3 benchmark/snapshot-throughput.py
```

The matrix runs each case three times, validates results, and removes only the
fresh copy directories it created after successful verification. Preserve its JSON
and log files. Set `SNAPSHOT_BENCH_RESUME=1` for a separate pause/resume run, for example:

```sh
PINTAIL_SNAPSHOT_THREADS=1 PINTAIL_SNAPSHOT_EXPAND_KEYS=1 \
SNAPSHOT_BENCH_RESUME=1 SNAPSHOT_BENCH_CHUNK_ROWS=10000 \
./target/release/examples/throughput composite 1 /path/to/new/resume-copy
```

The current harness strengthens verification to all-column checksums for new runs.
That verification occurs after the timed interval, so historical timings and their
original narrower validation remain explicitly distinguished above.

## Recommendation

Prioritize indexed composite-key paging and genuine CPU parallelism. Then implement
range scheduling for large individual tables, with immutable segment preparation
in parallel and serialized publication. Investigate source query plans, actual
transferred bytes and CDC catch-up separately before attributing a long deployment
sync to any one cause or promising an end-to-end speedup.

# Pintail

Pintail is a columnar analytical database that mirrors your MySQL database
and answers the queries MySQL is slow at. It ships as one binary: the
storage engine, the replication pipeline, the SQL engine, a MySQL-compatible
wire endpoint, and a web dashboard are all inside it.

You point it at a MySQL or MariaDB server. It takes a consistent snapshot,
follows the binlog (or polls, when binlogs are off), and keeps an
up-to-date columnar copy. The `GROUP BY` over 20 million rows that takes
MySQL half an hour comes back in seconds. Same data, same MySQL clients.

The engine is written from scratch in Rust. No DataFusion, no DuckDB, no
embedded ClickHouse. That was a deliberate choice: we wanted to own the
storage format and the executor end to end, and we benchmark every design
decision instead of assuming (see `experiments/` and
[docs/decisions.md](docs/decisions.md) for the receipts). Whether that bet
pays off against ClickHouse is a number you can check in
[benchmark/results.md](benchmark/results.md), including the queries where
it currently doesn't.

## Status

Pre-1.0. Single node only, with S3-compatible backup and restore, and the
replica is read-only by design. The parts that exist are tested hard: the
release gates kill real snapshot and CDC worker processes mid-write and
verify recovery, and every storage change has to keep a 20-million-row
benchmark byte-exact. The boundaries we know about are written down in
[docs/limitations.md](docs/limitations.md). It mirrors your MySQL data, so
losing a Pintail node loses nothing, but don't make it your only copy of
anything either.

## Quick start

```sh
docker compose up --build --detach
docker compose logs pintail
```

Open <http://127.0.0.1:8080>, create the first admin, then choose **Add
database**. Give it a MySQL DSN reachable from the container, probe it,
pick CDC or polling (the probe recommends one), and start the snapshot.
Once the state reads Streaming or Polling, the data is queryable.

The first boot prints the generated JWT and DSN-encryption secrets once.
Keep the `pintail-data` volume, and treat its logs as sensitive. The
dashboard and API listen on 8080, the MySQL endpoint on 3306; override with
`PINTAIL_HTTP_PORT` and `PINTAIL_WIRE_PORT`.

## Querying

Any MySQL client works. In the dashboard, create a database API key with
the `query` scope. The database name is the username, the key is the
password:

```sh
MYSQL_PWD='pk_your_key' mysql \
  --protocol=tcp \
  --host=127.0.0.1 \
  --port=3306 \
  --user=analytics \
  --database=analytics
```

Use a MySQL 8.4 or MariaDB CLI. Oracle's 9.x CLI dropped the
`mysql_native_password` client plugin that Pintail's challenge auth uses;
mysql2, PyMySQL, DBeaver, and Metabase all work.

The SQL surface is the analytical subset of the MySQL dialect: joins,
subqueries, aggregates, and window functions (`ROW_NUMBER`, `RANK`,
`DENSE_RANK`, and aggregates over `PARTITION BY` / `ORDER BY`). It is
checked against real MySQL with an oracle that compares results for 600
generated and hand-written queries.

## How it works, briefly

Ingested rows land in a WAL-backed memtable and flush into PTSEG segments,
Pintail's own columnar format. Dates, datetimes, and decimals are stored as
fixed-width integers when they round-trip exactly, so scans compare native
values instead of parsing text. Updates are resolved at read time with a
sweep-line that classifies each key range: ranges with one version stream
straight from the segment, and only genuinely overlapping ranges pay for a
merge. This is the part ClickHouse's ReplacingMergeTree makes you opt into
with `FINAL`; Pintail always returns the merged answer.

Scans decode in parallel, aggregation runs on per-batch thread-local state,
and predicated scans decode the filter column first so selective queries
skip most of the remaining bytes. The longer version is in
[docs/architecture.md](docs/architecture.md) and
[docs/format.md](docs/format.md).

## Benchmarks

The release gate replays eight analytical queries over 20,000,000 rows on
identically limited containers (8 CPUs, 8 GB each) and requires exact
result equality plus at least 50x over source MySQL in aggregate. ClickHouse
runs alongside as the reference we are chasing, measured both as plain
MergeTree and as ReplacingMergeTree with `FINAL`, which is the honest
comparison for an always-correct replica. Current numbers are in
[benchmark/results.md](benchmark/results.md). Reproduce them with:

```sh
(cd benchmark && bun install --frozen-lockfile && bun run benchmark)
```

There is also a 30-minute CDC soak at 5,500 events/s with source/replica
checksums and memory-slope tracking, recorded in
[tests/loadgen/results.md](tests/loadgen/results.md).

## Building from source

```sh
cd packages/dashboard
bun install --frozen-lockfile
bun run generate
cd ../..
cargo run --release -- --data-dir ./data
```

Rust 1.85+, Bun for the dashboard. `cargo build` regenerates the dashboard
assets when they change and embeds them in the binary. Configuration
precedence is CLI flags, then `PINTAIL_*` environment variables, then
`pintail.toml`; see `pintail.example.toml`.

## Contributing

Bug reports with a failing query are gold; so are MySQL compatibility gaps,
since the oracle can only generate what we thought to generate. If you want
to work on the engine itself, two things will save you a review round-trip:

- The codebase forbids `unsafe` outright, and performance claims need
  evidence: contested designs get a checksum-verified experiment in
  `experiments/` before they get adopted. [docs/decisions.md](docs/decisions.md)
  records what was tried and why some fast-looking ideas were rejected.
- `cargo fmt`, a clippy-clean build (`pedantic` is on and warnings are
  errors in CI), and `cargo test --workspace` are the baseline. The full
  release gate, which needs Docker and real MySQL containers, is documented
  in [docs/verification.md](docs/verification.md).

## Acknowledgements

Building from scratch doesn't mean inventing from scratch. Ideas we
borrowed, with sources:

- The merge-on-read range classification started from reading ClickHouse's
  `PartsSplitter`, and ClickHouse itself is the benchmark target that keeps
  us honest. Reading the ScyllaDB and DuckDB sources shaped several storage
  and executor decisions; the ones we adopted (and the ones that lost in
  our measurements) are logged in `experiments/RESULTS.md`.
- String columns use the 16-byte German-string views from the
  [Umbra paper](https://www.cidrdb.org/cidr2020/papers/p29-neumann-cidr20.pdf)
  by Neumann and Freitag.
- Date arithmetic is Howard Hinnant's
  [civil-date algorithms](https://howardhinnant.github.io/date_algorithms.html).
- The MySQL frontend stands on
  [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs) and
  [opensrv](https://github.com/databendlabs/opensrv), and the engine leans
  on rayon, zstd, lz4_flex, and xxHash daily.

## License

Apache-2.0.

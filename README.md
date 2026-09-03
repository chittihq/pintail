<h1 align="center">Pintail</h1>

<p align="center">A live analytical replica of your MySQL database, in one binary.</p>

<p align="center">
  <a href="https://github.com/chittihq/pintail/actions/workflows/ci.yml"><img src="https://github.com/chittihq/pintail/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/chittihq/pintail/actions/workflows/e2e.yml"><img src="https://github.com/chittihq/pintail/actions/workflows/e2e.yml/badge.svg" alt="E2E"></a>
  <a href="https://github.com/chittihq/pintail/releases"><img src="https://img.shields.io/github/v/release/chittihq/pintail?include_prereleases" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License"></a>
</p>

Pintail makes slow MySQL reports fast. Point it at a MySQL or MariaDB
server and it copies the data into a columnar store, keeps the copy in
sync from the binlog, and answers the queries MySQL struggles with: the
report that takes half an hour comes back in well under a second. Your
existing clients, BI tools and ORMs connect to Pintail the way they
connect to MySQL.

Everything ships in a single binary: storage, replication, the SQL engine,
a MySQL-compatible endpoint, and a web dashboard. The engine is written
from scratch in Rust, with no DuckDB or ClickHouse inside. Every design
decision is benchmarked rather than assumed, and the results are public,
including the queries where ClickHouse still wins.

## Features

- **Continuous sync.** Consistent initial snapshot, then row-level changes
  from the binlog (GTID or file position), or polling where the binlog is
  unavailable. Schema changes are followed. Works with the settings most
  servers already have, including managed MySQL.
- **MySQL wire protocol.** Any MySQL client, driver or BI tool connects
  with a database API key. Joins, subqueries, aggregates, window functions
  and CTEs: the parts of the dialect reports are made of, checked
  byte-for-byte against a real MySQL.
- **Columnar storage built for scans.** Compressed segments, merge-on-read
  over a write-ahead log, and results that are exact while changes are
  still streaming in.
- **Bounded by design.** Per-query and process-wide memory ceilings,
  spill to disk, and admission control that sheds overload as a MySQL
  error instead of unbounded latency.
- **Operations included.** A dashboard for databases, tables, replication
  state and dead letters; workspaces, members and API keys; S3-compatible
  backup and restore; an HTTP API for all of it.

## Quick start

Pintail ships as a public image, `ghcr.io/chittihq/pintail`, for
linux/amd64 and linux/arm64. Save this as `docker-compose.yml`:

```yaml
services:
  pintail:
    image: ghcr.io/chittihq/pintail:0.1.0
    ports:
      - "8080:8080"   # dashboard and HTTP API
      - "3306:3306"   # MySQL wire endpoint
    volumes:
      - pintail-data:/var/lib/pintail
    restart: unless-stopped

volumes:
  pintail-data:
```

Then:

```sh
docker compose up --detach
docker compose logs pintail   # the first boot prints the generated secrets once
```

Open <http://127.0.0.1:8080>, create the first admin, and choose **Add
database**. Give Pintail your MySQL connection string, let it check the
server (it recommends a sync mode), and start the first copy. Once the
state reads *Streaming* or *Polling*, you can query.

Keep the `pintail-data` volume: it holds the replica, the metadata and the
first-boot secrets. Put a TLS terminator in front of port 8080 for
anything beyond localhost. The repository's
[docker-compose.yml](docker-compose.yml) is the production-ready version
of the file above, with a memory limit, a health check and every
environment variable documented.

### One-line install on a Linux server

```sh
curl -fsSL https://raw.githubusercontent.com/chittihq/pintail/dev/scripts/install.sh | sh
```

The script installs Docker and the Compose plugin if they are missing,
writes the compose file above to `/opt/pintail`, starts the latest release,
waits for it to report healthy, and prints the dashboard address and the
first-boot secrets. Re-running it upgrades an existing installation and
leaves its configuration alone. `PINTAIL_VERSION`, `PINTAIL_DIR`,
`PINTAIL_HTTP_PORT` and `PINTAIL_WIRE_PORT` in the environment override the
defaults, and [scripts/install.sh](scripts/install.sh) is short enough to
read first.

MySQL 5.7, 8.x and MariaDB are supported as sources.

## Querying

Create a database API key with the `query` scope in the dashboard. The
database name is the username and the key is the password:

```sh
MYSQL_PWD='pk_your_key' mysql \
  --protocol=tcp \
  --host=127.0.0.1 \
  --port=3306 \
  --user=analytics \
  --database=analytics
```

The wire endpoint speaks `caching_sha2_password` and
`mysql_native_password`, so the `mysql` CLI, mysql2, PyMySQL, DBeaver,
Metabase, Prisma, Drizzle and Sequelize all work unchanged. What Pintail
implements of MySQL is tabulated in [parity.md](parity.md); the
differential oracle behind it holds 1,081 cases that pass byte-exactly
against MySQL 8.4 and 8.0. Known gaps are written down in
[docs/limitations.md](docs/limitations.md).

## How it works

Pintail keeps its own copy of your data, organized for scanning millions
of rows at a time, and applies changes from MySQL continuously. Every
query answers from the up-to-date, merged view: there is no "eventually
correct" mode, and results stay fast while data is streaming in. The
internals are written up in [docs/architecture.md](docs/architecture.md)
and [docs/format.md](docs/format.md).

## Benchmarks

<!-- benchmark:begin -->

Eight reporting queries over 20,000,000 rows, with MySQL, Pintail and
ClickHouse each in identical containers (8 CPUs, 8 GB). A result only counts
if it exactly matches MySQL's answer. Two numbers matter here and they say
different things, so they are reported separately rather than averaged into
one headline.

**Repeated queries.** Pintail keeps an exact-result memo for aggregates over
a settled snapshot, invalidated by any ingest, so re-running the same query
on an unchanged replica is served from it. ClickHouse's query cache is off,
so this compares Pintail's cache against ClickHouse's execution — a fair
measure of what a dashboard refresh costs, and not a measure of engine speed.

| Query | MySQL | Pintail (memo) | CH RMT+FINAL |
|---|---:|---:|---:|
| Full table count | 1,437 ms | 12 ms | 13 ms |
| Filtered count | 587 ms | 12 ms | 31 ms |
| Group by status | 34,398 ms | 13 ms | 69 ms |
| Region × status breakdown | 13,054 ms | 13 ms | 177 ms |
| Monthly revenue (2023) | 5,462 ms | 11 ms | 43 ms |
| Top 10 spenders | 889,417 ms | 77 ms | 176 ms |
| Regional analytics | 53,410 ms | 12 ms | 130 ms |
| Join users + orders | 796,769 ms | 15 ms | 163 ms |

**Novel queries — raw engine speed.** The same shapes with constants the memo
has never seen, so both engines actually execute. **ClickHouse is faster here.**
This is the honest measure of execution performance, and Pintail does not yet
win it.

| Query | MySQL | Pintail | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|
| Filtered count, novel constant | 1,125 ms | 407 ms | 53 ms | 0.13× |
| Group by region (novel group column) | 12,949 ms | 846 ms | 93 ms | 0.11× |
| Monthly revenue, novel year | 8,226 ms | 129 ms | 51 ms | 0.40× |
| Regional analytics, novel range | 56,145 ms | 544 ms | 139 ms | 0.26× |

ClickHouse is measured in both configurations: plain `MergeTree` for its
raw-speed ceiling, and `ReplacingMergeTree` read with `final = 1`, which is
the comparable one because it does the merge-on-read work a CDC replica owes
on every read. Full numbers, including the MergeTree column and per-query
resource use, are in [benchmark/results.md](benchmark/results.md). Reproduce
them with:

```sh
(cd benchmark && bun install --frozen-lockfile && bun run benchmark)
```

Caveats worth stating plainly: one synthetic dataset and eight query shapes,
on a shared host, measured as
`warm: 2 warmup + 15 measured; cold: 5 distinct memo-cold variants; MySQL baseline reused from 2026-09-03T08:22:24.512Z`. Enough to characterise these
queries and not enough to support a general claim about either engine. MySQL
runs with a 1 GB buffer pool, so its column is a baseline being escaped
rather than a tuned competitor.

<sub>Generated from `benchmark/results.json` (2026-09-03T08:25:58.423Z) by
`benchmark/render-readme-table.ts` — do not edit by hand.</sub>

<!-- benchmark:end -->

A 30-minute CDC soak that streams 5,500 changes per second while
continuously checking the copy stays identical is recorded in
[tests/loadgen/results.md](tests/loadgen/results.md), and a concurrency
sweep of simultaneous clients, including a memory-constrained profile, in
[tests/load/results.md](tests/load/results.md).

## Configuration

Configuration precedence is CLI flags, then `PINTAIL_*` environment
variables, then `pintail.toml`. Every option is described in
[pintail.example.toml](pintail.example.toml) and `pintail --help`.
`PINTAIL_LOG` selects verbosity (`error`, `info`, `debug`); no log line
carries a DSN, API key secret, invite token, session JWT or row value.

## Documentation

| Document | What it covers |
|---|---|
| [docs/architecture.md](docs/architecture.md) | How replication, storage and the query engine fit together |
| [docs/format.md](docs/format.md) | The on-disk segment and WAL format |
| [parity.md](parity.md) | MySQL features Pintail implements |
| [docs/limitations.md](docs/limitations.md) | Known gaps and unsupported cases |
| [docs/decisions.md](docs/decisions.md) | Design decisions, including the ideas that were measured and rejected |
| [docs/verification.md](docs/verification.md) | The oracle, end-to-end, browser and benchmark gates |
| [docs/development.md](docs/development.md) | Working on the codebase |
| [CHANGELOG.md](CHANGELOG.md) | Release notes |

## Status

Pintail is pre-1.0 and single-node. The replica is read-only by design.
It is tested hard: the suites crash the process mid-write and verify
nothing is lost, boot a real MySQL and throw writes, schema changes and
`kill -9` at the pair, then check every table still matches exactly.
Pintail mirrors your MySQL data, so losing a Pintail node loses nothing.
Do not make it your only copy of anything.

## Development

```sh
# Dashboard assets, embedded into the binary at build time
(cd packages/dashboard && bun install --frozen-lockfile && bun run generate)

# Build and run
cargo run --release -- --data-dir ./data

# Lint and unit tests
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Rust 1.88 or newer and Bun. The full release gate, which needs Docker and
real MySQL containers, is documented in
[docs/verification.md](docs/verification.md).

## Contributing

Bug reports with a failing query are gold, and so are MySQL compatibility
gaps: the oracle can only generate what we thought to generate. Two things
save a review round-trip when working on the engine:

- The codebase forbids `unsafe`, and performance claims need evidence.
  Contested designs get a checksum-verified experiment in `experiments/`
  before they are adopted; [docs/decisions.md](docs/decisions.md) records
  what was tried and why.
- `cargo fmt`, a clippy-clean build with `pedantic` on and warnings as
  errors, and a green `cargo test --workspace` are the baseline.

## Acknowledgements

Building from scratch does not mean inventing from scratch.

- The merge-on-read range classification started from reading ClickHouse's
  `PartsSplitter`, and ClickHouse is the benchmark target that keeps us
  honest. The ScyllaDB and DuckDB sources shaped several storage and
  executor decisions; what was adopted and what lost in our measurements
  is logged in `experiments/RESULTS.md`.
- String columns use the 16-byte German-string views from the
  [Umbra paper](https://www.cidrdb.org/cidr2020/papers/p29-neumann-cidr20.pdf)
  by Neumann and Freitag.
- Date arithmetic is Howard Hinnant's
  [civil-date algorithms](https://howardhinnant.github.io/date_algorithms.html).
- The MySQL frontend stands on
  [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs); the
  wire protocol is a from-scratch crate; the engine leans on rayon, zstd,
  lz4_flex and xxHash daily.

## License

[Apache-2.0](LICENSE).

# Pintail - Anything in milli seconds


Pintail makes slow MySQL reports fast. It keeps a live copy of your
MySQL database, organized for analytics, and answers the questions MySQL
struggles with: the report that takes half an hour comes back in well
under a second. Everything ships in one binary — storage, sync, SQL, a
MySQL-compatible endpoint, and a web dashboard.

You point it at a MySQL or MariaDB server and it copies the data and
stays in sync on its own. Your existing tools and clients connect to
Pintail exactly the way they connect to MySQL.

The engine is written from scratch in Rust — there is no DuckDB or
ClickHouse hiding inside. Every design decision is benchmarked instead of
assumed, and the results are public, including the queries where
ClickHouse still wins (see
[benchmark/results.md](benchmark/results.md)).

## Status

Pre-1.0. Single node only, with S3-compatible backup and restore, and the
copy is read-only by design. What exists is tested hard: the test suites
crash Pintail on purpose in the middle of writes and verify nothing is
lost, and an end-to-end test boots a real MySQL, throws writes, schema
changes, and a `kill -9` at it, then checks every table still matches
exactly. What Pintail supports of MySQL is tabulated in
[parity.md](parity.md); known limits are written down in
[docs/limitations.md](docs/limitations.md). Pintail mirrors your MySQL
data, so losing a Pintail node loses nothing — but don't make it your
only copy of anything either.

## Quick start

```sh
docker compose up --build --detach
docker compose logs pintail
```

Open <http://127.0.0.1:8080>, create the first admin, then choose **Add
database**. Give it your MySQL connection string, let Pintail check the
server (it recommends the best sync mode), and start the first copy. Once
the state reads Streaming or Polling, you can query.

Syncing works with the settings most servers already have, including
managed MySQL where you can't change them. MySQL 5.7, 8.x, and MariaDB
all work.

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

You can use joins, subqueries, aggregates, and window functions — the
parts of MySQL's dialect that reports are made of. Correctness is checked
against a real MySQL by comparing the results of 600 test queries.

## How it works, briefly

Pintail keeps its own copy of your data, organized for scanning millions
of rows at a time, and applies changes from MySQL continuously.
Every query answers from the up-to-date, merged view — there is no
"eventually correct" mode, and results stay fast even while data is
streaming in. If you want the internals, they are written up in
[docs/architecture.md](docs/architecture.md) and
[docs/format.md](docs/format.md).

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
| Full table count | 1,563 ms | 13 ms | 12 ms |
| Filtered count | 587 ms | 12 ms | 30 ms |
| Group by status | 35,782 ms | 14 ms | 69 ms |
| Region × status breakdown | 13,290 ms | 12 ms | 266 ms |
| Monthly revenue (2023) | 5,562 ms | 14 ms | 46 ms |
| Top 10 spenders | 810,282 ms | 99 ms | 254 ms |
| Regional analytics | 57,104 ms | 12 ms | 181 ms |
| Join users + orders | 834,387 ms | 12 ms | 202 ms |

**Novel queries — raw engine speed.** The same shapes with constants the memo
has never seen, so both engines actually execute. **ClickHouse is faster here.**
This is the honest measure of execution performance, and Pintail does not yet
win it.

| Query | MySQL | Pintail | CH RMT+FINAL | vs CH |
|---|---:|---:|---:|---:|
| Filtered count, novel constant | 1,130 ms | 434 ms | 58 ms | 0.13× |
| Group by region (novel group column) | 14,081 ms | 825 ms | 89 ms | 0.11× |
| Monthly revenue, novel year | 9,202 ms | 142 ms | 51 ms | 0.36× |
| Regional analytics, novel range | 57,980 ms | 486 ms | 183 ms | 0.38× |

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
`warm: 2 warmup + 15 measured; cold: 5 distinct memo-cold variants; MySQL baseline reused from 2026-08-16T18:01:03.714Z`. Enough to characterise these
queries and not enough to support a general claim about either engine. MySQL
runs with a 1 GB buffer pool, so its column is a baseline being escaped
rather than a tuned competitor.

<sub>Generated from `benchmark/results.json` (2026-08-18T08:32:17.049Z) by
`benchmark/render-readme-table.ts` — do not edit by hand.</sub>

<!-- benchmark:end -->

There is also a 30-minute stress test that streams 5,500 changes per
second while continuously checking the copy stays identical, recorded in
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

### Diagnostics

`PINTAIL_LOG` selects how much the server reports on stderr:

| Level | What it adds |
|---|---|
| `error` | failures only |
| `info` (default) | every API request with its duration, replication and backup lifecycle, the resumed CDC position |
| `debug` | per-table probe timings, per-chunk snapshot progress, idle poll cycles, storage flush and compaction decisions |

Durations are the point. A capability probe walks every table in the source
schema, so a large schema legitimately takes tens of seconds — a line reading
`probe done db=… tables=82 11803ms` distinguishes a server that finished the
work from one that hung, which client-side symptoms cannot.

No log line carries a DSN, API key secret, invite token, OAuth exchange code,
session JWT, or row value.

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
  [sqlparser-rs](https://github.com/apache/datafusion-sqlparser-rs) for SQL
  and a from-scratch `pintail-protocol` crate for the wire protocol itself;
  the engine leans on rayon, zstd, lz4_flex, and xxHash daily.

## License

Apache-2.0.

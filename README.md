# Pintail

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
exactly. Known limits are written down in
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

Eight typical reporting queries over 20 million rows, with MySQL,
Pintail, and ClickHouse each in identical containers (8 CPUs, 8 GB). A
result only counts if it exactly matches MySQL's answer. From the latest
run:

| Query | MySQL | Pintail | ClickHouse |
|---|---:|---:|---:|
| Full table count | 2.3 s | 152 ms | 151 ms |
| Filtered count | 1.2 s | 153 ms | 178 ms |
| Group by status | 64 s | 169 ms | 244 ms |
| Region × status breakdown | 24 s | 167 ms | 307 ms |
| Monthly revenue | 12 s | 168 ms | 210 ms |
| Top 10 spenders | 27 min | 220 ms | 284 ms |
| Regional analytics | 117 s | 170 ms | 294 ms |
| Join users + orders | 27 min | 173 ms | 631 ms |

The same file also keeps a table of first-time, never-cached queries,
where ClickHouse still wins some — published on purpose. Full numbers are
in [benchmark/results.md](benchmark/results.md). Reproduce them with:

```sh
(cd benchmark && bun install --frozen-lockfile && bun run benchmark)
```

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

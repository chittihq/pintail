# Pintail

Pintail is a columnar analytical database for MySQL, built from scratch in
Rust and distributed as one binary.

The project is under active development. Milestones M0 through M2 provide the
single-process skeleton, Pintail's durable PTSEG columnar storage core, and
the in-process MySQL-dialect query engine. Snapshot replication, CDC, polling,
the external query APIs, and the MySQL wire endpoint arrive in later
milestones.

## Run locally

```sh
(cd packages/dashboard && bun install)
cargo run --release -- --data-dir ./data
```

Open <http://127.0.0.1:8080>. On the first boot, Pintail prints the generated
JWT and DSN-encryption secrets once. The JWT secret lives in the SQLite
`settings` table; the DSN key is saved in `./data/secrets.toml` with owner-only
permissions on Unix.

Configuration precedence is CLI arguments, `PINTAIL_*` environment variables,
`pintail.toml`, then defaults. See `pintail.example.toml`.

## Run with Docker Compose

```sh
docker compose up --build
```

The dashboard and health endpoint are then available on
<http://127.0.0.1:8080>, with durable state in the `pintail-data` volume.

## Develop the dashboard

The dashboard uses Bun:

```sh
cd packages/dashboard
bun install
bun run dev
```

`cargo build` runs `bun run generate` when dashboard inputs change, then embeds
the resulting static assets in the Rust binary. Set
`PINTAIL_DASHBOARD_PREBUILT=1` only when the output was generated immediately
before the Cargo invocation, as the container and CI builds do.

## Verify the current milestone

```sh
(cd packages/dashboard && bun install --frozen-lockfile)
(cd packages/dashboard && bun run typecheck)
(cd packages/dashboard && bun run generate)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p pintail-sqllogic --test plan_quality
cargo test -p pintail-sqllogic --test mysql_oracle -- --ignored --nocapture
```

The final command starts a uniquely named MySQL 8.4 Docker container and
compares 600 deterministic queries over equivalent MySQL and Pintail data.
Current compatibility boundaries are recorded in
[`docs/limitations.md`](docs/limitations.md).

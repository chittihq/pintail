# Pintail

Pintail is a columnar analytical database for MySQL, built from scratch in
Rust and distributed as one binary.

The project is under active development. Milestone M0 provides the process
skeleton: configuration, SQLite control-plane migrations, first-boot secrets,
an embedded Nuxt dashboard placeholder, and the `/health` endpoint.

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

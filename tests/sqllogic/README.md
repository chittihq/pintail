# SQL logic corpus

The ignored `mysql_oracle` integration test runs 862 deterministic queries
against MySQL 8.4 and Pintail over pinned storage snapshots, then compares
normalized ordered rows.

Coverage layers:

1. **Parametric loops** (~557) — scalar templates with a varying integer.
2. **Hand-written edges** (~265) — windows, decimals, JSON, set ops, review fixes.
3. **Typed diversify** (40) — `orders` seed with `DECIMAL` / `DATETIME` / `JSON`
   columns and joins against `users`.

A non-Docker unit test (`documented_rejects_stay_explicit`) pins limitation
shapes that must fail closed. Inventory:

```sh
bun run scripts/oracle-coverage.ts
```

The harness starts a uniquely named MySQL container, batches the queries
through one client process, and removes the container even when a comparison
fails.

Run the explicit Docker-backed gate with:

```sh
cargo test -p pintail-sqllogic --test mysql_oracle -- --ignored --nocapture
```

Run inventory / reject unit tests without Docker:

```sh
cargo test -p pintail-sqllogic --test mysql_oracle
```

Run the physical pruning gate with:

```sh
cargo test -p pintail-sqllogic --test plan_quality
```

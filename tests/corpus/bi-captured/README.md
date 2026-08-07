# Captured BI SQL

This directory is the handoff point for issue #24. The existing
`tests/corpus/bi-shapes.sql` is reconstructed from BI-tool documentation; it is
not measured production evidence. Captures here must come from an actual
Metabase, Tableau, Looker, Superset, or application dashboard session.

## Capture

Use an existing query-log export when possible. If an administrator enables
MySQL's general log briefly, scope the time window and dashboard account, turn
it off immediately after the dashboard run, and export only `Query`/`Execute`
records. The harness accepts:

- JSONL with `argument`, `sql`, `query`, or `statement` plus an optional
  `command_type`;
- tab-separated `mysql.general_log` exports containing `Query` or `Execute`;
- MySQL general-log text lines; or
- a plain semicolon-delimited SQL file.

Raw SQL can contain credentials, tenant identifiers, emails, and filter
values. Put it under `tests/corpus/bi-captured/raw/`; that path and exact replay
reports are ignored by Git. Do not paste a raw capture into an issue. Replay
with accounts restricted to read-only source data; the harness independently
filters data-changing statements as a second boundary.

## Extract and replay

```sh
cd tests/e2e
bun install --frozen-lockfile
bun run bi-dogfood.ts \
  --input ../corpus/bi-captured/raw/metabase-general-log.jsonl \
  --report ../corpus/bi-captured/report.raw.json \
  --mysql-dsn "$BI_MYSQL_DSN" \
  --pintail-dsn "$BI_PINTAIL_DSN"
```

Without both DSNs the command still extracts, classifies, redacts, and
frequency-deduplicates the capture. With both DSNs it replays read/session SQL
through the same `mysql2` protocol client, compares fields and canonicalized
row multisets (preserving top-level `ORDER BY`), and records the exact MySQL and
Pintail error objects.

The command writes two files:

- `report.raw.json`: exact exemplar SQL and errors for local diagnosis only;
- `report.sanitized.json`: literals and error text redacted for review.

Review every sanitized entry before sharing it. For each `pintail_reject` or
`result_mismatch`, add the sanitized shape, exact Pintail error code/message,
frequency, BI tool/version, and the implement-or-workaround decision to #24.
Only a reviewed sanitized report may be committed here.

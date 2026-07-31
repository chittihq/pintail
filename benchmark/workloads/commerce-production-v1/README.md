# commerce-production-v1

Production-shaped multi-tenant commerce workload. Unlike `benchmark/` root (a single
orders table with uniform/cyclic data — kept as the engine microbenchmark), this workload
models what pintail will actually mirror in production:

- 15 tables with separated order / payment / fulfillment statuses
- Zipf-skewed tenants and customers, whale accounts, long-tail order sizes
- correlated columns (payment status conditioned on order status, region on country)
- transactional value preservation on line items; checkout-time shipping region
- multi-currency (totals only meaningful per currency — queries respect this)
- soft deletes, nullable fields, JSON payloads, UTF-8/emoji text, BINARY(16) ids
- hot recent data + cold history with weekday/hour seasonality
- a deliberate `ON DELETE CASCADE` (shipment_items) as the CDC negative control
- lifecycle mutation stream (state transitions, multi-row transactions, bursts,
  late arrivals) for the mixed read/write phase

Queries q07–q09 use **window functions** — per the 2026-07-31 owner decision they are a
v1 forcing function: pintail must grow window-function support to pass this workload.
Until then the runner records them as `unsupported` and the gate fails loudly.

## Running

```bash
cd benchmark
bun run run-production.ts --profile smoke                 # dev: ~2k orders, minutes
bun run run-production.ts --profile ci                    # per-merge: 1% (~200k orders)
bun run run-production.ts --profile full                  # release gate: 20M orders, hours
bun run run-production.ts --profile ci --engines mysql    # oracle only (no pintail)
bun run run-production.ts --profile ci --phases snapshot,warm
```

Phases (from `workload.ts`): seed-and-snapshot → cold → warm → mixed
(writers+readers for 30 min) → post-compaction → kill-restart-and-validate.
Results land in `results/latest.{json,md}`.

## Profile

`production-profile.json` currently holds **synthetic defaults**. Replace it with a
sanitized capture from a real production replica (same JSON shape: row counts, value
histograms, null rates, rows-per-parent, skew parameters, seasonality) — never raw
values or PII. The three execution modes planned: (1) synthetic shape (this), (2) masked
production snapshot for release validation, (3) read-only replica query shadow.

## Gates

- `exactResults`: pintail must match MySQL row-for-row (normalized) on every query
- `maximumDlq: 0`, `maximumReplicationLagSeconds: 5` during the mixed phase
- fingerprint convergence after mutations and after kill-restart
- expected failure today: `shipment_items` divergence after cascade deletes until the
  CDC-mode reconciler (GOAL.md §7, issue #2 item 1) ships — the negative control proves
  the harness catches it

## Known limits

- full-profile seeding is hours and tens of GB — release gates only, per owner decision
- the inventory uniqueness set and tenant-customer map are held in memory during
  seeding; fine to ~8M inventory rows, revisit beyond that
- ClickHouse comparison lives in the root benchmark (issue #3), not here

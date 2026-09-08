# Validation

Final development profile passed on committed source `df68ed2`:

`bun run scripts/validate.ts --profile development`

Run id: `2026-09-08T05-02-23-403Z-development`. Clean tree at launch.
Formatting/strict workspace lint, dashboard typecheck and unit stages passed:
988 tests passed, 36 skipped. This is the development profile, not an rc/stable
release gate. Builds and tests ran on the build server.

Before the measurements, the standalone core-engine-100 experiment passed
strict all-target clippy and its two existing unit tests. Its release SQL proof
then ran 1,176 states: 1,008 complete exact results and 168 recorded memory
refusals. Every process also checked pinned/latest storage contents and restart.
There was positive measured query/writer overlap in 1,016 states; barrier launch
alone does not guarantee overlap, especially for maintenance/no-op phases.

The independent MySQL 8.4.11 run passed 336 complete SQL comparisons and ten
semantic witnesses. The typed-key overlay screen passed 128 complete projected
scan comparisons with asserted execution-path counters. Python runner syntax
checks passed. The temporary overlay instrumentation was restored before the
final development profile; no production engine source change was required.

Evidence and limitations: [join report](RESULTS.md),
[overlay report](../overlay-proof/README.md). No production deployment or push.

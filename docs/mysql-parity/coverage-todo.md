# Parity coverage implementation todo

Worktree: `test/expand-oracle-coverage`. Plan: [coverage-plan.md](coverage-plan.md).
All edits and commits are local; compilation and execution use the configured build server.

- [x] 1. Structured comparison: NULL/text/binary separation, expected-type float policy, comparator regressions.
- [x] 2. Runtime inventory, stable IDs, explicit sessions, image/version provenance, complete red-run artifacts.
- [x] 3. Conversion/subquery matrices and richer typed fixtures, including negative cardinality cases.
- [x] 4. String/collation and temporal boundary matrices.
- [x] 5. Grouping/window and JSON composition coverage.
- [x] 6. Session, diagnostics, prepared protocol, and result metadata coverage.
- [x] 7. Snapshot/CDC/DDL/flush/restart and memory/spill replay packs.
- [x] 8. Extend seeded generators, save failures, and test reduction/replay.
- [x] 9. Review, final validation, and bank outcomes without masking parity failures.

The corpus now gates: every case matches MySQL 8.4 or sits on the reviewed
known-failure ledger, which fails the run when an entry goes stale. Results
are in [coverage-results.md](coverage-results.md); generation and candidate
validation in [generated-cases.md](generated-cases.md).

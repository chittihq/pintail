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
- [ ] 9. Review, final validation, and bank outcomes without masking parity failures.

Known failures remain required failing cases. Completion means implementing and
running the coverage work, not claiming all MySQL features have been implemented.

Harness slice: eight unit tests pass; structured differential results retain the known failures.
Runtime inventory is exported by the inventory unit test and checked for source freshness.

Boundary slice: 341 new MySQL-valid shapes, 1,714 runtime-inventoried cases total.
Clippy and eight oracle unit tests pass. Structured comparison reports 58 failures,
including documented boundaries and previously hidden NULL/type mismatches.

Replay/generation slice: lifecycle, diagnostics, prepared protocol, storage layouts,
and spill packs implemented and run. Eight model proposals produced seven
MySQL-valid regressions. The 800-query seeded sweep exposed 18 mismatches,
reduced to 15 distinct stored regressions. Final verification remains pending.

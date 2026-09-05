# Quality and performance implementation todo

Requested 2026-09-05. Work proceeds in this order, with one commit per
reviewable slice. Local tests and clippy cover each slice; one complete
development validation runs after implementation. Remote benchmark work is
serialized and uses only containers created by this repository's harnesses.

1. [x] **Partition and rewrite tests.** Exercise the real in-process
   parse/bind/plan/execute path. Compare the complete row multiset with the
   concatenation of TRUE, FALSE and NULL predicate partitions. Cover nullable
   values, duplicate projections, joins, aggregates and equivalent rewrites.
   Include deterministic generated cases in ordinary push CI.
2. [x] **Memory watchdog.** Sample pressure once per second, cancel the
   largest tracked running query, and let existing cooperative cancellation
   release its memory. Cover victim selection, no-pressure behavior, repeated
   ticks and query lifetime cleanup. Apply to HTTP and wire execution.
3. [x] **Fuzz targets.** Add bounded byte-input targets for unauthenticated
   wire decoding, binlog decoding and persisted-format readers, with a local
   smoke corpus and reproducible commands. Reject malformed input gracefully;
   never hide unexpected panics.
4. [x] **Instruction-count gate.** Add a deterministic in-process workload
   and Linux instruction-count comparison with explicit baseline provenance,
   thresholds and a CI job. Keep correctness checks and timed benchmarks.
5. [x] **Release profile, then PGO.** Enable explicit release optimization
   settings first. Add reproducible profile generation/use with a representative
   training workload, measure against the instruction baseline, and keep PGO
   opt-in unless evidence establishes a benefit. Do not claim an unmeasured gain.
6. [x] **Completeness ledger.** Extend the existing source-backed parity
   inventory rather than duplicate it. Distinguish linked differential
   evidence, implementation-only coverage and missing functions; validate
   evidence references and generated output freshness.
7. [ ] **Auditor benchmark kit.** Provide one documented command for a clean
   machine to run the published workload, with dependencies checked, isolated
   owned resources, machine/toolchain provenance and portable output artifacts.
8. [ ] **Two-tier admission.** Reserve bounded capacity for conservatively
   classified short queries without allowing unknown/heavy plans to consume
   the reserve. Keep the process-wide total bound and test contention, release
   and classification. Document that classification is not a latency guarantee.

## Evidence and decisions

- Item 4: four answer-checked in-process workloads, a Callgrind recorder,
  per-query 5% comparison, and a push/PR workflow are implemented. A baseline
  was recorded on Linux x86_64 with rustc 1.97.1 and Valgrind 3.19.0; an
  independent repeat passed all four comparisons. The ledger records three
  samples per query, source/binary hashes, compiler and workload identity.

- Item 3: three cargo-fuzz targets, committed seeds, local smoke checks in
  push CI, and a manual fuzz workflow are implemented. Wire and storage each
  completed 1,000 instrumented runs. Store clippy/tests pass. Local smoke has
  two passes and one explicitly ignored known-failing upstream binlog
  reproducer; unrestricted binlog fuzzing remains enabled and is **not**
  claimed green. `fuzz/README.md` records reproduction and scope. Malformed
  storage collection counts are now bounded before allocation.

- Item 2: the server owns a one-second watchdog. Executor tests verify
  90% thresholds, largest-victim selection, repeated ticks, cancellation,
  clone/worker accounting and lifetime cleanup. Strict clippy and all tests
  of `pintail-exec` and `pintail` pass. See the memory-pressure decision for
  cooperative-cancellation and untracked-allocation limits.

- Item 1: `partition_rewrites.rs` covers 107 predicate partitions and seven
  equivalent rewrites over deterministic nullable fixtures, including outer
  joins and duplicate projections. All executor tests and strict crate clippy
  pass; the new checks run in the existing workspace push-CI job (~0.2 seconds).

- The existing `cargo nextest run --workspace` push CI already runs local
  correctness tests. Item 1 adds metamorphic coverage to that path; it does
  not replace the MySQL differential oracle.
- Cargo already optimizes release builds by default. Item 5 makes additional
  release settings explicit and measures them; no automatic speedup is assumed.
- The parity inventory already exists under `docs/mysql-parity/`; its upstream
  snapshot and generated ledger remain tracked by the owner's prior decision.

- Item 5 measurement: the explicit release profile reduced total instructions
  by 5.91%; opt-in PGO reduced them a further 16.83% on the four-query training
  workload. Every query improved, and independent comparison runs passed.
  Original, tuned and PGO artifacts are banked separately. The full production
  Docker PGO image built successfully and its server binary passed startup
  (`--help`) validation on the remote Docker host.

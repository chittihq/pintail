# Verification record

All compilation, Clippy, Rust tests and measurements ran on the build server.
No production engine code changed, and no release was made.

## Complete development profile: PASS

Command: `bun run scripts/validate.ts --profile development`

- Run: `2026-09-07T20-36-55-405Z-development`.
- Checked HEAD: `4594ace`, clean at launch.
- rustc 1.97.0; cargo 1.97.0; bun 1.3.14.
- Formatting, workspace/all-target Clippy with `-D warnings`, and README table: PASS.
- Dashboard frozen install and typecheck: PASS.
- Workspace nextest: **988 passed, 36 skipped**; unit stage 2.8 minutes.

This is a complete development profile, not an RC/stable release gate. The
profile does not include the repository-wide MySQL oracle, E2E, browser,
recovery/soak, compose, benchmark or acceptance stages. The experiment's separate
MySQL comparisons and measurements below do not substitute for those gates.
The original report is in `validate-out/runs/<run>/report.md` on the build checkout.
Subsequent changes only clarify analysis/reporting and bank evidence; the Rust
experiment implementations and engine sources are unchanged after this gate.

## Standalone experiment crate: PASS

`CARGO_TARGET_DIR=target ~/.cargo/bin/cargo clippy --all-targets -- -D warnings`
and `CARGO_TARGET_DIR=target ~/.cargo/bin/cargo test`, from the experiment directory:

- 4,500 full-result comparisons across all alternatives, distributions, seeds and
  edge sizes, including empty inputs.
- 2,400 comparisons across real storage mutation/flush/compaction/replay states.
- Release binaries compiled with the experiment lockfile and system allocator.

## Measured evidence

- 990 screen processes, 60 larger-table confirmation processes and 18 further
  distribution-specific confirmations: **1,068 processes / 8,544 checked snapshots**.
- Each process checks complete query outputs, old pinned state, latest committed
  state and restart/reopen. Every trajectory performs actual compaction work.
- Transactional MySQL 8.4: **240 + 24 exact TSV comparisons**, all passed. These
  use decoded-event ingestion into Pintail, not native binlog transport.
- Actual SQL at 100,000 invented rows / 256 MiB: 200 exact answers and 40 memory
  refusals for the original shapes. The filtered/projected join and factorized
  join each returned all 24 exact answers. Resource refusals remain failures in
  the evidence and are not included as successful query timings.
- The post-tombstone-retirement replay reproducer demonstrates the boundary in
  FINDINGS.md. It is retained as an adverse result, not counted as a passing test.

Python runners and report generators were syntax-checked. Committed oracle
manifests hash the generated SQL/TSV exports; the full generated exports are local
artifacts reproducible by the oracle runner.

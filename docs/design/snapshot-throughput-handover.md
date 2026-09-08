# Snapshot throughput: branch reconciliation

The experiment's implementation and evidence were already on `dev` when this
handover was written. The outstanding work was to reconcile the original
branch's ancestry, not introduce its prototype into the current engine.

## History that determines the merge

- `experiment/snapshot-throughput` contains `a2b28a9` and `18165c6`.
- `8029a8f` already brought the experiment onto `dev`. Its stable patch ID
  matches `a2b28a9` exactly: `8d13d70982f111710539f7635ea0af98f283bd98`.
- `f6fdf7e` then replaced dedicated worker threads with cancellable tasks,
  made expanded composite-key seeks the normal path, folded chunk timings
  into debug logging, and moved the experiment under
  `experiments/snapshot-throughput/`.
- The original branch was 188 commits behind before the handover commit,
  and 189 behind at `5d5d6e9`. It remained unmerged in Git's ancestry even
  though its patch and subsequent refinements were present.

Read `git diff dev...experiment/snapshot-throughput` to see the branch's
changes since its merge base. The two-endpoint form, `git diff dev
experiment/snapshot-throughput` (also `dev..experiment/snapshot-throughput`),
compares the tips directly and includes all the newer work absent from the
old branch. It does **not** compare from the merge base.

The three-dot diff is necessary but not sufficient here: it does not detect
changes already cherry-picked onto `dev`. Check patch identity and the
subsequent history before treating those 787 added / 19 removed lines as
new work.

## Merge resolution

Merge `cc165c3` (`--no-ff`) reconciles the two original commits with `dev`.
Both `crates/pintail-snapshot/src/lib.rs` and the independently added
`crates/pintail-snapshot/examples/throughput.rs` had conflicts.

The resolution keeps the pre-merge `dev` version of both files byte-for-byte.
Restoring the prototype would undo task cancellation, return the default to serial
polling, reintroduce tuple pagination, and discard newer snapshot repairs.
It would also revert the example to a current-thread runtime.

The merge does not resurrect `benchmark/snapshot-throughput.py`, the old
`benchmark/snapshot-throughput/` evidence directory, or
`docs/experiments/snapshot-throughput.md`. The current harness and writeup
already live at the paths below. All five incoming evidence files,
including `18165c6`'s development validation report, match their current
copies byte-for-byte.

- [Current experiment writeup](../../experiments/snapshot-throughput/README.md)
- [Current harness](../../experiments/snapshot-throughput/harness.py)
- [Historical validation](../../experiments/snapshot-throughput/evidence/validation.md)
- [Measurements](../../experiments/snapshot-throughput/evidence/matrix-results.json)
- [Composite seek explanation](../../experiments/snapshot-throughput/evidence/composite-explain.log)
- [Pause/resume evidence](../../experiments/snapshot-throughput/evidence/resume-checks.json)

## Defaults already in effect

There is no new defaults decision in this reconciliation. `f6fdf7e` already
removed the three prototype switches:

- Snapshot workers use the existing runtime's `JoinSet`; conversion and
  writing use `block_in_place` on a multi-thread runtime. Dropping the
  snapshot future aborts its worker tasks.
- Keyed pages use expanded equality prefixes and a greater-than comparison.
  Comparisons still run in the source using its column types and collations.
- Chunk debug logs report fetch time and combined conversion/write time.

The earlier proposal to choose between enabling prototype threads,
enabling expanded keys, or retaining flags described the old branch, not
current `dev`. This merge preserves current behavior.

## Evidence limits still apply

The original experiment used a loopback source, local NVMe, warm caches,
and quiescent synthetic data. Its worker and composite-key measurements
are independent and must not be multiplied. The 3.48x composite-key figure
used small chunks to force 100 pages; it is not a general speedup claim.

The disjoint-view experiment demonstrates parallel read/encoding benefits,
not a range planner, shared-table segment assembly, or resumable range
journal. Cross-database budgeting and performance over a remote source
link are not established by those measurements. The dedicated-thread
prototype's operational questions are not reasons to restore it over the
current task implementation.

The current snapshot integration suite adds a composite-key copy in small
pages, process-kill recovery, and a resumed result checked against source
rows. The historical development report alone is not an RC gate.

## Verification for this reconciliation

Completed on merge commit `cc165c3`:

- `cargo clippy -p pintail-snapshot --all-targets -- -D warnings`: PASS.
- `cargo test -p pintail-snapshot --lib`: all six tests PASS.
- `bun run scripts/validate.ts --profile rc`: complete PASS, all nine stages,
  including both MySQL e2e legs, browser, compose, and external clients.

Builds and tests ran on the build server, with the prebuilt dashboard,
Node on `PATH`, and `TMPDIR` on the root filesystem. No benchmark was rerun,
and this reconciliation makes no new performance claim.

The [merge validation report](../../experiments/snapshot-throughput/evidence/merge-validation.md)
banks run `2026-09-08T19-19-12-740Z-rc`. Updated e2e and oracle ledgers are
banked in `tests/e2e/` and `tests/sqllogic/results-oracle.json`. The historical
experiment evidence remains unchanged.

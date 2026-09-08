# Merging `experiment/snapshot-throughput`

Instructions for taking the last unmerged branch forward. The experiment is
done and its evidence is banked; what remains is review, merge and a decision
about defaults.

## What the branch is

Two commits (`a2b28a9`, `18165c6`) on `experiment/snapshot-throughput`,
worktree at `../pintail-snapshot-throughput`. Against `dev` it adds 787 lines
and removes 19 across ten files. It is **188 commits behind `dev`**, so this
is a real merge, and `crates/pintail-snapshot/src/lib.rs` has moved
underneath it.

Read `git diff dev...experiment/snapshot-throughput`, not `git diff dev
experiment/snapshot-throughput`. The two-dot form compares from the merge
base and makes the branch look like it deletes most of the repository. That
misreading has already cost time on two other branches this week.

| Change | Measured effect | Flag |
| --- | --- | --- |
| Each snapshot worker on its own thread with its own current-thread runtime | 2.90x on four physical tables; 2.83x on four disjoint ranges of one table | `PINTAIL_SNAPSHOT_THREADS=1` |
| Composite-key seek predicates expanded into equality prefixes plus a `>` comparison | 3.48x (23.575s to 6.766s) | `PINTAIL_SNAPSHOT_EXPAND_KEYS=1` |
| Per-chunk fetch-await, conversion and bulk-write timings | diagnostics only | `PINTAIL_SNAPSHOT_PROFILE=1` |

Code sites: `crates/pintail-snapshot/src/lib.rs:451` (threads), `:710`
(expanded keys), `:822` (profile).

**Everything is opt-in and off by default.** Merging changes no behaviour
until someone turns a flag on. That makes the merge low-risk and also means
it delivers nothing on its own - see "The decision this needs" below.

## Why the composite-key change matters most

It is the one with a causal explanation rather than only a timing. From
`benchmark/snapshot-throughput/composite-explain.log`, on invented
identifiers:

- `(bucket, id) > (900, 0)` read 910,000 rows to produce 10,000, about 331 ms
- `bucket > 900 OR (bucket = 900 AND id > 0)` read 10,000 rows, about 4.23 ms

The tuple form makes the source scan the index from the beginning, so
successive pages repeatedly revisit rows already copied and snapshot cost
grows with the square of the page count. On a composite-key source large
enough to page many times, that is not a 3.48x constant - it is the
difference between finishing and not.

The 3.48x figure itself came from 10,000-row chunks chosen to force 100 pages
on a small dataset. Do not quote it as the gain at default chunk sizing.

## What the experiment does not establish

Taken from the branch's own writeup, which is candid and worth reading in
full at `docs/experiments/snapshot-throughput.md`:

- Loopback source connection, local NVMe, warm caches, no flushes. It does
  not measure a remote deployment link.
- The gains are independent experiments and **must not be multiplied**.
- The range case uses four disjoint MySQL views over one physical table and
  four independent Pintail stores. It proves parallel reads and encoding pay
  off; it does **not** implement a range planner, one-table segment assembly,
  or a resumable range journal. A shipping version needs those while keeping
  a single table manifest publisher.
- Dedicated-thread shutdown, cancellation, cross-database resource budgeting
  and operational defaults are explicitly unresolved.
- Source data was quiescent. Concurrent DDL, live CDC overlap and process-kill
  recovery were not exercised.
- Validation was the `development` profile at `a2b28a9`, banked in
  `benchmark/snapshot-throughput/validation.md`. That is not an rc or stable
  gate.

## Doing the merge

1. `git merge --no-ff experiment/snapshot-throughput` into `dev`. Expect
   conflicts in `crates/pintail-snapshot/src/lib.rs`; 188 commits have landed
   since the fork, including yesterday's `Value::DecimalAverage` work which
   touched `pintail-snapshot`.
2. When a conflict looks like the branch reverting recent work, it is the
   branch being old. Take `dev`'s side and re-apply the branch's intent on
   top.
3. Per-slice verification: `cargo clippy -p pintail-snapshot --all-targets --
   -D warnings` and the six snapshot unit tests.
4. Then the full gate: `bun run scripts/validate.ts --profile rc`.

Build and test on `server@venus-001`, never locally - see `AGENTS.md` for the
rsync workflow and the `PINTAIL_DASHBOARD_PREBUILT=1` requirement. Two
environment traps the branch hit and documented: the dashboard type check
needs Node on `PATH`, and a storage test needs `TMPDIR` on the root
filesystem.

## The decision this needs

Merging with every flag off is safe and inert. The value only arrives when a
default changes, and that is a judgement the experiment deliberately declines
to make. Put the question to the owner rather than deciding it inside the
merge:

- **Expanded composite keys on by default.** The strongest candidate. It is a
  predicate rewrite with a causal explanation, it preserves parameter order
  and source comparison semantics, and the pathological case it removes is
  severe. Needs a correctness argument that the rewrite is equivalent for
  every key type and collation, not only the tested integers.
- **Threaded workers on by default.** Bigger gain, more surface. Shutdown,
  cancellation and cross-database budgeting are named as unresolved, so this
  wants operational design before it becomes a default.
- **Neither, keep as flags.** Merge as an opt-in escape hatch for a slow
  source, and revisit when a real deployment needs it.

## Sequencing

Land this before starting anything that touches `pintail-snapshot`. It is
188 commits behind already, and every day of drift makes the merge worse for
no benefit.

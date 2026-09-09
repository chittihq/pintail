# Generated query validation

The fixed corpus includes seven reviewed proposals from `nex-agi/nex-n2.5-pro:free`
and 15 distinct minimized regressions from the two seeded sweeps. Normal oracle
runs read checked-in JSON; they never call a model or require an API key.

The model proposed eight queries. MySQL accepted seven; the eighth combined
incompatible implicit collations and is excluded. Four accepted proposals matched
Pintail, while three exposed numeric-overflow or ORDER BY binding failures.
The reviewed JSON records model, prompt hash, fixture hash, ordering review,
and observed status. These statuses are historical observations, not exemptions.

To propose another batch, first export a current inventory with the oracle
inventory unit test (`PINTAIL_ORACLE_INVENTORY=validate-out/oracle-inventory.json`).
Set `OPENROUTER_API_KEY` or `OPENROUTER_API_KEY_FILE` outside the repository, then
run `bun run scripts/generate-oracle-candidates.ts`. The model is fixed to the
requested free endpoint, with provider fallbacks disabled. Only invented test
schemas are sent. Proposals and raw responses go under ignored `validate-out/`.

Run the explicit `validates_generated_candidates` ignored Rust test with
`PINTAIL_CANDIDATES_PATH` pointing to that JSON (paths resolve from the test
crate). It rejects non-read-only statements, unapproved relations/functions,
LIMIT subsets, excessive syntax complexity, and unreviewed ROWS windows.
`PINTAIL_REVIEWED_CANDIDATE_HASHES` accepts comma-separated SHA-256 SQL hashes
only after reviewing unique ordering. The two initial CTE windows join primary
keys one-to-one; their order is unique or their partitions are singletons.

Each Pintail proposal runs in a subprocess with a five-second deadline, a
1 GiB virtual-memory ceiling, a file-size ceiling, and two worker threads.
MySQL has a five-second statement limit. Comparison preserves NULL, exact
numbers, arbitrary bytes, and duplicate multiplicity. Model output supplies
queries, never expected answers. Inspect every accepted query before promoting
it into the static corpus; record MySQL rejections separately.

The seeded sweep saves every outcome and attempts up to eight rounds of AST
reduction per mismatch, retaining MySQL validity and the observed failure class.
Reduction is bounded, not a claim of a globally minimal reproducer. It also
checks proposed metamorphic equivalences against MySQL. An empty UNION arm
changes ENUM ordering by converting it to text, so that rewrite excludes the
ENUM family.

Additional explicit coverage commands are `bun run tests/e2e/parity-replay.ts`
for sessions/wire/snapshot/CDC/DDL/restart, and the ignored
`matches_mysql_across_storage_layouts` Rust test for memtable, persisted, mixed,
compacted, reopened, and spill paths. The standard oracle stage runs required
Docker tests serially and excludes only the explicitly supplied proposal sweep
and its private worker.

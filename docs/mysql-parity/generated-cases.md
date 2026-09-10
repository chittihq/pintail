# Generated and reviewed query cases

The fixed corpus includes seven reviewed candidate queries and 15 distinct
minimized regressions from two seeded sweeps. Both live in checked-in JSON
(`tests/sqllogic/tests/support/oracle_reviewed_cases.json` and
`oracle_seed_cases.json`); normal oracle runs read those files and need no
network access or credentials.

A candidate is only a query. Expected answers always come from MySQL, never
from whatever proposed the query. Of the first eight candidates, MySQL accepted
seven; the eighth combined incompatible implicit collations and was dropped.
The reviewed JSON records the fixture hash, the ordering review and the status
observed when each case was admitted. Those statuses are history, not
exemptions: every case must match MySQL, or appear in the known-failure ledger
described below.

## Validating new candidates

Export a current inventory with the oracle inventory unit test
(`PINTAIL_ORACLE_INVENTORY=validate-out/oracle-inventory.json`), write
candidate queries against the invented test schemas into a JSON file, then run
the ignored `validates_generated_candidates` Rust test with
`PINTAIL_CANDIDATES_PATH` pointing at that file (paths resolve from the test
crate). Results go to `validate-out/candidates-validated.json`.

The validator rejects non-read-only statements, unapproved relations and
functions, LIMIT subsets, excessive syntax complexity and unreviewed ROWS
windows. `PINTAIL_REVIEWED_CANDIDATE_HASHES` accepts comma-separated SHA-256
SQL hashes, and only after the query's ordering has been reviewed as unique.
The two initial CTE windows join primary keys one-to-one, so their order is
unique or their partitions are singletons.

Each candidate runs against Pintail in a subprocess with a five-second
deadline, a 1 GiB virtual-memory ceiling, a file-size ceiling and two worker
threads; MySQL has a five-second statement limit. The comparison preserves
NULL, exact numbers, arbitrary bytes and duplicate multiplicity. Inspect every
accepted query before promoting it into the static corpus, and record MySQL
rejections separately.

## Seeded sweeps

The seeded sweep saves every outcome and attempts up to eight rounds of AST
reduction per mismatch, keeping the query valid for MySQL and the observed
failure class. Reduction is bounded; it does not claim a globally minimal
reproducer. The sweep also checks proposed metamorphic equivalences against
MySQL. An empty UNION arm changes ENUM ordering by converting the column to
text, so that rewrite excludes the ENUM family.

## Known-failure ledger

`tests/sqllogic/tests/support/oracle_known_failures.json` lists fixed-corpus
cases that diverge from MySQL through a limitation recorded in
`docs/limitations.md`. Each entry carries the case id, its SQL, the reason and
a quotation of the limitation it falls under. A listed case that fails is
reported as a warning. The run fails if a listed case starts matching MySQL
(the entry is stale), if an entry names no corpus case or different SQL, or
if its quotation is not found in `docs/limitations.md`. The storage-layout
replay warns on the same entries and leaves staleness to the fixed corpus.

## Other coverage commands

`bun run tests/e2e/parity-replay.ts` covers sessions, the wire protocol,
snapshot, CDC, DDL and restart. The ignored
`matches_mysql_across_storage_layouts` Rust test replays boundary cases over
memtable, persisted, mixed, compacted and reopened layouts, plus the spill
paths. The standard oracle stage runs the required Docker tests serially and
skips only the explicitly supplied candidate validation and its private
worker.

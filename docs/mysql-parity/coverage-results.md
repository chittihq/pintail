# Expanded parity coverage results

The coverage work is implemented and exercised. Parity is **not achieved**.
The fixed corpus grew from 1,229 to 1,736 cases; case counts do not measure full
MySQL compatibility. Repeated failures across storage/session phases are checks,
not distinct engine defects.

Final differential runs used clean commit `050be915df0ba18309268bb9864ff8c1d437e688`.
Each JSON report records the immutable image digest, exact version, fixture hash,
session settings, and code commit. All data and SQL are invented test fixtures.

| Run | Passing | Failing | Other |
|---|---:|---:|---|
| Fixed corpus, MySQL 8.4.11 | 1,660 | 76 | 1,736 MySQL-valid cases |
| Fixed corpus, MySQL 8.0.46 | 1,659 | 77 | 1,736 MySQL-valid cases |
| Seeded queries, MySQL 8.4.11 | 782 | 18 | 800 queries, two seeds |
| MySQL metamorphic equivalences | 161 | 0 | ENUM/UNION ordering precondition enforced |
| Storage layouts and memory ceilings | 209 | 45 | Includes all 12 passing stress comparisons |
| Sessions, wire and replication lifecycle | 680 | 186 | One explicit unsupported row-image boundary |
| Initial model proposals | 4 | 3 | Seven valid; one MySQL rejection |

Seven reviewed model queries and 15 distinct minimized seeded failures are now
required offline fixed-corpus cases. The seed sweep attempted reduction for every
mismatch; bounded reduction does not imply a globally minimal query. Model
proposals remain evidence candidates until validated and reviewed. The selected
free endpoint reported zero cost for the successful eight-proposal response.

All four stress operations (aggregate, sort, join, window) matched MySQL at
12 MiB, 24 MiB and 256 MiB limits. Spill-file counters prove the spill paths ran.
The five storage layouts retain nine failing boundary checks each. Lifecycle
coverage includes snapshot, CDC insert/update/delete, key and NULL updates,
decimal/enum ALTER, prepared statements, metadata, warnings, session modes,
time zones, and reconnect/restart. The lifecycle runner completed without an
infrastructure failure; its red checks are retained for investigation.

Validation: the touched Rust target passed clippy with warnings denied and all
10 non-Docker tests. The two Bun comparator tests and strict TypeScript checks
for the new lifecycle files passed. The server binary was built and exercised.
The final `development` profile **failed at fmt** on pre-existing formatting in
`crates/pintail-exec/src/collation/`; dashboard typecheck, workspace unit tests,
and parser-corpus stages were not reached. This is not a full green gate.
No release, push, or deployment was performed.

The requested [58-case report](../FAILING_QUERIES.md) remains the earlier
1,714-case snapshot. It is deliberately not relabeled as the final 76-case count.
Final evidence:

- [MySQL 8.4 fixed-corpus failures](oracle-outcomes-expansion.json)
- [MySQL 8.0 fixed-corpus failures](oracle-mysql80-expansion.json)
- [Seeded failures and reductions](oracle-fuzz-expansion.json)
- [Storage failures and spill checks](oracle-physical-expansion.json)
- [Lifecycle failures and phase counts](lifecycle-expansion.json)
- [Model proposal outcomes, including the rejection](generated-candidate-outcomes.json)
- [Generation and replay instructions](generated-cases.md)

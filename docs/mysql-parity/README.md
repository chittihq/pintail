# MySQL parity audit ledger

This ledger compares MySQL's SQL functions and system features with Pintail,
and records the work needed to establish parity. It is a **static source audit,
not a claim that Pintail is a complete MySQL replacement**. Start with the
[feature comparison](features.md), then use the [function comparison](functions.md)
for individual names and operators.

## Baseline and inventory

The audit was prepared on 2026-09-05 from a shallow clone of the official
[MySQL Server repository](https://github.com/mysql/mysql-server), branch `8.4`,
at commit [`99960bf74fa9`](https://github.com/mysql/mysql-server/tree/99960bf74fa919347e4f4e3ca47672f333d6e91f).
`MYSQL_VERSION` in that checkout says **8.4.11**. This identifies a development
branch snapshot; it does not claim that 8.4.11 is the latest released version,
or that a particular server image was built from that commit.

The inventory combines the [official built-in reference](https://dev.mysql.com/doc/refman/8.4/en/built-in-function-reference.html)
with the native registry in `sql/item_create.cc`, function symbols in `sql/lex.h`,
and grammar-token locations in `sql/sql_yacc.yy`. Exact upstream links and input
hashes are retained. The feature rows point to relevant upstream modules.
No MySQL implementation files are vendored into Pintail.

| Inventory | Rows |
|---|---:|
| Distinct callable names, including aliases and internal helpers | 467 |
| Operators and special constructs | 47 |
| Feature contracts | 87 |

Among the callable names, 125 have implementation evidence awaiting semantic
verification, 44 have known restrictions, 21 are identified gaps, 92 need
assessment, and 185 are outside the current scope. These numbers are **not a
compatibility percentage**: aliases, internal functions, optional modules and
different overloads would distort that measure. Feature totals are 8 with
implementation evidence, 46 partial, 10 gaps, 16 unassessed and 7 excluded.

MySQL 8.4 is the primary baseline because it is Pintail's principal oracle.
MySQL 8.0 must retain its own patch-pinned tests and evidence; this audit has
not performed an 8.0 source comparison. Earlier MySQL and MariaDB versions
must also be tracked separately rather than inferred from an 8.4 result.

## Reading the ledger

| Status | Meaning |
|---|---|
| `implemented-unverified` | A binding/implementation path was found. Complete overload and behavioral parity has not been demonstrated. |
| `partial` | An implementation exists with a documented or source-observed restriction/difference. |
| `gap` | A required MySQL operation is absent from the reviewed dispatch or explicitly documented as missing. |
| `unassessed` | Evidence is insufficient, or the intended external contract needs a decision. A failed name lookup alone stays here. |
| `out-of-scope` | Internal helper or an intentional product boundary; do not treat it as a missing analytics feature. |

`P0` means correctness, durability, authorization or a client-visible contract;
`P1` means common workload coverage; `P2` means broader compatibility or a scope
decision. These are proposed triage priorities, not an approved implementation
schedule. An excluded function can still be a gap for another product whose
goal is full MySQL replacement.

The current product distinguishes **replicated read-only databases** from
**local writable databases**. A write-related entry can be excluded for a
replica and a real gap for local writes. Physical InnoDB file compatibility,
MySQL clustering and GIS execution are not implied by SQL parity.
Owner instructions for this audit explicitly exclude point-in-time recovery
(decision dated 2026-08-04; issue #20 comment trail). This ledger does not
authorize rebuilding it.

Each function record in [ledger.json](ledger.json) includes names/spellings,
category, upstream evidence, Pintail evidence, extracted binder guards,
test-file occurrence hints, status, scope, priority and an acceptance contract.
Each feature record includes a stable ID and a concrete acceptance requirement.
`testOccurrences` is a text search over selected Rust/SQL tests: it can match
comments, negative cases or helpers. It is **not** a test result, complete
coverage map, or a count of assertions. Every generated function record is
marked `verification: not-run`.

## What this audit resolves, and what still needs review

The older [parity matrix](../../parity.md) remains useful for discovery, but
name/keyword matches do not prove execution or semantics. This ledger adds:

- Dedicated syntax and window dispatch, so missing extractor matches do not
  automatically become missing functions. `LAG`, `LEAD`, `NTILE`,
  `FIRST_VALUE` and `LAST_VALUE` are examples.
- A distinction between selected wire compatibility queries (`DATABASE()` and
  `VERSION()`) and composable scalar expressions across all query surfaces.
- Per-family overload and type requirements, including metadata, errors,
  warnings, collations, temporal precision and JSON identity.
- Separate local-write, replication, security, protocol and operational
  contracts. Accepting constraint syntax is not constraint enforcement.

Some existing documents conflict. In particular, `docs/limitations.md` contains
both older JSON-comparison exclusions and newer implemented JSON ordering,
and both binder support notes and parser-rejection notes for composite
`EXTRACT`. General read-only wording also coexists with local writable mode.
This audit records those conflicts without silently rewriting another task's
documentation or declaring a disputed behavior verified. Resolve them with
the actual parse-to-execute and live replication paths.

The built-in reference is not a complete catalog of every installable UDF,
component, plugin, system variable, statement variation or commercial product.
Source extraction records registry presence without evaluating conditional
build flags or runtime plugin installation. Native registry entries missing
from the manual remain `source-only`; runtime module entries may have manual
evidence without a native-registry location. Feature rows explicitly keep
loadable functions, Enterprise tooling, X Protocol and related scope decisions
visible. A deployment-specific audit must extend those inventories.

## Evidence required to close a row

1. Pin the MySQL runtime patch version/image digest and record `VERSION()`,
   `sql_mode`, charset/collation, time zone, relevant sysvars, and binlog
   configuration. Record the Pintail commit and any uncommitted changes.
2. Name the exact supported overloads, input types and execution contexts.
   Compare valid inputs, NULL/empty/boundary inputs and invalid forms.
3. Compare result values/bytes **and** column metadata, errors/SQLSTATE,
   warnings and side effects. Require deterministic ordering where the SQL
   defines it; do not invent an ordering requirement for unordered results.
4. Exercise literals and stored columns through parse, bind, plan and execution;
   include wire text, prepared binary results and HTTP where applicable.
5. For storage/replication-sensitive behavior, repeat over snapshot rows, CDC
   memtables, persisted segments, compaction and restart, including schema changes.
   Transaction, reconnection and failure cases need the real source pair.
6. Attach the test case IDs, command/configuration, result artifact and commit.
   Keep restrictions explicit; a passing simple example cannot close an entire
   function family. Update the curated review and regenerate the ledger.

Useful first slices are TYPE-02/07/10/15 and WIRE-02 (value/metadata correctness),
WRITE-06/07/08 (accepted syntax versus promised guarantees), CDC-04/05/09/10
(visibility and recovery), and SQL-07/09/11 plus the missing window/JSON functions
(query coverage). Follow repository verification policy: fast focused loops
during development, then one appropriate profile gate at the end.

## Regeneration

The committed inputs make regeneration independent of the temporary clone,
network and Docker:

```sh
bun run scripts/mysql-source-ledger.ts
bun run scripts/mysql-source-ledger.ts --check
```

The check fails if the generated function/feature documents or JSON are stale.
It validates review IDs and evidence paths and records hashes of the selected
Pintail entry-point files. Those hashes detect changes to the entry points,
not all transitive executor behavior; a passing check never certifies semantics.

To refresh upstream, create a new temporary checkout and download the manual:

```sh
audit_dir=$(mktemp -d /tmp/pintail-mysql-audit.XXXXXX)
git clone --depth 1 --single-branch --branch 8.4 \
  https://github.com/mysql/mysql-server.git "$audit_dir/mysql-server"
curl -fsSL https://dev.mysql.com/doc/refman/8.4/en/built-in-function-reference.html \
  -o "$audit_dir/functions.html"
bun run scripts/mysql-source-ledger.ts \
  --mysql-source "$audit_dir/mysql-server" --manual-html "$audit_dir/functions.html"
```

Review added/removed names, changed factories/grammar, source version and
manual changes; inspect curated conclusions again, update this summary, then
run `--check`. To reproduce this exact source snapshot after the branch moves,
fetch and check out the full commit recorded in [upstream.json](upstream.json).
The downloaded manual is mutable; its SHA-256 records the observed input,
while the committed inventory preserves the extracted names and links.

| File | Role |
|---|---|
| [upstream.json](upstream.json) | Extracted upstream names, source locations, manual references and input hashes |
| [review.json](review.json) | Curated function/operator assessments and acceptance rules |
| [features.json](features.json) | Curated feature contracts, findings and acceptance requirements |
| [ledger.json](ledger.json) | Generated, machine-readable comparison for filtering and future automation |
| [functions.md](functions.md), [features.md](features.md) | Generated review tables |
| [mysql-source-ledger.ts](../../scripts/mysql-source-ledger.ts) | Offline generator and consistency check |

## Differential coverage

`functions.md` and `ledger.json` keep coverage separate from semantic review:
`differential-tested` links at least one historical passing MySQL comparison;
`implementation-only` has implementation evidence but no linked passing case;
`missing` is a reviewed gap. `unassessed` and `out-of-scope` remain explicit.
A tested name does not certify every overload, edge case or execution path.

The committed `differential-evidence.json` pins the E2E bank commit, source
version, measured time, ledger/corpus SHA-256, SQL hashes, case names and passing
phases. Cases with WARN/FAIL or a documented gap are excluded. Skipped phases
provide no evidence. Function names inside comments, strings and quoted
identifiers do not count; bare keyword forms are conservatively unclassified.

After banking a matching E2E run, refresh with
`bun run scripts/refresh-differential-evidence.ts`, then regenerate with
`bun run scripts/mysql-source-ledger.ts`. Refresh verifies both the ledger and
query corpus against that immutable bank commit. Ordinary generation and
`--check` validate hashes and case mappings offline and reject stale evidence.

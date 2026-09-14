# Verification program

Owner decision, 2026-09-13: make Pintail's correctness evidence scale the way
its SQL oracle already does - by executable checks against an authoritative
answer, not by reading about other engines' mistakes.

## Premise

Three mature engines have spent decades finding and fixing the bugs a system
like Pintail can have. Their bug trackers are noisy, but their fix commits are
not: a fix that ships with a test is a minimized, reviewed reproduction. The
program turns that history into gates, and builds an oracle for every layer
that does not yet have one.

| Layer | Oracle | Before the program |
|---|---|---|
| SQL semantics | a live MySQL of the same major | 1,895 fixed cases and seeded sweeps, byte-exact, gating |
| MySQL's regression suite | a live MySQL | 114 files replayed, 79.5% of compared SELECTs exact; a report, not a gate |
| Change capture | none that generates cases | hand-written recovery scenarios and migration families |
| Storage recovery | a model of acknowledged commits | crash and recovery-sequence fuzzers; no disk faults |
| Kernels | the scalar path, incidentally | no independent reference evaluator |

## Rules

- **Upstream material never enters the repository.** Test suites of other
  engines are GPL or otherwise licensed apart from Pintail; they are fetched at
  run time at a pinned commit into a data directory outside the tree
  (`PINTAIL_ARCHAEOLOGY_DIR`, default `~/pintail-archaeology`).
- **What is committed is Pintail's own**: the tooling, generic bug-class
  descriptions, counts, and ledgers of upstream identifiers used to fetch a
  reproduction. Code and comments explain behaviour in Pintail's terms.
- **Every number is a ratchet.** A gate banks its count; a later run may raise
  it and may never lower it without a reviewed ledger entry naming the reason.
- **An oracle disagreement is a finding, never a skip.** Anything not compared
  is counted and named in the report.

## Phases

### P0 - Bug atlas

`scripts/archaeology/` clones the three upstream repositories at pinned refs,
extracts every commit that fixes a bug and adds or changes a test, classifies it
by the source paths it touches into a Pintail-relevant taxonomy, and renders
`docs/design/bug-atlas.md`: bug classes ranked by frequency, each mapped to the
Pintail gate that covers it or marked uncovered. Uncovered classes are the
backlog for the phases below.

### P1 - MySQL's suite as a gate

1. `tests/mtr` banks a per-file baseline of the statements that matched
   exactly, by identity, and fails when any of them stops matching; the `mtr`
   stage joins the rc profile. A count would hide one statement regressing
   while another started passing.
2. The replay widens to every query-shaped file of the main suite, and a second
   run replays MariaDB's suite - against the same MySQL 8.4 oracle, since what
   is being measured is Pintail's answers to those statements.
3. A replication mode runs each file's fixtures and DML on the source and
   compares after the replica catches up, so statements a local database could
   not follow become change-capture checks.
4. Error buckets from the report are burned down, largest first.

### P2 - Fix-commit regressions

Tests added alongside fixes in the atlas's SQL classes replay as a fetched
oracle layer, identified by upstream id in a ledger. Where the oracle's answer
changed between supported majors, the ledger records which answer Pintail gives.

### P3 - A change-capture oracle

The specification: at any committed checkpoint, the replica holds exactly the
source's rows as of that checkpoint.

1. **Deterministic simulation.** Generated event streams - typed inserts,
   updates and deletes, multi-table transactions, schema changes, rotations,
   reconnects that replay a suffix, torn batches - drive the apply path and the
   store in process, checked after every step against an in-memory reference
   model. Seeds reproduce exactly and need no container.
2. **Live workload matrix.** The same grammar runs against real servers across
   supported versions, row-image and row-metadata settings; the oracle is an
   ordered dump of the source at the checkpoint.

### P4 - Storage under faults

1. The open intermittent SIGSEGV in the sort path is root-caused and fixed.
2. A fault layer under the store's file I/O injects out-of-space, I/O errors,
   short and torn writes, lost fsyncs and bit flips in cold files. Recovery
   must reproduce every acknowledged commit or refuse to open; it must never
   read corrupt data as rows.

### P5 - Reference evaluators

Slow, independent evaluators for DECIMAL, collation, temporal, ENUM/SET and
JSON semantics. Property tests compare every vectorized kernel against them in
process, so execution work can move without an oracle round trip.

### P6 - Verification farm

A runner on an idle host cycles SQL fuzz seeds, simulation seeds and suite
sweeps, minimizes each disagreement and appends it to a findings ledger that
becomes the next fix's regression case.

## What the replay found

The first burn-down (P1c, P1d) fixed defects the existing gates did not reach.

- **Wrong answers.** Numeric readings of DATE, DATETIME and TIME; packed and
  punctuated date strings; two-digit years; exact integer casts of text;
  TRUNCATE and DIV on large integers; `CAST AS BINARY(n)`; GREATEST and LEAST
  across types; text past the TIME range; character set introducers read as
  UTF-8; local ENUM, SET, JSON and DECIMAL values stored as written; local
  tables given the wrong collation; session settings (`div_precision_increment`,
  `sql_select_limit`, `time_zone = @@global.time_zone`) accepted and ignored.
- **Refusals of valid SQL.** User variables, legacy `utf8` collations, string
  positions past the 64-bit range, literal column defaults, hexadecimal and
  signed literals in local INSERTs.
- **Change-capture stalls.** A copy waiting without bound on a table another
  session locked (and a timed-out global lock leaving tables marked for flush);
  a probe row count waiting on a write lock; two source tables whose names
  differ only in case stopping the stream for the whole database.

Open: in one replayed file the stream went quiet, with no error, after a copy
that fell back to per-worker snapshots beside a table in use.
Open: after a resnapshot fails on a locked table, every table in the database
reports the error until the supervisor retries, and one replay run saw a table
re-created with a new schema fail change capture with a fingerprint mismatch
that a rerun of the same file did not reproduce.

## Gates added

| Gate | Profile | Ratchet |
|---|---|---|
| `mtr` | rc | per-file exact SELECTs |
| `cdc-sim` (unit) | development | seeds x steps, zero divergence |
| store fault fuzz (unit) | development | scenarios claimed and required |
| kernel references (unit) | development | domains covered |

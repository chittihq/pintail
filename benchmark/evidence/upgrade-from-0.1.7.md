# Upgrading real 0.1.7 and 0.1.6 replicas to 0.1.8-rc2

The lifecycle tests forge old segments. This run upgrades replicas the old
releases actually wrote: v0.1.7 (`8ba6ac08`) and v0.1.6 (`596a934c`),
built from their tags, each replicating from MySQL 8.4 with the e2e gate's
binlog flags (ROW, FULL images, MINIMAL row metadata, GTID on). Each old
directory was copied, and the new build started on the copy. Release
builds, each in its own target directory, on an 8-vCPU cloud host.

Two passes. The first ran dev `dc5a0c2d` (segment format 8, the
background segment upgrade, metadata migrations 24 and 25, audit and
dead-letter pruning, the startup resync advice). The second ran the final
tree, rebased on the in-place `ADD COLUMN` with defaults (fills recorded
in schema history) and carrying the four commits listed at the end, over
fresh old replicas whose schema history includes columns added in place.

## The source

An invented schema of four tables, 502,000 rows seeded:

| Table | Key | Rows | Covers |
|---|---|---:|---|
| `t_types` | `INT AUTO_INCREMENT` | 300,000 | TINYINT, SMALLINT UNSIGNED, signed and unsigned MEDIUMINT, INT, BIGINT, BIGINT UNSIGNED near its top, DECIMAL(14,3), DOUBLE, DATE and DATETIME with zero dates (2% and 2.5%) and NULLs, DATETIME(3), TIMESTAMP with NULLs and the zero TIMESTAMP, TIME past 24 h and negative, YEAR, ENUM with NULL, SET with the empty set, JSON, BINARY(8) (every value with trailing zero bytes), VARBINARY, mixed-case VARCHAR, TEXT, TINYINT(1) |
| `t_textpk` | `VARCHAR(32)` | 60,000 | mixed-case text keys, signed MEDIUMINT, BINARY(4), DATE with zero dates, DECIMAL |
| `t_composite` | `(CHAR(4), INT)` | 100,000 | composite key, ENUM, zero dates, DECIMAL |
| `t_keyless` | none | 42,000 | exact duplicate rows, zero DATETIMEs, signed MEDIUMINT (keyless policy `auto_resync`) |

Change traffic per phase: 2,000 `t_types` inserts with the same value mix;
updates negating MEDIUMINTs, rewriting BINARY values, setting zero dates
and ENUM and JSON members on about 1.3% of rows; deletes of 0.5%; a
transaction of text-key inserts, updates, key-changing updates and deletes;
composite-key inserts, updates and key moves; keyless inserts.

ALTERs applied under the old binary: `ADD COLUMN ... NOT NULL DEFAULT 1
AFTER` (both old releases recopy it), a nullable `ADD COLUMN` (evolved in
place) and, in the second pass, two `NOT NULL` columns added without a
default, an INT and a DATE (evolved in place by both old releases). After
the upgrade, three columns added with defaults (INT, VARCHAR, DATETIME),
which the new build evolves in place over the upgraded segments.

The differential set is 47 to 62 queries over all four tables (it grows
with the added columns), every answer compared byte for byte with MySQL's:
counts; a CRC32 checksum over every column of every row of each table; sums
over every numeric type; group-bys on DATE, `YEAR(DATE)`, `DATE(DATETIME)`,
`DATE_FORMAT`, ENUM, SET, a JSON member, BINARY and VARCHAR, including the
zero-date groups; `COUNT(DISTINCT)`; MIN and MAX of every temporal type;
point lookups by integer, text and composite key; zero-date filters;
`ORDER BY ... LIMIT` over DATETIME, DATE, MEDIUMINT and both composite key
directions; and the groups, MIN, MAX and distinct count of every added
column. Each comparison first waits until the four checksums agree
(replication caught up).

## v0.1.7 to the new build

v0.1.7 copied the source (47 of 47 equal) and applied eight change phases
and the ALTERs, then was stopped with SIGTERM (exit in 2 s) and the
directory copied: the *clean* copy. It was started again under a
40-second write load and killed with SIGKILL 18 s in: the *killed* copy,
its checkpoint well behind the source.

| Check | First pass, clean | First pass, killed | Final tree, clean | Final tree, killed |
|---|---|---|---|---|
| Metadata migrations | 24 -> 25 | 24 -> 25 | 24 -> 25 | 24 -> 25 |
| Resumed at the stored checkpoint | yes (`pos=383426963`) | yes (`pos=436700726`, GTID 699 of 1147) | yes (`pos=1024703993`) | yes (`pos=379984`) |
| Recopies at startup | none | none | none | none |
| Old segments at start | 7 | 8 | 8 | 9 |
| Queries while the sweep runs | 49 / 49 | 49 / 49 | 58 / 62 | 58 / 62 |
| Resync advice | none | none | `t_keyless` (`k_zero`), `t_types` (`c_dz`) | the same |
| After the advised resyncs | - | - | 62 / 62 | 62 / 62 |
| All segments format 8 | after 10 s | after 5 s | yes | yes |
| Columns added with defaults after the upgrade, then three change phases | - | - | 62 / 62 | 62 / 62 |
| After merges went idle | 49 / 49 | 49 / 49 | 62 / 62 | 62 / 62 |

**Pass.** Without the ALTERs (first pass) the advice is right to stay
silent: v0.1.7 decodes negative MEDIUMINTs and BINARY trailing zeros
correctly and records copy generation 1 for every table it copied.

The four answers that differ before the resync in the final pass are the
two `NOT NULL` columns v0.1.7 added in place without a default: MySQL
fills the type's implicit value (0, the zero date) into every row it held,
v0.1.7 recorded no fill, those rows read NULL (43,500 of 62,400 keyless
rows; 62,817 of 317,100 `t_types` rows), and its merges stored that NULL.
v0.1.7 answers the same way; the new build records fills only for columns
it adds itself, so an upgrade alone leaves those rows wrong. The final
tree names such tables at startup and in the snapshot status, and the
advised resync repairs them:

```
replication.resync_advised database=upg_db (<id>) table=t_keyless columns=k_zero (NOT NULL,
added in place): rows the source held when a NOT NULL column was added without a default hold
the type's implicit value there (0, the empty string, the zero date), but a binary older than
0.1.8 recorded no fill for it and they read NULL. Resync this table to repair it: ...
```

The nullable column and the recopied `NOT NULL DEFAULT` column are not
named, and neither are the three columns added with defaults after the
upgrade, which carry their fills; all of those answered right throughout.

The replicas are small (about 33 MB, seven to twelve old segments), so
the startup merges rewrote most old segments and the sweep the rest
(`segments_upgraded` 1 to 2); `/api/storage` reported zero old segments in
every run.

## Downgrade: v0.1.7 on an upgraded directory

Started on the upgraded clean copy of both passes, with the same result;
the three recovery attempts ran on the first.

| Check | Result |
|---|---|
| Refuses to start | **No.** v0.1.7 starts and serves. Its metadata check never runs on a file at or past its own version, so schema 25 is opened as if it were 24. |
| What the operator sees | Every table in state `error`, `CDC storage failed: corrupt segment <path> at byte 5: unsupported format version`, logged again every few seconds; every query answers `replica is not ready: table <t> cannot be read: corrupt segment ... unsupported format version` |
| Data files | Unchanged: every segment, manifest and log file hashes the same before and after; only the metadata file changed (table states and errors) |
| The new build started again on it | streaming, every answer equal (49 / 49, 62 / 62) |
| Forced snapshot on v0.1.7 | Accepted (202), never starts; tables stay in `error` |
| Table resync on v0.1.7 | Fails on the same segment and retries in a loop |
| Remove the database and add it again on v0.1.7 | Recopies; 49 / 49 equal |

**Partial.** Nothing is corrupted, but v0.1.7 does not refuse cleanly, and
re-snapshotting in place does not work. The working downgrade is to start
the older release on a copy of the directory taken before the upgrade, or
to remove the database and add it again.

Two commits make the next downgrade refuse at startup: `MetaStore::open`
now refuses a metadata schema ahead of the binary on every open (the
refusal used to sit where only files behind the current schema reached
it), and a segment ahead of the build's format says `format version N is
newer than this build reads (M); a newer Pintail release wrote it`. The
first-pass build with them, started on a copy of the upgraded directory
whose metadata was set to schema 26, exits with code 1 and changes no file:

```
Error: metadata schema version 26 is newer than this binary supports (25): a newer
Pintail release has upgraded this data directory, and an older one cannot run on it;
start the newer release, or restore a copy of the data directory taken before the upgrade
```

## v0.1.6 to the new build

v0.1.6 copied the source with two of its own answers wrong (`SUM` over
BIGINT UNSIGNED near its top raised `numeric expression overflow`;
`DATE_FORMAT` of a zero DATETIME answered NULL), both right in the new
build. After its change phases the negative MEDIUMINTs and BINARY values
its change decoder stored were wrong as well.

| Check | First pass, clean | First pass, killed | Final tree, clean | Final tree, killed |
|---|---|---|---|---|
| Metadata migrations | 23 -> 25 | 23 -> 25 | 23 -> 25 | 23 -> 25 |
| Resumed at the stored checkpoint | yes (`pos=638119926`) | yes (`pos=699416350`) | yes (`pos=176520707`) | yes (`pos=228469447`) |
| Queries before the advised resyncs | 37 / 49 | 35 / 49 | 42 / 56 | 41 / 62 |
| Resync advice | `t_types` (`c_med`, `c_bin`), `t_textpk` (`mi`, `b`), `t_keyless` (`med`); not `t_composite` | the same | as the first pass, plus `k_zero` on `t_keyless` and `c_dz` on `t_types` (reason `values_stored_wrong_before_upgrade`) | the same |
| After the advised resyncs | 49 / 49 | 49 / 49 | 56 / 56 | 62 / 62 |
| All segments format 8 | yes | yes | yes | yes |
| Three change phases after the upgrade | 48 / 49 (the defect below) | 48 / 49 (the same) | 62 / 62 | 62 / 62 |
| After merges went idle | 49 / 49 | 49 / 49 | 62 / 62 | 62 / 62 |

Every answer that differed before the resyncs involved a signed MEDIUMINT,
a BINARY column or a `NOT NULL` column added without a default, each one
named by the advice. **Pass.**

## A wrong answer found on the way: `ORDER BY <integer> LIMIT k` after updates

In the first pass, right after the post-upgrade change phases, `SELECT id,
c_med FROM t_types ORDER BY c_med, id LIMIT 20` returned a newly inserted
row in place of an older row with a smaller value; after the next merge it
was right again. Repeating "one change phase, then the query set" 25 times
reproduced it in 6 of 25 rounds, always that query.

The cause predates this release (the side-index narrowing of a limited
sort, in v0.1.7-rc1). The narrowing asks the side index for the value at
or before which the segments hold `k` entries and prunes the scan to it;
the sort read an unnarrowed twin only when fewer than `k` rows came back.
The bound counts rows a newer version supersedes, and rows past the bound
can still come back, so after updates moved most rows at the bound
elsewhere, rows past it made up the count and the sort kept them. The fix
keeps the narrowed answer only when its `k`-th row lies at or before the
bound. An in-process test that moves all but ten rows at each extreme
fails without it (after a flush) and passes with it; the same 25-round
loop with the fixed build had 25 of 25 rounds equal, and the final pass
saw no such difference.

## Commits

- `fix(meta): refuse a metadata file a newer release migrated`
- `fix(store): say a segment came from a newer release when its format is ahead`
- `fix(exec): trust a side-index narrowed limit only when its k-th row is within the bound`
- `feat(api): advise a resync for NOT NULL columns an older binary added without a fill`

Gate on the final tree: `cargo fmt --check` and `cargo clippy --workspace
--all-targets -D warnings` clean; `cargo nextest run --workspace` 2,098 of
2,098 passed; `validate.ts --stages=e2e,migrations,recovery` PASS (e2e
7,061 passed, 0 failed, 6 documented-gap warnings; migrations 230 passed;
recovery 301 passed).

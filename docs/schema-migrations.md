# Schema migrations under a live mirror

What an `ALTER TABLE` does to a replica, measured rather than assumed.

## The shape of the problem

A source `ALTER TABLE` reaches the mirror as one binlog entry: a `Query` event
carrying the statement text. Isolating a value-rewriting `ALTER` into a binlog
file of its own shows exactly that and nothing more — a format descriptor, a
GTID, the statement, a rotate. Zero row events, for a statement that changed
two of the three rows in the table.

So the mirror is told that the declaration changed and is told nothing about
the values. Everything then rests on one question: **did the source rewrite the
rows it already had?** If it did not, the mirror adopts the new declaration and
keeps streaming. If it did, every untouched row on the replica is stale and
stays stale, because nothing later in the stream refers to it.

The trap is that Pintail's mapped type answers that question wrongly for a
whole class of migrations. `DATETIME` and `TIMESTAMP` both map to
`DateTime64`; `VARCHAR(64)` and `VARCHAR(8)` are both `Utf8`; `BIGINT` and
`SMALLINT` are both integers in the same storage lane; a generated column keeps
its result type when its expression is replaced. Every one of those reads as
"same type, safe" and every one of them rewrites rows.

## What MySQL 8.4 actually does

Measured against `mysql:8.4` with `sql-mode=NO_ENGINE_SUBSTITUTION` — the
source's own default, and the setting under which a narrowing conversion clips
instead of failing. Each row is a seeded value, the migration, and what the
source read back afterwards.

| Migration | Before | After | Rewrites rows |
|---|---|---|---|
| `INT` → `BIGINT` | `2147483647` | `2147483647` | no |
| `BIGINT` → `SMALLINT` | `100000`, `-100000` | `32767`, `-32768` | **yes** |
| `INT` → `INT UNSIGNED` | `-5` | `0` | **yes** |
| `DECIMAL(10,2)` → `(14,4)` | `12.34` | `12.3400` | no |
| `DECIMAL(14,4)` → `(10,1)` | `12.3456`, `-99999999.99` | `12.3`, `-100000000.0` | **yes** |
| `DECIMAL(10,2)` → `DECIMAL(10,2) UNSIGNED` | `-5.25` | `0.00` | **yes** |
| `DOUBLE` → `DOUBLE UNSIGNED` | `-2.5` | `0` | **yes** |
| `DECIMAL(20,4)` → `DOUBLE` | `1234567890123456.1234` | `1.234567890123456e15` | **yes** |
| `FLOAT` → `DOUBLE` | `0.1` | `0.10000000149011612` | **yes** |
| `VARCHAR(64)` → `TEXT` → `LONGTEXT` | 26 characters | 26 characters | no |
| `VARCHAR(64)` → `VARCHAR(8)` | 26 characters | 8 characters | **yes** |
| `TEXT` → `TINYTEXT` | 400 characters | 255 characters | **yes** |
| `CHAR(10)` → `CHAR(4)` | `abcdefghij` | `abcd` | **yes** |
| `VARBINARY(16)` → `VARBINARY(4)` | `0011223344556677` | `00112233` | **yes** |
| `VARCHAR` → `INT` | `'42'`, `'abc'`, `'7.9'` | `42`, `0`, `8` | **yes** |
| `TEXT` → `BLOB` | `636166C383C2A9` | `636166C383C2A9` | no |
| `latin1` → `utf8mb4` | `0xE9` | `0xC3A9` (the same character) | no |
| `utf8mb4_0900_ai_ci` → `utf8mb4_bin` | `Apple`, `apple` | `Apple`, `apple` | no |
| `DATETIME` → `TIMESTAMP` (in range) | `2025-01-01 00:00:00` | `2025-01-01 00:00:00` | no |
| `DATETIME` → `TIMESTAMP` (out of range) | `1960-01-01`, `2099-01-01` | `0000-00-00 00:00:00` | **yes** |
| `TIMESTAMP` → `DATETIME` | `2025-06-01 12:00:00` | `2025-06-01 12:00:00` | no |
| `DATETIME(6)` → `DATETIME(0)` | `12:00:00.654321` | `12:00:01` | **yes** |
| `DATETIME` → `DATE` | `2025-06-01 12:34:56` | `2025-06-01` | **yes** |
| `ENUM` reordered | `alpha=1` | `alpha=3` (label kept, ordinal moved) | no |
| `ENUM` member appended | `alpha=1`, `beta=2` | `alpha=1`, `beta=2` | no |
| `ENUM` member dropped | `beta=2` | `''=0` | **yes** |
| `ENUM` member renamed | `draft=1` | `''=0` | **yes** |
| `SET` member appended | `a,b` | `a,b` | no |
| `SET` reordered | `b,c` | `c,b` | **yes** |
| `BIT(16)` → `BIT(4)` | `65535` | `15` | **yes** |
| `INT NULL` → `INT NOT NULL` | `NULL` | `0` | **yes** |
| Generated `base*2` → `base*100` | `20`, `40` | `1000`, `2000` | **yes** |
| Generated virtual `base+1` → `base+1000` | `11` | `1010` | **yes** |
| Display width `INT(11)` → `INT(4)` | `5` | `5` | no |

Two results are worth keeping in mind because they cut against the obvious
guess:

- **An `ENUM` converts by label, not by ordinal.** Reordering the members
  leaves every stored label alone and only moves its number, so a mirror that
  stores labels and reads its ordering from the refreshed declaration stays
  correct. It is *removing* a member — and renaming one, which is a removal and
  an addition — that turns every row holding it into the empty label.
- **A `SET` is the opposite.** Its membership survives a reorder, but it
  renders in declaration order, so the stored text `b,c` reads back as `c,b`.
  Only appending is inert.

## The policy

`pintail_probe::stabilize_source_table` is where a refreshed schema is adopted
onto a running table, and it now reads the source's declaration rather than the
mapped type alone (`crates/pintail-probe/src/migration.rs`). A change is
adopted in place only when the declaration says the source left stored values
alone:

- declarations must stay in the same family, where a family is what converts
  into itself without rewriting: signed integers, unsigned integers, decimals,
  `FLOAT`, `DOUBLE`, `BIT`, `DATE`, `DATETIME`, `TIMESTAMP`, `TIME`, `YEAR`,
  character data (`CHAR`/`VARCHAR`/the `TEXT` sizes), binary data, `ENUM`,
  `SET`, `JSON`, and anything unrecognised against its own spelling;
- signedness may not change: an integer wears it in its family, and a
  `DECIMAL` or a float wears it on the column type alone, but MySQL converts
  every negative value to zero either way;
- capacity may grow and never shrink — bits for integers and `BIT`, bytes for
  strings and binaries (character sets included, so `latin1` to `utf8mb4`
  widens), fractional-second digits for temporals, and both halves
  independently for a decimal;
- an `ENUM` may gain and reorder members but never lose one; a `SET` may only
  gain them, at the end;
- a generated expression may not change, and a column may not start or stop
  being generated;
- nullability may loosen and not tighten.

A refusal is not a failure. It marks the table `needs_resync`, the supervisor
recopies it, and the recopy reads the rewritten values correctly. The cost is a
full copy of a table where an in-place adoption would sometimes have been
correct anyway — a `VARCHAR` shrink every value already fits inside, a dropped
`ENUM` member no row used. The declaration is all the stream has; it cannot see
the source's rows, so it takes the reading that cannot corrupt.

## The gate

`tests/e2e/migrations.ts` runs every family above against a live mirror and
asks three questions per table, because only the first of them catches a
migration adopted in place:

1. do the rows nobody wrote to after the migration still read the way the
   source reads them,
2. do the writes issued after the migration land correctly, and
3. does the same table still agree after a restart, which reloads the replica
   from disk rather than from memory?

Checking only the rows written after the migration passes every case in this
document while the table is wrong.

`FLOAT` is the one family with no case. A `FLOAT` column does not mirror even
before a migration — the source renders `1234.5678` as `1234.57` and the
replica as `1234.5677` (`docs/limitations.md`) — so a table built on one could
only ever report that older divergence, never what the migration did to it.

## What the gate found

The same twenty-nine families, same source container, same harness, against
the binary before this change and the binary after it:

| | Checks passed | Checks failed |
|---|---|---|
| Before | 107 | 36 |
| After | 143 | 0 |

The thirty-six were twelve families failing all three questions — the
untouched rows were stale, the table did not match the source, and a restart
did not heal it because the stale values were on disk:

| Family | What the replica held | What the source held |
|---|---|---|
| integer width narrows | `100000` | `32767` |
| varchar capacity shrinks | 26 characters | 8 |
| text family shrinks | 400 characters | 255 |
| varbinary capacity shrinks | `0011223344556677` | `00112233` |
| bit width narrows | `65535` | `15` |
| datetime becomes timestamp | `1960-01-01 00:00:00` | `0000-00-00 00:00:00` |
| enum member is dropped | `beta` | `''` |
| enum member is renamed | `draft` | `''` |
| set members reorder | `b,c` | `c,b` |
| nullable becomes not null | `NULL` | `0` |
| stored generated expression changes | `20` | `1000` |
| virtual generated expression changes | `11` | `1010` |

Each of those now quarantines the table and the resync recopies it, which is
why the same run reports nothing.

Run it with `bun run tests/e2e/migrations.ts`; `PINTAIL_E2E_BINARY` points it
at an already-built binary, which is how one source serves a before-and-after
comparison of the same cases. The banked result is
`tests/e2e/results-migrations.md`.

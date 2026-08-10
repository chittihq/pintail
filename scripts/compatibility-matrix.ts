/// Generates the MySQL keyword and function compatibility matrix in
/// `parity.md`.
///
/// Every column is derived from a real inventory rather than from memory,
/// because a compatibility matrix is exactly the artifact people migrate on
/// and exactly the artifact that is easy to fill in plausibly and wrongly:
///
///   MySQL keywords   information_schema.KEYWORDS (with the RESERVED flag)
///   MySQL functions  mysql.help_topic joined to its Function/Operator
///                    categories - MySQL's own documentation catalogue
///   ClickHouse       system.functions and system.keywords
///   Pintail          the binder's own match arms, the same source
///                    scripts/function-surface.ts reads
///
/// The one column that is NOT machine-derived is Pintail's keyword support:
/// the binder has no keyword table to read, it either binds a construct or
/// rejects it. Those cells come from the curated lists below and are marked
/// as such in the output, so a reader knows which column carries a weaker
/// warranty than the others.
///
/// Run with:
///   DOCKER_HOST=... bun run scripts/compatibility-matrix.ts

import { readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const inventory = process.env.MATRIX_INVENTORY_DIR
if (!inventory) {
  throw new Error('MATRIX_INVENTORY_DIR must point at the extracted inventories')
}

/// Keyword families a read-only analytical replica cannot encounter by
/// design. Marked "n/a" rather than "no": they are out of scope, not gaps,
/// and conflating the two would make the matrix read as far worse than the
/// engine is.
const OUT_OF_SCOPE = [
  /^(CREATE|ALTER|DROP|RENAME|TRUNCATE)$/,
  /^(INSERT|UPDATE|DELETE|REPLACE|MERGE|UPSERT)$/,
  /^(GRANT|REVOKE|FLUSH|RESET|PURGE|SHUTDOWN|INSTALL|UNINSTALL|CLONE)$/,
  /^(MASTER|SLAVE|SOURCE|REPLICA|RELAY|BINLOG|GTID|CHANNEL)/,
  /^(TABLESPACE|PARTITION|SUBPARTITION|ENGINE|ENGINES|DATAFILE|UNDOFILE)/,
  /^(TRIGGER|TRIGGERS|EVENT|EVENTS|PROCEDURE|FUNCTION|ROUTINE|CURSOR|HANDLER)$/,
  /^(TRANSACTION|COMMIT|ROLLBACK|SAVEPOINT|LOCK|UNLOCK|XA)$/,
  /^(USER|USERS|ROLE|PASSWORD|PRIVILEGES|PROXY|AUTHENTICATION|ATTRIBUTE)$/,
  /^(BACKUP|RESTORE|REPAIR|OPTIMIZE|CHECKSUM|ANALYZE|CACHE|PRELOAD)$/,
  /^(PLUGIN|PLUGINS|COMPONENT|DAEMON|SONAME|LIBRARY)$/,
  /^(GEOMCOLLECTION|GEOMETRY|GEOMETRYCOLLECTION|LINESTRING|MULTILINESTRING|MULTIPOINT|MULTIPOLYGON|POINT|POLYGON|SRID|SPATIAL)$/,
]

/// Query-surface keywords Pintail documents as supported. Curated: the
/// binder has no keyword table, so this is the honest source. Cross-checked
/// against parity.md's Surface section.
const PINTAIL_KEYWORDS = new Set(
  `SELECT FROM WHERE GROUP BY HAVING ORDER LIMIT OFFSET DISTINCT DISTINCTROW ALL AS ON USING
   JOIN INNER LEFT RIGHT CROSS OUTER NATURAL STRAIGHT_JOIN
   UNION INTERSECT EXCEPT WITH RECURSIVE
   AND OR NOT XOR IN EXISTS BETWEEN LIKE RLIKE REGEXP IS NULL TRUE FALSE UNKNOWN
   CASE WHEN THEN ELSE END IF IFNULL NULLIF COALESCE
   CAST CONVERT USING CHAR BINARY DECIMAL SIGNED UNSIGNED DATE DATETIME TIME JSON NCHAR
   OVER WINDOW PARTITION ROWS RANGE GROUPS PRECEDING FOLLOWING UNBOUNDED CURRENT ROW
   ASC DESC INTERVAL DAY MONTH YEAR HOUR MINUTE SECOND QUARTER WEEK MICROSECOND
   COUNT SUM AVG MIN MAX ANY_VALUE SEPARATOR
   TABLE VALUES DUAL DEFAULT ESCAPE COLLATE CHARACTER SET
   TRUE FALSE ELSEIF`
    .split(/\s+/)
    .filter(Boolean),
)

function lines(file: string): string[] {
  return readFileSync(join(inventory, file), 'utf8')
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
}

/// Pintail's callable names, read from the binder's match arms - the same
/// extraction scripts/function-surface.ts performs.
function pintailFunctions(): Set<string> {
  const source = ['binder/mod.rs', 'binder/function.rs']
    .map((module) => readFileSync(join(repository, 'crates/pintail-sql/src', module), 'utf8'))
    .join('\n')
  const names = new Set<string>()
  for (const line of source.split('\n')) {
    // Three dispatch shapes, not one. Missing the matches!() guard form is
    // what made DATE_ADD look unsupported when it is bound at
    // binder/function.rs - a false negative in the direction that
    // understates the engine.
    const isMatchArm = line.includes('=>')
    const isGuard = line.includes('function_name') || line.includes('name.as_str()')
    if (!isMatchArm && !isGuard) continue
    const head = isMatchArm ? line.split('=>')[0] : line
    for (const match of head.matchAll(/"([A-Z][A-Z0-9_]*)"/g)) {
      names.add(match[1])
    }
  }
  return names
}

const mysqlKeywords = lines('mysql-keywords.tsv').map((line) => {
  const [word, reserved] = line.split('\t')
  return { word, reserved: reserved === '1' }
})
// MySQL's help catalogue lists operators alongside functions; keep only the
// identifier-shaped names, since an operator is not a callable.
const mysqlFunctions = lines('mysql-functions.txt').filter((name) => /^[A-Z][A-Z0-9_]*$/i.test(name))
const chFunctions = new Set(lines('ch-functions.txt').map((name) => name.toUpperCase()))
const chKeywords = new Set(lines('ch-keywords.txt').map((name) => name.toUpperCase()))
const ptFunctions = pintailFunctions()

const YES = 'yes'
const NO = 'no'
const NA = 'n/a'

function outOfScope(word: string): boolean {
  return OUT_OF_SCOPE.some((pattern) => pattern.test(word))
}

const functionRows = mysqlFunctions.map((name) => {
  const upper = name.toUpperCase()
  return {
    name: upper,
    pintail: ptFunctions.has(upper) ? YES : NO,
    clickhouse: chFunctions.has(upper) ? YES : NO,
  }
})

const keywordRows = mysqlKeywords.map(({ word, reserved }) => ({
  word,
  reserved: reserved ? 'reserved' : '',
  pintail: PINTAIL_KEYWORDS.has(word) ? YES : outOfScope(word) ? NA : NO,
  clickhouse: chKeywords.has(word) ? YES : NO,
}))

function tally(rows: Array<{ pintail: string; clickhouse: string }>) {
  const count = (rows: Array<{ pintail: string; clickhouse: string }>, key: 'pintail' | 'clickhouse', value: string) =>
    rows.filter((row) => row[key] === value).length
  return {
    total: rows.length,
    pintailYes: count(rows, 'pintail', YES),
    pintailNa: count(rows, 'pintail', NA),
    chYes: count(rows, 'clickhouse', YES),
  }
}

const functionTally = tally(functionRows)
const keywordTally = tally(keywordRows)

const section = [
  '## MySQL keyword and function matrix',
  '',
  'Generated by `bun run scripts/compatibility-matrix.ts`. Every column is',
  'read from a live inventory rather than written from memory, because a',
  'compatibility matrix is the artifact people migrate on:',
  '',
  '| Column | Source |',
  '|---|---|',
  '| MySQL keywords | `information_schema.KEYWORDS` on MySQL 8.4 |',
  '| MySQL functions | `mysql.help_topic` joined to its Function/Operator categories — MySQL\'s own documentation catalogue |',
  '| ClickHouse | `system.functions` and `system.keywords` on `clickhouse/clickhouse-server:25.8`, matched case-insensitively so its MySQL-compatible aliases count |',
  '| Pintail functions | the binder\'s own match arms, the same source `scripts/function-surface.ts` reads |',
  '| Pintail keywords | **curated, not machine-read** — the binder has no keyword table, it either binds a construct or rejects it |',
  '',
  'There is no MySQL support column: this is MySQL\'s own keyword and function',
  'inventory, so every row would read "yes" and the column would carry no',
  'information.',
  '',
  '**The ClickHouse column measures the name, not the capability.** ClickHouse',
  'implements much of this surface under different spellings: it answers `no`',
  'to `JSON_EXTRACT` while shipping 28 `JSONExtract*` functions, and `no` to',
  '`DATE_ADD` while shipping `date_diff` and the `toYear`/`toMonth` family. A',
  '`no` here means "not callable by the MySQL name", which is what matters for',
  'pointing an existing MySQL client at it - not "cannot do this".',
  '',
  '`n/a` marks a keyword a read-only analytical replica cannot encounter by',
  'design — DDL, DML writes, replication and administration. Those are out of',
  'scope rather than missing, and counting them as gaps would make this table',
  'read as far worse than the engine is.',
  '',
  `**Functions:** ${functionTally.total} MySQL functions — Pintail ${functionTally.pintailYes}, ClickHouse ${functionTally.chYes}.`,
  '',
  `**Keywords:** ${keywordTally.total} MySQL keywords — Pintail ${keywordTally.pintailYes} supported and ${keywordTally.pintailNa} out of scope, ClickHouse ${keywordTally.chYes}.`,
  '',
  '### Functions',
  '',
  '| Function | Pintail | ClickHouse |',
  '|---|---|---|',
  ...functionRows.map((row) => `| \`${row.name}\` | ${row.pintail} | ${row.clickhouse} |`),
  '',
  '### Keywords',
  '',
  '| Keyword | MySQL reserved | Pintail | ClickHouse |',
  '|---|---|---|---|',
  ...keywordRows.map((row) => `| \`${row.word}\` | ${row.reserved} | ${row.pintail} | ${row.clickhouse} |`),
  '',
]

const parityPath = join(repository, 'parity.md')
const existing = readFileSync(parityPath, 'utf8')
const marker = '## MySQL keyword and function matrix'
const head = existing.includes(marker) ? existing.slice(0, existing.indexOf(marker)) : `${existing.trimEnd()}\n\n`
writeFileSync(parityPath, head + section.join('\n'))

console.log(
  `functions: ${functionTally.total} (pintail ${functionTally.pintailYes}, clickhouse ${functionTally.chYes})`,
)
console.log(
  `keywords: ${keywordTally.total} (pintail ${keywordTally.pintailYes} yes / ${keywordTally.pintailNa} n-a, clickhouse ${keywordTally.chYes})`,
)

/// Differential gate for schema migrations under a live mirror.
///
/// An `ALTER TABLE` reaches a replica as one DDL statement and nothing else.
/// The source rewrites the stored values the change affects - clipping a
/// narrowed integer, truncating a shrunk string, zeroing a datetime that left
/// the epoch window, recomputing a generated column - and emits no row event
/// for any of it. A mirror that adopts the new declaration in place therefore
/// keeps the pre-ALTER values, and nothing later corrects them.
///
/// Every case here runs one migration family against a table that is already
/// streaming, and then asks the three questions that catch it:
///
///   1. do the rows nobody touched still read the way the source reads them,
///   2. do the writes that follow the migration land correctly, and
///   3. does a restart - which reloads the replica from what was written to
///      disk rather than from memory - still agree?
///
/// Checking only the rows written after the migration passes every one of
/// these cases while the table is wrong, which is precisely the failure this
/// gate exists to refuse.
///
/// Run: `bun run tests/e2e/migrations.ts`. `PINTAIL_E2E_BINARY` points it at an
/// already-built binary, which is how one source container serves a before and
/// after comparison of the same cases.
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import {
  command,
  docker,
  dockerHost,
  dsnHost,
  publishedPort,
  freePort,
  waitForMysql,
  diffRows,
} from './lib'

const repository = resolve(import.meta.dir, '..', '..')
const DATABASE = 'pintail_migrations'
const MYSQL_IMAGE = process.env.PINTAIL_E2E_MYSQL_IMAGE ?? 'mysql:8.4'
const CONTAINER = process.env.PINTAIL_MIGRATIONS_MYSQL ?? 'pintail-migrations-mysql'
const SUPERVISOR_MS = process.env.PINTAIL_E2E_SUPERVISOR_MS ?? '2500'
/// A refused migration quarantines the table and the supervisor resyncs it;
/// the copies here are a handful of rows, so the wait is for the cadence.
const CONVERGE_MS = Number(process.env.PINTAIL_MIGRATIONS_CONVERGE_MS ?? 90_000)

/// One migration family, as an operator would run it.
///
/// `rewrites` records what MySQL 8.4 was measured to do to the rows already
/// stored: `true` when the ALTER changes at least one of them. It is not an
/// assertion about Pintail - the replica has to match the source either way -
/// but it says which cases can only pass by noticing the rewrite.
type Case = {
  name: string
  /// Columns after `id`, as the table is first created.
  before: string
  /// The seed rows, `(id, value)` literals.
  rows: string[]
  /// The migration itself.
  alter: string
  /// Writes issued after the migration, through the new declaration: one
  /// INSERT and one UPDATE, so the row path is exercised on both sides of the
  /// change. `{t}` is the table.
  writes: string[]
  /// Whether the migration rewrites values already stored at the source.
  rewrites: boolean
  /// What to select when comparing, when `id, v` would compare a rendering
  /// difference rather than the migration.
  projection?: string
}

const cases: Case[] = [
  {
    name: 'integer width narrows',
    before: 'v BIGINT NULL',
    rows: ["(1, 100000)", "(2, -100000)", "(3, 7)"],
    alter: 'MODIFY v SMALLINT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 123)",
      "UPDATE {t} SET v = 456 WHERE id = 3",
    ],
    rewrites: true,
  },
  {
    name: 'integer width widens',
    before: 'v INT NULL',
    rows: ["(1, 2147483647)", "(2, -2147483648)"],
    alter: 'MODIFY v BIGINT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 9223372036854775807)",
      "UPDATE {t} SET v = -9223372036854775808 WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'signedness changes',
    before: 'v INT NULL',
    rows: ["(1, -5)", "(2, 5)"],
    alter: 'MODIFY v INT UNSIGNED NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 4294967295)",
      "UPDATE {t} SET v = 17 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'decimal digits shrink',
    before: 'v DECIMAL(14,4) NULL',
    rows: ["(1, 12.3456)", "(2, -9999999.9999)"],
    alter: 'MODIFY v DECIMAL(10,1) NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 5.5)",
      "UPDATE {t} SET v = -1.4 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'decimal digits grow',
    before: 'v DECIMAL(12,2) NULL',
    rows: ["(1, 12.34)", "(2, -9999999999.99)"],
    alter: 'MODIFY v DECIMAL(14,2) NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 123456789012.34)",
      "UPDATE {t} SET v = -0.01 WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'decimal becomes double',
    before: 'v DECIMAL(20,4) NULL',
    rows: ["(1, 1234567890123456.1234)", "(2, -0.5)"],
    alter: 'MODIFY v DOUBLE NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 0.5)",
      "UPDATE {t} SET v = 2.25 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'float becomes double',
    before: 'v FLOAT NULL',
    rows: ["(1, 0.1)", "(2, 1234.5678)"],
    alter: 'MODIFY v DOUBLE NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 0.25)",
      "UPDATE {t} SET v = 7.5 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'varchar capacity shrinks',
    before: 'v VARCHAR(64) NULL',
    rows: ["(1, 'abcdefghijklmnopqrstuvwxyz')", "(2, 'short')"],
    alter: 'MODIFY v VARCHAR(8) NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'newvalue')",
      "UPDATE {t} SET v = 'tiny' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'varchar becomes longtext',
    before: 'v VARCHAR(64) NULL',
    rows: ["(1, 'abcdefghijklmnopqrstuvwxyz')", "(2, 'emoji 🦆 café')"],
    alter: 'MODIFY v LONGTEXT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, REPEAT('y', 600))",
      "UPDATE {t} SET v = 'replaced 🦆' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'text family shrinks',
    before: 'v TEXT NULL',
    rows: ["(1, REPEAT('x', 400))", "(2, 'small')"],
    alter: 'MODIFY v TINYTEXT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'fresh')",
      "UPDATE {t} SET v = 'rewritten' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'varchar becomes int',
    before: 'v VARCHAR(16) NULL',
    rows: ["(1, '42')", "(2, 'abc')", "(3, '7.9')"],
    alter: 'MODIFY v INT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 11)",
      "UPDATE {t} SET v = 12 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'text becomes blob',
    before: 'v TEXT CHARACTER SET utf8mb4 NULL',
    rows: ["(1, 'café')", "(2, 'plain')"],
    alter: 'MODIFY v BLOB NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, X'00FF10')",
      "UPDATE {t} SET v = X'DEADBEEF' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'latin1 becomes utf8mb4',
    before: 'v VARCHAR(32) CHARACTER SET latin1 NULL',
    rows: ["(1, _latin1 0xE9)", "(2, 'plain')"],
    alter: 'MODIFY v VARCHAR(32) CHARACTER SET utf8mb4 NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'après 🦆')",
      "UPDATE {t} SET v = 'changé' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'collation becomes case sensitive',
    before: 'v VARCHAR(32) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NULL',
    rows: ["(1, 'Apple')", "(2, 'apple')"],
    alter: 'MODIFY v VARCHAR(32) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'APPLE')",
      "UPDATE {t} SET v = 'ApPlE' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'datetime becomes timestamp',
    before: 'v DATETIME NULL',
    // Two of these sit outside the epoch window the source stores a TIMESTAMP
    // in, and the conversion zeroes them.
    rows: ["(1, '1960-01-01 00:00:00')", "(2, '2099-01-01 00:00:00')", "(3, '2025-01-01 00:00:00')"],
    alter: 'MODIFY v TIMESTAMP NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, '2030-03-03 03:03:03')",
      "UPDATE {t} SET v = '2001-02-03 04:05:06' WHERE id = 3",
    ],
    rewrites: true,
  },
  {
    name: 'timestamp becomes datetime',
    before: 'v TIMESTAMP NULL',
    rows: ["(1, '2025-06-01 12:00:00')", "(2, '1999-12-31 23:59:59')"],
    alter: 'MODIFY v DATETIME NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, '1935-01-01 00:00:00')",
      "UPDATE {t} SET v = '2050-01-01 00:00:00' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'datetime loses fractional seconds',
    before: 'v DATETIME(6) NULL',
    rows: ["(1, '2025-06-01 12:00:00.654321')", "(2, '2000-01-01 00:00:00.000001')"],
    alter: 'MODIFY v DATETIME(0) NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, '2025-07-07 07:07:07')",
      "UPDATE {t} SET v = '2026-08-08 08:08:08' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'datetime becomes date',
    before: 'v DATETIME NULL',
    rows: ["(1, '2025-06-01 12:34:56')", "(2, '1999-11-11 11:11:11')"],
    alter: 'MODIFY v DATE NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, '2027-09-09')",
      "UPDATE {t} SET v = '2028-10-10' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'enum members reorder',
    before: "v ENUM('alpha','beta','gamma') NULL",
    rows: ["(1, 'alpha')", "(2, 'beta')", "(3, 'gamma')"],
    alter: "MODIFY v ENUM('gamma','beta','alpha') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'beta')",
      "UPDATE {t} SET v = 'gamma' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'enum member is dropped',
    before: "v ENUM('alpha','beta','gamma') NULL",
    rows: ["(1, 'alpha')", "(2, 'beta')", "(3, 'gamma')"],
    alter: "MODIFY v ENUM('alpha','gamma') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'gamma')",
      "UPDATE {t} SET v = 'alpha' WHERE id = 3",
    ],
    rewrites: true,
  },
  {
    name: 'enum member is renamed',
    before: "v ENUM('draft','sent') NULL",
    rows: ["(1, 'draft')", "(2, 'sent')"],
    alter: "MODIFY v ENUM('pending','sent') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'pending')",
      "UPDATE {t} SET v = 'pending' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'enum member is appended',
    before: "v ENUM('alpha','beta') NULL",
    rows: ["(1, 'alpha')", "(2, 'beta')"],
    alter: "MODIFY v ENUM('alpha','beta','gamma') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'gamma')",
      "UPDATE {t} SET v = 'gamma' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'set members reorder',
    before: "v SET('a','b','c') NULL",
    rows: ["(1, 'a')", "(2, 'b,c')", "(3, 'a,b,c')"],
    alter: "MODIFY v SET('c','b','a') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'a,c')",
      "UPDATE {t} SET v = 'b,c' WHERE id = 3",
    ],
    rewrites: true,
  },
  {
    name: 'set member is appended',
    before: "v SET('a','b') NULL",
    rows: ["(1, 'a,b')", "(2, 'b')"],
    alter: "MODIFY v SET('a','b','c') NULL",
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 'a,c')",
      "UPDATE {t} SET v = 'a,b,c' WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'bit width narrows',
    before: 'v BIT(16) NULL',
    rows: ["(1, b'1111111111111111')", "(2, b'1')"],
    alter: 'MODIFY v BIT(4) NULL',
    // A BIT column reaches a MySQL client as raw bytes and reaches a Pintail
    // client as an integer (docs/limitations.md), so the comparison asks both
    // sides for the number and keeps this case about the migration.
    projection: 'id, v + 0 AS v',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, b'1010')",
      "UPDATE {t} SET v = b'11' WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'nullable becomes not null',
    before: 'v INT NULL',
    rows: ['(1, NULL)', '(2, 5)'],
    alter: 'MODIFY v INT NOT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, 9)",
      "UPDATE {t} SET v = 8 WHERE id = 2",
    ],
    rewrites: true,
  },
  {
    name: 'not null becomes nullable',
    before: 'v INT NOT NULL',
    rows: ['(1, 1)', '(2, 5)'],
    alter: 'MODIFY v INT NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, NULL)",
      "UPDATE {t} SET v = NULL WHERE id = 2",
    ],
    rewrites: false,
  },
  {
    name: 'varbinary capacity shrinks',
    before: 'v VARBINARY(16) NULL',
    rows: ["(1, X'0011223344556677')", "(2, X'01')"],
    alter: 'MODIFY v VARBINARY(4) NULL',
    writes: [
      "INSERT INTO {t} (id, v) VALUES (90, X'AABB')",
      "UPDATE {t} SET v = X'CC' WHERE id = 2",
    ],
    rewrites: true,
  },
]

/// Generated columns need a base column to compute from, so they are their own
/// shape rather than a `Case`.
const generatedCases = [
  {
    name: 'stored generated expression changes',
    kind: 'STORED',
    before: '(base * 2)',
    after: '(base * 100)',
  },
  {
    name: 'virtual generated expression changes',
    kind: 'VIRTUAL',
    before: '(base + 1)',
    after: '(base + 1000)',
  },
]

type Check = { table: string; family: string; check: string; status: 'PASS' | 'FAIL'; detail?: string }
const results: Check[] = []
const started = Date.now()

function log(message: string) {
  console.log(`[migrations +${((Date.now() - started) / 1000).toFixed(1)}s] ${message}`)
}

function record(table: string, family: string, check: string, status: Check['status'], detail?: string) {
  results.push({ table, family, check, status, detail })
  // A failure's whole difference goes to the log: the first line of it says
  // which row disagreed and nothing about how.
  log(`${status}  ${table} · ${check}`)
  if (detail) console.log(detail.split('\n').map((line) => `    ${line}`).join('\n'))
}

let mysqlConnection: mysql.Connection | undefined
let pintailWire: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailBinary = ''
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''
let wireSecret = ''
let databaseId = ''
let mysqlStarted = false

async function sql(statement: string) {
  await mysqlConnection!.query(statement)
}

async function mysqlRows(statement: string): Promise<unknown[][]> {
  const [rows] = await mysqlConnection!.query<mysql.RowDataPacket[]>({
    sql: statement,
    rowsAsArray: true,
  })
  return rows as unknown as unknown[][]
}

async function api<T>(path: string, options: { method?: string; body?: unknown; auth?: boolean } = {}): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(options.auth === false ? {} : { Authorization: `Bearer ${token}` }),
      ...(options.body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} returned ${response.status}: ${text}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

async function pintailQuery(statement: string): Promise<unknown[][]> {
  for (let attempt = 0; ; attempt += 1) {
    if (!pintailWire) {
      pintailWire = await mysql.createConnection({
        host: '127.0.0.1',
        port: pintailWirePort,
        user: DATABASE,
        password: wireSecret,
        database: DATABASE,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
        typeCast: (field, next) =>
          field.type === 'VAR_STRING' || field.type === 'STRING' || field.type === 'BLOB'
            ? field.buffer()
            : next(),
      })
    }
    const connection = pintailWire
    try {
      const [rows] = await connection.query<mysql.RowDataPacket[]>({ sql: statement, rowsAsArray: true })
      return rows as unknown as unknown[][]
    } catch (error) {
      if (/still being copied/.test(String(error)) && attempt < 360) {
        await Bun.sleep(500)
        continue
      }
      if (/ECONNREFUSED|ECONNRESET|EPIPE|closed state|Connection lost/i.test(String(error))) {
        pintailWire = undefined
        try {
          await connection.end()
        } catch {}
        if (attempt < 4) {
          await Bun.sleep(1000)
          continue
        }
      }
      throw error
    }
  }
}

/// Compares one table against the source until they agree or the window runs
/// out, and returns the surviving difference.
async function converge(table: string, columns: string, deadlineMs = CONVERGE_MS): Promise<string | undefined> {
  const statement = `SELECT ${columns} FROM ${table} ORDER BY id`
  const deadline = Date.now() + deadlineMs
  let last: string | undefined = 'never compared'
  for (;;) {
    const expected = await mysqlRows(statement)
    try {
      last = diffRows(expected, await pintailQuery(statement))
      if (last === undefined) return undefined
    } catch (error) {
      last = String(error)
    }
    if (Date.now() > deadline) return last
    await Bun.sleep(1000)
  }
}

/// The rows the migration did not write to, compared on their own: a mirror
/// that only replays post-migration events passes every other check here and
/// fails this one.
async function compareUntouched(table: string, columns: string, ids: number[]): Promise<string | undefined> {
  const statement = `SELECT ${columns} FROM ${table} WHERE id IN (${ids.join(',')}) ORDER BY id`
  const expected = await mysqlRows(statement)
  const actual = await pintailQuery(statement)
  return diffRows(expected, actual)
}

async function startPintail() {
  pintailWire = undefined
  pintailProcess = Bun.spawn(
    [pintailBinary, '--data-dir', pintailDataDir, '--http-bind', `127.0.0.1:${pintailHttpPort}`, '--wire-bind', `127.0.0.1:${pintailWirePort}`],
    {
      cwd: repository,
      stdout: 'inherit',
      stderr: 'inherit',
      env: { ...process.env, PINTAIL_SUPERVISOR_INTERVAL_MS: SUPERVISOR_MS, _RJEM_MALLOC_CONF: 'dirty_decay_ms:0,muzzy_decay_ms:0' },
    },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy within 120 seconds')
}

async function stopPintail() {
  if (!pintailProcess) return
  try {
    await pintailWire?.end()
  } catch {}
  pintailWire = undefined
  pintailProcess.kill('SIGTERM')
  await pintailProcess.exited
  pintailProcess = undefined
}

function tableName(index: number) {
  return `m_${String(index).padStart(2, '0')}`
}

async function main() {
  const host = await dockerHost()
  await docker('rm', '--force', '--volumes', CONTAINER).catch(() => undefined)
  log(`starting ${MYSQL_IMAGE} as ${CONTAINER}`)
  await docker(
    'run', '--detach', '--name', CONTAINER,
    '--publish', '0:3306',
    '--tmpfs', '/var/lib/mysql:rw,size=2g',
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env', `MYSQL_DATABASE=${DATABASE}`,
    MYSQL_IMAGE,
    '--server-id=943',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--binlog-row-metadata=MINIMAL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    // The source's own default. Under it a narrowing ALTER clips instead of
    // failing, which is exactly the shape this gate is about.
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(CONTAINER, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(`GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`)

  // Row 1 carries the evidence: it is seeded before the migration and no
  // later write names it, so a difference there is the migration's doing and
  // nothing else. An edit that broke that would turn a stale-row check into a
  // check of the write that overwrote the staleness.
  for (const testCase of cases) {
    if (!testCase.rows.some((row) => row.startsWith('(1,'))) {
      throw new Error(`${testCase.name} does not seed the reserved row 1`)
    }
    const touched = testCase.writes.find((write) => /\bid\s*=\s*1\b/.test(write))
    if (touched) throw new Error(`${testCase.name} writes to the reserved row 1: ${touched}`)
  }

  log('seeding one table per migration family')
  for (const [index, testCase] of cases.entries()) {
    const table = tableName(index)
    await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, ${testCase.before}) DEFAULT CHARACTER SET utf8mb4`)
    await sql(`INSERT INTO ${table} (id, v) VALUES ${testCase.rows.join(', ')}`)
  }
  for (const [index, generated] of generatedCases.entries()) {
    const table = `g_${index}`
    await sql(
      `CREATE TABLE ${table} (id INT PRIMARY KEY, base INT NOT NULL, ` +
        `v INT GENERATED ALWAYS AS ${generated.before} ${generated.kind}) DEFAULT CHARACTER SET utf8mb4`,
    )
    await sql(`INSERT INTO ${table} (id, base) VALUES (1, 10), (2, 20)`)
  }

  pintailBinary = process.env.PINTAIL_E2E_BINARY
    ? resolve(process.env.PINTAIL_E2E_BINARY)
    : await (async () => {
        log('building the release pintail binary')
        await command(['cargo', 'build', '--release', '-p', 'pintail'])
        const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'], { quiet: true })
        return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
      })()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-migrations-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  await startPintail()

  token = (
    await api<{ token: string }>('/api/auth/setup', {
      method: 'POST',
      auth: false,
      body: { email: 'migrations@pintail.local', password: 'migrations-gate-password' },
    })
  ).token
  databaseId = (
    await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: {
        name: DATABASE,
        dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${DATABASE}`,
        mode: 'cdc',
      },
    })
  ).id
  wireSecret = (
    await api<{ secret: string }>(`/api/databases/${databaseId}/api-keys`, {
      method: 'POST',
      body: { name: 'migrations-gate', scopes: ['query', 'read'] },
    })
  ).secret
  await api(`/api/databases/${databaseId}/probe`)
  await api(`/api/databases/${databaseId}/snapshot`, { method: 'POST', body: { force: false } })
  for (let attempt = 0; ; attempt += 1) {
    const status = await api<{ state: string; tables: Array<{ last_error?: string }> }>(
      `/api/databases/${databaseId}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(`snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`)
    }
    if (status.state === 'streaming' || status.state === 'polling') break
    if (attempt > 600) throw new Error('snapshot did not complete within ten minutes')
    await Bun.sleep(1000)
  }
  log('streaming; every table mirrored before the first migration')

  // The mirror has to be right before the migrations start, or a later
  // difference proves nothing about them.
  for (const [index, testCase] of cases.entries()) {
    const table = tableName(index)
    const difference = await converge(table, testCase.projection ?? 'id, v', 45_000)
    record(table, testCase.name, 'mirrors the source before the migration', difference ? 'FAIL' : 'PASS', difference)
  }

  log('running one migration per table, with live writes after each')
  const untouched = new Map<string, number[]>()
  for (const [index, testCase] of cases.entries()) {
    const table = tableName(index)
    // Keep the first seeded row out of every later write: its value is the one
    // only the source rewrote, and the only evidence of a migration the mirror
    // adopted when it should not have.
    untouched.set(table, [1])
    await sql(`ALTER TABLE ${table} ${testCase.alter}`)
    for (const write of testCase.writes) await sql(write.replaceAll('{t}', table))
  }
  for (const [index, generated] of generatedCases.entries()) {
    const table = `g_${index}`
    untouched.set(table, [1])
    await sql(`ALTER TABLE ${table} MODIFY v INT GENERATED ALWAYS AS ${generated.after} ${generated.kind}`)
    await sql(`INSERT INTO ${table} (id, base) VALUES (3, 30)`)
    await sql(`UPDATE ${table} SET base = 21 WHERE id = 2`)
  }

  const families = [
    ...cases.map((testCase, index) => ({
      table: tableName(index),
      name: testCase.name,
      columns: testCase.projection ?? 'id, v',
    })),
    ...generatedCases.map((generated, index) => ({
      table: `g_${index}`,
      name: generated.name,
      columns: 'id, base, v',
    })),
  ]
  log('checking every table against the source after its migration')
  for (const family of families) {
    const difference = await converge(family.table, family.columns)
    record(family.table, family.name, 'the whole table matches the source after the migration', difference ? 'FAIL' : 'PASS', difference)
    const stale = await compareUntouched(family.table, family.columns, untouched.get(family.table)!)
    record(family.table, family.name, 'rows the migration never wrote to are not stale', stale ? 'FAIL' : 'PASS', stale)
  }

  log('restarting the replica and reading every table back from disk')
  await stopPintail()
  await startPintail()
  for (const family of families) {
    const difference = await converge(family.table, family.columns)
    record(family.table, family.name, 'the table still matches the source after a restart', difference ? 'FAIL' : 'PASS', difference)
  }

  publish()
}

function publish() {
  const failed = results.filter((result) => result.status === 'FAIL')
  const byFamily = new Map<string, Check[]>()
  for (const result of results) {
    byFamily.set(result.family, [...(byFamily.get(result.family) ?? []), result])
  }
  const lines = [
    '# Pintail schema-migration differential gate',
    '',
    `Measured ${new Date().toISOString()} against \`${MYSQL_IMAGE}\`.`,
    '',
    `**${results.length - failed.length} passed, ${failed.length} failed.**`,
    '',
    '| Family | Check | Status | Detail |',
    '|---|---|---|---|',
    ...results.map(
      (result) =>
        `| ${result.family} | ${result.check} | ${result.status} | ${(result.detail ?? '')
          .split('\n')[0]
          .slice(0, 140)
          .replaceAll('|', '\\|')} |`,
    ),
    '',
  ]
  const report = lines.join('\n')
  Bun.write(process.env.PINTAIL_MIGRATIONS_REPORT ?? join(repository, 'tests/e2e/results-migrations.md'), report)
  console.log(report)
  if (failed.length) {
    console.error(`${failed.length} migration checks failed`)
    process.exitCode = 1
  }
}

async function teardown() {
  await stopPintail().catch(() => undefined)
  try {
    await mysqlConnection?.end()
  } catch {}
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
  if (mysqlStarted && process.env.PINTAIL_MIGRATIONS_KEEP !== '1') {
    await docker('rm', '--force', '--volumes', CONTAINER).catch(() => undefined)
  }
}

try {
  await main()
} catch (error) {
  console.error(error)
  process.exitCode = 1
} finally {
  await teardown()
}

// Filter-level benchmark: the WHERE clauses a reporting workload leans on,
// timed on MySQL 8.4 carrying the secondary indexes a production schema
// would, and on Pintail replicating the same table - with every answer
// compared, so a fast wrong result cannot pass.
//
//   DOCKER_HOST=ssh://<docker host> bun run run-filters.ts          # 20M rows
//   FILTER_ROWS=200000 bun run run-filters.ts                       # smoke
//
// The table is appended in time order - created_at rises with id, the way an
// application writing rows as events happen fills it - and also carries
// scheduled_at, scattered across the same span, so a column that tracks
// insertion order and one that does not are both measured. After the
// snapshot a mutation pass updates one row in a hundred, deletes one in a
// thousand and appends new rows at the end of the timeline, so Pintail
// answers from the layered state a replica keeps while its source is written
// to, not from a freshly loaded one.
//
// Environment:
//   FILTER_ROWS           table size (default 20,000,000)
//   FILTER_VARIANTS       distinct constants per filter (default 5)
//   PINTAIL_FILTER_IMAGE  reuse a built Pintail image instead of building one

import { writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const benchmarkDir = import.meta.dir
const repository = resolve(benchmarkDir, '..')
const rows = Number(process.env.FILTER_ROWS ?? 20_000_000)
if (!Number.isInteger(rows) || rows < 10_000 || rows % 10_000 !== 0) {
  throw new Error('FILTER_ROWS must be a multiple of 10,000')
}
const variantsPerFilter = Number(process.env.FILTER_VARIANTS ?? 5)
const fullScale = rows === 20_000_000
const runId = `pintail-filters-${process.pid}-${Date.now()}`
const mysqlName = `${runId}-mysql`
const pintailName = `${runId}-pintail`
const networkName = `${runId}-network`
const runVolume = `${runId}-mysql-data`
const pintailVolume = `${runId}-pintail-data`
const engineLimits = ['--cpus', '8', '--memory', '8g']
const mysqlImage = 'mysql:8.4'
// A buffer pool that holds the table and its indexes: MySQL is measured the
// way a well-provisioned production server runs it, not starved of memory.
const mysqlServerArgs = [
  '--server-id=911',
  '--log-bin=mysql-bin',
  '--binlog-format=ROW',
  '--binlog-row-image=FULL',
  '--binlog-row-metadata=FULL',
  '--gtid-mode=ON',
  '--enforce-gtid-consistency=ON',
  '--default-time-zone=+00:00',
  '--sql-mode=NO_ENGINE_SUBSTITUTION',
  '--innodb-buffer-pool-size=4G',
]
/// Seconds between consecutive rows' created_at.
const STEP_SECONDS = 2
const EPOCH_MS = Date.UTC(2024, 0, 1)
const SPAN_SECONDS = rows * STEP_SECONDS
const APPENDED_ROWS = Math.min(100_000, rows / 10)
const STATES = ['queued', 'sent', 'read', 'failed', 'held']

const schemaSql = `
CREATE TABLE digits (d INT PRIMARY KEY);
INSERT INTO digits VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9);
CREATE TABLE activity_log (
  id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
  account_id INT UNSIGNED NOT NULL,
  state VARCHAR(16) NOT NULL,
  channel VARCHAR(16) NOT NULL,
  amount DECIMAL(10,2) NOT NULL,
  note VARCHAR(64) NULL,
  created_at DATETIME NOT NULL,
  scheduled_at DATETIME NOT NULL,
  updated_at DATETIME NOT NULL,
  KEY idx_activity_created (created_at),
  KEY idx_activity_scheduled (scheduled_at),
  KEY idx_activity_account (account_id),
  KEY idx_activity_state (state)
) ENGINE=InnoDB;
CREATE PROCEDURE seed_activity(IN total_rows INT)
BEGIN
  DECLARE done INT DEFAULT 0;
  SET autocommit = 0;
  WHILE done < total_rows DO
    INSERT INTO activity_log
      (id, account_id, state, channel, amount, note, created_at, scheduled_at, updated_at)
    SELECT
      g,
      1 + MOD(g * 17, 200000),
      ELT(1 + MOD(g, 5), ${STATES.map((state) => `'${state}'`).join(', ')}),
      ELT(1 + MOD(g * 3, 4), 'email', 'sms', 'push', 'webhook'),
      ROUND(MOD(g * 7919, 100000) / 100, 2),
      IF(MOD(g, 10) = 0, NULL, CONCAT('note-', MOD(g, 997))),
      DATE_ADD('2024-01-01', INTERVAL g * ${STEP_SECONDS} SECOND),
      DATE_ADD('2024-01-01', INTERVAL MOD(g * 7919, ${SPAN_SECONDS}) SECOND),
      DATE_ADD('2024-01-01', INTERVAL g * ${STEP_SECONDS} SECOND)
    FROM (
      SELECT done + a.d * 1000 + b.d * 100 + c.d * 10 + e.d + 1 AS g
      FROM digits a CROSS JOIN digits b CROSS JOIN digits c CROSS JOIN digits e
    ) numbered;
    SET done = done + 10000;
    IF MOD(done, 1000000) = 0 THEN
      COMMIT;
    END IF;
  END WHILE;
  COMMIT;
  SET autocommit = 1;
END;
`
const fingerprint = createHash('sha256')
  .update(JSON.stringify({ schemaSql, rows, mysqlImage, mysqlServerArgs, engineLimits }))
  .digest('hex')
  .slice(0, 12)
const seedVolume = `pintail-filter-seed-${fingerprint}`

const log = (message: string) => console.log(`[filters] ${message}`)

async function run(args: string[], options: { allowFailure?: boolean } = {}): Promise<string> {
  const child = Bun.spawn(args, { cwd: repository, stdout: 'pipe', stderr: 'pipe' })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0 && !options.allowFailure) {
    throw new Error(`${args.join(' ')} failed (${status}): ${stderr.trim()}`)
  }
  return stdout.trim()
}
const docker = (...args: string[]) => run(['docker', ...args])

async function dockerHostAddress(): Promise<string> {
  const endpoint = process.env.DOCKER_HOST?.trim() ?? ''
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const config = await run(['ssh', '-G', target])
  const hostname = config.split('\n').find((line) => line.startsWith('hostname '))?.slice(9)
  if (!hostname) throw new Error('cannot resolve the docker host address')
  return hostname
}

async function publishedPort(container: string, port: number): Promise<number> {
  const output = await docker('port', container, `${port}/tcp`)
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`no published port ${port} on ${container}`)
  return Number(match[1])
}

async function volumeExists(name: string): Promise<boolean> {
  const output = await docker('volume', 'ls', '--quiet', '--filter', `name=^${name}$`)
  return output.split('\n').includes(name)
}

const clientOptions = {
  supportBigNumbers: true,
  bigNumberStrings: true,
  dateStrings: true,
  multipleStatements: true,
  enableKeepAlive: true,
} as const

async function connectMysql(host: string, port: number, attempts = 600): Promise<mysql.Connection> {
  let last: unknown
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        ...clientOptions,
      })
    } catch (error) {
      last = error
      await Bun.sleep(1000)
    }
  }
  throw new Error(`MySQL never accepted connections: ${last}`)
}

async function api<T>(
  baseUrl: string,
  path: string,
  options: { method?: string; token?: string; body?: unknown } = {},
): Promise<T> {
  const response = await fetch(`${baseUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      'content-type': 'application/json',
      ...(options.token ? { authorization: `Bearer ${options.token}` } : {}),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${path} returned ${response.status}: ${text}`)
  return (text ? JSON.parse(text) : undefined) as T
}

async function waitForHttp(baseUrl: string) {
  for (let attempt = 0; attempt < 600; attempt += 1) {
    try {
      if ((await fetch(`${baseUrl}/health`)).ok) return
    } catch {
      // not up yet
    }
    await Bun.sleep(1000)
  }
  throw new Error('Pintail never became healthy')
}

// ---------- the filters ----------

/// A DATETIME literal `seconds` after 2024-01-01 00:00:00.
function at(seconds: number): string {
  return new Date(EPOCH_MS + seconds * 1000).toISOString().slice(0, 19).replace('T', ' ')
}

/// The start of the `index`th window: spread across the span so no two
/// variants read the same rows, aligned to the hour.
function windowStart(index: number, lengthSeconds: number): number {
  const fraction = (0.13 + 0.17 * index) % 1
  const start = Math.floor(((SPAN_SECONDS - lengthSeconds) * fraction) / 3600) * 3600
  return Math.max(0, start)
}

const HOUR = 3600
const DAY = 86_400

type Filter = {
  name: string
  /// Whether the answer's row order is part of the result.
  ordered: boolean
  sql: (variant: number) => string
}

const filters: Filter[] = [
  {
    name: 'created_at, one hour: count',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, HOUR)
      return `SELECT COUNT(*) AS n FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + HOUR)}'`
    },
  },
  {
    name: 'created_at, one day: count and sum',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, DAY)
      return `SELECT COUNT(*) AS n, SUM(amount) AS total FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + DAY)}'`
    },
  },
  {
    name: 'created_at, one day: newest 50 rows',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, DAY)
      return `SELECT id, account_id, state, channel, amount, note, created_at FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + DAY)}' ORDER BY created_at DESC, id DESC LIMIT 50`
    },
  },
  {
    name: 'created_at, one day: every row, three columns',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, DAY)
      return `SELECT id, state, amount FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + DAY)}' ORDER BY id`
    },
  },
  {
    name: 'created_at, thirty days: per state',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, 30 * DAY)
      return `SELECT state, COUNT(*) AS n, SUM(amount) AS total FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + 30 * DAY)}' GROUP BY state ORDER BY state`
    },
  },
  {
    name: 'created_at, one day, and a state',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, DAY)
      return `SELECT COUNT(*) AS n FROM activity_log WHERE created_at >= '${at(start)}' AND created_at < '${at(start + DAY)}' AND state = 'failed'`
    },
  },
  {
    name: 'created_at from a moment: first 100 rows',
    ordered: true,
    sql: (v) =>
      `SELECT id, state, amount, created_at FROM activity_log WHERE created_at >= '${at(windowStart(v, DAY))}' ORDER BY created_at, id LIMIT 100`,
  },
  {
    name: 'scheduled_at (scattered), one day: count',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, DAY)
      return `SELECT COUNT(*) AS n FROM activity_log WHERE scheduled_at >= '${at(start)}' AND scheduled_at < '${at(start + DAY)}'`
    },
  },
  {
    name: 'account_id point: every row',
    ordered: true,
    sql: (v) =>
      `SELECT id, state, amount, created_at FROM activity_log WHERE account_id = ${1 + ((v * 40_503) % 200_000)} ORDER BY id`,
  },
  {
    name: 'account_id IN 100 ids: count',
    ordered: true,
    sql: (v) => {
      const ids = Array.from({ length: 100 }, (_, index) => 1 + ((v * 7_919 + index * 1_987) % 200_000))
      return `SELECT COUNT(*) AS n FROM activity_log WHERE account_id IN (${ids.join(', ')})`
    },
  },
  {
    name: 'primary key point',
    ordered: true,
    sql: (v) =>
      `SELECT id, account_id, state, amount, note, created_at FROM activity_log WHERE id = ${1 + Math.floor((rows - 1) * ((0.11 + 0.19 * v) % 1))}`,
  },
  {
    name: 'primary key range of 10,000: sum',
    ordered: true,
    sql: (v) => {
      const start = 1 + Math.floor((rows - 10_000) * ((0.07 + 0.23 * v) % 1))
      return `SELECT COUNT(*) AS n, SUM(amount) AS total FROM activity_log WHERE id BETWEEN ${start} AND ${start + 9_999}`
    },
  },
  {
    name: 'state = an updated value: count',
    ordered: true,
    sql: (v) =>
      `SELECT COUNT(*) AS n FROM activity_log WHERE state = 'archived' AND account_id > ${v * 1_000}`,
  },
  {
    name: 'amount range, no index anywhere: count',
    ordered: true,
    sql: (v) =>
      `SELECT COUNT(*) AS n FROM activity_log WHERE amount BETWEEN ${100 + v * 137}.00 AND ${101 + v * 137}.50`,
  },
  {
    name: 'DATE(created_at) = a day: count',
    ordered: true,
    sql: (v) =>
      `SELECT COUNT(*) AS n FROM activity_log WHERE DATE(created_at) = '${at(windowStart(v, DAY)).slice(0, 10)}'`,
  },
  {
    name: 'note LIKE a prefix, and a channel: count',
    ordered: true,
    sql: (v) =>
      `SELECT COUNT(*) AS n FROM activity_log WHERE note LIKE 'note-${10 + v}%' AND channel = 'sms'`,
  },
  {
    name: 'note IS NULL within a week: count',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, 7 * DAY)
      return `SELECT COUNT(*) AS n FROM activity_log WHERE note IS NULL AND created_at >= '${at(start)}' AND created_at < '${at(start + 7 * DAY)}'`
    },
  },
  {
    name: 'states, amount and a month together: count',
    ordered: true,
    sql: (v) => {
      const start = windowStart(v, 30 * DAY)
      return `SELECT COUNT(*) AS n FROM activity_log WHERE state IN ('failed', 'held') AND amount > 900 AND created_at >= '${at(start)}' AND created_at < '${at(start + 30 * DAY)}'`
    },
  },
]

// ---------- measurement ----------

type Query = (sql: string) => Promise<unknown[][]>

function makeQuery(connect: () => Promise<mysql.Connection>, database?: string): Query {
  let connection: mysql.Connection | undefined
  return async (sql: string) => {
    connection ??= await connect()
    if (database) await connection.query(`USE ${database}`)
    database = undefined
    const [result] = await connection.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
    return result as unknown as unknown[][]
  }
}

function canonical(result: unknown[][], ordered: boolean): string {
  const lines = result.map((row) => JSON.stringify(row.map((value) => (value === null ? null : String(value)))))
  if (!ordered) lines.sort()
  return lines.join('\n')
}

async function timed(query: Query, sql: string): Promise<{ ms: number; result: unknown[][] }> {
  const started = performance.now()
  const result = await query(sql)
  return { ms: performance.now() - started, result }
}

type Sample = { variant: number; mysqlMs: number; pintailMs: number; rows: number; exact: boolean }

const median = (values: number[]) => {
  const sorted = [...values].sort((left, right) => left - right)
  return sorted[Math.floor(sorted.length / 2)] ?? 0
}

async function measure(filter: Filter, mysqlQuery: Query, pintailQuery: Query): Promise<Sample[]> {
  const samples: Sample[] = []
  for (let variant = 0; variant < variantsPerFilter; variant += 1) {
    const sql = filter.sql(variant)
    // Each engine answers the statement twice and keeps the second: the
    // first pays for cold pages on either side, and the statement is new to
    // both, so neither replays a remembered answer. Alternating which engine
    // goes first keeps drift on the host from landing on one of them.
    const engines: Array<['mysql' | 'pintail', Query]> = [
      ['mysql', mysqlQuery],
      ['pintail', pintailQuery],
    ]
    if (variant % 2 === 1) engines.reverse()
    const answers: Record<string, { ms: number; result: unknown[][] }> = {}
    for (const [name, query] of engines) {
      await query(sql)
      answers[name] = await timed(query, sql)
    }
    const expected = canonical(answers.mysql.result, filter.ordered)
    const actual = canonical(answers.pintail.result, filter.ordered)
    if (expected !== actual) {
      const left = expected.split('\n')
      const right = actual.split('\n')
      let first = 0
      while (first < left.length && left[first] === right[first]) first += 1
      log(`MISMATCH ${filter.name} variant ${variant}: ${sql}`)
      log(`  rows: mysql ${left.length}, pintail ${right.length}; first difference at row ${first}`)
      log(`  mysql:   ${left.slice(first, first + 3).join(' | ')}`)
      log(`  pintail: ${right.slice(first, first + 3).join(' | ')}`)
    }
    samples.push({
      variant,
      mysqlMs: answers.mysql.ms,
      pintailMs: answers.pintail.ms,
      rows: answers.mysql.result.length,
      exact: expected === actual,
    })
  }
  return samples
}

/// The index MySQL chose for a statement, or "full scan".
async function mysqlAccess(mysqlQuery: Query, sql: string): Promise<string> {
  const plan = await mysqlQuery(`EXPLAIN FORMAT=TRADITIONAL ${sql}`)
  // Columns: id, select_type, table, partitions, type, possible_keys, key, ...
  const row = plan[0] ?? []
  const type = String(row[4] ?? '')
  const key = row[6] === null || row[6] === undefined ? '' : String(row[6])
  if (type === 'ALL') return 'full scan'
  return key ? `${key} (${type})` : type
}

// ---------- orchestration ----------

let cleanupDone = false
async function cleanup() {
  if (cleanupDone) return
  cleanupDone = true
  await docker('rm', '--force', '--volumes', mysqlName, pintailName).catch(() => undefined)
  await docker('network', 'rm', networkName).catch(() => undefined)
  await docker('volume', 'rm', runVolume).catch(() => undefined)
  await docker('volume', 'rm', pintailVolume).catch(() => undefined)
}

async function main() {
  const head = await run(['git', 'rev-parse', '--short', 'HEAD'])
  const dirty = (await run(['git', 'status', '--porcelain'])).length > 0
  log(`${rows.toLocaleString()} rows at ${head}${dirty ? ' (uncommitted changes)' : ''}`)
  await docker('network', 'create', networkName)
  await docker('volume', 'create', runVolume)
  const seeded = await volumeExists(seedVolume)
  if (seeded) {
    log(`restoring the seeded datadir from ${seedVolume}`)
    await docker('run', '--rm', '--volume', `${seedVolume}:/from:ro`, '--volume', `${runVolume}:/to`, 'alpine:3', 'sh', '-c', 'cp -a /from/. /to/')
  }
  await docker('pull', '--quiet', mysqlImage)
  await docker(
    'run', '--detach', '--name', mysqlName, '--network', networkName, '--publish', '0:3306',
    ...engineLimits, '--volume', `${runVolume}:/var/lib/mysql`,
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', mysqlImage, ...mysqlServerArgs,
  )
  const host = await dockerHostAddress()
  let mysqlPort = await publishedPort(mysqlName, 3306)
  let admin = await connectMysql(host, mysqlPort)
  if (!seeded) {
    log(`seeding ${rows.toLocaleString()} rows`)
    const started = performance.now()
    await admin.query('SET SESSION sql_log_bin=0')
    await admin.query('CREATE DATABASE filter_db')
    await admin.query('USE filter_db')
    await admin.query(schemaSql)
    await admin.query('CALL seed_activity(?)', [rows])
    await admin.query('DROP PROCEDURE seed_activity')
    await admin.query("CREATE USER IF NOT EXISTS 'bench'@'%' IDENTIFIED BY 'benchpass'")
    await admin.query(
      "GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'bench'@'%'",
    )
    log(`seeded in ${Math.round((performance.now() - started) / 1000)} s; caching the datadir`)
    await admin.end()
    await docker('stop', '--timeout', '600', mysqlName)
    await docker('volume', 'create', seedVolume)
    await docker('run', '--rm', '--volumes-from', mysqlName, '--volume', `${seedVolume}:/to`, 'alpine:3', 'sh', '-c', 'cp -a /var/lib/mysql/. /to/')
    await docker('start', mysqlName)
    mysqlPort = await publishedPort(mysqlName, 3306)
    admin = await connectMysql(host, mysqlPort, 1200)
  }
  await admin.query('USE filter_db')

  const image = process.env.PINTAIL_FILTER_IMAGE ?? 'pintail-filter-bench:latest'
  if (!process.env.PINTAIL_FILTER_IMAGE) {
    log('building the Pintail image on the docker host')
    await docker('build', '--tag', image, repository)
  }
  await docker(
    'run', '--detach', '--name', pintailName, '--network', networkName,
    '--publish', '0:8080', '--publish', '0:3306', '--volume', `${pintailVolume}:/var/lib/pintail`,
    ...engineLimits,
    '--env', `PINTAIL_QUERY_MEMORY_LIMIT_BYTES=${4 * 1024 * 1024 * 1024}`,
    // Every statement executes: the benchmark measures filtering, not the
    // settled-answer memo a repeated dashboard query would hit.
    '--env', 'PINTAIL_DISABLE_SETTLED_MEMO=1',
    image,
  )
  const pintailUrl = `http://${host}:${await publishedPort(pintailName, 8080)}`
  await waitForHttp(pintailUrl)
  const setup = await api<{ token: string }>(pintailUrl, '/api/auth/setup', {
    method: 'POST',
    body: { email: 'filters@pintail.local', password: 'filter-benchmark-run' },
  })
  const database = await api<{ id: string }>(pintailUrl, '/api/databases', {
    method: 'POST',
    token: setup.token,
    body: {
      name: 'filter_db',
      dsn: `mysql://bench:benchpass@${mysqlName}:3306/filter_db`,
      mode: 'cdc',
      include_tables: ['activity_log'],
    },
  })
  await api(pintailUrl, `/api/databases/${database.id}/probe`, { token: setup.token })
  await api(pintailUrl, `/api/databases/${database.id}/snapshot`, {
    method: 'POST',
    token: setup.token,
    body: { force: false },
  })
  const snapshotStarted = performance.now()
  for (;;) {
    const status = await api<{ state: string; tables: Array<{ name: string; rows: number; last_error?: string }> }>(
      pintailUrl, `/api/databases/${database.id}/snapshot/status`, { token: setup.token },
    )
    if (status.state === 'error') throw new Error(`snapshot failed: ${JSON.stringify(status.tables)}`)
    const copied = status.tables.find((table) => table.name === 'activity_log')?.rows ?? 0
    if ((status.state === 'streaming' || status.state === 'polling') && copied === rows) break
    await Bun.sleep(2000)
  }
  log(`snapshot complete in ${Math.round((performance.now() - snapshotStarted) / 1000)} s`)

  const key = await api<{ secret: string }>(pintailUrl, `/api/databases/${database.id}/api-keys`, {
    method: 'POST',
    token: setup.token,
    body: { name: 'filters', scopes: ['query'] },
  })
  const pintailWire = { host, port: await publishedPort(pintailName, 3306) }
  const pintailQuery = makeQuery(() =>
    mysql.createConnection({
      host: pintailWire.host,
      port: pintailWire.port,
      user: 'filter_db',
      password: key.secret,
      database: 'filter_db',
      ...clientOptions,
    }),
  )
  const mysqlQuery = makeQuery(() => connectMysql(host, mysqlPort), 'filter_db')

  // The source keeps being written to: one row in a hundred changes state,
  // one in a thousand goes away, and new rows arrive after the last one.
  log('mutating the source: updates, deletes, appended rows')
  const chunk = 1_000_000
  for (let low = 1; low <= rows; low += chunk) {
    const high = Math.min(rows, low + chunk - 1)
    await admin.query(
      `UPDATE activity_log SET state = 'archived', updated_at = '2025-06-01 00:00:00' WHERE id BETWEEN ${low} AND ${high} AND id % 100 = 7`,
    )
    await admin.query(`DELETE FROM activity_log WHERE id BETWEEN ${low} AND ${high} AND id % 1000 = 3`)
  }
  await admin.query(
    `INSERT INTO activity_log (id, account_id, state, channel, amount, note, created_at, scheduled_at, updated_at) ` +
      `SELECT id + ${rows}, account_id, 'queued', channel, amount, note, ` +
      `DATE_ADD(created_at, INTERVAL ${SPAN_SECONDS} SECOND), scheduled_at, ` +
      `DATE_ADD(updated_at, INTERVAL ${SPAN_SECONDS} SECOND) FROM activity_log WHERE id <= ${APPENDED_ROWS}`,
  )
  const convergence = "SELECT COUNT(*), SUM(state = 'archived'), MAX(id), SUM(amount) FROM activity_log"
  const expectedState = canonical(await mysqlQuery(convergence), true)
  const convergeStarted = performance.now()
  for (;;) {
    const actual = canonical(await pintailQuery(convergence), true)
    if (actual === expectedState) break
    if (performance.now() - convergeStarted > 30 * 60_000) {
      throw new Error(`replica never converged: mysql ${expectedState}, pintail ${actual}`)
    }
    await Bun.sleep(5000)
  }
  log(`replica converged ${Math.round((performance.now() - convergeStarted) / 1000)} s after the mutations`)

  const results: Array<{
    name: string
    access: string
    rows: number
    mysqlMedianMs: number
    mysqlMinMs: number
    pintailMedianMs: number
    pintailMinMs: number
    exact: boolean
    samples: Sample[]
  }> = []
  for (const filter of filters) {
    const samples = await measure(filter, mysqlQuery, pintailQuery)
    const access = await mysqlAccess(mysqlQuery, filter.sql(0))
    const row = {
      name: filter.name,
      access,
      rows: median(samples.map((sample) => sample.rows)),
      mysqlMedianMs: median(samples.map((sample) => sample.mysqlMs)),
      mysqlMinMs: Math.min(...samples.map((sample) => sample.mysqlMs)),
      pintailMedianMs: median(samples.map((sample) => sample.pintailMs)),
      pintailMinMs: Math.min(...samples.map((sample) => sample.pintailMs)),
      exact: samples.every((sample) => sample.exact),
      samples,
    }
    results.push(row)
    log(
      `${filter.name}: mysql ${row.mysqlMedianMs.toFixed(1)} ms (${access}), ` +
        `pintail ${row.pintailMedianMs.toFixed(1)} ms, ${row.exact ? 'exact' : 'MISMATCH'}`,
    )
  }

  const measuredAt = new Date().toISOString()
  const exactAll = results.every((row) => row.exact)
  const ms = (value: number) => (value < 10 ? value.toFixed(1) : Math.round(value).toLocaleString())
  const lines = [
    '# Filter-level benchmark: Pintail against MySQL',
    '',
    `Measured ${measuredAt} at \`${head}\`${dirty ? ' (uncommitted changes)' : ''} with ${rows.toLocaleString()} rows.`,
    '',
    'Both engines run on the docker host under identical limits (8 CPUs, 8 GB). MySQL 8.4',
    'has a 4 GB buffer pool and secondary indexes on `created_at`, `scheduled_at`,',
    '`account_id` and `state`, the way a production schema carries them; Pintail replicates',
    'the same table over CDC and runs every statement (its settled-answer memo is off).',
    '',
    '`created_at` rises with the primary key, as it does in a table an application appends',
    'to; `scheduled_at` is scattered across the same span. After the snapshot the source',
    'updated one row in a hundred, deleted one in a thousand and appended',
    `${APPENDED_ROWS.toLocaleString()} rows, and the replica converged before timing began, so`,
    'Pintail answers from the layered state a replica keeps while its source is written to.',
    '',
    `Each filter runs ${variantsPerFilter} distinct constants; each engine answers each statement twice`,
    'and the second is timed, over the MySQL wire protocol from the same client host. Every',
    'answer is compared row for row.',
    '',
    '| Filter | Rows back | MySQL access | MySQL median | MySQL min | Pintail median | Pintail min | Pintail vs MySQL | Exact |',
    '|---|---:|---|---:|---:|---:|---:|---:|:--|',
    ...results.map(
      (row) =>
        `| ${row.name} | ${row.rows.toLocaleString()} | ${row.access} | ${ms(row.mysqlMedianMs)} ms | ${ms(row.mysqlMinMs)} ms | ` +
        `${ms(row.pintailMedianMs)} ms | ${ms(row.pintailMinMs)} ms | ` +
        `${(row.mysqlMedianMs / row.pintailMedianMs).toFixed(2)}× | ${row.exact ? 'yes' : '**no**'} |`,
    ),
    '',
    `Every answer exact: ${exactAll ? 'yes' : '**no**'}. A ratio above 1 means Pintail answered faster.`,
    '',
  ]
  const suffix = fullScale ? '' : '-smoke'
  writeFileSync(join(benchmarkDir, `results-filters${suffix}.md`), lines.join('\n'))
  writeFileSync(
    join(benchmarkDir, `results-filters${suffix}.json`),
    `${JSON.stringify({ measuredAt, commit: head, dirty, rows, variantsPerFilter, results }, null, 2)}\n`,
  )
  log(`wrote results-filters${suffix}.md`)
  await admin.end()
  if (!exactAll) process.exitCode = 1
}

try {
  await main()
} catch (error) {
  console.error(error)
  process.exitCode = 1
} finally {
  await cleanup()
  console.log(process.exitCode ? 'FILTER-BENCH-FAIL' : 'FILTER-BENCH-DONE')
}

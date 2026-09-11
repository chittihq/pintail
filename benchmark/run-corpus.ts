#!/usr/bin/env bun
// Times every case of the MySQL differential oracle corpus on MySQL,
// ClickHouse and Pintail, at the fixture's own size and at amplified sizes.
//
// The corpus is the oracle's exported outcomes file (cases plus fixture
// SQL), so the queries timed here are exactly the ones the oracle proves
// correct. All three engines run as containers on one docker host under the
// same CPU and memory limits, and the client reaches each over the network:
// MySQL and Pintail through the MySQL protocol, ClickHouse over HTTP.
//
// Amplification copies every fixture row `scale` times with its primary key
// (and `user_id`, the one cross-table key) shifted per copy, so joins keep
// their per-copy fan-out and grouping keeps its cardinality. Ordered cases
// can tie across copies, so above scale 1 answers are compared as bags.
//
// Usage:
//   DOCKER_HOST=ssh://... bun run benchmark/run-corpus.ts \
//     --corpus validate-out/oracle-outcomes.json --scales 1,10000
//   [--runs 3] [--warmups 1] [--timeout-ms 10000] [--out benchmark/corpus]
//   [--pintail-image tag] [--families substring] [--limit N] [--keep] [--trace]
//
// Only results.csv is tracked. To rebuild it from a run's local JSON:
//   bun run benchmark/run-corpus.ts --csv-from benchmark/corpus/results.json

import mysql from 'mysql2/promise'
import { createHash } from 'node:crypto'
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { canonicalValue, docker, dockerHost, publishedPort, waitForMysql } from '../tests/e2e/lib.ts'

const repository = resolve(import.meta.dir, '..')
const args = process.argv.slice(2)
const option = (name: string, fallback?: string) => {
  const index = args.indexOf(name)
  if (index < 0) return fallback
  const value = args[index + 1]
  if (value === undefined || value.startsWith('--')) throw new Error(`${name} needs a value`)
  return value
}
const corpusPath = resolve(repository, option('--corpus', 'validate-out/oracle-outcomes.json')!)
const scales = option('--scales', '1,10000')!.split(',').map(Number)
const RUNS = Number(option('--runs', '3'))
const WARMUPS = Number(option('--warmups', '1'))
const TIMEOUT_MS = Number(option('--timeout-ms', '10000'))
// A result past this many rows is recorded as too large, not timed: at
// amplified scales some joins return tens of millions of rows, and
// buffering them measures the client, and exhausted its memory once.
const MAX_ROWS = Number(option('--max-rows', '1000000'))
// A warmup this slow gets one measured run: the median of three multi-second
// runs says little more than one, and the corpus has to finish.
const SLOW_MS = 2_000
const outDir = resolve(repository, option('--out', 'benchmark/corpus')!)
const familyFilter = option('--families')
const limit = option('--limit') ? Number(option('--limit')) : undefined
const keep = args.includes('--keep')
// Records Pintail's per-statement phase trace and writes trace-s<scale>.json:
// where each case's time went between the statement's arrival and its
// encoded response. Tracing adds per-statement bookkeeping, so a traced run
// attributes time; the untraced run is the one to compare engines with.
const trace = args.includes('--trace')
const TRACE_PATH = '/tmp/pintail-query-trace.log'
const SEED = 0x5eed

const runId = `pintail-corpus-${Date.now().toString(36)}`
const mysqlName = `${runId}-mysql`
const clickhouseName = `${runId}-clickhouse`
const pintailName = `${runId}-pintail`
const networkName = `${runId}-network`
const mysqlImage = 'mysql:8.4'
const clickhouseImage = 'clickhouse/clickhouse-server:26.8'
const pintailImage = option('--pintail-image') ?? `${runId}-image`
const buildImage = option('--pintail-image') === undefined
const engineLimits = ['--cpus', '8', '--memory', '8g']
const sourceUser = 'corpus'
const sourcePassword = 'corpus-bench-pass'
const clickhousePassword = 'corpus-bench'
// Row ids in the fixture are small; each copy moves its keys this far.
const KEY_STRIDE = 1_000_000

type Case = {
  id: string
  family: string
  sql: string
  sqlMode: string
  collation: string
  timeZone: string
  ordered: boolean
}
type Status = 'ok' | 'error' | 'timeout' | 'too-large'
type Timing = {
  status: Status
  medianMs?: number
  minMs?: number
  samples: number[]
  rows?: number
  error?: string
  digest?: string
  /// The rows, kept in memory to classify a difference; never written.
  kept?: Rows
}
type Outcome = {
  id: string
  family: string
  sql: string
  ordered: boolean
  mysql: Timing
  pintail: Timing
  clickhouse: Timing
  parity: { pintail: 'equal' | 'differs' | 'n/a'; clickhouse: 'equal' | 'differs' | 'n/a' }
}
type Engine = 'mysql' | 'pintail' | 'clickhouse'
type Rows = unknown[][]

function log(message: string) {
  console.log(`[corpus] ${message}`)
}

function mulberry32(seed: number) {
  let state = seed >>> 0
  return () => {
    state = (state + 0x6d2b79f5) >>> 0
    let t = state
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

function median(values: number[]): number {
  const sorted = [...values].sort((a, b) => a - b)
  const middle = Math.floor(sorted.length / 2)
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2
}

function geomean(values: number[]): number {
  if (values.length === 0) return Number.NaN
  return Math.exp(values.reduce((sum, value) => sum + Math.log(value), 0) / values.length)
}

/// One row as a comparable string. Numbers that both engines spell
/// differently (1.50 against 1.5) meet on their numeric value.
function rowKey(row: unknown[]): string {
  return row
    .map((value) => {
      const text = canonicalValue(value)
      const number = Number(text)
      return text !== '' && Number.isFinite(number) && /^-?[\d.]+(e[-+]?\d+)?$/i.test(text)
        ? `n:${number}`
        : text
    })
    .join('')
}

function digest(rows: Rows, ordered: boolean): string {
  const keys = rows.map(rowKey)
  if (!ordered) keys.sort()
  return createHash('sha256').update(keys.join('')).digest('hex').slice(0, 24)
}

/// A trailing LIMIT, which a difference's classification removes to see
/// the whole answer the LIMIT chose from.
const LIMIT_TAIL = /\s+LIMIT\s+\d+(\s*(,|OFFSET)\s*\d+)?\s*$/i

/// Why Pintail's answer differs from MySQL's: `order` when the rows are
/// the same in another order, `group_concat` when they are equal once
/// each cell's comma-separated members are sorted, `limit` when every row
/// Pintail returned is in MySQL's answer without the LIMIT, `other`
/// otherwise.
function classify(mysql: Rows, pintail: Rows, full: Rows | undefined): string {
  const bag = (rows: Rows) => JSON.stringify(rows.map(rowKey).sort())
  if (bag(mysql) === bag(pintail)) return 'order'
  const members = (rows: Rows) =>
    rows.map((row) => row.map((value) => canonicalValue(value).split(',').sort().join(',')))
  if (bag(members(mysql)) === bag(members(pintail))) return 'group_concat'
  if (full && mysql.length === pintail.length) {
    const available = new Map<string, number>()
    for (const row of full) available.set(rowKey(row), (available.get(rowKey(row)) ?? 0) + 1)
    const drawn = pintail.every((row) => {
      const left = available.get(rowKey(row)) ?? 0
      available.set(rowKey(row), left - 1)
      return left > 0
    })
    if (drawn) return 'limit'
  }
  return 'other'
}

function isTooLarge(message: string): boolean {
  return /row cap|more than \d+ rows|PINTAIL_MAX_RESULT_ROWS|TOO_MANY_ROWS|Limit for result exceeded/i.test(message)
}

function isTimeout(message: string): boolean {
  return /maximum statement execution time|max_execution_time|TIMEOUT_EXCEEDED|Timeout exceeded|client timeout/i.test(
    message,
  )
}

async function withClientTimeout<T>(work: Promise<T>, ms: number): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined
  const guard = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error('client timeout')), ms)
  })
  try {
    return await Promise.race([work, guard])
  } finally {
    clearTimeout(timer)
  }
}

/// The first column of the first row.
async function scalar(connection: mysql.Connection, sql: string): Promise<string> {
  const [rows] = await connection.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
  return String((rows as unknown as unknown[][])[0][0])
}

async function api<T>(
  baseUrl: string,
  path: string,
  options: { method?: string; token?: string; body?: unknown } = {},
): Promise<T> {
  const response = await fetch(`${baseUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(options.token ? { Authorization: `Bearer ${options.token}` } : {}),
      ...(options.body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} returned ${response.status}: ${text}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

/// A MySQL-protocol session that remembers its settings, so a case pays
/// for a SET only when it needs a different one, and never inside a timing.
class WireSession {
  private connection?: mysql.Connection
  private applied = new Map<string, string>()
  constructor(
    private readonly name: string,
    private readonly connect: () => Promise<mysql.Connection>,
  ) {}

  async ensure(settings: Record<string, string>) {
    this.connection ??= await this.connect()
    for (const [statement, value] of Object.entries(settings)) {
      if (this.applied.get(statement) === value) continue
      try {
        await this.connection.query(value)
      } catch (error) {
        log(`${this.name}: '${value}' was refused (${String(error).slice(0, 160)})`)
      }
      this.applied.set(statement, value)
    }
  }

  async query(sql: string, timeoutMs: number): Promise<Rows> {
    this.connection ??= await this.connect()
    const connection = this.connection
    try {
      const [rows] = await withClientTimeout(
        connection.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true }),
        timeoutMs,
      )
      return Array.isArray(rows) ? (rows as unknown as Rows) : []
    } catch (error) {
      if (String(error).includes('client timeout') || /closed|ECONNRESET|PROTOCOL/i.test(String(error))) {
        // The server may still be working; a fresh session starts clean.
        connection.destroy()
        this.connection = undefined
        this.applied.clear()
      }
      throw error
    }
  }

  close() {
    this.connection?.destroy()
  }
}

function connectOptions(host: string, port: number, user: string, password: string, database: string) {
  return () =>
    mysql.createConnection({
      host,
      port,
      user,
      password,
      database,
      supportBigNumbers: true,
      bigNumberStrings: true,
      dateStrings: true,
      enableKeepAlive: true,
      keepAliveInitialDelay: 10_000,
    })
}

async function clickhouseQuery(baseUrl: string, database: string, sql: string, timeoutMs: number): Promise<Rows> {
  const params = new URLSearchParams({
    database,
    default_format: 'JSONCompact',
    max_execution_time: String(Math.ceil(timeoutMs / 1000)),
    timeout_overflow_mode: 'throw',
    // MySQL's outer joins fill the missing side with NULL, not defaults.
    join_use_nulls: '1',
    use_query_cache: '0',
    max_result_rows: String(MAX_ROWS),
    result_overflow_mode: 'throw',
  })
  const response = await fetch(`${baseUrl}/?${params}`, {
    method: 'POST',
    headers: { Authorization: `Basic ${btoa(`default:${clickhousePassword}`)}` },
    body: sql,
    signal: AbortSignal.timeout(timeoutMs + 5_000),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(text.trim().slice(0, 400))
  if (!text.trim()) return []
  return (JSON.parse(text) as { data: Rows }).data
}

async function clickhouseStatement(baseUrl: string, sql: string, settings: Record<string, string> = {}) {
  const response = await fetch(`${baseUrl}/?${new URLSearchParams(settings)}`, {
    method: 'POST',
    headers: { Authorization: `Basic ${btoa(`default:${clickhousePassword}`)}` },
    body: sql,
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`ClickHouse: ${text.trim().slice(0, 400)}`)
  return text
}

async function waitFor(what: string, probe: () => Promise<boolean>, seconds = 180) {
  for (let attempt = 0; attempt < seconds * 2; attempt += 1) {
    try {
      if (await probe()) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error(`${what} did not become ready within ${seconds} s`)
}

/// Times one case on one engine: warmups first (the first also captures the
/// answer), then measured runs. A timeout or error ends the case early.
async function measure(run: () => Promise<Rows>, ordered: boolean): Promise<Timing> {
  const samples: number[] = []
  let rows: Rows | undefined
  let warmupMs = 0
  const once = async (): Promise<{ elapsed: number } | Timing> => {
    const started = performance.now()
    try {
      const result = await run()
      if (result.length > MAX_ROWS) throw new Error(`result exceeds the row cap of ${MAX_ROWS}`)
      rows ??= result
      return { elapsed: performance.now() - started }
    } catch (error) {
      const message = String(error instanceof Error ? error.message : error)
      return {
        status: isTooLarge(message) ? 'too-large' : isTimeout(message) ? 'timeout' : 'error',
        samples,
        error: message.slice(0, 300),
      }
    }
  }
  for (let index = 0; index < WARMUPS; index += 1) {
    const result = await once()
    if ('status' in result) return result
    warmupMs = Math.max(warmupMs, result.elapsed)
  }
  for (let index = 0; index < (warmupMs > SLOW_MS ? 1 : RUNS); index += 1) {
    const result = await once()
    if ('status' in result) return result
    samples.push(result.elapsed)
  }
  return {
    status: 'ok',
    medianMs: median(samples),
    minMs: Math.min(...samples),
    samples: samples.map((sample) => Math.round(sample * 1000) / 1000),
    rows: rows?.length ?? 0,
    digest: rows ? digest(rows, ordered) : undefined,
    kept: rows,
  }
}

async function main() {
  const corpus = JSON.parse(readFileSync(corpusPath, 'utf8')) as {
    cases: Case[]
    fixtureSQL: string
    provenance?: { session?: { sqlMode?: string } }
  }
  let cases = corpus.cases
  if (familyFilter) cases = cases.filter((entry) => entry.family.includes(familyFilter))
  if (limit !== undefined) cases = cases.slice(0, limit)
  const tables = [...corpus.fixtureSQL.matchAll(/CREATE TABLE (\w+)/g)].map((match) => match[1])
  log(`${cases.length} cases over ${tables.join(', ')}; scales ${scales.join(', ')}`)

  const host = await dockerHost()
  const cleanup = async () => {
    if (keep) return
    await docker('rm', '--force', '--volumes', mysqlName, clickhouseName, pintailName).catch(() => undefined)
    await docker('network', 'rm', networkName).catch(() => undefined)
  }
  process.on('SIGINT', () => void cleanup().then(() => process.exit(130)))
  try {
    await docker('network', 'create', networkName)
    await docker(
      'run', '--detach', '--name', mysqlName, '--network', networkName, '--publish', '0:3306',
      ...engineLimits, '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', mysqlImage,
      '--server-id=911', '--log-bin=mysql-bin', '--binlog-format=ROW', '--binlog-row-image=FULL',
      '--binlog-row-metadata=FULL', '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
      '--default-time-zone=+00:00', '--innodb-buffer-pool-size=1G',
    )
    await docker(
      'run', '--detach', '--name', clickhouseName, '--network', networkName, '--publish', '0:8123',
      '--ulimit', 'nofile=262144:262144', ...engineLimits,
      '--env', `CLICKHOUSE_PASSWORD=${clickhousePassword}`, clickhouseImage,
    )
    if (buildImage) {
      log('building the pintail image on the docker host')
      await docker('build', '--tag', pintailImage, repository)
    }
    await docker(
      'run', '--detach', '--name', pintailName, '--network', networkName,
      '--publish', '0:8080', '--publish', '0:3306', ...engineLimits,
      // The engine track: a repeated query must execute, not replay a memo.
      '--env', 'PINTAIL_DISABLE_SETTLED_MEMO=1',
      '--env', `PINTAIL_MAX_RESULT_ROWS=${MAX_ROWS}`,
      '--env', `PINTAIL_QUERY_MEMORY_LIMIT_BYTES=${4 * 1024 * 1024 * 1024}`,
      ...(trace ? ['--env', `PINTAIL_QUERY_TRACE=${TRACE_PATH}`] : []),
      pintailImage,
    )
    const mysqlPort = await publishedPort(mysqlName, 3306)
    const clickhouseUrl = `http://${host}:${await publishedPort(clickhouseName, 8123)}`
    const pintailUrl = `http://${host}:${await publishedPort(pintailName, 8080)}`
    const pintailWirePort = await publishedPort(pintailName, 3306)
    const admin = await waitForMysql(host, mysqlPort, 1200)
    await waitFor('ClickHouse', async () => (await fetch(`${clickhouseUrl}/ping`)).ok)
    await waitFor('Pintail', async () => (await fetch(`${pintailUrl}/health`)).ok)
    await admin.query(`CREATE USER '${sourceUser}'@'%' IDENTIFIED BY '${sourcePassword}'`)
    await admin.query(
      `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO '${sourceUser}'@'%'`,
    )
    const setup = await api<{ token: string }>(pintailUrl, '/api/auth/setup', {
      method: 'POST',
      body: { email: 'corpus@pintail.local', password: 'corpus-benchmark-run' },
    })
    const mysqlVersion = await scalar(admin, 'SELECT VERSION()')
    const pintailImageId = (await docker('image', 'inspect', '--format', '{{.Id}}', pintailImage)).stdout
    const clickhouseVersion = (await clickhouseStatement(clickhouseUrl, 'SELECT version()')).trim()

    const report: Record<string, unknown>[] = []
    for (const scale of scales) {
      const database = `corpus_s${scale}`
      log(`scale ${scale}: loading ${database}`)
      await admin.query(`CREATE DATABASE ${database}`)
      await admin.changeUser({ database })
      await admin.query('SET SESSION sql_log_bin = 0')
      await admin.query(`SET SESSION sql_mode = '${corpus.provenance?.session?.sqlMode ?? ''}'`)
      await admin.query(corpus.fixtureSQL)
      if (scale > 1) {
        await admin.query(`SET SESSION cte_max_recursion_depth = ${scale + 10}`)
        await admin.query('CREATE TABLE corpus_copies (k INT PRIMARY KEY)')
        await admin.query(
          `INSERT INTO corpus_copies WITH RECURSIVE seq(k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM seq WHERE k < ${scale - 1}) SELECT k FROM seq`,
        )
        for (const table of tables) {
          const [columns] = await admin.query<mysql.RowDataPacket[]>(
            'SELECT COLUMN_NAME AS name FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION',
            [database, table],
          )
          const names = columns.map((column) => String(column.name))
          const projection = names
            .map((name) => (name === 'id' || name === 'user_id' ? `t.\`${name}\` + c.k * ${KEY_STRIDE}` : `t.\`${name}\``))
            .join(', ')
          await admin.query(
            `INSERT INTO \`${table}\` (${names.map((name) => `\`${name}\``).join(', ')}) SELECT ${projection} FROM \`${table}\` t CROSS JOIN corpus_copies c`,
          )
        }
        await admin.query('DROP TABLE corpus_copies')
      }
      const rowCounts: Record<string, number> = {}
      for (const table of tables) {
        rowCounts[table] = Number(await scalar(admin, `SELECT COUNT(*) FROM \`${table}\``))
      }
      log(`scale ${scale}: ${JSON.stringify(rowCounts)}`)
      await admin.query('SET SESSION sql_log_bin = 1')

      // ClickHouse copies each table through its MySQL table function.
      const clickhouseTables: Record<string, string> = {}
      await clickhouseStatement(clickhouseUrl, `CREATE DATABASE ${database}`)
      for (const table of tables) {
        try {
          await clickhouseStatement(
            clickhouseUrl,
            `CREATE TABLE ${database}.\`${table}\` ENGINE = MergeTree ORDER BY tuple() AS SELECT * FROM mysql('${mysqlName}:3306', '${database}', '${table}', '${sourceUser}', '${sourcePassword}')`,
            { mysql_datatypes_support_level: 'decimal,datetime64,date2Date32' },
          )
          clickhouseTables[table] = 'loaded'
        } catch (error) {
          clickhouseTables[table] = String(error).slice(0, 300)
          log(`ClickHouse could not load ${table}: ${clickhouseTables[table]}`)
        }
      }

      // Pintail replicates the database the real way: snapshot, then CDC.
      const replica = await api<{ id: string }>(pintailUrl, '/api/databases', {
        method: 'POST',
        token: setup.token,
        body: {
          name: database,
          dsn: `mysql://${sourceUser}:${sourcePassword}@${mysqlName}:3306/${database}`,
          mode: 'cdc',
          include_tables: tables,
        },
      })
      await api(pintailUrl, `/api/databases/${replica.id}/probe`, { token: setup.token })
      await api(pintailUrl, `/api/databases/${replica.id}/snapshot`, {
        method: 'POST',
        token: setup.token,
        body: { force: false },
      })
      await waitFor(
        `Pintail snapshot of ${database}`,
        async () => {
          const status = await api<{ state: string; tables: Array<{ name: string; rows: number }> }>(
            pintailUrl,
            `/api/databases/${replica.id}/snapshot/status`,
            { token: setup.token },
          )
          if (status.state === 'error') throw new Error('snapshot failed')
          const rows = Object.fromEntries(status.tables.map((table) => [table.name, table.rows]))
          return (
            (status.state === 'streaming' || status.state === 'polling') &&
            tables.every((table) => rows[table] === rowCounts[table])
          )
        },
        3_600,
      )
      const key = await api<{ secret: string }>(pintailUrl, `/api/databases/${replica.id}/api-keys`, {
        method: 'POST',
        token: setup.token,
        body: { name: `corpus-${scale}`, scopes: ['query'] },
      })
      const sessions: Record<'mysql' | 'pintail', WireSession> = {
        mysql: new WireSession('MySQL', connectOptions(host, mysqlPort, 'root', 'pintail-root', database)),
        pintail: new WireSession('Pintail', connectOptions(host, pintailWirePort, database, key.secret, database)),
      }

      mkdirSync(outDir, { recursive: true })
      const partial = join(outDir, `partial-s${scale}.jsonl`)
      // Differing cases with both engines' rows, for telling a tie under a
      // LIMIT or a GROUP_CONCAT order from a wrong answer. Not tracked.
      const differences = join(outDir, `differences-s${scale}.jsonl`)
      writeFileSync(differences, '')
      const differenceKinds: Record<string, number> = {}
      writeFileSync(partial, '')
      const random = mulberry32(SEED + scale)
      const outcomes: Outcome[] = []
      const started = performance.now()
      for (const [index, entry] of cases.entries()) {
        const ordered = entry.ordered && scale === 1
        const settings = (engine: 'mysql' | 'pintail') => ({
          names: `SET NAMES utf8mb4 COLLATE ${entry.collation}`,
          zone: `SET time_zone = '${entry.timeZone}'`,
          mode: `SET SESSION sql_mode = '${entry.sqlMode}'`,
          deadline: `SET SESSION max_execution_time = ${TIMEOUT_MS}`,
          ...(engine === 'mysql'
            ? {
                limit: `SET SESSION sql_select_limit = ${MAX_ROWS + 1}`,
                switch:
                  entry.family === 'outer join-condition subquery'
                    ? "SET SESSION optimizer_switch = 'semijoin=off'"
                    : "SET SESSION optimizer_switch = 'default'",
              }
            : {}),
        })
        const runners: Record<Engine, () => Promise<Timing>> = {
          mysql: async () => {
            await sessions.mysql.ensure(settings('mysql'))
            return measure(() => sessions.mysql.query(entry.sql, TIMEOUT_MS + 5_000), ordered)
          },
          pintail: async () => {
            await sessions.pintail.ensure(settings('pintail'))
            return measure(() => sessions.pintail.query(entry.sql, TIMEOUT_MS + 5_000), ordered)
          },
          clickhouse: () => measure(() => clickhouseQuery(clickhouseUrl, database, entry.sql, TIMEOUT_MS), ordered),
        }
        const order: Engine[] = ['mysql', 'pintail', 'clickhouse']
        for (let position = order.length - 1; position > 0; position -= 1) {
          const swap = Math.floor(random() * (position + 1))
          ;[order[position], order[swap]] = [order[swap], order[position]]
        }
        const timings = {} as Record<Engine, Timing>
        for (const engine of order) timings[engine] = await runners[engine]()
        const agree = (engine: 'pintail' | 'clickhouse') =>
          timings.mysql.status !== 'ok' || timings[engine].status !== 'ok'
            ? 'n/a'
            : timings.mysql.digest === timings[engine].digest
              ? 'equal'
              : 'differs'
        if (agree('pintail') === 'differs') {
          const withoutLimit = entry.sql.replace(LIMIT_TAIL, '')
          let full: Rows | undefined
          if (withoutLimit !== entry.sql) {
            try {
              full = await sessions.mysql.query(withoutLimit, TIMEOUT_MS + 5_000)
            } catch {
              full = undefined
            }
          }
          const mysqlRows = timings.mysql.kept ?? []
          const pintailRows = timings.pintail.kept ?? []
          const kind = classify(mysqlRows, pintailRows, full)
          differenceKinds[kind] = (differenceKinds[kind] ?? 0) + 1
          const sample = (rows: Rows) => rows.slice(0, 50).map((row) => row.map(canonicalValue))
          appendFileSync(
            differences,
            `${JSON.stringify({ id: entry.id, family: entry.family, sql: entry.sql, kind, mysql: sample(mysqlRows), pintail: sample(pintailRows) })}\n`,
          )
        }
        for (const engine of order) delete timings[engine].kept
        outcomes.push({
          id: entry.id,
          family: entry.family,
          sql: entry.sql,
          ordered: entry.ordered,
          ...timings,
          parity: { pintail: agree('pintail'), clickhouse: agree('clickhouse') },
        })
        appendFileSync(partial, `${JSON.stringify(outcomes[outcomes.length - 1])}\n`)
        if ((index + 1) % 100 === 0) {
          log(`scale ${scale}: ${index + 1}/${cases.length} cases, ${Math.round((performance.now() - started) / 1000)} s`)
        }
      }
      sessions.mysql.close()
      sessions.pintail.close()
      if (trace) {
        // The writer flushes after 200 ms idle; give it that before reading.
        await Bun.sleep(1_000)
        const lines = (await docker('exec', pintailName, 'cat', TRACE_PATH)).stdout
        await docker('exec', pintailName, 'sh', '-c', `: > ${TRACE_PATH}`)
        const summary = summarizeTrace(scale, lines, outcomes)
        writeFileSync(join(outDir, `trace-s${scale}.json`), `${JSON.stringify(summary, null, 2)}\n`)
        log(`scale ${scale}: traced ${summary.traced} of ${outcomes.length} cases`)
      }
      report.push({ scale, database, rowCounts, clickhouseTables, outcomes, differences: differenceKinds })
    }

    const commit = (await Bun.$`git -C ${repository} rev-parse HEAD`.text()).trim()
    const artifact = {
      schemaVersion: 1,
      measuredAt: new Date().toISOString(),
      commit,
      corpus: {
        path: corpusPath.replace(`${repository}/`, ''),
        sha256: createHash('sha256').update(readFileSync(corpusPath)).digest('hex'),
        cases: cases.length,
      },
      engines: {
        mysql: `${mysqlImage} (${mysqlVersion})`,
        clickhouse: `${clickhouseImage} (${clickhouseVersion})`,
        pintail: `image ${pintailImageId.slice(0, 19)}, settled-result memo disabled`,
      },
      limits: engineLimits.join(' '),
      settings: { warmups: WARMUPS, runs: RUNS, timeoutMs: TIMEOUT_MS, slowMs: SLOW_MS, maxRows: MAX_ROWS, seed: SEED },
      scales: report,
    }
    mkdirSync(outDir, { recursive: true })
    writeFileSync(join(outDir, 'results.json'), `${JSON.stringify(artifact, null, 2)}\n`)
    writeFileSync(join(outDir, 'results.md'), summarize(artifact))
    writeFileSync(join(outDir, 'results.csv'), toCsv(artifact))
    log(`wrote results.json, results.md and results.csv under ${outDir}`)
  } finally {
    await cleanup()
  }
}

/// One row per case per scale, for plotting: each engine's median, minimum,
/// status and row count, Pintail's ratio to the other two, and parity. The
/// CSV is the tracked record, so every row carries the run's time and commit.
function toCsv(artifact: { measuredAt: string; commit: string; scales: Record<string, unknown>[] }): string {
  const scales = artifact.scales as { scale: number; outcomes: Outcome[] }[]
  const quote = (value: unknown) => {
    const text = value === undefined || value === null ? '' : String(value)
    return /[",\n]/.test(text) ? `"${text.replaceAll('"', '""')}"` : text
  }
  const ms = (timing: Timing) => (timing.status === 'ok' ? timing.medianMs!.toFixed(3) : '')
  const ratio = (a: Timing, b: Timing) =>
    a.status === 'ok' && b.status === 'ok' ? (a.medianMs! / Math.max(b.medianMs!, 0.001)).toFixed(4) : ''
  const header = [
    'measured_at', 'commit', 'scale', 'id', 'family',
    'pintail_ms', 'mysql_ms', 'clickhouse_ms',
    'pintail_min_ms', 'mysql_min_ms', 'clickhouse_min_ms',
    'pintail_status', 'mysql_status', 'clickhouse_status',
    'pintail_vs_mysql', 'pintail_vs_clickhouse',
    'rows', 'pintail_parity', 'clickhouse_parity', 'sql',
  ]
  const lines = [header.join(',')]
  for (const { scale, outcomes } of scales) {
    for (const o of outcomes) {
      lines.push(
        [
          artifact.measuredAt, artifact.commit.slice(0, 12), scale, o.id.slice(0, 16), o.family,
          ms(o.pintail), ms(o.mysql), ms(o.clickhouse),
          o.pintail.minMs?.toFixed(3), o.mysql.minMs?.toFixed(3), o.clickhouse.minMs?.toFixed(3),
          o.pintail.status, o.mysql.status, o.clickhouse.status,
          ratio(o.pintail, o.mysql), ratio(o.pintail, o.clickhouse),
          o.mysql.rows ?? o.pintail.rows, o.parity.pintail, o.parity.clickhouse, o.sql,
        ]
          .map(quote)
          .join(','),
      )
    }
  }
  return `${lines.join('\n')}\n`
}

function summarize(artifact: {
  measuredAt: string
  commit: string
  engines: Record<string, string>
  limits: string
  settings: Record<string, number>
  corpus: { cases: number }
  scales: Record<string, unknown>[]
}): string {
  const lines: string[] = [
    '# Oracle corpus on MySQL, ClickHouse and Pintail',
    '',
    `Measured ${artifact.measuredAt} at \`${artifact.commit.slice(0, 12)}\`. ` +
      `${artifact.corpus.cases} cases, ${artifact.settings.warmups} warmup and up to ` +
      `${artifact.settings.runs} measured runs each, ${artifact.settings.timeoutMs} ms timeout, ` +
      `results capped at ${artifact.settings.maxRows} rows, ` +
      `engines limited to \`${artifact.limits}\` on one docker host.`,
    '',
    ...Object.entries(artifact.engines).map(([engine, description]) => `- ${engine}: ${description}`),
    '',
    'Ratios are Pintail time over the other engine\'s (below 1 means Pintail is faster), ' +
      'as geometric means over cases where both engines answered.',
    '',
  ]
  const fmt = (value: number) => (Number.isFinite(value) ? value.toFixed(2) : 'n/a')
  for (const scaleEntry of artifact.scales) {
    const { scale, rowCounts } = scaleEntry as { scale: number; rowCounts: Record<string, number> }
    const outcomes = scaleEntry.outcomes as Outcome[]
    lines.push(`## Scale ${scale}`, '', `Rows: ${Object.entries(rowCounts).map(([t, n]) => `${t} ${n.toLocaleString()}`).join(', ')}.`, '')
    lines.push('| Engine | ok | error | timeout | too large | median of medians (ms) |', '|---|---:|---:|---:|---:|---:|')
    for (const engine of ['mysql', 'pintail', 'clickhouse'] as Engine[]) {
      const ok = outcomes.filter((o) => o[engine].status === 'ok')
      lines.push(
        `| ${engine} | ${ok.length} | ${outcomes.filter((o) => o[engine].status === 'error').length} | ` +
          `${outcomes.filter((o) => o[engine].status === 'timeout').length} | ${outcomes.filter((o) => o[engine].status === 'too-large').length} | ${fmt(median(ok.map((o) => o[engine].medianMs!)))} |`,
      )
    }
    const ratio = (o: Outcome, other: Engine) => o.pintail.medianMs! / Math.max(o[other].medianMs!, 0.001)
    const both = (other: Engine) => outcomes.filter((o) => o.pintail.status === 'ok' && o[other].status === 'ok')
    lines.push(
      '',
      `Pintail / MySQL: ${fmt(geomean(both('mysql').map((o) => ratio(o, 'mysql'))))} over ${both('mysql').length} cases. ` +
        `Pintail / ClickHouse: ${fmt(geomean(both('clickhouse').map((o) => ratio(o, 'clickhouse'))))} over ${both('clickhouse').length} cases.`,
      '',
      `Answers: Pintail differs from MySQL on ${outcomes.filter((o) => o.parity.pintail === 'differs').length} cases, ` +
        `ClickHouse on ${outcomes.filter((o) => o.parity.clickhouse === 'differs').length}.`,
      '',
      '### By family', '',
      '| Family | cases | Pintail / MySQL | Pintail / ClickHouse | Pintail errors+timeouts |',
      '|---|---:|---:|---:|---:|',
    )
    const families = new Map<string, Outcome[]>()
    for (const outcome of outcomes) families.set(outcome.family, [...(families.get(outcome.family) ?? []), outcome])
    const familyRows = [...families.entries()].map(([family, rows]) => {
      const vsMysql = geomean(rows.filter((o) => o.pintail.status === 'ok' && o.mysql.status === 'ok').map((o) => ratio(o, 'mysql')))
      const vsClickhouse = geomean(rows.filter((o) => o.pintail.status === 'ok' && o.clickhouse.status === 'ok').map((o) => ratio(o, 'clickhouse')))
      return { family, rows, vsMysql, vsClickhouse }
    })
    familyRows.sort((a, b) => (b.vsMysql || 0) - (a.vsMysql || 0))
    for (const { family, rows, vsMysql, vsClickhouse } of familyRows) {
      lines.push(`| ${family} | ${rows.length} | ${fmt(vsMysql)} | ${fmt(vsClickhouse)} | ${rows.filter((o) => o.pintail.status !== 'ok').length} |`)
    }
    const slowest = (other: Engine) =>
      both(other)
        .map((o) => ({ o, r: ratio(o, other) }))
        .sort((a, b) => b.r - a.r)
        .slice(0, 25)
    for (const other of ['mysql', 'clickhouse'] as Engine[]) {
      lines.push('', `### Slowest against ${other}`, '', `| Pintail ms | ${other} ms | ratio | family | SQL |`, '|---:|---:|---:|---|---|')
      for (const { o, r } of slowest(other)) {
        lines.push(`| ${o.pintail.medianMs!.toFixed(2)} | ${o[other].medianMs!.toFixed(2)} | ${r.toFixed(1)} | ${o.family} | \`${o.sql.replaceAll('|', '\\|').slice(0, 140)}\` |`)
      }
    }
    const failed = outcomes.filter((o) => o.pintail.status !== 'ok' && o.mysql.status === 'ok')
    lines.push('', '### Pintail errors and timeouts where MySQL answered', '')
    if (failed.length === 0) lines.push('None.')
    for (const o of failed.slice(0, 40)) {
      lines.push(`- ${o.pintail.status} (${o.family}): \`${o.sql.slice(0, 140)}\` - ${o.pintail.error ?? ''}`)
    }
    const differs = outcomes.filter((o) => o.parity.pintail === 'differs')
    lines.push('', '### Pintail answers that differ from MySQL', '')
    const kinds = (scaleEntry as { differences?: Record<string, number> }).differences
    if (kinds && Object.keys(kinds).length > 0) {
      lines.push(`By kind: ${Object.entries(kinds).map(([kind, count]) => `${kind} ${count}`).join(', ')}.`, '')
    }
    if (differs.length === 0) lines.push('None.')
    for (const o of differs.slice(0, 40)) lines.push(`- (${o.family}) \`${o.sql.slice(0, 160)}\``)
    lines.push('')
  }
  return `${lines.join('\n')}\n`
}

const csvFrom = option('--csv-from')
if (csvFrom) {
  const artifact = JSON.parse(readFileSync(resolve(repository, csvFrom), 'utf8'))
  mkdirSync(outDir, { recursive: true })
  writeFileSync(join(outDir, 'results.csv'), toCsv(artifact))
  log(`rebuilt ${join(outDir, 'results.csv')} from ${csvFrom}`)
} else {
  await main()
}

/// FNV-1a over the statement's UTF-8 bytes: the hash Pintail's query trace
/// names each statement by.
function statementHash(sql: string): string {
  let hash = 0xcbf29ce484222325n
  for (const byte of new TextEncoder().encode(sql)) {
    hash = ((hash ^ BigInt(byte)) * 0x100000001b3n) & 0xffffffffffffffffn
  }
  return hash.toString(16).padStart(16, '0')
}

// Phase marks are microseconds since the statement arrived; each segment is
// the time between one mark and the one before it.
const TRACE_SEGMENTS: Array<[segment: string, from: string | undefined, to: string]> = [
  ['session', undefined, 'dispatched'],
  ['hop_in', 'dispatched', 'worker'],
  ['parse', 'worker', 'parsed'],
  ['classify', 'parsed', 'classified'],
  ['admit', 'classified', 'admitted'],
  ['replica', 'admitted', 'replica'],
  ['catalog', 'replica', 'catalog'],
  ['metadata', 'catalog', 'metadata'],
  ['bind', 'metadata', 'bound'],
  ['present', 'bound', 'presented'],
  ['plan', 'presented', 'planned'],
  ['start', 'planned', 'started'],
  ['execute', 'started', 'collected'],
  ['hop_out', 'collected', 'returned'],
  ['encode', 'returned', 'encoded'],
]
const TRACE_COUNTERS = ['rows', 'materialized', 'scalar_rows', 'sorted_rows', 'regathered']

function summarizeTrace(scale: number, lines: string, outcomes: Outcome[]) {
  const byHash = new Map<string, Array<Record<string, string>>>()
  for (const line of lines.split('\n')) {
    if (!line.startsWith('sql=')) continue
    const fields = Object.fromEntries(line.split('\t').map((field) => field.split('=') as [string, string]))
    const list = byHash.get(fields.sql) ?? []
    list.push(fields)
    byHash.set(fields.sql, list)
  }
  const median = (values: number[]) => {
    const sorted = [...values].sort((left, right) => left - right)
    return sorted.length === 0 ? undefined : sorted[Math.floor(sorted.length / 2)]
  }
  const cases = outcomes.flatMap((outcome) => {
    const records = byHash.get(statementHash(outcome.sql))
    if (!records) return []
    const segments: Record<string, number> = {}
    for (const [segment, from, to] of TRACE_SEGMENTS) {
      const value = median(
        records
          .filter((record) => record[to] !== undefined && (from === undefined || record[from] !== undefined))
          .map((record) => Number(record[to]) - (from === undefined ? 0 : Number(record[from]))),
      )
      if (value !== undefined) segments[segment] = value
    }
    const counters = Object.fromEntries(
      TRACE_COUNTERS.map((name) => [name, Math.max(0, ...records.map((record) => Number(record[name] ?? 0)))]),
    )
    const timing = (engine: Engine) => (outcome[engine].status === 'ok' ? outcome[engine].medianMs : undefined)
    return [{
      id: outcome.id,
      family: outcome.family,
      pintailMs: timing('pintail'),
      mysqlMs: timing('mysql'),
      class: records[0]?.class,
      shared: records[0]?.shared,
      segments,
      counters,
    }]
  })
  const totals = (selected: typeof cases) => {
    const sums: Record<string, number> = {}
    for (const entry of selected) {
      for (const [segment, micros] of Object.entries(entry.segments)) sums[segment] = (sums[segment] ?? 0) + micros
    }
    return { cases: selected.length, microseconds: sums }
  }
  const slow = cases.filter(
    (entry) => entry.pintailMs !== undefined && entry.mysqlMs !== undefined && entry.pintailMs >= 2 * entry.mysqlMs,
  )
  return { scale, traced: cases.length, all: totals(cases), atLeastTwiceMysql: totals(slow), cases }
}

/// End-to-end differential gate: the whole product, production-shaped.
///
/// Boots a real MySQL source in Docker, builds and runs the real pintail
/// server binary, registers the source over the HTTP API, snapshots, and
/// then drives workload phases — CRUD inside transactions, type edge cases,
/// live DDL, seeded random churn, and a SIGKILL restart. After every phase
/// it proves two things against the live replica:
///
///   1. Convergence: every base table in MySQL reads back identically from
///      pintail (retried until the CDC supervisor catches up).
///   2. Query equivalence: the differential corpus in queries.ts returns
///      identical results from MySQL and pintail.
///
/// Operations pintail documents as gaps (table rename quarantine, in-place
/// type changes) are exercised too, but their divergences report as WARN
/// instead of failing the gate.
///
/// Run with: bun run run.ts            (full gate)
///           E2E_PHASES=crud,ddl ...   (subset while iterating)

import { createServer } from 'node:net'
import { Database } from 'bun:sqlite'
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { runOrmCompatibility, type MysqlEndpoint } from './orm-compat'
import { differentialQueries } from './queries'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
/// PINTAIL_E2E_KEEP_MYSQL=1 reuses one long-lived source container across
/// runs (database dropped and binlogs reset per run), trading a fresh boot
/// and timezone-table load for a stable name the harness never removes.
/// Source image for the gate. mysql:8.4 is the primary leg;
/// PINTAIL_E2E_MYSQL_IMAGE=mysql:8.0 runs the older-major leg (the reuse
/// path recreates a keep-container whose image differs).
const MYSQL_IMAGE = process.env.PINTAIL_E2E_MYSQL_IMAGE ?? 'mysql:8.4'
const KEEP_MYSQL = process.env.PINTAIL_E2E_KEEP_MYSQL === '1'
/// Base binlog row metadata for the whole gate. MySQL's own default is
/// MINIMAL, so that is the production shape the gate runs by default;
/// PINTAIL_E2E_BINLOG_METADATA=FULL runs the other leg. The drift phase
/// below always exercises the opposite setting in its window, so both
/// configurations see CDC traffic in every run.
const BINLOG_METADATA = process.env.PINTAIL_E2E_BINLOG_METADATA === 'FULL' ? 'FULL' : 'MINIMAL'
const OTHER_METADATA = BINLOG_METADATA === 'FULL' ? 'MINIMAL' : 'FULL'
const mysqlName = KEEP_MYSQL
  ? 'pintail-e2e-keep-mysql'
  : `pintail-e2e-mysql-${process.pid}-${nonce}`
const DATABASE = 'e2e_db'
/// The writable, Pintail-owned database the local-database phase creates.
const LOCAL_DATABASE = 'e2e_local'
const CONVERGE_TIMEOUT_MS = 180_000
/// One cadence for every poll loop. 250ms recovers the rounding waste the
/// old 2s granularity added to each of the ~20 convergence checks without
/// changing what any check accepts.
const POLL_MS = Number(process.env.PINTAIL_E2E_POLL_MS ?? 250)
const CONVERGE_POLL_MS = POLL_MS
/// The supervisor cadence the spawned pintail runs with. 2500ms halves the
/// adoption and re-probe wait proportional without turning supervision into
/// a busy loop; production stays at its 5s default.
const SUPERVISOR_MS = process.env.PINTAIL_E2E_SUPERVISOR_MS ?? '2500'

interface CheckResult {
  phase: string
  check: string
  status: 'PASS' | 'FAIL' | 'WARN' | 'SKIP'
  detail?: string
}

const results: CheckResult[] = []
/// Wall-clock per phase, split into the phase's own work, the convergence
/// sweep, and the corpus sweep — the split that says whether a slow gate is
/// waiting, polling, or round-tripping.
const phaseTimings: Array<{
  phase: string
  runSeconds: number
  convergeSeconds: number
  corpusSeconds: number
}> = []
/// Tables currently under a documented-gap operation, each with the exact
/// divergence signature the documentation predicts. A divergence matching
/// its signature reports WARN; anything else on the same table stays FAIL,
/// so unrelated regressions cannot hide behind a known gap.
const documentedGapTables = new Map<string, RegExp>()
/// Exact metadata divergences implied by the same documented operations.
/// Keep these signatures narrow: a different column, type, or nullability
/// mismatch must remain a gate failure.
const documentedMetadataGaps: RegExp[] = []

let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailBinary = ''
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''
let databaseId = ''
/// The LOCAL (Pintail-owned, writable) database the local-database phase
/// creates. It has no MySQL counterpart, so it is never part of
/// convergence or corpus verification.
let localDatabaseId = ''
let localWire: mysql.Connection | undefined
let mysqlStarted = false
let mysqlEndpoint: MysqlEndpoint | undefined

const runStarted = Date.now()

function log(message: string) {
  const elapsed = ((Date.now() - runStarted) / 1000).toFixed(1)
  console.log(`[e2e +${elapsed}s] ${message}`)
}

async function command(args: string[], options: { cwd?: string; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) {
    throw new Error(`${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`)
  }
  if (!options.quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

/// A host as it goes into a DSN: an IPv6 literal needs its brackets there.
function dsnHost(host: string): string {
  return host.includes(':') ? `[${host}]` : host
}

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (
      await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')
    ).stdout
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  // URL parsing keeps an IPv6 literal (ssh://user@[fd7a::1]) intact.
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const ssh = await command(['ssh', '-G', target], { quiet: true })
  const hostname = ssh.stdout
    .split('\n')
    .find((line) => line.startsWith('hostname '))
    ?.slice('hostname '.length)
  if (!hostname) throw new Error(`could not resolve Docker SSH target ${target}`)
  return hostname
}

async function publishedPort(name: string, containerPort: number): Promise<number> {
  const output = (await docker('port', name, `${containerPort}/tcp`)).stdout
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`Docker did not publish ${name}:${containerPort}`)
  return Number(match[1])
}

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer()
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      if (!address || typeof address === 'string') {
        server.close()
        reject(new Error('could not allocate a local port'))
        return
      }
      server.close((error) => (error ? reject(error) : resolvePort(address.port)))
    })
  })
}

async function waitForMysql(host: string, port: number, attempts = 240) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        multipleStatements: true,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
        enableKeepAlive: true,
        keepAliveInitialDelay: 10_000,
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready in time')
}

async function api<T>(
  path: string,
  options: { method?: string; body?: unknown; auth?: boolean } = {},
): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(options.auth === false ? {} : { Authorization: `Bearer ${token}` }),
      ...(options.body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) {
    throw new Error(`${options.method ?? 'GET'} ${path} returned ${response.status}: ${text}`)
  }
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

/// Queries run over the MySQL wire protocol — the way production clients
/// connect — so both engines' values arrive through the same mysql2 typing
/// (u64/decimal as exact strings, temporal as strings, binary as Buffers).
let pintailWire: mysql.Connection | undefined
let wireSecret = ''
let mysqlServerVersion = ''

async function pintailQuery(sql: string): Promise<unknown[][]> {
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
        // The wire library hardcodes charset 33 in column metadata
        // (docs/limitations.md), so binary columns would be lossily decoded
        // as utf8 text. Take raw buffers and let canonicalValue decide.
        typeCast: (field, next) => {
          if (field.type === 'VAR_STRING' || field.type === 'STRING' || field.type === 'BLOB') {
            return field.buffer()
          }
          return next()
        },
      })
    }
    const connection = pintailWire
    try {
      const [rows] = await connection.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
      return rows as unknown as unknown[][]
    } catch (error) {
      const transient = /ECONNREFUSED|ECONNRESET|EPIPE|closed state|Connection lost/i.test(
        String(error),
      )
      if (transient) {
        pintailWire = undefined
        try {
          await connection.end()
        } catch {}
        if (attempt < 2) {
          await Bun.sleep(1000)
          continue
        }
      }
      throw error
    }
  }
}

async function mysqlRows(sql: string): Promise<unknown[][]> {
  const [rows] = await mysqlConnection!.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
  return rows as unknown as unknown[][]
}

/// A small source-side pool for the corpus sweep: ~96 queries per phase were
/// serial round trips on one shared connection, which at a ~29ms link is
/// minutes of pure packet flight per run. The pintail side stays on its one
/// local connection (mysql2 pipelines it; the RTT there is loopback).
let mysqlPool: mysql.Pool | undefined

function corpusPool(): mysql.Pool {
  if (!mysqlPool) {
    mysqlPool = mysql.createPool({
      host: mysqlEndpoint!.host,
      port: mysqlEndpoint!.port,
      user: 'root',
      password: 'pintail-root',
      database: DATABASE,
      connectionLimit: 6,
      supportBigNumbers: true,
      bigNumberStrings: true,
      dateStrings: true,
    })
  }
  return mysqlPool
}

async function poolRows(sql: string): Promise<unknown[][]> {
  const [rows] = await corpusPool().query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
  return rows as unknown as unknown[][]
}

// ---------------------------------------------------------------------------
// Canonicalization: MySQL wire values (via mysql2) and pintail JSON values
// must map to one comparable form.

function canonicalValue(value: unknown): string {
  if (value === null || value === undefined) return 'NULL'
  if (Buffer.isBuffer(value)) {
    // Text arrives as Buffer from the pintail wire (charset-33 workaround
    // above) and binary arrives as Buffer from MySQL: valid UTF-8 compares
    // as text, anything else as hex, identically on both sides.
    const text = value.toString('utf8')
    if (Buffer.compare(Buffer.from(text, 'utf8'), value) === 0) {
      return canonicalValue(text)
    }
    return `0x${value.toString('hex')}`
  }
  if (value instanceof Date) return canonicalValue(value.toISOString())
  if (typeof value === 'object') return canonicalJson(value)
  let text = String(value)
  // JSON documents arrive as text from one side and objects from the other.
  if (text.startsWith('{') || text.startsWith('[')) {
    try {
      return canonicalJson(JSON.parse(text))
    } catch {}
  }
  // Temporal: unify 'T' separators, drop timezone suffix, trim only the
  // fractional-second zeros (never the seconds themselves).
  if (/^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}/.test(text)) {
    text = text.replace('T', ' ').replace(/(Z|[+-]\d{2}:?\d{2})$/, '')
    text = text.replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '')
    return text
  }
  // Numeric: fixed exponent-free form, 4 decimal places like the benchmark.
  if (text !== '' && /^-?\d+(\.\d+)?$/.test(text)) {
    // u64 values above 2^53 lose precision through Number; keep integers
    // longer than 15 digits as exact strings.
    if (/^-?\d+$/.test(text) && text.replace('-', '').length > 15) return text
    return Number(text).toFixed(4)
  }
  return text
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) =>
      a < b ? -1 : a > b ? 1 : 0,
    )
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(',')}}`
  }
  return JSON.stringify(canonicalValue(value))
}

function canonicalRow(row: unknown[], csvColumns?: number[]): string {
  return row
    .map((value, index) => {
      let canonical = canonicalValue(value)
      if (csvColumns?.includes(index)) {
        canonical = canonical.split(',').sort().join(',')
      }
      return canonical
    })
    .join('')
}

function diffRows(
  expected: unknown[][],
  actual: unknown[][],
  options: { multiset?: boolean; csvColumns?: number[] } = {},
): string | undefined {
  let left = expected.map((row) => canonicalRow(row, options.csvColumns))
  let right = actual.map((row) => canonicalRow(row, options.csvColumns))
  if (options.multiset) {
    left = [...left].sort()
    right = [...right].sort()
  }
  if (left.length !== right.length) {
    return `row count ${expected.length} vs ${actual.length}`
  }
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) {
      return `row ${index}:\n  mysql   ${left[index].replaceAll('', ' | ')}\n  pintail ${right[index].replaceAll('', ' | ')}`
    }
  }
  return undefined
}

// ---------------------------------------------------------------------------
// Convergence: every MySQL base table must read back identically.

async function baseTables(): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT TABLE_NAME FROM information_schema.TABLES ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME`,
  )
  return rows.map((row) => String(row[0]))
}

/// information_schema answers for immutable-per-phase metadata. Two of the
/// three round trips every convergence poll made were re-fetching this;
/// sql() invalidates it on any DDL so a stale shape can never hide a
/// divergence behind a projection that omits the new column.
const tableMetadataCache = new Map<string, { columns: string[]; key: string[] }>()

async function tableMetadata(table: string): Promise<{ columns: string[]; key: string[] }> {
  const cached = tableMetadataCache.get(table)
  if (cached) return cached
  const fresh = { columns: await tableColumns(table), key: await tableKey(table) }
  tableMetadataCache.set(table, fresh)
  return fresh
}

async function tableColumns(table: string): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT COLUMN_NAME FROM information_schema.COLUMNS ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_NAME = '${table}' ` +
      `AND GENERATION_EXPRESSION = '' ORDER BY ORDINAL_POSITION`,
  )
  return rows.map((row) => String(row[0]))
}

async function tableKey(table: string): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_NAME = '${table}' ` +
      `AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION`,
  )
  return rows.map((row) => String(row[0]))
}

async function tableDiff(table: string): Promise<string | undefined> {
  const { columns, key } = await tableMetadata(table)
  const projection = columns.map((column) => `\`${column}\``).join(', ')
  const order = (key.length > 0 ? key : columns).map((column) => `\`${column}\``).join(', ')
  const sql = `SELECT ${projection} FROM \`${table}\` ORDER BY ${order}`
  const expected = await mysqlRows(sql)
  let actual: unknown[][]
  try {
    actual = await pintailQuery(sql)
  } catch (error) {
    return `pintail query failed: ${error}`
  }
  // Keyless tables have no deterministic order on either side.
  return diffRows(expected, actual, { multiset: key.length === 0 })
}

async function metadataDiff(): Promise<string | undefined> {
  const query =
    `SELECT table_name, column_name, ordinal_position, data_type, column_type, ` +
    `is_nullable, character_maximum_length, character_octet_length, ` +
    `numeric_precision, numeric_scale, datetime_precision, column_default, ` +
    `extra, generation_expression ` +
    `FROM information_schema.columns WHERE table_schema = '${DATABASE}' ` +
    `ORDER BY table_name, ordinal_position`
  const expected = await mysqlRows(query)
  try {
    return diffRows(expected, await pintailQuery(query))
  } catch (error) {
    return `pintail metadata query failed: ${error}`
  }
}

async function verifyConvergence(phase: string) {
  const tables = await baseTables()
  const pending = new Map<string, string>()
  const settledGaps = new Map<string, string>()
  const deadline = Date.now() + CONVERGE_TIMEOUT_MS
  for (const table of tables) pending.set(table, 'not yet checked')
  while (pending.size > 0 && Date.now() < deadline) {
    for (const table of [...pending.keys()]) {
      const diff = await tableDiff(table)
      if (diff === undefined) {
        pending.delete(table)
        continue
      }
      pending.set(table, diff)
      // A documented gap NEVER converges by design: a diff matching its
      // signature is this table's final answer, and polling it to the
      // timeout added six minutes to the run while proving nothing. Stop
      // polling; the verdict loop below still grades it WARN or FAIL.
      const signature = documentedGapTables.get(table)
      if (signature?.test(diff)) {
        settledGaps.set(table, diff)
        pending.delete(table)
      }
    }
    if (pending.size > 0) await Bun.sleep(CONVERGE_POLL_MS)
  }
  for (const table of tables) {
    const diff = pending.get(table) ?? settledGaps.get(table)
    if (diff === undefined) {
      results.push({ phase, check: `converge:${table}`, status: 'PASS' })
    } else {
      const signature = documentedGapTables.get(table)
      const status = signature?.test(diff) ? 'WARN' : 'FAIL'
      results.push({ phase, check: `converge:${table}`, status, detail: diff })
      for (const line of diff.split('\n')) log(`${status} converge:${table} — ${line}`)
    }
  }
  if (pending.size === 0) {
    log(`${phase}: converged (${tables.length} tables)`)
  }
  const metadataDeadline = Date.now() + CONVERGE_TIMEOUT_MS
  let metadata = await metadataDiff()
  while (metadata !== undefined && Date.now() < metadataDeadline) {
    // A documented metadata gap never converges by design; its signature
    // is the final answer, and polling it to the timeout added three
    // minutes to the phase that carries it (same rule as the table loop).
    if (documentedMetadataGaps.some((signature) => signature.test(metadata))) break
    await Bun.sleep(CONVERGE_POLL_MS)
    metadata = await metadataDiff()
  }
  results.push({
    phase,
    check: 'converge:information_schema.columns',
    status:
      metadata === undefined
        ? 'PASS'
        : documentedMetadataGaps.some((signature) => signature.test(metadata))
          ? 'WARN'
          : 'FAIL',
    detail: metadata,
  })
}

async function verifyCorpus(phase: string) {
  // Some corpus tables are born mid-run (shipments carries the GEOMETRY
  // and SET coverage and is created by a later phase); a case whose table
  // does not exist yet SKIPs instead of failing on the source side.
  const existing = new Set(
    (
      await mysqlRows(
        `SELECT table_name FROM information_schema.tables WHERE table_schema = '${DATABASE}'`,
      )
    ).map((row) => String(row[0]).toLowerCase()),
  )
  // Each case runs source and replica sides concurrently, and cases fan out
  // over the source pool; results land in declaration order regardless.
  const settled = new Array<CheckResult>(differentialQueries.length)
  const CONCURRENCY = 6
  let next = 0
  async function runOne(index: number) {
    const query = differentialQueries[index]
    if (query.tables.some((table) => documentedGapTables.has(table))) {
      settled[index] = { phase, check: `query:${query.name}`, status: 'SKIP' }
      return
    }
    if (query.tables.some((table) => !existing.has(table.toLowerCase()))) {
      settled[index] = { phase, check: `query:${query.name}`, status: 'SKIP' }
      return
    }
    let expected: unknown[][]
    try {
      expected = await poolRows(query.sql)
    } catch (error) {
      settled[index] = {
        phase,
        check: `query:${query.name}`,
        status: 'FAIL',
        detail: `mysql rejected the corpus query: ${error}`,
      }
      return
    }
    try {
      const actual = await pintailQuery(query.sql)
      const diff = diffRows(expected, actual, { csvColumns: query.csvColumns })
      const failure = query.documentedGap ? ('WARN' as const) : ('FAIL' as const)
      settled[index] = {
        phase,
        check: `query:${query.name}`,
        status: diff === undefined ? 'PASS' : failure,
        detail: diff && query.documentedGap ? `${query.documentedGap}\n${diff}` : diff,
      }
      if (diff) for (const line of diff.split('\n')) log(`${failure} query:${query.name} — ${line}`)
    } catch (error) {
      // A documented gap warns when the engine REFUSES the query, not only
      // when it answers differently. Refusal is how an unimplemented feature
      // usually surfaces here - an unsupported collation is rejected at bind
      // time rather than producing a wrong row - so failing on it would mean
      // a gap could never be recorded before it was fixed, which is backwards:
      // the case exists to prove the fix.
      const failure = query.documentedGap ? ('WARN' as const) : ('FAIL' as const)
      settled[index] = {
        phase,
        check: `query:${query.name}`,
        status: failure,
        detail: query.documentedGap ? `${query.documentedGap}\n${error}` : String(error),
      }
      log(`${failure} query:${query.name} — ${error}`)
    }
  }
  await Promise.all(
    Array.from({ length: Math.min(CONCURRENCY, differentialQueries.length) }, async () => {
      for (;;) {
        const index = next
        next += 1
        if (index >= differentialQueries.length) return
        await runOne(index)
      }
    }),
  )
  results.push(...settled)
}

// ---------------------------------------------------------------------------
// Workload phases.

function mulberry32(seed: number) {
  let state = seed
  return () => {
    state |= 0
    state = (state + 0x6d2b79f5) | 0
    let t = Math.imul(state ^ (state >>> 15), 1 | state)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

async function sql(statement: string) {
  if (/^\s*(ALTER|CREATE|DROP|RENAME|TRUNCATE)\b/i.test(statement)) {
    tableMetadataCache.clear()
  }
  await mysqlConnection!.query(statement)
}

async function mysqlCount(table: string): Promise<string> {
  const [rows] = (await mysqlConnection!.query(
    `SELECT COUNT(*) AS n FROM ${table}`,
  )) as unknown as [Array<{ n: number | string }>]
  return String(rows[0]!.n)
}

/// A supervisor cycle may hold the database's job slot at any instant; the
/// 409 is correct API behavior, so callers retry it rather than failing.
async function retry409<T>(operation: () => Promise<T>): Promise<T> {
  const retryStart = Date.now()
  for (let attempt = 0; ; attempt += 1) {
    try {
      return await operation()
    } catch (error) {
      if (!String(error).includes('409') || Date.now() - retryStart > 60_000) throw error
      await Bun.sleep(POLL_MS)
    }
  }
}

async function waitForState(database: string, wanted: string, timeoutMs: number) {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    const status = await api<{ state: string; tables?: Array<{ name: string; last_error?: string }> }>(
      `/api/databases/${database}/snapshot/status`,
    )
    if (status.state === wanted) return
    if (Date.now() > deadline) {
      const errors = (status.tables ?? [])
        .filter((table) => table.last_error)
        .map((table) => `${table.name}: ${table.last_error}`)
        .join('; ')
      throw new Error(`state never reached ${wanted}: ${status.state}; ${errors}`)
    }
    await Bun.sleep(POLL_MS)
  }
}

async function phaseSeed() {
  // Chitti's conformance seed, vendored: case variants, trailing spaces,
  // mixed collations per column, an ENUM declared out of alphabetical
  // order, NULL join keys, a dangling FK alias, and timestamp ties - the
  // classes real parity bugs lived in. Loaded into the corpus schema with
  // its own DATABASE header stripped.
  const conformance = readFileSync(
    join(import.meta.dir, '..', 'corpus', 'conformance', 'seed.sql'),
    'utf8',
  )
    .split('\n')
    .filter((line) => !/^(DROP DATABASE|CREATE DATABASE|USE )/.test(line))
    .join('\n')
  await mysqlConnection!.query(conformance)

  // Self-referential fixture, shaped for the alias-misattribution class: a
  // table read through two aliases at once (created_by/updated_by), where
  // every wrong resolution is VISIBLE. created_by and updated_by differ on
  // most rows, so returning one alias's row for both changes values;
  // updated_by is sometimes NULL and sometimes DANGLING (id 99, which no row
  // has), so the correct answer is NULL where the buggy answer was the first
  // alias's row. A staging table with this shape attributed 605 of 4067
  // activities to the wrong person while every count looked right.
  await sql(`CREATE TABLE staff (
    id INT UNSIGNED PRIMARY KEY,
    name VARCHAR(64) NOT NULL,
    manager_id INT UNSIGNED NULL,
    created_by INT UNSIGNED NULL,
    updated_by INT UNSIGNED NULL,
    active TINYINT(1) NOT NULL DEFAULT 1
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`INSERT INTO staff (id, name, manager_id, created_by, updated_by, active) VALUES
    (1, 'Asha',   NULL, NULL, NULL, 1),
    (2, 'Bala',   1,    1,    NULL, 1),
    (3, 'Chitra', 1,    1,    2,    1),
    (4, 'Dev',    2,    1,    3,    0),
    (5, 'Esha',   2,    3,    99,   1),
    (6, 'Farid',  3,    2,    4,    1),
    (7, 'Gita',   3,    99,   1,    0),
    (8, 'Hari',   4,    5,    5,    1),
    (9, 'Indra',  4,    6,    99,   1),
    (10,'Jai',    5,    7,    2,    0),
    (11,'Kavi',   5,    1,    6,    1),
    (12,'Lata',   6,    8,    NULL, 1)`)
  await sql(`CREATE TABLE customers (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(64) NOT NULL,
    email VARCHAR(96) NULL,
    tier ENUM('free','pro','enterprise') NOT NULL DEFAULT 'free',
    tags SET('alpha','beta','vip') NOT NULL DEFAULT '',
    balance DECIMAL(12,2) NOT NULL DEFAULT 0,
    meta JSON NULL,
    avatar VARBINARY(16) NULL,
    latin_note VARCHAR(32) CHARACTER SET latin1 NULL,
    -- MySQL 5.x's default collation, which most existing schemas still carry
    -- because a table keeps whatever it was created with. The engine's own
    -- default is utf8mb4_0900_ai_ci, so without a column like this the gate
    -- never exercises a second collation at all.
    legacy_label VARCHAR(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE orders (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    customer_id INT UNSIGNED NOT NULL,
    status ENUM('pending','processing','shipped','delivered','cancelled') NOT NULL,
    total DECIMAL(12,2) NOT NULL,
    placed_on DATE NOT NULL,
    updated_at TIMESTAMP(6) NULL
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE order_items (
    order_id BIGINT UNSIGNED NOT NULL,
    line_no INT NOT NULL,
    product VARCHAR(64) NOT NULL,
    qty SMALLINT UNSIGNED NOT NULL,
    price DECIMAL(10,2) NOT NULL,
    PRIMARY KEY (order_id, line_no)
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE audit_log (note VARCHAR(128) NOT NULL) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE counters (
    id TINYINT UNSIGNED PRIMARY KEY,
    u8 TINYINT UNSIGNED NOT NULL,
    u16 SMALLINT UNSIGNED NOT NULL,
    u32 INT UNSIGNED NOT NULL,
    u64 BIGINT UNSIGNED NOT NULL,
    s64 BIGINT NOT NULL
  )`)
  // An empty ENUM member is legal and a REAL member (ordinal 1 here),
  // distinct from the error value (ordinal 0). Declaration order ('',
  // zz, aa) disagrees with alphabetical order (aa before zz), so a path
  // that demotes the empty member to plain text cannot pass by luck.
  await sql(`CREATE TABLE badges (
    id INT UNSIGNED PRIMARY KEY,
    v ENUM('','zz','aa') NOT NULL
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`INSERT INTO badges VALUES
    (1,'zz'), (2,''), (3,'aa'), (4,'zz'), (5,''), (6,'aa'), (7,'')`)

  const random = mulberry32(0x5eed)
  const tiers = ['free', 'pro', 'enterprise']
  const tags = ['', 'alpha', 'beta', 'vip', 'alpha,vip', 'beta,vip']
  const names = ['Asha', 'Bruno', 'Chloé', 'Dmitri', 'えみ', 'Farah', 'Göran', 'Priya']
  for (let id = 1; id <= 40; id += 1) {
    const name = `${names[id % names.length]} ${id}`
    const email = id % 7 === 0 ? 'NULL' : `'user${id}@example.com'`
    const tier = tiers[Math.floor(random() * 3)]
    const tag = tags[Math.floor(random() * tags.length)]
    const balance = (random() * 2000 - 500).toFixed(2)
    const meta =
      id % 5 === 0 ? 'NULL' : `'{"lang":"en","score":${Math.floor(random() * 100)}}'`
    const avatar = id % 6 === 0 ? 'NULL' : `X'${id.toString(16).padStart(4, '0')}beef'`
    await sql(
      `INSERT INTO customers (name, email, tier, tags, balance, meta, avatar, latin_note, legacy_label) VALUES ` +
        `('${name}', ${email}, '${tier}', '${tag}', ${balance}, ${meta}, ${avatar}, _latin1 0x636166E9, ` +
        // Values chosen to exercise what general_ci actually does differently:
        // ASCII case folding, Latin-1 accent folding onto the base letter,
        // PAD SPACE trailing spaces, and the supplementary-plane collapse
        // where every character above the BMP weighs the same.
        `${['\'Active\'', '\'active\'', '\'ACTIVE\'', '\'Ärger\'', '\'arger\'', '\'pending  \'', '\'pending\'', '\'😀\'', '\'𠀀\'', 'NULL'][id % 10]})`,
    )
  }
  const statuses = ['pending', 'processing', 'shipped', 'delivered', 'cancelled']
  for (let id = 1; id <= 200; id += 1) {
    const customer = 1 + Math.floor(random() * 40)
    const status = statuses[Math.floor(random() * statuses.length)]
    const total = (random() * 1000).toFixed(2)
    const day = 1 + Math.floor(random() * 28)
    const month = 1 + Math.floor(random() * 12)
    const updated =
      id % 3 === 0 ? 'NULL' : `'2025-0${(id % 9) + 1}-1${id % 10} 08:0${id % 10}:00'`
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, updated_at) VALUES ` +
        `(${customer}, '${status}', ${total}, '2024-${String(month).padStart(2, '0')}-${String(day).padStart(2, '0')}', ${updated})`,
    )
    const lines = 1 + Math.floor(random() * 3)
    for (let line = 1; line <= lines; line += 1) {
      await sql(
        `INSERT INTO order_items VALUES (${id}, ${line}, 'sku-${(id * 7 + line) % 50}', ` +
          `${1 + Math.floor(random() * 9)}, ${(random() * 200).toFixed(2)})`,
      )
    }
  }
  await sql(`INSERT INTO audit_log VALUES ('seed complete'), ('第二条 unicode note')`)
  await sql(
    `INSERT INTO counters VALUES (1, 200, 65535, 3000000000, 18446744073709551615, -9223372036854775808), ` +
      `(2, 0, 0, 0, 0, 9223372036854775807)`,
  )
}

async function phaseCrud() {
  // Point updates, bulk updates, deletes, and an explicit rollback.
  await sql(`UPDATE customers SET balance = balance + 10.55, tier = 'pro' WHERE id = 3`)
  await sql(`UPDATE orders SET status = 'delivered', updated_at = '2025-06-01 12:00:00' WHERE id <= 20`)
  await sql(`DELETE FROM orders WHERE id IN (5, 15, 25)`)
  await sql(`DELETE FROM order_items WHERE order_id IN (5, 15, 25)`)
  await sql(
    `INSERT INTO customers (id, name, email, balance) VALUES (1, 'dupe', 'x@y.z', 1) ` +
      `ON DUPLICATE KEY UPDATE balance = balance + 100`,
  )
  await mysqlConnection!.beginTransaction()
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (2, 'pending', 42.42, '2025-07-01')`)
  await sql(`UPDATE customers SET balance = balance - 42.42 WHERE id = 2`)
  await mysqlConnection!.commit()
  await mysqlConnection!.beginTransaction()
  await sql(`DELETE FROM customers WHERE id = 4`)
  await sql(`UPDATE orders SET total = 0 WHERE customer_id = 4`)
  await mysqlConnection!.rollback()
  await sql(`INSERT INTO audit_log VALUES ('crud complete')`)
}

async function phaseTypeEdges() {
  await sql(
    `INSERT INTO customers (name, email, tier, tags, balance, meta, avatar, latin_note) VALUES ` +
      `('emoji 🦆 café', '', 'enterprise', 'alpha,beta,vip', -0.01, '{"nested":{"deep":[1,2,3]},"nul":null}', X'00FF10', _latin1 0x80), ` +
      `('nulls', NULL, 'free', '', 0.00, NULL, NULL, NULL)`,
  )
  await sql(`UPDATE counters SET u8 = 255, u16 = 65534, u32 = 4294967295, u64 = 9223372036854775808 WHERE id = 2`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, updated_at) VALUES ` +
      `(1, 'pending', 0.01, '2020-02-29', '2038-01-19 03:14:07.499999'), ` +
      `(1, 'cancelled', 9999999999.99, '1970-01-01', '1970-01-01 00:00:01.000001')`,
  )
}

async function phaseDdl() {
  // ADD COLUMN, live writes into it, then DROP a different column.
  await sql(`ALTER TABLE orders ADD COLUMN coupon VARCHAR(24) NULL`)
  await sql(`UPDATE orders SET coupon = 'SUMMER10' WHERE id % 10 = 0`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, coupon) VALUES (7, 'shipped', 77.77, '2025-07-07', 'NEW7')`,
  )
  await sql(`ALTER TABLE customers DROP COLUMN latin_note`)
  await sql(`UPDATE customers SET balance = balance + 1 WHERE id = 1`)
  // CREATE TABLE mid-stream: the replica must pick it up automatically.
  // route (GEOMETRY) and services (SET) exist because real data found
  // both types broken while the gate stayed green: sakila's address lost
  // its SRID+WKB header through reconciliation and special_features
  // ordered alphabetically instead of by member bitmask. Convergence now
  // byte-checks a point, a linestring and a NULL every run, and the
  // corpus orders by the SET.
  await sql(`CREATE TABLE shipments (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    order_id BIGINT UNSIGNED NOT NULL,
    carrier VARCHAR(32) NOT NULL,
    shipped_on DATE NULL,
    route GEOMETRY NULL,
    services SET('fragile','insured','priority','tracked') NOT NULL DEFAULT ''
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(
    `INSERT INTO shipments (order_id, carrier, shipped_on, route, services) VALUES ` +
      `(1, 'DHL', '2025-07-08', ST_GeomFromText('POINT(-112.8185647 49.6999986)'), 'tracked'), ` +
      `(2, 'UPS', NULL, NULL, 'fragile,priority'), ` +
      `(3, 'FedEx', '2025-07-09', ST_GeomFromText('LINESTRING(0 0, 1 1, 2 2)'), 'insured,tracked')`,
  )
  await sql(`UPDATE shipments SET carrier = 'DHL Express' WHERE id = 1`)
  // Storage-compatible MODIFY COLUMN evolves in place — no resync: an
  // integer widening and a VARCHAR widening, with live writes after each.
  await sql(`ALTER TABLE shipments MODIFY COLUMN carrier VARCHAR(64) NOT NULL`)
  await sql(
    `UPDATE shipments SET carrier = 'A Rather Long Carrier Name For Widths' WHERE id = 2`,
  )
  await sql(`ALTER TABLE orders MODIFY COLUMN customer_id BIGINT UNSIGNED NOT NULL`)
  await sql(`UPDATE orders SET customer_id = 5000000001 WHERE id = 3`)
  await sql(`ALTER TABLE customers MODIFY COLUMN balance DECIMAL(14,2) NOT NULL DEFAULT 0`)
  await sql(`UPDATE customers SET balance = balance + 100000000000.25 WHERE id = 3`)
  // Index-only changes replicate rows straight through — no resync.
  await sql(`ALTER TABLE orders ADD INDEX status_idx (status)`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (9, 'processing', 12.34, '2025-07-10')`,
  )
  await sql(`ALTER TABLE orders DROP INDEX status_idx`)
  await sql(`UPDATE orders SET status = 'shipped' WHERE customer_id = 9 AND status = 'processing'`)
  // A whole-table character-set conversion. This is the statement an operator
  // runs to move a table between collations, and the schema tracker used to
  // stop replication dead on it - the SQL parser cannot represent it, so it
  // came back as a parse error rather than a change. Rows written afterwards
  // are what prove the stream survived it.
  await sql(`ALTER TABLE shipments CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci`)
  await sql(
    `INSERT INTO shipments (order_id, carrier, shipped_on) VALUES (4, 'Freight Post', '2025-07-11')`,
  )
  await sql(`UPDATE shipments SET carrier = 'Freight Express' WHERE carrier = 'Freight Post'`)
  // TRUNCATE and refill.
  await sql(`TRUNCATE TABLE audit_log`)
  await sql(`INSERT INTO audit_log VALUES ('after truncate')`)
}

async function phaseSchemaDriftMinimal() {
  // The same missed schema change, under whichever metadata setting the
  // rest of the gate is NOT running (the base is MINIMAL - production's
  // default - unless PINTAIL_E2E_BINLOG_METADATA=FULL flips the legs).
  // Under MINIMAL the table map omits column names, so a row image can
  // only be read positionally and the replica has nothing to align a
  // mismatched width against; re-probing is the only repair, and it works
  // precisely when the refreshed schema and the row in hand agree on
  // width - one ALTER, then the next INSERT.
  //
  // Written events carry whatever metadata was in force when they were
  // written, so this phase restores the base before it ends and converges
  // on its own.
  await sql(`SET GLOBAL binlog_row_metadata = '${OTHER_METADATA}'`)
  try {
    await sql(`SET sql_log_bin = 0`)
    await sql(`ALTER TABLE orders ADD COLUMN minimal_note VARCHAR(32) NULL`)
    await sql(`SET sql_log_bin = 1`)
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, minimal_note) VALUES ` +
        `(2, 'pending', 19.99, '2025-07-30', 'after-minimal-add')`,
    )
    await sql(`UPDATE orders SET minimal_note = 'seen' WHERE id % 6 = 0`)
    await sql(`DELETE FROM orders WHERE status = 'cancelled' AND total < 0`)
  } finally {
    await sql(`SET GLOBAL binlog_row_metadata = '${BINLOG_METADATA}'`)
  }
}

async function phaseSchemaDriftUnseen() {
  // A schema change the CDC stream never sees as DDL.
  //
  // Production hit this three days running: a hand-written
  // `ALTER TABLE Payment ADD COLUMN` landed, and the next INSERT arrived as a
  // row image one column wider than the probed schema. The decoder refused
  // the row - correctly, since it cannot know which column is which - and
  // marked the whole table for a full resnapshot. Every such row was dropped
  // until someone resynced.
  //
  // `sql_log_bin = 0` reproduces it exactly: the ALTER never enters the
  // binlog, so no DDL reaches the stream, while the row images that follow
  // carry the new width. The replica must re-probe, adopt the schema and keep
  // the rows - WITHOUT resnapshotting the table, which on a large table is
  // hours of copying to learn about one column.
  const widen = async (column: string, type: string) => {
    await sql(`SET sql_log_bin = 0`)
    await sql(`ALTER TABLE orders ADD COLUMN ${column} ${type}`)
    await sql(`SET sql_log_bin = 1`)
  }

  await widen('unseen_note', 'VARCHAR(32) NULL')
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, unseen_note) VALUES ` +
      `(3, 'pending', 31.50, '2025-08-01', 'after-hidden-add')`,
  )
  await sql(`UPDATE orders SET unseen_note = 'touched' WHERE id % 7 = 0`)

  // Stress: repeated invisible widenings interleaved with continuous writes
  // on the same table, so the heal runs against a moving schema rather than
  // a single quiet change. Each round adds a column the stream never hears
  // about and then writes through it immediately.
  for (let round = 0; round < 4; round += 1) {
    await widen(`drift_${round}`, 'INT NULL')
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, drift_${round}) VALUES ` +
        `(4, 'shipped', ${10 + round}.25, '2025-08-0${round + 2}', ${round * 11})`,
    )
    await sql(`UPDATE orders SET drift_${round} = ${round} WHERE id % 5 = 0`)
    await sql(`DELETE FROM orders WHERE status = 'cancelled' AND total < 0`)
  }

  // A drop the stream also never sees: the row image narrows, which is the
  // same defect mirrored, and must heal the same way.
  await sql(`SET sql_log_bin = 0`)
  await sql(`ALTER TABLE orders DROP COLUMN drift_0`)
  await sql(`SET sql_log_bin = 1`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
      `(5, 'delivered', 88.88, '2025-08-09')`,
  )
}

async function phaseDdlDocumentedGaps() {
  // Table rename is documented as quarantine; type changes are not part of
  // the DDL gate. Exercise both so regressions in the documented behavior
  // surface as WARN diffs, and improvements flip them to PASS.
  // Rename quarantine: the renamed table never appears in the replica.
  documentedGapTables.set('audit_log', /unknown table/)
  documentedGapTables.set('audit_history', /unknown table/)
  documentedMetadataGaps.push(
    /^row \d+:\n  mysql   audit_history( \|.*)\n  pintail audit_log\1$/,
  )
  await sql(`RENAME TABLE audit_log TO audit_history`)
  await sql(`INSERT INTO audit_history VALUES ('post rename')`)
  // In-place type change: replication stops applying to the table, so the
  // divergence is a row-content diff (never a missing table or an error).
  documentedGapTables.set('order_items', /^row \d+:/)
  await sql(`ALTER TABLE order_items MODIFY qty INT UNSIGNED NOT NULL`)
  await sql(`UPDATE order_items SET qty = qty + 1 WHERE order_id = 1`)
}

/// Runs one corpus query with convergence retries: writes are paused at the
/// call site, so the replica settles to the source state within the window.
async function liveQueryConverges(
  query: (typeof differentialQueries)[number],
  deadlineMs: number,
): Promise<string | undefined> {
  const deadline = Date.now() + deadlineMs
  let last: string | undefined = 'never compared'
  while (Date.now() < deadline) {
    const expected = await mysqlRows(query.sql)
    try {
      const actual = await pintailQuery(query.sql)
      last = diffRows(expected, actual, { csvColumns: query.csvColumns })
      if (last === undefined) return undefined
    } catch (error) {
      last = String(error)
    }
    await Bun.sleep(1000)
  }
  return last
}

/// ~12 seconds of source DML racing a six-connection read storm on the
/// wire. Three things are actually verified, stated exactly:
///
/// 1. Availability: a read answers or backpressures (1040) - never an
///    internal error, a protocol desync, or a dropped connection.
/// 2. Snapshot consistency DURING the storm: every read carries its own
///    redundancy inside ONE statement (a total recomputed two ways, a
///    join that can never dangle, an ordering the result must obey), so a
///    torn snapshot is detected at read time. Mid-storm values cannot be
///    compared across statements or engines - each statement legitimately
///    sees a different instant.
/// 3. Value correctness AFTER the storm: once the writes stop, the same
///    reads run differentially against MySQL and must match exactly; the
///    converge + corpus sweep that follows every phase then re-proves the
///    full corpus.
async function phaseContention() {
  interface ContentionRead {
    sql: string
    verify: (rows: unknown[][]) => string | null
  }
  const asNumber = (value: unknown) => Number(String(value))
  const reads: ContentionRead[] = [
    {
      // One statement, two derivations of the same total: a scalar
      // subquery and a grouped rollup. Any torn snapshot splits them.
      sql:
        'SELECT (SELECT COUNT(*) FROM orders) AS total, COALESCE(SUM(n), 0) AS regrouped ' +
        'FROM (SELECT COUNT(*) AS n FROM orders GROUP BY status) g',
      verify: (rows) => {
        if (rows.length !== 1) return `expected 1 row, got ${rows.length}`
        const [total, regrouped] = rows[0]!
        if (asNumber(total) !== asNumber(regrouped)) {
          return `torn snapshot: COUNT(*) ${total} != regrouped ${regrouped}`
        }
        return null
      },
    },
    {
      // Two derivations of the joinable-row count in one statement: an
      // EXISTS filter and the join itself. Data-independent - a row whose
      // customer genuinely does not exist (the type-edges phase plants
      // one) is excluded by BOTH sides - so any split is a torn snapshot.
      sql:
        'SELECT (SELECT COUNT(*) FROM orders o2 WHERE EXISTS ' +
        '(SELECT 1 FROM customers c2 WHERE c2.id = o2.customer_id)) - COUNT(*) AS torn ' +
        'FROM orders o JOIN customers c ON c.id = o.customer_id',
      verify: (rows) => {
        const torn = asNumber(rows[0]?.[0])
        return torn === 0 ? null : `join count split by ${torn} within one statement`
      },
    },
    {
      // Aggregate sanity within one statement over NOT NULL columns.
      sql: 'SELECT COUNT(*) AS n, COUNT(id) AS nid, MIN(id) AS lo, MAX(id) AS hi FROM orders',
      verify: (rows) => {
        const [n, nid, lo, hi] = rows[0]!.map(asNumber)
        if (n !== nid) return `COUNT(*) ${n} != COUNT(id) ${nid}`
        // An empty table answers NULL for MIN/MAX, which asNumber turns
        // into NaN, never undefined — so the emptiness check must be
        // NaN-aware or it checks nothing.
        if (n > 0 && (!Number.isFinite(lo!) || !Number.isFinite(hi!) || lo! > hi!)) {
          return `MIN ${lo} exceeds MAX ${hi}`
        }
        return null
      },
    },
    {
      // The result must obey its own ORDER BY and LIMIT.
      sql: 'SELECT id FROM orders ORDER BY id DESC LIMIT 20',
      verify: (rows) => {
        if (rows.length > 20) return `LIMIT 20 returned ${rows.length} rows`
        for (let index = 1; index < rows.length; index += 1) {
          if (asNumber(rows[index]![0]) >= asNumber(rows[index - 1]![0])) {
            return `ORDER BY id DESC violated at row ${index}`
          }
        }
        return null
      },
    },
    {
      // Every returned row must satisfy the predicate it was filtered by.
      sql: "SELECT id, tags FROM customers WHERE FIND_IN_SET('vip', tags) > 0 ORDER BY id",
      verify: (rows) => {
        for (const row of rows) {
          const tags = String(row[1] ?? '')
          if (!tags.split(',').includes('vip')) {
            return `row ${row[0]} tags ${tags} fails its own predicate`
          }
        }
        return null
      },
    },
  ]
  const deadline = Date.now() + 12_000
  let dmlOps = 0
  const dml = (async () => {
    let n = 0
    while (Date.now() < deadline) {
      n += 1
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
          `(${1 + (n % 40)}, 'pending', ${(n % 500)}.25, '2025-08-0${1 + (n % 9)}')`,
      )
      if (n % 3 === 0) await sql(`UPDATE orders SET total = total + 1 WHERE id % 17 = ${n % 17}`)
      if (n % 7 === 0) {
        await sql(`DELETE FROM orders WHERE status = 'cancelled' AND id % 23 = ${n % 23}`)
      }
      dmlOps = n
    }
  })()
  let completed = 0
  let backpressured = 0
  const violations: string[] = []
  const workers = Array.from({ length: 6 }, async (_, worker) => {
    const connection = await mysql.createConnection({
      host: '127.0.0.1',
      port: pintailWirePort,
      user: DATABASE,
      password: wireSecret,
      database: DATABASE,
    })
    try {
      for (let turn = worker; Date.now() < deadline; turn += 1) {
        const read = reads[turn % reads.length]!
        try {
          const [rows] = (await connection.query({
            sql: read.sql,
            rowsAsArray: true,
          })) as unknown as [unknown[][]]
          completed += 1
          const violation = read.verify(rows)
          if (violation && violations.length < 5) {
            violations.push(`${violation} (${read.sql})`)
          }
        } catch (error) {
          const raised = error as { errno?: number }
          if (raised.errno === 1040) {
            backpressured += 1
            await Bun.sleep(50)
            continue
          }
          throw new Error(`contention read failed beyond backpressure: ${error} (${read.sql})`)
        }
      }
    } finally {
      await connection.end().catch(() => {})
    }
  })
  await Promise.all([dml, ...workers])
  if (violations.length) {
    throw new Error(`contention consistency violations: ${violations.join('; ')}`)
  }
  // Floors, not exact counts: the point is that both sides genuinely ran
  // concurrently, not that a particular interleaving happened.
  if (completed < 100) {
    throw new Error(`contention storm completed only ${completed} reads (floor 100)`)
  }
  if (dmlOps < 30) throw new Error(`contention storm ran only ${dmlOps} DML ops (floor 30)`)
  // Writes have stopped: the same reads must now answer identically on
  // both engines once the replica converges on the final source state.
  const settleDeadline = Date.now() + 120_000
  for (;;) {
    const divergences: string[] = []
    for (const read of reads) {
      const expected = await mysqlRows(read.sql)
      const actual = await pintailQuery(read.sql)
      const canon = (rows: unknown[][]) =>
        JSON.stringify(rows.map((row) => row.map(canonicalValue)))
      if (canon(expected) !== canon(actual)) {
        divergences.push(read.sql)
      }
    }
    if (divergences.length === 0) break
    if (Date.now() > settleDeadline) {
      throw new Error(`post-storm reads never matched MySQL: ${divergences.join('; ')}`)
    }
    await Bun.sleep(POLL_MS)
  }
  log(
    `contention: ${completed} reads (${backpressured} backpressured) raced ${dmlOps} DML ops; ` +
      `all in-storm invariants held and post-storm reads match MySQL`,
  )
}

async function phaseChurn() {
  const random = mulberry32(0xc0ffee)
  const statuses = ['pending', 'processing', 'shipped', 'delivered', 'cancelled']
  const liveEligible = differentialQueries.filter(
    (query) =>
      !query.documentedGap && !query.tables.some((table) => documentedGapTables.has(table)),
  )
  let liveIndex = 0
  let inTransaction = false
  for (let op = 0; op < 400; op += 1) {
    // Every 100 operations, sweep three corpus queries against the live,
    // still-ingesting replica: this is what exercises the mid-ingest
    // aggregate paths rather than only the settled state.
    if (op > 0 && op % 100 === 0 && !inTransaction) {
      for (let sweep = 0; sweep < 3; sweep += 1) {
        const query = liveEligible[liveIndex % liveEligible.length]
        liveIndex += 1
        const diff = await liveQueryConverges(query, 60_000)
        results.push({
          phase: 'churn-live',
          check: `live:${query.name}`,
          status: diff === undefined ? 'PASS' : 'FAIL',
          detail: diff,
        })
        if (diff) for (const line of diff.split('\n')) log(`FAIL live:${query.name} — ${line}`)
      }
    }
    if (!inTransaction && random() < 0.1) {
      await mysqlConnection!.beginTransaction()
      inTransaction = true
    }
    const roll = random()
    if (roll < 0.45) {
      const customer = 1 + Math.floor(random() * 40)
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
          `(${customer}, '${statuses[Math.floor(random() * 5)]}', ${(random() * 500).toFixed(2)}, ` +
          `'2025-0${1 + Math.floor(random() * 9)}-0${1 + Math.floor(random() * 9)}')`,
      )
    } else if (roll < 0.75) {
      await sql(
        `UPDATE orders SET status = '${statuses[Math.floor(random() * 5)]}', ` +
          `total = total + ${(random() * 10 - 5).toFixed(2)} ` +
          `WHERE id = (SELECT id FROM (SELECT MIN(id) + FLOOR(RAND(${op}) * (MAX(id) - MIN(id))) AS id FROM orders) pick)`,
      )
    } else if (roll < 0.9) {
      await sql(`DELETE FROM orders WHERE id = (SELECT id FROM (SELECT MIN(id) AS id FROM orders WHERE status = 'cancelled') pick WHERE pick.id IS NOT NULL)`)
    } else {
      const customer = 1 + Math.floor(random() * 40)
      await sql(`UPDATE customers SET balance = balance + ${(random() * 20 - 10).toFixed(2)} WHERE id = ${customer}`)
    }
    if (inTransaction && random() < 0.3) {
      if (random() < 0.15) await mysqlConnection!.rollback()
      else await mysqlConnection!.commit()
      inTransaction = false
    }
  }
  if (inTransaction) await mysqlConnection!.commit()
}

/// Application servers and BI tools reach Pintail through a connection
/// pool, not a single socket: a fixed set of connections borrowed and
/// returned, where each borrower assumes it gets a clean session. This
/// phase drives a real mysql2 pool the way those clients do.
const POOL_SIZE = 4
const POOL_BORROWS = 40

async function phasePooling() {
  await sql(`INSERT INTO audit_log VALUES ('pooling phase')`)

  const settings = {
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: wireSecret,
    database: DATABASE,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
  }
  const pool = mysql.createPool({
    ...settings,
    connectionLimit: POOL_SIZE,
    waitForConnections: true,
  })
  const phase = 'pooling'
  try {
    // Far more borrows than sockets, issued at once: every borrow must get
    // a working connection and the same answer as the source.
    const [expected] = await mysqlRows('SELECT COUNT(*) FROM orders')
    const answers = await Promise.all(
      Array.from({ length: POOL_BORROWS }, async () => {
        const [rows] = await pool.query<mysql.RowDataPacket[]>({
          sql: 'SELECT COUNT(*) FROM orders',
          rowsAsArray: true,
        })
        return String((rows as unknown as unknown[][])[0][0])
      }),
    )
    const wrong = answers.filter((answer) => answer !== String(expected[0]))
    results.push({
      phase,
      check: `pool:concurrent-borrows(${POOL_BORROWS} over ${POOL_SIZE})`,
      status: wrong.length === 0 ? 'PASS' : 'FAIL',
      detail: wrong.length === 0 ? undefined : `${wrong.length} borrows disagreed: ${wrong[0]}`,
    })

    // Prepared statements are per-connection state; a pool prepares on
    // whichever socket it hands out.
    const prepared = await Promise.all(
      Array.from({ length: POOL_BORROWS }, async (_, index) => {
        const [rows] = await pool.execute<mysql.RowDataPacket[]>({
          sql: 'SELECT COUNT(*) FROM orders WHERE id > ?',
          values: [index % 5],
          rowsAsArray: true,
        })
        return (rows as unknown as unknown[][])[0][0]
      }),
    )
    results.push({
      phase,
      check: 'pool:prepared-statements',
      status: prepared.every((value) => value !== undefined && value !== null) ? 'PASS' : 'FAIL',
    })
  } finally {
    await pool.end()
  }

  // Session state must not survive a borrow. A one-connection pool
  // guarantees the second borrow is the same socket as the first.
  const single = mysql.createPool({ ...settings, connectionLimit: 1, waitForConnections: true })
  try {
    const first = await single.getConnection()
    await first.query("SET time_zone = '+05:30'")
    first.release()
    const second = await single.getConnection()
    const [rows] = await second.query<mysql.RowDataPacket[]>({
      sql: 'SELECT @@session.time_zone',
      rowsAsArray: true,
    })
    second.release()
    const zone = String((rows as unknown as unknown[][])[0][0])
    // Measured against MySQL 8.4 through this exact mysql2 sequence: the
    // second borrow reads '+05:30' there too. mysql2's pool does not reset a
    // connection on release, so session state surviving a borrow is MySQL's
    // behaviour, not a defect. Asserting 'SYSTEM' here asserted something no
    // MySQL server does. The explicit COM_RESET_CONNECTION / COM_CHANGE_USER
    // path is covered in crates/pintail-wire/tests/wire_compat.rs.
    const matches = zone === '+05:30'
    results.push({
      phase,
      check: 'pool:session-state-survives-borrow-like-mysql',
      status: matches ? 'PASS' : 'FAIL',
      detail: matches ? undefined : `expected MySQL's '+05:30', got ${zone}`,
    })
  } finally {
    await single.end()
  }
}

async function phaseOrmCompatibility() {
  const phase = 'orm-compat'
  if (!mysqlEndpoint) throw new Error('MySQL ORM endpoint was not initialized')
  const pintailEndpoint: MysqlEndpoint = {
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: wireSecret,
    database: DATABASE,
  }
  for (const result of await runOrmCompatibility(mysqlEndpoint, pintailEndpoint)) {
    results.push({
      phase,
      check: `${result.client}:${result.check}`,
      status: result.status,
      detail: result.detail,
    })
    if (result.status === 'FAIL') {
      log(`FAIL ${result.client}:${result.check} — ${result.detail}`)
    }
  }
}

/// A LOCAL database: created through the control plane, written through the
/// MySQL wire, and read back through it. There is no MySQL counterpart to
/// diff against - a local database is its own source - so every assertion
/// here is against MySQL's DOCUMENTED behaviour rather than a live server:
/// affected-row counts, the codes a client branches on, and rows surviving
/// a restart (checked in the restart phase, which runs after this one).
async function phaseLocalDatabase() {
  const phase = 'local-database'
  const check = async (name: string, run: () => Promise<void>) => {
    try {
      await run()
      results.push({ phase, check: name, status: 'PASS' })
    } catch (error) {
      results.push({ phase, check: name, status: 'FAIL', detail: String(error) })
      log(`FAIL ${name} — ${error}`)
    }
  }

  const created = await api<{ id: string }>('/api/databases/local', {
    method: 'POST',
    body: { name: LOCAL_DATABASE },
  })
  localDatabaseId = created.id
  const key = await api<{ secret: string }>(`/api/databases/${localDatabaseId}/api-keys`, {
    method: 'POST',
    body: { name: 'e2e-local', scopes: ['query', 'read'] },
  })
  localWire = await mysql.createConnection({
    host: '127.0.0.1',
    port: pintailWirePort,
    user: LOCAL_DATABASE,
    password: key.secret,
    database: LOCAL_DATABASE,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
  })

  await check('create table returns an OK packet', async () => {
    const [result] = await localWire!.query(
      'CREATE TABLE notes (id BIGINT UNSIGNED NOT NULL, body VARCHAR(64) NOT NULL, ' +
        'tag VARCHAR(16), PRIMARY KEY (id))',
    )
    const affected = (result as mysql.ResultSetHeader).affectedRows
    if (affected !== 0) throw new Error(`DDL reported ${affected} affected rows`)
  })

  await check('insert reports its affected rows', async () => {
    const [result] = await localWire!.query(
      "INSERT INTO notes (id, body, tag) VALUES (1, 'alpha', 'x'), (2, 'beta', NULL)",
    )
    const affected = (result as mysql.ResultSetHeader).affectedRows
    if (affected !== 2) throw new Error(`expected 2 affected rows, got ${affected}`)
  })

  await check('the rows read back through the same connection', async () => {
    const [rows] = await localWire!.query('SELECT id, body, tag FROM notes ORDER BY id')
    const actual = JSON.stringify(rows)
    const expected = JSON.stringify([
      { id: '1', body: 'alpha', tag: 'x' },
      { id: '2', body: 'beta', tag: null },
    ])
    if (actual !== expected) throw new Error(`read back ${actual}`)
  })

  await check('aggregates and predicates work on a local table', async () => {
    const [rows] = await localWire!.query(
      "SELECT COUNT(*) AS n, MIN(body) AS lo FROM notes WHERE tag IS NULL OR tag = 'x'",
    )
    const actual = JSON.stringify(rows)
    if (actual !== JSON.stringify([{ n: '2', lo: 'alpha' }])) {
      throw new Error(`aggregate answered ${actual}`)
    }
  })

  // Each rejection is a code a client branches on, not a generic failure.
  for (const [name, sql, code] of [
    ['duplicate key is 1062', "INSERT INTO notes (id, body) VALUES (1, 'again')", 1062],
    ['existing table is 1050', 'CREATE TABLE notes (id BIGINT PRIMARY KEY)', 1050],
    ['not-null violation is 1048', 'INSERT INTO notes (id, body) VALUES (3, NULL)', 1048],
    ['unknown table is 1146', "INSERT INTO absent (id) VALUES (1)", 1146],
    ['unknown column is 1054', "INSERT INTO notes (id, nope) VALUES (3, 'x')", 1054],
  ] as Array<[string, string, number]>) {
    await check(name, async () => {
      try {
        await localWire!.query(sql)
      } catch (error) {
        const actual = (error as { errno?: number }).errno
        if (actual !== code) throw new Error(`expected errno ${code}, got ${actual}`)
        return
      }
      throw new Error('the statement was accepted')
    })
  }

  await check('a refused write leaves the table unchanged', async () => {
    const [rows] = await localWire!.query('SELECT COUNT(*) AS n FROM notes')
    const n = (rows as Array<{ n: string }>)[0].n
    if (n !== '2') throw new Error(`expected 2 rows after the refusals, got ${n}`)
  })

  await check('the replicated database still refuses writes', async () => {
    try {
      await pintailQuery("INSERT INTO orders (customer_id, status, total, placed_on) " +
        "VALUES (1, 'pending', 1.00, '2025-01-01')")
    } catch (error) {
      const message = String(error)
      if (!message.includes('read-only')) throw new Error(`refused with ${message}`)
      return
    }
    throw new Error('a replicated database accepted a write')
  })

  await check('a local database is not scheduled for replication', async () => {
    const status = await api<{ state: string }>(`/api/databases/${localDatabaseId}/status`)
    if (status.state === 'streaming' || status.state === 'polling') {
      throw new Error(`a local database reported replication state ${status.state}`)
    }
    // Every source operation must refuse it rather than attempt a probe
    // against a DSN that is empty by construction.
    const response = await fetch(`${pintailUrl}/api/databases/${localDatabaseId}/probe`, {
      headers: { Authorization: `Bearer ${token}` },
    })
    if (response.ok) throw new Error('probing a local database succeeded')
  })
}

/// Re-checks the local database after the restart phase's SIGKILL: a
/// committed row that does not survive a kill was never committed.
async function verifyLocalDurability() {
  if (!localDatabaseId) return
  try {
    // The old connection died with the process.
    localWire = undefined
    const key = await api<{ secret: string }>(`/api/databases/${localDatabaseId}/api-keys`, {
      method: 'POST',
      body: { name: `e2e-local-${Date.now()}`, scopes: ['query', 'read'] },
    })
    const connection = await mysql.createConnection({
      host: '127.0.0.1',
      port: pintailWirePort,
      user: LOCAL_DATABASE,
      password: key.secret,
      database: LOCAL_DATABASE,
      supportBigNumbers: true,
      bigNumberStrings: true,
      dateStrings: true,
    })
    const [rows] = await connection.query('SELECT id, body FROM notes ORDER BY id')
    await connection.end()
    const actual = JSON.stringify(rows)
    const expected = JSON.stringify([
      { id: '1', body: 'alpha' },
      { id: '2', body: 'beta' },
    ])
    if (actual !== expected) throw new Error(`after restart the local table read ${actual}`)
    results.push({ phase: 'restart', check: 'local:rows survive a SIGKILL', status: 'PASS' })
  } catch (error) {
    results.push({
      phase: 'restart',
      check: 'local:rows survive a SIGKILL',
      status: 'FAIL',
      detail: String(error),
    })
    log(`FAIL local:rows survive a SIGKILL — ${error}`)
  }
}

async function phaseRestart() {
  log('SIGKILLing pintail mid-stream')
  pintailProcess!.kill(9)
  await pintailProcess!.exited
  // Write while the replica is down; the checkpoint must replay all of it.
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (9, 'processing', 123.45, '2025-08-01')`)
  await sql(`UPDATE customers SET tier = 'enterprise' WHERE id = 9`)
  await sql(`DELETE FROM orders WHERE id = (SELECT id FROM (SELECT MAX(id) AS id FROM orders) pick)`)
  await startPintail()
  await verifyLocalDurability()
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (10, 'shipped', 67.89, '2025-08-02')`)
}

/// Sorted-sample percentile, for the latency phases below.
function percentile(samples: number[], fraction: number): number {
  if (samples.length === 0) return 0
  const sorted = [...samples].sort((a, b) => a - b)
  return sorted[Math.min(sorted.length - 1, Math.floor(fraction * sorted.length))]!
}

/// Round-trip of one API call in milliseconds, or -1 when it failed.
async function timedApi(path: string): Promise<number> {
  const started = performance.now()
  try {
    await api(path)
    return performance.now() - started
  } catch {
    return -1
  }
}

/// The activity feed over a deployment's worth of control-plane history.
///
/// A replication cycle writes one sync_runs row every cadence and nothing
/// prunes it, so a long-running deployment carries hundreds of thousands.
/// The dashboard reads that table newest-first on every load, and a
/// production instance with 632,000 rows took 136 seconds per call while
/// polls piled up behind each other until they starved HTTP itself. The
/// history is seeded directly while pintail is down - the honest shape of
/// the bug is "the table got big", not "the supervisor ran for a year".
async function phaseActivityHistory() {
  const phase = 'activity-history'
  const HISTORY_ROWS = 150_000
  let seeded = 0

  log(`SIGKILLing pintail to seed ${HISTORY_ROWS} sync_runs rows of history`)
  pintailProcess!.kill(9)
  await pintailProcess!.exited
  const meta = new Database(join(pintailDataDir, 'pintail-meta.db'))
  try {
    const insert = meta.prepare(
      'INSERT INTO sync_runs (id, db_id, table_name, kind, status, rows, bytes, duration_ms, error, started_at) ' +
        "VALUES (?1, ?2, NULL, 'cdc', 'completed', 0, 0, 3, NULL, ?3)",
    )
    const seed = meta.transaction((count: number) => {
      // Spread across a day so the newest-first order is not insertion order.
      for (let index = 0; index < count; index += 1) {
        const second = index % 86_400
        const stamp = `2026-08-01T${String(Math.floor(second / 3600)).padStart(2, '0')}:${String(
          Math.floor(second / 60) % 60,
        ).padStart(2, '0')}:${String(second % 60).padStart(2, '0')}.${String(index % 1000).padStart(3, '0')}Z`
        insert.run(`hist_${index}`, databaseId, stamp)
      }
    })
    seed(HISTORY_ROWS)
    // Written to the file pintail opens, or the rest of this phase measures
    // an empty table and passes for nothing.
    seeded = Number(
      (meta.query('SELECT COUNT(*) AS n FROM sync_runs WHERE db_id = ?1').get(databaseId) as { n: number }).n,
    )
  } finally {
    meta.close()
  }
  record(
    phase,
    'activity-history:the history is in the control plane pintail reads',
    seeded >= HISTORY_ROWS ? 'PASS' : 'FAIL',
    `${seeded} sync_runs rows for ${databaseId}`,
  )
  await startPintail()
  const page = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=200`)
  record(
    phase,
    'activity-history:the feed pages the full history',
    page.length === 200 ? 'PASS' : 'FAIL',
    `limit=200 returned ${page.length}`,
  )

  // The two shapes the dashboard issues: scoped to one database, and the
  // workspace-wide feed. Sequential first, so the number is the query's own.
  const scoped: number[] = []
  const unscoped: number[] = []
  for (let round = 0; round < 20; round += 1) {
    scoped.push(await timedApi(`/api/activity?db=${databaseId}&limit=200`))
    unscoped.push(await timedApi('/api/activity?limit=200'))
  }
  const failed = [...scoped, ...unscoped].some((sample) => sample < 0)
  record(
    phase,
    'activity-history:scoped feed stays fast over a large history',
    !failed && percentile(scoped, 0.95) < 400 ? 'PASS' : 'FAIL',
    `p50 ${percentile(scoped, 0.5).toFixed(0)}ms p95 ${percentile(scoped, 0.95).toFixed(0)}ms over ${HISTORY_ROWS} rows`,
  )
  record(
    phase,
    'activity-history:workspace feed stays fast over a large history',
    !failed && percentile(unscoped, 0.95) < 400 ? 'PASS' : 'FAIL',
    `p50 ${percentile(unscoped, 0.5).toFixed(0)}ms p95 ${percentile(unscoped, 0.95).toFixed(0)}ms`,
  )

  // Then concurrently, the way polls actually arrive - and /health alongside,
  // because the production failure was the feed starving everything else.
  const concurrent: number[] = []
  const health: number[] = []
  for (let round = 0; round < 3; round += 1) {
    const batch = await Promise.all([
      ...Array.from({ length: 25 }, () => timedApi(`/api/activity?db=${databaseId}&limit=200`)),
      ...Array.from({ length: 5 }, () => timedApi('/health')),
    ])
    concurrent.push(...batch.slice(0, 25))
    health.push(...batch.slice(25))
  }
  record(
    phase,
    'activity-history:25 concurrent feed reads do not pile up',
    concurrent.every((sample) => sample >= 0) && percentile(concurrent, 0.99) < 2_000 ? 'PASS' : 'FAIL',
    `p50 ${percentile(concurrent, 0.5).toFixed(0)}ms p99 ${percentile(concurrent, 0.99).toFixed(0)}ms`,
  )
  record(
    phase,
    'activity-history:health answers while the feed is hammered',
    health.every((sample) => sample >= 0) && percentile(health, 0.95) < 500 ? 'PASS' : 'FAIL',
    `health p95 ${percentile(health, 0.95).toFixed(0)}ms`,
  )
}

/// Many dashboards open at once while replication is live.
///
/// The production cascade was polls arriving faster than they drained,
/// saturating every runtime worker so that even static assets took minutes
/// and CDC lag grew. This holds 25 pollers on the dashboard's endpoints for
/// twenty seconds while rows are written at the source, and asks three
/// things: nothing errors, latency stays bounded, and the replica still
/// converges - the last one is the proof the supervisor was not starved.
async function phasePollStorm() {
  const phase = 'poll-storm'
  const POLLERS = 25
  const DURATION_MS = 20_000
  const WRITES = 200
  const before = Number(await mysqlCount('orders'))

  const latencies: number[] = []
  const health: number[] = []
  let errors = 0
  const endpoints = [
    `/api/activity?db=${databaseId}&limit=200`,
    `/api/dlq?db=${databaseId}`,
    `/api/databases/${databaseId}/status`,
    '/status',
  ]
  const deadline = Date.now() + DURATION_MS
  const poller = async (seat: number) => {
    let turn = seat
    while (Date.now() < deadline) {
      const sample = await timedApi(endpoints[turn % endpoints.length]!)
      if (sample < 0) errors += 1
      else latencies.push(sample)
      turn += 1
      await Bun.sleep(100) // an open tab's cadence, not a tight loop
    }
  }
  const heartbeat = async () => {
    while (Date.now() < deadline) {
      health.push(await timedApi('/health'))
      await Bun.sleep(250)
    }
  }
  const writer = async () => {
    for (let index = 0; index < WRITES; index += 1) {
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (${1 + (index % 8)}, 'processing', ${index}.25, '2025-08-03')`,
      )
      await Bun.sleep(DURATION_MS / WRITES)
    }
  }
  await Promise.all([...Array.from({ length: POLLERS }, (_, seat) => poller(seat)), heartbeat(), writer()])

  record(
    phase,
    'poll-storm:no request fails under 25 open dashboards',
    errors === 0 ? 'PASS' : 'FAIL',
    `${errors} failed of ${latencies.length + errors}`,
  )
  record(
    phase,
    'poll-storm:latency stays bounded',
    percentile(latencies, 0.99) < 3_000 ? 'PASS' : 'FAIL',
    `${latencies.length} requests: p50 ${percentile(latencies, 0.5).toFixed(0)}ms p99 ${percentile(latencies, 0.99).toFixed(0)}ms`,
  )
  record(
    phase,
    'poll-storm:health never stalls',
    health.every((sample) => sample >= 0) && percentile(health, 0.99) < 1_000 ? 'PASS' : 'FAIL',
    `health p99 ${percentile(health, 0.99).toFixed(0)}ms`,
  )
  const converged = await waitUntil(async () => (await replicaCount('orders')) === before + WRITES, 120_000)
  record(
    phase,
    'poll-storm:replication keeps pace under the storm',
    converged ? 'PASS' : 'FAIL',
    `orders replica ${await replicaCount('orders')} vs source ${before + WRITES}`,
  )
}

/// A restart in the middle of a database's first snapshot.
///
/// No job survives a restart. The tables caught mid-copy are quarantined
/// at boot so partial data never answers as healthy - that part is by
/// design. What this phase demands is the other half: that the database
/// itself is not left behind. A production instance sat in 'snapshotting'
/// for over a day after exactly this, with 108 tables quarantined and 134
/// never copied, because a database in that state is never scheduled and
/// so never reaches the repair that would have drained them. Nobody clicks
/// anything here; recovery has to happen on its own.
async function phaseRestartDuringSnapshot() {
  const phase = 'restart-during-snapshot'
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  const schema = `interrupted_${nonce}`
  const ROWS = 300_000 // three durable chunks: enough copy to land a kill inside

  await sql(`CREATE DATABASE ${schema}`)
  await sql(`CREATE TABLE ${schema}.seed (n INT PRIMARY KEY)`)
  await sql(
    `INSERT INTO ${schema}.seed VALUES ${Array.from({ length: 100 }, (_, n) => `(${n})`).join(',')}`,
  )
  await sql(`CREATE TABLE ${schema}.big (id INT PRIMARY KEY, payload VARCHAR(64) NOT NULL)`)
  await sql(
    `INSERT INTO ${schema}.big SELECT a.n * 10000 + b.n * 100 + c.n, REPEAT('x', 48) ` +
      `FROM ${schema}.seed a, ${schema}.seed b, ${schema}.seed c WHERE a.n < 30`,
  )
  await sql(`CREATE TABLE ${schema}.small (id INT PRIMARY KEY, label VARCHAR(16) NOT NULL)`)
  await sql(`INSERT INTO ${schema}.small VALUES (1, 'one'), (2, 'two')`)

  let created = ''
  try {
    created = (
      await api<{ id: string }>('/api/databases', {
        method: 'POST',
        body: { name: schema, dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${schema}`, mode: 'cdc' },
      })
    ).id
    await reprobe(created)

    // A WRITE lock held by another session blocks every other session's
    // reads of the table, so the copy of `big` cannot finish while it is
    // held: the kill below lands with the snapshot provably in flight, on
    // every host speed, instead of racing a sub-second copy.
    const locker = await waitForMysql(host, mysqlPort)
    let caughtMidCopy = false
    try {
      await locker.query(`LOCK TABLES ${schema}.big WRITE`)
      await api(`/api/databases/${created}/snapshot`, { method: 'POST', body: { force: false } })
      // The database row reads 'created' throughout a first snapshot and
      // 'snapshotting' only during a forced one; the TABLE rows say
      // 'snapshotting' in both, so that is the in-flight signal.
      caughtMidCopy = await waitUntil(async () => {
        const status = await api<{ tables: Array<{ name: string; state: string }> }>(
          `/api/databases/${created}/snapshot/status`,
        )
        return status.tables.some((table) => table.state === 'snapshotting')
      }, 60_000)
      if (!caughtMidCopy) {
        record(phase, 'restart-during-snapshot:interrupts a copy in flight', 'FAIL', 'no table ever reported snapshotting')
        return
      }
      log('SIGKILLing pintail mid-snapshot')
      pintailProcess!.kill(9)
      await pintailProcess!.exited
      record(phase, 'restart-during-snapshot:interrupts a copy in flight', 'PASS')
    } finally {
      await locker.query('UNLOCK TABLES').catch(() => undefined)
      await locker.end().catch(() => undefined)
    }

    await startPintail()
    // No operator action from here on.
    const resumed = await waitUntil(async () => {
      const status = await api<{ state: string }>(`/api/databases/${created}/snapshot/status`)
      return status.state === 'streaming' || status.state === 'polling'
    }, 240_000)
    record(
      phase,
      'restart-during-snapshot:the database resumes replicating on its own',
      resumed ? 'PASS' : 'FAIL',
      resumed ? undefined : await replicationDiagnostics(created),
    )
    if (!resumed) return

    const complete = await waitUntil(async () => {
      const count = await api<{ count: number }>(`/api/tables/big/count?db=${created}`)
      return count.count === ROWS
    }, 240_000)
    record(
      phase,
      'restart-during-snapshot:every row arrives after the resume',
      complete ? 'PASS' : 'FAIL',
      `big: ${(await api<{ count: number }>(`/api/tables/big/count?db=${created}`).catch(() => ({ count: -1 }))).count} of ${ROWS}`,
    )
    const tables = await api<TableSummary[]>(`/api/tables?db=${created}`)
    const stuck = tables.filter((table) => table.state !== 'streaming' && table.state !== 'polling')
    record(
      phase,
      'restart-during-snapshot:no table is left quarantined',
      stuck.length === 0 ? 'PASS' : 'FAIL',
      stuck.map((table) => `${table.name}:${table.state}`).join(', ') || undefined,
    )
  } finally {
    if (created) await api(`/api/databases/${created}`, { method: 'DELETE' }).catch(() => undefined)
    await sql(`DROP DATABASE IF EXISTS ${schema}`)
  }
}

/// Peak RSS of the server process, in MB.
async function serverRssMb(): Promise<number> {
  if (!pintailProcess?.pid) return 0
  const child = Bun.spawn(['ps', '-o', 'rss=', '-p', String(pintailProcess.pid)], {
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const text = await new Response(child.stdout).text()
  await child.exited
  return Number(text.trim() || '0') / 1024
}

/// Memory pressure: the small-container scenario, all at once.
///
/// A 256 MB process budget, a 32 MB per-query ceiling and an admission
/// window of eight, against forty-eight wire clients reconnecting for every
/// query, eight HTTP query clients, six open dashboards and a CDC writer
/// committing into the table everyone is sorting. Nothing is allowed to
/// fail except the two designed answers - admission refusal and the memory
/// ceiling - and afterwards the server has to be the same server: inside
/// its ceiling, caught up, and answering plain queries, which is what
/// proves no permit or budget byte leaked under the storm.
async function phaseMemoryPressure() {
  const phase = 'memory-pressure'
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  const schema = `pressure_${nonce}`
  const ROWS = 200_000
  const WIRE_CLIENTS = 48
  const QUERIES_PER_CLIENT = 5
  const HTTP_CLIENTS = 8
  const DASHBOARDS = 6
  const BUDGET_MB = 256
  const RSS_CEILING_MB = 1024

  await sql(`CREATE DATABASE ${schema}`)
  await sql(`CREATE TABLE ${schema}.seed (n INT PRIMARY KEY)`)
  await sql(
    `INSERT INTO ${schema}.seed VALUES ${Array.from({ length: 100 }, (_, n) => `(${n})`).join(',')}`,
  )
  await sql(
    `CREATE TABLE ${schema}.big (id INT PRIMARY KEY, bucket INT NOT NULL, payload VARCHAR(64) NOT NULL)`,
  )
  await sql(
    `INSERT INTO ${schema}.big SELECT a.n * 10000 + b.n * 100 + c.n, c.n, CONCAT(REPEAT('x', 40), b.n) ` +
      `FROM ${schema}.seed a, ${schema}.seed b, ${schema}.seed c WHERE a.n < 20`,
  )

  const stopPintail = async () => {
    try {
      await pintailWire?.end()
    } catch {}
    pintailWire = undefined
    if (pintailProcess) {
      if (pintailProcess.exitCode === null) pintailProcess.kill()
      await pintailProcess.exited
      pintailProcess = undefined
    }
  }
  // Load shedding and the memory ceiling are the designed answers to
  // overload; anything else the storm surfaces is a defect.
  const kind = (message: string) => {
    if (/concurrent queries/i.test(message)) return 'admission-refused'
    if (/memory limit exceeded|memory budget/i.test(message)) return 'query-memory-limit'
    if (/ECONNRESET|socket hang up|closed/i.test(message)) return 'connection-dropped'
    if (/ETIMEDOUT|timeout/i.test(message)) return 'timeout'
    return 'other'
  }

  let created = ''
  let secret = ''
  let restarted = false
  try {
    created = (
      await api<{ id: string }>('/api/databases', {
        method: 'POST',
        body: { name: schema, dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${schema}`, mode: 'cdc' },
      })
    ).id
    await reprobe(created)
    secret = (
      await api<{ secret: string }>(`/api/databases/${created}/api-keys`, {
        method: 'POST',
        body: { name: 'pressure', scopes: ['query', 'read'] },
      })
    ).secret
    await api(`/api/databases/${created}/snapshot`, { method: 'POST', body: { force: false } })
    const ready = await waitUntil(async () => {
      const status = await api<{ state: string }>(`/api/databases/${created}/snapshot/status`)
      return status.state === 'streaming' || status.state === 'polling'
    }, 240_000)
    if (!ready) {
      record(phase, 'memory-pressure:the source replicates before the storm', 'FAIL', await replicationDiagnostics(created))
      return
    }

    await stopPintail()
    restarted = true
    await startPintail(32 * 1024 * 1024, {
      PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES: String(BUDGET_MB * 1024 * 1024),
      PINTAIL_MAX_CONCURRENT_QUERIES: '8',
    })

    const wire = () =>
      mysql.createConnection({
        host: '127.0.0.1',
        port: pintailWirePort,
        user: schema,
        password: secret,
        database: schema,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
      })
    const count = async (): Promise<number> => {
      let connection: mysql.Connection | undefined
      try {
        connection = await wire()
        const [rows] = await connection.query<mysql.RowDataPacket[]>({
          sql: 'SELECT COUNT(*) FROM big',
          rowsAsArray: true,
        })
        return Number((rows[0] as unknown[])[0])
      } catch {
        return -1
      } finally {
        await connection?.end().catch(() => undefined)
      }
    }
    // A sort that cannot fit 32 MB, an aggregate over every row, and a
    // scan-only count: three different paths to the ceiling.
    const statements = [
      'SELECT * FROM big ORDER BY payload DESC, id DESC LIMIT 500',
      'SELECT bucket, COUNT(*) c, MAX(payload) m FROM big GROUP BY bucket ORDER BY c DESC, bucket',
      'SELECT COUNT(*), MIN(id), MAX(id) FROM big',
    ]
    const endpoints = [
      `/api/activity?db=${created}&limit=200`,
      `/api/dlq?db=${created}`,
      `/api/databases/${created}/status`,
      '/status',
    ]
    const errors: Record<string, number> = {}
    const fail = (error: unknown) => {
      const bucket = kind(String(error))
      errors[bucket] = (errors[bucket] ?? 0) + 1
    }
    const health: number[] = []
    const wireLatencies: number[] = []
    let wireOk = 0
    let httpOk = 0
    let dashboardOk = 0
    let dashboardFailed = 0
    let peakRss = 0
    let written = 0
    let nextId = ROWS * 10
    let running = true

    const wireClient = async (seat: number) => {
      for (let turn = 0; turn < QUERIES_PER_CLIENT; turn += 1) {
        let connection: mysql.Connection | undefined
        try {
          connection = await wire()
          const started = performance.now()
          await connection.query({
            sql: statements[(seat + turn) % statements.length]!,
            rowsAsArray: true,
          })
          wireLatencies.push(performance.now() - started)
          wireOk += 1
        } catch (error) {
          fail(error)
        } finally {
          await connection?.end().catch(() => undefined)
        }
      }
    }
    const httpClient = async (seat: number) => {
      let turn = seat
      while (running) {
        try {
          await api('/api/query', {
            method: 'POST',
            body: { db: created, sql: statements[turn % statements.length] },
          })
          httpOk += 1
        } catch (error) {
          fail(error)
        }
        turn += 1
      }
    }
    const dashboard = async (seat: number) => {
      let turn = seat
      while (running) {
        const sample = await timedApi(endpoints[turn % endpoints.length]!)
        if (sample < 0) dashboardFailed += 1
        else dashboardOk += 1
        turn += 1
        await Bun.sleep(100)
      }
    }
    const heartbeat = async () => {
      while (running) {
        health.push(await timedApi('/health'))
        await Bun.sleep(250)
      }
    }
    const sampler = async () => {
      while (running) {
        peakRss = Math.max(peakRss, await serverRssMb())
        await Bun.sleep(200)
      }
    }
    const writer = async () => {
      while (running) {
        const values = Array.from({ length: 100 }, () => {
          const id = nextId
          nextId += 1
          return `(${id}, ${id % 100}, 'w${id}')`
        })
        await sql(`INSERT INTO ${schema}.big VALUES ${values.join(',')}`)
        written += values.length
        await Bun.sleep(250)
      }
    }
    const storm = Promise.all(
      Array.from({ length: WIRE_CLIENTS }, (_, seat) => wireClient(seat)),
    ).then(() => {
      running = false
    })
    await Promise.all([
      storm,
      ...Array.from({ length: HTTP_CLIENTS }, (_, seat) => httpClient(seat)),
      ...Array.from({ length: DASHBOARDS }, (_, seat) => dashboard(seat)),
      heartbeat(),
      sampler(),
      writer(),
    ])

    const summary =
      `wire ${wireOk} ok, http ${httpOk} ok, dashboards ${dashboardOk} ok; ` +
      (Object.entries(errors)
        .map(([bucket, total]) => `${bucket}×${total}`)
        .join(', ') || 'no errors')
    const alive =
      pintailProcess?.exitCode === null &&
      (await fetch(`${pintailUrl}/health`)
        .then((response) => response.ok)
        .catch(() => false))
    record(phase, 'memory-pressure:the process survives the storm', alive ? 'PASS' : 'FAIL', summary)
    const undesigned = Object.entries(errors).filter(
      ([bucket]) => bucket !== 'admission-refused' && bucket !== 'query-memory-limit',
    )
    record(
      phase,
      'memory-pressure:every failure is a designed refusal',
      undesigned.length === 0 && dashboardFailed === 0 ? 'PASS' : 'FAIL',
      `${undesigned.map(([bucket, total]) => `${bucket}×${total}`).join(', ') || 'only refusals'}; ${dashboardFailed} dashboard requests failed`,
    )
    record(
      phase,
      'memory-pressure:work still gets done',
      wireOk > 0 && httpOk > 0 ? 'PASS' : 'FAIL',
      `wire ${wireOk} of ${WIRE_CLIENTS * QUERIES_PER_CLIENT}, http ${httpOk}`,
    )
    // A wire query waits at most the admission timeout and then runs for
    // about a second here; anything far beyond that is time spent waiting
    // for a runtime worker, which is what an HTTP query running inline on
    // one used to cost every other connection.
    wireLatencies.sort((left, right) => left - right)
    record(
      phase,
      'memory-pressure:wire queries are not starved by the HTTP surface',
      wireLatencies.length > 0 && percentile(wireLatencies, 0.99) < 15_000 ? 'PASS' : 'FAIL',
      `wire p50 ${percentile(wireLatencies, 0.5).toFixed(0)}ms p99 ${percentile(wireLatencies, 0.99).toFixed(0)}ms over ${wireLatencies.length} queries`,
    )
    record(
      phase,
      'memory-pressure:health never stalls',
      health.every((sample) => sample >= 0) && percentile(health, 0.99) < 2_000 ? 'PASS' : 'FAIL',
      `health p99 ${percentile(health, 0.99).toFixed(0)}ms over ${health.length} samples`,
    )
    record(
      phase,
      'memory-pressure:the process stays inside its ceiling',
      peakRss > 0 && peakRss < RSS_CEILING_MB ? 'PASS' : 'FAIL',
      `peak RSS ${peakRss.toFixed(0)}MB with a ${BUDGET_MB}MB budget`,
    )
    const caughtUp = await waitUntil(async () => (await count()) >= ROWS + written, 120_000)
    record(
      phase,
      'memory-pressure:the replica catches up after the storm',
      caughtUp ? 'PASS' : 'FAIL',
      `big ${await count()} vs source ${ROWS + written}`,
    )
    let recovered = 0
    for (const statement of statements) {
      let connection: mysql.Connection | undefined
      try {
        connection = await wire()
        await connection.query({ sql: statement, rowsAsArray: true })
        recovered += 1
      } catch {
      } finally {
        await connection?.end().catch(() => undefined)
      }
    }
    record(
      phase,
      'memory-pressure:queries recover once the storm passes',
      recovered === statements.length ? 'PASS' : 'FAIL',
      `${recovered} of ${statements.length} sequential queries succeeded`,
    )
  } finally {
    if (restarted) {
      await stopPintail()
      await startPintail()
    }
    if (created) await api(`/api/databases/${created}`, { method: 'DELETE' }).catch(() => undefined)
    await sql(`DROP DATABASE IF EXISTS ${schema}`)
  }
}

async function phaseExecutionBudget() {
  const phase = 'execution-budget'
  const record = (check: string, ok: boolean, detail?: string) => {
    results.push({ phase, check, status: ok ? 'PASS' : 'FAIL', detail: ok ? undefined : detail })
  }

  // A self-join with no selective predicate: output grows with the square of
  // the rows per join key, so it is the shape that runs away in production
  // and the one an execution ceiling exists for.
  const runaway =
    'SELECT COUNT(*) AS n FROM order_items a ' +
    'JOIN order_items b ON a.order_id = b.order_id ' +
    'JOIN order_items c ON c.order_id = b.order_id ' +
    'JOIN order_items d ON d.order_id = c.order_id'

  // 1. The hint is honoured, and the error is MySQL's 1317 - drivers key
  //    their retry and timeout handling on the code, not the message.
  const started = Date.now()
  try {
    await pintailQuery(`SELECT /*+ MAX_EXECUTION_TIME(1) */ ${runaway.slice('SELECT '.length)}`)
    record('hint:interrupts a runaway join', false, 'the query completed without hitting its budget')
  } catch (failure) {
    const text = String(failure)
    const interrupted = /1317|max_execution_time/i.test(text)
    record('hint:interrupts a runaway join', interrupted, text)
    // And it interrupts promptly rather than merely reporting a timeout at
    // the end: the deadline is checked between batches, so a 1ms budget on a
    // multi-second query must not take multiple seconds to surface.
    const elapsed = Date.now() - started
    record(
      'hint:interrupts promptly',
      elapsed < 15_000,
      `took ${elapsed}ms to honour a 1ms budget`,
    )
  }

  // 2. A budget the query finishes inside must not interfere.
  try {
    const rows = await pintailQuery(
      'SELECT /*+ MAX_EXECUTION_TIME(60000) */ COUNT(*) AS n FROM orders',
    )
    record('hint:a generous budget runs to completion', Number(rows[0]?.[0]) > 0, JSON.stringify(rows))
  } catch (failure) {
    record('hint:a generous budget runs to completion', false, String(failure))
  }

  // 3. The hint tightens the session ceiling but must never loosen it, or an
  //    author could write their way out of an administrator's limit.
  try {
    await pintailQuery('SET SESSION max_execution_time = 1')
    let escaped = false
    try {
      await pintailQuery(`SELECT /*+ MAX_EXECUTION_TIME(600000) */ ${runaway.slice('SELECT '.length)}`)
      escaped = true
    } catch {}
    record('hint:cannot loosen the session ceiling', !escaped, 'a generous hint outran a 1ms session limit')
  } finally {
    await pintailQuery('SET SESSION max_execution_time = 0')
  }

  // 4. A hint Pintail does not implement still rejects. Silently ignoring it
  //    would run the query without the behaviour its author asked for.
  try {
    await pintailQuery('SELECT /*+ BKA(orders) */ COUNT(*) FROM orders')
    record('hint:an unimplemented hint rejects', false, 'BKA was accepted and silently ignored')
  } catch (failure) {
    record('hint:an unimplemented hint rejects', true, String(failure))
  }
}

async function phaseSpill() {
  const phase = 'spill'
  const repeatedJoinInput = Array.from(
    { length: 8 },
    () => 'SELECT i.order_id, c.id FROM order_items i CROSS JOIN customers c',
  ).join(' UNION ALL ')
  const checks: Array<[string, string, number]> = [
    [
      'sort',
      'SELECT o.id, c.id FROM orders o CROSS JOIN customers c ' +
        'ORDER BY c.id DESC, o.id',
      2 * 1024 * 1024,
    ],
    [
      'aggregate',
      'SELECT o.id, c.id, COUNT(*) FROM orders o CROSS JOIN customers c ' +
        'GROUP BY o.id, c.id',
      // MySQL 8.0's preceding mutation phases leave one input batch 136
      // bytes above 4 MiB. Give the batch headroom while keeping the limit
      // far below the grouped state, which still must spill.
      5 * 1024 * 1024,
    ],
    [
      'distinct',
      'SELECT DISTINCT o.id, c.id FROM orders o CROSS JOIN customers c',
      2 * 1024 * 1024,
    ],
    [
      'join',
      `SELECT COUNT(*) FROM orders o JOIN (${repeatedJoinInput}) d ON o.id = d.order_id`,
      5 * 512 * 1024,
    ],
  ]

  const stopPintail = async () => {
    try {
      await pintailWire?.end()
    } catch {}
    pintailWire = undefined
    if (pintailProcess) {
      if (pintailProcess.exitCode === null) pintailProcess.kill()
      await pintailProcess.exited
      pintailProcess = undefined
    }
  }

  try {
    await stopPintail()
    for (const [operator, sql, memoryLimit] of checks) {
      // The ordinary E2E process uses the production default. Each short-lived
      // restart admits one decoded input batch but keeps the ceiling below the
      // operator's accumulated state, so the spillable allocation is what
      // crosses the boundary. Aggregate batches have a larger working bound.
      await startPintail(memoryLimit)
      try {
        const rows = await pintailQuery(`EXPLAIN ANALYZE ${sql}`)
        const plan = rows.flat().map(String).join('\n')
        const spilled = /Spill files=[1-9][0-9]* bytes=[1-9][0-9]*/.test(plan)
        results.push({
          phase,
          check: `forced-spill:${operator}`,
          status: spilled ? 'PASS' : 'FAIL',
          detail: spilled ? undefined : `no spill reported:\n${plan}`,
        })
      } catch (error) {
        results.push({
          phase,
          check: `forced-spill:${operator}`,
          status: 'FAIL',
          detail: String(error),
        })
      } finally {
        await stopPintail()
      }
    }
  } finally {
    await stopPintail()
    await startPintail()
  }
}

/// Exercises the control-plane routes against the deployed binary: auth,
/// database lifecycle on a throwaway registration, table metadata, API-key
/// enable/disable round trip, mode switching, resync/reconcile, and the
/// observability endpoints. Each route records its own PASS/FAIL.
async function phaseControlPlane() {
  const check = async (name: string, run: () => Promise<void>) => {
    try {
      await run()
      results.push({ phase: 'control-plane', check: `api:${name}`, status: 'PASS' })
    } catch (error) {
      results.push({
        phase: 'control-plane',
        check: `api:${name}`,
        status: 'FAIL',
        detail: String(error),
      })
      log(`FAIL api:${name} — ${error}`)
    }
  }

  await check('auth login issues a fresh token', async () => {
    const login = await api<{ token: string }>('/api/auth/login', {
      method: 'POST',
      auth: false,
      body: { email: 'e2e@pintail.local', password: 'e2e-gate-password' },
    })
    if (!login.token) throw new Error('login returned no token')
    token = login.token
  })
  await check('auth setup status responds', async () => {
    await api('/api/auth/setup/status', { auth: false })
  })
  await check('health, status, and metrics respond', async () => {
    for (const path of ['/health', '/status', '/metrics']) {
      const response = await fetch(`${pintailUrl}${path}`, {
        headers: { Authorization: `Bearer ${token}` },
      })
      if (!response.ok) throw new Error(`${path} returned ${response.status}`)
    }
  })
  await check('databases list and detail agree', async () => {
    const list = await api<Array<{ id: string }>>('/api/databases')
    if (!list.some((database) => database.id === databaseId)) {
      throw new Error('registered database missing from the list')
    }
    await api(`/api/databases/${databaseId}`)
    await api(`/api/databases/${databaseId}/status`)
  })
  await check('connection test succeeds', async () => {
    await api(`/api/databases/${databaseId}/test`, { method: 'POST' })
  })
  await check('activity and dlq respond', async () => {
    await api(`/api/activity?db=${databaseId}&limit=10`)
    const dlq = await api<unknown[]>('/api/dlq')
    if (dlq.length > 0) throw new Error(`DLQ is not empty: ${JSON.stringify(dlq[0])}`)
  })
  await check('table metadata routes match the source', async () => {
    const tables = await api<Array<{ name: string }>>(`/api/tables?db=${databaseId}`)
    if (!tables.some((table) => table.name === 'orders')) {
      throw new Error('orders missing from table list')
    }
    await api(`/api/tables/orders/schema?db=${databaseId}`)
    await api(`/api/tables/orders/data?db=${databaseId}`)
    const counted = await api<unknown>(`/api/tables/orders/count?db=${databaseId}`)
    const expected = await mysqlRows('SELECT COUNT(*) FROM orders')
    const mysqlCount = Number(expected[0][0])
    const text = JSON.stringify(counted)
    if (!text.includes(String(mysqlCount))) {
      throw new Error(`count response ${text} does not contain MySQL's ${mysqlCount}`)
    }
  })
  await check('api key disable blocks the wire, enable restores it', async () => {
    const keys = await api<Array<{ id: string; name: string }>>(
      `/api/databases/${databaseId}/api-keys`,
    )
    const key = keys.find((candidate) => candidate.name === 'e2e-gate')
    if (!key) throw new Error('e2e-gate key missing from the list')
    await api(`/api/databases/${databaseId}/api-keys/${key.id}`, {
      method: 'PATCH',
      body: { enabled: false },
    })
    try {
      pintailWire = undefined
      let rejected = false
      try {
        await pintailQuery('SELECT 1 FROM orders LIMIT 1')
      } catch {
        rejected = true
      }
      if (!rejected) throw new Error('disabled key still authenticates on the wire')
    } finally {
      await api(`/api/databases/${databaseId}/api-keys/${key.id}`, {
        method: 'PATCH',
        body: { enabled: true },
      })
      pintailWire = undefined
    }
    await pintailQuery('SELECT 1 FROM orders LIMIT 1')
  })
  await check('sse event stream connects', async () => {
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), 5_000)
    try {
      const response = await fetch(`${pintailUrl}/api/events`, {
        headers: { Authorization: `Bearer ${token}` },
        signal: controller.signal,
      })
      if (!response.ok) throw new Error(`/api/events returned ${response.status}`)
      const type = response.headers.get('content-type') ?? ''
      if (!type.includes('event-stream')) throw new Error(`unexpected content type ${type}`)
    } catch (error) {
      if (!String(error).includes('abort')) throw error
    } finally {
      clearTimeout(timer)
      controller.abort()
    }
  })
  await check('mode switches to polling and back with exact counts', async () => {
    await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'polling' } })
    try {
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (11, 'pending', 55.55, '2025-08-03')`,
      )
      const deadline = Date.now() + 120_000
      for (;;) {
        const expected = await mysqlRows('SELECT COUNT(*) FROM orders')
        const actual = await pintailQuery('SELECT COUNT(*) FROM orders')
        if (String(expected[0][0]) === String(actual[0][0])) break
        if (Date.now() > deadline) {
          const status = await api<unknown>(`/api/databases/${databaseId}/status`)
          const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=5`)
          throw new Error(
            `polling never converged: ${expected[0][0]} vs ${actual[0][0]}; ` +
              `status: ${JSON.stringify(status)}; activity: ${JSON.stringify(activity)}`,
          )
        }
        await Bun.sleep(POLL_MS)
      }
    } finally {
      // Restore CDC even when convergence fails: leaving the database in
      // polling mode would cascade the failure into every later check.
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'cdc' } })
    }
  })
  await check('wire column types: temporal expressions advertise what MySQL advertises', async () => {
    // The defect class GEOMETRY had in 0.0.3 and DATE() still had in 0.0.4:
    // the VALUE matches byte-for-byte while the advertised column type
    // differs, so drivers decode a different JS/host type - a Date object
    // from MySQL, a string from Pintail - and every downstream consumer
    // silently changes behaviour. Value comparisons can never catch it;
    // only the type bytes can.
    await pintailQuery('SELECT 1')
    const battery = `SELECT
        DATE(placed_on) AS date_fn,
        LAST_DAY(placed_on) AS last_day_fn,
        FROM_DAYS(738000) AS from_days_fn,
        MAKEDATE(2025, 60) AS makedate_fn,
        CURDATE() AS curdate_fn,
        NOW() AS now_fn,
        CURTIME() AS curtime_fn,
        FROM_UNIXTIME(1735689600) AS from_unixtime_fn,
        DATE_ADD(placed_on, INTERVAL 1 DAY) AS date_plus_day,
        DATE_ADD(placed_on, INTERVAL 1 HOUR) AS date_plus_hour,
        DATE_ADD(updated_at, INTERVAL 1 DAY) AS datetime_plus_day,
        DATE_FORMAT(placed_on, '%Y-%m') AS date_format_fn,
        STR_TO_DATE('2025-01-15', '%Y-%m-%d') AS str_to_date_date,
        STR_TO_DATE('2025-01-15 10:30', '%Y-%m-%d %H:%i') AS str_to_date_datetime,
        STR_TO_DATE('10:30', '%H:%i') AS str_to_date_time,
        SEC_TO_TIME(3661) AS sec_to_time_fn,
        MAKETIME(10, 30, 0) AS maketime_fn,
        CONVERT_TZ(updated_at, '+00:00', '+05:30') AS convert_tz_fn,
        JSON_UNQUOTE(JSON_EXTRACT('{"k":"v"}', '$.k')) AS json_unquote_fn,
        placed_on AS plain_date,
        updated_at AS plain_timestamp
      FROM orders LIMIT 1`
    const [, mysqlFields] = (await mysqlConnection!.query(battery)) as unknown as [
      unknown,
      Array<{ name: string; columnType: number; characterSet: number }>,
    ]
    const [, pintailFields] = (await pintailWire!.query(battery)) as unknown as [
      unknown,
      Array<{ name: string; columnType: number; characterSet: number }>,
    ]
    const mismatches: string[] = []
    for (const [index, expected] of mysqlFields.entries()) {
      const actual = pintailFields[index]
      // The charset byte is half the decode contract: LONG_BLOB + binary is
      // a Buffer, LONG_BLOB + utf8mb4_bin is text. Comparing only the type
      // byte waved through exactly that divergence on JSON_UNQUOTE.
      if (
        !actual
        || actual.name !== expected.name
        || actual.columnType !== expected.columnType
        || actual.characterSet !== expected.characterSet
      ) {
        mismatches.push(
          `${expected.name}: mysql type ${expected.columnType}/cs ${expected.characterSet}, ` +
            `pintail ${actual?.columnType}/cs ${actual?.characterSet} (${actual?.name})`,
        )
      }
    }
    if (mismatches.length) {
      throw new Error(`wire types diverge: ${mismatches.join('; ')}`)
    }
  })
  await check('erroring queries carry MySQL errno and SQLSTATE', async () => {
    // Clients branch on error codes, not messages: an ORM that maps 1146
    // to "run migrations" and 1054 to "schema drift" misbehaves if every
    // rejection arrives as a 1064 parse error. Each case must error on
    // BOTH engines with the same errno/SQLSTATE pair; message text is the
    // engine's own.
    const cases = [
      { label: 'unknown column', sql: 'SELECT nope_col FROM orders' },
      { label: 'unknown column via alias', sql: 'SELECT o.nope_col FROM orders o' },
      { label: 'unknown relation qualifier', sql: 'SELECT missing.id FROM orders' },
      { label: 'unknown table', sql: 'SELECT * FROM no_such_table' },
      { label: 'unknown database', sql: 'SELECT * FROM no_such_db.orders' },
      { label: 'parse error', sql: 'SELECTT 1' },
      {
        label: 'ambiguous column',
        sql: 'SELECT id FROM orders JOIN customers ON customers.id = orders.customer_id',
      },
      { label: 'unsigned out of range', sql: 'SELECT id - 99999999999999999 FROM orders LIMIT 1' },
      { label: 'group function in WHERE', sql: 'SELECT id FROM orders WHERE SUM(total) > 1' },
    ]
    // Families not yet classified (both engines reject, but Pintail answers
    // 1064 where MySQL is specific): wrong native-function arity (1582),
    // duplicate derived-table column names (1060), ungrouped columns under
    // ONLY_FULL_GROUP_BY (1055 - this gate's MySQL disables that mode, so
    // the wire unit test pins the mapping instead), and permission errors
    // (structurally different auth models). The matrix covers what parity
    // exists; it is not a claim of complete error-code coverage.
    const capture = async (
      client: { query: (sql: string) => Promise<unknown> },
      sql: string,
    ): Promise<{ errno: number; sqlState: string } | null> => {
      try {
        await client.query(sql)
        return null
      } catch (error) {
        const raised = error as { errno?: number; sqlState?: string }
        return { errno: raised.errno ?? 0, sqlState: raised.sqlState ?? '' }
      }
    }
    const divergences: string[] = []
    for (const testCase of cases) {
      const expected = await capture(mysqlConnection!, testCase.sql)
      const actual = await capture(pintailWire!, testCase.sql)
      if (!expected) {
        divergences.push(`${testCase.label}: MySQL did not error - bad matrix case`)
        continue
      }
      if (!actual) {
        divergences.push(`${testCase.label}: pintail answered where MySQL raised ${expected.errno}`)
        continue
      }
      if (actual.errno !== expected.errno || actual.sqlState !== expected.sqlState) {
        divergences.push(
          `${testCase.label}: mysql ${expected.errno}/${expected.sqlState}, ` +
            `pintail ${actual.errno}/${actual.sqlState}`,
        )
      }
    }
    if (divergences.length) {
      throw new Error(`error semantics diverge: ${divergences.join('; ')}`)
    }
  })
  await check('the audit trail records the network peer of every action', async () => {
    // Actions above (snapshot starts, mode changes, wire connections) have
    // all been audited by now. Each row must say where it came from: user
    // actions carry the HTTP peer, wire.connect rows carry the socket peer.
    const events = await api<Array<{ action: string; actor_type: string; client_ip: string | null }>>(
      '/api/workspaces/audit-log?limit=200',
    )
    if (events.length === 0) throw new Error('the audit trail is empty')
    const userRows = events.filter((event) => event.actor_type === 'user')
    const wireRows = events.filter((event) => event.action === 'wire.connect')
    if (userRows.length === 0) throw new Error('no user actions were audited')
    const unattributed = userRows.filter((event) => !event.client_ip)
    if (unattributed.length > 0) {
      throw new Error(`${unattributed.length} of ${userRows.length} user actions carry no client_ip`)
    }
    if (wireRows.length > 0 && wireRows.every((event) => !event.client_ip)) {
      throw new Error('wire.connect rows carry no client_ip')
    }
  })
  await check('resync and reconcile are accepted', async () => {
    // A supervisor cycle may hold the job lock at this instant; the 409 is
    // correct API behavior, so retry briefly instead of failing the check.
    const retryStart = Date.now()
    for (let attempt = 0; ; attempt += 1) {
      try {
        await api(`/api/databases/${databaseId}/tables/orders/resync`, { method: 'POST' })
        break
      } catch (error) {
        if (!String(error).includes('409') || Date.now() - retryStart > 40_000) throw error
        await Bun.sleep(POLL_MS)
      }
    }
    // The resync schedules a snapshot job; reconcile is correctly refused
    // (409) while that job runs, so wait for streaming state first.
    const deadline = Date.now() + 120_000
    for (;;) {
      const status = await api<{
        state: string
        tables?: Array<{ name: string; last_error?: string }>
      }>(`/api/databases/${databaseId}/snapshot/status`)
      if (status.state === 'streaming' || status.state === 'polling') break
      if (status.state === 'error' || Date.now() > deadline) {
        const errors = (status.tables ?? [])
          .filter((table) => table.last_error)
          .map((table) => `${table.name}: ${table.last_error}`)
          .join('; ')
        const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=5`)
        throw new Error(
          `resync did not settle: ${status.state}; ${errors}; recent activity: ${JSON.stringify(activity)}`,
        )
      }
      await Bun.sleep(POLL_MS)
    }
    const retries = Date.now() + 60_000
    for (;;) {
      try {
        await api(`/api/databases/${databaseId}/tables/customers/reconcile`, { method: 'POST' })
        break
      } catch (error) {
        if (!String(error).includes('409') || Date.now() > retries) throw error
        await Bun.sleep(3_000)
      }
    }
  })
  await check('resync recopies only the table it names', async () => {
    // The endpoint takes a table name and used to resnapshot the whole
    // database, which on a large source is hours of copying to repair one
    // table. It now snapshots just that table behind its own binlog fence, so
    // what this asserts is the SCOPE, not the outcome - a full-database
    // resnapshot also ends with correct data, just far more slowly.
    //
    // A database-wide snapshot puts every table into 'snapshotting' and
    // rewrites its rows; a per-table one leaves the others alone. So the
    // observable is the other tables' row counts holding steady across the
    // operation, and the accepted job naming the one table it will touch.
    const countOf = async (table: string) => {
      const rows = await pintailQuery(`SELECT COUNT(*) FROM ${table}`)
      return String(rows[0][0])
    }
    // `orders` is in here deliberately. An earlier version of this check
    // pinned only the OTHER tables and passed while orders was left empty -
    // the store was cleared for the recopy and the chunk journal still said
    // every chunk was done, so the copy no-opped and reported the previous
    // run's totals. A resnapshot must leave the table it recopies identical,
    // not merely leave the others alone.
    const before = {
      orders: await countOf('orders'),
      customers: await countOf('customers'),
      order_items: await countOf('order_items'),
    }
    let accepted: { run_id: string; state: string; table: string } | undefined
    const retryStart = Date.now()
    for (let attempt = 0; ; attempt += 1) {
      try {
        accepted = await api<{ run_id: string; state: string; table: string }>(
          `/api/databases/${databaseId}/tables/orders/resync`,
          { method: 'POST' },
        )
        break
      } catch (error) {
        // The 409 is correct: one replication job per database at a time.
        // This check follows others that leave a job running, and on a
        // loaded host the slot can stay held for well over a minute, so the
        // budget is generous rather than marginal - a tight one turns host
        // load into a product failure.
        if (!String(error).includes('409') || Date.now() - retryStart > 120_000) throw error
        await Bun.sleep(POLL_MS)
      }
    }
    if (accepted.table !== 'orders' || accepted.state !== 'snapshotting') {
      throw new Error(`expected a per-table snapshot job, got ${JSON.stringify(accepted)}`)
    }
    const deadline = Date.now() + 120_000
    for (;;) {
      const status = await api<{ state: string; tables?: Array<{ name: string; last_error?: string }> }>(
        `/api/databases/${databaseId}/snapshot/status`,
      )
      if (status.state === 'streaming' || status.state === 'polling') break
      if (status.state === 'error' || Date.now() > deadline) {
        throw new Error(`per-table resync did not settle: ${JSON.stringify(status)}`)
      }
      await Bun.sleep(POLL_MS)
    }
    // The recopied table reappears asynchronously, so give the counts a
    // window to settle rather than reading them the instant the job reports
    // done.
    const settled = Date.now() + 60_000
    for (;;) {
      const actual: Record<string, string> = {}
      for (const table of Object.keys(before)) actual[table] = await countOf(table)
      const changed = Object.entries(before).filter(
        ([table, expected]) => actual[table] !== expected,
      )
      if (changed.length === 0) break
      if (Date.now() > settled) {
        throw new Error(
          `resync of orders left tables changed: ` +
            changed
              .map(([table, expected]) => `${table} ${expected} before, ${actual[table]} after`)
              .join('; '),
        )
      }
      await Bun.sleep(POLL_MS)
    }
  })
  await check('schema drift during downtime: purged DDL recovers by re-probe', async () => {
    // The reported stuck flow. A migration drops a column while replication
    // is paused, and the binlog containing that ALTER is purged before the
    // stream returns - so no DDL event will ever teach the mirror the new
    // shape. Every copy path used to SELECT the remembered column list and
    // die on the source's own "Unknown column", and since retries re-read
    // the same stale schema, the failure repeated forever. Recovery must
    // re-probe: copy the table as the source IS, not as it was.
    await sql(`CREATE TABLE drift_messages (
      id BIGINT PRIMARY KEY AUTO_INCREMENT,
      body VARCHAR(200) NOT NULL,
      sentByAdminId BIGINT NULL
    )`)
    await sql(`INSERT INTO drift_messages (body, sentByAdminId) VALUES ('one', 7), ('two', NULL)`)
    try {
      // Adopt the new table into the mirror. The accepted snapshot runs
      // asynchronously and the database state reads 'streaming' until the
      // job flips it, so the wait is on the OUTCOME: the new table tracked,
      // copied, and streaming.
      await retry409(() =>
        api(`/api/databases/${databaseId}/snapshot`, { method: 'POST', body: { force: true } }))
      const adopted = Date.now() + 180_000
      for (;;) {
        const tables = await api<Array<{ name: string; state: string; rows: number }>>(
          `/api/tables?db=${databaseId}`,
        )
        const table = tables.find((entry) => entry.name === 'drift_messages')
        if (table?.state === 'streaming' && table.rows === 2) break
        if (Date.now() > adopted) {
          throw new Error(`drift_messages was never adopted: ${JSON.stringify(table)}`)
        }
        await Bun.sleep(POLL_MS)
      }

      // Downtime: pause, migrate, and lose the binlog history of it.
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'paused' } })
      await sql(`ALTER TABLE drift_messages DROP COLUMN sentByAdminId`)
      await sql(`INSERT INTO drift_messages (body) VALUES ('three')`)
      await sql(`FLUSH BINARY LOGS`)
      await sql(`FLUSH BINARY LOGS`)
      const [logs] = (await mysqlConnection!.query('SHOW BINARY LOGS')) as unknown as [
        Array<{ Log_name: string }>,
      ]
      await sql(`PURGE BINARY LOGS TO '${logs[logs.length - 1]!.Log_name}'`)
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'auto' } })

      // The per-table resync is the operator's repair tool; it must adopt
      // the source's current shape rather than replaying the stale one.
      await retry409(() =>
        api(`/api/databases/${databaseId}/tables/drift_messages/resync`, { method: 'POST' }))
      const deadline = Date.now() + 180_000
      for (;;) {
        const tables = await api<Array<{ name: string; state: string; rows: number; last_error: string | null }>>(
          `/api/tables?db=${databaseId}`,
        )
        const table = tables.find((entry) => entry.name === 'drift_messages')
        if (table?.last_error?.includes('Unknown column')) {
          throw new Error(`resync copied the stale schema: ${table.last_error}`)
        }
        if (table?.state === 'streaming' && table.rows === 3) break
        if (Date.now() > deadline) {
          throw new Error(`drift resync never settled: ${JSON.stringify(table)}`)
        }
        await Bun.sleep(POLL_MS)
      }
      const rows = await pintailQuery(`SELECT id, body FROM drift_messages ORDER BY id`)
      const expected = [['1', 'one'], ['2', 'two'], ['3', 'three']]
      if (JSON.stringify(rows.map((row) => row.map(String))) !== JSON.stringify(expected)) {
        throw new Error(`drift table diverged: ${JSON.stringify(rows)}`)
      }
      // The stream itself must be live again, not just the copy: the purged
      // checkpoint forced a stream-level recovery, and a silent restart loop
      // here would pass every assertion above while replicating nothing.
      await sql(`INSERT INTO drift_messages (body) VALUES ('four')`)
      const streamed = Date.now() + 120_000
      for (;;) {
        const streaming = await pintailQuery(`SELECT COUNT(*) FROM drift_messages`)
        if (String(streaming[0][0]) === '4') break
        if (Date.now() > streamed) {
          throw new Error('the stream never recovered after the purged checkpoint')
        }
        await Bun.sleep(POLL_MS)
      }
    } finally {
      await sql(`DROP TABLE IF EXISTS drift_messages`)
      // The drop replicates and the fixture table leaves the mirror; later
      // checks never see it.
    }
  })
  await check('reset starts the mirror over with the saved connection', async () => {
    // The escape hatch for state wedged beyond per-table repair: clear the
    // mirror, re-probe, recopy, continue in the configured mode - without
    // re-entering any connection details. What is asserted: the reset is
    // accepted, every table comes back byte-countable, and streaming
    // resumes.
    const countBefore = await mysqlCount('orders')
    await retry409(() => api(`/api/databases/${databaseId}/reset`, { method: 'POST' }))
    await waitForState(databaseId, 'streaming', 300_000)
    const settled = Date.now() + 120_000
    for (;;) {
      const rows = await pintailQuery(`SELECT COUNT(*) FROM orders`)
      if (String(rows[0][0]) === countBefore) break
      if (Date.now() > settled) {
        throw new Error(`orders never converged after reset: ${rows[0][0]} vs ${countBefore}`)
      }
      await Bun.sleep(POLL_MS)
    }
    // And the stream is live again: a new source row arrives without help.
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (1, 'shipped', 12.34, '2026-08-20')`,
    )
    const streamed = Date.now() + 60_000
    for (;;) {
      const rows = await pintailQuery(`SELECT COUNT(*) FROM orders`)
      if (Number(rows[0][0]) === Number(countBefore) + 1) break
      if (Date.now() > streamed) throw new Error('the reset mirror is not streaming new rows')
      await Bun.sleep(POLL_MS)
    }
  })
  await check('keyless policy: ambiguity quarantines and exact multiplicity repairs', async () => {
    // A table with no primary or unique key replicates inserts, but a CDC
    // UPDATE cannot be targeted and must flag the table needs_resync under
    // the default quarantine policy. Switching the database to auto_resync
    // lets the supervisor repair it with a forced snapshot.
    const detail = await api<{ name: string; mode: string; keyless_policy: string }>(
      `/api/databases/${databaseId}`,
    )
    if (detail.keyless_policy !== 'quarantine') {
      throw new Error(`expected default policy quarantine, got ${detail.keyless_policy}`)
    }
    await sql(
      'CREATE TABLE keyless_log (label VARCHAR(32) NOT NULL, amount INT NOT NULL)',
    )
    await sql(
      "INSERT INTO keyless_log (label, amount) VALUES ('a', 1), ('b', 2), ('b', 2), ('c', 3)",
    )
    const insertDeadline = Date.now() + 120_000
    for (;;) {
      try {
        const actual = await pintailQuery('SELECT COUNT(*) FROM keyless_log')
        if (String(actual[0][0]) === '4') break
      } catch {
        // table not yet replicated
      }
      if (Date.now() > insertDeadline) throw new Error('keyless inserts never replicated')
      await Bun.sleep(POLL_MS)
    }
    await sql("UPDATE keyless_log SET amount = amount + 10 WHERE label = 'b'")
    const flagDeadline = Date.now() + 120_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string; key_mode: string; mutation_guarantee: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (
        table?.state === 'needs_resync' &&
        table.key_mode === 'append_row_id' &&
        table.mutation_guarantee === 'quarantined'
      ) break
      if (Date.now() > flagDeadline) {
        throw new Error(
          `keyless UPDATE never flagged needs_resync: ${JSON.stringify(table)}`,
        )
      }
      await Bun.sleep(POLL_MS)
    }
    let rejected = false
    try {
      await api(`/api/databases/${databaseId}`, {
        method: 'PUT',
        body: { name: detail.name, mode: detail.mode, keyless_policy: 'bogus' },
      })
    } catch {
      rejected = true
    }
    if (!rejected) throw new Error('bogus keyless_policy was accepted')
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'auto_resync' },
    })
    const repairDeadline = Date.now() + 180_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table && table.state !== 'needs_resync') {
        const expected = await mysqlRows('SELECT COUNT(*), SUM(amount) FROM keyless_log')
        const actual = await pintailQuery('SELECT COUNT(*), SUM(amount) FROM keyless_log')
        if (
          String(expected[0][0]) === String(actual[0][0]) &&
          String(expected[0][1]) === String(actual[0][1])
        ) {
          break
        }
      }
      if (Date.now() > repairDeadline) {
        const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=8`)
        throw new Error(
          `auto_resync never repaired keyless_log: ${JSON.stringify(table)}; ` +
            `activity: ${JSON.stringify(activity)}`,
        )
      }
      await Bun.sleep(3_000)
    }
    // Delete one of two byte-identical rows. Pintail must quarantine rather
    // than choose an append identity, then restore exact source multiplicity.
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'quarantine' },
    })
    await sql("DELETE FROM keyless_log WHERE label = 'b' LIMIT 1")
    const deleteFlagDeadline = Date.now() + 120_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table?.state === 'needs_resync') break
      if (Date.now() > deleteFlagDeadline) {
        throw new Error(`ambiguous keyless DELETE was not quarantined: ${JSON.stringify(table)}`)
      }
      await Bun.sleep(POLL_MS)
    }
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'auto_resync' },
    })
    const multiplicityDeadline = Date.now() + 180_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table && table.state !== 'needs_resync') {
        const statement = 'SELECT label, amount, COUNT(*) FROM keyless_log GROUP BY label, amount ORDER BY label, amount'
        const expected = await mysqlRows(statement)
        const actual = await pintailQuery(statement)
        if (diffRows(expected, actual) === undefined) break
      }
      if (Date.now() > multiplicityDeadline) {
        throw new Error(`keyless duplicate multiplicity was not repaired: ${JSON.stringify(table)}`)
      }
      await Bun.sleep(3_000)
    }
    // Later phases assume the quarantine default.
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'quarantine' },
    })
  })
  await check('a connection string carrying client driver options registers', async () => {
    // The parameters an application's own DSN carries - node mysql2 spells
    // them multipleStatements and dateStrings - configure that driver, not
    // the connection, and mysql_async refuses a URL containing them. So an
    // operator could not paste the string already in their .env: Chitti LMS
    // reported building their pools "with the query parameters dropped",
    // which is that refusal seen from the outside.
    const host = await dockerHost()
    const mysqlPort = await publishedPort(mysqlName, 3306)
    const dsn =
      `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${DATABASE}` +
      '?multipleStatements=true&dateStrings=date'
    // `name` is both the label and the source schema, and it is unique per
    // workspace - so this cannot reuse the gate's own schema to assert a
    // table list without colliding with the primary registration. It asserts
    // the claim that matters instead: registration alone proves only that
    // the string parsed, while a probe that reaches the server proves the
    // client parameters were dropped rather than mangled into a DSN pointing
    // somewhere else.
    const created = await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: { name: 'e2e_client_dsn', dsn, mode: 'cdc' },
    })
    try {
      const probe = await api<{ server?: { version?: string } }>(
        `/api/databases/${created.id}/probe`,
      )
      if (!probe.server?.version) {
        throw new Error(`probe did not reach the server: ${JSON.stringify(probe)}`)
      }
    } finally {
      await api(`/api/databases/${created.id}`, { method: 'DELETE' })
    }
  })
  await check('throwaway database lifecycle: create, update, delete', async () => {
    const host = await dockerHost()
    const mysqlPort = await publishedPort(mysqlName, 3306)
    const created = await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: {
        name: 'e2e_dup',
        dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${DATABASE}`,
        mode: 'cdc',
      },
    })
    await api(`/api/databases/${created.id}/probe`)
    await api(`/api/databases/${created.id}`, {
      method: 'PUT',
      body: { name: 'e2e_dup_renamed', mode: 'cdc' },
    })
    await api(`/api/databases/${created.id}`, { method: 'DELETE' })
    const list = await api<Array<{ id: string }>>('/api/databases')
    if (list.some((database) => database.id === created.id)) {
      throw new Error('deleted database still listed')
    }
  })
}

// ---------------------------------------------------------------------------
// Destructive lifecycle: what a dropped table or a dropped database does to
// the replica, in both replication modes.
//
// These phases assert inside themselves rather than leaning on the convergence
// sweep, and each restores the fixture before it returns. A dropped table is
// retained as an orphan, so it stays in the replica's catalog after MySQL has
// forgotten it; leaving one behind would make every later phase's
// information_schema comparison fail for a reason that has nothing to do with
// that phase.
//
// A check records PASS for the behaviour Pintail should have and WARN for a
// divergence that docs/limitations.md records as a known gap, so a fix flips it
// to PASS rather than needing the assertion rewritten.

async function waitUntil(
  predicate: () => Promise<boolean>,
  timeoutMs: number,
  pollMs = POLL_MS,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    if (await predicate().catch(() => false)) return true
    if (Date.now() >= deadline) return false
    await Bun.sleep(pollMs)
  }
}

function record(phase: string, check: string, status: CheckResult['status'], detail?: string) {
  results.push({ phase, check, status, detail })
  if (status !== 'PASS') log(`${status} ${check}${detail ? ` — ${detail}` : ''}`)
}

/// Replica row count, or undefined when the replica has no such table.
async function replicaCount(table: string): Promise<number | undefined> {
  try {
    const rows = await pintailQuery(`SELECT COUNT(*) FROM \`${table}\``)
    return Number(rows[0][0])
  } catch {
    return undefined
  }
}

async function sourceCount(table: string): Promise<number> {
  const rows = await mysqlRows(`SELECT COUNT(*) FROM \`${table}\``)
  return Number(rows[0][0])
}

interface TableSummary {
  name: string
  state: string
  rows: number
  last_error?: string
}

async function tableSummary(table: string, database = databaseId): Promise<TableSummary | undefined> {
  const tables = await api<TableSummary[]>(`/api/tables?db=${database}`)
  return tables.find((candidate) => candidate.name.toLowerCase() === table.toLowerCase())
}

/// Re-probing is the only operator action that retires an orphan: the stored
/// probe report is the table inventory the query engine builds its catalog
/// from, and DROP TABLE does not refresh it.
async function reprobe(database = databaseId) {
  await api(`/api/databases/${database}/probe`)
}

/// Everything the control plane can say about why a table has not arrived.
async function replicationDiagnostics(database = databaseId): Promise<string> {
  const [detail, tables, activity] = await Promise.all([
    api<unknown>(`/api/databases/${database}`).catch((error) => String(error)),
    api<TableSummary[]>(`/api/tables?db=${database}`)
      .then((rows) => rows.map((row) => `${row.name}:${row.state}`))
      .catch((error) => String(error)),
    api<unknown[]>(`/api/activity?db=${database}&limit=6`).catch((error) => String(error)),
  ])
  return JSON.stringify({ database: detail, tables, activity })
}

/// Waits for a table created mid-stream to be adopted by the replica.
///
/// The quiet retry is diagnostic, not padding: the harness polls the wire
/// while it waits, so a table that only appears once the queries stop points
/// at the supervisor being starved rather than at the adoption itself.
async function waitForAdoption(
  table: string,
  rows: number,
): Promise<{ status: CheckResult['status']; detail?: string }> {
  const arrived = async () => (await replicaCount(table)) === rows
  // Adoption is a supervisor cadence away, then a single-table snapshot, and
  // both queue behind whatever else the cadence is doing. In a full gate run
  // the same phase competes with eight other tables and a loaded host, and a
  // window sized for an idle one turns that contention into a product
  // failure - measured: the phase passes in isolation and failed twice in
  // full runs while `targets` climbed 5 -> 9 and nine auto-include snapshots
  // completed, which is adoption working, just later than the wait allowed.
  if (await waitUntil(arrived, 300_000, 5_000)) return { status: 'PASS' }
  await Bun.sleep(45_000)
  if (await arrived()) {
    return {
      status: 'WARN',
      detail: `${table} was adopted only after 45s with no query on the wire`,
    }
  }
  return { status: 'FAIL', detail: `${table} was never adopted: ${await replicationDiagnostics()}` }
}

/// Writes a row into `orders` and waits for the replica to agree, which is the
/// question these phases actually care about: whether the rest of the database
/// still replicates after the destructive operation.
async function ordersStillReplicate(timeoutMs = 120_000): Promise<boolean> {
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
      `(12, 'pending', 3.21, '2025-08-14')`,
  )
  const expected = await sourceCount('orders')
  return waitUntil(async () => (await replicaCount('orders')) === expected, timeoutMs)
}

const RETENTION_GAP =
  'DROP TABLE retains the replica as an orphan and does not refresh the stored ' +
  'probe report, so the table stays in the replica catalog until an operator re-probes'

async function phaseSnapshotDdlWindow() {
  const phase = 'snapshot-ddl-window'
  const table = 'snapshot_window_table'
  try {
    // A forced snapshot reads the STORED probe and then hands the stream a
    // position captured under its own read lock. Anything created between
    // the last probe and that position is therefore invisible to the
    // snapshot AND already behind the resumed stream - the CREATE TABLE is
    // lost, the table is never adopted, and nothing errors: the stream keeps
    // reporting healthy with one fewer target forever.
    //
    // The control-plane phase hit this by accident roughly one run in three,
    // which is exactly why it survived three runs being written off as a
    // race. Here the window is opened deliberately: create the table and
    // force the snapshot immediately, before any cadence can read the DDL.
    await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO ${table} VALUES (1, 'inside-the-window')`)
    const retryStart = Date.now()
    for (let attempt = 0; ; attempt += 1) {
      try {
        await api(`/api/databases/${databaseId}/snapshot`, {
          method: 'POST',
          body: { force: true },
        })
        break
      } catch (error) {
        // The job slot is held by a supervisor cycle; that is correct server
        // behaviour, so retry rather than fail.
        if (!String(error).includes('409') || Date.now() - retryStart > 40_000) throw error
        await Bun.sleep(POLL_MS)
      }
    }
    const adopted = await waitForAdoption(table, 1)
    results.push({
      phase,
      check: 'a table created just before a forced snapshot is still adopted',
      status: adopted.status,
      detail: adopted.detail,
    })
  } finally {
    await sql(`DROP TABLE IF EXISTS ${table}`)
    await reprobe()
  }
}

async function phaseDropTableCdc() {
  const phase = 'drop-table-cdc'
  const table = 'lifecycle_cdc_drop'
  try {
    await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO ${table} VALUES (1, 'one'), (2, 'two'), (3, 'three')`)
    const seeded = await waitForAdoption(table, 3)
    record(phase, 'drop-table:replicates before the drop', seeded.status, seeded.detail)
    if (seeded.status === 'FAIL') return

    await sql(`DROP TABLE ${table}`)
    const orphaned = await waitUntil(
      async () => (await tableSummary(table))?.state === 'excluded',
      120_000,
    )
    record(
      phase,
      'drop-table:source drop marks the table orphaned',
      orphaned ? 'PASS' : 'FAIL',
      orphaned ? undefined : `state is ${JSON.stringify(await tableSummary(table))}`,
    )

    const survived = await ordersStillReplicate()
    record(
      phase,
      'drop-table:the rest of the database keeps replicating',
      survived ? 'PASS' : 'FAIL',
      survived
        ? undefined
        : `orders never caught up after ${table} was dropped: ${await replicationDiagnostics()}`,
    )

    // Retention is deliberate, but it also means the replica keeps answering
    // for a table the source no longer has - including through
    // information_schema, where MySQL and Pintail now disagree.
    const retained = await replicaCount(table)
    record(
      phase,
      'drop-table:orphan is retired without an operator re-probe',
      retained === undefined ? 'PASS' : 'WARN',
      retained === undefined ? undefined : `${RETENTION_GAP} (${retained} rows still served)`,
    )

    await reprobe()
    const retired = await waitUntil(async () => (await replicaCount(table)) === undefined, 60_000)
    record(
      phase,
      'drop-table:re-probe retires the orphan from the catalog',
      retired ? 'PASS' : 'FAIL',
      retired ? undefined : 'the orphan is still queryable after a fresh probe',
    )
  } finally {
    await sql(`DROP TABLE IF EXISTS ${table}`)
    await reprobe()
  }
}

async function phaseDropTableRecreate() {
  const phase = 'drop-table-recreate'
  const table = 'lifecycle_recreate'
  const create = `CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`
  try {
    await sql(create)
    await sql(`INSERT INTO ${table} VALUES (1, 'first-life'), (2, 'first-life')`)
    const seeded = await waitForAdoption(table, 2)
    record(phase, 'recreate:first generation replicates', seeded.status, seeded.detail)
    if (seeded.status === 'FAIL') return

    await sql(`DROP TABLE ${table}`)
    // Wait for the orphan to land before recreating: within one CDC run the
    // target stays blocked, so a create in the same cycle would be measuring
    // the block rather than what happens on the cycles after it.
    await waitUntil(async () => (await tableSummary(table))?.state === 'excluded', 120_000)

    await sql(create)
    await sql(`INSERT INTO ${table} VALUES (10, 'second-life'), (11, 'second-life')`)
    const expected = await sourceCount(table)
    const converged = await waitUntil(async () => {
      const diff = await tableDiff(table)
      return diff === undefined
    }, 120_000)
    const actual = await replicaCount(table)
    record(
      phase,
      'recreate:a table recreated under the same name replicates as a new table',
      converged ? 'PASS' : 'WARN',
      converged
        ? undefined
        : `the source has ${expected} rows and the replica ${actual ?? 'no table'}: the ` +
          'orphaned store is reused instead of being resnapshotted, because the CREATE ' +
          'handler skips any name it already tracks',
    )

    const survived = await ordersStillReplicate()
    record(
      phase,
      'recreate:the rest of the database keeps replicating',
      survived ? 'PASS' : 'FAIL',
      survived ? undefined : await replicationDiagnostics(),
    )
  } finally {
    await sql(`DROP TABLE IF EXISTS ${table}`)
    await reprobe()
  }
}

async function phaseDropTablePolling() {
  const phase = 'drop-table-polling'
  const dropped = 'lifecycle_poll_drop'
  const truncated = 'lifecycle_poll_truncate'
  try {
    // Polling only visits tables the stored probe already names, so both
    // fixtures have to be adopted by CDC before the mode switch.
    for (const table of [dropped, truncated]) {
      await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`)
      await sql(`INSERT INTO ${table} VALUES (1, 'a'), (2, 'b'), (3, 'c')`)
    }
    const first = await waitForAdoption(dropped, 3)
    const second = await waitForAdoption(truncated, 3)
    const seeded = first.status === 'FAIL' || second.status === 'FAIL' ? 'FAIL' : first.status
    record(
      phase,
      'polling:fixtures replicate before the mode switch',
      seeded,
      first.detail ?? second.detail,
    )
    if (seeded === 'FAIL') return

    await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'polling' } })
    try {
      const healthy = await ordersStillReplicate()
      record(
        phase,
        'polling:database is healthy before the drop',
        healthy ? 'PASS' : 'FAIL',
        healthy ? undefined : await replicationDiagnostics(),
      )

      // A truncated table leaves no chunk for the cheap probe to find changed,
      // so the rows only disappear on a reconciling cycle. That is bounded by
      // the database's reconcile interval, not by the poll interval.
      await sql(`TRUNCATE TABLE ${truncated}`)
      const emptied = await waitUntil(async () => (await replicaCount(truncated)) === 0, 180_000)
      record(
        phase,
        'polling:TRUNCATE empties the replica',
        emptied ? 'PASS' : 'WARN',
        emptied
          ? undefined
          : `the replica still holds ${await replicaCount(truncated)} rows: polling sees a ` +
            'truncate only through reconciliation, so the replica reads high until the ' +
            "database's reconcile interval elapses",
      )

      await sql(`DROP TABLE ${dropped}`)
      const unaffected = await ordersStillReplicate(90_000)
      const status = await api<unknown>(`/api/databases/${databaseId}/status`)
      record(
        phase,
        'polling:one dropped table does not stop the other tables',
        unaffected ? 'PASS' : 'WARN',
        unaffected
          ? undefined
          : 'the whole poll cycle aborts on the first table that fails, so every other ' +
            `table stops replicating too: ${JSON.stringify(status)}`,
      )

      await reprobe()
      const recovered = await ordersStillReplicate()
      record(
        phase,
        'polling:re-probe restores replication for the surviving tables',
        recovered ? 'PASS' : 'FAIL',
        recovered ? undefined : `still stalled: ${JSON.stringify(status)}`,
      )
    } finally {
      // Leaving the database in polling mode would cascade into every later
      // phase, exactly as the control-plane mode check guards against.
      const runs = () =>
        api<Array<{ id: string; kind: string; status: string }>>(
          `/api/activity?db=${databaseId}&limit=50`,
        )
      const seen = new Set((await runs()).map((run) => run.id))
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'cdc' } })
      // Returning to cdc after polling rebuilds the handoff with a forced
      // snapshot on the next supervisor cadence, and the tables read as
      // empty while they are being recopied. That is the documented shape
      // of a forced resync, not a defect - but the phase must not end
      // before the rebuild has come and gone, or the corpus sweep (and any
      // later phase) lands inside the empty window. The mode switch itself
      // reports 'streaming' with the polling-era rows still present, so
      // waiting on state alone returns before the rebuild even starts;
      // only a snapshot run that did not exist before the switch proves it
      // ran.
      // The rebuild only exists when polling actually checkpointed: a fast
      // cadence can leave the phase's failing-poll window with no polling
      // checkpoint at all, and CDC then resumes directly - correct, and no
      // snapshot ever appears. Accept either shape: a completed rebuild
      // snapshot, or thirty seconds of streaming-with-rows during which no
      // new snapshot run has appeared (a pending rebuild fires within one
      // supervisor cadence, so a quiet half minute proves none is coming).
      const rebuilt = Date.now() + 240_000
      let quietSince = Date.now()
      let lastTrace = 0
      const known = new Set(seen)
      for (;;) {
        const current = await runs()
        // Completion is judged against the ORIGINAL pre-switch set: a run
        // first sampled as 'running' must still count when it completes.
        const done = current.some(
          (run) => !seen.has(run.id) && run.kind === 'snapshot' && run.status === 'completed',
        )
        if (done && ((await replicaCount('customers')) ?? 0) > 0) {
          break
        }
        // The quiet clock resets on any run id never sampled before.
        const fresh = current.filter((run) => !known.has(run.id) && run.kind === 'snapshot')
        if (fresh.length > 0) {
          for (const run of fresh) known.add(run.id)
          quietSince = Date.now()
        }
        const rows = (await replicaCount('customers')) ?? 0
        const status = await api<{ state: string }>(`/api/databases/${databaseId}`)
        if (
          status.state === 'streaming'
          && rows > 0
          && Date.now() - quietSince > 30_000
        ) {
          break
        }
        if (Date.now() - lastTrace > 5_000) {
          lastTrace = Date.now()
          const active = current
            .filter((run) => run.status === 'running' || run.status === 'pending')
            .map((run) => `${run.kind}:${run.status}:${run.id.slice(0, 12)}`)
            .join(' ')
          log(
            `handoff wait: state=${status.state} rows=${rows} fresh=${fresh.length} ` +
              `known=${known.size} quiet=${((Date.now() - quietSince) / 1000).toFixed(0)}s` +
              (active ? ` active=[${active}]` : ' active=[]'),
          )
        }
        if (Date.now() > rebuilt) {
          throw new Error('the CDC handoff rebuild never ran after the polling switch')
        }
        await Bun.sleep(POLL_MS)
      }
    }
  } finally {
    await sql(`DROP TABLE IF EXISTS ${dropped}`)
    await sql(`DROP TABLE IF EXISTS ${truncated}`)
    await reprobe()
  }
}

async function phaseDropDatabase() {
  const phase = 'drop-database'
  const source = 'e2e_lifecycle_db'
  const shadow = 'lifecycle_shadow'
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  let secondary = ''
  try {
    await sql(`DROP DATABASE IF EXISTS ${source}`)
    await sql(`CREATE DATABASE ${source} DEFAULT CHARACTER SET utf8mb4`)
    // A qualified CREATE/DROP is attributed in the binlog to the session's
    // current schema, not to the schema in the statement, and Pintail routes
    // DDL by that attribution. Building the second database from the main
    // session would therefore hand its DDL to the main database's stream.
    await sql(`USE ${source}`)
    await sql(`CREATE TABLE widgets (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`CREATE TABLE ${shadow} (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO widgets VALUES (1, 'a'), (2, 'b'), (3, 'c')`)
    await sql(`INSERT INTO ${shadow} VALUES (1, 'other-schema')`)
    await sql(`USE ${DATABASE}`)

    // Same table name in the replicated database, so a DDL statement aimed at
    // the other schema has somewhere wrong to land.
    await sql(`CREATE TABLE ${shadow} (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO ${shadow} VALUES (1, 'main-schema')`)
    const shadowed = await waitForAdoption(shadow, 1)
    record(phase, 'cross-schema:same-named table replicates first', shadowed.status, shadowed.detail)

    if (shadowed.status !== 'FAIL') {
      await sql(`DROP TABLE ${source}.${shadow}`)
      await sql(`INSERT INTO ${shadow} VALUES (2, 'written after the other schema was dropped')`)
      const isolated = await waitUntil(async () => (await replicaCount(shadow)) === 2, 90_000)
      record(
        phase,
        'cross-schema:dropping another schema\'s table leaves this one replicating',
        isolated ? 'PASS' : 'WARN',
        isolated
          ? undefined
          : 'DDL is matched on the bare table name and routed by the session schema, so ' +
            `DROP TABLE ${source}.${shadow} orphaned ${DATABASE}.${shadow}: ` +
            JSON.stringify(await tableSummary(shadow)),
      )
    }

    const created = await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: {
        name: source,
        dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${source}`,
        mode: 'cdc',
      },
    })
    secondary = created.id
    await reprobe(secondary)
    await api(`/api/databases/${secondary}/snapshot`, { method: 'POST', body: { force: false } })
    const snapshotted = await waitUntil(async () => {
      const status = await api<{ state: string }>(`/api/databases/${secondary}/snapshot/status`)
      return status.state === 'streaming' || status.state === 'polling'
    }, 180_000)
    record(phase, 'drop-database:second database snapshots', snapshotted ? 'PASS' : 'FAIL')
    if (!snapshotted) return

    const before = await api<{ count: number }>(`/api/tables/widgets/count?db=${secondary}`)
    record(
      phase,
      'drop-database:second database serves its rows',
      before.count === 3 ? 'PASS' : 'FAIL',
      before.count === 3 ? undefined : `expected 3 rows, got ${before.count}`,
    )

    await sql(`DROP DATABASE ${source}`)
    // Several supervisor cadences: whatever Pintail is going to notice, it has
    // noticed by now.
    const flagged = await waitUntil(async () => {
      const detail = await api<{ state: string }>(`/api/databases/${secondary}`)
      if (detail.state === 'error') return true
      const tables = await api<TableSummary[]>(`/api/tables?db=${secondary}`)
      return tables.every((table) => table.state === 'excluded')
    }, 90_000)
    const observed = {
      database: await api<{ state: string }>(`/api/databases/${secondary}`),
      tables: await api<TableSummary[]>(`/api/tables?db=${secondary}`),
    }
    record(
      phase,
      'drop-database:the deleted source is surfaced, not served silently',
      flagged ? 'PASS' : 'WARN',
      flagged
        ? undefined
        : 'DROP DATABASE is not classified as DDL at all, so no table is orphaned and ' +
          `nothing records the loss: ${JSON.stringify(observed)}`,
    )

    let probeRefused = false
    try {
      await reprobe(secondary)
    } catch {
      probeRefused = true
    }
    record(
      phase,
      'drop-database:re-probing a deleted source fails loudly',
      probeRefused ? 'PASS' : 'FAIL',
      probeRefused ? undefined : 'the probe succeeded against a database that no longer exists',
    )

    await api(`/api/databases/${secondary}/mode`, { method: 'POST', body: { mode: 'polling' } })
    const pollingNoticed = await waitUntil(async () => {
      const detail = await api<{ state: string }>(`/api/databases/${secondary}`)
      return detail.state === 'error'
    }, 90_000)
    record(
      phase,
      'drop-database:polling reports the deleted source as an error',
      pollingNoticed ? 'PASS' : 'WARN',
      pollingNoticed
        ? undefined
        : `polling kept reporting a healthy database: ${JSON.stringify(
            await api<unknown>(`/api/databases/${secondary}`),
          )}`,
    )

    const stillServed = await api<{ count: number }>(`/api/tables/widgets/count?db=${secondary}`)
      .then((response) => response.count)
      .catch(() => undefined)
    record(
      phase,
      'drop-database:reads against a deleted source do not claim to be current',
      stillServed === undefined ? 'PASS' : 'WARN',
      stillServed === undefined
        ? undefined
        : `${stillServed} rows are still served from the replica of a database MySQL no ` +
          'longer has, with nothing on the read path marking them stale',
    )
  } finally {
    await sql(`USE ${DATABASE}`)
    if (secondary) {
      try {
        await api(`/api/databases/${secondary}`, { method: 'DELETE' })
      } catch (error) {
        log(`cleanup: could not delete the throwaway database: ${error}`)
      }
    }
    await sql(`DROP DATABASE IF EXISTS ${source}`)
    await sql(`DROP TABLE IF EXISTS ${shadow}`)
    await reprobe()
  }
}

// ---------------------------------------------------------------------------
// Boot.

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_E2E_BINARY) return resolve(process.env.PINTAIL_E2E_BINARY)
  log('building the release pintail binary')
  await command(['cargo', 'build', '--release', '-p', 'pintail'])
  const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'], {
    quiet: true,
  })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function startPintail(queryMemoryLimit?: number, extraEnv: Record<string, string> = {}) {
  pintailWire = undefined
  pintailProcess = Bun.spawn(
    [
      pintailBinary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${pintailHttpPort}`,
      '--wire-bind',
      `127.0.0.1:${pintailWirePort}`,
    ],
    {
      cwd: repository,
      stdout: 'inherit',
      stderr: 'inherit',
      env: {
        ...process.env,
        // Test cadence: every adoption/auto-include/re-probe wait in the
        // lifecycle phases is a multiple of the supervisor interval.
        PINTAIL_SUPERVISOR_INTERVAL_MS: SUPERVISOR_MS,
        ...(queryMemoryLimit === undefined
          ? {}
          : { PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(queryMemoryLimit) }),
        ...extraEnv,
      },
    },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const response = await fetch(`${pintailUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy within 120 seconds')
}

async function main() {
  const host = await dockerHost()
  let reused = false
  if (KEEP_MYSQL) {
    const state = await docker(
      'inspect',
      '--format',
      '{{.State.Running}} {{.Config.Image}}',
      mysqlName,
    )
      .then((result) => result.stdout.trim())
      .catch(() => 'absent')
    if (state === `true ${MYSQL_IMAGE}`) {
      log(`reusing MySQL source ${mysqlName}`)
      reused = true
    } else if (state !== 'absent') {
      // Stopped, or running the wrong image for this leg - recreate.
      await docker('rm', '-f', mysqlName)
    }
  }
  if (!reused) log(`starting MySQL source ${mysqlName}`)
  if (!reused) await docker(
    'run',
    '--detach',
    '--name',
    mysqlName,
    '--publish',
    '0:3306',
    '--tmpfs',
    '/var/lib/mysql:rw,size=2g',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env',
    `MYSQL_DATABASE=${DATABASE}`,
    MYSQL_IMAGE,
    '--server-id=942',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    `--binlog-row-metadata=${BINLOG_METADATA}`,
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  {
    const [versionRows] = (await mysqlConnection.query('SELECT VERSION() AS v')) as unknown as [
      Array<{ v: string }>,
    ]
    mysqlServerVersion = versionRows[0]?.v ?? ''
  }
  mysqlEndpoint = {
    host,
    port: mysqlPort,
    user: 'pintail',
    password: 'pintail',
    database: DATABASE,
  }
  if (reused) {
    // A fresh logical source on the standing server: same guarantees a new
    // container gives, minus its boot and timezone load.
    await sql(`DROP DATABASE IF EXISTS ${DATABASE}`)
    await sql(`CREATE DATABASE ${DATABASE}`)
    // 8.4 renamed RESET MASTER; each major only accepts its own spelling.
    const [versionRow] = (await mysqlConnection!.query('SELECT VERSION() AS v')) as unknown as [
      Array<{ v: string }>,
    ]
    const resetStatement = versionRow[0]!.v.startsWith('8.0')
      ? 'RESET MASTER'
      : 'RESET BINARY LOGS AND GTIDS'
    await sql(resetStatement)
  }
  await sql(`USE ${DATABASE}`)
  // The variable is dynamic, so a reused keep-container created under the
  // other setting still runs this gate's configured base.
  await sql(`SET GLOBAL binlog_row_metadata = '${BINLOG_METADATA}'`)
  await sql(`CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )

  pintailBinary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-e2e-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  await startPintail()
  const setup = await api<{ token: string }>('/api/auth/setup', {
    method: 'POST',
    auth: false,
    body: { email: 'e2e@pintail.local', password: 'e2e-gate-password' },
  })
  token = setup.token

  log('seeding the source schema and initial rows')
  await phaseSeed()

  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: { name: DATABASE, dsn: `mysql://pintail:pintail@${dsnHost(host)}:${mysqlPort}/${DATABASE}`, mode: 'cdc' },
  })
  databaseId = database.id
  const apiKey = await api<{ secret: string }>(`/api/databases/${databaseId}/api-keys`, {
    method: 'POST',
    body: { name: 'e2e-gate', scopes: ['query', 'read'] },
  })
  wireSecret = apiKey.secret
  await api(`/api/databases/${databaseId}/probe`)
  const accepted = await api<{ run_id: string }>(`/api/databases/${databaseId}/snapshot`, {
    method: 'POST',
    body: { force: false },
  })
  log(`snapshot ${accepted.run_id} started`)
  for (let attempt = 0; ; attempt += 1) {
    const status = await api<{ state: string; tables: Array<{ name: string; last_error?: string }> }>(
      `/api/databases/${databaseId}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`,
      )
    }
    if (status.state === 'polling' || status.state === 'streaming') break
    if (attempt > 600) throw new Error('snapshot did not complete within ten minutes')
    await Bun.sleep(1000)
  }

  const phases: Array<[string, () => Promise<void>]> = [
    ['snapshot', async () => {}],
    ['orm-compat', phaseOrmCompatibility],
    ['crud', phaseCrud],
    ['type-edges', phaseTypeEdges],
    ['ddl', phaseDdl],
    ['schema-drift-minimal', phaseSchemaDriftMinimal],
    ['schema-drift-unseen', phaseSchemaDriftUnseen],
    ['churn', phaseChurn],
    ['contention', phaseContention],
    ['execution-budget', phaseExecutionBudget],
    ['spill', phaseSpill],
    ['pooling', phasePooling],
    ['local-database', phaseLocalDatabase],
    ['restart', phaseRestart],
    ['activity-history', phaseActivityHistory],
    ['poll-storm', phasePollStorm],
    ['control-plane', phaseControlPlane],
    ['snapshot-ddl-window', phaseSnapshotDdlWindow],
    ['drop-table-cdc', phaseDropTableCdc],
    ['drop-table-recreate', phaseDropTableRecreate],
    ['drop-table-polling', phaseDropTablePolling],
    ['restart-during-snapshot', phaseRestartDuringSnapshot],
    ['memory-pressure', phaseMemoryPressure],
    ['drop-database', phaseDropDatabase],
    // Last: the rename gap leaves the replica holding a table under a name
    // MySQL no longer uses, and the lifecycle phases re-probe, which would
    // retire that table from the catalog and turn the documented metadata
    // divergence into an undocumented one.
    ['ddl-documented-gaps', phaseDdlDocumentedGaps],
  ]
  const selected = process.env.E2E_PHASES?.split(',').map((phase) => phase.trim())
  for (const [name, run] of phases) {
    if (selected && name !== 'snapshot' && !selected.includes(name)) continue
    log(`phase: ${name}`)
    const phaseStart = Date.now()
    await run()
    const ranAt = Date.now()
    await verifyConvergence(name)
    const convergedAt = Date.now()
    await verifyCorpus(name)
    const done = Date.now()
    phaseTimings.push({
      phase: name,
      runSeconds: (ranAt - phaseStart) / 1000,
      convergeSeconds: (convergedAt - ranAt) / 1000,
      corpusSeconds: (done - convergedAt) / 1000,
    })
    log(
      `phase ${name}: run ${((ranAt - phaseStart) / 1000).toFixed(1)}s, ` +
        `converge ${((convergedAt - ranAt) / 1000).toFixed(1)}s, ` +
        `corpus ${((done - convergedAt) / 1000).toFixed(1)}s`,
    )
  }

  publish()
}

function publish() {
  const failed = results.filter((result) => result.status === 'FAIL')
  const warned = results.filter((result) => result.status === 'WARN')
  const passed = results.filter((result) => result.status === 'PASS')
  const skipped = results.filter((result) => result.status === 'SKIP')
  const corpusChecks = results.filter((result) => result.check.startsWith('query:'))
  const lines = [
    '# Pintail end-to-end differential gate',
    '',
    `Measured ${new Date().toISOString()}.`,
    '',
    // The environment is evidence: a ledger that does not say which
    // source version and metadata mode it ran against proves nothing
    // about either.
    `Source: \`${MYSQL_IMAGE}\` (server ${mysqlServerVersion || 'unknown'}), ` +
      `\`binlog_row_metadata=${BINLOG_METADATA}\`, ` +
      `${KEEP_MYSQL ? 'reused keep-container' : 'fresh container'}.`,
    '',
    `**${passed.length} passed, ${failed.length} failed, ${warned.length} documented-gap warnings, ${skipped.length} skipped.**`,
    '',
    // Honest denominators: the corpus replays after every settled phase,
    // so the headline counts checks, not independent behaviors.
    `${differentialQueries.length} unique corpus queries produced ` +
      `${corpusChecks.length} corpus checks across phases; the remaining ` +
      `checks are convergence, battery, and control-plane assertions.`,
    '',
    '| Phase | Check | Status | Detail |',
    '|---|---|---|---|',
    ...results.map(
      (result) =>
        `| ${result.phase} | ${result.check} | ${result.status} | ${(result.detail ?? '').split('\n')[0].replaceAll('|', '\\|')} |`,
    ),
    '',
    '## Timing',
    '',
    '| Phase | run s | converge s | corpus s |',
    '|---|---|---|---|',
    ...phaseTimings.map(
      (timing) =>
        `| ${timing.phase} | ${timing.runSeconds.toFixed(1)} | ${timing.convergeSeconds.toFixed(1)} | ${timing.corpusSeconds.toFixed(1)} |`,
    ),
    `| total | ${phaseTimings.reduce((sum, timing) => sum + timing.runSeconds, 0).toFixed(1)} | ${phaseTimings.reduce((sum, timing) => sum + timing.convergeSeconds, 0).toFixed(1)} | ${phaseTimings.reduce((sum, timing) => sum + timing.corpusSeconds, 0).toFixed(1)} |`,
    '',
  ]
  // Phase-subset runs write a separate artifact so an iteration loop never
  // overwrites the committed full-gate record.
  const partial = Boolean(process.env.E2E_PHASES)
  // A named leg (the 8.0 matrix run) banks its own ledger next to the
  // primary one instead of overwriting it.
  const leg = process.env.PINTAIL_E2E_RESULTS_SUFFIX ?? ''
  const suffix = partial ? '-partial' : leg
  writeFileSync(join(import.meta.dir, `results${suffix}.md`), lines.join('\n'))
  writeFileSync(join(import.meta.dir, `results${suffix}.json`), JSON.stringify(results, null, 2))
  log(`gate: ${failed.length === 0 ? 'PASS' : 'FAIL'} (${passed.length} passed, ${failed.length} failed, ${warned.length} warned)`)
  for (const failure of failed) {
    log(`  FAIL ${failure.phase}/${failure.check}: ${failure.detail?.split('\n')[0]}`)
  }
  if (failed.length > 0) {
    process.exitCode = 1
  }
}

async function cleanup() {
  try {
    pintailProcess?.kill()
  } catch {}
  try {
    await mysqlConnection?.end()
  } catch {}
  try {
    await mysqlPool?.end()
  } catch {}
  if (mysqlStarted && !KEEP_MYSQL) {
    try {
      await docker('rm', '--force', '--volumes', mysqlName)
    } catch (error) {
      log(`cleanup: ${error}`)
    }
  }
  if (pintailDataDir) {
    try {
      rmSync(pintailDataDir, { recursive: true, force: true })
    } catch {}
  }
}

try {
  await main()
} finally {
  await cleanup()
}

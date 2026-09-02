/// Concurrency load harness: what the engine does when queries arrive together.
///
/// Every other gate measures one query at a time. The benchmark reports
/// single-query latency, the oracle and E2E gates assert correctness on a
/// quiet server. None of them answer the question this harness exists for:
/// what happens when N clients query at once, on a machine that cannot
/// hold N copies of anything.
///
/// Three ceilings are in play. The per-query ceiling (`MemoryTracker`,
/// `PINTAIL_QUERY_MEMORY_LIMIT_BYTES`) bounds one query. The process budget
/// (`PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES`) bounds their sum, and since
/// the replica cache charges its resident memtables to the same budget, it
/// bounds what the server holds between queries too. Admission
/// (`PINTAIL_MAX_CONCURRENT_QUERIES`) bounds how many run at once. This
/// harness sets all three and measures where they land in practice: peak
/// RSS is the load-bearing number, because it is what distinguishes "the
/// ceilings held" from "each ceiling held and the process still grew".
///
/// Two profiles. The default sweeps concurrency with the production
/// defaults - the historical table, comparable across runs. `constrained`
/// is the scenario a small container faces: a small budget, a narrow
/// admission window, every client reconnecting per query, and the
/// dashboard, HTTP queries and a CDC writer all arriving at the same time
/// as the wire clients. It fails the run if the process outgrows its
/// ceiling or the replica stops keeping up.
///
/// Run with: bun run run.ts
///           LOAD_PROFILE=constrained bun run run.ts
///           LOAD_LEVELS=1,8,32 LOAD_MEMORY_MB=64 bun run run.ts
///
/// Every setting is an environment variable; the profile only supplies
/// defaults, and an explicit variable wins.

import { createServer } from 'node:net'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-load-mysql-${process.pid}-${nonce}`
const DATABASE = 'load_db'

const PROFILE = process.env.LOAD_PROFILE ?? 'default'
const PRESETS: Record<string, Record<string, string>> = {
  default: {},
  constrained: {
    LOAD_LEVELS: '16,64,128',
    LOAD_MEMORY_MB: '64',
    LOAD_TOTAL_MEMORY_MB: '512',
    LOAD_MAX_CONCURRENT: '16',
    LOAD_SIDELOADS: 'cdc,dashboard,http',
    LOAD_CONNECTION_STORM: '1',
    LOAD_RSS_CEILING_MB: '1024',
  },
}
if (!(PROFILE in PRESETS)) {
  throw new Error(`unknown LOAD_PROFILE ${PROFILE}; known: ${Object.keys(PRESETS).join(', ')}`)
}
function setting(name: string, fallback: string): string {
  return process.env[name] ?? PRESETS[PROFILE]![name] ?? fallback
}

/// Concurrency levels to sweep. Rising powers of two make the shape of the
/// degradation curve readable rather than a single pass/fail point.
const LEVELS = setting('LOAD_LEVELS', '1,4,16,64')
  .split(',')
  .map((level) => Number(level.trim()))
  .filter((level) => Number.isFinite(level) && level > 0)

/// Per-query ceiling for the server under test. Deliberately small: the
/// point is to reach the ceiling, not to avoid it.
const MEMORY_MB = Number(setting('LOAD_MEMORY_MB', '64'))

/// Process-wide budget across every query and the replica cache. Zero
/// leaves the server's default (three quarters of what the container has).
const TOTAL_MEMORY_MB = Number(setting('LOAD_TOTAL_MEMORY_MB', '0'))

/// Admission window. Zero leaves the server's default (cores x 4, at least 16).
const MAX_CONCURRENT = Number(setting('LOAD_MAX_CONCURRENT', '0'))

/// Queries per client per level.
const ITERATIONS = Number(setting('LOAD_ITERATIONS', '10'))

/// Rows seeded into the source. Large enough that a sort or aggregate has
/// to do real work, small enough that snapshotting stays under a minute.
const SEED_ROWS = Number(setting('LOAD_SEED_ROWS', '200000'))

/// What runs alongside the wire clients at every level: `cdc` keeps the
/// source changing so every query sees a moved stamp, `dashboard` polls the
/// control-plane endpoints an open tab hits, `http` sends queries through
/// the HTTP surface, which builds an engine per request.
const SIDELOADS = new Set(
  setting('LOAD_SIDELOADS', '')
    .split(',')
    .map((name) => name.trim())
    .filter(Boolean),
)

/// A connection per query instead of per client. What a pooled application
/// tier looks like from the server, and the shape that made a
/// per-connection replica cache expensive.
const CONNECTION_STORM = setting('LOAD_CONNECTION_STORM', '0') === '1'

/// Peak RSS a level may reach before the run fails. Zero disables the check.
const RSS_CEILING_MB = Number(setting('LOAD_RSS_CEILING_MB', '0'))

let mysqlConnection: mysql.Connection | undefined
let mysqlStarted = false
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailBinary = ''
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''
let databaseId = ''
let wireSecret = ''
let nextSourceId = SEED_ROWS

interface SideResult {
  completed: number
  failed: number
  p99Ms: number
  errors: Record<string, number>
}

interface LevelResult {
  concurrency: number
  completed: number
  failed: number
  p50Ms: number
  p95Ms: number
  p99Ms: number
  maxMs: number
  peakRssMb: number
  errors: Record<string, number>
  http?: SideResult
  dashboard?: SideResult
  /// Rows the CDC writer committed during the level, and how long after the
  /// level ended the replica had all of them (-1: never, within the wait).
  cdcRows?: number
  cdcConvergeMs?: number
}

const results: LevelResult[] = []

function log(message: string) {
  console.log(`[load] ${message}`)
}

async function command(args: string[], options: { cwd?: string; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
    stdout: options.quiet ? 'pipe' : 'inherit',
    stderr: options.quiet ? 'pipe' : 'inherit',
  })
  const stdout = options.quiet ? await new Response(child.stdout).text() : ''
  const stderr = options.quiet ? await new Response(child.stderr).text() : ''
  const status = await child.exited
  if (status !== 0) throw new Error(`${args.join(' ')} failed (${status}): ${stderr}`)
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

/// Resolve the host that published container ports are reachable on. The
/// Docker daemon here is remote over SSH, so "localhost" is wrong.
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

async function waitForMysql(host: string, port: number) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
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
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (options.auth !== false && token) headers.authorization = `Bearer ${token}`
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) {
    throw new Error(`${options.method ?? 'GET'} ${path} → ${response.status}: ${text}`)
  }
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

async function sql(statement: string) {
  await mysqlConnection!.query(statement)
}

/// Peak RSS of the server process, in MB. This is the number that decides
/// whether a per-query ceiling actually bounds the process.
async function serverRssMb(): Promise<number> {
  if (!pintailProcess?.pid) return 0
  try {
    const { stdout } = await command(['ps', '-o', 'rss=', '-p', String(pintailProcess.pid)], {
      quiet: true,
    })
    return Number(stdout.trim()) / 1024
  } catch {
    return 0
  }
}

function percentile(sorted: number[], fraction: number): number {
  if (sorted.length === 0) return 0
  const index = Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))
  return sorted[index]
}

/// Bucket an error by kind rather than by message, so a hundred distinct
/// row counts in one message do not read as a hundred distinct failures.
function errorKind(message: string): string {
  // Admission refusal is the designed response to overload, not a failure.
  // It must be distinguishable from a real error or the load evidence
  // cannot tell load shedding apart from the server falling over.
  if (/concurrent queries/i.test(message)) return 'admission-refused'
  if (/memory limit exceeded|memory budget/i.test(message)) return 'query-memory-limit'
  if (/too many connections|connection limit/i.test(message)) return 'connection-limit'
  if (/ECONNRESET|socket hang up|closed/i.test(message)) return 'connection-dropped'
  if (/ETIMEDOUT|timeout/i.test(message)) return 'timeout'
  if (/ECONNREFUSED/i.test(message)) return 'connection-refused'
  return 'other'
}

function tally(errors: Record<string, number>, message: string) {
  const kind = errorKind(message)
  errors[kind] = (errors[kind] ?? 0) + 1
}

function describeErrors(errors: Record<string, number>): string {
  return Object.entries(errors)
    .map(([kind, count]) => `${kind}×${count}`)
    .join(', ')
}

/// The query mix. Each shape allocates differently, so the mix exercises
/// more than one path to the ceiling: a sort that spills, an aggregate that
/// spills, and a GROUP_CONCAT that is documented NOT to spill and so fails
/// at the ceiling instead.
const QUERIES = [
  'SELECT region, COUNT(*) c, SUM(amount) s FROM events GROUP BY region ORDER BY s DESC',
  'SELECT * FROM events ORDER BY amount DESC, id ASC LIMIT 1000',
  'SELECT region, GROUP_CONCAT(note) FROM events GROUP BY region',
]

function wireConnection() {
  return mysql.createConnection({
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: wireSecret,
    database: DATABASE,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
  })
}

async function replicaEventCount(): Promise<number> {
  const connection = await wireConnection()
  try {
    const [rows] = await connection.query<mysql.RowDataPacket[]>({
      sql: 'SELECT COUNT(*) FROM events',
      rowsAsArray: true,
    })
    return Number((rows[0] as unknown[])[0])
  } finally {
    await connection.end().catch(() => {})
  }
}

/// Commits rows to the source for as long as `running` holds, so the
/// replica's files - and therefore every query's stamp - keep moving.
async function cdcWriter(running: () => boolean): Promise<number> {
  let written = 0
  while (running()) {
    const values: string[] = []
    for (let row = 0; row < 200; row += 1) {
      const id = nextSourceId
      nextSourceId += 1
      values.push(`(${id},'r${id % 64}',${(id % 10_000) / 100},'note-${id % 997}')`)
    }
    await sql(`INSERT INTO events VALUES ${values.join(',')}`)
    written += values.length
    await Bun.sleep(250)
  }
  return written
}

/// What an open dashboard tab does: the endpoints it polls, at its cadence.
async function dashboardPollers(running: () => boolean, count: number): Promise<SideResult> {
  const endpoints = [
    `/api/activity?db=${databaseId}&limit=200`,
    `/api/dlq?db=${databaseId}`,
    `/api/databases/${databaseId}/status`,
    '/status',
  ]
  const latencies: number[] = []
  const errors: Record<string, number> = {}
  let failed = 0
  const poller = async (seat: number) => {
    let turn = seat
    while (running()) {
      const started = performance.now()
      try {
        await api(endpoints[turn % endpoints.length]!)
        latencies.push(performance.now() - started)
      } catch (error) {
        tally(errors, String(error))
        failed += 1
      }
      turn += 1
      await Bun.sleep(100)
    }
  }
  await Promise.all(Array.from({ length: count }, (_, seat) => poller(seat)))
  latencies.sort((left, right) => left - right)
  return { completed: latencies.length, failed, p99Ms: percentile(latencies, 0.99), errors }
}

/// Queries through the HTTP surface, which builds an engine per request:
/// the path where a per-request replica load was invisible to the wire
/// numbers.
async function httpQueryClients(running: () => boolean, count: number): Promise<SideResult> {
  const latencies: number[] = []
  const errors: Record<string, number> = {}
  let failed = 0
  const client = async (seat: number) => {
    let turn = seat
    while (running()) {
      const started = performance.now()
      try {
        await api('/api/query', {
          method: 'POST',
          body: { db: databaseId, sql: QUERIES[turn % QUERIES.length] },
        })
        latencies.push(performance.now() - started)
      } catch (error) {
        tally(errors, String(error))
        failed += 1
      }
      turn += 1
    }
  }
  await Promise.all(Array.from({ length: count }, (_, seat) => client(seat)))
  latencies.sort((left, right) => left - right)
  return { completed: latencies.length, failed, p99Ms: percentile(latencies, 0.99), errors }
}

async function runLevel(concurrency: number): Promise<LevelResult> {
  log(
    `level ${concurrency}: ${ITERATIONS} queries per client` +
      (CONNECTION_STORM ? ', a connection per query' : '') +
      (SIDELOADS.size ? `, with ${[...SIDELOADS].join('+')}` : ''),
  )
  const latencies: number[] = []
  const errors: Record<string, number> = {}
  let completed = 0
  let failed = 0
  let peakRssMb = 0

  // Sample RSS while the level runs; a peak taken only at the end misses
  // the spike that matters.
  let running = true
  const sampler = (async () => {
    while (running) {
      peakRssMb = Math.max(peakRssMb, await serverRssMb())
      await Bun.sleep(200)
    }
  })()

  const client = async (index: number) => {
    let connection: mysql.Connection | undefined
    for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
      if (!connection || CONNECTION_STORM) {
        await connection?.end().catch(() => {})
        try {
          connection = await wireConnection()
        } catch (error) {
          tally(errors, String(error))
          failed += CONNECTION_STORM ? 1 : ITERATIONS - iteration
          if (CONNECTION_STORM) continue
          return
        }
      }
      const statement = QUERIES[(index + iteration) % QUERIES.length]
      const started = performance.now()
      try {
        await connection.query({ sql: statement, rowsAsArray: true })
        latencies.push(performance.now() - started)
        completed += 1
      } catch (error) {
        tally(errors, String(error))
        failed += 1
      }
    }
    await connection?.end().catch(() => {})
  }

  const replicaBefore = SIDELOADS.has('cdc') ? await replicaEventCount() : 0
  const isRunning = () => running
  const sideloads = {
    cdc: SIDELOADS.has('cdc') ? cdcWriter(isRunning) : undefined,
    dashboard: SIDELOADS.has('dashboard') ? dashboardPollers(isRunning, 4) : undefined,
    http: SIDELOADS.has('http')
      ? httpQueryClients(isRunning, Math.max(1, Math.floor(concurrency / 4)))
      : undefined,
  }
  await Promise.all(Array.from({ length: concurrency }, (_, index) => client(index)))
  running = false
  await sampler
  const [cdcRows, dashboard, http] = await Promise.all([
    sideloads.cdc,
    sideloads.dashboard,
    sideloads.http,
  ])

  // The replica has to hold every row the writer committed, and how long
  // that takes after the storm is the recovery number.
  let cdcConvergeMs: number | undefined
  if (cdcRows !== undefined) {
    const wanted = replicaBefore + cdcRows
    const started = performance.now()
    cdcConvergeMs = -1
    while (performance.now() - started < 120_000) {
      if ((await replicaEventCount()) >= wanted) {
        cdcConvergeMs = performance.now() - started
        break
      }
      await Bun.sleep(500)
    }
  }

  latencies.sort((left, right) => left - right)
  return {
    concurrency,
    completed,
    failed,
    p50Ms: percentile(latencies, 0.5),
    p95Ms: percentile(latencies, 0.95),
    p99Ms: percentile(latencies, 0.99),
    maxMs: latencies.at(-1) ?? 0,
    peakRssMb,
    errors,
    http,
    dashboard,
    cdcRows,
    cdcConvergeMs,
  }
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_LOAD_BINARY) return resolve(process.env.PINTAIL_LOAD_BINARY)
  log('building the release pintail binary')
  await command(['cargo', 'build', '--release', '-p', 'pintail'])
  const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'], {
    quiet: true,
  })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function startPintail() {
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
        PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(MEMORY_MB * 1024 * 1024),
        ...(TOTAL_MEMORY_MB > 0
          ? { PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES: String(TOTAL_MEMORY_MB * 1024 * 1024) }
          : {}),
        ...(MAX_CONCURRENT > 0 ? { PINTAIL_MAX_CONCURRENT_QUERIES: String(MAX_CONCURRENT) } : {}),
      },
    },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (pintailProcess.exitCode !== null) {
      throw new Error(`pintail exited during startup (exit ${pintailProcess.exitCode})`)
    }
    try {
      const response = await fetch(`${pintailUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy within 120 seconds')
}

async function seed() {
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )
  await sql(`
    CREATE TABLE events (
      id BIGINT UNSIGNED PRIMARY KEY,
      region VARCHAR(32) NOT NULL,
      amount DECIMAL(12,2) NOT NULL,
      note VARCHAR(64) NOT NULL
    )
  `)
  log(`seeding ${SEED_ROWS} rows`)
  // Batched multi-row inserts: row-at-a-time would dominate the runtime.
  const batch = 5_000
  for (let start = 0; start < SEED_ROWS; start += batch) {
    const values: string[] = []
    for (let row = start; row < Math.min(start + batch, SEED_ROWS); row += 1) {
      values.push(`(${row},'r${row % 64}',${(row % 10_000) / 100},'note-${row % 997}')`)
    }
    await sql(`INSERT INTO events VALUES ${values.join(',')}`)
  }
}

function side(result: SideResult | undefined): string {
  if (!result) return '—'
  const errors = describeErrors(result.errors)
  return `${result.completed} ok / ${result.failed} failed, p99 ${result.p99Ms.toFixed(0)}ms${errors ? ` (${errors})` : ''}`
}

function cdc(result: LevelResult): string {
  if (result.cdcRows === undefined) return '—'
  if (result.cdcConvergeMs === undefined || result.cdcConvergeMs < 0) {
    return `${result.cdcRows} rows, NOT converged`
  }
  return `${result.cdcRows} rows, converged in ${result.cdcConvergeMs.toFixed(0)}ms`
}

function resultsPath(extension: string): string {
  const suffix = PROFILE === 'default' ? '' : `-${PROFILE}`
  return join(import.meta.dir, `results${suffix}.${extension}`)
}

function publish() {
  const lines = [
    `# Pintail concurrency load results${PROFILE === 'default' ? '' : ` (${PROFILE})`}`,
    '',
    `Measured ${new Date().toISOString()}.`,
    '',
    `Per-query memory ceiling: ${MEMORY_MB} MB. ` +
      `Process budget: ${TOTAL_MEMORY_MB > 0 ? `${TOTAL_MEMORY_MB} MB` : 'server default'}. ` +
      `Admission: ${MAX_CONCURRENT > 0 ? `${MAX_CONCURRENT} concurrent` : 'server default'}.`,
    `Seed rows: ${SEED_ROWS}. Queries per client per level: ${ITERATIONS}. ` +
      `Connections: ${CONNECTION_STORM ? 'one per query' : 'one per client'}. ` +
      `Side-loads: ${SIDELOADS.size ? [...SIDELOADS].join(', ') : 'none'}. ` +
      `RSS ceiling: ${RSS_CEILING_MB > 0 ? `${RSS_CEILING_MB} MB` : 'unchecked'}.`,
    '',
    '| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors | HTTP queries | Dashboard | CDC |',
    '|---:|---:|---:|---:|---:|---:|---:|---:|---|---|---|---|',
    ...results.map((result) => {
      const errors = describeErrors(result.errors)
      return `| ${result.concurrency} | ${result.completed} | ${result.failed} | ${result.p50Ms.toFixed(0)} | ${result.p95Ms.toFixed(0)} | ${result.p99Ms.toFixed(0)} | ${result.maxMs.toFixed(0)} | ${result.peakRssMb.toFixed(0)} | ${errors || '—'} | ${side(result.http)} | ${side(result.dashboard)} | ${cdc(result)} |`
    }),
    '',
  ]
  writeFileSync(resultsPath('md'), lines.join('\n'))
  writeFileSync(
    resultsPath('json'),
    JSON.stringify(
      {
        profile: PROFILE,
        memoryMb: MEMORY_MB,
        totalMemoryMb: TOTAL_MEMORY_MB,
        maxConcurrent: MAX_CONCURRENT,
        seedRows: SEED_ROWS,
        iterations: ITERATIONS,
        connectionStorm: CONNECTION_STORM,
        sideloads: [...SIDELOADS],
        rssCeilingMb: RSS_CEILING_MB,
        results,
      },
      null,
      2,
    ),
  )
}

/// What the profile promised: the process stayed under its ceiling, and
/// the replica held every row the writer committed. Either failing is a
/// defect in the server, not in the harness, so the run exits non-zero.
function verdict(): string[] {
  const failures: string[] = []
  for (const result of results) {
    if (RSS_CEILING_MB > 0 && result.peakRssMb > RSS_CEILING_MB) {
      failures.push(
        `level ${result.concurrency}: peak RSS ${result.peakRssMb.toFixed(0)}MB over the ${RSS_CEILING_MB}MB ceiling`,
      )
    }
    if (result.cdcRows !== undefined && (result.cdcConvergeMs ?? -1) < 0) {
      failures.push(`level ${result.concurrency}: replica never caught up with the CDC writer`)
    }
    // Load shedding and the memory ceiling are designed answers. A dropped
    // connection, a timeout or anything unclassified is not.
    for (const [kind, count] of Object.entries(result.errors)) {
      if (kind !== 'admission-refused' && kind !== 'query-memory-limit') {
        failures.push(`level ${result.concurrency}: ${count} wire ${kind} errors`)
      }
    }
  }
  return failures
}

async function main() {
  const host = await dockerHost()
  log(`profile ${PROFILE}`)
  log(`starting MySQL source ${mysqlName}`)
  await docker(
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
    'mysql:8.4',
    '--server-id=947',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--binlog-row-metadata=FULL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await seed()

  pintailBinary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-load-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  await startPintail()

  const setup = await api<{ token: string }>('/api/auth/setup', {
    method: 'POST',
    auth: false,
    body: { email: 'load@pintail.local', password: 'load-gate-password' },
  })
  token = setup.token
  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: {
      name: DATABASE,
      dsn: `mysql://pintail:pintail@${host}:${mysqlPort}/${DATABASE}`,
      mode: 'cdc',
    },
  })
  databaseId = database.id
  const apiKey = await api<{ secret: string }>(`/api/databases/${databaseId}/api-keys`, {
    method: 'POST',
    body: { name: 'load-gate', scopes: ['query', 'read'] },
  })
  wireSecret = apiKey.secret
  await api(`/api/databases/${databaseId}/probe`)
  const accepted = await api<{ run_id: string }>(`/api/databases/${databaseId}/snapshot`, {
    method: 'POST',
    body: { force: false },
  })
  log(`snapshot ${accepted.run_id} started`)
  for (;;) {
    const status = await api<{ state: string; tables: Array<{ last_error?: string }> }>(
      `/api/databases/${databaseId}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables.map((table) => table.last_error).filter(Boolean).join('; ')}`,
      )
    }
    if (status.state === 'polling' || status.state === 'streaming') break
    await Bun.sleep(1_000)
  }
  log('snapshot converged')

  for (const level of LEVELS) {
    const result = await runLevel(level)
    results.push(result)
    const errors = describeErrors(result.errors)
    log(
      `level ${level}: ${result.completed} ok, ${result.failed} failed, ` +
        `p50 ${result.p50Ms.toFixed(0)}ms p99 ${result.p99Ms.toFixed(0)}ms, ` +
        `peak RSS ${result.peakRssMb.toFixed(0)}MB${errors ? ` — ${errors}` : ''}` +
        (result.http ? `; http ${side(result.http)}` : '') +
        (result.dashboard ? `; dashboard ${side(result.dashboard)}` : '') +
        (result.cdcRows !== undefined ? `; cdc ${cdc(result)}` : ''),
    )
  }
  publish()
  log(`results written to ${resultsPath('md')}`)
  const failures = verdict()
  if (failures.length) throw new Error(`the profile's promises did not hold:\n  ${failures.join('\n  ')}`)
}

async function teardown() {
  if (pintailProcess) pintailProcess.kill()
  await mysqlConnection?.end().catch(() => {})
  if (mysqlStarted) await docker('rm', '--force', mysqlName).catch(() => {})
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
}

try {
  await main()
} catch (error) {
  console.error(`[load] FAILED: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await teardown()
}

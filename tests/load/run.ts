/// Concurrency load harness: what the engine does when queries arrive together.
///
/// Every other gate measures one query at a time. The benchmark reports
/// single-query latency, the oracle and E2E gates assert correctness on a
/// quiet server. None of them answer the question this harness exists for:
/// what happens when N clients query at once.
///
/// That matters because the query memory ceiling is enforced PER QUERY
/// (`MemoryTracker`, built in `Execution::start_with_deadline`), while the
/// wire server accepts connections unconditionally — no semaphore, no
/// admission control. The arithmetic bound is therefore
/// `concurrent_queries x per_query_limit`, and nothing enforces the left
/// factor. This harness measures where that lands in practice.
///
/// It reports, per concurrency level: latency percentiles, error counts
/// bucketed by kind, and the server's peak RSS. RSS is the load-bearing
/// number — it is what distinguishes "the ceiling held" from "the ceiling
/// held per query and the process still grew without bound".
///
/// Run with: bun run run.ts
///           LOAD_LEVELS=1,8,32 LOAD_MEMORY_MB=64 bun run run.ts

import { createServer } from 'node:net'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-load-mysql-${process.pid}-${nonce}`
const DATABASE = 'load_db'

/// Concurrency levels to sweep. Rising powers of two make the shape of the
/// degradation curve readable rather than a single pass/fail point.
const LEVELS = (process.env.LOAD_LEVELS ?? '1,4,16,64')
  .split(',')
  .map((level) => Number(level.trim()))
  .filter((level) => Number.isFinite(level) && level > 0)

/// Per-query ceiling for the server under test. Deliberately small: the
/// point is to reach the ceiling, not to avoid it.
const MEMORY_MB = Number(process.env.LOAD_MEMORY_MB ?? '64')

/// Queries per client per level.
const ITERATIONS = Number(process.env.LOAD_ITERATIONS ?? '10')

/// Rows seeded into the source. Large enough that a sort or aggregate has
/// to do real work, small enough that snapshotting stays under a minute.
const SEED_ROWS = Number(process.env.LOAD_SEED_ROWS ?? '200000')

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
  const target = endpoint.slice('ssh://'.length).split('@').at(-1)!.split(':')[0]
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
  if (/query memory limit exceeded/i.test(message)) return 'query-memory-limit'
  if (/too many connections|connection limit/i.test(message)) return 'connection-limit'
  if (/ECONNRESET|socket hang up|closed/i.test(message)) return 'connection-dropped'
  if (/ETIMEDOUT|timeout/i.test(message)) return 'timeout'
  if (/ECONNREFUSED/i.test(message)) return 'connection-refused'
  return 'other'
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

async function runLevel(concurrency: number): Promise<LevelResult> {
  log(`level ${concurrency}: ${ITERATIONS} queries per client`)
  const latencies: number[] = []
  const errors: Record<string, number> = {}
  let completed = 0
  let failed = 0
  let peakRssMb = 0

  // Sample RSS while the level runs; a peak taken only at the end misses
  // the spike that matters.
  let sampling = true
  const sampler = (async () => {
    while (sampling) {
      peakRssMb = Math.max(peakRssMb, await serverRssMb())
      await Bun.sleep(200)
    }
  })()

  const client = async (index: number) => {
    let connection: mysql.Connection | undefined
    try {
      connection = await mysql.createConnection({
        host: '127.0.0.1',
        port: pintailWirePort,
        user: DATABASE,
        password: wireSecret,
        database: DATABASE,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
      })
    } catch (error) {
      const kind = errorKind(String(error))
      errors[kind] = (errors[kind] ?? 0) + 1
      failed += ITERATIONS
      return
    }
    for (let iteration = 0; iteration < ITERATIONS; iteration += 1) {
      const statement = QUERIES[(index + iteration) % QUERIES.length]
      const started = performance.now()
      try {
        await connection.query({ sql: statement, rowsAsArray: true })
        latencies.push(performance.now() - started)
        completed += 1
      } catch (error) {
        const kind = errorKind(String(error))
        errors[kind] = (errors[kind] ?? 0) + 1
        failed += 1
      }
    }
    await connection.end().catch(() => {})
  }

  await Promise.all(Array.from({ length: concurrency }, (_, index) => client(index)))
  sampling = false
  await sampler

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

function publish() {
  const lines = [
    '# Pintail concurrency load results',
    '',
    `Measured ${new Date().toISOString()}.`,
    '',
    `Per-query memory ceiling: ${MEMORY_MB} MB. Seed rows: ${SEED_ROWS}.`,
    `Queries per client per level: ${ITERATIONS}.`,
    '',
    '| Concurrency | Completed | Failed | p50 ms | p95 ms | p99 ms | max ms | peak RSS MB | Errors |',
    '|---:|---:|---:|---:|---:|---:|---:|---:|---|',
    ...results.map((result) => {
      const errors = Object.entries(result.errors)
        .map(([kind, count]) => `${kind}×${count}`)
        .join(', ')
      return `| ${result.concurrency} | ${result.completed} | ${result.failed} | ${result.p50Ms.toFixed(0)} | ${result.p95Ms.toFixed(0)} | ${result.p99Ms.toFixed(0)} | ${result.maxMs.toFixed(0)} | ${result.peakRssMb.toFixed(0)} | ${errors || '—'} |`
    }),
    '',
  ]
  writeFileSync(join(import.meta.dir, 'results.md'), lines.join('\n'))
  writeFileSync(
    join(import.meta.dir, 'results.json'),
    JSON.stringify({ memoryMb: MEMORY_MB, seedRows: SEED_ROWS, iterations: ITERATIONS, results }, null, 2),
  )
}

async function main() {
  const host = await dockerHost()
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
    const errors = Object.entries(result.errors)
      .map(([kind, count]) => `${kind}×${count}`)
      .join(', ')
    log(
      `level ${level}: ${result.completed} ok, ${result.failed} failed, ` +
        `p50 ${result.p50Ms.toFixed(0)}ms p99 ${result.p99Ms.toFixed(0)}ms, ` +
        `peak RSS ${result.peakRssMb.toFixed(0)}MB${errors ? ` — ${errors}` : ''}`,
    )
  }
  publish()
  log(`results written to ${join(import.meta.dir, 'results.md')}`)
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

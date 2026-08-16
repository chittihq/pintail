import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createServer } from 'node:net'
import mysql from 'mysql2/promise'
import { benchmarkQueries } from './queries'
import { pintailSpecificMinimumRegressions, type ComparableReport } from './evidence'

type CommandResult = { stdout: string; stderr: string }
type EngineTiming = {
  medianMs: number
  p95Ms: number
  minMs: number
  runs: number
  meanMs: number
  stddevMs: number
  ci95LowMs: number
  ci95HighMs: number
  /// Every measured execution, sorted. Published so the summary can be
  /// checked rather than trusted.
  samplesMs: number[]
}
type EngineResources = { cpuPeakPct: number; cpuAvgPct: number; memPeakMb: number }
type QueryResult = {
  name: string
  mysqlMs: number
  pintailMs: number
  clickhouseMs: number
  clickhouseFinalMs: number
  speedup: number
  /// CH RMT+FINAL medianMs / pintail medianMs: >1 means pintail is faster
  /// than ClickHouse doing the same merge-on-read duty.
  speedupVsClickhouse: number
  coldOnly: boolean
  timings: Record<string, EngineTiming>
  resources: Record<string, EngineResources>
  pintailMatchesMysql: boolean
  clickhouseFinalMatchesMysql: boolean
  pintailExplain?: string
}

const benchmarkDir = import.meta.dir
const repository = resolve(benchmarkDir, '..')
const scale = Number(process.env.BENCHMARK_SCALE ?? '1')
if (!Number.isFinite(scale) || scale <= 0 || scale > 10) {
  throw new Error('BENCHMARK_SCALE must be greater than zero and at most 10')
}
const batches = Math.max(1, Math.round(scale * 2000))
const orderRows = batches * 10_000
const fullGate = orderRows === 20_000_000
const runId = `pintail-m9-bench-${process.pid}-${Date.now()}`
const mysqlName = `${runId}-mysql`
const clickhouseName = `${runId}-clickhouse`
const pintailName = `${runId}-pintail`
const networkName = `${runId}-network`
// Fairness: every engine runs on the docker host under identical limits.
// PINTAIL_BENCHMARK_LOCAL=1 restores the old local-process mode for dev.
const containerizedPintail = process.env.PINTAIL_BENCHMARK_LOCAL !== '1'
const engineLimits = ['--cpus', '8', '--memory', '8g']

// Five samples is too few to separate two engines whose spread overlaps, and
// one warmup does not reliably settle a cold page cache. Raised, and made
// configurable so a smoke run can stay cheap without the default being cheap.
const WARMUP_COUNT = Number(process.env.BENCHMARK_WARMUPS ?? 2)
const RUN_COUNT = Number(process.env.BENCHMARK_RUNS ?? 15)

// Order matters as much as the count. Measuring the engines in a fixed
// sequence lets any drift on a shared host - another tenant, a thermal ramp -
// land on whichever always goes last and read as a property of that engine.
// The order is shuffled per query from this seed, which the artifact records:
// a shuffled order that cannot be reproduced is just an unexplained one.
//
// Shuffled rather than interleaved: each engine's samples stay contiguous, so
// the container resource sampler still attributes CPU and memory to the engine
// that spent it, which interleaving would smear across all three.
const ENGINE_ORDER_SEED = Number(process.env.BENCHMARK_SEED ?? 0x5eed)

// Concurrency levels. One client measures an engine at rest; a server is
// asked many things at once, and that is where admission, memory accounting
// and lock contention show up.
const CONCURRENCY_CLIENTS = (process.env.BENCHMARK_CONCURRENCY ?? '1,4,8,16')
  .split(',')
  .map((value) => Number(value.trim()))
  .filter((value) => Number.isFinite(value) && value > 0)
const CONCURRENCY_SECONDS = Number(process.env.BENCHMARK_CONCURRENCY_SECONDS ?? 10)
const mysqlImage = 'mysql:8.4'
const mysqlServerArgs = [
  '--server-id=909',
  '--log-bin=mysql-bin',
  '--binlog-format=ROW',
  '--binlog-row-image=FULL',
  '--binlog-row-metadata=FULL',
  '--gtid-mode=ON',
  '--enforce-gtid-consistency=ON',
  '--default-time-zone=+00:00',
  '--sql-mode=NO_ENGINE_SUBSTITUTION',
  '--innodb-buffer-pool-size=1G',
]

// The MySQL baseline (seeded data and cold query timings) is a pure function
// of these inputs. Reruns with an identical fingerprint reuse the seeded
// datadir volume and the recorded cold timings instead of paying ~10 minutes
// of seeding and ~an hour of single-core MySQL queries again.
// Seeded data is a pure function of these inputs ONLY — query text must
// not participate, or every new benchmark query would force a ~10-minute
// reseed. Baseline entries carry their own per-query SQL hash instead.
const benchmarkFingerprint = createHash('sha256')
  .update(
    JSON.stringify({
      schemaSql: readFileSync(join(benchmarkDir, 'schema.sql'), 'utf8'),
      seedSql: readFileSync(join(benchmarkDir, 'seed.sql'), 'utf8'),
      orderRows,
      engineLimits,
      mysqlImage,
      mysqlServerArgs,
    }),
  )
  .digest('hex')
const sqlHash = (sql: string) => createHash('sha256').update(sql).digest('hex').slice(0, 16)
const seedVolumeName = `pintail-bench-seed-${benchmarkFingerprint.slice(0, 12)}`
const runVolumeName = `${runId}-mysql-data`
const pintailVolumeName = `${runId}-pintail-data`
const baselinePath = join(benchmarkDir, 'mysql-baseline.json')
type MysqlBaseline = {
  fingerprint: string
  // Cold timings are hardware-bound: a baseline from one docker host must
  // never be reused on another. Persist only a one-way fingerprint so a
  // tracked benchmark ledger does not disclose private infrastructure names.
  hostFingerprint: string
  measuredAt: string
  gitCommit?: string
  queries: Record<string, { ms: number; canonical: string; sqlHash?: string }>
}
let baselineProvenance: string | undefined
let runVolumeCreated = false
let dockerHostName = ''
let engineFingerprint = ''

const hostFingerprint = () => createHash('sha256').update(dockerHostName).digest('hex')

function loadMysqlBaseline(): MysqlBaseline | undefined {
  if (!existsSync(baselinePath)) return undefined
  try {
    const parsed = JSON.parse(readFileSync(baselinePath, 'utf8')) as MysqlBaseline
    if (parsed.fingerprint !== benchmarkFingerprint) return undefined
    if (parsed.hostFingerprint !== hostFingerprint()) {
      log('MySQL baseline was measured on a different docker host: remeasuring')
      return undefined
    }
    return parsed
  } catch {
    return undefined
  }
}

async function volumeExists(name: string): Promise<boolean> {
  try {
    await docker('volume', 'inspect', name)
    return true
  } catch {
    return false
  }
}
const clickhouseHeaders = {
  Authorization: `Basic ${btoa('default:pintail-benchmark')}`,
}
const dataDir = mkdtempSync(join(tmpdir(), 'pintail-m9-benchmark-'))
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let mysqlConnection: mysql.Connection | undefined
let mysqlEndpoint: { host: string; port: number } | undefined
let dockerCreated = false

function log(message: string) {
  console.log(`[benchmark] ${message}`)
}

async function command(
  args: string[],
  options: { cwd?: string; stdin?: string; quiet?: boolean } = {},
): Promise<CommandResult> {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
    stdin: options.stdin === undefined ? 'ignore' : new Blob([options.stdin]),
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) {
    throw new Error(
      `${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`,
    )
  }
  if (!options.quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (
      await docker(
        'context',
        'inspect',
        context,
        '--format',
        '{{.Endpoints.docker.Host}}',
      )
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
      server.close((error) => {
        if (error) reject(error)
        else resolvePort(address.port)
      })
    })
  })
}

async function waitForMysql(host: string, port: number, attempts = 120) {
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
        // Cold analytic queries keep the TCP session silent for minutes
        // while the server computes; keepalives stop idle timeouts on the
        // path to a remote docker host from dropping the connection.
        enableKeepAlive: true,
        keepAliveInitialDelay: 10_000,
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready within 60 seconds')
}

async function waitForClickhouse(baseUrl: string) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/ping`, { headers: clickhouseHeaders })
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('ClickHouse did not become ready within 60 seconds')
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
  if (!response.ok) {
    throw new Error(`${options.method ?? 'GET'} ${path} returned ${response.status}: ${text}`)
  }
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

async function waitForHttp(baseUrl: string) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('Pintail did not become ready within 120 seconds')
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_BENCHMARK_BINARY) {
    return resolve(process.env.PINTAIL_BENCHMARK_BINARY)
  }
  log('building the release binary')
  const build = Bun.spawn(['cargo', 'build', '--release', '-p', 'pintail'], {
    cwd: repository,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  if ((await build.exited) !== 0) throw new Error('release build failed')
  const metadata = await command(
    ['cargo', 'metadata', '--format-version', '1', '--no-deps'],
    { quiet: true },
  )
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function seedSource(connection: mysql.Connection) {
  log(`seeding ${orderRows.toLocaleString()} deterministic orders`)
  await connection.query('SET SESSION sql_log_bin=0')
  await connection.query('CREATE DATABASE benchmark_db')
  await connection.query('USE benchmark_db')
  await connection.query(readFileSync(join(benchmarkDir, 'schema.sql'), 'utf8'))
  await connection.query(readFileSync(join(benchmarkDir, 'seed.sql'), 'utf8'))
  const started = performance.now()
  await connection.query('CALL seed_orders(?)', [batches])
  await connection.query('DROP PROCEDURE seed_orders')
  await connection.query(
    "CREATE USER IF NOT EXISTS 'benchmark'@'%' IDENTIFIED BY 'benchmarkpass'",
  )
  await connection.query(
    "GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT " +
      "ON *.* TO 'benchmark'@'%'",
  )
  await connection.query('SET SESSION sql_log_bin=1')
  await connection.query(
    'CREATE TABLE benchmark_cdc_marker (id INT); DROP TABLE benchmark_cdc_marker',
  )
  log(`source seed completed in ${Math.round(performance.now() - started).toLocaleString()} ms`)
}

async function importClickhouse(baseUrl: string) {
  log('importing the source tables into ClickHouse')
  const query = async (sql: string) => {
    const response = await fetch(`${baseUrl}/?database=default`, {
      method: 'POST',
      headers: clickhouseHeaders,
      body: sql,
    })
    const text = await response.text()
    if (!response.ok) throw new Error(`ClickHouse import failed: ${text}`)
  }
  await query('CREATE DATABASE benchmark')
  const tables = {
    users: {
      schema:
        'id UInt32, name String, email String, region String, ' +
        'created_at DateTime, updated_at DateTime',
      projection: '*',
    },
    products: {
      schema:
        'id UInt32, name String, category String, price Decimal(10,2), ' +
        'created_at DateTime, updated_at DateTime',
      projection:
        'id, name, category, toDecimal64(price, 2), created_at, updated_at',
    },
    orders: {
      schema:
        'id UInt64, user_id UInt32, product_id UInt32, quantity UInt32, ' +
        'unit_price Decimal(10,2), total_amount Decimal(12,2), status String, ' +
        'region String, order_date Date, created_at DateTime, updated_at DateTime',
      projection:
        'id, user_id, product_id, quantity, toDecimal64(unit_price, 2), ' +
        'toDecimal64(total_amount, 2), status, region, order_date, created_at, updated_at',
    },
  }
  // benchmark:      plain MergeTree — the raw-speed ceiling reference.
  // benchmark_rmt:  ReplacingMergeTree read with final=1 — ClickHouse doing
  //                 the same always-correct merge-on-read duty pintail does
  //                 (issue #3 step 0: the apples-to-apples reference).
  await query('CREATE DATABASE benchmark_rmt')
  for (const [table, definition] of Object.entries(tables)) {
    for (const [database, engine] of [
      ['benchmark', 'MergeTree'],
      ['benchmark_rmt', 'ReplacingMergeTree'],
    ]) {
      await query(
        `CREATE TABLE ${database}.${table} (${definition.schema}) ` +
          `ENGINE = ${engine} ORDER BY id`,
      )
      await query(
        `INSERT INTO ${database}.${table} SELECT ${definition.projection} ` +
          `FROM mysql('${mysqlName}:3306', 'benchmark_db', '${table}', ` +
          "'benchmark', 'benchmarkpass')",
      )
    }
  }
}

async function createReplica(baseUrl: string, token: string, dsn: string): Promise<string> {
  const database = await api<{ id: string }>(baseUrl, '/api/databases', {
    method: 'POST',
    token,
    body: {
      name: 'benchmark_db',
      dsn,
      mode: 'cdc',
      include_tables: ['orders', 'products', 'users'],
    },
  })
  await api(baseUrl, `/api/databases/${database.id}/probe`, { token })
  const accepted = await api<{ run_id: string }>(
    baseUrl,
    `/api/databases/${database.id}/snapshot`,
    { method: 'POST', token, body: { force: false } },
  )
  log(`snapshot ${accepted.run_id} started`)
  for (let attempt = 0; attempt < 14_400; attempt += 1) {
    const status = await api<{
      state: string
      tables: Array<{ name: string; rows: number; last_error?: string }>
    }>(baseUrl, `/api/databases/${database.id}/snapshot/status`, { token })
    if (status.state === 'error') {
      const activity = await api<unknown[]>(
        baseUrl,
        `/api/activity?db=${database.id}&limit=10`,
        { token },
      )
      throw new Error(
        `snapshot failed: ${status.tables
          .map((table) => table.last_error)
          .filter(Boolean)
          .join('; ')}\n${JSON.stringify(activity, null, 2)}`,
      )
    }
    const rows = Object.fromEntries(status.tables.map((table) => [table.name, table.rows]))
    if (
      (status.state === 'polling' || status.state === 'streaming') &&
      rows.orders === orderRows &&
      rows.users === 100_000 &&
      rows.products === 10_000
    ) {
      return database.id
    }
    if (attempt % 60 === 0) {
      log(
        `snapshot progress: ${Number(rows.orders ?? 0).toLocaleString()} / ${orderRows.toLocaleString()} orders`,
      )
    }
    await Bun.sleep(1000)
  }
  throw new Error('snapshot did not complete within four hours')
}

/// Verification runs straight after the snapshot, which is the longest
/// silence on this connection in the whole run - twenty million rows is
/// minutes of it - so the session is routinely dead by the time the first
/// count is asked for. `mysqlColdQuery` already reconnects for the same
/// reason on the query path; this asks through it rather than holding its own
/// handle, so a dropped session costs a reconnect instead of the run.
async function verifyCounts(
  clickhouseUrl: string,
  pintailUrl: string,
  token: string,
  databaseId: string,
) {
  const mysqlRows = await mysqlColdQuery('SELECT COUNT(*) AS count FROM benchmark_db.orders')
  const clickhouseResponse = await fetch(`${clickhouseUrl}/?database=benchmark`, {
    method: 'POST',
    headers: clickhouseHeaders,
    body: 'SELECT COUNT(*) FROM orders FORMAT JSONCompact',
  })
  if (!clickhouseResponse.ok) throw new Error(await clickhouseResponse.text())
  const clickhouseRows = (await clickhouseResponse.json()) as { data: unknown[][] }
  const pintailRows = await api<{ rows: unknown[][] }>(pintailUrl, '/api/query', {
    method: 'POST',
    token,
    body: { db: databaseId, sql: 'SELECT COUNT(*) FROM orders' },
  })
  const counts = [
    Number(mysqlRows[0][0]),
    Number(clickhouseRows.data[0][0]),
    Number(pintailRows.rows[0][0]),
  ]
  if (counts.some((count) => count !== orderRows)) {
    throw new Error(`row-count verification failed: MySQL/ClickHouse/Pintail=${counts.join('/')}`)
  }
  log(`all engines expose ${orderRows.toLocaleString()} orders`)
}

/// Polls docker stats for one container while an engine is being measured.
/// CPU% is cumulative across cores (an 8-cpu container can read 800%).
function startResourceSampler(container: string) {
  const samples: { cpuPct: number; memMb: number }[] = []
  let active = true
  const loop = (async () => {
    while (active) {
      try {
        const out = (
          await docker('stats', '--no-stream', '--format', '{{.CPUPerc}}|{{.MemUsage}}', container)
        ).stdout
        const [cpuText, memText] = out.split('|')
        const cpuPct = Number.parseFloat(cpuText)
        // MemUsage reads "512.3MiB / 8GiB": only the usage half decides
        // the unit, or the ever-present GiB limit inflates MiB by 1024.
        const usageText = memText.split('/')[0]
        const memValue = Number.parseFloat(usageText)
        const memMb = usageText.includes('GiB')
          ? memValue * 1024
          : usageText.includes('KiB')
            ? memValue / 1024
            : memValue
        if (Number.isFinite(cpuPct) && Number.isFinite(memMb)) {
          samples.push({ cpuPct, memMb })
        }
      } catch {
        // Container gone or stats hiccup: keep sampling.
      }
      if (active) await Bun.sleep(250)
    }
  })()
  return {
    async stop(): Promise<EngineResources> {
      active = false
      await loop
      if (samples.length === 0) return { cpuPeakPct: 0, cpuAvgPct: 0, memPeakMb: 0 }
      return {
        cpuPeakPct: Math.round(Math.max(...samples.map((sample) => sample.cpuPct))),
        cpuAvgPct: Math.round(
          samples.reduce((total, sample) => total + sample.cpuPct, 0) / samples.length,
        ),
        memPeakMb: Math.round(Math.max(...samples.map((sample) => sample.memMb))),
      }
    },
  }
}

async function sampled<T>(
  container: string | undefined,
  operation: () => Promise<T>,
): Promise<{ value: T; resources: EngineResources }> {
  if (!container) {
    return { value: await operation(), resources: { cpuPeakPct: 0, cpuAvgPct: 0, memPeakMb: 0 } }
  }
  const sampler = startResourceSampler(container)
  try {
    const value = await operation()
    return { value, resources: await sampler.stop() }
  } catch (error) {
    await sampler.stop()
    throw error
  }
}

// The MySQL side of a cold query can run for many minutes (Q6 exceeds 13)
// with zero traffic on the wire, long enough for an idle timeout between
// this machine and a remote docker host to kill the session. The failure
// only surfaces on the next command, so reconnect once and re-run it.
async function mysqlColdQuery(sql: string): Promise<unknown[][]> {
  const run = async () => {
    const [rows] = await mysqlConnection!.query<mysql.RowDataPacket[]>({
      sql,
      rowsAsArray: true,
    })
    return rows as unknown as unknown[][]
  }
  try {
    return await run()
  } catch (error) {
    if (!mysqlEndpoint) throw error
    log(`MySQL connection dropped (${error}); reconnecting and retrying`)
    mysqlConnection?.destroy()
    mysqlConnection = await waitForMysql(mysqlEndpoint.host, mysqlEndpoint.port, 240)
    await mysqlConnection.query('USE benchmark_db')
    return run()
  }
}

async function timed<T>(operation: () => Promise<T>): Promise<{ value: T; ms: number }> {
  const started = performance.now()
  const value = await operation()
  return { value, ms: Math.max(1, Math.round(performance.now() - started)) }
}

function summarizeTimings(times: number[]): EngineTiming {
  const sorted = [...times].sort((a, b) => a - b)
  const at = (index: number) =>
    Math.max(1, Math.round(sorted[Math.min(sorted.length - 1, index)]))
  // Spread, not just the middle. A median alone cannot distinguish a stable
  // measurement from one that happened to land there: two engines reported as
  // 40ms and 44ms are indistinguishable if either swings 15ms run to run, and
  // the summary said nothing about that. The raw samples travel too, so a
  // reader can check the summary rather than take it.
  const mean = sorted.reduce((total, value) => total + value, 0) / sorted.length
  const variance =
    sorted.length > 1
      ? sorted.reduce((total, value) => total + (value - mean) ** 2, 0) / (sorted.length - 1)
      : 0
  const stddev = Math.sqrt(variance)
  // Normal-approximation 95% interval on the mean. Honest only as a rough
  // width at these sample counts, which is why the samples are published.
  const halfWidth = sorted.length > 1 ? (1.96 * stddev) / Math.sqrt(sorted.length) : 0
  const round2 = (value: number) => Math.round(value * 100) / 100
  return {
    medianMs: at(Math.floor(sorted.length / 2)),
    p95Ms: at(Math.ceil(sorted.length * 0.95) - 1),
    minMs: at(0),
    runs: sorted.length,
    meanMs: round2(mean),
    stddevMs: round2(stddev),
    ci95LowMs: round2(Math.max(0, mean - halfWidth)),
    ci95HighMs: round2(mean + halfWidth),
    samplesMs: sorted.map((value) => round2(value)),
  }
}

async function measuredVariants<T, V>(
  variants: V[],
  operation: (variant: V) => Promise<T>,
): Promise<{ values: T[]; timing: EngineTiming }> {
  const values: T[] = []
  const times: number[] = []
  for (const variant of variants) {
    const sample = await timed(() => operation(variant))
    values.push(sample.value)
    times.push(sample.ms)
  }
  return { values, timing: summarizeTimings(times) }
}

/// Warm multi-iteration measurement: median/p95/min over `runs` after
/// `warmups` unmeasured executions. MySQL keeps a single cold run (it is the
/// baseline being escaped, and its full-scale queries run for minutes).
async function measured<T>(
  operation: () => Promise<T>,
  warmups: number,
  runs: number,
): Promise<{ value: T; timing: EngineTiming }> {
  let value!: T
  for (let i = 0; i < warmups; i += 1) {
    value = await operation()
  }
  const times: number[] = []
  for (let i = 0; i < runs; i += 1) {
    const started = performance.now()
    value = await operation()
    times.push(performance.now() - started)
  }
  return {
    value,
    timing: summarizeTimings(times),
  }
}

/// Order-sensitive canonical form for cross-engine result comparison:
/// numbers normalized to 4 decimal places, everything else stringified.
function canonicalRows(rows: unknown[][]): string {
  return rows
    .map((row) =>
      row
        .map((value) => {
          if (value === null || value === undefined) return 'NULL'
          const text = String(value)
          if (text !== '' && /^-?\d+(\.\d+)?$/.test(text)) {
            return Number(text).toFixed(4)
          }
          return text
        })
        .join('\u0001'),
    )
    // Sorted before joining: the comparison is a multiset check, insensitive
    // to tie-ordering under an under-determined ORDER BY (e.g. Q3 at smoke
    // scale, where every status count ties). Presentation-order correctness
    // belongs to the sqllogic oracle; this gate checks content.
    .sort()
    .join('\n')
}

/// Deterministic PRNG, so a shuffled engine order is reproducible from the
/// seed the artifact records.
function mulberry32(seed: number): () => number {
  let state = seed >>> 0
  return () => {
    state = (state + 0x6d2b79f5) >>> 0
    let value = Math.imul(state ^ (state >>> 15), 1 | state)
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296
  }
}

/// Runs the measurements in a shuffled order and returns them keyed by name,
/// so callers read results by engine rather than by position.
async function inShuffledOrder<T>(
  random: () => number,
  work: Record<string, () => Promise<T>>,
): Promise<Record<string, T>> {
  const names = Object.keys(work)
  for (let index = names.length - 1; index > 0; index -= 1) {
    const swap = Math.floor(random() * (index + 1))
    ;[names[index], names[swap]] = [names[swap], names[index]]
  }
  const out: Record<string, T> = {}
  for (const name of names) {
    out[name] = await work[name]()
  }
  return out
}

/// One engine's behaviour under N concurrent clients.
type ConcurrencyPoint = {
  clients: number
  completed: number
  /// Completed queries per second across all clients.
  throughput: number
  medianMs: number
  p95Ms: number
  errors: number
}

/// Drives `clients` concurrent callers at one operation for `seconds`.
///
/// Every published number so far is single-client, which says nothing about
/// the case a server actually faces. Two engines with the same median can
/// differ completely here: one holds its latency and adds throughput, the
/// other collapses because its admission or memory accounting serialises.
///
/// Throughput and p95 together, because either alone misleads - throughput
/// can rise while the slowest decile becomes unusable, and a flat p95 can
/// hide an engine that stopped accepting work.
async function measureConcurrency(
  operation: () => Promise<unknown>,
  clients: number,
  seconds: number,
): Promise<ConcurrencyPoint> {
  const latencies: number[] = []
  let errors = 0
  const until = performance.now() + seconds * 1000
  const client = async () => {
    while (performance.now() < until) {
      const started = performance.now()
      try {
        await operation()
        latencies.push(performance.now() - started)
      } catch {
        // Counted, not thrown: an engine that refuses work under load has
        // told us something, and losing the run would lose the finding.
        errors += 1
      }
    }
  }
  const started = performance.now()
  await Promise.all(Array.from({ length: clients }, client))
  const elapsed = (performance.now() - started) / 1000
  const sorted = [...latencies].sort((left, right) => left - right)
  const at = (index: number) =>
    sorted.length === 0 ? 0 : Math.round(sorted[Math.min(sorted.length - 1, index)])
  return {
    clients,
    completed: sorted.length,
    throughput: Math.round((sorted.length / elapsed) * 10) / 10,
    medianMs: at(Math.floor(sorted.length / 2)),
    p95Ms: at(Math.ceil(sorted.length * 0.95) - 1),
    errors,
  }
}

async function runQueries(
  connection: mysql.Connection,
  clickhouseUrl: string,
  pintailUrl: string,
  token: string,
  databaseId: string,
  options: { memoDisabled?: boolean } = {},
): Promise<QueryResult[]> {
  const results: QueryResult[] = []
  const warmups = WARMUP_COUNT
  const runs = RUN_COUNT
  const engineOrder = mulberry32(ENGINE_ORDER_SEED)
  const clickhouseQuery = async (database: string, sql: string, settings: string) => {
    // Retried once on a dropped connection, for the same reason the MySQL side
    // reconnects: a query that runs for minutes leaves the socket silent, and
    // something between here and the docker host resets it. A `FINAL` scan of
    // twenty million rows is long enough to hit that, and losing it fails the
    // whole stage - a 47-minute run ended on one ECONNRESET with every other
    // query already measured.
    //
    // The retry runs INSIDE the caller's timing, so a sample that needed one
    // carries the failed attempt's latency too. That is a contaminated sample,
    // not a clean one - it is accepted because the alternative is losing a
    // 47-minute stage to a single reset, and because the reported statistics
    // are the minimum and the median over fifteen runs, which one inflated
    // sample does not move. The retry is logged so a contaminated run is
    // visible rather than silent.
    const attempt = async () => {
      const response = await fetch(`${clickhouseUrl}/?database=${database}`, {
        method: 'POST',
        headers: clickhouseHeaders,
        body: `${sql}${settings} FORMAT JSONCompact`,
      })
      const text = await response.text()
      if (!response.ok) throw new Error(`ClickHouse query failed: ${text}`)
      return (JSON.parse(text) as { data: unknown[][] }).data
    }
    try {
      return await attempt()
    } catch (error) {
      const reset = /ECONNRESET|socket connection was closed|fetch failed|ConnectionRefused|Unable to connect/i.test(
        String(error),
      )
      if (!reset) throw error
      // A dropped connection here has meant the server CRASHED, not a
      // transient socket blip: the container restarts (policy above) but
      // needs time to come back. Poll readiness before the second attempt,
      // and capture the tail so the crash is diagnosable from the log.
      log(`  ClickHouse connection dropped (${error}); waiting for the server to return`)
      await docker('logs', '--tail', '20', clickhouseName)
        .then((tail) => log(`  clickhouse container tail:\n${tail}`))
        .catch(() => {})
      const deadline = Date.now() + 90_000
      while (Date.now() < deadline) {
        try {
          const ping = await fetch(`${clickhouseUrl}/ping`, { signal: AbortSignal.timeout(2_000) })
          if (ping.ok) break
        } catch {
          // still down
        }
        await new Promise((resolve) => setTimeout(resolve, 2_000))
      }
      return attempt()
    }
  }
  const baseline = loadMysqlBaseline()
  const freshBaseline: MysqlBaseline['queries'] = {}
  const saveBaseline = async () => {
    const record: MysqlBaseline = {
      fingerprint: benchmarkFingerprint,
      hostFingerprint: hostFingerprint(),
      measuredAt: new Date().toISOString(),
      gitCommit: (await command(['git', 'rev-parse', 'HEAD'], { quiet: true })).stdout,
      queries: { ...(baseline?.queries ?? {}), ...freshBaseline },
    }
    writeFileSync(baselinePath, `${JSON.stringify(record, null, 2)}\n`)
    log(`  MySQL baseline cached to ${baselinePath}`)
  }
  for (const query of benchmarkQueries) {
    log(query.name)
    const resources: Record<string, EngineResources> = {}
    const variants = query.coldOnly
      ? (query.coldVariants ?? []).map((variant) => ({
          sql: variant.sql,
          clickhouseSql: variant.clickhouseSql ?? variant.sql,
        }))
      : [{ sql: query.sql, clickhouseSql: query.clickhouseSql ?? query.sql }]
    if (variants.length === 0) throw new Error(`${query.name} has no cold variants`)
    const mysqlTimes: number[] = []
    const mysqlCanonicals: string[] = []
    for (const [index, variant] of variants.entries()) {
      const baselineKey = query.coldOnly ? `${query.name} [variant ${index + 1}]` : query.name
      const cached = baseline?.queries[baselineKey]
      const cacheValid =
        cached && (cached.sqlHash === undefined || cached.sqlHash === sqlHash(variant.sql))
      if (cached && cacheValid) {
        mysqlTimes.push(cached.ms)
        mysqlCanonicals.push(cached.canonical)
        baselineProvenance = baseline?.measuredAt
        log(`  MySQL baseline reused from ${baseline?.measuredAt} (${cached.ms} ms cold)`)
        continue
      }
      const mysqlSampled = await sampled(mysqlName, () => timed(() => mysqlColdQuery(variant.sql)))
      const mysqlRun = mysqlSampled.value
      resources.mysql = mysqlSampled.resources
      const canonical = canonicalRows(mysqlRun.value)
      mysqlTimes.push(mysqlRun.ms)
      mysqlCanonicals.push(canonical)
      freshBaseline[baselineKey] = {
        ms: mysqlRun.ms,
        canonical,
        sqlHash: sqlHash(variant.sql),
      }
      // Cold MySQL timings cost minutes each; persist after every variant so
      // a crash later in the run never throws measured work away.
      await saveBaseline()
    }
    const mysqlTiming = summarizeTimings(mysqlTimes)
    const mysqlMs = mysqlTiming.medianMs
    // Shuffled per query from the run seed, so no engine is always last.
    const measurements = await inShuffledOrder(engineOrder, {
      pintail: () =>
        sampled(containerizedPintail ? pintailName : undefined, () =>
          query.coldOnly
            ? measuredVariants(variants, (variant) =>
                api<{ rows: unknown[][] }>(pintailUrl, '/api/query', {
                  method: 'POST',
                  token,
                  body: { db: databaseId, sql: variant.sql },
                }),
              )
            : measured(
                () =>
                  api<{ rows: unknown[][] }>(pintailUrl, '/api/query', {
                    method: 'POST',
                    token,
                    body: { db: databaseId, sql: query.sql },
                  }),
                warmups,
                runs,
              ).then((run) => ({ values: [run.value], timing: run.timing })),
        ),
      clickhouse: () =>
        sampled(clickhouseName, () =>
          query.coldOnly
            ? measuredVariants(variants, (variant) =>
                clickhouseQuery('benchmark', variant.clickhouseSql, ''),
              )
            : measured(
                () => clickhouseQuery('benchmark', variants[0].clickhouseSql, ''),
                warmups,
                runs,
              ).then((run) => ({ values: [run.value], timing: run.timing })),
        ),
      // The comparable reference: ReplacingMergeTree doing pintail's
      // merge-on-read duty on every read (`final = 1`), same data, same host,
      // same limits.
      clickhouseFinal: () =>
        sampled(clickhouseName, () =>
          query.coldOnly
            ? measuredVariants(variants, (variant) =>
                clickhouseQuery('benchmark_rmt', variant.clickhouseSql, ' SETTINGS final = 1'),
              )
            : measured(
                () =>
                  clickhouseQuery(
                    'benchmark_rmt',
                    variants[0].clickhouseSql,
                    ' SETTINGS final = 1',
                  ),
                warmups,
                runs,
              ).then((run) => ({ values: [run.value], timing: run.timing })),
        ),
    })
    const pintailRun = measurements.pintail.value
    resources.pintail = measurements.pintail.resources
    const clickhouseRun = measurements.clickhouse.value
    resources.clickhouse = measurements.clickhouse.resources
    const clickhouseFinalRun = measurements.clickhouseFinal.value
    resources.clickhouseFinal = measurements.clickhouseFinal.resources
    const pintailMatchesMysql = pintailRun.values.every(
      (value, index) => canonicalRows(value.rows) === mysqlCanonicals[index],
    )
    const clickhouseFinalMatchesMysql = clickhouseFinalRun.values.every(
      (value, index) => canonicalRows(value) === mysqlCanonicals[index],
    )
    let pintailExplain: string | undefined
    try {
      const explain = await api<{ rows: unknown[][] }>(pintailUrl, '/api/query', {
        method: 'POST',
        token,
        body: { db: databaseId, sql: `EXPLAIN ANALYZE ${query.sql}` },
      })
      pintailExplain = explain.rows.map((row) => row.join(' ')).join('\n')
    } catch {
      pintailExplain = undefined
    }
    const speedup = mysqlMs / pintailRun.timing.medianMs
    const speedupVsClickhouse = clickhouseFinalRun.timing.medianMs / pintailRun.timing.medianMs
    results.push({
      name: query.name,
      mysqlMs,
      pintailMs: pintailRun.timing.medianMs,
      clickhouseMs: clickhouseRun.timing.medianMs,
      clickhouseFinalMs: clickhouseFinalRun.timing.medianMs,
      speedup,
      speedupVsClickhouse,
      coldOnly: query.coldOnly === true,
      timings: {
        pintail: pintailRun.timing,
        clickhouse: clickhouseRun.timing,
        clickhouseFinal: clickhouseFinalRun.timing,
        mysql: mysqlTiming,
      },
      resources,
      pintailMatchesMysql,
      clickhouseFinalMatchesMysql,
      pintailExplain,
    })
    if (!pintailMatchesMysql) log(`RESULT MISMATCH: pintail differs from MySQL on ${query.name}`)
    log(
      `MySQL ${mysqlMs} ms | Pintail ${pintailRun.timing.medianMs} ms | ` +
        `ClickHouse ${clickhouseRun.timing.medianMs} ms | ` +
        `CH RMT+FINAL ${clickhouseFinalRun.timing.medianMs} ms | ` +
        `${speedup.toFixed(1)}× vs MySQL | ${speedupVsClickhouse.toFixed(2)}× vs CH`,
    )
  }
  return results
}

function publishResults(
  allResults: QueryResult[],
  engineResults: QueryResult[] = [],
  concurrency: Array<{ query: string; pintail: ConcurrencyPoint; clickhouse: ConcurrencyPoint }> = [],
) {
  // Gate totals keep their original definition: repeat-query medians of
  // the canonical eight. Novel (cold) rows publish separately — they
  // measure the engine, not the memo.
  const results = allResults.filter((row) => !row.coldOnly)
  const novelResults = allResults.filter((row) => row.coldOnly)
  const totals = results.reduce(
    (total, row) => ({
      mysqlMs: total.mysqlMs + row.mysqlMs,
      pintailMs: total.pintailMs + row.pintailMs,
      clickhouseMs: total.clickhouseMs + row.clickhouseMs,
      clickhouseFinalMs: total.clickhouseFinalMs + row.clickhouseFinalMs,
    }),
    { mysqlMs: 0, pintailMs: 0, clickhouseMs: 0, clickhouseFinalMs: 0 },
  )
  const speedup = totals.mysqlMs / totals.pintailMs
  const suffix = fullGate ? '' : '-smoke'
  const generatedAt = new Date().toISOString()
  const mismatches = allResults.filter((row) => !row.pintailMatchesMysql).map((row) => row.name)
  const report = {
    generatedAt,
    scale,
    rows: { users: 100_000, products: 10_000, orders: orderRows },
    methodology: {
      pintailPlacement: containerizedPintail
        ? 'container on the docker host, --cpus=8 --memory=8g (same as MySQL/ClickHouse)'
        : 'LOCAL PROCESS — cross-host numbers, not comparable',
      iterations: baselineProvenance
        ? `warm: ${WARMUP_COUNT} warmup + ${RUN_COUNT} measured; cold: 5 distinct memo-cold variants; MySQL baseline reused from ${baselineProvenance}`
        : `warm: ${WARMUP_COUNT} warmup + ${RUN_COUNT} measured; cold: 5 distinct memo-cold variants`,
      /// Engine order is shuffled per query from this seed; same seed, same
      /// order, so two runs can be compared without wondering about it.
      engineOrderSeed: ENGINE_ORDER_SEED,
      hostFingerprint: hostFingerprint(),
      engineFingerprint,
      /// Reported per engine as median, mean, p95, stddev and a 95% interval,
      /// alongside every raw sample, so the summary can be checked.
      statistics: 'median, mean, p95, stddev, ci95, and all raw samples',
      references: {
        clickhouse: 'plain MergeTree (raw-speed ceiling)',
        clickhouseFinal: 'ReplacingMergeTree, final=1 (merge-on-read duty, query cache off)',
      },
      pintailSettledMemo:
        'bare full-table aggregates on a settled replica (empty memtable) are served ' +
        'from a manifest-generation-keyed exact result memo; any ingest invalidates it ' +
        'by construction. ClickHouse ships a query cache too, disabled by default and ' +
        'TTL-stale; pintail\'s is provably fresh, so it stays on.',
    },
    gate: {
      scope: 'memo-dashboard latency, not raw engine speed',
      requiredSpeedup: 50,
      enforced: fullGate,
      passed: speedup >= 50 && mismatches.length === 0,
      resultMismatches: mismatches,
    },
    queries: results,
    novelQueries: novelResults,
    /// The same canonical queries measured against a pintail whose result
    /// memo is off, so both engines execute. This is the engine-speed track;
    /// the `queries` table above is the cache-latency one.
    engineQueries: engineResults.filter((row) => !row.coldOnly),
    /// Throughput and p95 at each client count, both engines computing.
    concurrency,
    totals: {
      ...totals,
      speedup,
      speedupVsClickhouse: totals.clickhouseFinalMs / totals.pintailMs,
    },
  }
  const resultPath = join(benchmarkDir, `results${suffix}.json`)
  let previousReport: ComparableReport | undefined
  if (existsSync(resultPath)) {
    try {
      previousReport = JSON.parse(readFileSync(resultPath, 'utf8')) as ComparableReport
    } catch {
      previousReport = undefined
    }
  }
  const suspiciousRegressions = pintailSpecificMinimumRegressions(previousReport, report)
  if (suspiciousRegressions.length >= 2) {
    throw new Error(
      'refusing to bank a Pintail-specific minimum regression while the control is stable:\n' +
        suspiciousRegressions.join('\n'),
    )
  }
  writeFileSync(resultPath, `${JSON.stringify(report, null, 2)}\n`)
  const lines = [
    '# Pintail analytical benchmark results',
    '',
    `Measured ${generatedAt} with ${orderRows.toLocaleString()} orders.`,
    '',
    'All engines run on the docker host under identical limits (8 CPUs, 8 GB).',
    baselineProvenance
      ? `Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants. MySQL baseline measured ${baselineProvenance}.`
      : 'Canonical queries: 5 warm runs; ad-hoc queries: 5 distinct cold variants.',
    'CH RMT+FINAL = ReplacingMergeTree read with `final = 1` — ClickHouse doing',
    "pintail's always-correct merge-on-read duty.",
    '',
    'NOT like for like: the canonical table is served from pintail\'s settled',
    "aggregate memo, while ClickHouse's query cache is off and it executes every",
    'run. It measures what a repeated dashboard query costs, not engine speed.',
    'The novel-query table below is the engine-speed comparison - both engines',
    'execute there, and ClickHouse is currently faster.',
    '',
    '> Historical evidence warning: the 2026-08-11 run banked by `de974db` is',
    '> withdrawn. Pintail minima regressed on Q1/Q3 while unchanged MySQL and',
    '> ClickHouse controls did not, so the repository\'s host-noise rule did not apply.',
    '> The current artifact supersedes it; the harness now rejects that signature.',
    '',
    '## Repeated queries (memo-served — dashboard refresh cost, not engine speed)',
    '',
    '| Query | MySQL | Pintail (memo) | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |',
    '|---|---:|---:|---:|---:|---:|---:|:--|',
    ...results.map(
      (row) =>
        `| ${row.name} | ${row.mysqlMs.toLocaleString()} ms | ` +
        `${row.pintailMs.toLocaleString()} ms | ${row.speedup.toFixed(1)}× | ` +
        `${row.clickhouseMs.toLocaleString()} ms | ` +
        `${row.clickhouseFinalMs.toLocaleString()} ms | ` +
        `${row.speedupVsClickhouse.toFixed(2)}× | ` +
        `${row.pintailMatchesMysql ? 'yes' : 'MISMATCH'} |`,
    ),
    `| **Total** | **${totals.mysqlMs.toLocaleString()} ms** | ` +
      `**${totals.pintailMs.toLocaleString()} ms** | **${speedup.toFixed(1)}×** | ` +
      `**${totals.clickhouseMs.toLocaleString()} ms** | ` +
      `**${totals.clickhouseFinalMs.toLocaleString()} ms** | ` +
      `**${(totals.clickhouseFinalMs / totals.pintailMs).toFixed(2)}×** | |`,
    '',
    fullGate
      ? `Memo-dashboard release gate: ${speedup >= 50 && mismatches.length === 0 ? 'PASS' : 'FAIL'} (required ≥50× and exact results; not an engine-speed gate).`
      : 'Smoke scale only: the release speedup gate was not enforced.',
    '',
    ...(concurrency.length > 0
      ? [
          '## Concurrency (memo disabled — both engines executing)',
          '',
          'One client measures an engine at rest. This is the shape a server',
          'actually meets, and where admission, memory accounting and lock',
          'contention appear. Throughput and p95 together: throughput alone can',
          'rise while the slowest decile becomes unusable, and a flat p95 can',
          'hide an engine that has stopped accepting work.',
          '',
          '| Clients | Pintail /s | Pintail p95 | Pintail errors | CH /s | CH p95 | CH errors |',
          '|---:|---:|---:|---:|---:|---:|---:|',
          ...concurrency.map(
            (row) =>
              `| ${row.pintail.clients} | ${row.pintail.throughput} | ${row.pintail.p95Ms} ms | ` +
              `${row.pintail.errors} | ${row.clickhouse.throughput} | ${row.clickhouse.p95Ms} ms | ` +
              `${row.clickhouse.errors} |`,
          ),
          '',
        ]
      : []),
    ...(engineResults.length > 0
      ? [
          '## Engine speed (memo DISABLED — both engines execute)',
          '',
          'The canonical queries against a pintail restarted with its settled',
          'aggregate memo off, on the same replica. This is the like-for-like',
          'comparison: the table at the top measures a cache hit against',
          "ClickHouse's execution, which is a different question.",
          '',
          '| Query | MySQL | Pintail (no memo) | CH MergeTree | CH RMT+FINAL | vs CH |',
          '|---|---:|---:|---:|---:|---:|',
          ...engineResults
            .filter((row) => !row.coldOnly)
            .map(
              (row) =>
                `| ${row.name} | ${row.mysqlMs.toLocaleString()} ms | ` +
                `${row.pintailMs.toLocaleString()} ms | ` +
                `${row.clickhouseMs.toLocaleString()} ms | ` +
                `${row.clickhouseFinalMs.toLocaleString()} ms | ` +
                `${row.speedupVsClickhouse.toFixed(2)}× |`,
            ),
          '',
        ]
      : []),
    '## Novel queries (median of 5 memo-cold variants — RAW ENGINE SPEED)',
    '',
    'Both engines execute every run here. This is the comparison that speaks\n'
      + 'to execution performance.',
    '',
    'Each row is the median of five distinct predicate variants, each run once',
    'per engine with no warmup. Pintail therefore cannot replay an exact-result',
    'memo entry. Excluded from the release-gate totals.',
    '',
    '| Query | MySQL | Pintail | vs MySQL | CH MergeTree | CH RMT+FINAL | vs CH | Exact |',
    '|---|---:|---:|---:|---:|---:|---:|:--|',
    ...novelResults.map(
      (row) =>
        `| ${row.name} | ${row.mysqlMs.toLocaleString()} ms | ` +
        `${row.pintailMs.toLocaleString()} ms | ${row.speedup.toFixed(1)}× | ` +
        `${row.clickhouseMs.toLocaleString()} ms | ` +
        `${row.clickhouseFinalMs.toLocaleString()} ms | ` +
        `${row.speedupVsClickhouse.toFixed(2)}× | ` +
        `${row.pintailMatchesMysql ? 'yes' : 'MISMATCH'} |`,
    ),
    '',
    '## Resources during measured runs',
    '',
    'Peak container CPU (cumulative across 8 cores, so up to 800%) and peak',
    'memory, sampled via `docker stats` every 250 ms while each engine ran.',
    'MySQL shows n/a when its cold baseline came from the cache.',
    '',
    '| Query | Pintail CPU | Pintail mem | CH CPU | CH mem | MySQL CPU | MySQL mem |',
    '|---|---:|---:|---:|---:|---:|---:|',
    ...results.map((row) => {
      const cell = (resources?: EngineResources) =>
        resources && (resources.cpuPeakPct > 0 || resources.memPeakMb > 0)
          ? `${resources.cpuPeakPct}% | ${Math.round(resources.memPeakMb).toLocaleString()} MB`
          : 'n/a | n/a'
      return (
        `| ${row.name} | ${cell(row.resources.pintail)} | ` +
        `${cell(row.resources.clickhouse)} | ${cell(row.resources.mysql)} |`
      )
    }),
    '',
  ]
  writeFileSync(join(benchmarkDir, `results${suffix}.md`), `${lines.join('\n')}\n`)
  log(`aggregate speedup: ${speedup.toFixed(1)}×`)
  if (mismatches.length > 0) {
    throw new Error(`benchmark result mismatches vs MySQL: ${mismatches.join(', ')}`)
  }
  if (fullGate && speedup < 50) {
    throw new Error(`benchmark gate failed: ${speedup.toFixed(1)}× is below 50×`)
  }
}

async function cleanup() {
  // Engine logs outlive failures: a crashed pintail container's last lines
  // are the only evidence once cleanup removes it (run #10, socket-closed).
  try {
    const logs = await docker('logs', '--tail', '200', pintailName)
    const merged = [logs.stdout, logs.stderr].filter(Boolean).join('\n')
    if (merged.trim()) log(`pintail container tail:\n${merged}`)
  } catch {
    // Container never started or already gone.
  }
  if (mysqlConnection) {
    await mysqlConnection.end().catch(() => undefined)
    mysqlConnection = undefined
  }
  if (pintailProcess) {
    pintailProcess.kill('SIGTERM')
    const exited = await Promise.race([
      pintailProcess.exited.then(() => true),
      Bun.sleep(10_000).then(() => false),
    ])
    if (!exited) pintailProcess.kill('SIGKILL')
    pintailProcess = undefined
  }
  if (dockerCreated) {
    await docker('rm', '--force', '--volumes', mysqlName, clickhouseName, pintailName).catch(
      () => undefined,
    )
    await docker('network', 'rm', networkName).catch(() => undefined)
    if (runVolumeCreated) {
      await docker('volume', 'rm', runVolumeName).catch(() => undefined)
    }
    // Always: this one is created by the pintail container regardless of
    // whether the MySQL seed volume was.
    await docker('volume', 'rm', pintailVolumeName).catch(() => undefined)
  }
  rmSync(dataDir, { recursive: true, force: true })
}

async function main() {
  const info = await docker('info', '--format', '{{.Name}} {{.ServerVersion}} {{.OSType}}')
  log(`Docker: ${info.stdout}`)
  dockerHostName = info.stdout.split(' ')[0] || 'unknown'
  // The docker image builds from the WORKING TREE: concurrent edits change
  // what gets measured (or break the build mid-edit). Refuse dirty trees so
  // every measurement is attributable to a commit.
  const dirty = (await command(['git', 'status', '--porcelain'], { quiet: true })).stdout
  if (dirty.trim() && process.env.PINTAIL_BENCHMARK_ALLOW_DIRTY !== '1') {
    throw new Error(
      'working tree is dirty — commit first so the benchmark measures an attributable state, or set PINTAIL_BENCHMARK_ALLOW_DIRTY=1',
    )
  }
  const engineTrees = await command(
    ['git', 'rev-parse', 'HEAD:crates', 'HEAD:Cargo.toml', 'HEAD:Cargo.lock'],
    { quiet: true },
  )
  engineFingerprint = createHash('sha256').update(engineTrees.stdout).digest('hex')
  await docker('network', 'create', networkName)
  dockerCreated = true
  const haveSeedVolume = await volumeExists(seedVolumeName)
  if (haveSeedVolume) {
    // Copy the cached datadir into a per-run volume: the cache itself stays
    // read-only, so a killed run can never corrupt it.
    log(`restoring seeded MySQL datadir from volume ${seedVolumeName}`)
    await docker('volume', 'create', runVolumeName)
    runVolumeCreated = true
    await docker(
      'run',
      '--rm',
      '--volume',
      `${seedVolumeName}:/from:ro`,
      '--volume',
      `${runVolumeName}:/to`,
      'alpine:3',
      'sh',
      '-c',
      'cp -a /from/. /to/',
    )
  }
  await docker(
    'run',
    '--detach',
    '--name',
    mysqlName,
    '--network',
    networkName,
    '--publish',
    '0:3306',
    ...engineLimits,
    ...(haveSeedVolume ? ['--volume', `${runVolumeName}:/var/lib/mysql`] : []),
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    mysqlImage,
    ...mysqlServerArgs,
  )
  await docker(
    'run',
    '--detach',
    '--name',
    clickhouseName,
    '--network',
    networkName,
    '--publish',
    '0:8123',
    // ClickHouse has crashed mid-run under its memory limit three times in
    // one day; without a restart policy the container stays dead and every
    // retry is guaranteed ConnectionRefused. Restarted, its loaded tables
    // survive in the container filesystem and the run can continue.
    '--restart',
    'on-failure:3',
    ...engineLimits,
    '--env',
    'CLICKHOUSE_PASSWORD=pintail-benchmark',
    'clickhouse/clickhouse-server:25.8',
  )
  const host = await dockerHost()
  let mysqlPort = await publishedPort(mysqlName, 3306)
  const clickhousePort = await publishedPort(clickhouseName, 8123)
  const clickhouseUrl = `http://${host}:${clickhousePort}`
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await waitForClickhouse(clickhouseUrl)
  if (haveSeedVolume) {
    const [rows] = await mysqlConnection.query<mysql.RowDataPacket[]>(
      'SELECT COUNT(*) AS count FROM benchmark_db.orders',
    )
    if (Number(rows[0].count) !== orderRows) {
      throw new Error(
        `restored seed volume ${seedVolumeName} holds ${rows[0].count} orders, expected ${orderRows}; ` +
          'remove the volume to reseed',
      )
    }
  } else {
    await seedSource(mysqlConnection)
    // Snapshot the freshly seeded datadir for later runs: stop mysqld for a
    // consistent copy, capture its volume, and bring it back.
    log(`caching seeded datadir as volume ${seedVolumeName}`)
    // A freshly seeded InnoDB needs minutes to flush on shutdown; the
    // default 10s grace would SIGKILL it and taint the cached datadir.
    await docker('stop', '--timeout', '600', mysqlName)
    await docker('volume', 'create', seedVolumeName)
    await docker(
      'run',
      '--rm',
      '--volumes-from',
      mysqlName,
      '--volume',
      `${seedVolumeName}:/to`,
      'alpine:3',
      'sh',
      '-c',
      'cp -a /var/lib/mysql/. /to/',
    )
    await docker('start', mysqlName)
    // The ephemeral published port changes across restarts; re-resolve it.
    // Restart after a heavy seed can also replay redo for a while.
    mysqlPort = await publishedPort(mysqlName, 3306)
    mysqlConnection = await waitForMysql(host, mysqlPort, 1200)
  }
  mysqlEndpoint = { host, port: mysqlPort }
  // Neither the restored-volume connection nor the post-snapshot reconnect
  // has a default schema; the seed path only gets one via USE in seed.sql.
  await mysqlConnection.query('USE benchmark_db')
  await importClickhouse(clickhouseUrl)

  let pintailUrl: string
  let dsn: string
  if (containerizedPintail) {
    log('building the pintail image on the docker host (same host + limits as MySQL/ClickHouse)')
    await docker('build', '--tag', 'pintail-benchmark:latest', repository)
    await docker(
      'run',
      '--detach',
      '--name',
      pintailName,
      '--network',
      networkName,
      '--publish',
      '0:8080',
      // A NAMED volume, so the replica survives the container. The engine
      // track restarts pintail with its result memo disabled, and reattaching
      // the same replica turns that into a ~30s restart instead of another
      // full snapshot of every row.
      '--volume',
      `${pintailVolumeName}:/var/lib/pintail`,
      ...engineLimits,
      '--env',
      `PINTAIL_QUERY_MEMORY_LIMIT_BYTES=${4 * 1024 * 1024 * 1024}`,
      'pintail-benchmark:latest',
    )
    const pintailPort = await publishedPort(pintailName, 8080)
    pintailUrl = `http://${host}:${pintailPort}`
    dsn = `mysql://benchmark:benchmarkpass@${mysqlName}:3306/benchmark_db`
  } else {
    const binary = await buildPintail()
    const httpPort = await freePort()
    const wirePort = await freePort()
    pintailUrl = `http://127.0.0.1:${httpPort}`
    pintailProcess = Bun.spawn(
      [
        binary,
        '--data-dir',
        dataDir,
        '--http-bind',
        `127.0.0.1:${httpPort}`,
        '--wire-bind',
        `127.0.0.1:${wirePort}`,
      ],
      {
        cwd: repository,
        env: {
          ...process.env,
          PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(4 * 1024 * 1024 * 1024),
        },
        stdout: 'inherit',
        stderr: 'inherit',
      },
    )
    dsn = `mysql://benchmark:benchmarkpass@${host}:${mysqlPort}/benchmark_db`
  }
  await waitForHttp(pintailUrl)
  const setup = await api<{ token: string }>(pintailUrl, '/api/auth/setup', {
    method: 'POST',
    body: { email: 'benchmark@pintail.local', password: 'benchmark-release-gate' },
  })
  const databaseId = await createReplica(pintailUrl, setup.token, dsn)
  await verifyCounts(clickhouseUrl, pintailUrl, setup.token, databaseId)
  const results = await runQueries(
    mysqlConnection,
    clickhouseUrl,
    pintailUrl,
    setup.token,
    databaseId,
  )

  // The engine track. Everything above measures pintail with its settled
  // aggregate memo live, which for a repeated query reports the cost of a
  // cache hit - a real number, and not one that says anything about how fast
  // the engine computes. Restarting with the memo off makes the same queries
  // execute, against the same data, so the comparison is finally like for
  // like.
  //
  // Restarted rather than re-seeded: the replica sits on a named volume, so
  // this costs a container restart instead of another full snapshot.
  let engineResults: QueryResult[] = []
  const concurrency: Array<{
    query: string
    pintail: ConcurrencyPoint
    clickhouse: ConcurrencyPoint
  }> = []
  if (containerizedPintail) {
    log('restarting pintail with the result memo disabled (engine-speed track)')
    await docker('rm', '--force', pintailName).catch(() => undefined)
    await docker(
      'run',
      '--detach',
      '--name',
      pintailName,
      '--network',
      networkName,
      '--publish',
      '0:8080',
      '--volume',
      `${pintailVolumeName}:/var/lib/pintail`,
      ...engineLimits,
      '--env',
      `PINTAIL_QUERY_MEMORY_LIMIT_BYTES=${4 * 1024 * 1024 * 1024}`,
      '--env',
      'PINTAIL_DISABLE_SETTLED_MEMO=1',
      'pintail-benchmark:latest',
    )
    const enginePort = await publishedPort(pintailName, 8080)
    const engineUrl = `http://${host}:${enginePort}`
    await waitForHttp(engineUrl)
    engineResults = await runQueries(
      mysqlConnection,
      clickhouseUrl,
      engineUrl,
      setup.token,
      databaseId,
      { memoDisabled: true },
    )

    // Concurrency, on the same memo-disabled server so both engines are
    // computing rather than replaying. One client says nothing about the case
    // a server actually faces: two engines with the same median can diverge
    // completely here, one holding latency while adding throughput and the
    // other collapsing as its admission or memory accounting serialises.
    const concurrencyQuery =
      benchmarkQueries.find((query) => !query.coldOnly) ?? benchmarkQueries[0]
    for (const clients of CONCURRENCY_CLIENTS) {
      const pintailPoint = await measureConcurrency(
        () =>
          api<{ rows: unknown[][] }>(engineUrl, '/api/query', {
            method: 'POST',
            token: setup.token,
            body: { db: databaseId, sql: concurrencyQuery.sql },
          }),
        clients,
        CONCURRENCY_SECONDS,
      )
      const clickhousePoint = await measureConcurrency(
        async () => {
          const response = await fetch(`${clickhouseUrl}/?database=benchmark_rmt`, {
            method: 'POST',
            headers: clickhouseHeaders,
            body: `${concurrencyQuery.clickhouseSql ?? concurrencyQuery.sql} SETTINGS final = 1 FORMAT JSONCompact`,
          })
          if (!response.ok) throw new Error(await response.text())
          await response.text()
        },
        clients,
        CONCURRENCY_SECONDS,
      )
      concurrency.push({ query: concurrencyQuery.name, pintail: pintailPoint, clickhouse: clickhousePoint })
      log(
        `concurrency ${clients}: pintail ${pintailPoint.throughput}/s p95 ${pintailPoint.p95Ms}ms ` +
          `(${pintailPoint.errors} errors) | clickhouse ${clickhousePoint.throughput}/s ` +
          `p95 ${clickhousePoint.p95Ms}ms (${clickhousePoint.errors} errors)`,
      )
    }
  } else {
    log('SKIPPING the engine-speed track: it needs the containerized pintail')
  }

  publishResults(results, engineResults, concurrency)
}

try {
  await main()
} finally {
  await cleanup()
}

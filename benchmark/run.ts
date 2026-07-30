import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createServer } from 'node:net'
import mysql from 'mysql2/promise'
import { benchmarkQueries } from './queries'

type CommandResult = { stdout: string; stderr: string }
type QueryResult = {
  name: string
  mysqlMs: number
  pintailMs: number
  clickhouseMs: number
  speedup: number
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
const networkName = `${runId}-network`
const clickhouseHeaders = {
  Authorization: `Basic ${btoa('default:pintail-benchmark')}`,
}
const dataDir = mkdtempSync(join(tmpdir(), 'pintail-m9-benchmark-'))
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let mysqlConnection: mysql.Connection | undefined
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
  const context = (await docker('context', 'show')).stdout
  const endpoint = (
    await docker(
      'context',
      'inspect',
      context,
      '--format',
      '{{.Endpoints.docker.Host}}',
    )
  ).stdout
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

async function waitForMysql(host: string, port: number) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        multipleStatements: true,
        supportBigNumbers: true,
        bigNumberStrings: true,
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
    "GRANT SELECT, RELOAD, REPLICATION CLIENT ON *.* TO 'benchmark'@'%'",
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
  for (const [table, definition] of Object.entries(tables)) {
    await query(
      `CREATE TABLE benchmark.${table} (${definition.schema}) ` +
        'ENGINE = MergeTree ORDER BY id',
    )
    await query(
      `INSERT INTO benchmark.${table} SELECT ${definition.projection} ` +
        `FROM mysql('${mysqlName}:3306', 'benchmark_db', '${table}', ` +
        "'benchmark', 'benchmarkpass')",
    )
  }
}

async function createReplica(baseUrl: string, token: string, dsn: string): Promise<string> {
  const database = await api<{ id: string }>(baseUrl, '/api/databases', {
    method: 'POST',
    token,
    body: {
      name: 'benchmark_db',
      dsn,
      mode: 'polling',
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

async function verifyCounts(
  connection: mysql.Connection,
  clickhouseUrl: string,
  pintailUrl: string,
  token: string,
  databaseId: string,
) {
  const [mysqlRows] = await connection.query<mysql.RowDataPacket[]>(
    'SELECT COUNT(*) AS count FROM benchmark_db.orders',
  )
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
    Number(mysqlRows[0].count),
    Number(clickhouseRows.data[0][0]),
    Number(pintailRows.rows[0][0]),
  ]
  if (counts.some((count) => count !== orderRows)) {
    throw new Error(`row-count verification failed: MySQL/ClickHouse/Pintail=${counts.join('/')}`)
  }
  log(`all engines expose ${orderRows.toLocaleString()} orders`)
}

async function timed<T>(operation: () => Promise<T>): Promise<{ value: T; ms: number }> {
  const started = performance.now()
  const value = await operation()
  return { value, ms: Math.max(1, Math.round(performance.now() - started)) }
}

async function runQueries(
  connection: mysql.Connection,
  clickhouseUrl: string,
  pintailUrl: string,
  token: string,
  databaseId: string,
): Promise<QueryResult[]> {
  const results: QueryResult[] = []
  for (const query of benchmarkQueries) {
    log(query.name)
    const mysqlRun = await timed(() => connection.query(query.sql))
    const pintailRun = await timed(() =>
      api(pintailUrl, '/api/query', {
        method: 'POST',
        token,
        body: { db: databaseId, sql: query.sql },
      }),
    )
    const clickhouseRun = await timed(async () => {
      const response = await fetch(`${clickhouseUrl}/?database=benchmark`, {
        method: 'POST',
        headers: clickhouseHeaders,
        body: `${query.clickhouseSql ?? query.sql} FORMAT JSONCompact`,
      })
      const text = await response.text()
      if (!response.ok) throw new Error(`ClickHouse query failed: ${text}`)
      return text
    })
    const speedup = mysqlRun.ms / pintailRun.ms
    results.push({
      name: query.name,
      mysqlMs: mysqlRun.ms,
      pintailMs: pintailRun.ms,
      clickhouseMs: clickhouseRun.ms,
      speedup,
    })
    log(
      `MySQL ${mysqlRun.ms} ms | Pintail ${pintailRun.ms} ms | ` +
        `ClickHouse ${clickhouseRun.ms} ms | ${speedup.toFixed(1)}×`,
    )
  }
  return results
}

function publishResults(results: QueryResult[]) {
  const totals = results.reduce(
    (total, row) => ({
      mysqlMs: total.mysqlMs + row.mysqlMs,
      pintailMs: total.pintailMs + row.pintailMs,
      clickhouseMs: total.clickhouseMs + row.clickhouseMs,
    }),
    { mysqlMs: 0, pintailMs: 0, clickhouseMs: 0 },
  )
  const speedup = totals.mysqlMs / totals.pintailMs
  const suffix = fullGate ? '' : '-smoke'
  const generatedAt = new Date().toISOString()
  const report = {
    generatedAt,
    scale,
    rows: { users: 100_000, products: 10_000, orders: orderRows },
    gate: { requiredSpeedup: 50, enforced: fullGate, passed: speedup >= 50 },
    queries: results,
    totals: { ...totals, speedup },
  }
  writeFileSync(
    join(benchmarkDir, `results${suffix}.json`),
    `${JSON.stringify(report, null, 2)}\n`,
  )
  const lines = [
    '# Pintail analytical benchmark results',
    '',
    `Measured ${generatedAt} with ${orderRows.toLocaleString()} orders.`,
    '',
    '| Query | MySQL | Pintail | Speedup | ClickHouse reference |',
    '|---|---:|---:|---:|---:|',
    ...results.map(
      (row) =>
        `| ${row.name} | ${row.mysqlMs.toLocaleString()} ms | ` +
        `${row.pintailMs.toLocaleString()} ms | ${row.speedup.toFixed(1)}× | ` +
        `${row.clickhouseMs.toLocaleString()} ms |`,
    ),
    `| **Total** | **${totals.mysqlMs.toLocaleString()} ms** | ` +
      `**${totals.pintailMs.toLocaleString()} ms** | **${speedup.toFixed(1)}×** | ` +
      `**${totals.clickhouseMs.toLocaleString()} ms** |`,
    '',
    fullGate
      ? `Release gate: ${speedup >= 50 ? 'PASS' : 'FAIL'} (required ≥50×).`
      : 'Smoke scale only: the release speedup gate was not enforced.',
    '',
  ]
  writeFileSync(join(benchmarkDir, `results${suffix}.md`), `${lines.join('\n')}\n`)
  log(`aggregate speedup: ${speedup.toFixed(1)}×`)
  if (fullGate && speedup < 50) {
    throw new Error(`benchmark gate failed: ${speedup.toFixed(1)}× is below 50×`)
  }
}

async function cleanup() {
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
    await docker('rm', '--force', '--volumes', mysqlName, clickhouseName).catch(() => undefined)
    await docker('network', 'rm', networkName).catch(() => undefined)
  }
  rmSync(dataDir, { recursive: true, force: true })
}

async function main() {
  const info = await docker('info', '--format', '{{.Name}} {{.ServerVersion}} {{.OSType}}')
  log(`Docker: ${info.stdout}`)
  await docker('network', 'create', networkName)
  dockerCreated = true
  await docker(
    'run',
    '--detach',
    '--name',
    mysqlName,
    '--network',
    networkName,
    '--publish',
    '0:3306',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    'mysql:8.4',
    '--skip-log-bin',
    '--default-time-zone=+00:00',
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
    '--innodb-buffer-pool-size=1G',
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
    '--env',
    'CLICKHOUSE_PASSWORD=pintail-benchmark',
    'clickhouse/clickhouse-server:25.8',
  )
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  const clickhousePort = await publishedPort(clickhouseName, 8123)
  const clickhouseUrl = `http://${host}:${clickhousePort}`
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await waitForClickhouse(clickhouseUrl)
  await seedSource(mysqlConnection)
  await importClickhouse(clickhouseUrl)

  const binary = await buildPintail()
  const httpPort = await freePort()
  const wirePort = await freePort()
  const pintailUrl = `http://127.0.0.1:${httpPort}`
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
        PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(256 * 1024 * 1024),
      },
      stdout: 'inherit',
      stderr: 'inherit',
    },
  )
  await waitForHttp(pintailUrl)
  const setup = await api<{ token: string }>(pintailUrl, '/api/auth/setup', {
    method: 'POST',
    body: { email: 'benchmark@pintail.local', password: 'benchmark-release-gate' },
  })
  const dsn = `mysql://benchmark:benchmarkpass@${host}:${mysqlPort}/benchmark_db`
  const databaseId = await createReplica(pintailUrl, setup.token, dsn)
  await verifyCounts(mysqlConnection, clickhouseUrl, pintailUrl, setup.token, databaseId)
  const results = await runQueries(
    mysqlConnection,
    clickhouseUrl,
    pintailUrl,
    setup.token,
    databaseId,
  )
  publishResults(results)
}

try {
  await main()
} finally {
  await cleanup()
}

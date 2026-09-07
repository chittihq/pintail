import { copyFile, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql, { type Connection } from 'mysql2/promise'

const repository = resolve(import.meta.dir, '../../..')
const runId = `pintail-bi-${process.pid}-${Date.now().toString(36)}`
const network = `${runId}-network`
const source = `${runId}-mysql`
const engine = `${runId}-engine`
const metabase = `${runId}-metabase`
const image = `${runId}:test`
const metabaseImage = 'metabase/metabase:v0.63.16'
const containers: string[] = []
const checks: string[] = []
let connection: Connection | undefined
let context: string | undefined
let networkCreated = false
let imageCreated = false
const started = new Date().toISOString()

async function command(args: string[], cwd = repository): Promise<string> {
  const child = Bun.spawn(args, { cwd, stdout: 'pipe', stderr: 'pipe', env: process.env })
  const [stdout, stderr, code] = await Promise.all([
    new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
  ])
  if (code !== 0) throw new Error(`${args[0]} ${args[1]} failed (${code}): ${stderr}\n${stdout}`)
  return stdout.trim()
}
const docker = (...args: string[]) => command(['docker', ...args])
const mark = (name: string) => { checks.push(name); console.log(`PASS ${name}`) }
function require(value: unknown, message: string): asserts value {
  if (!value) throw new Error(message)
}
async function until<T>(label: string, probe: () => Promise<T | undefined>, seconds = 180): Promise<T> {
  const deadline = Date.now() + seconds * 1000
  let last: unknown
  while (Date.now() < deadline) {
    try { const result = await probe(); if (result !== undefined) return result } catch (error) { last = error }
    await Bun.sleep(1000)
  }
  throw new Error(`${label} timed out: ${last ?? 'not ready'}`)
}
async function dockerHost(): Promise<string> {
  const endpoint = process.env.DOCKER_HOST || await docker('context', 'inspect', '--format', '{{.Endpoints.docker.Host}}')
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = new URL(endpoint).hostname
  const config = await command(['ssh', '-G', target])
  const host = config.split('\n').find(line => line.startsWith('hostname '))?.slice(9)
  require(host, 'Docker SSH hostname missing')
  return host.includes(':') ? `[${host}]` : host
}
async function port(name: string, number: number) {
  const value = await docker('port', name, `${number}/tcp`)
  const match = value.split('\n')[0]?.match(/:(\d+)$/)
  require(match, 'published port missing')
  return Number(match[1])
}
async function start(name: string, args: string[]) {
  await docker('run', '--detach', '--name', name, '--network', network, ...args)
  containers.push(name)
}
async function api(base: string, path: string, body?: unknown, headers: Record<string, string> = {}): Promise<any> {
  const response = await fetch(base + path, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'Content-Type': 'application/json', ...headers },
    body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(60_000),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${path}: ${response.status} ${text}`)
  return text ? JSON.parse(text) : undefined
}

async function main() {
  console.log('Building the test binary on the build host')
  const binary = process.env.PINTAIL_BI_BINARY ?? join(repository, 'target/recovery/pintail')
  if (!process.env.PINTAIL_BI_BINARY) {
    await command([process.env.CARGO ?? 'cargo', 'build', '--profile', 'recovery', '-p', 'pintail'])
  }
  context = await mkdtemp(join(tmpdir(), 'pintail-bi-image-'))
  await copyFile(binary, join(context, 'pintail'))
  await copyFile(join(import.meta.dir, 'Dockerfile'), join(context, 'Dockerfile'))
  await command(['strip', '--strip-debug', join(context, 'pintail')])
  console.log('Packaging the test binary and starting isolated containers')
  await docker('build', '--tag', image, context)
  imageCreated = true
  await docker('network', 'create', network)
  networkCreated = true
  const host = await dockerHost()
  await start(source, ['--publish', '0:3306', '--tmpfs', '/var/lib/mysql:rw,size=512m',
    '--env', 'MYSQL_ROOT_PASSWORD=bi-test-password', '--env', 'MYSQL_ROOT_HOST=%',
    'mysql:8.4', '--server-id=947', '--log-bin=mysql-bin', '--binlog-format=ROW',
    '--binlog-row-image=FULL', '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION'])
  const sourcePort = await port(source, 3306)
  connection = await until('source', async () => {
    const client = await mysql.createConnection({ host, port: sourcePort, user: 'root', password: 'bi-test-password', multipleStatements: true })
    await client.query('SELECT 1')
    return client
  })
  await connection.query(`CREATE DATABASE bi_test; USE bi_test;
    CREATE TABLE events (id BIGINT UNSIGNED PRIMARY KEY, occurred_at DATETIME(6) NOT NULL, score INT NOT NULL);
    INSERT INTO events VALUES (1, '2024-01-02 03:04:05.123456', 10), (2, '2024-01-18 12:00:00', 20), (3, '2024-02-04 09:00:00', 30);`)
  await start(engine, ['--publish', '0:8080', '--publish', '0:3306', '--tmpfs', '/data:rw,size=512m',
    '--env', 'PINTAIL_TELEMETRY=off', image])
  const base = `http://${host}:${await port(engine, 8080)}`
  await until('engine', async () => (await fetch(`${base}/health`)).ok ? true : undefined)
  const setup = await api(base, '/api/auth/setup', { email: 'bi-test@example.invalid', password: 'bi-test-password-long' })
  const auth = { Authorization: `Bearer ${setup.token}` }
  const database = await api(base, '/api/databases', { name: 'bi_test', dsn: `mysql://root:bi-test-password@${source}:3306/bi_test`, mode: 'cdc' }, auth)
  await api(base, `/api/databases/${database.id}/probe`, undefined, auth)
  await api(base, `/api/databases/${database.id}/snapshot`, { force: false }, auth)
  await until('snapshot', async () => {
    const status = await api(base, `/api/databases/${database.id}/snapshot/status`, undefined, auth)
    return ['streaming', 'polling'].includes(status.state) && status.tables.some((t: any) => t.name === 'events' && t.rows === 3) ? true : undefined
  })
  const key = await api(base, `/api/databases/${database.id}/api-keys`, { name: 'bi-test' }, auth)
  mark('replica ready')
  await start(metabase, ['--publish', '0:3000', '--memory', '2g',
    '--env', 'MB_ANON_TRACKING_ENABLED=false', '--env', 'MB_CHECK_FOR_UPDATES=false',
    '--env', 'MB_SITE_URL=http://localhost:3000', '--env', 'JAVA_TIMEZONE=UTC', metabaseImage])
  const mb = `http://${host}:${await port(metabase, 3000)}`
  await until('Metabase startup', async () => (await fetch(`${mb}/api/health`)).ok ? true : undefined, 300)
  const properties = await api(mb, '/api/session/properties')
  const session = await api(mb, '/api/setup', {
    token: properties['setup-token'], user: { email: 'bi-test@example.invalid', first_name: 'BI', last_name: 'Test', password: 'bi-test-password-long1!' },
    prefs: { site_name: 'Pintail BI test', allow_tracking: false },
  })
  const mbAuth = { 'X-Metabase-Session': session.id }
  const attached = await api(mb, '/api/database', { name: 'Pintail', engine: 'mysql',
    details: { host: engine, port: 3306, dbname: 'bi_test', user: 'bi_test', password: key.secret, ssl: false },
    is_full_sync: true, is_on_demand: false }, mbAuth)
  await api(mb, `/api/database/${attached.id}/sync_schema`, {}, mbAuth)
  let discovery: unknown
  const table = await until('Metabase schema discovery', async () => {
    const metadata = await api(mb, `/api/database/${attached.id}/metadata`, undefined, mbAuth)
    discovery = metadata.tables?.map((t: any) => ({ name: t.name, fields: t.fields?.map((f: any) => f.name) }))
    const found = metadata.tables?.find((t: any) => t.name === 'events' && t.fields?.length === 3)
    if (!found) console.log(`Schema discovery: ${JSON.stringify(discovery)}`)
    return found
  })
  const date = table.fields.find((f: any) => f.name === 'occurred_at')
  const score = table.fields.find((f: any) => f.name === 'score')
  require(date && score, 'Metabase did not discover the temporal and numeric columns')
  mark('Metabase MySQL schema sync')
  async function saved(name: string, query: unknown, expected: unknown[][]) {
    const card = await api(mb, '/api/card', { name, display: 'table', visualization_settings: {},
      dataset_query: { database: attached.id, type: 'query', query } }, mbAuth)
    const result = await api(mb, `/api/card/${card.id}/query`, {}, mbAuth)
    require(result.status === 'completed', `${name}: ${JSON.stringify(result.error ?? result)}`)
    const rows = result.data.rows.map((row: any[]) => row.map(value => typeof value === 'string' && /^2024-\d\d-\d\dT/.test(value) ? value.slice(0, 10) : value))
    require(JSON.stringify(rows) === JSON.stringify(expected), `${name}: ${JSON.stringify(rows)}`)
    mark(name)
  }
  await saved('Metabase saved monthly question', { 'source-table': table.id, aggregation: [['count']],
    breakout: [['field', date.id, { 'temporal-unit': 'month' }]],
    'order-by': [['asc', ['field', date.id, { 'temporal-unit': 'month' }]]] }, [['2024-01-01', 2], ['2024-02-01', 1]])
  await saved('Metabase saved filtered question', { 'source-table': table.id, aggregation: [['count']],
    filter: ['>', ['field', score.id, null], 15] }, [[2]])
}

let failure: string | undefined
try { await main() } catch (error) {
  failure = error instanceof Error ? error.message : String(error)
  console.error(failure)
  for (const name of containers) console.error(await docker('logs', '--tail', '100', name).catch(() => 'logs unavailable'))
} finally {
  await connection?.end().catch(() => undefined)
  for (const name of containers.reverse()) await docker('rm', '--force', '--volumes', name).catch(() => undefined)
  if (networkCreated) await docker('network', 'rm', network).catch(() => undefined)
  if (imageCreated) await docker('image', 'rm', image).catch(() => undefined)
  if (context) await rm(context, { recursive: true, force: true })
  await Bun.write(join(import.meta.dir, 'results.json'), JSON.stringify({ started, finished: new Date().toISOString(), metabaseImage, checks, status: failure ? 'FAIL' : 'PASS', failure }, null, 2) + '\n')
}
console.log(`BI-CLIENTS-${failure ? 'FAIL' : 'DONE'}`)
process.exit(failure ? 1 : 0)

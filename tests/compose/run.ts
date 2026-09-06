#!/usr/bin/env bun
// The compose gate: one functional check run INSIDE the shipped
// docker-compose.yml, on the shared Docker host.
//
// Every other gate launches the bare binary on the machine running the
// harness, so the file that defines a deployment's resource envelope - its
// descriptor limit, its memory limit, which environment variables reach the
// process - was never under test. Two production failures came from exactly
// that gap: a container running with Docker's 1024-descriptor default, and a
// concurrency knob the compose file silently dropped. This gate builds the
// image from the working tree, brings the stack up through the compose file
// with a MySQL source beside it, snapshots a table wide enough to spill under
// a tight query ceiling, and proves three things through the container:
//
//   1. the startup limits line reports the compose file's descriptor limit
//      and the concurrency the environment asked for;
//   2. a grouped aggregation that spills completes inside the container and
//      answers byte-for-byte what MySQL answers;
//   3. the spill held a bounded number of open files while it ran.
//
// Cleanup removes only what this gate created: its compose project, its
// volumes, its MySQL container and its image tag.

import { resolve } from 'node:path'
import mysql, { type Connection } from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const composeFile = resolve(repository, 'docker-compose.yml')
const PROJECT = 'pintail-compose-gate'
const MYSQL_IMAGE = process.env.PINTAIL_COMPOSE_MYSQL_IMAGE ?? 'mysql:8.4'
const MYSQL_NAME = `${PROJECT}-mysql`
const DATABASE = 'compose_gate'
const ROWS = 300_000
/// Tight enough that the grouped aggregation below has to spill its map;
/// the scan batch of this table is a few MiB, so the query still runs.
const QUERY_MEMORY = 64 * 1024 * 1024
const TOTAL_MEMORY = 256 * 1024 * 1024
const CONCURRENT_QUERIES = 3
/// What docker-compose.yml sets; the limits line must report it.
const OPEN_FILES = '1048576/1048576'

const log = (message: string) => console.log(`[compose-gate ${new Date().toISOString()}] ${message}`)

async function command(args: string[], options: { env?: Record<string, string>; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: repository,
    stdout: 'pipe',
    stderr: 'pipe',
    env: { ...process.env, ...(options.env ?? {}) },
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

const docker = (...args: string[]) => command(['docker', ...args], { quiet: true })

function composeEnv(imageTag: string): Record<string, string> {
  return {
    PINTAIL_VERSION: imageTag,
    // Ephemeral host ports, so the gate never collides with a deployment.
    PINTAIL_HTTP_PORT: '0',
    PINTAIL_WIRE_PORT: '0',
    PINTAIL_MAX_CONCURRENT_QUERIES: String(CONCURRENT_QUERIES),
    PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(QUERY_MEMORY),
    PINTAIL_TOTAL_QUERY_MEMORY_LIMIT_BYTES: String(TOTAL_MEMORY),
    PINTAIL_MEMORY_LIMIT: '1G',
  }
}

const compose = (imageTag: string, ...args: string[]) =>
  command(['docker', 'compose', '--file', composeFile, '--project-name', PROJECT, ...args], {
    env: composeEnv(imageTag),
    quiet: true,
  })

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')).stdout
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const ssh = await command(['ssh', '-G', target], { quiet: true })
  const hostname = ssh.stdout
    .split('\n')
    .find((line) => line.startsWith('hostname '))
    ?.slice('hostname '.length)
  if (!hostname) throw new Error(`could not resolve Docker SSH target ${target}`)
  return hostname
}

function urlHost(host: string): string {
  return host.includes(':') ? `[${host}]` : host
}

async function publishedPort(container: string, port: number): Promise<number> {
  const output = (await docker('port', container, `${port}/tcp`)).stdout
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`Docker did not publish ${container}:${port}`)
  return Number(match[1])
}

async function waitForMysql(host: string, port: number, user: string, password: string, database?: string) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user,
        password,
        database,
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
  throw new Error(`${host}:${port} did not answer as MySQL in time`)
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
  throw new Error('the composed pintail did not answer /health within 120 seconds')
}

/// One table shaped to spill a grouped aggregation at the gate's ceiling:
/// sixty thousand groups, a distinct text key per group, and a datetime.
async function seed(connection: Connection) {
  await connection.query(`CREATE DATABASE IF NOT EXISTS ${DATABASE}`)
  await connection.query(`USE ${DATABASE}`)
  await connection.query(`
    CREATE TABLE events (
      id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
      grp INT NOT NULL,
      tag VARCHAR(16) NOT NULL,
      score INT NOT NULL,
      label VARCHAR(48) NOT NULL,
      created_at DATETIME NOT NULL
    )`)
  const batch = 5_000
  for (let start = 1; start <= ROWS; start += batch) {
    const values: string[] = []
    for (let id = start; id < start + batch && id <= ROWS; id += 1) {
      const grp = id % 60_000
      const day = 1 + (id % 28)
      const month = 1 + (id % 12)
      values.push(
        `(${id}, ${grp}, 't${(id / 60_000) | 0}', ${(id % 1_000) - 500}, 'label-${String((id * 31) % ROWS).padStart(7, '0')}', '2026-${String(month).padStart(2, '0')}-${String(day).padStart(2, '0')} ${String(id % 24).padStart(2, '0')}:00:00')`,
      )
    }
    await connection.query(`INSERT INTO events VALUES ${values.join(',')}`)
  }
  await connection.query(`CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await connection.query(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )
}

/// Sixty thousand groups with a distinct set each, which is what spills at
/// the gate's ceiling; folded to one row on the outside because the wire
/// endpoint caps a result at ten thousand rows, so the whole grouped result
/// is checked through its totals and a five-thousand-group slice is checked
/// row by row.
const GROUPED = `SELECT COUNT(*), SUM(n), SUM(s), SUM(d), MIN(first_seen), MAX(last_tag) FROM (
    SELECT grp, COUNT(*) AS n, SUM(score) AS s, COUNT(DISTINCT label) AS d,
           MIN(created_at) AS first_seen, MAX(tag) AS last_tag
    FROM events WHERE DATE(created_at) >= '2026-01-01' GROUP BY grp
  ) t`
const GROUPED_SLICE = `SELECT grp, COUNT(*), SUM(score), COUNT(DISTINCT label), MIN(created_at), MAX(tag)
  FROM events WHERE DATE(created_at) >= '2026-01-01' AND grp < 5000 GROUP BY grp ORDER BY grp`

function canonical(rows: unknown[][]): string[] {
  return rows.map((row) => row.map((value) => (value === null ? 'NULL' : String(value))).join('\t'))
}

async function createReplica(baseUrl: string, token: string, dsn: string): Promise<string> {
  const database = await api<{ id: string }>(baseUrl, '/api/databases', {
    method: 'POST',
    token,
    body: { name: DATABASE, dsn, mode: 'cdc', include_tables: ['events'] },
  })
  await api(baseUrl, `/api/databases/${database.id}/probe`, { token })
  await api(baseUrl, `/api/databases/${database.id}/snapshot`, {
    method: 'POST',
    token,
    body: { force: false },
  })
  for (let attempt = 0; attempt < 1_800; attempt += 1) {
    const status = await api<{
      state: string
      tables: Array<{ name: string; rows: number; last_error?: string }>
    }>(baseUrl, `/api/databases/${database.id}/snapshot/status`, { token })
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables.map((table) => table.last_error).filter(Boolean).join('; ')}`,
      )
    }
    const events = status.tables.find((table) => table.name === 'events')?.rows ?? 0
    if ((status.state === 'polling' || status.state === 'streaming') && events === ROWS) {
      return database.id
    }
    if (attempt % 30 === 0) log(`snapshot progress: ${events.toLocaleString()} / ${ROWS.toLocaleString()} rows`)
    await Bun.sleep(1000)
  }
  throw new Error('snapshot did not complete within thirty minutes')
}

let imageTag = ''
let mysqlStarted = false
let stackStarted = false
let mysqlConnection: Connection | undefined
let pintailConnection: Connection | undefined

async function main() {
  const sha = (await command(['git', 'rev-parse', '--short=12', 'HEAD'], { quiet: true })).stdout
  imageTag = `gate-${sha}`
  const image = `ghcr.io/chittihq/pintail:${imageTag}`
  const host = await dockerHost()

  log('the compose file must parse before anything is built on its account')
  await compose(imageTag, 'config', '--quiet')

  log(`building ${image} on the docker host from the working tree`)
  await docker('build', '--tag', image, repository)

  log(`starting the MySQL source ${MYSQL_NAME}`)
  await docker('rm', '-f', MYSQL_NAME).catch(() => undefined)
  await docker(
    'run', '--detach', '--name', MYSQL_NAME, '--publish', '0:3306',
    '--tmpfs', '/var/lib/mysql:rw,size=2g',
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', '--env', `MYSQL_DATABASE=${DATABASE}`,
    MYSQL_IMAGE,
    '--server-id=943', '--log-bin=mysql-bin', '--binlog-format=ROW', '--binlog-row-image=FULL',
    '--binlog-row-metadata=MINIMAL', '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(MYSQL_NAME, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort, 'root', 'pintail-root')
  log(`seeding ${ROWS.toLocaleString()} rows`)
  await seed(mysqlConnection)

  log('bringing the stack up through docker-compose.yml')
  await compose(imageTag, 'down', '--volumes', '--remove-orphans').catch(() => undefined)
  await compose(imageTag, 'up', '--detach', '--wait', '--wait-timeout', '180')
  stackStarted = true
  const container = (await compose(imageTag, 'ps', '--quiet', 'pintail')).stdout.split('\n')[0]
  if (!container) throw new Error('compose reported no pintail container')
  // The source joins the stack's network so the DSN can name it directly.
  const networks = (
    await docker('inspect', '--format', '{{range $name, $_ := .NetworkSettings.Networks}}{{$name}} {{end}}', container)
  ).stdout.trim().split(/\s+/)
  const network = networks[0]
  if (!network) throw new Error('the composed pintail container is on no network')
  await docker('network', 'connect', network, MYSQL_NAME)

  const httpPort = await publishedPort(container, 8080)
  const wirePort = await publishedPort(container, 3306)
  const baseUrl = `http://${urlHost(host)}:${httpPort}`
  await waitForHttp(baseUrl)

  log('checking the limits the process resolved inside the container')
  const logs = (await compose(imageTag, 'logs', '--no-log-prefix', 'pintail')).stdout
  const limits = logs.split('\n').find((line) => line.includes('pintail limits:'))
  if (!limits) throw new Error(`no limits line in the container log:\n${logs.slice(-2000)}`)
  log(limits.slice(limits.indexOf('pintail limits:')))
  const expectations = [
    `concurrent_queries=${CONCURRENT_QUERIES}`,
    `open_files=${OPEN_FILES}`,
    'query_memory=0.06GiB',
    'shared_memory=0.25GiB',
  ]
  const missing = expectations.filter((expected) => !limits.includes(expected))
  if (missing.length > 0) {
    throw new Error(`the limits line lacks ${missing.join(', ')}: the compose file dropped a setting`)
  }

  log('snapshotting the source through the container')
  const setup = await api<{ token: string }>(baseUrl, '/api/auth/setup', {
    method: 'POST',
    body: { email: 'compose-gate@pintail.local', password: 'compose-gate-password' },
  })
  const dsn = `mysql://pintail:pintail@${MYSQL_NAME}:3306/${DATABASE}`
  const databaseId = await createReplica(baseUrl, setup.token, dsn)

  log('running a grouped aggregation that spills, through the wire port')
  // A wire login is a per-database API key, the way a BI tool connects.
  const apiKey = await api<{ secret: string }>(baseUrl, `/api/databases/${databaseId}/api-keys`, {
    method: 'POST',
    token: setup.token,
    body: { name: 'compose-gate', scopes: ['query', 'read'] },
  })
  pintailConnection = await waitForMysql(host, wirePort, DATABASE, apiKey.secret, DATABASE)
  // EXPLAIN ANALYZE first: a settled snapshot memoizes a grouped result, so
  // the run after the first is a replay that spills nothing.
  const [explain] = (await pintailConnection.query({ sql: `EXPLAIN ANALYZE ${GROUPED}`, rowsAsArray: true })) as unknown as [
    unknown[][],
  ]
  const plan = explain.map((row) => row.join(' ')).join('\n')
  const spill = plan.match(/Spill files=(\d+) bytes=(\d+) active_bytes=(\d+) quota_failures=(\d+) peak_handles=(\d+)/)
  if (!spill) throw new Error(`EXPLAIN ANALYZE reported no spill line:\n${plan}`)
  const files = Number(spill[1])
  const peak = Number(spill[5])
  log(`spill: ${files} files, peak ${peak} open at once`)
  if (files === 0) throw new Error('the ceiling was meant to force a spill and did not')
  if (peak > 17) throw new Error(`the spill held ${peak} files open at once; the bound is 17`)
  await mysqlConnection.query(`USE ${DATABASE}`)
  for (const [label, sql] of [
    ['grouped totals', GROUPED],
    ['grouped slice', GROUPED_SLICE],
  ] as const) {
    const [mysqlRows] = (await mysqlConnection.query({ sql, rowsAsArray: true })) as unknown as [unknown[][]]
    const [pintailArray] = (await pintailConnection.query({ sql, rowsAsArray: true })) as unknown as [
      unknown[][],
    ]
    const ours = canonical(pintailArray)
    const theirs = canonical(mysqlRows)
    if (ours.length !== theirs.length) {
      throw new Error(`${label}: row count differs: pintail ${ours.length}, mysql ${theirs.length}`)
    }
    for (let index = 0; index < ours.length; index += 1) {
      if (ours[index] !== theirs[index]) {
        throw new Error(`${label}: row ${index} differs:\n  pintail ${ours[index]}\n  mysql   ${theirs[index]}`)
      }
    }
    log(`${label}: ${ours.length.toLocaleString()} rows match MySQL byte for byte`)
  }

  log('PASS')
}

async function cleanup() {
  await pintailConnection?.end().catch(() => undefined)
  await mysqlConnection?.end().catch(() => undefined)
  if (stackStarted) await compose(imageTag, 'down', '--volumes', '--remove-orphans').catch(() => undefined)
  if (mysqlStarted) await docker('rm', '-f', MYSQL_NAME).catch(() => undefined)
  if (imageTag) await docker('image', 'rm', `ghcr.io/chittihq/pintail:${imageTag}`).catch(() => undefined)
}

try {
  await main()
} catch (error) {
  log(`FAIL: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await cleanup()
}

/// Runs the TPC-H workload against MySQL and pintail and compares the answers.
///
/// Separate from `run-production.ts` rather than folded into it. That runner is
/// the release gate's `accept` stage, and its query parameterisation is built
/// around the commerce workload's sampling kinds - Zipf tenants, Zipf
/// customers, relative dates - none of which TPC-H has: the specification
/// fixes its substitution parameters, so the whole mechanism reduces to
/// literal replacement here. Extracting a shared harness is worth doing; doing
/// it inside the file the gate runs, to serve a workload that had never
/// executed once, is not the order to do it in.
///
/// What the two share is the shape of the setup, and that shape is duplicated
/// here deliberately and visibly: same MySQL flags, same snapshot wait, same
/// exact-result comparison.

import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { seedTpch } from './workloads/tpch-v1/seed'
import manifest from './workloads/tpch-v1/workload'
import type { TpchQuerySpec } from './workloads/tpch-v1/workload'

function arg(name: string, fallback: string): string {
  const index = process.argv.indexOf(`--${name}`)
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback
}

const profileName = arg('profile', 'smoke') as keyof typeof manifest.profiles
const benchmarkDir = import.meta.dir
const workloadDir = join(benchmarkDir, 'workloads', 'tpch-v1')
const repository = resolve(benchmarkDir, '..')
const runId = `pintail-tpch-${process.pid}-${Date.now()}`
const scale = manifest.profiles[profileName]?.scale
if (scale === undefined) throw new Error(`unknown profile ${profileName}`)

const log = (m: string) => console.log(`[tpch] ${m}`)

async function command(args: string[]) {
  const child = Bun.spawn(args, { cwd: repository, stdout: 'pipe', stderr: 'pipe' })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) throw new Error(`${args.join(' ')} failed (${status}): ${stderr.trim()}`)
  return stdout.trim()
}

const docker = (...args: string[]) => command(['docker', ...args])

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = await docker('context', 'show')
    endpoint = await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = endpoint.slice('ssh://'.length).split('@').at(-1)!.split(':')[0]
  const ssh = await command(['ssh', '-G', target])
  const hostname = ssh.split('\n').find((l) => l.startsWith('hostname '))?.slice(9)
  if (!hostname) throw new Error(`cannot resolve docker ssh host ${target}`)
  return hostname
}

async function publishedPort(name: string, port: number): Promise<number> {
  const output = await docker('port', name, `${port}/tcp`)
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`no published port for ${name}:${port}`)
  return Number(match[1])
}

const mysqlName = `${runId}-mysql`
let mysqlConn: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailUrl = ''
let pintailToken = ''
let pintailDb = ''
let pintailDataDir = ''

async function startMysql(): Promise<{ host: string; port: number }> {
  log('starting MySQL container (binlog enabled)')
  await docker(
    'run', '-d', '--name', mysqlName, '-p', '0:3306',
    '-e', 'MYSQL_ROOT_PASSWORD=pintail-root',
    'mysql:8.4',
    '--log-bin=binlog', '--binlog-format=ROW', '--binlog-row-image=FULL',
    '--binlog-row-metadata=FULL', '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
    '--max-allowed-packet=268435456',
  )
  const host = await dockerHost()
  const port = await publishedPort(mysqlName, 3306)
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const conn = await mysql.createConnection({
        host, port, user: 'root', password: 'pintail-root',
        multipleStatements: true, supportBigNumbers: true, bigNumberStrings: true,
        // Text temporals: a JS Date stringifies with the local zone and drops
        // fractional seconds, which breaks exact-result comparison.
        dateStrings: true,
      })
      await conn.query('SELECT 1')
      mysqlConn = conn
      return { host, port }
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready')
}

async function api<T>(path: string, options: { method?: string; body?: unknown } = {}): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(pintailToken ? { Authorization: `Bearer ${pintailToken}` } : {}),
      'content-type': 'application/json',
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  if (!response.ok) throw new Error(`${path} -> ${response.status}: ${await response.text()}`)
  return (await response.json()) as T
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_BENCHMARK_BINARY) return resolve(process.env.PINTAIL_BENCHMARK_BINARY)
  log('building pintail (release)')
  const build = Bun.spawn(['cargo', 'build', '--release', '-p', 'pintail'], {
    cwd: repository, stdout: 'inherit', stderr: 'inherit',
  })
  if ((await build.exited) !== 0) throw new Error('pintail build failed')
  const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'])
  return join(JSON.parse(metadata).target_directory, 'release', 'pintail')
}

async function startPintail(binary: string): Promise<void> {
  const port = 18100
  pintailUrl = `http://127.0.0.1:${port}`
  pintailProcess = Bun.spawn(
    [binary, '--data-dir', pintailDataDir, '--http-bind', `127.0.0.1:${port}`,
     '--wire-bind', '127.0.0.1:0'],
    {
      cwd: repository, stdout: 'inherit', stderr: 'inherit',
      env: { ...process.env, PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(4 * 1024 * 1024 * 1024) },
    },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (pintailProcess.exitCode !== null) {
      throw new Error(`pintail exited during startup (exit ${pintailProcess.exitCode})`)
    }
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy')
}

const TABLES = ['region', 'nation', 'supplier', 'part', 'partsupp', 'customer', 'orders', 'lineitem']

async function replicate(mysqlHost: string, mysqlPort: number): Promise<void> {
  const setup = await api<{ token: string }>('/api/auth/setup', {
    method: 'POST',
    body: { email: 'bench@pintail.dev', password: 'pintail-bench-password' },
  })
  pintailToken = setup.token
  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: {
      name: 'tpch',
      dsn: `mysql://benchmark:benchmarkpass@${mysqlHost}:${mysqlPort}/tpch`,
      mode: 'cdc',
      include_tables: TABLES,
    },
  })
  pintailDb = database.id
  await api(`/api/databases/${pintailDb}/probe`)
  const accepted = await api<{ run_id: string }>(`/api/databases/${pintailDb}/snapshot`, {
    method: 'POST', body: { force: false },
  })
  log(`snapshot ${accepted.run_id} started`)
  for (let attempt = 0; attempt < 28_800; attempt += 1) {
    const status = await api<{
      state: string
      tables: Array<{ name: string; rows: number; last_error?: string }>
    }>(`/api/databases/${pintailDb}/snapshot/status`)
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`,
      )
    }
    if (status.state === 'streaming' || status.state === 'polling') return
    if (attempt % 60 === 0) {
      const total = status.tables.reduce((a, t) => a + t.rows, 0)
      log(`snapshot progress: ${total.toLocaleString()} rows`)
      // A snapshot cannot finish once its source is gone, but pintail keeps
      // polling a server that no longer answers; one inspect a minute turns a
      // silent hang into a named failure.
      const alive = await docker('inspect', '-f', '{{.State.Running}}', mysqlName).catch(
        () => 'missing',
      )
      if (alive.trim() !== 'true') {
        throw new Error(`snapshot source ${mysqlName} is no longer running (${alive.trim()})`)
      }
    }
    await Bun.sleep(1000)
  }
  throw new Error('snapshot did not complete')
}

/// TPC-H fixes its substitution parameters, so this is literal replacement -
/// no sampling, no distributions. `:name` is replaced with the manifest value,
/// quoted when it is text.
function substitute(sql: string, params: Record<string, string | number>): string {
  let out = sql
  for (const [name, value] of Object.entries(params)) {
    const literal = typeof value === 'number' ? String(value) : `'${String(value).replace(/'/g, "''")}'`
    out = out.replaceAll(`:${name}`, literal)
  }
  return out
}

function fingerprint(rows: unknown[][]): string {
  return rows.map((row) => row.map((cell) => (cell === null ? ' ' : String(cell))).join('')).join('')
}

interface QueryOutcome {
  id: string
  class: string
  status: 'ok' | 'mismatch' | 'error'
  mysqlMs?: number
  pintailMs?: number
  rows?: number
  detail?: string
}

async function runQueries(): Promise<QueryOutcome[]> {
  const outcomes: QueryOutcome[] = []
  for (const spec of manifest.queries as TpchQuerySpec[]) {
    const sql = substitute(readFileSync(join(workloadDir, spec.sqlFile), 'utf8'), spec.params)
    const outcome: QueryOutcome = { id: spec.id, class: spec.class, status: 'ok' }
    try {
      const mysqlStart = performance.now()
      const [mysqlRows] = await mysqlConn!.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
      outcome.mysqlMs = Math.round(performance.now() - mysqlStart)

      const pintailStart = performance.now()
      const pintail = await api<{ rows: unknown[][] }>('/api/query', {
        method: 'POST', body: { db: pintailDb, sql },
      })
      outcome.pintailMs = Math.round(performance.now() - pintailStart)
      outcome.rows = pintail.rows.length

      const expected = fingerprint(mysqlRows as unknown as unknown[][])
      const actual = fingerprint(pintail.rows)
      if (expected !== actual) {
        outcome.status = 'mismatch'
        outcome.detail =
          `rows mysql=${(mysqlRows as unknown[]).length} pintail=${pintail.rows.length}`
        const diffDir = join(workloadDir, 'results', 'diffs')
        mkdirSync(diffDir, { recursive: true })
        writeFileSync(
          join(diffDir, `${spec.id}.txt`),
          `--- mysql\n${expected.replaceAll('', '\n')}\n--- pintail\n${actual.replaceAll('', '\n')}\n`,
        )
      }
    } catch (error) {
      outcome.status = 'error'
      outcome.detail = String(error).slice(0, 400)
    }
    log(
      `${spec.id}: ${outcome.status}` +
        (outcome.status === 'ok'
          ? ` mysql=${outcome.mysqlMs}ms pintail=${outcome.pintailMs}ms rows=${outcome.rows}`
          : ` — ${outcome.detail}`),
    )
    outcomes.push(outcome)
  }
  return outcomes
}

async function teardown() {
  try {
    await mysqlConn?.end()
  } catch {}
  pintailProcess?.kill()
  await docker('rm', '-f', mysqlName).catch(() => undefined)
}

async function main() {
  const started = new Date().toISOString()
  let outcomes: QueryOutcome[] = []
  try {
    const { host, port } = await startMysql()
    await mysqlConn!.query('CREATE DATABASE tpch')
    await mysqlConn!.query('USE tpch')
    await mysqlConn!.query(readFileSync(join(workloadDir, 'schema.mysql.sql'), 'utf8'))
    // The replica connects as this user, exactly as the commerce workload does.
    // No explicit auth plugin: MySQL 8.4 no longer loads mysql_native_password
    // by default, and naming it fails with ER_PLUGIN_IS_NOT_LOADED. Both other
    // runners let the server pick, and so does this one.
    await mysqlConn!.query(
      "CREATE USER IF NOT EXISTS 'benchmark'@'%' IDENTIFIED BY 'benchmarkpass'",
    )
    await mysqlConn!.query("GRANT ALL PRIVILEGES ON *.* TO 'benchmark'@'%' WITH GRANT OPTION")
    await mysqlConn!.query('FLUSH PRIVILEGES')

    // Seeding is not replication traffic. Both other runners drop it from the
    // binlog for the load, and the reason matters here more than there: the
    // snapshot is taken after seeding, so every seeded row would be written
    // twice - once into the binlog nothing will read, once into the snapshot
    // that actually populates the replica. At sf1 that is six million
    // lineitems of pure waste.
    await mysqlConn!.query('SET SESSION sql_log_bin=0')
    const counts = await seedTpch(mysqlConn!, scale, manifest.seed, log)
    await mysqlConn!.query('SET SESSION sql_log_bin=1')

    pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-tpch-'))
    await startPintail(await buildPintail())
    await replicate(host, port)
    log('replica ready; running the suite')
    outcomes = await runQueries()

    const failures = outcomes.filter((o) => o.status !== 'ok')
    const report = {
      workload: manifest.id,
      profile: profileName,
      scale,
      startedAt: started,
      finishedAt: new Date().toISOString(),
      counts,
      queries: outcomes,
      gate: { exactResults: manifest.gates.exactResults, passed: failures.length === 0 },
    }
    const resultsDir = join(workloadDir, 'results')
    mkdirSync(resultsDir, { recursive: true })
    writeFileSync(join(resultsDir, 'latest.json'), `${JSON.stringify(report, null, 2)}\n`)
    writeFileSync(
      join(resultsDir, 'latest.md'),
      `# TPC-H — ${profileName} (scale ${scale})\n\n` +
        `| query | class | status | mysql | pintail | rows |\n|---|---|---|---|---|---|\n` +
        outcomes
          .map(
            (o) =>
              `| ${o.id} | ${o.class} | ${o.status} | ${o.mysqlMs ?? '-'}ms | ${o.pintailMs ?? '-'}ms | ${o.rows ?? '-'} |`,
          )
          .join('\n') +
        '\n',
    )
    if (failures.length > 0) {
      log(`FAIL — ${failures.length} of ${outcomes.length} queries did not match MySQL`)
      process.exitCode = 1
    } else {
      log(`PASS — ${outcomes.length} queries, all byte-exact against MySQL`)
    }
  } finally {
    await teardown()
  }
}

await main()

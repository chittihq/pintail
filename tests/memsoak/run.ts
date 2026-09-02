/// Memory-churn soak: does the server's resident memory follow what is live,
/// or the history of what its supervisor has opened?
///
/// Every other memory measurement in this repository samples a binary built
/// and run on the developer's machine. That is the wrong operating system:
/// the release image is Linux, and a staging node running it held seven
/// gigabytes of heap for half a gigabyte of data because glibc malloc kept
/// every thread's freed memory inside that thread's arena while the
/// supervisor opened and dropped three hundred table stores every cycle.
/// Nothing here could see it - eleven tables for ten minutes on macOS is a
/// few megabytes of churn on an allocator that behaves differently.
///
/// So this soak runs the actual Linux image, on the docker host, against a
/// source with hundreds of small tables and the supervisor on a fast cadence,
/// and samples the container's memory for ten minutes. The verdict is the
/// slope after warm-up: resident memory may settle, it may not climb. Whatever
/// the allocator, a server that retains memory per cycle fails here.
///
/// Run with: bun run run.ts
///           MEMSOAK_IMAGE=ghcr.io/chittihq/pintail:0.1.0 bun run run.ts
///           MEMSOAK_TABLES=300 MEMSOAK_DURATION_MS=600000 bun run run.ts

import { writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const network = `pintail-memsoak-${nonce}`
const mysqlName = `pintail-memsoak-mysql-${nonce}`
const pintailName = `pintail-memsoak-${nonce}`
const builtImage = `pintail-memsoak:${nonce}`
const DATABASE = 'soak_db'

/// Tables in the source. The retention scales with tables opened per cycle,
/// so this is the knob that makes a small leak visible in ten minutes.
const TABLES = Number(process.env.MEMSOAK_TABLES ?? '300')
const ROWS_PER_TABLE = Number(process.env.MEMSOAK_ROWS ?? '50')
/// Source write rate during the soak: rows per batch and the pause between
/// batches. The default is a trickle; a heavy rate (thousands of rows a
/// second over large tables) drives memtable flushes and compactions, the
/// large transient allocations a quiet source never produces.
const WRITE_ROWS = Number(process.env.MEMSOAK_WRITE_ROWS ?? '1')
const WRITE_MS = Number(process.env.MEMSOAK_WRITE_MS ?? '500')
/// Wire query clients running analytical shapes over the seeded tables for
/// the length of the soak, each on a fresh connection, with this pause
/// between queries. A query's working set is the largest transient
/// allocation this server makes, and it lands on whichever blocking thread
/// picks the query up - which is exactly the pattern a per-thread allocator
/// retains. Zero runs no queries.
const QUERY_CLIENTS = Number(process.env.MEMSOAK_QUERY_CLIENTS ?? '0')
const QUERY_MS = Number(process.env.MEMSOAK_QUERY_MS ?? '2000')
/// Supervisor cadence inside the container. Production runs 5000ms; a fast
/// cadence compresses an hour of production churn into minutes.
const SUPERVISOR_MS = Number(process.env.MEMSOAK_SUPERVISOR_MS ?? '300')
const DURATION_MS = Number(process.env.MEMSOAK_DURATION_MS ?? '600000')
/// Samples inside the warm-up are reported but not judged: the first cycles
/// load every table for the first time and legitimately grow.
const WARMUP_MS = Number(process.env.MEMSOAK_WARMUP_MS ?? '120000')
const SAMPLE_MS = Number(process.env.MEMSOAK_SAMPLE_MS ?? '15000')
/// Container memory limit. Small on purpose: past it the kernel swaps or
/// kills, and both are recorded as failures.
const MEMORY_LIMIT = process.env.MEMSOAK_MEMORY_LIMIT ?? '1g'
/// The verdict. After warm-up, resident plus swapped memory may climb at
/// most this fast, and by at most this much in total.
const SLOPE_LIMIT_MIB_PER_MIN = Number(process.env.MEMSOAK_SLOPE_LIMIT ?? '2')
const GROWTH_LIMIT_MIB = Number(process.env.MEMSOAK_GROWTH_LIMIT ?? '128')
/// An image to test instead of building one from the tree.
const IMAGE = process.env.MEMSOAK_IMAGE ?? ''
/// Extra environment for the server container, comma-separated NAME=VALUE
/// pairs: how an allocator setting is tried without a new image.
const EXTRA_ENV = (process.env.MEMSOAK_ENV ?? '')
  .split(',')
  .map((pair) => pair.trim())
  .filter(Boolean)

interface Sample {
  atMs: number
  rssPlusSwapMib: number
  cgroupMib: number
  cdcStarts: number
}

let mysqlConnection: mysql.Connection | undefined
let started: { network: boolean; mysql: boolean; pintail: boolean; image: boolean } = {
  network: false,
  mysql: false,
  pintail: false,
  image: false,
}
let pintailUrl = ''
let token = ''

function log(message: string) {
  console.log(`[memsoak] ${message}`)
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

async function waitForMysql(host: string, port: number) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        multipleStatements: true,
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
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} → ${response.status}: ${text}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

async function sql(statement: string) {
  await mysqlConnection!.query(statement)
}

function tableName(index: number): string {
  return `t_${String(index).padStart(4, '0')}`
}

async function seed() {
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )
  log(`seeding ${TABLES} tables of ${ROWS_PER_TABLE} rows`)
  // One statement per table and 5,000 rows per INSERT: large tables would
  // otherwise exceed MySQL's packet limit in a single statement.
  for (let index = 0; index < TABLES; index += 1) {
    const table = tableName(index)
    await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, v INT NOT NULL, note VARCHAR(32) NOT NULL)`)
    for (let start = 0; start < ROWS_PER_TABLE; start += 5_000) {
      const rows = Array.from(
        { length: Math.min(5_000, ROWS_PER_TABLE - start) },
        (_, offset) => `(${start + offset}, ${(start + offset) * 7}, 'n${start + offset}')`,
      )
      await sql(`INSERT INTO ${table} VALUES ${rows.join(',')}`)
    }
  }
}

/// Resident plus swapped memory of the server process, and the cgroup's own
/// count, both in MiB. Swap is counted because a process that has been
/// pushed to swap has not stopped holding the memory.
async function sample(atMs: number): Promise<Sample> {
  const status = (
    await docker(
      'exec',
      pintailName,
      'sh',
      '-c',
      'awk \'/VmRSS|VmSwap/ {s+=$2} END {print s}\' /proc/1/status; cat /sys/fs/cgroup/memory.current',
    )
  ).stdout.split('\n')
  const logs = await docker('logs', pintailName)
  const cdcStarts = (logs.stdout + logs.stderr).split('\n').filter((line) => line.includes('cdc start')).length
  return {
    atMs,
    rssPlusSwapMib: Number(status[0]) / 1024,
    cgroupMib: Number(status[1]) / 1024 / 1024,
    cdcStarts,
  }
}

/// Least-squares slope of memory over time, in MiB per minute.
function slope(samples: Sample[]): number {
  if (samples.length < 2) return 0
  const n = samples.length
  const meanX = samples.reduce((sum, s) => sum + s.atMs, 0) / n
  const meanY = samples.reduce((sum, s) => sum + s.rssPlusSwapMib, 0) / n
  let numerator = 0
  let denominator = 0
  for (const s of samples) {
    numerator += (s.atMs - meanX) * (s.rssPlusSwapMib - meanY)
    denominator += (s.atMs - meanX) ** 2
  }
  return denominator === 0 ? 0 : (numerator / denominator) * 60_000
}

function publish(image: string, samples: Sample[], verdict: string[]) {
  const judged = samples.filter((s) => s.atMs >= WARMUP_MS)
  const first = judged[0]
  const last = judged.at(-1)
  const lines = [
    '# Pintail memory-churn soak',
    '',
    `Measured ${new Date().toISOString()} on \`${image}\` (Linux container, ${MEMORY_LIMIT} limit${EXTRA_ENV.length ? `, env ${EXTRA_ENV.join(" ")}` : ""}).`,
    '',
    `${TABLES} tables × ${ROWS_PER_TABLE} rows, writes of ${WRITE_ROWS} rows every ${WRITE_MS}ms, ${QUERY_CLIENTS} query clients every ${QUERY_MS}ms, supervisor every ${SUPERVISOR_MS}ms, ` +
      `${(DURATION_MS / 60_000).toFixed(0)} min sampled every ${SAMPLE_MS / 1000}s, ` +
      `first ${(WARMUP_MS / 60_000).toFixed(0)} min are warm-up.`,
    '',
    `**Verdict: ${verdict.length ? 'FAIL' : 'PASS'}.** ` +
      (first && last
        ? `After warm-up: ${first.rssPlusSwapMib.toFixed(0)} → ${last.rssPlusSwapMib.toFixed(0)} MiB ` +
          `(${(last.rssPlusSwapMib - first.rssPlusSwapMib).toFixed(0)} MiB growth, ` +
          `slope ${slope(judged).toFixed(2)} MiB/min, limits ${GROWTH_LIMIT_MIB} MiB and ${SLOPE_LIMIT_MIB_PER_MIN} MiB/min), ` +
          `${last.cdcStarts - first.cdcStarts} CDC cycles.`
        : 'no samples after warm-up.'),
    ...verdict.map((line) => `- ${line}`),
    '',
    '| t (s) | RSS+swap MiB | cgroup MiB | CDC cycles |',
    '|---:|---:|---:|---:|',
    ...samples.map(
      (s) => `| ${(s.atMs / 1000).toFixed(0)} | ${s.rssPlusSwapMib.toFixed(0)} | ${s.cgroupMib.toFixed(0)} | ${s.cdcStarts} |`,
    ),
    '',
  ]
  writeFileSync(join(import.meta.dir, 'results.md'), lines.join('\n'))
  writeFileSync(
    join(import.meta.dir, 'results.json'),
    JSON.stringify(
      {
        image,
        tables: TABLES,
        rowsPerTable: ROWS_PER_TABLE,
        supervisorMs: SUPERVISOR_MS,
        durationMs: DURATION_MS,
        warmupMs: WARMUP_MS,
        memoryLimit: MEMORY_LIMIT,
        slopeLimitMibPerMin: SLOPE_LIMIT_MIB_PER_MIN,
        growthLimitMib: GROWTH_LIMIT_MIB,
        slopeMibPerMin: slope(judged),
        verdict,
        samples,
      },
      null,
      2,
    ),
  )
}

async function main() {
  const host = await dockerHost()
  let image = IMAGE
  if (!image) {
    log('building the release image from the working tree')
    await docker('build', '--tag', builtImage, repository)
    started.image = true
    image = builtImage
  }

  await docker('network', 'create', network)
  started.network = true
  log(`starting MySQL source ${mysqlName}`)
  await docker(
    'run', '--detach', '--name', mysqlName, '--network', network,
    '--publish', '0:3306',
    '--tmpfs', '/var/lib/mysql:rw,size=2g',
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env', `MYSQL_DATABASE=${DATABASE}`,
    'mysql:8.4',
    '--server-id=948', '--log-bin=mysql-bin', '--binlog-format=ROW',
    '--binlog-row-image=FULL', '--binlog-row-metadata=FULL',
    '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  started.mysql = true
  mysqlConnection = await waitForMysql(host, await publishedPort(mysqlName, 3306))
  await seed()

  log(`starting ${image} with a ${MEMORY_LIMIT} limit`)
  await docker(
    'run', '--detach', '--name', pintailName, '--network', network,
    '--memory', MEMORY_LIMIT, '--publish', '0:8080', '--publish', '0:3306',
    '--env', `PINTAIL_SUPERVISOR_INTERVAL_MS=${SUPERVISOR_MS}`,
    '--env', 'PINTAIL_LOG=info',
    ...EXTRA_ENV.flatMap((pair) => ['--env', pair]),
    image,
  )
  started.pintail = true
  pintailUrl = `http://${host}:${await publishedPort(pintailName, 8080)}`
  for (let attempt = 0; ; attempt += 1) {
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) break
    } catch {}
    if (attempt > 120) throw new Error('pintail did not become healthy within a minute')
    await Bun.sleep(500)
  }

  token = (
    await api<{ token: string }>('/api/auth/setup', {
      method: 'POST',
      auth: false,
      body: { email: 'memsoak@pintail.local', password: 'memsoak-password-1' },
    })
  ).token
  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: { name: DATABASE, dsn: `mysql://pintail:pintail@${mysqlName}:3306/${DATABASE}`, mode: 'cdc' },
  })
  await api(`/api/databases/${database.id}/probe`)
  await api(`/api/databases/${database.id}/snapshot`, { method: 'POST', body: { force: false } })
  for (;;) {
    const status = await api<{ state: string; tables: Array<{ last_error?: string }> }>(
      `/api/databases/${database.id}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(`snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`)
    }
    if (status.state === 'streaming' || status.state === 'polling') break
    await Bun.sleep(1_000)
  }
  log(`replicating ${TABLES} tables; sampling for ${(DURATION_MS / 60_000).toFixed(0)} minutes`)

  // A trickle of source writes so cycles carry real work: stamps move,
  // WALs grow and flush, the way a quiet production source behaves.
  let writing = true
  let written = 0
  let batches = 0
  const writer = (async () => {
    while (writing) {
      const table = tableName(batches % TABLES)
      const base = ROWS_PER_TABLE + Math.floor(batches / TABLES) * WRITE_ROWS
      const rows = Array.from({ length: WRITE_ROWS }, (_, row) => `(${base + row}, ${written + row}, 'w')`)
      await sql(`INSERT INTO ${table} VALUES ${rows.join(',')}`).catch(() => {})
      written += WRITE_ROWS
      batches += 1
      await Bun.sleep(WRITE_MS)
    }
  })()

  let queriesOk = 0
  let queriesFailed = 0
  const queryClients: Promise<void>[] = []
  if (QUERY_CLIENTS > 0) {
    const key = await api<{ secret: string }>(`/api/databases/${database.id}/api-keys`, {
      method: 'POST',
      body: { name: 'memsoak', scopes: ['query', 'read'] },
    })
    const wirePort = await publishedPort(pintailName, 3306)
    const shapes = (index: number) => {
      const a = tableName(index % TABLES)
      const b = tableName((index + 1) % TABLES)
      return [
        `SELECT v % 97 AS k, COUNT(DISTINCT id) AS d, SUM(v) AS s FROM ${a} GROUP BY k ORDER BY k`,
        `SELECT a.v % 50 AS k, COUNT(DISTINCT b.id) AS d FROM ${a} a JOIN ${b} b ON a.id = b.id GROUP BY k ORDER BY k`,
        `SELECT * FROM ${a} ORDER BY note DESC, id LIMIT 100`,
      ]
    }
    for (let seat = 0; seat < QUERY_CLIENTS; seat += 1) {
      queryClients.push(
        (async () => {
          let turn = seat
          while (writing) {
            let connection: mysql.Connection | undefined
            try {
              connection = await mysql.createConnection({
                host,
                port: wirePort,
                user: DATABASE,
                password: key.secret,
                database: DATABASE,
              })
              const statements = shapes(turn)
              await connection.query({ sql: statements[turn % statements.length]!, rowsAsArray: true })
              queriesOk += 1
            } catch {
              queriesFailed += 1
            } finally {
              await connection?.end().catch(() => {})
            }
            turn += 1
            await Bun.sleep(QUERY_MS)
          }
        })(),
      )
    }
    log(`${QUERY_CLIENTS} query clients running`)
  }

  const samples: Sample[] = []
  const soakStarted = performance.now()
  while (performance.now() - soakStarted < DURATION_MS) {
    const atMs = performance.now() - soakStarted
    const s = await sample(atMs)
    samples.push(s)
    log(`t=${(atMs / 1000).toFixed(0)}s rss+swap=${s.rssPlusSwapMib.toFixed(0)}MiB cgroup=${s.cgroupMib.toFixed(0)}MiB cycles=${s.cdcStarts}`)
    await Bun.sleep(SAMPLE_MS)
  }
  writing = false
  await writer
  await Promise.all(queryClients)
  if (QUERY_CLIENTS > 0) log(`queries: ${queriesOk} ok, ${queriesFailed} failed`)

  const verdict: string[] = []
  const state = (await docker('inspect', pintailName, '--format', '{{.State.Running}} {{.State.OOMKilled}}')).stdout
  if (state !== 'true false') verdict.push(`the container is not running cleanly (running/oom: ${state})`)
  const judged = samples.filter((s) => s.atMs >= WARMUP_MS)
  const growth = judged.length >= 2 ? judged.at(-1)!.rssPlusSwapMib - judged[0]!.rssPlusSwapMib : 0
  const rate = slope(judged)
  if (judged.length < 4) verdict.push(`only ${judged.length} samples after warm-up`)
  if (rate > SLOPE_LIMIT_MIB_PER_MIN) verdict.push(`memory climbs ${rate.toFixed(2)} MiB/min after warm-up (limit ${SLOPE_LIMIT_MIB_PER_MIN})`)
  if (growth > GROWTH_LIMIT_MIB) verdict.push(`memory grew ${growth.toFixed(0)} MiB after warm-up (limit ${GROWTH_LIMIT_MIB})`)
  publish(image, samples, verdict)
  log(`results written to ${join(import.meta.dir, 'results.md')}`)
  if (verdict.length) throw new Error(`memory does not settle:\n  ${verdict.join('\n  ')}`)
  log(`PASS: ${growth.toFixed(0)} MiB growth, ${rate.toFixed(2)} MiB/min after warm-up`)
}

async function teardown() {
  await mysqlConnection?.end().catch(() => {})
  if (started.pintail) await docker('rm', '--force', '--volumes', pintailName).catch(() => {})
  if (started.mysql) await docker('rm', '--force', '--volumes', mysqlName).catch(() => {})
  if (started.network) await docker('network', 'rm', network).catch(() => {})
  if (started.image) await docker('rmi', builtImage).catch(() => {})
}

try {
  await main()
} catch (error) {
  console.error(`[memsoak] FAILED: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await teardown()
}

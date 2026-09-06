#!/usr/bin/env bun
// Profiles a few benchmark queries on the docker host with the scan and
// execution thread pools controlled separately, and prints every run's
// per-operator profile.
//
// Reuses what a finished benchmark leaves behind: the image is rebuilt from
// the working tree, and the replica comes from a copy of a benchmark run's
// pintail data volume (PINTAIL_PROFILE_DATA_VOLUME), so no MySQL source and
// no twenty-million-row snapshot are needed. The container runs under the
// benchmark's own limits (8 CPUs, 8 GB) with the settled memo off, so every
// run executes. Output goes to stdout and to PINTAIL_PROFILE_REPORT when set.
//
// Development tooling: nothing here is a gate, and its numbers are not
// banked.

import { resolve } from 'node:path'
import { writeFileSync } from 'node:fs'

const repository = resolve(import.meta.dir, '..')
const DATA_VOLUME = process.env.PINTAIL_PROFILE_DATA_VOLUME
const THREADS = (process.env.PINTAIL_PROFILE_THREADS ?? '8x8,4x8,2x8,8x4,8x2,16x8,8x16')
  .split(',')
  .map((pair) => pair.trim().split('x').map(Number) as [number, number])
const RUNS = Number(process.env.PINTAIL_PROFILE_RUNS ?? 3)
const QUERIES: Array<{ name: string; sql: string }> = [
  { name: 'Q2', sql: "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'shipped'" },
  { name: 'N1', sql: "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'shipped' AND id >= 1" },
  {
    name: 'Q5',
    sql: "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS revenue FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' GROUP BY yr, mo ORDER BY yr, mo",
  },
  {
    name: 'Q8',
    sql: 'SELECT u.region, COUNT(*) AS cnt, ROUND(SUM(o.total_amount), 2) AS total FROM orders o JOIN users u ON o.user_id = u.id GROUP BY u.region ORDER BY total DESC',
  },
]
const runId = `pintail-profile-${process.pid}`
const containerName = `${runId}-pintail`
const volumeName = `${runId}-data`
const engineLimits = ['--cpus', '8', '--memory', '8g']

const log = (message: string) => console.log(`[profile ${new Date().toISOString()}] ${message}`)
const report: string[] = []
const emit = (line: string) => {
  console.log(line)
  report.push(line)
}

async function command(args: string[], quiet = true) {
  const child = Bun.spawn(args, { cwd: repository, stdout: 'pipe', stderr: 'pipe' })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) throw new Error(`${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`)
  if (!quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}
const docker = (...args: string[]) => command(['docker', ...args])

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')).stdout
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const ssh = await command(['ssh', '-G', target])
  const hostname = ssh.stdout.split('\n').find((line) => line.startsWith('hostname '))?.slice('hostname '.length)
  if (!hostname) throw new Error(`could not resolve Docker SSH target ${target}`)
  return hostname.includes(':') ? `[${hostname}]` : hostname
}

async function publishedPort(name: string, port: number): Promise<number> {
  const output = (await docker('port', name, `${port}/tcp`)).stdout
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`Docker did not publish ${name}:${port}`)
  return Number(match[1])
}

async function api<T>(baseUrl: string, path: string, options: { method?: string; token?: string; body?: unknown } = {}): Promise<T> {
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

async function waitForHttp(baseUrl: string) {
  for (let attempt = 0; attempt < 480; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become ready within four minutes')
}

let started = false

async function startPintail(image: string, scanThreads: number, execThreads: number, host: string) {
  await docker('rm', '-f', containerName).catch(() => undefined)
  await docker(
    'run', '--detach', '--name', containerName, '--publish', '0:8080',
    '--volume', `${volumeName}:/var/lib/pintail`,
    ...engineLimits,
    '--env', `PINTAIL_QUERY_MEMORY_LIMIT_BYTES=${4 * 1024 * 1024 * 1024}`,
    '--env', 'PINTAIL_PROFILE=1',
    '--env', 'PINTAIL_DISABLE_SETTLED_MEMO=1',
    '--env', `PINTAIL_SCAN_THREADS=${scanThreads}`,
    '--env', `RAYON_NUM_THREADS=${execThreads}`,
    image,
  )
  started = true
  const port = await publishedPort(containerName, 8080)
  const baseUrl = `http://${host}:${port}`
  await waitForHttp(baseUrl)
  return baseUrl
}

function profileBlock(rows: unknown[][]): string {
  const lines = rows.map((row) => row.map(String).join(' '))
  const start = lines.findIndex((line) => line.startsWith('Profile total='))
  return start < 0 ? lines.join('\n') : lines.slice(start).join('\n')
}

async function main() {
  if (!DATA_VOLUME) {
    throw new Error('set PINTAIL_PROFILE_DATA_VOLUME to a benchmark run\'s pintail data volume')
  }
  const host = await dockerHost()
  const sha = (await command(['git', 'rev-parse', '--short=12', 'HEAD'])).stdout
  const image = `pintail-profile:${sha}`
  log(`building ${image} on the docker host`)
  await docker('build', '--tag', image, repository)
  log(`copying ${DATA_VOLUME} into ${volumeName}`)
  await docker('volume', 'create', volumeName)
  await docker('run', '--rm', '--volume', `${DATA_VOLUME}:/from:ro`, '--volume', `${volumeName}:/to`, 'alpine:3', 'sh', '-c', 'cp -a /from/. /to/')

  emit(`# Thread-pool profiles at ${sha}`)
  emit('')
  emit('Container: 8 CPUs, 8 GB, 4 GiB query ceiling, settled memo off. scan x exec threads.')
  emit('')
  const summary: string[] = ['| query | scan x exec | run 1 | run 2 | run 3 | min |', '|---|---|---|---|---|---|']
  for (const [scan, exec] of THREADS) {
    log(`starting pintail with scan=${scan} exec=${exec}`)
    const baseUrl = await startPintail(image, scan, exec, host)
    const login = await api<{ token: string }>(baseUrl, '/api/auth/login', {
      method: 'POST',
      body: { email: 'benchmark@pintail.local', password: 'benchmark-release-gate' },
    })
    const databases = await api<unknown>(baseUrl, '/api/databases', { token: login.token })
    const list = (Array.isArray(databases) ? databases : (databases as { databases?: unknown[] }).databases ?? []) as Array<{ id: string }>
    const databaseId = list[0]?.id
    if (!databaseId) throw new Error(`no database in the copied volume: ${JSON.stringify(databases).slice(0, 200)}`)
    const count = await api<{ rows: unknown[][] }>(baseUrl, '/api/query', {
      method: 'POST', token: login.token, body: { db: databaseId, sql: 'SELECT COUNT(*) FROM orders' },
    })
    log(`orders in the replica: ${count.rows[0]?.[0]}`)
    emit(`## scan=${scan} exec=${exec}`)
    emit('')
    for (const query of QUERIES) {
      const timings: number[] = []
      let lastProfile = ''
      for (let run = 0; run < RUNS; run += 1) {
        const startedAt = performance.now()
        const explain = await api<{ rows: unknown[][] }>(baseUrl, '/api/query', {
          method: 'POST', token: login.token, body: { db: databaseId, sql: `EXPLAIN ANALYZE ${query.sql}` },
        })
        timings.push(performance.now() - startedAt)
        lastProfile = profileBlock(explain.rows)
      }
      const shown = timings.map((ms) => `${ms.toFixed(0)}ms`)
      summary.push(`| ${query.name} | ${scan}x${exec} | ${shown.join(' | ')} | ${Math.min(...timings).toFixed(0)}ms |`)
      emit(`### ${query.name} (${shown.join(', ')})`)
      emit('```')
      emit(lastProfile)
      emit('```')
      emit('')
    }
  }
  emit('## Summary (wall time of EXPLAIN ANALYZE through the HTTP API)')
  emit('')
  for (const line of summary) emit(line)
  if (process.env.PINTAIL_PROFILE_REPORT) {
    writeFileSync(process.env.PINTAIL_PROFILE_REPORT, report.join('\n') + '\n')
    log(`report written to ${process.env.PINTAIL_PROFILE_REPORT}`)
  }
}

async function cleanup() {
  if (started) await docker('rm', '-f', containerName).catch(() => undefined)
  await docker('volume', 'rm', volumeName).catch(() => undefined)
}

try {
  await main()
} catch (error) {
  log(`FAIL: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await cleanup()
}

// Production-shaped workload runner.
//
//   bun run run-production.ts --workload commerce-production-v1 --profile smoke
//   bun run run-production.ts --profile ci --engines mysql,pintail
//   bun run run-production.ts --profile full            # release gate
//
// Engines: mysql is always the oracle; pintail replicates from it via CDC and
// must return exact results. Queries that pintail cannot yet execute (e.g.
// window functions before the executor grows them) are recorded as
// `unsupported` per-query — the run completes, the gate fails loudly.

import { mkdirSync, readFileSync, writeFileSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { Rng, Zipf, seedWorkload, sqlDatetime } from './workloads/commerce-production-v1/seed'
import type { SeedProfile, SeedResult } from './workloads/commerce-production-v1/seed'
import { loadDataset } from './workloads/commerce-production-v1/load'
import { startMutations } from './workloads/commerce-production-v1/mutations'
import {
  TABLES, compareFingerprints, mysqlFingerprints, normalizeRows, pintailFingerprints,
} from './workloads/commerce-production-v1/validations'
import manifest from './workloads/commerce-production-v1/workload'
import type { QuerySpec } from './workloads/commerce-production-v1/workload'

// ---------- arguments ----------

function arg(name: string, fallback: string): string {
  const index = process.argv.indexOf(`--${name}`)
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback
}

const workloadId = arg('workload', 'commerce-production-v1')
const profileName = arg('profile', 'smoke') as keyof typeof manifest.profiles
const datasetAlias = arg('dataset', '')
const dsRepo = resolve(arg('ds-repo', join(import.meta.dir, '..', '..', 'pintail-ds')))
const engines = arg('engines', 'mysql,pintail').split(',')
const phaseFilter = arg('phases', '').split(',').filter(Boolean)
const benchmarkDir = import.meta.dir
const workloadDir = join(benchmarkDir, 'workloads', workloadId)
const repository = resolve(benchmarkDir, '..')
const runId = `pintail-prod-${process.pid}-${Date.now()}`
const scale = manifest.profiles[profileName]?.scale
if (!scale) throw new Error(`unknown profile ${profileName}`)

const log = (m: string) => console.log(`[production] ${m}`)

// ---------- docker helpers (mirrors run.ts conventions) ----------

async function command(args: string[], options: { stdin?: string } = {}) {
  const child = Bun.spawn(args, {
    cwd: repository,
    stdin: options.stdin === undefined ? 'ignore' : new Blob([options.stdin]),
    stdout: 'pipe',
    stderr: 'pipe',
  })
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

// ---------- engine plumbing ----------

const mysqlName = `${runId}-mysql`
let mysqlConn: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailUrl = ''
let pintailToken = ''
let pintailDb = ''
let pintailDataDir = ''
let pintailBinary = ''

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
        // Text temporals: JS Date objects stringify with the local zone and
        // drop fractional seconds, which breaks exact-result comparison.
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
      ...(options.body === undefined ? {} : { 'Content-Type': 'application/json' }),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} → ${response.status}: ${text}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
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

async function startPintail(): Promise<void> {
  const port = 18099
  pintailUrl = `http://127.0.0.1:${port}`
  // The wire endpoint has no disable switch; an ephemeral loopback bind
  // keeps it out of the way.
  pintailProcess = Bun.spawn(
    [
      pintailBinary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${port}`,
      '--wire-bind',
      '127.0.0.1:0',
    ],
    // Inherit rather than pipe: unread pipes hide every runtime error and
    // eventually fill and block the server's logging. This way pintail's
    // tracing lands in the runner's own output.
    {
      cwd: repository,
      stdout: 'inherit',
      stderr: 'inherit',
      // Same per-query budget the main benchmark harness grants (and in the
      // same ballpark as the 8g containers MySQL runs in); the 64MB default
      // is a wire-protocol safety net, not a benchmark configuration.
      env: {
        ...process.env,
        PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(4 * 1024 * 1024 * 1024),
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
  throw new Error('pintail did not become healthy')
}

async function setupPintail(mysqlHost: string, mysqlPort: number): Promise<void> {
  pintailBinary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-prod-'))
  await startPintail()
  const setup = await api<{ token: string }>('/api/auth/setup', {
    method: 'POST',
    body: { email: 'bench@pintail.dev', password: 'pintail-bench-password' },
  })
  pintailToken = setup.token
  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: {
      name: 'production_db',
      dsn: `mysql://benchmark:benchmarkpass@${mysqlHost}:${mysqlPort}/production_db`,
      mode: 'cdc',
      // The replicated set is the verified set plus the lag sentinel. It is
      // deliberately NOT in TABLES: that list drives fingerprint comparison,
      // and a probe table written to during the phase is harness scaffolding
      // rather than workload data to be checked.
      include_tables: [...TABLES, 'lag_probe'],
    },
  })
  pintailDb = database.id
  // The control plane refuses to snapshot an unprobed database (the probe
  // discovers tables and capabilities); the dashboard wizard does this
  // implicitly, API callers must do it explicitly.
  await api(`/api/databases/${pintailDb}/probe`)
  const accepted = await api<{ run_id: string }>(`/api/databases/${pintailDb}/snapshot`, {
    method: 'POST', body: { force: false },
  })
  log(`pintail snapshot ${accepted.run_id} started`)
  for (let attempt = 0; attempt < 28_800; attempt += 1) {
    const status = await api<{ state: string; tables: Array<{ name: string; rows: number; last_error?: string }> }>(
      `/api/databases/${pintailDb}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(`snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`)
    }
    if (status.state === 'streaming' || status.state === 'polling') return
    if (attempt % 60 === 0) {
      const total = status.tables.reduce((a, t) => a + t.rows, 0)
      log(`snapshot progress: ${total.toLocaleString()} rows`)
      // A snapshot cannot finish once its source container is gone, but
      // pintail keeps polling a server that no longer answers — that is how
      // this stage once sat silent for 54 minutes. One inspect per minute
      // turns it into an immediate, named failure.
      const alive = await docker('inspect', '-f', '{{.State.Running}}', mysqlName).catch(
        () => 'missing',
      )
      if (alive.trim() !== 'true') {
        throw new Error(
          `snapshot source container ${mysqlName} is no longer running (${alive.trim()})`,
        )
      }
    }
    await Bun.sleep(1000)
  }
  throw new Error('snapshot did not complete')
}

async function queryPintail(sql: string): Promise<unknown[][]> {
  const result = await api<{ rows: unknown[][] }>('/api/query', {
    method: 'POST', body: { db: pintailDb, sql },
  })
  return result.rows
}

// ---------- query parameterization ----------

interface PreparedQuery extends QuerySpec {
  sql: string
}

function loadQueries(): PreparedQuery[] {
  return manifest.queries.map((spec) => ({
    ...spec,
    sql: readFileSync(join(workloadDir, spec.sqlFile), 'utf8'),
  }))
}

function substitute(
  query: PreparedQuery,
  rng: Rng,
  seedResult: SeedResult,
  tenantZipf: Zipf,
  customerZipf: Zipf,
): string {
  let sql = query.sql
  for (const [name, spec] of Object.entries(query.params)) {
    let literal: string
    switch (spec.kind) {
      case 'zipfTenant':
        literal = String(1 + tenantZipf.sample(rng))
        break
      case 'zipfCustomer': {
        const hot = [...seedResult.customersByTenantSample.values()].flat()
        literal = hot.length > 0 && rng.chance(0.7)
          ? String(hot[rng.int(hot.length)])
          : String(1 + customerZipf.sample(rng))
        break
      }
      case 'now':
        literal = `'${sqlDatetime(seedResult.now)}'`
        break
      case 'daysAgo': {
        const days = spec.choices[rng.int(spec.choices.length)]
        literal = `'${sqlDatetime(new Date(seedResult.now.getTime() - days * 86_400_000))}'`
        break
      }
    }
    sql = sql.replaceAll(`:${name}`, literal)
  }
  return sql
}

// ---------- measurement ----------

interface QueryRun {
  id: string
  engine: string
  status: 'ok' | 'unsupported' | 'error' | 'mismatch'
  /// A gap the workload declares in advance, so the gate can tell one apart
  /// from a query the engine simply failed to answer. Recorded on the run
  /// rather than looked up later, so the artifact says which it was.
  declaredGap?: boolean
  medianMs?: number
  p95Ms?: number
  runs?: number[]
  error?: string
}

function summarize(times: number[]): { medianMs: number; p95Ms: number } {
  const sorted = [...times].sort((a, b) => a - b)
  return {
    medianMs: sorted[Math.floor(sorted.length / 2)],
    p95Ms: sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * 0.95) - 1)],
  }
}

async function runQuerySuite(
  phaseId: string,
  runs: number,
  warmups: number,
  seedResult: SeedResult,
): Promise<QueryRun[]> {
  const queries = loadQueries()
  const tenantZipf = new Zipf(seedResult.counts.tenants, 1.15)
  const customerZipf = new Zipf(seedResult.counts.customers, 1.05)
  const results: QueryRun[] = []
  for (const query of queries) {
    const rng = new Rng(manifest.seed * 100 + phaseId.length * 17 + queries.indexOf(query))
    const sql = substitute(query, rng, seedResult, tenantZipf, customerZipf)

    // MySQL (oracle + baseline timing)
    let mysqlRows: unknown[][] | undefined
    const mysqlTimes: number[] = []
    try {
      for (let i = 0; i < warmups; i += 1) await mysqlConn!.query(sql)
      for (let i = 0; i < runs; i += 1) {
        const t = performance.now()
        const [rows] = await mysqlConn!.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
        mysqlTimes.push(performance.now() - t)
        mysqlRows = rows as unknown[][]
      }
      results.push({ id: query.id, engine: 'mysql', status: 'ok', runs: mysqlTimes, ...summarize(mysqlTimes) })
    } catch (error) {
      results.push({ id: query.id, engine: 'mysql', status: 'error', error: String(error) })
      continue
    }

    if (!engines.includes('pintail')) continue

    const pintailTimes: number[] = []
    let pintailRows: unknown[][] | undefined
    try {
      for (let i = 0; i < warmups; i += 1) await queryPintail(sql)
      for (let i = 0; i < runs; i += 1) {
        const t = performance.now()
        pintailRows = await queryPintail(sql)
        pintailTimes.push(performance.now() - t)
      }
      const exact =
        !manifest.gates.exactResults ||
        normalizeRows(pintailRows ?? []) === normalizeRows(mysqlRows ?? [])
      results.push({
        id: query.id,
        engine: 'pintail',
        status: exact ? 'ok' : 'mismatch',
        runs: pintailTimes,
        ...summarize(pintailTimes),
      })
      if (!exact) {
        log(`RESULT MISMATCH on ${query.id}`)
        // Row-level evidence, or a mismatch is undebuggable after teardown.
        const diffDir = join(workloadDir, 'results', 'diffs')
        mkdirSync(diffDir, { recursive: true })
        const left = normalizeRows(mysqlRows ?? []).split('\n')
        const right = normalizeRows(pintailRows ?? []).split('\n')
        const first = left.findIndex((line, index) => line !== right[index])
        const at = first === -1 ? Math.min(left.length, right.length) : first
        const window = (lines: string[]) =>
          lines.slice(Math.max(0, at - 2), at + 8).join('\n')
        writeFileSync(
          join(diffDir, `${query.id}.txt`),
          `rows mysql=${left.length} pintail=${right.length} first_diff_row=${at}\n` +
            `--- mysql\n${window(left)}\n--- pintail\n${window(right)}\n`,
        )
        log(`  diff evidence: results/diffs/${query.id}.txt (first diff at row ${at})`)
      }
    } catch (error) {
      const message = String(error)
      const unsupported = /unsupported|not\s+implemented|parse|syntax/i.test(message)
      results.push({
        id: query.id,
        engine: 'pintail',
        status: unsupported ? 'unsupported' : 'error',
        declaredGap: query.requiresWindowFunctions === true,
        error: message.slice(0, 500),
      })
      log(`${query.id}: pintail ${unsupported ? 'UNSUPPORTED' : 'ERROR'}${query.requiresWindowFunctions ? ' (window functions — v1 forcing function)' : ''}`)
    }
  }
  return results
}

// ---------- phases ----------

async function main() {
  const profile = JSON.parse(
    readFileSync(join(workloadDir, 'production-profile.json'), 'utf8'),
  ) as SeedProfile
  const report: Record<string, unknown> = {
    workload: workloadId,
    profile: profileName,
    scale,
    engines,
    startedAt: new Date().toISOString(),
    phases: {},
  }

  const { host, port } = await startMysql()
  let seedResult: SeedResult
  if (datasetAlias) {
    seedResult = await loadDataset(mysqlConn!, {
      workloadId,
      alias: datasetAlias,
      dsRepo,
      cacheDir: join(benchmarkDir, '.dataset-cache', workloadId),
      workloadDir,
      mysqlName,
      docker,
      log,
    })
  } else {
    await mysqlConn!.query('SET SESSION sql_log_bin=0')
    await mysqlConn!.query('CREATE DATABASE production_db')
    await mysqlConn!.query('USE production_db')
    await mysqlConn!.query(readFileSync(join(workloadDir, 'schema.mysql.sql'), 'utf8'))
    seedResult = await seedWorkload(mysqlConn!, profile, scale, manifest.seed, log)
  }
  await mysqlConn!.query('SET SESSION sql_log_bin=0')
  await mysqlConn!.query("CREATE USER IF NOT EXISTS 'benchmark'@'%' IDENTIFIED BY 'benchmarkpass'")
  await mysqlConn!.query(
    "GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'benchmark'@'%'",
  )
  await mysqlConn!.query('SET SESSION sql_log_bin=1')

  const phases = manifest.phases.filter(
    (phase) => phaseFilter.length === 0 || phaseFilter.includes(phase.id),
  )

  for (const phase of phases) {
    log(`=== phase ${phase.id} (${phase.action}) ===`)
    const startedAt = performance.now()
    switch (phase.action) {
      case 'seed-and-snapshot': {
        if (engines.includes('pintail')) await setupPintail(host, port)
        break
      }
      case 'query-suite': {
        const results = await runQuerySuite(phase.id, phase.runs ?? 3, phase.warmups ?? 0, seedResult)
        ;(report.phases as Record<string, unknown>)[phase.id] = results
        break
      }
      case 'cdc-and-query': {
        if (!engines.includes('pintail')) break
        const writerCount = phase.writers ?? 4
        const writerConns: mysql.Connection[] = []
        for (let i = 0; i < writerCount; i += 1) {
          const conn = await mysql.createConnection({
            host, port, user: 'root', password: 'pintail-root', supportBigNumbers: true, bigNumberStrings: true,
          })
          await conn.query('USE production_db')
          writerConns.push(conn)
        }
        const controller = startMutations(writerConns, profile, seedResult, manifest.seed, log)
        const readUntil = Date.now() + (phase.durationSeconds ?? 300) * 1000
        const readerRuns: QueryRun[][] = []
        while (Date.now() < readUntil) {
          readerRuns.push(await runQuerySuite('mixed', 1, 0, seedResult))
        }
        const stats = await controller.stop()
        for (const conn of writerConns) await conn.end()
        log(`mutations: ${JSON.stringify(stats)}`)

        // Source-to-visible lag, measured rather than assumed. The phase used
        // to sleep twice the gate's limit and call it settled, which cannot
        // fail and cannot report how far behind the replica actually was -
        // the number this whole phase exists to produce. A sentinel row is
        // written and then polled for, so what is recorded is the time the
        // replica took, not the time the harness waited.
        const sentinel = `lag-probe-${phase.id}`
        const lagStarted = performance.now()
        await mysqlConn!.query('INSERT INTO lag_probe (marker) VALUES (?)', [sentinel])
        let visibleAfterMs: number | null = null
        const lagDeadline = Date.now() + manifest.gates.maximumReplicationLagSeconds * 4000
        while (Date.now() < lagDeadline) {
          const rows = await queryPintail(
            `SELECT COUNT(*) AS n FROM lag_probe WHERE marker = '${sentinel}'`,
          ).catch(() => [] as unknown[][])
          if (Number(rows?.[0]?.[0] ?? 0) > 0) {
            visibleAfterMs = Math.round(performance.now() - lagStarted)
            break
          }
          await Bun.sleep(50)
        }
        if (visibleAfterMs === null) {
          log(
            `REPLICATION LAG EXCEEDED: sentinel not visible within ` +
              `${manifest.gates.maximumReplicationLagSeconds * 4}s`,
          )
          process.exitCode = 1
        } else {
          log(`source-to-visible lag: ${visibleAfterMs}ms`)
        }

        // Query latency WHILE the writers were running. The phase counted its
        // reader passes and threw the timings away, so "queries stay fast
        // under ingest" had no number behind it.
        const underLoad = new Map<string, number[]>()
        for (const pass of readerRuns) {
          for (const run of pass) {
            if (run.engine !== 'pintail' || run.medianMs === undefined) continue
            const seen = underLoad.get(run.id) ?? []
            seen.push(run.medianMs)
            underLoad.set(run.id, seen)
          }
        }
        const underLoadLatency = [...underLoad.entries()].map(([id, samples]) => {
          const sorted = [...samples].sort((left, right) => left - right)
          return {
            id,
            passes: sorted.length,
            medianMs: sorted[Math.floor(sorted.length / 2)],
            p95Ms: sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * 0.95) - 1)],
            maxMs: sorted[sorted.length - 1],
          }
        })
        const a = await mysqlFingerprints(mysqlConn!)
        const b = await pintailFingerprints(queryPintail)
        const mismatches = compareFingerprints(a, b)
        // Known-limitation allowlist (each entry links its tracking issue):
        // expected divergences warn, everything else is a gate failure.
        const allowlist = JSON.parse(
          readFileSync(join(benchmarkDir, 'expected-failures.json'), 'utf8'),
        ) as Array<{ table: string; phase: string; reason: string; link: string }>
        const expected = mismatches.filter((m) =>
          allowlist.some((entry) => entry.table === m.table && entry.phase === phase.id),
        )
        const unexpected = mismatches.filter((m) => !expected.includes(m))
        ;(report.phases as Record<string, unknown>)[phase.id] = {
          mutationStats: stats,
          readerPasses: readerRuns.length,
          sourceToVisibleLagMs: visibleAfterMs,
          underLoadLatency,
          fingerprintMismatches: unexpected,
          expectedFingerprintMismatches: expected,
        }
        for (const m of expected) {
          const entry = allowlist.find((e) => e.table === m.table && e.phase === phase.id)
          log(`expected divergence on ${m.table}: ${entry?.reason} (${entry?.link})`)
        }
        if (unexpected.length > 0) {
          log(`CONVERGENCE MISMATCHES: ${JSON.stringify(unexpected)}`)
          process.exitCode = 1
        }
        break
      }
      case 'compact-and-query': {
        if (!engines.includes('pintail')) break
        try {
          await api(`/api/databases/${pintailDb}/compact`, { method: 'POST' })
          log('compaction requested')
        } catch {
          log('compaction endpoint unavailable — timing post-steady-state instead')
        }
        const results = await runQuerySuite(phase.id, phase.runs ?? 7, 2, seedResult)
        ;(report.phases as Record<string, unknown>)[phase.id] = results
        break
      }
      case 'kill-restart-and-validate': {
        if (!engines.includes('pintail') || !pintailProcess) break
        log('killing pintail (SIGKILL)')
        pintailProcess.kill(9)
        await pintailProcess.exited
        await startPintail()
        const a = await mysqlFingerprints(mysqlConn!)
        const b = await pintailFingerprints(queryPintail)
        const mismatches = compareFingerprints(a, b)
        ;(report.phases as Record<string, unknown>)[phase.id] = { fingerprintMismatches: mismatches }
        if (mismatches.length > 0) log(`POST-RESTART MISMATCHES: ${JSON.stringify(mismatches)}`)
        break
      }
    }
    log(`phase ${phase.id} finished in ${Math.round((performance.now() - startedAt) / 1000)}s`)
  }

  report.finishedAt = new Date().toISOString()
  const resultsDir = join(workloadDir, 'results')
  mkdirSync(resultsDir, { recursive: true })
  writeFileSync(join(resultsDir, 'latest.json'), JSON.stringify(report, null, 2))
  writeFileSync(join(resultsDir, 'latest.md'), renderMarkdown(report))
  log(`results written to ${join(resultsDir, 'latest.{json,md}')}`)

  // The gate. Every failing outcome above records itself in the report and
  // carries on, so that one broken query still produces evidence for the
  // others - useful while developing, and wrong as an acceptance signal. A
  // run where queries errored, were unsupported, or disagreed with MySQL was
  // still exiting zero, which meant CI could report success for a workload
  // that never ran. Nothing downstream can tell that apart from a real pass,
  // so the check belongs here rather than in the reader of the artifact.
  const failures: string[] = []
  for (const [phaseId, data] of Object.entries(report.phases as Record<string, unknown>)) {
    if (!Array.isArray(data)) continue
    for (const run of data as QueryRun[]) {
      if (run.status === 'ok') continue
      // An unsupported query counts as a declared gap only where the workload
      // said so ahead of the run - window functions are v1's known hole. Any
      // other refusal is a query the engine could not answer, which is the
      // thing being measured, not an exemption from it.
      const declared = run.status === 'unsupported' && run.declaredGap === true
      if (declared) {
        log(`declared gap: ${phaseId}/${run.id} (${run.engine}) unsupported`)
        continue
      }
      failures.push(`${phaseId}/${run.id} (${run.engine}): ${run.status}`)
    }
  }
  if (failures.length > 0) {
    log(`GATE FAILED - ${failures.length} query outcome(s) not ok:`)
    for (const failure of failures) log(`  ${failure}`)
    process.exitCode = 1
  }
}

function renderMarkdown(report: Record<string, unknown>): string {
  const lines: string[] = [
    `# ${report.workload} — ${report.profile} profile`,
    '',
    `Run: ${report.startedAt} → ${report.finishedAt}. Engines: ${(report.engines as string[]).join(', ')}. Scale: ${report.scale}.`,
    '',
  ]
  for (const [phaseId, data] of Object.entries(report.phases as Record<string, unknown>)) {
    lines.push(`## Phase: ${phaseId}`, '')
    if (Array.isArray(data)) {
      lines.push('| Query | Engine | Status | Median ms | p95 ms |', '|---|---|---|---:|---:|')
      for (const run of data as QueryRun[]) {
        lines.push(
          `| ${run.id} | ${run.engine} | ${run.status} | ${run.medianMs?.toFixed(1) ?? '—'} | ${run.p95Ms?.toFixed(1) ?? '—'} |`,
        )
      }
    } else {
      lines.push('```json', JSON.stringify(data, null, 2), '```')
    }
    lines.push('')
  }
  return lines.join('\n')
}

async function teardown() {
  try { await mysqlConn?.end() } catch {}
  try { pintailProcess?.kill() } catch {}
  try { await docker('rm', '-f', mysqlName) } catch {}
}

main()
  .catch((error) => {
    console.error(error)
    process.exitCode = 1
  })
  .finally(teardown)

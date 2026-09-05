/// Session-resource harness: what the process holds for clients that run
/// nothing.
///
/// The concurrency harness (`run.ts`) measures queries arriving together.
/// This one measures the two things a client can accumulate while
/// executing nothing, which the query ceilings never see: open
/// connections, each a task plus a session plus an engine handle, and
/// prepared statements, each keeping its statement text for the life of
/// the session. Peak RSS is again the load-bearing number - it is what the
/// connection and prepared-statement ceilings are sized against.
///
/// Three phases, each sampled after it settles:
///   idle      N authenticated connections, no statements
///   prepared  P of them each holding M distinct prepared statements
///   released  every statement closed and every connection ended
///
/// Needs no MySQL: the sessions authenticate against a LOCAL database, so
/// the whole run is one process on this machine. Bun and the release
/// binary are the only requirements.
///
/// Run with: bun run sessions.ts
///           SESSIONS_IDLE=1000 SESSIONS_PREPARING=50 SESSIONS_STATEMENTS=1024 bun run sessions.ts
///           PINTAIL_LOAD_BINARY=target/release/pintail bun run sessions.ts

import { createServer } from 'node:net'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const DATABASE = 'sessions_db'

/// Idle connections to hold. The default is the connection ceiling itself,
/// so the number banked is the worst case an operator can reach without
/// raising the limit.
const IDLE = Number(process.env.SESSIONS_IDLE ?? '1000')
/// How many of those connections then fill their prepared-statement
/// allowance, and to how many statements. The default statement count is
/// the per-session ceiling.
const PREPARING = Number(process.env.SESSIONS_PREPARING ?? '50')
const STATEMENTS = Number(process.env.SESSIONS_STATEMENTS ?? '1024')

let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''

function log(message: string) {
  console.log(`[sessions] ${message}`)
}

async function command(argv: string[], options: { quiet?: boolean } = {}) {
  const child = Bun.spawn(argv, {
    cwd: repository,
    stdout: 'pipe',
    stderr: options.quiet ? 'pipe' : 'inherit',
  })
  const stdout = await new Response(child.stdout).text()
  const code = await child.exited
  if (code !== 0) throw new Error(`${argv.join(' ')} exited ${code}`)
  return { stdout }
}

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer()
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      if (typeof address === 'object' && address) {
        const { port } = address
        server.close(() => resolvePort(port))
      } else {
        reject(new Error('could not allocate a port'))
      }
    })
  })
}

/// Resident set of the server, in MB, from the process table - the same
/// reading the concurrency harness banks, so the two ledgers compare.
async function residentMb(): Promise<number> {
  if (!pintailProcess) return 0
  const { stdout } = await command(['ps', '-o', 'rss=', '-p', String(pintailProcess.pid)], {
    quiet: true,
  })
  return Number(stdout.trim()) / 1024
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

async function startPintail(binary: string) {
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-sessions-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  pintailProcess = Bun.spawn(
    [
      binary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${pintailHttpPort}`,
      '--wire-bind',
      `127.0.0.1:${pintailWirePort}`,
    ],
    {
      cwd: repository,
      stdout: 'ignore',
      stderr: 'inherit',
      env: {
        ...process.env,
        // The idle phase must not be cut short by the idle timeout, and the
        // ceilings are what is being measured, so both sit above the load.
        PINTAIL_WIRE_IDLE_TIMEOUT_SECONDS: '3600',
        PINTAIL_WIRE_MAX_CONNECTIONS: String(IDLE + 16),
        PINTAIL_WIRE_MAX_PREPARED_STATEMENTS: String(STATEMENTS),
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

async function api<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: body === undefined ? 'GET' : 'POST',
    headers: {
      'content-type': 'application/json',
      ...(token ? { authorization: `Bearer ${token}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  })
  if (!response.ok) throw new Error(`${path}: ${response.status} ${await response.text()}`)
  return (await response.json()) as T
}

/// A local database with one table, and a key its sessions authenticate
/// with. Local rather than replicated so the harness needs no MySQL.
async function provision(): Promise<string> {
  token = (
    await api<{ token: string }>('/api/auth/setup', {
      email: 'sessions@example.com',
      password: 'sessions-harness-pass',
    })
  ).token
  const database = await api<{ id: string }>('/api/databases/local', { name: DATABASE })
  const key = await api<{ secret: string }>(`/api/databases/${database.id}/api-keys`, {
    name: 'sessions',
    scopes: ['query', 'read'],
  })
  const bootstrap = await mysql.createConnection({
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: key.secret,
    database: DATABASE,
  })
  // PREPARE previews each statement against the catalog, so the catalog
  // needs a table in it.
  await bootstrap.query('CREATE TABLE probe (id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (id))')
  await bootstrap.end()
  return key.secret
}

interface Sample {
  phase: string
  detail: string
  rssMb: number
}

async function main() {
  const binary = await buildPintail()
  await startPintail(binary)
  const samples: Sample[] = []
  const sample = async (phase: string, detail: string) => {
    // Let the allocator's background purge and the accept loop settle so a
    // reading is the phase's resting state rather than its wake.
    await Bun.sleep(2_000)
    const rssMb = await residentMb()
    samples.push({ phase, detail, rssMb })
    log(`${phase}: ${detail} — RSS ${rssMb.toFixed(0)} MB`)
  }
  const connections: mysql.Connection[] = []
  try {
    const secret = await provision()
    await sample('baseline', 'server provisioned, no sessions')

    log(`opening ${IDLE} idle connections`)
    for (let opened = 0; opened < IDLE; opened += 1) {
      connections.push(
        await mysql.createConnection({
          host: '127.0.0.1',
          port: pintailWirePort,
          user: DATABASE,
          password: secret,
          database: DATABASE,
        }),
      )
    }
    await sample('idle', `${IDLE} authenticated connections, no statements`)

    log(`preparing ${STATEMENTS} statements on each of ${PREPARING} connections`)
    const held: Array<{ close(): Promise<void> }> = []
    for (const connection of connections.slice(0, PREPARING)) {
      for (let index = 0; index < STATEMENTS; index += 1) {
        // Distinct text per statement: the driver caches by SQL and the
        // server keeps the text, so a repeated statement measures neither.
        held.push(await connection.prepare(`SELECT id, ${index} AS n FROM probe WHERE id = ?`))
      }
    }
    await sample(
      'prepared',
      `${PREPARING} sessions × ${STATEMENTS} prepared statements (${held.length} total)`,
    )

    log('closing every statement')
    for (const statement of held) await statement.close()
    await sample('closed', 'statements closed, connections still open')

    log('ending every connection')
    for (const connection of connections.splice(0)) await connection.end()
    await sample('released', 'every connection ended')

    const now = new Date().toISOString()
    const baseline = samples[0]!.rssMb
    const lines = [
      '# Pintail session-resource results',
      '',
      `Measured ${now} on ${process.platform}/${process.arch}.`,
      '',
      `Idle connections: ${IDLE}. Preparing sessions: ${PREPARING} × ${STATEMENTS} statements.`,
      'Every session authenticates against a local database; no query runs.',
      '',
      '| phase | detail | RSS MB | over baseline MB |',
      '|---|---|---:|---:|',
      ...samples.map(
        (entry) =>
          `| ${entry.phase} | ${entry.detail} | ${entry.rssMb.toFixed(0)} | ${(entry.rssMb - baseline).toFixed(0)} |`,
      ),
      '',
      perSession(samples),
      '',
      'Per-session and per-statement costs are the differences between phases',
      'divided by the counts; they are what the connection and prepared-statement',
      'ceilings (`--wire-max-connections`, `--wire-max-prepared-statements`) are',
      'sized against. "released" says what the process gives back: a resting RSS',
      'well above baseline after every session ended would be a leak.',
      '',
    ]
    writeFileSync(join(import.meta.dir, 'results-sessions.md'), lines.join('\n'))
    writeFileSync(
      join(import.meta.dir, 'results-sessions.json'),
      JSON.stringify({ measuredAt: now, idle: IDLE, preparing: PREPARING, statements: STATEMENTS, samples }, null, 2),
    )
    log('banked results-sessions.md')
  } finally {
    for (const connection of connections) await connection.end().catch(() => {})
    pintailProcess?.kill()
    await pintailProcess?.exited
    rmSync(pintailDataDir, { recursive: true, force: true })
  }
}

function perSession(samples: Sample[]): string {
  const by = Object.fromEntries(samples.map((entry) => [entry.phase, entry.rssMb]))
  const perConnectionKb = ((by.idle! - by.baseline!) * 1024) / IDLE
  const perStatementKb = ((by.prepared! - by.idle!) * 1024) / (PREPARING * STATEMENTS)
  return [
    `Per idle connection: ${perConnectionKb.toFixed(1)} KB.`,
    `Per prepared statement: ${perStatementKb.toFixed(2)} KB.`,
    `At the default ceilings (1000 connections, 1024 statements each), a client that`,
    `fills both holds at most ${((perConnectionKb * 1000 + perStatementKb * 1000 * 1024) / 1024).toFixed(0)} MB`,
    `of session state before the first query runs.`,
  ].join(' ')
}

await main()

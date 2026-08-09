/// Browser smoke suite: the dashboard, driven like an operator would.
///
/// Boots a real MySQL source in Docker and the release pintail binary, then
/// walks the embedded dashboard in headless Chromium: first-boot operator
/// setup, the add-database wizard (connection test, capability probe, table
/// selection, snapshot start), replication reaching streaming, and the SQL
/// console returning typed results over /api/query. A second pass loads the
/// login screen at a 390-pixel phone viewport.
///
/// Run with: bun run smoke              (builds the release binary)
///           PINTAIL_E2E_BINARY=... bun run smoke
///
/// Screenshots land in tests/browser/artifacts/ on every failure.

import { createServer } from 'node:net'
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { chromium } from 'playwright'
import type { Browser, Page } from 'playwright'
import { redactBootSecrets } from './output'

const repository = resolve(import.meta.dir, '..', '..')
const artifacts = join(import.meta.dir, 'artifacts')
const cargoBinary = join(homedir(), '.cargo', 'bin', 'cargo')
const cargoTargetDir = join(repository, 'target')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-browser-mysql-${process.pid}-${nonce}`
const DATABASE = 'smoke_db'
const OPERATOR = { email: 'smoke@pintail.local', password: 'browser-smoke-password' }

interface CheckResult {
  check: string
  status: 'PASS' | 'FAIL'
  detail?: string
}

const results: CheckResult[] = []
let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailStdout: Promise<string> | undefined
let pintailStderr: Promise<string> | undefined
let browser: Browser | undefined
let page: Page | undefined
let pintailDataDir = ''
let pintailUrl = ''
let mysqlStarted = false

function log(message: string) {
  console.log(`[browser] ${message}`)
}

async function command(args: string[], options: { quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: repository,
    env: { ...process.env, CARGO_TARGET_DIR: cargoTargetDir },
    stdout: 'pipe',
    stderr: 'pipe',
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
  return { stdout: stdout.trim() }
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
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready in time')
}

async function check(name: string, action: () => Promise<void>) {
  try {
    await action()
    results.push({ check: name, status: 'PASS' })
    log(`PASS ${name}`)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    results.push({ check: name, status: 'FAIL', detail })
    log(`FAIL ${name} — ${detail}`)
    if (page) {
      mkdirSync(artifacts, { recursive: true })
      const file = join(artifacts, `${name.replaceAll(/[^a-z0-9]+/gi, '-')}.png`)
      await page.screenshot({ path: file, fullPage: true }).catch(() => {})
      log(`screenshot: ${file}`)
    }
  }
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_E2E_BINARY) return resolve(process.env.PINTAIL_E2E_BINARY)
  log('building the release pintail binary')
  await command([cargoBinary, 'build', '--release', '-p', 'pintail'])
  const metadata = await command([cargoBinary, 'metadata', '--format-version', '1', '--no-deps'], {
    quiet: true,
  })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
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
    '/var/lib/mysql:rw,size=1g',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env',
    `MYSQL_DATABASE=${DATABASE}`,
    'mysql:8.4',
    '--server-id=943',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--binlog-row-metadata=FULL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  const sql = async (statement: string) => {
    await mysqlConnection!.query(statement)
  }
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )
  // The SQL console's default query is `SELECT * FROM events LIMIT 100`, so
  // the seed table is named events and the smoke exercises the console
  // exactly as it first opens.
  await sql(`CREATE TABLE events (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    kind VARCHAR(32) NOT NULL,
    amount DECIMAL(10,2) NOT NULL,
    happened_at DATETIME NOT NULL
  )`)
  await sql(`INSERT INTO events (kind, amount, happened_at) VALUES
    ('signup', 0.00, '2026-08-01 10:00:00'),
    ('purchase', 49.90, '2026-08-01 11:30:00'),
    ('purchase', 12.50, '2026-08-02 09:15:00'),
    ('refund', -12.50, '2026-08-02 16:45:00')`)

  const binary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-browser-'))
  const httpPort = await freePort()
  const wirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${httpPort}`
  pintailProcess = Bun.spawn(
    [
      binary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${httpPort}`,
      '--wire-bind',
      `127.0.0.1:${wirePort}`,
    ],
    { cwd: repository, stdout: 'pipe', stderr: 'pipe' },
  )
  pintailStdout = new Response(pintailProcess.stdout).text()
  pintailStderr = new Response(pintailProcess.stderr).text()
  for (let attempt = 0; ; attempt += 1) {
    try {
      const response = await fetch(`${pintailUrl}/health`)
      if (response.ok) break
    } catch {}
    if (attempt >= 240) throw new Error('pintail did not become healthy within 120 seconds')
    await Bun.sleep(500)
  }

  browser = await chromium.launch()
  page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
  page.setDefaultTimeout(20_000)

  await check('first boot shows operator setup', async () => {
    await page!.goto(pintailUrl)
    await page!.getByRole('heading', { name: 'Create the operator' }).waitFor()
  })

  await check('operator setup signs in', async () => {
    await page!.getByLabel('Email').fill(OPERATOR.email)
    await page!.getByLabel('Password').fill(OPERATOR.password)
    await page!.getByRole('button', { name: 'Initialize Pintail' }).click()
    await page!.getByText('Node healthy').waitFor()
  })

  await check('wizard tests the connection and probes capabilities', async () => {
    await page!.getByRole('link', { name: 'Add database' }).first().click()
    await page!.getByLabel('MySQL schema').fill(DATABASE)
    await page!
      .getByLabel('MySQL DSN')
      .fill(`mysql://pintail:pintail@${host}:${mysqlPort}/${DATABASE}`)
    await page!.getByRole('button', { name: 'Test connection' }).click()
    // Step 2 headline is the probed server version; the CDC recommendation
    // proves every capability check ran against the live source.
    await page!.getByText('Recommended: CDC').waitFor()
  })

  await check('wizard selects tables and starts the snapshot', async () => {
    await page!.getByRole('button', { name: 'Choose tables' }).click()
    await page!.getByText('events', { exact: true }).waitFor()
    await page!.getByRole('button', { name: 'Review & start' }).click()
    await page!.waitForURL(/\/databases\/[^/?]+\?tab=snapshot$/)
    await page!.getByRole('heading', { name: DATABASE }).waitFor()
  })

  await check('replication reaches streaming', async () => {
    // The databases list renders each mirror's live state; streaming means
    // snapshot completed and the CDC supervisor took over. A reload drops
    // the SPA back on the overview page, so navigate to the list each pass.
    const deadline = Date.now() + 120_000
    for (;;) {
      await page!.reload()
      await page!.getByRole('link', { name: 'Databases', exact: true }).click()
      await Bun.sleep(1_000)
      const body = (await page!.textContent('body')) ?? ''
      if (/streaming/i.test(body)) break
      if (Date.now() > deadline) throw new Error('database never showed streaming state')
      await Bun.sleep(2_000)
    }
  })

  await check('SQL console returns typed results', async () => {
    await page!.getByRole('link', { name: 'SQL Console', exact: true }).click()
    await page!.getByRole('heading', { name: 'SQL Console' }).waitFor()
    await page!.getByRole('button', { name: 'Run' }).click()
    await page!.getByText('4 rows ·').waitFor()
    await page!.getByRole('cell', { name: 'purchase' }).first().waitFor()
  })

  await check('login screen renders at a phone viewport', async () => {
    // A fresh context has no stored session, so it lands on the login form.
    const context = await browser!.newContext({ viewport: { width: 390, height: 844 } })
    try {
      const phone = await context.newPage()
      await phone.goto(pintailUrl)
      await phone.getByLabel('Email').waitFor()
      const overflow = await phone.evaluate(
        () => document.documentElement.scrollWidth > window.innerWidth + 1,
      )
      if (overflow) throw new Error('login page overflows horizontally at 390px')
    } finally {
      await context.close()
    }
  })

  const failed = results.filter((result) => result.status === 'FAIL')
  log(`gate: ${failed.length === 0 ? 'PASS' : 'FAIL'} (${results.length - failed.length} passed, ${failed.length} failed)`)
  if (failed.length > 0) process.exitCode = 1
}

async function cleanup() {
  await browser?.close().catch(() => {})
  pintailProcess?.kill()
  await pintailProcess?.exited.catch(() => {})
  if (process.exitCode) {
    const [stdout, stderr] = await Promise.all([
      pintailStdout ?? Promise.resolve(''),
      pintailStderr ?? Promise.resolve(''),
    ])
    const captured = redactBootSecrets(`${stdout}${stderr}`).trim()
    if (captured) console.error(captured)
  }
  await mysqlConnection?.end().catch(() => {})
  if (mysqlStarted) await docker('rm', '-f', mysqlName).catch(() => {})
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
}

try {
  await main()
} catch (error) {
  log(`fatal: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await cleanup()
}

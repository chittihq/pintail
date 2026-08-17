/// Browser smoke suite: the dashboard, driven like an operator would.
///
/// Boots a real MySQL source and a real S3-compatible store (RustFS) in
/// Docker alongside the release pintail binary, then walks the embedded
/// dashboard in headless Chromium: first-boot operator setup, the
/// add-database wizard (connection test, capability probe, table selection,
/// snapshot start), replication reaching streaming, the SQL console returning
/// typed results over /api/query, workspace create and switch, API key
/// lifecycle, replication mode changes and resnapshot, a backup destination
/// saved, run and restored side-by-side, dead-letter discard and an
/// unrecoverable retry, team invite and revoke, and the activity and settings
/// surfaces. A second pass loads the login screen at a 390-pixel phone
/// viewport.
///
/// The object store is real rather than stubbed because the backup UI is
/// gated on a destination the server confirmed it could reach, and restore
/// has to read back the objects that same server wrote.
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
const rustfsName = `pintail-browser-rustfs-${process.pid}-${nonce}`
// Pinned rather than :latest - RustFS is pre-1.0 and the gate should not
// change behaviour because an upstream tag moved.
const RUSTFS_IMAGE = 'rustfs/rustfs:1.0.0-beta.12'
const RUSTFS = { user: 'rustfsadmin', password: 'rustfs-secret', bucket: 'pintail-browser' }
const DATABASE = 'smoke_db'
// A schema the probe user can reach but holds no table privilege on.
const RESTRICTED_DATABASE = 'restricted_db'
const API_KEY_NAME = 'browser-gate-key'
const RESTORED_DATABASE = 'browser gate restore'
// Two keyless tables, not one. A quarantine durably marks its table as
// needing resync, and the CDC loop skips every later event for a blocked
// target, so one table yields exactly one dead letter until it is resynced.
const APPEND_TABLE = 'notes'
const APPEND_TABLE_RETRY = 'memos'
const INVITE_EMAIL = 'teammate@pintail.local'
// The seeded table the console assertions complete against. Named here so the
// completion check cannot accidentally pass on a SQL keyword.
const DATABASE_TABLE = 'events'
const OPERATOR = { email: 'smoke@pintail.local', password: 'browser-smoke-password' }

interface CheckResult {
  check: string
  status: 'PASS' | 'FAIL'
  detail?: string
}

const results: CheckResult[] = []
/// Browser-side errors, captured so a failing check can report what the page
/// itself complained about. A silent no-op in the dashboard - a rejected
/// dynamic import, a handler that threw - is invisible from Playwright's side
/// otherwise, and reads identically to a feature that did nothing.
const pageErrors: string[] = []
let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailStdout: Promise<string> | undefined
let pintailStderr: Promise<string> | undefined
let browser: Browser | undefined
let page: Page | undefined
let pintailDataDir = ''
let pintailUrl = ''
let mysqlStarted = false
let rustfsStarted = false
let rustfsEndpoint = ''

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
    for (const error of pageErrors.slice(-5)) log(`  browser ${error}`)
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
  // A user that can connect and satisfy the capability probe but holds no
  // privilege on any table. information_schema.TABLES lists only tables the
  // caller can access, so this is what a real misconfigured grant looks
  // like: connection fine, probe green, table list empty.
  // Its own schema, because the smoke schema is registered by the wizard
  // check and Pintail refuses to register the same source twice.
  await sql(`CREATE DATABASE ${RESTRICTED_DATABASE}`)
  await sql(`CREATE TABLE ${RESTRICTED_DATABASE}.hidden (id INT PRIMARY KEY)`)
  await sql(`CREATE USER 'nogrants'@'%' IDENTIFIED BY 'nogrants'`)
  await sql(
    `GRANT RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'nogrants'@'%'`,
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
  // No primary key and no unique key, so this table replicates in
  // append_row_id mode. That is what makes a dead letter reproducible: an
  // UPDATE or DELETE against it has no stable source key, so CDC quarantines
  // the row rather than guessing which duplicate to touch. Inserts are
  // unaffected, which is why the snapshot and streaming checks still pass.
  for (const table of [APPEND_TABLE, APPEND_TABLE_RETRY]) {
    await sql(`CREATE TABLE ${table} (body VARCHAR(64) NOT NULL)`)
    await sql(`INSERT INTO ${table} (body) VALUES ('first'), ('second')`)
  }

  // A real S3-compatible destination. The backup pages cannot be exercised
  // against a fake: "Backup now" stays disabled until the server confirms a
  // destination it could actually reach, and restore has to read back objects
  // this same server wrote.
  log(`starting RustFS ${rustfsName}`)
  await docker(
    'run',
    '--detach',
    '--name',
    rustfsName,
    '--publish',
    '0:9000',
    '--env',
    `RUSTFS_ACCESS_KEY=${RUSTFS.user}`,
    '--env',
    `RUSTFS_SECRET_KEY=${RUSTFS.password}`,
    RUSTFS_IMAGE,
  )
  rustfsStarted = true
  const rustfsPort = await publishedPort(rustfsName, 9000)
  rustfsEndpoint = `http://${host}:${rustfsPort}`
  for (let attempt = 0; ; attempt += 1) {
    try {
      const response = await fetch(`${rustfsEndpoint}/health`)
      if (response.ok) break
    } catch {}
    if (attempt >= 120) throw new Error('RustFS did not become ready in time')
    await Bun.sleep(500)
  }
  // mc runs on the Docker host and reaches RustFS over the container's own
  // network namespace, so bucket creation does not depend on the published
  // port being reachable from here. mc is an S3 client, not a MinIO-only one.
  await docker(
    'run',
    '--rm',
    '--network',
    `container:${rustfsName}`,
    '--entrypoint',
    'sh',
    'minio/mc:latest',
    '-c',
    `mc alias set local http://127.0.0.1:9000 ${RUSTFS.user} ${RUSTFS.password} >/dev/null` +
      ` && mc mb --ignore-existing local/${RUSTFS.bucket} >/dev/null`,
  )

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
  page.on('pageerror', (error) => pageErrors.push(`pageerror: ${error.message}`))
  page.on('console', (message) => {
    if (message.type() === 'error') pageErrors.push(`console: ${message.text()}`)
  })

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

  await check('the console completes real table names and formats SQL', async () => {
    // Completion is fed from the local replica, so this only passes once the
    // snapshot has landed - which is why it runs after the streaming check
    // rather than beside the other console assertions.
    await page!.getByRole('link', { name: 'SQL Console', exact: true }).click()
    await page!.getByRole('heading', { name: 'SQL Console' }).waitFor()
    const editor = page!.locator('.cm-content')
    await editor.waitFor({ timeout: 20_000 })

    // Type a prefix of a table that exists only in THIS source, so a passing
    // assertion cannot come from a built-in keyword list.
    await editor.click()
    await page!.keyboard.press('ControlOrMeta+A')
    await page!.keyboard.type('SELECT * FROM even')
    const option = page!.locator('.cm-tooltip-autocomplete').getByText(DATABASE_TABLE, { exact: true })
    await option.waitFor({ timeout: 15_000 })
    await page!.keyboard.press('Escape')

    // Columns complete after a table qualifier, which is the part that needs
    // the schema map rather than a plain identifier list.
    await page!.keyboard.press('ControlOrMeta+A')
    await page!.keyboard.type(`SELECT ${DATABASE_TABLE}.ki`)
    const column = page!.locator('.cm-tooltip-autocomplete').getByText('kind', { exact: true })
    await column.waitFor({ timeout: 15_000 })
    await page!.keyboard.press('Escape')

    // Formatting rewrites the text without changing what it means. The input
    // is deliberately ugly - one line, lowercase, collapsed whitespace.
    await page!.keyboard.press('ControlOrMeta+A')
    await page!.keyboard.type(`select id,kind from ${DATABASE_TABLE} where kind='purchase' order by id`)
    await page!.getByTestId('format-sql').click()
    // Polled rather than slept on. Formatting dynamically imports a 256KB
    // chunk, so the rewrite lands after the click by an amount that depends on
    // the machine - a fixed delay passes here and fails on a loaded CI host.
    const formatDeadline = Date.now() + 20_000
    let formatted = ''
    for (;;) {
      formatted = (await editor.innerText()).trim()
      if (formatted.includes('\n')) break
      if (Date.now() > formatDeadline) {
        throw new Error(`format never rewrote the buffer: ${formatted}`)
      }
      await Bun.sleep(250)
    }
    if (!formatted.includes('\n')) throw new Error(`format produced one line: ${formatted}`)
    if (!/FROM/.test(formatted)) throw new Error(`format did not upper-case keywords: ${formatted}`)
    // And the query still runs, which is the claim that matters: a formatter
    // that produced pretty but invalid SQL would pass every check above.
    await page!.getByRole('button', { name: 'Run' }).click()
    await page!.getByText('2 rows ·').waitFor({ timeout: 20_000 })
  })

  await check('creating a workspace closes its dialog', async () => {
    // Asserts the dialog CLOSES, not merely that the workspace was created.
    // The bug this covers left the POST succeeding while the dialog stayed
    // open with its spinner running, because the handler awaited an SSE
    // consumer that never returns - so every assertion about the workspace
    // existing passed while the UI was wedged.
    await page!.getByRole('button', { name: 'Pintail' }).click()
    await page!.getByRole('menuitem', { name: 'Create workspace' }).click()
    const dialog = page!.getByRole('dialog')
    await dialog.getByRole('heading', { name: 'Create a workspace' }).waitFor()
    await dialog.getByLabel('Name').fill('Browser gate workspace')
    await dialog.getByRole('button', { name: 'Create workspace' }).click()
    // The whole point: the dialog must go away on its own.
    await dialog.waitFor({ state: 'hidden', timeout: 15_000 })
    // And the new workspace must be the active one in the switcher.
    await page!.getByText('Browser gate workspace').first().waitFor()
  })

  await check('switching workspaces settles on the chosen one', async () => {
    // switchWorkspace goes through the same enterWorkspace that wedged the
    // create dialog, so it was broken by the identical mechanism and nothing
    // covered it. The assertion is that the switcher LABEL changes - proof
    // the handler ran to completion - rather than that the API returned.
    await page!.getByRole('button', { name: 'Pintail' }).click()
    await page!.getByRole('menuitem', { name: 'My workspace' }).click()
    await page!
      .getByRole('button', { name: 'Pintail' })
      .filter({ hasText: 'My workspace' })
      .waitFor({ timeout: 15_000 })
    // Switching rebuilds the session, so the databases of the original
    // workspace must be visible again rather than the new one's emptiness.
    await page!.getByRole('link', { name: 'Databases', exact: true }).click()
    await page!.getByText(DATABASE).first().waitFor({ timeout: 15_000 })
  })

  await check('a workspace switch never flashes the connection wizard', async () => {
    // enterWorkspace clears the database cache before it reloads, and the
    // databases page keyed its empty state on the cache alone - so a switch
    // started from /databases flashed "No databases yet / Start the
    // connection wizard" for the whole reload, then navigated away. (The
    // switcher's redirect to Overview fires only AFTER the reload, which is
    // why the flash happens on the page the operator is still looking at.)
    // Locally the reload completes in milliseconds and no assertion after
    // settling can see it, so the reload is held open by delaying
    // /api/databases and the page inspected mid-window.
    //
    // Leg 1: a workspace with nothing in it must still end at the wizard -
    // the fix must not suppress the genuine empty state.
    await page!.getByRole('button', { name: 'Pintail' }).click()
    await page!.getByRole('menuitem', { name: 'Browser gate workspace' }).click()
    await page!
      .getByRole('button', { name: 'Pintail' })
      .filter({ hasText: 'Browser gate workspace' })
      .waitFor({ timeout: 15_000 })
    await page!.getByRole('link', { name: 'Databases', exact: true }).click()
    await page!.getByText('Start the connection wizard').waitFor({ timeout: 15_000 })

    // Leg 2: switch back to the populated workspace with the reload held
    // open. The redirect to Overview only fires after the reload completes,
    // so mid-window we are still on /databases - exactly where the flash
    // showed. The wizard must leave the moment the switch starts, not
    // linger until the redirect displaces it.
    await page!.route('**/api/databases', async (route) => {
      await Bun.sleep(700)
      // The delay can outlive the unroute below; a route torn down
      // mid-sleep has already been handled, and the throw would land as an
      // unhandled rejection that fails the run after every check passed.
      await route.continue().catch(() => {})
    })
    try {
      await page!.getByRole('button', { name: 'Pintail' }).click()
      await page!.getByRole('menuitem', { name: 'My workspace' }).click()
      await Bun.sleep(350)
      const body = (await page!.textContent('body')) ?? ''
      if (body.includes('Start the connection wizard') || body.includes('No databases yet')) {
        throw new Error('the connection wizard is showing mid-switch to a populated workspace')
      }
    } finally {
      await page!.unroute('**/api/databases')
    }
    // Settle where the next check expects to be: back in the populated
    // workspace, on the databases list, rows visible.
    await page!
      .getByRole('button', { name: 'Pintail' })
      .filter({ hasText: 'My workspace' })
      .waitFor({ timeout: 15_000 })
    await page!.getByRole('link', { name: 'Databases', exact: true }).click()
    await page!.getByText(DATABASE).first().waitFor({ timeout: 15_000 })
  })

  await check('View opens a data preview without leaving the tables list', async () => {
    // The preview reads through the query engine - the same merge-on-read
    // path the SQL console uses - so the assertion is on a seeded VALUE, not
    // just on the dialog appearing: a dialog that opened but queried the
    // wrong table or database would show the right title over wrong rows.
    await page!.goto(`${pintailUrl}/databases`)
    await page!.getByRole('link', { name: DATABASE }).first().click()
    await page!.getByRole('row', { name: /events/ }).getByRole('button', { name: 'View' }).click()
    const dialog = page!.getByRole('dialog')
    await dialog.getByRole('heading', { name: 'events' }).waitFor({ timeout: 15_000 })
    await dialog.getByText('49.90').waitFor({ timeout: 15_000 })
    await dialog.getByText(/First 4 rows/).waitFor()
    await page!.keyboard.press('Escape')
    await dialog.waitFor({ state: 'hidden', timeout: 10_000 })
    // And the operator is still where they acted from: the tables list, not
    // the snapshot tab and not another page.
    await page!.getByRole('button', { name: 'View' }).first().waitFor()
  })

  await check('an empty table list explains itself', async () => {
    // Regression for a wizard that rendered an empty bordered box with
    // Review & start disabled and no reason given. The cause is almost never
    // an empty schema - information_schema hides tables the connecting user
    // cannot access - so the empty state has to say so or the operator has
    // nothing to act on.
    await page!.goto(`${pintailUrl}/databases/new`)
    await page!.getByLabel('MySQL schema').fill(RESTRICTED_DATABASE)
    await page!
      .getByLabel('MySQL DSN')
      .fill(`mysql://nogrants:nogrants@${host}:${mysqlPort}/${RESTRICTED_DATABASE}`)
    await page!.getByRole('button', { name: 'Test connection' }).click()
    await page!.getByText('Recommended:').waitFor({ timeout: 20_000 })
    await page!.getByRole('button', { name: 'Choose tables' }).click()
    const empty = page!.getByTestId('wizard-no-tables')
    await empty.waitFor({ timeout: 20_000 })
    // The message must name the actual cause and the fix, not just report
    // emptiness.
    await empty.getByText('privilege').first().waitFor()
    await empty.getByText('GRANT SELECT').first().waitFor()
  })

  await check('an API key is created once, disabled and revoked', async () => {
    // Every assertion here is re-checked after a reload. The page reloads
    // from the server after each mutation, so an assertion made against the
    // live DOM alone would pass on a request that never reached the server -
    // which is exactly how the create-workspace bug survived its test.
    await page!.goto(`${pintailUrl}/keys`)
    await page!.getByRole('heading', { name: 'API Keys' }).waitFor()
    await page!.getByLabel('Name').fill(API_KEY_NAME)
    await page!.getByRole('button', { name: 'Create' }).click()

    // The secret is shown exactly once, so capture it and prove it is real.
    const secret = page!.getByTestId('revealed-secret')
    await secret.waitFor({ timeout: 15_000 })
    const revealed = (await secret.textContent())?.trim() || ''
    if (revealed.length < 16) throw new Error(`implausible key secret: ${revealed.length} chars`)

    const row = page!.getByRole('row').filter({ hasText: API_KEY_NAME })
    await row.waitFor()
    await row.getByText('enabled').waitFor()

    // "Shown once" is a security claim, not a UI detail: after a reload the
    // secret must be gone, because the server stores only its SHA-256 hash.
    await page!.reload()
    await row.waitFor({ timeout: 15_000 })
    if (await secret.isVisible()) throw new Error('the key secret survived a reload')

    // Disable, and require the change to survive a reload rather than
    // trusting the badge the click swapped in.
    await row.getByRole('button', { name: 'Disable' }).click()
    await row.getByText('disabled').waitFor({ timeout: 15_000 })
    await page!.reload()
    await row.getByText('disabled').waitFor({ timeout: 15_000 })

    // Revoke. The row must be gone after a reload too - a DELETE that 4xx'd
    // would still empty the local list on an optimistic implementation.
    await page!.getByRole('button', { name: `Delete ${API_KEY_NAME}` }).click()
    await row.waitFor({ state: 'detached', timeout: 15_000 })
    await page!.reload()
    await page!.getByRole('heading', { name: 'API Keys' }).waitFor()
    if ((await row.count()) > 0) throw new Error('the revoked key came back after a reload')
  })

  await check('pausing, changing mode and resnapshotting all take effect', async () => {
    // Mode is server state that the page re-polls every 8 seconds, so a
    // control that only updated the local ref would look correct for one tick
    // and then silently revert. Every assertion below is therefore made after
    // leaving the page and coming back, which forces a fresh load from the
    // control plane.
    //
    // Re-entry is a client-side navigation rather than reload(). The detail
    // route is dynamic and is not prerendered, so a hard load can land on the
    // SPA fallback instead of the database - which would assert nothing.
    const reopen = async () => {
      await page!.getByRole('link', { name: 'Databases', exact: true }).click()
      await page!.getByRole('link', { name: DATABASE }).first().click()
      await page!.getByRole('heading', { name: DATABASE }).waitFor({ timeout: 15_000 })
    }
    const openSettingsTab = async () => {
      await page!.getByRole('tab', { name: 'settings' }).click()
      await page!.getByText('Requested mode').waitFor({ timeout: 15_000 })
    }

    await reopen()

    // Pause. The button label is the state: it flips to Resume only if the
    // server accepted, because the page renders database.mode.
    await page!.getByRole('button', { name: 'Pause' }).click()
    await page!.getByRole('button', { name: 'Resume' }).waitFor({ timeout: 15_000 })
    await reopen()
    await page!.getByRole('button', { name: 'Resume' }).waitFor({ timeout: 15_000 })

    // Resume, likewise.
    await page!.getByRole('button', { name: 'Resume' }).click()
    await page!.getByRole('button', { name: 'Pause' }).waitFor({ timeout: 15_000 })
    await reopen()
    await page!.getByRole('button', { name: 'Pause' }).waitFor({ timeout: 15_000 })

    // The Requested mode select reaches modes the pause button cannot, and
    // every one of them used to be confirmed as "Replication resumed".
    await openSettingsTab()
    await page!.getByRole('combobox').last().click()
    await page!.getByRole('option', { name: 'Polling' }).click()
    await page!.getByText('Replication mode set to POLLING').waitFor({ timeout: 15_000 })
    await reopen()
    await openSettingsTab()
    await page!.getByRole('combobox').last().getByText('Polling').waitFor({ timeout: 15_000 })

    // Put it back, so the resnapshot below and any later check start from the
    // mode the wizard chose.
    await page!.getByRole('combobox').last().click()
    await page!.getByRole('option', { name: 'Auto' }).click()
    // Not "resumed": this leaves polling, not paused. That distinction is the
    // whole point of the toast change.
    await page!.getByText('Replication mode set to auto').waitFor({ timeout: 15_000 })

    // Resnapshot must actually re-run rather than merely being acknowledged,
    // so this waits for the mirror to reach streaming again.
    //
    // Retried, because the control plane holds ONE job slot per database and
    // answers 409 while a supervisor cycle owns it. That is correct server
    // behaviour - two concurrent snapshots of one mirror would be worse - so
    // the click is repeated until it lands rather than the rejection being
    // treated as a failure.
    const resnapshotDeadline = Date.now() + 90_000
    for (;;) {
      await page!.getByRole('button', { name: 'Resnapshot' }).click()
      const accepted = await page!
        .getByText('Resnapshot accepted')
        .waitFor({ timeout: 10_000 })
        .then(() => true)
        .catch(() => false)
      if (accepted) break
      if (Date.now() > resnapshotDeadline) {
        throw new Error('resnapshot was refused for 90s - the job slot never freed')
      }
      await Bun.sleep(3_000)
    }
    const deadline = Date.now() + 120_000
    for (;;) {
      await page!.getByRole('link', { name: 'Databases', exact: true }).click()
      await Bun.sleep(1_000)
      if (/streaming/i.test((await page!.textContent('body')) ?? '')) break
      if (Date.now() > deadline) throw new Error('database never returned to streaming')
      await Bun.sleep(2_000)
      await page!.getByRole('link', { name: DATABASE }).first().click()
    }
  })

  await check('a backup destination saves, runs and restores', async () => {
    await page!.goto(`${pintailUrl}/backups`)
    await page!.getByRole('heading', { name: 'Backups' }).waitFor()

    // Backup now is gated on a *server-confirmed* destination, so this also
    // proves the gate is real rather than a disabled attribute that never
    // flips.
    const backupNow = page!.getByRole('button', { name: 'Backup now' })
    if (!(await backupNow.isDisabled())) {
      throw new Error('Backup now must be disabled before a destination is saved')
    }

    await page!.getByLabel('Bucket').fill(RUSTFS.bucket)
    await page!.getByLabel('Object prefix').fill('browser-gate')
    await page!.getByLabel('Endpoint', { exact: false }).fill(rustfsEndpoint)
    await page!.getByLabel('Region').fill('us-east-1')
    await page!.getByLabel('Access key ID').fill(RUSTFS.user)
    await page!.getByLabel('Secret access key').fill(RUSTFS.password)
    await page!.getByRole('button', { name: 'Save destination' }).click()
    await page!.getByText('Backup destination saved').waitFor({ timeout: 20_000 })
    await page!.getByText('Configured', { exact: true }).waitFor({ timeout: 20_000 })

    // Credentials are write-only: the form clears them on reload rather than
    // rendering a secret the server accepted. Re-reading them would mean the
    // API hands back a stored secret access key.
    await page!.goto(`${pintailUrl}/backups`)
    await page!.getByText('Configured', { exact: true }).waitFor({ timeout: 20_000 })
    if ((await page!.getByLabel('Secret access key').inputValue()) !== '') {
      throw new Error('the stored secret access key was rendered back into the form')
    }

    // Run one, and require it to reach completed rather than merely accepted -
    // a destination that 403s still produces a row, with status failed.
    await page!.getByRole('button', { name: 'Backup now' }).click()
    await page!.getByText('Backup started').waitFor({ timeout: 20_000 })
    // Scoped to the history table's status cells, not the page text. The
    // restore select is labelled "Completed backup", so matching /completed/
    // against the body succeeds before any backup has run and asserts nothing.
    const statusCell = (status: string) =>
      page!.getByRole('cell').filter({ hasText: new RegExp(`^${status}$`) })
    const deadline = Date.now() + 120_000
    for (;;) {
      await page!.getByRole('button', { name: 'Refresh backup history' }).click()
      await Bun.sleep(1_000)
      if ((await statusCell('failed').count()) > 0) throw new Error('the backup run failed')
      if ((await statusCell('completed').count()) > 0) break
      if (Date.now() > deadline) throw new Error('the backup never completed')
      await Bun.sleep(2_000)
    }

    // Restore side-by-side. The recovery point select is populated only from
    // completed backups, so the button enabling is itself the proof that one
    // landed - and it is the precondition the previous version skipped past.
    await page!.getByLabel('New database name').fill(RESTORED_DATABASE)
    const restore = page!.getByRole('button', { name: 'Verify and restore' })
    for (let attempt = 0; await restore.isDisabled(); attempt += 1) {
      if (attempt >= 60) throw new Error('restore stayed disabled after a completed backup')
      await Bun.sleep(500)
    }
    await restore.click()
    await page!.getByText('Backup restored as a new detached database').waitFor({ timeout: 120_000 })

    // The restored mirror must be a real, separate database in the control
    // plane rather than a toast - and the source must still be there.
    await page!.getByRole('link', { name: 'Databases', exact: true }).click()
    await page!.getByText(RESTORED_DATABASE).first().waitFor({ timeout: 20_000 })
    await page!.getByText(DATABASE).first().waitFor({ timeout: 20_000 })
  })

  await check('a dead letter is discarded, and an unrecoverable retry refuses', async () => {
    // Waits for a dead letter naming the append-only table to appear on the
    // Activity page. Polling is by reload rather than a fixed sleep: the row
    // only exists once the CDC stream has read the binlog event, and how long
    // that takes is not something the test should assert.
    // Dead letters render as cards in a grid, not as table rows, so this
    // targets the card by test id. getByRole('row') silently matches nothing
    // here and turns a missing dead letter into an indistinguishable timeout.
    const deadLetters = (table: string) =>
      page!.getByTestId('dead-letter').filter({ hasText: table })
    const awaitDeadLetter = async (table: string, what: string) => {
      const deadline = Date.now() + 90_000
      for (;;) {
        await page!.goto(`${pintailUrl}/activity`)
        await page!.getByRole('heading', { name: 'Activity' }).waitFor({ timeout: 20_000 })
        if ((await deadLetters(table).count()) > 0) return deadLetters(table).first()
        if (Date.now() > deadline) throw new Error(`no dead letter appeared for ${what}`)
        await Bun.sleep(3_000)
      }
    }

    // Precondition, asserted rather than assumed: a dead letter can only be
    // produced by a table that is actually being replicated. Without this, a
    // table the wizard never included fails identically to CDC not
    // quarantining, and the message would send us after the wrong bug.
    await page!.goto(`${pintailUrl}/databases`)
    await page!.getByRole('link', { name: DATABASE }).first().click()
    await page!.getByRole('heading', { name: DATABASE }).waitFor({ timeout: 20_000 })
    // Not exact: the cell carries the table name plus its key-mode badge, so
    // the accessible name is "notes KEYLESS · INSERT ONLY".
    for (const table of [APPEND_TABLE, APPEND_TABLE_RETRY]) {
      await page!.getByRole('cell', { name: table }).first().waitFor({ timeout: 20_000 })
    }

    // Wait for the mirror to be streaming before mutating the source. A
    // quarantine can only happen once CDC is reading the binlog, and the
    // resnapshot above leaves it snapshotting for a while - issuing the UPDATE
    // into that window means the event is consumed by the snapshot instead of
    // producing a dead letter.
    const streamingDeadline = Date.now() + 120_000
    for (;;) {
      await page!.goto(`${pintailUrl}/databases`)
      await Bun.sleep(1_000)
      if (/streaming/i.test((await page!.textContent('body')) ?? '')) break
      if (Date.now() > streamingDeadline) throw new Error('mirror never resumed streaming')
      await Bun.sleep(3_000)
    }

    // An UPDATE with no stable source key quarantines rather than guessing
    // which duplicate row to touch.
    await sql(`UPDATE ${APPEND_TABLE} SET body = 'changed' WHERE body = 'first'`)
    const discardable = await awaitDeadLetter(APPEND_TABLE, 'the quarantined UPDATE')
    await discardable.getByRole('button', { name: 'Discard' }).click()
    await discardable.waitFor({ state: 'detached', timeout: 20_000 })
    // Discard must be durable, not just a row removed from the local list.
    await page!.goto(`${pintailUrl}/activity`)
    await page!.getByRole('heading', { name: 'Activity' }).waitFor({ timeout: 20_000 })
    await Bun.sleep(1_000)
    if ((await deadLetters(APPEND_TABLE).count()) > 0) {
      throw new Error('the discarded dead letter came back')
    }

    // A DELETE quarantines for the same reason. Retry cannot recover this one:
    // it runs a table reconcile, and reconciliation needs a source key that a
    // keyless table does not have - the dead letter's own message says the
    // remedy is resnapshot. What is asserted is that retry refuses loudly and
    // names the reason, rather than failing silently or appearing to succeed.
    //
    // This is the refusal path, not retry's success path. Covering the success
    // path needs a decode failure on a KEYED table, which no SQL statement
    // reliably produces.
    await sql(`DELETE FROM ${APPEND_TABLE_RETRY} WHERE body = 'second'`)
    const unretryable = await awaitDeadLetter(APPEND_TABLE_RETRY, 'the quarantined DELETE')
    await unretryable.getByRole('button', { name: 'Retry safely' }).click()
    await page!.getByText(/no source key/i).waitFor({ timeout: 60_000 })
    // And the record survives a refused retry, so the operator can still act.
    await page!.goto(`${pintailUrl}/activity`)
    await page!.getByRole('heading', { name: 'Activity' }).waitFor({ timeout: 20_000 })
    if ((await deadLetters(APPEND_TABLE_RETRY).count()) === 0) {
      throw new Error('a refused retry removed the dead letter')
    }
  })

  await check('a teammate is invited, listed and revoked', async () => {
    await page!.goto(`${pintailUrl}/team`)
    await page!.getByRole('heading', { name: 'Team' }).waitFor()

    // The operator is a member, and cannot remove themselves - the remove
    // control is absent on your own row rather than present and failing.
    const self = page!.getByRole('row').filter({ hasText: OPERATOR.email })
    await self.waitFor({ timeout: 20_000 })
    if ((await self.getByRole('button', { name: 'Remove member' }).count()) > 0) {
      throw new Error('the signed-in operator can remove themselves')
    }

    await page!.getByLabel('Email').fill(INVITE_EMAIL)
    await page!.getByRole('combobox').first().click()
    await page!.getByRole('option', { name: 'Operator' }).click()
    // exact: the accessible-name match is a case-insensitive substring, so a
    // bare "Invite" also matches the "Revoke invite" buttons in the table.
    await page!.getByRole('button', { name: 'Invite', exact: true }).click()

    // The link is shown once and carries a token the recipient needs.
    const link = page!.getByTestId('invite-link')
    await link.waitFor({ timeout: 20_000 })
    const href = (await link.textContent())?.trim() || ''
    if (!/\/accept-invite\?token=.{16,}/.test(href)) {
      throw new Error(`invite link looks wrong: ${href}`)
    }

    const invite = page!.getByRole('row').filter({ hasText: INVITE_EMAIL })
    await invite.waitFor({ timeout: 20_000 })
    await invite.getByText('pending', { exact: true }).waitFor()
    await invite.getByText('operator', { exact: true }).waitFor()

    // Like the API key secret, the token is not recoverable: a reload must not
    // reproduce a link that would let anyone holding the screen join.
    await page!.goto(`${pintailUrl}/team`)
    await invite.waitFor({ timeout: 20_000 })
    if (await link.isVisible()) throw new Error('the invite link survived a reload')

    // Revoking is durable, and the control disappears once it has no effect.
    await invite.getByRole('button', { name: 'Revoke invite' }).click()
    await page!.getByText('Invite revoked').waitFor({ timeout: 20_000 })
    await invite.getByText('revoked', { exact: true }).waitFor({ timeout: 20_000 })
    await page!.goto(`${pintailUrl}/team`)
    await invite.getByText('revoked', { exact: true }).waitFor({ timeout: 20_000 })
    if ((await invite.getByRole('button', { name: 'Revoke invite' }).count()) > 0) {
      throw new Error('a revoked invite still offers revoke')
    }
  })

  await check('activity records the work the gate has already done', async () => {
    // This page is a durable record, so the assertions are that earlier checks
    // in this run are visible here. Asserting merely that the page renders
    // would pass against an empty log, which is the failure worth catching.
    await page!.goto(`${pintailUrl}/activity`)
    await page!.getByRole('heading', { name: 'Activity' }).waitFor()
    await page!.getByRole('cell', { name: 'Snapshot' }).first().waitFor({ timeout: 20_000 })

    // The audit trail sits behind its own tab now, so this also asserts the
    // tab exists and reveals its content rather than reading a card that
    // happened to be on the page.
    await page!.getByRole('tab', { name: 'Audit trail' }).click()
    await page!.getByRole('heading', { name: 'Audit trail' }).waitFor()
    for (const action of ['database.create', 'api_key.create', 'backup.restore']) {
      await page!.getByText(action, { exact: true }).first().waitFor({ timeout: 20_000 })
    }
    // Actions are attributed, not anonymous.
    await page!.getByText(OPERATOR.email).first().waitFor()

    // Refreshing must not empty the table.
    await page!.getByRole('button', { name: 'Refresh audit trail' }).click()
    await page!.getByText('database.create', { exact: true }).first().waitFor({ timeout: 20_000 })

    // Back to the activity tab for the filter, which applies to that table.
    await page!.getByRole('tab', { name: 'Activity', exact: true }).click()
    await page!.getByRole('cell', { name: 'Snapshot' }).first().waitFor({ timeout: 20_000 })

    // Filtering to one database keeps its own rows rather than clearing.
    await page!.getByRole('combobox').first().click()
    await page!.getByRole('option', { name: DATABASE, exact: true }).click()
    await page!.getByRole('cell', { name: DATABASE }).first().waitFor({ timeout: 20_000 })
  })

  await check('settings reports the session, endpoint and operations surface', async () => {
    await page!.goto(`${pintailUrl}/settings`)
    await page!.getByRole('heading', { name: 'Settings' }).waitFor()
    for (const card of ['Current session', 'Interface', 'Client endpoint', 'Google sign-in', 'Operations']) {
      await page!.getByRole('heading', { name: card, exact: true }).waitFor({ timeout: 20_000 })
    }
    // The session card identifies the session by subject and role, not by
    // email, so this asserts a real resolved subject rather than a blank or a
    // placeholder where the JWT claims should be.
    await page!.getByText(/^usr_[A-Za-z0-9]+$/).first().waitFor({ timeout: 20_000 })
    await page!.getByText('admin', { exact: true }).first().waitFor({ timeout: 20_000 })

    // The Prometheus surface is linked and actually serves metrics. A dead
    // link here is invisible from the page itself.
    const metrics = await page!.request.get(`${pintailUrl}/metrics`)
    if (!metrics.ok()) throw new Error(`/metrics returned ${metrics.status()}`)
    if (!/^# (HELP|TYPE) /m.test(await metrics.text())) {
      throw new Error('/metrics did not return Prometheus text format')
    }

    // Google credentials are write-only: the secret is never rendered back.
    if ((await page!.getByLabel('Client secret').inputValue()) !== '') {
      throw new Error('the stored Google client secret was rendered back into the form')
    }
  })

  await check('the Google public URL rejects a non-HTTPS origin inline', async () => {
    // A rejected public URL used to 400 the whole save, so the client secret
    // was never stored and the enable toggle appeared to turn itself off
    // while the card still read "Not Configured" - three symptoms, one
    // discarded field, and no indication which.
    await page!.getByRole('link', { name: 'Settings', exact: true }).click()
    const publicUrl = page!.getByLabel('Public URL')
    await publicUrl.fill('http://pintail.example.com')
    const urlError = page!.getByTestId('domain-url-error')
    await urlError.waitFor()
    await urlError.getByText('HTTPS').first().waitFor()
    // Blocking the save is the point: submitting could only 400 and lose the
    // credentials typed alongside it.
    const save = page!.getByRole('button', { name: 'Save Google settings' })
    if (!(await save.isDisabled())) throw new Error('save must be blocked while the URL is invalid')
    // A valid origin clears it.
    await publicUrl.fill('https://pintail.example.com')
    await urlError.waitFor({ state: 'hidden' })
    // localhost is the documented exception, so http must be accepted there.
    await publicUrl.fill('http://localhost:8080')
    await urlError.waitFor({ state: 'hidden' })
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
  // -v as well: the image declares a /data volume, so a bare rm leaves an
  // anonymous volume behind on the shared Docker host every run.
  if (rustfsStarted) await docker('rm', '-f', '-v', rustfsName).catch(() => {})
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

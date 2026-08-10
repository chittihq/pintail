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
// A schema the probe user can reach but holds no table privilege on.
const RESTRICTED_DATABASE = 'restricted_db'
const API_KEY_NAME = 'browser-gate-key'
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
    await page!.getByRole('button', { name: 'Resnapshot' }).click()
    await page!.getByText('Resnapshot accepted').waitFor({ timeout: 20_000 })
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

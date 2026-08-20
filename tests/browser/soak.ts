/// Production-shaped browser soak: the dashboard driven the way a real
/// deployment gets used, at real volume, for as long as it takes.
///
/// The fast smoke gate proves every surface works once, in seconds, on rows
/// you can count by hand. This suite exists for the failures that only shape
/// and scale produce - the ones reported from production as "nothing is
/// happening": a 2M-row initial sync watched from the wizard, dashboard
/// actions issued while ingest is live, a two-minute convergence wait, an
/// 18M-row backfill arriving through CDC while the console keeps answering,
/// a full Reset at 20M rows with its progress strip, and the sakila dataset
/// - a real schema with ENUM, SET, YEAR, GEOMETRY and foreign keys - walked
/// through the same wizard and console. MySQL is the oracle throughout:
/// every convergence assertion is a value-level comparison, not a row count
/// alone.
///
/// Run with: bun run scripts/validate.ts --stages=soak
///           (or directly: bun run soak.ts in tests/browser)
///
/// Deliberately NOT part of the release chain or the default stage list -
/// this takes tens of minutes by design. Screenshots land in
/// tests/browser/artifacts/ on every failure.

import { createServer } from 'node:net'
import { gunzipSync } from 'node:zlib'
import { mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { chromium } from 'playwright'
import type { Browser, Page } from 'playwright'

const repository = resolve(import.meta.dir, '..', '..')
const artifacts = join(import.meta.dir, 'artifacts')
const cargoBinary = join(homedir(), '.cargo', 'bin', 'cargo')
const cargoTargetDir = join(repository, 'target')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-soak-mysql-${process.pid}-${nonce}`
const DATABASE = 'soak_db'
const SAKILA = 'sakila'
const OPERATOR = { email: 'soak@pintail.local', password: 'browser-soak-password' }

/// Wave sizes. The base seeds 1k rows and doubles server-side, so wave one
/// lands at 2,048,000 and the backfill raises the total to 20,480,000.
const WAVE_ONE_DOUBLINGS = 11
const WAVE_ONE_ROWS = 1_000 * 2 ** WAVE_ONE_DOUBLINGS
const BACKFILL_BATCHES = 9
const TOTAL_ROWS = WAVE_ONE_ROWS * (1 + BACKFILL_BATCHES)

interface CheckResult {
  check: string
  status: 'PASS' | 'FAIL'
  detail?: string
}

const results: CheckResult[] = []
const pageErrors: string[] = []
let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailStderr: Promise<string> | undefined
let browser: Browser | undefined
let page: Page | undefined
let pintailDataDir = ''
let pintailUrl = ''
let mysqlStarted = false

function log(message: string) {
  console.log(`[soak] ${new Date().toISOString()} ${message}`)
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
  const started = Date.now()
  try {
    await action()
    results.push({ check: name, status: 'PASS' })
    log(`PASS ${name} (${Math.round((Date.now() - started) / 1000)}s)`)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    results.push({ check: name, status: 'FAIL', detail })
    log(`FAIL ${name} — ${detail}`)
    if (pageErrors.length) log(`  browser console: ${pageErrors.slice(-3).join(' | ')}`)
    try {
      mkdirSync(artifacts, { recursive: true })
      await page?.screenshot({
        path: join(artifacts, `soak-${name.replaceAll(/[^a-z0-9]+/gi, '-')}.png`),
        fullPage: true,
      })
    } catch {}
  }
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_E2E_BINARY) return resolve(process.env.PINTAIL_E2E_BINARY)
  log('building the release binary')
  await command([cargoBinary, 'build', '--release', '-p', 'pintail'])
  return join(cargoTargetDir, 'release', 'pintail')
}

async function sql(statement: string) {
  await mysqlConnection!.query(statement)
}

async function sqlValue(statement: string): Promise<string> {
  const [rows] = (await mysqlConnection!.query(statement)) as unknown as [
    Array<Record<string, unknown>>,
  ]
  return String(Object.values(rows[0] ?? {})[0])
}

/// Runs one query through the SQL console UI and returns the first cell of
/// the first row as rendered - the browser IS the client under test, so
/// convergence checks read what an operator would read.
async function consoleValue(database: string, query: string): Promise<string> {
  await page!.goto(`${pintailUrl}/sql`)
  await page!.getByRole('heading', { name: 'SQL Console' }).waitFor()
  // Selected by NAME through the picker, the way an operator does - the URL
  // parameter wants the record id, and passing the name there left the
  // console asking a database that "does not exist".
  await page!.getByRole('combobox').first().click()
  await page!.getByRole('option', { name: database }).click()
  const editor = page!.locator('.cm-content')
  await editor.waitFor({ timeout: 20_000 })
  await editor.click()
  await page!.keyboard.press('ControlOrMeta+A')
  await page!.keyboard.type(query)
  await page!.getByRole('button', { name: 'Run' }).click()
  // Whichever the console renders first decides: results, or its own error
  // - which fails the check immediately with the real reason instead of a
  // two-minute timeout that hides it.
  const rows = page!.getByText(/\d+ rows? ·/)
  const failure = page!.locator('.text-destructive').first()
  const outcome = await Promise.race([
    rows.waitFor({ timeout: 120_000 }).then(() => 'rows' as const),
    failure.waitFor({ timeout: 120_000 }).then(() => 'error' as const),
  ])
  if (outcome === 'error') {
    throw new Error(`console query failed: ${((await failure.textContent()) ?? '').trim()}`)
  }
  const cell = page!.locator('table tbody tr').first().locator('td').first()
  return ((await cell.textContent()) ?? '').trim()
}

/// The mirrored row count for one table, read from the dashboard's own
/// tables view rather than any API shortcut.
async function mirroredCount(database: string, table: string): Promise<number> {
  const value = await consoleValue(database, `SELECT COUNT(*) FROM ${table}`)
  return Number(value.replaceAll(',', ''))
}

async function main() {
  const host = await dockerHost()
  log(`starting MySQL source ${mysqlName} (disk-backed - the volume is the point)`)
  await docker(
    'run',
    '--detach',
    '--name',
    mysqlName,
    '--publish',
    '0:3306',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env',
    `MYSQL_DATABASE=${DATABASE}`,
    'mysql:8.4',
    '--server-id=944',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--binlog-row-metadata=FULL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    '--innodb-buffer-pool-size=2G',
    '--max-binlog-size=512M',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )

  // The workload table: realistic width and type variety, an index a real
  // service would have, and enough entropy that checksum comparisons mean
  // something. amount is derived from id so every value-level assertion is
  // deterministic without shipping fixture files.
  await sql(`CREATE TABLE traffic (
    id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    user_id INT UNSIGNED NOT NULL,
    kind ENUM('view','click','purchase','refund') NOT NULL,
    amount DECIMAL(12,2) NOT NULL,
    payload VARCHAR(120) NOT NULL,
    created_at DATETIME(3) NOT NULL,
    INDEX idx_user (user_id)
  )`)
  // A small side table so per-table actions have a fast target while the
  // big one is busy.
  await sql(`CREATE TABLE annotations (
    id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
    note VARCHAR(64) NOT NULL
  )`)
  await sql(`INSERT INTO annotations (note) VALUES ('alpha'), ('beta'), ('gamma')`)

  log(`seeding wave one: ${WAVE_ONE_ROWS.toLocaleString()} rows via server-side doubling`)
  await sql(`INSERT INTO traffic (user_id, kind, amount, payload, created_at)
    SELECT seq.n % 5000,
           ELT(1 + (seq.n % 4), 'view', 'click', 'purchase', 'refund'),
           ROUND((seq.n % 99991) / 7, 2),
           CONCAT('payload-', MD5(seq.n)),
           TIMESTAMPADD(SECOND, seq.n % 86400, '2026-08-01 00:00:00')
    FROM (SELECT a.n + b.n * 10 + c.n * 100 AS n
          FROM (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
                UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) a,
               (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
                UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) b,
               (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4
                UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) c) seq`)
  for (let doubling = 0; doubling < WAVE_ONE_DOUBLINGS; doubling += 1) {
    await sql(`INSERT INTO traffic (user_id, kind, amount, payload, created_at)
      SELECT user_id, kind, amount, payload, created_at FROM traffic`)
  }
  const seeded = await sqlValue('SELECT COUNT(*) FROM traffic')
  log(`wave one seeded: ${Number(seeded).toLocaleString()} rows`)

  // sakila: the real dataset, restored from the vendored dump. DEFINER
  // clauses assume the dumping user exists; root does here.
  log('loading sakila from the vendored dump')
  // DELIMITER is a mysql CLI directive, not server SQL, so the trigger,
  // procedure and function blocks it brackets cannot travel over a driver
  // connection. Pintail mirrors base tables only; the routines add nothing
  // this suite asserts on.
  const raw = gunzipSync(
    readFileSync(join(repository, 'tests', 'corpus', 'real-data', 'sakila-db.sql.gz')),
  ).toString('utf8')
  const kept: string[] = []
  let skippingRoutine = false
  for (const line of raw.split('\n')) {
    const stripped = line.trim()
    if (!skippingRoutine && stripped.startsWith('DELIMITER') && stripped !== 'DELIMITER ;') {
      skippingRoutine = true
      continue
    }
    if (skippingRoutine) {
      if (stripped === 'DELIMITER ;') skippingRoutine = false
      continue
    }
    kept.push(line)
  }
  const dump = kept.join('\n')
  await mysqlConnection.query(dump)
  await sql(`USE ${DATABASE}`)

  const binary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-soak-'))
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
    { cwd: repository, stdout: 'ignore', stderr: 'pipe' },
  )
  pintailStderr = new Response(pintailProcess.stderr).text()
  for (let attempt = 0; ; attempt += 1) {
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) break
    } catch {}
    if (attempt >= 240) throw new Error('pintail did not become healthy within 120 seconds')
    await Bun.sleep(500)
  }

  browser = await chromium.launch()
  page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
  page.setDefaultTimeout(30_000)
  page.on('pageerror', (error) => pageErrors.push(`pageerror: ${error.message}`))
  page.on('console', (message) => {
    if (message.type() === 'error') pageErrors.push(`console: ${message.text()}`)
  })

  await check('operator setup signs in', async () => {
    await page!.goto(pintailUrl)
    await page!.getByRole('heading', { name: 'Create the operator' }).waitFor()
    await page!.getByLabel('Email').fill(OPERATOR.email)
    await page!.getByLabel('Password').fill(OPERATOR.password)
    await page!.getByRole('button', { name: 'Initialize Pintail' }).click()
    await page!.getByText('Node healthy').waitFor()
  })

  await check(`wave one: ${WAVE_ONE_ROWS.toLocaleString()} rows sync through the wizard with visible progress`, async () => {
    await page!.getByRole('link', { name: 'Add database' }).first().click()
    await page!.getByLabel('MySQL schema').fill(DATABASE)
    await page!
      .getByLabel('MySQL DSN')
      .fill(`mysql://pintail:pintail@${host}:${mysqlPort}/${DATABASE}`)
    await page!.getByRole('button', { name: 'Test connection' }).click()
    await page!.getByText('Recommended: CDC').waitFor({ timeout: 60_000 })
    await page!.getByRole('button', { name: 'Choose tables' }).click()
    await page!.getByText('traffic', { exact: true }).waitFor()
    await page!.getByRole('button', { name: 'Review & start' }).click()
    await page!.waitForURL(/\/databases\/[^/?]+\?tab=snapshot$/, { timeout: 60_000 })

    // While the copy is OBSERVABLE it must be visible: whenever a poll
    // catches the snapshotting state, the progress strip has to be there
    // too. A host fast enough to finish between two polls owes no strip -
    // the strip exists for copies long enough to worry an operator.
    let observedCopy = false
    let sawProgress = false
    const deadline = Date.now() + 15 * 60_000
    for (;;) {
      const copying = (await page!.textContent('body'))?.includes('snapshotting') ?? false
      if (copying) observedCopy = true
      if (!sawProgress && (await page!.getByTestId('copy-progress').count()) > 0) {
        sawProgress = true
        log('copy-progress strip is visible')
      }
      const body = (await page!.textContent('body')) ?? ''
      if (/streaming/i.test(body) && !/snapshotting/i.test(body)) break
      if (Date.now() > deadline) {
        throw new Error(`initial sync never reached streaming (progress strip seen: ${sawProgress})`)
      }
      await Bun.sleep(1_000)
    }
    if (observedCopy && !sawProgress) {
      throw new Error('the copy was observable but never showed progress')
    }
    const mirrored = await mirroredCount(DATABASE, 'traffic')
    if (mirrored !== WAVE_ONE_ROWS) {
      throw new Error(`mirror has ${mirrored} rows, source has ${WAVE_ONE_ROWS}`)
    }
  })

  await check('dashboard stays responsive during live ingest, and a two-minute wait converges', async () => {
    // A steady drip, the way production writes arrive - inserted in small
    // transactions so CDC sees ordinary traffic, not one giant batch.
    let dripping = true
    let dripped = 0
    const drip = (async () => {
      while (dripping) {
        await sql(`INSERT INTO traffic (user_id, kind, amount, payload, created_at)
          SELECT user_id, kind, amount, CONCAT('drip-', payload), created_at
          FROM traffic ORDER BY id LIMIT 500`)
        dripped += 500
        await Bun.sleep(1_000)
      }
    })()

    try {
      // The console answers aggregates while ingest is live.
      const started = Date.now()
      const sum = await consoleValue(DATABASE, 'SELECT ROUND(SUM(amount), 2) FROM traffic')
      const elapsed = Date.now() - started
      if (elapsed > 60_000) throw new Error(`console aggregate took ${elapsed}ms under ingest`)
      if (!/^\d/.test(sum)) throw new Error(`console aggregate returned ${JSON.stringify(sum)}`)

      // A per-table action lands while the big table is busy: resync the
      // small side table and watch it complete.
      await page!.goto(`${pintailUrl}/databases`)
      await page!.getByRole('link', { name: DATABASE }).first().click()
      await page!.getByRole('heading', { name: DATABASE }).waitFor()
      const row = page!.getByRole('row').filter({ hasText: 'annotations' })
      await row.getByRole('button', { name: 'Resync', exact: true }).click()
      await page!
        .getByText('resnapshot accepted; other tables keep replicating')
        .waitFor({ timeout: 180_000 })

      // Pause and resume under load - the flow that used to unschedule the
      // database forever.
      await page!.getByRole('button', { name: 'Pause' }).click()
      await page!.getByRole('button', { name: 'Resume' }).waitFor({ timeout: 30_000 })
      await page!.getByRole('button', { name: 'Resume' }).click()
      await page!.getByRole('button', { name: 'Pause' }).waitFor({ timeout: 30_000 })
    } finally {
      dripping = false
      await drip
    }
    log(`drip ingest wrote ${dripped.toLocaleString()} rows`)

    // The user's own framing: wait two minutes, the data must be in sync.
    await Bun.sleep(120_000)
    const source = Number(await sqlValue('SELECT COUNT(*) FROM traffic'))
    const mirrored = await mirroredCount(DATABASE, 'traffic')
    if (mirrored !== source) {
      throw new Error(`after the two-minute wait the mirror has ${mirrored}, source has ${source}`)
    }
  })

  await check(`backfill to ${TOTAL_ROWS.toLocaleString()} rows streams through CDC with visible liveness`, async () => {
    const before = Number(await sqlValue('SELECT COUNT(*) FROM traffic'))
    log(`backfilling from ${before.toLocaleString()} rows`)
    // 256k-row transactions: production backfills batch their commits, and a
    // single 2M-row binlog transaction would test the applier's memory
    // ceiling rather than its throughput.
    const CHUNK = 256_000
    const toAdd = WAVE_ONE_ROWS * BACKFILL_BATCHES
    for (let added = 0; added < toAdd; added += CHUNK) {
      await sql(`INSERT INTO traffic (user_id, kind, amount, payload, created_at)
        SELECT user_id, kind, amount, CONCAT('w2-${added}-', payload), created_at
        FROM traffic ORDER BY id LIMIT ${Math.min(CHUNK, toAdd - added)}`)
      if ((added / CHUNK) % 8 === 7) {
        log(`backfill: ${(added + CHUNK).toLocaleString()} of ${toAdd.toLocaleString()} rows committed at the source`)
      }
    }
    const source = Number(await sqlValue('SELECT COUNT(*) FROM traffic'))

    // Convergence with a liveness contract: the mirrored count must GROW at
    // every sample. Total time is allowed to be long - that is the soak -
    // but ten minutes without progress is a wedge, which is the exact
    // failure mode this suite exists to catch.
    let last = await mirroredCount(DATABASE, 'traffic')
    let lastGrowth = Date.now()
    const deadline = Date.now() + 90 * 60_000
    for (;;) {
      if (last >= source) break
      await Bun.sleep(30_000)
      const now = await mirroredCount(DATABASE, 'traffic')
      if (now > last) {
        lastGrowth = Date.now()
        log(`mirror at ${now.toLocaleString()} / ${source.toLocaleString()} rows`)
      }
      if (Date.now() - lastGrowth > 10 * 60_000) {
        throw new Error(`ingest stalled at ${now} of ${source} rows for ten minutes`)
      }
      if (Date.now() > deadline) throw new Error(`backfill never converged: ${now} of ${source}`)
      last = now
    }

    // Value-level agreement, not just cardinality: the checksum the mirror
    // serves must be MySQL's answer.
    const sourceSum = await sqlValue(
      `SELECT CONCAT(COUNT(*), ':', ROUND(SUM(amount), 2)) FROM traffic WHERE kind = 'purchase'`,
    )
    const mirroredSum = await consoleValue(
      DATABASE,
      `SELECT CONCAT(COUNT(*), ':', ROUND(SUM(amount), 2)) FROM traffic WHERE kind = 'purchase'`,
    )
    if (sourceSum !== mirroredSum) {
      throw new Error(`purchase checksum diverged: source ${sourceSum}, mirror ${mirroredSum}`)
    }
  })

  await check(`Reset mirror at ${TOTAL_ROWS.toLocaleString()} rows shows progress and converges`, async () => {
    await page!.goto(`${pintailUrl}/databases`)
    await page!.getByRole('link', { name: DATABASE }).first().click()
    await page!.getByRole('heading', { name: DATABASE }).waitFor()
    await page!.getByRole('tab', { name: 'settings' }).click()
    await page!.getByTestId('reset-mirror').click()
    await page!.getByRole('dialog').getByRole('button', { name: 'Reset mirror' }).click()
    await page!
      .getByText(/Mirror reset; a fresh snapshot is running|Reset queued/)
      .first()
      .waitFor({ timeout: 60_000 })

    // At this volume the strip is not optional, and its percent must move.
    const strip = page!.getByTestId('copy-progress')
    await strip.waitFor({ timeout: 10 * 60_000 })
    const firstReading = (await strip.textContent()) ?? ''
    let moved = false
    const deadline = Date.now() + 45 * 60_000
    for (;;) {
      const body = (await page!.textContent('body')) ?? ''
      const done = /streaming/i.test(body) && (await strip.count()) === 0
      if (!moved && (await strip.count()) > 0) {
        const reading = (await strip.textContent()) ?? ''
        if (reading !== firstReading) moved = true
      }
      if (done) break
      if (Date.now() > deadline) throw new Error('the reset never settled back to streaming')
      await Bun.sleep(5_000)
    }
    if (!moved) throw new Error('the reset copy progress never advanced')
    const source = Number(await sqlValue('SELECT COUNT(*) FROM traffic'))
    const mirrored = await mirroredCount(DATABASE, 'traffic')
    if (mirrored !== source) throw new Error(`after reset: mirror ${mirrored}, source ${source}`)
  })

  await check('sakila: a real schema registers, streams, and answers exactly', async () => {
    await page!.goto(`${pintailUrl}/databases/new`)
    await page!.getByLabel('MySQL schema').fill(SAKILA)
    await page!
      .getByLabel('MySQL DSN')
      .fill(`mysql://pintail:pintail@${host}:${mysqlPort}/${SAKILA}`)
    await page!.getByRole('button', { name: 'Test connection' }).click()
    await page!.getByText(/Recommended:/).waitFor({ timeout: 120_000 })
    await page!.getByRole('button', { name: 'Choose tables' }).click()
    await page!.getByText('rental', { exact: true }).waitFor()
    await page!.getByRole('button', { name: 'Review & start' }).click()
    await page!.waitForURL(/\/databases\/[^/?]+\?tab=snapshot$/, { timeout: 60_000 })
    const deadline = Date.now() + 10 * 60_000
    for (;;) {
      const body = (await page!.textContent('body')) ?? ''
      if (/streaming/i.test(body) && !/snapshotting/i.test(body)) break
      if (Date.now() > deadline) throw new Error('sakila never reached streaming')
      await Bun.sleep(3_000)
    }
    // Two of the queries that found real bugs during the differential work:
    // ENUM grouping and a join aggregate.
    const pairs: Array<[string, string]> = [
      [`SELECT CONCAT(rating, ':', COUNT(*)) FROM film GROUP BY rating ORDER BY rating LIMIT 1`, 'enum rating group'],
      [
        `SELECT CONCAT(c.name, ':', COUNT(*)) FROM film_category fc JOIN category c ON c.category_id = fc.category_id GROUP BY c.name ORDER BY COUNT(*) DESC, c.name LIMIT 1`,
        'category join aggregate',
      ],
    ]
    await sql(`USE ${SAKILA}`)
    try {
      for (const [query, label] of pairs) {
        const sourceValue = await sqlValue(query)
        const mirroredValue = await consoleValue(SAKILA, query)
        if (sourceValue !== mirroredValue) {
          throw new Error(`${label} diverged: source ${sourceValue}, mirror ${mirroredValue}`)
        }
      }
    } finally {
      await sql(`USE ${DATABASE}`)
    }
  })

  await check('the session ends with no dead letters and no error banner', async () => {
    await page!.goto(`${pintailUrl}/activity`)
    await page!.getByRole('heading', { name: 'Activity' }).waitFor()
    const body = (await page!.textContent('body')) ?? ''
    if (/Dead-letter queue/i.test(body)) throw new Error('the soak left dead letters behind')
  })
}

async function teardown() {
  try {
    await browser?.close()
  } catch {}
  try {
    pintailProcess?.kill()
    if (pintailStderr) {
      const stderr = await pintailStderr
      const errors = stderr.split('\n').filter((line) => /error/i.test(line))
      if (errors.length) log(`pintail stderr errors: ${errors.slice(-5).join(' | ')}`)
    }
  } catch {}
  try {
    await mysqlConnection?.end()
  } catch {}
  if (mysqlStarted) {
    try {
      await docker('rm', '--force', mysqlName)
    } catch {}
  }
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
}

try {
  await main()
} catch (error) {
  results.push({
    check: 'harness',
    status: 'FAIL',
    detail: error instanceof Error ? error.message : String(error),
  })
} finally {
  await teardown()
}

const failed = results.filter((result) => result.status === 'FAIL')
log(`soak: ${failed.length === 0 ? 'PASS' : 'FAIL'} (${results.length - failed.length} passed, ${failed.length} failed)`)
for (const failure of failed) log(`  FAIL ${failure.check}: ${failure.detail}`)
process.exit(failed.length === 0 ? 0 : 1)

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { createServer } from 'node:net'
import mysql from 'mysql2/promise'

type MetricSample = {
  elapsedSeconds: number
  generatedEvents: number
  ingestedEvents: number
  eventLag: number
  lagSeconds: number
  rssBytes: number
  deadLetters: number
}

type Checksum = {
  rows: number
  idChecksum: number
  revisions: number
  minimumId: number
  maximumId: number
}

const loadgenDir = import.meta.dir
const repository = resolve(loadgenDir, '../..')
const durationSeconds = Number(process.env.SOAK_DURATION_SECONDS ?? '1800')
const targetEventsPerSecond = Number(process.env.SOAK_EVENTS_PER_SECOND ?? '5500')
if (!Number.isFinite(durationSeconds) || durationSeconds < 10) {
  throw new Error('SOAK_DURATION_SECONDS must be at least 10')
}
if (!Number.isFinite(targetEventsPerSecond) || targetEventsPerSecond < 1000) {
  throw new Error('SOAK_EVENTS_PER_SECOND must be at least 1000')
}
const fullGate = durationSeconds >= 1800
const rowsPerBatch = Math.ceil(targetEventsPerSecond)
const insertsPerBatch = Math.ceil(rowsPerBatch / 2)
const updatesPerBatch = Math.floor(rowsPerBatch / 4)
const deletesPerBatch = rowsPerBatch - insertsPerBatch - updatesPerBatch
const baselineRows = 100_000
const runId = `pintail-m9-soak-${process.pid}-${Date.now()}`
const mysqlName = `${runId}-mysql`
const networkName = `${runId}-network`
const dataDir = mkdtempSync(join(tmpdir(), 'pintail-m9-soak-'))
let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let dockerCreated = false

function log(message: string) {
  console.log(`[soak] ${message}`)
}

async function command(args: string[], options: { quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: repository,
    stdin: 'ignore',
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) {
    throw new Error(
      `${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`,
    )
  }
  if (!options.quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

/// A host as it goes into a DSN: an IPv6 literal needs its brackets there.
function dsnHost(host: string): string {
  return host.includes(':') ? `[]` : host
}

async function dockerHost() {
  const context = (await docker('context', 'show')).stdout
  const endpoint = (
    await docker(
      'context',
      'inspect',
      context,
      '--format',
      '{{.Endpoints.docker.Host}}',
    )
  ).stdout
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  // URL parsing keeps an IPv6 literal (ssh://user@[fd7a::1]) intact.
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const ssh = await command(['ssh', '-G', target], { quiet: true })
  const hostname = ssh.stdout
    .split('\n')
    .find((line) => line.startsWith('hostname '))
    ?.slice('hostname '.length)
  if (!hostname) throw new Error(`could not resolve Docker SSH target ${target}`)
  return hostname
}

async function publishedPort(name: string, containerPort: number) {
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
      server.close((error) => {
        if (error) reject(error)
        else resolvePort(address.port)
      })
    })
  })
}

async function waitForMysql(host: string, port: number) {
  for (let attempt = 0; attempt < 120; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        supportBigNumbers: true,
        bigNumberStrings: true,
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready within 60 seconds')
}

async function waitForHttp(baseUrl: string) {
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const response = await fetch(`${baseUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('Pintail did not become ready within 120 seconds')
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

async function buildPintail() {
  if (process.env.PINTAIL_SOAK_BINARY) return resolve(process.env.PINTAIL_SOAK_BINARY)
  log('building the release binary')
  const build = Bun.spawn(['cargo', 'build', '--release', '-p', 'pintail'], {
    cwd: repository,
    stdout: 'inherit',
    stderr: 'inherit',
  })
  if ((await build.exited) !== 0) throw new Error('release build failed')
  const metadata = await command(
    ['cargo', 'metadata', '--format-version', '1', '--no-deps'],
    { quiet: true },
  )
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function seedSource(connection: mysql.Connection) {
  log(`seeding ${baselineRows.toLocaleString()} baseline rows`)
  await connection.query('CREATE DATABASE soak_db')
  await connection.query(`
    CREATE TABLE soak_db.events (
      id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
      tenant_id INT UNSIGNED NOT NULL,
      revision INT UNSIGNED NOT NULL,
      payload VARCHAR(128) NOT NULL,
      created_at DATETIME(6) NOT NULL,
      updated_at DATETIME(6) NOT NULL,
      KEY idx_tenant (tenant_id)
    ) ENGINE=InnoDB
  `)
  await connection.query(`
    CREATE TABLE soak_db.numbers (
      n INT UNSIGNED NOT NULL PRIMARY KEY
    ) ENGINE=InnoDB
  `)
  await connection.query(`
    INSERT INTO soak_db.numbers (n)
    SELECT ones.n + tens.n * 10 + hundreds.n * 100 + thousands.n * 1000
    FROM
      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
       UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6
       UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) ones
    CROSS JOIN
      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
       UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6
       UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) tens
    CROSS JOIN
      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
       UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6
       UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) hundreds
    CROSS JOIN
      (SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3
       UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6
       UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9) thousands
  `)
  await connection.query(`
    CREATE PROCEDURE soak_db.seed_events()
    BEGIN
      DECLARE offset_value INT DEFAULT 0;
      WHILE offset_value < ${baselineRows} DO
        INSERT INTO soak_db.events
          (id, tenant_id, revision, payload, created_at, updated_at)
        SELECT
          offset_value + n + 1,
          MOD(offset_value + n, 128),
          0,
          CONCAT('seed-', offset_value + n + 1),
          UTC_TIMESTAMP(6),
          UTC_TIMESTAMP(6)
        FROM soak_db.numbers;
        SET offset_value = offset_value + 10000;
      END WHILE;
    END
  `)
  await connection.query('CALL soak_db.seed_events()')
  await connection.query('DROP PROCEDURE soak_db.seed_events')
  await connection.query(
    "CREATE USER 'replicator'@'%' IDENTIFIED BY 'replicatorpass'",
  )
  await connection.query(
    'GRANT SELECT, RELOAD, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO \'replicator\'@\'%\'',
  )
  await connection.query(`
    CREATE PROCEDURE soak_db.apply_batch(
      IN batch_number BIGINT UNSIGNED,
      IN next_id BIGINT UNSIGNED,
      IN previous_insert_start BIGINT UNSIGNED
    )
    BEGIN
      DECLARE update_start BIGINT UNSIGNED;
      DECLARE update_end BIGINT UNSIGNED;
      DECLARE wrapped BIGINT UNSIGNED;
      SET update_start = MOD(batch_number * ${updatesPerBatch}, ${baselineRows}) + 1;
      SET update_end = LEAST(${baselineRows}, update_start + ${updatesPerBatch} - 1);
      SET wrapped = ${updatesPerBatch} - (update_end - update_start + 1);

      START TRANSACTION;
      INSERT INTO soak_db.events
        (id, tenant_id, revision, payload, created_at, updated_at)
      SELECT
        next_id + n,
        MOD(next_id + n, 128),
        0,
        CONCAT('batch-', batch_number, '-', n),
        UTC_TIMESTAMP(6),
        UTC_TIMESTAMP(6)
      FROM soak_db.numbers
      WHERE n < ${insertsPerBatch};

      UPDATE soak_db.events
      SET revision = revision + 1,
          payload = CONCAT('update-', batch_number),
          updated_at = UTC_TIMESTAMP(6)
      WHERE id BETWEEN update_start AND update_end;

      IF wrapped > 0 THEN
        UPDATE soak_db.events
        SET revision = revision + 1,
            payload = CONCAT('update-', batch_number),
            updated_at = UTC_TIMESTAMP(6)
        WHERE id BETWEEN 1 AND wrapped;
      END IF;

      IF previous_insert_start > 0 THEN
        DELETE FROM soak_db.events
        WHERE id BETWEEN previous_insert_start
          AND previous_insert_start + ${deletesPerBatch} - 1;
      END IF;
      COMMIT;
    END
  `)
}

async function createReplica(baseUrl: string, token: string, dsn: string) {
  const database = await api<{ id: string }>(baseUrl, '/api/databases', {
    method: 'POST',
    token,
    body: { name: 'soak_db', dsn, mode: 'cdc', include_tables: ['events'] },
  })
  await api(baseUrl, `/api/databases/${database.id}/probe`, { token })
  const accepted = await api<{ run_id: string }>(
    baseUrl,
    `/api/databases/${database.id}/snapshot`,
    { method: 'POST', token, body: { force: false } },
  )
  log(`snapshot ${accepted.run_id} started`)
  for (let attempt = 0; attempt < 1800; attempt += 1) {
    const status = await api<{
      state: string
      tables: Array<{ name: string; rows: number; last_error?: string }>
    }>(baseUrl, `/api/databases/${database.id}/snapshot/status`, { token })
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables
          .map((table) => table.last_error)
          .filter(Boolean)
          .join('; ')}`,
      )
    }
    const rows = status.tables.find((table) => table.name === 'events')?.rows
    if (status.state === 'streaming' && rows === baselineRows) return database.id
    await Bun.sleep(1000)
  }
  throw new Error('initial snapshot did not complete within 30 minutes')
}

function metric(body: string, name: string, labels = '') {
  const escaped = labels.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const match = body.match(new RegExp(`^${name}${escaped} ([0-9.]+)$`, 'm'))
  return match ? Number(match[1]) : 0
}

async function sampleMetrics(
  baseUrl: string,
  databaseId: string,
  started: number,
  generatedEvents: number,
): Promise<MetricSample> {
  const response = await fetch(`${baseUrl}/metrics`)
  if (!response.ok) throw new Error(`metrics returned ${response.status}`)
  const body = await response.text()
  const labels = `{database="${databaseId}"}`
  const ingestedEvents = metric(body, 'pintail_ingested_rows_total')
  return {
    elapsedSeconds: (performance.now() - started) / 1000,
    generatedEvents,
    ingestedEvents,
    eventLag: Math.max(0, generatedEvents - ingestedEvents),
    lagSeconds: metric(body, 'pintail_replication_lag_seconds', labels),
    rssBytes: metric(body, 'pintail_process_resident_memory_bytes'),
    deadLetters: metric(body, 'pintail_dead_letters', labels),
  }
}

async function writeBatch(
  connection: mysql.Connection,
  batch: number,
  nextId: number,
  previousInsertStart: number | undefined,
) {
  await connection.query('CALL soak_db.apply_batch(?, ?, ?)', [
    batch,
    nextId,
    previousInsertStart ?? 0,
  ])
  return (
    insertsPerBatch +
    updatesPerBatch +
    (previousInsertStart === undefined ? 0 : deletesPerBatch)
  )
}

async function sourceChecksum(connection: mysql.Connection): Promise<Checksum> {
  const [rows] = await connection.query<mysql.RowDataPacket[]>(`
    SELECT COUNT(*) AS row_count,
           SUM(MOD(id, 1000003)) AS id_checksum,
           SUM(revision) AS revisions,
           MIN(id) AS minimum_id,
           MAX(id) AS maximum_id
    FROM soak_db.events
  `)
  return {
    rows: Number(rows[0].row_count),
    idChecksum: Number(rows[0].id_checksum),
    revisions: Number(rows[0].revisions),
    minimumId: Number(rows[0].minimum_id),
    maximumId: Number(rows[0].maximum_id),
  }
}

async function replicaChecksum(
  baseUrl: string,
  token: string,
  databaseId: string,
): Promise<Checksum> {
  const result = await api<{ rows: unknown[][] }>(baseUrl, '/api/query', {
    method: 'POST',
    token,
    body: {
      db: databaseId,
      sql:
        'SELECT COUNT(*) AS row_count, SUM(id % 1000003) AS id_checksum, ' +
        'SUM(revision) AS revisions, MIN(id) AS minimum_id, MAX(id) AS maximum_id ' +
        'FROM events',
    },
  })
  const row = result.rows[0]
  return {
    rows: Number(row[0]),
    idChecksum: Number(row[1]),
    revisions: Number(row[2]),
    minimumId: Number(row[3]),
    maximumId: Number(row[4]),
  }
}

function equalChecksum(left: Checksum, right: Checksum) {
  return (
    left.rows === right.rows &&
    left.idChecksum === right.idChecksum &&
    left.revisions === right.revisions &&
    left.minimumId === right.minimumId &&
    left.maximumId === right.maximumId
  )
}

function average(values: number[]) {
  return values.reduce((sum, value) => sum + value, 0) / Math.max(1, values.length)
}

function rssSlopeBytesPerHour(samples: MetricSample[]) {
  if (samples.length < 2) return 0
  const xs = samples.map((sample) => sample.elapsedSeconds)
  const ys = samples.map((sample) => sample.rssBytes)
  const meanX = average(xs)
  const meanY = average(ys)
  const numerator = xs.reduce(
    (sum, x, index) => sum + (x - meanX) * (ys[index] - meanY),
    0,
  )
  const denominator = xs.reduce((sum, x) => sum + (x - meanX) ** 2, 0)
  return denominator === 0 ? 0 : (numerator / denominator) * 3600
}

function publish(
  samples: MetricSample[],
  generatedEvents: number,
  writerSeconds: number,
  source: Checksum,
  replica: Checksum,
) {
  const rss = samples.map((sample) => sample.rssBytes)
  const third = Math.max(1, Math.floor(samples.length / 3))
  const firstThirdRss = average(rss.slice(0, third))
  const lastThirdRss = average(rss.slice(-third))
  const slope = rssSlopeBytesPerHour(samples)
  const throughput = generatedEvents / writerSeconds
  const maxEventLag = Math.max(...samples.map((sample) => sample.eventLag))
  const maxLagSeconds = Math.max(...samples.map((sample) => sample.lagSeconds))
  const maxRss = Math.max(...rss)
  const initialRss = rss[0] ?? 0
  const maxDeadLetters = Math.max(...samples.map((sample) => sample.deadLetters))
  const gates = {
    duration: writerSeconds >= 1800,
    throughput: throughput >= 5000,
    convergence: equalChecksum(source, replica),
    eventLag: maxEventLag <= 330_000,
    timeLag: maxLagSeconds <= 60,
    deadLetters: maxDeadLetters === 0,
    rssLastThird: lastThirdRss <= firstThirdRss + 256 * 1024 * 1024,
    rssMaximum: maxRss <= initialRss + 512 * 1024 * 1024,
    rssSlope: slope <= 128 * 1024 * 1024,
  }
  const passed = Object.values(gates).every(Boolean)
  const suffix = fullGate ? '' : '-smoke'
  const report = {
    generatedAt: new Date().toISOString(),
    configuration: {
      durationSeconds,
      targetEventsPerSecond,
      insertsPerBatch,
      updatesPerBatch,
      deletesPerBatch,
    },
    outcome: {
      writerSeconds,
      generatedEvents,
      throughput,
      maxEventLag,
      maxLagSeconds,
      initialRss,
      maxRss,
      firstThirdRss,
      lastThirdRss,
      rssSlopeBytesPerHour: slope,
      maxDeadLetters,
      source,
      replica,
    },
    gate: { enforced: fullGate, passed, checks: gates },
    samples,
  }
  writeFileSync(
    join(loadgenDir, `results${suffix}.json`),
    `${JSON.stringify(report, null, 2)}\n`,
  )
  const mib = (bytes: number) => (bytes / 1024 / 1024).toFixed(1)
  const lines = [
    '# Pintail CDC soak results',
    '',
    `Measured ${report.generatedAt} for ${writerSeconds.toFixed(1)} seconds.`,
    '',
    '| Measurement | Result |',
    '|---|---:|',
    `| Generated events | ${generatedEvents.toLocaleString()} |`,
    `| Throughput | ${throughput.toFixed(1)} events/s |`,
    `| Maximum event lag | ${maxEventLag.toLocaleString()} |`,
    `| Maximum time lag | ${maxLagSeconds.toFixed(1)} s |`,
    `| Initial / maximum RSS | ${mib(initialRss)} / ${mib(maxRss)} MiB |`,
    `| First / last third average RSS | ${mib(firstThirdRss)} / ${mib(lastThirdRss)} MiB |`,
    `| Fitted RSS slope | ${mib(slope)} MiB/hour |`,
    `| Maximum DLQ depth | ${maxDeadLetters} |`,
    `| Source / replica rows | ${source.rows.toLocaleString()} / ${replica.rows.toLocaleString()} |`,
    '',
    fullGate
      ? `Release gate: ${passed ? 'PASS' : 'FAIL'}.`
      : 'Smoke duration only: the release soak gate was not enforced.',
    '',
  ]
  writeFileSync(join(loadgenDir, `results${suffix}.md`), `${lines.join('\n')}\n`)
  if (fullGate && !passed) {
    throw new Error(`soak gate failed: ${JSON.stringify(gates)}`)
  }
}

async function cleanup() {
  if (mysqlConnection) {
    await mysqlConnection.end().catch(() => undefined)
    mysqlConnection = undefined
  }
  if (pintailProcess) {
    pintailProcess.kill('SIGTERM')
    const exited = await Promise.race([
      pintailProcess.exited.then(() => true),
      Bun.sleep(10_000).then(() => false),
    ])
    if (!exited) pintailProcess.kill('SIGKILL')
    pintailProcess = undefined
  }
  if (dockerCreated) {
    await docker('rm', '--force', '--volumes', mysqlName).catch(() => undefined)
    await docker('network', 'rm', networkName).catch(() => undefined)
  }
  rmSync(dataDir, { recursive: true, force: true })
}

async function main() {
  const info = await docker('info', '--format', '{{.Name}} {{.ServerVersion}} {{.OSType}}')
  log(`Docker: ${info.stdout}`)
  await docker('network', 'create', networkName)
  dockerCreated = true
  await docker(
    'run',
    '--detach',
    '--name',
    mysqlName,
    '--network',
    networkName,
    '--publish',
    '0:3306',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    'mysql:8.4',
    '--server-id=91',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  await seedSource(mysqlConnection)

  const binary = await buildPintail()
  const httpPort = await freePort()
  const wirePort = await freePort()
  const pintailUrl = `http://127.0.0.1:${httpPort}`
  pintailProcess = Bun.spawn(
    [
      binary,
      '--data-dir',
      dataDir,
      '--http-bind',
      `127.0.0.1:${httpPort}`,
      '--wire-bind',
      `127.0.0.1:${wirePort}`,
    ],
    {
      cwd: repository,
      env: {
        ...process.env,
        PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(256 * 1024 * 1024),
      },
      stdout: 'inherit',
      stderr: 'inherit',
    },
  )
  await waitForHttp(pintailUrl)
  const setup = await api<{ token: string }>(pintailUrl, '/api/auth/setup', {
    method: 'POST',
    body: { email: 'soak@pintail.local', password: 'pintail-release-soak' },
  })
  const dsn = `mysql://replicator:replicatorpass@${dsnHost(host)}:${mysqlPort}/soak_db`
  const databaseId = await createReplica(pintailUrl, setup.token, dsn)
  log(
    `generating ${targetEventsPerSecond.toLocaleString()} events/s for ` +
      `${durationSeconds.toLocaleString()} seconds`,
  )

  const samples: MetricSample[] = []
  const started = performance.now()
  let generatedEvents = 0
  let nextId = 1_000_000
  let previousInsertStart: number | undefined
  let batch = 0
  samples.push(await sampleMetrics(pintailUrl, databaseId, started, generatedEvents))
  while ((performance.now() - started) / 1000 < durationSeconds) {
    const insertStart = nextId
    generatedEvents += await writeBatch(
      mysqlConnection,
      batch,
      insertStart,
      previousInsertStart,
    )
    previousInsertStart = insertStart
    nextId += insertsPerBatch
    batch += 1
    if (batch % 5 === 0) {
      const sample = await sampleMetrics(
        pintailUrl,
        databaseId,
        started,
        generatedEvents,
      )
      samples.push(sample)
      log(
        `${sample.elapsedSeconds.toFixed(0)}s: ${generatedEvents.toLocaleString()} events, ` +
          `lag ${sample.eventLag.toLocaleString()}, RSS ` +
          `${(sample.rssBytes / 1024 / 1024).toFixed(1)} MiB, DLQ ${sample.deadLetters}`,
      )
    }
    const deadline = started + (batch * rowsPerBatch * 1000) / targetEventsPerSecond
    const remaining = deadline - performance.now()
    if (remaining > 0) await Bun.sleep(remaining)
  }
  const writerSeconds = (performance.now() - started) / 1000
  const source = await sourceChecksum(mysqlConnection)
  log('writer stopped; waiting for checksum convergence')
  let replica = await replicaChecksum(pintailUrl, setup.token, databaseId)
  for (let attempt = 0; attempt < 150 && !equalChecksum(source, replica); attempt += 1) {
    await Bun.sleep(2000)
    samples.push(
      await sampleMetrics(pintailUrl, databaseId, started, generatedEvents),
    )
    replica = await replicaChecksum(pintailUrl, setup.token, databaseId)
  }
  samples.push(await sampleMetrics(pintailUrl, databaseId, started, generatedEvents))
  publish(samples, generatedEvents, writerSeconds, source, replica)
}

try {
  await main()
} finally {
  await cleanup()
}

// Dedicated differential lifecycle runner; all containers and processes are owned by this run.
import mysql from 'mysql2/promise'
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { docker, dockerHost, dsnHost, freePort, publishedPort, waitForMysql } from './lib'
import { rows, fields, errorDetails } from './parity-support'

type Case = { id: string; family: string; sql: string; ordered: boolean; sqlMode?: string }
const root = resolve(import.meta.dir, '../..')
const inventory = JSON.parse(readFileSync(join(root, 'validate-out/oracle-inventory.json'), 'utf8')) as { fixtureSQL: string; cases: Case[]; sourceSha256: string }
const requested = process.env.PINTAIL_ORACLE_MYSQL_IMAGE ?? 'mysql:8.4'
try { await docker('image', 'inspect', requested) } catch { await docker('pull', requested) }
const image = (await docker('image', 'inspect', requested, '--format', '{{index .RepoDigests 0}}')).stdout
if (!image.includes('@sha256:')) throw new Error('MySQL image digest is required')
const profiles = (process.env.PINTAIL_REPLAY_PROFILES ?? 'FULL:MINIMAL,FULL:FULL,MINIMAL:FULL').split(',')
const binary = resolve(process.env.PINTAIL_E2E_BINARY ?? join(root, 'target/debug/pintail'))
const reportPath = resolve(process.env.PINTAIL_PARITY_REPLAY_REPORT ?? join(root, 'validate-out/parity-replay.json'))
const outcomes: Record<string, unknown>[] = []
const versions: Record<string, string> = {}
const report = () => {
  mkdirSync(resolve(reportPath, '..'), { recursive: true })
  writeFileSync(reportPath, JSON.stringify({ image, versions, profiles, corpus: inventory.sourceSha256,
    verdict: outcomes.some(o => o.status === 'FAIL') ? 'FAIL' : 'PASS', boundaries: outcomes.filter(o => o.status === 'BOUNDARY').length, outcomes }, null, 2) + '\n')
}
const options = { supportBigNumbers: true, bigNumberStrings: true, dateStrings: true, jsonStrings: true,
  rowsAsArray: true, typeCast: (field: any, next: () => unknown) =>
    ['VAR_STRING', 'STRING', 'BLOB', 'TINY_BLOB', 'MEDIUM_BLOB', 'LONG_BLOB'].includes(field.type) ? field.buffer() : next() }
const seen = new Map<string, number>()
const pack = inventory.cases.filter(c => {
  if (!c.family.startsWith('boundary') && !['enum relational interactions', 'outer join null interactions', 'nullable window frames'].includes(c.family)) return false
  const n = seen.get(c.family) ?? 0
  seen.set(c.family, n + 1)
  return n < 4
})
// Query all stored typed values as well as expressions selected above.
pack.push({ id: 'temporal-source-rounding', family: 'source temporal rounding', sql: 'SELECT * FROM temporal_roundtrip ORDER BY id', ordered: true })
pack.push({ id: 'stored-boundaries', family: 'stored values', sql: 'SELECT * FROM bounds ORDER BY id', ordered: true })

for (const profile of profiles) {
  const [rowImage, metadata] = profile.split(':')
  if (!['FULL', 'MINIMAL', 'NOBLOB'].includes(rowImage!) || !['FULL', 'MINIMAL'].includes(metadata!)) throw new Error(`Invalid replay profile ${profile}`)
  const name = `pintail-parity-replay-${process.pid}-${Date.now()}`
  const data = mkdtempSync(join(tmpdir(), 'pintail-parity-replay-'))
  const httpPort = await freePort(), wirePort = await freePort()
  let processHandle: ReturnType<typeof Bun.spawn> | undefined
  let source: mysql.Connection | undefined, replica: mysql.Connection | undefined
  let token = '', db = '', secret = ''
  const api = async (path: string, body?: unknown) => {
    const response = await fetch(`http://127.0.0.1:${httpPort}${path}`, {
      method: body === undefined ? 'GET' : 'POST', headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` },
      body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(30_000),
    })
    if (!response.ok) throw new Error(`${path}: ${response.status}: ${await response.text()}`)
    return response.json() as Promise<any>
  }
  const until = async (label: string, check: () => Promise<boolean>, timeout = 120_000) => {
    const end = Date.now() + timeout
    let last: unknown
    while (Date.now() < end) {
      try { if (await check()) return } catch (e) { last = e }
      await Bun.sleep(250)
    }
    throw new Error(`${label} timed out: ${String(last ?? '')}`)
  }
  const start = async () => {
    processHandle = Bun.spawn([binary, '--data-dir', data, '--http-bind', `127.0.0.1:${httpPort}`, '--wire-bind', `127.0.0.1:${wirePort}`], {
      cwd: root, stdout: Bun.file(join(data, 'server.log')), stderr: Bun.file(join(data, 'server-error.log')),
      env: { ...process.env, PINTAIL_SUPERVISOR_INTERVAL_MS: '250' },
    })
    await until('healthy', async () => (await fetch(`http://127.0.0.1:${httpPort}/health`)).ok)
  }
  const stop = async () => {
    await replica?.end().catch(() => {}); replica = undefined
    if (processHandle && processHandle.exitCode === null) {
      processHandle.kill('SIGTERM')
      await Promise.race([processHandle.exited, Bun.sleep(10_000)])
      if (processHandle.exitCode === null) { processHandle.kill('SIGKILL'); await processHandle.exited }
    }
  }
  const connectReplica = async () => {
    replica = await mysql.createConnection({ ...options, host: '127.0.0.1', port: wirePort, user: 'app', database: 'app', password: secret })
  }
  const outcome = (phase: string, test: string, status: string, detail: unknown) => outcomes.push({ profile, phase, test, status, detail })
  const compare = async (phase: string, test: string, sql: string, params?: Array<string | number | null | Buffer>, checkMetadata = false, ordered = true) => {
    const query = async (connection: mysql.Connection) => {
      try {
        const [values, metadata] = params === undefined ? await connection.query(sql) : await connection.execute(sql, params)
        return { rows: rows(values as unknown[][], ordered), fields: fields(metadata) }
      } catch (e) { return { error: errorDetails(e) } }
    }
    const expected = await query(source!), actual = await query(replica!)
    const errorMatch = expected.error && actual.error && expected.error.errno !== undefined && expected.error.errno === actual.error.errno && expected.error.sqlState === actual.error.sqlState
    const match = expected.error || actual.error ? errorMatch : JSON.stringify(expected.rows) === JSON.stringify(actual.rows)
    outcome(phase, test, match ? 'PASS' : 'FAIL', { sql, params, expected, actual })
    if (checkMetadata && !expected.error && !actual.error) outcome(phase, `${test}:metadata`, JSON.stringify(expected.fields) === JSON.stringify(actual.fields) ? 'PASS' : 'FAIL', { expected: expected.fields, actual: actual.fields })
  }
  const configure = async (mode = 'ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION') => {
    for (const connection of [source!, replica!]) {
      await connection.query("SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci")
      await connection.query("SET time_zone='+00:00'")
      await connection.query('SET sql_mode=?', [mode])
    }
  }
  const replay = async (phase: string) => {
    for (const c of pack) {
      try { await configure(c.sqlMode || undefined) } catch(e) { outcome(phase, c.id + ':session', 'FAIL', errorDetails(e)); continue }
      await compare(phase, c.id, c.sql, undefined, false, c.ordered)
    }
    report()
  }
  const converge = async (epoch: number) => until(`CDC epoch ${epoch}`, async () => {
    const [result] = await replica!.query('SELECT epoch FROM parity_epoch WHERE id=1')
    return Number((result as unknown[][])[0]?.[0]) === epoch
  })
  try {
    await docker('run', '--detach', '--name', name, '--publish', '0:3306', '--tmpfs', '/var/lib/mysql:rw,size=2g',
      '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', '--env', 'MYSQL_DATABASE=app', image,
      '--server-id=942', '--log-bin=mysql-bin', '--binlog-format=ROW', `--binlog-row-image=${rowImage}`, `--binlog-row-metadata=${metadata}`,
      '--gtid-mode=ON', '--enforce-gtid-consistency=ON', '--default-time-zone=+00:00')
    const host = await dockerHost(), port = await publishedPort(name, 3306)
    const ready = await waitForMysql(host, port); await ready.end()
    // Startup initialization can restart the daemon once; the established connection follows it.
    await Bun.sleep(1_000)
    source = await mysql.createConnection({ ...options, host, port, user: 'root', password: 'pintail-root', database: 'app', multipleStatements: true })
    await source.query("SET NAMES utf8mb4 COLLATE utf8mb4_0900_ai_ci")
    const [version] = await source.query('SELECT VERSION()')
    versions[profile] = String((version as unknown[][])[0]![0])
    await source.query(inventory.fixtureSQL)
    await source.query("CREATE TABLE temporal_roundtrip (id INT PRIMARY KEY, dt0 DATETIME(0), dt3 DATETIME(3), dt6 DATETIME(6), t0 TIME(0), t3 TIME(3))")
    const temporalInsert = "INSERT INTO temporal_roundtrip VALUES (?, '2024-02-29 23:59:59.999999', '2024-02-29 23:59:59.999999', '2024-02-29 23:59:59.999999', '23:59:59.999999', '23:59:59.999999')"
    await source.query(temporalInsert, [1])
    await source.query("SET sql_mode='TIME_TRUNCATE_FRACTIONAL'")
    await source.query(temporalInsert, [2])
    await source.query("SET sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'")
    await source.query('CREATE TABLE parity_epoch (id INT PRIMARY KEY, epoch INT NOT NULL); INSERT INTO parity_epoch VALUES (1,0)')
    await source.query("CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'; GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'")
    await docker('exec', name, 'sh', '-c', 'mysql_tzinfo_to_sql /usr/share/zoneinfo 2>/dev/null | MYSQL_PWD=pintail-root mysql -uroot mysql')
    await start()
    token = (await api('/api/auth/setup', { email: 'parity@example.invalid', password: 'parity-test-password' })).token
    db = (await api('/api/databases', { name: 'app', dsn: `mysql://pintail:pintail@${dsnHost(host)}:${port}/app`, mode: 'cdc' })).id
    secret = (await api(`/api/databases/${db}/api-keys`, { name: 'parity', scopes: ['query', 'read'] })).secret
    const probe = await api(`/api/databases/${db}/probe`)
    if (rowImage !== 'FULL') {
      const capabilities = probe.capabilities ?? probe.report?.capabilities
      outcome('scope-boundary', 'full-row-image-required', capabilities?.full_row_image === false ? 'BOUNDARY' : 'FAIL', capabilities ?? probe)
      continue
    }
    await api(`/api/databases/${db}/snapshot`, { force: false })
    await until('snapshot streaming', async () => (await api(`/api/databases/${db}/snapshot/status`)).state === 'streaming')
    await connectReplica()
    await replay('snapshot')
    await source.query("BEGIN; INSERT INTO bounds (id,n,u,whole,frac,txt) VALUES (9,-7,7,7,7.125,'new'); UPDATE parity_epoch SET epoch=1; COMMIT")
    await converge(1); await replay('cdc-insert')
    await source.query("BEGIN; UPDATE bounds SET frac=-1.125,txt=NULL WHERE id=9; UPDATE bounds SET id=10 WHERE id=7; UPDATE parity_epoch SET epoch=2; COMMIT")
    await converge(2); await replay('cdc-update-key-null')
    await source.query('BEGIN; DELETE FROM bounds WHERE id=9; UPDATE parity_epoch SET epoch=3; COMMIT')
    await converge(3); await replay('cdc-delete')
    await source.query("ALTER TABLE bounds MODIFY frac DECIMAL(22,4) NULL; ALTER TABLE orders MODIFY status ENUM('pending','processing','shipped','delivered','cancelled','new') NOT NULL")
    await source.query("BEGIN; UPDATE bounds SET frac=1.1234 WHERE id=5; UPDATE orders SET status='new' WHERE id=1; UPDATE parity_epoch SET epoch=4; COMMIT")
    await converge(4); await replay('ddl-schema-history')
    // These statements are prepared before the restart and metadata change checks below.
    const params: [string, Array<string | number | null | Buffer>][] = [
      ['SELECT ? AS value', [null]], ['SELECT CAST(? AS UNSIGNED)', ['18446744073709551615']],
      ['SELECT CAST(? AS DECIMAL(20,3))', ['1.005']], ['SELECT HEX(?)', [Buffer.from([0,255,254])]],
      ['SELECT id,frac FROM bounds WHERE id=?', [5]], ['SELECT CAST(? AS DATETIME(6))', ['2024-02-29 23:59:59.999999']],
      ['SELECT id FROM bounds ORDER BY id LIMIT ?', [2]],
    ]
    for (let repeat=0; repeat<2; repeat++) for (let i=0; i<params.length; i++) await compare('prepared', `${repeat}:${i}`, params[i]![0], params[i]![1], true)
    await source.query('ALTER TABLE bounds MODIFY frac DECIMAL(24,5) NULL')
    await source.query('BEGIN; UPDATE bounds SET frac=1.12345 WHERE id=5; UPDATE parity_epoch SET epoch=5; COMMIT')
    await converge(5)
    await compare('prepared-after-ddl', 'reprepare-scale', 'SELECT id,frac FROM bounds WHERE id=?', [5], true)
    for (const mode of ['', 'ONLY_FULL_GROUP_BY', 'NO_UNSIGNED_SUBTRACTION', 'STRICT_TRANS_TABLES', 'ERROR_FOR_DIVISION_BY_ZERO', 'ANSI_QUOTES,PIPES_AS_CONCAT,NO_BACKSLASH_ESCAPES']) {
      for (const [label, connection] of [['mysql', source], ['pintail', replica]] as const) {
        try { await connection!.query(`SET sql_mode='${mode}'`); outcome('session-mode', `${label}:${mode}`, 'PASS', 'accepted') }
        catch(e) { outcome('session-mode', `${label}:${mode}`, 'FAIL', errorDetails(e)) }
      }
      await compare('session-mode', mode, 'SELECT CAST(0 AS UNSIGNED) - 1, 1/0')
      await compare('diagnostics', `${mode}:warnings`, 'SHOW WARNINGS')
      await compare('session-mode', `${mode}:division`, 'SELECT 1/0')
      await compare('diagnostics', `${mode}:division-warnings`, 'SHOW WARNINGS')
      await compare('session-mode', `${mode}:group`, 'SELECT id, MAX(frac) FROM bounds GROUP BY id ORDER BY id')
      await compare('diagnostics', `${mode}:cleared-warnings`, 'SHOW WARNINGS')
    }
    await source.query("SET sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'")
    await replica!.query("SET sql_mode='ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION'")
    for (const zone of ['+00:00', '+05:30', 'America/New_York']) {
      await source.query(`SET time_zone='${zone}'`)
      try { await replica!.query(`SET time_zone='${zone}'`) } catch(e) { outcome('time-zone', zone, 'FAIL', errorDetails(e)); continue }
      await compare('time-zone', zone, 'SELECT id,stamp,dt6 FROM bounds ORDER BY id')
      await compare('time-zone', `${zone}:dst`, "SELECT CONVERT_TZ('2024-03-10 06:59:59','+00:00','America/New_York'), CONVERT_TZ('2024-03-10 07:00:00','+00:00','America/New_York')")
    }
    await source.query("SET time_zone='+00:00'"); await replica!.query("SET time_zone='+00:00'")
    for (const sql of ['SELECT (SELECT n FROM bounds)', 'SELECT unknown_column FROM bounds', 'SELECT id,frac FROM bounds GROUP BY frac', "SELECT JSON_EXTRACT('{}', '$[')"]) await compare('negative', sql, sql)
    await replica!.query("SET time_zone='+05:30'")
    await stop(); await start(); await connectReplica(); await converge(5)
    await source.end()
    source = await mysql.createConnection({ ...options, host, port, user: 'root', password: 'pintail-root', database: 'app' })
    await compare('session-reconnect', 'default-zone', 'SELECT @@session.time_zone')
    await replay('restart')
  } catch(e) {
    outcomes.push({ profile, phase: 'infrastructure', status: 'FAIL', detail: errorDetails(e) })
  } finally {
    await stop(); await source?.end().catch(() => {})
    await docker('rm', '--force', name).catch(() => {})
    report()
  }
}
console.log(`PARITY-REPLAY-DONE: ${outcomes.filter(o => o.status === 'PASS').length}/${outcomes.length} checks passed; ${reportPath}`)
process.exitCode = outcomes.some(o => o.status === 'FAIL') ? 1 : 0

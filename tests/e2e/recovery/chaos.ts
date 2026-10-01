// Chaos lane: a live MySQL source, a Pintail replica over change capture, a
// writer that never stops, and a replica process that is killed again and
// again - by signal at random instants and by failpoint at the durable steps
// of a flush, a merge and a checkpoint.
//
// Each cycle checks two things. While the replica catches up after the kill,
// a table whose keys are never reused must not go backwards: a row the
// replica had already dropped may not be visible again, a row it held may not
// vanish unless the source deleted it, and a row's counter may not decrease.
// Once the writer pauses and the replica reaches the writer's last
// transaction, every table is compared with the source row for row.
//
//   bun tests/e2e/recovery/chaos.ts            (CHAOS_CYCLES=200 by default)
//
// CHAOS_SEED picks the workload, CHAOS_DIR where the data directory, the
// server logs and `chaos.jsonl` (one line per cycle) go, and
// PINTAIL_RECOVERY_BINARY a server built with `--features failpoints`.
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import mysql from 'mysql2/promise'
import { freePort, dsnHost } from '../lib'
import { rows as encodeRows } from '../parity-support'
import { Source, repository, until } from './harness'

const cycles = Number(process.env.CHAOS_CYCLES ?? 200)
const seed = Number(process.env.CHAOS_SEED ?? 4343)
const dir = process.env.CHAOS_DIR ?? join(repository, 'validate-out/chaos', new Date().toISOString().replaceAll(':', '-'))
const binary = process.env.PINTAIL_RECOVERY_BINARY ?? join(repository, 'target/recovery/pintail')
const journalSeed = Number(process.env.CHAOS_JOURNAL_ROWS ?? 200_000)
const dataDir = join(dir, 'data')
mkdirSync(dataDir, { recursive: true })
const ledger = join(dir, 'chaos.jsonl')
const schema = 'chaos'

let random = seed >>> 0
function rand(): number {
  random = (random + 0x6d2b79f5) >>> 0
  let t = random
  t = Math.imul(t ^ (t >>> 15), t | 1)
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296
}
const between = (low: number, high: number) => low + Math.floor(rand() * (high - low + 1))
const pick = <T>(values: T[]) => values[Math.floor(rand() * values.length)]

const sites = [
  'store.flush.after_segment', 'store.flush.after_manifest', 'store.flush.after_wal_reset',
  'store.merge.after_reserve', 'store.merge.before_publish', 'store.merge.after_publish',
  'cdc.after_ingest', 'cdc.after_table_ingest', 'cdc.after_first_table_sync', 'cdc.before_checkpoint_commit', 'cdc.after_checkpoint_commit',
  'store.wal.before_sync', 'cdc.ddl.after_evolve', 'cdc.ddl.after_history',
]

// ---------------------------------------------------------------- source
const source = new Source()
await source.start()
await source.root.query(`CREATE DATABASE ${schema}`)
const writer = await source.connect(schema)
const reader = await source.connect(schema)
await writer.query(`
  CREATE TABLE journal (id BIGINT PRIMARY KEY, ver INT NOT NULL, bucket INT NOT NULL, amount DECIMAL(12,2) NOT NULL, note VARCHAR(40) NULL);
  CREATE TABLE slots (id INT PRIMARY KEY, state VARCHAR(12) NOT NULL, n BIGINT NOT NULL);
  CREATE TABLE pairs (a INT NOT NULL, b INT NOT NULL, v INT NOT NULL, tag VARCHAR(16) NULL, PRIMARY KEY (a, b));
  CREATE TABLE scratch (id INT PRIMARY KEY, v INT NOT NULL);
  CREATE TABLE shape (id INT PRIMARY KEY, v INT NOT NULL);
  CREATE TABLE epoch (id INT PRIMARY KEY, n BIGINT NOT NULL);
  INSERT INTO epoch VALUES (1, 0);
  SET SESSION cte_max_recursion_depth = 10000000;
  INSERT INTO journal WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < ${journalSeed})
    SELECT i, 0, i % 97, (i % 10000) / 100, CONCAT('seed ', i) FROM s;
  INSERT INTO slots WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 40000)
    SELECT i, 'seed', i FROM s;
  INSERT INTO pairs WITH RECURSIVE s(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM s WHERE i < 39999)
    SELECT 1 + i DIV 20, 1 + i % 20, i, NULL FROM s;
  INSERT INTO shape WITH RECURSIVE s(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM s WHERE i < 5000)
    SELECT i, i FROM s;`)

// ---------------------------------------------------------------- replica
const httpPort = await freePort()
const wirePort = await freePort()
let server: ReturnType<typeof Bun.spawn> | undefined
let starts = 0
let currentLog = ''
const alive = () => server !== undefined && server.exitCode === null && server.signalCode === null
function spawnServer(failpoint: string) {
  starts++
  currentLog = join(dir, `server-${String(starts).padStart(4, '0')}.log`)
  server = Bun.spawn([binary, '--data-dir', dataDir, '--http-bind', `127.0.0.1:${httpPort}`, '--wire-bind', `127.0.0.1:${wirePort}`], {
    cwd: repository, stdout: Bun.file(`${currentLog}.out`), stderr: Bun.file(currentLog),
    env: { ...process.env, PINTAIL_FAILPOINT: failpoint, PINTAIL_SUPERVISOR_INTERVAL_MS: '250', PINTAIL_LOG: 'debug',
      PINTAIL_MEMTABLE_KB: process.env.PINTAIL_MEMTABLE_KB ?? '256',
      PINTAIL_COMPACTION_INPUT_ROWS: process.env.PINTAIL_COMPACTION_INPUT_ROWS ?? '120000',
      PINTAIL_COMPACTION_OUTPUT_ROWS: process.env.PINTAIL_COMPACTION_OUTPUT_ROWS ?? '40000' },
  })
}
/** True once the server answers; false when it died first (an armed failpoint may fire during the open). */
async function healthy(timeout = 90_000): Promise<boolean> {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    if (!alive()) return false
    try { if ((await fetch(`http://127.0.0.1:${httpPort}/health`, { signal: AbortSignal.timeout(1000) })).ok) return true } catch {}
    await Bun.sleep(40)
  }
  throw new Error('the replica did not answer within its start deadline')
}
async function kill(signal: 'SIGKILL' | 'SIGTERM'): Promise<string> {
  if (!server) return 'none'
  if (!alive()) return 'already-dead'
  server.kill(signal)
  const exited = await Promise.race([server.exited.then(() => true), Bun.sleep(30_000).then(() => false)])
  if (!exited) { server.kill('SIGKILL'); await server.exited; return 'term-timeout' }
  return signal
}
let token = ''
async function api<T>(path: string, body?: unknown, method = body === undefined ? 'GET' : 'POST'): Promise<T> {
  const response = await fetch(`http://127.0.0.1:${httpPort}${path}`, { method,
    headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}) },
    body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15_000) })
  const text = await response.text()
  if (!response.ok) throw new Error(`${path}: HTTP ${response.status}: ${text}`)
  return (text ? JSON.parse(text) : undefined) as T
}
spawnServer('')
if (!await healthy()) throw new Error('the replica did not start')
token = (await api<{ token: string }>('/api/auth/setup', { email: 'chaos@example.invalid', password: 'chaos-lane-password' })).token
const database = (await api<{ id: string }>('/api/databases', { name: schema, mode: 'cdc', keyless_policy: 'auto_resync',
  dsn: `mysql://pintail:pintail@${dsnHost(source.host)}:${source.port}/${schema}` })).id
await api(`/api/databases/${database}/probe`)
const key = (await api<{ secret: string }>(`/api/databases/${database}/api-keys`, { name: 'chaos', scopes: ['read', 'query'] })).secret
await api(`/api/databases/${database}/snapshot`, { force: false })
let replica: mysql.Connection | undefined
async function replicaRows(sql: string, timeout = 30_000): Promise<unknown[][]> {
  replica ??= await mysql.createConnection({ host: '127.0.0.1', port: wirePort, user: schema, password: key, database: schema,
    supportBigNumbers: true, bigNumberStrings: true, dateStrings: true, connectTimeout: 3_000 })
  try {
    const [result] = await replica.query({ sql, rowsAsArray: true, timeout })
    return result as unknown[][]
  } catch (error) { replica.destroy(); replica = undefined; throw error }
}
async function sourceRows(sql: string): Promise<unknown[][]> {
  const [result] = await reader.query({ sql, rowsAsArray: true, timeout: 60_000 })
  return result as unknown[][]
}
const status = () => api<{ state: string; tables: Array<{ name: string; state: string }> }>(`/api/databases/${database}/snapshot/status`)
await until('first copy streams', async () => (await status()).state === 'streaming', 300_000)

// ---------------------------------------------------------------- writer
let nextJournal = journalSeed + 1
let lowJournal = 1
let nextScratch = 1
let shapeColumns: string[] = []
let shapeColumnSerial = 0
let alterAllowed = false
let transactions = 0
let writing = true
let writerIdle: Promise<void> = Promise.resolve()
/** When the source acknowledged the delete of a journal id. */
const deletedAt = new Map<number, number>()
type Op = { sql: string; deletes?: number[] }
function journalOp(): Op {
  const roll = rand()
  if (roll < 0.34) {
    const count = between(20, 400)
    const values = Array.from({ length: count }, () => { const id = nextJournal++; return `(${id},0,${id % 97},${(id % 10000) / 100},'new ${id}')` })
    return { sql: `INSERT INTO journal VALUES ${values.join(',')}` }
  }
  const from = between(lowJournal, nextJournal - 1)
  if (roll < 0.74) {
    const to = from + between(1, 2500)
    return { sql: `UPDATE journal SET ver = ver + 1, amount = amount + 0.25, note = 'u${transactions}' WHERE id BETWEEN ${from} AND ${to}` }
  }
  if (roll < 0.9) {
    const to = Math.min(from + between(1, 300), nextJournal - 1)
    const ids = Array.from({ length: to - from + 1 }, (_, index) => from + index)
    return { sql: `DELETE FROM journal WHERE id BETWEEN ${from} AND ${to}`, deletes: ids }
  }
  const modulus = between(3, 11), remainder = between(0, modulus - 1), to = Math.min(from + 4000, nextJournal - 1)
  const ids: number[] = []
  for (let id = from; id <= to; id++) if (id % modulus === remainder) ids.push(id)
  return { sql: `DELETE FROM journal WHERE id BETWEEN ${from} AND ${to} AND id % ${modulus} = ${remainder}`, deletes: ids }
}
function slotsOp(): Op {
  const from = between(1, 39_500), to = from + between(1, 400), roll = rand()
  const values = Array.from({ length: to - from + 1 }, (_, index) => `(${from + index},'r${transactions % 1000}',${transactions})`).join(',')
  if (roll < 0.3) return { sql: `DELETE FROM slots WHERE id BETWEEN ${from} AND ${to}; INSERT INTO slots VALUES ${values}` }
  if (roll < 0.45) return { sql: `DELETE FROM slots WHERE id BETWEEN ${from} AND ${to}` }
  if (roll < 0.6) return { sql: `INSERT IGNORE INTO slots VALUES ${values}` }
  if (roll < 0.75) return { sql: `REPLACE INTO slots VALUES ${values}` }
  if (roll < 0.87) return { sql: `INSERT INTO slots VALUES ${values} ON DUPLICATE KEY UPDATE n = n + 1, state = 'dup'` }
  return { sql: `UPDATE slots SET n = n + 1, state = 'upd' WHERE id BETWEEN ${from} AND ${to}` }
}
function pairsOp(): Op {
  const a = between(1, 2000), roll = rand()
  if (roll < 0.25) return { sql: `UPDATE IGNORE pairs SET b = b + 1000 WHERE a = ${a} AND b < 1000` }
  if (roll < 0.5) return { sql: `UPDATE IGNORE pairs SET b = b - 1000 WHERE a = ${a} AND b >= 1000` }
  if (roll < 0.6) return { sql: `UPDATE IGNORE pairs SET a = a + 100000 WHERE a BETWEEN ${a} AND ${a + 8}` }
  if (roll < 0.7) return { sql: `UPDATE IGNORE pairs SET a = a - 100000 WHERE a BETWEEN ${a + 100000} AND ${a + 100008}` }
  if (roll < 0.8) return { sql: `DELETE FROM pairs WHERE a BETWEEN ${a} AND ${a + 5}` }
  if (roll < 0.9) return { sql: `INSERT IGNORE INTO pairs VALUES ${Array.from({ length: 120 }, (_, index) => `(${a + (index % 6)},${1 + Math.floor(index / 6)},${transactions},'back')`).join(',')}` }
  return { sql: `UPDATE pairs SET v = v + 1, tag = 't${transactions % 100}' WHERE a BETWEEN ${a} AND ${a + 40}` }
}
function scratchOp(truncates = true): Op {
  if (truncates && rand() < 0.08) { nextScratch = 1; return { sql: 'TRUNCATE TABLE scratch' } }
  const values = Array.from({ length: between(10, 200) }, () => `(${nextScratch++},${transactions})`).join(',')
  return { sql: `INSERT INTO scratch VALUES ${values}` }
}
function shapeOp(): Op {
  const from = between(1, 4900)
  const set = shapeColumns.length ? `, ${pick(shapeColumns)} = id % 7` : ''
  return { sql: `UPDATE shape SET v = v + 1${set} WHERE id BETWEEN ${from} AND ${from + between(1, 300)}` }
}
async function alterShape() {
  if (shapeColumns.length < 3 && rand() < 0.7) {
    const column = `c${++shapeColumnSerial}`
    await writer.query(`ALTER TABLE shape ADD COLUMN ${column} INT NULL`)
    shapeColumns.push(column)
  } else if (shapeColumns.length) {
    await writer.query(`ALTER TABLE shape DROP COLUMN ${shapeColumns.shift()}`)
  }
}
async function writeLoop() {
  while (writing) {
    transactions++
    const roll = rand()
    if (alterAllowed && roll < 0.004) { alterAllowed = false; await alterShape(); continue }
    const op = (truncates = true) => { const r = rand(); return r < 0.5 ? journalOp() : r < 0.7 ? slotsOp() : r < 0.87 ? pairsOp() : r < 0.94 ? scratchOp(truncates) : shapeOp() }
    if (roll < 0.04) {
      // Rolled back: nothing of it may reach the replica.
      await writer.query(`START TRANSACTION; ${slotsOp().sql}; ${pairsOp().sql}; ${shapeOp().sql}; ROLLBACK`)
    } else if (roll < 0.3) {
      const ops = Array.from({ length: between(2, 4) }, () => op(false))
      await writer.query(`START TRANSACTION; ${ops.map(o => o.sql).join('; ')}; COMMIT`)
      const now = Date.now()
      for (const o of ops) for (const id of o.deletes ?? []) if (!deletedAt.has(id)) deletedAt.set(id, now)
    } else {
      const single = op()
      await writer.query(single.sql)
      const now = Date.now()
      for (const id of single.deletes ?? []) if (!deletedAt.has(id)) deletedAt.set(id, now)
    }
    // Keep the journal near its seeded size: retire the oldest keys in bulk.
    if (nextJournal - lowJournal > journalSeed * 1.3 && rand() < 0.05) lowJournal += between(100, 3000)
    if (transactions % 8 === 0) await Bun.sleep(between(0, 25))
  }
}
function startWriter() { writing = true; writerIdle = writeLoop() }
async function pauseWriter() { writing = false; await writerIdle }

// Queries that keep the scan paths busy between the checks: they build the
// newer segments' key index and read the memtable image while both change.
let probing = true
const prober = (async () => {
  const probe = await mysql.createPool({ host: '127.0.0.1', port: wirePort, user: schema, password: key, database: schema, connectionLimit: 2, connectTimeout: 2_000 })
  const statements = [
    'SELECT bucket, COUNT(*), SUM(amount), MAX(ver) FROM journal GROUP BY bucket ORDER BY bucket',
    'SELECT COUNT(*), SUM(n) FROM slots WHERE state <> \'seed\'',
    'SELECT a, COUNT(*), SUM(v) FROM pairs WHERE a < 300 GROUP BY a ORDER BY a',
    'SELECT COUNT(*), MAX(id) FROM journal WHERE ver > 2',
    'SELECT j.bucket, COUNT(*) FROM journal j JOIN slots s ON s.id = j.bucket + 1 GROUP BY j.bucket ORDER BY j.bucket',
  ]
  while (probing) {
    try { await probe.query({ sql: pick(statements), timeout: 20_000 }) } catch {}
    await Bun.sleep(60)
  }
  await probe.end().catch(() => {})
})()

// ---------------------------------------------------------------- checks
type View = Map<number, number>
async function journalView(): Promise<{ view: View; started: number; ended: number }> {
  const started = Date.now()
  const result = await replicaRows('SELECT id, ver FROM journal')
  const view: View = new Map()
  for (const row of result) view.set(Number(row[0]), Number(row[1]))
  return { view, started, ended: Date.now() }
}
async function tableDiff(table: string): Promise<string | undefined> {
  const columns = (await sourceRows(`SELECT column_name FROM information_schema.columns WHERE table_schema='${schema}' AND table_name='${table}' ORDER BY ordinal_position`)).map(row => `\`${row[0]}\``)
  const keys = (await sourceRows(`SELECT column_name FROM information_schema.key_column_usage WHERE table_schema='${schema}' AND table_name='${table}' AND constraint_name='PRIMARY' ORDER BY ordinal_position`)).map(row => `\`${row[0]}\``)
  const sql = `SELECT ${columns.join(',')} FROM \`${table}\` ORDER BY ${keys.join(',')}`
  const [want, got] = [encodeRows(await sourceRows(sql)), encodeRows(await replicaRows(sql, 120_000))]
  if (want.length !== got.length) {
    const have = new Set(got), wanted = new Set(want)
    const missing = want.filter(row => !have.has(row)).slice(0, 3), extra = got.filter(row => !wanted.has(row)).slice(0, 3)
    return `${table}: ${want.length} source rows, ${got.length} replica rows; missing ${JSON.stringify(missing)} extra ${JSON.stringify(extra)}`
  }
  const index = want.findIndex((row, i) => row !== got[i])
  return index < 0 ? undefined : `${table}: row ${index} source=${want[index]} replica=${got[index]}`
}
const tables = ['journal', 'slots', 'pairs', 'scratch', 'shape', 'epoch']
async function parity(): Promise<string[]> {
  const diffs: string[] = []
  for (const table of tables) {
    try { const diff = await tableDiff(table); if (diff) diffs.push(diff) }
    catch (error) { diffs.push(`${table}: ${String(error).slice(0, 300)}`) }
  }
  return diffs
}
function logFlags(path: string): string[] {
  let text = ''
  try { text = readFileSync(path, 'utf8') } catch { return [] }
  const flags: string[] = []
  for (const [flag, pattern] of [['panic', /panicked/], ['renumbered', /numbering restarted/], ['needs-resync', /needs_resync|quarantined/],
    ['recopy', /resnapshot|copied again|recopy/i], ['corrupt', /corrupt/i], ['fold', /folds one key range/], ['index', /indexed the keys/]] as const) {
    const count = text.split('\n').filter(line => pattern.test(line)).length
    if (count) flags.push(`${flag}:${count}`)
  }
  return flags
}
const failpointFired = (path: string) => { try { return /failpoint \S+ hit \d+: aborting/.exec(readFileSync(path, 'utf8'))?.[0] } catch { return undefined } }

// ---------------------------------------------------------------- cycles
let violations = 0, parityFailures = 0, late = 0
const killCounts: Record<string, number> = {}
startWriter()
let epoch = 0
let purgeTo = ''
let armed = ''
for (let cycle = 1; cycle <= cycles; cycle++) {
  const record: Record<string, unknown> = { cycle, start: starts, armed }
  const problems: string[] = []
  alterAllowed = cycle % 3 === 0
  await Bun.sleep(between(300, 2500))
  // What the replica shows before it dies.
  let before: Awaited<ReturnType<typeof journalView>> | undefined
  try { if (alive()) before = await journalView() } catch (error) { record.beforeError = String(error).slice(0, 200) }
  await Bun.sleep(between(0, 1200))
  // The kill.
  const logBefore = currentLog
  let how: string
  if (armed) {
    const deadline = Date.now() + 6000
    while (alive() && Date.now() < deadline) await Bun.sleep(20)
    how = alive() ? await kill('SIGKILL') + '(failpoint not reached)' : (failpointFired(logBefore) ?? 'died')
    if (!alive() && server) await server.exited
  } else {
    how = await kill(rand() < 0.25 ? 'SIGTERM' : 'SIGKILL')
  }
  replica?.destroy(); replica = undefined
  record.kill = how
  const kind = how.replace(/ hit \d+: aborting/, '').replace(/^failpoint /, '')
  killCounts[kind] = (killCounts[kind] ?? 0) + 1
  record.flagsBefore = logFlags(logBefore)
  await Bun.sleep(between(0, 1500))
  // A second death while the first one is still being recovered from.
  if (rand() < 0.2) {
    spawnServer('')
    await Bun.sleep(between(15, 600))
    record.killDuringOpen = await kill('SIGKILL')
    record.flagsDuringOpen = logFlags(currentLog)
  }
  armed = rand() < 0.4 ? `${pick(sites)}@${between(1, 40)}` : ''
  spawnServer(armed)
  let up = await healthy()
  if (!up) {
    // The armed site fired while the tables were opening: that is a kill too.
    record.diedOpening = failpointFired(currentLog) ?? 'died without a failpoint'
    if (!failpointFired(currentLog)) problems.push(`the replica died while opening: see ${currentLog}`)
    armed = ''
    spawnServer('')
    up = await healthy()
    if (!up) { problems.push('the replica cannot start'); record.problems = problems; appendFileSync(ledger, JSON.stringify(record) + '\n'); break }
  }
  // While it catches up: nothing it showed before the kill may be undone.
  let maximum = 0
  if (before) for (const id of before.view.keys()) if (id > maximum) maximum = id
  const missing = new Map<number, number>()
  let polls = 0, pollErrors = 0
  const pollUntil = Date.now() + between(500, 2500)
  while (before && Date.now() < pollUntil && alive()) {
    let now: Awaited<ReturnType<typeof journalView>>
    try { now = await journalView() } catch (error) { pollErrors++; record.pollError = String(error).slice(0, 200); await Bun.sleep(30); continue }
    polls++
    let reappeared = 0, regressed = 0
    const samples: string[] = []
    for (const [id, version] of now.view) {
      if (id > maximum) continue
      const had = before.view.get(id)
      if (had === undefined) { reappeared++; if (samples.length < 5) samples.push(`reappeared ${id}@${version}`) }
      else if (version < had) { regressed++; if (samples.length < 5) samples.push(`regressed ${id}: ${had} -> ${version}`) }
    }
    for (const id of before.view.keys()) if (!now.view.has(id) && !missing.has(id)) missing.set(id, now.ended)
    if (reappeared || regressed) problems.push(`poll ${polls}: ${reappeared} deleted rows visible again, ${regressed} rows went back a version (${samples.join('; ')})`)
    await Bun.sleep(40)
  }
  record.polls = polls
  if (pollErrors) record.pollErrors = pollErrors
  // Quiesce and compare everything.
  await pauseWriter()
  const lost = [...missing].filter(([id, seen]) => { const at = deletedAt.get(id); return at === undefined || at > seen + 100 })
  if (lost.length) problems.push(`${lost.length} rows the replica held before the kill were absent before the source deleted them (${lost.slice(0, 5).map(([id]) => id).join(', ')})`)
  epoch++
  await writer.query(`UPDATE epoch SET n = ${epoch} WHERE id = 1`)
  const waited = Date.now()
  let diffs: string[] = ['not compared']
  let reached = false
  while (Date.now() - waited < 240_000) {
    if (!alive()) {
      // An armed site reached during the catch-up: start again, unarmed.
      record.diedCatchingUp = failpointFired(currentLog) ?? 'died without a failpoint'
      if (!failpointFired(currentLog)) problems.push(`the replica died while catching up: see ${currentLog}`)
      replica?.destroy(); replica = undefined
      armed = ''
      spawnServer('')
      if (!await healthy()) break
    }
    try { reached = String((await replicaRows('SELECT n FROM epoch WHERE id = 1'))[0]?.[0]) === String(epoch) } catch { reached = false }
    if (reached) {
      diffs = await parity()
      if (!diffs.length) break
      // A table being copied again answers late; give it the rest of the deadline.
      await Bun.sleep(1000)
    } else await Bun.sleep(100)
  }
  record.convergeMs = Date.now() - waited
  record.reachedEpoch = reached
  if (diffs.length) { parityFailures++; problems.push(...diffs.map(diff => `parity: ${diff}`)) }
  else if (record.convergeMs as number > 60_000) late++
  try { const state = await status(); const odd = state.tables.filter(t => !['streaming', 'completed'].includes(t.state)); if (state.state !== 'streaming' || odd.length) record.states = { database: state.state, tables: odd } } catch {}
  record.flagsAfter = logFlags(currentLog)
  record.journal = { next: nextJournal, transactions }
  if (problems.length) { violations++; record.problems = problems }
  appendFileSync(ledger, JSON.stringify(record) + '\n')
  console.log(`cycle ${cycle}: ${how}${problems.length ? ` PROBLEMS ${JSON.stringify(problems).slice(0, 600)}` : ' ok'} (${record.convergeMs} ms)`)
  if (!diffs.length && cycle % 10 === 0) {
    // Rotate the source log and drop the files the replica finished with ten cycles ago.
    const [current] = await sourceRows('SHOW BINARY LOG STATUS')
    await writer.query('FLUSH BINARY LOGS')
    if (purgeTo) await writer.query(`PURGE BINARY LOGS TO '${purgeTo}'`)
    purgeTo = String(current[0])
  }
  if (diffs.length) { console.log('a difference outlived the catch-up deadline: stopping with the data directory kept'); break }
  startWriter()
}
await pauseWriter()
probing = false
await prober
const summary = { seed, cycles, completed: readFileSync(ledger, 'utf8').trim().split('\n').length, violations, parityFailures, late, killCounts, starts }
writeFileSync(join(dir, 'summary.json'), JSON.stringify(summary, null, 2))
console.log(`CHAOS-DONE ${JSON.stringify(summary)}`)
replica?.destroy()
if (!parityFailures) { await kill('SIGTERM'); writer.destroy(); reader.destroy(); await source.close() }
process.exit(violations ? 1 : 0)

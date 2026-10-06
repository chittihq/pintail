/// How far, and for how long, a mirror drifts from MySQL when MySQL applies
/// a foreign-key action itself - and what repairing that drift costs.
///
/// `ON DELETE/UPDATE CASCADE` and `SET NULL` run inside InnoDB and write no
/// row events for the child rows they change, so change capture never sees
/// them. Pintail flags the child tables and repairs them with a cascade
/// reconciliation pass every `reconcile_interval_seconds`. This harness
/// measures that trade on an invented schema:
///
///   drift  - concurrent writers over a two-level CASCADE chain and a
///            composite-key SET NULL / ON UPDATE CASCADE pair, for each
///            reconcile interval. Every sample first reads both sides as
///            they stand (lag reads BEHIND, an unrepaired action AHEAD),
///            then pauses the writers, waits for the mirror to catch up
///            with a marker row and diffs again: whatever still differs is
///            the invisible action alone. Each stale row's lifetime is
///            tracked until a pass removes it.
///   cost   - one cascade pass over child tables of each size, on an idle
///            server: wall time (from the server's own log line), process
///            CPU, peak resident memory, rows compared and verified.
///   checks - shapes the repair must get right: `foreign_key_checks = 0`
///            deletes (no cascade at the source), a parent key deleted and
///            inserted again before a pass, REPLACE on a parent (also
///            through two levels), a composite parent key moved then reused,
///            `ON UPDATE CASCADE` into a child's primary key (then the old
///            key reused), and a child without a key beside keyed ones.
///
/// Linux only: it reads the server's CPU and memory from /proc.
///
/// Run with: PINTAIL_BINARY=target/release/pintail bun run fk-cascade-drift.ts
///   DRIFT_PHASES=drift,cost,checks   which phases (default all three)
///   DRIFT_INTERVALS=600,60,15        reconcile intervals the drift phase runs
///   DRIFT_SECONDS=<n>                seconds per interval (default 1.5 x interval + 60, at least 180)
///   DRIFT_SIZES=100000,1000000,10000000  child-table sizes the cost phase seeds
///   DRIFT_WRITERS=4                  concurrent writer connections (DRIFT_WRITER_PAUSE_MS=200 think time each)
///   DRIFT_COST_TAGS=1                tags per tagged item in the cost phase (0: single-level only)
/// Results go to validate-out/fk-cascade-drift/<run>/ (results.json, notes.log, pintail.log).

import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs'
import { openSync, readSync, closeSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { docker, dockerHost, dsnHost, freePort, publishedPort, waitForMysql } from './lib.ts'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const outDir = join(repository, 'validate-out', 'fk-cascade-drift', nonce)
const mysqlName = `pintail-fkdrift-mysql-${process.pid}-${nonce}`
const PHASES = new Set((process.env.DRIFT_PHASES ?? 'drift,cost,checks').split(',').map((p) => p.trim()))
const INTERVALS = (process.env.DRIFT_INTERVALS ?? '600,60,15').split(',').map(Number)
const SIZES = (process.env.DRIFT_SIZES ?? '100000,1000000,10000000').split(',').map(Number)
const WRITERS = Number(process.env.DRIFT_WRITERS ?? 4)
/// Writer think time between operations, per writer.
const WRITER_PAUSE_MS = Number(process.env.DRIFT_WRITER_PAUSE_MS ?? 200)
/// Seconds of writing between two synced samples.
const SAMPLE_EVERY_S = Number(process.env.DRIFT_SAMPLE_S ?? 10)
/// An interval the server never reaches on its own within a run.
const IDLE_INTERVAL_S = 1_000_000_000
const CLOCK_TICKS = 100

mkdirSync(outDir, { recursive: true })
const logPath = join(outDir, 'pintail.log')
const notes: string[] = []
const results: Record<string, unknown> = { startedAt: new Date().toISOString() }

function log(message: string) {
  const line = `[fk-drift ${new Date().toISOString().slice(11, 19)}] ${message}`
  console.log(line)
  notes.push(line)
}

let mysqlHost = ''
let mysqlPort = 0
let root: mysql.Connection | undefined
let pintail: ReturnType<typeof Bun.spawn> | undefined
let pintailUrl = ''
let wirePort = 0
let token = ''
let dataDir = ''

async function api<T>(path: string, options: { method?: string; body?: unknown; auth?: boolean } = {}): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      'content-type': 'application/json',
      ...(options.auth === false ? {} : { authorization: `Bearer ${token}` }),
    },
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} returned ${response.status}: ${text.slice(0, 400)}`)
  return (text ? JSON.parse(text) : undefined) as T
}

// ---------------------------------------------------------------------------
// The server's own account: log lines, CPU and memory

/// Server log lines as they arrive, stamped with the harness clock (the
/// server's lines carry no time of their own).
const logLines: Array<{ at: number; line: string }> = []
let logOffset = 0
function readLog() {
  let size = 0
  try {
    size = statSync(logPath).size
  } catch {
    return
  }
  if (size <= logOffset) return
  const fd = openSync(logPath, 'r')
  const buffer = Buffer.alloc(size - logOffset)
  readSync(fd, buffer, 0, buffer.length, logOffset)
  closeSync(fd)
  const text = buffer.toString('utf8')
  const end = text.lastIndexOf('\n')
  if (end < 0) return
  logOffset += Buffer.byteLength(text.slice(0, end + 1))
  const at = Date.now()
  for (const line of text.slice(0, end).split('\n')) logLines.push({ at, line })
}
const logTimer = setInterval(readLog, 100)

interface PassLine {
  at: number
  db: string
  tables: number
  rowsCompared: number
  tombstoned: number
  refreshed: number
  elapsedMs: number
  /// The part of the pass the replication cycle waited for (absent before
  /// the pass moved off the cycle, when all of it was).
  applyMs?: number
}
function passes(db: string, since: number): PassLine[] {
  readLog()
  const out: PassLine[] = []
  for (const { at, line } of logLines) {
    if (at < since) continue
    const m = /cascade reconcile db=(\S+) tables=(\d+) rows_compared=(\d+) tombstoned=(\d+) refreshed=(\d+) elapsed_ms=(\d+)(?: apply_ms=(\d+))?/.exec(line)
    if (m && m[1] === db) {
      out.push({
        at, db, tables: Number(m[2]), rowsCompared: Number(m[3]), tombstoned: Number(m[4]), refreshed: Number(m[5]), elapsedMs: Number(m[6]),
        applyMs: m[7] === undefined ? undefined : Number(m[7]),
      })
    }
  }
  return out
}
function tablePasses(db: string, since: number) {
  readLog()
  const out: Array<{ at: number; table: string; rowsCompared: number; sourceVerified: number; tombstoned: number; refreshed: number; elapsedMs: number }> = []
  for (const { at, line } of logLines) {
    if (at < since) continue
    const m = /cascade reconcile db=(\S+) table=(\S+) rows_compared=(\d+) source_verified=(\d+) tombstoned=(\d+) refreshed=(\d+)(?: elapsed_ms=(\d+))?/.exec(line)
    if (m && m[1] === db) out.push({ at, table: m[2]!, rowsCompared: Number(m[3]), sourceVerified: Number(m[4]), tombstoned: Number(m[5]), refreshed: Number(m[6]), elapsedMs: Number(m[7] ?? 0) })
  }
  return out
}

function processSample(): { at: number; cpuS: number; rssMb: number } {
  const stat = readFileSync(`/proc/${pintail!.pid}/stat`, 'utf8')
  const fields = stat.slice(stat.lastIndexOf(')') + 2).split(' ')
  // utime and stime are fields 14 and 15 of the whole line, 12 and 13 here.
  const cpuS = (Number(fields[11]) + Number(fields[12])) / CLOCK_TICKS
  const status = readFileSync(`/proc/${pintail!.pid}/status`, 'utf8')
  const rssKb = Number(/VmRSS:\s+(\d+)/.exec(status)?.[1] ?? 0)
  return { at: Date.now(), cpuS, rssMb: rssKb / 1024 }
}

/// Process samples every 100ms while running, for CPU and peak memory.
let samples: Array<{ at: number; cpuS: number; rssMb: number }> = []
const sampleTimer = setInterval(() => {
  if (!pintail?.pid) return
  try {
    samples.push(processSample())
    if (samples.length > 200_000) samples = samples.slice(-100_000)
  } catch {}
}, 100)

function window(from: number, to: number) {
  const inside = samples.filter((s) => s.at >= from && s.at <= to)
  if (inside.length < 2) return { cpuS: 0, peakRssMb: 0, startRssMb: 0 }
  return {
    cpuS: inside[inside.length - 1]!.cpuS - inside[0]!.cpuS,
    peakRssMb: Math.max(...inside.map((s) => s.rssMb)),
    startRssMb: inside[0]!.rssMb,
  }
}

/// Waits until the server has used under 2% of a core for five seconds:
/// background merges after a copy run for minutes and would be billed to
/// whatever is measured next.
async function waitIdle(limitMs = 600_000) {
  const deadline = Date.now() + limitMs
  for (;;) {
    const a = processSample()
    await Bun.sleep(5_000)
    const b = processSample()
    const rate = (b.cpuS - a.cpuS) / ((b.at - a.at) / 1000)
    if (rate < 0.02) return rate
    if (Date.now() > deadline) {
      log(`server never went idle (last ${(rate * 100).toFixed(1)}% of a core); measuring anyway`)
      return rate
    }
  }
}

// ---------------------------------------------------------------------------
// Source and mirror plumbing

async function sql(statement: string, params: unknown[] = []) {
  return root!.query(statement, params)
}

async function connect(database: string) {
  return mysql.createConnection({
    host: mysqlHost, port: mysqlPort, user: 'root', password: 'pintail-root', database,
    supportBigNumbers: true, bigNumberStrings: true, dateStrings: true,
  })
}

interface Mirror {
  id: string
  schema: string
  wire: mysql.Connection
  /// What a keyless table the stream cannot follow gets: kept quarantined
  /// for an operator (the default) or recopied automatically.
  keylessPolicy: string
}

async function mirror(schema: string, intervalS: number, keylessPolicy = 'quarantine'): Promise<Mirror> {
  const created = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: {
      name: schema,
      dsn: `mysql://root:pintail-root@${dsnHost(mysqlHost)}:${mysqlPort}/${schema}`,
      mode: 'cdc',
      reconcile_interval_seconds: intervalS,
      keyless_policy: keylessPolicy,
    },
  })
  // The snapshot copies what the probe found.
  await api(`/api/databases/${created.id}/probe`)
  await api(`/api/databases/${created.id}/snapshot`, { method: 'POST', body: { force: false } })
  const deadline = Date.now() + 3_600_000
  for (;;) {
    const status = await api<{ state: string }>(`/api/databases/${created.id}/snapshot/status`)
    if (status.state === 'streaming') break
    if (status.state === 'error' || Date.now() > deadline) throw new Error(`${schema} never reached streaming: ${JSON.stringify(status).slice(0, 800)}`)
    await Bun.sleep(1_000)
  }
  const key = await api<{ secret: string }>(`/api/databases/${created.id}/api-keys`, {
    method: 'POST',
    body: { name: 'fk-drift', scopes: ['query', 'read'] },
  })
  const wire = await mysql.createConnection({
    host: '127.0.0.1', port: wirePort, user: schema, password: key.secret, database: schema,
    supportBigNumbers: true, bigNumberStrings: true, dateStrings: true,
  })
  return { id: created.id, schema, wire, keylessPolicy }
}

async function setInterval_(m: Mirror, seconds: number) {
  await api(`/api/databases/${m.id}`, { method: 'PUT', body: { name: m.schema, mode: 'cdc', reconcile_interval_seconds: seconds, keyless_policy: m.keylessPolicy } })
}

/// Writes a marker on the source and waits for the mirror to show it:
/// change capture applies in commit order, so everything before it is in.
let markerSeq = 0
async function catchUp(m: Mirror, source: mysql.Connection, limitMs = 1_800_000) {
  markerSeq += 1
  await source.query('INSERT INTO drift_marker VALUES (?)', [markerSeq])
  const started = Date.now()
  for (;;) {
    try {
      const [rows] = await m.wire.query({ sql: 'SELECT MAX(id) FROM drift_marker', rowsAsArray: true })
      if (Number((rows as unknown[][])[0]?.[0] ?? 0) >= markerSeq) return Date.now() - started
    } catch {}
    if (Date.now() - started > limitMs) throw new Error(`mirror did not reach marker ${markerSeq} in ${limitMs}ms`)
    await Bun.sleep(50)
  }
}

/// The compared projection of each child table, keyed by its primary key.
const CHILDREN: Record<string, { sql: string; keyColumns: number }> = {
  catalog_item: { sql: 'SELECT id, vendor_id, price_cents FROM catalog_item', keyColumns: 1 },
  item_tag: { sql: 'SELECT item_id, tag, weight FROM item_tag', keyColumns: 2 },
  stock_slot: { sql: 'SELECT id, region, code, qty FROM stock_slot', keyColumns: 1 },
}

/// Children only the checks phase creates: an `ON UPDATE CASCADE` into a
/// child's primary key, and a cascade-affected child without any key
/// (compared as a multiset: keyColumns 0).
const CHECK_CHILDREN: Record<string, { sql: string; keyColumns: number }> = {
  ...CHILDREN,
  slot_lot: { sql: 'SELECT region, code, lot, qty FROM slot_lot', keyColumns: 3 },
  item_note: { sql: 'SELECT item_id, body FROM item_note', keyColumns: 0 },
}
const CHECK_SCHEMA = [
  'CREATE TABLE slot_lot (region CHAR(2) NOT NULL, code INT NOT NULL, lot INT NOT NULL, qty INT NOT NULL, PRIMARY KEY (region, code, lot), ' +
    'FOREIGN KEY (region, code) REFERENCES depot (region, code) ON DELETE CASCADE ON UPDATE CASCADE)',
  'CREATE TABLE item_note (item_id BIGINT NOT NULL, body VARCHAR(16) NOT NULL, FOREIGN KEY (item_id) REFERENCES catalog_item (id) ON DELETE CASCADE)',
]

async function dump(connection: mysql.Connection, table: string): Promise<Map<string, string>> {
  const spec = CHECK_CHILDREN[table]!
  const [rows] = await connection.query({ sql: spec.sql, rowsAsArray: true })
  const k = spec.keyColumns
  const out = new Map<string, string>()
  for (const row of rows as unknown[][]) {
    const text = row.map((v) => (v === null ? 'NULL' : String(v)))
    if (k === 0) {
      // No key: each distinct row is a key, numbered by occurrence.
      const whole = text.join('|')
      let n = 0
      while (out.has(`${whole}#${n}`)) n += 1
      out.set(`${whole}#${n}`, '')
    } else {
      out.set(text.slice(0, k).join('|'), text.slice(k).join('|'))
    }
  }
  return out
}

interface Diff {
  /// Rows the mirror holds that the source no longer has.
  ahead: number
  /// Rows the source has that the mirror does not hold yet.
  behind: number
  /// Same key, different values.
  differing: number
  /// Keys in any of the three, for lifetime tracking.
  keys: string[]
}
function diff(source: Map<string, string>, replica: Map<string, string>): Diff {
  let ahead = 0
  let behind = 0
  let differing = 0
  const keys: string[] = []
  for (const [key, value] of replica) {
    const want = source.get(key)
    if (want === undefined) {
      ahead += 1
      keys.push(`-${key}`)
    } else if (want !== value) {
      differing += 1
      keys.push(`~${key}`)
    }
  }
  for (const key of source.keys()) {
    if (!replica.has(key)) {
      behind += 1
      keys.push(`+${key}`)
    }
  }
  return { ahead, behind, differing, keys }
}

// ---------------------------------------------------------------------------
// The invented schema

const SCHEMA = [
  'CREATE TABLE vendor (id INT PRIMARY KEY, name VARCHAR(32) NOT NULL)',
  'CREATE TABLE catalog_item (id BIGINT PRIMARY KEY, vendor_id INT NOT NULL, price_cents INT NOT NULL, label VARCHAR(48) NOT NULL, ' +
    'KEY (vendor_id), FOREIGN KEY (vendor_id) REFERENCES vendor (id) ON DELETE CASCADE ON UPDATE CASCADE)',
  'CREATE TABLE item_tag (item_id BIGINT NOT NULL, tag SMALLINT NOT NULL, weight INT NOT NULL, PRIMARY KEY (item_id, tag), ' +
    'FOREIGN KEY (item_id) REFERENCES catalog_item (id) ON DELETE CASCADE)',
  'CREATE TABLE depot (region CHAR(2) NOT NULL, code INT NOT NULL, PRIMARY KEY (region, code))',
  'CREATE TABLE stock_slot (id BIGINT PRIMARY KEY, region CHAR(2) NULL, code INT NULL, qty INT NOT NULL, KEY (region, code), ' +
    'FOREIGN KEY (region, code) REFERENCES depot (region, code) ON DELETE SET NULL ON UPDATE CASCADE)',
  'CREATE TABLE drift_marker (id INT PRIMARY KEY)',
  'CREATE TABLE seed (n INT PRIMARY KEY)',
]
const REGIONS = ['na', 'eu', 'ap', 'sa']

async function createSchema(schema: string) {
  await sql(`DROP DATABASE IF EXISTS ${schema}`)
  await sql(`CREATE DATABASE ${schema}`)
  for (const statement of SCHEMA) await sql(statement.replace(/^CREATE TABLE /, `CREATE TABLE ${schema}.`).replace(/REFERENCES (\w+)/, `REFERENCES ${schema}.$1`))
  await sql(`INSERT INTO ${schema}.seed VALUES ${Array.from({ length: 100 }, (_, n) => `(${n})`).join(',')}`)
}

/// `items` catalog items over items/perVendor vendors, a tenth of the items
/// tagged with `tags` tags each, and `depots` depots with `slotsPerDepot`
/// slots. Inserted in blocks of a million rows so no statement grows with
/// the table.
async function seed(schema: string, items: number, perVendor: number, tags: number, depots: number, slotsPerDepot: number) {
  const vendors = Math.ceil(items / perVendor)
  await sql(`INSERT INTO ${schema}.vendor SELECT a.n * 10000 + b.n * 100 + c.n, CONCAT('v', a.n, b.n, c.n) FROM ${schema}.seed a, ${schema}.seed b, ${schema}.seed c WHERE a.n * 10000 + b.n * 100 + c.n < ${vendors}`)
  const blocks = Math.ceil(items / 10_000)
  for (let from = 0; from < blocks; from += 100) {
    const to = Math.min(from + 100, blocks)
    await sql(
      `INSERT INTO ${schema}.catalog_item SELECT i, i DIV ${perVendor}, (i * 37) % 100000, CONCAT('item-', i) FROM (` +
        `SELECT a.n * 10000 + b.n * 100 + c.n AS i FROM (SELECT x.n * 100 + y.n AS n FROM ${schema}.seed x, ${schema}.seed y WHERE x.n * 100 + y.n >= ${from} AND x.n * 100 + y.n < ${to}) a, ` +
        `${schema}.seed b, ${schema}.seed c) g WHERE i < ${items}`,
    )
  }
  if (tags > 0) {
    for (let tag = 0; tag < tags; tag += 1) {
      await sql(`INSERT INTO ${schema}.item_tag SELECT id, ${tag}, (id + ${tag}) % 97 FROM ${schema}.catalog_item WHERE id % 10 = 0`)
    }
  }
  for (let d = 0; d < depots; d += 1) {
    await sql(`INSERT INTO ${schema}.depot VALUES (?, ?)`, [REGIONS[d % 4], d])
  }
  if (depots > 0) {
    await sql(
      `INSERT INTO ${schema}.stock_slot SELECT d.code * 1000 + s.n, d.region, d.code, (d.code + s.n) % 50 FROM ${schema}.depot d, ${schema}.seed s WHERE s.n < ${slotsPerDepot}`,
    )
  }
  return { vendors }
}

// ---------------------------------------------------------------------------
// Writers

interface Live {
  vendors: number[]
  nextVendor: number
  nextItem: number
  depots: Array<[string, number]>
  nextDepot: number
  nextSlot: number
}

type Op = 'vendor+items' | 'vendor-delete' | 'vendor-rekey' | 'item-update' | 'depot+slots' | 'depot-delete' | 'depot-rekey' | 'slot-insert'
const MIX: Array<[Op, number]> = [
  ['vendor+items', 28], ['vendor-delete', 15], ['vendor-rekey', 5], ['item-update', 20],
  ['depot+slots', 10], ['depot-delete', 8], ['depot-rekey', 7], ['slot-insert', 7],
]
const MIX_TOTAL = MIX.reduce((n, [, w]) => n + w, 0)
function pickOp(): Op {
  let r = Math.random() * MIX_TOTAL
  for (const [op, weight] of MIX) {
    r -= weight
    if (r < 0) return op
  }
  return 'item-update'
}
function takeRandom<T>(list: T[]): T | undefined {
  if (!list.length) return undefined
  const index = Math.floor(Math.random() * list.length)
  const value = list[index]!
  list[index] = list[list.length - 1]!
  list.pop()
  return value
}
function peekRandom<T>(list: T[]): T | undefined {
  return list.length ? list[Math.floor(Math.random() * list.length)] : undefined
}

async function runOp(c: mysql.Connection, live: Live, op: Op) {
  switch (op) {
    case 'vendor+items': {
      const vendor = live.nextVendor++
      const count = 1 + Math.floor(Math.random() * 20)
      const first = live.nextItem
      live.nextItem += count
      await c.query('START TRANSACTION')
      await c.query('INSERT INTO vendor VALUES (?, ?)', [vendor, `v${vendor}`])
      const items = Array.from({ length: count }, (_, i) => [first + i, vendor, Math.floor(Math.random() * 100000), `item-${first + i}`])
      await c.query('INSERT INTO catalog_item VALUES ?', [items])
      const tags = items.flatMap(([id]) => Array.from({ length: Math.floor(Math.random() * 4) }, (_, t) => [id, t, Math.floor(Math.random() * 97)]))
      if (tags.length) await c.query('INSERT INTO item_tag VALUES ?', [tags])
      await c.query('COMMIT')
      live.vendors.push(vendor)
      return
    }
    case 'vendor-delete': {
      const vendor = takeRandom(live.vendors)
      if (vendor !== undefined) await c.query('DELETE FROM vendor WHERE id = ?', [vendor])
      return
    }
    case 'vendor-rekey': {
      const vendor = takeRandom(live.vendors)
      if (vendor === undefined) return
      const next = live.nextVendor++
      await c.query('UPDATE vendor SET id = ? WHERE id = ?', [next, vendor])
      live.vendors.push(next)
      return
    }
    case 'item-update': {
      // Seeded items and the ones added since, equally likely.
      const ids = Array.from({ length: 5 }, () =>
        Math.random() < 0.5 ? Math.floor(Math.random() * 100_000) : 10_000_000 + Math.floor(Math.random() * Math.max(1, live.nextItem - 10_000_000)))
      await c.query('UPDATE catalog_item SET price_cents = price_cents + 1 WHERE id IN (?)', [ids])
      return
    }
    case 'depot+slots': {
      const depot: [string, number] = [REGIONS[live.nextDepot % 4]!, live.nextDepot++]
      const count = 1 + Math.floor(Math.random() * 10)
      await c.query('START TRANSACTION')
      await c.query('INSERT INTO depot VALUES (?, ?)', depot)
      await c.query('INSERT INTO stock_slot VALUES ?', [Array.from({ length: count }, () => [live.nextSlot++, depot[0], depot[1], Math.floor(Math.random() * 50)])])
      await c.query('COMMIT')
      live.depots.push(depot)
      return
    }
    case 'depot-delete': {
      const depot = takeRandom(live.depots)
      if (depot) await c.query('DELETE FROM depot WHERE region = ? AND code = ?', depot)
      return
    }
    case 'depot-rekey': {
      const depot = takeRandom(live.depots)
      if (!depot) return
      const next: [string, number] = [depot[0], live.nextDepot++]
      await c.query('UPDATE depot SET code = ? WHERE region = ? AND code = ?', [next[1], depot[0], depot[1]])
      live.depots.push(next)
      return
    }
    case 'slot-insert': {
      const depot = peekRandom(live.depots)
      if (depot) await c.query('INSERT INTO stock_slot VALUES (?, ?, ?, ?)', [live.nextSlot++, depot[0], depot[1], 1])
      return
    }
  }
}

class Writers {
  private paused = false
  private stopped = false
  private inFlight = 0
  private resume: (() => void) | undefined
  private gate: Promise<void> = Promise.resolve()
  readonly ops: Record<string, number> = {}
  errors = 0
  private loops: Promise<void>[] = []

  constructor(private connections: mysql.Connection[], private live: Live) {}

  start() {
    this.loops = this.connections.map(async (c) => {
      while (!this.stopped) {
        await this.gate
        if (this.stopped) break
        this.inFlight += 1
        const op = pickOp()
        try {
          await runOp(c, this.live, op)
          this.ops[op] = (this.ops[op] ?? 0) + 1
        } catch {
          // Deadlocks and lock waits between cascades are expected; the
          // source rolled the statement back, so there is nothing to follow.
          this.errors += 1
          await c.query('ROLLBACK').catch(() => {})
        } finally {
          this.inFlight -= 1
        }
        await Bun.sleep(WRITER_PAUSE_MS * (0.5 + Math.random()))
      }
    })
  }

  async pause() {
    this.paused = true
    this.gate = new Promise((resolveGate) => (this.resume = resolveGate))
    while (this.inFlight > 0) await Bun.sleep(5)
  }

  unpause() {
    if (!this.paused) return
    this.paused = false
    this.resume?.()
  }

  async stop() {
    this.stopped = true
    this.unpause()
    await Promise.all(this.loops)
  }
}

// ---------------------------------------------------------------------------
// Phase: drift under concurrent writers, per reconcile interval

async function phaseDrift() {
  const schema = 'fk_drift'
  await createSchema(schema)
  // 100k catalog items over 2,000 vendors, 30k tags, 20k slots over 500 depots.
  const { vendors } = await seed(schema, 100_000, 50, 3, 500, 40)
  const m = await mirror(schema, IDLE_INTERVAL_S)
  const source = await connect(schema)
  log(`drift: mirror ${m.id} streaming`)
  const live: Live = {
    vendors: Array.from({ length: vendors }, (_, i) => i),
    nextVendor: 1_000_000,
    nextItem: 10_000_000,
    depots: Array.from({ length: 500 }, (_, d) => [REGIONS[d % 4]!, d] as [string, number]),
    nextDepot: 1_000_000,
    nextSlot: 100_000_000,
  }
  const out: Record<string, unknown>[] = []
  for (const interval of INTERVALS) {
    // Start every interval converged: one pass, then the interval under test,
    // so its first scheduled pass falls one interval after this one.
    await setInterval_(m, 1)
    const settleFrom = Date.now()
    await catchUp(m, source)
    while (!passes(m.id, settleFrom).some((p) => p.at > settleFrom)) await Bun.sleep(200)
    await setInterval_(m, interval)
    const runSeconds = Number(process.env.DRIFT_SECONDS ?? Math.max(180, Math.round(interval * 1.5 + 60)))
    const connections = await Promise.all(Array.from({ length: WRITERS }, () => connect(schema)))
    const writers = new Writers(connections, live)
    const started = Date.now()
    const phaseStart = started
    writers.start()
    const series: Array<Record<string, number>> = []
    const firstSeen = new Map<string, number>()
    const lifetimes: number[] = []
    let writingMs = 0
    let lastResume = Date.now()
    while (Date.now() - started < runSeconds * 1000) {
      await Bun.sleep(SAMPLE_EVERY_S * 1000)
      const t = (Date.now() - started) / 1000
      // As the two sides stand: source first, mirror right after.
      const live_: Record<string, Diff> = {}
      for (const table of Object.keys(CHILDREN)) {
        const s = await dump(source, table)
        const r = await dump(m.wire, table)
        live_[table] = diff(s, r)
      }
      await writers.pause()
      writingMs += Date.now() - lastResume
      const lagMs = await catchUp(m, source)
      const synced: Record<string, Diff> = {}
      const stale = new Set<string>()
      for (const table of Object.keys(CHILDREN)) {
        const s = await dump(source, table)
        const r = await dump(m.wire, table)
        synced[table] = diff(s, r)
        for (const key of synced[table]!.keys) stale.add(`${table}:${key}`)
      }
      writers.unpause()
      lastResume = Date.now()
      const now = Date.now()
      for (const key of stale) if (!firstSeen.has(key)) firstSeen.set(key, now)
      for (const [key, seen] of firstSeen) {
        if (!stale.has(key)) {
          lifetimes.push((now - seen) / 1000)
          firstSeen.delete(key)
        }
      }
      const row: Record<string, number> = { t, lagMs }
      for (const table of Object.keys(CHILDREN)) {
        row[`${table}.live.ahead`] = live_[table]!.ahead
        row[`${table}.live.behind`] = live_[table]!.behind
        row[`${table}.live.differing`] = live_[table]!.differing
        row[`${table}.synced.ahead`] = synced[table]!.ahead
        row[`${table}.synced.behind`] = synced[table]!.behind
        row[`${table}.synced.differing`] = synced[table]!.differing
      }
      row.stale = stale.size
      series.push(row)
      log(`drift ${interval}s t=${t.toFixed(0)}: stale ${stale.size} (item ahead ${synced.catalog_item!.ahead}, tag ahead ${synced.item_tag!.ahead}, slot differing ${synced.stock_slot!.differing}); live item ahead ${live_.catalog_item!.ahead} behind ${live_.catalog_item!.behind}; catch-up ${lagMs}ms`)
    }
    await writers.stop()
    for (const c of connections) await c.end().catch(() => {})
    const ended = Date.now()
    const censored = [...firstSeen.values()].map((seen) => (ended - seen) / 1000)
    const runPasses = passes(m.id, phaseStart)
    const staleSeries = series.map((r) => r.stale!)
    const sorted = [...lifetimes].sort((a, b) => a - b)
    const pct = (p: number) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(p * sorted.length))]! : 0)
    const sumOf = (suffix: string) => series.reduce((n, r) => n + Object.keys(CHILDREN).reduce((k, t) => k + r[`${t}.${suffix}`]!, 0), 0)
    const summary = {
      interval,
      seconds: (ended - started) / 1000,
      writingSeconds: writingMs / 1000,
      ops: writers.ops,
      writerErrors: writers.errors,
      samples: series.length,
      staleMean: staleSeries.reduce((a, b) => a + b, 0) / Math.max(1, staleSeries.length),
      staleMax: Math.max(0, ...staleSeries),
      staleRowSeconds: staleSeries.reduce((a, b) => a + b, 0) * SAMPLE_EVERY_S,
      lifetimes: { n: lifetimes.length, p50: pct(0.5), p90: pct(0.9), p99: pct(0.99), max: sorted[sorted.length - 1] ?? 0 },
      stillStaleAtEnd: censored.length,
      stillStaleMaxAge: Math.max(0, ...censored),
      liveAheadSum: sumOf('live.ahead'),
      liveBehindSum: sumOf('live.behind'),
      liveDifferingSum: sumOf('live.differing'),
      syncedBehindSum: sumOf('synced.behind'),
      passes: runPasses,
      series,
    }
    out.push(summary)
    log(
      `drift ${interval}s done: stale mean ${summary.staleMean.toFixed(1)} max ${summary.staleMax}; lifetime p50 ${summary.lifetimes.p50}s p90 ${summary.lifetimes.p90}s max ${summary.lifetimes.max}s; ` +
        `${runPasses.length} passes, ${summary.stillStaleAtEnd} still stale; synced behind ${summary.syncedBehindSum}`,
    )
  }
  await source.end()
  await m.wire.end()
  // Its passes would otherwise be billed to the cost phase.
  await api(`/api/databases/${m.id}`, { method: 'DELETE' }).catch(() => {})
  results.drift = out
}

// ---------------------------------------------------------------------------
// Phase: what one pass costs, by child-table size

async function phaseCost() {
  const out: Record<string, unknown>[] = []
  for (const size of SIZES) {
    const schema = `fk_cost_${size}`
    await createSchema(schema)
    const seeding = Date.now()
    // A hundred items per vendor; a tenth of the items carry DRIFT_COST_TAGS
    // tags (default one), whose pass looks every row up in the item table.
    const { vendors } = await seed(schema, size, 100, Number(process.env.DRIFT_COST_TAGS ?? 1), 0, 0)
    log(`cost ${size}: seeded in ${((Date.now() - seeding) / 1000).toFixed(0)}s`)
    const copying = Date.now()
    const m = await mirror(schema, IDLE_INTERVAL_S)
    const source = await connect(schema)
    await catchUp(m, source, 3_600_000)
    // The first pass of a new table runs at once (it has never been
    // reconciled); wait for it so it is not counted below.
    const firstDeadline = Date.now() + 3_600_000
    while (passes(m.id, copying).length === 0 && Date.now() < firstDeadline) await Bun.sleep(1_000)
    log(`cost ${size}: copied and first pass done in ${((Date.now() - copying) / 1000).toFixed(0)}s`)
    const idleRate = await waitIdle()
    const measured: Record<string, unknown>[] = []
    // The first pass a process runs on a table also sweeps it for children
    // whose parent the replica lacks; later passes read only what the stream
    // queued.
    const first = passes(m.id, copying)[0]
    if (first) {
      measured.push({ label: 'first pass (sweep)', elapsedMs: first.elapsedMs, applyMs: first.applyMs, rowsCompared: first.rowsCompared, tombstoned: first.tombstoned })
      log(`cost ${size} (first pass, sweep): ${first.elapsedMs}ms work, ${first.applyMs ?? first.elapsedMs}ms in the cycle, ${first.rowsCompared} rows compared`)
    }
    // Pass 1 repairs a cascade over 1% of the vendors (1% of the items and
    // their tags); pass 2 finds nothing to repair.
    const doomed = Math.max(1, Math.floor(vendors / 100))
    await source.query(`DELETE FROM vendor WHERE id % 100 = 7 AND id < ${doomed * 100}`)
    await catchUp(m, source)
    for (const label of ['repairs 1%', 'nothing to repair']) {
      const before = processSample()
      const from = Date.now()
      await setInterval_(m, 1)
      const deadline = Date.now() + 3_600_000
      let pass: PassLine | undefined
      while (!(pass = passes(m.id, from)[0]) && Date.now() < deadline) await Bun.sleep(100)
      await setInterval_(m, IDLE_INTERVAL_S)
      if (!pass) throw new Error(`cost ${size}: no pass within the hour`)
      const to = pass.at
      const w = window(from, to)
      const perTable = tablePasses(m.id, from).filter((p) => p.at === pass!.at)
      const cpuS = Math.max(0, w.cpuS - idleRate * ((to - from) / 1000))
      measured.push({
        label,
        elapsedMs: pass.elapsedMs,
        applyMs: pass.applyMs,
        cpuS: Number(cpuS.toFixed(2)),
        rssBeforeMb: Number(before.rssMb.toFixed(0)),
        peakRssMb: Number(w.peakRssMb.toFixed(0)),
        rssGrowthMb: Number((w.peakRssMb - before.rssMb).toFixed(0)),
        rowsCompared: pass.rowsCompared,
        tombstoned: pass.tombstoned,
        refreshed: pass.refreshed,
        perTable,
      })
      log(`cost ${size} (${label}): ${pass.elapsedMs}ms (${pass.applyMs ?? pass.elapsedMs}ms in the cycle), ${cpuS.toFixed(2)} CPU-s, peak RSS +${(w.peakRssMb - before.rssMb).toFixed(0)}MB, ${pass.rowsCompared} rows compared, ${pass.tombstoned} tombstoned`)
      await waitIdle()
    }
    const [[sourceItems]] = (await source.query({ sql: 'SELECT COUNT(*) FROM catalog_item', rowsAsArray: true })) as unknown as [[[string]]]
    const [[mirrorItems]] = (await m.wire.query({ sql: 'SELECT COUNT(*) FROM catalog_item', rowsAsArray: true })) as unknown as [[[string]]]
    out.push({ size, vendors, measured, converged: String(sourceItems[0]) === String(mirrorItems[0]), sourceItems: sourceItems[0], mirrorItems: mirrorItems[0] })
    await source.end()
    await m.wire.end()
    await api(`/api/databases/${m.id}`, { method: 'DELETE' }).catch(() => {})
    await sql(`DROP DATABASE IF EXISTS ${schema}`)
    await sql('PURGE BINARY LOGS BEFORE NOW()').catch(() => {})
  }
  results.cost = out
}

// ---------------------------------------------------------------------------
// Phase: shapes the repair must get right

async function phaseChecks() {
  const schema = 'fk_checks'
  await createSchema(schema)
  await seed(schema, 2_000, 20, 2, 40, 10)
  for (const statement of CHECK_SCHEMA) {
    await sql(statement.replace(/^CREATE TABLE /, `CREATE TABLE ${schema}.`).replace(/REFERENCES (\w+)/, `REFERENCES ${schema}.$1`))
  }
  await sql(`INSERT INTO ${schema}.slot_lot SELECT d.region, d.code, s.n, s.n + 1 FROM ${schema}.depot d, ${schema}.seed s WHERE s.n < 3`)
  await sql(`INSERT INTO ${schema}.item_note SELECT id, CONCAT('n', id % 7) FROM ${schema}.catalog_item WHERE id % 5 = 0`)
  const m = await mirror(schema, IDLE_INTERVAL_S, 'auto_resync')
  const source = await connect(schema)
  const checks: Record<string, unknown>[] = []

  const forcePass = async () => {
    await catchUp(m, source)
    const from = Date.now()
    await setInterval_(m, 1)
    while (passes(m.id, from).length === 0) await Bun.sleep(100)
    await setInterval_(m, IDLE_INTERVAL_S)
    return tablePasses(m.id, from)
  }
  /// Rows an earlier check left wrong for good; each check counts only the
  /// rows it made differ.
  let residual = new Set<string>()
  let lastKeys = new Set<string>()
  const compare = async () => {
    const out: Record<string, { ahead: number; behind: number; differing: number }> = {}
    lastKeys = new Set()
    for (const table of Object.keys(CHECK_CHILDREN)) {
      const d = diff(await dump(source, table), await dump(m.wire, table))
      const fresh = d.keys.filter((key) => !residual.has(`${table}:${key}`))
      for (const key of d.keys) lastKeys.add(`${table}:${key}`)
      out[table] = {
        ahead: fresh.filter((key) => key.startsWith('-')).length,
        behind: fresh.filter((key) => key.startsWith('+')).length,
        differing: fresh.filter((key) => key.startsWith('~')).length,
      }
    }
    return out
  }
  const clean = (d: Record<string, { ahead: number; behind: number; differing: number }>) =>
    Object.values(d).every((x) => x.ahead + x.behind + x.differing === 0)
  /// A keyless child is repaired by a recopy, which the supervisor starts
  /// after the pass: give it a minute to land before comparing.
  const settled = async () => {
    const deadline = Date.now() + 60_000
    for (;;) {
      let out: Awaited<ReturnType<typeof compare>>
      try {
        out = await compare()
      } catch (error) {
        // The table being recopied may refuse reads until the copy lands.
        if (Date.now() > deadline) throw error
        await Bun.sleep(1_000)
        continue
      }
      if (clean(out) || Date.now() > deadline) return out
      const keyed = Object.entries(out).every(([table, x]) => table === 'item_note' || x.ahead + x.behind + x.differing === 0)
      if (!keyed) return out
      await Bun.sleep(1_000)
    }
  }
  const check = async (name: string, expectation: string, act: () => Promise<void>) => {
    await act()
    await catchUp(m, source)
    const beforePass = await compare()
    const first = await forcePass()
    const afterPass = await settled()
    const second = await forcePass()
    const afterSecond = await compare()
    residual = lastKeys
    const result = {
      name, expectation,
      staleBeforePass: !clean(beforePass),
      exactAfterPass: clean(afterPass),
      exactAfterSecondPass: clean(afterSecond),
      beforePass, afterPass,
      pass: first, secondPass: second,
    }
    checks.push(result)
    log(`check ${name}: stale before pass ${result.staleBeforePass}, exact after pass ${result.exactAfterPass}, after a second ${result.exactAfterSecondPass} - ${JSON.stringify(afterPass)}`)
  }
  await forcePass()

  await check('fk-checks-off delete', 'source keeps the orphans; the mirror must keep them too', async () => {
    await source.query('SET SESSION foreign_key_checks = 0')
    await source.query('DELETE FROM vendor WHERE id IN (3, 4)')
    await source.query("DELETE FROM depot WHERE region = 'na' AND code = 0")
    await source.query('SET SESSION foreign_key_checks = 1')
  })
  await check('orphans stay through later passes', 'the orphans are verified again by each pass and kept', async () => {})
  await check('plain cascade', 'a pass removes the cascaded rows at both levels and nulls the SET NULL rows', async () => {
    await source.query('DELETE FROM vendor WHERE id IN (10, 11)')
    await source.query("DELETE FROM depot WHERE region = 'eu' AND code = 1")
  })
  await check('parent key reused', 'deleted then inserted again before a pass: the parent exists, so the pass does not look at its stale children', async () => {
    await source.query('DELETE FROM vendor WHERE id = 20')
    await source.query("INSERT INTO vendor VALUES (20, 'again')")
  })
  await check('REPLACE on a parent', 'REPLACE deletes the parent row and cascades before inserting it again', async () => {
    await source.query("REPLACE INTO vendor VALUES (30, 'replaced')")
  })
  await check('composite key moved then reused', 'ON UPDATE CASCADE moves the slots; a new depot takes the old key', async () => {
    await source.query("UPDATE depot SET code = 9001 WHERE region = 'ap' AND code = 2")
    await source.query("INSERT INTO depot VALUES ('ap', 2)")
  })
  await check('composite key moved', 'ON UPDATE CASCADE moves the slots to the new key', async () => {
    await source.query("UPDATE depot SET code = 9002 WHERE region = 'sa' AND code = 3")
  })
  await check('ON UPDATE CASCADE into a child key', 'the lots move to the new depot key, which is part of their own primary key', async () => {
    await source.query("UPDATE depot SET code = 9003 WHERE region = 'na' AND code = 4")
  })
  await check('child key moved, then the old key reused', 'the lots move away and a new depot takes the old key without lots', async () => {
    await source.query("UPDATE depot SET code = 9004 WHERE region = 'eu' AND code = 5")
    await source.query("INSERT INTO depot VALUES ('eu', 5)")
  })
  await check('keyless child beside keyed ones', 'a cascade into a child without a key is recopied; its keyed siblings are repaired in the same pass', async () => {
    await source.query('DELETE FROM vendor WHERE id IN (40, 41)')
  })
  await check('REPLACE through two levels', 'REPLACE on a vendor cascades to its items and their tags and notes', async () => {
    await source.query("REPLACE INTO vendor VALUES (50, 'replaced')")
  })
  await source.end()
  await m.wire.end()
  await api(`/api/databases/${m.id}`, { method: 'DELETE' }).catch(() => {})
  results.checks = checks
}

// ---------------------------------------------------------------------------

async function main() {
  const binary = resolve(process.env.PINTAIL_BINARY ?? join(repository, 'target', 'release', 'pintail'))
  mysqlHost = await dockerHost()
  log(`starting MySQL ${mysqlName}`)
  await docker(
    'run', '--detach', '--name', mysqlName, '--publish', '0:3306',
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', 'mysql:8.4',
    '--server-id=977', '--log-bin=mysql-bin', '--binlog-format=ROW', '--binlog-row-image=FULL',
    '--gtid-mode=ON', '--enforce-gtid-consistency=ON', '--default-time-zone=+00:00',
    '--innodb-buffer-pool-size=3G', '--innodb-flush-log-at-trx-commit=2', '--sync-binlog=0',
    '--innodb-redo-log-capacity=2G', '--binlog-expire-logs-seconds=3600', '--max-allowed-packet=256M',
  )
  mysqlPort = await publishedPort(mysqlName, 3306)
  root = await waitForMysql(mysqlHost, mysqlPort)
  dataDir = mkdtempSync(join(process.env.DRIFT_DATA_ROOT ?? tmpdir(), 'pintail-fkdrift-'))
  const httpPort = await freePort()
  wirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${httpPort}`
  // The production supervisor cadence (no override): the interval is only
  // checked once per cycle, so the cadence is part of what is measured.
  pintail = Bun.spawn([binary, '--data-dir', dataDir, '--http-bind', `127.0.0.1:${httpPort}`, '--wire-bind', `127.0.0.1:${wirePort}`], {
    stdout: Bun.file(logPath),
    stderr: Bun.file(logPath),
    env: { ...process.env, TZ: 'UTC', PINTAIL_LOG: 'info' },
  })
  for (let attempt = 0; ; attempt += 1) {
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) break
    } catch {}
    if (attempt > 240) throw new Error('pintail did not become healthy')
    await Bun.sleep(500)
  }
  token = (await api<{ token: string }>('/api/auth/setup', { method: 'POST', auth: false, body: { email: 'drift@pintail.local', password: 'fk-drift-password' } })).token
  const cpu = readFileSync('/proc/cpuinfo', 'utf8').match(/model name\s*:\s*(.+)/)?.[1] ?? 'unknown'
  results.host = { cpu, cores: navigator.hardwareConcurrency, binary }
  log(`host ${cpu}, ${navigator.hardwareConcurrency} cores`)

  // Each phase's results reach disk as it ends: a later phase on the
  // largest sizes can outlast the time a run is given.
  if (PHASES.has('checks')) await phaseChecks().then(writeOut)
  if (PHASES.has('drift')) await phaseDrift().then(writeOut)
  if (PHASES.has('cost')) await phaseCost().then(writeOut)
}

function writeOut() {
  results.finishedAt = new Date().toISOString()
  writeFileSync(join(outDir, 'results.json'), JSON.stringify(results, null, 2) + '\n')
  writeFileSync(join(outDir, 'notes.log'), notes.join('\n') + '\n')
}

try {
  await main()
  writeOut()
  log(`FK-DRIFT-DONE results in ${outDir}`)
} catch (error) {
  writeOut()
  console.error(`[fk-drift] FK-DRIFT-FAIL ${error instanceof Error ? (error.stack ?? error.message) : String(error)}`)
  appendFileSync(join(outDir, 'notes.log'), `FAIL ${String(error)}\n`)
  process.exitCode = 1
} finally {
  clearInterval(logTimer)
  clearInterval(sampleTimer)
  await root?.end().catch(() => {})
  pintail?.kill()
  await docker('rm', '--force', '--volumes', mysqlName).catch(() => {})
  if (dataDir) rmSync(dataDir, { recursive: true, force: true })
}

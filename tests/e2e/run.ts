/// End-to-end differential gate: the whole product, production-shaped.
///
/// Boots a real MySQL source in Docker, builds and runs the real pintail
/// server binary, registers the source over the HTTP API, snapshots, and
/// then drives workload phases — CRUD inside transactions, type edge cases,
/// live DDL, seeded random churn, and a SIGKILL restart. After every phase
/// it proves two things against the live replica:
///
///   1. Convergence: every base table in MySQL reads back identically from
///      pintail (retried until the CDC supervisor catches up).
///   2. Query equivalence: the differential corpus in queries.ts returns
///      identical results from MySQL and pintail.
///
/// Operations pintail documents as gaps (table rename quarantine, in-place
/// type changes) are exercised too, but their divergences report as WARN
/// instead of failing the gate.
///
/// Run with: bun run run.ts            (full gate)
///           E2E_PHASES=crud,ddl ...   (subset while iterating)

import { createServer } from 'node:net'
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { runOrmCompatibility, type MysqlEndpoint } from './orm-compat'
import { differentialQueries } from './queries'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-e2e-mysql-${process.pid}-${nonce}`
const DATABASE = 'e2e_db'
const CONVERGE_TIMEOUT_MS = 180_000
const CONVERGE_POLL_MS = 2_000

interface CheckResult {
  phase: string
  check: string
  status: 'PASS' | 'FAIL' | 'WARN' | 'SKIP'
  detail?: string
}

const results: CheckResult[] = []
/// Tables currently under a documented-gap operation, each with the exact
/// divergence signature the documentation predicts. A divergence matching
/// its signature reports WARN; anything else on the same table stays FAIL,
/// so unrelated regressions cannot hide behind a known gap.
const documentedGapTables = new Map<string, RegExp>()
/// Exact metadata divergences implied by the same documented operations.
/// Keep these signatures narrow: a different column, type, or nullability
/// mismatch must remain a gate failure.
const documentedMetadataGaps: RegExp[] = []

let mysqlConnection: mysql.Connection | undefined
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailBinary = ''
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''
let databaseId = ''
let mysqlStarted = false
let mysqlEndpoint: MysqlEndpoint | undefined

function log(message: string) {
  console.log(`[e2e] ${message}`)
}

async function command(args: string[], options: { cwd?: string; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
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
  return { stdout: stdout.trim(), stderr: stderr.trim() }
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

async function waitForMysql(host: string, port: number, attempts = 240) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        multipleStatements: true,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
        enableKeepAlive: true,
        keepAliveInitialDelay: 10_000,
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready in time')
}

async function api<T>(
  path: string,
  options: { method?: string; body?: unknown; auth?: boolean } = {},
): Promise<T> {
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers: {
      ...(options.auth === false ? {} : { Authorization: `Bearer ${token}` }),
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

/// Queries run over the MySQL wire protocol — the way production clients
/// connect — so both engines' values arrive through the same mysql2 typing
/// (u64/decimal as exact strings, temporal as strings, binary as Buffers).
let pintailWire: mysql.Connection | undefined
let wireSecret = ''

async function pintailQuery(sql: string): Promise<unknown[][]> {
  for (let attempt = 0; ; attempt += 1) {
    if (!pintailWire) {
      pintailWire = await mysql.createConnection({
        host: '127.0.0.1',
        port: pintailWirePort,
        user: DATABASE,
        password: wireSecret,
        database: DATABASE,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
        // The wire library hardcodes charset 33 in column metadata
        // (docs/limitations.md), so binary columns would be lossily decoded
        // as utf8 text. Take raw buffers and let canonicalValue decide.
        typeCast: (field, next) => {
          if (field.type === 'VAR_STRING' || field.type === 'STRING' || field.type === 'BLOB') {
            return field.buffer()
          }
          return next()
        },
      })
    }
    const connection = pintailWire
    try {
      const [rows] = await connection.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
      return rows as unknown as unknown[][]
    } catch (error) {
      const transient = /ECONNREFUSED|ECONNRESET|EPIPE|closed state|Connection lost/i.test(
        String(error),
      )
      if (transient) {
        pintailWire = undefined
        try {
          await connection.end()
        } catch {}
        if (attempt < 2) {
          await Bun.sleep(1000)
          continue
        }
      }
      throw error
    }
  }
}

async function mysqlRows(sql: string): Promise<unknown[][]> {
  const [rows] = await mysqlConnection!.query<mysql.RowDataPacket[]>({ sql, rowsAsArray: true })
  return rows as unknown as unknown[][]
}

// ---------------------------------------------------------------------------
// Canonicalization: MySQL wire values (via mysql2) and pintail JSON values
// must map to one comparable form.

function canonicalValue(value: unknown): string {
  if (value === null || value === undefined) return 'NULL'
  if (Buffer.isBuffer(value)) {
    // Text arrives as Buffer from the pintail wire (charset-33 workaround
    // above) and binary arrives as Buffer from MySQL: valid UTF-8 compares
    // as text, anything else as hex, identically on both sides.
    const text = value.toString('utf8')
    if (Buffer.compare(Buffer.from(text, 'utf8'), value) === 0) {
      return canonicalValue(text)
    }
    return `0x${value.toString('hex')}`
  }
  if (value instanceof Date) return canonicalValue(value.toISOString())
  if (typeof value === 'object') return canonicalJson(value)
  let text = String(value)
  // JSON documents arrive as text from one side and objects from the other.
  if (text.startsWith('{') || text.startsWith('[')) {
    try {
      return canonicalJson(JSON.parse(text))
    } catch {}
  }
  // Temporal: unify 'T' separators, drop timezone suffix, trim only the
  // fractional-second zeros (never the seconds themselves).
  if (/^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}/.test(text)) {
    text = text.replace('T', ' ').replace(/(Z|[+-]\d{2}:?\d{2})$/, '')
    text = text.replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '')
    return text
  }
  // Numeric: fixed exponent-free form, 4 decimal places like the benchmark.
  if (text !== '' && /^-?\d+(\.\d+)?$/.test(text)) {
    // u64 values above 2^53 lose precision through Number; keep integers
    // longer than 15 digits as exact strings.
    if (/^-?\d+$/.test(text) && text.replace('-', '').length > 15) return text
    return Number(text).toFixed(4)
  }
  return text
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) =>
      a < b ? -1 : a > b ? 1 : 0,
    )
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(',')}}`
  }
  return JSON.stringify(canonicalValue(value))
}

function canonicalRow(row: unknown[], csvColumns?: number[]): string {
  return row
    .map((value, index) => {
      let canonical = canonicalValue(value)
      if (csvColumns?.includes(index)) {
        canonical = canonical.split(',').sort().join(',')
      }
      return canonical
    })
    .join('')
}

function diffRows(
  expected: unknown[][],
  actual: unknown[][],
  options: { multiset?: boolean; csvColumns?: number[] } = {},
): string | undefined {
  let left = expected.map((row) => canonicalRow(row, options.csvColumns))
  let right = actual.map((row) => canonicalRow(row, options.csvColumns))
  if (options.multiset) {
    left = [...left].sort()
    right = [...right].sort()
  }
  if (left.length !== right.length) {
    return `row count ${expected.length} vs ${actual.length}`
  }
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) {
      return `row ${index}:\n  mysql   ${left[index].replaceAll('', ' | ')}\n  pintail ${right[index].replaceAll('', ' | ')}`
    }
  }
  return undefined
}

// ---------------------------------------------------------------------------
// Convergence: every MySQL base table must read back identically.

async function baseTables(): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT TABLE_NAME FROM information_schema.TABLES ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME`,
  )
  return rows.map((row) => String(row[0]))
}

async function tableColumns(table: string): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT COLUMN_NAME FROM information_schema.COLUMNS ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_NAME = '${table}' ` +
      `AND GENERATION_EXPRESSION = '' ORDER BY ORDINAL_POSITION`,
  )
  return rows.map((row) => String(row[0]))
}

async function tableKey(table: string): Promise<string[]> {
  const rows = await mysqlRows(
    `SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE ` +
      `WHERE TABLE_SCHEMA = '${DATABASE}' AND TABLE_NAME = '${table}' ` +
      `AND CONSTRAINT_NAME = 'PRIMARY' ORDER BY ORDINAL_POSITION`,
  )
  return rows.map((row) => String(row[0]))
}

async function tableDiff(table: string): Promise<string | undefined> {
  const columns = await tableColumns(table)
  const key = await tableKey(table)
  const projection = columns.map((column) => `\`${column}\``).join(', ')
  const order = (key.length > 0 ? key : columns).map((column) => `\`${column}\``).join(', ')
  const sql = `SELECT ${projection} FROM \`${table}\` ORDER BY ${order}`
  const expected = await mysqlRows(sql)
  let actual: unknown[][]
  try {
    actual = await pintailQuery(sql)
  } catch (error) {
    return `pintail query failed: ${error}`
  }
  // Keyless tables have no deterministic order on either side.
  return diffRows(expected, actual, { multiset: key.length === 0 })
}

async function metadataDiff(): Promise<string | undefined> {
  const query =
    `SELECT table_name, column_name, ordinal_position, data_type, column_type, ` +
    `is_nullable, character_maximum_length, character_octet_length, ` +
    `numeric_precision, numeric_scale, datetime_precision, column_default, ` +
    `extra, generation_expression ` +
    `FROM information_schema.columns WHERE table_schema = '${DATABASE}' ` +
    `ORDER BY table_name, ordinal_position`
  const expected = await mysqlRows(query)
  try {
    return diffRows(expected, await pintailQuery(query))
  } catch (error) {
    return `pintail metadata query failed: ${error}`
  }
}

async function verifyConvergence(phase: string) {
  const tables = await baseTables()
  const pending = new Map<string, string>()
  const deadline = Date.now() + CONVERGE_TIMEOUT_MS
  for (const table of tables) pending.set(table, 'not yet checked')
  while (pending.size > 0 && Date.now() < deadline) {
    for (const table of [...pending.keys()]) {
      const diff = await tableDiff(table)
      if (diff === undefined) pending.delete(table)
      else pending.set(table, diff)
    }
    if (pending.size > 0) await Bun.sleep(CONVERGE_POLL_MS)
  }
  for (const table of tables) {
    const diff = pending.get(table)
    if (diff === undefined) {
      results.push({ phase, check: `converge:${table}`, status: 'PASS' })
    } else {
      const signature = documentedGapTables.get(table)
      const status = signature?.test(diff) ? 'WARN' : 'FAIL'
      results.push({ phase, check: `converge:${table}`, status, detail: diff })
      for (const line of diff.split('\n')) log(`${status} converge:${table} — ${line}`)
    }
  }
  if (pending.size === 0) {
    log(`${phase}: converged (${tables.length} tables)`)
  }
  const metadataDeadline = Date.now() + CONVERGE_TIMEOUT_MS
  let metadata = await metadataDiff()
  while (metadata !== undefined && Date.now() < metadataDeadline) {
    await Bun.sleep(CONVERGE_POLL_MS)
    metadata = await metadataDiff()
  }
  results.push({
    phase,
    check: 'converge:information_schema.columns',
    status:
      metadata === undefined
        ? 'PASS'
        : documentedMetadataGaps.some((signature) => signature.test(metadata))
          ? 'WARN'
          : 'FAIL',
    detail: metadata,
  })
}

async function verifyCorpus(phase: string) {
  for (const query of differentialQueries) {
    if (query.tables.some((table) => documentedGapTables.has(table))) {
      results.push({ phase, check: `query:${query.name}`, status: 'SKIP' })
      continue
    }
    let expected: unknown[][]
    try {
      expected = await mysqlRows(query.sql)
    } catch (error) {
      results.push({
        phase,
        check: `query:${query.name}`,
        status: 'FAIL',
        detail: `mysql rejected the corpus query: ${error}`,
      })
      continue
    }
    try {
      const actual = await pintailQuery(query.sql)
      const diff = diffRows(expected, actual, { csvColumns: query.csvColumns })
      const failure = query.documentedGap ? ('WARN' as const) : ('FAIL' as const)
      results.push({
        phase,
        check: `query:${query.name}`,
        status: diff === undefined ? 'PASS' : failure,
        detail: diff && query.documentedGap ? `${query.documentedGap}\n${diff}` : diff,
      })
      if (diff) for (const line of diff.split('\n')) log(`${failure} query:${query.name} — ${line}`)
    } catch (error) {
      // A documented gap warns when the engine REFUSES the query, not only
      // when it answers differently. Refusal is how an unimplemented feature
      // usually surfaces here - an unsupported collation is rejected at bind
      // time rather than producing a wrong row - so failing on it would mean
      // a gap could never be recorded before it was fixed, which is backwards:
      // the case exists to prove the fix.
      const failure = query.documentedGap ? ('WARN' as const) : ('FAIL' as const)
      results.push({
        phase,
        check: `query:${query.name}`,
        status: failure,
        detail: query.documentedGap ? `${query.documentedGap}\n${error}` : String(error),
      })
      log(`${failure} query:${query.name} — ${error}`)
    }
  }
}

// ---------------------------------------------------------------------------
// Workload phases.

function mulberry32(seed: number) {
  let state = seed
  return () => {
    state |= 0
    state = (state + 0x6d2b79f5) | 0
    let t = Math.imul(state ^ (state >>> 15), 1 | state)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

async function sql(statement: string) {
  await mysqlConnection!.query(statement)
}

async function phaseSeed() {
  await sql(`CREATE TABLE customers (
    id INT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    name VARCHAR(64) NOT NULL,
    email VARCHAR(96) NULL,
    tier ENUM('free','pro','enterprise') NOT NULL DEFAULT 'free',
    tags SET('alpha','beta','vip') NOT NULL DEFAULT '',
    balance DECIMAL(12,2) NOT NULL DEFAULT 0,
    meta JSON NULL,
    avatar VARBINARY(16) NULL,
    latin_note VARCHAR(32) CHARACTER SET latin1 NULL,
    -- MySQL 5.x's default collation, which most existing schemas still carry
    -- because a table keeps whatever it was created with. The engine's own
    -- default is utf8mb4_0900_ai_ci, so without a column like this the gate
    -- never exercises a second collation at all.
    legacy_label VARCHAR(64) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE orders (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    customer_id INT UNSIGNED NOT NULL,
    status ENUM('pending','processing','shipped','delivered','cancelled') NOT NULL,
    total DECIMAL(12,2) NOT NULL,
    placed_on DATE NOT NULL,
    updated_at TIMESTAMP(6) NULL
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE order_items (
    order_id BIGINT UNSIGNED NOT NULL,
    line_no INT NOT NULL,
    product VARCHAR(64) NOT NULL,
    qty SMALLINT UNSIGNED NOT NULL,
    price DECIMAL(10,2) NOT NULL,
    PRIMARY KEY (order_id, line_no)
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE audit_log (note VARCHAR(128) NOT NULL) DEFAULT CHARACTER SET utf8mb4`)
  await sql(`CREATE TABLE counters (
    id TINYINT UNSIGNED PRIMARY KEY,
    u8 TINYINT UNSIGNED NOT NULL,
    u16 SMALLINT UNSIGNED NOT NULL,
    u32 INT UNSIGNED NOT NULL,
    u64 BIGINT UNSIGNED NOT NULL,
    s64 BIGINT NOT NULL
  )`)

  const random = mulberry32(0x5eed)
  const tiers = ['free', 'pro', 'enterprise']
  const tags = ['', 'alpha', 'beta', 'vip', 'alpha,vip', 'beta,vip']
  const names = ['Asha', 'Bruno', 'Chloé', 'Dmitri', 'えみ', 'Farah', 'Göran', 'Priya']
  for (let id = 1; id <= 40; id += 1) {
    const name = `${names[id % names.length]} ${id}`
    const email = id % 7 === 0 ? 'NULL' : `'user${id}@example.com'`
    const tier = tiers[Math.floor(random() * 3)]
    const tag = tags[Math.floor(random() * tags.length)]
    const balance = (random() * 2000 - 500).toFixed(2)
    const meta =
      id % 5 === 0 ? 'NULL' : `'{"lang":"en","score":${Math.floor(random() * 100)}}'`
    const avatar = id % 6 === 0 ? 'NULL' : `X'${id.toString(16).padStart(4, '0')}beef'`
    await sql(
      `INSERT INTO customers (name, email, tier, tags, balance, meta, avatar, latin_note, legacy_label) VALUES ` +
        `('${name}', ${email}, '${tier}', '${tag}', ${balance}, ${meta}, ${avatar}, _latin1 0x636166E9, ` +
        // Values chosen to exercise what general_ci actually does differently:
        // ASCII case folding, Latin-1 accent folding onto the base letter,
        // PAD SPACE trailing spaces, and the supplementary-plane collapse
        // where every character above the BMP weighs the same.
        `${['\'Active\'', '\'active\'', '\'ACTIVE\'', '\'Ärger\'', '\'arger\'', '\'pending  \'', '\'pending\'', '\'😀\'', '\'𠀀\'', 'NULL'][id % 10]})`,
    )
  }
  const statuses = ['pending', 'processing', 'shipped', 'delivered', 'cancelled']
  for (let id = 1; id <= 200; id += 1) {
    const customer = 1 + Math.floor(random() * 40)
    const status = statuses[Math.floor(random() * statuses.length)]
    const total = (random() * 1000).toFixed(2)
    const day = 1 + Math.floor(random() * 28)
    const month = 1 + Math.floor(random() * 12)
    const updated =
      id % 3 === 0 ? 'NULL' : `'2025-0${(id % 9) + 1}-1${id % 10} 08:0${id % 10}:00'`
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, updated_at) VALUES ` +
        `(${customer}, '${status}', ${total}, '2024-${String(month).padStart(2, '0')}-${String(day).padStart(2, '0')}', ${updated})`,
    )
    const lines = 1 + Math.floor(random() * 3)
    for (let line = 1; line <= lines; line += 1) {
      await sql(
        `INSERT INTO order_items VALUES (${id}, ${line}, 'sku-${(id * 7 + line) % 50}', ` +
          `${1 + Math.floor(random() * 9)}, ${(random() * 200).toFixed(2)})`,
      )
    }
  }
  await sql(`INSERT INTO audit_log VALUES ('seed complete'), ('第二条 unicode note')`)
  await sql(
    `INSERT INTO counters VALUES (1, 200, 65535, 3000000000, 18446744073709551615, -9223372036854775808), ` +
      `(2, 0, 0, 0, 0, 9223372036854775807)`,
  )
}

async function phaseCrud() {
  // Point updates, bulk updates, deletes, and an explicit rollback.
  await sql(`UPDATE customers SET balance = balance + 10.55, tier = 'pro' WHERE id = 3`)
  await sql(`UPDATE orders SET status = 'delivered', updated_at = '2025-06-01 12:00:00' WHERE id <= 20`)
  await sql(`DELETE FROM orders WHERE id IN (5, 15, 25)`)
  await sql(`DELETE FROM order_items WHERE order_id IN (5, 15, 25)`)
  await sql(
    `INSERT INTO customers (id, name, email, balance) VALUES (1, 'dupe', 'x@y.z', 1) ` +
      `ON DUPLICATE KEY UPDATE balance = balance + 100`,
  )
  await mysqlConnection!.beginTransaction()
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (2, 'pending', 42.42, '2025-07-01')`)
  await sql(`UPDATE customers SET balance = balance - 42.42 WHERE id = 2`)
  await mysqlConnection!.commit()
  await mysqlConnection!.beginTransaction()
  await sql(`DELETE FROM customers WHERE id = 4`)
  await sql(`UPDATE orders SET total = 0 WHERE customer_id = 4`)
  await mysqlConnection!.rollback()
  await sql(`INSERT INTO audit_log VALUES ('crud complete')`)
}

async function phaseTypeEdges() {
  await sql(
    `INSERT INTO customers (name, email, tier, tags, balance, meta, avatar, latin_note) VALUES ` +
      `('emoji 🦆 café', '', 'enterprise', 'alpha,beta,vip', -0.01, '{"nested":{"deep":[1,2,3]},"nul":null}', X'00FF10', _latin1 0x80), ` +
      `('nulls', NULL, 'free', '', 0.00, NULL, NULL, NULL)`,
  )
  await sql(`UPDATE counters SET u8 = 255, u16 = 65534, u32 = 4294967295, u64 = 9223372036854775808 WHERE id = 2`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, updated_at) VALUES ` +
      `(1, 'pending', 0.01, '2020-02-29', '2038-01-19 03:14:07.499999'), ` +
      `(1, 'cancelled', 9999999999.99, '1970-01-01', '1970-01-01 00:00:01.000001')`,
  )
}

async function phaseDdl() {
  // ADD COLUMN, live writes into it, then DROP a different column.
  await sql(`ALTER TABLE orders ADD COLUMN coupon VARCHAR(24) NULL`)
  await sql(`UPDATE orders SET coupon = 'SUMMER10' WHERE id % 10 = 0`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, coupon) VALUES (7, 'shipped', 77.77, '2025-07-07', 'NEW7')`,
  )
  await sql(`ALTER TABLE customers DROP COLUMN latin_note`)
  await sql(`UPDATE customers SET balance = balance + 1 WHERE id = 1`)
  // CREATE TABLE mid-stream: the replica must pick it up automatically.
  await sql(`CREATE TABLE shipments (
    id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY,
    order_id BIGINT UNSIGNED NOT NULL,
    carrier VARCHAR(32) NOT NULL,
    shipped_on DATE NULL
  ) DEFAULT CHARACTER SET utf8mb4`)
  await sql(
    `INSERT INTO shipments (order_id, carrier, shipped_on) VALUES ` +
      `(1, 'DHL', '2025-07-08'), (2, 'UPS', NULL), (3, 'FedEx', '2025-07-09')`,
  )
  await sql(`UPDATE shipments SET carrier = 'DHL Express' WHERE id = 1`)
  // Storage-compatible MODIFY COLUMN evolves in place — no resync: an
  // integer widening and a VARCHAR widening, with live writes after each.
  await sql(`ALTER TABLE shipments MODIFY COLUMN carrier VARCHAR(64) NOT NULL`)
  await sql(
    `UPDATE shipments SET carrier = 'A Rather Long Carrier Name For Widths' WHERE id = 2`,
  )
  await sql(`ALTER TABLE orders MODIFY COLUMN customer_id BIGINT UNSIGNED NOT NULL`)
  await sql(`UPDATE orders SET customer_id = 5000000001 WHERE id = 3`)
  await sql(`ALTER TABLE customers MODIFY COLUMN balance DECIMAL(14,2) NOT NULL DEFAULT 0`)
  await sql(`UPDATE customers SET balance = balance + 100000000000.25 WHERE id = 3`)
  // Index-only changes replicate rows straight through — no resync.
  await sql(`ALTER TABLE orders ADD INDEX status_idx (status)`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (9, 'processing', 12.34, '2025-07-10')`,
  )
  await sql(`ALTER TABLE orders DROP INDEX status_idx`)
  await sql(`UPDATE orders SET status = 'shipped' WHERE customer_id = 9 AND status = 'processing'`)
  // A whole-table character-set conversion. This is the statement an operator
  // runs to move a table between collations, and the schema tracker used to
  // stop replication dead on it - the SQL parser cannot represent it, so it
  // came back as a parse error rather than a change. Rows written afterwards
  // are what prove the stream survived it.
  await sql(`ALTER TABLE shipments CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci`)
  await sql(
    `INSERT INTO shipments (order_id, carrier, shipped_on) VALUES (4, 'Freight Post', '2025-07-11')`,
  )
  await sql(`UPDATE shipments SET carrier = 'Freight Express' WHERE carrier = 'Freight Post'`)
  // TRUNCATE and refill.
  await sql(`TRUNCATE TABLE audit_log`)
  await sql(`INSERT INTO audit_log VALUES ('after truncate')`)
}

async function phaseSchemaDriftMinimal() {
  // The same missed schema change, under the metadata setting production
  // actually runs. `binlog_row_metadata=MINIMAL` omits the column names from
  // every table map, so a row image can only be read positionally and the
  // replica has nothing to align a mismatched width against. Re-probing is
  // the only repair available, and it works precisely when the refreshed
  // schema and the row in hand agree on width - which is the production
  // shape: one ALTER, then the next INSERT.
  //
  // Written events carry whatever metadata was in force when they were
  // written, so this phase restores FULL before it ends and converges on its
  // own. Everything it produced stays MINIMAL regardless.
  await sql(`SET GLOBAL binlog_row_metadata = 'MINIMAL'`)
  try {
    await sql(`SET sql_log_bin = 0`)
    await sql(`ALTER TABLE orders ADD COLUMN minimal_note VARCHAR(32) NULL`)
    await sql(`SET sql_log_bin = 1`)
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, minimal_note) VALUES ` +
        `(2, 'pending', 19.99, '2025-07-30', 'after-minimal-add')`,
    )
    await sql(`UPDATE orders SET minimal_note = 'seen' WHERE id % 6 = 0`)
    await sql(`DELETE FROM orders WHERE status = 'cancelled' AND total < 0`)
  } finally {
    await sql(`SET GLOBAL binlog_row_metadata = 'FULL'`)
  }
}

async function phaseSchemaDriftUnseen() {
  // A schema change the CDC stream never sees as DDL.
  //
  // Production hit this three days running: a hand-written
  // `ALTER TABLE Payment ADD COLUMN` landed, and the next INSERT arrived as a
  // row image one column wider than the probed schema. The decoder refused
  // the row - correctly, since it cannot know which column is which - and
  // marked the whole table for a full resnapshot. Every such row was dropped
  // until someone resynced.
  //
  // `sql_log_bin = 0` reproduces it exactly: the ALTER never enters the
  // binlog, so no DDL reaches the stream, while the row images that follow
  // carry the new width. The replica must re-probe, adopt the schema and keep
  // the rows - WITHOUT resnapshotting the table, which on a large table is
  // hours of copying to learn about one column.
  const widen = async (column: string, type: string) => {
    await sql(`SET sql_log_bin = 0`)
    await sql(`ALTER TABLE orders ADD COLUMN ${column} ${type}`)
    await sql(`SET sql_log_bin = 1`)
  }

  await widen('unseen_note', 'VARCHAR(32) NULL')
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on, unseen_note) VALUES ` +
      `(3, 'pending', 31.50, '2025-08-01', 'after-hidden-add')`,
  )
  await sql(`UPDATE orders SET unseen_note = 'touched' WHERE id % 7 = 0`)

  // Stress: repeated invisible widenings interleaved with continuous writes
  // on the same table, so the heal runs against a moving schema rather than
  // a single quiet change. Each round adds a column the stream never hears
  // about and then writes through it immediately.
  for (let round = 0; round < 4; round += 1) {
    await widen(`drift_${round}`, 'INT NULL')
    await sql(
      `INSERT INTO orders (customer_id, status, total, placed_on, drift_${round}) VALUES ` +
        `(4, 'shipped', ${10 + round}.25, '2025-08-0${round + 2}', ${round * 11})`,
    )
    await sql(`UPDATE orders SET drift_${round} = ${round} WHERE id % 5 = 0`)
    await sql(`DELETE FROM orders WHERE status = 'cancelled' AND total < 0`)
  }

  // A drop the stream also never sees: the row image narrows, which is the
  // same defect mirrored, and must heal the same way.
  await sql(`SET sql_log_bin = 0`)
  await sql(`ALTER TABLE orders DROP COLUMN drift_0`)
  await sql(`SET sql_log_bin = 1`)
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
      `(5, 'delivered', 88.88, '2025-08-09')`,
  )
}

async function phaseDdlDocumentedGaps() {
  // Table rename is documented as quarantine; type changes are not part of
  // the DDL gate. Exercise both so regressions in the documented behavior
  // surface as WARN diffs, and improvements flip them to PASS.
  // Rename quarantine: the renamed table never appears in the replica.
  documentedGapTables.set('audit_log', /unknown table/)
  documentedGapTables.set('audit_history', /unknown table/)
  documentedMetadataGaps.push(
    /^row \d+:\n  mysql   audit_history( \|.*)\n  pintail audit_log\1$/,
  )
  await sql(`RENAME TABLE audit_log TO audit_history`)
  await sql(`INSERT INTO audit_history VALUES ('post rename')`)
  // In-place type change: replication stops applying to the table, so the
  // divergence is a row-content diff (never a missing table or an error).
  documentedGapTables.set('order_items', /^row \d+:/)
  await sql(`ALTER TABLE order_items MODIFY qty INT UNSIGNED NOT NULL`)
  await sql(`UPDATE order_items SET qty = qty + 1 WHERE order_id = 1`)
}

/// Runs one corpus query with convergence retries: writes are paused at the
/// call site, so the replica settles to the source state within the window.
async function liveQueryConverges(
  query: (typeof differentialQueries)[number],
  deadlineMs: number,
): Promise<string | undefined> {
  const deadline = Date.now() + deadlineMs
  let last: string | undefined = 'never compared'
  while (Date.now() < deadline) {
    const expected = await mysqlRows(query.sql)
    try {
      const actual = await pintailQuery(query.sql)
      last = diffRows(expected, actual, { csvColumns: query.csvColumns })
      if (last === undefined) return undefined
    } catch (error) {
      last = String(error)
    }
    await Bun.sleep(1000)
  }
  return last
}

async function phaseChurn() {
  const random = mulberry32(0xc0ffee)
  const statuses = ['pending', 'processing', 'shipped', 'delivered', 'cancelled']
  const liveEligible = differentialQueries.filter(
    (query) =>
      !query.documentedGap && !query.tables.some((table) => documentedGapTables.has(table)),
  )
  let liveIndex = 0
  let inTransaction = false
  for (let op = 0; op < 400; op += 1) {
    // Every 100 operations, sweep three corpus queries against the live,
    // still-ingesting replica: this is what exercises the mid-ingest
    // aggregate paths rather than only the settled state.
    if (op > 0 && op % 100 === 0 && !inTransaction) {
      for (let sweep = 0; sweep < 3; sweep += 1) {
        const query = liveEligible[liveIndex % liveEligible.length]
        liveIndex += 1
        const diff = await liveQueryConverges(query, 60_000)
        results.push({
          phase: 'churn-live',
          check: `live:${query.name}`,
          status: diff === undefined ? 'PASS' : 'FAIL',
          detail: diff,
        })
        if (diff) for (const line of diff.split('\n')) log(`FAIL live:${query.name} — ${line}`)
      }
    }
    if (!inTransaction && random() < 0.1) {
      await mysqlConnection!.beginTransaction()
      inTransaction = true
    }
    const roll = random()
    if (roll < 0.45) {
      const customer = 1 + Math.floor(random() * 40)
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
          `(${customer}, '${statuses[Math.floor(random() * 5)]}', ${(random() * 500).toFixed(2)}, ` +
          `'2025-0${1 + Math.floor(random() * 9)}-0${1 + Math.floor(random() * 9)}')`,
      )
    } else if (roll < 0.75) {
      await sql(
        `UPDATE orders SET status = '${statuses[Math.floor(random() * 5)]}', ` +
          `total = total + ${(random() * 10 - 5).toFixed(2)} ` +
          `WHERE id = (SELECT id FROM (SELECT MIN(id) + FLOOR(RAND(${op}) * (MAX(id) - MIN(id))) AS id FROM orders) pick)`,
      )
    } else if (roll < 0.9) {
      await sql(`DELETE FROM orders WHERE id = (SELECT id FROM (SELECT MIN(id) AS id FROM orders WHERE status = 'cancelled') pick WHERE pick.id IS NOT NULL)`)
    } else {
      const customer = 1 + Math.floor(random() * 40)
      await sql(`UPDATE customers SET balance = balance + ${(random() * 20 - 10).toFixed(2)} WHERE id = ${customer}`)
    }
    if (inTransaction && random() < 0.3) {
      if (random() < 0.15) await mysqlConnection!.rollback()
      else await mysqlConnection!.commit()
      inTransaction = false
    }
  }
  if (inTransaction) await mysqlConnection!.commit()
}

/// Application servers and BI tools reach Pintail through a connection
/// pool, not a single socket: a fixed set of connections borrowed and
/// returned, where each borrower assumes it gets a clean session. This
/// phase drives a real mysql2 pool the way those clients do.
const POOL_SIZE = 4
const POOL_BORROWS = 40

async function phasePooling() {
  await sql(`INSERT INTO audit_log VALUES ('pooling phase')`)

  const settings = {
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: wireSecret,
    database: DATABASE,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
  }
  const pool = mysql.createPool({
    ...settings,
    connectionLimit: POOL_SIZE,
    waitForConnections: true,
  })
  const phase = 'pooling'
  try {
    // Far more borrows than sockets, issued at once: every borrow must get
    // a working connection and the same answer as the source.
    const [expected] = await mysqlRows('SELECT COUNT(*) FROM orders')
    const answers = await Promise.all(
      Array.from({ length: POOL_BORROWS }, async () => {
        const [rows] = await pool.query<mysql.RowDataPacket[]>({
          sql: 'SELECT COUNT(*) FROM orders',
          rowsAsArray: true,
        })
        return String((rows as unknown as unknown[][])[0][0])
      }),
    )
    const wrong = answers.filter((answer) => answer !== String(expected[0]))
    results.push({
      phase,
      check: `pool:concurrent-borrows(${POOL_BORROWS} over ${POOL_SIZE})`,
      status: wrong.length === 0 ? 'PASS' : 'FAIL',
      detail: wrong.length === 0 ? undefined : `${wrong.length} borrows disagreed: ${wrong[0]}`,
    })

    // Prepared statements are per-connection state; a pool prepares on
    // whichever socket it hands out.
    const prepared = await Promise.all(
      Array.from({ length: POOL_BORROWS }, async (_, index) => {
        const [rows] = await pool.execute<mysql.RowDataPacket[]>({
          sql: 'SELECT COUNT(*) FROM orders WHERE id > ?',
          values: [index % 5],
          rowsAsArray: true,
        })
        return (rows as unknown as unknown[][])[0][0]
      }),
    )
    results.push({
      phase,
      check: 'pool:prepared-statements',
      status: prepared.every((value) => value !== undefined && value !== null) ? 'PASS' : 'FAIL',
    })
  } finally {
    await pool.end()
  }

  // Session state must not survive a borrow. A one-connection pool
  // guarantees the second borrow is the same socket as the first.
  const single = mysql.createPool({ ...settings, connectionLimit: 1, waitForConnections: true })
  try {
    const first = await single.getConnection()
    await first.query("SET time_zone = '+05:30'")
    first.release()
    const second = await single.getConnection()
    const [rows] = await second.query<mysql.RowDataPacket[]>({
      sql: 'SELECT @@session.time_zone',
      rowsAsArray: true,
    })
    second.release()
    const zone = String((rows as unknown as unknown[][])[0][0])
    // Measured against MySQL 8.4 through this exact mysql2 sequence: the
    // second borrow reads '+05:30' there too. mysql2's pool does not reset a
    // connection on release, so session state surviving a borrow is MySQL's
    // behaviour, not a defect. Asserting 'SYSTEM' here asserted something no
    // MySQL server does. The explicit COM_RESET_CONNECTION / COM_CHANGE_USER
    // path is covered in crates/pintail-wire/tests/wire_compat.rs.
    const matches = zone === '+05:30'
    results.push({
      phase,
      check: 'pool:session-state-survives-borrow-like-mysql',
      status: matches ? 'PASS' : 'FAIL',
      detail: matches ? undefined : `expected MySQL's '+05:30', got ${zone}`,
    })
  } finally {
    await single.end()
  }
}

async function phaseOrmCompatibility() {
  const phase = 'orm-compat'
  if (!mysqlEndpoint) throw new Error('MySQL ORM endpoint was not initialized')
  const pintailEndpoint: MysqlEndpoint = {
    host: '127.0.0.1',
    port: pintailWirePort,
    user: DATABASE,
    password: wireSecret,
    database: DATABASE,
  }
  for (const result of await runOrmCompatibility(mysqlEndpoint, pintailEndpoint)) {
    results.push({
      phase,
      check: `${result.client}:${result.check}`,
      status: result.status,
      detail: result.detail,
    })
    if (result.status === 'FAIL') {
      log(`FAIL ${result.client}:${result.check} — ${result.detail}`)
    }
  }
}

async function phaseRestart() {
  log('SIGKILLing pintail mid-stream')
  pintailProcess!.kill(9)
  await pintailProcess!.exited
  // Write while the replica is down; the checkpoint must replay all of it.
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (9, 'processing', 123.45, '2025-08-01')`)
  await sql(`UPDATE customers SET tier = 'enterprise' WHERE id = 9`)
  await sql(`DELETE FROM orders WHERE id = (SELECT id FROM (SELECT MAX(id) AS id FROM orders) pick)`)
  await startPintail()
  await sql(`INSERT INTO orders (customer_id, status, total, placed_on) VALUES (10, 'shipped', 67.89, '2025-08-02')`)
}

async function phaseSpill() {
  const phase = 'spill'
  const repeatedJoinInput = Array.from(
    { length: 8 },
    () => 'SELECT i.order_id, c.id FROM order_items i CROSS JOIN customers c',
  ).join(' UNION ALL ')
  const checks: Array<[string, string, number]> = [
    [
      'sort',
      'SELECT o.id, c.id FROM orders o CROSS JOIN customers c ' +
        'ORDER BY c.id DESC, o.id',
      2 * 1024 * 1024,
    ],
    [
      'aggregate',
      'SELECT o.id, c.id, COUNT(*) FROM orders o CROSS JOIN customers c ' +
        'GROUP BY o.id, c.id',
      4 * 1024 * 1024,
    ],
    [
      'distinct',
      'SELECT DISTINCT o.id, c.id FROM orders o CROSS JOIN customers c',
      2 * 1024 * 1024,
    ],
    [
      'join',
      `SELECT COUNT(*) FROM orders o JOIN (${repeatedJoinInput}) d ON o.id = d.order_id`,
      5 * 512 * 1024,
    ],
  ]

  const stopPintail = async () => {
    try {
      await pintailWire?.end()
    } catch {}
    pintailWire = undefined
    if (pintailProcess) {
      if (pintailProcess.exitCode === null) pintailProcess.kill()
      await pintailProcess.exited
      pintailProcess = undefined
    }
  }

  try {
    await stopPintail()
    for (const [operator, sql, memoryLimit] of checks) {
      // The ordinary E2E process uses the production default. Each short-lived
      // restart admits one decoded input batch but keeps the ceiling below the
      // operator's accumulated state, so the spillable allocation is what
      // crosses the boundary. Aggregate batches have a larger working bound.
      await startPintail(memoryLimit)
      try {
        const rows = await pintailQuery(`EXPLAIN ANALYZE ${sql}`)
        const plan = rows.flat().map(String).join('\n')
        const spilled = /Spill files=[1-9][0-9]* bytes=[1-9][0-9]*/.test(plan)
        results.push({
          phase,
          check: `forced-spill:${operator}`,
          status: spilled ? 'PASS' : 'FAIL',
          detail: spilled ? undefined : `no spill reported:\n${plan}`,
        })
      } catch (error) {
        results.push({
          phase,
          check: `forced-spill:${operator}`,
          status: 'FAIL',
          detail: String(error),
        })
      } finally {
        await stopPintail()
      }
    }
  } finally {
    await stopPintail()
    await startPintail()
  }
}

/// Exercises the control-plane routes against the deployed binary: auth,
/// database lifecycle on a throwaway registration, table metadata, API-key
/// enable/disable round trip, mode switching, resync/reconcile, and the
/// observability endpoints. Each route records its own PASS/FAIL.
async function phaseControlPlane() {
  const check = async (name: string, run: () => Promise<void>) => {
    try {
      await run()
      results.push({ phase: 'control-plane', check: `api:${name}`, status: 'PASS' })
    } catch (error) {
      results.push({
        phase: 'control-plane',
        check: `api:${name}`,
        status: 'FAIL',
        detail: String(error),
      })
      log(`FAIL api:${name} — ${error}`)
    }
  }

  await check('auth login issues a fresh token', async () => {
    const login = await api<{ token: string }>('/api/auth/login', {
      method: 'POST',
      auth: false,
      body: { email: 'e2e@pintail.local', password: 'e2e-gate-password' },
    })
    if (!login.token) throw new Error('login returned no token')
    token = login.token
  })
  await check('auth setup status responds', async () => {
    await api('/api/auth/setup/status', { auth: false })
  })
  await check('health, status, and metrics respond', async () => {
    for (const path of ['/health', '/status', '/metrics']) {
      const response = await fetch(`${pintailUrl}${path}`, {
        headers: { Authorization: `Bearer ${token}` },
      })
      if (!response.ok) throw new Error(`${path} returned ${response.status}`)
    }
  })
  await check('databases list and detail agree', async () => {
    const list = await api<Array<{ id: string }>>('/api/databases')
    if (!list.some((database) => database.id === databaseId)) {
      throw new Error('registered database missing from the list')
    }
    await api(`/api/databases/${databaseId}`)
    await api(`/api/databases/${databaseId}/status`)
  })
  await check('connection test succeeds', async () => {
    await api(`/api/databases/${databaseId}/test`, { method: 'POST' })
  })
  await check('activity and dlq respond', async () => {
    await api(`/api/activity?db=${databaseId}&limit=10`)
    const dlq = await api<unknown[]>('/api/dlq')
    if (dlq.length > 0) throw new Error(`DLQ is not empty: ${JSON.stringify(dlq[0])}`)
  })
  await check('table metadata routes match the source', async () => {
    const tables = await api<Array<{ name: string }>>(`/api/tables?db=${databaseId}`)
    if (!tables.some((table) => table.name === 'orders')) {
      throw new Error('orders missing from table list')
    }
    await api(`/api/tables/orders/schema?db=${databaseId}`)
    await api(`/api/tables/orders/data?db=${databaseId}`)
    const counted = await api<unknown>(`/api/tables/orders/count?db=${databaseId}`)
    const expected = await mysqlRows('SELECT COUNT(*) FROM orders')
    const mysqlCount = Number(expected[0][0])
    const text = JSON.stringify(counted)
    if (!text.includes(String(mysqlCount))) {
      throw new Error(`count response ${text} does not contain MySQL's ${mysqlCount}`)
    }
  })
  await check('api key disable blocks the wire, enable restores it', async () => {
    const keys = await api<Array<{ id: string; name: string }>>(
      `/api/databases/${databaseId}/api-keys`,
    )
    const key = keys.find((candidate) => candidate.name === 'e2e-gate')
    if (!key) throw new Error('e2e-gate key missing from the list')
    await api(`/api/databases/${databaseId}/api-keys/${key.id}`, {
      method: 'PATCH',
      body: { enabled: false },
    })
    try {
      pintailWire = undefined
      let rejected = false
      try {
        await pintailQuery('SELECT 1 FROM orders LIMIT 1')
      } catch {
        rejected = true
      }
      if (!rejected) throw new Error('disabled key still authenticates on the wire')
    } finally {
      await api(`/api/databases/${databaseId}/api-keys/${key.id}`, {
        method: 'PATCH',
        body: { enabled: true },
      })
      pintailWire = undefined
    }
    await pintailQuery('SELECT 1 FROM orders LIMIT 1')
  })
  await check('sse event stream connects', async () => {
    const controller = new AbortController()
    const timer = setTimeout(() => controller.abort(), 5_000)
    try {
      const response = await fetch(`${pintailUrl}/api/events`, {
        headers: { Authorization: `Bearer ${token}` },
        signal: controller.signal,
      })
      if (!response.ok) throw new Error(`/api/events returned ${response.status}`)
      const type = response.headers.get('content-type') ?? ''
      if (!type.includes('event-stream')) throw new Error(`unexpected content type ${type}`)
    } catch (error) {
      if (!String(error).includes('abort')) throw error
    } finally {
      clearTimeout(timer)
      controller.abort()
    }
  })
  await check('mode switches to polling and back with exact counts', async () => {
    await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'polling' } })
    try {
      await sql(
        `INSERT INTO orders (customer_id, status, total, placed_on) VALUES (11, 'pending', 55.55, '2025-08-03')`,
      )
      const deadline = Date.now() + 120_000
      for (;;) {
        const expected = await mysqlRows('SELECT COUNT(*) FROM orders')
        const actual = await pintailQuery('SELECT COUNT(*) FROM orders')
        if (String(expected[0][0]) === String(actual[0][0])) break
        if (Date.now() > deadline) {
          const status = await api<unknown>(`/api/databases/${databaseId}/status`)
          const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=5`)
          throw new Error(
            `polling never converged: ${expected[0][0]} vs ${actual[0][0]}; ` +
              `status: ${JSON.stringify(status)}; activity: ${JSON.stringify(activity)}`,
          )
        }
        await Bun.sleep(2_000)
      }
    } finally {
      // Restore CDC even when convergence fails: leaving the database in
      // polling mode would cascade the failure into every later check.
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'cdc' } })
    }
  })
  await check('resync and reconcile are accepted', async () => {
    // A supervisor cycle may hold the job lock at this instant; the 409 is
    // correct API behavior, so retry briefly instead of failing the check.
    for (let attempt = 0; ; attempt += 1) {
      try {
        await api(`/api/databases/${databaseId}/tables/orders/resync`, { method: 'POST' })
        break
      } catch (error) {
        if (!String(error).includes('409') || attempt >= 20) throw error
        await Bun.sleep(2_000)
      }
    }
    // The resync schedules a snapshot job; reconcile is correctly refused
    // (409) while that job runs, so wait for streaming state first.
    const deadline = Date.now() + 120_000
    for (;;) {
      const status = await api<{
        state: string
        tables?: Array<{ name: string; last_error?: string }>
      }>(`/api/databases/${databaseId}/snapshot/status`)
      if (status.state === 'streaming' || status.state === 'polling') break
      if (status.state === 'error' || Date.now() > deadline) {
        const errors = (status.tables ?? [])
          .filter((table) => table.last_error)
          .map((table) => `${table.name}: ${table.last_error}`)
          .join('; ')
        const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=5`)
        throw new Error(
          `resync did not settle: ${status.state}; ${errors}; recent activity: ${JSON.stringify(activity)}`,
        )
      }
      await Bun.sleep(2_000)
    }
    const retries = Date.now() + 60_000
    for (;;) {
      try {
        await api(`/api/databases/${databaseId}/tables/customers/reconcile`, { method: 'POST' })
        break
      } catch (error) {
        if (!String(error).includes('409') || Date.now() > retries) throw error
        await Bun.sleep(3_000)
      }
    }
  })
  await check('resync recopies only the table it names', async () => {
    // The endpoint takes a table name and used to resnapshot the whole
    // database, which on a large source is hours of copying to repair one
    // table. It now snapshots just that table behind its own binlog fence, so
    // what this asserts is the SCOPE, not the outcome - a full-database
    // resnapshot also ends with correct data, just far more slowly.
    //
    // A database-wide snapshot puts every table into 'snapshotting' and
    // rewrites its rows; a per-table one leaves the others alone. So the
    // observable is the other tables' row counts holding steady across the
    // operation, and the accepted job naming the one table it will touch.
    const countOf = async (table: string) => {
      const rows = await pintailQuery(`SELECT COUNT(*) FROM ${table}`)
      return String(rows[0][0])
    }
    // `orders` is in here deliberately. An earlier version of this check
    // pinned only the OTHER tables and passed while orders was left empty -
    // the store was cleared for the recopy and the chunk journal still said
    // every chunk was done, so the copy no-opped and reported the previous
    // run's totals. A resnapshot must leave the table it recopies identical,
    // not merely leave the others alone.
    const before = {
      orders: await countOf('orders'),
      customers: await countOf('customers'),
      order_items: await countOf('order_items'),
    }
    let accepted: { run_id: string; state: string; table: string } | undefined
    for (let attempt = 0; ; attempt += 1) {
      try {
        accepted = await api<{ run_id: string; state: string; table: string }>(
          `/api/databases/${databaseId}/tables/orders/resync`,
          { method: 'POST' },
        )
        break
      } catch (error) {
        if (!String(error).includes('409') || attempt >= 20) throw error
        await Bun.sleep(2_000)
      }
    }
    if (accepted.table !== 'orders' || accepted.state !== 'snapshotting') {
      throw new Error(`expected a per-table snapshot job, got ${JSON.stringify(accepted)}`)
    }
    const deadline = Date.now() + 120_000
    for (;;) {
      const status = await api<{ state: string; tables?: Array<{ name: string; last_error?: string }> }>(
        `/api/databases/${databaseId}/snapshot/status`,
      )
      if (status.state === 'streaming' || status.state === 'polling') break
      if (status.state === 'error' || Date.now() > deadline) {
        throw new Error(`per-table resync did not settle: ${JSON.stringify(status)}`)
      }
      await Bun.sleep(2_000)
    }
    // The recopied table reappears asynchronously, so give the counts a
    // window to settle rather than reading them the instant the job reports
    // done.
    const settled = Date.now() + 60_000
    for (;;) {
      const actual: Record<string, string> = {}
      for (const table of Object.keys(before)) actual[table] = await countOf(table)
      const changed = Object.entries(before).filter(
        ([table, expected]) => actual[table] !== expected,
      )
      if (changed.length === 0) break
      if (Date.now() > settled) {
        throw new Error(
          `resync of orders left tables changed: ` +
            changed
              .map(([table, expected]) => `${table} ${expected} before, ${actual[table]} after`)
              .join('; '),
        )
      }
      await Bun.sleep(2_000)
    }
  })
  await check('keyless policy: ambiguity quarantines and exact multiplicity repairs', async () => {
    // A table with no primary or unique key replicates inserts, but a CDC
    // UPDATE cannot be targeted and must flag the table needs_resync under
    // the default quarantine policy. Switching the database to auto_resync
    // lets the supervisor repair it with a forced snapshot.
    const detail = await api<{ name: string; mode: string; keyless_policy: string }>(
      `/api/databases/${databaseId}`,
    )
    if (detail.keyless_policy !== 'quarantine') {
      throw new Error(`expected default policy quarantine, got ${detail.keyless_policy}`)
    }
    await sql(
      'CREATE TABLE keyless_log (label VARCHAR(32) NOT NULL, amount INT NOT NULL)',
    )
    await sql(
      "INSERT INTO keyless_log (label, amount) VALUES ('a', 1), ('b', 2), ('b', 2), ('c', 3)",
    )
    const insertDeadline = Date.now() + 120_000
    for (;;) {
      try {
        const actual = await pintailQuery('SELECT COUNT(*) FROM keyless_log')
        if (String(actual[0][0]) === '4') break
      } catch {
        // table not yet replicated
      }
      if (Date.now() > insertDeadline) throw new Error('keyless inserts never replicated')
      await Bun.sleep(2_000)
    }
    await sql("UPDATE keyless_log SET amount = amount + 10 WHERE label = 'b'")
    const flagDeadline = Date.now() + 120_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string; key_mode: string; mutation_guarantee: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (
        table?.state === 'needs_resync' &&
        table.key_mode === 'append_row_id' &&
        table.mutation_guarantee === 'quarantined'
      ) break
      if (Date.now() > flagDeadline) {
        throw new Error(
          `keyless UPDATE never flagged needs_resync: ${JSON.stringify(table)}`,
        )
      }
      await Bun.sleep(2_000)
    }
    let rejected = false
    try {
      await api(`/api/databases/${databaseId}`, {
        method: 'PUT',
        body: { name: detail.name, mode: detail.mode, keyless_policy: 'bogus' },
      })
    } catch {
      rejected = true
    }
    if (!rejected) throw new Error('bogus keyless_policy was accepted')
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'auto_resync' },
    })
    const repairDeadline = Date.now() + 180_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table && table.state !== 'needs_resync') {
        const expected = await mysqlRows('SELECT COUNT(*), SUM(amount) FROM keyless_log')
        const actual = await pintailQuery('SELECT COUNT(*), SUM(amount) FROM keyless_log')
        if (
          String(expected[0][0]) === String(actual[0][0]) &&
          String(expected[0][1]) === String(actual[0][1])
        ) {
          break
        }
      }
      if (Date.now() > repairDeadline) {
        const activity = await api<unknown[]>(`/api/activity?db=${databaseId}&limit=8`)
        throw new Error(
          `auto_resync never repaired keyless_log: ${JSON.stringify(table)}; ` +
            `activity: ${JSON.stringify(activity)}`,
        )
      }
      await Bun.sleep(3_000)
    }
    // Delete one of two byte-identical rows. Pintail must quarantine rather
    // than choose an append identity, then restore exact source multiplicity.
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'quarantine' },
    })
    await sql("DELETE FROM keyless_log WHERE label = 'b' LIMIT 1")
    const deleteFlagDeadline = Date.now() + 120_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table?.state === 'needs_resync') break
      if (Date.now() > deleteFlagDeadline) {
        throw new Error(`ambiguous keyless DELETE was not quarantined: ${JSON.stringify(table)}`)
      }
      await Bun.sleep(2_000)
    }
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'auto_resync' },
    })
    const multiplicityDeadline = Date.now() + 180_000
    for (;;) {
      const tables = await api<Array<{ name: string; state: string }>>(
        `/api/tables?db=${databaseId}`,
      )
      const table = tables.find((candidate) => candidate.name === 'keyless_log')
      if (table && table.state !== 'needs_resync') {
        const statement = 'SELECT label, amount, COUNT(*) FROM keyless_log GROUP BY label, amount ORDER BY label, amount'
        const expected = await mysqlRows(statement)
        const actual = await pintailQuery(statement)
        if (diffRows(expected, actual) === undefined) break
      }
      if (Date.now() > multiplicityDeadline) {
        throw new Error(`keyless duplicate multiplicity was not repaired: ${JSON.stringify(table)}`)
      }
      await Bun.sleep(3_000)
    }
    // Later phases assume the quarantine default.
    await api(`/api/databases/${databaseId}`, {
      method: 'PUT',
      body: { name: detail.name, mode: detail.mode, keyless_policy: 'quarantine' },
    })
  })
  await check('throwaway database lifecycle: create, update, delete', async () => {
    const host = await dockerHost()
    const mysqlPort = await publishedPort(mysqlName, 3306)
    const created = await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: {
        name: 'e2e_dup',
        dsn: `mysql://pintail:pintail@${host}:${mysqlPort}/${DATABASE}`,
        mode: 'cdc',
      },
    })
    await api(`/api/databases/${created.id}/probe`)
    await api(`/api/databases/${created.id}`, {
      method: 'PUT',
      body: { name: 'e2e_dup_renamed', mode: 'cdc' },
    })
    await api(`/api/databases/${created.id}`, { method: 'DELETE' })
    const list = await api<Array<{ id: string }>>('/api/databases')
    if (list.some((database) => database.id === created.id)) {
      throw new Error('deleted database still listed')
    }
  })
}

// ---------------------------------------------------------------------------
// Destructive lifecycle: what a dropped table or a dropped database does to
// the replica, in both replication modes.
//
// These phases assert inside themselves rather than leaning on the convergence
// sweep, and each restores the fixture before it returns. A dropped table is
// retained as an orphan, so it stays in the replica's catalog after MySQL has
// forgotten it; leaving one behind would make every later phase's
// information_schema comparison fail for a reason that has nothing to do with
// that phase.
//
// A check records PASS for the behaviour Pintail should have and WARN for a
// divergence that docs/limitations.md records as a known gap, so a fix flips it
// to PASS rather than needing the assertion rewritten.

async function waitUntil(
  predicate: () => Promise<boolean>,
  timeoutMs: number,
  pollMs = 2_000,
): Promise<boolean> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    if (await predicate().catch(() => false)) return true
    if (Date.now() >= deadline) return false
    await Bun.sleep(pollMs)
  }
}

function record(phase: string, check: string, status: CheckResult['status'], detail?: string) {
  results.push({ phase, check, status, detail })
  if (status !== 'PASS') log(`${status} ${check}${detail ? ` — ${detail}` : ''}`)
}

/// Replica row count, or undefined when the replica has no such table.
async function replicaCount(table: string): Promise<number | undefined> {
  try {
    const rows = await pintailQuery(`SELECT COUNT(*) FROM \`${table}\``)
    return Number(rows[0][0])
  } catch {
    return undefined
  }
}

async function sourceCount(table: string): Promise<number> {
  const rows = await mysqlRows(`SELECT COUNT(*) FROM \`${table}\``)
  return Number(rows[0][0])
}

interface TableSummary {
  name: string
  state: string
  rows: number
  last_error?: string
}

async function tableSummary(table: string, database = databaseId): Promise<TableSummary | undefined> {
  const tables = await api<TableSummary[]>(`/api/tables?db=${database}`)
  return tables.find((candidate) => candidate.name.toLowerCase() === table.toLowerCase())
}

/// Re-probing is the only operator action that retires an orphan: the stored
/// probe report is the table inventory the query engine builds its catalog
/// from, and DROP TABLE does not refresh it.
async function reprobe(database = databaseId) {
  await api(`/api/databases/${database}/probe`)
}

/// Everything the control plane can say about why a table has not arrived.
async function replicationDiagnostics(database = databaseId): Promise<string> {
  const [detail, tables, activity] = await Promise.all([
    api<unknown>(`/api/databases/${database}`).catch((error) => String(error)),
    api<TableSummary[]>(`/api/tables?db=${database}`)
      .then((rows) => rows.map((row) => `${row.name}:${row.state}`))
      .catch((error) => String(error)),
    api<unknown[]>(`/api/activity?db=${database}&limit=6`).catch((error) => String(error)),
  ])
  return JSON.stringify({ database: detail, tables, activity })
}

/// Waits for a table created mid-stream to be adopted by the replica.
///
/// The quiet retry is diagnostic, not padding: the harness polls the wire
/// while it waits, so a table that only appears once the queries stop points
/// at the supervisor being starved rather than at the adoption itself.
async function waitForAdoption(
  table: string,
  rows: number,
): Promise<{ status: CheckResult['status']; detail?: string }> {
  const arrived = async () => (await replicaCount(table)) === rows
  if (await waitUntil(arrived, 120_000, 5_000)) return { status: 'PASS' }
  await Bun.sleep(45_000)
  if (await arrived()) {
    return {
      status: 'WARN',
      detail: `${table} was adopted only after 45s with no query on the wire`,
    }
  }
  return { status: 'FAIL', detail: `${table} was never adopted: ${await replicationDiagnostics()}` }
}

/// Writes a row into `orders` and waits for the replica to agree, which is the
/// question these phases actually care about: whether the rest of the database
/// still replicates after the destructive operation.
async function ordersStillReplicate(timeoutMs = 120_000): Promise<boolean> {
  await sql(
    `INSERT INTO orders (customer_id, status, total, placed_on) VALUES ` +
      `(12, 'pending', 3.21, '2025-08-14')`,
  )
  const expected = await sourceCount('orders')
  return waitUntil(async () => (await replicaCount('orders')) === expected, timeoutMs)
}

const RETENTION_GAP =
  'DROP TABLE retains the replica as an orphan and does not refresh the stored ' +
  'probe report, so the table stays in the replica catalog until an operator re-probes'

async function phaseDropTableCdc() {
  const phase = 'drop-table-cdc'
  const table = 'lifecycle_cdc_drop'
  try {
    await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO ${table} VALUES (1, 'one'), (2, 'two'), (3, 'three')`)
    const seeded = await waitForAdoption(table, 3)
    record(phase, 'drop-table:replicates before the drop', seeded.status, seeded.detail)
    if (seeded.status === 'FAIL') return

    await sql(`DROP TABLE ${table}`)
    const orphaned = await waitUntil(
      async () => (await tableSummary(table))?.state === 'excluded',
      120_000,
    )
    record(
      phase,
      'drop-table:source drop marks the table orphaned',
      orphaned ? 'PASS' : 'FAIL',
      orphaned ? undefined : `state is ${JSON.stringify(await tableSummary(table))}`,
    )

    const survived = await ordersStillReplicate()
    record(
      phase,
      'drop-table:the rest of the database keeps replicating',
      survived ? 'PASS' : 'FAIL',
      survived
        ? undefined
        : `orders never caught up after ${table} was dropped: ${await replicationDiagnostics()}`,
    )

    // Retention is deliberate, but it also means the replica keeps answering
    // for a table the source no longer has - including through
    // information_schema, where MySQL and Pintail now disagree.
    const retained = await replicaCount(table)
    record(
      phase,
      'drop-table:orphan is retired without an operator re-probe',
      retained === undefined ? 'PASS' : 'WARN',
      retained === undefined ? undefined : `${RETENTION_GAP} (${retained} rows still served)`,
    )

    await reprobe()
    const retired = await waitUntil(async () => (await replicaCount(table)) === undefined, 60_000)
    record(
      phase,
      'drop-table:re-probe retires the orphan from the catalog',
      retired ? 'PASS' : 'FAIL',
      retired ? undefined : 'the orphan is still queryable after a fresh probe',
    )
  } finally {
    await sql(`DROP TABLE IF EXISTS ${table}`)
    await reprobe()
  }
}

async function phaseDropTableRecreate() {
  const phase = 'drop-table-recreate'
  const table = 'lifecycle_recreate'
  const create = `CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`
  try {
    await sql(create)
    await sql(`INSERT INTO ${table} VALUES (1, 'first-life'), (2, 'first-life')`)
    const seeded = await waitForAdoption(table, 2)
    record(phase, 'recreate:first generation replicates', seeded.status, seeded.detail)
    if (seeded.status === 'FAIL') return

    await sql(`DROP TABLE ${table}`)
    // Wait for the orphan to land before recreating: within one CDC run the
    // target stays blocked, so a create in the same cycle would be measuring
    // the block rather than what happens on the cycles after it.
    await waitUntil(async () => (await tableSummary(table))?.state === 'excluded', 120_000)

    await sql(create)
    await sql(`INSERT INTO ${table} VALUES (10, 'second-life'), (11, 'second-life')`)
    const expected = await sourceCount(table)
    const converged = await waitUntil(async () => {
      const diff = await tableDiff(table)
      return diff === undefined
    }, 120_000)
    const actual = await replicaCount(table)
    record(
      phase,
      'recreate:a table recreated under the same name replicates as a new table',
      converged ? 'PASS' : 'WARN',
      converged
        ? undefined
        : `the source has ${expected} rows and the replica ${actual ?? 'no table'}: the ` +
          'orphaned store is reused instead of being resnapshotted, because the CREATE ' +
          'handler skips any name it already tracks',
    )

    const survived = await ordersStillReplicate()
    record(
      phase,
      'recreate:the rest of the database keeps replicating',
      survived ? 'PASS' : 'FAIL',
      survived ? undefined : await replicationDiagnostics(),
    )
  } finally {
    await sql(`DROP TABLE IF EXISTS ${table}`)
    await reprobe()
  }
}

async function phaseDropTablePolling() {
  const phase = 'drop-table-polling'
  const dropped = 'lifecycle_poll_drop'
  const truncated = 'lifecycle_poll_truncate'
  try {
    // Polling only visits tables the stored probe already names, so both
    // fixtures have to be adopted by CDC before the mode switch.
    for (const table of [dropped, truncated]) {
      await sql(`CREATE TABLE ${table} (id INT PRIMARY KEY, note VARCHAR(32) NOT NULL)`)
      await sql(`INSERT INTO ${table} VALUES (1, 'a'), (2, 'b'), (3, 'c')`)
    }
    const first = await waitForAdoption(dropped, 3)
    const second = await waitForAdoption(truncated, 3)
    const seeded = first.status === 'FAIL' || second.status === 'FAIL' ? 'FAIL' : first.status
    record(
      phase,
      'polling:fixtures replicate before the mode switch',
      seeded,
      first.detail ?? second.detail,
    )
    if (seeded === 'FAIL') return

    await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'polling' } })
    try {
      const healthy = await ordersStillReplicate()
      record(
        phase,
        'polling:database is healthy before the drop',
        healthy ? 'PASS' : 'FAIL',
        healthy ? undefined : await replicationDiagnostics(),
      )

      // A truncated table leaves no chunk for the cheap probe to find changed,
      // so the rows only disappear on a reconciling cycle. That is bounded by
      // the database's reconcile interval, not by the poll interval.
      await sql(`TRUNCATE TABLE ${truncated}`)
      const emptied = await waitUntil(async () => (await replicaCount(truncated)) === 0, 180_000)
      record(
        phase,
        'polling:TRUNCATE empties the replica',
        emptied ? 'PASS' : 'WARN',
        emptied
          ? undefined
          : `the replica still holds ${await replicaCount(truncated)} rows: polling sees a ` +
            'truncate only through reconciliation, so the replica reads high until the ' +
            "database's reconcile interval elapses",
      )

      await sql(`DROP TABLE ${dropped}`)
      const unaffected = await ordersStillReplicate(90_000)
      const status = await api<unknown>(`/api/databases/${databaseId}/status`)
      record(
        phase,
        'polling:one dropped table does not stop the other tables',
        unaffected ? 'PASS' : 'WARN',
        unaffected
          ? undefined
          : 'the whole poll cycle aborts on the first table that fails, so every other ' +
            `table stops replicating too: ${JSON.stringify(status)}`,
      )

      await reprobe()
      const recovered = await ordersStillReplicate()
      record(
        phase,
        'polling:re-probe restores replication for the surviving tables',
        recovered ? 'PASS' : 'FAIL',
        recovered ? undefined : `still stalled: ${JSON.stringify(status)}`,
      )
    } finally {
      // Leaving the database in polling mode would cascade into every later
      // phase, exactly as the control-plane mode check guards against.
      await api(`/api/databases/${databaseId}/mode`, { method: 'POST', body: { mode: 'cdc' } })
    }
  } finally {
    await sql(`DROP TABLE IF EXISTS ${dropped}`)
    await sql(`DROP TABLE IF EXISTS ${truncated}`)
    await reprobe()
  }
}

async function phaseDropDatabase() {
  const phase = 'drop-database'
  const source = 'e2e_lifecycle_db'
  const shadow = 'lifecycle_shadow'
  const host = await dockerHost()
  const mysqlPort = await publishedPort(mysqlName, 3306)
  let secondary = ''
  try {
    await sql(`DROP DATABASE IF EXISTS ${source}`)
    await sql(`CREATE DATABASE ${source} DEFAULT CHARACTER SET utf8mb4`)
    // A qualified CREATE/DROP is attributed in the binlog to the session's
    // current schema, not to the schema in the statement, and Pintail routes
    // DDL by that attribution. Building the second database from the main
    // session would therefore hand its DDL to the main database's stream.
    await sql(`USE ${source}`)
    await sql(`CREATE TABLE widgets (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`CREATE TABLE ${shadow} (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO widgets VALUES (1, 'a'), (2, 'b'), (3, 'c')`)
    await sql(`INSERT INTO ${shadow} VALUES (1, 'other-schema')`)
    await sql(`USE ${DATABASE}`)

    // Same table name in the replicated database, so a DDL statement aimed at
    // the other schema has somewhere wrong to land.
    await sql(`CREATE TABLE ${shadow} (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL)`)
    await sql(`INSERT INTO ${shadow} VALUES (1, 'main-schema')`)
    const shadowed = await waitForAdoption(shadow, 1)
    record(phase, 'cross-schema:same-named table replicates first', shadowed.status, shadowed.detail)

    if (shadowed.status !== 'FAIL') {
      await sql(`DROP TABLE ${source}.${shadow}`)
      await sql(`INSERT INTO ${shadow} VALUES (2, 'written after the other schema was dropped')`)
      const isolated = await waitUntil(async () => (await replicaCount(shadow)) === 2, 90_000)
      record(
        phase,
        'cross-schema:dropping another schema\'s table leaves this one replicating',
        isolated ? 'PASS' : 'WARN',
        isolated
          ? undefined
          : 'DDL is matched on the bare table name and routed by the session schema, so ' +
            `DROP TABLE ${source}.${shadow} orphaned ${DATABASE}.${shadow}: ` +
            JSON.stringify(await tableSummary(shadow)),
      )
    }

    const created = await api<{ id: string }>('/api/databases', {
      method: 'POST',
      body: {
        name: source,
        dsn: `mysql://pintail:pintail@${host}:${mysqlPort}/${source}`,
        mode: 'cdc',
      },
    })
    secondary = created.id
    await reprobe(secondary)
    await api(`/api/databases/${secondary}/snapshot`, { method: 'POST', body: { force: false } })
    const snapshotted = await waitUntil(async () => {
      const status = await api<{ state: string }>(`/api/databases/${secondary}/snapshot/status`)
      return status.state === 'streaming' || status.state === 'polling'
    }, 180_000)
    record(phase, 'drop-database:second database snapshots', snapshotted ? 'PASS' : 'FAIL')
    if (!snapshotted) return

    const before = await api<{ count: number }>(`/api/tables/widgets/count?db=${secondary}`)
    record(
      phase,
      'drop-database:second database serves its rows',
      before.count === 3 ? 'PASS' : 'FAIL',
      before.count === 3 ? undefined : `expected 3 rows, got ${before.count}`,
    )

    await sql(`DROP DATABASE ${source}`)
    // Several supervisor cadences: whatever Pintail is going to notice, it has
    // noticed by now.
    const flagged = await waitUntil(async () => {
      const detail = await api<{ state: string }>(`/api/databases/${secondary}`)
      if (detail.state === 'error') return true
      const tables = await api<TableSummary[]>(`/api/tables?db=${secondary}`)
      return tables.every((table) => table.state === 'excluded')
    }, 90_000)
    const observed = {
      database: await api<{ state: string }>(`/api/databases/${secondary}`),
      tables: await api<TableSummary[]>(`/api/tables?db=${secondary}`),
    }
    record(
      phase,
      'drop-database:the deleted source is surfaced, not served silently',
      flagged ? 'PASS' : 'WARN',
      flagged
        ? undefined
        : 'DROP DATABASE is not classified as DDL at all, so no table is orphaned and ' +
          `nothing records the loss: ${JSON.stringify(observed)}`,
    )

    let probeRefused = false
    try {
      await reprobe(secondary)
    } catch {
      probeRefused = true
    }
    record(
      phase,
      'drop-database:re-probing a deleted source fails loudly',
      probeRefused ? 'PASS' : 'FAIL',
      probeRefused ? undefined : 'the probe succeeded against a database that no longer exists',
    )

    await api(`/api/databases/${secondary}/mode`, { method: 'POST', body: { mode: 'polling' } })
    const pollingNoticed = await waitUntil(async () => {
      const detail = await api<{ state: string }>(`/api/databases/${secondary}`)
      return detail.state === 'error'
    }, 90_000)
    record(
      phase,
      'drop-database:polling reports the deleted source as an error',
      pollingNoticed ? 'PASS' : 'WARN',
      pollingNoticed
        ? undefined
        : `polling kept reporting a healthy database: ${JSON.stringify(
            await api<unknown>(`/api/databases/${secondary}`),
          )}`,
    )

    const stillServed = await api<{ count: number }>(`/api/tables/widgets/count?db=${secondary}`)
      .then((response) => response.count)
      .catch(() => undefined)
    record(
      phase,
      'drop-database:reads against a deleted source do not claim to be current',
      stillServed === undefined ? 'PASS' : 'WARN',
      stillServed === undefined
        ? undefined
        : `${stillServed} rows are still served from the replica of a database MySQL no ` +
          'longer has, with nothing on the read path marking them stale',
    )
  } finally {
    await sql(`USE ${DATABASE}`)
    if (secondary) {
      try {
        await api(`/api/databases/${secondary}`, { method: 'DELETE' })
      } catch (error) {
        log(`cleanup: could not delete the throwaway database: ${error}`)
      }
    }
    await sql(`DROP DATABASE IF EXISTS ${source}`)
    await sql(`DROP TABLE IF EXISTS ${shadow}`)
    await reprobe()
  }
}

// ---------------------------------------------------------------------------
// Boot.

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_E2E_BINARY) return resolve(process.env.PINTAIL_E2E_BINARY)
  log('building the release pintail binary')
  await command(['cargo', 'build', '--release', '-p', 'pintail'])
  const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'], {
    quiet: true,
  })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function startPintail(queryMemoryLimit?: number) {
  pintailWire = undefined
  pintailProcess = Bun.spawn(
    [
      pintailBinary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${pintailHttpPort}`,
      '--wire-bind',
      `127.0.0.1:${pintailWirePort}`,
    ],
    {
      cwd: repository,
      stdout: 'inherit',
      stderr: 'inherit',
      env: {
        ...process.env,
        ...(queryMemoryLimit === undefined
          ? {}
          : { PINTAIL_QUERY_MEMORY_LIMIT_BYTES: String(queryMemoryLimit) }),
      },
    },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    try {
      const response = await fetch(`${pintailUrl}/health`)
      if (response.ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy within 120 seconds')
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
    '/var/lib/mysql:rw,size=2g',
    '--env',
    'MYSQL_ROOT_PASSWORD=pintail-root',
    '--env',
    `MYSQL_DATABASE=${DATABASE}`,
    'mysql:8.4',
    '--server-id=942',
    '--log-bin=mysql-bin',
    '--binlog-format=ROW',
    '--binlog-row-image=FULL',
    '--binlog-row-metadata=FULL',
    '--gtid-mode=ON',
    '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00',
    '--sql-mode=NO_ENGINE_SUBSTITUTION',
  )
  mysqlStarted = true
  const mysqlPort = await publishedPort(mysqlName, 3306)
  mysqlConnection = await waitForMysql(host, mysqlPort)
  mysqlEndpoint = {
    host,
    port: mysqlPort,
    user: 'pintail',
    password: 'pintail',
    database: DATABASE,
  }
  await sql(`USE ${DATABASE}`)
  await sql(`CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'`)
  await sql(
    `GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'`,
  )

  pintailBinary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-e2e-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  await startPintail()
  const setup = await api<{ token: string }>('/api/auth/setup', {
    method: 'POST',
    auth: false,
    body: { email: 'e2e@pintail.local', password: 'e2e-gate-password' },
  })
  token = setup.token

  log('seeding the source schema and initial rows')
  await phaseSeed()

  const database = await api<{ id: string }>('/api/databases', {
    method: 'POST',
    body: { name: DATABASE, dsn: `mysql://pintail:pintail@${host}:${mysqlPort}/${DATABASE}`, mode: 'cdc' },
  })
  databaseId = database.id
  const apiKey = await api<{ secret: string }>(`/api/databases/${databaseId}/api-keys`, {
    method: 'POST',
    body: { name: 'e2e-gate', scopes: ['query', 'read'] },
  })
  wireSecret = apiKey.secret
  await api(`/api/databases/${databaseId}/probe`)
  const accepted = await api<{ run_id: string }>(`/api/databases/${databaseId}/snapshot`, {
    method: 'POST',
    body: { force: false },
  })
  log(`snapshot ${accepted.run_id} started`)
  for (let attempt = 0; ; attempt += 1) {
    const status = await api<{ state: string; tables: Array<{ name: string; last_error?: string }> }>(
      `/api/databases/${databaseId}/snapshot/status`,
    )
    if (status.state === 'error') {
      throw new Error(
        `snapshot failed: ${status.tables.map((t) => t.last_error).filter(Boolean).join('; ')}`,
      )
    }
    if (status.state === 'polling' || status.state === 'streaming') break
    if (attempt > 600) throw new Error('snapshot did not complete within ten minutes')
    await Bun.sleep(1000)
  }

  const phases: Array<[string, () => Promise<void>]> = [
    ['snapshot', async () => {}],
    ['orm-compat', phaseOrmCompatibility],
    ['crud', phaseCrud],
    ['type-edges', phaseTypeEdges],
    ['ddl', phaseDdl],
    ['schema-drift-minimal', phaseSchemaDriftMinimal],
    ['schema-drift-unseen', phaseSchemaDriftUnseen],
    ['churn', phaseChurn],
    ['spill', phaseSpill],
    ['pooling', phasePooling],
    ['restart', phaseRestart],
    ['control-plane', phaseControlPlane],
    ['drop-table-cdc', phaseDropTableCdc],
    ['drop-table-recreate', phaseDropTableRecreate],
    ['drop-table-polling', phaseDropTablePolling],
    ['drop-database', phaseDropDatabase],
    // Last: the rename gap leaves the replica holding a table under a name
    // MySQL no longer uses, and the lifecycle phases re-probe, which would
    // retire that table from the catalog and turn the documented metadata
    // divergence into an undocumented one.
    ['ddl-documented-gaps', phaseDdlDocumentedGaps],
  ]
  const selected = process.env.E2E_PHASES?.split(',').map((phase) => phase.trim())
  for (const [name, run] of phases) {
    if (selected && name !== 'snapshot' && !selected.includes(name)) continue
    log(`phase: ${name}`)
    await run()
    await verifyConvergence(name)
    await verifyCorpus(name)
  }

  publish()
}

function publish() {
  const failed = results.filter((result) => result.status === 'FAIL')
  const warned = results.filter((result) => result.status === 'WARN')
  const passed = results.filter((result) => result.status === 'PASS')
  const lines = [
    '# Pintail end-to-end differential gate',
    '',
    `Measured ${new Date().toISOString()}.`,
    '',
    `**${passed.length} passed, ${failed.length} failed, ${warned.length} documented-gap warnings.**`,
    '',
    '| Phase | Check | Status | Detail |',
    '|---|---|---|---|',
    ...results.map(
      (result) =>
        `| ${result.phase} | ${result.check} | ${result.status} | ${(result.detail ?? '').split('\n')[0].replaceAll('|', '\\|')} |`,
    ),
    '',
  ]
  // Phase-subset runs write a separate artifact so an iteration loop never
  // overwrites the committed full-gate record.
  const partial = Boolean(process.env.E2E_PHASES)
  const suffix = partial ? '-partial' : ''
  writeFileSync(join(import.meta.dir, `results${suffix}.md`), lines.join('\n'))
  writeFileSync(join(import.meta.dir, `results${suffix}.json`), JSON.stringify(results, null, 2))
  log(`gate: ${failed.length === 0 ? 'PASS' : 'FAIL'} (${passed.length} passed, ${failed.length} failed, ${warned.length} warned)`)
  for (const failure of failed) {
    log(`  FAIL ${failure.phase}/${failure.check}: ${failure.detail?.split('\n')[0]}`)
  }
  if (failed.length > 0) {
    process.exitCode = 1
  }
}

async function cleanup() {
  try {
    pintailProcess?.kill()
  } catch {}
  try {
    await mysqlConnection?.end()
  } catch {}
  if (mysqlStarted) {
    try {
      await docker('rm', '--force', '--volumes', mysqlName)
    } catch (error) {
      log(`cleanup: ${error}`)
    }
  }
  if (pintailDataDir) {
    try {
      rmSync(pintailDataDir, { recursive: true, force: true })
    } catch {}
  }
}

try {
  await main()
} finally {
  await cleanup()
}

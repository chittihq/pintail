/// Change capture against generated workloads, across source versions and
/// binlog settings.
///
/// Each leg starts a source with one configuration - server major, GTID on or
/// off, row metadata, row image, transaction compression - and a Pintail
/// mirroring a schema of typed tables: every storage type at its edges, a
/// composite key, a secondary unique key and no key at all. A seeded
/// generator then plays rounds of transactions against the source - inserts,
/// updates that keep or move the key, deletes, multi-table and oversized
/// transactions - with schema changes and Pintail restarts between rounds.
/// After every round each table's full contents are compared byte for byte
/// through the MySQL protocol until they converge or the round's budget runs
/// out. The oracle is the source itself.
///
/// Run:     bun run cdc-matrix.ts
///          CDC_MATRIX_LEGS=mysql84-gtid,mariadb114 CDC_MATRIX_ROUNDS=40 CDC_MATRIX_SEED=7 bun run cdc-matrix.ts
/// Binary:  PINTAIL_CDC_MATRIX_BINARY=../../target/release/pintail (built otherwise)

import { mkdtempSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { command, docker, dockerHost, dsnHost, freePort, publishedPort } from './lib.ts'

const repository = resolve(import.meta.dir, '..', '..')
const ROUNDS = Number(process.env.CDC_MATRIX_ROUNDS ?? '24')
const SEED = Number(process.env.CDC_MATRIX_SEED ?? '1')
const CONVERGE_MS = Number(process.env.CDC_MATRIX_CONVERGE_MS ?? '90000')
const DATABASE = 'matrix'

interface Leg {
  name: string
  image: string
  flavor: 'mysql' | 'mariadb'
  args: string[]
  /// Pintail streams this leg; otherwise the probe demotes it to polling.
  cdc: boolean
  collation: string
}

const MYSQL_BASE = ['--server-id=971', '--log-bin=mysql-bin', '--binlog-format=ROW', '--default-time-zone=+00:00']
const LEGS: Leg[] = [
  { name: 'mysql84-gtid', image: 'mysql:8.4', flavor: 'mysql', cdc: true, collation: 'utf8mb4_0900_ai_ci',
    args: [...MYSQL_BASE, '--gtid-mode=ON', '--enforce-gtid-consistency=ON', '--binlog-row-image=FULL', '--binlog-row-metadata=MINIMAL'] },
  { name: 'mysql84-filepos-full-metadata', image: 'mysql:8.4', flavor: 'mysql', cdc: true, collation: 'utf8mb4_0900_ai_ci',
    args: [...MYSQL_BASE, '--binlog-row-image=FULL', '--binlog-row-metadata=FULL'] },
  { name: 'mysql84-compressed', image: 'mysql:8.4', flavor: 'mysql', cdc: true, collation: 'utf8mb4_0900_ai_ci',
    args: [...MYSQL_BASE, '--gtid-mode=ON', '--enforce-gtid-consistency=ON', '--binlog-row-image=FULL', '--binlog-transaction-compression=ON'] },
  { name: 'mysql84-minimal-image', image: 'mysql:8.4', flavor: 'mysql', cdc: false, collation: 'utf8mb4_0900_ai_ci',
    args: [...MYSQL_BASE, '--binlog-row-image=MINIMAL'] },
  { name: 'mysql80-gtid', image: 'mysql:8.0', flavor: 'mysql', cdc: true, collation: 'utf8mb4_0900_ai_ci',
    args: [...MYSQL_BASE, '--gtid-mode=ON', '--enforce-gtid-consistency=ON', '--binlog-row-image=FULL'] },
  { name: 'mysql57', image: 'mysql:5.7', flavor: 'mysql', cdc: true, collation: 'utf8mb4_general_ci',
    args: [...MYSQL_BASE, '--binlog-row-image=FULL', '--character-set-server=utf8mb4', '--collation-server=utf8mb4_general_ci'] },
  { name: 'mariadb114', image: 'mariadb:11.4', flavor: 'mariadb', cdc: true, collation: 'utf8mb4_general_ci',
    args: ['--server-id=972', '--log-bin=mysql-bin', '--binlog-format=ROW', '--binlog-row-image=FULL', '--default-time-zone=+00:00'] },
  { name: 'mariadb106', image: 'mariadb:10.6', flavor: 'mariadb', cdc: true, collation: 'utf8mb4_general_ci',
    args: ['--server-id=973', '--log-bin=mysql-bin', '--binlog-format=ROW', '--binlog-row-image=FULL', '--default-time-zone=+00:00'] },
]

function log(message: string) {
  console.log(`[cdc-matrix] ${message}`)
}

// ---------------------------------------------------------------------------
// Deterministic generation

class Random {
  constructor(private state: number) {
    this.state = (state * 2654435761) >>> 0 || 1
  }
  next(): number {
    let x = this.state
    x ^= x << 13
    x ^= x >>> 17
    x ^= x << 5
    this.state = x >>> 0
    return this.state / 4294967296
  }
  below(n: number): number {
    return Math.floor(this.next() * n)
  }
  chance(p: number): boolean {
    return this.next() < p
  }
  pick<T>(items: readonly T[]): T {
    return items[this.below(items.length)]!
  }
}

const q = (value: string) => `'${value.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`

const STRINGS = ['', 'a', 'A', 'ä', 'Ä', 'trailing  ', ' leading', 'ß', 'ss', '😀 emoji', "quote ' mark", 'back\\slash', '日本語', 'x'.repeat(40)]
const DECIMALS = ['0', '-0.000001', '99999999999999.999999', '-99999999999999.999999', '1.5', '-2.25', '0.000001', '123456.789']
const DOUBLES = ['0', '1.5', '-1e300', '1e-300', '3.141592653589793', '-0.1', '12345678901234567']
const DATETIMES = ['1000-01-01 00:00:00', '9999-12-31 23:59:59.999999', '2020-02-29 12:34:56.5', '1970-01-01 00:00:01', '2038-01-19 03:14:07.999999']
const TIMESTAMPS = ['1970-01-01 00:00:01', '2038-01-19 03:14:07.999', '2024-06-30 23:59:59.5']
const TIMES = ['-838:59:59', '838:59:59', '00:00:00', '-00:00:01.5', '12:30:00.25']
const DATES = ['1000-01-01', '9999-12-31', '2000-02-29', '1999-12-31']
const JSONS = ['{}', '[]', '{"a": 1, "b": [true, null, "x"]}', '{"nested": {"deep": {"value": 1.25}}}', '"string"', '12345678901234567890', '{"unicode": "\\u00e9\\u4e2d"}']

interface Table {
  name: string
  keyColumns: string[]
  /// Generates a full row: column list and SQL literals.
  row(random: Random, key: number[]): Record<string, string>
  keyless: boolean
}

function nullable(random: Random, literal: string): string {
  return random.chance(0.12) ? 'NULL' : literal
}

function tables(leg: Leg): { ddl: string[]; tables: Table[] } {
  const json = leg.flavor === 'mariadb' ? 'LONGTEXT' : 'JSON'
  const ddl = [
    `CREATE TABLE typed (
      id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
      i INT, u INT UNSIGNED, big BIGINT, ubig BIGINT UNSIGNED,
      d DECIMAL(20,6), f DOUBLE, fl FLOAT,
      dt DATETIME(6), ts TIMESTAMP(3) NULL, tm TIME(2), dte DATE, yr YEAR,
      s VARCHAR(48) COLLATE ${leg.collation}, c CHAR(8) COLLATE ${leg.collation},
      b VARBINARY(16), j ${json}, e ENUM('red','green','blue',''), st SET('x','y','z'),
      bt BIT(5), tx TEXT, bl BLOB
    ) DEFAULT CHARSET=utf8mb4 COLLATE=${leg.collation}`,
    `CREATE TABLE pairs (a INT NOT NULL, b VARCHAR(12) NOT NULL, v INT, note VARCHAR(20), PRIMARY KEY (a, b)) DEFAULT CHARSET=utf8mb4 COLLATE=${leg.collation}`,
    `CREATE TABLE uniq (code INT NOT NULL, label VARCHAR(16), UNIQUE KEY code_key (code)) DEFAULT CHARSET=utf8mb4 COLLATE=${leg.collation}`,
    `CREATE TABLE heap (x INT, y VARCHAR(10)) DEFAULT CHARSET=utf8mb4 COLLATE=${leg.collation}`,
  ]
  const typed: Table = {
    name: 'typed', keyColumns: ['id'], keyless: false,
    row: (r, [id]) => ({
      id: String(id),
      i: nullable(r, String(r.pick([0, -1, 2147483647, -2147483648, 42]))),
      u: nullable(r, String(r.pick([0, 4294967295, 7]))),
      big: nullable(r, String(r.pick(['-9223372036854775808', '9223372036854775807', '0', '-5']))),
      ubig: nullable(r, String(r.pick(['18446744073709551615', '0', '9007199254740993']))),
      d: nullable(r, r.pick(DECIMALS)),
      f: nullable(r, r.pick(DOUBLES)),
      fl: nullable(r, r.pick(['0', '1.5', '-3.25', '100.125'])),
      dt: nullable(r, q(r.pick(DATETIMES))),
      ts: nullable(r, q(r.pick(TIMESTAMPS))),
      tm: nullable(r, q(r.pick(TIMES))),
      dte: nullable(r, q(r.pick(DATES))),
      yr: nullable(r, String(r.pick([1901, 2155, 2000]))),
      s: nullable(r, q(r.pick(STRINGS))),
      c: nullable(r, q(r.pick(['', 'ab', 'Ab ', 'ç']))),
      b: nullable(r, r.pick(["X''", "X'00FF'", "X'DEADBEEF00'", "'text'"])),
      j: nullable(r, q(r.pick(JSONS))),
      e: nullable(r, q(r.pick(['red', 'green', 'blue', '']))),
      st: nullable(r, q(r.pick(['', 'x', 'x,z', 'x,y,z']))),
      bt: nullable(r, r.pick(["b'0'", "b'11111'", "b'101'"])),
      tx: nullable(r, q(r.chance(0.1) ? 'long '.repeat(4000) : r.pick(STRINGS))),
      bl: nullable(r, r.pick(["X''", "X'0102030405'", `X'${'AB'.repeat(r.below(3) * 2000)}'`])),
    }),
  }
  const pairs: Table = {
    name: 'pairs', keyColumns: ['a', 'b'], keyless: false,
    row: (r, [a, b]) => ({ a: String(a), b: q(['k', 'm', 'kk', 'z'][b!]!), v: nullable(r, String(r.below(1000) - 500)), note: nullable(r, q(r.pick(STRINGS).slice(0, 20))) }),
  }
  const uniq: Table = {
    name: 'uniq', keyColumns: ['code'], keyless: false,
    row: (r, [code]) => ({ code: String(code), label: nullable(r, q(r.pick(STRINGS).slice(0, 16))) }),
  }
  const heap: Table = {
    name: 'heap', keyColumns: [], keyless: true,
    row: (r) => ({ x: nullable(r, String(r.below(10))), y: nullable(r, q(r.pick(['a', 'b', '']))) }),
  }
  return { ddl, tables: [typed, pairs, uniq, heap] }
}

// ---------------------------------------------------------------------------
// Source and mirror

async function textConnection(options: mysql.ConnectionOptions) {
  return mysql.createConnection({
    ...options,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
    typeCast: (field) => {
      const buffer = field.buffer()
      if (buffer === null) return null
      // The mirror serves BIT as an unsigned integer (docs/limitations.md);
      // the source's bit string compares as the same number.
      if (field.type === 'BIT') return BigInt(`0x${buffer.toString('hex') || '0'}`).toString()
      return field.charsetNr === 63 && !['LONGLONG', 'LONG', 'NEWDECIMAL', 'DOUBLE', 'FLOAT', 'TINY', 'SHORT', 'INT24', 'YEAR', 'DATE', 'DATETIME', 'TIMESTAMP', 'TIME', 'JSON'].includes(field.type)
        ? `0x${buffer.toString('hex')}`
        : buffer.toString('utf8')
    },
  })
}

class Mirror {
  process: ReturnType<typeof Bun.spawn> | undefined
  url = ''
  wirePort = 0
  token = ''
  constructor(readonly binary: string, readonly dataDir: string) {}

  async start() {
    const http = await freePort()
    this.wirePort = await freePort()
    this.url = `http://127.0.0.1:${http}`
    this.process = Bun.spawn([this.binary, '--data-dir', this.dataDir, '--http-bind', `127.0.0.1:${http}`, '--wire-bind', `127.0.0.1:${this.wirePort}`], {
      cwd: repository,
      stdout: 'ignore',
      stderr: 'ignore',
      env: { ...process.env, PINTAIL_LOG: 'error', PINTAIL_SUPERVISOR_INTERVAL_MS: '200' },
    })
    for (let attempt = 0; attempt < 240; attempt += 1) {
      try {
        if ((await fetch(`${this.url}/health`)).ok) return
      } catch {}
      await Bun.sleep(250)
    }
    throw new Error('pintail did not become healthy')
  }

  async stop() {
    this.process?.kill('SIGKILL')
    await this.process?.exited
    this.process = undefined
  }

  async api<T>(path: string, method = 'GET', body?: unknown): Promise<T> {
    const response = await fetch(`${this.url}${path}`, {
      method,
      headers: { 'content-type': 'application/json', ...(this.token ? { authorization: `Bearer ${this.token}` } : {}) },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
    const text = await response.text()
    if (!response.ok) throw new Error(`${method} ${path} -> ${response.status}: ${text}`)
    return (text ? JSON.parse(text) : undefined) as T
  }
}

async function dump(connection: mysql.Connection, table: string): Promise<string[]> {
  const [fields] = await connection.query<mysql.RowDataPacket[]>({ sql: `SELECT * FROM \`${table}\` LIMIT 0` })
  void fields
  const [rows] = await connection.query<mysql.RowDataPacket[][]>({ sql: `SELECT * FROM \`${table}\``, rowsAsArray: true })
  return (rows as unknown as (string | null)[][]).map((row) => row.map((value) => (value === null ? '\\N' : value)).join('\t')).sort()
}

function firstDifference(expected: string[], actual: string[]): string {
  if (expected.length !== actual.length) return `${expected.length} source rows, ${actual.length} mirrored`
  const index = expected.findIndex((row, i) => row !== actual[i])
  return `row ${index}:\n    source  ${expected[index]?.slice(0, 400)}\n    mirror  ${actual[index]?.slice(0, 400)}`
}

// ---------------------------------------------------------------------------
// One leg

interface LegResult {
  leg: string
  mode: string
  rounds: number
  transactions: number
  statements: number
  schemaChanges: number
  restarts: number
  divergences: string[]
  error?: string
}

async function runLeg(leg: Leg, binary: string): Promise<LegResult> {
  const result: LegResult = { leg: leg.name, mode: '', rounds: 0, transactions: 0, statements: 0, schemaChanges: 0, restarts: 0, divergences: [] }
  const random = new Random(SEED * 7919 + leg.name.length)
  const container = `pintail-e2e-cdc-matrix-${leg.name}-${process.pid}`
  const host = await dockerHost()
  const dataDir = mkdtempSync(join(tmpdir(), 'pintail-cdc-matrix-'))
  const mirror = new Mirror(binary, dataDir)
  let source: mysql.Connection | undefined
  let replica: mysql.Connection | undefined
  try {
    await docker('rm', '--force', container).catch(() => {})
    await docker('run', '--detach', '--name', container, '--publish', '0:3306', '--tmpfs', '/var/lib/mysql:rw,size=3g',
      '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', '--env', 'MARIADB_ROOT_PASSWORD=pintail-root', leg.image, ...leg.args)
    const port = await publishedPort(container, 3306)
    for (let attempt = 0; ; attempt += 1) {
      try {
        source = await textConnection({ host, port, user: 'root', password: 'pintail-root', multipleStatements: false })
        await source.query('SELECT 1')
        break
      } catch {
        if (attempt > 360) throw new Error(`${leg.image} did not become ready`)
        await Bun.sleep(500)
      }
    }
    await source.query(`CREATE DATABASE ${DATABASE}`)
    await source.query(`USE ${DATABASE}`)
    const schema = tables(leg)
    for (const statement of schema.ddl) await source.query(statement)
    const live = new Map(schema.tables.map((table) => [table.name, table]))
    const keys = new Map<string, Set<string>>(schema.tables.map((table) => [table.name, new Set()]))

    await mirror.start()
    mirror.token = (await mirror.api<{ token: string }>('/api/auth/setup', 'POST', { email: 'matrix@pintail.local', password: 'matrix-gate-password' })).token
    const database = await mirror.api<{ id: string }>('/api/databases', 'POST', {
      name: DATABASE,
      dsn: `mysql://root:pintail-root@${dsnHost(host)}:${port}/${DATABASE}`,
      mode: 'auto',
      keyless_policy: 'auto_resync',
      poll_interval_seconds: 1,
      reconcile_interval_seconds: 2,
    })
    const probe = await mirror.api<{ recommended_mode?: string; mode?: string }>(`/api/databases/${database.id}/probe`)
    result.mode = String(probe.recommended_mode ?? probe.mode ?? '?')
    await mirror.api(`/api/databases/${database.id}/snapshot`, 'POST', { force: false })
    const key = await mirror.api<{ secret: string }>(`/api/databases/${database.id}/api-keys`, 'POST', { name: 'matrix', scopes: ['query', 'read'] })
    const connectReplica = async () => {
      await replica?.end().catch(() => {})
      replica = await textConnection({ host: '127.0.0.1', port: mirror.wirePort, user: DATABASE, password: key.secret, database: DATABASE })
    }

    const execute = async (sql: string) => {
      result.statements += 1
      await source!.query(sql)
    }

    const transaction = async () => {
      const oversized = random.chance(0.04)
      const count = oversized ? 2000 + random.below(3000) : 1 + random.below(12)
      await execute('START TRANSACTION')
      for (let n = 0; n < count; n += 1) {
        const table = random.pick([...live.values()])
        const present = keys.get(table.name)!
        const roll = random.next()
        if (table.keyless) {
          if (roll < 0.97 || oversized) {
            const row = table.row(random, [])
            await execute(`INSERT INTO \`${table.name}\` (${Object.keys(row).join(',')}) VALUES (${Object.values(row).join(',')})`)
          } else {
            // A keyless change the stream cannot follow: the table resyncs.
            await execute(`DELETE FROM \`${table.name}\` WHERE x = ${random.below(10)} LIMIT 1`)
          }
          continue
        }
        const keyParts = () => (table.name === 'pairs' ? [random.below(20), random.below(4)] : [random.below(table.name === 'typed' ? 400 : 60)])
        const where = (parts: number[]) => {
          const row = table.row(random, parts)
          return table.keyColumns.map((column) => `\`${column}\` = ${row[column]}`).join(' AND ')
        }
        const parts = keyParts()
        const identity = parts.join(':')
        if (!present.has(identity) || roll < 0.45) {
          if (present.has(identity)) continue
          const row = table.row(random, parts)
          await execute(`INSERT INTO \`${table.name}\` (${Object.keys(row).join(',')}) VALUES (${Object.values(row).join(',')})`)
          present.add(identity)
        } else if (roll < 0.8) {
          const moveTo = random.chance(0.15) ? keyParts() : parts
          const target = moveTo.join(':')
          if (target !== identity && present.has(target)) continue
          const row = table.row(random, moveTo)
          const assignments = Object.entries(row).map(([column, value]) => `\`${column}\` = ${value}`).join(', ')
          await execute(`UPDATE \`${table.name}\` SET ${assignments} WHERE ${where(parts)}`)
          present.delete(identity)
          present.add(target)
        } else {
          await execute(`DELETE FROM \`${table.name}\` WHERE ${where(parts)}`)
          present.delete(identity)
        }
      }
      await execute('COMMIT')
      result.transactions += 1
    }

    const schemaChange = async () => {
      result.schemaChanges += 1
      const choice = random.below(6)
      const added = `added_${result.schemaChanges}`
      switch (choice) {
        case 0:
          await execute(`ALTER TABLE pairs ADD COLUMN ${added} VARCHAR(8) NULL`)
          break
        case 1:
          await execute('ALTER TABLE uniq MODIFY label VARCHAR(64)')
          break
        case 2:
          await execute('ALTER TABLE typed FORCE')
          break
        case 3:
          await execute('TRUNCATE TABLE uniq')
          keys.get('uniq')!.clear()
          break
        case 4: {
          await execute('CREATE TABLE heap_next (x INT, y VARCHAR(10))')
          await execute('INSERT INTO heap_next SELECT * FROM heap')
          await execute('RENAME TABLE heap TO heap_old, heap_next TO heap')
          await execute('DROP TABLE heap_old')
          break
        }
        default: {
          await execute('DROP TABLE uniq')
          await execute(`CREATE TABLE uniq (code INT NOT NULL, label VARCHAR(16), extra INT, UNIQUE KEY code_key (code)) DEFAULT CHARSET=utf8mb4 COLLATE=${leg.collation}`)
          keys.get('uniq')!.clear()
          const table = live.get('uniq')!
          const base = table.row
          live.set('uniq', { ...table, row: (r, k) => ({ ...base(r, k), extra: nullable(r, String(r.below(9))) }) })
        }
      }
    }

    const converge = async (round: number) => {
      const deadline = Date.now() + CONVERGE_MS
      let pending = new Map<string, string>([...live.keys()].map((name) => [name, 'not compared']))
      while (pending.size && Date.now() < deadline) {
        for (const name of [...pending.keys()]) {
          try {
            if (!replica) await connectReplica()
            const expected = await dump(source!, name)
            const actual = await dump(replica!, name)
            if (expected.length === actual.length && expected.every((row, i) => row === actual[i])) pending.delete(name)
            else pending.set(name, firstDifference(expected, actual))
          } catch (error) {
            pending.set(name, `mirror query failed: ${String(error).slice(0, 300)}`)
            replica = undefined
          }
        }
        if (pending.size) await Bun.sleep(500)
      }
      for (const [name, detail] of pending) {
        const status = await mirror.api<unknown>(`/api/databases/${database.id}/snapshot/status`).catch((error) => String(error))
        const entry = `round ${round}, table ${name}: ${detail}\n    status ${JSON.stringify(status).slice(0, 600)}`
        result.divergences.push(entry)
        log(`${leg.name} DIVERGED ${entry}`)
      }
      return pending.size === 0
    }

    for (let round = 1; round <= ROUNDS; round += 1) {
      result.rounds = round
      if (round > 1 && random.chance(0.2)) await schemaChange()
      const restartMidRound = random.chance(0.12)
      const transactions = 1 + random.below(8)
      for (let n = 0; n < transactions; n += 1) {
        if (restartMidRound && n === Math.floor(transactions / 2)) {
          await mirror.stop()
          await transaction()
          await mirror.start()
          result.restarts += 1
          replica = undefined
          continue
        }
        await transaction()
      }
      if (!(await converge(round))) break
    }
  } catch (error) {
    result.error = error instanceof Error ? error.message : String(error)
    log(`${leg.name} ERROR ${result.error}`)
  } finally {
    await replica?.end().catch(() => {})
    await source?.end().catch(() => {})
    await mirror.stop().catch(() => {})
    await docker('rm', '--force', '--volumes', container).catch(() => {})
    rmSync(dataDir, { recursive: true, force: true })
  }
  return result
}

async function main() {
  const selected = process.env.CDC_MATRIX_LEGS ? LEGS.filter((leg) => process.env.CDC_MATRIX_LEGS!.split(',').includes(leg.name)) : LEGS
  let binary = process.env.PINTAIL_CDC_MATRIX_BINARY ? resolve(process.env.PINTAIL_CDC_MATRIX_BINARY) : ''
  if (!binary) {
    await command(['cargo', 'build', '--release', '-p', 'pintail'])
    binary = join(repository, 'target', 'release', 'pintail')
  }
  const results: LegResult[] = []
  for (const leg of selected) {
    const started = performance.now()
    const result = await runLeg(leg, binary)
    results.push(result)
    log(`${leg.name}: mode ${result.mode}, ${result.rounds} rounds, ${result.transactions} transactions, ${result.statements} statements, ${result.schemaChanges} schema changes, ${result.restarts} restarts, ${result.divergences.length} divergences${result.error ? `, error ${result.error}` : ''} in ${((performance.now() - started) / 1000).toFixed(0)}s`)
  }
  const failed = results.filter((r) => r.divergences.length || r.error)
  const lines = [
    '# Change capture matrix',
    '',
    `Measured ${new Date().toISOString()}, seed ${SEED}, ${ROUNDS} rounds per leg. Generated by \`tests/e2e/cdc-matrix.ts\`.`,
    '',
    '| Leg | Mode | Rounds | Transactions | Statements | Schema changes | Restarts | Result |',
    '|---|---|---:|---:|---:|---:|---:|---|',
    ...results.map((r) => `| ${r.leg} | ${r.mode} | ${r.rounds} | ${r.transactions} | ${r.statements} | ${r.schemaChanges} | ${r.restarts} | ${r.error ? 'ERROR' : r.divergences.length ? 'DIVERGED' : 'PASS'} |`),
    '',
    ...failed.flatMap((r) => [`## ${r.leg}`, '', ...(r.error ? [`Error: ${r.error}`, ''] : []), ...r.divergences.map((d) => `\`\`\`\n${d}\n\`\`\`\n`)]),
  ]
  writeFileSync(join(import.meta.dir, 'results-cdc-matrix.md'), lines.join('\n'))
  log(failed.length ? `CDC-MATRIX-FAIL: ${failed.map((r) => r.leg).join(', ')}` : 'CDC-MATRIX-PASS')
  process.exitCode = failed.length ? 1 : 0
}

await main()

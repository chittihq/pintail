import { Database } from 'bun:sqlite'
import { appendFileSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { docker, dockerHost, dsnHost, publishedPort, freePort, waitForMysql } from '../lib'
import { assertAutomaticRequest, exactDiff } from './policy'
import { seedStandard, transfer } from './schema'
import { SourceProxy } from './proxy'

export const repository = resolve(import.meta.dir, '../../..')
export type Area = 'baseline' | 'mode' | 'cdc' | 'purge' | 'schema' | 'poll' | 'boundaries' | 'outage' | 'operator'
export type Mode = 'cdc' | 'polling'
export interface Check { scenario: string; area: Area; check: string; status: 'PASS' | 'FAIL' | 'WARN'; detail?: string }
export interface Scenario {
  slug: string
  area: Area
  promise: string
  mode?: Mode
  proxy?: boolean
  seed?: (ctx: Context) => Promise<void>
  run: (ctx: Context) => Promise<void>
  /** An explicitly scoped documented gap. All other tables still compare. */
  gap?: { table: string; pattern: RegExp; promise: string }
}
export interface ApiOptions { method?: string; body?: unknown }
const rawApis = new WeakMap<Context, <T>(path: string, options?: ApiOptions) => Promise<T>>()
/** Only scenarios/operator.ts imports this capability. */
export function operatorApi<T>(ctx: Context, path: string, options?: ApiOptions): Promise<T> {
  if (ctx.scenario.area !== 'operator') throw new Error('operator API requires operator scenario')
  return rawApis.get(ctx)!<T>(path, options)
}
export async function until(label: string, predicate: () => Promise<boolean>, timeout = 180_000): Promise<void> {
  const deadline = Date.now() + timeout
  let last = ''
  do {
    try { if (await predicate()) return } catch (error) { last = String(error) }
    await Bun.sleep(100)
  } while (Date.now() < deadline)
  throw new Error(`${label} timed out${last ? `: ${last}` : ''}`)
}
const identifier = (name: string) => `\`${name.replaceAll('`', '``')}\``
export class Source {
  name = `pintail-recovery-mysql-${process.pid}-${crypto.randomUUID().slice(0, 8)}`
  host = ''
  port = 0
  root!: mysql.Connection
  created = false
  async start() {
    this.host = await dockerHost()
    await docker('run', '--detach', '--name', this.name, '--publish', '0:3306', '--tmpfs', '/var/lib/mysql:rw,size=2g',
      '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', 'mysql:8.4', '--server-id=953', '--log-bin=mysql-bin',
      '--binlog-format=ROW', '--binlog-row-image=FULL', '--binlog-row-metadata=MINIMAL', '--gtid-mode=ON',
      '--enforce-gtid-consistency=ON', '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION')
    this.created = true
    this.port = await publishedPort(this.name, 3306)
    this.root = await waitForMysql(this.host, this.port)
    await this.root.query("CREATE USER 'pintail'@'%' IDENTIFIED BY 'pintail'; GRANT SELECT, RELOAD, LOCK TABLES, REPLICATION SLAVE, REPLICATION CLIENT ON *.* TO 'pintail'@'%'")
  }
  async connect(schema?: string) {
    return mysql.createConnection({ host: this.host, port: this.port, user: 'root', password: 'pintail-root', database: schema,
      multipleStatements: true, supportBigNumbers: true, bigNumberStrings: true, dateStrings: true, connectTimeout: 5_000 })
  }
  async close() {
    this.root?.destroy()
    if (this.created) {
      await docker('unpause', this.name).catch(() => {})
      await docker('rm', '-f', this.name)
    }
  }
}

export class Context {
  readonly schema: string
  readonly dataDir: string
  readonly artifactDir: string
  readonly seed = Number(process.env.PINTAIL_RECOVERY_SEED ?? 953)
  httpPort = 0
  wirePort = 0
  databaseId = ''
  token = ''
  key = ''
  mode: Mode
  proxy?: SourceProxy
  sourceConnection!: mysql.Connection
  private replica?: mysql.Connection
  private process?: ReturnType<typeof Bun.spawn>
  private pumps: Promise<void>[] = []
  private eventAbort?: AbortController
  private eventPump?: Promise<void>
  private stderr = ''
  private startIndex = 0
  private churnConnection?: mysql.Connection
  private churnTask?: Promise<void>
  private churning = false
  private churnError?: unknown
  commits = 0
  rollbacks = 0
  attempts = 0
  events: string[] = []
  checks: Check[] = []
  restartCount = 0
  constructor(readonly source: Source, readonly binary: string, readonly scenario: Scenario, runDir: string) {
    this.schema = `rec_${scenario.slug.replaceAll('-', '_')}_${crypto.randomUUID().slice(0, 6)}`
    this.dataDir = mkdtempSync(join(tmpdir(), 'pintail-recovery-'))
    this.artifactDir = join(runDir, scenario.slug)
    mkdirSync(this.artifactDir, { recursive: true })
    this.mode = scenario.mode ?? 'cdc'
    rawApis.set(this, this.request.bind(this))
  }
  get path() { return `/api/databases/${this.databaseId}` }
  get alive() { return this.process !== undefined && this.process.exitCode === null && this.process.signalCode === null }
  check(name: string, condition: boolean, detail = '') {
    this.checks.push({ scenario: this.scenario.slug, area: this.scenario.area, check: name, status: condition ? 'PASS' : 'FAIL', detail })
    if (!condition) throw new Error(`${name}: ${detail}`)
  }
  async sql(statement: string): Promise<void> { await this.sourceConnection.query({ sql: statement, timeout: 15_000 }) }
  async rows(statement: string): Promise<unknown[][]> {
    const [rows] = await this.sourceConnection.query({ sql: statement, rowsAsArray: true, timeout: 15_000 })
    return rows as unknown[][]
  }
  async replicaRows(statement: string): Promise<unknown[][]> {
    this.replica ??= await mysql.createConnection({ host: '127.0.0.1', port: this.wirePort, user: this.schema,
      password: this.key, database: this.schema, supportBigNumbers: true, bigNumberStrings: true, dateStrings: true, connectTimeout: 3_000 })
    try {
      const [rows] = await this.replica.query({ sql: statement, rowsAsArray: true, timeout: 20_000 })
      return rows as unknown[][]
    } catch (error) { this.replica.destroy(); this.replica = undefined; throw error }
  }
  private async request<T>(path: string, options: ApiOptions = {}): Promise<T> {
    const response = await fetch(`http://127.0.0.1:${this.httpPort}${path}`, {
      method: options.method ?? 'GET', headers: { 'Content-Type': 'application/json', ...(this.token ? { Authorization: `Bearer ${this.token}` } : {}) },
      body: options.body === undefined ? undefined : JSON.stringify(options.body), signal: AbortSignal.timeout(15_000),
    })
    const text = await response.text()
    if (!response.ok) throw new Error(`${path}: HTTP ${response.status}: ${text}`)
    return (text ? JSON.parse(text) : undefined) as T
  }
  api<T>(path: string, options: ApiOptions = {}): Promise<T> {
    assertAutomaticRequest(path, options.body, this.scenario.area === 'mode')
    return this.request(path, options)
  }
  async switchMode(mode: Mode) {
    await this.api(`${this.path}/mode`, { method: 'POST', body: { mode } })
    this.mode = mode
  }
  async start(failpoint = '') {
    this.replica?.destroy(); this.replica = undefined
    this.stderr = ''
    this.startIndex++
    const env = { ...process.env, PINTAIL_FAILPOINT: failpoint, PINTAIL_SUPERVISOR_INTERVAL_MS: '250',
      PINTAIL_SNAPSHOT_WORKERS: '1', PINTAIL_LOG_LEVEL: 'debug' }
    this.process = Bun.spawn([this.binary, '--data-dir', this.dataDir, '--http-bind', `127.0.0.1:${this.httpPort}`,
      '--wire-bind', `127.0.0.1:${this.wirePort}`], { cwd: repository, stdout: 'pipe', stderr: 'pipe', env })
    const capture = async (stream: ReadableStream<Uint8Array>, kind: string) => {
      const decoder = new TextDecoder()
      let text = ''
      const logPath = join(this.artifactDir, `process-${this.startIndex}-${kind}.log`)
      writeFileSync(logPath, '')
      for await (const chunk of stream) {
        const part = decoder.decode(chunk, { stream: true }); text += part
        appendFileSync(logPath, part)
        if (kind === 'stderr') this.stderr += part
      }
      text += decoder.decode()
      writeFileSync(logPath, text)
    }
    this.pumps = [capture(this.process.stdout as ReadableStream<Uint8Array>, 'stdout'), capture(this.process.stderr as ReadableStream<Uint8Array>, 'stderr')]
    await until('HTTP ready', async () => {
      if (!this.alive) {
        if (failpoint && this.stderr.includes('failpoint ') && this.stderr.includes(': aborting')) return true
        throw new Error(`pintail exited: ${this.stderr.slice(-1500)}`)
      }
      return (await fetch(`http://127.0.0.1:${this.httpPort}/health`, { signal: AbortSignal.timeout(1000) })).ok
    }, 30_000)
    if (this.token && this.alive) this.subscribeEvents()
  }
  private subscribeEvents() {
    this.eventAbort?.abort()
    this.eventAbort = new AbortController()
    const signal = this.eventAbort.signal
    this.eventPump = (async () => {
      const response = await fetch(`http://127.0.0.1:${this.httpPort}/api/events`, { headers: { Authorization: `Bearer ${this.token}` }, signal })
      if (!response.ok || !response.body) throw new Error(`events HTTP ${response.status}`)
      const decoder = new TextDecoder()
      let pending = ''
      for await (const chunk of response.body) {
        pending += decoder.decode(chunk, { stream: true })
        const lines = pending.split('\n'); pending = lines.pop()!
        for (const line of lines) if (line.startsWith('data:')) this.events.push(line.slice(5).trim())
      }
    })().catch(error => { if (!signal.aborted) this.events.push(`event stream ended: ${error}`) })
  }
  async stop() {
    this.eventAbort?.abort()
    this.replica?.destroy(); this.replica = undefined
    if (this.process) {
      if (this.alive) this.process.kill(9)
      await this.process.exited
      await Promise.all(this.pumps)
      this.process = undefined
    }
    await this.eventPump
  }
  async restart(failpoint = '') { await this.stop(); this.restartCount++; await this.start(failpoint) }
  async setup() {
    this.httpPort = await freePort(); this.wirePort = await freePort()
    await this.source.root.query(`CREATE DATABASE ${identifier(this.schema)}`)
    this.sourceConnection = await this.source.connect(this.schema)
    await (this.scenario.seed ?? seedStandard)(this)
    await this.start()
    this.token = (await this.api<{ token: string }>('/api/auth/setup', { method: 'POST', body: { email: 'recovery@example.com', password: 'recovery-suite-password' } })).token
    this.subscribeEvents()
    if (this.scenario.proxy) { this.proxy = new SourceProxy(this.source.host, this.source.port); await this.proxy.start() }
    const dsn = `mysql://pintail:pintail@${this.proxy ? '127.0.0.1' : dsnHost(this.source.host)}:${this.proxy?.localPort ?? this.source.port}/${this.schema}`
    const database = await this.api<{ id: string }>('/api/databases', { method: 'POST', body: { name: this.schema, dsn, mode: this.mode,
      keyless_policy: 'auto_resync', poll_interval_seconds: 1, reconcile_interval_seconds: 5 } })
    this.databaseId = database.id
    await this.api(`${this.path}/probe`)
    this.key = (await this.api<{ secret: string }>(`${this.path}/api-keys`, { method: 'POST', body: { name: 'recovery', scopes: ['read', 'query'] } })).secret
    await this.api(`${this.path}/snapshot`, { method: 'POST', body: { force: false } })
    await this.converge('baseline')
  }
  async awaitEvent(pattern: RegExp) {
    await until(`event ${pattern}`, async () => this.events.some(event => pattern.test(event)))
    this.check(`event:${pattern.source}`, true)
  }
  async status(): Promise<{ state: string; tables: Array<{ name: string; state: string; last_error?: string; last_reconcile_at?: string }> }> {
    return this.api(`${this.path}/snapshot/status`)
  }
  async activity(): Promise<Array<{ kind: string; status: string; error?: string; id: string }>> {
    return this.api(`/api/activity?db=${this.databaseId}&limit=1000`)
  }
  async startChurn() {
    if (this.churning) return
    this.churnConnection = await this.source.connect(this.schema)
    this.churning = true
    this.churnError = undefined
    this.churnTask = (async () => {
      while (this.churning) {
        const n = ++this.attempts
        const rollback = n % 10 === 0
        try {
          await transfer(this.churnConnection!, n + this.seed, rollback)
          if (rollback) this.rollbacks++; else this.commits++
        } catch (error) { this.churnError = error; this.churning = false; break }
        // Pace the workload, not synchronization; scenarios wait on counters.
        await Bun.sleep(25)
      }
    })()
    await until('churn commits and rolls back', async () => {
      if (this.churnError) throw this.churnError
      return this.commits > 0 && this.rollbacks > 0
    }, 20_000)
  }
  async stopChurn(allowSourceFailure = false) {
    this.churning = false
    await this.churnTask
    this.churnConnection?.destroy(); this.churnConnection = undefined
    if (this.churnError && !allowSourceFailure) throw this.churnError
  }
  async fired(site: string, action: 'abort' | 'error' = 'abort') {
    await until(`failpoint ${site}`, async () => this.stderr.includes(`failpoint ${site} hit `), 60_000)
    const match = this.stderr.match(new RegExp(`failpoint ${site.replaceAll('.', '\\.')} hit (\\d+): (aborting|error)`))
    this.check(`interrupts at ${site}`, !!match && match[2] === (action === 'abort' ? 'aborting' : 'error'), match?.[0] ?? 'no witness')
    if (action === 'abort') {
      await until('process aborted', async () => !this.alive, 10_000)
      await this.stop()
      this.durable(`fault-${site}`)
    }
  }
  durable(label: string): Record<string, any[]> {
    if (this.alive) throw new Error('durable metadata must be captured while process is down')
    const database = new Database(join(this.dataDir, 'pintail-meta.db'), { readonly: true })
    try {
      const result: Record<string, any[]> = {}
      for (const table of ['checkpoints', 'tables', 'poll_states', 'poll_chunk_states', 'snapshot_chunks', 'sync_runs']) {
        result[table] = database.query(`SELECT * FROM ${table} WHERE db_id=?`).all(this.databaseId)
      }
      writeFileSync(join(this.artifactDir, `durable-${label}.json`), JSON.stringify(result, null, 2))
      this.check(`durable-before-restart:${label}`, true, JSON.stringify({ checkpoints: result.checkpoints, tables: result.tables.map(t => ({ name: t.name, state: t.state })) }))
      return result
    } finally { database.close() }
  }
  async allRows(query: string, replica: boolean): Promise<unknown[][]> {
    const result: unknown[][] = []
    for (let offset = 0; ; offset += 5000) {
      const page = await (replica ? this.replicaRows(`${query} LIMIT 5000 OFFSET ${offset}`) : this.rows(`${query} LIMIT 5000 OFFSET ${offset}`))
      result.push(...page)
      if (page.length < 5000) return result
    }
  }
  pollStates(): Array<Record<string, any>> {
    const database = new Database(join(this.dataDir, 'pintail-meta.db'), { readonly: true })
    try { return database.query('SELECT * FROM poll_states WHERE db_id=?').all(this.databaseId) as Array<Record<string, any>> }
    finally { database.close() }
  }
  async compareExact(): Promise<string | undefined> {
    const tables = (await this.rows(`SELECT table_name FROM information_schema.tables WHERE table_schema='${this.schema}' AND table_type='BASE TABLE' ORDER BY table_name`)).map(row => String(row[0]))
    for (const table of tables) {
      if (this.scenario.gap?.table === table) continue
      const columns = (await this.rows(`SELECT column_name FROM information_schema.columns WHERE table_schema='${this.schema}' AND table_name='${table}' ORDER BY ordinal_position`)).map(row => identifier(String(row[0])))
      const keys = (await this.rows(`SELECT column_name FROM information_schema.key_column_usage WHERE table_schema='${this.schema}' AND table_name='${table}' AND constraint_name='PRIMARY' ORDER BY ordinal_position`)).map(row => identifier(String(row[0])))
      const query = `SELECT ${columns.join(',')} FROM ${identifier(table)}${keys.length ? ` ORDER BY ${keys.join(',')}` : ''}`
      let diff = exactDiff(await this.allRows(query, false), await this.allRows(query, true), keys.length === 0)
      if (diff) return `${table}: ${diff}`
      if (!keys.length) {
        const grouped = `SELECT ${columns.join(',')},COUNT(*) FROM ${identifier(table)} GROUP BY ${columns.join(',')}`
        diff = exactDiff(await this.allRows(grouped, false), await this.allRows(grouped, true), true)
        if (diff) return `${table} grouped multiplicity: ${diff}`
      }
    }
    const gap = this.scenario.gap ? ` AND table_name <> '${this.scenario.gap.table}'` : ''
    const metadata = `SELECT table_name,column_name,ordinal_position,data_type,column_type,is_nullable,character_maximum_length,character_octet_length,numeric_precision,numeric_scale,datetime_precision,column_default,extra,generation_expression FROM information_schema.columns WHERE table_schema='${this.schema}'${gap} ORDER BY table_name,ordinal_position`
    return exactDiff(await this.rows(metadata), await this.replicaRows(metadata))
  }
  async converge(label: string) {
    let last = ''
    await until(`convergence ${label}`, async () => {
      last = await this.compareExact() ?? ''
      if (last) return false
      const status = await this.status()
      const dlq = await this.api<any[]>(`/api/dlq?db=${this.databaseId}`)
      if (dlq.length) { last = `DLQ not yet repaired: ${JSON.stringify(dlq)}`; return false }
      return ['streaming', 'polling'].includes(status.state)
        && status.tables.every(table => ['streaming', 'polling', 'completed'].includes(table.state) || table.name === this.scenario.gap?.table)
    }).catch(error => { throw new Error(`${error}; last diff: ${last}`) })
    this.check(`converged:${label}`, true)
    const dlq = await this.api<any[]>(`/api/dlq?db=${this.databaseId}`)
    this.check(`dlq:${label}`, dlq.length === 0, JSON.stringify(dlq))
  }
  async liveWrites() {
    // Standard tables all see INSERT, UPDATE and DELETE, plus a rolled-back
    // transaction. Keyless operations use distinct values so their identity
    // is unambiguous; duplicate seed rows still exercise the multiset diff.
    await this.sql(`START TRANSACTION;
      INSERT INTO accounts VALUES(900000,'later',3.14,NOW(6)); UPDATE accounts SET balance=4.14,updated_at=NOW(6) WHERE id=900000; DELETE FROM accounts WHERE id=900000;
      INSERT INTO ledger(id,account_id,amount,note) VALUES(900000,1,3.14,'later'); UPDATE ledger SET note='changed' WHERE id=900000; DELETE FROM ledger WHERE id=900000;
      INSERT INTO audit VALUES('later','single'); UPDATE audit SET payload='changed' WHERE kind='later'; DELETE FROM audit WHERE kind='later'; COMMIT;
      START TRANSACTION; UPDATE accounts SET balance=999999 WHERE id=1; INSERT INTO ledger(id,account_id,amount,note) VALUES(900002,1,7,'rolled-back'); INSERT INTO audit VALUES('rollback','absent'); ROLLBACK;
      INSERT INTO accounts VALUES(900001,'retained',7.77,NOW(6)); INSERT INTO ledger(id,account_id,amount,note) VALUES(900001,1,7.77,'retained'); INSERT INTO audit VALUES('retained','must arrive')`)
    const tables = (await this.rows(`SELECT table_name FROM information_schema.tables WHERE table_schema='${this.schema}' AND table_name NOT IN ('accounts','ledger','audit')`)).map(r => String(r[0]))
    for (const table of tables) {
      if (table === this.scenario.gap?.table) continue
      const columns = await this.rows(`SELECT column_name FROM information_schema.columns WHERE table_schema='${this.schema}' AND table_name='${table}' ORDER BY ordinal_position`)
      if (columns.some(c => c[0] === 'value')) await this.sql(`INSERT INTO ${identifier(table)}(id,value) VALUES(999999,'live'); UPDATE ${identifier(table)} SET value='updated' WHERE id=999999; DELETE FROM ${identifier(table)} WHERE id=999999; INSERT INTO ${identifier(table)}(id,value) VALUES(999998,'retained')`)
    }
  }
  async proveConverged() {
    await this.stopChurn()
    await this.converge('after-recovery')
    await this.liveWrites()
    await this.converge('after-live-writes')
    await this.restart()
    await this.converge('after-second-restart')
    this.check('automatic:no-manual-repair-event', this.scenario.area === 'operator' || !this.events.some(e => /resync\.manual/.test(e)))
  }
  async close() {
    this.churning = false; this.churnConnection?.destroy()
    await this.churnTask?.catch(() => {})
    await this.stop()
    this.sourceConnection?.destroy()
    await this.proxy?.close()
    writeFileSync(join(this.artifactDir, 'events.json'), JSON.stringify(this.events, null, 2))
    await this.source.root.query(`DROP DATABASE IF EXISTS ${identifier(this.schema)}`)
    rmSync(this.dataDir, { recursive: true, force: true })
  }
}

export async function runScenario(source: Source, binary: string, scenario: Scenario, runDir: string): Promise<Check[]> {
  const ctx = new Context(source, binary, scenario, runDir)
  try {
    await ctx.setup()
    await ctx.startChurn()
    const before = ctx.commits
    await scenario.run(ctx)
    await until('writer advanced through injection', async () => ctx.commits > before, 10_000)
    ctx.check('churn:commits-and-rollbacks-through-injection', ctx.commits > before && ctx.rollbacks > 0, `seed=${ctx.seed}; committed=${ctx.commits}; rolled_back=${ctx.rollbacks}`)
    await ctx.proveConverged()
  } catch (error) {
    ctx.checks.push({ scenario: scenario.slug, area: scenario.area, check: 'scenario', status: 'FAIL', detail: String(error) })
  } finally {
    await ctx.close().catch(error => ctx.checks.push({ scenario: scenario.slug, area: scenario.area, check: 'teardown', status: 'FAIL', detail: String(error) }))
  }
  return ctx.checks
}

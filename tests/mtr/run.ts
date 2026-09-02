/// MySQL's own regression suite, replayed against Pintail.
///
/// `mysql-test/` in the MySQL server source is thirty years of what MySQL's
/// developers thought to check: every `func_*`, `group_by`, `subselect` and
/// `window_functions` file is a fixture followed by hundreds of SELECTs. Our
/// oracle can only generate what we thought to generate; this replays what
/// they did.
///
/// A read-only analytical replica has no business with most of the suite -
/// DDL, DML, transactions, locking, privileges, replication - so only the
/// query side is claimed. Each file's fixtures are built with CREATE TABLE and
/// INSERT into a per-file Pintail local database (the same query engine the
/// CDC replica runs, without waiting on replication for every statement) and
/// into a per-file MySQL schema, then every SELECT runs against both and the
/// answers are compared. Live MySQL is the oracle, not the `.result` files:
/// those pin one server version and carry EXPLAIN plans and warning text.
///
/// Tables the suite mutates in ways a local database cannot follow (UPDATE,
/// DELETE, ALTER, INSERT ... SELECT, views, temporaries) are marked tainted
/// from that statement on, and queries touching them are reported as skipped
/// rather than compared against a state we could not reproduce. Files
/// re-create tables under the same name constantly, so every CREATE gets a
/// fresh physical name (`t1__3`) and later references are rewritten to it;
/// DROP becomes a no-op on both sides, which keeps the two states identical.
///
/// The test files are GPLv2 and are fetched at run time from
/// github.com/mysql/mysql-server, never vendored. Every statement is
/// classified, every classification is counted, and the report says how many
/// SELECTs matched MySQL byte-for-byte - the headline number - next to how
/// many could not be compared and why.
///
/// Run with: bun run run.ts
///           MTR_FILES=func_math,func_str bun run run.ts
///           MTR_FILES='^(func_|group_by|order_by)' bun run run.ts
///           MTR_REF=8.4 PINTAIL_MTR_BINARY=../../target/release/pintail bun run run.ts

import { createServer } from 'node:net'
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'

const repository = resolve(import.meta.dir, '..', '..')
const nonce = Date.now().toString(36)
const mysqlName = `pintail-mtr-mysql-${process.pid}-${nonce}`
const cacheDir = join(import.meta.dir, '.cache')
const diffsDir = join(import.meta.dir, 'diffs')

/// Branch or tag of mysql/mysql-server to fetch the suite from.
const REF = process.env.MTR_REF ?? '8.4'
/// Which main-suite files to run: a comma list of names, or a regex when it
/// starts with `^`. The default is the query-shaped subset.
const FILES = process.env.MTR_FILES ?? ''
const DEFAULT_PATTERN =
  /^(func_(?!aes|digest|md5|rollback|str_debug|str_no_ps|str_myisam|date_add_myisam|group_innodb|in_icp|in_mrr|prefix_key|rand|system|uuid|compress|misc)|select|window_functions|json|group_by|order_by|subselect|derived|cte|having|union|distinct|limit|null|case|type_|round|negation|varbinary)/
const LIMIT = Number(process.env.MTR_LIMIT ?? '0')
/// Fetch and classify only - no containers, no server. The fast check that
/// the tokenizer still understands the suite.
const PARSE_ONLY = process.env.MTR_PARSE_ONLY === '1'
/// Per-statement client-side timeout on both sides.
const QUERY_TIMEOUT_MS = Number(process.env.MTR_QUERY_TIMEOUT_MS ?? '30000')

type Kind =
  | 'exact'
  | 'mismatch'
  | 'name-mismatch'
  | 'pintail-error'
  | 'mysql-error'
  | 'tainted'
  | 'setup'
  | 'setup-rejected'
  | 'unsupported-setup'
  | 'session'
  | 'skipped'

interface FileResult {
  file: string
  statements: number
  counts: Record<Kind, number>
  parserNote?: string
  errorClasses: Record<string, number>
}

interface Stmt {
  sql: string
  sorted: boolean
  masked: boolean
  expectError: boolean
  line: number
}

let mysqlRoot: mysql.Connection | undefined
let mysqlStarted = false
let mysqlHost = ''
let mysqlPort = 0
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailDataDir = ''
let pintailHttpPort = 0
let pintailWirePort = 0
let pintailUrl = ''
let token = ''

function log(message: string) {
  console.log(`[mtr] ${message}`)
}

async function command(args: string[], options: { cwd?: string; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
    stdout: options.quiet ? 'pipe' : 'inherit',
    stderr: options.quiet ? 'pipe' : 'inherit',
  })
  const stdout = options.quiet ? await new Response(child.stdout).text() : ''
  const stderr = options.quiet ? await new Response(child.stderr).text() : ''
  const status = await child.exited
  if (status !== 0) throw new Error(`${args.join(' ')} failed (${status}): ${stderr}`)
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')).stdout
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  const target = endpoint.slice('ssh://'.length).split('@').at(-1)!.split(':')[0]
  const ssh = await command(['ssh', '-G', target], { quiet: true })
  const hostname = ssh.stdout.split('\n').find((line) => line.startsWith('hostname '))?.slice('hostname '.length)
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

/// Every value as the text the server sent, so the comparison is the one the
/// oracle makes: byte-for-byte, no client-side number or date parsing.
const textConnection = (options: mysql.ConnectionOptions) =>
  mysql.createConnection({
    ...options,
    typeCast: (field) => field.string(),
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
  })

async function api<T>(path: string, options: { method?: string; body?: unknown; auth?: boolean } = {}): Promise<T> {
  const headers: Record<string, string> = { 'content-type': 'application/json' }
  if (options.auth !== false && token) headers.authorization = `Bearer ${token}`
  const response = await fetch(`${pintailUrl}${path}`, {
    method: options.method ?? 'GET',
    headers,
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  })
  const text = await response.text()
  if (!response.ok) throw new Error(`${options.method ?? 'GET'} ${path} → ${response.status}: ${text}`)
  return text ? (JSON.parse(text) as T) : (undefined as T)
}

// ---------------------------------------------------------------------------
// Fetching the suite

async function fetchText(url: string): Promise<string> {
  const response = await fetch(url, { headers: { 'user-agent': 'pintail-mtr' } })
  if (!response.ok) throw new Error(`${url} → ${response.status}`)
  return response.text()
}

async function listTestFiles(): Promise<string[]> {
  const cached = join(cacheDir, REF, '_index.json')
  if (existsSync(cached)) return JSON.parse(readFileSync(cached, 'utf8')) as string[]
  const tree = JSON.parse(
    await fetchText(`https://api.github.com/repos/mysql/mysql-server/git/trees/${REF}?recursive=1`),
  ) as { tree: Array<{ path: string; type: string }>; truncated: boolean }
  const files = tree.tree
    .filter((entry) => entry.type === 'blob' && /^mysql-test\/t\/[^/]+\.test$/.test(entry.path))
    .map((entry) => entry.path.slice('mysql-test/t/'.length, -'.test'.length))
    .sort()
  if (files.length === 0) throw new Error(`no test files found under mysql-test/t at ${REF}`)
  mkdirSync(join(cacheDir, REF), { recursive: true })
  writeFileSync(cached, JSON.stringify(files))
  return files
}

async function fetchTestFile(name: string): Promise<string> {
  const path = join(cacheDir, REF, `${name}.test`)
  if (existsSync(path)) return readFileSync(path, 'utf8')
  const text = await fetchText(
    `https://raw.githubusercontent.com/mysql/mysql-server/${REF}/mysql-test/t/${name}.test`,
  )
  mkdirSync(join(cacheDir, REF), { recursive: true })
  writeFileSync(path, text)
  return text
}

// ---------------------------------------------------------------------------
// Parsing the mysqltest language, just enough

/// Commands that take the rest of the line (through the delimiter) as their
/// argument, so the statement that follows must not be read as SQL.
const STATEMENT_COMMANDS = new Set([
  'eval', 'query', 'query_vertical', 'query_horizontal', 'send', 'send_eval', 'let', 'echo', 'exec',
  'exec_in_background', 'system', 'error', 'die', 'skip', 'source', 'connect', 'connection',
  'disconnect', 'sleep', 'real_sleep', 'reap', 'remove_file', 'remove_files_wildcard', 'cat_file',
  'copy_file', 'move_file', 'chmod', 'mkdir', 'rmdir', 'list_files', 'file_exists', 'change_user',
  'character_set', 'replace_column', 'replace_result', 'replace_regex', 'replace_numeric_round',
  'sorted_result', 'partially_sorted_result', 'lowercase_result', 'disable_warnings', 'enable_warnings',
  'disable_query_log', 'enable_query_log', 'disable_result_log', 'enable_result_log', 'disable_info',
  'enable_info', 'disable_metadata', 'enable_metadata', 'disable_ps_protocol', 'enable_ps_protocol',
  'disable_abort_on_error', 'enable_abort_on_error', 'disable_reconnect', 'enable_reconnect',
  'disable_testcase', 'enable_testcase', 'disable_connect_log', 'enable_connect_log', 'disable_async_client',
  'enable_async_client', 'disable_ps2_protocol', 'enable_ps2_protocol', 'disable_session_track_info',
  'enable_session_track_info', 'horizontal_results', 'vertical_results', 'start_timer', 'end_timer',
  'inc', 'dec', 'sync_with_master', 'sync_slave_with_master', 'save_master_pos', 'wait_for_slave_to_stop',
  'require', 'result_format', 'force-rmdir', 'force-cpdir', 'output', 'expr', 'assert', 'reset_connection',
  'exit', 'shutdown_server', 'wait_for_pop_to_finish',
])
/// Commands whose body runs until a terminator line.
const BLOCK_COMMANDS = new Set(['perl', 'write_file', 'append_file'])

function parse(text: string): { statements: Stmt[]; note?: string } {
  const lines = text.split('\n')
  const statements: Stmt[] = []
  let delimiter = ';'
  let buffer: string[] = []
  let bufferStart = 0
  let sorted = false
  let masked = false
  let expectError = false
  let braceDepth = 0
  let note: string | undefined
  let blockTerminator: string | undefined

  const flush = (line: number) => {
    const sql = buffer.join('\n').trim()
    buffer = []
    if (!sql) return
    statements.push({ sql, sorted, masked, expectError, line })
    sorted = false
    masked = false
    expectError = false
  }

  for (let index = 0; index < lines.length; index += 1) {
    const raw = lines[index]!
    const line = raw.trim()
    if (blockTerminator !== undefined) {
      if (line === blockTerminator) blockTerminator = undefined
      continue
    }
    if (braceDepth > 0) {
      // Inside a while/if block: control flow we do not run.
      braceDepth += (line.match(/{/g) ?? []).length
      braceDepth -= (line.match(/}/g) ?? []).length
      continue
    }
    if (buffer.length === 0) {
      if (line === '' || line.startsWith('#')) continue
      const command = line.startsWith('--') ? line.slice(2).trim() : line
      const word = command.split(/[\s(]/)[0]?.toLowerCase() ?? ''
      if (word === 'delimiter') {
        const next = command.slice('delimiter'.length).trim()
        delimiter = next.endsWith(delimiter) && next.length > delimiter.length ? next.slice(0, -delimiter.length).trim() : next
        if (delimiter === '') delimiter = ';'
        continue
      }
      if (BLOCK_COMMANDS.has(word)) {
        blockTerminator = command.match(/<<\s*(\w+)/)?.[1] ?? 'EOF'
        continue
      }
      if (word === 'while' || word === 'if') {
        braceDepth = (line.match(/{/g) ?? []).length - (line.match(/}/g) ?? []).length
        if (braceDepth <= 0) braceDepth = 1
        continue
      }
      if (line === '}' || line === '{') continue
      if (line.startsWith('--') || STATEMENT_COMMANDS.has(word)) {
        if (word === 'error') expectError = true
        if (word === 'sorted_result' || word === 'partially_sorted_result') sorted = true
        if (word.startsWith('replace_') || word === 'lowercase_result') masked = true
        if (word === 'query' || word === 'query_vertical' || word === 'query_horizontal') {
          // `--query SELECT ...` is a statement in disguise; keep the SQL.
          buffer.push(command.slice(word.length))
          bufferStart = index + 1
          if (command.trimEnd().endsWith(delimiter)) {
            buffer[buffer.length - 1] = buffer[buffer.length - 1]!.trimEnd().slice(0, -delimiter.length)
            flush(bufferStart)
          }
          continue
        }
        // Multi-line commands (eval/let/echo) run to the delimiter.
        if (STATEMENT_COMMANDS.has(word) && !command.trimEnd().endsWith(delimiter) && !line.startsWith('--')) {
          let cursor = index + 1
          while (cursor < lines.length && !lines[cursor]!.trimEnd().endsWith(delimiter)) cursor += 1
          index = cursor
        }
        if (word === 'eval' || word === 'let' || word === 'echo' || word === 'exec' || word === 'system') {
          // They consumed the statement; whatever flags were set apply to nothing.
          sorted = false
          masked = false
          expectError = false
        }
        continue
      }
      bufferStart = index + 1
    }
    const content = raw.replace(/\s+$/, '')
    if (content.endsWith(delimiter)) {
      buffer.push(content.slice(0, -delimiter.length))
      flush(bufferStart)
    } else {
      buffer.push(content)
    }
  }
  if (buffer.length) note = `${buffer.length} trailing lines without a delimiter were dropped`
  return { statements, note }
}

// ---------------------------------------------------------------------------
// Classification and rewriting

const IDENT = String.raw`\x60?([A-Za-z_][A-Za-z0-9_]*)\x60?`

function firstWords(sql: string): string {
  return sql.replace(/^\s*\/\*.*?\*\/\s*/s, '').toLowerCase().replace(/\s+/g, ' ').slice(0, 40)
}

function createdTable(sql: string): string | undefined {
  return new RegExp(String.raw`^\s*create\s+(?:temporary\s+)?table\s+(?:if\s+not\s+exists\s+)?(?:\w+\.)?${IDENT}`, 'i').exec(sql)?.[1]
}

/// Tables a statement changes in a way a local database cannot follow.
function mutatedTables(sql: string): string[] {
  const found: string[] = []
  const patterns = [
    String.raw`^\s*update\s+(?:low_priority\s+)?(?:ignore\s+)?${IDENT}`,
    String.raw`^\s*delete\s+(?:low_priority\s+)?(?:quick\s+)?(?:ignore\s+)?from\s+${IDENT}`,
    String.raw`^\s*delete\s+${IDENT}\s+from`,
    String.raw`^\s*alter\s+(?:ignore\s+)?table\s+${IDENT}`,
    String.raw`^\s*truncate\s+(?:table\s+)?${IDENT}`,
    String.raw`^\s*replace\s+(?:into\s+)?${IDENT}`,
    String.raw`^\s*insert\s+(?:\w+\s+)*?(?:into\s+)?${IDENT}`,
    String.raw`^\s*load\s+data\s+.*?\s+into\s+table\s+${IDENT}`,
    String.raw`^\s*rename\s+table\s+${IDENT}`,
    String.raw`^\s*create\s+(?:or\s+replace\s+)?(?:algorithm\s*=\s*\w+\s+)?(?:definer\s*=\s*\S+\s+)?(?:sql\s+security\s+\w+\s+)?view\s+${IDENT}`,
    String.raw`^\s*create\s+(?:temporary\s+)?table\s+(?:if\s+not\s+exists\s+)?${IDENT}`,
    String.raw`^\s*(?:create|drop)\s+(?:unique\s+|fulltext\s+|spatial\s+)?index\s+\w+\s+on\s+${IDENT}`,
  ]
  for (const pattern of patterns) {
    const match = new RegExp(pattern, 'is').exec(sql)
    if (match?.[1]) found.push(match[1].toLowerCase())
  }
  return found
}

function isQuery(sql: string): boolean {
  return /^\s*(?:\/\*.*?\*\/\s*)?(?:select|with|\(\s*select|values|table)\b/is.test(sql) && !/\binto\s+(?:outfile|dumpfile|@)/i.test(sql)
}

function isSimpleInsert(sql: string): boolean {
  return /^\s*insert\s+(?:ignore\s+)?(?:into\s+)?[\w`.]+\s*(?:\([^)]*\)\s*)?values?\s*\(/is.test(sql) && !/\bon\s+duplicate\b|^\s*insert\s+ignore/i.test(sql)
}

function isSimpleCreate(sql: string): boolean {
  if (!/^\s*create\s+table\s+(?:if\s+not\s+exists\s+)?[\w`.]+\s*\(/is.test(sql)) return false
  if (/\)\s*(?:as\s+)?select\b|\blike\s+[\w`]/i.test(sql)) return false
  return true
}

function isSession(sql: string): boolean {
  return /^\s*(?:set\s+(?!global\b|@|password)|use\s+\w+)/i.test(sql)
}

function hasOuterOrderBy(sql: string): boolean {
  let depth = 0
  const lower = sql.toLowerCase()
  for (let index = 0; index < lower.length; index += 1) {
    const char = lower[index]
    if (char === '(') depth += 1
    else if (char === ')') depth -= 1
    else if (depth === 0 && lower.startsWith('order by', index)) return true
  }
  return false
}

class Epochs {
  private epochs = new Map<string, number>()
  private tainted = new Set<string>()

  create(name: string, supported: boolean) {
    const key = name.toLowerCase()
    this.epochs.set(key, (this.epochs.get(key) ?? 0) + 1)
    if (supported) this.tainted.delete(key)
    else this.tainted.add(key)
  }

  taint(name: string) {
    const key = name.toLowerCase()
    if (!this.epochs.has(key)) this.epochs.set(key, 1)
    this.tainted.add(key)
  }

  physical(name: string): string {
    const key = name.toLowerCase()
    const epoch = this.epochs.get(key)
    return epoch === undefined ? name : `${name}__${epoch}`
  }

  /// Rewrites every tracked table name to its current physical name.
  rewrite(sql: string): string {
    let out = sql
    for (const [key, epoch] of this.epochs) {
      out = out.replace(new RegExp(String.raw`(?<![\w$.])\x60?(${key})\x60?(?![\w$])`, 'gi'), (whole, matched: string) =>
        whole.startsWith('`') ? `\`${matched}__${epoch}\`` : `${matched}__${epoch}`,
      )
    }
    return out
  }

  touchesTainted(sql: string): string | undefined {
    for (const key of this.tainted) {
      if (new RegExp(String.raw`(?<![\w$.])\x60?${key}\x60?(?![\w$])`, 'i').test(sql)) return key
    }
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Comparing

interface Answer {
  names: string[]
  rows: string[][]
}

function canonicalRows(rows: string[][], ordered: boolean): string[] {
  const lines = rows.map((row) => row.map((value) => (value === null ? 'NULL' : String(value))).join('\t'))
  return ordered ? lines : lines.sort()
}

function errorClass(message: string): string {
  const cleaned = message.replace(/`[^`]*`|'[^']*'|"[^"]*"|\b\d+\b/g, '_').replace(/\s+/g, ' ').trim()
  return cleaned.slice(0, 90)
}

async function runFile(name: string, text: string, root: mysql.Connection, host: string): Promise<FileResult> {
  const counts: Record<Kind, number> = {
    exact: 0, mismatch: 0, 'name-mismatch': 0, 'pintail-error': 0, 'mysql-error': 0, tainted: 0,
    setup: 0, 'setup-rejected': 0, 'unsupported-setup': 0, session: 0, skipped: 0,
  }
  const errorClasses: Record<string, number> = {}
  const { statements, note } = parse(text)
  const schema = `mtr_${name.replace(/[^a-z0-9]/gi, '_').toLowerCase()}`.slice(0, 48)
  const diffLines: string[] = []

  await root.query(`DROP DATABASE IF EXISTS ${schema}`)
  await root.query(`CREATE DATABASE ${schema}`)
  const local = await api<{ id: string }>('/api/databases/local', { method: 'POST', body: { name: schema } })
  const key = await api<{ secret: string }>(`/api/databases/${local.id}/api-keys`, {
    method: 'POST',
    body: { name: 'mtr', scopes: ['query', 'read'] },
  })
  const my = await textConnection({ host, port: mysqlPort, user: 'root', password: 'pintail-root', database: schema, multipleStatements: false })
  const pt = await textConnection({ host: '127.0.0.1', port: pintailWirePort, user: schema, password: key.secret, database: schema })
  const epochs = new Epochs()

  const run = async (connection: mysql.Connection, sql: string): Promise<Answer> => {
    const [rows, fields] = await connection.query<mysql.RowDataPacket[][]>({ sql, rowsAsArray: true, timeout: QUERY_TIMEOUT_MS })
    return { names: (fields ?? []).map((field) => field.name), rows: rows as unknown as string[][] }
  }

  try {
    for (const statement of statements) {
      const sql = statement.sql
      if (statement.expectError) {
        counts.skipped += 1
        continue
      }
      if (isSession(sql)) {
        counts.session += 1
        await my.query(sql).catch(() => {})
        await pt.query(sql).catch(() => {})
        continue
      }
      if (/^\s*drop\s+(?:temporary\s+)?table\b/i.test(sql)) {
        counts.setup += 1
        continue
      }
      if (isQuery(sql)) {
        if (statement.masked) {
          counts.skipped += 1
          continue
        }
        const taintedTable = epochs.touchesTainted(sql)
        if (taintedTable) {
          counts.tainted += 1
          continue
        }
        const rewritten = epochs.rewrite(sql)
        let expected: Answer
        try {
          expected = await run(my, rewritten)
        } catch {
          counts['mysql-error'] += 1
          continue
        }
        let actual: Answer
        try {
          actual = await run(pt, rewritten)
        } catch (error) {
          counts['pintail-error'] += 1
          const cls = errorClass(String(error))
          errorClasses[cls] = (errorClasses[cls] ?? 0) + 1
          continue
        }
        const ordered = hasOuterOrderBy(sql) && !statement.sorted
        const want = canonicalRows(expected.rows, ordered)
        const got = canonicalRows(actual.rows, ordered)
        const rowsMatch = want.length === got.length && want.every((line, index) => line === got[index])
        const namesMatch = expected.names.join('\t') === actual.names.join('\t')
        if (rowsMatch && namesMatch) {
          counts.exact += 1
        } else if (rowsMatch) {
          counts['name-mismatch'] += 1
          diffLines.push(`## line ${statement.line}: column names\n\n\`\`\`sql\n${sql}\n\`\`\`\nmysql:   ${expected.names.join(' | ')}\npintail: ${actual.names.join(' | ')}\n`)
        } else {
          counts.mismatch += 1
          const firstDiff = want.findIndex((line, index) => line !== got[index])
          diffLines.push(
            `## line ${statement.line}\n\n\`\`\`sql\n${sql}\n\`\`\`\n${want.length} vs ${got.length} rows, ${ordered ? 'ordered' : 'unordered'} compare\n` +
              `mysql:   ${want.slice(Math.max(0, firstDiff), firstDiff + 3).join(' ‖ ') || '(no rows)'}\n` +
              `pintail: ${got.slice(Math.max(0, firstDiff), firstDiff + 3).join(' ‖ ') || '(no rows)'}\n`,
          )
        }
        continue
      }
      const created = createdTable(sql)
      if (created !== undefined) {
        if (isSimpleCreate(sql) && !/^\s*create\s+temporary/i.test(sql)) {
          epochs.create(created, true)
          const rewritten = epochs.rewrite(sql)
          try {
            await my.query(rewritten)
          } catch {
            // MySQL itself rejects it (engine, option, or syntax the branch lacks).
            epochs.taint(created)
            counts['unsupported-setup'] += 1
            continue
          }
          try {
            await pt.query(rewritten)
            counts.setup += 1
          } catch (error) {
            epochs.taint(created)
            counts['setup-rejected'] += 1
            const cls = `DDL: ${errorClass(String(error))}`
            errorClasses[cls] = (errorClasses[cls] ?? 0) + 1
          }
        } else {
          epochs.create(created, false)
          await my.query(epochs.rewrite(sql)).catch(() => {})
          counts['unsupported-setup'] += 1
        }
        continue
      }
      if (isSimpleInsert(sql)) {
        const target = mutatedTables(sql)[0]
        if (target && epochs.touchesTainted(target)) {
          await my.query(epochs.rewrite(sql)).catch(() => {})
          counts['unsupported-setup'] += 1
          continue
        }
        const rewritten = epochs.rewrite(sql)
        try {
          await my.query(rewritten)
        } catch {
          counts['unsupported-setup'] += 1
          if (target) epochs.taint(target)
          continue
        }
        try {
          await pt.query(rewritten)
          counts.setup += 1
        } catch (error) {
          if (target) epochs.taint(target)
          counts['setup-rejected'] += 1
          const cls = `INSERT: ${errorClass(String(error))}`
          errorClasses[cls] = (errorClasses[cls] ?? 0) + 1
        }
        continue
      }
      // Everything else: run on MySQL so its state stays ahead of ours, and
      // taint whatever it touched so no query compares against it.
      for (const table of mutatedTables(sql)) epochs.taint(table)
      const shape = firstWords(sql)
      if (/^(show|explain|describe|desc|analyze|flush|check|optimize|repair|checksum|lock|unlock|begin|start|commit|rollback|savepoint|release|prepare|execute|deallocate|handler|kill|reset|purge|do|call|help|xa|set global|set password|set @|grant|revoke|create user|drop user|create database|drop database|drop view|drop index|drop procedure|drop function|drop trigger|drop event|install|uninstall|cache|load index|create schema|drop schema|signal|resignal|get diagnostics)\b/.test(shape)) {
        counts.skipped += 1
        if (/^(create database|drop database|create schema|drop schema|kill|shutdown)/.test(shape)) continue
        await my.query(epochs.rewrite(sql)).catch(() => {})
        continue
      }
      counts['unsupported-setup'] += 1
      await my.query(epochs.rewrite(sql)).catch(() => {})
    }
  } finally {
    await my.end().catch(() => {})
    await pt.end().catch(() => {})
    await root.query(`DROP DATABASE IF EXISTS ${schema}`).catch(() => {})
    await api(`/api/databases/${local.id}`, { method: 'DELETE' }).catch(() => {})
  }
  if (diffLines.length) {
    mkdirSync(diffsDir, { recursive: true })
    writeFileSync(join(diffsDir, `${name}.md`), `# ${name}.test\n\n${diffLines.join('\n')}`)
  }
  return { file: name, statements: statements.length, counts, parserNote: note, errorClasses }
}

// ---------------------------------------------------------------------------
// The server under test

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_MTR_BINARY) return resolve(process.env.PINTAIL_MTR_BINARY)
  log('building the release pintail binary')
  await command(['cargo', 'build', '--release', '-p', 'pintail'])
  const metadata = await command(['cargo', 'metadata', '--format-version', '1', '--no-deps'], { quiet: true })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function startPintail(binary: string) {
  pintailProcess = Bun.spawn(
    [binary, '--data-dir', pintailDataDir, '--http-bind', `127.0.0.1:${pintailHttpPort}`, '--wire-bind', `127.0.0.1:${pintailWirePort}`],
    { cwd: repository, stdout: 'ignore', stderr: 'ignore', env: { ...process.env, PINTAIL_LOG: 'error' } },
  )
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (pintailProcess.exitCode !== null) throw new Error(`pintail exited during startup (exit ${pintailProcess.exitCode})`)
    try {
      if ((await fetch(`${pintailUrl}/health`)).ok) return
    } catch {}
    await Bun.sleep(500)
  }
  throw new Error('pintail did not become healthy within 120 seconds')
}

// ---------------------------------------------------------------------------
// Report

function publish(results: FileResult[], mysqlVersion: string) {
  const total = (kind: Kind) => results.reduce((sum, r) => sum + r.counts[kind], 0)
  const compared = total('exact') + total('mismatch') + total('name-mismatch')
  const queries = compared + total('pintail-error') + total('tainted')
  const classes: Record<string, number> = {}
  for (const r of results) for (const [cls, n] of Object.entries(r.errorClasses)) classes[cls] = (classes[cls] ?? 0) + n
  const topClasses = Object.entries(classes).sort((a, b) => b[1] - a[1]).slice(0, 25)
  const lines = [
    "# MySQL's regression suite against Pintail",
    '',
    `Measured ${new Date().toISOString()}: \`mysql-test/t\` from mysql/mysql-server at \`${REF}\`, oracle MySQL ${mysqlVersion}, ${results.length} files.`,
    '',
    `**${total('exact').toLocaleString()} of ${compared.toLocaleString()} compared SELECTs match MySQL byte-for-byte** ` +
      `(${compared ? ((100 * total('exact')) / compared).toFixed(1) : '0'}%). ` +
      `${total('mismatch').toLocaleString()} differ in rows, ${total('name-mismatch').toLocaleString()} in column names only. ` +
      `${total('pintail-error').toLocaleString()} SELECTs Pintail could not run, ${total('tainted').toLocaleString()} were not compared because their tables were changed by statements a local database cannot follow, ` +
      `${total('mysql-error').toLocaleString()} failed on MySQL itself. ` +
      `Fixtures: ${total('setup').toLocaleString()} accepted, ${total('setup-rejected').toLocaleString()} rejected by Pintail, ${total('unsupported-setup').toLocaleString()} outside the replayed subset.`,
    '',
    'Column names are compared with rows. Row order is compared when the outer query has ORDER BY and the test did not ask for sorted results; otherwise rows are compared as multisets.',
    '',
    '| File | Statements | Exact | Mismatch | Names | Pintail error | Tainted | MySQL error | Setup ok | Setup rejected | Unsupported | Session | Skipped |',
    '|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|',
    ...results.map(
      (r) =>
        `| ${r.file}${r.parserNote ? ' †' : ''} | ${r.statements} | ${r.counts.exact} | ${r.counts.mismatch} | ${r.counts['name-mismatch']} | ${r.counts['pintail-error']} | ${r.counts.tainted} | ${r.counts['mysql-error']} | ${r.counts.setup} | ${r.counts['setup-rejected']} | ${r.counts['unsupported-setup']} | ${r.counts.session} | ${r.counts.skipped} |`,
    ),
    '',
    '† the parser dropped trailing lines it could not delimit.',
    '',
    '## What Pintail could not run, by message shape',
    '',
    '| Count | Message |',
    '|---:|---|',
    ...topClasses.map(([cls, n]) => `| ${n} | ${cls.replace(/\|/g, '\\|')} |`),
    '',
    `Per-file diffs for mismatches are written to \`tests/mtr/diffs/\` (not committed).`,
    '',
  ]
  writeFileSync(join(import.meta.dir, 'results.md'), lines.join('\n'))
  writeFileSync(
    join(import.meta.dir, 'results.json'),
    JSON.stringify({ ref: REF, mysqlVersion, measuredAt: new Date().toISOString(), totals: { exact: total('exact'), compared, queries }, results }, null, 2),
  )
}

async function main() {
  const host = await dockerHost()
  const all = await listTestFiles()
  let selected: string[]
  if (FILES.startsWith('^')) selected = all.filter((name) => new RegExp(FILES).test(name))
  else if (FILES) selected = FILES.split(',').map((name) => name.trim()).filter(Boolean)
  else selected = all.filter((name) => DEFAULT_PATTERN.test(name))
  if (LIMIT > 0) selected = selected.slice(0, LIMIT)
  log(`${selected.length} of ${all.length} main-suite files selected at ${REF}`)
  if (PARSE_ONLY) {
    let totals = { statements: 0, queries: 0, creates: 0, inserts: 0, session: 0, other: 0 }
    for (const name of selected) {
      const { statements, note } = parse(await fetchTestFile(name))
      const c = { queries: 0, creates: 0, inserts: 0, session: 0, other: 0 }
      for (const s of statements) {
        if (isQuery(s.sql)) c.queries += 1
        else if (createdTable(s.sql) !== undefined) c.creates += 1
        else if (isSimpleInsert(s.sql)) c.inserts += 1
        else if (isSession(s.sql)) c.session += 1
        else c.other += 1
      }
      totals = { statements: totals.statements + statements.length, queries: totals.queries + c.queries, creates: totals.creates + c.creates, inserts: totals.inserts + c.inserts, session: totals.session + c.session, other: totals.other + c.other }
      log(`${name}: ${statements.length} statements, ${c.queries} queries, ${c.creates} creates, ${c.inserts} inserts, ${c.session} session, ${c.other} other${note ? ` (${note})` : ''}`)
    }
    log(`total: ${totals.statements} statements, ${totals.queries} queries, ${totals.creates} creates, ${totals.inserts} inserts, ${totals.session} session, ${totals.other} other`)
    return
  }

  log(`starting MySQL oracle ${mysqlName}`)
  await docker(
    'run', '--detach', '--name', mysqlName, '--publish', '0:3306', '--tmpfs', '/var/lib/mysql:rw,size=2g',
    '--env', 'MYSQL_ROOT_PASSWORD=pintail-root', 'mysql:8.4',
    '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION', '--max-allowed-packet=256M',
  )
  mysqlStarted = true
  mysqlHost = host
  mysqlPort = await publishedPort(mysqlName, 3306)
  for (let attempt = 0; ; attempt += 1) {
    try {
      mysqlRoot = await mysql.createConnection({ host, port: mysqlPort, user: 'root', password: 'pintail-root' })
      break
    } catch {
      if (attempt > 240) throw new Error('MySQL did not become ready in time')
      await Bun.sleep(500)
    }
  }
  const [[version]] = await mysqlRoot.query<mysql.RowDataPacket[][]>({ sql: 'SELECT VERSION()', rowsAsArray: true })
  const mysqlVersion = String((version as unknown as string[])[0])

  const binary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-mtr-'))
  pintailHttpPort = await freePort()
  pintailWirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${pintailHttpPort}`
  await startPintail(binary)
  token = (
    await api<{ token: string }>('/api/auth/setup', { method: 'POST', auth: false, body: { email: 'mtr@pintail.local', password: 'mtr-gate-password' } })
  ).token

  rmSync(diffsDir, { recursive: true, force: true })
  const results: FileResult[] = []
  for (const name of selected) {
    let text: string
    try {
      text = await fetchTestFile(name)
    } catch (error) {
      log(`${name}: could not fetch (${error instanceof Error ? error.message : String(error)})`)
      continue
    }
    const started = performance.now()
    const result = await runFile(name, text, mysqlRoot, mysqlHost)
    results.push(result)
    const c = result.counts
    log(
      `${name}: ${c.exact} exact, ${c.mismatch} mismatch, ${c['name-mismatch']} names, ${c['pintail-error']} pintail-error, ${c.tainted} tainted ` +
        `(${c.setup} setup ok, ${c['setup-rejected']} rejected) in ${((performance.now() - started) / 1000).toFixed(1)}s`,
    )
  }
  publish(results, mysqlVersion)
  const exact = results.reduce((s, r) => s + r.counts.exact, 0)
  const compared = results.reduce((s, r) => s + r.counts.exact + r.counts.mismatch + r.counts['name-mismatch'], 0)
  log(`${exact} of ${compared} compared SELECTs exact; report at ${join(import.meta.dir, 'results.md')}`)
}

async function teardown() {
  await mysqlRoot?.end().catch(() => {})
  if (pintailProcess) pintailProcess.kill()
  if (mysqlStarted) await docker('rm', '--force', '--volumes', mysqlName).catch(() => {})
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
}

try {
  await main()
} catch (error) {
  console.error(`[mtr] FAILED: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await teardown()
}

#!/usr/bin/env bun

import { readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import mysql, { type Connection } from 'mysql2/promise'

type QueryClass = 'read' | 'session' | 'ignored'

export interface CapturedShape {
  query: string
  shape: string
  count: number
  class: QueryClass
}

export interface QueryOutcome {
  ok: boolean
  fields?: string[]
  rows?: unknown[]
  error?: {
    code?: string
    errno?: number
    sqlState?: string
    message: string
  }
}

export interface ReplayEntry extends CapturedShape {
  mysql?: QueryOutcome
  pintail?: QueryOutcome
  status: string
}

export interface SanitizedQueryOutcome {
  ok: boolean
  fields?: string[]
  rowCount?: number
  error?: QueryOutcome['error']
}

export interface SanitizedReplayEntry extends Omit<ReplayEntry, 'mysql' | 'pintail'> {
  mysql?: SanitizedQueryOutcome
  pintail?: SanitizedQueryOutcome
}

const READ_KEYWORDS = new Set([
  'SELECT',
  'WITH',
  'SHOW',
  'DESCRIBE',
  'DESC',
  'EXPLAIN',
])
const SESSION_KEYWORDS = new Set(['SET', 'USE'])

function stripLeadingComments(sql: string): string {
  let rest = sql
  while (rest.length > 0) {
    rest = rest.trimStart()
    if (rest.startsWith('--') || rest.startsWith('#')) {
      const newline = rest.indexOf('\n')
      rest = newline < 0 ? '' : rest.slice(newline + 1)
      continue
    }
    if (rest.startsWith('/*')) {
      const end = rest.indexOf('*/', 2)
      rest = end < 0 ? '' : rest.slice(end + 2)
      continue
    }
    break
  }
  return rest.trim()
}

function queryClass(sql: string): QueryClass {
  const stripped = stripLeadingComments(sql)
  const keyword = stripped.match(/^([A-Za-z]+)/u)?.[1]?.toUpperCase()
  if (keyword === 'WITH') {
    const topLevelWords = topLevelKeywordWords(stripped)
    const operation = topLevelWords.find(
      (word, index) =>
        index > 0 &&
        (READ_KEYWORDS.has(word) || ['INSERT', 'UPDATE', 'DELETE', 'REPLACE'].includes(word)),
    )
    return operation && READ_KEYWORDS.has(operation) ? 'read' : 'ignored'
  }
  if (keyword && READ_KEYWORDS.has(keyword)) return 'read'
  if (keyword && SESSION_KEYWORDS.has(keyword)) {
    const scope = topLevelKeywordWords(stripped)[1]
    if (scope && ['GLOBAL', 'PERSIST', 'PERSIST_ONLY', 'PASSWORD'].includes(scope)) {
      return 'ignored'
    }
    return 'session'
  }
  return 'ignored'
}

export function splitSql(text: string): string[] {
  const statements: string[] = []
  let start = 0
  let quote = ''
  let lineComment = false
  let blockComment = false
  for (let index = 0; index < text.length; index += 1) {
    const current = text[index]
    const next = text[index + 1] ?? ''
    if (lineComment) {
      if (current === '\n') lineComment = false
      continue
    }
    if (blockComment) {
      if (current === '*' && next === '/') {
        blockComment = false
        index += 1
      }
      continue
    }
    if (quote) {
      if (current === '\\') {
        index += 1
      } else if (current === quote) {
        if (text[index + 1] === quote) index += 1
        else quote = ''
      }
      continue
    }
    if (current === "'" || current === '"' || current === '`') {
      quote = current
    } else if ((current === '-' && next === '-') || current === '#') {
      lineComment = true
      if (current === '-') index += 1
    } else if (current === '/' && next === '*') {
      blockComment = true
      index += 1
    } else if (current === ';') {
      const statement = text.slice(start, index).trim()
      if (statement) statements.push(statement)
      start = index + 1
    }
  }
  const tail = text.slice(start).trim()
  if (tail) statements.push(tail)
  return statements
}

function queryFromRecord(line: string): string | undefined {
  try {
    const value = JSON.parse(line) as Record<string, unknown>
    const command = String(value.command_type ?? value.command ?? 'Query')
    if (!/^(Query|Execute)$/iu.test(command)) return undefined
    for (const key of ['argument', 'sql', 'query', 'statement']) {
      if (typeof value[key] === 'string') return value[key]
    }
  } catch {
    // Not JSONL; try exported general-log text below.
  }
  const tab = line.match(/\t(?:Query|Execute)\t([\s\S]+)$/u)
  if (tab) return tab[1]
  return line.match(/\b(?:Query|Execute)\s+([\s\S]+)$/u)?.[1]
}

export function capturedQueries(text: string): string[] {
  const records = text
    .split(/\r?\n/u)
    .map(queryFromRecord)
    .filter((query): query is string => query !== undefined)
  if (records.length > 0) return records.flatMap(splitSql)
  return splitSql(text)
}

export function redactSql(sql: string): string {
  return sql
    .replace(/(?:\b0x[0-9a-f]+|x'[0-9a-f]*')/giu, '?')
    .replace(/'(?:[^'\\]|\\.|'')*'/gu, "'?'")
    .replace(/\b\d+(?:\.\d+)?(?:e[+-]?\d+)?\b/giu, '?')
    .replace(/\s+/gu, ' ')
    .trim()
}

export function extractShapes(text: string): CapturedShape[] {
  const shapes = new Map<string, CapturedShape>()
  for (const query of capturedQueries(text)) {
    const normalized = query.trim().replace(/;+\s*$/u, '')
    if (!normalized) continue
    const shape = redactSql(normalized)
    const classification = queryClass(normalized)
    const key = `${classification}\0${shape.toUpperCase()}`
    const existing = shapes.get(key)
    if (existing) existing.count += 1
    else shapes.set(key, { query: normalized, shape, count: 1, class: classification })
  }
  return [...shapes.values()].sort(
    (left, right) => right.count - left.count || left.shape.localeCompare(right.shape),
  )
}

function canonical(value: unknown): unknown {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number') {
    if (Number.isNaN(value)) return 'NaN'
    if (!Number.isFinite(value)) return value > 0 ? 'Infinity' : '-Infinity'
    return Object.is(value, -0) ? 0 : value
  }
  if (typeof value === 'bigint') return value.toString()
  if (value instanceof Date) return value.toISOString()
  if (Buffer.isBuffer(value)) return { binary: value.toString('base64') }
  if (Array.isArray(value)) return value.map(canonical)
  if (typeof value === 'object') {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>).map(([key, item]) => [key, canonical(item)]),
    )
  }
  return String(value)
}

function topLevelKeywordWords(sql: string): string[] {
  let depth = 0
  let quote = ''
  let lineComment = false
  let blockComment = false
  let word = ''
  const words: string[] = []
  const finishWord = () => {
    if (word) words.push(word)
    word = ''
  }
  for (let index = 0; index < sql.length; index += 1) {
    const current = sql[index]
    const next = sql[index + 1] ?? ''
    if (lineComment) {
      if (current === '\n') lineComment = false
      continue
    }
    if (blockComment) {
      if (current === '*' && next === '/') {
        blockComment = false
        index += 1
      }
      continue
    }
    if (quote) {
      if (current === '\\') index += 1
      else if (current === quote) {
        if (next === quote) index += 1
        else quote = ''
      }
      continue
    }
    if (current === "'" || current === '"' || current === '`') {
      quote = current
      continue
    }
    if ((current === '-' && next === '-') || current === '#') {
      finishWord()
      lineComment = true
      if (current === '-') index += 1
      continue
    }
    if (current === '/' && next === '*') {
      finishWord()
      blockComment = true
      index += 1
      continue
    }
    if (current === '(') {
      finishWord()
      depth += 1
    } else if (current === ')') {
      finishWord()
      depth = Math.max(0, depth - 1)
    } else if (depth === 0 && /[A-Za-z_]/u.test(current)) {
      word += current.toUpperCase()
    } else if (depth === 0) {
      finishWord()
    }
  }
  finishWord()
  return words
}

function topLevelOrderBy(sql: string): boolean {
  const words = topLevelKeywordWords(sql)
  return words.some((word, index) => word === 'ORDER' && words[index + 1] === 'BY')
}

async function execute(connection: Connection, sql: string): Promise<QueryOutcome> {
  try {
    const [rows, fields] = await connection.query({ sql, timeout: 30_000, rowsAsArray: true })
    const normalized = canonical(rows) as unknown[]
    if (Array.isArray(normalized) && !topLevelOrderBy(sql)) {
      normalized.sort((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)))
    }
    return {
      ok: true,
      fields: fields?.map((field) => field.name),
      rows: normalized,
    }
  } catch (error) {
    const failure = error as {
      code?: string
      errno?: number
      sqlState?: string
      message?: string
    }
    return {
      ok: false,
      error: {
        code: failure.code,
        errno: failure.errno,
        sqlState: failure.sqlState,
        message: failure.message ?? String(error),
      },
    }
  }
}

function status(mysqlResult: QueryOutcome, pintailResult: QueryOutcome, classification: QueryClass): string {
  if (classification === 'session') {
    if (mysqlResult.ok && pintailResult.ok) return 'session_match'
    if (mysqlResult.ok) return 'pintail_reject'
    if (pintailResult.ok) return 'pintail_accepts_mysql_reject'
    return 'both_reject'
  }
  if (mysqlResult.ok && !pintailResult.ok) return 'pintail_reject'
  if (!mysqlResult.ok && pintailResult.ok) return 'pintail_accepts_mysql_reject'
  if (!mysqlResult.ok && !pintailResult.ok) return 'both_reject'
  return JSON.stringify(mysqlResult) === JSON.stringify(pintailResult) ? 'match' : 'result_mismatch'
}

async function connect(dsn: string): Promise<Connection> {
  const url = new URL(dsn)
  if (url.protocol !== 'mysql:') throw new Error('BI replay DSNs must use the mysql:// scheme')
  return mysql.createConnection({
    host: url.hostname,
    port: url.port ? Number(url.port) : 3306,
    user: decodeURIComponent(url.username),
    password: decodeURIComponent(url.password),
    database: decodeURIComponent(url.pathname.replace(/^\//u, '')) || undefined,
    rowsAsArray: true,
    dateStrings: true,
    supportBigNumbers: true,
    bigNumberStrings: true,
  })
}

function sanitizedOutcome(outcome: QueryOutcome | undefined): SanitizedQueryOutcome | undefined {
  if (!outcome) return undefined
  if (outcome.ok) {
    return {
      ok: true,
      fields: outcome.fields,
      rowCount: Array.isArray(outcome.rows) ? outcome.rows.length : undefined,
    }
  }
  return {
    ok: false,
    error: outcome.error && {
      ...outcome.error,
      message: redactSql(outcome.error.message),
    },
  }
}

export function sanitized(entries: ReplayEntry[]): SanitizedReplayEntry[] {
  return entries.map((entry) => ({
    ...entry,
    query: entry.shape,
    mysql: sanitizedOutcome(entry.mysql),
    pintail: sanitizedOutcome(entry.pintail),
  }))
}

function argument(name: string): string | undefined {
  const index = process.argv.indexOf(name)
  return index >= 0 ? process.argv[index + 1] : undefined
}

async function main(): Promise<void> {
  const input = argument('--input')
  const report = argument('--report')
  if (!input || !report) {
    throw new Error(
      'usage: bun run bi-dogfood.ts --input CAPTURE --report LOCAL_REPORT.raw.json (optional replay: BI_MYSQL_DSN + BI_PINTAIL_DSN)',
    )
  }
  if (!report.endsWith('.raw.json')) {
    throw new Error('--report must end in .raw.json so the exact report remains gitignored')
  }
  const shapes = extractShapes(readFileSync(resolve(input), 'utf8'))
  const mysqlDsn = process.env.BI_MYSQL_DSN
  const pintailDsn = process.env.BI_PINTAIL_DSN
  const entries: ReplayEntry[] = shapes.map((shape) => ({
    ...shape,
    status: shape.class === 'ignored' ? 'ignored_non_read' : 'captured',
  }))
  if (mysqlDsn && pintailDsn) {
    const mysqlConnection = await connect(mysqlDsn)
    const pintailConnection = await connect(pintailDsn)
    try {
      for (const entry of entries) {
        if (entry.class === 'ignored') continue
        entry.mysql = await execute(mysqlConnection, entry.query)
        entry.pintail = await execute(pintailConnection, entry.query)
        entry.status = status(entry.mysql, entry.pintail, entry.class)
      }
    } finally {
      await Promise.all([mysqlConnection.end(), pintailConnection.end()])
    }
  }
  const output = resolve(report)
  writeFileSync(output, `${JSON.stringify(entries, null, 2)}\n`)
  writeFileSync(
    output.replace(/\.raw\.json$/u, '.sanitized.json'),
    `${JSON.stringify(sanitized(entries), null, 2)}\n`,
  )
  const counts = new Map<string, number>()
  for (const entry of entries) counts.set(entry.status, (counts.get(entry.status) ?? 0) + 1)
  console.log(
    [...counts.entries()]
      .sort(([left], [right]) => left.localeCompare(right))
      .map(([name, count]) => `${name}: ${count}`)
      .join('\n'),
  )
}

if (import.meta.main) await main()

import { isDeepStrictEqual } from 'node:util'

export interface MysqlEndpoint {
  host: string
  port: number
  user: string
  password: string
  database: string
}

export interface OrmCompatibilityResult {
  client: 'sequelize' | 'drizzle' | 'prisma'
  check: string
  status: 'PASS' | 'FAIL'
  detail?: string
}

export interface Captured<T> {
  value: T
  sql: string[]
}

function normalizedSql(sql: string): string {
  return sql
    .replace(/^Executing \([^)]*\):\s*/u, '')
    .replace(/\s+/gu, ' ')
    .trim()
}

export function canonical(value: unknown): unknown {
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
    const candidate = value as { toJSON?: () => unknown }
    if (typeof candidate.toJSON === 'function') {
      const json = candidate.toJSON()
      if (json !== value) return canonical(json)
    }
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, item]) => [key, canonical(item)]),
    )
  }
  return String(value)
}

function rendered(value: unknown): string {
  return JSON.stringify(canonical(value))
}

export function compareCaptured<T>(
  client: OrmCompatibilityResult['client'],
  check: string,
  mysql: Captured<T>,
  pintail: Captured<T>,
): OrmCompatibilityResult[] {
  const mysqlValue = canonical(mysql.value)
  const pintailValue = canonical(pintail.value)
  const mysqlSql = mysql.sql.map(normalizedSql)
  const pintailSql = pintail.sql.map(normalizedSql)
  const valuesMatch = isDeepStrictEqual(mysqlValue, pintailValue)
  const sqlMatches = isDeepStrictEqual(mysqlSql, pintailSql)
  return [
    {
      client,
      check: `${check}:result`,
      status: valuesMatch ? 'PASS' : 'FAIL',
      detail: valuesMatch
        ? undefined
        : `mysql ${rendered(mysqlValue)}\npintail ${rendered(pintailValue)}`,
    },
    {
      client,
      check: `${check}:generated-sql`,
      status: sqlMatches ? 'PASS' : 'FAIL',
      detail: sqlMatches
        ? undefined
        : `mysql ${rendered(mysqlSql)}\npintail ${rendered(pintailSql)}`,
    },
  ]
}

export async function captureFailure(
  client: OrmCompatibilityResult['client'],
  check: string,
  run: () => Promise<OrmCompatibilityResult[]>,
): Promise<OrmCompatibilityResult[]> {
  try {
    return await run()
  } catch (error) {
    return [{ client, check, status: 'FAIL', detail: String(error) }]
  }
}

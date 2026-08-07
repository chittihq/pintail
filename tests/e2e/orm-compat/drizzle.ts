import { mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { and, asc, count, eq, gte, isNotNull, lte, sum } from 'drizzle-orm'
import { drizzle, type MySql2Database } from 'drizzle-orm/mysql2'
import {
  bigint,
  date,
  decimal,
  int,
  mysqlEnum,
  mysqlTable,
  varchar,
} from 'drizzle-orm/mysql-core'
import mysql from 'mysql2/promise'
import {
  captureFailure,
  compareCaptured,
  type Captured,
  type MysqlEndpoint,
  type OrmCompatibilityResult,
} from './common'

const customers = mysqlTable('customers', {
  id: int('id', { unsigned: true }).primaryKey().autoincrement(),
  name: varchar('name', { length: 64 }).notNull(),
  email: varchar('email', { length: 96 }),
  tier: mysqlEnum('tier', ['free', 'pro', 'enterprise']).notNull(),
  balance: decimal('balance', { precision: 12, scale: 2 }).notNull(),
})

const orders = mysqlTable('orders', {
  id: bigint('id', { mode: 'bigint', unsigned: true }).primaryKey().autoincrement(),
  customerId: int('customer_id', { unsigned: true }).notNull(),
  status: mysqlEnum('status', [
    'pending',
    'processing',
    'shipped',
    'delivered',
    'cancelled',
  ]).notNull(),
  total: decimal('total', { precision: 12, scale: 2 }).notNull(),
  placedOn: date('placed_on', { mode: 'string' }).notNull(),
})

const schema = { customers, orders }

async function withClient<T>(
  endpoint: MysqlEndpoint,
  run: (db: MySql2Database<typeof schema>) => Promise<T>,
): Promise<Captured<T>> {
  const pool = mysql.createPool({
    ...endpoint,
    supportBigNumbers: true,
    bigNumberStrings: true,
    dateStrings: true,
    connectionLimit: 1,
  })
  const statements: string[] = []
  const db = drizzle(pool, {
    schema,
    mode: 'default',
    logger: { logQuery: (query) => statements.push(query) },
  })
  try {
    return { value: await run(db), sql: statements }
  } finally {
    await pool.end()
  }
}

async function parity<T>(
  check: string,
  mysqlEndpoint: MysqlEndpoint,
  pintailEndpoint: MysqlEndpoint,
  run: (db: MySql2Database<typeof schema>) => Promise<T>,
): Promise<OrmCompatibilityResult[]> {
  return captureFailure('drizzle', check, async () => {
    const expected = await withClient(mysqlEndpoint, run)
    const actual = await withClient(pintailEndpoint, run)
    return compareCaptured('drizzle', check, expected, actual)
  })
}

function connectionUrl(endpoint: MysqlEndpoint): string {
  const user = encodeURIComponent(endpoint.user)
  const password = encodeURIComponent(endpoint.password)
  const database = encodeURIComponent(endpoint.database)
  return `mysql://${user}:${password}@${endpoint.host}:${endpoint.port}/${database}`
}

function artifacts(directory: string, root = directory): Record<string, string> {
  const result: Record<string, string> = {}
  for (const entry of readdirSync(directory).sort()) {
    const path = join(directory, entry)
    if (statSync(path).isDirectory()) Object.assign(result, artifacts(path, root))
    else {
      const relative = path.slice(root.length + 1)
      result[relative] = readFileSync(path, 'utf8').replaceAll('\r\n', '\n')
    }
  }
  return result
}

async function introspect(endpoint: MysqlEndpoint): Promise<Captured<Record<string, string>>> {
  const temporary = mkdtempSync(join(tmpdir(), 'pintail-drizzle-'))
  const output = join(temporary, 'schema')
  const executable = resolve(import.meta.dir, '..', 'node_modules', '.bin', 'drizzle-kit')
  try {
    const child = Bun.spawn(
      [
        executable,
        'pull',
        '--dialect=mysql',
        `--url=${connectionUrl(endpoint)}`,
        `--out=${output}`,
        '--introspect-casing=preserve',
      ],
      { stdout: 'pipe', stderr: 'pipe' },
    )
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(child.stdout).text(),
      new Response(child.stderr).text(),
      child.exited,
    ])
    if (exitCode !== 0) {
      throw new Error(`drizzle-kit pull failed (${exitCode}): ${stderr.trim() || stdout.trim()}`)
    }
    return { value: artifacts(output), sql: [] }
  } finally {
    rmSync(temporary, { recursive: true, force: true })
  }
}

export async function runDrizzleCompatibility(
  mysqlEndpoint: MysqlEndpoint,
  pintailEndpoint: MysqlEndpoint,
): Promise<OrmCompatibilityResult[]> {
  const results: OrmCompatibilityResult[] = []
  results.push(
    ...(await captureFailure('drizzle', 'introspection', async () => {
      const expected = await introspect(mysqlEndpoint)
      const actual = await introspect(pintailEndpoint)
      return compareCaptured('drizzle', 'introspection', expected, actual)
    })),
  )
  results.push(
    ...(await parity('point-and-filtered-reads', mysqlEndpoint, pintailEndpoint, async (db) => {
      const point = await db
        .select({
          id: customers.id,
          name: customers.name,
          email: customers.email,
          tier: customers.tier,
          balance: customers.balance,
        })
        .from(customers)
        .where(eq(customers.id, 7))
        .limit(1)
      const filtered = await db
        .select({ id: customers.id, name: customers.name, balance: customers.balance })
        .from(customers)
        .where(and(gte(customers.balance, '0'), isNotNull(customers.email)))
        .orderBy(asc(customers.id))
        .limit(5)
        .offset(1)
      return { point, filtered }
    })),
  )
  results.push(
    ...(await parity('relation-read', mysqlEndpoint, pintailEndpoint, async (db) =>
      db
        .select({
          customerId: customers.id,
          customerName: customers.name,
          orderId: orders.id,
          orderTotal: orders.total,
        })
        .from(customers)
        .leftJoin(orders, eq(orders.customerId, customers.id))
        .where(lte(customers.id, 3))
        .orderBy(asc(customers.id), asc(orders.id)),
    )),
  )
  results.push(
    ...(await parity('grouped-aggregate', mysqlEndpoint, pintailEndpoint, async (db) =>
      db
        .select({
          customerId: orders.customerId,
          orderCount: count(orders.id),
          orderTotal: sum(orders.total),
        })
        .from(orders)
        .groupBy(orders.customerId)
        .having(gte(count(orders.id), 2))
        .orderBy(asc(orders.customerId))
        .limit(10),
    )),
  )
  return results
}

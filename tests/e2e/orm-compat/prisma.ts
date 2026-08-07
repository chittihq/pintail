import { PrismaMariaDb } from '@prisma/adapter-mariadb'
import { resolve } from 'node:path'
import {
  captureFailure,
  compareCaptured,
  type Captured,
  type MysqlEndpoint,
  type OrmCompatibilityResult,
} from './common'
import { PrismaClient } from './generated/prisma/client'

async function withClient<T>(
  endpoint: MysqlEndpoint,
  run: (client: PrismaClient) => Promise<T>,
): Promise<Captured<T>> {
  const adapter = new PrismaMariaDb(
    {
      host: endpoint.host,
      port: endpoint.port,
      user: endpoint.user,
      password: endpoint.password,
      database: endpoint.database,
      connectionLimit: 1,
      bigIntAsNumber: false,
      dateStrings: true,
    },
    { database: endpoint.database },
  )
  const statements: string[] = []
  const client = new PrismaClient({ adapter, log: [{ emit: 'event', level: 'query' }] })
  client.$on('query', (event) => statements.push(`${event.query} -- params ${event.params}`))
  try {
    await client.$connect()
    statements.length = 0
    try {
      return { value: await run(client), sql: statements }
    } catch (error) {
      throw new Error(`${error}\ngenerated statements: ${JSON.stringify(statements)}`)
    }
  } finally {
    await client.$disconnect()
  }
}

async function parity<T>(
  check: string,
  mysqlEndpoint: MysqlEndpoint,
  pintailEndpoint: MysqlEndpoint,
  run: (client: PrismaClient) => Promise<T>,
): Promise<OrmCompatibilityResult[]> {
  return captureFailure('prisma', check, async () => {
    const expected = await withClient(mysqlEndpoint, run)
    const actual = await withClient(pintailEndpoint, run)
    return compareCaptured('prisma', check, expected, actual)
  })
}

function connectionUrl(endpoint: MysqlEndpoint): string {
  const user = encodeURIComponent(endpoint.user)
  const password = encodeURIComponent(endpoint.password)
  const database = encodeURIComponent(endpoint.database)
  return `mysql://${user}:${password}@${endpoint.host}:${endpoint.port}/${database}`
}

function normalizeSchema(schema: string): string {
  return schema
    .replaceAll('\r\n', '\n')
    .split('\n')
    .map((line) => line.trimEnd())
    .join('\n')
    .trim()
}

async function introspect(endpoint: MysqlEndpoint): Promise<Captured<string>> {
  const executable = resolve(import.meta.dir, '..', 'node_modules', '.bin', 'prisma')
  const schema = resolve(import.meta.dir, 'prisma', 'schema.prisma')
  const child = Bun.spawn(
    [
      executable,
      'db',
      'pull',
      '--print',
      `--url=${connectionUrl(endpoint)}`,
      `--schema=${schema}`,
    ],
    { stdout: 'pipe', stderr: 'pipe' },
  )
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (exitCode !== 0) {
    const safeError = (stderr.trim() || stdout.trim()).replaceAll(endpoint.password, '<redacted>')
    throw new Error(`prisma db pull failed (${exitCode}): ${safeError}`)
  }
  return { value: normalizeSchema(stdout), sql: [] }
}

export async function runPrismaCompatibility(
  mysqlEndpoint: MysqlEndpoint,
  pintailEndpoint: MysqlEndpoint,
): Promise<OrmCompatibilityResult[]> {
  const results: OrmCompatibilityResult[] = []
  results.push(
    ...(await captureFailure('prisma', 'introspection', async () => {
      const expected = await introspect(mysqlEndpoint)
      const actual = await introspect(pintailEndpoint)
      return compareCaptured('prisma', 'introspection', expected, actual)
    })),
  )
  results.push(
    ...(await parity('point-and-filtered-reads', mysqlEndpoint, pintailEndpoint, async (client) => {
      const point = await client.customer.findUnique({
        where: { id: 7 },
        select: { id: true, name: true, email: true, tier: true, balance: true },
      })
      const filtered = await client.customer.findMany({
        where: { balance: { gte: 0 }, email: { not: null } },
        select: { id: true, name: true, balance: true },
        orderBy: { id: 'asc' },
        skip: 1,
        take: 5,
      })
      return { point, filtered }
    })),
  )
  results.push(
    ...(await parity('relation-read', mysqlEndpoint, pintailEndpoint, async (client) =>
      client.customer.findMany({
        where: { id: { lte: 3 } },
        select: {
          id: true,
          name: true,
          orders: {
            select: { id: true, total: true },
            orderBy: { id: 'asc' },
          },
        },
        orderBy: { id: 'asc' },
      }),
    )),
  )
  results.push(
    ...(await parity('grouped-aggregate', mysqlEndpoint, pintailEndpoint, async (client) =>
      client.order.groupBy({
        by: ['customerId'],
        _count: { id: true },
        _sum: { total: true },
        having: { id: { _count: { gte: 2 } } },
        orderBy: { customerId: 'asc' },
        take: 10,
      }),
    )),
  )
  return results
}

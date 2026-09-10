/// Replays unchanged TPC-H queries against an existing synthetic replica and
/// its MySQL source. Setup and snapshot work stay outside query timing.
/// The private session JSON contains {url, token, db, mysql:{host,port,user,
/// password,database}}. Keep it outside the repository and never bank it.
import { createHash } from 'node:crypto'
import { readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import mysql from 'mysql2/promise'
import workload from './workloads/tpch-v1/workload'

function arg(name: string, fallback = ''): string {
  const i = process.argv.indexOf(`--${name}`)
  return i < 0 ? fallback : process.argv[i + 1] ?? fallback
}
function required(name: string): string {
  const value = arg(name)
  if (!value) throw new Error(`--${name} is required`)
  return value
}
const runs = Number(arg('runs', '5'))
const warmups = Number(arg('warmups', '2'))
if (!Number.isInteger(runs) || runs < 1 || !Number.isInteger(warmups) || warmups < 0) {
  throw new Error('runs must be positive and warmups nonnegative integers')
}
const session = JSON.parse(readFileSync(required('session'), 'utf8'))
const revision = required('revision')
const pid = required('pid')
if (!/^\d+$/.test(pid)) throw new Error('pid must identify the measured Linux server process')
const environment = Object.fromEntries(readFileSync(`/proc/${pid}/environ`, 'utf8')
  .split('\0').filter(Boolean).map(entry => {
    const i = entry.indexOf('='); return [entry.slice(0, i), entry.slice(i + 1)]
  }))
if (environment.PINTAIL_DISABLE_SETTLED_MEMO !== '1') throw new Error('settled result memo must be disabled')
const hash = (value: string | Buffer) => createHash('sha256').update(value).digest('hex')
const binarySha256 = hash(readFileSync(`/proc/${pid}/exe`))
const selected = arg('queries').split(',').filter(Boolean)
const queries = workload.queries.filter(q => selected.length === 0 || selected.includes(q.id))
if (!queries.length || selected.some(id => !queries.some(q => q.id === id))) throw new Error('unknown query id')
const connection = await mysql.createConnection({
  ...session.mysql, supportBigNumbers: true, bigNumberStrings: true, dateStrings: true,
})
const normalize = (rows: unknown[][]) => rows.map(row => row.map(value => value === null ? null : String(value)))
async function query(sql: string) {
  const start = performance.now()
  const response = await fetch(`${session.url}/api/query`, {
    method: 'POST', headers: {'content-type':'application/json', Authorization:`Bearer ${session.token}`},
    body: JSON.stringify({db:session.db, sql}),
  })
  const result = await response.json() as {rows:unknown[][], truncated?:boolean, stats:unknown}
  if (!response.ok || result.truncated) throw new Error(`query failed (${response.status}): ${JSON.stringify(result)}`)
  return {ms:performance.now()-start, result}
}
const outcomes = []
const startedAt = new Date().toISOString()
try {
  for (const spec of queries) {
    let sql = readFileSync(join(import.meta.dir, 'workloads/tpch-v1', spec.sqlFile), 'utf8')
    for (const [name, value] of Object.entries(spec.params)) {
      sql = sql.replaceAll(`:${name}`, typeof value === 'number' ? String(value) : `'${value.replaceAll("'", "''")}'`)
    }
    const timings = []
    let answer: unknown[][] = []
    for (let run = -warmups; run < runs; run++) {
      const sampleStartedAt = new Date().toISOString()
      const start = performance.now()
      const [expected] = await connection.query<mysql.RowDataPacket[]>({sql,rowsAsArray:true})
      const mysqlMs = performance.now()-start
      const measured = await query(sql)
      answer = normalize(measured.result.rows)
      if (JSON.stringify(answer) !== JSON.stringify(normalize(expected as unknown as unknown[][]))) {
        throw new Error(`${spec.id}: byte mismatch against MySQL`)
      }
      if (run >= 0) timings.push({startedAt:sampleStartedAt,finishedAt:new Date().toISOString(),mysqlMs,pintailMs:measured.ms,stats:measured.result.stats})
    }
    const ordered = timings.map(t => t.pintailMs).sort((a,b) => a-b)
    const profile = process.argv.includes('--analyze') ? (await query(`EXPLAIN ANALYZE ${sql}`)).result : undefined
    outcomes.push({id:spec.id,sqlSha256:hash(sql),exact:true,answer,answerSha256:hash(JSON.stringify(answer)),
      timings,medianMs:ordered[Math.floor(ordered.length/2)],p95Ms:ordered[Math.ceil(ordered.length*0.95)-1],profile})
    console.log(`${spec.id}: exact; median=${outcomes.at(-1)!.medianMs.toFixed(2)}ms p95=${outcomes.at(-1)!.p95Ms.toFixed(2)}ms`)
  }
  const counts: Record<string,unknown> = {}
  for (const table of ['region','nation','supplier','part','partsupp','customer','orders','lineitem']) {
    const [rows] = await connection.query<mysql.RowDataPacket[]>(`SELECT COUNT(*) AS n FROM ${table}`)
    counts[table] = String(rows[0].n)
  }
  const [settings] = await connection.query<mysql.RowDataPacket[]>('SELECT @@innodb_buffer_pool_size AS bytes')
  writeFileSync(required('out'), JSON.stringify({revision,binarySha256,startedAt,finishedAt:new Date().toISOString(),
    runs,warmups,settledMemo:false,queryMemoryBytes:environment.PINTAIL_QUERY_MEMORY_LIMIT_BYTES,
    querySpillBytes:environment.PINTAIL_QUERY_SPILL_LIMIT_BYTES ?? '1073741824',
    globalSpillBytes:environment.PINTAIL_GLOBAL_SPILL_LIMIT_BYTES ?? '8589934592',
    mysqlBufferPoolBytes:String(settings[0].bytes),counts,outcomes},null,2)+'\n')
  const target = Number(arg('target-ms','0'))
  if (target > 0 && outcomes.some(q => q.p95Ms > target)) throw new Error(`p95 exceeds ${target}ms target`)
  console.log('TPC-H-REPLAY-DONE')
} finally {
  await connection.end()
}

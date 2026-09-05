import mysql from 'mysql2/promise'
import { until, type Context, type Scenario } from '../harness'
import { dsnHost } from '../../lib'
import { exactDiff } from '../policy'
import { seedBig } from './schema'
import { purgeWhileDown } from './purge'

/** docs/design/recovery-suite.md §8; the API's supervisor contains a failed source; recovery promises catch-up, not a retry cap. */
async function bystander(ctx: Context) {
  const schema = `${ctx.schema}_other`
  ctx.additionalSchemas.add(schema)
  await ctx.source.root.query({sql:`CREATE DATABASE \`${schema}\`; CREATE TABLE \`${schema}\`.events(id BIGINT PRIMARY KEY,value VARCHAR(32)); INSERT INTO \`${schema}\`.events VALUES(1,'before')`,timeout:15_000})
  const database = await ctx.api<{id:string}>('/api/databases', { method:'POST', body:{ name:schema,
    dsn:`mysql://pintail:pintail@${dsnHost(ctx.source.host)}:${ctx.source.port}/${schema}`, mode:'cdc' } })
  const path = `/api/databases/${database.id}`
  await ctx.api(`${path}/probe`)
  const key = await ctx.api<{secret:string}>(`${path}/api-keys`, { method:'POST', body:{name:'bystander',scopes:['query','read']} })
  await ctx.api(`${path}/snapshot`, { method:'POST', body:{force:false} })
  let connection: mysql.Connection | undefined
  let generation = 1
  let running = true
  let failure: unknown
  const writer = await ctx.source.connect(schema)
  let committed = 10
  const task = (async () => {
    while (running) {
      const n = committed + 1
      try { await writer.query({sql:"INSERT INTO events VALUES(?,'continuous')",timeout:15_000}, [n]); committed = n }
      catch (error) { failure = error; return }
      await Bun.sleep(50)
    }
  })()
  const verify = async () => {
    const before = committed
    await until('bystander writer continues', async () => { if (failure) throw failure; return committed > before })
    const n = ++generation
    await ctx.source.root.query({sql:`INSERT INTO \`${schema}\`.events VALUES(${-n},'during-outage'); UPDATE \`${schema}\`.events SET value='updated' WHERE id=1`,timeout:15_000})
    // A stable prefix can compare exactly while later source rows keep arriving.
    const maximum = committed
    await until('bystander receives writes during victim outage', async () => {
      try {
        connection ??= await mysql.createConnection({ host:'127.0.0.1',port:ctx.wirePort,user:schema,password:key.secret,database:schema })
        const [a] = await writer.query({sql:`SELECT * FROM events WHERE id<=${maximum} ORDER BY id`,rowsAsArray:true,timeout:15_000})
        const [b] = await connection.query({sql:`SELECT * FROM events WHERE id<=${maximum} ORDER BY id`,rowsAsArray:true,timeout:15_000})
        return exactDiff(a as unknown[][], b as unknown[][]) === undefined
      } catch(error) { connection?.destroy(); connection=undefined; throw error }
    })
    ctx.check(`bystander:live-through-outage-${n}`, true, `source commits continued; exact prefix through ${maximum}`)
  }
  // Leave the schema reachable until Context stops Pintail, then drop it
  // with the other owned schemas. Teardown must not manufacture an outage.
  return { verify, close: async () => { running=false; await task; writer.destroy(); connection?.destroy(); if(failure) throw failure } }
}
async function outage(ctx: Context, repeat: number, query?: RegExp) {
  if (!ctx.proxy) throw new Error('victim proxy missing')
  const other = await bystander(ctx)
  try {
    await other.verify()
    for (let i = 0; i < repeat; i++) {
      const before = ctx.commits
      const known = new Set((await ctx.activity()).map(r => r.id))
      if (query) {
        ctx.proxy.cutOnQuery(query)
        await until('scheduled source query interrupted', async () => ctx.proxy!.blocked && !!ctx.proxy!.lastCutQuery)
        ctx.check('outage:query-witness', true, ctx.proxy.lastCutQuery)
      } else ctx.proxy.cut()
      await until('source failure visible in a new run', async () => (await ctx.activity()).some(r => !known.has(r.id) && r.status === 'error'))
      ctx.check(`outage:${i}:error-visible`, true)
      await other.verify()
      await until('writer commits while victim cannot connect', async () => ctx.commits >= before + 10)
      ctx.check(`outage:${i}:source-writes-continue`, true)
      const reconcileBeforeRestore = query ? ctx.pollStates().find(s=>s.table_name==='accounts')?.last_reconcile_at : undefined
      ctx.proxy.restore()
      if (query) {
        await until('scheduled reconciliation recovers after outage',async()=>{
          const at=ctx.pollStates().find(s=>s.table_name==='accounts')?.last_reconcile_at
          return !!at && at!==reconcileBeforeRestore
        })
        ctx.check('outage:scheduled-reconciliation-completed',true)
      }
      await ctx.stopChurn()
      await ctx.converge(`outage-${i}-restored`)
      await ctx.startChurn()
    }
  } finally { ctx.proxy.restore(); await other.close() }
}
export const outageScenarios: Scenario[] = [
  { slug:'outage-during-cdc',area:'outage',proxy:true,promise:'crates/pintail-api/src/supervisor.rs: per-database failure containment',run:ctx=>outage(ctx,1) },
  { slug:'outage-repeated',area:'outage',proxy:true,promise:'docs/design/recovery-suite.md §8: repeated failures and eventual catch-up',run:ctx=>outage(ctx,3) },
  { slug:'outage-during-snapshot',area:'outage',proxy:true,seed:seedBig,promise:'crates/pintail-api/src/supervisor.rs: interrupted snapshot recovery',run:async ctx=>{
    const other = await bystander(ctx)
    try {
      await other.verify()
      await purgeWhileDown(ctx)
      const known = new Set(ctx.durable('before-snapshot-outage').sync_runs.map(r=>r.id))
      ctx.proxy!.cutOnQuery(/^SELECT `id`, `value` FROM .*`big`/i)
      await ctx.start()
      await until('snapshot source query interrupted', async()=>ctx.proxy!.blocked && !!ctx.proxy!.lastCutQuery)
      ctx.check('outage:snapshot-query-witness',true,ctx.proxy!.lastCutQuery)
      await until('snapshot failure visible while source unavailable',async()=> (await ctx.activity()).some(r=>!known.has(r.id)&&r.status==='error'))
      ctx.check('outage:interrupted-snapshot-source-error',true)
      await other.verify()
    } finally { ctx.proxy!.restore(); await other.close() }
  } },
  { slug:'outage-during-reconcile',area:'outage',mode:'polling',proxy:true,promise:'crates/pintail-poll/src/lib.rs: failed reconciliation does not commit state',run:async ctx=>{
    await outage(ctx,1,/^SELECT (?!COUNT).+ FROM [^ ]+\.`accounts` ORDER BY /i)
  } },
]

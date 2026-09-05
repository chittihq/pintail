import { operatorApi, until, type Context, type Scenario } from '../harness'
import { partialIsNotHealthy } from './purge'

async function post(ctx: Context, suffix: string) {
  const deadline = Date.now() + 60_000
  while (true) {
    try { await operatorApi(ctx, `${ctx.path}${suffix}`, { method: 'POST' }); return }
    catch (error) {
      if (!String(error).includes('HTTP 409:') || Date.now() >= deadline) throw error
      // Wait for the competing job to yield, never repeat an accepted reset.
      await Bun.sleep(100)
    }
  }
}
/** docs/limitations.md:245 and :473; supervisor.rs:48: explicit repair is separate from automatic recovery. */
export const operatorScenarios: Scenario[] = [
  { slug:'operator-poll-keyless-schema-quarantine',area:'operator',mode:'polling',promise:'crates/pintail-api/src/supervisor.rs: quarantine contains schema drift to one table',run:async ctx=>{
    await operatorApi(ctx,ctx.path,{method:'PUT',body:{name:ctx.schema,mode:'polling',keyless_policy:'quarantine',poll_interval_seconds:1,reconcile_interval_seconds:5}})
    await ctx.sql('ALTER TABLE audit MODIFY payload VARCHAR(128)')
    await until('keyless schema drift quarantined',async()=> (await ctx.status()).tables.some(t=>t.name==='audit'&&t.state==='needs_resync'))
    await ctx.sql("INSERT INTO accounts VALUES(800000,'quarantine-witness',1.00,NOW(6))")
    await until('healthy table continues while keyless table awaits repair',async()=>
      String((await ctx.replicaRows('SELECT id FROM accounts WHERE id=800000'))[0]?.[0])==='800000')
    ctx.check('quarantine:healthy-table-continues', (await ctx.status()).tables.some(t=>t.name==='audit'&&t.state==='needs_resync'))
    await ctx.restart('snapshot.chunk.after_ingest')
    await post(ctx,'/tables/audit/resync')
    await ctx.fired('snapshot.chunk.after_ingest')
    partialIsNotHealthy(ctx,'operator-poll-keyless-copy','table')
    await ctx.start()
    await until('interrupted keyless polling copy resumes automatically',async()=> (await ctx.status()).tables.some(t=>t.name==='audit'&&t.state==='polling'))
    ctx.check('operator:interrupted-copy-resumes-without-repost',true)
  } },
  { slug:'operator-resync-table-abort',area:'operator',promise:'docs/limitations.md: quarantined keyless table requires generation rebuild',run:async ctx=>{
    await operatorApi(ctx, ctx.path, {method:'PUT',body:{name:ctx.schema,mode:'cdc',keyless_policy:'quarantine',poll_interval_seconds:1,reconcile_interval_seconds:5}})
    await ctx.sql("UPDATE audit SET payload='quarantine' WHERE kind='seed'")
    await until('audit quarantined', async()=> (await ctx.status()).tables.some(t=>t.name==='audit'&&t.state==='needs_resync'))
    await ctx.restart('snapshot.chunk.after_ingest')
    await post(ctx,'/tables/audit/resync')
    await ctx.fired('snapshot.chunk.after_ingest')
    partialIsNotHealthy(ctx,'operator-table-copy','table')
    await ctx.start()
    await until('interrupted keyless CDC copy resumes automatically',async()=> (await ctx.status()).tables.some(t=>t.name==='audit'&&t.state==='streaming'))
    ctx.check('operator:interrupted-copy-resumes-without-repost',true)
    await operatorApi(ctx,ctx.path,{method:'PUT',body:{name:ctx.schema,mode:'cdc',keyless_policy:'auto_resync',poll_interval_seconds:1,reconcile_interval_seconds:5}})
  } },
  { slug:'operator-reset-abort',area:'operator',promise:'crates/pintail-api/src/snapshot.rs: reset and interrupted snapshot recovery',run:async ctx=>{
    await ctx.restart('snapshot.chunk.after_ingest@2')
    await post(ctx,'/reset')
    await ctx.fired('snapshot.chunk.after_ingest')
    partialIsNotHealthy(ctx,'operator-reset')
    await ctx.start()
  } },
  { slug:'operator-reconcile-abort',area:'operator',mode:'polling',promise:'crates/pintail-poll/src/lib.rs: reconciliation checkpoint follows WAL',run:async ctx=>{
    await until('all scheduled reconciliations completed', async()=>ctx.pollStates().length===3 && ctx.pollStates().every(t=>!!t.last_reconcile_at))
    await operatorApi(ctx,ctx.path,{method:'PUT',body:{name:ctx.schema,mode:'polling',keyless_policy:'auto_resync',poll_interval_seconds:60,reconcile_interval_seconds:3600}})
    await ctx.restart('poll.reconcile.before_state_commit')
    await post(ctx,'/tables/accounts/reconcile')
    await ctx.fired('poll.reconcile.before_state_commit')
    const state=ctx.durable('operator-reconcile')
    ctx.check('operator:reconcile-was-running',state.sync_runs.some(r=>r.kind==='reconcile'&&r.status==='running'))
    await ctx.start()
    await operatorApi(ctx,ctx.path,{method:'PUT',body:{name:ctx.schema,mode:'polling',keyless_policy:'auto_resync',poll_interval_seconds:1,reconcile_interval_seconds:5}})
  } },
]

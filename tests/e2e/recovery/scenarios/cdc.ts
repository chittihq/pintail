import { until, type Context, type Scenario } from '../harness'

/** GOAL.md:324 and pintail-cdc/src/lib.rs:4: all touched WALs precede the SQLite position. */
async function durability(ctx: Context, site: string, committed = false) {
  await ctx.stopChurn()
  await ctx.converge('before-fault')
  await ctx.stop()
  const before = ctx.durable('before-cdc-fault').checkpoints[0]
  await ctx.startChurn()
  const first = ctx.commits
  await until('multi-table transactions accumulate during downtime', async () => ctx.commits >= first + 2)
  await ctx.start(site)
  await ctx.fired(site)
  const after = ctx.durable('after-cdc-fault').checkpoints[0]
  if (committed) {
    ctx.check('checkpoint:advances-after-commit', after.gtid_set !== before.gtid_set, JSON.stringify({ before: before.gtid_set, after: after.gtid_set }))
  } else {
    ctx.check('checkpoint:previous-transaction-retained', after.gtid_set === before.gtid_set && after.binlog_pos === before.binlog_pos,
      JSON.stringify({ before: before.gtid_set, after: after.gtid_set }))
  }
  const subset = await ctx.rows(`SELECT GTID_SUBSET('${after.gtid_set}', @@GLOBAL.gtid_executed)`)
  ctx.check('checkpoint:belongs-to-source-history', String(subset[0][0]) === '1')
  await ctx.start()
}

export async function metadataError(ctx: Context) {
  // Arm after the initial copy: meta.before_commit also covers snapshot chunks.
  const known = new Set((await ctx.activity()).map(r => r.id))
  await ctx.restart('meta.before_commit=error')
  await ctx.fired('meta.before_commit', 'error')
  await until('metadata failure visible in sync history', async () => (await ctx.activity()).some(r => r.status === 'error' && /failpoint meta.before_commit/.test(r.error ?? '')))
  ctx.check('metadata:error-visible', true)
  const generation = ctx.restartCount
  await until('supervisor succeeds after metadata error', async () => (await ctx.activity()).some(r => !known.has(r.id) && r.status === 'completed' && r.kind === ctx.mode))
  ctx.check('metadata:retries-without-restart', ctx.alive && ctx.restartCount === generation)
}

export const cdcScenarios: Scenario[] = [
  ...['cdc.after_ingest', 'cdc.after_first_table_sync', 'cdc.before_checkpoint_commit', 'cdc.after_checkpoint_commit', 'store.wal.before_sync'].map(site => ({
    slug: site === 'store.wal.before_sync' ? 'cdc-wal-before-sync' : site.replaceAll('.', '-').replaceAll('_', '-'),
    area: 'cdc' as const, promise: 'crates/pintail-cdc/src/lib.rs: crate durability contract',
    run: (ctx: Context) => durability(ctx, site, site === 'cdc.after_checkpoint_commit'),
  })),
  { slug: 'cdc-meta-commit-error', area: 'cdc', promise: 'crates/pintail-cdc/src/lib.rs: checkpoint commit after WAL synchronization', run: metadataError },
]

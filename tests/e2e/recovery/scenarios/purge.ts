import { until, type Context, type Scenario } from '../harness'

/** docs/limitations.md:327 and supervisor.rs:48: CDC purge recovery: once per invocation, from a fresh source snapshot. */
export async function purgeWhileDown(ctx: Context) {
  await ctx.stop()
  const old = ctx.durable('before-purge').checkpoints[0]
  const before = ctx.commits
  await until('committed writes beyond captured position', async () => ctx.commits >= before + 10)
  await ctx.sql('FLUSH BINARY LOGS; FLUSH BINARY LOGS')
  const file = String((await ctx.rows('SHOW BINARY LOG STATUS'))[0][0])
  await ctx.sql(`PURGE BINARY LOGS TO '${file}'`)
  const retained = await ctx.rows('SHOW BINARY LOGS')
  ctx.check('purge:required-file-is-gone', !retained.some(row => row[0] === old.binlog_file))
}
export function partialIsNotHealthy(ctx: Context, label: string, scope: 'database' | 'table' = 'database') {
  const state = ctx.durable(label)
  const incomplete = new Set(state.snapshot_chunks.filter(c => c.status !== 'completed').map(c => c.table_name))
  for (const table of state.tables) if (!table.copy_complete) incomplete.add(table.name)
  ctx.check(`partial-copy:not-healthy:${label}`, state.tables.every(t => !incomplete.has(t.name) || !['streaming','polling'].includes(t.state)))
  // A one-table repair leaves the database live for its other tables.
  if (incomplete.size && scope === 'database') ctx.check(`partial-database:not-healthy:${label}`, !['streaming','polling'].includes(state.databases[0]?.state))
}
async function recover(ctx: Context, faults: string[]) {
  await purgeWhileDown(ctx)
  for (const [index, fault] of faults.entries()) {
    await ctx.start(fault)
    if (index === 0) await ctx.diagnostic(/cdc\.resnapshot .*unavailable source position/)
    await ctx.fired(fault.split('@')[0])
    partialIsNotHealthy(ctx, `after-${fault.replaceAll('@', '-')}`)
  }
  await ctx.start()
  if (!faults.length) await ctx.diagnostic(/cdc\.resnapshot .*unavailable source position/)
  await until('purge recovery returns to streaming', async () => (await ctx.status()).state === 'streaming')
}
export const purgeScenarios: Scenario[] = [
  { slug: 'purge-auto-resnapshot', area: 'purge', promise: 'docs/limitations.md: automatic purge recovery', run: ctx => recover(ctx, []) },
  { slug: 'purge-resnapshot-abort-once', area: 'purge', promise: 'crates/pintail-api/src/supervisor.rs: interrupted copy recovery', run: ctx => recover(ctx, ['snapshot.chunk.after_ingest@2']) },
  { slug: 'purge-resnapshot-abort-twice', area: 'purge', promise: 'docs/limitations.md: purge recovery once per runner invocation', run: ctx => recover(ctx, ['snapshot.chunk.after_ingest@2', 'snapshot.table.before_complete']) },
  { slug: 'purge-resnapshot-position-abort', area: 'purge', promise: 'crates/pintail-cdc/src/lib.rs: durable resnapshot handoff', run: ctx => recover(ctx, ['cdc.resnapshot.after_targets']) },
]

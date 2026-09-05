import { until, type Context, type Scenario } from '../harness'

async function pollingCheckpoint(ctx: Context) {
  await ctx.switchMode('polling')
  await until('a changed polling cycle completed', async () => (await ctx.activity()).some(run => run.kind === 'polling' && run.status === 'completed'))
  await ctx.stop()
  const state = ctx.durable('polling-handoff')
  ctx.check('handoff:starts-with-polling-checkpoint', state.checkpoints[0]?.kind === 'polling')
}

async function handoff(ctx: Context, failpoint = '') {
  await pollingCheckpoint(ctx)
  await ctx.start(failpoint)
  const known = new Set((await ctx.activity()).map(r => r.id))
  await ctx.switchMode('cdc')
  if (failpoint) {
    await ctx.fired(failpoint.split('@')[0])
    await ctx.start()
  }
  await until('CDC handoff snapshot completes', async () => (await ctx.activity()).some(r => !known.has(r.id) && r.kind === 'snapshot' && r.status === 'completed') && (await ctx.status()).state === 'streaming')
  await ctx.stop()
  const state = ctx.durable('cdc-handoff')
  ctx.check('handoff:has-new-gtid-checkpoint', state.checkpoints[0]?.kind === 'gtid')
  await ctx.start()
}

/** supervisor.rs: polling checkpoints cannot resume CDC; handoff takes a fresh snapshot. */
export const modeScenarios: Scenario[] = [
  { slug: 'mode-cdc-poll-cdc', area: 'mode', promise: 'crates/pintail-api/src/supervisor.rs: polling handoff', run: async ctx => {
    await handoff(ctx)
    ctx.check('handoff:automatic-event', ctx.events.some(e => /resync.auto/.test(e) && /handoff/.test(e)))
  } },
  { slug: 'mode-handoff-abort', area: 'mode', promise: 'crates/pintail-api/src/supervisor.rs: interrupted handoff', run: ctx => handoff(ctx, 'supervisor.handoff.after_begin') },
  { slug: 'mode-handoff-snapshot-abort', area: 'mode', promise: 'crates/pintail-api/src/supervisor.rs: interrupted snapshot recovery', run: ctx => handoff(ctx, 'snapshot.chunk.after_ingest@2') },
  { slug: 'mode-poll-during-cdc-lag', area: 'mode', promise: 'crates/pintail-api/src/supervisor.rs: fresh handoff preserves polling-era writes', run: async ctx => {
    await ctx.stop()
    const before = ctx.commits
    await until('writes accumulate while replica is down', async () => ctx.commits >= before + 10)
    await ctx.start()
    await handoff(ctx)
  } },
]

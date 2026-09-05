import { until, type Context, type Scenario } from '../harness'
import { metadataError } from './cdc'

async function strategies(ctx: Context) {
  await until('polling checkpoint exists', async () => (await ctx.activity()).some(r => r.kind === 'polling' && r.status === 'completed'))
  await ctx.stop()
  const state = ctx.durable('poll-strategies')
  const accounts = state.poll_states.find(r => r.table_name === 'accounts')
  ctx.check('poll:cursor-strategy', accounts?.cursor_column === 'updated_at')
  ctx.check('poll:checksum-strategy', state.poll_chunk_states.some(r => r.table_name === 'ledger'))
  const tables = await ctx.rows(`SELECT COUNT(*) FROM information_schema.table_constraints WHERE table_schema='${ctx.schema}' AND table_name='audit' AND constraint_type IN ('PRIMARY KEY','UNIQUE')`)
  ctx.check('poll:keyless-fixture', String(tables[0][0]) === '0')
  return state
}
async function interrupted(ctx: Context, fault: string) {
  const before = await strategies(ctx)
  await ctx.start(fault)
  await ctx.fired(fault.split('@')[0])
  const state = ctx.durable('poll-interrupted')
  ctx.check('poll:retains-durable-poll-state', state.poll_states.length === 3 && state.checkpoints[0]?.kind === 'polling')
  const table = fault.startsWith('poll.append') ? 'audit' : 'ledger'
  const previous = before.poll_states.find(r => r.table_name === table)
  const current = state.poll_states.find(r => r.table_name === table)
  ctx.check('poll:interrupted-table-state-is-old', JSON.stringify(current) === JSON.stringify(previous), table)
  if (fault.startsWith('poll.checksum')) ctx.check('poll:chunk-journal-is-old',
    JSON.stringify(state.poll_chunk_states.filter(r => r.table_name === table)) === JSON.stringify(before.poll_chunk_states.filter(r => r.table_name === table)))
  await ctx.start()
}

/** crates/pintail-poll/src/lib.rs:238 (run_poll_cycle): every changed WAL precedes its cursor/version commit. */
export const pollScenarios: Scenario[] = [
  ...[
    ['poll-after-ingest', 'poll.after_ingest@3'],
    ['poll-before-state-commit', 'poll.before_state_commit@3'],
    ['poll-append-after-reset', 'poll.append.after_reset'],
    ['poll-checksum-before-chunk-commit', 'poll.checksum.before_chunk_commit'],
  ].map(([slug, fault]) => ({ slug, area: 'poll' as const, mode: 'polling' as const,
    promise: 'crates/pintail-poll/src/lib.rs: run_poll_cycle durability', run: (ctx: Context) => interrupted(ctx, fault) })),
  { slug: 'poll-meta-commit-error', area: 'poll', mode: 'polling', promise: 'crates/pintail-poll/src/lib.rs: atomic poll state', run: metadataError },
]

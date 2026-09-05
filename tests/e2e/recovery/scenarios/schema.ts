import { until, type Context, type Scenario } from '../harness'
import { seedStandard } from '../schema'
import { purgeWhileDown, partialIsNotHealthy } from './purge'

export async function seedBig(ctx: Context) {
  await seedStandard(ctx)
  await ctx.sql(`CREATE TABLE big(id BIGINT PRIMARY KEY,value VARCHAR(32));
    INSERT INTO big WITH RECURSIVE d(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM d WHERE n<9)
    SELECT 1+a.n+10*b.n+100*c.n+1000*e.n+10000*f.n+100000*g.n,'original'
    FROM d a CROSS JOIN d b CROSS JOIN d c CROSS JOIN d e CROSS JOIN d f CROSS JOIN d g
    WHERE 1+a.n+10*b.n+100*c.n+1000*e.n+10000*f.n+100000*g.n <= 300000`)
}
async function interruptedRepair(ctx: Context, mutation: string) {
  await purgeWhileDown(ctx)
  // With one worker the small accounts/audit tables finish first; the
  // third durable chunk is big's first page, before its journal commit.
  await ctx.start('snapshot.chunk.after_ingest@3')
  await ctx.fired('snapshot.chunk.after_ingest')
  partialIsNotHealthy(ctx, 'schema-window')
  await ctx.sql(mutation)
  await ctx.start()
}
/** docs/limitations.md:369 and :373; DDL and polling: drift re-probe and generation replacement. */
export const schemaScenarios: Scenario[] = [
  { slug: 'repair-alter-add-column', area: 'schema', seed: seedBig, promise: 'docs/limitations.md: ADD COLUMN evolves schema', run: ctx => interruptedRepair(ctx,
    "ALTER TABLE big ADD COLUMN flag TINYINT NOT NULL DEFAULT 0; INSERT INTO big VALUES(400001,'new',1)") },
  { slug: 'repair-truncate', area: 'schema', seed: seedBig, promise: 'docs/limitations.md: TRUNCATE replaces generation', run: ctx => interruptedRepair(ctx,
    `TRUNCATE TABLE big; INSERT INTO big VALUES ${Array.from({length:100},(_,i)=>`(${400001+i},'replacement')`).join(',')}`) },
  { slug: 'repair-drop-recreate', area: 'schema', seed: seedBig, promise: 'docs/limitations.md: DROP and recreated table identity', run: ctx => interruptedRepair(ctx,
    "DROP TABLE big; CREATE TABLE big(id BIGINT PRIMARY KEY,value VARCHAR(32),new_flag BIGINT DEFAULT NULL); INSERT INTO big VALUES(400001,'replacement',7),(400002,'delete-later',8)") },
  { slug: 'repair-rename', area: 'schema', seed: seedBig, promise: 'docs/limitations.md: rename during interrupted forced resnapshot leaves stale progress', run: async ctx => {
    await interruptedRepair(ctx, 'RENAME TABLE big TO big2')
    ctx.gap = {table:'big',pattern:/^snapshotting$/,promise:'docs/limitations.md: stale old-name progress after interrupted resnapshot rename'}
  } },
  { slug: 'reconcile-alter', area: 'schema', mode: 'polling', seed: seedBig, promise: 'docs/limitations.md: polling re-probe after DDL', run: async ctx => {
    const previous = ctx.pollStates().find(t => t.table_name === 'big')?.last_reconcile_at
    await until('scheduled reconciliation reaches large table', async () => ctx.pollStates().find(t => t.table_name === 'big')?.last_reconcile_at !== previous)
    // Interrupt between page ingestion and metadata, then change the
    // schema before retry: deterministic overlap with the failed cycle.
    await ctx.restart('poll.reconcile.before_state_commit@3')
    await ctx.fired('poll.reconcile.before_state_commit')
    await ctx.sql('ALTER TABLE big ADD COLUMN flag TINYINT NOT NULL DEFAULT 0')
    await ctx.start()
  } },
]

import { until, type Context, type Scenario } from '../harness'
import { seedStandard } from '../schema'

/** GOAL.md:354 (§9), docs/limitations.md:334 and :338: inclusive boundaries and reconciliation. */
const scenario = (slug: string, run: (ctx: Context) => Promise<void>): Scenario => ({
  slug, area: 'boundaries', mode: 'polling', proxy: slug === 'poll-delete-insert-neutral', seed: async ctx => {
    await seedStandard(ctx)
    if (slug === 'poll-update-no-timestamp') await ctx.sql("INSERT INTO accounts VALUES(10000,'old',1.00,'2000-01-01'),(10001,'maximum',1.00,'2024-01-01')")
  }, promise: 'GOAL.md §9; docs/limitations.md DDL and polling', run,
})
export const boundaryScenarios = [
  scenario('poll-timestamp-ties', async ctx => {
    await ctx.stopChurn()
    for (let batch = 0; batch < 3; batch++) {
      const length = batch === 2 ? 1666 : 1667
      const values = Array.from({ length }, (_, i) => `(${10000 + batch * 1667 + i},'tie',1.01,'2030-01-01 00:00:00.123456')`)
      const known = new Set((await ctx.activity()).map(r=>r.id))
      await ctx.sql(`START TRANSACTION; INSERT INTO accounts VALUES ${values.join(',')}; COMMIT`)
      const last = 10000 + batch * 1667 + length - 1
      await until('poll commits this timestamp-tie batch before the next transaction', async()=>
        (await ctx.activity()).some(r=>!known.has(r.id)&&r.kind==='polling'&&r.status==='completed')
        && String((await ctx.replicaRows(`SELECT id FROM accounts WHERE id=${last}`))[0]?.[0])===String(last))
      ctx.check(`ties:observed-cycle-${batch}`,true)
    }
    await ctx.startChurn()
  }),
  scenario('poll-update-no-timestamp', async ctx => {
    // Below the maximum cursor, so boundary rereads cannot accidentally
    // mask a missing full-value reconciliation.
    await ctx.stopChurn(); await ctx.converge('before-unchanged-cursor')
    await ctx.sql('UPDATE accounts SET balance=77.77,updated_at=updated_at WHERE id=10000')
    await ctx.startChurn()
  }),
  scenario('poll-backdated-update', async ctx => {
    await ctx.sql("INSERT INTO accounts VALUES(10000,'backdated',1.00,'2030-01-01')")
    await ctx.stopChurn(); await ctx.converge('before-backdated-cursor')
    await ctx.sql("UPDATE accounts SET balance=88.88,updated_at='2000-01-01' WHERE id=10000")
    await ctx.startChurn()
  }),
  scenario('poll-delete-insert-neutral', async ctx => {
    await ctx.stopChurn()
    const values = Array.from({ length: 25000 }, (_, i) => `(${10000+i},'old',1.00,'2030-01-01')`)
    await ctx.sql(`INSERT INTO accounts VALUES ${values.join(',')}`)
    await ctx.converge('before-neutral-mutation')
    const before = await ctx.rows('SELECT COUNT(*),MAX(updated_at) FROM accounts')
    const replacement = Array.from({ length: 500 }, (_, i) => `(${50000+i},'new',2.00,'2030-01-01')`)
    ctx.proxy!.holdOnQuery(/FROM .*`accounts`.*LIMIT 10000 OFFSET 10000/i)
    await until('second polling page held at the source',async()=>!!ctx.proxy!.heldQuery)
    ctx.check('neutral:mutation-during-pagination',true,ctx.proxy!.heldQuery)
    try {
      await ctx.sql(`START TRANSACTION; DELETE FROM accounts WHERE id>=10000 AND id<10500; INSERT INTO accounts VALUES ${replacement.join(',')}; COMMIT`)
    } finally { ctx.proxy!.releaseQuery() }
    const after = await ctx.rows('SELECT COUNT(*),MAX(updated_at) FROM accounts')
    ctx.check('fixture:count-max-token-unchanged', JSON.stringify(before) === JSON.stringify(after))
    await ctx.startChurn()
  }),
  scenario('poll-keyless-dup-churn', async ctx => {
    await ctx.sql("INSERT INTO audit VALUES('dup','same'),('dup','same'),('dup','same')")
    await ctx.stopChurn(); await ctx.converge('before-duplicate-delete')
    await ctx.sql("DELETE FROM audit WHERE kind='dup' LIMIT 1")
    await ctx.startChurn()
  }),
]

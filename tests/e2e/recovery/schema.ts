import type { Context } from './harness'
export const standardSchema = `
CREATE TABLE accounts (id BIGINT PRIMARY KEY, owner VARCHAR(64), balance DECIMAL(12,2), updated_at DATETIME(6) NOT NULL);
CREATE TABLE ledger (id BIGINT PRIMARY KEY, account_id BIGINT, amount DECIMAL(12,2), note VARCHAR(64));
CREATE TABLE audit (kind VARCHAR(32), payload VARCHAR(64));
INSERT INTO accounts VALUES (1,'Ada',100.00,'2024-01-01'),(2,'Linus',100.00,'2024-01-01');
INSERT INTO ledger VALUES (1,1,1.00,'seed'),(2,2,2.00,'seed');
INSERT INTO audit VALUES ('seed','duplicate'),('seed','duplicate');`

export async function seedStandard(ctx: Context) { await ctx.sql(standardSchema) }

/** Every committed generation contains changes to every standard table. */
export async function transfer(connection: import('mysql2/promise').Connection, sequence: number, rollback: boolean) {
  const query = (sql:string, values:(string|number)[] = []) => connection.query({sql,timeout:15_000},values)
  let timer: ReturnType<typeof setTimeout> | undefined
  const transaction = (async () => {
    await query('START TRANSACTION')
    try {
      await query('UPDATE accounts SET balance=balance+?,updated_at=NOW(6) WHERE id=1', [sequence % 2 ? 1 : -1])
      await query('UPDATE accounts SET balance=balance-?,updated_at=NOW(6) WHERE id=2', [sequence % 2 ? 1 : -1])
      await query("INSERT INTO ledger(id,account_id,amount,note) VALUES (?,1,1.00,?),(?,2,-1.00,?)", [sequence * 2 + 3, `tx-${sequence}`, sequence * 2 + 4, `tx-${sequence}`])
      await query("DELETE FROM ledger WHERE id > 2 AND id < (SELECT boundary FROM (SELECT MAX(id)-16 AS boundary FROM ledger) q)")
      await query("INSERT INTO audit VALUES ('transfer',?)", [`tx-${sequence}`])
      await query(rollback ? 'ROLLBACK' : 'COMMIT')
    } catch (error) {
      await query('ROLLBACK').catch(() => {})
      throw error
    }
  })()
  try {
    await Promise.race([transaction,new Promise<never>((_,reject)=>{
      timer=setTimeout(()=>{connection.destroy();reject(new Error('source transaction exceeded 30-second deadline'))},30_000)
    })])
  } finally { clearTimeout(timer) }
}

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
  await connection.beginTransaction()
  try {
    await connection.query('UPDATE accounts SET balance=balance+?,updated_at=NOW(6) WHERE id=1', [sequence % 2 ? 1 : -1])
    await connection.query('UPDATE accounts SET balance=balance-?,updated_at=NOW(6) WHERE id=2', [sequence % 2 ? 1 : -1])
    await connection.query("INSERT INTO ledger(id,account_id,amount,note) VALUES (?,1,1.00,?),(?,2,-1.00,?)", [sequence * 2 + 3, `tx-${sequence}`, sequence * 2 + 4, `tx-${sequence}`])
    await connection.query("DELETE FROM ledger WHERE id > 2 AND id < (SELECT boundary FROM (SELECT MAX(id)-16 AS boundary FROM ledger) q)")
    await connection.query("INSERT INTO audit VALUES ('transfer',?)", [`tx-${sequence}`])
    if (rollback) await connection.rollback()
    else await connection.commit()
  } catch (error) {
    await connection.rollback().catch(() => {})
    throw error
  }
}

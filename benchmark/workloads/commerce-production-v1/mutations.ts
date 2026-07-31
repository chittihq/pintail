// Mutation stream for the mixed phase: state transitions, multi-row
// transactions, bursts, soft deletes, late arrivals, and the cascade-delete
// negative control. Deterministic per writer seed.

import type mysql from 'mysql2/promise'
import { Rng, TimeSampler, Zipf, sqlDatetime, weightedPick, personName } from './seed'
import type { SeedProfile, SeedResult } from './seed'

export interface MutationStats {
  inserts: number
  updates: number
  deletes: number
  transactions: number
  cascadeDeletes: number
  errors: number
}

export interface MutationController {
  stop(): Promise<MutationStats>
}

export function startMutations(
  connections: mysql.Connection[],
  profile: SeedProfile,
  seedResult: SeedResult,
  seedValue: number,
  log: (m: string) => void,
): MutationController {
  const stats: MutationStats = {
    inserts: 0, updates: 0, deletes: 0, transactions: 0, cascadeDeletes: 0, errors: 0,
  }
  let running = true
  const m = profile.mutations
  const maxOrder = seedResult.counts.orders
  const time = new TimeSampler(profile.time, seedResult.now)
  const tenantZipf = new Zipf(seedResult.counts.tenants, 1.15)

  const workers = connections.map(async (conn, index) => {
    const rng = new Rng(seedValue * 1000 + index)
    let nextExternal = 0
    while (running) {
      // burst pattern: every burstEverySeconds, run at burstFactor for 10s
      const second = Math.floor(Date.now() / 1000)
      const inBurst = second % m.burstEverySeconds < 10
      const pace = inBurst ? 1000 / m.burstFactor : 1000
      try {
        const roll = rng.next()
        if (roll < m.insertShare) {
          // new order lifecycle start: order + items + event in one transaction
          await conn.beginTransaction()
          const customer = 1 + rng.int(seedResult.counts.customers)
          const tenant = seedResult.tenantOfCustomer[customer]
          const placed = m.lateArrivalShare > rng.next()
            ? time.sample(rng) // late-arriving historical timestamp
            : new Date()
          nextExternal += 1
          const [orderResult] = await conn.query<mysql.ResultSetHeader>(
            `INSERT INTO orders (external_id,tenant_id,customer_id,currency,subtotal_amount,discount_amount,tax_amount,shipping_amount,total_amount,order_status,payment_status,fulfillment_status,shipping_country,sales_channel,placed_at,updated_at)
             VALUES (UNHEX('${rng.hex(16)}'),${tenant},${customer},'INR',100,0,18,0,118,'pending','pending','unfulfilled','IN','web','${sqlDatetime(placed)}','${sqlDatetime(new Date())}')`,
          )
          const orderId = orderResult.insertId
          await conn.query(
            `INSERT INTO order_items (order_id,tenant_id,product_variant_id,sku,product_name,quantity,unit_price,discount_amount,tax_amount,total_amount,created_at,updated_at)
             VALUES (${orderId},${tenant},${1 + rng.int(seedResult.counts.product_variants)},'SKU-live',${conn.escape(personName(rng, profile))},1,100,0,18,118,'${sqlDatetime(placed)}','${sqlDatetime(placed)}')`,
          )
          await conn.query(
            `INSERT INTO order_events (order_id,tenant_id,event_type,actor,created_at)
             VALUES (${orderId},${tenant},'order.created','system','${sqlDatetime(new Date())}')`,
          )
          await conn.commit()
          stats.inserts += 3
          stats.transactions += 1
        } else if (roll < m.insertShare + m.updateShare) {
          // status transition on an existing order (multi-row txn sometimes)
          const orderId = 1 + rng.int(maxOrder)
          const target = weightedPick(rng, { confirmed: 0.3, completed: 0.5, cancelled: 0.2 })
          if (rng.next() < m.multiRowTransactionShare) {
            await conn.beginTransaction()
            await conn.query(
              `UPDATE orders SET order_status='${target}', updated_at='${sqlDatetime(new Date())}'${target === 'completed' ? `, completed_at='${sqlDatetime(new Date())}'` : ''} WHERE id=${orderId}`,
            )
            await conn.query(
              `INSERT INTO order_events (order_id,tenant_id,event_type,actor,created_at)
               SELECT id, tenant_id, 'order.${target}', 'operator', '${sqlDatetime(new Date())}' FROM orders WHERE id=${orderId}`,
            )
            await conn.commit()
            stats.transactions += 1
            stats.updates += 1
            stats.inserts += 1
          } else {
            await conn.query(
              `UPDATE orders SET payment_status=IF(payment_status='pending','paid',payment_status), updated_at='${sqlDatetime(new Date())}' WHERE id=${orderId}`,
            )
            stats.updates += 1
          }
          // occasional price change + inventory adjustment
          if (rng.chance(0.1)) {
            await conn.query(
              `UPDATE product_variants SET list_price=list_price*1.01, updated_at='${sqlDatetime(new Date())}' WHERE id=${1 + rng.int(seedResult.counts.product_variants)}`,
            )
            stats.updates += 1
          }
          if (rng.chance(0.15)) {
            await conn.query(
              `UPDATE inventory_balances SET on_hand=GREATEST(0,on_hand-1), updated_at='${sqlDatetime(new Date())}' WHERE id=${1 + rng.int(Math.max(1, seedResult.counts.inventory_balances))}`,
            )
            stats.updates += 1
          }
        } else {
          // deletes: mostly soft, sometimes hard, rarely the cascade control
          const orderId = 1 + rng.int(maxOrder)
          if (rng.next() < m.softDeleteShare) {
            await conn.query(
              `UPDATE orders SET deleted_at='${sqlDatetime(new Date())}', updated_at='${sqlDatetime(new Date())}' WHERE id=${orderId} AND deleted_at IS NULL`,
            )
            stats.updates += 1
          } else if (rng.chance(0.2)) {
            // NEGATIVE CONTROL: deleting a shipment cascades to shipment_items
            // invisibly to the binlog. The reconciler must catch these.
            const [rows] = await conn.query<mysql.RowDataPacket[]>(
              `SELECT id FROM shipments WHERE order_id=${orderId} LIMIT 1`,
            )
            if (rows.length > 0) {
              await conn.query(`DELETE FROM shipments WHERE id=${rows[0].id}`)
              stats.deletes += 1
              stats.cascadeDeletes += 1
            }
          } else {
            await conn.query(`DELETE FROM order_events WHERE order_id=${orderId} AND event_type='note.added'`)
            stats.deletes += 1
          }
        }
      } catch (error) {
        stats.errors += 1
        try { await conn.rollback() } catch {}
        if (stats.errors % 100 === 1) log(`mutation error (worker ${index}): ${error}`)
      }
      await Bun.sleep(pace / Math.max(1, m.updatesPerSecondAtFullScale / connections.length / 10))
    }
  })

  return {
    async stop() {
      running = false
      await Promise.allSettled(workers)
      return stats
    },
  }
}

// Exact-result and convergence gates for commerce-production-v1.

import type mysql from 'mysql2/promise'

export const TABLES = [
  'tenants', 'customers', 'customer_addresses', 'categories', 'products',
  'product_variants', 'warehouses', 'inventory_balances', 'orders',
  'order_items', 'payments', 'refunds', 'shipments', 'shipment_items',
  'order_events',
] as const

export interface TableFingerprint {
  rows: number
  amountSum: string | null
  maxUpdated: string | null
}

export type Fingerprints = Record<string, TableFingerprint>

const AMOUNT_COLUMN: Record<string, string | null> = {
  tenants: null,
  customers: 'lifetime_value',
  customer_addresses: null,
  categories: null,
  products: null,
  product_variants: 'list_price',
  warehouses: null,
  inventory_balances: 'on_hand',
  orders: 'total_amount',
  order_items: 'total_amount',
  payments: 'amount',
  refunds: 'amount',
  shipments: null,
  shipment_items: 'quantity',
  order_events: null,
}

const UPDATED_COLUMN: Record<string, string | null> = {
  tenants: 'updated_at', customers: 'updated_at', customer_addresses: 'updated_at',
  categories: 'updated_at', products: 'updated_at', product_variants: 'updated_at',
  warehouses: 'updated_at', inventory_balances: 'updated_at', orders: 'updated_at',
  order_items: 'updated_at', payments: 'updated_at', refunds: 'updated_at',
  shipments: 'updated_at', shipment_items: 'created_at', order_events: 'created_at',
}

export async function mysqlFingerprints(conn: mysql.Connection): Promise<Fingerprints> {
  const out: Fingerprints = {}
  for (const table of TABLES) {
    const amount = AMOUNT_COLUMN[table]
    const updated = UPDATED_COLUMN[table]
    const [rows] = await conn.query<mysql.RowDataPacket[]>(
      `SELECT COUNT(*) AS c${amount ? `, CAST(SUM(${amount}) AS CHAR) AS s` : ''}${updated ? `, CAST(MAX(${updated}) AS CHAR) AS m` : ''} FROM ${table}`,
    )
    out[table] = {
      rows: Number(rows[0].c),
      amountSum: amount ? (rows[0].s ?? null) : null,
      maxUpdated: updated ? (rows[0].m ?? null) : null,
    }
  }
  return out
}

export async function pintailFingerprints(
  queryPintail: (sql: string) => Promise<unknown[][]>,
): Promise<Fingerprints> {
  const out: Fingerprints = {}
  for (const table of TABLES) {
    const amount = AMOUNT_COLUMN[table]
    const updated = UPDATED_COLUMN[table]
    const rows = await queryPintail(
      `SELECT COUNT(*)${amount ? `, SUM(${amount})` : ''}${updated ? `, MAX(${updated})` : ''} FROM ${table}`,
    )
    const row = rows[0] ?? []
    out[table] = {
      rows: Number(row[0]),
      amountSum: amount ? String(row[1] ?? '') : null,
      maxUpdated: updated ? String(row[amount ? 2 : 1] ?? '') : null,
    }
  }
  return out
}

export interface Mismatch {
  table: string
  field: string
  mysql: unknown
  pintail: unknown
}

export function compareFingerprints(a: Fingerprints, b: Fingerprints): Mismatch[] {
  const mismatches: Mismatch[] = []
  for (const table of TABLES) {
    if (a[table].rows !== b[table].rows) {
      mismatches.push({ table, field: 'rows', mysql: a[table].rows, pintail: b[table].rows })
    }
    if (a[table].amountSum !== null && b[table].amountSum !== null) {
      const x = Number.parseFloat(a[table].amountSum)
      const y = Number.parseFloat(b[table].amountSum)
      if (Math.abs(x - y) > Math.max(1e-6, Math.abs(x) * 1e-9)) {
        mismatches.push({ table, field: 'amountSum', mysql: a[table].amountSum, pintail: b[table].amountSum })
      }
    }
  }
  return mismatches
}

export function normalizeRows(rows: unknown[][]): string {
  return rows
    .map((row) =>
      row
        .map((value) => {
          if (value === null || value === undefined) return 'NULL'
          if (typeof value === 'number') return value.toFixed(6)
          const text = String(value)
          // Datetime fractional seconds: MySQL prints the column's fsp
          // width, pintail prints canonical microseconds — trim trailing
          // fraction zeros on both sides so '.548' and '.548000' agree.
          const datetime = text.match(/^(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2})(?:\.(\d+))?$/)
          if (datetime) {
            const fraction = (datetime[2] ?? '').replace(/0+$/, '')
            return fraction ? `${datetime[1]}.${fraction}` : datetime[1]
          }
          const asNumber = Number(text)
          if (text !== '' && Number.isFinite(asNumber) && /^-?\d+(\.\d+)?$/.test(text)) {
            return asNumber.toFixed(6)
          }
          return text
        })
        .join(''),
    )
    .join('\n')
}

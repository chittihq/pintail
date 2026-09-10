import type { FieldPacket } from 'mysql2/promise'

export function cell(value: unknown): unknown {
  if (value === null) return { null: true }
  if (Buffer.isBuffer(value)) return { bytes: value.toString('hex') }
  if (typeof value === 'string') return { bytes: Buffer.from(value).toString('hex') }
  if (typeof value === 'number' || typeof value === 'bigint') return { bytes: Buffer.from(String(value)).toString('hex') }
  throw new Error(`Unexpected client value carrier: ${typeof value}`)
}

export function rows(values: unknown[][], ordered = true): string[] {
  const encoded = values.map(row => JSON.stringify(row.map(cell)))
  return ordered ? encoded : encoded.sort()
}

export function fields(values: FieldPacket[]) {
  return values.map(f => ({ type: f.columnType, scale: f.decimals, charset: f.characterSet,
    unsigned: typeof f.flags === 'number' ? Boolean(f.flags & 32) : f.flags.includes('UNSIGNED'), nullable: typeof f.flags === 'number' ? !(f.flags & 1) : !f.flags.includes('NOT_NULL') }))
}

export function errorDetails(error: unknown) {
  const e = error as { errno?: number; sqlState?: string; message?: string }
  return { errno: e.errno, sqlState: e.sqlState, message: e.message ?? String(error) }
}

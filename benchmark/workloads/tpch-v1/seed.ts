/// TPC-H data generation, deterministic from a seed.
///
/// Generated rather than downloaded. `dbgen` would be the reference, but it
/// means shipping a C toolchain into every environment that runs the
/// benchmark, and the value of TPC-H here is its SHAPE - the cardinalities,
/// the join fan-out, the skew - not byte-identical text. What matters is that
/// two runs at the same scale produce the same database, which a seeded PRNG
/// gives and a download does not.
///
/// Row counts follow the specification's ratios so the joins fan out the way
/// the queries assume: 150k customers, 1.5M orders and ~6M lineitems per unit
/// scale factor, with roughly four lines per order.

import type mysql from 'mysql2/promise'

/// Deterministic PRNG. Same seed, same database.
class Rng {
  private state: number

  constructor(seed: number) {
    this.state = seed >>> 0 || 1
  }

  next(): number {
    this.state = (this.state + 0x6d2b79f5) >>> 0
    let value = Math.imul(this.state ^ (this.state >>> 15), 1 | this.state)
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296
  }

  int(maxExclusive: number): number {
    return Math.floor(this.next() * maxExclusive)
  }

  pick<T>(values: readonly T[]): T {
    return values[this.int(values.length)]
  }

  /// Two decimal places, as every money column in the schema is.
  money(min: number, max: number): string {
    return (min + this.next() * (max - min)).toFixed(2)
  }
}

const REGIONS = ['AFRICA', 'AMERICA', 'ASIA', 'EUROPE', 'MIDDLE EAST'] as const

/// The specification's 25 nations, in its order, so `n_nationkey` means the
/// same thing here as in any other TPC-H result.
const NATIONS: Array<[string, number]> = [
  ['ALGERIA', 0], ['ARGENTINA', 1], ['BRAZIL', 1], ['CANADA', 1], ['EGYPT', 4],
  ['ETHIOPIA', 0], ['FRANCE', 3], ['GERMANY', 3], ['INDIA', 2], ['INDONESIA', 2],
  ['IRAN', 4], ['IRAQ', 4], ['JAPAN', 2], ['JORDAN', 4], ['KENYA', 0],
  ['MOROCCO', 0], ['MOZAMBIQUE', 0], ['PERU', 1], ['CHINA', 2], ['ROMANIA', 3],
  ['SAUDI ARABIA', 4], ['VIETNAM', 2], ['RUSSIA', 3], ['UNITED KINGDOM', 3],
  ['UNITED STATES', 1],
]

const SEGMENTS = ['BUILDING', 'AUTOMOBILE', 'MACHINERY', 'HOUSEHOLD', 'FURNITURE'] as const
const PRIORITIES = ['1-URGENT', '2-HIGH', '3-MEDIUM', '4-NOT SPECIFIED', '5-LOW'] as const
const SHIPMODES = ['AIR', 'AIR REG', 'RAIL', 'SHIP', 'TRUCK', 'MAIL', 'FOB'] as const
const INSTRUCTIONS = ['DELIVER IN PERSON', 'COLLECT COD', 'NONE', 'TAKE BACK RETURN'] as const
const CONTAINERS = ['SM CASE', 'LG BOX', 'JUMBO PACK', 'WRAP BAG', 'MED DRUM'] as const
const TYPES = ['STANDARD BRUSHED STEEL', 'SMALL PLATED COPPER', 'PROMO BURNISHED NICKEL',
  'ECONOMY ANODIZED BRASS', 'LARGE POLISHED TIN'] as const

/// TPC-H dates run 1992-01-01 to 1998-12-31; the queries' constants assume it.
const START_DATE = Date.UTC(1992, 0, 1)
const DATE_SPAN_DAYS = 2557

function isoDate(dayOffset: number): string {
  return new Date(START_DATE + dayOffset * 86_400_000).toISOString().slice(0, 10)
}

export interface TpchCounts {
  region: number
  nation: number
  supplier: number
  part: number
  partsupp: number
  customer: number
  orders: number
  lineitem: number
}

/// Buffers rows and flushes in batches, because a row-at-a-time insert of a
/// million lineitems spends its life in round trips.
class BatchWriter {
  private rows: string[] = []

  constructor(
    private readonly conn: mysql.Connection,
    private readonly prefix: string,
    private readonly batchSize = 1000,
  ) {}

  async push(values: string): Promise<void> {
    this.rows.push(`(${values})`)
    if (this.rows.length >= this.batchSize) await this.flush()
  }

  async flush(): Promise<void> {
    if (this.rows.length === 0) return
    await this.conn.query(`${this.prefix} VALUES ${this.rows.join(',')}`)
    this.rows = []
  }
}

const quote = (text: string) => `'${text.replace(/'/g, "''")}'`

/// Seeds a TPC-H database at `scale`, returning what it wrote.
///
/// Scale is the specification's factor: 1 is the full ~6M-lineitem dataset,
/// and fractions produce proportionally smaller ones. Everything derives from
/// `seed`, so a run is reproducible.
export async function seedTpch(
  conn: mysql.Connection,
  scale: number,
  seed: number,
  log: (message: string) => void,
): Promise<TpchCounts> {
  const rng = new Rng(seed)
  const suppliers = Math.max(1, Math.round(10_000 * scale))
  const parts = Math.max(1, Math.round(200_000 * scale))
  const customers = Math.max(1, Math.round(150_000 * scale))
  const orders = Math.max(1, Math.round(1_500_000 * scale))

  log(`seeding TPC-H at scale ${scale}: ${orders.toLocaleString()} orders`)

  const regionWriter = new BatchWriter(conn, 'INSERT INTO region (r_regionkey,r_name,r_comment)')
  for (const [index, name] of REGIONS.entries()) {
    await regionWriter.push(`${index},${quote(name)},${quote(`region ${index}`)}`)
  }
  await regionWriter.flush()

  const nationWriter = new BatchWriter(
    conn,
    'INSERT INTO nation (n_nationkey,n_name,n_regionkey,n_comment)',
  )
  for (const [index, [name, region]] of NATIONS.entries()) {
    await nationWriter.push(`${index},${quote(name)},${region},${quote(`nation ${index}`)}`)
  }
  await nationWriter.flush()

  const supplierWriter = new BatchWriter(
    conn,
    'INSERT INTO supplier (s_suppkey,s_name,s_address,s_nationkey,s_phone,s_acctbal,s_comment)',
  )
  for (let key = 1; key <= suppliers; key += 1) {
    await supplierWriter.push(
      `${key},${quote(`Supplier#${String(key).padStart(9, '0')}`)},` +
        `${quote(`address ${rng.int(100_000)}`)},${rng.int(25)},` +
        `${quote(`${10 + rng.int(24)}-${100 + rng.int(900)}-${100 + rng.int(900)}-${1000 + rng.int(9000)}`)},` +
        `${rng.money(-999, 9999)},${quote('supplier comment')}`,
    )
  }
  await supplierWriter.flush()

  const partWriter = new BatchWriter(
    conn,
    'INSERT INTO part (p_partkey,p_name,p_mfgr,p_brand,p_type,p_size,p_container,p_retailprice,p_comment)',
  )
  for (let key = 1; key <= parts; key += 1) {
    const manufacturer = 1 + rng.int(5)
    await partWriter.push(
      `${key},${quote(`part ${key}`)},${quote(`Manufacturer#${manufacturer}`)},` +
        `${quote(`Brand#${manufacturer}${1 + rng.int(5)}`)},${quote(rng.pick(TYPES))},` +
        `${1 + rng.int(50)},${quote(rng.pick(CONTAINERS))},${rng.money(900, 2100)},` +
        `${quote('part comment')}`,
    )
  }
  await partWriter.flush()

  // Four suppliers per part, as the specification has it: this is what gives
  // partsupp its fan-out and the join its shape.
  const partsuppWriter = new BatchWriter(
    conn,
    'INSERT INTO partsupp (ps_partkey,ps_suppkey,ps_availqty,ps_supplycost,ps_comment)',
  )
  let partsuppRows = 0
  for (let partKey = 1; partKey <= parts; partKey += 1) {
    for (let slot = 0; slot < 4; slot += 1) {
      const suppKey = 1 + ((partKey + slot * Math.floor(suppliers / 4)) % suppliers)
      await partsuppWriter.push(
        `${partKey},${suppKey},${rng.int(10_000)},${rng.money(1, 1000)},${quote('partsupp comment')}`,
      )
      partsuppRows += 1
    }
  }
  await partsuppWriter.flush()

  const customerWriter = new BatchWriter(
    conn,
    'INSERT INTO customer (c_custkey,c_name,c_address,c_nationkey,c_phone,c_acctbal,c_mktsegment,c_comment)',
  )
  for (let key = 1; key <= customers; key += 1) {
    await customerWriter.push(
      `${key},${quote(`Customer#${String(key).padStart(9, '0')}`)},` +
        `${quote(`address ${rng.int(100_000)}`)},${rng.int(25)},` +
        `${quote(`${10 + rng.int(24)}-${100 + rng.int(900)}-${100 + rng.int(900)}-${1000 + rng.int(9000)}`)},` +
        `${rng.money(-999, 9999)},${quote(rng.pick(SEGMENTS))},${quote('customer comment')}`,
    )
  }
  await customerWriter.flush()

  const orderWriter = new BatchWriter(
    conn,
    'INSERT INTO orders (o_orderkey,o_custkey,o_orderstatus,o_totalprice,o_orderdate,' +
      'o_orderpriority,o_clerk,o_shippriority,o_comment)',
  )
  const lineWriter = new BatchWriter(
    conn,
    'INSERT INTO lineitem (l_orderkey,l_partkey,l_suppkey,l_linenumber,l_quantity,' +
      'l_extendedprice,l_discount,l_tax,l_returnflag,l_linestatus,l_shipdate,' +
      'l_commitdate,l_receiptdate,l_shipinstruct,l_shipmode,l_comment)',
  )
  let lineRows = 0
  for (let orderKey = 1; orderKey <= orders; orderKey += 1) {
    const orderDay = rng.int(DATE_SPAN_DAYS)
    const custKey = 1 + rng.int(customers)
    // One to seven lines, averaging four, which is the ratio the query
    // cardinalities assume.
    const lines = 1 + rng.int(7)
    let total = 0
    for (let line = 1; line <= lines; line += 1) {
      const quantity = 1 + rng.int(50)
      const price = Number(rng.money(900, 105_000))
      const discount = Number(rng.money(0, 0.1))
      const tax = Number(rng.money(0, 0.08))
      const shipDay = orderDay + 1 + rng.int(120)
      total += price * (1 - discount) * (1 + tax)
      await lineWriter.push(
        `${orderKey},${1 + rng.int(parts)},${1 + rng.int(suppliers)},${line},` +
          `${quantity}.00,${price.toFixed(2)},${discount.toFixed(2)},${tax.toFixed(2)},` +
          `${quote(rng.next() < 0.25 ? 'R' : rng.next() < 0.5 ? 'A' : 'N')},` +
          `${quote(rng.next() < 0.5 ? 'O' : 'F')},` +
          `${quote(isoDate(shipDay))},${quote(isoDate(orderDay + 30))},` +
          `${quote(isoDate(shipDay + rng.int(30)))},${quote(rng.pick(INSTRUCTIONS))},` +
          `${quote(rng.pick(SHIPMODES))},${quote('lineitem comment')}`,
      )
      lineRows += 1
    }
    await orderWriter.push(
      `${orderKey},${custKey},${quote(rng.next() < 0.5 ? 'O' : 'F')},${total.toFixed(2)},` +
        `${quote(isoDate(orderDay))},${quote(rng.pick(PRIORITIES))},` +
        `${quote(`Clerk#${String(1 + rng.int(1000)).padStart(9, '0')}`)},0,${quote('order comment')}`,
    )
  }
  await orderWriter.flush()
  await lineWriter.flush()

  const counts: TpchCounts = {
    region: REGIONS.length,
    nation: NATIONS.length,
    supplier: suppliers,
    part: parts,
    partsupp: partsuppRows,
    customer: customers,
    orders,
    lineitem: lineRows,
  }
  log(`TPC-H seeded: ${JSON.stringify(counts)}`)
  return counts
}

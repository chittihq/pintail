// Deterministic production-shaped seeder for commerce-production-v1.
// Honors production-profile.json distributions: Zipf tenant/customer skew,
// correlated statuses, lifecycle-consistent children, seasonality, whales,
// UTF-8/emoji strings, JSON payloads, soft deletes.

import type mysql from 'mysql2/promise'

// ---------- deterministic PRNG ----------

export class Rng {
  private s: number
  constructor(seed: number) {
    this.s = seed >>> 0 || 1
  }
  next(): number {
    // mulberry32
    this.s = (this.s + 0x6d2b79f5) >>> 0
    let t = this.s
    t = Math.imul(t ^ (t >>> 15), t | 1)
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61)
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
  int(maxExclusive: number): number {
    return Math.floor(this.next() * maxExclusive)
  }
  pick<T>(items: T[]): T {
    return items[this.int(items.length)]
  }
  chance(p: number): boolean {
    return this.next() < p
  }
  hex(bytes: number): string {
    let out = ''
    for (let i = 0; i < bytes; i += 1) {
      out += this.int(256).toString(16).padStart(2, '0')
    }
    return out
  }
}

export class Zipf {
  private cdf: Float64Array
  constructor(n: number, s: number) {
    this.cdf = new Float64Array(n)
    let sum = 0
    for (let i = 0; i < n; i += 1) {
      sum += 1 / Math.pow(i + 1, s)
      this.cdf[i] = sum
    }
    for (let i = 0; i < n; i += 1) this.cdf[i] /= sum
  }
  sample(rng: Rng): number {
    const u = rng.next()
    let lo = 0
    let hi = this.cdf.length - 1
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (this.cdf[mid] < u) lo = mid + 1
      else hi = mid
    }
    return lo // 0-based rank
  }
}

export function weightedPick(rng: Rng, weights: Record<string, number>): string {
  const u = rng.next()
  let acc = 0
  let last = ''
  for (const [key, w] of Object.entries(weights)) {
    acc += w
    last = key
    if (u < acc) return key
  }
  return last
}

// ---------- time sampling ----------

export class TimeSampler {
  private dayWeights: Float64Array
  private hourCdf: Float64Array
  readonly end: Date

  constructor(profileTime: {
    historyDays: number
    hotRecentDays: number
    hotRecentShare: number
    weekdayWeights: number[]
    hourWeights: number[]
  }, end: Date) {
    this.end = end
    const n = profileTime.historyDays
    this.dayWeights = new Float64Array(n)
    let sum = 0
    for (let d = 0; d < n; d += 1) {
      // d = days before end; hot recent window carries hotRecentShare of mass
      const base = d < profileTime.hotRecentDays
        ? profileTime.hotRecentShare / profileTime.hotRecentDays
        : (1 - profileTime.hotRecentShare) / (n - profileTime.hotRecentDays)
      const date = new Date(end.getTime() - d * 86_400_000)
      const weekday = profileTime.weekdayWeights[date.getUTCDay() === 0 ? 6 : date.getUTCDay() - 1]
      this.dayWeights[d] = base * weekday
      sum += this.dayWeights[d]
    }
    let acc = 0
    for (let d = 0; d < n; d += 1) {
      acc += this.dayWeights[d] / sum
      this.dayWeights[d] = acc
    }
    const hours = profileTime.hourWeights
    const hsum = hours.reduce((a, b) => a + b, 0)
    this.hourCdf = new Float64Array(24)
    let hacc = 0
    for (let h = 0; h < 24; h += 1) {
      hacc += hours[h] / hsum
      this.hourCdf[h] = hacc
    }
  }

  sample(rng: Rng): Date {
    const u = rng.next()
    let lo = 0
    let hi = this.dayWeights.length - 1
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (this.dayWeights[mid] < u) lo = mid + 1
      else hi = mid
    }
    const uh = rng.next()
    let hour = 0
    while (hour < 23 && this.hourCdf[hour] < uh) hour += 1
    const date = new Date(this.end.getTime() - lo * 86_400_000)
    date.setUTCHours(hour, rng.int(60), rng.int(60), rng.int(1000))
    return date
  }
}

export function sqlDatetime(d: Date): string {
  return d.toISOString().slice(0, 23).replace('T', ' ')
}

function addSeconds(d: Date, seconds: number): Date {
  return new Date(d.getTime() + seconds * 1000)
}

// ---------- name/text pools ----------

const FIRST_NAMES = ['Aarav', 'Priya', 'María', 'José', 'Wei', 'Fatima', 'John', 'Emma', 'Søren', 'Zoë', 'Håkon', 'Ayşe', 'Иван', 'あきら', '민준', 'Nguyễn', 'Olá', 'Chidi', 'Amara', 'Diego']
const LAST_NAMES = ['Kumar', 'Sharma', 'García', 'Müller', 'Chen', 'Al-Rashid', 'Smith', 'Johansson', "O'Brien", 'Silva', 'Kowalski', 'Yılmaz', 'Петров', '田中', '김', 'Trần', 'Okafor', 'Mbeki', 'Rossi', 'Dubois']
const EMOJI = ['🎁', '⭐', '🔥', '💜', '🌟', '✨', '🚀', '🌈']
const PRODUCT_ADJ = ['Premium', 'Classic', 'Eco', 'Ultra', 'Compact', 'Wireless', 'Organic', 'Smart', 'Vintage', 'Pro']
const PRODUCT_NOUN = ['Headphones', 'Backpack', 'Water Bottle', 'Notebook', 'Sneakers', 'Desk Lamp', 'T-Shirt', 'Coffee Maker', 'Yoga Mat', 'Charger', 'Keyboard', 'Sunglasses']
const CITIES = ['Chennai', 'Mumbai', 'New York', 'Berlin', 'London', 'Dubai', 'Singapore', 'Bengaluru', 'San Francisco', 'Coimbatore']
const REGIONS: Record<string, string[]> = {
  IN: ['Tamil Nadu', 'Maharashtra', 'Karnataka', 'Delhi', 'Kerala'],
  US: ['California', 'New York', 'Texas', 'Washington', 'Florida'],
  DE: ['Bavaria', 'Berlin', 'Hamburg'],
  GB: ['England', 'Scotland', 'Wales'],
  AE: ['Dubai', 'Abu Dhabi'],
  SG: ['Central'],
}
const CARRIERS = ['delhivery', 'bluedart', 'fedex', 'ups', 'dhl', 'shiprocket']
const EVENT_TYPES_LIFECYCLE = ['order.created', 'payment.attempted', 'payment.captured', 'inventory.reserved', 'shipment.created', 'shipment.shipped', 'shipment.delivered', 'order.completed', 'order.cancelled', 'refund.processed', 'note.added']

export function personName(rng: Rng, profile: SeedProfile): string {
  const name = `${rng.pick(FIRST_NAMES)} ${rng.pick(LAST_NAMES)}`
  if (rng.chance(profile.strings.emojiNameRate)) return `${name} ${rng.pick(EMOJI)}`
  return name
}

// ---------- profile types ----------

export interface SeedProfile {
  time: {
    historyDays: number
    hotRecentDays: number
    hotRecentShare: number
    weekdayWeights: number[]
    hourWeights: number[]
  }
  tables: Record<string, any>
  skew: { whaleCustomers: number; whaleOrderMultiplier: number }
  strings: { emojiNameRate: number; utf8NameRate: number; metadataBytes: { typical: number; max: number } }
  mutations: Record<string, number>
}

export interface SeedCounts {
  tenants: number
  customers: number
  customer_addresses: number
  categories: number
  products: number
  product_variants: number
  warehouses: number
  inventory_balances: number
  orders: number
}

export function scaledCounts(profile: SeedProfile, scale: number): SeedCounts {
  const row = (table: string, min = 1) =>
    Math.max(min, Math.round(profile.tables[table].rows * scale))
  return {
    tenants: Math.max(10, Math.round(profile.tables.tenants.rows * Math.sqrt(scale))),
    customers: row('customers', 100),
    customer_addresses: row('customer_addresses', 120),
    categories: Math.max(20, Math.round(profile.tables.categories.rows * Math.sqrt(scale))),
    products: row('products', 50),
    product_variants: row('product_variants', 80),
    warehouses: Math.max(4, Math.round(profile.tables.warehouses.rows * Math.sqrt(scale))),
    inventory_balances: row('inventory_balances', 200),
    orders: row('orders', 200),
  }
}

// ---------- batched writer ----------

class BatchWriter {
  private rows: string[] = []
  total = 0
  constructor(
    private conn: mysql.Connection,
    private insertPrefix: string,
    private batchSize = 2000,
  ) {}
  async push(rowSql: string) {
    this.rows.push(rowSql)
    this.total += 1
    if (this.rows.length >= this.batchSize) await this.flush()
  }
  async flush() {
    if (this.rows.length === 0) return
    await this.conn.query(`${this.insertPrefix} VALUES ${this.rows.join(',')}`)
    this.rows = []
  }
}

// ---------- main seeder ----------

/// Fixed time anchor: keeps generated data and query parameter substitution
/// deterministic across seeding runs and dataset loads.
export const SEED_ANCHOR = new Date('2026-07-01T00:00:00Z')

export interface SeedResult {
  counts: SeedCounts
  childCounts: Record<string, number>
  tenantOfCustomer: Uint32Array
  customersByTenantSample: Map<number, number[]>
  now: Date
}

export async function seedWorkload(
  conn: mysql.Connection,
  profile: SeedProfile,
  scale: number,
  seedValue: number,
  log: (m: string) => void,
): Promise<SeedResult> {
  const rng = new Rng(seedValue)
  const counts = scaledCounts(profile, scale)
  const now = SEED_ANCHOR
  const time = new TimeSampler(profile.time, now)
  const esc = (v: string) => conn.escape(v)
  const dec = (v: number) => (Math.round(v * 10000) / 10000).toFixed(4)

  log(`seeding at scale ${scale}: ${counts.orders.toLocaleString()} orders`)
  const started = performance.now()
  // Children batch-flush independently of parents; generated data is
  // referentially consistent, so FK enforcement during bulk load is disabled.
  await conn.query('SET SESSION foreign_key_checks=0')

  // --- tenants ---
  {
    const w = new BatchWriter(conn, 'INSERT INTO tenants (external_id,name,plan,country,created_at,updated_at,deleted_at)')
    for (let i = 0; i < counts.tenants; i += 1) {
      const created = time.sample(rng)
      const plan = weightedPick(rng, { free: 0.3, growth: 0.4, scale: 0.2, enterprise: 0.1 })
      const country = weightedPick(rng, profile.tables.orders.distributions.shipping_country)
      await w.push(`(UNHEX('${rng.hex(16)}'),${esc(`Tenant ${i + 1}`)},'${plan}','${country}','${sqlDatetime(created)}','${sqlDatetime(created)}',NULL)`)
    }
    await w.flush()
  }

  // --- categories (two-level hierarchy) ---
  {
    const w = new BatchWriter(conn, 'INSERT INTO categories (parent_id,name,path,created_at,updated_at)')
    const roots = Math.max(5, Math.floor(counts.categories / 10))
    for (let i = 0; i < counts.categories; i += 1) {
      const isRoot = i < roots
      const parent = isRoot ? 'NULL' : String(1 + rng.int(roots))
      const name = `${rng.pick(PRODUCT_ADJ)} ${rng.pick(PRODUCT_NOUN)} ${i}`
      const path = isRoot ? `/${i}` : `/${parent}/${i}`
      const created = time.sample(rng)
      await w.push(`(${parent},${esc(name)},${esc(path)},'${sqlDatetime(created)}','${sqlDatetime(created)}')`)
    }
    await w.flush()
  }

  // --- warehouses ---
  {
    const w = new BatchWriter(conn, 'INSERT INTO warehouses (tenant_id,code,name,country,region,created_at,updated_at)')
    for (let i = 0; i < counts.warehouses; i += 1) {
      const tenant = 1 + rng.int(Math.min(counts.tenants, 50)) // big tenants own warehouses
      const country = weightedPick(rng, profile.tables.orders.distributions.shipping_country)
      const region = REGIONS[country] ? rng.pick(REGIONS[country]) : null
      const created = time.sample(rng)
      await w.push(`(${tenant},'WH${String(i + 1).padStart(3, '0')}',${esc(`Warehouse ${i + 1}`)},'${country}',${region ? esc(region) : 'NULL'},'${sqlDatetime(created)}','${sqlDatetime(created)}')`)
    }
    await w.flush()
  }

  // --- customers (zipf tenant assignment; remember tenant per customer) ---
  const tenantZipf = new Zipf(counts.tenants, profile.tables.orders.distributions.tenant_id.parameter)
  const tenantOfCustomer = new Uint32Array(counts.customers + 1)
  {
    const w = new BatchWriter(conn, 'INSERT INTO customers (external_id,tenant_id,email,full_name,locale,marketing_opt_in,lifetime_value,created_at,updated_at,deleted_at)')
    for (let i = 1; i <= counts.customers; i += 1) {
      const tenant = 1 + tenantZipf.sample(rng)
      tenantOfCustomer[i] = tenant
      const created = time.sample(rng)
      const name = personName(rng, profile)
      const locale = rng.pick(['en-IN', 'en-US', 'ta-IN', 'hi-IN', 'de-DE', 'fr-FR', null as unknown as string])
      const softDeleted = rng.chance(0.002)
      await w.push(`(UNHEX('${rng.hex(16)}'),${tenant},${esc(`user${i}@example.com`)},${esc(name)},${locale ? esc(locale) : 'NULL'},${rng.chance(0.4) ? 1 : 0},0,'${sqlDatetime(created)}','${sqlDatetime(created)}',${softDeleted ? `'${sqlDatetime(addSeconds(created, 86_400))}'` : 'NULL'})`)
    }
    await w.flush()
  }

  // customersByTenant sample for query parameterization (hot tenants only)
  const customersByTenantSample = new Map<number, number[]>()
  for (let i = 1; i <= counts.customers; i += 1) {
    const t = tenantOfCustomer[i]
    if (t <= 20) {
      const arr = customersByTenantSample.get(t) ?? []
      if (arr.length < 1000) arr.push(i)
      customersByTenantSample.set(t, arr)
    }
  }

  // --- customer addresses ---
  {
    const w = new BatchWriter(conn, 'INSERT INTO customer_addresses (customer_id,tenant_id,kind,country,region,city,postal_code,is_default,created_at,updated_at)')
    for (let i = 0; i < counts.customer_addresses; i += 1) {
      const customer = 1 + rng.int(counts.customers)
      const country = weightedPick(rng, profile.tables.orders.distributions.shipping_country)
      const region = REGIONS[country] && rng.chance(0.85) ? rng.pick(REGIONS[country]) : null
      const created = time.sample(rng)
      await w.push(`(${customer},${tenantOfCustomer[customer]},'${i % 2 === 0 ? 'shipping' : 'billing'}','${country}',${region ? esc(region) : 'NULL'},${esc(rng.pick(CITIES))},${rng.chance(0.9) ? esc(String(10000 + rng.int(89999))) : 'NULL'},${i % 2},'${sqlDatetime(created)}','${sqlDatetime(created)}')`)
    }
    await w.flush()
  }

  // --- products + variants ---
  const variantPrice = new Float64Array(counts.product_variants + 1)
  const variantProduct = new Uint32Array(counts.product_variants + 1)
  const variantCurrency: string[] = new Array(counts.product_variants + 1)
  {
    const w = new BatchWriter(conn, 'INSERT INTO products (external_id,tenant_id,category_id,name,description,brand,created_at,updated_at,deleted_at)')
    for (let i = 1; i <= counts.products; i += 1) {
      const tenant = 1 + tenantZipf.sample(rng)
      const category = 1 + rng.int(counts.categories)
      const name = `${rng.pick(PRODUCT_ADJ)} ${rng.pick(PRODUCT_NOUN)} ${rng.chance(0.05) ? rng.pick(EMOJI) : ''} #${i}`
      const created = time.sample(rng)
      const description = rng.chance(0.7) ? esc(`Description for product ${i}. `.repeat(1 + rng.int(6))) : 'NULL'
      await w.push(`(UNHEX('${rng.hex(16)}'),${tenant},${category},${esc(name)},${description},${rng.chance(0.8) ? esc(`Brand${rng.int(500)}`) : 'NULL'},'${sqlDatetime(created)}','${sqlDatetime(created)}',${rng.chance(0.01) ? `'${sqlDatetime(now)}'` : 'NULL'})`)
    }
    await w.flush()
  }
  {
    const w = new BatchWriter(conn, 'INSERT INTO product_variants (product_id,tenant_id,sku,attributes,currency,list_price,cost_price,weight_grams,created_at,updated_at,deleted_at)')
    const currencyDist = profile.tables.orders.distributions.currency
    for (let i = 1; i <= counts.product_variants; i += 1) {
      const product = 1 + rng.int(counts.products)
      variantProduct[i] = product
      const currency = weightedPick(rng, currencyDist)
      variantCurrency[i] = currency
      const price = 1 + Math.round(Math.exp(rng.next() * 7) * 100) / 100 // long-tail up to ~$1000
      variantPrice[i] = price
      const created = time.sample(rng)
      const attributes = rng.chance(0.75)
        ? esc(JSON.stringify({ size: rng.pick(['S', 'M', 'L', 'XL']), color: rng.pick(['red', 'blue', 'black', 'green']) }))
        : 'NULL'
      await w.push(`(${product},1,${esc(`SKU-${i}-${rng.hex(3).toUpperCase()}`)},${attributes},'${currency}',${dec(price)},${rng.chance(0.8) ? dec(price * 0.6) : 'NULL'},${rng.chance(0.9) ? 50 + rng.int(5000) : 'NULL'},'${sqlDatetime(created)}','${sqlDatetime(created)}',${rng.chance(0.008) ? `'${sqlDatetime(now)}'` : 'NULL'})`)
    }
    await w.flush()
  }

  // --- inventory balances ---
  {
    const w = new BatchWriter(conn, 'INSERT INTO inventory_balances (tenant_id,variant_id,warehouse_id,on_hand,reserved,reorder_point,updated_at)')
    const pairs = new Set<number>()
    // The (variant, warehouse) pair space bounds how many unique balances exist.
    const target = Math.min(
      counts.inventory_balances,
      counts.product_variants * counts.warehouses,
    )
    let written = 0
    while (written < target) {
      const variant = 1 + rng.int(counts.product_variants)
      const warehouse = 1 + rng.int(counts.warehouses)
      const key = variant * (counts.warehouses + 1) + warehouse
      if (pairs.has(key)) continue
      pairs.add(key)
      written += 1
      const updated = time.sample(rng)
      await w.push(`(1,${variant},${warehouse},${rng.int(500)},${rng.int(40)},${rng.chance(0.7) ? 10 + rng.int(90) : 'NULL'},'${sqlDatetime(updated)}')`)
    }
    await w.flush()
  }

  // --- orders + children in one streaming pass ---
  const orderDist = profile.tables.orders.distributions
  const nullRates = profile.tables.orders.nullRates
  const itemsDist = profile.tables.order_items.rowsPerParent
  const customerZipf = new Zipf(counts.customers, orderDist.customer_id.parameter)
  const whales = profile.skew.whaleCustomers

  const wOrders = new BatchWriter(conn, 'INSERT INTO orders (external_id,tenant_id,customer_id,currency,subtotal_amount,discount_amount,tax_amount,shipping_amount,total_amount,order_status,payment_status,fulfillment_status,shipping_country,shipping_region,sales_channel,promotion_code,metadata,placed_at,cancelled_at,completed_at,updated_at,deleted_at)')
  const wItems = new BatchWriter(conn, 'INSERT INTO order_items (order_id,tenant_id,product_variant_id,sku,product_name,quantity,unit_price,discount_amount,tax_amount,total_amount,created_at,updated_at)')
  const wPayments = new BatchWriter(conn, 'INSERT INTO payments (order_id,tenant_id,attempt,provider,method,status,failure_code,currency,amount,provider_ref,created_at,updated_at)')
  const wRefunds = new BatchWriter(conn, 'INSERT INTO refunds (order_id,payment_id,tenant_id,reason,status,currency,amount,created_at,updated_at)')
  const wShipments = new BatchWriter(conn, 'INSERT INTO shipments (order_id,tenant_id,warehouse_id,carrier,tracking_code,status,shipped_at,delivered_at,created_at,updated_at)')
  const wShipItems = new BatchWriter(conn, 'INSERT INTO shipment_items (shipment_id,order_item_id,tenant_id,quantity,created_at)')
  const wEvents = new BatchWriter(conn, 'INSERT INTO order_events (order_id,tenant_id,event_type,actor,payload,created_at)')

  let itemId = 0
  let paymentId = 0
  let shipmentId = 0
  const providerDist = profile.tables.payments.distributions.provider
  const failureDist = profile.tables.payments.distributions.failure_code

  for (let orderId = 1; orderId <= counts.orders; orderId += 1) {
    // customer: whales get boosted probability
    let customer: number
    if (rng.chance(0.04) && whales > 0) customer = 1 + rng.int(Math.min(whales, counts.customers))
    else customer = 1 + customerZipf.sample(rng)
    const tenant = tenantOfCustomer[customer]
    const placed = time.sample(rng)

    const orderStatus = weightedPick(rng, orderDist.order_status)
    // correlated: payment/fulfillment condition on order status
    let paymentStatus: string
    let fulfillment: string
    if (orderStatus === 'cancelled') {
      paymentStatus = weightedPick(rng, { failed: 0.45, refunded: 0.35, pending: 0.2 })
      fulfillment = 'unfulfilled'
    } else if (orderStatus === 'pending') {
      paymentStatus = weightedPick(rng, { pending: 0.7, authorized: 0.25, failed: 0.05 })
      fulfillment = 'unfulfilled'
    } else if (orderStatus === 'confirmed') {
      paymentStatus = weightedPick(rng, { paid: 0.8, authorized: 0.2 })
      fulfillment = weightedPick(rng, { unfulfilled: 0.5, partial: 0.35, fulfilled: 0.15 })
    } else {
      paymentStatus = weightedPick(rng, { paid: 0.95, refunded: 0.05 })
      fulfillment = weightedPick(rng, { fulfilled: 0.93, partial: 0.03, returned: 0.04 })
    }

    // items: long-tail count
    const u = rng.next()
    let itemCount =
      u < itemsDist.p1 ? 1
      : u < itemsDist.p1 + itemsDist.p2 ? 2
      : u < itemsDist.p1 + itemsDist.p2 + itemsDist.p3 ? 3
      : u < itemsDist.p1 + itemsDist.p2 + itemsDist.p3 + itemsDist.p4 ? 4
      : u < 1 - itemsDist.p9to40 ? 5 + rng.int(4)
      : 9 + rng.int(32)

    let subtotal = 0
    const currency = weightedPick(rng, orderDist.currency)
    const orderItemIds: Array<{ id: number; qty: number }> = []
    for (let k = 0; k < itemCount; k += 1) {
      itemId += 1
      const variant = 1 + rng.int(counts.product_variants)
      const qty = 1 + (rng.chance(0.8) ? rng.int(3) : rng.int(20))
      const unit = variantPrice[variant]
      const lineDiscount = rng.chance(0.25) ? unit * qty * 0.1 : 0
      const lineTax = (unit * qty - lineDiscount) * 0.18
      const lineTotal = unit * qty - lineDiscount + lineTax
      subtotal += unit * qty
      orderItemIds.push({ id: itemId, qty })
      await wItems.push(`(${orderId},${tenant},${variant},${esc(`SKU-${variant}`)},${esc(`${rng.pick(PRODUCT_ADJ)} ${rng.pick(PRODUCT_NOUN)}`)},${qty},${dec(unit)},${dec(lineDiscount)},${dec(lineTax)},${dec(lineTotal)},'${sqlDatetime(placed)}','${sqlDatetime(placed)}')`)
    }
    const discount = rng.chance(1 - nullRates.promotion_code) ? subtotal * 0.08 : 0
    const tax = (subtotal - discount) * 0.18
    const shipping = rng.chance(0.6) ? 4.99 : 0
    const total = subtotal - discount + tax + shipping

    const country = weightedPick(rng, orderDist.shipping_country)
    const region = REGIONS[country] && !rng.chance(nullRates.shipping_region) ? rng.pick(REGIONS[country]) : null
    const cancelled = orderStatus === 'cancelled' ? addSeconds(placed, 3600 + rng.int(86_400 * 3)) : null
    const completed = orderStatus === 'completed' ? addSeconds(placed, 3600 * 4 + rng.int(86_400 * 9)) : null
    const metadata = rng.chance(1 - nullRates.metadata)
      ? esc(JSON.stringify({
          source: rng.pick(['organic', 'ads', 'email', 'referral']),
          device: rng.pick(['ios', 'android', 'desktop']),
          note: rng.chance(0.1) ? `gift ${rng.pick(EMOJI)} wrap please`.repeat(1 + rng.int(8)) : undefined,
        }))
      : 'NULL'
    const promo = discount > 0 ? esc(`PROMO${rng.int(200)}`) : 'NULL'
    const softDeleted = rng.chance(1 - nullRates.deleted_at)

    await wOrders.push(`(UNHEX('${rng.hex(16)}'),${tenant},${customer},'${currency}',${dec(subtotal)},${dec(discount)},${dec(tax)},${dec(shipping)},${dec(total)},'${orderStatus}','${paymentStatus}','${fulfillment}','${country}',${region ? esc(region) : 'NULL'},'${weightedPick(rng, orderDist.sales_channel)}',${promo},${metadata},'${sqlDatetime(placed)}',${cancelled ? `'${sqlDatetime(cancelled)}'` : 'NULL'},${completed ? `'${sqlDatetime(completed)}'` : 'NULL'},'${sqlDatetime(placed)}',${softDeleted ? `'${sqlDatetime(now)}'` : 'NULL'})`)

    // payments: attempts with failures before success
    const attemptsDist = profile.tables.payments.attemptsPerOrder
    const ua = rng.next()
    const attempts = ua < attemptsDist.one ? 1 : ua < attemptsDist.one + attemptsDist.two ? 2 : 3 + rng.int(2)
    for (let a = 1; a <= attempts; a += 1) {
      paymentId += 1
      const isLast = a === attempts
      let status: string
      if (!isLast) status = 'failed'
      else if (paymentStatus === 'paid' || paymentStatus === 'refunded') status = 'captured'
      else if (paymentStatus === 'failed') status = 'failed'
      else if (paymentStatus === 'authorized') status = 'authorized'
      else status = 'pending'
      const failure = status === 'failed' ? `'${weightedPick(rng, failureDist)}'` : 'NULL'
      const at = addSeconds(placed, (a - 1) * (60 + rng.int(600)))
      await wPayments.push(`(${orderId},${tenant},${a},'${weightedPick(rng, providerDist)}','${rng.pick(['card', 'upi', 'netbanking', 'wallet', 'cod'])}','${status}',${failure},'${currency}',${dec(total)},${rng.chance(0.9) ? esc(`ref_${rng.hex(8)}`) : 'NULL'},'${sqlDatetime(at)}','${sqlDatetime(at)}')`)
    }

    // refunds
    if (paymentStatus === 'refunded' || rng.chance(profile.tables.refunds.ordersWithRefundRate * 0.5)) {
      const partial = rng.chance(profile.tables.refunds.partialShare)
      const at = addSeconds(placed, 86_400 * (2 + rng.int(20)))
      await wRefunds.push(`(${orderId},${paymentId},${tenant},'${rng.pick(['damaged', 'wrong_item', 'late', 'customer_request', 'fraud'])}','${weightedPick(rng, { processed: 0.85, pending: 0.08, approved: 0.04, rejected: 0.03 })}','${currency}',${dec(partial ? total * (0.1 + rng.next() * 0.5) : total)},'${sqlDatetime(at)}','${sqlDatetime(at)}')`)
    }

    // shipments + shipment_items
    if (fulfillment === 'fulfilled' || fulfillment === 'partial' || fulfillment === 'returned') {
      const perDist = profile.tables.shipments.perFulfilledOrder
      const us = rng.next()
      const shipmentCount = us < perDist.one ? 1 : us < perDist.one + perDist.two ? 2 : 3
      for (let s = 0; s < shipmentCount; s += 1) {
        shipmentId += 1
        const createdAt = addSeconds(placed, 3600 * (2 + rng.int(48)))
        const shippedAt = addSeconds(createdAt, 3600 * (1 + rng.int(24)))
        const delivered = fulfillment !== 'partial' || s < shipmentCount - 1
        const deliveredAt = delivered ? addSeconds(shippedAt, 3600 * (12 + rng.int(120))) : null
        const status = delivered ? 'delivered' : weightedPick(rng, { in_transit: 0.7, picked: 0.2, pending: 0.08, lost: 0.02 })
        await wShipments.push(`(${orderId},${tenant},${1 + rng.int(counts.warehouses)},'${rng.pick(CARRIERS)}',${esc(`TRK${rng.hex(9).toUpperCase()}`)},'${status}','${sqlDatetime(shippedAt)}',${deliveredAt ? `'${sqlDatetime(deliveredAt)}'` : 'NULL'},'${sqlDatetime(createdAt)}','${sqlDatetime(createdAt)}')`)
        // spread order items across shipments
        for (const item of orderItemIds.filter((_, idx) => idx % shipmentCount === s)) {
          await wShipItems.push(`(${shipmentId},${item.id},${tenant},${item.qty},'${sqlDatetime(createdAt)}')`)
        }
      }
    }

    // order events (append-only audit trail)
    const eventCfg = profile.tables.order_events.eventsPerOrder
    const eventCount = Math.min(eventCfg.max, Math.max(eventCfg.min, Math.round(eventCfg.mean + (rng.next() - 0.5) * 4)))
    for (let e = 0; e < eventCount; e += 1) {
      const type = e === 0 ? 'order.created' : rng.pick(EVENT_TYPES_LIFECYCLE)
      const at = addSeconds(placed, e * (600 + rng.int(7200)))
      const payload = rng.chance(0.6) ? esc(JSON.stringify({ seq: e, by: rng.pick(['system', 'webhook']) })) : 'NULL'
      await wEvents.push(`(${orderId},${tenant},'${type}','${rng.pick(['system', 'customer', 'operator', 'webhook'])}',${payload},'${sqlDatetime(at)}')`)
    }

    if (orderId % 100_000 === 0) {
      log(`  orders ${orderId.toLocaleString()} / ${counts.orders.toLocaleString()} (items ${itemId.toLocaleString()}, events ${wEvents.total.toLocaleString()})`)
    }
  }

  await wOrders.flush()
  await wItems.flush()
  await wPayments.flush()
  await wRefunds.flush()
  await wShipments.flush()
  await wShipItems.flush()
  await wEvents.flush()
  await conn.query('SET SESSION foreign_key_checks=1')

  const childCounts = {
    order_items: wItems.total,
    payments: wPayments.total,
    refunds: wRefunds.total,
    shipments: wShipments.total,
    shipment_items: wShipItems.total,
    order_events: wEvents.total,
  }
  log(`seed completed in ${Math.round((performance.now() - started) / 1000)}s: ${JSON.stringify(childCounts)}`)
  return { counts, childCounts, tenantOfCustomer, customersByTenantSample, now }
}

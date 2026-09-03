// Dataset loader: resolve a published pintail-ds dataset (local checkout or
// GitHub), verify checksums, bulk-load via LOAD DATA INFILE with secondary
// indexes deferred to after the load, and reconstruct the metadata that query
// parameterization needs. Returns a SeedResult equivalent to live seeding.

import { createHash } from 'node:crypto'
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import type mysql from 'mysql2/promise'
import { SEED_ANCHOR } from './seed'
import type { SeedResult } from './seed'
import { TABLES } from './validations'

const RAW_BASE = 'https://raw.githubusercontent.com/chittihq/pintail-ds/main'

interface ManifestPart {
  name: string
  bytes: number
  sha256: string
  urls?: string[]
}

interface ManifestFile extends ManifestPart {
  rows: number
  parts?: ManifestPart[]
}

interface DsManifest {
  workload: string
  hash: string
  profile: string
  scale: number
  files: ManifestFile[]
}

export interface LoadOptions {
  workloadId: string
  alias: string
  dsRepo: string
  cacheDir: string
  workloadDir: string
  mysqlName: string
  docker: (...args: string[]) => Promise<string>
  log: (m: string) => void
}

async function sha256File(path: string): Promise<string> {
  const hasher = createHash('sha256')
  hasher.update(new Uint8Array(await Bun.file(path).arrayBuffer()))
  return hasher.digest('hex')
}

async function fetchBytes(urls: string[]): Promise<Uint8Array | null> {
  for (const url of urls) {
    try {
      const response = await fetch(url)
      if (response.ok) return new Uint8Array(await response.arrayBuffer())
    } catch {}
  }
  return null
}

async function readJson<T>(localPath: string, rawUrl: string): Promise<T> {
  if (existsSync(localPath)) return JSON.parse(readFileSync(localPath, 'utf8')) as T
  const bytes = await fetchBytes([rawUrl])
  if (!bytes) throw new Error(`cannot resolve ${rawUrl} (no local checkout, fetch failed)`)
  return JSON.parse(new TextDecoder().decode(bytes)) as T
}

export async function resolveDataset(
  o: LoadOptions,
): Promise<{ manifest: DsManifest; dir: string }> {
  const workloadPath = `datasets/${o.workloadId}`
  const aliases = await readJson<Record<string, string>>(
    join(o.dsRepo, workloadPath, 'aliases.json'),
    `${RAW_BASE}/${workloadPath}/aliases.json`,
  )
  const hash = aliases[o.alias] ?? o.alias
  const manifest = await readJson<DsManifest>(
    join(o.dsRepo, workloadPath, hash, 'manifest.json'),
    `${RAW_BASE}/${workloadPath}/${hash}/manifest.json`,
  )
  const dir = join(o.cacheDir, hash)
  mkdirSync(dir, { recursive: true })

  for (const file of manifest.files) {
    const target = join(dir, file.name)
    if (existsSync(target) && (await sha256File(target)) === file.sha256) continue
    const localData = join(o.dsRepo, workloadPath, hash, 'data', file.name)
    if (existsSync(localData)) {
      writeFileSync(target, readFileSync(localData))
    } else if (file.parts && file.parts.length > 0) {
      const buffers: Uint8Array[] = []
      for (const part of file.parts) {
        const bytes = await fetchBytes(part.urls ?? [])
        if (!bytes) throw new Error(`failed to fetch part ${part.name}`)
        const digest = createHash('sha256').update(bytes).digest('hex')
        if (digest !== part.sha256) throw new Error(`part checksum mismatch: ${part.name}`)
        buffers.push(bytes)
      }
      const whole = new Uint8Array(buffers.reduce((a, b) => a + b.length, 0))
      let offset = 0
      for (const buffer of buffers) {
        whole.set(buffer, offset)
        offset += buffer.length
      }
      writeFileSync(target, whole)
    } else {
      const bytes = await fetchBytes(file.urls ?? [])
      if (!bytes) throw new Error(`failed to fetch ${file.name} from all mirrors`)
      writeFileSync(target, bytes)
    }
    const digest = await sha256File(target)
    if (digest !== file.sha256) {
      throw new Error(`checksum mismatch for ${file.name}: expected ${file.sha256}, got ${digest}`)
    }
    o.log(`fetched ${file.name} (${file.rows.toLocaleString()} rows)`)
  }
  return { manifest, dir }
}

/// Split the schema into an index-free CREATE set plus one ALTER per table
/// that re-adds every stripped KEY/UNIQUE KEY after the bulk load.
export function stripSecondaryIndexes(schema: string): { stripped: string; alters: string[] } {
  const addsByTable = new Map<string, string[]>()
  let currentTable = ''
  const kept: string[] = []
  for (const line of schema.split('\n')) {
    const create = line.match(/^CREATE TABLE (\w+)/)
    if (create) currentTable = create[1]
    const key = line.match(/^\s{2}((?:UNIQUE )?KEY\s+.*?),?\s*$/)
    if (key && currentTable) {
      const adds = addsByTable.get(currentTable) ?? []
      adds.push(`ADD ${key[1]}`)
      addsByTable.set(currentTable, adds)
      continue
    }
    kept.push(line)
  }
  const stripped = kept.join('\n').replace(/,(\s*\n\s*\)\s*ENGINE)/g, '$1')
  const alters = [...addsByTable.entries()].map(
    ([table, adds]) => `ALTER TABLE ${table} ${adds.join(', ')}`,
  )
  return { stripped, alters }
}

/// Moves the decompressed dataset into the container's secure_file_priv
/// directory. Against a local daemon this is a plain `docker cp`. Against an
/// ssh:// context, `docker cp` streams hundreds of megabytes of tar through
/// the docker API connection and reliably wedges it, so the tar is gzipped
/// locally and piped over plain ssh to a `docker cp -` running against the
/// remote host's own socket instead.
async function copyDatasetIntoContainer(txtDir: string, o: LoadOptions): Promise<void> {
  const context = (await o.docker('context', 'show')).trim()
  const endpoint = (
    await o.docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')
  ).trim()
  if (!endpoint.startsWith('ssh://')) {
    await o.docker('cp', `${txtDir}/.`, `${o.mysqlName}:/var/lib/mysql-files/ds`)
    return
  }
  // URL parsing keeps an IPv6 literal (ssh://user@[fd7a::1]) intact, and
  // ssh wants that literal WITHOUT its brackets: it rejects the bracketed
  // form outright, which read here as an unresolvable hostname "[fd7a".
  const endpointUrl = new URL(endpoint)
  const endpointHost = endpointUrl.hostname.replace(/^\[|\]$/g, '')
  const target = endpointUrl.username
    ? `${endpointUrl.username}@${endpointHost}`
    : endpointHost
  o.log('copying dataset over ssh (gzipped tar into remote docker cp)')
  // --no-xattrs/--no-mac-metadata: macOS tar embeds Apple extended
  // attributes that a Linux daemon cannot restore (lsetxattr fails).
  const pipeline = `set -o pipefail; tar --no-xattrs --no-mac-metadata -C ${JSON.stringify(txtDir)} -cf - . | gzip -1 | ssh ${JSON.stringify(target)} 'gzip -dc | docker cp - ${JSON.stringify(`${o.mysqlName}:/var/lib/mysql-files/ds`)}'`
  // Job control puts the pipeline in its own process group so the whole
  // chain can be signalled. Killing the shell alone leaves tar and ssh
  // orphaned to PPID 1, still holding the transfer open — measured, after a
  // deadline fired and the copy carried on regardless.
  const supervised = [
    'set -m',
    `{ ${pipeline} ; } &`,
    'PIPELINE=$!',
    `trap 'kill -TERM -$PIPELINE 2>/dev/null; exit 143' TERM INT`,
    'wait $PIPELINE',
  ].join('\n')
  const child = Bun.spawn(['bash', '-c', supervised], {
    stdout: 'ignore',
    stderr: 'pipe',
  })
  // The transfer emits nothing until it finishes, so report elapsed time to
  // prove the process is alive — but liveness is not progress. A timer keeps
  // printing whether or not a byte moves, which is how a wedged copy used to
  // consume the whole stage budget while looking healthy. Counting bytes
  // would need a second local copy of a multi-gigabyte stream, so instead the
  // copy gets its own deadline: well past the ~17 minutes a healthy transfer
  // takes, far short of the 120-minute stage budget it used to burn.
  const COPY_DEADLINE_SECONDS = 45 * 60
  const started = performance.now()
  let heartbeat: ReturnType<typeof setInterval>
  // eslint-disable-next-line prefer-const
  heartbeat = setInterval(() => {
    const elapsed = Math.round((performance.now() - started) / 1000)
    if (elapsed > COPY_DEADLINE_SECONDS) {
      // Stop the timer before signalling: leaving it armed re-fired every
      // 30 seconds, and that stream of output also kept the stage's stall
      // watchdog from ever noticing the copy had stopped progressing.
      clearInterval(heartbeat)
      o.log(`dataset copy exceeded ${COPY_DEADLINE_SECONDS}s — killing it`)
      child.kill()
      return
    }
    o.log(`still copying dataset (${elapsed}s elapsed, deadline ${COPY_DEADLINE_SECONDS}s)`)
  }, 30_000)
  try {
    if ((await child.exited) !== 0) {
      throw new Error(`ssh dataset copy failed: ${await new Response(child.stderr).text()}`)
    }
  } finally {
    clearInterval(heartbeat)
  }
  o.log(`dataset copied in ${Math.round((performance.now() - started) / 1000)}s`)
}

export async function loadDataset(conn: mysql.Connection, o: LoadOptions): Promise<SeedResult> {
  const started = performance.now()
  const { manifest, dir } = await resolveDataset(o)
  o.log(`loading dataset ${manifest.hash} (profile ${manifest.profile}, scale ${manifest.scale})`)

  // decompress locally, copy into the container's secure_file_priv dir
  const txtDir = join(dir, 'txt')
  mkdirSync(txtDir, { recursive: true })
  for (const file of manifest.files) {
    const out = join(txtDir, file.name.replace(/\.tsv\.zst$/, '.txt'))
    if (!existsSync(out)) {
      const child = Bun.spawn(['zstd', '-d', '-f', join(dir, file.name), '-o', out], {
        stdout: 'ignore',
        stderr: 'pipe',
      })
      if ((await child.exited) !== 0) {
        throw new Error(`zstd -d failed for ${file.name}: ${await new Response(child.stderr).text()}`)
      }
    }
  }
  await o.docker('exec', o.mysqlName, 'mkdir', '-p', '/var/lib/mysql-files/ds')
  await o.docker('exec', o.mysqlName, 'chown', 'mysql:mysql', '/var/lib/mysql-files/ds')
  await copyDatasetIntoContainer(txtDir, o)

  const schema = readFileSync(join(o.workloadDir, 'schema.mysql.sql'), 'utf8')
  const { stripped, alters } = stripSecondaryIndexes(schema)

  await conn.query('SET SESSION sql_log_bin=0')
  await conn.query('CREATE DATABASE production_db')
  await conn.query('USE production_db')
  await conn.query(stripped)
  await conn.query('SET SESSION foreign_key_checks=0, unique_checks=0')
  for (const table of TABLES) {
    const t = performance.now()
    // Binary charset loads BINARY(16) columns verbatim, but JSON columns
    // reject binary strings — route those through @vars + CONVERT.
    const [cols] = await conn.query<mysql.RowDataPacket[]>(
      `SELECT COLUMN_NAME AS name, DATA_TYPE AS type FROM information_schema.columns
       WHERE table_schema='production_db' AND table_name=? ORDER BY ORDINAL_POSITION`,
      [table],
    )
    const columnExprs: string[] = []
    const sets: string[] = []
    for (const col of cols) {
      if (col.type === 'json') {
        columnExprs.push(`@j_${col.name}`)
        sets.push(`\`${col.name}\` = CONVERT(@j_${col.name} USING utf8mb4)`)
      } else {
        columnExprs.push(`\`${col.name}\``)
      }
    }
    await conn.query(
      `LOAD DATA INFILE '/var/lib/mysql-files/ds/${table}.txt' INTO TABLE ${table} ` +
        `CHARACTER SET binary (${columnExprs.join(',')})` +
        (sets.length > 0 ? ` SET ${sets.join(', ')}` : ''),
    )
    o.log(`  loaded ${table} in ${Math.round(performance.now() - t)} ms`)
  }
  await conn.query('SET SESSION foreign_key_checks=1, unique_checks=1')
  o.log('creating secondary indexes')
  for (const alter of alters) {
    await conn.query(alter)
  }
  await o.docker('exec', o.mysqlName, 'rm', '-rf', '/var/lib/mysql-files/ds')
  rmSync(txtDir, { recursive: true, force: true })
  await conn.query('SET SESSION sql_log_bin=1')

  // reconstruct metadata for query parameterization
  const count = async (table: string): Promise<number> => {
    const [rows] = await conn.query<mysql.RowDataPacket[]>(`SELECT COUNT(*) AS c FROM ${table}`)
    return Number(rows[0].c)
  }
  const counts = {
    tenants: await count('tenants'),
    customers: await count('customers'),
    customer_addresses: await count('customer_addresses'),
    categories: await count('categories'),
    products: await count('products'),
    product_variants: await count('product_variants'),
    warehouses: await count('warehouses'),
    inventory_balances: await count('inventory_balances'),
    orders: await count('orders'),
  }
  const tenantOfCustomer = new Uint32Array(counts.customers + 1)
  const customersByTenantSample = new Map<number, number[]>()
  const [customerRows] = await conn.query<mysql.RowDataPacket[]>(
    'SELECT id, tenant_id FROM customers ORDER BY id',
  )
  for (const row of customerRows) {
    const id = Number(row.id)
    const tenant = Number(row.tenant_id)
    if (id < tenantOfCustomer.length) tenantOfCustomer[id] = tenant
    if (tenant <= 20) {
      const arr = customersByTenantSample.get(tenant) ?? []
      if (arr.length < 1000) arr.push(id)
      customersByTenantSample.set(tenant, arr)
    }
  }
  const childCounts = Object.fromEntries(
    manifest.files
      .map((file) => [file.name.replace(/\.tsv\.zst$/, ''), file.rows] as const)
      .filter(([name]) => !(name in counts)),
  )
  o.log(`dataset load completed in ${Math.round((performance.now() - started) / 1000)}s`)
  return { counts, childCounts, tenantOfCustomer, customersByTenantSample, now: SEED_ANCHOR }
}

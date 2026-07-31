// Generate, export, and publish a commerce-production-v1 dataset to pintail-ds.
//
//   bun run publish-dataset.ts --profile smoke [--ds-repo ../pintail-ds]
//
// Pipeline: fresh MySQL container -> deterministic seed -> mysqldump --tab
// (server-side TSV) -> docker cp -> zstd -> sha256 manifest -> write into the
// pintail-ds checkout (data committed in-repo for smoke/ci; full tiers emit
// files + manifest and are uploaded as chunked GitHub Release assets by hand
// or CI — see pintail-ds README).

import { createHash } from 'node:crypto'
import { mkdirSync, readFileSync, rmSync, writeFileSync, readdirSync, statSync, existsSync } from 'node:fs'

async function sha256File(path: string): Promise<string> {
  const hasher = createHash('sha256')
  for await (const chunk of Bun.file(path).stream()) hasher.update(chunk)
  return hasher.digest('hex')
}
import { join, resolve } from 'node:path'
import mysql from 'mysql2/promise'
import { seedWorkload } from './workloads/commerce-production-v1/seed'
import type { SeedProfile } from './workloads/commerce-production-v1/seed'
import { TABLES } from './workloads/commerce-production-v1/validations'
import manifest from './workloads/commerce-production-v1/workload'

function arg(name: string, fallback: string): string {
  const index = process.argv.indexOf(`--${name}`)
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback
}

const workloadId = 'commerce-production-v1'
const profileName = arg('profile', 'smoke') as keyof typeof manifest.profiles
// repo: data committed into pintail-ds (smoke/ci tiers). release: uploaded as
// GitHub Release assets on pintail-ds, chunked below 2 GB (full tier default).
const store = arg('store', profileName === 'full' ? 'release' : 'repo')
const MAX_PART_BYTES = 1_900_000_000
const dsRepo = resolve(arg('ds-repo', join(import.meta.dir, '..', '..', 'pintail-ds')))
const scale = manifest.profiles[profileName]?.scale
if (!scale) throw new Error(`unknown profile ${profileName}`)
const benchmarkDir = import.meta.dir
const workloadDir = join(benchmarkDir, 'workloads', workloadId)
const runId = `pintail-ds-${process.pid}`
const mysqlName = `${runId}-mysql`
const log = (m: string) => console.log(`[publish] ${m}`)

async function command(args: string[]): Promise<string> {
  const child = Bun.spawn(args, { stdout: 'pipe', stderr: 'pipe' })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) throw new Error(`${args.join(' ')} failed (${status}): ${stderr.trim()}`)
  return stdout.trim()
}
const docker = (...args: string[]) => command(['docker', ...args])

function sha256Hex(data: Uint8Array | string): string {
  return createHash('sha256').update(data).digest('hex')
}

async function main() {
  const profileJson = readFileSync(join(workloadDir, 'production-profile.json'), 'utf8')
  const profile = JSON.parse(profileJson) as SeedProfile
  const seederVersion = sha256Hex(readFileSync(join(workloadDir, 'seed.ts'), 'utf8')).slice(0, 16)
  const datasetHash = sha256Hex(
    JSON.stringify({ workloadId, seed: manifest.seed, scale, profileJson, seederVersion }),
  ).slice(0, 16)
  log(`dataset hash ${datasetHash} (profile ${profileName}, scale ${scale}, seeder ${seederVersion})`)

  const datasetDir = join(dsRepo, 'datasets', workloadId, datasetHash)
  if (existsSync(join(datasetDir, 'manifest.json'))) {
    log('manifest already exists for these inputs — nothing to do')
    return
  }

  log('starting MySQL container')
  await docker(
    'run', '-d', '--name', mysqlName, '-p', '0:3306',
    '-e', 'MYSQL_ROOT_PASSWORD=pintail-root', 'mysql:8.4',
    '--max-allowed-packet=268435456', '--skip-log-bin',
  )
  try {
    const context = await docker('context', 'show')
    const endpoint = await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')
    let host = '127.0.0.1'
    if (endpoint.startsWith('ssh://')) {
      const target = endpoint.slice('ssh://'.length).split('@').at(-1)!.split(':')[0]
      const ssh = await command(['ssh', '-G', target])
      host = ssh.split('\n').find((l) => l.startsWith('hostname '))!.slice(9)
    }
    const portOut = await docker('port', mysqlName, '3306/tcp')
    const port = Number(portOut.split('\n')[0].match(/:(\d+)$/)![1])

    let conn: mysql.Connection | undefined
    for (let attempt = 0; attempt < 240 && !conn; attempt += 1) {
      try {
        conn = await mysql.createConnection({
          host, port, user: 'root', password: 'pintail-root',
          multipleStatements: true, supportBigNumbers: true, bigNumberStrings: true,
        })
        await conn.query('SELECT 1')
      } catch {
        conn = undefined
        await Bun.sleep(500)
      }
    }
    if (!conn) throw new Error('MySQL not ready')

    await conn.query('CREATE DATABASE production_db')
    await conn.query('USE production_db')
    await conn.query(readFileSync(join(workloadDir, 'schema.mysql.sql'), 'utf8'))
    const seedResult = await seedWorkload(conn, profile, scale, manifest.seed, log)
    await conn.end()

    log('exporting TSV via mysqldump --tab (in-container)')
    await docker('exec', mysqlName, 'mkdir', '-p', '/var/lib/mysql-files/ds')
    await docker('exec', mysqlName, 'chown', 'mysql:mysql', '/var/lib/mysql-files/ds')
    await docker(
      'exec', mysqlName, 'mysqldump', '-uroot', '-ppintail-root',
      '--tab=/var/lib/mysql-files/ds', '--no-create-info', 'production_db',
    )
    const exportDir = join(benchmarkDir, '.dataset-export', datasetHash)
    rmSync(exportDir, { recursive: true, force: true })
    mkdirSync(exportDir, { recursive: true })
    await docker('cp', `${mysqlName}:/var/lib/mysql-files/ds/.`, exportDir)

    log(`compressing with zstd and hashing (store=${store})`)
    mkdirSync(datasetDir, { recursive: true })
    if (store === 'repo') mkdirSync(join(datasetDir, 'data'), { recursive: true })
    const releaseTag = `ds-${workloadId}-${datasetHash}`
    const files: Array<Record<string, unknown>> = []
    const releaseAssets: string[] = []
    for (const table of TABLES) {
      const txt = join(exportDir, `${table}.txt`)
      if (!existsSync(txt)) throw new Error(`missing export for ${table}`)
      const rows = seedRowCount(table, seedResult)
      await command(['zstd', '-9', '--force', '--rm', txt, '-o', `${txt}.zst`])
      const name = `${table}.tsv.zst`
      const zst = `${txt}.zst`
      const bytes = statSync(zst).size
      const sha256 = await sha256File(zst)
      if (store === 'repo') {
        await command(['cp', zst, join(datasetDir, 'data', name)])
        files.push({
          name, bytes, sha256, rows,
          urls: [
            `https://github.com/chittihq/pintail-ds/raw/main/datasets/${workloadId}/${datasetHash}/data/${name}`,
          ],
        })
      } else if (bytes <= MAX_PART_BYTES) {
        releaseAssets.push(zst)
        files.push({
          name, bytes, sha256, rows,
          urls: [
            `https://github.com/chittihq/pintail-ds/releases/download/${releaseTag}/${name}`,
          ],
        })
      } else {
        log(`  chunking ${name} (${(bytes / 1e9).toFixed(1)} GB)`)
        await command(['split', '-b', '1900m', zst, `${zst}.part-`])
        const parts: Array<Record<string, unknown>> = []
        for (const partFile of readdirSync(exportDir).filter((f) => f.startsWith(`${table}.tsv.zst.part-`)).sort()) {
          const partPath = join(exportDir, partFile)
          releaseAssets.push(partPath)
          parts.push({
            name: partFile,
            bytes: statSync(partPath).size,
            sha256: await sha256File(partPath),
            urls: [
              `https://github.com/chittihq/pintail-ds/releases/download/${releaseTag}/${partFile}`,
            ],
          })
        }
        files.push({ name, bytes, sha256, rows, parts })
      }
    }
    if (store === 'release') {
      log(`creating release ${releaseTag} on chittihq/pintail-ds (${releaseAssets.length} assets)`)
      await command([
        'gh', 'release', 'create', releaseTag, '-R', 'chittihq/pintail-ds',
        '-t', `${workloadId} ${profileName} dataset ${datasetHash}`,
        '-n', `Generated by pintail benchmark/publish-dataset.ts. Profile ${profileName}, scale ${scale}, seed ${manifest.seed}. Verify via the manifest in this repo.`,
      ])
      for (const asset of releaseAssets) {
        log(`  uploading ${asset.split('/').at(-1)}`)
        await command(['gh', 'release', 'upload', releaseTag, '-R', 'chittihq/pintail-ds', asset])
      }
    }
    rmSync(exportDir, { recursive: true, force: true })

    writeFileSync(
      join(datasetDir, 'manifest.json'),
      JSON.stringify(
        {
          workload: workloadId,
          hash: datasetHash,
          profile: profileName,
          scale,
          seed: manifest.seed,
          seederVersion,
          generatedAt: new Date().toISOString(),
          mysqlImage: 'mysql:8.4',
          format: 'mysqldump-tab-tsv+zstd',
          files,
        },
        null,
        2,
      ),
    )
    const aliasFile = join(dsRepo, 'datasets', workloadId, 'aliases.json')
    const aliases = existsSync(aliasFile) ? JSON.parse(readFileSync(aliasFile, 'utf8')) : {}
    aliases[profileName] = datasetHash
    writeFileSync(aliasFile, JSON.stringify(aliases, null, 2))
    const totalBytes = files.reduce((a, f) => a + (f.bytes as number), 0)
    log(`published ${files.length} files, ${(totalBytes / 1e6).toFixed(1)} MB → ${datasetDir}`)
    log(`alias '${profileName}' → ${datasetHash}`)
  } finally {
    await docker('rm', '-f', mysqlName).catch(() => {})
  }
}

function seedRowCount(table: string, seedResult: Awaited<ReturnType<typeof seedWorkload>>): number {
  const counts = seedResult.counts as unknown as Record<string, number>
  if (table in counts) return counts[table]
  if (table in seedResult.childCounts) return seedResult.childCounts[table]
  return -1
}

main().catch((error) => {
  console.error(error)
  process.exit(1)
})

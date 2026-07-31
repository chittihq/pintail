// Verifies that pintail-ds dataset aliases are reproducible by the CURRENT
// seeder: recomputes the dataset hash from today's inputs and compares it to
// each alias target. Drift means the alias points at data the current code can
// no longer regenerate — acceptable (datasets are immutable and checksummed)
// but worth a loud warning; pass --strict to fail instead.
//
//   bun run verify-dataset-provenance.ts [--strict] [--ds-repo ../pintail-ds]

import { createHash } from 'node:crypto'
import { existsSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import manifest from './workloads/commerce-production-v1/workload'

const strict = process.argv.includes('--strict')
const dsRepoIndex = process.argv.indexOf('--ds-repo')
const dsRepo =
  dsRepoIndex >= 0
    ? resolve(process.argv[dsRepoIndex + 1])
    : resolve(join(import.meta.dir, '..', '..', 'pintail-ds'))

const workloadId = 'commerce-production-v1'
const workloadDir = join(import.meta.dir, 'workloads', workloadId)
const RAW = 'https://raw.githubusercontent.com/chittihq/pintail-ds/main'

function sha256Hex(data: string): string {
  return createHash('sha256').update(data).digest('hex')
}

async function readJson<T>(localPath: string, rawUrl: string): Promise<T> {
  if (existsSync(localPath)) return JSON.parse(readFileSync(localPath, 'utf8')) as T
  const response = await fetch(rawUrl)
  if (!response.ok) throw new Error(`cannot fetch ${rawUrl}: ${response.status}`)
  return (await response.json()) as T
}

const profileJson = readFileSync(join(workloadDir, 'production-profile.json'), 'utf8')
const seederVersion = sha256Hex(readFileSync(join(workloadDir, 'seed.ts'), 'utf8')).slice(0, 16)
const aliases = await readJson<Record<string, string>>(
  join(dsRepo, 'datasets', workloadId, 'aliases.json'),
  `${RAW}/datasets/${workloadId}/aliases.json`,
)

let drift = 0
for (const [alias, hash] of Object.entries(aliases)) {
  const scale = manifest.profiles[alias as keyof typeof manifest.profiles]?.scale
  if (!scale) {
    console.log(`?  alias '${alias}' has no matching profile — skipping`)
    continue
  }
  const expected = sha256Hex(
    JSON.stringify({ workloadId, seed: manifest.seed, scale, profileJson, seederVersion }),
  ).slice(0, 16)
  if (expected === hash) {
    console.log(`ok alias '${alias}' → ${hash} (reproducible by current seeder)`)
  } else {
    drift += 1
    console.log(
      `!! alias '${alias}' → ${hash}, but current inputs produce ${expected} — ` +
        'the dataset predates seeder/profile changes; regenerate via publish-dataset.ts ' +
        'to restore reproducibility (data itself remains checksum-verified)',
    )
  }
}

if (drift > 0 && strict) process.exit(1)

// Classify a completed replay's mismatches without changing its comparisons.
import { readFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

type Category = 'fixture-write' | 'routine' | 'diagnostic'
type Entry = { file: string; id: string; category: Category; reason: string }
type Counts = Record<string, number>
type Result = { file: string; counts: Counts }

const directory = dirname(fileURLToPath(import.meta.url))
const run = process.argv[2]
if (!run) throw new Error('Usage: bun run tests/mtr/read-scope.ts <completed-run-directory>')
const root = resolve(run)
const manifest = JSON.parse(readFileSync(join(directory, 'read-scope.json'), 'utf8')) as {
  version: number; entries: Entry[]
}
if (manifest.version !== 1) throw new Error('Unsupported scope manifest version')
const entries = new Map<string, Entry>()
for (const entry of manifest.entries) {
  const key = `${entry.file}/${entry.id}`
  if (entries.has(key) || !/^[a-f0-9]{16}$/.test(entry.id) || !entry.reason.trim()
      || !['fixture-write', 'routine', 'diagnostic'].includes(entry.category)) {
    throw new Error(`Invalid or duplicate scope entry: ${key}`)
  }
  entries.set(key, entry)
}
const result = JSON.parse(readFileSync(join(root, 'results.json'), 'utf8')) as {
  totals: Counts; results: Result[]
}
const provenance = JSON.parse(readFileSync(join(root, 'run.json'), 'utf8')) as {
  provenance: unknown
}
const groups: Record<Category | 'read-or-unresolved', Array<{ file: string; id: string; reason: string }>> = {
  'read-or-unresolved': [], 'fixture-write': [], routine: [], diagnostic: [],
}
let failures = 0
for (const file of result.results) {
  const count = (file.counts.mismatch ?? 0) + (file.counts['name-mismatch'] ?? 0)
  if (!count) continue
  const diff = readFileSync(join(root, 'diffs', `${file.file}.md`), 'utf8')
  const ids = [...diff.matchAll(/^## line \d+ \(([a-f0-9]{16})\)/gm)].map(match => match[1]!)
  if (ids.length !== count || new Set(ids).size !== count) {
    throw new Error(`${file.file}: expected ${count} distinct mismatch IDs, found ${ids.length}`)
  }
  failures += count
  for (const id of ids) {
    const entry = entries.get(`${file.file}/${id}`)
    groups[entry?.category ?? 'read-or-unresolved'].push({
      file: file.file, id,
      reason: entry?.reason ?? 'Read semantics or an unresolved cause; not excluded from the read target.',
    })
  }
}
if (failures !== result.totals.compared! - result.totals.exact!) {
  throw new Error('Mismatch inventory does not reconcile with the original totals')
}
console.log(JSON.stringify({
  run: root, provenance: provenance.provenance,
  originalTotals: result.totals,
  pintailErrors: result.results.reduce((sum, file) => sum + (file.counts['pintail-error'] ?? 0), 0),
  mismatchCounts: Object.fromEntries(Object.entries(groups).map(([category, rows]) => [category, rows.length])),
  mismatches: groups,
  note: 'This is a mismatch breakdown, not a revised agreement denominator. Refused and uncomparable reads remain visible in the original replay report.',
}, null, 2))

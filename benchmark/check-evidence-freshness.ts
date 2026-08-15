/// Fails when banked evidence describes code that no longer exists.
///
/// Every result file in this repository is produced by a run and then
/// committed, which makes it indistinguishable from a fresh one afterwards. It
/// has gone wrong twice: the README's benchmark table shipped through a release
/// still quoting the previous run's numbers, and the TPC-H results recorded a
/// pass produced two days before the join-inference rule they exercise was
/// rewritten. Both looked exactly like current evidence.
///
/// The test is ancestry, not timestamps. For each artifact, the last commit
/// touching the code that produces it must be an ancestor of - or the same as -
/// the last commit touching the artifact. Commit timestamps are not usable for
/// this: a rebase or a cherry-pick reorders them freely, while ancestry
/// survives both.
///
/// Uncommitted changes are ignored deliberately. This answers "does the banked
/// evidence describe HEAD", which is a question about committed history; a
/// dirty tree is already refused by the stages that produce evidence.

import { spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')

function git(args: string[]): string {
  const result = spawnSync('git', args, { cwd: repository, encoding: 'utf8' })
  if (result.status !== 0) throw new Error(`git ${args.join(' ')}: ${result.stderr.trim()}`)
  return result.stdout.trim()
}

/// The last commit that touched any of these paths, or empty when none has.
function lastCommit(paths: string[]): string {
  return git(['log', '-1', '--format=%H', '--', ...paths])
}

function isAncestor(ancestor: string, descendant: string): boolean {
  if (ancestor === descendant) return true
  const result = spawnSync('git', ['merge-base', '--is-ancestor', ancestor, descendant], {
    cwd: repository,
  })
  return result.status === 0
}

interface Artifact {
  /// What the evidence is called in a failure message.
  name: string
  /// Files the run writes. Their newest commit dates the evidence.
  evidence: string[]
  /// Code whose change invalidates that evidence.
  sources: string[]
  /// How to produce it again.
  refresh: string
}

/// The engine crates. A change to any of them can alter both what a query
/// answers and how fast, so every measured artifact depends on all of them.
const ENGINE = [
  'crates/pintail-exec',
  'crates/pintail-sql',
  'crates/pintail-store',
  'crates/pintail-types',
]

const ARTIFACTS: Artifact[] = [
  {
    name: 'analytical benchmark',
    evidence: ['benchmark/results.json', 'benchmark/results.md'],
    sources: [...ENGINE, 'benchmark/run.ts', 'benchmark/queries.ts'],
    refresh: 'bun run scripts/validate.ts --stages=bench',
  },
  {
    name: 'TPC-H workload',
    evidence: ['benchmark/workloads/tpch-v1/results/latest.json'],
    sources: [
      ...ENGINE,
      'benchmark/run-tpch.ts',
      'benchmark/workloads/tpch-v1/queries',
      'benchmark/workloads/tpch-v1/schema.mysql.sql',
      'benchmark/workloads/tpch-v1/seed.ts',
      'benchmark/workloads/tpch-v1/workload.ts',
    ],
    refresh: 'bun run benchmark/run-tpch.ts --profile ci',
  },
  {
    name: 'production workload',
    evidence: ['benchmark/workloads/commerce-production-v1/results/latest.json'],
    sources: [
      ...ENGINE,
      'benchmark/run-production.ts',
      'benchmark/workloads/commerce-production-v1/queries',
      'benchmark/workloads/commerce-production-v1/workload.ts',
    ],
    refresh: 'bun run scripts/validate.ts --stages=accept',
  },
  {
    name: 'end-to-end differential gate',
    evidence: ['tests/e2e/results.md'],
    sources: [...ENGINE, 'tests/e2e/run.ts', 'tests/e2e/cases'],
    refresh: 'bun run scripts/validate.ts --stages=e2e',
  },
]

const stale: string[] = []
for (const artifact of ARTIFACTS) {
  const present = artifact.evidence.filter((path) => existsSync(join(repository, path)))
  if (present.length === 0) continue
  const banked = lastCommit(present)
  if (!banked) continue
  const sources = artifact.sources.filter((path) => existsSync(join(repository, path)))
  const changed = lastCommit(sources)
  if (!changed || isAncestor(changed, banked)) continue
  const subject = git(['log', '-1', '--format=%h %s', changed])
  stale.push(
    `${artifact.name}: banked at ${banked.slice(0, 7)}, but the code changed after it in ${subject}\n` +
      `    refresh with: ${artifact.refresh}`,
  )
}

if (stale.length > 0) {
  console.error('Banked evidence describes code that has since changed:\n')
  for (const entry of stale) console.error(`  ${entry}\n`)
  console.error(
    'Evidence that predates the code it measures is indistinguishable from current\n' +
      'evidence, which is how a release shipped a benchmark table describing an\n' +
      'earlier run. Re-run the stage, or revert the code change.',
  )
  process.exit(1)
}

console.log(`Banked evidence is current for all ${ARTIFACTS.length} artifacts.`)

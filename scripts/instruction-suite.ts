#!/usr/bin/env bun
/// Instruction counts of the executor's query shapes, and their comparison.
///
///   bun run scripts/instruction-suite.ts run [--out counts.tsv] [--target <dir>]
///   bun run scripts/instruction-suite.ts compare <rev-a> <rev-b>
///   bun run scripts/instruction-suite.ts diff <a.tsv> <b.tsv>
///
/// `run` measures the working tree. `compare` checks each revision out
/// beside the repository, lays this tree's suite over it so both engines
/// answer the same cases, and measures both. Linux only: the counts come
/// from Valgrind. See docs/instruction-counts.md.
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const suitePath = 'benchmark/instruction-suite'
const cargo = process.env.CARGO ?? 'cargo'
// What the measured process runs under; the bench sets the same for the
// counted runs, this is for the native answer digests.
const engine = { PINTAIL_DISABLE_SETTLED_MEMO: '1', RAYON_NUM_THREADS: '1', PINTAIL_SCAN_THREADS: '1' }

type Counts = { instructions: number; l1Miss: number; llMiss: number; cycles: number; digest: string }
type Table = Map<string, Counts>

async function sh(command: string[], cwd: string, env: Record<string, string> = {}): Promise<string> {
  const child = Bun.spawn(command, { cwd, env: { ...process.env, ...env }, stdout: 'pipe', stderr: 'inherit' })
  const output = await new Response(child.stdout).text()
  if ((await child.exited) !== 0) throw new Error(`${command.join(' ')} failed in ${cwd}`)
  return output
}

/// Reads the runner's report: one block per case, a line per metric. "LL
/// Hits" are reads the first-level cache missed and the last level served;
/// "RAM Hits" missed both.
function parse(report: string): Table {
  const table: Table = new Map()
  let name = ''
  const seen: Record<string, number> = {}
  for (const line of report.split('\n')) {
    const header = line.match(/^\S+::shape (\w+):/)
    if (header) {
      name = header[1]
      continue
    }
    const metric = line.match(/^\s+(Instructions|LL Hits|RAM Hits|Estimated Cycles):\s+(\d+)\|/)
    if (!metric || !name) continue
    seen[metric[1]] = Number(metric[2])
    if (metric[1] === 'Estimated Cycles') {
      table.set(name, {
        instructions: seen['Instructions'],
        l1Miss: seen['LL Hits'] + seen['RAM Hits'],
        llMiss: seen['RAM Hits'],
        cycles: seen['Estimated Cycles'],
        digest: '',
      })
    }
  }
  if (table.size === 0) throw new Error('the runner reported no case')
  return table
}

async function measure(tree: string, target: string): Promise<Table> {
  const suite = join(tree, suitePath)
  const env = { CARGO_TARGET_DIR: target }
  const table = parse(await sh([cargo, 'bench', '--bench', 'shapes'], suite, env))
  await sh([cargo, 'build', '--release', '--bin', 'answers'], suite, env)
  const answers = await sh([join(target, 'release', 'answers')], suite, engine)
  for (const line of answers.trim().split('\n')) {
    const [name, , digest] = line.split('\t')
    const counts = table.get(name)
    if (counts) counts.digest = digest
  }
  return table
}

const serialize = (table: Table) =>
  ['case\tinstructions\tl1_misses\tll_misses\testimated_cycles\tanswer']
    .concat([...table].map(([name, c]) => [name, c.instructions, c.l1Miss, c.llMiss, c.cycles, c.digest].join('\t')))
    .join('\n') + '\n'

function load(path: string): Table {
  const table: Table = new Map()
  for (const line of readFileSync(path, 'utf8').trim().split('\n').slice(1)) {
    const [name, instructions, l1Miss, llMiss, cycles, digest] = line.split('\t')
    table.set(name, { instructions: +instructions, l1Miss: +l1Miss, llMiss: +llMiss, cycles: +cycles, digest })
  }
  return table
}

const percent = (before: number, after: number) =>
  before === 0 ? 'n/a' : `${after >= before ? '+' : ''}${((after / before - 1) * 100).toFixed(2)}%`

function report(a: Table, b: Table, labelA: string, labelB: string) {
  console.log(`| case | instructions ${labelA} | ${labelB} | change | L1 misses | LL misses | est. cycles | answer |`)
  console.log('|---|---:|---:|---:|---:|---:|---:|---|')
  for (const [name, before] of a) {
    const after = b.get(name)
    if (!after) continue
    console.log(
      `| ${name} | ${before.instructions} | ${after.instructions} | ${percent(before.instructions, after.instructions)} | ` +
        `${percent(before.l1Miss, after.l1Miss)} | ${percent(before.llMiss, after.llMiss)} | ` +
        `${percent(before.cycles, after.cycles)} | ${before.digest === after.digest ? 'same' : 'DIFFERS'} |`,
    )
  }
}

const [mode, ...rest] = process.argv.slice(2)
if (mode === 'run') {
  const flag = (name: string) => (rest.includes(name) ? rest[rest.indexOf(name) + 1] : undefined)
  const out = flag('--out')
  // A second target directory keeps a differently built suite (RUSTFLAGS
  // with a profile, say) apart from the plain one.
  const table = await measure(repository, resolve(flag('--target') ?? join(repository, suitePath, 'target')))
  if (out) writeFileSync(out, serialize(table))
  process.stdout.write(serialize(table))
} else if (mode === 'diff' && rest.length === 2) {
  report(load(rest[0]), load(rest[1]), rest[0], rest[1])
} else if (mode === 'compare' && rest.length === 2) {
  const work = join(repository, 'target', 'instruction-compare')
  mkdirSync(work, { recursive: true })
  const tables: Table[] = []
  const labels: string[] = []
  for (const revision of rest) {
    const sha = (await sh(['git', 'rev-parse', '--short=12', `${revision}^{commit}`], repository)).trim()
    const tree = join(work, `tree-${sha}`)
    if (existsSync(tree)) await sh(['git', 'worktree', 'remove', '--force', tree], repository)
    await sh(['git', 'worktree', 'add', '--detach', tree, sha], repository)
    try {
      rmSync(join(tree, suitePath), { recursive: true, force: true })
      cpSync(join(repository, suitePath), join(tree, suitePath), {
        recursive: true,
        filter: (source) => !source.includes(join(suitePath, 'target')),
      })
      const table = await measure(tree, join(work, `target-${sha}`))
      writeFileSync(join(work, `${sha}.tsv`), serialize(table))
      tables.push(table)
      labels.push(sha)
    } finally {
      await sh(['git', 'worktree', 'remove', '--force', tree], repository)
    }
  }
  report(tables[0], tables[1], labels[0], labels[1])
} else {
  console.error('usage: instruction-suite.ts run [--out file] [--target dir] | compare <rev-a> <rev-b> | diff <a.tsv> <b.tsv>')
  process.exit(2)
}

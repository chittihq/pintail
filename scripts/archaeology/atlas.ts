#!/usr/bin/env bun
/// The bug atlas: what three mature engines had to fix, grouped into the bug
/// classes Pintail can have, with the Pintail gate that covers each class.
///
/// Usage:  bun run scripts/archaeology/atlas.ts fetch     # clone or update
///         bun run scripts/archaeology/atlas.ts extract   # records-*.jsonl
///         bun run scripts/archaeology/atlas.ts render    # docs/design/bug-atlas.md
///
/// Upstream repositories are cloned without file contents into
/// PINTAIL_ARCHAEOLOGY_DIR (default ~/pintail-archaeology), outside this tree.
/// Only paths, dates and bug identifiers are read from their history; the
/// rendered atlas carries counts and Pintail's own class descriptions, never
/// upstream text.
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync, appendFileSync, rmSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'
import { CLASSES, bugIds, classify, testPaths, type Project } from './taxonomy.ts'

const root = resolve(import.meta.dir, '..', '..')
export const dataDir = process.env.PINTAIL_ARCHAEOLOGY_DIR ?? join(homedir(), 'pintail-archaeology')

export const UPSTREAMS: Record<Project, { url: string; dir: string; sparse: string[] }> = {
  mysql: { url: 'https://github.com/mysql/mysql-server.git', dir: 'mysql-server', sparse: ['mysql-test'] },
  mariadb: { url: 'https://github.com/MariaDB/server.git', dir: 'mariadb-server', sparse: ['mysql-test'] },
  clickhouse: { url: 'https://github.com/ClickHouse/ClickHouse.git', dir: 'clickhouse', sparse: ['docs/changelogs', 'CHANGELOG.md'] },
}

export type CommitRecord = {
  project: Project
  sha: string
  date: string
  ids: string[]
  classes: string[]
  tests: string[]
  paths: number
}

function run(cwd: string, ...args: string[]) {
  const proc = Bun.spawnSync(['git', '-C', cwd, ...args], { stdout: 'pipe', stderr: 'pipe' })
  if (proc.exitCode !== 0) throw new Error(`git ${args.join(' ')} failed: ${proc.stderr.toString()}`)
  return proc.stdout.toString()
}

function fetchAll() {
  mkdirSync(dataDir, { recursive: true })
  for (const [project, upstream] of Object.entries(UPSTREAMS)) {
    const dir = join(dataDir, upstream.dir)
    if (!existsSync(join(dir, '.git'))) {
      console.log(`${project}: cloning`)
      const clone = Bun.spawnSync(['git', 'clone', '--filter=blob:none', '--no-checkout', upstream.url, dir], { stdout: 'inherit', stderr: 'inherit' })
      if (clone.exitCode !== 0) throw new Error(`${project}: clone failed`)
    } else {
      console.log(`${project}: fetching`)
      run(dir, 'fetch', '--filter=blob:none', 'origin')
      run(dir, 'reset', '--soft', 'origin/HEAD')
    }
    run(dir, 'sparse-checkout', 'set', ...upstream.sparse)
    run(dir, 'checkout', '--force', 'HEAD')
    console.log(`${project}: ${run(dir, 'rev-parse', 'HEAD').trim()}`)
  }
}

/// Pull requests the ClickHouse changelogs list under a bug-fix heading.
export function changelogBugFixes(markdown: string): Set<number> {
  const prs = new Set<number>()
  let inFix = false
  for (const line of markdown.split('\n')) {
    const heading = line.match(/^#{2,6}\s+(.*)$/)
    if (heading) { inFix = /bug\s*fix/i.test(heading[1]); continue }
    if (!inFix) continue
    for (const m of line.matchAll(/(?:pull\/|#)(\d{3,6})\b/g)) prs.add(Number(m[1]))
  }
  return prs
}

function clickhouseFixes(dir: string): Set<number> {
  const prs = new Set<number>()
  const files = [join(dir, 'CHANGELOG.md')]
  const logs = join(dir, 'docs/changelogs')
  if (existsSync(logs)) for (const name of readdirSync(logs)) if (name.endsWith('.md')) files.push(join(logs, name))
  for (const file of files) if (existsSync(file)) for (const pr of changelogBugFixes(readFileSync(file, 'utf8'))) prs.add(pr)
  return prs
}

async function* logRecords(dir: string, args: string[]) {
  const proc = Bun.spawn(['git', '-C', dir, 'log', ...args, '--name-only', '--format=%x1e%H%x1f%aI%x1f%B%x1d'], { stdout: 'pipe', stderr: 'inherit' })
  const decoder = new TextDecoder()
  let buffer = ''
  const parse = (chunk: string) => {
    const header = chunk.indexOf('\x1d')
    if (header < 0) return undefined
    const [sha, date, message] = chunk.slice(0, header).split('\x1f')
    const paths = chunk.slice(header + 1).split('\n').map((p) => p.trim()).filter(Boolean)
    return { sha, date, message, paths }
  }
  for await (const bytes of proc.stdout) {
    buffer += decoder.decode(bytes, { stream: true })
    let at: number
    while ((at = buffer.indexOf('\x1e', 1)) > 0) {
      const record = parse(buffer.slice(1, at))
      buffer = buffer.slice(at)
      if (record) yield record
    }
  }
  const last = parse(buffer.slice(1))
  if (last) yield last
  if ((await proc.exited) !== 0) throw new Error(`git log failed in ${dir}`)
}

async function extract() {
  for (const [name, upstream] of Object.entries(UPSTREAMS)) {
    const project = name as Project
    const dir = join(dataDir, upstream.dir)
    if (!existsSync(join(dir, '.git'))) { console.log(`${project}: not cloned, skipping`); continue }
    const out = join(dataDir, `records-${project}.jsonl`)
    rmSync(out, { force: true })
    const fixes = project === 'clickhouse' ? clickhouseFixes(dir) : undefined
    // ClickHouse lands every change as a merged pull request, so the merge's
    // diff against its first parent is the change; the other two land fixes
    // as ordinary commits carrying the bug identifier.
    const args = project === 'clickhouse' ? ['--first-parent', '--merges', '-m', 'HEAD'] : ['--no-merges', 'HEAD']
    let seen = 0, kept = 0
    let batch: string[] = []
    for await (const commit of logRecords(dir, args)) {
      seen++
      let ids = bugIds(project, commit.message)
      if (project === 'clickhouse') ids = ids.filter((id) => fixes!.has(Number(id)))
      if (!ids.length) continue
      const tests = testPaths(project, commit.paths)
      const record: CommitRecord = { project, sha: commit.sha, date: commit.date, ids, classes: classify(project, commit.paths), tests, paths: commit.paths.length }
      batch.push(JSON.stringify(record))
      kept++
      if (batch.length >= 2000) { appendFileSync(out, batch.join('\n') + '\n'); batch = [] }
    }
    if (batch.length) appendFileSync(out, batch.join('\n') + '\n')
    const head = run(dir, 'rev-parse', 'HEAD').trim()
    writeFileSync(join(dataDir, `records-${project}.meta.json`), JSON.stringify({ project, head, seen, kept, fixPullRequests: fixes?.size ?? null, extractedAt: new Date().toISOString() }, null, 2) + '\n')
    console.log(`${project}: ${kept} fix commits of ${seen} at ${head.slice(0, 12)}`)
  }
}

export function loadRecords(project: Project): CommitRecord[] {
  const file = join(dataDir, `records-${project}.jsonl`)
  if (!existsSync(file)) return []
  return readFileSync(file, 'utf8').split('\n').filter(Boolean).map((line) => JSON.parse(line))
}

/// Unique bugs per class: a bug fixed by several commits (forward merges,
/// cherry-picks into release branches) counts once, under the union of every
/// class its commits touched.
export function summarize(records: CommitRecord[]) {
  const bugs = new Map<string, { year: number; classes: Set<string>; tested: boolean }>()
  for (const r of records) {
    for (const id of r.ids) {
      const bug = bugs.get(id) ?? { year: Number(r.date.slice(0, 4)), classes: new Set(), tested: false }
      bug.year = Math.min(bug.year, Number(r.date.slice(0, 4)))
      r.classes.forEach((c) => bug.classes.add(c))
      bug.tested ||= r.tests.length > 0
      bugs.set(id, bug)
    }
  }
  const perClass = new Map<string, { bugs: number; tested: number; byEra: Map<string, number> }>()
  let unclassified = 0
  for (const bug of bugs.values()) {
    if (!bug.classes.size) { unclassified++; continue }
    const era = `${Math.floor(bug.year / 5) * 5}`
    for (const c of bug.classes) {
      const row = perClass.get(c) ?? { bugs: 0, tested: 0, byEra: new Map() }
      row.bugs++
      if (bug.tested) row.tested++
      row.byEra.set(era, (row.byEra.get(era) ?? 0) + 1)
      perClass.set(c, row)
    }
  }
  return { total: bugs.size, unclassified, perClass }
}

function render() {
  const projects: Project[] = ['mysql', 'mariadb', 'clickhouse']
  const summaries = Object.fromEntries(projects.map((p) => [p, summarize(loadRecords(p))])) as Record<Project, ReturnType<typeof summarize>>
  const meta = Object.fromEntries(projects.map((p) => {
    const file = join(dataDir, `records-${p}.meta.json`)
    return [p, existsSync(file) ? JSON.parse(readFileSync(file, 'utf8')) : null]
  }))
  const fmt = (n: number) => n.toLocaleString('en-US')
  const lines: string[] = []
  lines.push('# Bug atlas', '')
  lines.push('Generated by `bun run scripts/archaeology/atlas.ts render`. Do not edit by hand.', '')
  lines.push('What two relational engines and one columnar engine had to fix over their')
  lines.push('histories, grouped into the bug classes Pintail can have. A bug counts once')
  lines.push('however many commits fixed it; "with test" means at least one of those commits')
  lines.push('added or changed a regression test. Classes come from the source paths a fix')
  lines.push('touched (`scripts/archaeology/taxonomy.ts`), so a count is a measure of where')
  lines.push('repair work landed, not a severity.', '')
  lines.push('| Upstream | Head | Bugs with fixes | Unclassified |', '|---|---|---:|---:|')
  const label: Record<Project, string> = { mysql: 'MySQL', mariadb: 'MariaDB', clickhouse: 'ClickHouse' }
  for (const p of projects) {
    const s = summaries[p]
    lines.push(`| ${label[p]} | ${meta[p] ? '`' + meta[p].head.slice(0, 12) + '`' : 'not extracted'} | ${fmt(s.total)} | ${fmt(s.unclassified)} |`)
  }
  lines.push('')
  lines.push('## Classes', '')
  lines.push('| Class | Relevance | MySQL | MariaDB | ClickHouse | With test | Pintail gates |', '|---|---|---:|---:|---:|---:|---|')
  const ranked = [...CLASSES].sort((a, b) => {
    const total = (id: string) => projects.reduce((n, p) => n + (summaries[p].perClass.get(id)?.bugs ?? 0), 0)
    const order = { core: 0, adjacent: 1, 'out-of-scope': 2 }
    return order[a.relevance] - order[b.relevance] || total(b.id) - total(a.id)
  })
  for (const cls of ranked) {
    const cells = projects.map((p) => cls.rules[p] ? fmt(summaries[p].perClass.get(cls.id)?.bugs ?? 0) : '-')
    const tested = projects.reduce((n, p) => n + (summaries[p].perClass.get(cls.id)?.tested ?? 0), 0)
    const gates = cls.relevance === 'out-of-scope' ? 'n/a' : cls.gates.length ? cls.gates.map((g) => '`' + g + '`').join(', ') : '**uncovered**'
    lines.push(`| ${cls.title} | ${cls.relevance} | ${cells.join(' | ')} | ${fmt(tested)} | ${gates} |`)
  }
  lines.push('')
  lines.push('## Trajectories', '')
  lines.push('Bugs per class by the five-year period of their first fix, all three upstreams together.', '')
  const eras = [...new Set(projects.flatMap((p) => [...summaries[p].perClass.values()].flatMap((r) => [...r.byEra.keys()])))].sort()
  lines.push(`| Class | ${eras.map((e) => `${e}-${(Number(e) + 4) % 100 < 10 ? '0' : ''}${(Number(e) + 4) % 100}`).join(' | ')} |`)
  lines.push(`|---|${eras.map(() => '---:').join('|')}|`)
  for (const cls of ranked.filter((c) => c.relevance !== 'out-of-scope')) {
    const counts = eras.map((e) => projects.reduce((n, p) => n + (summaries[p].perClass.get(cls.id)?.byEra.get(e) ?? 0), 0))
    lines.push(`| ${cls.title} | ${counts.map(fmt).join(' | ')} |`)
  }
  lines.push('')
  lines.push('## What each class means for Pintail', '')
  for (const cls of ranked.filter((c) => c.relevance !== 'out-of-scope')) lines.push(`- **${cls.title}** - ${cls.description}`)
  lines.push('')
  writeFileSync(join(root, 'docs/design/bug-atlas.md'), lines.join('\n'))
  console.log('wrote docs/design/bug-atlas.md')
}

if (import.meta.main) {
  const command = process.argv[2]
  if (command === 'fetch') fetchAll()
  else if (command === 'extract') await extract()
  else if (command === 'render') render()
  else { console.error('usage: atlas.ts fetch|extract|render'); process.exit(2) }
}

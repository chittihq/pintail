#!/usr/bin/env bun
// Rebuild the audit ledger without Docker or network access. To refresh its
// upstream inventory, supply a completed MySQL checkout and downloaded manual.
import { createHash } from 'node:crypto'
import { execFileSync } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs'
import { join, relative, resolve } from 'node:path'
import { surface } from './function-surface.ts'

const root = resolve(import.meta.dir, '..')
const dir = join(root, 'docs/mysql-parity')
const args = process.argv.slice(2)
const option = (name: string) => {
  const i = args.indexOf(name)
  if (i < 0) return undefined
  if (!args[i + 1] || args[i + 1].startsWith('--')) throw new Error(`Missing ${name} value`)
  return resolve(args[i + 1])
}
const mysql = option('--mysql-source')
const manual = option('--manual-html')
const check = args.includes('--check')
if (!!mysql !== !!manual) throw new Error('Supply both --mysql-source and --manual-html')
if (check && mysql) throw new Error('--check cannot refresh the inventory')
const read = (file: string) => readFileSync(file, 'utf8')
const hash = (value: string) => createHash('sha256').update(value).digest('hex')
const git = (cwd: string, ...command: string[]) => execFileSync('git', ['-C', cwd, ...command], { encoding: 'utf8' }).trim()
const lineAt = (source: string, offset: number) => source.slice(0, offset).split('\n').length
const json = (value: unknown) => JSON.stringify(value, null, 2) + '\n'
const decode = (html: string) => html.replace(/<[^>]+>/g, '').replace(/&(?:amp|lt|gt|quot|apos|nbsp);/g,
  (m) => ({ '&amp;': '&', '&lt;': '<', '&gt;': '>', '&quot;': '"', '&apos;': "'", '&nbsp;': ' ' })[m]!)
  .replace(/\s+/g, ' ').trim()
type SourceRef = { path: string; line: number; mechanism: string }
type UpstreamEntry = { id: string; name: string; forms: string[]; category: string; kind: string; internal: boolean; deprecated: boolean; manual: string[]; source: SourceRef[] }
type Inventory = { schemaVersion: number; baseline: Record<string, string>; entries: UpstreamEntry[] }
mkdirSync(dir, { recursive: true })
let upstream: Inventory
if (mysql && manual) {
  if (git(mysql, 'status', '--porcelain')) throw new Error('MySQL checkout must be clean and complete')
  const html = read(manual)
  const entries = new Map<string, UpstreamEntry>()
  for (const row of html.matchAll(/<tr>\s*<th scope="row">([\s\S]*?)<\/th>([\s\S]*?)<\/tr>/g)) {
    const href = row[1].match(/href="([^"]+)"/)?.[1]
    if (!href) throw new Error('Manual row has no reference')
    const forms = [...row[1].matchAll(/<code[^>]*>([\s\S]*?)<\/code>/g)].map((m) => decode(m[1]))
    if (!forms.length) throw new Error('Manual row has no spelling')
    const operator = href.includes('#operator_') || /^(CASE|BINARY|MATCH|IN\(\)|EXISTS\(\)|MEMBER OF)/.test(forms[0])
    const names = operator ? [forms.join(', ')] : [...new Set(forms.map((s) => s.replace(/\(.*$/, '').toUpperCase()))]
    for (const name of names) {
      const id = operator ? `operator:${href.split('#')[1]}` : `function:${name}`
      const entry = entries.get(id) ?? { id, name, forms: [], category: href.split('.')[0], kind: operator ? 'operator' : 'function', internal: false, deprecated: false, manual: [], source: [] }
      entry.forms = [...new Set([...entry.forms, ...forms])]
      entry.manual = [...new Set([...entry.manual, `https://dev.mysql.com/doc/refman/8.4/en/${href}`])]
      entry.internal ||= /Internal use only/i.test(decode(row[2]))
      entry.deprecated ||= /<td>\s*Yes\s*<\/td>/.test(row[2])
      entries.set(id, entry)
    }
  }
  if (entries.size < 300) throw new Error('Manual extraction incomplete: fewer than 300 entries')
  const addSource = (name: string, ref: SourceRef) => {
    const id = `function:${name}`
    const entry = entries.get(id) ?? { id, name, forms: [name + '()'], category: 'source-only', kind: 'function', internal: false, deprecated: false, manual: [], source: [] }
    entry.source.push(ref)
    entries.set(id, entry)
  }
  const registryPath = 'sql/item_create.cc'
  const registry = read(join(mysql, registryPath))
  const start = registry.indexOf('func_array[]')
  if (start < 0) throw new Error('Native registry changed shape')
  const end = registry.indexOf('};', start)
  if (end < 0) throw new Error('Native registry is unterminated')
  for (const m of registry.slice(start, end).matchAll(/\{\s*"([A-Z0-9_]+)"\s*,/g)) {
    addSource(m[1], { path: registryPath, line: lineAt(registry, start + m.index!), mechanism: 'native-registry' })
    const factory = registry.slice(start + m.index! + m[0].length).match(/^\s*([A-Z0-9_]+)\(/)?.[1]
    if (factory?.includes('INTERNAL')) entries.get(`function:${m[1]}`)!.internal = true
  }
  const lexPath = 'sql/lex.h'
  const lex = read(join(mysql, lexPath))
  for (const m of lex.matchAll(/\{\s*SYM_FN\("([A-Z0-9_]+)"\s*,\s*([A-Z0-9_]+)\)/g)) {
    addSource(m[1], { path: lexPath, line: lineAt(lex, m.index!), mechanism: 'lexer-function' })
  }
  // Grammar-only constructs are located by their own production or lexer token.
  const grammarPath = 'sql/sql_yacc.yy'
  const grammar = read(join(mysql, grammarPath))
  const bodies = grammar.indexOf('%%')
  for (const entry of entries.values()) {
    if (entry.source.length || entry.kind === 'operator') continue
    const escaped = entry.name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
    const symbol = lex.match(new RegExp(`SYM(?:_FN|_HK)?\\("${escaped}"\\s*,\\s*([A-Z0-9_]+)\\)`))?.[1]
    if (!symbol) continue
    const found = new RegExp(`\\b${symbol}\\b`).exec(grammar.slice(bodies))
    if (found) entry.source.push({ path: grammarPath, line: lineAt(grammar, bodies + found.index), mechanism: 'grammar-token' })
  }
  const version = read(join(mysql, 'MYSQL_VERSION'))
  const component = (part: string) => version.match(new RegExp(`MYSQL_VERSION_${part}=(.*)`))?.[1] ?? '?'
  if (component('MAJOR') !== '8' || component('MINOR') !== '4') {
    throw new Error('This inventory uses the 8.4 manual; supply MySQL 8.4 source')
  }
  upstream = {
    schemaVersion: 1,
    baseline: {
      repository: 'https://github.com/mysql/mysql-server', branch: git(mysql, 'branch', '--show-current'),
      commit: git(mysql, 'rev-parse', 'HEAD'), commitDate: git(mysql, 'show', '-s', '--format=%cI', 'HEAD'),
      sourceVersion: ['MAJOR', 'MINOR', 'PATCH'].map(component).join('.'),
      refKind: 'development-branch snapshot; not a release certification',
      manual: 'https://dev.mysql.com/doc/refman/8.4/en/built-in-function-reference.html',
      manualSha256: hash(html), registrySha256: hash(registry), lexerSha256: hash(lex), grammarSha256: hash(grammar),
    },
    entries: [...entries.values()].sort((a, b) => a.id.localeCompare(b.id)),
  }
  writeFileSync(join(dir, 'upstream.json'), json(upstream))
} else upstream = JSON.parse(read(join(dir, 'upstream.json')))

type Assessment = { status: string; note: string; evidence: string[]; scope: string; priority: string; acceptance?: string }
type Feature = Assessment & { id: string; name: string; category: string; source: SourceRef[]; acceptance: string }
const review: { functions: Record<string, Assessment>; operators: Record<string, Assessment>; acceptanceByCategory: Record<string, string>; defaultAcceptance: string } = JSON.parse(read(join(dir, 'review.json')))
const features: Feature[] = JSON.parse(read(join(dir, 'features.json')))
const implementationPaths = ['crates/pintail-sql/src/binder/mod.rs', 'crates/pintail-sql/src/binder/function.rs', 'crates/pintail-sql/src/lib.rs', 'crates/pintail-wire/src/server.rs', 'crates/pintail-write/src/engine.rs']
const implementation = implementationPaths.map((path) => ({ path, sha256: hash(read(join(root, path))) }))
const binder = surface()
const sourceLocation = (name: string) => {
  for (const path of implementationPaths.slice(0, 2)) {
    const content = read(join(root, path))
    const offset = content.indexOf(`"${name}"`)
    if (offset >= 0) return `${path}:${lineAt(content, offset)}`
  }
  return ''
}
// Occurrences are navigation hints, never a claim that an assertion ran/passed.
const corpusFiles: string[] = []
function walk(path: string) {
  for (const entry of readdirSync(join(root, path), { withFileTypes: true })) {
    const child = `${path}/${entry.name}`
    if (entry.isDirectory()) walk(child)
    else if (/\.(rs|sql)$/.test(entry.name)) corpusFiles.push(child)
  }
}
walk('crates/pintail-exec/tests')
walk('tests/sqllogic/tests')
const corpus = corpusFiles.sort().map((path) => ({ path, content: read(join(root, path)) }))
const occurrences = (name: string) => {
  if (!/^[A-Z][A-Z0-9_]*$/.test(name)) return []
  const regex = new RegExp(`\\b${name}\\s*\\(`, 'i')
  return corpus.flatMap(({ path, content }) => {
    const m = regex.exec(content)
    return m ? [`${path}:${lineAt(content, m.index)}`] : []
  }).slice(0, 4)
}
const ids = new Set(upstream.entries.map((e) => e.id))
for (const name of Object.keys(review.functions)) {
  if (!ids.has(`function:${name}`)) throw new Error(`Review references absent upstream function ${name}`)
}
const entries = upstream.entries.map((entry) => {
  const curated: Partial<Assessment> = (entry.kind === 'function' ? review.functions[entry.name] : review.operators[entry.id]) ?? {}
  const arities = entry.kind === 'function' ? [...(binder.get(entry.name) ?? [])] : []
  const location = arities.length ? sourceLocation(entry.name) : ''
  const status = curated.status ?? (entry.internal ? 'out-of-scope' : arities.length ? 'implemented-unverified' : 'unassessed')
  const note = curated.note ?? (entry.internal ? 'MySQL implementation detail; public SQL parity does not require this helper.' : arities.length ? 'Binder dispatch found; validate overloads, semantics, errors, metadata and execution paths.' : 'No binder name match. Check parser rewrites, dedicated syntax, wire handling and optional modules before declaring a gap.')
  return { ...entry, status, scope: curated.scope ?? (entry.internal ? 'internal' : 'triage'), priority: curated.priority ?? (entry.internal ? 'excluded' : 'P2'),
    pintailEvidence: curated.evidence ?? (location ? [location] : []), binderGuards: arities,
    testOccurrences: occurrences(entry.name), verification: 'not-run', note,
    acceptance: curated.acceptance ?? review.acceptanceByCategory[entry.category] ?? review.defaultAcceptance }
})
const allowed = new Set(['implemented-unverified', 'partial', 'gap', 'unassessed', 'out-of-scope'])
for (const entry of [...entries, ...features]) {
  if (!allowed.has(entry.status)) throw new Error(`Invalid status: ${entry.id}`)
  if (!entry.id || !entry.note || !entry.acceptance) throw new Error(`Incomplete review: ${entry.id}`)
}
if (new Set(features.map((f) => f.id)).size !== features.length) throw new Error('Duplicate feature ID')
for (const entry of [...entries, ...features]) {
  for (const evidence of 'pintailEvidence' in entry ? entry.pintailEvidence : entry.evidence) {
    const path = evidence.split(':')[0]
    if (!existsSync(join(root, path))) throw new Error(`Missing evidence ${evidence}`)
  }
  if (mysql) for (const ref of entry.source ?? []) {
    if (!existsSync(join(mysql, ref.path))) throw new Error(`Missing upstream source ${ref.path}`)
  }
}
const tally = (items: { status: string }[]) => Object.fromEntries([...new Set(items.map((e) => e.status))].sort().map((s) => [s, items.filter((e) => e.status === s).length]))
const ledger = { schemaVersion: 1, baseline: upstream.baseline, pintailImplementation: implementation,
  verification: 'Static source/document review; no new MySQL differential run',
  counts: { entries: entries.length, functions: entries.filter((e) => e.kind === 'function').length, operators: entries.filter((e) => e.kind === 'operator').length, features: features.length, functionStatus: tally(entries.filter((e) => e.kind === 'function')), featureStatus: tally(features) },
  entries, features }
const escape = (s: string) => s.replace(/\|/g, '&#124;').replace(/\n/g, ' ')
const localLink = (ref: string) => {
  const [path, line] = ref.split(':')
  return `[${escape(ref)}](../../${path}${line ? '#L' + line : ''})`
}
const upstreamLink = (ref: SourceRef) => `[${ref.path}:${ref.line}](https://github.com/mysql/mysql-server/blob/${upstream.baseline.commit}/${ref.path}#L${ref.line})`
const functionMarkdown = ['# MySQL function and operator comparison ledger', '',
  'Generated by `bun run scripts/mysql-source-ledger.ts`. Policy and verification contract: [README](README.md). Machine-readable details and binder guards: [ledger.json](ledger.json).', '',
  `MySQL source: **${upstream.baseline.sourceVersion}**, branch **${upstream.baseline.branch}**, commit [${upstream.baseline.commit.slice(0, 12)}](https://github.com/mysql/mysql-server/tree/${upstream.baseline.commit}). This is a development-branch snapshot, not a tagged release.`, '',
  `${ledger.counts.functions} distinct callable names; ${ledger.counts.operators} operator/construct rows. Aliases count as separate names; overloads are not separate rows. Source-only entries and internal helpers remain visible. No row is certified by this static audit.`, '',
  '| Status | Callable names |', '|---|---:|', ...Object.entries(ledger.counts.functionStatus).map(([s, n]) => `| ${s} | ${n} |`), '',
  ...['function', 'operator'].flatMap((kind) => [`## ${kind === 'function' ? 'Functions' : 'Operators and special constructs'}`, '',
    '| Name | Family | Status / priority | MySQL evidence | Pintail evidence | Review / next check |', '|---|---|---|---|---|---|',
    ...entries.filter((e) => e.kind === kind).map((e) => `| ${escape(e.name)} | ${e.category} | ${e.status} / ${e.priority} | ${[...e.source.map(upstreamLink), ...e.manual.map((url) => `[manual](${url})`)].join('; ') || 'Unresolved'} | ${e.pintailEvidence.map(localLink).join('; ') || 'Not located'} | ${escape(e.note)} |`), '']),
].join('\n')
const featureMarkdown = ['# MySQL feature comparison ledger', '',
  'Generated from [features.json](features.json). Scope and status meanings: [README](README.md). MySQL source links use the inventory commit. Entries describe external contracts, not a requirement to reproduce MySQL internals.', '',
  ...[...new Set(features.map((f) => f.category))].flatMap((category) => [`## ${category}`, '',
    '| ID / feature | Scope | Status / priority | MySQL source | Pintail evidence | Finding | Acceptance requirement |', '|---|---|---|---|---|---|---|',
    ...features.filter((f) => f.category === category).map((f) => `| ${f.id}: ${escape(f.name)} | ${f.scope} | ${f.status} / ${f.priority} | ${f.source.map(upstreamLink).join('; ')} | ${f.evidence.map(localLink).join('; ')} | ${escape(f.note)} | ${escape(f.acceptance)} |`), '']),
].join('\n')
for (const [name, content] of [['ledger.json', json(ledger)], ['functions.md', functionMarkdown], ['features.md', featureMarkdown]]) {
  const path = join(dir, name)
  if (check) {
    if (!existsSync(path) || read(path) !== content) throw new Error(`Stale ${relative(root, path)}; regenerate the ledger`)
  } else writeFileSync(path, content)
}
console.log(JSON.stringify(ledger.counts))
console.log(check ? 'Ledger consistency: PASS' : 'Ledger generated; semantic parity remains unverified')

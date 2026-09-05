import { createHash } from 'node:crypto'

export const sha256 = (text: string) => createHash('sha256').update(text).digest('hex')
export interface CoverageQuery { name: string; sql: string; documentedGap?: string }

// Deliberately conservative: quoted identifiers, literals and comments cannot
// establish function coverage. Bare keyword forms without parentheses remain
// unclassified. This records exercised names, not overload completeness.
export function functionCalls(sql: string): string[] {
  const code = sql.replace(/'(?:\\.|''|[^'\\])*'|"(?:\\.|""|[^"\\])*"|`(?:``|[^`])*`|\/\*[\s\S]*?\*\/|--[^\n]*|#[^\n]*/g, ' ')
  const tokens = code.match(/[A-Za-z_][A-Za-z_0-9]*|\S/g) ?? []
  const calls = new Set<string>()
  const stack: { name: string; typeArgument: boolean }[] = []
  for (let i = 0; i < tokens.length; i++) {
    const token = tokens[i].toUpperCase()
    const parent = stack.at(-1)
    if (token === 'AS' && parent?.name === 'CAST') parent.typeArgument = true
    if (token === ',' && parent?.name === 'CONVERT') parent.typeArgument = true
    if (token === '(') {
      const name = /^[A-Za-z_][A-Za-z_0-9]*$/.test(tokens[i - 1] ?? '') ? tokens[i - 1].toUpperCase() : ''
      if (name && !parent?.typeArgument) calls.add(name)
      stack.push({ name, typeArgument: parent?.typeArgument ?? false })
    } else if (token === ')') stack.pop()
  }
  return [...calls].sort()
}

export function differentialCases(ledger: string, queries: CoverageQuery[]) {
  const phases = new Map<string, Set<string>>()
  const failures = new Set<string>()
  for (const line of ledger.split('\n')) {
    const row = /^\| ([^|]+) \| query:([^|]+) \| (PASS|FAIL|WARN|SKIP) \|/.exec(line)
    if (!row) continue
    const [, phase, name, status] = row
    if (status === 'FAIL' || status === 'WARN') failures.add(name)
    if (status === 'PASS') {
      const found = phases.get(name) ?? new Set<string>()
      found.add(phase)
      phases.set(name, found)
    }
  }
  if (new Set(queries.map((q) => q.name)).size !== queries.length) throw new Error('Duplicate differential case name')
  return queries.filter((q) => !q.documentedGap && !failures.has(q.name) && phases.has(q.name))
    .map((q) => ({ name: q.name, sqlSha256: sha256(q.sql), functions: functionCalls(q.sql), phases: [...phases.get(q.name)!].sort() }))
    .sort((a, b) => a.name.localeCompare(b.name))
}

export interface EvidenceRun {
  schemaVersion: number; commit: string; measuredAt: string; source: string
  ledgerSha256: string; corpusSha256: string
  cases: ReturnType<typeof differentialCases>
}

export interface OracleEvidence extends EvidenceRun { sourceCommit: string }
export interface DifferentialEvidence extends EvidenceRun { oracle?: OracleEvidence }

export function oracleEvidenceFromLedger(ledger: string, corpus: string, commit: string): OracleEvidence {
  const run = JSON.parse(ledger)
  if (run.schemaVersion !== 1 || run.verdict !== 'PASS' || run.cleanTree !== true) throw new Error('Oracle evidence requires a clean PASS run')
  if (!/^[a-f0-9]{40}$/.test(commit) || !/^[a-f0-9]{40}$/.test(run.commit) ||
      typeof run.measuredAt !== 'string' || !Number.isFinite(Date.parse(run.measuredAt)) ||
      typeof run.source !== 'string' || !run.source.startsWith('MySQL ')) throw new Error('Invalid oracle provenance')
  if (run.corpusSha256 !== sha256(corpus)) throw new Error('Stale oracle corpus')
  const expected = Number(/const EXPECTED_CASES: usize = (\d+);/.exec(corpus)?.[1])
  if (!expected || run.expectedCases !== expected || !Array.isArray(run.cases) || run.cases.length !== expected) throw new Error('Oracle evidence is not a complete fixed corpus')
  const names = new Set<string>()
  const cases = run.cases.map((row: { name: string; sql: string; ordered: boolean; status: string }) => {
    if (typeof row.name !== 'string' || !row.name || typeof row.sql !== 'string' || !row.sql || typeof row.ordered !== 'boolean' || row.status !== 'PASS') throw new Error('Oracle case lacks SQL or PASS result')
    if (names.has(row.name)) throw new Error('Duplicate oracle case name')
    names.add(row.name)
    return { name: `oracle:${row.name}`, sqlSha256: sha256(row.sql), functions: functionCalls(row.sql), phases: ['fixed-oracle'] }
  }).sort((a: { name: string }, b: { name: string }) => a.name.localeCompare(b.name))
  return {
    schemaVersion: 1, commit, sourceCommit: run.commit, measuredAt: run.measuredAt, source: run.source,
    ledgerSha256: sha256(ledger), corpusSha256: sha256(corpus), cases,
  }
}

export function validateOracleEvidence(evidence: OracleEvidence, ledger: string, corpus: string) {
  if (JSON.stringify(evidence) !== JSON.stringify(oracleEvidenceFromLedger(ledger, corpus, evidence.commit))) throw new Error('Oracle evidence does not match the recorded run')
}

export function validateEvidence(evidence: DifferentialEvidence, ledger: string, corpus: string, queries: CoverageQuery[]) {
  if (evidence.measuredAt !== /^Measured (.+)\.$/m.exec(ledger)?.[1] || evidence.source !== /^Source: (.+)$/m.exec(ledger)?.[1]) throw new Error('Differential provenance does not match the ledger')
  if (evidence.schemaVersion !== 1 || !/^[a-f0-9]{40}$/.test(evidence.commit)) throw new Error('Invalid evidence provenance')
  if (evidence.ledgerSha256 !== sha256(ledger) || evidence.corpusSha256 !== sha256(corpus)) throw new Error('Stale differential evidence; refresh from a banked run')
  if (JSON.stringify(evidence.cases) !== JSON.stringify(differentialCases(ledger, queries))) throw new Error('Differential evidence does not match PASS records and SQL')
  if (!evidence.cases.length) throw new Error('Differential evidence contains no passing cases')
}

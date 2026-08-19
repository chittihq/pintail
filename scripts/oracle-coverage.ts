#!/usr/bin/env bun
/// Inventory the MySQL differential oracle and related SQL corpora.
///
/// Case *count* is a weak diversity signal: the generator loops the same AST
/// template many times. This script reports families, rough template entropy,
/// and which binder functions never appear in the oracle SQL surface.
///
/// ```text
/// bun run scripts/oracle-coverage.ts
/// bun run scripts/oracle-coverage.ts --json
/// ```

import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const repository = join(import.meta.dir, '..')
const asJson = process.argv.includes('--json')

function surface(): Map<string, Set<string>> {
  // The binder split into a module; the scalar-function surface lives
  // across its files now.
  const source = ['mod.rs', 'function.rs']
    .map((name) =>
      readFileSync(join(repository, 'crates/pintail-sql/src/binder', name), 'utf8'),
    )
    .join('\n')
  const found = new Map<string, Set<string>>()
  const arm = /^\s*((?:"[A-Z0-9_]+"\s*\|\s*)*"[A-Z0-9_]+")\s*(if\s+.+?)?\s*=>/gm
  for (const match of source.matchAll(arm)) {
    const names = [...match[1].matchAll(/"([A-Z0-9_]+)"/g)].map((m) => m[1])
    const guard = (match[2] ?? '').replace(/\s+/g, ' ').trim()
    for (const name of names) {
      if (!found.has(name)) found.set(name, new Set())
      found.get(name)!.add(guard || 'any arity')
    }
  }
  for (const match of source.matchAll(/function_name == "([A-Z0-9_]+)"/g)) {
    if (!found.has(match[1])) found.set(match[1], new Set(['keyword argument']))
  }
  return found
}

const SYNTAX_FORMS = new Set([
  'CAST',
  'CONVERT',
  'EXTRACT',
  'SUBSTRING',
  'TRIM',
  'POSITION',
  'COUNT',
  'SUM',
  'AVG',
  'MIN',
  'MAX',
  'GROUP_CONCAT',
  'JSON_ARRAYAGG',
  'JSON_OBJECTAGG',
  'ROW_NUMBER',
  'RANK',
  'DENSE_RANK',
  'LAG',
  'LEAD',
  'NTILE',
  'FIRST_VALUE',
  'LAST_VALUE',
  'OVER',
  'IF',
])

const NOT_CALLS = new Set([
  'IF',
  'IN',
  'VALUES',
  'AND',
  'OR',
  'NOT',
  'ON',
  'USING',
  'WHEN',
  'THEN',
  'ELSE',
  'SELECT',
  'FROM',
  'WHERE',
  'BY',
  'AS',
  'UNION',
  'INTERSECT',
  'EXCEPT',
  'EXISTS',
  'ALL',
  'ANY',
  'WITH',
  'PARTITION',
  'ORDER',
  'GROUP',
  'HAVING',
  'LIMIT',
  'OFFSET',
  'DECIMAL',
  'CHAR',
  'BINARY',
  'SIGNED',
  'UNSIGNED',
  'INTERVAL',
  'CASE',
  'END',
  'LEFT',
  'RIGHT',
  'INNER',
  'OUTER',
  'CROSS',
  'JOIN',
  'RECURSIVE',
  'DISTINCT',
  'TRUE',
  'FALSE',
  'NULL',
  'WINDOW',
])

function calls(sql: string): string[] {
  const stripped = sql
    .replace(/--[^\n]*/g, ' ')
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/'(?:[^'\\]|\\.|'')*'/g, "''")
  return [...stripped.matchAll(/\b([A-Za-z_][A-Za-z0-9_]*)\s*\(/g)].map((m) =>
    m[1].toUpperCase(),
  )
}

function templateKey(sql: string): string {
  return sql
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/'(?:[^'\\]|\\.|'')*'/g, "'?'")
    .replace(/\b\d+(\.\d+)?\b/g, 'N')
    .replace(/\{[^}]+\}/g, '{p}')
}

function unquoteRustStringLiteral(lit: string): string {
  // Concatenated "..." "..." pieces. Rust line continuations are `\` + newline
  // inside a single literal, so escape matching must allow newlines (`.` does not).
  const parts = [...lit.matchAll(/"((?:\\[\s\S]|[^"\\])*)"/g)].map((m) =>
    m[1]
      .replace(/\\\n/g, '')
      .replace(/\\n/g, '\n')
      .replace(/\\t/g, '\t')
      .replace(/\\"/g, '"')
      .replace(/\\\\/g, '\\'),
  )
  return parts.join('')
}

function extractOracle(source: string): {
  expected: number | null
  families: Map<string, number>
  sqlSnippets: string[]
  generatedCaseCount: number
} {
  const expectedMatch = source.match(/EXPECTED_CASES:\s*usize\s*=\s*(\d+)/)
  const expected = expectedMatch ? Number(expectedMatch[1]) : null
  const families = new Map<string, number>()
  const sqlSnippets: string[] = []

  const loopCounts = [...source.matchAll(/for value in 0\.\.(\d+)/g)].map((m) =>
    Number(m[1]),
  )
  const generatedCaseCount = loopCounts.reduce((a, b) => a + b, 0)

  // Each parametric loop block: family name once, SQL format string once, N cases.
  for (const block of source.matchAll(
    /for value in 0\.\.(\d+)\s*\{([\s\S]*?)\n    \}/g,
  )) {
    const n = Number(block[1])
    const body = block[2]
    const family = body.match(/family:\s*"([^"]+)"/)?.[1]
    if (family) families.set(family, (families.get(family) ?? 0) + n)
    const fmt = body.match(
      /format!\s*\(\s*((?:"(?:\\[\s\S]|[^"\\])*"\s*)+)/,
    )
    if (fmt) sqlSnippets.push(unquoteRustStringLiteral(fmt[1]))
    else {
      for (const f of body.matchAll(
        /format!\s*\(\s*((?:"(?:\\[\s\S]|[^"\\])*"\s*)+)/g,
      )) {
        sqlSnippets.push(unquoteRustStringLiteral(f[1]))
      }
    }
  }

  // ordered("family", "sql" ...) including multi-line Rust string continuations.
  for (const match of source.matchAll(
    /(?:ordered|unordered)\(\s*"([^"]+)"\s*,\s*((?:"(?:\\[\s\S]|[^"\\])*"\s*)+)/g,
  )) {
    const family = match[1]
    families.set(family, (families.get(family) ?? 0) + 1)
    sqlSnippets.push(unquoteRustStringLiteral(match[2]))
  }

  return { expected, families, sqlSnippets, generatedCaseCount }
}

function extractE2e(source: string): { names: string[]; sql: string[] } {
  const names = [...source.matchAll(/name:\s*'([^']+)'/g)].map((m) => m[1])
  const sql: string[] = []
  // sql: '...' or multi-line concatenation
  for (const match of source.matchAll(
    /sql:\s*\n?\s*((?:'[^']*'\s*(?:\+\s*)?)+)/g,
  )) {
    const piece = match[1]
      .replace(/'([^']*)'/g, '$1')
      .replace(/\+/g, '')
      .replace(/\s+/g, ' ')
      .trim()
    if (piece.length > 8) sql.push(piece)
  }
  return { names, sql }
}

const oraclePath = join(
  repository,
  'tests/sqllogic/tests/mysql_oracle.rs',
)
const e2ePath = join(repository, 'tests/e2e/queries.ts')
const biPath = join(repository, 'tests/corpus/bi-shapes.sql')

const oracleSrc = readFileSync(oraclePath, 'utf8')
const e2eSrc = readFileSync(e2ePath, 'utf8')
const biSrc = readFileSync(biPath, 'utf8')

const oracle = extractOracle(oracleSrc)
const e2e = extractE2e(e2eSrc)
const supported = surface()

const oracleOnlyFreq = new Map<string, number>()
for (const sql of oracle.sqlSnippets) {
  for (const name of calls(sql)) {
    if (NOT_CALLS.has(name)) continue
    oracleOnlyFreq.set(name, (oracleOnlyFreq.get(name) ?? 0) + 1)
  }
}

const templates = new Map<string, number>()
for (const sql of oracle.sqlSnippets) {
  const key = templateKey(sql)
  templates.set(key, (templates.get(key) ?? 0) + 1)
}

const familyTotal = [...oracle.families.values()].reduce((a, b) => a + b, 0)
const handWrittenApprox = familyTotal - oracle.generatedCaseCount

const uncovered = [...supported.keys()]
  .filter((name) => !oracleOnlyFreq.has(name) && !SYNTAX_FORMS.has(name))
  .sort()

const diversifyCases = [...oracle.families.entries()]
  .filter(([k]) => k.startsWith('diversify'))
  .reduce((a, [, n]) => a + n, 0)

const report = {
  expectedCases: oracle.expected,
  inventoriedCases: familyTotal,
  generatedParametricCases: oracle.generatedCaseCount,
  handWrittenAndNamedCases: handWrittenApprox,
  diversifyTypedCases: diversifyCases,
  uniqueSqlTemplatesApprox: templates.size,
  familyCount: oracle.families.size,
  families: Object.fromEntries(
    [...oracle.families.entries()].sort(
      (a, b) => b[1] - a[1] || a[0].localeCompare(b[0]),
    ),
  ),
  e2eDifferentialShapes: e2e.names.length,
  biShapeStatements: (biSrc.match(/^\s*SELECT\b/gim) ?? []).length,
  binderCallableNames: supported.size,
  oracleFunctionNamesTouched: oracleOnlyFreq.size,
  binderFunctionsNeverInOracle: uncovered,
  topOracleFunctions: [...oracleOnlyFreq.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .slice(0, 25),
  diversityNote:
    'Parametric loops share one AST template each; prefer uniqueSqlTemplatesApprox and typed-column diversify cases over raw EXPECTED_CASES when judging coverage.',
}

if (asJson) {
  console.log(JSON.stringify(report, null, 2))
  process.exit(0)
}

console.log('MySQL compatibility corpus coverage')
console.log('===================================')
console.log(`oracle EXPECTED_CASES:       ${report.expectedCases ?? 'n/a'}`)
console.log(`oracle inventoried:          ${report.inventoriedCases}`)
console.log(`  parametric (loops):        ${report.generatedParametricCases}`)
console.log(`  hand-written / named:      ${report.handWrittenAndNamedCases}`)
console.log(`  diversify (typed tables):  ${report.diversifyTypedCases}`)
console.log(
  `  unique templates (approx): ${report.uniqueSqlTemplatesApprox}`,
)
console.log(`  families:                  ${report.familyCount}`)
console.log(`e2e differential shapes:     ${report.e2eDifferentialShapes}`)
console.log(`bi-shapes SELECT stmts:      ${report.biShapeStatements}`)
console.log(`binder callable names:       ${report.binderCallableNames}`)
console.log(
  `functions seen in oracle:    ${report.oracleFunctionNamesTouched}`,
)
console.log('')
console.log('Top families by case count:')
for (const [family, n] of Object.entries(report.families).slice(0, 25)) {
  console.log(`  ${String(n).padStart(4)}  ${family}`)
}
console.log('')
console.log(
  `Binder functions never appearing in oracle SQL (${uncovered.length}):`,
)
if (uncovered.length === 0) {
  console.log('  (none)')
} else {
  const show = uncovered.slice(0, 40)
  for (const name of show) console.log(`  ${name}`)
  if (uncovered.length > show.length) {
    console.log(`  … and ${uncovered.length - show.length} more`)
  }
}
console.log('')
console.log(report.diversityNote)
console.log('')
console.log('Diversify guidance: grow typed multi-table cases and e2e shapes;')
console.log('promote bi-captured mismatches; do not inflate for-value loops.')

import { expect, test } from 'bun:test'
import { differentialCases, functionCalls, sha256, validateEvidence, oracleEvidenceFromLedger } from './differential-coverage.ts'

test('literals, identifiers and comments cannot establish coverage', () => {
  expect(functionCalls("SELECT SUM(x), 'ABS(x)', \"MAX(x)\", `MIN(x)` /* AVG(x) */ -- COUNT(x)\n# POW(x)\n, COALESCE(x, 0)")).toEqual(['COALESCE', 'SUM'])
})
test('only real PASS rows with matching non-gap cases establish evidence', () => {
  const queries = [{ name: 'ok', sql: 'SELECT SUM(x)' }, { name: 'bad', sql: 'SELECT ABS(x)' }, { name: 'gap', sql: 'SELECT MAX(x)', documentedGap: 'gap' }]
  const ledger = 'Measured date.\nSource: mysql\n| snapshot | query:ok | PASS | |\n| cdc | query:ok | SKIP | |\n| snapshot | query:bad | PASS | |\n| cdc | query:bad | WARN | |\n| snapshot | query:gap | PASS | |\nFAKE query:bad PASS'
  expect(differentialCases(ledger, queries)).toEqual([{ name: 'ok', sqlSha256: sha256(queries[0].sql), functions: ['SUM'], phases: ['snapshot'] }])
  const evidence = { schemaVersion: 1, commit: 'a'.repeat(40), measuredAt: 'date', source: 'mysql', ledgerSha256: sha256(ledger), corpusSha256: sha256('corpus'), cases: differentialCases(ledger, queries) }
  expect(() => validateEvidence(evidence, ledger, 'corpus', queries)).not.toThrow()
  expect(() => validateEvidence({ ...evidence, source: 'forged' }, ledger, 'corpus', queries)).toThrow('provenance')
  expect(() => validateEvidence(evidence, ledger, 'changed', queries)).toThrow('Stale')
  expect(() => validateEvidence({ ...evidence, cases: [] }, ledger, 'corpus', queries)).toThrow('does not match')
})

test('CAST and CONVERT type parameters do not count as function calls', () => {
  expect(functionCalls("SELECT CAST(customer_id AS CHAR(32)), CONVERT(n, DECIMAL(12, 2)), CHAR(65), CAST(ABS(n) AS SIGNED)")).toEqual(['ABS', 'CAST', 'CHAR', 'CONVERT'])
  expect(functionCalls("SELECT CAST(customer_id AS CHAR(32))")).toEqual(['CAST'])
})


test('fixed oracle evidence includes executed SQL and rejects incomplete or dirty runs', () => {
  const corpus = 'const EXPECTED_CASES: usize = 2;'
  const run = {
    schemaVersion: 1, verdict: 'PASS', cleanTree: true,
    commit: 'b'.repeat(40), measuredAt: '2026-09-06T00:00:00Z', source: 'MySQL 8.4.11',
    corpusSha256: sha256(corpus), expectedCases: 2,
    cases: [
      { name: '0000:numeric', sql: 'SELECT ABS(-1), FLOOR(1.5)', ordered: false, status: 'PASS' },
      { name: '0001:json', sql: "SELECT JSON_SET('{}', '$.n', 1)", ordered: true, status: 'PASS' },
    ],
  }
  const parse = (value: unknown) => oracleEvidenceFromLedger(JSON.stringify(value), corpus, 'a'.repeat(40))
  expect(parse(run).cases.flatMap((c) => c.functions)).toEqual(['ABS', 'FLOOR', 'JSON_SET'])
  expect(parse(run).sourceCommit).toBe(run.commit)
  expect(() => parse({ ...run, cases: run.cases.slice(1) })).toThrow('complete')
  expect(() => parse({ ...run, cleanTree: false })).toThrow('clean')
  expect(() => parse({ ...run, corpusSha256: 'stale' })).toThrow('corpus')
  expect(() => parse({ ...run, cases: [run.cases[0], run.cases[0]] })).toThrow('Duplicate')
  expect(() => parse({ ...run, cases: [run.cases[0], { ...run.cases[1], status: 'FAIL' }] })).toThrow('PASS')
})

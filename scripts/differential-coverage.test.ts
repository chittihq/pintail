import { expect, test } from 'bun:test'
import { differentialCases, functionCalls, sha256, validateEvidence } from './differential-coverage.ts'

test('literals, identifiers and comments cannot establish coverage', () => {
  expect(functionCalls("SELECT SUM(x), 'ABS(x)', \"MAX(x)\", `MIN(x)` /* AVG(x) */ -- COUNT(x)\n# POW(x)\n, COALESCE(x, 0)")).toEqual(['COALESCE', 'SUM'])
})
test('only real PASS rows with matching non-gap cases establish evidence', () => {
  const queries = [{ name: 'ok', sql: 'SELECT SUM(x)' }, { name: 'bad', sql: 'SELECT ABS(x)' }, { name: 'gap', sql: 'SELECT MAX(x)', documentedGap: 'gap' }]
  const ledger = '| snapshot | query:ok | PASS | |\n| cdc | query:ok | SKIP | |\n| snapshot | query:bad | PASS | |\n| cdc | query:bad | WARN | |\n| snapshot | query:gap | PASS | |\nFAKE query:bad PASS'
  expect(differentialCases(ledger, queries)).toEqual([{ name: 'ok', sqlSha256: sha256(queries[0].sql), functions: ['SUM'], phases: ['snapshot'] }])
  const evidence = { schemaVersion: 1, commit: 'a'.repeat(40), measuredAt: 'date', source: 'mysql', ledgerSha256: sha256(ledger), corpusSha256: sha256('corpus'), cases: differentialCases(ledger, queries) }
  expect(() => validateEvidence(evidence, ledger, 'corpus', queries)).not.toThrow()
  expect(() => validateEvidence(evidence, ledger, 'changed', queries)).toThrow('Stale')
  expect(() => validateEvidence({ ...evidence, cases: [] }, ledger, 'corpus', queries)).toThrow('does not match')
})

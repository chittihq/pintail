#!/usr/bin/env bun
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { resolve } from 'node:path'
import { differentialQueries } from '../tests/e2e/queries.ts'
import { differentialCases, sha256, validateEvidence, oracleEvidenceFromLedger, type DifferentialEvidence } from './differential-coverage.ts'

const root = resolve(import.meta.dir, '..')
const git = (...args: string[]) => execFileSync('git', ['-C', root, ...args], { encoding: 'utf8' })
const ledgerPath = 'tests/e2e/results.md'
const corpusPath = 'tests/e2e/queries.ts'
const commit = git('log', '-1', '--format=%H', '--', ledgerPath).trim()
const ledger = readFileSync(resolve(root, ledgerPath), 'utf8')
const corpus = readFileSync(resolve(root, corpusPath), 'utf8')
for (const [path, text] of [[ledgerPath, ledger], [corpusPath, corpus]]) {
  if (git('show', `${commit}:${path}`) !== text) throw new Error(`${path} differs from the bank commit; bank a matching differential run first`)
}
const evidence: DifferentialEvidence = {
  schemaVersion: 1, commit,
  measuredAt: /^Measured (.+)\.$/m.exec(ledger)?.[1] ?? '',
  source: /^Source: (.+)$/m.exec(ledger)?.[1] ?? '',
  ledgerSha256: sha256(ledger), corpusSha256: sha256(corpus),
  cases: differentialCases(ledger, differentialQueries),
}
if (!evidence.measuredAt || !evidence.source) throw new Error('Missing run provenance')
validateEvidence(evidence, ledger, corpus, differentialQueries)
const oraclePath = 'tests/sqllogic/results-oracle.json'
const oracleCorpusPath = 'tests/sqllogic/tests/mysql_oracle.rs'
if (existsSync(resolve(root, oraclePath))) {
  const oracleCommit = git('log', '-1', '--format=%H', '--', oraclePath).trim()
  const oracleLedger = readFileSync(resolve(root, oraclePath), 'utf8')
  const oracleCorpus = readFileSync(resolve(root, oracleCorpusPath), 'utf8')
  for (const [path, text] of [[oraclePath, oracleLedger], [oracleCorpusPath, oracleCorpus]]) {
    if (git('show', `${oracleCommit}:${path}`) !== text) throw new Error(`${path} differs from the oracle bank commit`)
  }
  evidence.oracle = oracleEvidenceFromLedger(oracleLedger, oracleCorpus, oracleCommit)
  if (git('show', `${evidence.oracle.sourceCommit}:${oracleCorpusPath}`) !== oracleCorpus) throw new Error('Oracle source differs from the measured commit')
  console.log(`Linked ${evidence.oracle.cases.length} fixed oracle cases at ${oracleCommit}`)
}
writeFileSync(resolve(root, 'docs/mysql-parity/differential-evidence.json'), JSON.stringify(evidence, null, 2) + '\n')
console.log(`Linked ${evidence.cases.length} passing cases at ${commit}`)

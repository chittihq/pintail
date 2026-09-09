#!/usr/bin/env bun
// Propose synthetic SQL offline; the Rust candidate gate decides what is valid.
import { readFileSync, mkdirSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { resolve, dirname } from 'node:path'

const root = resolve(import.meta.dir, '..')
const model = 'nex-agi/nex-n2.5-pro:free'
const key = process.env.OPENROUTER_API_KEY ?? (process.env.OPENROUTER_API_KEY_FILE ? readFileSync(process.env.OPENROUTER_API_KEY_FILE, 'utf8').trim() : '')
if (!key) throw new Error('Set OPENROUTER_API_KEY or OPENROUTER_API_KEY_FILE')
const inventory = JSON.parse(readFileSync(resolve(root, 'validate-out/oracle-inventory.json'), 'utf8'))
for (const [file, expected] of Object.entries({ 'tests/sqllogic/tests/mysql_oracle.rs': inventory.sourceSha256, ...inventory.sourceFiles })) {
  if (createHash('sha256').update(readFileSync(resolve(root, file))).digest('hex') !== expected) throw new Error('Runtime inventory is stale; export it from the inventory unit test first')
}
const output = resolve(process.env.PINTAIL_CANDIDATES_PATH ?? resolve(root, 'validate-out/llm-candidates.json'))
const target = process.env.PINTAIL_CANDIDATE_FOCUS ?? 'unusual compositions of nullable decimal expressions, quantified subqueries, string collation and JSON paths, CTEs and window frames'
const schema = inventory.fixtureSQL.match(/CREATE TABLE[^;]+;/g)?.join('\n')
if (!schema) throw new Error('The runtime inventory must include synthetic CREATE TABLE fixtures')
const prompt = `Propose exactly 8 distinct MySQL 8.4 SELECT queries for differential testing.\nFocus: ${target}.\nSynthetic schema:\n${schema}\nThese are tiny seeded tables. bounds has 8 rows with nullable numeric extremes, positive/negative/zero decimal values, NULL and empty strings, Unicode, binary bytes, leap dates, precision 0/3/6 datetimes and signed TIME intervals.\nRequirements: only a single SELECT or nonrecursive WITH...SELECT statement per case; no writes, locks, INTO, file access, system tables, user variables, random/current-time functions, sleep, benchmarks, stored functions, DDL, or recursive CTEs. Use only the supplied tables. Keep joins to two tables and each SQL below 2000 characters. Do not use LIMIT. ROWS windows need a unique primary-key ordering or a full singleton partition; these receive a separate manual review. Do not merely change literals between cases. Prefer a minimal projection that isolates one semantic question. All multirow results must have deterministic total ORDER BY; do not add a window ordering key that changes its peers. Avoid nondeterministic GROUP_CONCAT/JSON aggregates. Each case must be valid MySQL 8.4 under ONLY_FULL_GROUP_BY. No expected results: the database supplies those. Return JSON only: {"cases":[{"family":"short category","sql":"query without trailing semicolon","rationale":"what boundary this probes"}]}.`
const response = await fetch('https://openrouter.ai/api/v1/chat/completions', {
  method: 'POST', headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
  body: JSON.stringify({ model, provider: { allow_fallbacks: false }, max_tokens: 6000, reasoning: { enabled: false }, temperature: 0.7,
    response_format: { type: 'json_object' }, messages: [{ role: 'user', content: prompt }] }),
  signal: AbortSignal.timeout(300_000),
})
if (!response.ok) throw new Error(`OpenRouter returned HTTP ${response.status}; no candidates accepted`)
const result = await response.json() as any
if (result.error) throw new Error(`OpenRouter model error: ${result.error.code ?? 'unknown'}`)
mkdirSync(dirname(output), { recursive: true })
writeFileSync(output + '.response.json', JSON.stringify(result, null, 2) + '\n')
const content = result.choices?.[0]?.message?.content
if (typeof content !== 'string') throw new Error('Model returned no text candidates')
const decoded = JSON.parse(content.replace(/^```(?:json)?\s*/, '').replace(/\s*```$/, ''))
if (!Array.isArray(decoded.cases)) throw new Error('Model response lacks a cases array')
const known = new Set(inventory.cases.map((c: any) => c.sql.trim().replace(/;$/, '')))
const cases: { family: string; sql: string; rationale: string }[] = []
for (const c of decoded.cases.slice(0, 40)) {
  if (typeof c.sql !== 'string' || c.sql.length > 4096 || typeof c.family !== 'string' || typeof c.rationale !== 'string') continue
  const sql = c.sql.trim().replace(/;$/, '')
  if (known.has(sql)) continue
  known.add(sql)
  cases.push({ family: c.family, sql, rationale: c.rationale })
}
mkdirSync(dirname(output), { recursive: true })
writeFileSync(output, JSON.stringify({ schemaVersion: 1, model, returnedModel: result.model, generatedAt: new Date().toISOString(),
  prompt, promptSha256: createHash('sha256').update(prompt).digest('hex'), fixtureSha256: inventory.fixtureSha256,
  usage: result.usage, finishReason: result.choices?.[0]?.finish_reason, cases }, null, 2) + '\n')
console.log(`CANDIDATES-DONE: ${cases.length} proposals saved for validation using ${model}`)

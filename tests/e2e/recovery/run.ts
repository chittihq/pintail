import { createReadStream, mkdirSync, writeFileSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { Source, repository, runScenario, type Check, type Scenario } from './harness'
import { selected, ledgerDetail } from './policy'
import { command } from '../lib'
import { baseline } from './scenarios/baseline'

import { modeScenarios } from './scenarios/mode'
import { cdcScenarios } from './scenarios/cdc'
import { purgeScenarios } from './scenarios/purge'
import { schemaScenarios } from './scenarios/schema'
import { pollScenarios } from './scenarios/poll'
import { boundaryScenarios } from './scenarios/boundaries'
import { outageScenarios } from './scenarios/outage'

const scenarios: Scenario[] = [baseline, ...modeScenarios, ...cdcScenarios, ...purgeScenarios, ...schemaScenarios, ...pollScenarios, ...boundaryScenarios, ...outageScenarios]
const arg = process.argv.find(arg => arg.startsWith('--only='))?.slice(7)
  ?? (process.argv.includes('--only') ? process.argv[process.argv.indexOf('--only') + 1] : '')
if ((process.argv.includes('--only') || process.argv.some(a=>a.startsWith('--only='))) && !arg?.trim()) throw new Error('--only requires a scenario pattern')
const patterns = arg?.split(',').map(p=>p.trim()).filter(Boolean) ?? []
const requested = scenarios.filter(s => selected(s.slug, patterns))
if (!requested.length) throw new Error('no recovery scenarios match --only')
if (process.argv.includes('--list')) { console.log(requested.map(s => `${s.slug}\t${s.promise}`).join('\n')); process.exit(0) }
const runDir = join(repository, 'validate-out/recovery', new Date().toISOString().replaceAll(':', '-'))
mkdirSync(runDir, { recursive: true })
const source = new Source()
const checks: Check[] = []
const started = Date.now()
const binary = process.env.PINTAIL_RECOVERY_BINARY ?? join(repository, 'target/debug/pintail')
let completed = 0
const head = (await command(['git', 'rev-parse', 'HEAD'], { quiet: true })).stdout
const dirty = !!(await command(['git', 'status', '--porcelain'], { quiet: true })).stdout.trim()
const rust = (await command([join(homedir(), '.cargo/bin/rustc'), '--version'], { quiet: true })).stdout
let sourceVersion = 'unavailable'
let binaryHash = 'unavailable'
try {
  if (process.env.PINTAIL_RECOVERY_BINARY && !patterns.length) throw new Error('A full recovery ledger requires a checkout build; PINTAIL_RECOVERY_BINARY is only allowed with --only')
  if (!process.env.PINTAIL_RECOVERY_BINARY) {
    const build = Bun.spawn([join(homedir(), '.cargo/bin/cargo'), 'build', '-p', 'pintail', '--features', 'failpoints'], {
      cwd: repository, env: { ...process.env, CARGO_TARGET_DIR: join(repository, 'target') }, stdout: 'inherit', stderr: 'inherit',
    })
    if (await build.exited !== 0) throw new Error('recovery binary build failed')
  }
  const hash = createHash('sha256')
  for await (const chunk of createReadStream(binary)) hash.update(chunk)
  binaryHash = hash.digest('hex')
  await source.start()
  const [version] = await source.root.query({sql:'SELECT VERSION() AS version',timeout:15_000})
  sourceVersion = (version as Array<{version:string}>)[0].version
  for (const scenario of requested) {
    console.log(`recovery: ${scenario.slug} starting`)
    const results = await runScenario(source, binary, scenario, runDir)
    checks.push({ scenario: scenario.slug, area: scenario.area, check: 'contract', status: 'PASS', detail: scenario.promise }, ...results); completed++
    const failed = results.some(r => r.status === 'FAIL')
    console.log(`recovery: ${scenario.slug} ${failed ? 'FAIL' : results.some(r=>r.status==='WARN') ? 'PASS (documented WARN)' : 'PASS'}`)
    if (failed) { for (const result of results.filter(r => r.status === 'FAIL')) console.error(source.host ? result.detail?.replaceAll(source.host, '<source>') : result.detail); break }
  }
} catch (error) {
  checks.push({ scenario: 'harness', area: 'baseline', check: 'run', status: 'FAIL', detail: String(error) })
} finally {
  await source.close().catch(error => checks.push({ scenario: 'harness', area: 'baseline', check: 'cleanup', status: 'FAIL', detail: String(error) }))
  const passed = completed === requested.length && checks.length > 0 && !checks.some(r => r.status === 'FAIL')
  const sanitize = (text: string) => (source.host ? text.replaceAll(source.host, '<source>') : text).replaceAll(source.name, '<source-container>').replaceAll('|', '\\|').replaceAll('\n', ' ')
  const ledger = [`# Recovery suite — ${new Date().toISOString()}`, '', `Verdict: **${passed ? patterns.length ? 'PASS (SUBSET)' : 'PASS' : 'FAIL'}**`, '',
    `HEAD: ${head}; ${rust}; Bun ${Bun.version}.`,
    `Working tree: ${dirty ? 'dirty' : 'clean'}. Binary: ${process.env.PINTAIL_RECOVERY_BINARY ? 'supplied for subset' : 'built from checkout'}; SHA-256: ${binaryHash}.`,
    `Source: MySQL ${sourceVersion}; ROW/FULL images; MINIMAL metadata; GTID. Seed: ${Number(process.env.PINTAIL_RECOVERY_SEED ?? 953)}.`,
    `Checks: ${checks.filter(r=>r.status==='PASS').length} PASS, ${checks.filter(r=>r.status==='WARN').length} WARN, ${checks.filter(r=>r.status==='FAIL').length} FAIL.`,
    `Scenarios: ${completed}/${requested.length} requested; ${scenarios.length} registered. Duration: ${((Date.now()-started)/60000).toFixed(1)} minutes.`, '',
    '| scenario | check | status | detail |', '|---|---|---|---|', ...checks.map(r => `| ${r.scenario} | ${r.check} | ${r.status} | ${ledgerDetail(r.status, sanitize(r.detail ?? ''))} |`), ''].join('\n')
  const filename = patterns.length ? 'results-recovery-partial.md' : 'results-recovery.md'
  writeFileSync(join(repository, 'tests/e2e', filename), ledger)
  writeFileSync(join(runDir, 'results.md'), ledger)
  writeFileSync(join(runDir, 'results.json'), JSON.stringify(checks, null, 2))
  console.log(`RECOVERY-${passed ? 'DONE' : 'FAIL'}: ${completed}/${requested.length} scenarios`)
  if (!passed) process.exitCode = 1
}

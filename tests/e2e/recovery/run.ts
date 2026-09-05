import { mkdirSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join } from 'node:path'
import { Source, repository, runScenario, type Check, type Scenario } from './harness'
import { selected } from './policy'
import { command } from '../lib'
import { baseline } from './scenarios/baseline'

import { modeScenarios } from './scenarios/mode'

const scenarios: Scenario[] = [baseline, ...modeScenarios]
const arg = process.argv.find(arg => arg.startsWith('--only='))?.slice(7)
  ?? (process.argv.includes('--only') ? process.argv[process.argv.indexOf('--only') + 1] : '')
const patterns = arg?.split(',').filter(Boolean) ?? []
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
const rust = (await command([join(homedir(), '.cargo/bin/rustc'), '--version'], { quiet: true })).stdout
let sourceVersion = 'unavailable'
try {
  if (!process.env.PINTAIL_RECOVERY_BINARY) {
    const build = Bun.spawn([join(homedir(), '.cargo/bin/cargo'), 'build', '-p', 'pintail', '--features', 'failpoints'], {
      cwd: repository, env: { ...process.env, CARGO_TARGET_DIR: join(repository, 'target') }, stdout: 'inherit', stderr: 'inherit',
    })
    if (await build.exited !== 0) throw new Error('recovery binary build failed')
  }
  await source.start()
  const [version] = await source.root.query('SELECT VERSION() AS version')
  sourceVersion = (version as Array<{version:string}>)[0].version
  for (const scenario of requested) {
    console.log(`recovery: ${scenario.slug} starting`)
    const results = await runScenario(source, binary, scenario, runDir)
    checks.push({ scenario: scenario.slug, area: scenario.area, check: 'contract', status: 'PASS', detail: scenario.promise }, ...results); completed++
    const failed = results.some(r => r.status === 'FAIL')
    console.log(`recovery: ${scenario.slug} ${failed ? 'FAIL' : 'PASS'}`)
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
    `Source: MySQL ${sourceVersion}; ROW/FULL images; MINIMAL metadata; GTID. Seed: ${process.env.PINTAIL_RECOVERY_SEED ?? 953}.`,
    `Scenarios: ${completed}/${requested.length} requested; ${scenarios.length} registered. Duration: ${((Date.now()-started)/60000).toFixed(1)} minutes.`, '',
    '| scenario | check | status | detail |', '|---|---|---|---|', ...checks.map(r => `| ${r.scenario} | ${r.check} | ${r.status} | ${sanitize(r.detail ?? '')} |`), ''].join('\n')
  const filename = patterns.length ? 'results-recovery-partial.md' : 'results-recovery.md'
  writeFileSync(join(repository, 'tests/e2e', filename), ledger)
  writeFileSync(join(runDir, 'results.md'), ledger)
  writeFileSync(join(runDir, 'results.json'), JSON.stringify(checks, null, 2))
  console.log(`RECOVERY-${passed ? 'DONE' : 'FAIL'}: ${completed}/${requested.length} scenarios`)
  if (!passed) process.exitCode = 1
}

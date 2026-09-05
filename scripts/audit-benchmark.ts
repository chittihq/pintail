#!/usr/bin/env bun
import { closeSync, openSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const root = resolve(import.meta.dir, '..')
const args = process.argv.slice(2)
let smoke = false, check = false, ref = 'HEAD', output = ''
for (let i = 0; i < args.length; i++) {
  if (args[i] === '--smoke') smoke = true
  else if (args[i] === '--check') check = true
  else if (['--ref', '--output'].includes(args[i]) && args[i + 1] && !args[i + 1].startsWith('--')) {
    if (args[i++] === '--ref') ref = args[i]
    else output = resolve(args[i])
  } else throw new Error(`Unknown or incomplete option: ${args[i]}`)
}
if (!check && !output) throw new Error('Use --output <new-directory> [--ref <commit>] [--smoke], or --check')
if (output && existsSync(output)) throw new Error('Output directory must not already exist')
const run = async (command: string[], cwd = root) => {
  const child = Bun.spawn(command, { cwd, stdout: 'pipe', stderr: 'pipe' })
  const [stdout, stderr, code] = await Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited])
  if (code !== 0) throw new Error(`${command[0]} failed (${code}): ${stderr}`)
  return stdout.trim()
}
const commit = await run(['git', 'rev-parse', '--verify', `${ref}^{commit}`])
const gitVersion = await run(['git', '--version'])
const dockerVersion = await run(['docker', 'version', '--format', '{{.Server.Version}}'])
// Select only portable hardware fields; never serialize daemon names/endpoints.
const hardware = JSON.parse(await run(['docker', 'info', '--format', '{"cpus":{{.NCPU}},"memoryBytes":{{.MemTotal}},"architecture":"{{.Architecture}}","os":"{{.OSType}}","kernel":"{{.KernelVersion}}"}']))
if (hardware.os !== 'linux' || hardware.cpus < 8 || hardware.memoryBytes < 24 * 1024 ** 3) throw new Error('The Docker daemon needs Linux, at least 8 CPUs and 24 GiB RAM (three equally limited engines)')
console.log(`Auditor preflight passed: ${commit.slice(0, 12)}, ${hardware.cpus} CPUs, ${Math.floor(hardware.memoryBytes / 1024 ** 3)} GiB. ${smoke ? 'Smoke' : 'Full 20M-row'} workload.`)
if (check) process.exit(0)
mkdirSync(output, { recursive: true })
const scratch = mkdtempSync(join(tmpdir(), 'pintail-auditor-'))
const checkout = join(scratch, 'source')
const provenance: Record<string, unknown> = { schemaVersion: 1, commit, mode: smoke ? 'smoke-not-published-evidence' : 'full', startedAt: new Date().toISOString(), tools: { bun: Bun.version, git: gitVersion, docker: dockerVersion }, hardware, cacheReuse: false, status: 'RUNNING' }
const save = () => writeFileSync(join(output, 'provenance.json'), JSON.stringify(provenance, null, 2) + '\n')
save()
try {
  await run(['git', 'clone', '--quiet', '--no-hardlinks', '--no-checkout', root, checkout])
  await run(['git', 'checkout', '--quiet', '--detach', commit], checkout)
  if (!readFileSync(join(checkout, 'benchmark/run.ts'), 'utf8').includes('PINTAIL_BENCHMARK_AUDIT')) throw new Error('Selected revision predates isolated auditor support')
  await run(['bun', 'install', '--frozen-lockfile'], join(checkout, 'benchmark'))
  const env = { ...process.env }
  for (const key of Object.keys(env)) if (key.startsWith('BENCHMARK_') || key.startsWith('PINTAIL_BENCHMARK_')) delete env[key]
  Object.assign(env, { PINTAIL_BENCHMARK_AUDIT: '1', BENCHMARK_SCALE: smoke ? '0.001' : '1' })
  if (smoke) Object.assign(env, { BENCHMARK_WARMUPS: '1', BENCHMARK_RUNS: '3', BENCHMARK_CONCURRENCY: '1,4', BENCHMARK_CONCURRENCY_SECONDS: '2' })
  console.log(`Running in an isolated checkout. Progress: ${join(output, 'private-run.log')}`)
  const log = openSync(join(output, 'private-run.log'), 'w', 0o600)
  let code: number
  try {
    const child = Bun.spawn(['bun', 'run', 'run.ts'], { cwd: join(checkout, 'benchmark'), env, stdout: log, stderr: log })
    code = await child.exited
  } finally { closeSync(log) }
  const suffix = smoke ? '-smoke' : ''
  for (const file of [`results${suffix}.json`, `results${suffix}.md`, 'mysql-baseline.json']) {
    const path = join(checkout, 'benchmark', file)
    // A failed run may leave the checkout's old tracked results in place.
    if (existsSync(path)) {
      const changed = await run(['git', 'diff', '--numstat', '--', `benchmark/${file}`], checkout)
      const untracked = await run(['git', 'ls-files', '--others', '--exclude-standard', '--', `benchmark/${file}`], checkout)
      if (changed || untracked) cpSync(path, join(output, file))
    }
  }
  if (code !== 0) throw new Error(`Benchmark failed (${code}); see private-run.log`)
  if (!existsSync(join(output, `results${suffix}.json`))) throw new Error('Benchmark produced no new report')
  provenance.status = 'PASS'
  console.log(`AUDITOR-DONE: artifacts in ${output}`)
} catch (error) {
  provenance.status = 'FAIL'
  throw error
} finally {
  provenance.finishedAt = new Date().toISOString()
  save()
  rmSync(scratch, { recursive: true, force: true })
}

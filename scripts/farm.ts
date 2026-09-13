#!/usr/bin/env bun
/// The verification farm: the generated gates, run on fresh seeds for as long
/// as a host is free, with every failure kept as a reproduction.
///
/// Each cycle builds HEAD once and walks the jobs below on seeds no earlier
/// cycle used - the seed cursor lives in validate-out/farm/state.json - so a
/// farm left running for a week covers a week of distinct inputs rather than
/// the same ones again. A failing job's log is kept under
/// validate-out/farm/findings/, its signature (the first panic or divergence
/// line, with numbers masked) is appended to findings.jsonl, and a signature
/// already on file is counted rather than stored twice. summary.md says what
/// ran, on which seeds, and what is open.
///
/// Usage:  bun run scripts/farm.ts                       # until stopped
///         bun run scripts/farm.ts --cycles 3 --jobs cdc-sim,disk-faults
/// Docker jobs (sql-fuzz, cdc-matrix, mtr-replica) use the DOCKER_HOST the
/// shell has; run the farm on a host no gate is measuring on.
import { spawnSync } from 'node:child_process'
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const farmDir = join(repository, 'validate-out', 'farm')
const findingsDir = join(farmDir, 'findings')
const statePath = join(farmDir, 'state.json')
const cargo = process.env.CARGO ?? 'cargo'

interface Job {
  name: string
  /// Seeds one run consumes, so the cursor advances past them.
  seedsPerRun: number
  docker: boolean
  command(seed: number): { args: string[]; cwd?: string; env: Record<string, string> }
}

const cargoEnv = { PINTAIL_DASHBOARD_PREBUILT: '1', CARGO_TARGET_DIR: join(repository, 'target') }

const JOBS: Job[] = [
  {
    name: 'cdc-sim',
    seedsPerRun: 200,
    docker: false,
    command: (seed) => ({
      args: [cargo, 'test', '--release', '-p', 'pintail-cdc', '--lib', 'simulation'],
      env: { ...cargoEnv, PINTAIL_CDC_SIM_SEED_BASE: String(seed), PINTAIL_CDC_SIM_SEEDS: '200', PINTAIL_CDC_SIM_STEPS: '300' },
    }),
  },
  {
    name: 'disk-faults',
    seedsPerRun: 5000,
    docker: false,
    command: (seed) => ({
      args: [cargo, 'test', '--release', '-p', 'pintail-store', '--test', 'disk_faults'],
      env: { ...cargoEnv, PINTAIL_DISK_FAULT_SEED_BASE: String(seed), PINTAIL_DISK_FAULT_SEEDS: '5000' },
    }),
  },
  {
    name: 'kernel-diff',
    seedsPerRun: 1,
    docker: false,
    command: (seed) => ({
      args: [cargo, 'test', '--release', '-p', 'pintail-exec', '--lib', 'differential'],
      env: { ...cargoEnv, PINTAIL_KERNEL_DIFF_SEED: String(seed), PINTAIL_KERNEL_DIFF_CASES: '100000' },
    }),
  },
  {
    name: 'sql-fuzz',
    seedsPerRun: 8,
    docker: true,
    command: (seed) => ({
      args: [cargo, 'test', '--release', '-p', 'pintail-sqllogic', '--test', 'mysql_oracle', 'fuzzes_against_configured_mysql', '--', '--ignored', '--nocapture'],
      env: {
        ...cargoEnv,
        PINTAIL_FUZZ_CASES: '2500',
        PINTAIL_FUZZ_SEEDS: Array.from({ length: 8 }, (_, i) => `0x${(0x5eed0000 + seed + i).toString(16)}`).join(','),
      },
    }),
  },
  {
    name: 'cdc-matrix',
    seedsPerRun: 1,
    docker: true,
    command: (seed) => ({
      args: ['bun', 'run', 'cdc-matrix.ts'],
      cwd: join(repository, 'tests', 'e2e'),
      env: { CDC_MATRIX_SEED: String(seed), CDC_MATRIX_ROUNDS: '60', PINTAIL_CDC_MATRIX_BINARY: join(repository, 'target', 'release', 'pintail') },
    }),
  },
]

function flag(name: string): string | undefined {
  const index = process.argv.indexOf(`--${name}`)
  return index > 0 ? process.argv[index + 1] : undefined
}

function loadState(): Record<string, number> {
  return existsSync(statePath) ? JSON.parse(readFileSync(statePath, 'utf8')) : {}
}

/// The first line that says what went wrong, with numbers masked so the same
/// defect on another seed shares a signature.
export function signature(log: string): string {
  const line =
    log.split('\n').find((l) => /panicked at|DIVERGED|differed|MISMATCH|FAIL:|error\[/.test(l) && !/test result/.test(l)) ??
    log.split('\n').reverse().find((l) => l.trim()) ??
    'no output'
  return line.replace(/\d+/g, 'N').replace(/\s+/g, ' ').trim().slice(0, 240)
}

function git(...args: string[]): string {
  return spawnSync('git', ['-C', repository, ...args], { encoding: 'utf8' }).stdout.trim()
}

function main() {
  mkdirSync(findingsDir, { recursive: true })
  const cycles = Number(flag('cycles') ?? 'Infinity')
  const selected = flag('jobs')?.split(',')
  const jobs = JOBS.filter((job) => !selected || selected.includes(job.name))
  const known = new Set<string>(
    existsSync(join(farmDir, 'findings.jsonl'))
      ? readFileSync(join(farmDir, 'findings.jsonl'), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l).signature)
      : [],
  )
  const ran: string[] = []
  for (let cycle = 1; cycle <= cycles; cycle += 1) {
    const commit = git('rev-parse', '--short=12', 'HEAD')
    const build = spawnSync(cargo, ['build', '--release', '-p', 'pintail'], { cwd: repository, env: { ...process.env, ...cargoEnv }, stdio: 'inherit' })
    if (build.status !== 0) {
      console.error('[farm] build failed; stopping')
      process.exit(1)
    }
    for (const job of jobs) {
      const state = loadState()
      const seed = state[job.name] ?? 1
      const { args, cwd, env } = job.command(seed)
      const started = Date.now()
      const run = spawnSync(args[0]!, args.slice(1), {
        cwd: cwd ?? repository,
        env: { ...process.env, ...env },
        encoding: 'utf8',
        maxBuffer: 256 * 1024 * 1024,
      })
      const log = `${run.stdout ?? ''}\n${run.stderr ?? ''}`
      const seconds = Math.round((Date.now() - started) / 1000)
      writeFileSync(statePath, JSON.stringify({ ...loadState(), [job.name]: seed + job.seedsPerRun }, null, 2) + '\n')
      let outcome = 'pass'
      if (run.status !== 0) {
        const sig = signature(log)
        outcome = known.has(sig) ? 'known finding' : 'NEW FINDING'
        if (!known.has(sig)) {
          known.add(sig)
          const file = join(findingsDir, `${new Date().toISOString().replace(/[:.]/g, '-')}-${job.name}-${seed}.log`)
          writeFileSync(file, log)
          appendFileSync(
            join(farmDir, 'findings.jsonl'),
            JSON.stringify({ at: new Date().toISOString(), job: job.name, seed, commit, signature: sig, log: file }) + '\n',
          )
        }
      }
      const line = `cycle ${cycle} ${job.name} seeds ${seed}..${seed + job.seedsPerRun - 1} at ${commit}: ${outcome} in ${seconds}s`
      console.log(`[farm] ${line}`)
      ran.push(`- ${line}`)
      writeSummary(ran)
    }
  }
}

function writeSummary(ran: string[]) {
  const findings = existsSync(join(farmDir, 'findings.jsonl'))
    ? readFileSync(join(farmDir, 'findings.jsonl'), 'utf8').split('\n').filter(Boolean).map((l) => JSON.parse(l))
    : []
  writeFileSync(
    join(farmDir, 'summary.md'),
    [
      '# Verification farm',
      '',
      `Updated ${new Date().toISOString()}. Seed cursors: \`${JSON.stringify(loadState())}\`.`,
      '',
      `## Open findings (${findings.length})`,
      '',
      ...findings.map((f) => `- **${f.job}** seed ${f.seed} at ${f.commit}: \`${f.signature}\` (${f.log})`),
      '',
      '## This session',
      '',
      ...ran.slice(-200),
      '',
    ].join('\n'),
  )
}

if (import.meta.main) main()

/// Sequential validation driver: the one detached process that runs every
/// gate in the right order and survives the failure modes ad-hoc runs kept
/// hitting (parallel runs filling the shared Docker host's disk, MySQL
/// containers probed during their init restart, dirty trees measured by the
/// benchmark, hung stages, crashed containers cleaned up before their logs
/// were read).
///
/// Usage:  bun run scripts/validate.ts [--stages fmt,unit,oracle,e2e,bench,accept]
///
/// Progress is streamed to stdout and mirrored into validate-status.log
/// (one line per transition — poll this file, not the process), with the
/// final verdict in validate-report.md. Exit code 0 only when every
/// requested stage passes.

import { spawn } from 'node:child_process'
import { appendFileSync, existsSync, mkdirSync, rmSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const reportDir = join(repository, 'validate-out')
const statusPath = join(reportDir, 'validate-status.log')
const reportPath = join(reportDir, 'validate-report.md')
const lockPath = join(reportDir, 'validate.lock')

/// Container name prefixes this repository's harnesses create. Cleanup
/// touches ONLY these — never a deployed compose stack or anything else on
/// the shared Docker host.
const OWNED_CONTAINER_PREFIXES = [
  'pintail-e2e-',
  'pintail-browser-',
  'pintail-m9-bench-',
  'pintail-m4-',
  'pintail-prod-',
  'pintail-mysql-oracle-',
]

/// Remote-host free space required before the storage-heavy stages run.
const MIN_DOCKER_HOST_FREE_GB = 15

/// Log signatures that mean "environment hiccup, retry once" rather than a
/// product failure.
const TRANSIENT_SIGNATURES = [
  /Can't connect to local MySQL server through socket/,
  /Connection lost: The server closed the connection/i,
  /did not become ready/i,
  /ECONNRESET|ECONNREFUSED|EPIPE/,
]

/// Signatures that mean the shared host itself is unhealthy; abort the run
/// instead of retrying into the same wall.
const HOST_FAILURE_SIGNATURES = [
  /ER_RECORD_FILE_FULL|No space left on device|disk full/i,
  /Cannot connect to the Docker daemon/i,
]

interface Stage {
  name: string
  /// Needs the shared Docker host (disk preflight + container capture).
  remote: boolean
  timeoutMinutes: number
  command: string[]
  cwd?: string
  env?: Record<string, string>
}

const STAGES: Stage[] = [
  {
    name: 'fmt',
    remote: false,
    timeoutMinutes: 10,
    command: ['bash', '-c', 'cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings'],
  },
  {
    name: 'unit',
    remote: false,
    timeoutMinutes: 20,
    command: ['cargo', 'test', '--workspace'],
  },
  {
    name: 'oracle',
    remote: true,
    timeoutMinutes: 20,
    command: [
      'cargo', 'test', '-p', 'pintail-sqllogic', '--test', 'mysql_oracle', '--', '--ignored', '--nocapture',
    ],
    env: { PINTAIL_DASHBOARD_PREBUILT: '1' },
  },
  {
    name: 'e2e',
    remote: true,
    timeoutMinutes: 60,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'tests', 'e2e'),
  },
  {
    name: 'bench',
    remote: true,
    timeoutMinutes: 90,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'benchmark'),
  },
  {
    name: 'accept',
    remote: true,
    timeoutMinutes: 120,
    command: [
      'bun', 'run', 'run-production.ts',
      '--profile', 'ci', '--dataset', 'ci',
      '--phases', 'snapshot,cold', '--engines', 'mysql,pintail',
    ],
    cwd: join(repository, 'benchmark'),
  },
]

function status(line: string) {
  const stamped = `${new Date().toISOString()} ${line}`
  console.log(stamped)
  appendFileSync(statusPath, `${stamped}\n`)
}

async function run(
  command: string[],
  options: { cwd?: string; env?: Record<string, string>; timeoutMinutes?: number } = {},
): Promise<{ code: number | null; output: string; timedOut: boolean }> {
  return new Promise((resolvePromise) => {
    // Some machines wrap `cargo`; honor CARGO and default the target dir
    // so builds land in-repo instead of on a slow external volume.
    const argv =
      command[0] === 'cargo' && process.env.CARGO ? [process.env.CARGO, ...command.slice(1)] : command
    const child = spawn(argv[0], argv.slice(1), {
      cwd: options.cwd ?? repository,
      env: { CARGO_TARGET_DIR: join(repository, 'target'), ...process.env, ...options.env },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let output = ''
    let timedOut = false
    const budget = (options.timeoutMinutes ?? 30) * 60_000
    const timer = setTimeout(() => {
      timedOut = true
      child.kill('SIGKILL')
    }, budget)
    const capture = (chunk: Buffer) => {
      output += chunk.toString()
      if (output.length > 4_000_000) output = output.slice(-2_000_000)
    }
    child.stdout?.on('data', capture)
    child.stderr?.on('data', capture)
    child.on('close', (code) => {
      clearTimeout(timer)
      resolvePromise({ code, output, timedOut })
    })
    child.on('error', (error) => {
      clearTimeout(timer)
      resolvePromise({ code: null, output: `${output}\nspawn error: ${error}`, timedOut })
    })
  })
}

async function docker(args: string[], timeoutMinutes = 2) {
  return run(['docker', ...args], { timeoutMinutes })
}

/// Preflight for stages that use the shared Docker host: daemon reachable
/// within a hard deadline (a wedged bulk transfer makes every call hang),
/// enough free disk for dataset loads, and no leftover harness containers
/// from a crashed earlier run.
async function dockerHostPreflight(): Promise<string | null> {
  const ping = await docker(['version', '--format', '{{.Server.Version}}'])
  if (ping.timedOut || ping.code !== 0) {
    return 'docker daemon unreachable or wedged (a stalled bulk transfer blocks every call; kill it and retry)'
  }
  const contexts = await docker(['context', 'show'])
  const remote = !contexts.output.includes('desktop')
  if (remote) {
    const disk = await run([
      'bash', '-c',
      `TARGET=$(docker context inspect $(docker context show) --format '{{.Endpoints.docker.Host}}' | sed 's|^ssh://||; s|:.*$||'); ` +
      `ssh -o BatchMode=yes -o ConnectTimeout=10 "$TARGET" "df -kP / | tail -1 | awk '{print \\$4}'"`,
    ], { timeoutMinutes: 2 })
    const freeKb = Number(disk.output.trim().split('\n').pop())
    if (Number.isFinite(freeKb)) {
      const freeGb = freeKb / 1024 / 1024
      if (freeGb < MIN_DOCKER_HOST_FREE_GB) {
        return `docker host has ${freeGb.toFixed(1)}GB free; need ${MIN_DOCKER_HOST_FREE_GB}GB — free space before running storage-heavy stages`
      }
      status(`preflight: docker host free space ${freeGb.toFixed(1)}GB`)
    } else {
      status('preflight: could not read docker host free space (continuing)')
    }
  }
  const leftovers = await docker(['ps', '-a', '--format', '{{.Names}}'])
  const owned = leftovers.output
    .split('\n')
    .filter((name) => OWNED_CONTAINER_PREFIXES.some((prefix) => name.startsWith(prefix)))
  for (const name of owned) {
    status(`preflight: removing leftover harness container ${name}`)
    await docker(['rm', '-f', name])
  }
  return null
}

/// Snapshot crashed-container evidence BEFORE harness cleanup erases it.
async function captureContainerEvidence(stage: string) {
  const listing = await docker(['ps', '-a', '--format', '{{.Names}}\t{{.Status}}'])
  const suspects = listing.output
    .split('\n')
    .filter((line) => OWNED_CONTAINER_PREFIXES.some((prefix) => line.startsWith(prefix)))
  for (const line of suspects) {
    const name = line.split('\t')[0]
    const logs = await docker(['logs', '--tail', '40', name])
    appendFileSync(
      join(reportDir, `${stage}-containers.log`),
      `== ${line}\n${logs.output}\n`,
    )
  }
}

function classify(output: string): 'transient' | 'host' | 'product' {
  if (HOST_FAILURE_SIGNATURES.some((signature) => signature.test(output))) return 'host'
  if (TRANSIENT_SIGNATURES.some((signature) => signature.test(output))) return 'transient'
  return 'product'
}

async function main() {
  const requested = (process.argv.find((arg) => arg.startsWith('--stages='))?.slice(9) ??
    process.env.VALIDATE_STAGES ?? 'fmt,unit,oracle,e2e,bench,accept')
    .split(',')
    .map((stage) => stage.trim())
  mkdirSync(reportDir, { recursive: true })
  if (existsSync(lockPath)) {
    console.error(`another validation run appears active (${lockPath}); remove the lock if it is stale`)
    process.exit(2)
  }
  writeFileSync(lockPath, `${process.pid} ${new Date().toISOString()}\n`)
  writeFileSync(statusPath, '')
  const results: Array<{ name: string; verdict: string; minutes: number; note: string }> = []

  try {
    // The benchmark refuses dirty trees; fail everything fast instead of
    // discovering it forty minutes in.
    const dirty = await run(['git', 'status', '--porcelain'])
    if (dirty.output.trim() && requested.includes('bench')) {
      status('ABORT: working tree is dirty and the bench stage is requested — commit first')
      process.exit(2)
    }

    for (const stage of STAGES) {
      if (!requested.includes(stage.name)) continue
      // Earlier stages write harness artifacts (the e2e gate rewrites its
      // results ledger), and the benchmark refuses dirty trees. When the
      // only dirt is a known harness artifact, commit it so the bench
      // still measures exactly one commit; any other dirt still aborts.
      if (stage.name === 'bench') {
        const HARNESS_ARTIFACTS = ['tests/e2e/results.md']
        const midRunDirty = await run(['git', 'status', '--porcelain'])
        const dirtyPaths = midRunDirty.output
          .trim()
          .split('\n')
          .filter((line) => line.trim().length > 0)
          .map((line) => line.slice(3))
        if (dirtyPaths.length > 0) {
          const onlyArtifacts = dirtyPaths.every((path) =>
            HARNESS_ARTIFACTS.some((artifact) => path === artifact || path.startsWith('tests/e2e/results-partial')))
          if (!onlyArtifacts) {
            status('ABORT before bench: working tree has non-harness changes — commit first')
            results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: 'dirty tree' })
            break
          }
          await run(['git', 'add', ...dirtyPaths])
          const committed = await run(['git', 'commit', '-m', 'e2e: bank gate artifacts from validate.ts run'])
          if (committed.code !== 0) {
            status('ABORT before bench: could not commit harness artifacts')
            results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: 'artifact commit failed' })
            break
          }
          status('bench: committed harness artifacts so the tree matches one commit')
        }
      }
      if (stage.remote) {
        const problem = await dockerHostPreflight()
        if (problem) {
          status(`ABORT before ${stage.name}: ${problem}`)
          results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: problem })
          break
        }
      }
      let verdict = 'FAIL'
      let note = ''
      const started = Date.now()
      for (let attempt = 1; attempt <= 2; attempt += 1) {
        status(`${stage.name}: attempt ${attempt} starting`)
        const outcome = await run(stage.command, {
          cwd: stage.cwd,
          env: stage.env,
          timeoutMinutes: stage.timeoutMinutes,
        })
        writeFileSync(join(reportDir, `${stage.name}-attempt${attempt}.log`), outcome.output)
        if (outcome.timedOut) {
          note = `timed out after ${stage.timeoutMinutes} minutes`
          await captureContainerEvidence(stage.name)
          break
        }
        if (outcome.code === 0) {
          verdict = 'PASS'
          break
        }
        await captureContainerEvidence(stage.name)
        const kind = classify(outcome.output)
        note = `exit ${outcome.code} (${kind})`
        if (kind === 'host') {
          status(`${stage.name}: host-level failure — aborting the run`)
          attempt = 2
          break
        }
        if (kind === 'transient' && attempt === 1) {
          status(`${stage.name}: transient failure, retrying once`)
          continue
        }
        break
      }
      const minutes = (Date.now() - started) / 60_000
      results.push({ name: stage.name, verdict, minutes, note })
      status(`${stage.name}: ${verdict}${note ? ` — ${note}` : ''} (${minutes.toFixed(1)}m)`)
      if (verdict !== 'PASS') break
    }
  } finally {
    rmSync(lockPath, { force: true })
  }

  const allPassed = results.length > 0 && results.every((result) => result.verdict === 'PASS')
  const lines = [
    `# Validation report — ${new Date().toISOString()}`,
    '',
    `Verdict: **${allPassed ? 'PASS' : 'FAIL'}**`,
    '',
    '| stage | verdict | minutes | note |',
    '|---|---|---|---|',
    ...results.map((result) =>
      `| ${result.name} | ${result.verdict} | ${result.minutes.toFixed(1)} | ${result.note} |`),
    '',
    'Per-stage logs sit next to this report; crashed-container logs are',
    'captured as <stage>-containers.log before harness cleanup removes them.',
  ]
  writeFileSync(reportPath, lines.join('\n'))
  status(`DONE: ${allPassed ? 'PASS' : 'FAIL'} — report at ${reportPath}`)
  process.exit(allPassed ? 0 : 1)
}

await main()

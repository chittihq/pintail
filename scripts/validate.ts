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
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
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
  /// Kill the stage after this long without output. Defaults to 20
  /// minutes; raise it for stages with legitimately silent stretches, such
  /// as a container image build.
  stallMinutes?: number
  /// Needs the shared Docker host (disk preflight + container capture).
  remote: boolean
  /// Stages sharing a lane run concurrently. Remote stages intentionally have
  /// no lane: the repository protocol gives oracle, E2E, benchmark, and
  /// acceptance exclusive use of the shared Docker host.
  lane?: string
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
    // nextest schedules tests across the workspace's binaries in
    // parallel; there are no doctests to lose.
    command: ['cargo', 'nextest', 'run', '--workspace'],
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
    // Builds a container image before it says anything.
    stallMinutes: 25,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'tests', 'e2e'),
  },
  {
    name: 'bench',
    remote: true,
    timeoutMinutes: 90,
    stallMinutes: 25,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'benchmark'),
  },
  {
    name: 'accept',
    remote: true,
    timeoutMinutes: 120,
    // The stage that hung twice. Its dataset copy and snapshot both report
    // progress while working, so a long silence here means stuck rather
    // than busy — but leave room for a slow link between heartbeats.
    stallMinutes: 20,
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
  options: {
    cwd?: string
    env?: Record<string, string>
    timeoutMinutes?: number
    /// Kill a stage that has produced no output for this long. The total
    /// budget catches a stage that is slow; this catches one that is stuck,
    /// which is the failure we actually keep hitting — a wedged docker link
    /// or a vanished container leaves the harness waiting in silence, and
    /// silence is indistinguishable from progress until the whole budget is
    /// gone.
    stallMinutes?: number
    /// Stage name, so heartbeats say which stage is alive.
    label?: string
  } = {},
): Promise<{ code: number | null; output: string; timedOut: boolean; stalled: boolean }> {
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
    let stalled = false
    let lastActivity = Date.now()
    let lastLine = ''
    let lastHeartbeat = Date.now()
    const budget = (options.timeoutMinutes ?? 30) * 60_000
    const stallBudget = (options.stallMinutes ?? 20) * 60_000
    const timer = setTimeout(() => {
      timedOut = true
      child.kill('SIGKILL')
    }, budget)
    // Poll rather than reset a timer per chunk: a chatty stage would
    // otherwise rebuild the timer thousands of times a second.
    const watchdog = setInterval(() => {
      const quiet = Date.now() - lastActivity
      if (quiet > stallBudget) {
        stalled = true
        child.kill('SIGKILL')
        return
      }
      // Heartbeat so a long stage is visibly alive in the status log, and
      // so a reader can see what it was doing when it stopped.
      if (child.exitCode === null && Date.now() - lastHeartbeat >= 60_000) {
        lastHeartbeat = Date.now()
        const quietFor = Math.round(quiet / 1000)
        status(`${options.label ?? 'stage'}: alive, ${quietFor}s since output — ${lastLine.slice(0, 120)}`)
      }
    }, 15_000)
    const capture = (chunk: Buffer) => {
      const text = chunk.toString()
      output += text
      if (output.length > 4_000_000) output = output.slice(-2_000_000)
      lastActivity = Date.now()
      const lines = text.split('\n').filter((line) => line.trim().length > 0)
      if (lines.length > 0) lastLine = lines[lines.length - 1]
    }
    child.stdout?.on('data', capture)
    child.stderr?.on('data', capture)
    const finish = (result: { code: number | null; output: string }) => {
      clearTimeout(timer)
      clearInterval(watchdog)
      resolvePromise({ ...result, timedOut, stalled })
    }
    child.on('close', (code) => finish({ code, output }))
    child.on('error', (error) =>
      finish({ code: null, output: `${output}\nspawn error: ${error}` }),
    )
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
  const context = await docker(['context', 'show'])
  const inspected = await docker([
    'context',
    'inspect',
    context.output.trim(),
    '--format',
    '{{.Endpoints.docker.Host}}',
  ])
  // DOCKER_HOST overrides Docker's selected context. Use that same effective
  // endpoint for the SSH disk check or the preflight can probe one machine
  // while every Docker command targets another.
  const endpoint = process.env.DOCKER_HOST?.trim() || inspected.output.trim()
  const remote = endpoint.startsWith('ssh://')
  if (remote) {
    const sshTarget = endpoint.slice('ssh://'.length).replace(/\/.*$/, '').replace(/:\d+$/, '')
    const disk = await run([
      'ssh',
      '-o',
      'BatchMode=yes',
      '-o',
      'ConnectTimeout=10',
      sshTarget,
      "df -kP / | tail -1 | awk '{print $4}'",
    ], { timeoutMinutes: 2 })
    const freeKb = Number(disk.output.trim().split('\n').pop())
    if (Number.isFinite(freeKb)) {
      let freeGb = freeKb / 1024 / 1024
      if (freeGb < MIN_DOCKER_HOST_FREE_GB) {
        // Reclaim regenerable space we own before giving up: build cache
        // and dangling images. Named volumes and other projects' state
        // are never touched.
        status(`preflight: ${freeGb.toFixed(1)}GB free < ${MIN_DOCKER_HOST_FREE_GB}GB — pruning build cache and dangling images`)
        await docker(['builder', 'prune', '-f'])
        await docker(['image', 'prune', '-f'])
        const after = await run([
          'ssh',
          '-o',
          'BatchMode=yes',
          '-o',
          'ConnectTimeout=10',
          sshTarget,
          "df -kP / | tail -1 | awk '{print $4}'",
        ], { timeoutMinutes: 5 })
        const afterKb = Number(after.output.trim().split('\n').pop())
        if (Number.isFinite(afterKb)) freeGb = afterKb / 1024 / 1024
        if (freeGb < MIN_DOCKER_HOST_FREE_GB) {
          return `docker host has ${freeGb.toFixed(1)}GB free after pruning; need ${MIN_DOCKER_HOST_FREE_GB}GB`
        }
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
  // A killed run leaves its lock behind, and every later run then refuses
  // to start until someone clears it by hand — which reads as "still
  // running" for as long as nobody looks. Trust the recorded pid instead of
  // the file's existence: if that process is gone, so is the run.
  if (existsSync(lockPath)) {
    const holder = Number.parseInt(readFileSync(lockPath, 'utf8').trim().split(/\s+/)[0] ?? '', 10)
    let alive = false
    if (Number.isInteger(holder) && holder > 0) {
      try {
        // Signal 0 tests for the process without touching it.
        process.kill(holder, 0)
        alive = true
      } catch (error) {
        // EPERM means it exists under another user, which still counts as
        // active; anything else means it is gone.
        alive = (error as NodeJS.ErrnoException).code === 'EPERM'
      }
    }
    if (alive) {
      console.error(`another validation run is active (pid ${holder}, ${lockPath})`)
      process.exit(2)
    }
    console.error(`clearing stale lock from pid ${holder || 'unknown'} (${lockPath})`)
    rmSync(lockPath, { force: true })
  }
  writeFileSync(lockPath, `${process.pid} ${new Date().toISOString()}\n`)
  writeFileSync(statusPath, '')
  /// Commits known harness result ledgers so remote stages see one clean
  /// commit. Returns false when non-artifact paths are dirty.
  async function commitHarnessArtifacts(context: string): Promise<boolean> {
    const ARTIFACT_PREFIXES = [
      'tests/e2e/results',
      'benchmark/results.',
      'benchmark/workloads/commerce-production-v1/results/',
    ]
    const porcelain = await run(['git', 'status', '--porcelain'])
    // No global trim: it would eat the first line's leading status space
    // and shift the path slice by one.
    const dirtyPaths = porcelain.output
      .split('\n')
      .filter((line) => line.trim().length > 0)
      .map((line) => line.slice(3))
    if (dirtyPaths.length === 0) return true
    if (!dirtyPaths.every((path) => ARTIFACT_PREFIXES.some((prefix) => path.startsWith(prefix)))) {
      status(`dirty non-artifact paths: ${dirtyPaths.join(', ')}`)
      return false
    }
    await run(['git', 'add', ...dirtyPaths])
    const committed = await run(['git', 'commit', '-m', `harness: bank gate artifacts (before ${context})`])
    if (committed.code === 0) status(`${context}: banked harness artifacts as one commit`)
    return committed.code === 0
  }

  const results: Array<{ name: string; verdict: string; minutes: number; note: string }> = []

  try {
    // The benchmark refuses dirty trees; fail everything fast instead of
    // discovering it forty minutes in. Harness artifacts (results ledgers
    // earlier stages rewrite) are auto-committed instead of aborting.
    if (requested.includes('bench') && !(await commitHarnessArtifacts('launch'))) {
      status('ABORT: working tree has non-artifact changes and bench is requested — commit first')
      process.exit(2)
    }

    // Consecutive stages sharing a lane run together; everything else keeps
    // its own group of one. Remote stages have no lane and therefore run in
    // the declared fmt → unit → oracle → e2e → bench → accept order.
    // A name that matches no stage would otherwise be dropped in silence and
    // the run would still report PASS — a typo in --stages must not look
    // like a green gate.
    const unknown = requested.filter(
      (name) => !STAGES.some((stage) => stage.name === name),
    )
    if (unknown.length > 0) {
      status(`ABORT: unknown stage(s) requested: ${unknown.join(', ')}`)
      process.exit(2)
    }
    const groups: (typeof STAGES)[] = []
    for (const stage of STAGES) {
      if (!requested.includes(stage.name)) continue
      const previous = groups.at(-1)
      if (stage.lane && previous?.at(-1)?.lane === stage.lane) {
        previous.push(stage)
      } else {
        groups.push([stage])
      }
    }

    /// Runs one stage to a verdict. `null` means the run must stop before it
    /// started — a dirty tree or an unusable host.
    const runStage = async (stage: (typeof STAGES)[number]) => {
        // Earlier stages rewrite harness artifacts and the benchmark
        // refuses dirty trees: bank artifacts before every remote stage.
        if (stage.remote && !(await commitHarnessArtifacts(stage.name))) {
          status(`ABORT before ${stage.name}: working tree has non-artifact changes — commit first`)
          results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: 'dirty tree' })
          return undefined
        }
        if (stage.remote) {
          const problem = await dockerHostPreflight()
          if (problem) {
            status(`ABORT before ${stage.name}: ${problem}`)
            results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: problem })
            return undefined
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
            stallMinutes: stage.stallMinutes,
            label: stage.name,
          })
          writeFileSync(join(reportDir, `${stage.name}-attempt${attempt}.log`), outcome.output)
          if (outcome.stalled) {
            // Distinct from a timeout: the stage was not slow, it stopped
            // making progress. Two hangs in one session looked like this —
            // a vanished container and a wedged docker link — and both spent
            // the whole budget in silence.
            note = `stalled — no output for ${stage.stallMinutes ?? 20} minutes`
            status(`${stage.name}: no output for ${stage.stallMinutes ?? 20} minutes — killing`)
            await captureContainerEvidence(stage.name)
            break
          }
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
        status(`${stage.name}: ${verdict}${note ? ` — ${note}` : ''} (${minutes.toFixed(1)}m)`)
        return { name: stage.name, verdict, minutes, note }
    }

    for (const group of groups) {
      if (group.length > 1) {
        status(`lane ${group[0].lane}: ${group.map((one) => one.name).join(' + ')} together`)
      }
      const settled = await Promise.all(group.map((one) => runStage(one)))
      const outcomes = settled.filter((outcome) => outcome !== undefined)
      results.push(...outcomes)
      // A stage that returned nothing aborted before starting and has
      // already recorded its own ABORTED row; either way the run stops.
      if (outcomes.length !== group.length) break
      if (outcomes.some((outcome) => outcome.verdict !== 'PASS')) break
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

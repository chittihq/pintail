/// Sequential validation driver: the one detached process that runs every
/// gate in the right order and survives the failure modes ad-hoc runs kept
/// hitting (parallel runs filling the shared Docker host's disk, MySQL
/// containers probed during their init restart, dirty trees measured by the
/// benchmark, hung stages, crashed containers cleaned up before their logs
/// were read).
///
/// Usage:  bun run scripts/validate.ts [--stages fmt,unit,oracle,e2e,browser,bench,accept]
///
/// Progress is streamed to stdout and mirrored into validate-status.log
/// (one line per transition — poll this file, not the process), with the
/// final verdict in validate-report.md. Exit code 0 only when every
/// requested stage passes.

import { spawn } from 'node:child_process'
import { appendFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs'
import { homedir } from 'node:os'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const reportDir = join(repository, 'validate-out')
const statusPath = join(reportDir, 'validate-status.log')
const reportPath = join(reportDir, 'validate-report.md')
const lockPath = join(reportDir, 'validate.lock')
const cargoBinary = process.env.CARGO ?? join(homedir(), '.cargo', 'bin', 'cargo')

/// Container name prefixes this repository's harnesses create. Cleanup
/// touches ONLY these — never a deployed compose stack or anything else on
/// the shared Docker host.
/// Containers the harnesses deliberately keep across runs (persistent
/// sources reused by PINTAIL_E2E_KEEP_MYSQL); never removed as leftovers.
const KEPT_CONTAINER_MARKER = '-keep-'

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
    command: [
      'bash',
      '-c',
      // The README's benchmark table is generated from the result artifact.
      // Checking it here means a hand-edit, or a banked run nobody regenerated
      // against, fails the gate instead of leaving the published numbers
      // describing a run that cannot be identified - which is how they drifted
      // to advertising 152ms where the artifact recorded 10ms.
      '"$CARGO" fmt --all --check && "$CARGO" clippy --workspace --all-targets -- -D warnings' +
        ' && bun run benchmark/render-readme-table.ts --check' +
        // The table check catches evidence nobody regenerated the README
        // against. It does not catch evidence nobody regenerated at all: a
        // result file committed before the code it measures looks identical to
        // a current one, which is how the TPC-H results came to record a pass
        // produced two days before the rule they exercise was rewritten.
        ' && bun run benchmark/check-evidence-freshness.ts',
    ],
  },
  {
    name: 'unit',
    remote: false,
    // Fresh macOS test binaries can spend several minutes each in first-launch
    // provenance checks before nextest has even built its test list. Keep that
    // discovery cost distinct from a test stall and within the gate budget.
    timeoutMinutes: 40,
    stallMinutes: 30,
    // Serial discovery avoids macOS launching every fresh test binary into
    // concurrent provenance checks. The tests themselves are fast enough that
    // this is materially quicker and more reliable than loader fan-out.
    command: ['cargo', 'nextest', 'run', '--test-threads', '1', '--workspace'],
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
    // PINTAIL_E2E_DOCKER_HOST points this stage's source container at a
    // different daemon (a second host, or a local one) without moving the
    // rest of the suite; the release chain's DOCKER_HOST stays authoritative
    // when the override is absent.
    env: process.env.PINTAIL_E2E_DOCKER_HOST
      ? { DOCKER_HOST: process.env.PINTAIL_E2E_DOCKER_HOST }
      : undefined,
    // The drift and reset checks each force a full-corpus recopy and the
    // harness re-diffs every query after each phase; the grown gate runs
    // ~65 minutes on the shared host.
    timeoutMinutes: 90,
    // Builds a container image before it says anything.
    stallMinutes: 25,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'tests', 'e2e'),
  },
  {
    // The second-major MySQL leg: the same full gate against mysql:8.0 on
    // a fresh container (never the keep-container, whose state belongs to
    // the primary leg). Banks its own ledger (results-mysql80.md). Part of
    // the rc stage list - a version we claim is covered has to gate.
    name: 'e2e-mysql80',
    remote: true,
    env: {
      ...(process.env.PINTAIL_E2E_DOCKER_HOST
        ? { DOCKER_HOST: process.env.PINTAIL_E2E_DOCKER_HOST }
        : {}),
      PINTAIL_E2E_MYSQL_IMAGE: 'mysql:8.0',
      PINTAIL_E2E_KEEP_MYSQL: '',
      PINTAIL_E2E_RESULTS_SUFFIX: '-mysql80',
    },
    timeoutMinutes: 90,
    stallMinutes: 25,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'tests', 'e2e'),
  },
  {
    // Production-shaped browser soak: 2M-row initial sync, dashboard actions
    // under live ingest, an 18M-row CDC backfill, Reset at 20M, and the
    // sakila dataset - tens of minutes BY DESIGN. Opt-in only; never in the
    // default stage list or the release chain.
    name: 'soak',
    remote: true,
    timeoutMinutes: 180,
    stallMinutes: 45,
    command: ['bun', 'run', 'soak.ts'],
    cwd: join(repository, 'tests', 'browser'),
  },
  {
    // The release image on the docker host with hundreds of tables and a fast
    // supervisor: resident memory must settle after warm-up. The only
    // memory measurement that runs on Linux, which is where the allocator
    // that hoarded seven gigabytes on staging lives.
    name: 'memsoak',
    remote: true,
    timeoutMinutes: 45,
    stallMinutes: 20,
    command: ['bun', 'run', 'run.ts'],
    cwd: join(repository, 'tests', 'memsoak'),
  },
  {
    name: 'browser',
    remote: true,
    timeoutMinutes: 30,
    // Builds the release binary before Chromium starts producing output.
    stallMinutes: 25,
    command: ['bun', 'run', 'gate'],
    cwd: join(repository, 'tests', 'browser'),
    env: { PINTAIL_DASHBOARD_PREBUILT: '1' },
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
    /// Whether this stage runs on the shared Docker host, which is the only
    /// case where a load probe is meaningful.
    remote?: boolean
    /// Stage name, so heartbeats say which stage is alive.
    label?: string
  } = {},
): Promise<{
  code: number | null
  output: string
  timedOut: boolean
  stalled: boolean
  stallReason: string
}> {
  return new Promise((resolvePromise) => {
    // Resolve Cargo explicitly so validation cannot accidentally use a shell
    // wrapper, and keep every build artifact under the repository target.
    const argv = command[0] === 'cargo' ? [cargoBinary, ...command.slice(1)] : command
    const child = spawn(argv[0], argv.slice(1), {
      cwd: options.cwd ?? repository,
      env: {
        ...process.env,
        CARGO: cargoBinary,
        CARGO_TARGET_DIR: join(repository, 'target'),
        ...options.env,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    })
    let output = ''
    let timedOut = false
    let stalled = false
    let stallReason = ''
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
    // Load on the build host, sampled while a remote stage is silent.
    //
    // Reported, never acted on. An earlier version killed a stage that was
    // silent while the host was idle, on the theory that the work had never
    // started - but every remote stage begins with a LOCAL cargo build
    // (tests/e2e/run.ts, tests/browser/run.ts, benchmark/run.ts all build the
    // release binary first) whose output is fully buffered. During a cold
    // build there is legitimately no output and the remote host is
    // legitimately at zero, so that rule killed healthy stages. The
    // distinction is real and worth showing; it is not safe to automate.
    let hostLoad: number | null = null
    let lastProbe = 0
    let probing = false
    const watchdog = setInterval(() => {
      const quiet = Date.now() - lastActivity
      if (quiet > stallBudget) {
        stalled = true
        const idle = hostLoad !== null && hostLoad < IDLE_LOAD
        stallReason = `no output for ${Math.round(stallBudget / 60_000)} minutes`
          + (hostLoad === null
            ? ''
            : idle
              ? ` while the build host sat idle (load ${hostLoad.toFixed(2)}) — the work never started`
              : ` while the build host was busy (load ${hostLoad.toFixed(2)}) — work was in progress`)
        child.kill('SIGKILL')
        return
      }
      // Throttled to once a minute. The previous comment claimed a cache and
      // had none, so a long silence opened an SSH connection every fifteen
      // seconds to the host it already suspected of being wedged.
      if (
        options.remote
        && quiet > 90_000
        && !probing
        && Date.now() - lastProbe >= 60_000
      ) {
        probing = true
        lastProbe = Date.now()
        void remoteLoadAverage()
          .then((load) => {
            hostLoad = load
          })
          .finally(() => {
            probing = false
          })
      }
      // Heartbeat so a long stage is visibly alive in the status log, and so a
      // reader can see what it was doing when it stopped. The host's load
      // travels with it: "alive" previously meant only that a process existed,
      // which is exactly what made a wedged stage look like a slow one.
      if (child.exitCode === null && Date.now() - lastHeartbeat >= 60_000) {
        lastHeartbeat = Date.now()
        const quietFor = Math.round(quiet / 1000)
        const busy = hostLoad === null
          ? ''
          : ` — host load ${hostLoad.toFixed(2)}${hostLoad < IDLE_LOAD ? ' (IDLE)' : ''}`
        status(`${options.label ?? 'stage'}: alive, ${quietFor}s since output${busy} — ${lastLine.slice(0, 120)}`)
      }
    }, 15_000)
    const capture = (chunk: Buffer) => {
      const text = chunk.toString()
      output += text
      if (output.length > 4_000_000) output = output.slice(-2_000_000)
      lastActivity = Date.now()
      hostLoad = null
      const lines = text.split('\n').filter((line) => line.trim().length > 0)
      if (lines.length > 0) lastLine = lines[lines.length - 1]
    }
    child.stdout?.on('data', capture)
    child.stderr?.on('data', capture)
    const finish = (result: { code: number | null; output: string }) => {
      clearTimeout(timer)
      clearInterval(watchdog)
      resolvePromise({ ...result, timedOut, stalled, stallReason })
    }
    child.on('close', (code) => finish({ code, output }))
    child.on('error', (error) =>
      finish({ code: null, output: `${output}\nspawn error: ${error}` }),
    )
  })
}

async function dockerWith(
  args: string[],
  timeoutMinutes = 2,
  env?: Record<string, string>,
) {
  return run(['docker', ...args], { timeoutMinutes, env })
}

async function docker(args: string[], timeoutMinutes = 2) {
  return dockerWith(args, timeoutMinutes)
}

/// Preflight for stages that use the shared Docker host: daemon reachable
/// within a hard deadline (a wedged bulk transfer makes every call hang),
/// enough free disk for dataset loads, and no leftover harness containers
/// from a crashed earlier run.
/// The SSH target of the shared Docker host, once the preflight has resolved
/// it. Null when Docker is local, where a busy-ness probe is meaningless.
let remoteSshTarget: string | null = null

/// One-minute load average on the build host, or null if it cannot be read.
///
/// This exists because "the stage has produced no output" and "the stage is
/// wedged" are different claims, and the harness could not previously tell
/// them apart. A Rust workspace build saturates every core; a host sitting at
/// 0.07 while a stage claims to be building is not slow, it is stuck.
async function remoteLoadAverage(): Promise<number | null> {
  if (!remoteSshTarget) return null
  const probe = await run([
    'ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
    remoteSshTarget, "cut -d' ' -f1 /proc/loadavg",
  ], { timeoutMinutes: 1 })
  const load = Number(probe.output.trim().split('\n').pop())
  return Number.isFinite(load) ? load : null
}

/// Below this, a machine that is supposedly compiling is doing nothing.
const IDLE_LOAD = 0.5

async function dockerHostPreflight(
  stageEnv?: Record<string, string>,
): Promise<string | null> {
  const docker = (args: string[], timeoutMinutes = 2) =>
    dockerWith(args, timeoutMinutes, stageEnv)
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
  const endpoint =
    stageEnv?.DOCKER_HOST?.trim() || process.env.DOCKER_HOST?.trim() || inspected.output.trim()
  const remote = endpoint.startsWith('ssh://')
  if (remote) {
    const sshTarget = endpoint.slice('ssh://'.length).replace(/\/.*$/, '').replace(/:\d+$/, '')
    // Remembered so the liveness probe can reach the same machine without
    // re-deriving it mid-stage.
    remoteSshTarget = sshTarget
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
    .filter((name) => !name.includes(KEPT_CONTAINER_MARKER))
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
    process.env.VALIDATE_STAGES ?? 'fmt,unit,oracle,e2e,browser,bench,accept')
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
  /// The benchmark refuses a dirty tree, and earlier stages rewrite result
  /// ledgers. Rather than committing a receipt to the product branch for
  /// every gate — which made roughly one commit in eight a scoreboard entry
  /// — set the ledgers aside so the tree is clean for the remote stages and
  /// put them back when the run finishes. The evidence survives on disk;
  /// committing it stays a deliberate act.
  const ARTIFACT_PREFIXES = [
    'tests/e2e/results',
    'benchmark/results.',
    'benchmark/workloads/commerce-production-v1/results/',
  ]
  const shelfDir = join(reportDir, 'artifact-shelf')
  let shelved: string[] = []

  async function shelveHarnessArtifacts(context: string): Promise<boolean> {
    const porcelain = await run(['git', '--no-optional-locks', 'status', '--porcelain'])
    // No global trim: it would eat the first line's leading status space
    // and shift the path slice by one.
    const entries = porcelain.output
      .split('\n')
      .filter((line) => line.trim().length > 0)
      .map((line) => ({ untracked: line.startsWith('??'), path: line.slice(3) }))
    if (entries.length === 0) return true
    if (
      !entries.every((entry) => ARTIFACT_PREFIXES.some((prefix) => entry.path.startsWith(prefix)))
    ) {
      status(`dirty non-artifact paths: ${entries.map((entry) => entry.path).join(', ')}`)
      return false
    }
    for (const { path } of entries) {
      const shelfPath = join(shelfDir, path)
      mkdirSync(join(shelfPath, '..'), { recursive: true })
      if (existsSync(join(repository, path))) {
        writeFileSync(shelfPath, readFileSync(join(repository, path)))
      }
      if (!shelved.includes(path)) shelved.push(path)
    }
    // Restore the committed content so the remote stages see a clean tree.
    // A ledger with no committed ancestor (a new leg's first run) cannot be
    // checked out - it is removed instead, and comes back from the shelf.
    const tracked = entries.filter((entry) => !entry.untracked).map((entry) => entry.path)
    for (const entry of entries) {
      if (entry.untracked) rmSync(join(repository, entry.path), { force: true })
    }
    if (tracked.length > 0) {
      const restored = await run(['git', 'checkout', '--', ...tracked])
      if (restored.code !== 0) return false
    }
    status(`${context}: shelved ${entries.length} harness ledger(s); tree clean`)
    return true
  }

  /// Returns the shelved ledgers to the working tree so the run's evidence is
  /// on disk, uncommitted, for whoever decides to keep it.
  async function unshelveHarnessArtifacts(): Promise<void> {
    // A stage that ran after shelving may have written FRESH evidence to a
    // shelved path; restoring the pre-run copy over it would replace the
    // run's ledger with a stale one (this clobbered a green mysql80 ledger
    // with the prior failing run's). Only paths the stages left at their
    // committed state are restored.
    const porcelain = await run(['git', '--no-optional-locks', 'status', '--porcelain'])
    const rewritten = new Set(
      porcelain.output
        .split('\n')
        .filter((line) => line.trim().length > 0)
        .map((line) => line.slice(3)),
    )
    let restored = 0
    for (const path of shelved) {
      if (rewritten.has(path)) continue
      const shelfPath = join(shelfDir, path)
      if (!existsSync(shelfPath)) continue
      mkdirSync(join(repository, path, '..'), { recursive: true })
      writeFileSync(join(repository, path), readFileSync(shelfPath))
      restored += 1
    }
    if (restored > 0) {
      status(`restored ${restored} harness ledger(s) to the working tree, uncommitted`)
    }
    shelved = []
  }

  const results: Array<{ name: string; verdict: string; minutes: number; note: string }> = []

  try {
    // The benchmark refuses dirty trees; fail everything fast instead of
    // discovering it forty minutes in. Harness artifacts (results ledgers
    // earlier stages rewrite) are auto-committed instead of aborting.
    if (requested.includes('bench') && !(await shelveHarnessArtifacts('launch'))) {
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
    // e2e, browser, and accept each build the same release binary; build it
    // once here and hand every stage the path through the overrides they
    // already honor. Skipped when the caller exported a binary of its own.
    const binaryConsumers = ['e2e', 'browser', 'accept', 'soak']
    if (
      !process.env.PINTAIL_E2E_BINARY
      && requested.some((name) => binaryConsumers.includes(name))
    ) {
      status('prebuild: cargo build --release --package pintail')
      const prebuild = await run(
        [cargoBinary, 'build', '--release', '--package', 'pintail'],
        { timeoutMinutes: 30, label: 'prebuild' },
      )
      if (prebuild.code !== 0) {
        status('ABORT: release prebuild failed')
        writeFileSync(join(reportDir, 'prebuild.log'), prebuild.output)
        process.exit(2)
      }
      const binary = join(repository, 'target', 'release', 'pintail')
      process.env.PINTAIL_E2E_BINARY = binary
      process.env.PINTAIL_BENCHMARK_BINARY = binary
      status(`prebuild: exported ${binary}`)
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
        if (stage.remote && !(await shelveHarnessArtifacts(stage.name))) {
          status(`ABORT before ${stage.name}: working tree has non-artifact changes — commit first`)
          results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: 'dirty tree' })
          return undefined
        }
        if (stage.remote) {
          const problem = await dockerHostPreflight(stage.env)
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
            remote: stage.remote,
            label: stage.name,
          })
          writeFileSync(join(reportDir, `${stage.name}-attempt${attempt}.log`), outcome.output)
          if (outcome.stalled) {
            // Distinct from a timeout: the stage was not slow, it stopped
            // making progress. Two hangs in one session looked like this —
            // a vanished container and a wedged docker link — and both spent
            // the whole budget in silence.
            note = `stalled — ${outcome.stallReason}`
            status(`${stage.name}: ${outcome.stallReason} — killing`)
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

    // The local stages (fmt, unit) touch no container and no ledger; they
    // run as one serial chain concurrent with the remote sequence, which
    // takes them off the critical path. PINTAIL_VALIDATE_OVERLAP=0 restores
    // the strictly sequential order.
    const overlap = process.env.PINTAIL_VALIDATE_OVERLAP !== '0'
    const localNames = ['fmt', 'unit']
    const localStages = overlap
      ? groups.flat().filter((stage) => localNames.includes(stage.name))
      : []
    const remoteGroups = overlap
      ? groups
          .map((group) => group.filter((stage) => !localNames.includes(stage.name)))
          .filter((group) => group.length > 0)
      : groups
    const localResults: Array<{ name: string; verdict: string; minutes: number; note: string }> = []
    const localRun = (async () => {
      for (const stage of localStages) {
        const outcome = await runStage(stage)
        if (!outcome) return
        localResults.push(outcome)
        if (outcome.verdict !== 'PASS') return
      }
    })()
    if (localStages.length > 0) {
      status(`lane local: ${localStages.map((one) => one.name).join(' → ')} overlapping the remote sequence`)
    }

    for (const group of remoteGroups) {
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
    await localRun
    // Report rows keep the declared stage order whatever finished first.
    const ordered = [...localResults, ...results].sort(
      (left, right) =>
        STAGES.findIndex((stage) => stage.name === left.name)
        - STAGES.findIndex((stage) => stage.name === right.name),
    )
    results.length = 0
    results.push(...ordered)
    if (localStages.length > 0 && localResults.length !== localStages.length) {
      // A local stage aborted without a row; make the miss visible.
      for (const stage of localStages) {
        if (!results.some((result) => result.name === stage.name)) {
          results.push({ name: stage.name, verdict: 'ABORTED', minutes: 0, note: 'did not run' })
        }
      }
    }
  } finally {
    // Always give the ledgers back, including on abort — the evidence is the
    // point of the run, and it must not be lost just because a stage failed.
    await unshelveHarnessArtifacts()
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

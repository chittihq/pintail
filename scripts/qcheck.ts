/// Differential quick-check: one query against a live MySQL + Pintail pair.
///
/// The develop-verify loop between full gate runs (AGENTS.md "Fast debug
/// loop"): spins a dedicated MySQL container on the configured Docker host,
/// seeds it, points a locally built Pintail at it over the wire, executes the
/// given SQL on both engines and diffs the answers. MySQL is the oracle -
/// when the two disagree, MySQL is right until proven otherwise (it corrected
/// the ENUM comparison model the first day this loop ran).
///
/// Usage:
///   bun run scripts/qcheck.ts "SELECT ..."                one query
///   bun run scripts/qcheck.ts --file queries.sql          one query per line
///   bun run scripts/qcheck.ts --seed seed.sql "SELECT.."  custom schema/rows
///   bun run scripts/qcheck.ts --keep "SELECT ..."         leave the pair up
///
/// The MySQL host is derived from DOCKER_HOST (ssh://<host>), so no
/// infrastructure names live in this file. Requires the Docker host's mapped
/// ports to be reachable from this machine.

import { spawnSync, spawn } from 'bun'
import { readFileSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'

const repository = join(import.meta.dir, '..')

function fail(message: string): never {
  console.error(`qcheck: ${message}`)
  process.exit(1)
}

const args = [...process.argv.slice(2)]
let seedPath: string | undefined
let queryFile: string | undefined
let keep = false
const queries: string[] = []
while (args.length > 0) {
  const arg = args.shift()!
  if (arg === '--seed') seedPath = args.shift() ?? fail('--seed needs a path')
  else if (arg === '--file') queryFile = args.shift() ?? fail('--file needs a path')
  else if (arg === '--keep') keep = true
  else queries.push(arg)
}
if (queryFile) {
  for (const line of readFileSync(queryFile, 'utf8').split('\n')) {
    const trimmed = line.trim()
    if (trimmed.length > 0 && !trimmed.startsWith('--')) queries.push(trimmed)
  }
}
if (queries.length === 0) fail('no query given; usage is at the top of this file')

const dockerHost = process.env.DOCKER_HOST ?? ''
const hostName = dockerHost.replace(/^ssh:\/\//, '').replace(/\/.*$/, '')
if (!hostName) fail('DOCKER_HOST must point at the shared Docker host (ssh://<host>)')

const DEFAULT_SEED = `
CREATE TABLE items (
  id INT PRIMARY KEY,
  status ENUM('pending','processing','shipped','delivered','cancelled') NOT NULL,
  label VARCHAR(32) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL,
  name VARCHAR(32) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci NOT NULL,
  total DECIMAL(10,2) NOT NULL,
  placed_on DATE NOT NULL
);
INSERT INTO items VALUES
  (1,'delivered','red','Ann',10.50,'2026-01-01'),
  (2,'pending','RED','ann',20.00,'2026-01-02'),
  (3,'processing','red ','ann ',1.25,'2026-01-03'),
  (4,'cancelled','blue','Bob',7.75,'2026-01-04'),
  (5,'shipped','BLUE','bob',3.10,'2026-01-05'),
  (6,'processing','green','Cara',9.99,'2026-01-06');
`

function run(cmd: string[], options: { env?: Record<string, string> } = {}) {
  const result = spawnSync(cmd, {
    env: { ...process.env, ...options.env },
    stdout: 'pipe',
    stderr: 'pipe',
  })
  return {
    ok: result.exitCode === 0,
    stdout: result.stdout.toString(),
    stderr: result.stderr.toString(),
  }
}

const container = `pintail-qcheck-${process.pid}`
const cleanup: Array<() => void> = []
async function teardown() {
  if (keep) {
    console.log(`qcheck: --keep set; container ${container} and local server left running`)
    return
  }
  for (const step of cleanup.reverse()) {
    try {
      step()
    } catch {}
  }
}

try {
  console.log('qcheck: building pintail')
  const build = run(['cargo', 'build', '-p', 'pintail'], {
    env: { CARGO_TARGET_DIR: 'target' },
  })
  if (!build.ok) fail(`cargo build failed:\n${build.stderr.slice(-2000)}`)

  console.log(`qcheck: starting MySQL on the docker host`)
  const started = run([
    'docker', 'run', '-d', '--name', container,
    '-e', 'MYSQL_ROOT_PASSWORD=root', '-e', 'MYSQL_DATABASE=qcheck',
    '-p', '0:3306', 'mysql:8.4',
    '--server-id=949', '--log-bin=mysql-bin', '--binlog-format=ROW',
    '--binlog-row-image=FULL', '--binlog-row-metadata=FULL',
    '--gtid-mode=ON', '--enforce-gtid-consistency=ON',
    '--default-time-zone=+00:00', '--sql-mode=NO_ENGINE_SUBSTITUTION',
  ])
  if (!started.ok) fail(`docker run failed: ${started.stderr}`)
  cleanup.push(() => run(['docker', 'rm', '-f', container]))
  const port = run(['docker', 'port', container, '3306'])
    .stdout.split('\n')[0]
    ?.split(':')
    .pop()
  if (!port) fail('could not read the published MySQL port')

  const mysql = (sql: string) =>
    run(['docker', 'exec', container, 'mysql', '-uroot', '-proot', '-N', 'qcheck', '-e', sql])
  for (let attempt = 0; ; attempt += 1) {
    if (run(['docker', 'exec', container, 'mysql', '-uroot', '-proot', '-e', 'SELECT 1']).ok) break
    if (attempt > 60) fail('MySQL did not become ready')
    await Bun.sleep(2000)
  }
  console.log('qcheck: seeding')
  const seedSql = seedPath ? readFileSync(seedPath, 'utf8') : DEFAULT_SEED
  const seeded = mysql(
    `CREATE USER IF NOT EXISTS 'pintail'@'%' IDENTIFIED BY 'pintail'; GRANT ALL ON *.* TO 'pintail'@'%'; ${seedSql}`,
  )
  if (!seeded.ok) fail(`seed failed: ${seeded.stderr.slice(-1000)}`)

  console.log('qcheck: starting pintail')
  const dataDir = mkdtempSync(join(tmpdir(), 'pintail-qcheck-'))
  const httpPort = 18140 + (process.pid % 100)
  const server = spawn(
    [
      join(repository, 'target', 'debug', 'pintail'),
      '--data-dir', dataDir,
      '--http-bind', `127.0.0.1:${httpPort}`,
      '--wire-bind', `127.0.0.1:${httpPort + 1}`,
    ],
    { cwd: repository, stdout: 'ignore', stderr: 'ignore' },
  )
  cleanup.push(() => server.kill())
  const api = `http://127.0.0.1:${httpPort}/api`
  for (let attempt = 0; ; attempt += 1) {
    try {
      if ((await fetch(`http://127.0.0.1:${httpPort}/health`)).ok) break
    } catch {}
    if (attempt > 60) fail('pintail did not become healthy')
    await Bun.sleep(500)
  }
  const call = async (path: string, body?: unknown, token?: string) => {
    const response = await fetch(`${api}${path}`, {
      method: body === undefined ? 'GET' : 'POST',
      headers: {
        'content-type': 'application/json',
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: body === undefined ? undefined : JSON.stringify(body),
    })
    if (!response.ok) fail(`${path} -> ${response.status}: ${await response.text()}`)
    return response.json() as Promise<Record<string, unknown>>
  }
  const setup = await call('/auth/setup', {
    email: 'qcheck@pintail.local',
    password: 'qcheck-password-1',
  })
  const token = setup.token as string
  const database = await call(
    '/databases',
    { name: 'qcheck', dsn: `mysql://pintail:pintail@${hostName}:${port}/qcheck`, mode: 'cdc' },
    token,
  )
  const id = database.id as string
  await call(`/databases/${id}/probe`, undefined, token)
  await call(`/databases/${id}/snapshot`, { force: false }, token)
  for (let attempt = 0; ; attempt += 1) {
    const status = await call(`/databases/${id}/snapshot/status`, undefined, token)
    if (status.state === 'streaming') break
    if (status.state === 'error') fail('snapshot failed')
    if (attempt > 120) fail('snapshot did not reach streaming')
    await Bun.sleep(1000)
  }

  let failures = 0
  for (const sql of queries) {
    const expected = mysql(sql)
    const pintailAnswer = await call('/query', { db: id, sql }, token)
    const pintailRows = (pintailAnswer.rows as unknown[][])
      .map((row) => row.map((value) => (value === null ? 'NULL' : String(value))).join('\t'))
      .join('\n')
    const mysqlRows = expected.ok ? expected.stdout.trimEnd() : `ERROR: ${expected.stderr.trim()}`
    if (mysqlRows === pintailRows) {
      console.log(`PASS  ${sql}`)
    } else {
      failures += 1
      console.log(`FAIL  ${sql}`)
      console.log(`  mysql:\n${mysqlRows.split('\n').map((l) => `    ${l}`).join('\n')}`)
      console.log(`  pintail:\n${pintailRows.split('\n').map((l) => `    ${l}`).join('\n')}`)
    }
  }
  await teardown()
  process.exit(failures === 0 ? 0 : 1)
} catch (error) {
  await teardown()
  fail(String(error))
}

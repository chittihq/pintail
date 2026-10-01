#!/usr/bin/env bun
/// Records a sampling profile of one statement with samply, as a file the
/// Firefox profiler opens offline.
///
///   bun run scripts/profile-samply.ts server --binary target/release/pintail \
///     --data-dir <replica dir> --db <database id> --email <login> \
///     --sql-file query.sql --out q.json.gz [--runs 5] [--pause-ms 300] [--rate 4000]
///   bun run scripts/profile-samply.ts shape <case> --out shape.json.gz [--runs 20]
///
/// `server` starts the binary on an existing data directory with the result
/// memo off, warms the statement, attaches samply to the process and runs
/// the statement `--runs` times with a pause between, so each execution is
/// a separate burst on the timeline. The password comes from
/// PINTAIL_PROFILE_PASSWORD. `<out>.statements.json` records when each
/// execution started and ended (epoch milliseconds). `shape` profiles one
/// case of benchmark/instruction-suite in process, fixture load included.
/// Linux only. See docs/instruction-counts.md.
import { readFileSync, writeFileSync } from 'node:fs'
import { join, resolve } from 'node:path'

const repository = resolve(import.meta.dir, '..')
const [mode, ...rest] = process.argv.slice(2)
const flags = new Map<string, string>()
const positional: string[] = []
for (let index = 0; index < rest.length; index += 1) {
  if (rest[index].startsWith('--')) flags.set(rest[index].slice(2), rest[(index += 1)])
  else positional.push(rest[index])
}
const need = (name: string) => {
  const value = flags.get(name)
  if (!value) throw new Error(`--${name} is required`)
  return value
}
// Thread timelines need context-switch records; presymbolication writes the
// symbols beside the profile so another machine can load it.
const samply = ['samply', 'record', '--save-only', '--unstable-presymbolicate', '--cswitch-markers', '--rate', flags.get('rate') ?? '4000']

if (mode === 'shape' && positional.length === 1) {
  const suite = join(repository, 'benchmark/instruction-suite')
  const build = Bun.spawn(['cargo', 'build', '--release', '--bin', 'answers'], { cwd: suite, stdout: 'inherit', stderr: 'inherit' })
  if ((await build.exited) !== 0) process.exit(1)
  const record = Bun.spawn(
    [...samply, '-o', resolve(need('out')), '--', join(suite, 'target/release/answers'), flags.get('runs') ?? '20', positional[0]],
    { env: { ...process.env, PINTAIL_DISABLE_SETTLED_MEMO: '1' }, stdout: 'inherit', stderr: 'inherit' },
  )
  process.exit(await record.exited)
} else if (mode === 'server') {
  const out = resolve(need('out'))
  const sql = readFileSync(need('sql-file'), 'utf8').trim()
  const port = Number(flags.get('port') ?? 18150)
  const base = `http://127.0.0.1:${port}`
  const server = Bun.spawn(
    [resolve(need('binary')), '--data-dir', need('data-dir'), '--http-bind', `127.0.0.1:${port}`, '--wire-bind', `127.0.0.1:${port + 1}`],
    { env: { ...process.env, PINTAIL_DISABLE_SETTLED_MEMO: '1' }, stdout: 'ignore', stderr: 'ignore' },
  )
  try {
    for (let attempt = 0; ; attempt += 1) {
      try {
        if ((await fetch(`${base}/health`)).ok) break
      } catch {}
      if (attempt > 480) throw new Error('the server did not become ready')
      await Bun.sleep(250)
    }
    const post = async (path: string, body: unknown, token?: string) => {
      const response = await fetch(`${base}${path}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', ...(token ? { Authorization: `Bearer ${token}` } : {}) },
        body: JSON.stringify(body),
      })
      if (!response.ok) throw new Error(`${path} returned ${response.status}: ${await response.text()}`)
      return response.json()
    }
    const password = process.env.PINTAIL_PROFILE_PASSWORD
    if (!password) throw new Error('PINTAIL_PROFILE_PASSWORD is required')
    const { token } = (await post('/api/auth/login', { email: need('email'), password })) as { token: string }
    const run = () => post('/api/query', { db: need('db'), sql }, token)
    for (let warm = 0; warm < 3; warm += 1) await run()
    const record = Bun.spawn([...samply, '-o', out, '-p', String(server.pid)], { stdout: 'inherit', stderr: 'inherit' })
    // samply needs a moment to attach to every thread.
    await Bun.sleep(2000)
    const pause = Number(flags.get('pause-ms') ?? 300)
    const statements: Array<{ start: number; end: number }> = []
    for (let index = 0; index < Number(flags.get('runs') ?? 5); index += 1) {
      await Bun.sleep(pause)
      const start = performance.timeOrigin + performance.now()
      await run()
      statements.push({ start, end: performance.timeOrigin + performance.now() })
    }
    await Bun.sleep(pause)
    record.kill('SIGINT')
    await record.exited
    writeFileSync(`${out}.statements.json`, JSON.stringify({ sql, statements }, null, 2))
  } finally {
    server.kill('SIGTERM')
    await server.exited
  }
} else {
  console.error('usage: profile-samply.ts server --binary .. --data-dir .. --db .. --email .. --sql-file .. --out .. | shape <case> --out ..')
  process.exit(2)
}

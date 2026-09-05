// Shared stateless process, connection, and comparison helpers for E2E gates.
import { createServer } from 'node:net'
import { resolve } from 'node:path'
import mysql from 'mysql2/promise'
const repository = resolve(import.meta.dir, '..', '..')

export async function command(args: string[], options: { cwd?: string; quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: options.cwd ?? repository,
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) {
    throw new Error(`${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`)
  }
  if (!options.quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim(), stderr: stderr.trim() }
}

export async function docker(...args: string[]) {
  return command(['docker', ...args], { quiet: true })
}

/// A host as it goes into a DSN: an IPv6 literal needs its brackets there.
export function dsnHost(host: string): string {
  return host.includes(':') ? `[${host}]` : host
}

export async function dockerHost(): Promise<string> {
  let endpoint = process.env.DOCKER_HOST?.trim()
  if (!endpoint) {
    const context = (await docker('context', 'show')).stdout
    endpoint = (
      await docker('context', 'inspect', context, '--format', '{{.Endpoints.docker.Host}}')
    ).stdout
  }
  if (!endpoint.startsWith('ssh://')) return '127.0.0.1'
  // URL parsing keeps an IPv6 literal (ssh://user@[fd7a::1]) intact.
  const target = new URL(endpoint).hostname.replace(/^\[|\]$/g, '')
  const ssh = await command(['ssh', '-G', target], { quiet: true })
  const hostname = ssh.stdout
    .split('\n')
    .find((line) => line.startsWith('hostname '))
    ?.slice('hostname '.length)
  if (!hostname) throw new Error(`could not resolve Docker SSH target ${target}`)
  return hostname
}

export async function publishedPort(name: string, containerPort: number): Promise<number> {
  const output = (await docker('port', name, `${containerPort}/tcp`)).stdout
  const match = output.split('\n')[0]?.match(/:(\d+)$/)
  if (!match) throw new Error(`Docker did not publish ${name}:${containerPort}`)
  return Number(match[1])
}

export async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer()
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      if (!address || typeof address === 'string') {
        server.close()
        reject(new Error('could not allocate a local port'))
        return
      }
      server.close((error) => (error ? reject(error) : resolvePort(address.port)))
    })
  })
}

export async function waitForMysql(host: string, port: number, attempts = 240) {
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      const connection = await mysql.createConnection({
        host,
        port,
        user: 'root',
        password: 'pintail-root',
        multipleStatements: true,
        supportBigNumbers: true,
        bigNumberStrings: true,
        dateStrings: true,
        enableKeepAlive: true,
        keepAliveInitialDelay: 10_000,
      })
      await connection.query('SELECT 1')
      return connection
    } catch {
      await Bun.sleep(500)
    }
  }
  throw new Error('MySQL did not become ready in time')
}

export function canonicalValue(value: unknown): string {
  if (value === null || value === undefined) return 'NULL'
  if (Buffer.isBuffer(value)) {
    // Text arrives as Buffer from the pintail wire (charset-33 workaround
    // above) and binary arrives as Buffer from MySQL: valid UTF-8 compares
    // as text, anything else as hex, identically on both sides.
    const text = value.toString('utf8')
    if (Buffer.compare(Buffer.from(text, 'utf8'), value) === 0) {
      return canonicalValue(text)
    }
    return `0x${value.toString('hex')}`
  }
  if (value instanceof Date) return canonicalValue(value.toISOString())
  if (typeof value === 'object') return canonicalJson(value)
  let text = String(value)
  // JSON documents arrive as text from one side and objects from the other.
  if (text.startsWith('{') || text.startsWith('[')) {
    try {
      return canonicalJson(JSON.parse(text))
    } catch {}
  }
  // Temporal: unify 'T' separators, drop timezone suffix, trim only the
  // fractional-second zeros (never the seconds themselves).
  if (/^\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}/.test(text)) {
    text = text.replace('T', ' ').replace(/(Z|[+-]\d{2}:?\d{2})$/, '')
    text = text.replace(/(\.\d*?)0+$/, '$1').replace(/\.$/, '')
    return text
  }
  // Numeric: fixed exponent-free form, 4 decimal places like the benchmark.
  if (text !== '' && /^-?\d+(\.\d+)?$/.test(text)) {
    // u64 values above 2^53 lose precision through Number; keep integers
    // longer than 15 digits as exact strings.
    if (/^-?\d+$/.test(text) && text.replace('-', '').length > 15) return text
    return Number(text).toFixed(4)
  }
  return text
}

export function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(',')}]`
  if (value !== null && typeof value === 'object') {
    const entries = Object.entries(value as Record<string, unknown>).sort(([a], [b]) =>
      a < b ? -1 : a > b ? 1 : 0,
    )
    return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(',')}}`
  }
  return JSON.stringify(canonicalValue(value))
}

export function canonicalRow(row: unknown[], csvColumns?: number[]): string {
  return row
    .map((value, index) => {
      let canonical = canonicalValue(value)
      if (csvColumns?.includes(index)) {
        canonical = canonical.split(',').sort().join(',')
      }
      return canonical
    })
    .join('')
}

export function diffRows(
  expected: unknown[][],
  actual: unknown[][],
  options: { multiset?: boolean; csvColumns?: number[] } = {},
): string | undefined {
  let left = expected.map((row) => canonicalRow(row, options.csvColumns))
  let right = actual.map((row) => canonicalRow(row, options.csvColumns))
  if (options.multiset) {
    left = [...left].sort()
    right = [...right].sort()
  }
  if (left.length !== right.length) {
    return `row count ${expected.length} vs ${actual.length}`
  }
  for (let index = 0; index < left.length; index += 1) {
    if (left[index] !== right[index]) {
      return `row ${index}:\n  mysql   ${left[index].replaceAll('', ' | ')}\n  pintail ${right[index].replaceAll('', ' | ')}`
    }
  }
  return undefined
}


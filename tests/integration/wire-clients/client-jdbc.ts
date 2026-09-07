import { createHash } from 'node:crypto'
import { mkdir } from 'node:fs/promises'
import { join } from 'node:path'

const version = '9.6.0'
const sha256 = '66df1d453789dc8cb759a7dc17f58646893bf28483f262328650f170472a6ead'
const cache = join(import.meta.dir, '.cache')
await mkdir(cache, { recursive: true })
const jar = join(cache, `mysql-connector-j-${version}.jar`)
let bytes: Uint8Array
if (await Bun.file(jar).exists()) {
  bytes = new Uint8Array(await Bun.file(jar).arrayBuffer())
} else {
  const response = await fetch(`https://repo.maven.apache.org/maven2/com/mysql/mysql-connector-j/${version}/mysql-connector-j-${version}.jar`)
  if (!response.ok) throw new Error(`JDBC download failed: ${response.status}`)
  bytes = new Uint8Array(await response.arrayBuffer())
}
if (createHash('sha256').update(bytes).digest('hex') !== sha256) throw new Error('JDBC checksum mismatch')
await Bun.write(jar, bytes)
const child = Bun.spawn([process.env.PINTAIL_JAVA ?? 'java', '--class-path', jar, 'Client.java'], {
  cwd: import.meta.dir, env: process.env, stdout: 'inherit', stderr: 'inherit',
})
process.exit(await child.exited)

import { expect, test } from 'bun:test'
import { createConnection, createServer } from 'node:net'
import { SourceProxy } from './proxy'

test('source proxy forwards packets, cuts a named query and restores connections', async () => {
  const received: Buffer[] = []
  const server = createServer(socket => { socket.on('data', data => { received.push(Buffer.isBuffer(data) ? data : Buffer.from(data)); socket.write(data) }); socket.on('error', () => {}) })
  await new Promise<void>(resolve => server.listen(0, '127.0.0.1', resolve))
  const proxy = new SourceProxy('127.0.0.1', (server.address() as {port:number}).port)
  await proxy.start()
  const connect = () => createConnection({host:'127.0.0.1',port:proxy.localPort})
  const packet = (sql:string) => { const body=Buffer.from(`\x03${sql}`); const header=Buffer.alloc(4); header.writeUIntLE(body.length,0,3); return Buffer.concat([header,body]) }
  const client = connect(); client.on('error', () => {})
  try {
    proxy.cutOnQuery(/^SELECT id FROM repair$/)
    const normal = packet('SELECT 1')
    const echoed = new Promise<Buffer>(resolve => client.once('data', resolve))
    client.write(normal)
    expect(await echoed).toEqual(normal)
    const cut = packet('SELECT id FROM repair')
    const closed = new Promise<void>(resolve => client.once('close', () => resolve()))
    client.write(cut.subarray(0, 2)); client.write(cut.subarray(2))
    await closed
    expect(proxy.blocked).toBe(true)
    expect(proxy.lastCutQuery).toBe('SELECT id FROM repair')
    expect(Buffer.concat(received).includes(Buffer.from('SELECT id FROM repair'))).toBe(false)
    proxy.restore()
    const retry = connect(); retry.on('error', () => {})
    try {
      const reply = new Promise<Buffer>(resolve => retry.once('data', resolve))
      retry.write(normal); expect(await reply).toEqual(normal)
    } finally { retry.destroy() }
  } finally {
    client.destroy(); await proxy.close(); await new Promise<void>(resolve => server.close(() => resolve()))
  }
}, 10_000)

test('source proxy holds a page until the concurrent mutation is released', async () => {
  const received: Buffer[] = []
  const server=createServer(socket=>{socket.on('data',data=>{const bytes=Buffer.isBuffer(data)?data:Buffer.from(data);received.push(bytes);socket.write(bytes)});socket.on('error',()=>{})})
  await new Promise<void>(resolve=>server.listen(0,'127.0.0.1',resolve))
  const proxy=new SourceProxy('127.0.0.1',(server.address() as {port:number}).port)
  await proxy.start()
  const client=createConnection({host:'127.0.0.1',port:proxy.localPort});client.on('error',()=>{})
  try {
    proxy.holdOnQuery(/OFFSET 10000$/)
    const body=Buffer.from('\x03SELECT id FROM accounts LIMIT 10000 OFFSET 10000'),header=Buffer.alloc(4)
    header.writeUIntLE(body.length,0,3);const packet=Buffer.concat([header,body])
    const echoed=new Promise<Buffer>(resolve=>client.once('data',resolve))
    client.write(packet)
    const deadline=Date.now()+1000
    while(!proxy.heldQuery&&Date.now()<deadline) await Bun.sleep(5)
    expect(proxy.heldQuery).toContain('OFFSET 10000')
    expect(received.length).toBe(0)
    proxy.releaseQuery()
    expect(await echoed).toEqual(packet)
  } finally {client.destroy();await proxy.close();await new Promise<void>(resolve=>server.close(()=>resolve()))}
}, 10_000)

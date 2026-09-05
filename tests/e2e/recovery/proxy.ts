import { createConnection, createServer, type Socket } from 'node:net'
/** A fault affects only the victim's replication connections; writers and bystanders remain live. */
export class SourceProxy {
  private server = createServer(client => {
    if (this.blocked) { client.destroy(); return }
    const upstream = createConnection({ host: this.host, port: this.port })
    this.sockets.add(client); this.sockets.add(upstream)
    const dispose = () => { client.destroy(); upstream.destroy(); this.sockets.delete(client); this.sockets.delete(upstream) }
    client.on('error', dispose); upstream.on('error', dispose)
    client.on('close', dispose); upstream.on('close', dispose)
    let pending = Buffer.alloc(0)
    let held: Buffer[] | undefined
    const forward = (data: Buffer) => { if (!upstream.write(data)) client.pause() }
    upstream.on('drain', () => client.resume())
    client.on('data', data => {
      const bytes = Buffer.isBuffer(data) ? data : Buffer.from(data)
      if (held) { held.push(bytes); return }
      pending = Buffer.concat([pending, bytes])
      while (pending.length >= 4) {
        const length = pending.readUIntLE(0, 3)
        if (pending.length < length + 4) break
        const packet = pending.subarray(4, length + 4)
        pending = pending.subarray(length + 4)
        // COM_QUERY or COM_STMT_PREPARE. This fixture uses an unencrypted
        // MySQL connection; authentication packets are never recorded.
        if (packet[0] === 3 || packet[0] === 22) {
          const query = packet.subarray(1).toString('utf8')
          if (this.queryFault?.test(query)) {
            this.lastCutQuery = query; this.queryFault = undefined
            this.cut(); return
          }
          if (this.queryHold?.test(query)) {
            this.heldQuery = query; this.queryHold = undefined; held = [bytes]
            this.releaseHeld = () => { const chunks = held ?? []; held = undefined; for (const chunk of chunks) forward(chunk) }
            return
          }
        }
      }
      forward(bytes)
    })
    client.on('end', () => upstream.end()); upstream.pipe(client)
  })
  private sockets = new Set<Socket>()
  blocked = false
  localPort = 0
  private queryFault?: RegExp
  private queryHold?: RegExp
  private releaseHeld?: () => void
  lastCutQuery = ''
  heldQuery = ''
  constructor(private host: string, private port: number) {}
  async start() {
    await new Promise<void>((resolve, reject) => { this.server.once('error', reject); this.server.listen(0, '127.0.0.1', resolve) })
    this.localPort = (this.server.address() as { port: number }).port
  }
  cut() { this.blocked = true; for (const socket of this.sockets) socket.destroy(); this.sockets.clear() }
  cutOnQuery(pattern: RegExp) { this.lastCutQuery = ''; this.queryFault = pattern }
  holdOnQuery(pattern: RegExp) { this.heldQuery = ''; this.queryHold = pattern }
  releaseQuery() { this.releaseHeld?.(); this.releaseHeld = undefined }
  restore() { this.blocked = false }
  async close() { this.cut(); await new Promise<void>(resolve => this.server.close(() => resolve())) }
}

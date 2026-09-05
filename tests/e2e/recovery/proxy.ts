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
    client.pipe(upstream); upstream.pipe(client)
  })
  private sockets = new Set<Socket>()
  blocked = false
  localPort = 0
  constructor(private host: string, private port: number) {}
  async start() {
    await new Promise<void>((resolve, reject) => { this.server.once('error', reject); this.server.listen(0, '127.0.0.1', resolve) })
    this.localPort = (this.server.address() as { port: number }).port
  }
  cut() { this.blocked = true; for (const socket of this.sockets) socket.destroy(); this.sockets.clear() }
  restore() { this.blocked = false }
  async close() { this.cut(); await new Promise<void>(resolve => this.server.close(() => resolve())) }
}

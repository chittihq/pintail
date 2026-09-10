// Isolated dashboard regressions: generated assets, synthetic API, no Docker.
// Run after generating the dashboard: bun run dashboard
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { readFile, stat } from 'node:fs/promises'
import { join, resolve } from 'node:path'
import { chromium } from 'playwright'

const repository = resolve(import.meta.dir, '../..')
const publicDir = join(repository, 'packages/dashboard/.output/public')
const tooltipBuild = await Bun.build({
  entrypoints: [join(repository, 'packages/dashboard/tests/chart-tooltip.ts')],
  target: 'browser',
})
assert(tooltipBuild.success, String(tooltipBuild.logs))
const tooltipScript = await tooltipBuild.outputs[0]!.text()
const database = {
  id: 'test-db', name: 'Test database', mode: 'cdc', effective_mode: 'cdc',
  state: 'streaming', include_tables: [], exclude_tables: [],
  poll_interval_seconds: 60, reconcile_interval_seconds: 3600,
  created_at: '2026-01-01', updated_at: '2026-01-01',
}
let resultRows = 10_000
const server = createServer(async (request, response) => {
  const path = new URL(request.url!, 'http://localhost').pathname
  const json = (value: unknown) => {
    response.setHeader('Content-Type', 'application/json')
    response.end(JSON.stringify(value))
  }
  if (path === '/tooltip-test.js') {
    response.setHeader('Content-Type', 'text/javascript')
    response.end(tooltipScript)
    return
  }
  if (path === '/api/events' || path === '/api/vitals') {
    response.writeHead(200, { 'Content-Type': 'text/event-stream' })
    response.write(': connected\n\n')
    return
  }
  if (path === '/status') return json({ status: 'ok', version: 'test', wire: { enabled: false } })
  if (path === '/api/session') return json({ subject: 'test', role: 'admin', workspace_id: 'test', scopes: [] })
  if (path === '/api/workspaces') return json([{ id: 'test', name: 'Test', slug: 'test', role: 'admin' }])
  if (path === '/api/auth/setup/status') return json({ required: false })
  if (path === '/api/auth/google/status') return json({ enabled: false })
  if (path === '/api/storage') return json(null)
  if (path === '/api/databases') return json([database])
  if (path === '/api/databases/test-db/status') return json({ database, tables: 1, rows: resultRows })
  if (path === '/api/tables/columns') return json({ tables: { events: ['id', 'value'] } })
  if (path === '/api/query') return json({
    fields: [{ name: 'id', data_type: 'Int64' }, { name: 'value', data_type: 'String' }],
    rows: Array.from({ length: resultRows }, (_, id) => [id, `Synthetic value ${id}`]),
    stats: { rows: resultRows, duration_ms: 10, blocks_read: 1, blocks_pruned: 0 },
  })
  if (path.startsWith('/api/')) return json([])
  try {
    let file = join(publicDir, path)
    try {
      if ((await stat(file)).isDirectory()) file = join(file, 'index.html')
    } catch {
      file = join(publicDir, 'index.html')
    }
    response.setHeader('Content-Type', file.endsWith('.js') ? 'text/javascript'
      : file.endsWith('.css') ? 'text/css' : file.endsWith('.json') ? 'application/json'
        : file.endsWith('.html') ? 'text/html' : 'application/octet-stream')
    response.end(await readFile(file))
  } catch {
    response.writeHead(404).end()
  }
})
await new Promise<void>(done => server.listen(0, '127.0.0.1', done))
const address = server.address()
assert(address && typeof address !== 'string')
const browser = await chromium.launch({ headless: true })
try {
  const page = await browser.newPage()
  const errors: string[] = []
  page.on('pageerror', error => errors.push(error.message))
  await page.addInitScript(() => localStorage.setItem('pintail.token', 'test'))
  await page.goto(`http://127.0.0.1:${address.port}/sql`)
  const lifecycle = await page.evaluate(async () => {
    // The fixture bundles the real helper and Vue into a browser module.
    const fixture = await import('/tooltip-test.js')
    return fixture.checkTooltipLifecycle()
  })
  assert.equal(lifecycle.mounted, 201)
  assert.equal(lifecycle.unmounted, lifecycle.mounted, 'every detached tooltip must unmount')
  assert.equal(lifecycle.latest, '<span>199:999</span>', 'changed x must not reuse stale HTML')
  console.log('PASS: tooltip renders release component lifecycles and keep labels current')

  await page.getByRole('button', { name: 'Run', exact: true }).click()
  await page.getByText('Synthetic value 0', { exact: true }).waitFor()
  const renderedRows = page.locator('tbody tr')
  assert.equal(await renderedRows.count(), 100, 'large SQL results must mount only one page')
  const cdp = await page.context().newCDPSession(page)
  await cdp.send('HeapProfiler.collectGarbage')
  const heap = await cdp.send('Runtime.getHeapUsage')
  const dom = await cdp.send('Memory.getDOMCounters')
  assert(dom.nodes < 10_000, 'offscreen result cells must not accumulate DOM nodes')
  console.log(`SQL result: ${(heap.usedSize / 1e6).toFixed(1)} MB JS heap, ${dom.nodes} DOM nodes`)
  await page.getByRole('button', { name: 'Last page', exact: true }).click()
  await page.getByText('Synthetic value 9999', { exact: true }).waitFor()
  assert.equal(await renderedRows.count(), 100)
  await page.getByRole('button', { name: 'Previous page', exact: true }).click()
  await page.getByText('Synthetic value 9800', { exact: true }).waitFor()
  await page.getByRole('button', { name: 'First page', exact: true }).click()
  await page.getByRole('button', { name: 'Next page', exact: true }).click()
  await page.getByText('Synthetic value 100', { exact: true }).waitFor()
  // A smaller new result must reset the page, not appear empty on the old page.
  resultRows = 3
  await page.getByRole('button', { name: 'Run', exact: true }).click()
  await page.getByText('3 rows ·', { exact: false }).waitFor()
  assert.equal(await renderedRows.count(), 3)
  assert(await page.getByRole('button', { name: 'Next page', exact: true }).isDisabled())
  resultRows = 0
  await page.getByRole('button', { name: 'Run', exact: true }).click()
  await page.getByText('0 rows ·', { exact: false }).waitFor()
  assert.equal(await renderedRows.count(), 0)
  assert(await page.getByRole('button', { name: 'Last page', exact: true }).isDisabled())
  console.log('PASS: bounded SQL rows, page navigation, result replacement and empty results')

  assert.deepEqual(errors, [])
} finally {
  await browser.close()
  server.closeAllConnections()
  await new Promise<void>(done => server.close(() => done()))
}

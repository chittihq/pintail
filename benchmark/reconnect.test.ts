import { expect, test } from 'bun:test'
import { waitForPublishedEndpoint } from './reconnect'

test('reconnects through the new Docker port and preserves IPv6', async () => {
  const probes: string[] = []
  const endpoint = await waitForPublishedEndpoint(
    'http://[::1]:31000',
    async () => 32000,
    async (url) => { probes.push(url); return true },
  )
  expect(endpoint).toBe('http://[::1]:32000')
  expect(probes).toEqual(['http://[::1]:32000'])
})

test('resolves the mapping again after missing ports and failed readiness', async () => {
  let mappings = 0
  const probes: string[] = []
  const endpoint = await waitForPublishedEndpoint(
    'http://localhost:31000',
    async () => {
      mappings++
      if (mappings === 1) throw new Error('no mapping during restart')
      return 32000 + mappings
    },
    async (url) => {
      probes.push(url)
      if (probes.length === 1) throw new Error('connection refused')
      return probes.length === 3
    },
    { timeoutMs: 1_000, retryMs: 1 },
  )
  expect(mappings).toBe(4)
  expect(probes).toEqual([
    'http://localhost:32002', 'http://localhost:32003', 'http://localhost:32004',
  ])
  expect(endpoint).toBe('http://localhost:32004')
})

test('does not return a stale or unready endpoint after the deadline', async () => {
  await expect(waitForPublishedEndpoint(
    'http://localhost:31000',
    async () => 32000,
    async () => false,
    { timeoutMs: 10, retryMs: 1 },
  )).rejects.toThrow('Timed out waiting')
})

test('aborts a stalled Docker lookup at the deadline', async () => {
  let lookupSignal: AbortSignal | undefined
  let probes = 0
  await expect(waitForPublishedEndpoint(
    'http://localhost:31000',
    async (signal) => {
      lookupSignal = signal
      return new Promise<number>(() => {})
    },
    async () => { probes++; return true },
    { timeoutMs: 10 },
  )).rejects.toThrow('Timed out waiting')
  expect(lookupSignal?.aborted).toBe(true)
  expect(probes).toBe(0)
})

test('rejects readiness that completes after the deadline', async () => {
  let finish: (ready: boolean) => void = () => {}
  const result = waitForPublishedEndpoint(
    'http://localhost:31000',
    async () => 32000,
    () => new Promise<boolean>((resolve) => { finish = resolve }),
    { timeoutMs: 10 },
  )
  await expect(result).rejects.toThrow('Timed out waiting')
  finish(true)
  await expect(result).rejects.toThrow('Timed out waiting')
})

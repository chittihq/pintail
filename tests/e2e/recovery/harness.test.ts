import { expect, test } from 'bun:test'
import { until } from './harness'

test('state waits enforce their deadline even when an operation never returns', async () => {
  await expect(until('stalled operation', () => new Promise<boolean>(() => {}), 20)).rejects.toThrow('exceeded deadline')
})

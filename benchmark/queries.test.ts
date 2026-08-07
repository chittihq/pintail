import { expect, test } from 'bun:test'
import { benchmarkQueries } from './queries'

test('cold benchmark queries use five distinct memo-cold variants', () => {
  const cold = benchmarkQueries.filter((query) => query.coldOnly)
  expect(cold).toHaveLength(4)
  for (const query of cold) {
    expect(query.coldVariants).toHaveLength(5)
    expect(new Set(query.coldVariants?.map((variant) => variant.sql)).size).toBe(5)
  }
})

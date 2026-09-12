import { expect, test } from 'bun:test'
import { skew } from './version-skew.ts'

test('statements split by which major Pintail matches', () => {
  const rows = skew(
    { oracle: '8.4', files: { a: ['1', '2', '3'], b: ['9'] } },
    { oracle: '8.0', files: { a: ['1', '4'], b: ['9'], c: ['7'] } },
  )
  expect(rows).toEqual([
    { file: 'a', both: 1, newerOnly: 2, olderOnly: 1 },
    { file: 'c', both: 0, newerOnly: 0, olderOnly: 1 },
  ])
})

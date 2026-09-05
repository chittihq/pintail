import { expect, test } from 'bun:test'
import { assertAutomaticRequest, exactDiff, gtidContains, selected } from './policy'

test('automatic requests reject repair routes including encoded paths and force defaults', () => {
  for (const path of ['/api/databases/d/resync', '/api/databases/d/tables/t/reconcile/', '/api/databases/d/reset?x=1', '/api/dlq/e/retry', '/api/databases/d/mode', '/api/databases/d/%72esync']) {
    expect(() => assertAutomaticRequest(path, {})).toThrow('forbidden')
  }
  expect(() => assertAutomaticRequest('/api/databases/d/snapshot', {})).toThrow()
  expect(() => assertAutomaticRequest('/api/databases/d/snapshot', { force: true })).toThrow()
  expect(() => assertAutomaticRequest('/api/databases/d/snapshot', { force: false })).not.toThrow()
  expect(() => assertAutomaticRequest('/api/databases/d/mode', {}, true)).not.toThrow()
})
test('exact comparator detects duplicate loss, small decimal changes and ambiguous delimiters', () => {
  expect(exactDiff([[1], [1]], [[1]], true)).toBeDefined()
  expect(exactDiff([['1.00001']], [['1.00002']])).toBeDefined()
  expect(exactDiff([[null]], [['NULL']])).toBeDefined()
  expect(exactDiff([['a\x01b', 'c']], [['a', 'b\x01c']])).toBeDefined()
  expect(exactDiff([[2], [1]], [['1'], ['2']], true)).toBeUndefined()
})
test('scenario selectors are anchored glob unions', () => {
  expect(selected('poll-a', ['cdc-*', 'poll-*'])).toBe(true)
  expect(selected('baseline', ['poll-*'])).toBe(false)
})
test('GTID membership handles holes, multiple sources and integers beyond 2^53', () => {
  expect(gtidContains('a:1-3:7-9,b:9007199254740992-9007199254740994', 'a:4')).toBe(false)
  expect(gtidContains('a:1-3:7-9,b:9007199254740992-9007199254740994', 'b:9007199254740993')).toBe(true)
})

import { expect, test } from 'bun:test'
import { cell, rows } from './parity-support'

test('NULL, text, arbitrary bytes and numeric precision remain distinct', () => {
  expect(cell(null)).not.toEqual(cell('NULL'))
  expect(cell(Buffer.from([255]))).not.toEqual(cell(Buffer.from([254])))
  expect(cell('9007199254740993')).not.toEqual(cell('9007199254740992'))
  expect(cell('1.00')).not.toEqual(cell(1))
  expect(cell('a\tb\nc\0')).toEqual(cell(Buffer.from('a\tb\nc\0')))
})
test('bag comparison preserves duplicates without imposing order', () => {
  expect(rows([[null], ['NULL']], false)).toEqual(rows([['NULL'], [null]], false))
  expect(rows([[null], [null]], false)).not.toEqual(rows([[null], ['NULL']], false))
  expect(rows([[1], [2]], true)).not.toEqual(rows([[2], [1]], true))
})

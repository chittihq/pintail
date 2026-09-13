import { expect, test } from 'bun:test'
import { hasOuterOrderBy, unorderedLimit, withoutLiterals } from './sql-reading.ts'

test('a literal, an identifier and a comment name no syntax', () => {
  expect(hasOuterOrderBy("SELECT 'order by x' FROM t")).toBe(false)
  expect(hasOuterOrderBy('SELECT `order by` FROM t')).toBe(false)
  expect(hasOuterOrderBy('SELECT a FROM t -- order by a')).toBe(false)
  expect(hasOuterOrderBy('SELECT a FROM t /* order by a */')).toBe(false)
  expect(hasOuterOrderBy("SELECT 'x' FROM t ORDER BY a")).toBe(true)
  // A doubled quote stays inside the literal.
  expect(withoutLiterals("SELECT 'it''s order by' , a")).not.toContain('order')
})

test('the words of ORDER BY may be spaced however the author liked', () => {
  expect(hasOuterOrderBy('SELECT a FROM t ORDER  BY a')).toBe(true)
  expect(hasOuterOrderBy('SELECT a FROM t ORDER\n   BY a')).toBe(true)
  // ORDER BY NULL asks for no order at all.
  expect(hasOuterOrderBy('SELECT a FROM t GROUP BY a ORDER BY NULL')).toBe(false)
})

test('a LIMIT is decided by an ORDER BY at its own level', () => {
  expect(unorderedLimit('SELECT a FROM t LIMIT 3')).toBe(true)
  expect(unorderedLimit('SELECT a FROM t ORDER BY a LIMIT 3')).toBe(false)
  // The outer ORDER BY does not decide which rows the derived table kept.
  expect(unorderedLimit('SELECT * FROM (SELECT a FROM t LIMIT 3) d ORDER BY a')).toBe(true)
  expect(unorderedLimit('SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) d ORDER BY a')).toBe(false)
  // A LIMIT written inside a string is not a LIMIT.
  expect(unorderedLimit("SELECT 'limit 3' FROM t ORDER BY a")).toBe(false)
})

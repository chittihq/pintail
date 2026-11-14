import { expect, test } from 'bun:test'
import { hasOuterOrderBy, SessionDrift, sessionKeys, unorderedLimit, withoutLiterals } from './sql-reading.ts'

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

test('a session statement names the state it sets', () => {
  expect(sessionKeys('SET NAMES gbk')).toEqual(['names'])
  expect(sessionKeys('set character set latin1')).toEqual(['names'])
  expect(sessionKeys('SET collation_connection = gbk_bin, character_set_results = gbk')).toEqual(['names'])
  expect(sessionKeys('USE other')).toEqual(['use'])
  expect(sessionKeys("SET SESSION sql_mode = 'ANSI,TRADITIONAL', @@session.time_zone = '+00:00'")).toEqual(['sql_mode', 'time_zone'])
  expect(sessionKeys("SET @a = CONCAT('x,y', 1), @b := 2")).toEqual(['@a', '@b'])
  expect(sessionKeys('GET DIAGNOSTICS @n = NUMBER')).toEqual([])
  // State that changes no answer is not tracked.
  expect(sessionKeys("SET optimizer_switch = 'semijoin=off', sort_buffer_size = 32768")).toEqual([])
  expect(sessionKeys('SET character_set_database = gbk, @@lock_wait_timeout = 1')).toEqual([])
})

test('sessions stay apart from a refusal until both sides set that state again', () => {
  const drift = new SessionDrift()
  expect(drift.reaches('SELECT 1')).toBeUndefined()
  // Both ran, or neither: the sessions agree.
  drift.note('SET NAMES utf8mb4', true, true)
  drift.note('SET NAMES nothing', false, false)
  expect(drift.reaches('SELECT 1')).toBeUndefined()
  drift.note('SET NAMES gbk', true, false)
  expect(drift.reaches('SELECT 1')).toBe('names')
  // Another state set on both sides does not bring this one back.
  drift.note("SET sql_mode = ''", true, true)
  expect(drift.reaches('SELECT 1')).toBe('names')
  // A second refusal of the same state changes nothing; both running does.
  drift.note('SET NAMES gb2312', true, false)
  drift.note('SET character_set_results = latin1', true, true)
  expect(drift.reaches('SELECT 1')).toBeUndefined()
})

test('a user variable reaches only the statements that name it', () => {
  const drift = new SessionDrift()
  drift.note('SET @total = (SELECT 1)', true, false)
  expect(drift.reaches('SELECT a FROM t')).toBeUndefined()
  expect(drift.reaches('SELECT @totals')).toBeUndefined()
  expect(drift.reaches('SELECT a + @Total FROM t')).toBe('@total')
  drift.note('SET @total = 2', true, true)
  expect(drift.reaches('SELECT @total')).toBeUndefined()
})

import { expect, test } from 'bun:test'
import { bugsByFile, coverage } from './regressions.ts'
import type { CommitRecord } from './atlas.ts'

const record = (ids: string[], classes: string[], tests: string[]): CommitRecord => ({
  project: 'mysql', sha: 'x', date: '2020-01-01', ids, classes, tests, paths: tests.length,
})

test('only SQL-class fixes that changed a main-suite file count, once per bug and file', () => {
  const { files, classes } = bugsByFile('mysql', [
    record(['1'], ['decimal'], ['mysql-test/t/type_decimal.test', 'mysql-test/r/type_decimal.result']),
    record(['1'], ['decimal'], ['mysql-test/t/type_decimal.test']),
    record(['2'], ['replication-apply'], ['mysql-test/t/type_decimal.test']),
    record(['3'], ['json'], ['mysql-test/suite/json/t/json.test']),
    record(['4'], ['json', 'aggregation'], ['mysql-test/t/json.test']),
  ])
  expect([...files.entries()].map(([f, ids]) => [f, [...ids]])).toEqual([['type_decimal', ['1']], ['json', ['4']]])
  expect(classes.get('aggregation')?.size).toBe(1)
})

test('coverage joins replay results and ranks by bugs', () => {
  const files = new Map([['a', new Set(['1'])], ['b', new Set(['2', '3'])]])
  const rows = coverage(files, [{ file: 'a', statements: 3, counts: { exact: 2, mismatch: 1, 'pintail-error': 4 } }])
  expect(rows).toEqual([
    { file: 'b', bugs: 2, replayed: false, exact: 0, compared: 0, errors: 0 },
    { file: 'a', bugs: 1, replayed: true, exact: 2, compared: 3, errors: 4 },
  ])
})

import { expect, test } from 'bun:test'
import { CLASSES, bugIds, classify, testPaths } from './taxonomy.ts'
import { changelogBugFixes, summarize, type CommitRecord } from './atlas.ts'

test('class ids are unique and every in-scope class names its rules', () => {
  const ids = CLASSES.map((c) => c.id)
  expect(new Set(ids).size).toBe(ids.length)
  for (const cls of CLASSES) expect(Object.keys(cls.rules).length).toBeGreaterThan(0)
})

test('paths decide the class, and relevant work is never also out of scope', () => {
  expect(classify('mysql', ['strings/decimal.cc', 'mysql-test/t/type_newdecimal.test'])).toEqual(['decimal'])
  expect(classify('mariadb', ['sql/log_event.cc', 'storage/innobase/row/row0mysql.cc'])).toEqual(['binlog-events'])
  expect(classify('mysql', ['storage/innobase/btr/btr0cur.cc'])).toEqual(['out-of-scope'])
  expect(classify('clickhouse', ['src/Storages/MergeTree/MergeTask.cpp'])).toEqual(['columnar-merge'])
  expect(classify('clickhouse', ['src/Server/MySQLHandler.cpp'])).toEqual(['wire-protocol'])
  expect(classify('mysql', ['README'])).toEqual([])
})

test('bug identifiers and test paths follow each project convention', () => {
  expect(bugIds('mysql', 'Bug#12345678 wrong result\n\nAlso Bug #12345678 and BUG#9876543')).toEqual(['12345678', '9876543'])
  expect(bugIds('mariadb', 'MDEV-30000 MDEV-30000 fix; see MDEV-123')).toEqual(['30000', '123'])
  expect(testPaths('mysql', ['mysql-test/t/json.test', 'mysql-test/suite/rpl/r/x.result', 'sql/item.cc'])).toHaveLength(2)
  expect(testPaths('mariadb', ['mysql-test/main/derived.test', 'mysql-test/suite/rpl/t/a.test'])).toHaveLength(2)
  expect(testPaths('clickhouse', ['tests/queries/0_stateless/01234_x.sql', 'tests/integration/a.py'])).toHaveLength(1)
})

test('only pull requests under a bug-fix heading count as fixes', () => {
  const log = '#### New Feature\n* thing [#100](https://x/pull/100)\n#### Bug Fix (user-visible misbehavior)\n* fix (#2001). * other [#2002](https://x/pull/2002)\n#### Performance\n* fast #3000'
  expect([...changelogBugFixes(log)].sort()).toEqual([2001, 2002])
})

test('a bug fixed by several commits counts once under the union of their classes', () => {
  const rec = (sha: string, date: string, classes: string[], tests: string[]): CommitRecord => ({ project: 'mysql', sha, date, ids: ['7'], classes, tests, paths: 1 })
  const s = summarize([rec('a', '2012-01-01', ['decimal'], []), rec('b', '2019-01-01', ['temporal'], ['mysql-test/t/x.test'])])
  expect(s.total).toBe(1)
  expect(s.perClass.get('decimal')).toMatchObject({ bugs: 1, tested: 1 })
  expect(s.perClass.get('temporal')!.byEra.get('2010')).toBe(1)
})

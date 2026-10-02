import { describe, expect, test } from 'bun:test'
import { bagEqual, explain, membersSorted, orderOf, spellingFolded, tiebroken, topK, validTopK, type Rows } from './corpus-compare.ts'

describe('topK', () => {
  test('adds the tie group and drops the LIMIT', () => {
    expect(topK('SELECT id, name FROM items WHERE id >= 1 ORDER BY name DESC LIMIT 2')).toEqual({
      ranked:
        'SELECT id, name, DENSE_RANK() OVER (ORDER BY name DESC) AS corpus_tie_group FROM items WHERE id >= 1 ORDER BY name DESC',
      offset: 0,
      count: 2,
    })
  })

  test('reads both spellings of an offset', () => {
    expect(topK('SELECT id FROM items ORDER BY price LIMIT 5, 3')).toMatchObject({ offset: 5, count: 3 })
    expect(topK('SELECT id FROM items ORDER BY price LIMIT 3 OFFSET 5')).toMatchObject({ offset: 5, count: 3 })
  })

  test('finds the outer clauses past subqueries, quotes and a CTE', () => {
    const shape = topK(
      "WITH picked AS (SELECT id FROM items ORDER BY id LIMIT 9) SELECT id, (SELECT 'from, order by') FROM picked ORDER BY id % 2 LIMIT 1",
    )
    expect(shape?.ranked).toBe(
      "WITH picked AS (SELECT id FROM items ORDER BY id LIMIT 9) SELECT id, (SELECT 'from, order by'), DENSE_RANK() OVER (ORDER BY id % 2) AS corpus_tie_group FROM picked ORDER BY id % 2",
    )
  })

  test('declines what the rewrite could change the meaning of', () => {
    expect(topK('SELECT id FROM items LIMIT 2')).toBeUndefined()
    expect(topK('SELECT id FROM items ORDER BY 1 LIMIT 2')).toBeUndefined()
    expect(topK('SELECT DISTINCT kind FROM items ORDER BY kind LIMIT 2')).toBeUndefined()
    expect(topK('SELECT id FROM a UNION SELECT id FROM b ORDER BY id LIMIT 2')).toBeUndefined()
    expect(topK('SELECT id FROM (SELECT id FROM items ORDER BY id LIMIT 2) picked')).toBeUndefined()
  })
})

describe('validTopK', () => {
  // (id, name, tie group): two rows tie first, three tie second.
  const full: Rows = [
    [7, 'b', 1],
    [3, 'b', 1],
    [1, 'a', 2],
    [2, 'a', 2],
    [9, 'a', 2],
  ]
  const shape = { offset: 0, count: 3 }

  test('accepts any choice among the tied rows', () => {
    expect(validTopK([[7, 'b'], [3, 'b'], [1, 'a']], full, shape)).toBe(true)
    expect(validTopK([[3, 'b'], [7, 'b'], [9, 'a']], full, shape)).toBe(true)
  })

  test('refuses a row from the wrong tie group, a repeat, a stranger and a short answer', () => {
    expect(validTopK([[7, 'b'], [1, 'a'], [3, 'b']], full, shape)).toBe(false)
    expect(validTopK([[7, 'b'], [7, 'b'], [1, 'a']], full, shape)).toBe(false)
    expect(validTopK([[7, 'b'], [3, 'b'], [4, 'a']], full, shape)).toBe(false)
    expect(validTopK([[7, 'b'], [3, 'b']], full, shape)).toBe(false)
  })

  test('counts positions from the offset', () => {
    expect(validTopK([[2, 'a'], [9, 'a']], full, { offset: 2, count: 2 })).toBe(true)
    expect(validTopK([[3, 'b'], [9, 'a']], full, { offset: 2, count: 2 })).toBe(false)
    expect(validTopK([[9, 'a']], full, { offset: 4, count: 5 })).toBe(true)
  })
})

describe('tiebroken', () => {
  test('orders every window to the row', () => {
    expect(tiebroken('SELECT id, ROW_NUMBER() OVER (PARTITION BY kind ORDER BY price DESC) FROM items')).toBe(
      'SELECT id, ROW_NUMBER() OVER (PARTITION BY kind ORDER BY price DESC, id) FROM items',
    )
    expect(tiebroken('SELECT FIRST_VALUE(price) OVER (PARTITION BY kind) FROM items')).toBe(
      'SELECT FIRST_VALUE(price) OVER (PARTITION BY kind ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM items',
    )
    expect(tiebroken('SELECT SUM(price) OVER (ORDER BY kind ROWS 2 PRECEDING) FROM items')).toBe(
      'SELECT SUM(price) OVER (ORDER BY kind, id ROWS 2 PRECEDING) FROM items',
    )
  })

  test('leaves a window already ordered by id, and a statement without one', () => {
    expect(tiebroken('SELECT ROW_NUMBER() OVER (ORDER BY id DESC) FROM items')).toBeUndefined()
    expect(tiebroken('SELECT id FROM items ORDER BY price')).toBeUndefined()
  })
})

test('membersSorted sorts JSON arrays and comma lists, nothing else', () => {
  expect(membersSorted([['[3, 1, 2]', 'c,a,b', 'plain', 7]])).toEqual(membersSorted([['[1, 2, 3]', 'a,b,c', 'plain', 7]]))
  expect(bagEqual(membersSorted([['[1, 2]']]), membersSorted([['[1, 3]']]))).toBe(false)
})

test('spellingFolded folds case, accents and trailing spaces', () => {
  expect(spellingFolded([['RED ', 'Ärger']])).toEqual([['red', 'arger']])
})

describe('explain', () => {
  const never = async (): Promise<Rows> => {
    throw new Error('not asked')
  }

  test('a different row is a difference', async () => {
    expect(await explain('SELECT id FROM items', [[1]], [[2]], never)).toEqual({ parity: 'differs', kind: 'other' })
  })

  test('a valid top-k is not comparable, and says what was checked', async () => {
    const full: Rows = [[1, 5, 1], [2, 5, 1], [3, 4, 2]]
    const verdict = await explain('SELECT id, price FROM items ORDER BY price DESC LIMIT 1', [[1, 5]], [[2, 5]], async (engine, sql) => {
      expect(engine).toBe('mysql')
      expect(sql).toContain('DENSE_RANK()')
      return full
    })
    expect(verdict).toMatchObject({ parity: 'not-comparable', check: 'each row is drawn from the tie group of its position' })
  })

  test('a top-k row from a later tie group is a difference', async () => {
    const full: Rows = [[1, 5, 1], [2, 5, 1], [3, 4, 2]]
    const ask = async (_engine: string, sql: string): Promise<Rows> => (sql.includes('DENSE_RANK') ? full : full.map((row) => row.slice(0, 2)))
    expect(await explain('SELECT id, price FROM items ORDER BY price DESC LIMIT 1', [[1, 5]], [[3, 4]], ask)).toEqual({
      parity: 'differs',
      kind: 'limit',
    })
  })

  test('a window is not comparable only when the tie-broken statement agrees', async () => {
    const sql = 'SELECT id, ROW_NUMBER() OVER (ORDER BY price) FROM items'
    const agree = await explain(sql, [[1, 1], [2, 2]], [[1, 2], [2, 1]], async () => [[1, 1], [2, 2]])
    expect(agree.parity).toBe('not-comparable')
    const disagree = await explain(sql, [[1, 1], [2, 2]], [[1, 2], [2, 1]], async (engine) =>
      engine === 'mysql' ? [[1, 1], [2, 2]] : [[1, 2], [2, 1]],
    )
    expect(disagree).toEqual({ parity: 'differs', kind: 'other' })
  })

  test('a grouped key in another spelling is not comparable; an ungrouped one differs', async () => {
    expect((await explain('SELECT kind, COUNT(*) FROM items GROUP BY kind', [['RED', 2]], [['red', 2]], never)).parity).toBe('not-comparable')
    expect((await explain('SELECT kind FROM items WHERE id = 1', [['RED']], [['red']], never)).parity).toBe('differs')
  })
})

describe('orderOf', () => {
  const sql = 'SELECT id, price FROM items ORDER BY price'
  const full: Rows = [[1, 4, 1], [2, 4, 1], [3, 5, 2]]
  const ask = async (): Promise<Rows> => full

  test('accepts tied rows in either order', async () => {
    expect(await orderOf(sql, [[1, 4], [2, 4], [3, 5]], [[2, 4], [1, 4], [3, 5]], ask)).toBe('checked')
  })

  test('refuses the same rows out of order', async () => {
    expect(await orderOf(sql, [[1, 4], [2, 4], [3, 5]], [[3, 5], [1, 4], [2, 4]], ask)).toBe('differs')
  })

  test('proves nothing without an ORDER BY, or when the source answer does not fit the rewrite', async () => {
    expect(await orderOf('SELECT id FROM items', [[1]], [[1]], ask)).toBe('unchecked')
    expect(await orderOf(sql, [[3, 5], [1, 4], [2, 4]], [[3, 5], [1, 4], [2, 4]], ask)).toBe('unchecked')
  })
})

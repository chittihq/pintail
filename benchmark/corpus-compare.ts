// Tells a wrong answer from a second right one.
//
// At the fixture's own size every corpus statement has one answer. Once
// each row is copied many times, some statements have several: a LIMIT
// cuts through rows whose sort keys tie, a window orders tied rows, an
// aggregate concatenates its members in whatever order it met them, a
// case-insensitive group shows one member's spelling. Two engines may then
// both be right and still return different rows.
//
// Nothing here calls such a pair equal. Each check either proves the
// second answer is one the statement allows, and the case is recorded as
// not comparable with the reason and the check that passed, or it proves
// nothing and the case stays a difference.

import { canonicalValue } from '../tests/e2e/lib.ts'

export type Rows = unknown[][]

/// One row as a comparable string. Numbers that both engines spell
/// differently (1.50 against 1.5) meet on their numeric value.
export function rowKey(row: unknown[]): string {
  return row
    .map((value) => {
      const text = canonicalValue(value)
      const number = Number(text)
      return text !== '' && Number.isFinite(number) && /^-?[\d.]+(e[-+]?\d+)?$/i.test(text)
        ? `n:${number}`
        : text
    })
    .join('\u0001')
}

export function bagEqual(left: Rows, right: Rows): boolean {
  if (left.length !== right.length) return false
  const a = left.map(rowKey).sort()
  const b = right.map(rowKey).sort()
  return a.every((key, index) => key === b[index])
}

type Mark = { word: string; at: number; end: number }

const WORDS = /^(SELECT|DISTINCT|FROM|ORDER\s+BY|LIMIT|UNION|INTERSECT|EXCEPT|OVER|ROWS|RANGE|GROUPS)\b/i

/// The clause keywords of a statement with the parenthesis depth each one
/// sits at, skipping quoted text.
function marks(sql: string): Array<Mark & { depth: number }> {
  const found: Array<Mark & { depth: number }> = []
  let depth = 0
  for (let at = 0; at < sql.length; at += 1) {
    const char = sql[at]
    if (char === "'" || char === '"' || char === '`') {
      at += 1
      while (at < sql.length && sql[at] !== char) {
        if (sql[at] === '\\' && char !== '`') at += 1
        at += 1
      }
      continue
    }
    if (char === '(') depth += 1
    else if (char === ')') depth -= 1
    else if (/[A-Za-z]/.test(char) && (at === 0 || !/[\w$.]/.test(sql[at - 1]))) {
      const match = WORDS.exec(sql.slice(at, at + 16))
      if (match) {
        found.push({ word: match[1].toUpperCase().replace(/\s+/, ' '), at, end: at + match[0].length, depth })
        at += match[0].length - 1
      } else {
        while (at + 1 < sql.length && /[\w$]/.test(sql[at + 1])) at += 1
      }
    }
  }
  return found
}

/// The index of the parenthesis closing the one at `open`.
function closing(sql: string, open: number): number {
  let depth = 0
  for (let at = open; at < sql.length; at += 1) {
    const char = sql[at]
    if (char === "'" || char === '"' || char === '`') {
      at += 1
      while (at < sql.length && sql[at] !== char) {
        if (sql[at] === '\\' && char !== '`') at += 1
        at += 1
      }
    } else if (char === '(') depth += 1
    else if (char === ')') {
      depth -= 1
      if (depth === 0) return at
    }
  }
  return -1
}

export type TopK = {
  /// The statement without its LIMIT and with one more column: the number
  /// of the tie group each row's sort keys put it in.
  ranked: string
  offset: number
  count: number
}

/// Rewrites `SELECT ... ORDER BY keys [LIMIT n]` so the source engine says
/// which rows tie: the same statement without the LIMIT, each row carrying
/// `DENSE_RANK()` over the same keys. Declines (undefined) a statement the
/// rewrite could change the meaning of: a set operation, DISTINCT, a
/// position or no ORDER BY at all.
export function ordered(sql: string): TopK | undefined {
  const text = sql.trim().replace(/;\s*$/, '')
  const top = marks(text).filter((mark) => mark.depth === 0)
  if (top.some((mark) => ['UNION', 'INTERSECT', 'EXCEPT'].includes(mark.word))) return undefined
  const select = top.findIndex((mark) => mark.word === 'SELECT')
  if (select < 0 || top[select + 1]?.word === 'DISTINCT') return undefined
  const from = top.find((mark, index) => index > select && mark.word === 'FROM')
  const limit = top.at(-1)?.word === 'LIMIT' ? top.at(-1) : undefined
  const order = top.at(limit ? -2 : -1)
  if (!from || order?.word !== 'ORDER BY' || order.at < from.at) return undefined
  let [offset, count] = [0, Number.POSITIVE_INFINITY]
  if (limit) {
    const tail = /^\s*(\d+)\s*(?:(,|OFFSET)\s*(\d+))?\s*$/i.exec(text.slice(limit.end))
    if (!tail) return undefined
    const [first, second] = [Number(tail[1]), tail[3] === undefined ? undefined : Number(tail[3])]
    ;[offset, count] = second === undefined ? [0, first] : tail[2] === ',' ? [first, second] : [second, first]
  }
  const end = limit?.at ?? text.length
  const keys = text.slice(order.end, end).trim()
  // A bare number is a select-list position there and a constant here.
  if (keys.split(',').some((key) => /^\s*\d+\s*(ASC|DESC)?\s*$/i.test(key))) return undefined
  return {
    ranked: `${text.slice(0, from.at).trimEnd()}, DENSE_RANK() OVER (ORDER BY ${keys}) AS corpus_tie_group ${text.slice(from.at, end).trimEnd()}`,
    offset,
    count,
  }
}

/// `ordered` for a statement that ends in a LIMIT, and nothing otherwise.
export function topK(sql: string): TopK | undefined {
  const shape = ordered(sql)
  return shape && Number.isFinite(shape.count) ? shape : undefined
}

/// Whether two answers holding the same rows are both in the order the
/// statement asks for: `checked` when the source engine's tie groups put
/// every row of both at a position it may take, `differs` when they do for
/// MySQL's answer and not for Pintail's, `unchecked` when the statement
/// states no order or the rewrite does not describe it.
export async function orderOf(sql: string, mysql: Rows, pintail: Rows, ask: Ask): Promise<'checked' | 'differs' | 'unchecked'> {
  const shape = ordered(sql)
  if (!shape) return 'unchecked'
  let full: Rows
  try {
    full = await ask('mysql', shape.ranked)
  } catch {
    return 'unchecked'
  }
  if (!validTopK(mysql, full, shape)) return 'unchecked'
  return validTopK(pintail, full, shape) ? 'checked' : 'differs'
}

/// Whether `answer` is a valid answer to the LIMIT statement `ranked` was
/// made from: as many rows as the LIMIT leaves, the row at each position
/// drawn from the tie group that position belongs to, and no row drawn
/// more often than the full answer holds it.
export function validTopK(answer: Rows, full: Rows, shape: Pick<TopK, 'offset' | 'count'>): boolean {
  if (answer.length !== Math.max(0, Math.min(shape.count, full.length - shape.offset))) return false
  const pool = new Map<string, number>()
  const member = (group: unknown, row: unknown[]) => `${String(group)}\u0002${rowKey(row)}`
  for (const row of full) {
    const key = member(row.at(-1), row.slice(0, -1))
    pool.set(key, (pool.get(key) ?? 0) + 1)
  }
  return answer.every((row, index) => {
    const key = member(full[shape.offset + index].at(-1), row)
    const left = pool.get(key) ?? 0
    pool.set(key, left - 1)
    return left > 0
  })
}

/// Gives every window a total order by appending `id` to its ORDER BY, or,
/// where it has none, ordering it by `id` under a frame that still spans
/// the partition. Undefined when no window changed. The result is another
/// statement, with one answer; it only reads where `id` is one column.
export function tiebroken(sql: string): string | undefined {
  let result = ''
  let copied = 0
  let changed = false
  const all = marks(sql)
  for (const [index, mark] of all.entries()) {
    if (mark.word !== 'OVER') continue
    const open = sql.slice(mark.end).search(/\S/) + mark.end
    if (sql[open] !== '(') continue
    const close = closing(sql, open)
    if (close < 0) return undefined
    const inside = all.filter((inner, at) => at > index && inner.at < close && inner.depth === mark.depth + 1)
    const order = inside.find((inner) => inner.word === 'ORDER BY')
    const frame = inside.find((inner) => ['ROWS', 'RANGE', 'GROUPS'].includes(inner.word))
    let spec: string
    if (order) {
      const end = frame?.at ?? close
      const keys = sql.slice(order.end, end)
      if (keys.split(',').some((key) => /^\s*id\s*(ASC|DESC)?\s*$/i.test(key))) continue
      spec = `${sql.slice(open + 1, end).trimEnd()}, id${frame ? ` ${sql.slice(end, close)}` : ''}`
    } else if (frame) {
      continue
    } else {
      spec = `${sql.slice(open + 1, close).trim()} ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING`.trim()
    }
    result += `${sql.slice(copied, open + 1)}${spec}`
    copied = close
    changed = true
  }
  return changed ? result + sql.slice(copied) : undefined
}

/// Each cell with the members of a list in sorted order: the elements of a
/// JSON array, or the comma-separated parts of a concatenation.
export function membersSorted(rows: Rows): Rows {
  return rows.map((row) =>
    row.map((value) => {
      const text = canonicalValue(value)
      if (text.startsWith('[')) {
        try {
          const parsed: unknown = JSON.parse(text)
          if (Array.isArray(parsed)) return JSON.stringify(parsed.map((item) => JSON.stringify(item)).sort())
        } catch {}
      }
      return text.includes(',') ? text.split(',').sort().join(',') : text
    }),
  )
}

/// Each text cell without the differences a case- and accent-insensitive,
/// space-padded collation ignores.
export function spellingFolded(rows: Rows): Rows {
  return rows.map((row) =>
    row.map((value) =>
      canonicalValue(value).normalize('NFD').replace(/\p{M}/gu, '').toLowerCase().trimEnd(),
    ),
  )
}

/// A trailing LIMIT, which a difference's classification removes to see
/// the whole answer the LIMIT chose from.
const LIMIT_TAIL = /\s+LIMIT\s+\d+(\s*(,|OFFSET)\s*\d+)?\s*$/i

export function withoutLimit(sql: string): string | undefined {
  const stripped = sql.replace(LIMIT_TAIL, '')
  return stripped === sql ? undefined : stripped
}

/// What kind of difference an unexplained one is: `order` when the rows
/// are the same in another order, `limit` when every row Pintail returned
/// is in MySQL's answer without the LIMIT, `other` otherwise.
export function classify(mysql: Rows, pintail: Rows, full: Rows | undefined): string {
  if (bagEqual(mysql, pintail)) return 'order'
  if (full && mysql.length === pintail.length) {
    const available = new Map<string, number>()
    for (const row of full) available.set(rowKey(row), (available.get(rowKey(row)) ?? 0) + 1)
    const drawn = pintail.every((row) => {
      const left = available.get(rowKey(row)) ?? 0
      available.set(rowKey(row), left - 1)
      return left > 0
    })
    if (drawn) return 'limit'
  }
  return 'other'
}

export type Verdict =
  | { parity: 'not-comparable'; reason: string; check: string }
  | { parity: 'differs'; kind: string }

/// Runs a statement on one engine; a refusal or timeout rejects.
export type Ask = (engine: 'mysql' | 'pintail', sql: string) => Promise<Rows>

/// Decides whether two different answers are both answers. The checks run
/// from the cheapest; each one that passes names what it proved.
export async function explain(sql: string, mysql: Rows, pintail: Rows, ask: Ask): Promise<Verdict> {
  const attempt = async <T>(work: () => Promise<T>): Promise<T | undefined> => {
    try {
      return await work()
    } catch {
      return undefined
    }
  }
  if (bagEqual(membersSorted(mysql), membersSorted(pintail))) {
    return {
      parity: 'not-comparable',
      reason: 'an aggregate lists its members in no stated order',
      check: 'equal once the members of each list are sorted',
    }
  }
  if (/\bGROUP_CONCAT\s*\(/i.test(sql)) {
    const uncut = await attempt(async () => {
      const answers: Rows[] = []
      for (const engine of ['mysql', 'pintail'] as const) {
        await ask(engine, 'SET SESSION group_concat_max_len = 1073741824')
        try {
          answers.push(await ask(engine, sql))
        } finally {
          await ask(engine, 'SET SESSION group_concat_max_len = DEFAULT').catch(() => undefined)
        }
      }
      return answers
    })
    if (uncut && bagEqual(membersSorted(uncut[0]), membersSorted(uncut[1]))) {
      return {
        parity: 'not-comparable',
        reason: 'GROUP_CONCAT lists its members in no stated order and is cut at group_concat_max_len',
        check: 'equal uncut, once the members of each list are sorted',
      }
    }
  }
  const shape = topK(sql)
  if (shape) {
    const full = await attempt(() => ask('mysql', shape.ranked))
    // MySQL's own answer has to pass, or the rewrite does not describe
    // this statement and proves nothing about another engine's answer.
    if (full && validTopK(mysql, full, shape) && validTopK(pintail, full, shape)) {
      return {
        parity: 'not-comparable',
        reason: 'the LIMIT cuts through rows whose sort keys tie',
        check: 'each row is drawn from the tie group of its position',
      }
    }
  }
  const total = tiebroken(sql)
  if (total) {
    const both = await attempt(async () => [await ask('mysql', total), await ask('pintail', total)])
    if (both && bagEqual(both[0], both[1])) {
      return {
        parity: 'not-comparable',
        reason: 'a window orders rows whose sort keys tie',
        check: 'equal once every window is ordered to the row',
      }
    }
  }
  if (/\b(GROUP\s+BY|DISTINCT)\b/i.test(sql) && bagEqual(spellingFolded(mysql), spellingFolded(pintail))) {
    return {
      parity: 'not-comparable',
      reason: 'a group of keys equal under their collation shows one member\'s spelling',
      check: 'equal once case, accents and trailing spaces are folded',
    }
  }
  const stripped = withoutLimit(sql)
  const full = stripped ? await attempt(() => ask('mysql', stripped)) : undefined
  return { parity: 'differs', kind: classify(mysql, pintail, full) }
}

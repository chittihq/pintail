/// Reading MySQL's own test SQL well enough to decide how to compare it.
///
/// Its own module so it can be tested: importing `run.ts` runs the whole
/// harness, containers and all.

/// The statement with every string, quoted identifier and comment blanked
/// to spaces, so a scan reads syntax and never their contents. A row value
/// of `'order by x'` used to count as an ORDER BY, and a commented-out one
/// did too.
export function withoutLiterals(sql: string): string {
  let out = ''
  for (let index = 0; index < sql.length; index += 1) {
    const char = sql[index]!
    if (char === "'" || char === '"' || char === '`') {
      out += ' '
      index += 1
      while (index < sql.length) {
        if (sql[index] === '\\' && char !== '`') index += 1
        else if (sql[index] === char) {
          // A doubled quote stays inside the literal.
          if (sql[index + 1] === char) index += 1
          else break
        }
        out += ' '
        index += 1
      }
      out += ' '
      continue
    }
    if (char === '-' && sql[index + 1] === '-') {
      while (index < sql.length && sql[index] !== '\n') { out += ' '; index += 1 }
      out += '\n'
      continue
    }
    if (char === '#') {
      while (index < sql.length && sql[index] !== '\n') { out += ' '; index += 1 }
      out += '\n'
      continue
    }
    if (char === '/' && sql[index + 1] === '*') {
      while (index < sql.length && !(sql[index] === '*' && sql[index + 1] === '/')) { out += ' '; index += 1 }
      out += '  '
      index += 1
      continue
    }
    out += char
  }
  return out
}

/// A LIMIT keeps whichever rows the server reaches first unless an ORDER BY
/// at its OWN level decides them, and neither server defines that choice, so
/// such a statement's rows are not compared.
///
/// The level matters: `SELECT * FROM (SELECT ... LIMIT 3) t ORDER BY x` has
/// an outer ORDER BY, but which three rows the derived table kept is still
/// undefined, and comparing them banked a coin flip as an exact match.
export function unorderedLimit(sql: string): boolean {
  const lower = withoutLiterals(sql).toLowerCase()
  const ordered: boolean[] = [false]
  for (let index = 0; index < lower.length; index += 1) {
    const char = lower[index]
    if (char === '(') ordered.push(false)
    else if (char === ')') { if (ordered.length > 1) ordered.pop() }
    else if (/^order\s+by\b/.test(lower.slice(index, index + 32))) {
      // ORDER BY NULL asks for no order at all.
      ordered[ordered.length - 1] = !/^order\s+by\s+null\b/.test(lower.slice(index))
    } else if (/^limit\s+\d/.test(lower.slice(index, index + 32)) && !ordered[ordered.length - 1]) {
      return true
    }
  }
  return false
}

export function hasOuterOrderBy(sql: string): boolean {
  let depth = 0
  const lower = withoutLiterals(sql).toLowerCase()
  for (let index = 0; index < lower.length; index += 1) {
    const char = lower[index]
    if (char === '(') depth += 1
    else if (char === ')') depth -= 1
    // Whitespace between the words is the author's business: `ORDER  BY`
    // and one split across lines are the same clause.
    else if (depth === 0 && /^order\s+by\b/.test(lower.slice(index, index + 32))) {
      return !/^order\s+by\s+null\b/.test(lower.slice(index))
    }
  }
  return false
}

/// System variables whose session value changes what a SELECT answers.
/// One the two sides disagree on that is not here (an optimizer switch, a
/// buffer size, a lock timeout) leaves every answer comparable.
const ANSWER_STATE = new Set([
  'sql_mode', 'time_zone', 'timestamp', 'lc_time_names', 'group_concat_max_len', 'div_precision_increment',
  'max_sort_length', 'default_week_format', 'sql_select_limit', 'windowing_use_high_precision',
  'cte_max_recursion_depth', 'block_encryption_mode',
])

/// The answer-changing session state a session statement sets, as keys:
/// `names` for the connection character sets, `use` for the default
/// database, one of the system variables above, or `@name` for a user
/// variable.
export function sessionKeys(sql: string): string[] {
  if (/^\s*use\s/i.test(sql)) return ['use']
  if (/^\s*set\s+(?:names|character\s+set|charset)\b/i.test(sql)) return ['names']
  const assignments = /^\s*set\s+([\s\S]*)$/i.exec(sql)?.[1]
  if (assignments === undefined) return []
  const keys = new Set<string>()
  // Commas inside a call or a string separate nothing.
  for (const part of assignments.replace(/'(?:\\.|''|[^'\\])*'|"(?:\\.|""|[^"\\])*"/g, "''").split(/,(?![^()]*\))/)) {
    const name = /^\s*(?:(?:session|local)\s+|@@(?:session\.|local\.)?)?(@?[\w$.]+)\s*:?=/i.exec(part)?.[1]?.toLowerCase()
    if (name === undefined) continue
    if (/^(?:character_set_(?:client|connection|results)|collation_connection)$/.test(name)) keys.add('names')
    else if (name.startsWith('@') || ANSWER_STATE.has(name)) keys.add(name)
  }
  return [...keys]
}

/// Session state the two sides no longer share. A session statement one
/// side refused and the other ran leaves the state it sets different on the
/// two; the next statement both sides run for the same state brings it back
/// together. Until then a SELECT is compared as before - one that still
/// matches was never touched by the difference - and one both sides answer
/// differently is counted as `session-diverged`, not as a mismatch of its
/// own. A user variable only reaches the statements that name it.
export class SessionDrift {
  private apart = new Set<string>()

  note(sql: string, mysqlRan: boolean, pintailRan: boolean) {
    for (const key of sessionKeys(sql)) {
      if (mysqlRan !== pintailRan) this.apart.add(key)
      else if (mysqlRan) this.apart.delete(key)
    }
  }

  reaches(sql: string): string | undefined {
    for (const key of this.apart) {
      if (!key.startsWith('@')) return key
      if (new RegExp(`${key.replace(/[$.]/g, '\\$&')}(?![\\w$.])`, 'i').test(sql)) return key
    }
    return undefined
  }
}

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

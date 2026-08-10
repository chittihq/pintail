#!/usr/bin/env bun
/// Reads the callable function surface straight out of the binder and diffs a
/// corpus of SQL against it.
///
/// The binder's match arms are the only place that decides whether a name
/// resolves, so they are the single source of truth. A hand-maintained list
/// drifts — issues #11, #13 and #17 all carried unchecked boxes for shipped
/// work because nothing regenerated them.
///
/// ```text
/// bun run scripts/function-surface.ts                 # print the surface
/// bun run scripts/function-surface.ts corpus/*.sql    # rank what is missing
/// ```

import { readFileSync } from 'node:fs'
import { join } from 'node:path'

const repository = join(import.meta.dir, '..')

/// One callable name and the argument counts the binder accepts for it.
interface Overload {
  name: string
  arities: string[]
}

/// Pulls `"NAME" | "ALIAS" if args.len() == N => Function::X` arms out of the
/// binder. The arity guard is kept verbatim rather than parsed into a range:
/// the guards include forms like `matches!(args.len(), 1 | 2)` and
/// `!args.is_empty()`, and a faithful copy is more useful in a compatibility
/// matrix than a lossy normalization.
export function surface(): Map<string, Set<string>> {
  // The binder is split across modules; read every one of them or the
  // surface silently loses the callables that moved out of mod.rs.
  const source = ['binder/mod.rs', 'binder/function.rs']
    .map((module) => readFileSync(join(repository, 'crates/pintail-sql/src', module), 'utf8'))
    .join('\n')
  const found = new Map<string, Set<string>>()
  // Names sit at the head of a match arm, optionally alternated, optionally
  // followed by a guard, and always followed by `=>`.
  // The guard runs to the fat arrow. It cannot be `[^=]+` because arity
  // guards contain `==`, which is what made the first version of this
  // report 29 names out of 107.
  const arm = /^\s*((?:"[A-Z0-9_]+"\s*\|\s*)*"[A-Z0-9_]+")\s*(if\s+.+?)?\s*=>/gm
  for (const match of source.matchAll(arm)) {
    const names = [...match[1].matchAll(/"([A-Z0-9_]+)"/g)].map((m) => m[1])
    const guard = (match[2] ?? '').replace(/\s+/g, ' ').trim()
    for (const name of names) {
      if (!found.has(name)) found.set(name, new Set())
      found.get(name)!.add(guard || 'any arity')
    }
  }
  // Functions whose first argument is a bare unit keyword (TIMESTAMPADD,
  // TIMESTAMPDIFF) are dispatched by an equality test ahead of the match,
  // because the argument list cannot be bound generically. Missing these
  // made the first run report TIMESTAMPDIFF as an unsupported gap while the
  // oracle had covered it for months.
  for (const match of source.matchAll(/function_name == "([A-Z0-9_]+)"/g)) {
    if (!found.has(match[1])) found.set(match[1], new Set(['keyword argument']))
  }
  // A third dispatch shape: a matches!() guard ahead of the match, used
  // where several names share one binder path. DATE_ADD, DATE_SUB and
  // TIMESTAMPADD are bound this way, and reading only the two shapes above
  // reported them as unsupported gaps - the same failure the equality-test
  // handler above was added to fix, one shape further along.
  for (const guard of source.matchAll(
    /matches!\(\s*(?:function_name|name)\.as_str\(\)\s*,([^)]*)\)/g,
  )) {
    for (const match of guard[1].matchAll(/"([A-Z0-9_]+)"/g)) {
      if (!found.has(match[1])) found.set(match[1], new Set(['shared binder path']))
    }
  }
  return found
}

/// Every identifier used in call position. Deliberately crude: it over-reports
/// (table-valued syntax, CTE names) rather than under-reports, because a
/// missed call is a silently unranked gap while a false positive is visible
/// and gets discarded by eye.
function calls(sql: string): string[] {
  const stripped = sql
    .replace(/--[^\n]*/g, ' ')
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .replace(/'(?:[^'\\]|\\.|'')*'/g, "''")
  return [...stripped.matchAll(/\b([A-Za-z_][A-Za-z0-9_]*)\s*\(/g)].map((m) =>
    m[1].toUpperCase(),
  )
}

/// Names that parse as dedicated syntax rather than a call, so the binder
/// never sees them as a function name and the extractor above cannot find
/// them. They are supported; the corpus just spells them like calls.
const SYNTAX_FORMS = new Set([
  'CAST',
  // CONVERT binds through Expr::Convert (binder.rs:1897), both the
  // CAST-equivalent and USING-charset forms. Omitting it here reported it as
  // a gap and that error reached issue #17 before being caught.
  'CONVERT',
  'EXTRACT',
  'SUBSTRING',
  'TRIM',
  'POSITION',
  'COUNT',
  'SUM',
  'AVG',
  'MIN',
  'MAX',
  'GROUP_CONCAT',
  'JSON_ARRAYAGG',
  'ROW_NUMBER',
  'RANK',
  'DENSE_RANK',
  'OVER',
])

/// SQL keywords that precede a parenthesis and are not calls at all.
const NOT_CALLS = new Set([
  'IF',
  'IN',
  'VALUES',
  'AND',
  'OR',
  'NOT',
  'ON',
  'USING',
  'WHEN',
  'THEN',
  'ELSE',
  'SELECT',
  'FROM',
  'WHERE',
  'BY',
  'AS',
  'UNION',
  'EXISTS',
  'ALL',
  'ANY',
  'WITH',
  'PARTITION',
  'ORDER',
  'GROUP',
  'HAVING',
  'LIMIT',
  'OFFSET',
  'DECIMAL',
  'CHAR',
  'BINARY',
  'SIGNED',
  'UNSIGNED',
  'INTERVAL',
])

// Report only when run directly. The compatibility matrix imports
// `surface()` rather than keeping a second extractor - two readers of the
// same binder drift - so importing must not print or exit.
function report() {
  const supported = surface()
  const files = process.argv.slice(2)

  if (files.length === 0) {
    const rows = [...supported.entries()].sort(([a], [b]) => a.localeCompare(b))
    for (const [name, arities] of rows) {
      console.log(`${name.padEnd(20)} ${[...arities].join(' | ')}`)
    }
    console.log(`\n${rows.length} callable names`)
    return
  }

const seen = new Map<string, number>()
  for (const file of files) {
    for (const name of calls(readFileSync(file, 'utf8'))) {
      if (NOT_CALLS.has(name)) continue
      seen.set(name, (seen.get(name) ?? 0) + 1)
    }
  }

  const missing = [...seen.entries()]
    .filter(([name]) => !supported.has(name) && !SYNTAX_FORMS.has(name))
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))

  console.log(`corpus: ${files.length} files, ${seen.size} distinct call names`)
  console.log(`supported surface: ${supported.size} callable names\n`)
  if (missing.length === 0) {
    console.log('no unsupported functions in this corpus')
  } else {
    console.log('unsupported, by frequency:')
    for (const [name, count] of missing) {
      console.log(`  ${String(count).padStart(4)}  ${name}`)
    }
  }

}

if (import.meta.main) {
  report()
}

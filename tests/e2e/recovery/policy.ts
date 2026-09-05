/** Automatic tests cannot accidentally repair the condition they claim heals itself. */
export function assertAutomaticRequest(path: string, body: unknown, allowMode = false): void {
  const normalized = decodeURIComponent(new URL(path, 'http://fixture').pathname).replace(/\/+$/, '')
  if (/\/(resync|reconcile|reset)$/.test(normalized)
    || /\/dlq\/[^/]+\/retry$/.test(normalized)
    || (/\/mode$/.test(normalized) && !allowMode)
    || (/\/snapshot$/.test(normalized) && (body as { force?: unknown } | undefined)?.force !== false)) {
    throw new Error(`operator endpoint forbidden in automatic scenario: ${normalized}`)
  }
}

/** Exact text/byte values; no floating-point rounding and no delimiter collisions. */
export function exactDiff(expected: unknown[][], actual: unknown[][], multiset = false): string | undefined {
  const encode = (rows: unknown[][]) => rows.map(row => JSON.stringify(row.map(value =>
    value === null ? null : Buffer.isBuffer(value) ? { bytes: value.toString('hex') } : String(value))))
  const left = encode(expected), right = encode(actual)
  if (multiset) { left.sort(); right.sort() }
  if (left.length !== right.length) return `row multiplicity ${left.length} vs ${right.length}`
  const index = left.findIndex((row, i) => row !== right[i])
  return index < 0 ? undefined : `row ${index}: source=${left[index]} replica=${right[index]}`
}

export function selected(slug: string, patterns: string[]): boolean {
  return patterns.length === 0 || patterns.some(pattern => {
    const escaped = pattern.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replaceAll('*', '.*')
    return new RegExp(`^${escaped}$`).test(slug)
  })
}

export function gtidContains(set: string, transaction: string): boolean {
  const [uuid, number] = transaction.split(':')
  if (!uuid || !number || !/^\d+$/.test(number)) throw new Error('expected one GTID')
  const wanted = BigInt(number)
  return set.split(',').some(part => {
    const [sid, ...intervals] = part.trim().split(':')
    return sid.toLowerCase() === uuid.toLowerCase() && intervals.some(interval => {
      const [a, b = a] = interval.split('-')
      return wanted >= BigInt(a) && wanted <= BigInt(b)
    })
  })
}

/** Bootstrap failures can contain an unresolved Docker context or SSH target. */
export function ledgerDetail(status: 'PASS' | 'FAIL' | 'WARN', detail: string): string {
  return status === 'FAIL' ? 'Failed; details retained in private run artifacts.' : detail
}

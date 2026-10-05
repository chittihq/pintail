/// Cached reads against cached reads, and what each cache serves after the
/// data changes (issue #36).
///
/// The canonical table in `results.md` times a Pintail settled-memo hit
/// against a ClickHouse execution with its query cache off. That is a real
/// number, but not a cache-against-cache one. This study measures three
/// things the existing tracks do not:
///
/// 1. **Static, both caches warm.** Pintail's settled memo and ClickHouse's
///    query cache both on, both warmed, every timed answer checked against
///    MySQL and every timed answer proved to be a hit (Pintail's memo
///    counters on `/metrics`, ClickHouse's `QueryCacheHits` profile event in
///    `system.query_log`). A shape a cache does not serve is reported as
///    such, never averaged in.
/// 2. **Freshness after ingestion.** Both caches warm, then a deterministic
///    insert, update or delete is committed to MySQL, and dashboard-style
///    readers poll each engine. Every served answer is classified against
///    MySQL's answers before and after the change, so a stale answer is
///    visible as stale instead of reading as a fast one.
/// 3. **ClickHouse's two freshness modes.** Native TTL expiry, and an
///    explicit `SYSTEM DROP QUERY CACHE` issued once the change is visible,
///    with the visibility check and the drop charged as coordination cost.
///
/// Replication delay and cache staleness are kept apart with a row-level
/// probe of the changed keys that no result cache serves: an old aggregate
/// before the probe sees the change is the replica lagging; an old aggregate
/// after it is the cache serving a stale answer.
///
/// This module holds the study's pure logic (classification, summaries,
/// rendering) and its orchestration over an injected context, so `run.ts`
/// keeps owning containers and connections, and the logic is testable
/// without either.

export type Verdict = 'old' | 'new' | 'other'

/// One served answer during a freshness observation: when it was asked
/// (ms after the source commit), how long it took, what it was, whether a
/// result cache answered it (null when it could not be determined), and
/// whether the engine's data already showed the change when it was asked.
export type ServedSample = {
  t: number
  ms: number
  v: Verdict
  hit: boolean | null
  afterVisible: boolean
}

export type TimingSummary = {
  medianMs: number
  p95Ms: number
  minMs: number
  runs: number
}

export type StaticRow = {
  name: string
  exact: { pintail: boolean; clickhouse: boolean; clickhouseExecuted: boolean }
  /// Settled-memo hits counted on `/metrics` across the timed runs, and the
  /// number of timed runs. Equal means every timed answer was a hit.
  pintailHits: number
  clickhouseHits: number
  runs: number
  /// Misses during warmup: the entry being written. A shape with no hits
  /// is one the cache does not serve.
  pintailWarmupMisses: number
  clickhouseWarmupMisses: number
  pintailCached: TimingSummary
  clickhouseCached: TimingSummary
  /// The same statement on RMT+FINAL with the query cache off: the
  /// execution reference the cached numbers are read against.
  clickhouseExecuted: TimingSummary
  /// Why a shape is not counted as a cache hit, when it is not.
  pintailNote?: string
  clickhouseNote?: string
}

export type FreshnessQueryResult = {
  query: string
  /// Whether this event changed MySQL's answer to this query. An unchanged
  /// query is the control: it shows how far each engine's invalidation
  /// reaches.
  affected: boolean
  /// Warm-cache proof before the change: was the last warm answer a hit.
  warmHit: boolean | null
  firstCorrectAtMs: number | null
  firstCorrectLatencyMs: number | null
  firstCorrectHit: boolean | null
  /// Old answers asked before the engine's data showed the change: the
  /// replica lagging, not the cache.
  staleBeforeVisible: number
  /// Old answers asked after the engine's data showed the change: the
  /// result cache serving a stale answer.
  staleAfterVisible: number
  staleAfterVisibleHits: number
  /// Last stale answer asked after visibility, ms after the commit.
  lastStaleAtMs: number | null
  /// First correct answer minus visibility: how long the cache held the
  /// served answer behind the engine's own data.
  cacheLagMs: number | null
  wrong: number
  /// Answers after the first correct one: steady state after the change.
  after: TimingSummary | null
  afterHits: number
  afterSamples: number
  /// No correct answer within the observation timeout: a failure, never a
  /// fresh read.
  censored: boolean
  samples: Array<[number, number, string, number]>
}

export type FreshnessEngineResult = {
  engine: 'pintail' | 'clickhouse'
  /// When the row-level probe of the changed keys first matched MySQL, ms
  /// after the source commit. Null when it never did within the timeout.
  visibleAtMs: number | null
  probes: number
  queries: FreshnessQueryResult[]
}

export type FreshnessEvent = {
  mode: 'ttl' | 'drop'
  kind: 'insert' | 'update' | 'delete'
  rows: number
  keyRange: [number, number]
  /// Source commit round trip; t = 0 is when it returned.
  mysqlCommitMs: number
  /// ClickHouse is loaded by the harness, not by a replication pipeline:
  /// the changed rows are written into it as soon as MySQL commits, so its
  /// delay is a lower bound for any CDC pipeline feeding it.
  clickhouseAppliedAtMs: number
  /// Age of ClickHouse's cache entries when the change committed.
  clickhouseEntryAgeMs: number | null
  /// Drop mode only: the coordinator's wait for visibility and its drop.
  coordination?: {
    visibleAtMs: number
    probes: number
    probeMs: number
    dropMs: number
    droppedAtMs: number
  }
  pintailWarmWaitMs: number
  engines: FreshnessEngineResult[]
}

export type CacheStudy = {
  generatedAt: string
  settings: {
    clickhouse: {
      queryCacheTtlStaticS: number
      queryCacheTtlFreshnessS: number
      perQuery: string
      server: Record<string, string>
    }
    pintail: {
      memo: string
      maxEntries: number
      maxRows: number
    }
    warmups: number
    runs: number
    pollMs: number
    timeoutS: number
    afterSamples: number
    order: string
  }
  static: StaticRow[]
  freshness: FreshnessEvent[]
}

export const PINTAIL_MEMO_MAX_ENTRIES = 32
export const PINTAIL_MEMO_MAX_ROWS = 1 << 17

/// The query-cache settings every cached ClickHouse read carries. Minimum
/// runs and duration at zero so the first execution writes the entry; the
/// TTL is the only expiry, and the study sets it per mode.
export function clickhouseCacheSettings(ttlSeconds: number): string {
  return (
    `use_query_cache = 1, query_cache_ttl = ${ttlSeconds}, ` +
    'query_cache_min_query_runs = 0, query_cache_min_query_duration = 0'
  )
}

export function classify(canonical: string, before: string, after: string): Verdict {
  if (canonical === after) return 'new'
  if (canonical === before) return 'old'
  return 'other'
}

/// Reads one unlabelled counter out of a Prometheus text exposition.
export function prometheusValue(text: string, name: string): number {
  for (const line of text.split('\n')) {
    if (line.startsWith(`${name} `)) return Number(line.slice(name.length + 1).trim())
  }
  throw new Error(`metric ${name} is missing`)
}

export function summarize(values: number[]): TimingSummary {
  const sorted = [...values].sort((a, b) => a - b)
  const at = (index: number) => round1(sorted[Math.max(0, Math.min(sorted.length - 1, index))] ?? 0)
  return {
    medianMs: at(Math.floor(sorted.length / 2)),
    p95Ms: at(Math.ceil(sorted.length * 0.95) - 1),
    minMs: at(0),
    runs: sorted.length,
  }
}

const round1 = (value: number) => Math.round(value * 10) / 10

/// Summarizes one engine's answers to one query over one change event.
export function summarizeFreshness(
  query: string,
  affected: boolean,
  warmHit: boolean | null,
  samples: ServedSample[],
  visibleAtMs: number | null,
): FreshnessQueryResult {
  const firstCorrect = samples.find((sample) => sample.v === 'new')
  const stale = samples.filter((sample) => sample.v === 'old')
  const staleAfter = stale.filter((sample) => sample.afterVisible)
  const afterFirst = firstCorrect ? samples.slice(samples.indexOf(firstCorrect) + 1) : []
  // Steady state counts only correct answers: an answer that flips back to
  // old after a correct one is stale and already counted above.
  const steady = afterFirst.filter((sample) => sample.v === 'new')
  return {
    query,
    affected,
    warmHit,
    firstCorrectAtMs: firstCorrect ? round1(firstCorrect.t + firstCorrect.ms) : null,
    firstCorrectLatencyMs: firstCorrect ? round1(firstCorrect.ms) : null,
    firstCorrectHit: firstCorrect ? firstCorrect.hit : null,
    staleBeforeVisible: stale.length - staleAfter.length,
    staleAfterVisible: staleAfter.length,
    staleAfterVisibleHits: staleAfter.filter((sample) => sample.hit === true).length,
    lastStaleAtMs: staleAfter.length > 0 ? round1(staleAfter[staleAfter.length - 1].t) : null,
    cacheLagMs:
      firstCorrect && visibleAtMs !== null
        ? round1(Math.max(0, firstCorrect.t + firstCorrect.ms - visibleAtMs))
        : null,
    wrong: samples.filter((sample) => sample.v === 'other').length,
    after: steady.length > 0 ? summarize(steady.map((sample) => sample.ms)) : null,
    afterHits: steady.filter((sample) => sample.hit === true).length,
    afterSamples: steady.length,
    censored: affected ? !firstCorrect : false,
    samples: samples.map((sample) => [
      round1(sample.t),
      round1(sample.ms),
      `${sample.v}${sample.afterVisible ? '' : '*'}`,
      sample.hit === null ? -1 : sample.hit ? 1 : 0,
    ]),
  }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const ms = (value: number | null | undefined) =>
  value === null || value === undefined
    ? 'n/a'
    : `${value < 10 && !Number.isInteger(value) ? value.toFixed(1) : Math.round(value).toLocaleString()} ms`

const label = (name: string) => name.replace(/^[QFC]\d+:\s*/, '')

const hitCell = (hits: number, runs: number) => `${hits}/${runs}`

export function renderStaticTable(study: CacheStudy, labels = true): string[] {
  return [
    '| Query | Pintail memo hit | memo hits | CH query-cache hit | cache hits | CH RMT+FINAL executed | Exact |',
    '|---|---:|---:|---:|---:|---:|:--|',
    ...study.static.map((row) => {
      const exact = row.exact.pintail && row.exact.clickhouse && row.exact.clickhouseExecuted
      const pintail =
        row.pintailHits === 0 ? `${ms(row.pintailCached.medianMs)} (not cached)` : ms(row.pintailCached.medianMs)
      const clickhouse =
        row.clickhouseHits === 0
          ? `${ms(row.clickhouseCached.medianMs)} (not cached)`
          : ms(row.clickhouseCached.medianMs)
      return (
        `| ${labels ? label(row.name) : row.name} | ${pintail} | ${hitCell(row.pintailHits, row.runs)} | ` +
        `${clickhouse} | ${hitCell(row.clickhouseHits, row.runs)} | ` +
        `${ms(row.clickhouseExecuted.medianMs)} | ${exact ? 'yes' : 'MISMATCH'} |`
      )
    }),
  ]
}

const engineName = (event: FreshnessEvent, engine: FreshnessEngineResult) =>
  engine.engine === 'pintail'
    ? 'Pintail'
    : event.mode === 'ttl'
      ? 'CH (TTL)'
      : 'CH (drop)'

/// One row per event, engine and affected query.
export function renderFreshnessTable(study: CacheStudy): string[] {
  const rows: string[] = []
  for (const event of study.freshness) {
    for (const engine of event.engines) {
      for (const query of engine.queries) {
        const steady = query.after
          ? `${ms(query.after.medianMs)} (${query.afterHits}/${query.afterSamples} hits)`
          : 'n/a'
        rows.push(
          `| ${event.kind} | ${engineName(event, engine)} | ${label(query.query)}${query.affected ? '' : ' (control)'} | ` +
            `${query.warmHit === null ? 'n/a' : query.warmHit ? 'yes' : 'no'} | ${ms(engine.visibleAtMs)} | ` +
            `${query.censored ? 'TIMEOUT' : query.affected ? ms(query.firstCorrectAtMs) : 'unchanged'} | ` +
            `${query.staleAfterVisible}${query.staleAfterVisibleHits ? ` (${query.staleAfterVisibleHits} hits)` : ''} | ` +
            `${query.staleBeforeVisible} | ${query.wrong} | ` +
            `${query.affected ? ms(query.firstCorrectLatencyMs) : 'n/a'} | ${steady} |`,
        )
      }
    }
  }
  return [
    '| Change | Engine | Query | Warm hit | Data visible | First correct | Stale after visible | Old before visible | Wrong | First correct took | After: median (hits) |',
    '|---|---|---|:-:|---:|---:|---:|---:|---:|---:|---:|',
    ...rows,
  ]
}

export function renderCoordinationTable(study: CacheStudy): string[] {
  const events = study.freshness.filter((event) => event.coordination)
  if (events.length === 0) return []
  return [
    '| Change | CH rows applied | Visible to coordinator | Probes | Probe time | Drop | Cache dropped at |',
    '|---|---:|---:|---:|---:|---:|---:|',
    ...events.map((event) => {
      const c = event.coordination!
      return (
        `| ${event.kind} | ${ms(event.clickhouseAppliedAtMs)} | ${ms(c.visibleAtMs)} | ${c.probes} | ` +
        `${ms(c.probeMs)} | ${ms(c.dropMs)} | ${ms(c.droppedAtMs)} |`
      )
    }),
  ]
}

/// The `results.md` section: every mode, its settings, and its tables.
export function renderCacheStudyMarkdown(study: CacheStudy): string[] {
  const s = study.settings
  return [
    '## Cached reads, both caches on (issue #36)',
    '',
    "Pintail's settled memo against ClickHouse's query cache, both warmed",
    `before timing. ClickHouse reads RMT+FINAL with \`${s.clickhouse.perQuery}\``,
    `(TTL ${s.clickhouse.queryCacheTtlStaticS} s, longer than the timed phase). Every`,
    'timed answer is compared with MySQL, and the hit columns count timed',
    "answers proved to be cache hits: Pintail's `pintail_settled_memo_hits_total`",
    "on `/metrics`, ClickHouse's `QueryCacheHits` in `system.query_log`. A shape",
    'a cache did not serve says `not cached`; its time is an execution.',
    `${s.warmups} warmups, ${s.runs} timed runs per engine per query.`,
    '',
    ...renderStaticTable(study),
    '',
    '## Freshness after ingestion',
    '',
    'Both caches warm, then one deterministic change commits to MySQL and',
    `readers poll every ${s.pollMs} ms for up to ${s.timeoutS} s. Each answer is`,
    "classified against MySQL's answer before and after. *Data visible* is when",
    "a row-level read of the changed keys, which no result cache serves, first",
    "matches MySQL: before it an old answer is replication delay, after it an",
    'old answer is the cache serving stale data. ClickHouse is written by the',
    'harness the moment MySQL commits (no pipeline), so its delay is a floor.',
    `ClickHouse TTL mode uses \`query_cache_ttl = ${s.clickhouse.queryCacheTtlFreshnessS}\`;`,
    'drop mode issues `SYSTEM DROP QUERY CACHE` once a probe sees the change.',
    'A timed-out query is reported as TIMEOUT, never as a fresh read. *Warm hit*',
    "says whether the engine's last answer before the change was a proved cache",
    'hit; `no` means that engine was executing, not caching, going into the change.',
    'Visibility is sampled once per poll, so it carries up to one poll of error.',
    '',
    ...renderFreshnessTable(study),
    '',
    ...(renderCoordinationTable(study).length > 0
      ? ['ClickHouse drop-mode coordination cost:', '', ...renderCoordinationTable(study), '']
      : []),
  ]
}

/// The README's compact form.
export function renderCacheStudyReadme(study: CacheStudy): string[] {
  const staleAfter = (engine: 'pintail' | 'clickhouse', mode?: 'ttl' | 'drop') =>
    study.freshness
      .filter((event) => mode === undefined || event.mode === mode)
      .flatMap((event) => event.engines.filter((entry) => entry.engine === engine))
      .flatMap((entry) => entry.queries)
      .reduce((total, query) => total + query.staleAfterVisible, 0)
  const pintailWarm = study.freshness.filter((event) =>
    event.engines.some(
      (entry) =>
        entry.engine === 'pintail' && entry.queries.every((query) => query.warmHit !== false),
    ),
  ).length
  return [
    '**Repeated queries — both caches on.** Pintail\'s settled memo against',
    "ClickHouse's query cache, both warmed, every timed answer proved a hit and",
    "checked against MySQL. The executed RMT+FINAL column is the uncached",
    'reference.',
    '',
    ...renderStaticTable(study),
    '',
    '**After a change.** With the caches warm, an insert, update or delete',
    'commits to MySQL and readers keep polling. Stale answers served after the',
    `change was visible in the engine's own data: Pintail ${staleAfter('pintail')},`,
    `ClickHouse TTL mode ${staleAfter('clickhouse', 'ttl')}, ClickHouse with an explicit`,
    `cache drop ${staleAfter('clickhouse', 'drop')}. Pintail's memo was warm going into`,
    `${pintailWarm} of ${study.freshness.length} changes and executed otherwise. Per-event timings are in`,
    '[benchmark/results.md](benchmark/results.md).',
    '',
  ]
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

export type StudyQuery = { name: string; sql: string; clickhouseSql?: string }

export type CacheStudyContext = {
  log: (message: string) => void
  canonical: (rows: unknown[][]) => string
  mysql: (sql: string) => Promise<unknown[][]>
  /// A source write, never retried (a retry could apply it twice); resolves
  /// to the rows it changed.
  mysqlWrite: (sql: string) => Promise<number>
  pintail: (sql: string) => Promise<unknown[][]>
  /// Settled memo hits so far, from `/metrics`.
  pintailMemoHits: () => Promise<number>
  /// A ClickHouse read; `queryId` tags it for the `system.query_log` lookup.
  clickhouse: (sql: string, queryId?: string) => Promise<unknown[][]>
  /// A ClickHouse statement with no result set.
  clickhouseExec: (sql: string) => Promise<void>
  /// ClickHouse's own definition of each mirrored table, and the
  /// projection that reads it from MySQL's table function.
  clickhouseTables: Record<string, { schema: string; projection: string }>
  /// `mysql('<host>:3306', 'benchmark_db', '<table>', user, password)`.
  mysqlTableFunction: (table: string) => string
  orderRows: number
  runTag: string
}

export type CacheStudyOptions = {
  warmups: number
  runs: number
  staticTtlS: number
  freshnessTtlS: number
  pollMs: number
  timeoutS: number
  afterSamples: number
  pintailSettleS: number
}

const timed = async <T>(operation: () => Promise<T>) => {
  const started = performance.now()
  const value = await operation()
  return { value, ms: performance.now() - started }
}

/// Query ids whose `QueryCacheHits` was non-zero, out of those asked.
async function clickhouseHitsByQueryId(
  context: CacheStudyContext,
  prefix: string,
): Promise<Map<string, boolean>> {
  await context.clickhouseExec('SYSTEM FLUSH LOGS')
  const rows = await context.clickhouse(
    "SELECT query_id, ProfileEvents['QueryCacheHits'] > 0 FROM system.query_log " +
      `WHERE type = 'QueryFinish' AND startsWith(query_id, '${prefix}')`,
  )
  return new Map(rows.map((row) => [String(row[0]), Number(row[1]) === 1]))
}

async function staticPhase(
  context: CacheStudyContext,
  options: CacheStudyOptions,
  queries: StudyQuery[],
  expected: (name: string) => string,
): Promise<StaticRow[]> {
  const settings = ` SETTINGS final = 1, ${clickhouseCacheSettings(options.staticTtlS)}`
  const prefix = `${context.runTag}-static-`
  // A clean cache, so the first warmup of each query is its write.
  await context.clickhouseExec('SYSTEM DROP QUERY CACHE')
  const rows: StaticRow[] = []
  const clickhouseIds: Array<{ warm: string[]; timed: string[] }> = []
  for (const [index, query] of queries.entries()) {
    context.log(`cache study (static): ${query.name}`)
    const want = expected(query.name)
    const clickhouseSql = query.clickhouseSql ?? query.sql
    // Pintail: hits counted around every answer, outside its timing.
    let pintailWarmupMisses = 0
    let pintailExact = true
    for (let warm = 0; warm < options.warmups; warm += 1) {
      const before = await context.pintailMemoHits()
      const answer = await context.pintail(query.sql)
      if ((await context.pintailMemoHits()) === before) pintailWarmupMisses += 1
      pintailExact &&= context.canonical(answer) === want
    }
    const pintailTimes: number[] = []
    let pintailHits = 0
    for (let run = 0; run < options.runs; run += 1) {
      const before = await context.pintailMemoHits()
      const sample = await timed(() => context.pintail(query.sql))
      if ((await context.pintailMemoHits()) > before) pintailHits += 1
      pintailTimes.push(sample.ms)
      pintailExact &&= context.canonical(sample.value) === want
    }
    // ClickHouse cached, then the same statement executed with the cache off.
    const ids = { warm: [] as string[], timed: [] as string[] }
    let clickhouseExact = true
    for (let warm = 0; warm < options.warmups; warm += 1) {
      const id = `${prefix}${index}-w${warm}`
      ids.warm.push(id)
      const answer = await context.clickhouse(`${clickhouseSql}${settings}`, id)
      clickhouseExact &&= context.canonical(answer) === want
    }
    const clickhouseTimes: number[] = []
    for (let run = 0; run < options.runs; run += 1) {
      const id = `${prefix}${index}-r${run}`
      ids.timed.push(id)
      const sample = await timed(() => context.clickhouse(`${clickhouseSql}${settings}`, id))
      clickhouseTimes.push(sample.ms)
      clickhouseExact &&= context.canonical(sample.value) === want
    }
    clickhouseIds.push(ids)
    const executedTimes: number[] = []
    let executedExact = true
    const uncached = ' SETTINGS final = 1, use_query_cache = 0'
    for (let warm = 0; warm < options.warmups; warm += 1) {
      await context.clickhouse(`${clickhouseSql}${uncached}`)
    }
    for (let run = 0; run < options.runs; run += 1) {
      const sample = await timed(() => context.clickhouse(`${clickhouseSql}${uncached}`))
      executedTimes.push(sample.ms)
      executedExact &&= context.canonical(sample.value) === want
    }
    rows.push({
      name: query.name,
      exact: { pintail: pintailExact, clickhouse: clickhouseExact, clickhouseExecuted: executedExact },
      pintailHits,
      clickhouseHits: 0,
      runs: options.runs,
      pintailWarmupMisses,
      clickhouseWarmupMisses: 0,
      pintailCached: summarize(pintailTimes),
      clickhouseCached: summarize(clickhouseTimes),
      clickhouseExecuted: summarize(executedTimes),
    })
  }
  const hits = await clickhouseHitsByQueryId(context, prefix)
  for (const [index, row] of rows.entries()) {
    const ids = clickhouseIds[index]
    row.clickhouseHits = ids.timed.filter((id) => hits.get(id) === true).length
    row.clickhouseWarmupMisses = ids.warm.filter((id) => hits.get(id) === false).length
    if (row.pintailHits === 0) {
      row.pintailNote =
        'no settled-memo hit: this shape is answered without the memo (for a bare COUNT(*), from table statistics)'
    } else if (row.pintailHits < row.runs) {
      row.pintailNote = `${row.runs - row.pintailHits} timed answers were not memo hits`
    }
    if (row.clickhouseHits === 0) row.clickhouseNote = 'no query-cache hit recorded'
    else if (row.clickhouseHits < row.runs) {
      row.clickhouseNote = `${row.runs - row.clickhouseHits} timed answers were not cache hits`
    }
    context.log(
      `  ${row.name}: Pintail ${row.pintailCached.medianMs} ms (${row.pintailHits}/${row.runs} memo hits) | ` +
        `CH cached ${row.clickhouseCached.medianMs} ms (${row.clickhouseHits}/${row.runs} hits) | ` +
        `CH executed ${row.clickhouseExecuted.medianMs} ms`,
    )
  }
  return rows
}

/// The freshness fixture: a keyed band of orders the event creates,
/// rewrites or removes. Invented values, deterministic per event.
function eventBand(
  kind: FreshnessEvent['kind'],
  index: number,
  orderRows: number,
  maxId: number,
  band: number,
) {
  // Above the highest key: the seed's auto-increment leaves gaps, so the
  // row count is not the top of the key space.
  if (kind === 'insert') {
    const low = maxId + 1 + index * band
    return [low, low + band - 1] as [number, number]
  }
  if (kind === 'update') {
    const low = 1 + index * band
    return [low, low + band - 1] as [number, number]
  }
  const low = Math.floor(orderRows / 2) + 1 + index * band
  return [low, low + band - 1] as [number, number]
}

function mysqlChange(kind: FreshnessEvent['kind'], [low, high]: [number, number]): string {
  if (kind === 'insert') {
    return (
      'INSERT INTO orders (id, user_id, product_id, quantity, unit_price, total_amount, ' +
      'status, region, order_date, created_at, updated_at) ' +
      `WITH RECURSIVE n (i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM n WHERE i < ${high - low}) ` +
      `SELECT ${low} + i, 1 + MOD(i * 37, 1000), 1 + MOD(i * 11, 100), 1 + MOD(i, 5), 19.99, ` +
      "19.99 * (1 + MOD(i, 5)), 'pending', 'eu-west', '2023-06-15', " +
      "'2023-06-15 12:00:00', '2023-06-15 12:00:00' FROM n"
    )
  }
  if (kind === 'update') {
    return (
      "UPDATE orders SET status = 'returned', total_amount = total_amount + 1.00, " +
      `updated_at = '2026-01-01 00:00:00' WHERE id BETWEEN ${low} AND ${high}`
    )
  }
  return `DELETE FROM orders WHERE id BETWEEN ${low} AND ${high}`
}

const FRESHNESS_DATABASE = 'benchmark_cdc'

/// Served queries for the freshness phase. The ClickHouse text names the
/// versioned database explicitly so its cache entries never share a key
/// with the static phase's.
export const freshnessQueries: Array<StudyQuery & { clickhouseSql: string }> = [
  {
    name: 'F1: Count and total',
    sql: 'SELECT COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total FROM orders',
    clickhouseSql: `SELECT COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total FROM ${FRESHNESS_DATABASE}.orders`,
  },
  {
    name: 'F2: Group by status',
    sql: 'SELECT status, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt FROM orders GROUP BY status ORDER BY cnt DESC',
    clickhouseSql: `SELECT status, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt FROM ${FRESHNESS_DATABASE}.orders GROUP BY status ORDER BY cnt DESC`,
  },
  {
    name: 'C1: Users by region (unchanged table)',
    sql: 'SELECT region, COUNT(*) AS cnt FROM users GROUP BY region ORDER BY region',
    clickhouseSql: `SELECT region, COUNT(*) AS cnt FROM ${FRESHNESS_DATABASE}.users GROUP BY region ORDER BY region`,
  },
]

const probeSql = ([low, high]: [number, number], database?: string) =>
  `SELECT id, status, total_amount FROM ${database ? `${database}.` : ''}orders ` +
  `WHERE id BETWEEN ${low} AND ${high} ORDER BY id`

async function setUpFreshnessReplica(context: CacheStudyContext) {
  context.log(`cache study: building the versioned ClickHouse copy (${FRESHNESS_DATABASE})`)
  await context.clickhouseExec(`DROP DATABASE IF EXISTS ${FRESHNESS_DATABASE}`)
  await context.clickhouseExec(`CREATE DATABASE ${FRESHNESS_DATABASE}`)
  for (const table of ['orders', 'users']) {
    const definition = context.clickhouseTables[table]
    // A version and a deleted flag per row, the shape a CDC pipeline keeps:
    // FINAL returns the newest version of each key and drops deleted ones.
    await context.clickhouseExec(
      `CREATE TABLE ${FRESHNESS_DATABASE}.${table} (${definition.schema}, ` +
        '_version UInt64, _is_deleted UInt8) ' +
        'ENGINE = ReplacingMergeTree(_version, _is_deleted) ORDER BY id',
    )
    await context.clickhouseExec(
      `INSERT INTO ${FRESHNESS_DATABASE}.${table} SELECT *, 1, 0 FROM benchmark_rmt.${table}`,
    )
    await context.clickhouseExec(`OPTIMIZE TABLE ${FRESHNESS_DATABASE}.${table} FINAL`)
  }
}

async function applyClickhouseChange(
  context: CacheStudyContext,
  kind: FreshnessEvent['kind'],
  [low, high]: [number, number],
  version: number,
) {
  if (kind === 'delete') {
    await context.clickhouseExec(
      `INSERT INTO ${FRESHNESS_DATABASE}.orders SELECT * EXCEPT (_version, _is_deleted), ${version}, 1 ` +
        `FROM ${FRESHNESS_DATABASE}.orders FINAL WHERE id BETWEEN ${low} AND ${high}`,
    )
    return
  }
  await context.clickhouseExec(
    `INSERT INTO ${FRESHNESS_DATABASE}.orders SELECT ${context.clickhouseTables.orders.projection}, ` +
      `${version}, 0 FROM ${context.mysqlTableFunction('orders')} WHERE id BETWEEN ${low} AND ${high}`,
  )
}

type Observer = {
  visibleAtMs: number | null
  probes: number
  samples: ServedSample[][]
  /// ClickHouse query ids per sample, resolved to hits after the event.
  ids: string[][]
}

/// Polls one engine until every affected query has a correct answer and
/// `afterSamples` more, or the timeout. Each cycle probes the changed keys
/// (until they match) and then asks every served query once.
async function observe(
  context: CacheStudyContext,
  options: CacheStudyOptions,
  engine: 'pintail' | 'clickhouse',
  t0: number,
  probe: { mysql: string; expected: string },
  answers: Array<{ before: string; after: string; affected: boolean }>,
  settings: string,
  idPrefix: string,
): Promise<Observer> {
  const observer: Observer = {
    visibleAtMs: null,
    probes: 0,
    samples: answers.map(() => []),
    ids: answers.map(() => []),
  }
  const deadline = t0 + options.timeoutS * 1000
  let cycle = 0
  const done = () =>
    answers.every((answer, index) => {
      const samples = observer.samples[index]
      const first = samples.findIndex((sample) => sample.v === 'new')
      if (!answer.affected) return samples.length >= options.afterSamples
      return first !== -1 && samples.length - first - 1 >= options.afterSamples
    })
  while (performance.now() < deadline && !done()) {
    const cycleStart = performance.now()
    if (observer.visibleAtMs === null) {
      observer.probes += 1
      const rows =
        engine === 'pintail'
          ? await context.pintail(probe.mysql)
          : await context.clickhouse(
              `${probeSql(probeRange(probe.mysql), FRESHNESS_DATABASE)} SETTINGS final = 1, use_query_cache = 0`,
            )
      if (context.canonical(rows) === probe.expected) observer.visibleAtMs = performance.now() - t0
    }
    for (const [index, query] of freshnessQueries.entries()) {
      const answer = answers[index]
      const afterVisible = observer.visibleAtMs !== null
      const t = performance.now() - t0
      if (engine === 'pintail') {
        const before = await context.pintailMemoHits()
        const sample = await timed(() => context.pintail(query.sql))
        const hit = (await context.pintailMemoHits()) > before
        observer.samples[index].push({
          t,
          ms: sample.ms,
          v: classify(context.canonical(sample.value), answer.before, answer.after),
          hit,
          afterVisible,
        })
      } else {
        const id = `${idPrefix}-${index}-${cycle}`
        const sample = await timed(() => context.clickhouse(`${query.clickhouseSql}${settings}`, id))
        observer.ids[index].push(id)
        observer.samples[index].push({
          t,
          ms: sample.ms,
          v: classify(context.canonical(sample.value), answer.before, answer.after),
          hit: null,
          afterVisible,
        })
      }
    }
    cycle += 1
    const elapsed = performance.now() - cycleStart
    if (elapsed < options.pollMs) await Bun.sleep(options.pollMs - elapsed)
  }
  return observer
}

const probeRange = (sql: string): [number, number] => {
  const match = sql.match(/BETWEEN (\d+) AND (\d+)/)
  if (!match) throw new Error(`probe has no key range: ${sql}`)
  return [Number(match[1]), Number(match[2])]
}

async function freshnessEvent(
  context: CacheStudyContext,
  options: CacheStudyOptions,
  mode: FreshnessEvent['mode'],
  kind: FreshnessEvent['kind'],
  index: number,
  band: number,
  maxId: number,
): Promise<FreshnessEvent> {
  const range = eventBand(kind, index, context.orderRows, maxId, band)
  const settings = ` SETTINGS final = 1, ${clickhouseCacheSettings(options.freshnessTtlS)}`
  const tag = `${context.runTag}-fresh-${index}`
  context.log(`cache study (freshness): ${mode} ${kind} keys ${range[0]}-${range[1]}`)
  const before = await Promise.all(freshnessQueries.map((query) => context.mysql(query.sql)))
  const beforeCanonical = before.map((rows) => context.canonical(rows))

  // MySQL's answers after the change, taken inside a rehearsal of it that
  // is rolled back (a rolled-back transaction never reaches the binlog), so
  // the oracle is ready before the real commit and the readers start the
  // moment it returns instead of after MySQL's own aggregate queries.
  await context.mysqlWrite('START TRANSACTION')
  let rehearsedRows: number
  let afterCanonical: string[]
  let expectedProbe: string
  try {
    rehearsedRows = await context.mysqlWrite(mysqlChange(kind, range))
    afterCanonical = []
    for (const query of freshnessQueries) afterCanonical.push(context.canonical(await context.mysql(query.sql)))
    expectedProbe = context.canonical(await context.mysql(probeSql(range)))
  } finally {
    await context.mysqlWrite('ROLLBACK')
  }
  const unchanged = context.canonical(await context.mysql(freshnessQueries[0].sql))
  if (unchanged !== beforeCanonical[0]) throw new Error('the rehearsal of the change was not rolled back')

  // Warm Pintail: its memo serves a settled table only, so after the
  // previous event this waits for the replica to flush and settle.
  const warmStarted = performance.now()
  const pintailWarm: Array<boolean> = freshnessQueries.map(() => false)
  while (performance.now() - warmStarted < options.pintailSettleS * 1000) {
    for (const [queryIndex, query] of freshnessQueries.entries()) {
      if (pintailWarm[queryIndex]) continue
      const hitsBefore = await context.pintailMemoHits()
      const answer = await context.pintail(query.sql)
      const correct = context.canonical(answer) === beforeCanonical[queryIndex]
      pintailWarm[queryIndex] = correct && (await context.pintailMemoHits()) > hitsBefore
    }
    if (pintailWarm.every(Boolean)) break
    await Bun.sleep(250)
  }
  const pintailWarmWaitMs = Math.round(performance.now() - warmStarted)

  // Warm ClickHouse: a clean cache, then two reads each; the second is the
  // proof of a hit. Written last so the entries are as young as possible
  // when the change commits.
  await context.clickhouseExec('SYSTEM DROP QUERY CACHE')
  const clickhouseWarmIds: string[] = []
  const warmWrittenAt = performance.now()
  for (const [queryIndex, query] of freshnessQueries.entries()) {
    for (let warm = 0; warm < 2; warm += 1) {
      const id = `${tag}-warm-${queryIndex}-${warm}`
      const answer = await context.clickhouse(`${query.clickhouseSql}${settings}`, id)
      if (context.canonical(answer) !== beforeCanonical[queryIndex]) {
        throw new Error(`ClickHouse disagrees with MySQL before the ${kind} on ${query.name}`)
      }
      if (warm === 1) clickhouseWarmIds.push(id)
    }
  }

  const commit = await timed(() => context.mysqlWrite(mysqlChange(kind, range)))
  const t0 = performance.now()
  if (commit.value === 0 || commit.value !== rehearsedRows) {
    throw new Error(`the ${kind} of keys ${range[0]}-${range[1]} changed ${commit.value} rows, rehearsed ${rehearsedRows}`)
  }
  const clickhouseEntryAgeMs = Math.round(t0 - warmWrittenAt)
  const answers = freshnessQueries.map((_, queryIndex) => ({
    before: beforeCanonical[queryIndex],
    after: afterCanonical[queryIndex],
    affected: beforeCanonical[queryIndex] !== afterCanonical[queryIndex],
  }))

  let clickhouseAppliedAtMs = 0
  let clickhouseApplied: () => void = () => {}
  const appliedSignal = new Promise<void>((resolve) => {
    clickhouseApplied = resolve
  })
  const mirror = (async () => {
    await applyClickhouseChange(context, kind, range, 2 + index)
    clickhouseAppliedAtMs = performance.now() - t0
    clickhouseApplied()
  })()
  const coordinator =
    mode === 'drop'
      ? (async () => {
          await appliedSignal
          // The coordinator cannot know the rows landed from the write
          // alone in a real pipeline: it reads the changed keys, cache off,
          // until they match, and only then drops the cache.
          let probes = 0
          let probeMs = 0
          let visibleAtMs = 0
          const deadline = t0 + options.timeoutS * 1000
          while (performance.now() < deadline) {
            probes += 1
            const sample = await timed(() =>
              context.clickhouse(
                `${probeSql(range, FRESHNESS_DATABASE)} SETTINGS final = 1, use_query_cache = 0`,
              ),
            )
            probeMs += sample.ms
            if (context.canonical(sample.value) === expectedProbe) {
              visibleAtMs = performance.now() - t0
              break
            }
            await Bun.sleep(10)
          }
          const drop = await timed(() => context.clickhouseExec('SYSTEM DROP QUERY CACHE'))
          return {
            visibleAtMs: round1(visibleAtMs),
            probes,
            probeMs: round1(probeMs),
            dropMs: round1(drop.ms),
            droppedAtMs: round1(performance.now() - t0),
          }
        })()
      : Promise.resolve(undefined)
  const probe = { mysql: probeSql(range), expected: expectedProbe }
  const [pintail, clickhouse, , coordination] = await Promise.all([
    observe(context, options, 'pintail', t0, probe, answers, '', `${tag}-p`),
    observe(context, options, 'clickhouse', t0, probe, answers, settings, `${tag}-c`),
    mirror,
    coordinator,
  ])
  const hits = await clickhouseHitsByQueryId(context, tag)
  for (const [queryIndex, ids] of clickhouse.ids.entries()) {
    for (const [sampleIndex, id] of ids.entries()) {
      const hit = hits.get(id)
      clickhouse.samples[queryIndex][sampleIndex].hit = hit === undefined ? null : hit
    }
  }
  const engines: FreshnessEngineResult[] = [
    { engine: 'pintail', observer: pintail, warm: pintailWarm.map((hit) => hit as boolean | null) },
    {
      engine: 'clickhouse',
      observer: clickhouse,
      warm: clickhouseWarmIds.map((id) => hits.get(id) ?? null),
    },
  ].map(({ engine, observer, warm }) => ({
    engine: engine as FreshnessEngineResult['engine'],
    visibleAtMs: observer.visibleAtMs === null ? null : round1(observer.visibleAtMs),
    probes: observer.probes,
    queries: freshnessQueries.map((query, queryIndex) =>
      summarizeFreshness(
        query.name,
        answers[queryIndex].affected,
        warm[queryIndex] ?? null,
        observer.samples[queryIndex],
        observer.visibleAtMs,
      ),
    ),
  }))
  for (const engine of engines) {
    for (const query of engine.queries) {
      context.log(
        `  ${engine.engine} ${query.query}: visible ${engine.visibleAtMs} ms, first correct ` +
          `${query.firstCorrectAtMs} ms, stale after visible ${query.staleAfterVisible}, ` +
          `old before visible ${query.staleBeforeVisible}, wrong ${query.wrong}` +
          `${query.censored ? ', TIMEOUT' : ''}`,
      )
    }
  }
  return {
    mode,
    kind,
    rows: commit.value,
    keyRange: range,
    mysqlCommitMs: round1(commit.ms),
    clickhouseAppliedAtMs: round1(clickhouseAppliedAtMs),
    clickhouseEntryAgeMs,
    ...(coordination ? { coordination } : {}),
    pintailWarmWaitMs,
    engines,
  }
}

/// Runs the static phase (read-only; safe on the shared replica).
export async function runStaticCacheStudy(
  context: CacheStudyContext,
  options: CacheStudyOptions,
  queries: StudyQuery[],
  expected: (name: string) => string,
): Promise<StaticRow[]> {
  return staticPhase(context, options, queries, expected)
}

/// Runs the freshness phase. It changes the source, so it must be the last
/// thing a run measures.
export async function runFreshnessCacheStudy(
  context: CacheStudyContext,
  options: CacheStudyOptions,
): Promise<FreshnessEvent[]> {
  await setUpFreshnessReplica(context)
  const band = Math.max(10, Math.min(1000, Math.floor(context.orderRows / 40)))
  const maxId = Number((await context.mysql('SELECT MAX(id) FROM orders'))[0][0])
  const events: FreshnessEvent[] = []
  let index = 0
  for (const mode of ['ttl', 'drop'] as const) {
    for (const kind of ['insert', 'update', 'delete'] as const) {
      events.push(await freshnessEvent(context, options, mode, kind, index, band, maxId))
      index += 1
    }
  }
  return events
}

export async function clickhouseQueryCacheServerSettings(
  context: CacheStudyContext,
): Promise<Record<string, string>> {
  const rows = await context.clickhouse(
    "SELECT name, value FROM system.server_settings WHERE startsWith(name, 'query_cache') ORDER BY name",
  )
  return Object.fromEntries(rows.map((row) => [String(row[0]), String(row[1])]))
}

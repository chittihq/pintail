import { expect, test } from 'bun:test'
import {
  classify,
  clickhouseCacheSettings,
  prometheusValue,
  renderCacheStudyMarkdown,
  renderCacheStudyReadme,
  summarizeFreshness,
  type CacheStudy,
  type ServedSample,
} from './cache-study'

const sample = (t: number, v: ServedSample['v'], hit: boolean | null, afterVisible: boolean): ServedSample => ({
  t,
  ms: 2,
  v,
  hit,
  afterVisible,
})

test('an answer is classified against both oracle answers, never by timing', () => {
  expect(classify('b', 'a', 'b')).toBe('new')
  expect(classify('a', 'a', 'b')).toBe('old')
  expect(classify('c', 'a', 'b')).toBe('other')
})

test('stale answers after visibility are cache staleness, before it replication delay', () => {
  const samples = [
    sample(0, 'old', true, false),
    sample(100, 'old', true, false),
    sample(200, 'old', true, true),
    sample(300, 'old', true, true),
    sample(400, 'new', false, true),
    sample(500, 'new', true, true),
    sample(600, 'new', true, true),
  ]
  const result = summarizeFreshness('F1', true, true, samples, 150)
  expect(result.staleBeforeVisible).toBe(2)
  expect(result.staleAfterVisible).toBe(2)
  expect(result.staleAfterVisibleHits).toBe(2)
  expect(result.firstCorrectAtMs).toBe(402)
  expect(result.firstCorrectHit).toBe(false)
  expect(result.cacheLagMs).toBe(252)
  expect(result.afterHits).toBe(2)
  expect(result.afterSamples).toBe(2)
  expect(result.censored).toBe(false)
  expect(result.lastStaleAtMs).toBe(300)
})

test('no correct answer within the timeout is censored, not fresh', () => {
  const result = summarizeFreshness('F1', true, true, [sample(0, 'old', true, true)], 0)
  expect(result.censored).toBe(true)
  expect(result.firstCorrectAtMs).toBeNull()
  // An unaffected control has nothing to become correct, so it never censors.
  expect(summarizeFreshness('C1', false, true, [sample(0, 'new', true, true)], 0).censored).toBe(false)
})

test('a correct answer that flips back to old counts as stale, not as steady state', () => {
  const result = summarizeFreshness(
    'F2',
    true,
    true,
    [sample(0, 'new', false, true), sample(10, 'old', true, true), sample(20, 'new', true, true)],
    0,
  )
  expect(result.staleAfterVisible).toBe(1)
  expect(result.afterSamples).toBe(1)
})

test('the cached ClickHouse read carries every eligibility setting', () => {
  const settings = clickhouseCacheSettings(60)
  expect(settings).toContain('use_query_cache = 1')
  expect(settings).toContain('query_cache_ttl = 60')
  expect(settings).toContain('query_cache_min_query_runs = 0')
  expect(settings).toContain('query_cache_min_query_duration = 0')
})

test('memo counters are read from the Prometheus exposition', () => {
  const text = '# HELP x\n# TYPE pintail_settled_memo_hits_total counter\npintail_settled_memo_hits_total 42\n'
  expect(prometheusValue(text, 'pintail_settled_memo_hits_total')).toBe(42)
  expect(() => prometheusValue(text, 'missing_total')).toThrow()
})

const timing = { medianMs: 1.5, p95Ms: 2, minMs: 1, runs: 3 }

const study: CacheStudy = {
  generatedAt: '2026-10-06T00:00:00.000Z',
  settings: {
    clickhouse: { queryCacheTtlStaticS: 3600, queryCacheTtlFreshnessS: 60, perQuery: clickhouseCacheSettings(3600), server: {} },
    pintail: { memo: 'settled', maxEntries: 32, maxRows: 131072 },
    warmups: 2,
    runs: 3,
    pollMs: 100,
    timeoutS: 120,
    afterSamples: 5,
    order: 'fixed',
  },
  static: [
    {
      name: 'Q1: Full table count',
      exact: { pintail: true, clickhouse: true, clickhouseExecuted: true },
      pintailHits: 0,
      clickhouseHits: 3,
      runs: 3,
      pintailWarmupMisses: 2,
      clickhouseWarmupMisses: 1,
      pintailCached: timing,
      clickhouseCached: timing,
      clickhouseExecuted: { ...timing, medianMs: 40 },
    },
  ],
  freshness: [
    {
      mode: 'ttl',
      kind: 'update',
      rows: 10,
      keyRange: [1, 10],
      mysqlCommitMs: 3,
      clickhouseAppliedAtMs: 20,
      clickhouseEntryAgeMs: 50,
      pintailWarmWaitMs: 0,
      engines: [
        {
          engine: 'clickhouse',
          visibleAtMs: 25,
          probes: 1,
          queries: [summarizeFreshness('F1: Count and total', true, true, [sample(30, 'old', true, true)], 25)],
        },
      ],
    },
  ],
}

test('rendering marks uncached shapes, stale reads and timeouts explicitly', () => {
  const markdown = renderCacheStudyMarkdown(study).join('\n')
  expect(markdown).toContain('(not cached)')
  expect(markdown).toContain('| 0/3 |')
  expect(markdown).toContain('TIMEOUT')
  expect(markdown).toContain('CH (TTL)')
  const readme = renderCacheStudyReadme(study).join('\n')
  expect(readme).toContain('ClickHouse TTL mode 1')
})

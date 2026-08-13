// commerce-production-v1 workload manifest.
// Consumed by benchmark/run-production.ts. Pure data — no side effects.

export type ParamSpec =
  | { kind: 'zipfTenant' }
  | { kind: 'zipfCustomer' }
  | { kind: 'now' }
  | { kind: 'daysAgo'; choices: number[] }

export interface QuerySpec {
  id: string
  class: string
  sqlFile: string
  weight: number
  requiresWindowFunctions: boolean
  /// A MySQL behaviour this engine does not implement yet, named here BEFORE
  /// the run rather than explained after it. A declared gap warns; anything
  /// else that fails to execute fails the gate. The distinction is the whole
  /// point: "we knew and wrote it down" is evidence, "it errored again" is
  /// not.
  declaredGap?: string
  params: Record<string, ParamSpec>
  resultComparison: 'ordered' | 'unordered'
  latencySlaMs: { median: number; p95: number }
}

export interface Phase {
  id: string
  action:
    | 'seed-and-snapshot'
    | 'query-suite'
    | 'cdc-and-query'
    | 'compact-and-query'
    | 'kill-restart-and-validate'
  runs?: number
  warmups?: number
  durationSeconds?: number
  writers?: number
  readers?: number
}

export default {
  id: 'commerce-production-v1',
  seed: 42,

  profiles: {
    // scale multiplies every row count in production-profile.json
    smoke: { scale: 0.0001 },
    ci: { scale: 0.01 },
    full: { scale: 1 },
  },

  profileFile: './production-profile.json',

  phases: [
    { id: 'snapshot', action: 'seed-and-snapshot' },
    { id: 'cold', action: 'query-suite', runs: 3 },
    { id: 'warm', action: 'query-suite', warmups: 2, runs: 7 },
    // Two ingestion rates, not one. A single rate reports whether queries
    // stayed fast under that load and cannot say whether they degrade with it,
    // which is the question a replica is actually asked. The light pass runs
    // briefly because it exists to be compared against the heavy one.
    {
      id: 'mixed-light',
      action: 'cdc-and-query',
      durationSeconds: 300,
      writers: 2,
      readers: 16,
    },
    {
      id: 'mixed',
      action: 'cdc-and-query',
      durationSeconds: 1800,
      writers: 8,
      readers: 16,
    },
    { id: 'post-compaction', action: 'compact-and-query', runs: 7 },
    { id: 'restart', action: 'kill-restart-and-validate' },
  ] satisfies Phase[],

  gates: {
    exactResults: true,
    maximumDlq: 0,
    maximumReplicationLagSeconds: 5,
    reportMedian: true,
    reportP95: true,
    reportVariance: true,
  },

  queries: [
    {
      id: 'q01-tenant-revenue',
      class: 'executive-dashboard',
      sqlFile: './queries/q01-tenant-revenue.sql',
      weight: 15,
      requiresWindowFunctions: false,
      params: { tenantId: { kind: 'zipfTenant' }, windowStart: { kind: 'daysAgo', choices: [365] } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 250, p95: 750 },
    },
    {
      id: 'q02-customer-history',
      class: 'operational-lookup',
      sqlFile: './queries/q02-customer-history.sql',
      weight: 20,
      requiresWindowFunctions: false,
      params: { customerId: { kind: 'zipfCustomer' } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 50, p95: 200 },
    },
    {
      id: 'q03-fulfillment-backlog',
      class: 'operational-dashboard',
      sqlFile: './queries/q03-fulfillment-backlog.sql',
      weight: 12,
      requiresWindowFunctions: false,
      params: { tenantId: { kind: 'zipfTenant' }, now: { kind: 'now' } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 250, p95: 750 },
    },
    {
      id: 'q04-inventory-risk',
      class: 'operational-dashboard',
      sqlFile: './queries/q04-inventory-risk.sql',
      weight: 8,
      requiresWindowFunctions: false,
      params: { tenantId: { kind: 'zipfTenant' }, now: { kind: 'now' } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 500, p95: 1500 },
    },
    {
      id: 'q05-payment-failures',
      class: 'risk-analytics',
      sqlFile: './queries/q05-payment-failures.sql',
      weight: 10,
      requiresWindowFunctions: false,
      params: { now: { kind: 'now' } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 400, p95: 1200 },
    },
    {
      id: 'q06-refund-rate',
      class: 'quality-analytics',
      sqlFile: './queries/q06-refund-rate.sql',
      weight: 8,
      requiresWindowFunctions: false,
      params: { windowStart: { kind: 'daysAgo', choices: [90] } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 800, p95: 2400 },
    },
    {
      id: 'q07-product-performance',
      class: 'merchandising-analytics',
      sqlFile: './queries/q07-product-performance.sql',
      weight: 8,
      requiresWindowFunctions: true,
      params: { now: { kind: 'now' } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 800, p95: 2400 },
    },
    {
      id: 'q08-regional-cohorts',
      class: 'growth-analytics',
      sqlFile: './queries/q08-regional-cohorts.sql',
      weight: 6,
      requiresWindowFunctions: true,
      params: { windowStart: { kind: 'daysAgo', choices: [365] } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 1500, p95: 4000 },
    },
    {
      id: 'q09-order-lifecycle',
      class: 'lifecycle-analytics',
      sqlFile: './queries/q09-order-lifecycle.sql',
      weight: 6,
      requiresWindowFunctions: true,
      params: { windowStart: { kind: 'daysAgo', choices: [365] } },
      resultComparison: 'ordered',
      latencySlaMs: { median: 1500, p95: 4000 },
    },
    {
      id: 'q10-wide-operational-join',
      class: 'wide-join',
      sqlFile: './queries/q10-wide-operational-join.sql',
      weight: 7,
      requiresWindowFunctions: false,
      params: {
        tenantId: { kind: 'zipfTenant' },
        windowStart: { kind: 'daysAgo', choices: [30] },
        windowEnd: { kind: 'now' },
      },
      resultComparison: 'ordered',
      latencySlaMs: { median: 800, p95: 2400 },
    },
    {
      // Absence, not presence: the engine cannot early-exit on a match.
      id: 'q11-dormant-customers',
      class: 'anti-join',
      sqlFile: './queries/q11-dormant-customers.sql',
      weight: 5,
      requiresWindowFunctions: false,
      params: {
        tenantId: { kind: 'zipfTenant' },
        windowStart: { kind: 'daysAgo', choices: [30, 90] },
      },
      resultComparison: 'ordered',
      latencySlaMs: { median: 1200, p95: 3000 },
    },
    {
      // Hundreds of thousands of groups rather than a handful, so the cost
      // lands on the hash table rather than on the scan.
      id: 'q12-per-customer-revenue',
      class: 'high-cardinality-grouping',
      sqlFile: './queries/q12-per-customer-revenue.sql',
      weight: 6,
      requiresWindowFunctions: false,
      params: {
        tenantId: { kind: 'zipfTenant' },
        windowStart: { kind: 'daysAgo', choices: [30, 90, 365] },
        windowEnd: { kind: 'daysAgo', choices: [1] },
      },
      resultComparison: 'ordered',
      latencySlaMs: { median: 1500, p95: 4000 },
    },
  ] satisfies QuerySpec[],
}

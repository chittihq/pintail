// tpch-v1 workload manifest.
//
// A recognised suite, so the numbers can be read against results published
// elsewhere rather than only against our own synthetic commerce workload.
// Every query here is the specification's, unmodified except for a total
// ORDER BY where the standard leaves ties - the specification cares about the
// answer set, and a differential gate cares about the order too.
//
// Chosen for joins. The engine-speed track puts pintail furthest behind on
// exactly that work, and TPC-H is where a join benchmark is expected to be
// argued.
//
// ClickBench is deliberately absent. Its dataset is a single 100M-row table
// of roughly 70GB, and pintail can only be loaded through MySQL - so a run
// means loading it into a source first, which no CI runner has the disk or the
// hours for. It belongs on the dedicated host, run deliberately, and is
// tracked separately rather than half-built here.

export interface TpchQuerySpec {
  id: string
  class: string
  sqlFile: string
  /// TPC-H substitution parameters, per the specification's ranges.
  params: Record<string, string | number>
  resultComparison: 'ordered'
}

export default {
  id: 'tpch-v1',
  seed: 42,

  profiles: {
    // The specification's scale factor. 1 is the full ~6M-lineitem dataset;
    // smoke exists so the harness itself can be exercised in seconds, and is
    // far too small for any number worth publishing.
    smoke: { scale: 0.0005 },
    ci: { scale: 0.01 },
    sf1: { scale: 1 },
  },

  schemaFile: './schema.mysql.sql',

  queries: [
    {
      // No joins: isolates scan and aggregate throughput, so a regression here
      // cannot be blamed on join planning.
      id: 'q01-pricing-summary',
      class: 'scan-aggregate',
      sqlFile: './queries/q01-pricing-summary.sql',
      params: { deliveryDate: '1998-12-01', deltaDays: 90 },
      resultComparison: 'ordered',
    },
    {
      // Three tables and a top-N over an aggregate.
      id: 'q03-shipping-priority',
      class: 'join-topn',
      sqlFile: './queries/q03-shipping-priority.sql',
      params: { segment: 'BUILDING', orderDate: '1995-03-15' },
      resultComparison: 'ordered',
    },
    {
      // Six tables. The join order decides this one, and it is the closest
      // thing in the suite to the query pintail is slowest on.
      id: 'q05-local-supplier-volume',
      class: 'join-wide',
      sqlFile: './queries/q05-local-supplier-volume.sql',
      params: { region: 'ASIA', orderDate: '1994-01-01' },
      resultComparison: 'ordered',
    },
    {
      // Four tables grouped by customer identity: hundreds of thousands of
      // groups rather than a handful, so the hash table decides it.
      id: 'q10-returned-item-reporting',
      class: 'join-high-cardinality',
      sqlFile: './queries/q10-returned-item-reporting.sql',
      params: { orderDate: '1993-10-01' },
      resultComparison: 'ordered',
    },
  ] satisfies TpchQuerySpec[],

  gates: {
    // Same standard as every other workload: an answer only counts if MySQL
    // agrees with it.
    exactResults: true,
    maximumDlq: 0,
    maximumReplicationLagSeconds: 5,
  },
}

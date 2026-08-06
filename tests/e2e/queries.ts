/// Differential query corpus: every statement runs on MySQL and on Pintail
/// (`/api/query`) after each workload phase, and the normalized results must
/// be identical. Queries stay inside the documented M2 SQL surface
/// (docs/limitations.md): windows without explicit frames, no recursive
/// CTEs, set operations limited to UNION ALL, subqueries uncorrelated.
///
/// Every ORDER BY ends with a unique key so ordering is fully determined;
/// the comparison is order-sensitive.

export interface DifferentialQuery {
  name: string
  sql: string
  /// Table names the query touches. A query is skipped while any of its
  /// tables is under a documented-gap operation (e.g. renamed away).
  tables: string[]
  /// Zero-based result columns holding comma-separated aggregates
  /// (GROUP_CONCAT) whose element order MySQL leaves unspecified; elements
  /// are sorted before comparison.
  csvColumns?: number[]
  /// Divergence is a documented limitation (docs/limitations.md), reported
  /// as WARN so regressions stay visible and a fix flips it to PASS.
  documentedGap?: string
}

export const differentialQueries: DifferentialQuery[] = [
  {
    name: 'point lookup by key',
    sql: 'SELECT id, name, email, tier, balance FROM customers WHERE id = 7',
    tables: ['customers'],
  },
  {
    name: 'range scan with compound predicate',
    sql:
      "SELECT id, customer_id, status, total FROM orders " +
      "WHERE total > 50 AND status <> 'cancelled' ORDER BY id LIMIT 50",
    tables: ['orders'],
  },
  {
    name: 'inner join with aggregation',
    sql:
      'SELECT c.tier, COUNT(*) AS orders_count, ROUND(SUM(o.total), 2) AS revenue ' +
      'FROM orders o JOIN customers c ON o.customer_id = c.id ' +
      'GROUP BY c.tier ORDER BY revenue DESC, c.tier',
    tables: ['orders', 'customers'],
  },
  {
    name: 'left join preserves unmatched rows',
    sql:
      'SELECT c.id, c.name, COUNT(o.id) AS order_count ' +
      'FROM customers c LEFT JOIN orders o ON o.customer_id = c.id ' +
      'GROUP BY c.id, c.name ORDER BY order_count DESC, c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'three-way join through items',
    sql:
      'SELECT c.name, i.product, i.qty, ROUND(i.qty * i.price, 2) AS line_total ' +
      'FROM order_items i ' +
      'JOIN orders o ON i.order_id = o.id ' +
      'JOIN customers c ON o.customer_id = c.id ' +
      'ORDER BY line_total DESC, o.id, i.line_no LIMIT 40',
    tables: ['order_items', 'orders', 'customers'],
  },
  {
    name: 'union all across sources',
    sql:
      "SELECT id AS entity_id, 'customer' AS kind FROM customers WHERE tier = 'enterprise' " +
      'UNION ALL ' +
      "SELECT id AS entity_id, 'order' AS kind FROM orders WHERE total > 900 " +
      'ORDER BY kind, entity_id',
    tables: ['customers', 'orders'],
  },
  {
    name: 'group by with having',
    sql:
      'SELECT customer_id, COUNT(*) AS n, ROUND(AVG(total), 2) AS avg_total ' +
      'FROM orders GROUP BY customer_id HAVING COUNT(*) >= 2 ' +
      'ORDER BY n DESC, customer_id LIMIT 30',
    tables: ['orders'],
  },
  {
    name: 'conditional decimal sum keeps the fraction',
    sql:
      'SELECT CAST(status AS CHAR) AS status_text, COUNT(*) AS n, ' +
      'SUM(CASE WHEN total > 100 THEN total ELSE 0 END) AS big_orders, ' +
      'SUM(CASE WHEN total > 100 THEN 1 ELSE 0 END) / COUNT(*) AS big_share ' +
      'FROM orders GROUP BY status_text ORDER BY status_text',
    tables: ['orders'],
  },
  {
    name: 'distinct count and min max',
    sql:
      'SELECT COUNT(DISTINCT customer_id) AS buyers, ' +
      'ROUND(MIN(total), 2) AS min_total, ROUND(MAX(total), 2) AS max_total FROM orders',
    tables: ['orders'],
  },
  {
    name: 'uncorrelated in-subquery',
    sql:
      'SELECT id, name FROM customers ' +
      "WHERE id IN (SELECT customer_id FROM orders WHERE status = 'delivered') " +
      'ORDER BY id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'scalar subquery threshold',
    sql:
      'SELECT id, total FROM orders ' +
      'WHERE total > (SELECT AVG(total) FROM orders) ORDER BY total DESC, id LIMIT 20',
    tables: ['orders'],
  },
  {
    name: 'non-recursive cte',
    sql:
      'WITH spend AS (' +
      '  SELECT customer_id, SUM(total) AS lifetime FROM orders GROUP BY customer_id' +
      ') ' +
      'SELECT c.id, c.name, ROUND(s.lifetime, 2) AS lifetime ' +
      'FROM customers c JOIN spend s ON s.customer_id = c.id ' +
      'ORDER BY lifetime DESC, c.id LIMIT 15',
    tables: ['customers', 'orders'],
  },
  {
    name: 'date bucketing',
    sql:
      'SELECT YEAR(placed_on) AS yr, MONTH(placed_on) AS mo, COUNT(*) AS n ' +
      'FROM orders GROUP BY yr, mo ORDER BY yr, mo',
    tables: ['orders'],
  },
  {
    name: 'string functions and like',
    sql:
      "SELECT id, UPPER(name) AS shout, CHAR_LENGTH(name) AS len FROM customers " +
      "WHERE name LIKE '%a%' ORDER BY id LIMIT 25",
    tables: ['customers'],
  },
  {
    name: 'json constructor preserves json versus text',
    sql:
      "SELECT id, JSON_OBJECT('json', meta, 'text', CAST(meta AS CHAR)) AS object_value, " +
      'JSON_ARRAY(meta, CAST(meta AS CHAR)) AS array_value ' +
      'FROM customers ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'json aggregate embeds documents',
    sql:
      'SELECT id, JSON_ARRAYAGG(meta) AS documents FROM customers ' +
      'GROUP BY id ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'regular expression read transforms',
    sql:
      "SELECT id, REGEXP_LIKE(name, '^[[:alpha:]]'), " +
      "REGEXP_INSTR(name, '[0-9]+'), REGEXP_REPLACE(name, '[0-9]+', '#') " +
      'FROM customers ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'case expression buckets',
    sql:
      'SELECT CASE WHEN total >= 500 THEN \'high\' WHEN total >= 100 THEN \'mid\' ' +
      "ELSE 'low' END AS bucket, COUNT(*) AS n " +
      'FROM orders GROUP BY bucket ORDER BY bucket',
    tables: ['orders'],
  },
  {
    name: 'null handling',
    sql:
      'SELECT COUNT(*) AS with_null FROM orders WHERE updated_at IS NULL',
    tables: ['orders'],
  },
  {
    name: 'coalesce and ifnull',
    sql:
      "SELECT id, COALESCE(email, '<none>') AS contact FROM customers ORDER BY id LIMIT 25",
    tables: ['customers'],
  },
  {
    name: 'enum and set filters',
    sql:
      "SELECT id, tier, tags FROM customers WHERE tier IN ('pro', 'enterprise') " +
      'ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'unsigned boundary readback',
    sql: 'SELECT id, u8, u16, u32, u64, s64 FROM counters ORDER BY id',
    tables: ['counters'],
  },
  {
    name: 'derived table',
    sql:
      'SELECT bucket_day, COUNT(*) AS orders_that_day FROM (' +
      '  SELECT id, placed_on AS bucket_day FROM orders' +
      ') d GROUP BY bucket_day ORDER BY orders_that_day DESC, bucket_day LIMIT 15',
    tables: ['orders'],
  },
  {
    name: 'group_concat single expression',
    sql:
      'SELECT customer_id, GROUP_CONCAT(DISTINCT status) AS statuses ' +
      'FROM orders GROUP BY customer_id ORDER BY customer_id LIMIT 10',
    tables: ['orders'],
    csvColumns: [1],
  },
  {
    name: 'window ranking per group',
    sql:
      'SELECT id, customer_id, ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY id) AS seq ' +
      'FROM orders ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'window share of total over grouped output',
    sql:
      // CAST the ENUM tiebreaker: MySQL orders bare ENUMs by declaration
      // index while Pintail orders them as text (documented limitation).
      'SELECT status, COUNT(*) AS n, ' +
      'ROUND(COUNT(*) * 100 / SUM(COUNT(*)) OVER (), 2) AS pct, ' +
      'ROW_NUMBER() OVER (ORDER BY COUNT(*) DESC, CAST(status AS CHAR)) AS busiest ' +
      'FROM orders GROUP BY status ORDER BY busiest',
    tables: ['orders'],
  },
  {
    name: 'window running total',
    sql:
      'SELECT id, ROUND(SUM(total) OVER (ORDER BY id), 2) AS running ' +
      'FROM orders ORDER BY id LIMIT 30',
    tables: ['orders'],
  },
]

export type BenchmarkQuery = {
  name: string
  sql: string
  clickhouseSql?: string
  /// Novel-query mode: run each distinct variant once with no warmup on every
  /// engine. These rows measure raw engine speed and remain outside the
  /// release-gate totals (which keep their original definition).
  coldOnly?: boolean
  /// Semantically equivalent workload shapes with different predicates.
  /// Each variant runs once, keeping every sample outside Pintail's exact
  /// result memo while still allowing a median instead of one noisy timing.
  coldVariants?: Array<{ sql: string; clickhouseSql?: string }>
}

export const benchmarkQueries: BenchmarkQuery[] = [
  {
    name: 'Q1: Full table count',
    sql: 'SELECT COUNT(*) AS cnt FROM orders',
  },
  {
    name: 'Q2: Filtered count',
    sql: "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'shipped'",
  },
  {
    name: 'Q3: Group by status',
    sql: 'SELECT status, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt FROM orders GROUP BY status ORDER BY cnt DESC',
  },
  {
    name: 'Q4: Region × status breakdown',
    sql: 'SELECT region, status, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total FROM orders GROUP BY region, status ORDER BY total DESC, region, status LIMIT 20',
  },
  {
    name: 'Q5: Monthly revenue (2023)',
    sql: "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS revenue FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' GROUP BY yr, mo ORDER BY yr, mo",
    clickhouseSql:
      "SELECT toYear(order_date) AS yr, toMonth(order_date) AS mo, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS revenue FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' GROUP BY yr, mo ORDER BY yr, mo",
  },
  {
    name: 'Q6: Top 10 spenders',
    sql: 'SELECT user_id, COUNT(*) AS order_count, ROUND(SUM(total_amount), 2) AS total_spent FROM orders GROUP BY user_id ORDER BY total_spent DESC, user_id LIMIT 10',
  },
  {
    name: 'Q7: Regional analytics',
    sql: "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users FROM orders WHERE order_date BETWEEN '2022-01-01' AND '2023-12-31' GROUP BY region ORDER BY total DESC",
    clickhouseSql:
      "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, ROUND(MAX(total_amount), 2) AS max_amt, uniqExact(user_id) AS unique_users FROM orders WHERE order_date BETWEEN '2022-01-01' AND '2023-12-31' GROUP BY region ORDER BY total DESC",
  },
  {
    name: 'Q8: Join users + orders',
    sql: 'SELECT u.region, COUNT(*) AS cnt, ROUND(SUM(o.total_amount), 2) AS total FROM orders o JOIN users u ON o.user_id = u.id GROUP BY u.region ORDER BY total DESC',
  },
  {
    name: 'N1: Filtered count, novel constant',
    sql: "SELECT COUNT(*) AS cnt FROM orders WHERE status = 'delivered'",
    coldOnly: true,
    coldVariants: [
      ['delivered', 1],
      ['pending', 2],
      ['cancelled', 3],
      ['processing', 4],
      ['shipped', 5],
    ].map(([status, minimumId]) => ({
      sql: `SELECT COUNT(*) AS cnt FROM orders WHERE status = '${status}' AND id >= ${minimumId}`,
    })),
  },
  {
    name: 'N2: Group by region (novel group column)',
    sql: 'SELECT region, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt \
FROM orders GROUP BY region ORDER BY cnt DESC',
    coldOnly: true,
    coldVariants: [1, 2, 3, 4, 5].map((minimumId) => ({
      sql: `SELECT region, COUNT(*) AS cnt, ROUND(AVG(total_amount), 2) AS avg_amt \
FROM orders WHERE id >= ${minimumId} GROUP BY region ORDER BY cnt DESC`,
    })),
  },
  {
    name: 'N3: Monthly revenue, novel year',
    sql: "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
ROUND(SUM(total_amount), 2) AS revenue FROM orders \
WHERE order_date >= '2022-01-01' AND order_date < '2023-01-01' \
GROUP BY yr, mo ORDER BY yr, mo",
    coldOnly: true,
    coldVariants: [
      ['2020-01-01', '2021-01-01'],
      ['2021-01-01', '2022-01-01'],
      ['2022-01-01', '2023-01-01'],
      ['2024-01-01', '2025-01-01'],
      ['2020-07-01', '2021-07-01'],
    ].map(([start, end]) => ({
      sql: `SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, \
ROUND(SUM(total_amount), 2) AS revenue FROM orders \
WHERE order_date >= '${start}' AND order_date < '${end}' \
GROUP BY yr, mo ORDER BY yr, mo`,
    })),
  },
  {
    name: 'N4: Regional analytics, novel range',
    sql: "SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
FROM orders WHERE order_date BETWEEN '2021-01-01' AND '2022-12-31' \
GROUP BY region ORDER BY total DESC",
    coldOnly: true,
    coldVariants: [
      ['2020-01-01', '2021-12-31'],
      ['2021-01-01', '2022-12-31'],
      ['2020-01-01', '2022-12-31'],
      ['2021-01-01', '2023-12-31'],
      ['2023-01-01', '2024-12-31'],
    ].map(([start, end]) => ({
      sql: `SELECT region, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total, \
ROUND(AVG(total_amount), 2) AS avg_amt, ROUND(MIN(total_amount), 2) AS min_amt, \
ROUND(MAX(total_amount), 2) AS max_amt, COUNT(DISTINCT user_id) AS unique_users \
FROM orders WHERE order_date BETWEEN '${start}' AND '${end}' \
GROUP BY region ORDER BY total DESC`,
    })),
  },
]

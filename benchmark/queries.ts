export type BenchmarkQuery = {
  name: string
  sql: string
  clickhouseSql?: string
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
    sql: 'SELECT region, status, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS total FROM orders GROUP BY region, status ORDER BY total DESC LIMIT 20',
  },
  {
    name: 'Q5: Monthly revenue (2023)',
    sql: "SELECT YEAR(order_date) AS yr, MONTH(order_date) AS mo, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS revenue FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' GROUP BY yr, mo ORDER BY yr, mo",
    clickhouseSql:
      "SELECT toYear(order_date) AS yr, toMonth(order_date) AS mo, COUNT(*) AS cnt, ROUND(SUM(total_amount), 2) AS revenue FROM orders WHERE order_date >= '2023-01-01' AND order_date < '2024-01-01' GROUP BY yr, mo ORDER BY yr, mo",
  },
  {
    name: 'Q6: Top 10 spenders',
    sql: 'SELECT user_id, COUNT(*) AS order_count, ROUND(SUM(total_amount), 2) AS total_spent FROM orders GROUP BY user_id ORDER BY total_spent DESC LIMIT 10',
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
]

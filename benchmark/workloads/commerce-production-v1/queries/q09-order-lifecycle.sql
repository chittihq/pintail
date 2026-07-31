-- class: lifecycle-analytics  [WINDOW FUNCTIONS — v1 forcing function]
-- Placed→completed duration by month: average, median, p95 (window-based).
WITH durations AS (
  SELECT
    DATE_FORMAT(placed_at, '%Y-%m') AS month,
    TIMESTAMPDIFF(SECOND, placed_at, completed_at) AS seconds_to_complete,
    ROW_NUMBER() OVER (
      PARTITION BY DATE_FORMAT(placed_at, '%Y-%m')
      ORDER BY TIMESTAMPDIFF(SECOND, placed_at, completed_at)
    ) AS rn,
    COUNT(*) OVER (PARTITION BY DATE_FORMAT(placed_at, '%Y-%m')) AS n
  FROM orders
  WHERE order_status = 'completed'
    AND completed_at IS NOT NULL
    AND placed_at >= :windowStart
)
SELECT
  month,
  MAX(n) AS completed_orders,
  AVG(seconds_to_complete) / 3600 AS avg_hours,
  MAX(CASE WHEN rn = CEIL(n / 2) THEN seconds_to_complete END) / 3600 AS median_hours,
  MAX(CASE WHEN rn = CEIL(n * 0.95) THEN seconds_to_complete END) / 3600 AS p95_hours
FROM durations
GROUP BY month
ORDER BY month;

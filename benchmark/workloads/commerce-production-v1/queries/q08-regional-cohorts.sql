-- class: growth-analytics  [WINDOW FUNCTIONS — v1 forcing function]
-- Customer first-order cohorts by shipping country: repeat rate within 90 days.
WITH first_orders AS (
  SELECT
    customer_id,
    shipping_country,
    placed_at,
    ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY placed_at) AS order_seq,
    MIN(placed_at) OVER (PARTITION BY customer_id) AS first_order_at
  FROM orders
  WHERE deleted_at IS NULL
)
SELECT
  DATE_FORMAT(first_order_at, '%Y-%m') AS cohort_month,
  shipping_country,
  COUNT(DISTINCT customer_id) AS cohort_customers,
  COUNT(DISTINCT CASE
    WHEN order_seq > 1 AND placed_at <= first_order_at + INTERVAL 90 DAY
    THEN customer_id END) AS repeat_within_90d,
  COUNT(DISTINCT CASE
    WHEN order_seq > 1 AND placed_at <= first_order_at + INTERVAL 90 DAY
    THEN customer_id END) / COUNT(DISTINCT customer_id) AS repeat_rate
FROM first_orders
WHERE first_order_at >= :windowStart
GROUP BY cohort_month, shipping_country
ORDER BY cohort_month, shipping_country;

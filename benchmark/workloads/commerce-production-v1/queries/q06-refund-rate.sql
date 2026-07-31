-- class: quality-analytics
-- Refund rate and refunded value share by product category, trailing quarter.
SELECT
  c.name AS category,
  COUNT(DISTINCT o.id) AS orders,
  COUNT(DISTINCT r.order_id) AS refunded_orders,
  COUNT(DISTINCT r.order_id) / COUNT(DISTINCT o.id) AS refund_rate,
  SUM(o.total_amount) AS gross_value,
  COALESCE(SUM(r.amount), 0) AS refunded_value
FROM orders o
JOIN order_items i ON i.order_id = o.id
JOIN product_variants v ON v.id = i.product_variant_id
JOIN products p ON p.id = v.product_id
JOIN categories c ON c.id = p.category_id
LEFT JOIN refunds r ON r.order_id = o.id AND r.status = 'processed'
WHERE o.placed_at >= :windowStart
  AND o.deleted_at IS NULL
GROUP BY c.name
HAVING orders >= 100
ORDER BY refund_rate DESC
LIMIT 50;

-- class: operational-dashboard
-- Unfulfilled/partial backlog for a tenant, bucketed by age.
SELECT
  fulfillment_status,
  CASE
    WHEN placed_at >= :now - INTERVAL 6 HOUR THEN '0-6h'
    WHEN placed_at >= :now - INTERVAL 24 HOUR THEN '6-24h'
    WHEN placed_at >= :now - INTERVAL 72 HOUR THEN '24-72h'
    ELSE '72h+'
  END AS age_bucket,
  COUNT(*) AS orders,
  SUM(total_amount) AS value_at_risk
FROM orders
WHERE tenant_id = :tenantId
  AND fulfillment_status IN ('unfulfilled', 'partial')
  AND order_status IN ('pending', 'confirmed')
  AND deleted_at IS NULL
GROUP BY fulfillment_status, 2
ORDER BY fulfillment_status, age_bucket;

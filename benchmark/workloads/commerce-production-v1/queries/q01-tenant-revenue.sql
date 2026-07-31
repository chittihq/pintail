-- class: executive-dashboard
-- Monthly revenue for one tenant over the trailing year, split by currency.
-- Multi-currency: totals are only meaningful grouped by currency.
SELECT
  DATE_FORMAT(placed_at, '%Y-%m') AS month,
  currency,
  COUNT(*) AS orders,
  SUM(total_amount) AS gross_revenue,
  SUM(discount_amount) AS discounts,
  SUM(CASE WHEN order_status = 'cancelled' THEN total_amount ELSE 0 END) AS cancelled_value
FROM orders
WHERE tenant_id = :tenantId
  AND placed_at >= :windowStart
  AND deleted_at IS NULL
GROUP BY DATE_FORMAT(placed_at, '%Y-%m'), currency
ORDER BY month, currency;

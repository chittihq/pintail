-- class: operational-dashboard
-- SKUs whose sellable stock cannot cover trailing-14-day demand velocity.
SELECT
  v.sku,
  SUM(b.on_hand - b.reserved) AS sellable,
  d.units_14d,
  d.units_14d / 14.0 AS daily_velocity,
  SUM(b.on_hand - b.reserved) / (d.units_14d / 14.0) AS days_of_cover
FROM inventory_balances b
JOIN product_variants v ON v.id = b.variant_id
JOIN (
  SELECT i.product_variant_id, SUM(i.quantity) AS units_14d
  FROM order_items i
  JOIN orders o ON o.id = i.order_id
  WHERE o.tenant_id = :tenantId
    AND o.placed_at >= :now - INTERVAL 14 DAY
    AND o.order_status <> 'cancelled'
  GROUP BY i.product_variant_id
) d ON d.product_variant_id = b.variant_id
WHERE b.tenant_id = :tenantId
GROUP BY v.sku, d.units_14d
HAVING days_of_cover < 7
ORDER BY days_of_cover ASC
LIMIT 100;

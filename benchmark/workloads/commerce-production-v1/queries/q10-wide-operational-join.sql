-- class: wide-join
-- One tenant's operational picture across orders, items, payments, shipments
-- for a date window: the "everything join" a support/ops dashboard runs.
SELECT
  o.order_status,
  o.payment_status,
  o.fulfillment_status,
  COUNT(DISTINCT o.id) AS orders,
  SUM(o.total_amount) AS order_value,
  COUNT(DISTINCT i.id) AS line_items,
  COUNT(DISTINCT CASE WHEN p.status = 'failed' THEN p.id END) AS failed_payments,
  COUNT(DISTINCT s.id) AS shipments,
  COUNT(DISTINCT CASE WHEN s.status = 'delivered' THEN s.id END) AS delivered
FROM orders o
LEFT JOIN order_items i ON i.order_id = o.id
LEFT JOIN payments p ON p.order_id = o.id
LEFT JOIN shipments s ON s.order_id = o.id
WHERE o.tenant_id = :tenantId
  AND o.placed_at >= :windowStart
  AND o.placed_at < :windowEnd
  AND o.deleted_at IS NULL
GROUP BY o.order_status, o.payment_status, o.fulfillment_status
ORDER BY o.order_status, o.payment_status, o.fulfillment_status;

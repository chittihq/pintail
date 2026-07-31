-- class: operational-lookup
-- One customer's recent orders with line summaries (support-console shape).
SELECT
  o.id,
  o.placed_at,
  o.order_status,
  o.payment_status,
  o.fulfillment_status,
  o.currency,
  o.total_amount,
  COUNT(i.id) AS item_count,
  SUM(i.quantity) AS units
FROM orders o
JOIN order_items i ON i.order_id = o.id
WHERE o.customer_id = :customerId
  AND o.deleted_at IS NULL
GROUP BY o.id, o.placed_at, o.order_status, o.payment_status,
         o.fulfillment_status, o.currency, o.total_amount
ORDER BY o.placed_at DESC
LIMIT 50;

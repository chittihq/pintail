-- class: anti-join
-- Customers with no order in the window: the "who stopped buying" question
-- every retention dashboard asks, and the one shape the corpus had none of.
--
-- An anti-join is not a join with a filter. The engine must prove absence,
-- which means it cannot stop at the first match and cannot use a semi-join's
-- early exit - it either builds the full right side or probes every left row.
-- Nothing else here exercises that.
SELECT
  c.id AS customer_id,
  c.email,
  c.lifetime_value,
  c.created_at
FROM customers c
WHERE c.tenant_id = :tenantId
  AND c.deleted_at IS NULL
  AND NOT EXISTS (
    SELECT 1
    FROM orders o
    WHERE o.customer_id = c.id
      AND o.placed_at >= :windowStart
      AND o.deleted_at IS NULL
  )
ORDER BY c.lifetime_value DESC, c.id
LIMIT 200;

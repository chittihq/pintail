-- class: high-cardinality-grouping
-- Revenue per customer across the window. One group per customer rather than
-- per status or per currency, so the grouping produces hundreds of thousands
-- of groups instead of a handful.
--
-- This is the shape the corpus was thinnest on, and it is the one that
-- separates engines: a low-cardinality GROUP BY fits in cache and is decided
-- by scan speed, while this one is decided by how the hash table is built,
-- probed and spilled. The engine-speed track showed pintail furthest behind
-- on exactly that work, so the benchmark should ask the question directly
-- rather than infer it from a join.
SELECT
  o.customer_id,
  COUNT(*) AS orders,
  SUM(o.total_amount) AS gross_revenue,
  AVG(o.total_amount) AS average_order_value,
  MAX(o.placed_at) AS last_order_at
FROM orders o
WHERE o.tenant_id = :tenantId
  AND o.placed_at >= :windowStart
  -- Upper-bounded away from the write head. Compared against a source that is
  -- still being written to, any aggregate is a race, and HAVING turns a single
  -- lagging row into a whole missing group: a customer at two orders drops to
  -- one and leaves the result entirely. Excluding the last day asks the same
  -- question of data that has stopped moving, so a mismatch here means a real
  -- divergence rather than a snapshot taken mid-write.
  AND o.placed_at < :windowEnd
  AND o.deleted_at IS NULL
GROUP BY o.customer_id
HAVING COUNT(*) > 1
ORDER BY gross_revenue DESC, o.customer_id
LIMIT 500;

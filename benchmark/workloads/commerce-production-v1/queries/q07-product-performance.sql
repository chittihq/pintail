-- class: merchandising-analytics  [WINDOW FUNCTIONS — v1 forcing function]
-- Top products by revenue with share-of-category, trailing 90 days.
SELECT * FROM (
  SELECT
    c.name AS category,
    i.product_name,
    SUM(i.total_amount) AS revenue,
    SUM(i.quantity) AS units,
    SUM(SUM(i.total_amount)) OVER (PARTITION BY c.name) AS category_revenue,
    SUM(i.total_amount) / SUM(SUM(i.total_amount)) OVER (PARTITION BY c.name) AS category_share,
    -- product_name breaks ties. ROW_NUMBER over an ordering key that ties is
    -- undefined - two products on identical revenue may take 2 and 3 in either
    -- order, and no engine promises which. Compared across engines that reads
    -- as a divergence when both answers are correct.
    ROW_NUMBER() OVER (
      PARTITION BY c.name
      ORDER BY SUM(i.total_amount) DESC, i.product_name
    ) AS rank_in_category
  FROM order_items i
  JOIN orders o ON o.id = i.order_id
  JOIN product_variants v ON v.id = i.product_variant_id
  JOIN products p ON p.id = v.product_id
  JOIN categories c ON c.id = p.category_id
  WHERE o.placed_at >= :now - INTERVAL 90 DAY
    AND o.order_status = 'completed'
  GROUP BY c.name, i.product_name
) ranked
WHERE rank_in_category <= 5
ORDER BY category, rank_in_category;

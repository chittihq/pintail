-- TPC-H Q3: Shipping Priority.
-- Three-way join with a top-N over an aggregate. The ORDER BY is the
-- specification's, and o_orderkey makes it total: revenue ties otherwise
-- leave which rows the LIMIT keeps undefined.
SELECT
  l_orderkey,
  SUM(l_extendedprice * (1 - l_discount)) AS revenue,
  o_orderdate,
  o_shippriority
FROM customer, orders, lineitem
WHERE c_mktsegment = :segment
  AND c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND o_orderdate < :orderDate
  AND l_shipdate > :orderDate
GROUP BY l_orderkey, o_orderdate, o_shippriority
ORDER BY revenue DESC, o_orderdate, l_orderkey
LIMIT 10;

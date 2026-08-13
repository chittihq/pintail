-- TPC-H Q5: Local Supplier Volume.
-- Six tables joined, which is the shape the engine-speed track showed pintail
-- furthest behind on. The join order matters more here than anywhere else in
-- the suite.
SELECT
  n_name,
  SUM(l_extendedprice * (1 - l_discount)) AS revenue
FROM customer, orders, lineitem, supplier, nation, region
WHERE c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND l_suppkey = s_suppkey
  AND c_nationkey = s_nationkey
  AND s_nationkey = n_nationkey
  AND n_regionkey = r_regionkey
  AND r_name = :region
  AND o_orderdate >= :orderDate
  AND o_orderdate < DATE_ADD(:orderDate, INTERVAL 1 YEAR)
GROUP BY n_name
ORDER BY revenue DESC, n_name;

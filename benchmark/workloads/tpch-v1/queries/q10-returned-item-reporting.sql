-- TPC-H Q10: Returned Item Reporting.
-- Four-way join grouping by a wide key - customer identity rather than a
-- handful of statuses - so the cost lands on the hash table rather than the
-- scan.
SELECT
  c_custkey,
  c_name,
  SUM(l_extendedprice * (1 - l_discount)) AS revenue,
  c_acctbal,
  n_name,
  c_address,
  c_phone,
  c_comment
FROM customer, orders, lineitem, nation
WHERE c_custkey = o_custkey
  AND l_orderkey = o_orderkey
  AND o_orderdate >= :orderDate
  AND o_orderdate < DATE_ADD(:orderDate, INTERVAL 3 MONTH)
  AND l_returnflag = 'R'
  AND c_nationkey = n_nationkey
GROUP BY c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment
ORDER BY revenue DESC, c_custkey
LIMIT 20;

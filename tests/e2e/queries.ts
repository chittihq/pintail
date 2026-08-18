/// Differential query corpus: every statement runs over the MySQL wire
/// protocol on MySQL and Pintail after each workload phase, and the normalized
/// results must be identical. Queries stay inside the supported SQL surface
/// documented in `docs/limitations.md`.
///
/// Every ORDER BY ends with a unique key so ordering is fully determined;
/// the comparison is order-sensitive.

export interface DifferentialQuery {
  name: string
  sql: string
  /// Table names the query touches. A query is skipped while any of its
  /// tables is under a documented-gap operation (e.g. renamed away).
  tables: string[]
  /// Zero-based result columns holding comma-separated aggregates
  /// (GROUP_CONCAT) whose element order MySQL leaves unspecified; elements
  /// are sorted before comparison.
  csvColumns?: number[]
  /// Divergence is a documented limitation (docs/limitations.md), reported
  /// as WARN so regressions stay visible and a fix flips it to PASS.
  documentedGap?: string
}

export const differentialQueries: DifferentialQuery[] = [
  {
    name: 'point lookup by key',
    sql: 'SELECT id, name, email, tier, balance FROM customers WHERE id = 7',
    tables: ['customers'],
  },
  {
    name: 'range scan with compound predicate',
    sql:
      "SELECT id, customer_id, status, total FROM orders " +
      "WHERE total > 50 AND status <> 'cancelled' ORDER BY id LIMIT 50",
    tables: ['orders'],
  },
  {
    name: 'inner join with aggregation',
    sql:
      'SELECT c.tier, COUNT(*) AS orders_count, ROUND(SUM(o.total), 2) AS revenue ' +
      'FROM orders o JOIN customers c ON o.customer_id = c.id ' +
      'GROUP BY c.tier ORDER BY revenue DESC, c.tier',
    tables: ['orders', 'customers'],
  },
  {
    // Chitti LMS PT-1, reduced to this fixture: an ON clause carrying BOTH an
    // equality and a comparison between the two inputs. The equality is what
    // the hash join keys on; the comparison is a residual tested per
    // candidate pair. Pintail rejected the whole join for the residual.
    name: 'join with a residual comparison between both inputs',
    sql:
      'SELECT COUNT(*) AS n FROM orders o ' +
      'JOIN order_items i ON i.order_id = o.id AND i.price <= o.total',
    tables: ['orders', 'order_items'],
  },
  {
    // The half that makes the residual load-bearing rather than cosmetic. An
    // order whose every item fails the residual must SURVIVE as a
    // NULL-extended row; moving the same predicate into WHERE drops it
    // instead, which is why Chitti had no semantics-preserving workaround.
    name: 'left join keeps rows whose only matches fail the residual',
    sql:
      'SELECT o.id, COUNT(i.order_id) AS kept ' +
      'FROM orders o LEFT JOIN order_items i ' +
      '  ON i.order_id = o.id AND i.price > o.total ' +
      'GROUP BY o.id ORDER BY o.id LIMIT 40',
    tables: ['orders', 'order_items'],
  },
  {
    // A residual referencing a nullable left column through COALESCE, which
    // is the exact shape of their liveClassEffectiveFrom predicate.
    name: 'residual comparison through coalesce on a nullable column',
    sql:
      'SELECT COUNT(*) AS n FROM customers c ' +
      'LEFT JOIN orders o ON o.customer_id = c.id ' +
      "  AND o.placed_on >= COALESCE(c.created_at, '1900-01-01')",
    tables: ['customers', 'orders'],
  },
  {
    // The reported production shape verbatim: one table read through TWO
    // aliases whose values differ per row, with references that are NULL or
    // DANGLING. Every failure mode is visible in the values: collapsing the
    // aliases changes names, and a dangling reference must come back NULL
    // rather than as the other alias's row.
    name: 'created-by and updated-by resolve through separate aliases',
    sql:
      'SELECT s.id, s.name, c.name AS created_by_name, u.name AS updated_by_name ' +
      'FROM staff s ' +
      'LEFT JOIN staff c ON c.id = s.created_by ' +
      'LEFT JOIN staff u ON u.id = s.updated_by ' +
      'ORDER BY s.id',
    tables: ['staff'],
  },
  {
    // The same pair with the join order reversed - the reporter established
    // the misattribution follows POSITION, not name.
    name: 'alias pair with the join order reversed',
    sql:
      'SELECT s.id, u.name AS updated_by_name, c.name AS created_by_name ' +
      'FROM staff s ' +
      'LEFT JOIN staff u ON u.id = s.updated_by ' +
      'LEFT JOIN staff c ON c.id = s.created_by ' +
      'ORDER BY s.id',
    tables: ['staff'],
  },
  {
    // The four-alias chain from the execution-budget phase, at a size where
    // the answer is byte-checkable. This is the shape where key ORIENTATION
    // goes wrong: with table-level provenance both sides of every conjunct
    // look identical, and a key compiled against the wrong side either
    // resolves to the wrong alias (same physical column, silently fine) or
    // fails to resolve at all.
    name: 'four aliases of one table joined in a chain',
    sql:
      'SELECT COUNT(*) AS n FROM staff a ' +
      'JOIN staff b ON a.manager_id = b.manager_id ' +
      'JOIN staff c ON c.manager_id = b.manager_id ' +
      'JOIN staff d ON d.manager_id = c.manager_id',
    tables: ['staff'],
  },
  {
    // A single-side conjunct on a self-join ON clause. Relation-instance
    // provenance is what lets this split to one input; before, a self-join
    // bailed out of splitting entirely because its sides looked identical.
    name: 'self-join with a single-side predicate in the ON clause',
    sql:
      'SELECT e.id, e.name, m.name AS manager_name ' +
      'FROM staff e ' +
      'JOIN staff m ON m.id = e.manager_id AND m.active = 1 ' +
      'ORDER BY e.id',
    tables: ['staff'],
  },
  {
    // Manager chain two levels up, LEFT so the roots survive NULL-extended.
    name: 'self-join manager chain preserves the roots',
    sql:
      'SELECT e.id, e.name, m.name AS manager, mm.name AS grand_manager ' +
      'FROM staff e ' +
      'LEFT JOIN staff m ON m.id = e.manager_id ' +
      'LEFT JOIN staff mm ON mm.id = m.manager_id ' +
      'ORDER BY e.id',
    tables: ['staff'],
  },
  {
    // A table joined twice under two aliases. Physically the two inputs are
    // the same database, table and column ids, so a resolver keyed on those
    // alone returns the FIRST alias for both - silently, with plausible
    // values. Reported against a staging table where 605 of 4067 rows
    // attributed an activity to the wrong person.
    //
    // The second alias deliberately matches NOTHING, so the correct answer
    // is NULL and the wrong answer is the first alias's row. A count would
    // not catch it; only the values do.
    name: 'a table joined twice under two aliases keeps them distinct',
    sql:
      'SELECT o.id, c1.name AS placer, c2.name AS phantom ' +
      'FROM orders o ' +
      'LEFT JOIN customers c1 ON c1.id = o.customer_id ' +
      'LEFT JOIN customers c2 ON c2.id = o.id + 100000 ' +
      'ORDER BY o.id LIMIT 30',
    tables: ['orders', 'customers'],
  },
  {
    // The same shape with the aliases reversed: the reporter found the bug
    // followed the JOIN POSITION rather than the name, so both orders have
    // to be covered.
    name: 'aliases stay distinct when the empty side joins first',
    sql:
      'SELECT o.id, c1.name AS phantom, c2.name AS placer ' +
      'FROM orders o ' +
      'LEFT JOIN customers c1 ON c1.id = o.id + 100000 ' +
      'LEFT JOIN customers c2 ON c2.id = o.customer_id ' +
      'ORDER BY o.id LIMIT 30',
    tables: ['orders', 'customers'],
  },
  {
    name: 'left join preserves unmatched rows',
    sql:
      'SELECT c.id, c.name, COUNT(o.id) AS order_count ' +
      'FROM customers c LEFT JOIN orders o ON o.customer_id = c.id ' +
      'GROUP BY c.id, c.name ORDER BY order_count DESC, c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'right join preserves unmatched rows',
    sql:
      'SELECT c.id, c.name, COUNT(o.id) AS order_count ' +
      'FROM orders o RIGHT JOIN customers c ON o.customer_id = c.id ' +
      'GROUP BY c.id, c.name ORDER BY order_count DESC, c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'three-way join through items',
    sql:
      'SELECT c.name, i.product, i.qty, ROUND(i.qty * i.price, 2) AS line_total ' +
      'FROM order_items i ' +
      'JOIN orders o ON i.order_id = o.id ' +
      'JOIN customers c ON o.customer_id = c.id ' +
      'ORDER BY line_total DESC, o.id, i.line_no LIMIT 40',
    tables: ['order_items', 'orders', 'customers'],
  },
  {
    name: 'union all across sources',
    sql:
      "SELECT id AS entity_id, 'customer' AS kind FROM customers WHERE tier = 'enterprise' " +
      'UNION ALL ' +
      "SELECT id AS entity_id, 'order' AS kind FROM orders WHERE total > 900 " +
      'ORDER BY kind, entity_id',
    tables: ['customers', 'orders'],
  },
  {
    name: 'intersect customer identifiers',
    sql:
      'SELECT id FROM customers WHERE id <= 30 ' +
      'INTERSECT SELECT customer_id FROM orders WHERE customer_id <= 30 ORDER BY id',
    tables: ['customers', 'orders'],
  },
  {
    name: 'except customer identifiers',
    sql:
      'SELECT id FROM customers WHERE id <= 30 ' +
      'EXCEPT SELECT customer_id FROM orders WHERE customer_id <= 30 ORDER BY id',
    tables: ['customers', 'orders'],
  },
  {
    // Chitti LMS PT-2: an ORDER BY expression over an aggregate. Neither an
    // output name nor a source column, so it needs a hidden projection
    // evaluated after grouping. Their workarounds - ORDER BY alias and
    // ORDER BY ordinal - already worked, which is what proved the aggregate
    // itself was reachable and only the expression form was not.
    // Tie-broken on status, an ENUM: the tie order proves the declared
    // ordinal governs the sort (it was retargeted to customer_id while
    // ENUM ordering diverged, and restored with the ordinal fix).
    name: 'order by an expression over an aggregate',
    sql:
      'SELECT status, COUNT(*) AS c FROM orders GROUP BY status ' +
      'ORDER BY COALESCE(COUNT(*), 0) DESC, status',
    tables: ['orders'],
  },
  {
    // The real shape from health-section-students: a whole tree over several
    // different aggregates, not a single wrapped call.
    name: 'order by a tree over several aggregates',
    sql:
      'SELECT o.customer_id, COUNT(*) AS orders_count FROM orders o ' +
      'GROUP BY o.customer_id ' +
      'ORDER BY (LEAST(30, COUNT(*) * 3.0) + LEAST(20, COALESCE(SUM(o.total), 0) / 100)) DESC, ' +
      '  o.customer_id LIMIT 25',
    tables: ['orders'],
  },
  {
    // An aggregate that appears ONLY in ORDER BY must still be computed
    // rather than left dangling.
    name: 'order by an aggregate absent from the select list',
    sql:
      'SELECT status FROM orders GROUP BY status ' +
      'ORDER BY SUM(total) DESC, status',
    tables: ['orders'],
  },
  {
    name: 'group by with having',
    sql:
      'SELECT customer_id, COUNT(*) AS n, ROUND(AVG(total), 2) AS avg_total ' +
      'FROM orders GROUP BY customer_id HAVING COUNT(*) >= 2 ' +
      'ORDER BY n DESC, customer_id LIMIT 30',
    tables: ['orders'],
  },
  {
    name: 'conditional decimal sum keeps the fraction',
    sql:
      'SELECT CAST(status AS CHAR) AS status_text, COUNT(*) AS n, ' +
      'SUM(CASE WHEN total > 100 THEN total ELSE 0 END) AS big_orders, ' +
      'SUM(CASE WHEN total > 100 THEN 1 ELSE 0 END) / COUNT(*) AS big_share ' +
      'FROM orders GROUP BY status_text ORDER BY status_text',
    tables: ['orders'],
  },
  {
    name: 'distinct count and min max',
    sql:
      'SELECT COUNT(DISTINCT customer_id) AS buyers, ' +
      'ROUND(MIN(total), 2) AS min_total, ROUND(MAX(total), 2) AS max_total FROM orders',
    tables: ['orders'],
  },
  {
    name: 'uncorrelated in-subquery',
    sql:
      'SELECT id, name FROM customers ' +
      "WHERE id IN (SELECT customer_id FROM orders WHERE status = 'delivered') " +
      'ORDER BY id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'correlated exists with inner predicate',
    sql:
      'SELECT c.id, c.name FROM customers c ' +
      "WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.status = 'delivered') " +
      'ORDER BY c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'correlated scalar aggregate',
    sql:
      'SELECT c.id, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS order_count ' +
      'FROM customers c ORDER BY c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'correlated scalar unique lookup',
    sql:
      'SELECT c.id, (SELECT o.total FROM orders o WHERE o.id = c.id) AS matching_total ' +
      'FROM customers c ORDER BY c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'scalar subquery threshold',
    sql:
      'SELECT id, total FROM orders ' +
      'WHERE total > (SELECT AVG(total) FROM orders) ORDER BY total DESC, id LIMIT 20',
    tables: ['orders'],
  },
  {
    name: 'non-recursive cte',
    sql:
      'WITH spend AS (' +
      '  SELECT customer_id, SUM(total) AS lifetime FROM orders GROUP BY customer_id' +
      ') ' +
      'SELECT c.id, c.name, ROUND(s.lifetime, 2) AS lifetime ' +
      'FROM customers c JOIN spend s ON s.customer_id = c.id ' +
      'ORDER BY lifetime DESC, c.id LIMIT 15',
    tables: ['customers', 'orders'],
  },
  {
    name: 'bounded recursive cte',
    sql:
      'WITH RECURSIVE seq(n) AS (' +
      'SELECT 1 UNION ALL SELECT n + 1 FROM seq WHERE n < 10' +
      ') SELECT n FROM seq ORDER BY n',
    tables: [],
  },
  {
    name: 'date bucketing',
    sql:
      'SELECT YEAR(placed_on) AS yr, MONTH(placed_on) AS mo, COUNT(*) AS n ' +
      'FROM orders GROUP BY yr, mo ORDER BY yr, mo',
    tables: ['orders'],
  },
  {
    name: 'string functions and like',
    sql:
      "SELECT id, UPPER(name) AS shout, CHAR_LENGTH(name) AS len FROM customers " +
      "WHERE name LIKE '%a%' ORDER BY id LIMIT 25",
    tables: ['customers'],
  },
  {
    name: 'looker symmetric key helpers',
    sql:
      'SELECT id, MD5(CAST(id AS CHAR)) AS digest, ' +
      'CONV(SUBSTRING(MD5(CAST(id AS CHAR)), 1, 15), 16, 10) AS numeric_key, ' +
      "SUBSTRING_INDEX(email, '@', -1) AS email_domain " +
      'FROM customers ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'json constructor preserves json versus text',
    sql:
      "SELECT id, JSON_OBJECT('json', meta, 'text', CAST(meta AS CHAR)) AS object_value, " +
      'JSON_ARRAY(meta, CAST(meta AS CHAR)) AS array_value ' +
      'FROM customers ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'json aggregate embeds documents',
    sql:
      'SELECT id, JSON_ARRAYAGG(meta) AS documents FROM customers ' +
      'GROUP BY id ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'regular expression read transforms',
    sql:
      "SELECT id, REGEXP_LIKE(name, '^[[:alpha:]]'), " +
      "REGEXP_INSTR(name, '[0-9]+'), REGEXP_REPLACE(name, '[0-9]+', '#') " +
      'FROM customers ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'case expression buckets',
    sql:
      'SELECT CASE WHEN total >= 500 THEN \'high\' WHEN total >= 100 THEN \'mid\' ' +
      "ELSE 'low' END AS bucket, COUNT(*) AS n " +
      'FROM orders GROUP BY bucket ORDER BY bucket',
    tables: ['orders'],
  },
  {
    name: 'null handling',
    sql:
      'SELECT COUNT(*) AS with_null FROM orders WHERE updated_at IS NULL',
    tables: ['orders'],
  },
  {
    name: 'coalesce and ifnull',
    sql:
      "SELECT id, COALESCE(email, '<none>') AS contact FROM customers ORDER BY id LIMIT 25",
    tables: ['customers'],
  },
  {
    name: 'enum and set filters',
    sql:
      "SELECT id, tier, tags FROM customers WHERE tier IN ('pro', 'enterprise') " +
      'ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'unsigned boundary readback',
    sql: 'SELECT id, u8, u16, u32, u64, s64 FROM counters ORDER BY id',
    tables: ['counters'],
  },
  {
    name: 'derived table',
    sql:
      'SELECT bucket_day, COUNT(*) AS orders_that_day FROM (' +
      '  SELECT id, placed_on AS bucket_day FROM orders' +
      ') d GROUP BY bucket_day ORDER BY orders_that_day DESC, bucket_day LIMIT 15',
    tables: ['orders'],
  },
  {
    name: 'group_concat single expression',
    sql:
      'SELECT customer_id, GROUP_CONCAT(DISTINCT status) AS statuses ' +
      'FROM orders GROUP BY customer_id ORDER BY customer_id LIMIT 10',
    tables: ['orders'],
    csvColumns: [1],
  },
  {
    name: 'window ranking per group',
    sql:
      'SELECT id, customer_id, ROW_NUMBER() OVER (PARTITION BY customer_id ORDER BY id) AS seq ' +
      'FROM orders ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'window share of total over grouped output',
    sql:
      // CAST the ENUM tiebreaker: MySQL orders bare ENUMs by declaration
      // index while Pintail orders them as text (documented limitation).
      'SELECT status, COUNT(*) AS n, ' +
      'ROUND(COUNT(*) * 100 / SUM(COUNT(*)) OVER (), 2) AS pct, ' +
      'ROW_NUMBER() OVER (ORDER BY COUNT(*) DESC, CAST(status AS CHAR)) AS busiest ' +
      'FROM orders GROUP BY status ORDER BY busiest',
    tables: ['orders'],
  },
  {
    name: 'window running total',
    sql:
      'SELECT id, ROUND(SUM(total) OVER (ORDER BY id), 2) AS running ' +
      'FROM orders ORDER BY id LIMIT 30',
    tables: ['orders'],
  },
  // --- diversify batch: typed / multi-join / reject-adjacent product shapes ---
  {
    name: 'decimal column average beyond simple sum',
    sql:
      'SELECT customer_id, ROUND(AVG(total), 4) AS avg_total, ' +
      'ROUND(SUM(total) / COUNT(*), 4) AS mean_check ' +
      'FROM orders GROUP BY customer_id HAVING COUNT(*) >= 2 ' +
      'ORDER BY avg_total DESC, customer_id LIMIT 20',
    tables: ['orders'],
  },
  {
    name: 'json extract filter on customer meta',
    sql:
      "SELECT id, meta ->> '$.tier' AS tier_path, JSON_TYPE(meta) AS meta_type " +
      'FROM customers WHERE meta IS NOT NULL ORDER BY id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'fan-out join group concat line products',
    sql:
      'SELECT o.id, o.customer_id, COUNT(i.line_no) AS line_count, ' +
      'ROUND(SUM(i.qty * i.price), 2) AS items_total ' +
      'FROM orders o JOIN order_items i ON i.order_id = o.id ' +
      'GROUP BY o.id, o.customer_id HAVING COUNT(i.line_no) >= 1 ' +
      'ORDER BY items_total DESC, o.id LIMIT 30',
    tables: ['orders', 'order_items'],
  },
  {
    name: 'outer join customers without recent orders',
    sql:
      'SELECT c.id, c.name, o.id AS order_id ' +
      'FROM customers c LEFT JOIN orders o ON o.customer_id = c.id AND o.total > 500 ' +
      'WHERE o.id IS NULL ORDER BY c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'set op union distinct tiers and statuses',
    sql:
      "SELECT CAST(tier AS CHAR) AS label FROM customers WHERE id <= 20 " +
      'UNION ' +
      "SELECT CAST(status AS CHAR) AS label FROM orders WHERE id <= 20 " +
      'ORDER BY label',
    tables: ['customers', 'orders'],
  },
  {
    name: 'temporal convert and date_format grain',
    sql:
      "SELECT DATE_FORMAT(placed_on, '%Y-%m-01') AS month_grain, COUNT(*) AS n, " +
      'ROUND(SUM(total), 2) AS revenue FROM orders ' +
      "GROUP BY month_grain ORDER BY month_grain",
    tables: ['orders'],
  },
  {
    name: 'correlated not exists open orders',
    sql:
      'SELECT c.id, c.name FROM customers c ' +
      "WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.status = 'pending') " +
      'ORDER BY c.id LIMIT 25',
    tables: ['customers', 'orders'],
  },
  {
    name: 'window lag payment-shaped totals',
    sql:
      'SELECT id, customer_id, total, ' +
      'LAG(total) OVER (PARTITION BY customer_id ORDER BY id) AS prev_total ' +
      'FROM orders ORDER BY customer_id, id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'multi-key join items to orders',
    sql:
      'SELECT i.order_id, i.line_no, o.status, ROUND(i.qty * i.price, 2) AS line_total ' +
      'FROM order_items i JOIN orders o ' +
      'ON i.order_id = o.id AND o.customer_id = o.customer_id ' +
      "WHERE o.status IN ('delivered', 'shipped') " +
      'ORDER BY line_total DESC, i.order_id, i.line_no LIMIT 40',
    tables: ['order_items', 'orders'],
  },
  {
    name: 'between and null-safe coalesce on balance',
    sql:
      'SELECT id, name, COALESCE(balance, 0) AS bal FROM customers ' +
      'WHERE COALESCE(balance, 0) BETWEEN 0 AND 500 ORDER BY bal DESC, id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'intersect all-style customer buyers',
    sql:
      'SELECT customer_id AS id FROM orders WHERE total > 10 ' +
      'INTERSECT ' +
      'SELECT id FROM customers WHERE id <= 50 ' +
      'ORDER BY id LIMIT 30',
    tables: ['orders', 'customers'],
  },
  {
    name: 'derived table status revenue share',
    sql:
      'SELECT status, revenue, ' +
      'ROUND(revenue * 100 / SUM(revenue) OVER (), 2) AS pct ' +
      'FROM (' +
      '  SELECT CAST(status AS CHAR) AS status, SUM(total) AS revenue FROM orders GROUP BY status' +
      ') s ORDER BY revenue DESC, status',
    tables: ['orders'],
  },
  // --- utf8mb4_general_ci ---
  //
  // The executor compares one collation, utf8mb4_0900_ai_ci, and refuses the
  // rest at bind time. general_ci is MySQL 5.x's default and most existing
  // schemas still carry it, so these run against a column declared with it.
  // They WARN until the executor can compare it, then flip to PASS - which is
  // what makes the fix verifiable rather than asserted.
  {
    name: 'general_ci: equality folds ASCII case',
    sql: "SELECT COUNT(*) AS n FROM customers WHERE legacy_label = 'active'",
    tables: ['customers'],
  },
  {
    name: 'general_ci: equality folds Latin-1 accents onto the base letter',
    sql: "SELECT COUNT(*) AS n FROM customers WHERE legacy_label = 'arger'",
    tables: ['customers'],
  },
  {
    name: 'general_ci: trailing spaces are insignificant (PAD SPACE)',
    sql: "SELECT COUNT(*) AS n FROM customers WHERE legacy_label = 'pending'",
    tables: ['customers'],
  },
  {
    name: 'general_ci: every supplementary character compares equal',
    sql: "SELECT COUNT(*) AS n FROM customers WHERE legacy_label = '\u{1f600}'",
    tables: ['customers'],
  },
  {
    name: 'general_ci: grouping partitions by collated equality',
    sql: 'SELECT legacy_label, COUNT(*) AS n FROM customers GROUP BY legacy_label ORDER BY n DESC, legacy_label',
    tables: ['customers'],
  },
  {
    name: 'general_ci: ordering follows the collation, not code points',
    sql: 'SELECT legacy_label FROM customers WHERE legacy_label IS NOT NULL ORDER BY legacy_label, id LIMIT 25',
    tables: ['customers'],
  },
  {
    name: 'general_ci: DISTINCT collapses collation-equal values',
    sql: 'SELECT DISTINCT legacy_label FROM customers ORDER BY legacy_label',
    tables: ['customers'],
  },
  {
    // Counts only. Which SPELLING a case-insensitive group reports is the
    // separate case below; this one asserts that the join matches the right
    // rows, which is what the collation decides.
    name: 'general_ci: joining on a collated column',
    sql: 'SELECT COUNT(*) AS n FROM customers c JOIN customers d ON c.legacy_label = d.legacy_label',
    tables: ['customers'],
  },
  {
    // 'Active' and 'active' are one group under general_ci, and both engines
    // agree on its size - the counts match exactly. They disagree on which of
    // the two spellings represents it, because each reports the first one its
    // own scan happened to reach, and the scans do not share an order. MySQL
    // does not define this either; matching it would mean matching its row
    // order, which is not something a different storage engine can promise.
    name: 'general_ci: representative spelling of a collated group',
    sql: 'SELECT legacy_label, COUNT(*) AS n FROM customers GROUP BY legacy_label ORDER BY n, legacy_label',
    tables: ['customers'],
    documentedGap:
      'the spelling reported for a case-insensitively equal group follows scan order, which differs from MySQL (#10)',
  },
  {
    // Every comparison here is internally consistent - the join compares two
    // general_ci columns, the grouping compares one 0900_ai_ci column - and
    // MySQL answers it. Pintail resolves ONE collation per query, so it
    // refuses. A real limitation of that choice, kept visible rather than
    // written out of the corpus.
    name: 'general_ci: mixing collations across separate comparisons',
    sql: 'SELECT c.tier, COUNT(*) AS n FROM customers c JOIN customers d ON c.legacy_label = d.legacy_label GROUP BY c.tier ORDER BY c.tier',
    tables: ['customers'],
    documentedGap:
      'a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10)',
  },
  {
    // MySQL orders an ENUM by its DECLARED ordinal - for orders.status that
    // is pending, processing, shipped, delivered, cancelled - never by the
    // label text. The whole family below pins the ordinal rule across every
    // surface it governs; label ordering passes none of them, because the
    // declaration order and the alphabetical order disagree everywhere.
    name: 'enum: order by ascends by declared ordinal',
    sql: 'SELECT id, status FROM orders ORDER BY status, id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'enum: order by descends by declared ordinal',
    sql: 'SELECT id, status FROM orders ORDER BY status DESC, id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'enum: min and max follow the ordinal',
    sql: 'SELECT MIN(status), MAX(status) FROM orders',
    tables: ['orders'],
  },
  {
    // A string constant in a range predicate coerces to its declared
    // ordinal: > 'processing' keeps shipped, delivered AND cancelled.
    name: 'enum: a greater-than range compares ordinals',
    sql: "SELECT COUNT(*) FROM orders WHERE status > 'processing'",
    tables: ['orders'],
  },
  {
    name: 'enum: a less-than range compares ordinals',
    sql: "SELECT COUNT(*) FROM orders WHERE status < 'delivered'",
    tables: ['orders'],
  },
  {
    name: 'enum: between spans the declared interval',
    sql: "SELECT COUNT(*) FROM orders WHERE status BETWEEN 'processing' AND 'delivered'",
    tables: ['orders'],
  },
  {
    name: 'enum: distinct orders by ordinal',
    sql: 'SELECT DISTINCT status FROM orders ORDER BY status',
    tables: ['orders'],
  },
  {
    name: 'enum: a limited sort keeps the lowest ordinals',
    sql: 'SELECT id, status FROM orders ORDER BY status, id LIMIT 5',
    tables: ['orders'],
  },
  {
    name: 'enum: a window order walks the ordinal',
    sql:
      'SELECT id, status, ROW_NUMBER() OVER (ORDER BY status, id) AS r ' +
      'FROM orders ORDER BY r LIMIT 40',
    tables: ['orders'],
  },
]

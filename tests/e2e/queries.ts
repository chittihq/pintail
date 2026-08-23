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
  // conformance: Chitti's adversarial seed, exercised by design intent.
  {
    name: 'conformance: triple-alias person join with a dangling FK',
    sql:
      'SELECT f.factId, c.name AS created_by, u.name AS updated_by, o.name AS owned_by ' +
      'FROM Fact f LEFT JOIN Person c ON c.personId = f.createdBy ' +
      'LEFT JOIN Person u ON u.personId = f.updatedBy ' +
      'LEFT JOIN Person o ON o.personId = f.ownedBy ORDER BY f.factId',
    tables: ['Fact', 'Person'],
  },
  {
    // Which equal spelling represents a ci group is undefined in MySQL
    // itself, so the projection folds it: both engines must agree on the
    // groups, counts and sums, never on an unspecified representative.
    name: 'conformance: mixed-collation double grouping',
    sql:
      'SELECT UPPER(d.code) AS code_key, UPPER(d.label) AS label_key, ' +
      'COUNT(*) AS n, ROUND(SUM(f.amount), 2) AS total ' +
      'FROM Dim d JOIN Fact f ON f.dimId = d.dimId ' +
      'GROUP BY d.code, d.label ORDER BY code_key, label_key, n',
    tables: ['Dim', 'Fact'],
  },
  {
    name: 'conformance: enum ordinal ordering disagrees with labels',
    sql: 'SELECT dimId, status FROM Dim ORDER BY status, dimId',
    tables: ['Dim'],
  },
  {
    // TRIM folds the PAD-space representative the same way UPPER folds case.
    name: 'conformance: trailing-space grouping under PAD semantics',
    sql: "SELECT CONCAT('[', TRIM(padded), ']') AS k, COUNT(*) AS n FROM Dim GROUP BY padded ORDER BY k, n",
    tables: ['Dim'],
  },
  {
    name: 'conformance: case-variant code grouping',
    sql: 'SELECT UPPER(code) AS code_key, COUNT(*) AS n FROM Fact GROUP BY code ORDER BY code_key, n',
    tables: ['Fact'],
  },
  {
    name: 'conformance: anti-join finds the event-less dimension',
    sql:
      'SELECT d.dimId FROM Dim d LEFT JOIN Event e ON e.dimId = d.dimId ' +
      'WHERE e.eventId IS NULL ORDER BY d.dimId',
    tables: ['Dim', 'Event'],
  },
  {
    name: 'conformance: nullable join key NULL-extends',
    sql:
      'SELECT f.factId, d.code FROM Fact f LEFT JOIN Dim d ON d.dimId = f.nullableDimId ' +
      'ORDER BY f.factId',
    tables: ['Fact', 'Dim'],
  },
  {
    name: 'conformance: timestamp ties page deterministically with a tiebreaker',
    sql: 'SELECT factId, createdAt FROM Fact ORDER BY createdAt DESC, factId DESC LIMIT 5',
    tables: ['Fact'],
  },
  {
    name: 'conformance: date bucketing over the fact table',
    sql:
      'SELECT DATE(createdAt) AS d, COUNT(*) AS n, ROUND(SUM(amount), 2) AS total ' +
      'FROM Fact GROUP BY DATE(createdAt) ORDER BY d',
    tables: ['Fact'],
  },
  {
    name: 'conformance: decimal aggregate spanning negatives and zero',
    sql:
      'SELECT ROUND(SUM(amount), 2), ROUND(MIN(amount), 2), ROUND(MAX(amount), 2), COUNT(*) FROM Fact',
    tables: ['Fact'],
  },
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
    // The high-volume generated oracle found that SQL's unary-minus AST
    // around `-2` sent an exact computed DECIMAL through approximate
    // nearest-even rounding: the seeded 50.00 row returned 0, not 100.
    name: 'computed decimal rounds negative digits half away from zero',
    sql:
      'SELECT id, total, ROUND(total + 0.00, -2) AS rounded, ' +
      'TRUNCATE(total + 0.00, -2) AS truncated FROM orders ORDER BY id LIMIT 40',
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
    name: 'enum: min and max compare as strings',
    sql: 'SELECT MIN(status), MAX(status) FROM orders',
    tables: ['orders'],
  },
  {
    // MySQL compares an ENUM to a string constant AS A STRING (confirmed
    // differentially); only sorting follows the declared ordinal.
    name: 'enum: a greater-than range compares as strings',
    sql: "SELECT COUNT(*) FROM orders WHERE status > 'processing'",
    tables: ['orders'],
  },
  {
    name: 'enum: a less-than range compares as strings',
    sql: "SELECT COUNT(*) FROM orders WHERE status < 'delivered'",
    tables: ['orders'],
  },
  {
    name: 'enum: between compares as strings',
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
  {
    // Grouping keys of two collations answers, each key folded by its own
    // rules (the 0.0.3 refusal a customer hit on sectionName+schoolName).
    // Wrapped in a count so the assertion is fold arithmetic, not the
    // representative spelling documented as gap #10.
    name: 'collation: mixed grouping answers with per-key folds',
    sql:
      'SELECT COUNT(*) AS n_groups FROM ' +
      '(SELECT legacy_label, tier FROM customers GROUP BY legacy_label, tier) g',
    tables: ['customers'],
  },
  {
    // Each DISTINCT folds under its own column's collation in one query:
    // legacy_label under general_ci, name under 0900_ai_ci.
    name: 'collation: distinct counts fold per column collation',
    sql: 'SELECT COUNT(DISTINCT legacy_label) AS ci_folds, COUNT(DISTINCT name) AS ai_folds FROM customers',
    tables: ['customers'],
  },
  {
    name: 'collation: regrouping a mixed grouping stays exact',
    sql:
      'SELECT tier, COUNT(*) AS n FROM ' +
      '(SELECT legacy_label, tier FROM customers GROUP BY legacy_label, tier) g ' +
      'GROUP BY tier ORDER BY tier',
    tables: ['customers'],
  },
  {
    // MySQL orders a SET by its member bitmask ('fragile'=1 before
    // 'tracked'=8, 'fragile,priority'=5 between), never alphabetically.
    name: 'set: order by walks the member bitmask',
    sql: 'SELECT id, services FROM shipments ORDER BY services, id',
    tables: ['shipments'],
  },
  {
    name: 'set: grouping orders groups by bitmask',
    sql: 'SELECT services, COUNT(*) FROM shipments GROUP BY services ORDER BY services',
    tables: ['shipments'],
  },
  {
    // ENUM('') has a REAL empty member at ordinal 1: it sorts and groups
    // FIRST by declaration index, and 'zz' (2) stays ahead of 'aa' (3) —
    // a path that demotes '' to plain text goes alphabetical instead.
    name: 'enum: the empty member groups by its ordinal',
    sql: 'SELECT v, COUNT(*) AS n FROM badges GROUP BY v ORDER BY v',
    tables: ['badges'],
  },
  {
    name: 'enum: the empty member sorts by its ordinal',
    sql: 'SELECT id FROM badges ORDER BY v, id',
    tables: ['badges'],
  },
  {
    name: 'enum: the empty member is selectable by text',
    sql: "SELECT COUNT(*) AS n FROM badges WHERE v = ''",
    tables: ['badges'],
  },
  {
    // GEOMETRY reads back byte-identical to MySQL: internal SRID + WKB.
    name: 'geometry: hex round-trips the internal format',
    sql: 'SELECT id, HEX(route) FROM shipments ORDER BY id',
    tables: ['shipments'],
  },
  {
    // MySQL's LENGTH of a geometry is the WKB byte count plus the 4-byte
    // SRID prefix; both engines must agree on the exact byte size.
    name: 'geometry: byte length includes the srid prefix',
    sql: 'SELECT id, LENGTH(route) AS bytes FROM shipments ORDER BY id',
    tables: ['shipments'],
  },
  {
    name: 'geometry: null routes filter and count',
    sql:
      'SELECT COUNT(*) AS total, COUNT(route) AS routed FROM shipments',
    tables: ['shipments'],
  },
  {
    // Spatial functions do not exist in the mirror (docs/limitations.md:
    // WKB is retained but there is no spatial logical type or function).
    // The refusal stays visible as a WARN rather than written out.
    name: 'geometry: spatial functions are a documented gap',
    sql: 'SELECT id, ST_AsText(route) AS wkt FROM shipments ORDER BY id',
    tables: ['shipments'],
    documentedGap:
      'spatial query functions are not implemented; geometry is carried as bytes only',
  },
  {
    name: 'set: find_in_set filters by membership',
    sql:
      "SELECT id, tags FROM customers WHERE FIND_IN_SET('vip', tags) > 0 ORDER BY id",
    tables: ['customers'],
  },
  {
    // Confirmed against live 8.4: comparing a SET to a string does NOT
    // reorder members - 'vip,alpha' matches nothing, only the stored
    // declaration-order spelling 'alpha,vip' matches. Both directions
    // are pinned so neither engine starts normalizing.
    name: 'set: equality is literal, not member-normalized',
    sql:
      "SELECT SUM(tags = 'vip,alpha') AS reversed_hits, SUM(tags = 'alpha,vip') AS declared_hits " +
      'FROM customers',
    tables: ['customers'],
  },
  {
    name: 'set: distinct values walk the bitmask including empty',
    sql: 'SELECT DISTINCT tags FROM customers ORDER BY tags',
    tables: ['customers'],
  },
  {
    name: 'set: grouped counts order by bitmask not text',
    sql: 'SELECT tags, COUNT(*) AS n FROM customers GROUP BY tags ORDER BY tags',
    tables: ['customers'],
  },
  {
    // MySQL compares SET values numerically (by member bitmask), so a
    // range predicate over tags is a bitmask range, not a string range.
    name: 'set: a range predicate compares the bitmask',
    sql: "SELECT id, tags FROM customers WHERE tags > 'alpha' ORDER BY tags, id",
    tables: ['customers'],
  },

  // -------------------------------------------------------------------------
  // Corpus rebalancing (2026-08-21 audit): the shapes below widen the thin
  // spots the diversity audit named - joins past three tables, JSON reads
  // beyond one extract, temporal grains beyond DATE_FORMAT, and regex beyond
  // a single query. The star schema (Fact/Dim/Person/Event) carries the
  // high-arity joins; the row counts stay small so the sweep stays fast.

  { // 4 aliases, 3 tables: the star's audit columns resolve through two
    // Person aliases while the dimension inner-joins.
    name: 'star: fact with dimension and two audit persons',
    sql:
      'SELECT f.factId, d.code, pc.name AS created_by, pu.name AS updated_by ' +
      'FROM Fact f JOIN Dim d ON d.dimId = f.dimId ' +
      'LEFT JOIN Person pc ON pc.personId = f.createdBy ' +
      'LEFT JOIN Person pu ON pu.personId = f.updatedBy ORDER BY f.factId',
    tables: ['Fact', 'Dim', 'Person'],
  },
  {
    name: 'star: five-alias chain fans out through events',
    sql:
      'SELECT f.factId, d.code, e.eventId, p.name AS owner ' +
      'FROM Fact f JOIN Dim d ON d.dimId = f.dimId ' +
      'LEFT JOIN Event e ON e.dimId = d.dimId ' +
      'LEFT JOIN Person p ON p.personId = f.ownedBy ' +
      'ORDER BY f.factId, e.eventId, d.code',
    tables: ['Fact', 'Dim', 'Event', 'Person'],
  },
  {
    name: 'star: grouped rollup counts facts and events per dimension',
    sql:
      'SELECT d.dimId, UPPER(d.code) AS code_key, COUNT(DISTINCT f.factId) AS facts, ' +
      'COUNT(DISTINCT e.eventId) AS events ' +
      'FROM Dim d LEFT JOIN Fact f ON f.dimId = d.dimId ' +
      'LEFT JOIN Event e ON e.dimId = d.dimId ' +
      'GROUP BY d.dimId, code_key ORDER BY d.dimId',
    tables: ['Dim', 'Fact', 'Event'],
  },
  {
    name: 'star: five tables bridge the shop and the star',
    sql:
      'SELECT i.order_id, i.line_no, c.name AS customer, f.code AS fact_code, d.code AS dim_code ' +
      'FROM order_items i JOIN orders o ON o.id = i.order_id ' +
      'JOIN customers c ON c.id = o.customer_id ' +
      'JOIN Fact f ON f.factId = i.line_no ' +
      'LEFT JOIN Dim d ON d.dimId = f.dimId ' +
      'ORDER BY i.order_id, i.line_no',
    tables: ['order_items', 'orders', 'customers', 'Fact', 'Dim'],
  },
  {
    name: 'star: null join keys stay unmatched through a four-table chain',
    sql:
      'SELECT f.factId, d2.code AS nullable_dim, p.name AS owner, e.eventId ' +
      'FROM Fact f LEFT JOIN Dim d2 ON d2.dimId = f.nullableDimId ' +
      'LEFT JOIN Person p ON p.personId = f.ownedBy ' +
      'LEFT JOIN Event e ON e.dimId = f.nullableDimId ' +
      'ORDER BY f.factId, e.eventId',
    tables: ['Fact', 'Dim', 'Person', 'Event'],
  },
  {
    name: 'star: date-windowed join keeps only overlapping activity',
    sql:
      'SELECT f.factId, e.eventId, e.at FROM Fact f JOIN Event e ON e.dimId = f.dimId ' +
      "WHERE e.at >= '2025-01-01' AND f.createdAt < '2025-07-23' " +
      'ORDER BY f.factId, e.eventId',
    tables: ['Fact', 'Event'],
  },
  {
    name: 'json: length and keys survive null documents',
    sql: 'SELECT id, JSON_LENGTH(meta) AS len, JSON_KEYS(meta) AS ks FROM customers ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'json: contains_path filters the documented rows',
    sql:
      "SELECT id FROM customers WHERE JSON_CONTAINS_PATH(meta, 'one', '$.score') ORDER BY id",
    tables: ['customers'],
  },
  {
    name: 'json: json_value reads a scalar with sql semantics',
    sql:
      "SELECT id, JSON_VALUE(meta, '$.score') AS score FROM customers " +
      'WHERE meta IS NOT NULL ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'json: object construction embeds an extracted scalar',
    sql:
      "SELECT id, JSON_OBJECT('tier', tier, 'score', meta -> '$.score') AS doc " +
      'FROM customers ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'json: search locates a literal value',
    sql: "SELECT id, JSON_SEARCH(meta, 'one', 'en') AS hit FROM customers ORDER BY id",
    tables: ['customers'],
  },
  {
    name: 'json: grouping by an extracted scalar',
    sql:
      "SELECT meta ->> '$.lang' AS lang, COUNT(*) AS n FROM customers GROUP BY lang ORDER BY lang",
    tables: ['customers'],
  },
  {
    name: 'json: merge_patch overlays and reads back',
    sql:
      'SELECT id, JSON_TYPE(JSON_MERGE_PATCH(meta, \'{"seen":true}\')) AS t, ' +
      "JSON_UNQUOTE(JSON_EXTRACT(JSON_MERGE_PATCH(meta, '{\"seen\":true}'), '$.seen')) AS seen " +
      'FROM customers WHERE meta IS NOT NULL ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'temporal: quarter, weekday and name grains agree',
    sql:
      'SELECT id, QUARTER(placed_on) AS q, DAYOFWEEK(placed_on) AS dw, ' +
      'DAYNAME(placed_on) AS dn, MONTHNAME(placed_on) AS mn FROM orders ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'temporal: month-end bucketing via last_day',
    sql:
      'SELECT LAST_DAY(placed_on) AS month_end, COUNT(*) AS n FROM orders ' +
      'GROUP BY month_end ORDER BY month_end',
    tables: ['orders'],
  },
  {
    name: 'temporal: timestampdiff spans date and datetime operands',
    sql:
      'SELECT factId, TIMESTAMPDIFF(DAY, effectiveFrom, createdAt) AS age_days ' +
      'FROM Fact ORDER BY factId',
    tables: ['Fact'],
  },
  {
    name: 'temporal: datetime range keeps the year window',
    sql:
      "SELECT eventId, at FROM Event WHERE at BETWEEN '2025-01-01' AND '2025-12-31 23:59:59' " +
      'ORDER BY eventId',
    tables: ['Event'],
  },
  {
    name: 'temporal: date_sub bound in the predicate',
    sql:
      "SELECT id FROM orders WHERE placed_on >= DATE_SUB('2024-12-31', INTERVAL 6 MONTH) " +
      'ORDER BY id',
    tables: ['orders'],
  },
  {
    name: 'temporal: year-month split grouping',
    sql:
      'SELECT YEAR(placed_on) AS y, MONTH(placed_on) AS m, COUNT(*) AS n FROM orders ' +
      'GROUP BY y, m ORDER BY y, m',
    tables: ['orders'],
  },
  {
    name: 'temporal: sub-day grains on a microsecond timestamp',
    sql:
      'SELECT id, HOUR(updated_at) AS h, MINUTE(updated_at) AS mi FROM orders ' +
      'WHERE updated_at IS NOT NULL ORDER BY id LIMIT 30',
    tables: ['orders'],
  },
  {
    name: 'regex: substr extracts the mail domain',
    sql: "SELECT id, REGEXP_SUBSTR(email, '@[a-z.]+') AS domain FROM customers ORDER BY id",
    tables: ['customers'],
  },
  {
    name: 'regex: the REGEXP operator anchors a class',
    sql: "SELECT id, name FROM customers WHERE name REGEXP '^[A-G]' ORDER BY id",
    tables: ['customers'],
  },
  {
    name: 'regex: replace folds suffix classes before grouping',
    sql:
      "SELECT REGEXP_REPLACE(status, '(ing|ed)$', '*') AS s, COUNT(*) AS n FROM orders " +
      'GROUP BY status ORDER BY s, n',
    tables: ['orders'],
  },

  // -------------------------------------------------------------------------
  // BI-tool shapes (tests/corpus/bi-shapes.sql), previously an optional
  // side harness, now gate queries. Each is the documented compilation
  // pattern of Metabase, Superset, Looker or Tableau, adapted to the gate
  // schema with deterministic anchors: fixed dates replace CURDATE(),
  // ties order through id, and ANY_VALUE reads a group-constant column.

  {
    name: 'bi metabase: month grain through convert_tz',
    sql:
      "SELECT DATE_FORMAT(CONVERT_TZ(updated_at, '+00:00', '-05:00'), '%Y-%m-01') AS grain, " +
      'COUNT(*) AS n, ROUND(SUM(total), 2) AS revenue FROM orders ' +
      'WHERE updated_at IS NOT NULL GROUP BY grain ORDER BY grain',
    tables: ['orders'],
  },
  {
    name: 'bi metabase: iso week bucketing',
    sql:
      "SELECT DATE_FORMAT(placed_on, '%x-%v') AS iso_week, COUNT(*) AS n FROM orders " +
      'GROUP BY iso_week ORDER BY iso_week',
    tables: ['orders'],
  },
  {
    name: 'bi metabase: display formats for weekday, pretty date and clock',
    sql:
      "SELECT id, DATE_FORMAT(placed_on, '%W') AS weekday_name, " +
      "DATE_FORMAT(placed_on, '%b %e, %Y') AS pretty, DATE_FORMAT(updated_at, '%r') AS clock " +
      'FROM orders ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'bi metabase: previous-period revenue window',
    sql:
      'SELECT ROUND(COALESCE(SUM(total), 0), 2) AS revenue FROM orders ' +
      "WHERE placed_on >= DATE_ADD('2024-07-01', INTERVAL -1 MONTH) AND placed_on < '2024-07-01'",
    tables: ['orders'],
  },
  {
    name: 'bi superset: week-start grain with a rolling average',
    sql:
      'SELECT DATE_ADD(placed_on, INTERVAL -WEEKDAY(placed_on) DAY) AS week_start, ' +
      'ROUND(SUM(total), 2) AS revenue, ' +
      'ROUND(AVG(SUM(total)) OVER (ORDER BY DATE_ADD(placed_on, INTERVAL -WEEKDAY(placed_on) DAY) ' +
      'ROWS BETWEEN 6 PRECEDING AND CURRENT ROW), 2) AS rolling_7 ' +
      'FROM orders GROUP BY week_start ORDER BY week_start',
    tables: ['orders'],
  },
  {
    name: 'bi superset: running total over grouped revenue',
    sql:
      'SELECT customer_id, ROUND(SUM(total), 2) AS revenue, ' +
      'ROUND(SUM(SUM(total)) OVER (ORDER BY customer_id ' +
      'ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW), 2) AS running ' +
      'FROM orders GROUP BY customer_id ORDER BY customer_id',
    tables: ['orders'],
  },
  {
    name: 'bi superset: lag and lead against a named window',
    sql:
      'SELECT id, total, LAG(total, 1, 0) OVER w AS prev_total, LEAD(total) OVER w AS next_total ' +
      'FROM orders WINDOW w AS (PARTITION BY customer_id ORDER BY placed_on, id) ' +
      'ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'bi superset: quartile counts from ntile',
    sql:
      'SELECT quartile, COUNT(*) AS n FROM ' +
      '(SELECT NTILE(4) OVER (ORDER BY total, id) AS quartile FROM orders) q ' +
      'GROUP BY quartile ORDER BY quartile',
    tables: ['orders'],
  },
  {
    name: 'bi superset: first and last value over an unbounded frame',
    sql:
      'SELECT id, FIRST_VALUE(status) OVER w AS first_status, LAST_VALUE(status) OVER w AS last_status ' +
      'FROM orders WINDOW w AS (PARTITION BY customer_id ORDER BY placed_on, id ' +
      'ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'bi superset: compound interval grains',
    sql:
      "SELECT id, DATE_ADD(updated_at, INTERVAL '1-2' YEAR_MONTH) AS shifted, " +
      "DATE_SUB(updated_at, INTERVAL '3 4:00:00' DAY_SECOND) AS backdated " +
      'FROM orders WHERE updated_at IS NOT NULL ORDER BY id LIMIT 40',
    tables: ['orders'],
    documentedGap:
      "compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them",
  },
  {
    // Looker's symmetric aggregate: MD5 the key, CONV a 15-hex-digit
    // prefix to decimal, scale, and reassemble a fan-out-safe SUM.
    name: 'bi looker: symmetric aggregate across a fanned-out join',
    sql:
      'SELECT COALESCE(CAST(SUM(DISTINCT CAST(CONV(SUBSTR(MD5(o.id), 1, 15), 16, 10) AS DECIMAL(38,0)) * 1000000000 ' +
      '+ CAST(o.total * 100 AS DECIMAL(38,0))) ' +
      '- SUM(DISTINCT CAST(CONV(SUBSTR(MD5(o.id), 1, 15), 16, 10) AS DECIMAL(38,0)) * 1000000000) ' +
      'AS DECIMAL(38,0)) / 100, 0) AS total_revenue ' +
      'FROM orders o LEFT JOIN order_items i ON i.order_id = o.id',
    tables: ['orders', 'order_items'],
  },
  {
    // ANY_VALUE over a group-constant column so both engines must agree.
    name: 'bi looker: any_value reads a functionally dependent column',
    sql:
      'SELECT id, ANY_VALUE(status) AS a_status, COUNT(*) AS n FROM orders ' +
      'GROUP BY id ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'bi tableau: explicit cast ladder',
    sql:
      'SELECT id, CAST(total AS DECIMAL(18,4)) AS amt, CAST(updated_at AS DATE) AS d, ' +
      'CAST(customer_id AS CHAR(32)) AS t, CONVERT(status, CHAR) AS s ' +
      'FROM orders ORDER BY id LIMIT 40',
    tables: ['orders'],
  },
  {
    name: 'bi tableau: the stddev and variance family',
    sql:
      'SELECT ROUND(STDDEV(total), 2) AS sd, ROUND(STDDEV_POP(total), 2) AS sdp, ' +
      'ROUND(STDDEV_SAMP(total), 2) AS sds, ROUND(VARIANCE(total) * 1E-12, 6) AS v, ' +
      'ROUND(VAR_POP(total) * 1E-12, 6) AS vp, ROUND(VAR_SAMP(total) * 1E-12, 6) AS vs FROM orders',
    tables: ['orders'],
  },
  {
    name: 'bi tableau: bit aggregates over an unsigned flag column',
    sql: 'SELECT BIT_OR(u8) AS any_flag, BIT_AND(u8) AS all_flags, BIT_XOR(u8) AS parity FROM counters',
    tables: ['counters'],
  },
  {
    name: 'bi shared: substring_index dimension cleanup',
    sql:
      "SELECT id, SUBSTRING_INDEX(email, '@', -1) AS domain, " +
      "SUBSTRING_INDEX(SUBSTRING_INDEX(email, '@', 1), '.', 1) AS localpart " +
      'FROM customers ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'bi shared: json validity and typed path filter',
    sql:
      'SELECT id FROM customers WHERE JSON_VALID(meta) ' +
      "AND JSON_CONTAINS(meta, '\"en\"', '$.lang') " +
      "AND JSON_TYPE(JSON_EXTRACT(meta, '$.score')) = 'INTEGER' ORDER BY id",
    tables: ['customers'],
  },
  {
    name: 'bi shared: contains_path over several paths at once',
    sql:
      "SELECT id, JSON_CONTAINS_PATH(meta, 'all', '$.lang', '$.score') AS has_both " +
      'FROM customers ORDER BY id',
    tables: ['customers'],
  },
  {
    name: 'bi shared: maketime from extracted parts',
    sql:
      'SELECT id, MAKETIME(HOUR(updated_at), MINUTE(updated_at), 0) AS t FROM orders ' +
      'WHERE updated_at IS NOT NULL ORDER BY id LIMIT 30',
    tables: ['orders'],
  },
  {
    name: 'bi shared: extract year_month grouping',
    sql:
      'SELECT EXTRACT(YEAR_MONTH FROM placed_on) AS ym, COUNT(*) AS n FROM orders ' +
      'GROUP BY ym ORDER BY ym',
    tables: ['orders'],
  },
  {
    name: 'bi shared: keyset-free pagination with limit offset',
    sql: 'SELECT id, total FROM orders ORDER BY total DESC, id LIMIT 10 OFFSET 10',
    tables: ['orders'],
  },

  // -------------------------------------------------------------------------
  // Concentration rebalancing (review 2026-08-21): 112 of 147 queries were
  // single-table and orders/customers carried most of the load. These
  // twelve spread reads across staff, counters, audit_log, Person, Event,
  // Dim, order_items and shipments, seven of them through joins.

  {
    name: 'staff: three-level management chain with an inactive tail',
    sql:
      'SELECT s.id, s.name, s.active, m.name AS manager, gm.name AS grand_manager ' +
      'FROM staff s LEFT JOIN staff m ON m.id = s.manager_id ' +
      'LEFT JOIN staff gm ON gm.id = m.manager_id ORDER BY s.id',
    tables: ['staff'],
  },
  {
    name: 'staff: active split with id extremes',
    sql:
      'SELECT active, COUNT(*) AS n, MIN(id) AS lo, MAX(id) AS hi FROM staff ' +
      'GROUP BY active ORDER BY active',
    tables: ['staff'],
  },
  {
    name: 'counters: full unsigned ladder readback',
    sql: 'SELECT id, u8, u16, u32, u64, s64 FROM counters ORDER BY id',
    tables: ['counters'],
  },
  {
    name: 'counters: greatest and least across widths',
    sql: 'SELECT id, GREATEST(u8, u16) AS g, LEAST(u32, u64) AS l FROM counters ORDER BY id',
    tables: ['counters'],
  },
  {
    name: 'dim: enum status split',
    sql: 'SELECT status, COUNT(*) AS n FROM Dim GROUP BY status ORDER BY status',
    tables: ['Dim'],
  },
  {
    name: 'dim: pattern filter across collated columns',
    sql: "SELECT dimId, code, label FROM Dim WHERE code LIKE '%a%' ORDER BY dimId",
    tables: ['Dim'],
  },
  {
    name: 'person: anti-join finds owners without facts',
    sql:
      'SELECT p.personId, p.name FROM Person p ' +
      'LEFT JOIN Fact f ON f.ownedBy = p.personId ' +
      'WHERE f.factId IS NULL ORDER BY p.personId',
    tables: ['Person', 'Fact'],
  },
  {
    name: 'person: created-fact counts through a scalar subquery',
    sql:
      'SELECT p.personId, p.name, ' +
      '(SELECT COUNT(*) FROM Fact f WHERE f.createdBy = p.personId) AS created ' +
      'FROM Person p ORDER BY p.personId',
    tables: ['Person', 'Fact'],
  },
  {
    name: 'event: lag over per-dimension timelines',
    sql:
      'SELECT eventId, dimId, LAG(at) OVER (PARTITION BY dimId ORDER BY at, eventId) AS prev_at ' +
      'FROM Event ORDER BY eventId',
    tables: ['Event'],
  },
  {
    name: 'event: daily grain per dimension code',
    sql:
      'SELECT d.code, DATE(e.at) AS day, COUNT(*) AS n FROM Event e ' +
      'JOIN Dim d ON d.dimId = e.dimId GROUP BY d.code, day ORDER BY d.code, day',
    tables: ['Event', 'Dim'],
  },
  {
    name: 'order_items: product rollup without the orders table',
    sql:
      'SELECT product, SUM(qty) AS units, ROUND(SUM(qty * price), 2) AS revenue ' +
      'FROM order_items GROUP BY product ORDER BY product',
    tables: ['order_items'],
  },
  {
    name: 'shipments: carrier value through the items bridge',
    sql:
      'SELECT s.carrier, COUNT(DISTINCT s.order_id) AS shipped_orders, ' +
      'ROUND(SUM(i.qty * i.price), 2) AS shipped_value ' +
      'FROM shipments s JOIN order_items i ON i.order_id = s.order_id ' +
      'GROUP BY s.carrier ORDER BY s.carrier',
    tables: ['shipments', 'order_items'],
  },
  {
    // Chitti's coll-10: JSON-extracted strings compare utf8mb4_bin, and
    // that collation must survive a derived-table boundary - DISTINCT
    // above it keeps "Google" and "google" apart exactly as MySQL does.
    name: 'json: distinct case variants survive a derived table',
    sql:
      "SELECT COUNT(DISTINCT s) AS variants FROM " +
      "(SELECT meta->>'$.lang' AS s FROM customers WHERE meta IS NOT NULL) d",
    tables: ['customers'],
  },
]

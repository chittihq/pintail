//! `LATERAL` derived tables against `MySQL`: inner and outer joins, empty
//! and NULL-keyed inner results, top-N per group, set operations, nesting,
//! and the table inside CTEs and subqueries. Every case orders its result
//! by a unique key, so a row out of place is a failure.

use super::OracleCase;

const FAMILY: &str = "lateral derived table";

const SQL: &[&str] = &[
    // Inner forms: comma, JOIN, CROSS JOIN and STRAIGHT_JOIN all drop a
    // left row the table has nothing for.
    "SELECT u.id, d.id, d.total FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON TRUE ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u CROSS JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u STRAIGHT_JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON d.total > u.id * 10 ORDER BY u.id, d.id",
    "SELECT * FROM users u, LATERAL (SELECT o.id AS order_id, o.total, o.status FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.order_id",
    "SELECT d.*, u.name FROM users u, LATERAL (SELECT o.id AS order_id, o.placed_at FROM orders o WHERE o.user_id = u.id) d ORDER BY d.order_id",
    // Outer forms keep the left row, with NULLs for the table's columns.
    "SELECT u.id, d.id, d.total FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON TRUE ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON d.total > 20 ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.total FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON d.total > u.id * 10 ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON FALSE ORDER BY u.id",
    "SELECT u.id FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND o.total > 60) d ON TRUE WHERE d.id IS NULL ORDER BY u.id",
    "SELECT u.id, COALESCE(d.total, -1) FROM users u LEFT JOIN LATERAL (SELECT o.total FROM orders o WHERE o.user_id = u.id + 8) d ON TRUE ORDER BY u.id",
    "SELECT o.id, d.name FROM orders o LEFT JOIN LATERAL (SELECT u.name FROM users u WHERE u.id = o.user_id) d ON TRUE ORDER BY o.id",
    // RIGHT JOIN swaps its operands, so a LATERAL table on the left of
    // one reads the table on its right, and one on the right reads what
    // precedes the join.
    "SELECT d.id, u.id FROM LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d RIGHT JOIN users u ON TRUE ORDER BY u.id, d.id",
    "SELECT e.id, u.id, d.id FROM events e, users u RIGHT JOIN LATERAL (SELECT o.id, o.user_id FROM orders o WHERE o.id = e.id) d ON d.user_id = u.id WHERE e.id < 5 ORDER BY e.id",
    // Top-N per group, the shape the clause exists for.
    "SELECT u.id, d.id, d.total FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.total DESC, o.id LIMIT 2) d ORDER BY u.id, d.total DESC, d.id",
    "SELECT u.id, d.id, d.total FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.total DESC, o.id LIMIT 1) d ORDER BY u.id",
    "SELECT u.id, d.id, d.placed_at FROM users u LEFT JOIN LATERAL (SELECT o.id, o.placed_at FROM orders o WHERE o.user_id = u.id ORDER BY o.placed_at DESC LIMIT 1) d ON TRUE ORDER BY u.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1 OFFSET 1) d ORDER BY u.id",
    "SELECT u.id, d.id FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.id DESC LIMIT 2, 5) d ON TRUE ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id <= u.id ORDER BY o.placed_at DESC LIMIT 3) d ORDER BY u.id, d.id",
    "SELECT e.id, d.id, d.score FROM events e, LATERAL (SELECT x.id, x.score FROM events x WHERE x.score > e.score ORDER BY x.score LIMIT 1) d ORDER BY e.id",
    "SELECT e.id, d.id FROM events e LEFT JOIN LATERAL (SELECT x.id FROM events x WHERE x.active = e.active AND x.id < e.id ORDER BY x.id DESC LIMIT 2) d ON TRUE ORDER BY e.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.total LIMIT 0) d ORDER BY u.id",
    // Aggregates: an ungrouped one answers every left row, a grouped one
    // only those with rows.
    "SELECT u.id, d.c, d.s, d.latest FROM users u, LATERAL (SELECT COUNT(*) AS c, SUM(o.total) AS s, MAX(o.placed_at) AS latest FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id",
    "SELECT u.id, d.c FROM users u, LATERAL (SELECT COUNT(*) AS c FROM orders o WHERE o.user_id = u.id + 20) d ORDER BY u.id",
    "SELECT u.id, d.status, d.c, d.s FROM users u, LATERAL (SELECT o.status, COUNT(*) AS c, SUM(o.total) AS s FROM orders o WHERE o.user_id = u.id GROUP BY o.status) d ORDER BY u.id, d.status",
    "SELECT u.id, d.status, d.c FROM users u LEFT JOIN LATERAL (SELECT o.status, COUNT(*) AS c FROM orders o WHERE o.user_id = u.id GROUP BY o.status HAVING COUNT(*) > 1) d ON TRUE ORDER BY u.id, d.status",
    "SELECT u.id, d.a, d.m FROM users u, LATERAL (SELECT AVG(o.total) AS a, MIN(o.total) AS m FROM orders o WHERE o.user_id >= u.id) d ORDER BY u.id",
    "SELECT u.id, d.s FROM users u, LATERAL (SELECT SUM(u.id) AS s FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id",
    "SELECT u.id, d.names FROM users u, LATERAL (SELECT GROUP_CONCAT(o.status ORDER BY o.id SEPARATOR '/') AS names FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id",
    "SELECT u.id, d.c FROM users u, LATERAL (SELECT COUNT(DISTINCT o.status) AS c FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id",
    // DISTINCT and set operations inside.
    "SELECT u.id, d.status FROM users u, LATERAL (SELECT DISTINCT o.status FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.status",
    "SELECT u.id, d.v FROM users u, LATERAL (SELECT o.id AS v FROM orders o WHERE o.user_id = u.id UNION ALL SELECT -u.id) d ORDER BY u.id, d.v",
    "SELECT u.id, d.v FROM users u, LATERAL (SELECT o.user_id AS v FROM orders o WHERE o.user_id = u.id UNION SELECT u.id) d ORDER BY u.id, d.v",
    "SELECT u.id, d.v FROM users u LEFT JOIN LATERAL (SELECT o.id AS v FROM orders o WHERE o.user_id = u.id INTERSECT SELECT e.id FROM events e WHERE e.active = 1) d ON TRUE ORDER BY u.id, d.v",
    "SELECT u.id, d.v FROM users u, LATERAL (SELECT o.id AS v FROM orders o WHERE o.user_id = u.id EXCEPT SELECT e.id FROM events e WHERE e.score > u.id * 20) d ORDER BY u.id, d.v",
    "SELECT u.id, d.v FROM users u, LATERAL ((SELECT o.id AS v FROM orders o WHERE o.user_id = u.id ORDER BY o.id DESC LIMIT 1) UNION ALL (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1)) d ORDER BY u.id, d.v",
    // NULL correlation values: `=` finds nothing for them, `<=>` finds
    // the other NULLs.
    "SELECT e.id, d.c FROM events e, LATERAL (SELECT COUNT(*) AS c FROM events x WHERE x.note = e.note) d ORDER BY e.id",
    "SELECT e.id, d.c FROM events e, LATERAL (SELECT COUNT(*) AS c FROM events x WHERE x.note <=> e.note) d ORDER BY e.id",
    "SELECT e.id, d.id FROM events e LEFT JOIN LATERAL (SELECT x.id FROM events x WHERE x.note = e.note AND x.id <> e.id) d ON TRUE ORDER BY e.id, d.id",
    "SELECT e.id, d.id FROM events e, LATERAL (SELECT x.id FROM events x WHERE x.note <=> e.note AND x.id > e.id) d ORDER BY e.id, d.id",
    "SELECT e.id, d.n FROM events e, LATERAL (SELECT e.note AS n) d ORDER BY e.id",
    "SELECT e.id, d.n FROM events e, LATERAL (SELECT COALESCE(e.note, 'none') AS n, e.score + 1 AS s) d WHERE d.s > 40 ORDER BY e.id",
    // No inner table at all, and outer values in the select list.
    "SELECT u.id, d.twice, d.loud FROM users u, LATERAL (SELECT u.id * 2 AS twice, CONCAT(u.name, '!') AS loud) d ORDER BY u.id",
    "SELECT u.id, d.n FROM users u, LATERAL (SELECT u.name AS n FROM DUAL WHERE u.id > 5) d ORDER BY u.id",
    "SELECT u.id, d.id, d.owner FROM users u, LATERAL (SELECT o.id, u.name AS owner FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.one FROM users u, LATERAL (SELECT 1 AS one) d ORDER BY u.id",
    "SELECT d.one FROM LATERAL (SELECT 1 AS one) d",
    "SELECT u.id, d.c FROM users u, LATERAL (SELECT COUNT(*) AS c FROM orders) d ORDER BY u.id",
    // Several earlier tables, and tables after it.
    "SELECT u.id, o.id, d.c FROM users u JOIN orders o ON o.user_id = u.id, LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.user_id = u.id AND p.total > o.total) d ORDER BY o.id",
    "SELECT u.id, o.id, d.c FROM users u JOIN orders o ON o.user_id = u.id JOIN LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.user_id = u.id AND p.id < o.id) d ON d.c > 0 ORDER BY o.id",
    "SELECT u.id, e.id, d.id FROM users u, events e JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND o.id = e.id) d ON TRUE ORDER BY u.id, e.id",
    "SELECT u.id, e.id, d.id FROM users u, events e LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND o.id = e.id) d ON TRUE WHERE u.id < 3 AND e.id < 4 ORDER BY u.id, e.id",
    "SELECT u.id, d.id, e.name FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d JOIN events e ON e.id = d.id ORDER BY u.id, d.id",
    "SELECT u.id, d.id, e.name FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d LEFT JOIN events e ON e.id = d.id AND e.active = 1 ORDER BY u.id, d.id",
    "SELECT u.id, d.id, e.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d, events e WHERE e.id = d.id + 1 ORDER BY u.id, d.id",
    "SELECT a.id, b.id, d.c FROM users a JOIN users b ON b.id = a.id + 1, LATERAL (SELECT COUNT(*) AS c FROM orders o WHERE o.user_id IN (a.id, b.id)) d ORDER BY a.id",
    "SELECT u.id, d.id FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.total > 100 LEFT JOIN LATERAL (SELECT e.id FROM events e WHERE e.id = o.id) d ON TRUE ORDER BY u.id, d.id",
    // Chained and nested tables.
    "SELECT u.id, d.b, e.c FROM users u, LATERAL (SELECT u.id + 1 AS b) d, LATERAL (SELECT d.b * 2 AS c) e ORDER BY u.id",
    "SELECT u.id, d.id, e.name FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d, LATERAL (SELECT x.name FROM events x WHERE x.id = d.id AND x.score > u.id * 10) e ORDER BY u.id, d.id",
    "SELECT u.id, d.id, e.c FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1) d ON TRUE LEFT JOIN LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.total > d.total) e ON TRUE ORDER BY u.id",
    "SELECT u.id, d.id, d.c FROM users u, LATERAL (SELECT o.id, i.c FROM orders o, LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.user_id = o.user_id AND p.id <= o.id) i WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.c FROM users u, LATERAL (SELECT o.id, i.c FROM orders o, LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.user_id = u.id AND p.id <= o.id) i WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT i.id FROM (SELECT o.id FROM orders o WHERE o.user_id = u.id) i) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (WITH c AS (SELECT o.id FROM orders o WHERE o.user_id = u.id) SELECT c.id FROM c) d ORDER BY u.id, d.id",
    // Joins, subqueries and windows inside the table.
    "SELECT u.id, d.id, d.name FROM users u, LATERAL (SELECT o.id, v.name FROM orders o JOIN users v ON v.id = o.user_id WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND o.total > (SELECT AVG(p.total) FROM orders p WHERE p.user_id = u.id)) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND EXISTS (SELECT 1 FROM events e WHERE e.id = o.id AND e.score > u.id * 5)) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.rn FROM users u, LATERAL (SELECT o.id, ROW_NUMBER() OVER (ORDER BY o.total DESC, o.id) AS rn FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.rn",
    "SELECT u.id, d.id, d.running FROM users u, LATERAL (SELECT o.id, SUM(o.total) OVER (ORDER BY o.id) AS running FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.tag FROM users u, LATERAL (SELECT o.id, CASE WHEN o.total > 50 THEN 'big' ELSE 'small' END AS tag FROM orders o WHERE o.user_id = u.id AND o.status IN ('shipped', 'pending')) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, d.score FROM users u, LATERAL (SELECT o.id, JSON_EXTRACT(o.meta, '$.score') AS score FROM orders o WHERE o.user_id = u.id AND o.meta IS NOT NULL) d ORDER BY u.id, d.id",
    "SELECT u.id, d.day FROM users u, LATERAL (SELECT DATE(o.placed_at) AS day FROM orders o WHERE o.user_id = u.id AND o.placed_at >= '2024-06-01') d ORDER BY u.id, d.day",
    // Filters, grouping, ordering and limits outside.
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d WHERE u.id < 4 AND d.total > 10 ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d WHERE u.name IN ('user-01', 'user-03') ORDER BY u.id, d.id",
    "SELECT u.id, d.m FROM users u, LATERAL (SELECT MAX(o.total) AS m FROM orders o WHERE o.user_id = u.id) d WHERE d.m IS NOT NULL ORDER BY u.id",
    "SELECT d.status, COUNT(*), SUM(d.total), MIN(u.id) FROM users u, LATERAL (SELECT o.status, o.total FROM orders o WHERE o.user_id = u.id) d GROUP BY d.status ORDER BY d.status",
    "SELECT u.id, COUNT(d.id), COALESCE(SUM(d.total), 0) FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id AND o.status <> 'cancelled') d ON TRUE GROUP BY u.id ORDER BY u.id",
    "SELECT u.id % 2, COUNT(*), MAX(d.total) FROM users u, LATERAL (SELECT o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.total DESC, o.id LIMIT 1) d GROUP BY u.id % 2 ORDER BY 1",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ORDER BY d.id DESC LIMIT 4",
    "SELECT u.id, d.id FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON TRUE ORDER BY u.id DESC, d.id LIMIT 3 OFFSET 2",
    "SELECT DISTINCT d.status FROM users u, LATERAL (SELECT o.status FROM orders o WHERE o.user_id = u.id) d ORDER BY d.status",
    "SELECT COUNT(*), COUNT(d.id), SUM(d.total) FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON TRUE",
    // Column lists, USING and NATURAL.
    "SELECT u.id, d.p, d.q FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d (p, q) ORDER BY u.id, d.p",
    "SELECT * FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d USING (id) ORDER BY id",
    "SELECT * FROM users u NATURAL JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY id",
    // Inside a CTE, a derived table and subqueries of every kind.
    "WITH top AS (SELECT u.id AS user_id, d.id AS order_id, d.total FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.total DESC, o.id LIMIT 1) d) SELECT * FROM top ORDER BY user_id",
    "WITH top AS (SELECT u.id AS user_id, d.total FROM users u, LATERAL (SELECT o.total FROM orders o WHERE o.user_id = u.id ORDER BY o.total DESC, o.id LIMIT 1) d) SELECT u.name, top.total FROM users u LEFT JOIN top ON top.user_id = u.id ORDER BY u.id",
    "SELECT x.user_id, x.c FROM (SELECT u.id AS user_id, d.c FROM users u, LATERAL (SELECT COUNT(*) AS c FROM orders o WHERE o.user_id = u.id) d) x WHERE x.c > 1 ORDER BY x.user_id",
    "SELECT COUNT(*) FROM (SELECT d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d) x",
    "SELECT e.id FROM events e WHERE e.id IN (SELECT d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1) d) ORDER BY e.id",
    "SELECT e.id, (SELECT COUNT(*) FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d) AS c FROM events e WHERE e.id < 3 ORDER BY e.id",
    "SELECT u.id, (SELECT MAX(d.total) FROM LATERAL (SELECT o.total FROM orders o WHERE o.user_id = u.id) d) AS m FROM users u ORDER BY u.id",
    "SELECT u.id, (SELECT SUM(d.total) FROM orders o, LATERAL (SELECT p.total FROM orders p WHERE p.id = o.id AND p.user_id = u.id) d) AS s FROM users u ORDER BY u.id",
    "SELECT u.id, (SELECT COUNT(*) FROM events e, LATERAL (SELECT o.id FROM orders o WHERE o.id = e.id AND o.user_id = u.id) d WHERE e.active = 1) AS c FROM users u ORDER BY u.id",
    "SELECT u.id FROM users u WHERE EXISTS (SELECT 1 FROM events e, LATERAL (SELECT o.id FROM orders o WHERE o.id = e.id AND o.user_id = u.id) d WHERE e.score > 20) ORDER BY u.id",
    "SELECT u.id FROM users u WHERE u.id IN (SELECT d.user_id FROM events e, LATERAL (SELECT o.user_id FROM orders o WHERE o.id = e.id AND o.total > u.id) d) ORDER BY u.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d UNION ALL SELECT u.id, d.id FROM users u, LATERAL (SELECT e.id FROM events e WHERE e.id = u.id AND e.active = 1) d ORDER BY 1, 2",
    // Unsigned, decimal, temporal and text values crossing the boundary.
    "SELECT o.id, d.id FROM orders o, LATERAL (SELECT e.id FROM events e WHERE e.id = o.id AND e.score < o.total) d ORDER BY o.id",
    "SELECT o.id, d.c FROM orders o, LATERAL (SELECT COUNT(*) AS c FROM orders p WHERE p.placed_at < o.placed_at AND p.status = o.status) d ORDER BY o.id",
    "SELECT o.id, d.total FROM orders o, LATERAL (SELECT p.total FROM orders p WHERE p.total > o.total ORDER BY p.total LIMIT 1) d ORDER BY o.id",
    "SELECT o.id, d.gap FROM orders o, LATERAL (SELECT DATEDIFF(o.placed_at, MAX(p.placed_at)) AS gap FROM orders p WHERE p.placed_at < o.placed_at) d ORDER BY o.id",
    "SELECT e.id, d.id FROM events e, LATERAL (SELECT x.id FROM events x WHERE x.name > e.name ORDER BY x.name LIMIT 1) d ORDER BY e.id",
    "SELECT e.id, d.c FROM events e, LATERAL (SELECT COUNT(*) AS c FROM users u WHERE u.name < REPLACE(e.name, 'event', 'user')) d ORDER BY e.id",
    "SELECT o.id, d.tags FROM orders o, LATERAL (SELECT JSON_LENGTH(o.meta, '$.tags') AS tags, JSON_EXTRACT(o.meta, '$.items[0]') AS first_item) d WHERE d.tags > 0 ORDER BY o.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.n < b.n) d ORDER BY b.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.u >= b.u) d ORDER BY b.id",
    "SELECT b.id, d.c, d.s FROM bounds b, LATERAL (SELECT COUNT(*) AS c, SUM(x.frac) AS s FROM bounds x WHERE x.frac <= b.frac) d ORDER BY b.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.approx > b.approx) d ORDER BY b.id",
    "SELECT b.id, d.id FROM bounds b LEFT JOIN LATERAL (SELECT x.id FROM bounds x WHERE x.day > b.day ORDER BY x.day LIMIT 1) d ON TRUE ORDER BY b.id",
    "SELECT b.id, d.id FROM bounds b LEFT JOIN LATERAL (SELECT x.id FROM bounds x WHERE x.dt6 < b.dt6 ORDER BY x.dt6 DESC LIMIT 1) d ON TRUE ORDER BY b.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.clock <= b.clock) d ORDER BY b.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.txt = b.txt) d ORDER BY b.id",
    "SELECT b.id, d.c FROM bounds b, LATERAL (SELECT COUNT(*) AS c FROM bounds x WHERE x.raw = b.raw) d ORDER BY b.id",
    "SELECT b.id, d.whole, d.dt3, d.clock FROM bounds b, LATERAL (SELECT b.whole, b.dt3, b.clock) d ORDER BY b.id",
    // The left side is itself derived, grouped or a CTE.
    "SELECT x.user_id, d.id FROM (SELECT o.user_id, MAX(o.total) AS top FROM orders o GROUP BY o.user_id) x, LATERAL (SELECT p.id FROM orders p WHERE p.user_id = x.user_id AND p.total = x.top) d ORDER BY x.user_id, d.id",
    "SELECT x.id, d.c FROM (SELECT u.id FROM users u WHERE u.id > 5) x LEFT JOIN LATERAL (SELECT COUNT(*) AS c FROM orders o WHERE o.user_id = x.id) d ON TRUE ORDER BY x.id",
    "WITH big AS (SELECT o.id, o.user_id FROM orders o WHERE o.total > 60) SELECT big.id, d.name FROM big, LATERAL (SELECT u.name FROM users u WHERE u.id = big.user_id) d ORDER BY big.id",
    "SELECT x.v, d.c FROM (SELECT 1 AS v UNION ALL SELECT 3 UNION ALL SELECT 9) x, LATERAL (SELECT COUNT(*) AS c FROM orders o WHERE o.user_id = x.v) d ORDER BY x.v",
    // Only the lateral table's columns are read above, or none at all.
    "SELECT d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ORDER BY d.id",
    "SELECT COUNT(*) FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d",
    "SELECT COUNT(*) FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON TRUE",
    "SELECT u.name FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id AND o.status = 'pending') d ORDER BY u.name, d.id",
    // Subqueries beside it, which lower to joins on the same FROM clause.
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d WHERE EXISTS (SELECT 1 FROM events e WHERE e.id = d.id AND e.active = 1) ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.id = d.id) ORDER BY u.id, d.id",
    "SELECT u.id, d.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d WHERE d.id IN (SELECT e.id FROM events e WHERE e.score > 30) ORDER BY u.id, d.id",
    "SELECT u.id, d.id, (SELECT e.name FROM events e WHERE e.id = d.id) AS name FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, (SELECT COUNT(*) FROM orders p WHERE p.user_id = u.id AND p.id > d.id) AS later FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, d.id",
    "SELECT u.id, d.id, ROW_NUMBER() OVER (PARTITION BY u.id ORDER BY d.total DESC, d.id) AS rn FROM users u, LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ORDER BY u.id, rn",
];

pub fn cases() -> Vec<OracleCase> {
    SQL.iter()
        .map(|sql| OracleCase {
            sql_mode: "",
            family: FAMILY,
            sql: (*sql).to_owned(),
            ordered: true,
        })
        .collect()
}

/// Shapes with no evaluation here, which must say so and name the clause,
/// and the references `MySQL` itself refuses.
pub fn rejects() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        (
            "reject lateral inside a parenthesized join",
            "SELECT u.id FROM users u JOIN (events e JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON TRUE) ON TRUE",
            "lateral",
        ),
        (
            "reject lateral before a right join in a longer chain",
            "SELECT u.id FROM users u LEFT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON TRUE RIGHT JOIN events e ON e.id = u.id",
            "lateral",
        ),
        (
            "reject subquery in a lateral join condition",
            "SELECT u.id FROM users u LEFT JOIN LATERAL (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id) d ON d.total > (SELECT AVG(total) FROM orders p WHERE p.user_id = u.id)",
            "lateral",
        ),
        (
            "reject derived table reading its left without lateral",
            "SELECT u.id FROM users u, (SELECT o.id FROM orders o WHERE o.user_id = u.id) d",
            "unknown column|u.id",
        ),
        (
            "reject lateral reading a table to its right",
            "SELECT u.id FROM LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d, users u",
            "unknown column|u.id",
        ),
        (
            "reject lateral on the right of a right join reading its left",
            "SELECT u.id FROM users u RIGHT JOIN LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id) d ON TRUE",
            "unknown column|u.id",
        ),
        (
            "reject lateral without an alias",
            "SELECT u.id FROM users u, LATERAL (SELECT o.id FROM orders o WHERE o.user_id = u.id)",
            "",
        ),
    ]
}

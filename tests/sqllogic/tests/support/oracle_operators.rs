//! Operators, constructs and zero-argument functions, each across the
//! fixture's types, NULLs, three-valued logic, mixed-type coercion and the
//! boundary rows. One family per group of ledger rows, so a failure names
//! the operator it belongs to.
//!
//! Clock and identifier functions have no fixed answer; their cases compare
//! one reading with another inside the statement, or check a shape (length,
//! pattern, distinctness), so both engines print the same thing at any time.

use super::OracleCase;

const COMPARISON: &[&str] = &[
    "SELECT id, n = u, n < u, n <= u, n > u, n >= u, n <> u, n != u, n <=> u FROM bounds ORDER BY id",
    "SELECT id, n = approx, n < approx, n >= approx, u > approx, u <=> approx, frac = approx, frac < approx, frac <> approx FROM bounds ORDER BY id",
    "SELECT id, whole = n, whole > u, whole <= frac, whole <=> NULL, frac >= -0.005, frac < 0.005, frac = '0.005', frac = 5e-3 FROM bounds ORDER BY id",
    "SELECT id, txt = 'A', txt < 'b', txt <=> 'null', txt >= 'a', txt <> 'A ', txt = 'é', txt = 'e', txt > '' FROM bounds ORDER BY id",
    "SELECT id, fixed = 'A', fixed > 'NULL', fixed <= 'a ', fixed <=> 'É', fixed <> 'e', fixed != 'x', fixed < 'b' FROM bounds ORDER BY id",
    "SELECT id, raw = X'ff', raw < X'61', raw >= X'00', raw <=> X'', raw <> 'NULL', raw > 'a', raw <= X'fe' FROM bounds ORDER BY id",
    "SELECT id, day = '2024-02-29', day < dt0, day <= dt3, dt3 > dt0, dt6 >= dt3, dt6 <> stamp, dt6 <=> stamp, day = dt0, day > 20240101, dt0 < 20250101000000 FROM bounds ORDER BY id",
    "SELECT id, clock = '00:00:00', clock < '00:00:00', clock >= '-00:00:00.000001', clock <=> NULL, clock > 0, clock <> '838:59:59' FROM bounds ORDER BY id",
    "SELECT '1' = 1, '1a' = 1, 'a' = 0, '' = 0, ' 1' = 1, '1e0' = 1, '0x10' = 16, '1' = '1.0', 1 = 1.0, 1 < '2', '10' < '9', 10 < '9', '10' < 9, NULL = NULL, NULL <=> NULL, NULL <> 1, 1 <=> NULL, 1 >= NULL, 'a' <= 'A', 'a' != 'A ', 0.1 + 0.2 = 0.3, 1e0 = 1, 0.1e0 + 0.2e0 > 0.3e0",
    "SELECT id, status = 'shipped', status > 'pending', status < 'shipped', status = 3, status >= 3, status <> 'SHIPPED', status <=> 'delivered', status != 1, status <= 'processing' FROM orders ORDER BY id",
    "SELECT (1, 2) = (1, 2), (1, 2) < (1, 3), (1, NULL) = (1, NULL), (1, NULL) <=> (1, NULL), (2, NULL) > (1, NULL), (1, 2) <> (1, 2), (1, 2) >= (1, 2), (1, 2) <= (0, 9), (1, NULL) <> (2, NULL), (1, NULL) < (2, 0)",
    "SELECT id FROM bounds WHERE n > u ORDER BY id",
    "SELECT id FROM bounds WHERE n <=> u OR u <=> NULL ORDER BY id",
    "SELECT id FROM bounds WHERE whole >= n AND frac <= approx ORDER BY id",
    "SELECT id FROM orders WHERE total >= 50 AND total < 100.00 AND user_id != 3 ORDER BY id",
    "SELECT id FROM events WHERE note = 'ALPHA' ORDER BY id",
    "SELECT id FROM events WHERE tag > 'blue' AND label <= 'b' ORDER BY id",
    "SELECT id, meta->'$.score' = 1, meta->'$.score' > 1.5, meta->'$.score' <= 3, meta->'$.tags[0]' = 'premium', meta->>'$.tags[0]' = 'premium', meta->'$.items' <> JSON_ARRAY(1), meta->'$.score' >= 2, meta->'$.score' != 0 FROM orders ORDER BY id",
    "SELECT e.id, e.id = u.id, e.score > u.id * 10, e.name < u.name, e.name <> u.name, e.note <=> u.name FROM events e LEFT JOIN users u ON u.id = e.id + 2 ORDER BY e.id",
    "SELECT id, placed_at = '2024-02-29 12:34:56', placed_at > '2024-12-31', placed_at <= '2024-03-01', placed_at <> placed_at, placed_at >= 20250101, placed_at < 20240229123457 FROM orders ORDER BY id",
];

const LOGICAL: &[&str] = &[
    "SELECT a.v, b.v, a.v AND b.v, a.v OR b.v, a.v XOR b.v, NOT a.v, a.v && b.v, ! a.v FROM (SELECT 1 AS v UNION ALL SELECT 0 UNION ALL SELECT NULL) a CROSS JOIN (SELECT 1 AS v UNION ALL SELECT 0 UNION ALL SELECT NULL) b ORDER BY a.v, b.v",
    "SELECT 2 AND 3, 0.1 AND 1, 0.0 OR 0, '1' AND 1, 'a' OR 0, '' XOR 1, NOT 5, NOT 0.0, NOT '0', NOT 'a', NOT NULL, -1 AND 1, '0.4' AND 1, 1e-10 AND 1, 0e0 OR 0, 2 XOR 3, -1 XOR 0, NOT -1, NOT 0.1",
    "SELECT id, n AND u, n OR u, n XOR u, NOT n, NOT u, approx AND 1, frac OR 0, NOT frac, NOT approx, whole XOR 1, NOT whole FROM bounds ORDER BY id",
    "SELECT id, txt OR 0, NOT txt, fixed AND 1, day AND 1, NOT day, dt0 OR 0, dt6 XOR 1 FROM bounds ORDER BY id",
    "SELECT 1 OR 0 AND 0, (1 OR 0) AND 0, NOT 1 = 2, NOT (1 = 2), ! 1 = 2, 1 XOR 1 XOR 1, 1 OR NULL AND 0, NOT NULL IS NULL, 1 XOR NULL, 0 AND NULL, 1 AND NULL, 0 OR NULL, 1 OR NULL, NOT 1 + 1, ! 1 + 1, 1 XOR 0 OR 1, 1 OR 1 XOR 1, 0 AND 1 XOR 1, NOT 0 AND 0, NOT NOT 2, ! ! 2",
    "SELECT id FROM events WHERE active AND note IS NULL OR score > 80 ORDER BY id",
    "SELECT id FROM events WHERE active XOR (note IS NULL) ORDER BY id",
    "SELECT id FROM events WHERE NOT active AND NOT (score < 50 OR note = 'beta') ORDER BY id",
    "SELECT id FROM bounds WHERE NOT (n > 0) ORDER BY id",
    "SELECT id FROM bounds WHERE NOT (n > 0 AND u > 0) OR (n XOR u) ORDER BY id",
    "SELECT id FROM bounds WHERE n AND NOT u ORDER BY id",
    "SELECT id, active AND score > 50, active OR note IS NULL, active XOR score > 50, NOT active, NOT (active OR score > 50), ! active FROM events ORDER BY id",
    "SELECT u.id, o.id, o.total > 50 AND u.id < 3, o.total > 50 OR u.id < 3, o.total > 50 XOR u.id < 3, NOT o.total > 50 FROM users u LEFT JOIN orders o ON o.user_id = u.id AND NOT o.status = 'shipped' ORDER BY u.id, o.id",
    "SELECT active, SUM(score > 30 AND note IS NOT NULL), SUM(score > 30 OR note IS NULL), SUM(NOT score > 30), SUM(score > 30 XOR active) FROM events GROUP BY active HAVING NOT SUM(score) < 0 AND (COUNT(*) > 100 OR MAX(score) XOR 0) ORDER BY active",
];

const RANGE_AND_MEMBERSHIP: &[&str] = &[
    "SELECT id, n BETWEEN u AND 9223372036854775807, u BETWEEN n AND 100, n NOT BETWEEN -1 AND 1, u NOT BETWEEN 1 AND 18446744073709551615, frac BETWEEN -0.005 AND 0.005, approx BETWEEN -0.5 AND 1.5, approx NOT BETWEEN frac AND whole, n BETWEEN NULL AND 0, n BETWEEN 0 AND NULL, n NOT BETWEEN NULL AND 0, n BETWEEN 1 AND -1 FROM bounds ORDER BY id",
    "SELECT id, day BETWEEN '2024-02-29' AND '2025-01-01', dt3 BETWEEN day AND dt6, dt0 NOT BETWEEN '2024-03-31' AND '2024-11-03 00:00:00', clock BETWEEN '-00:00:01' AND '00:00:01', stamp BETWEEN dt0 AND dt3, day BETWEEN 20240229 AND 20250101, clock NOT BETWEEN '-100:00:00' AND '100:00:00' FROM bounds ORDER BY id",
    "SELECT id, name BETWEEN 'event-03' AND 'EVENT-07', note BETWEEN 'a' AND 'b', note NOT BETWEEN 'alpha' AND 'alpha', tag BETWEEN 'blue' AND 'green', score BETWEEN '20' AND '70', score NOT BETWEEN 20.5 AND 70.5, label BETWEEN 'a' AND 'b' FROM events ORDER BY id",
    "SELECT id, txt BETWEEN '' AND 'a', txt NOT BETWEEN 'a' AND 'e', fixed BETWEEN 'A' AND 'a ', raw BETWEEN X'00' AND X'61', raw NOT BETWEEN X'61' AND X'ff', whole BETWEEN -1 AND 3, whole NOT BETWEEN n AND u FROM bounds ORDER BY id",
    "SELECT 1 BETWEEN 1 AND 1, 1 BETWEEN 2 AND 0, 1 BETWEEN '0' AND '2', '1' BETWEEN 0 AND 2, 'b' BETWEEN 'A' AND 'C', 2 BETWEEN 1 AND '3x', 1.5 BETWEEN 1 AND 2, NULL BETWEEN 1 AND 2, 1 BETWEEN NULL AND 2, 3 BETWEEN NULL AND 2, 1 NOT BETWEEN NULL AND 0, 1 NOT BETWEEN 2 AND NULL, 0 NOT BETWEEN 2 AND NULL, 1 BETWEEN 0 AND 2 AND 0, 1 NOT BETWEEN 0 AND 2 OR 1, '2024-06-01' BETWEEN '2024-01-01' AND '2024-12-31', 10 BETWEEN '9' AND '11', '10' BETWEEN '9' AND '11'",
    "SELECT id FROM orders WHERE placed_at BETWEEN '2024-03-01' AND '2025-01-01' AND total NOT BETWEEN 10 AND 100 ORDER BY id",
    "SELECT id FROM orders WHERE status BETWEEN 'processing' AND 'delivered' ORDER BY id",
    "SELECT id FROM orders WHERE status NOT BETWEEN 2 AND 4 ORDER BY id",
    "SELECT id FROM bounds WHERE n BETWEEN -9223372036854775808 AND -1 OR u BETWEEN 9223372036854775808 AND 18446744073709551615 ORDER BY id",
    "SELECT id, n IN (u, 0, -1), n NOT IN (u, 0, -1), u IN (n, NULL), u NOT IN (n, NULL), u IN (10, 100.0, '1'), frac IN (0, 1.5, '2.500'), approx IN (0.1, -0.5, 2.5e0), approx NOT IN (0, NULL), n IN (NULL), NULL IN (n), n NOT IN (9223372036854775807, -9223372036854775808) FROM bounds ORDER BY id",
    "SELECT id, txt IN ('A', 'null', 'E'), txt NOT IN ('a', ''), fixed IN ('A ', 'X'), raw IN (X'ff', 'NULL'), raw NOT IN (X'00', X''), day IN ('2024-02-29', '2025-01-01'), day NOT IN (dt0), dt3 IN (dt0, dt6), clock IN ('00:00:00', '100:00:00'), whole IN (0, 1, 2, 3) FROM bounds ORDER BY id",
    "SELECT 1 IN ('1', 2), '1' IN (1, 2), 'a' IN (0), 1 IN (1.0), 1.5 IN ('1.5'), NULL IN (1, NULL), 2 IN (1, NULL), 1 IN (1, NULL), 2 NOT IN (1, NULL), 1 NOT IN (1, NULL), (1, 2) IN ((1, 2), (3, 4)), (1, NULL) IN ((1, 2)), (1, NULL) IN ((2, 2)), (1, 2) NOT IN ((1, NULL)), (1, 2) NOT IN ((2, NULL)), 'abc' IN ('ABC'), 'a' IN ('a '), 1 IN (1), 1 NOT IN (2), 0 IN ('a', 'b'), 3 IN (1, 2, 1 + 2)",
    "SELECT id FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 50) ORDER BY id",
    "SELECT id FROM users WHERE id NOT IN (SELECT user_id FROM orders) ORDER BY id",
    "SELECT id FROM bounds WHERE n NOT IN (SELECT u FROM bounds) ORDER BY id",
    "SELECT id FROM bounds WHERE n IN (SELECT u FROM bounds) ORDER BY id",
    "SELECT id FROM bounds WHERE n NOT IN (SELECT u FROM bounds WHERE u IS NOT NULL) ORDER BY id",
    "SELECT id, id IN (SELECT user_id FROM orders), id NOT IN (SELECT user_id FROM orders WHERE total < 1), (id, name) IN (SELECT id, name FROM users WHERE id > 4) FROM users ORDER BY id",
    "SELECT id FROM orders WHERE status IN ('shipped', 'PENDING') ORDER BY id",
    "SELECT id FROM orders WHERE status NOT IN (1, 3) ORDER BY id",
    "SELECT id FROM orders WHERE (user_id, status) IN ((1, 'shipped'), (2, 'pending'), (3, 'nothing')) ORDER BY id",
    "SELECT id FROM orders WHERE placed_at IN ('2024-03-01', '2025-01-01 00:00:00', '2024-01-15 10:00:00.000') OR total IN (7, '50', 100.0) ORDER BY id",
    "SELECT id FROM events WHERE note IN ('ALPHA', NULL) OR tag NOT IN ('red', 'blue') ORDER BY id",
];

const NULL_TESTS: &[&str] = &[
    "SELECT id, n IS NULL, n IS NOT NULL, u IS NULL, whole IS NOT NULL, frac IS NULL, approx IS NOT NULL, txt IS NULL, raw IS NOT NULL, day IS NULL, dt6 IS NOT NULL, clock IS NULL, stamp IS NOT NULL FROM bounds ORDER BY id",
    "SELECT id, n IS TRUE, n IS FALSE, n IS UNKNOWN, n IS NOT TRUE, n IS NOT FALSE, n IS NOT UNKNOWN, approx IS TRUE, frac IS FALSE, frac IS NOT TRUE, (n > 0) IS UNKNOWN, (n > u) IS NOT FALSE, whole IS TRUE FROM bounds ORDER BY id",
    "SELECT id, txt IS TRUE, txt IS NOT FALSE, fixed IS FALSE, day IS TRUE, day IS NOT FALSE, dt6 IS FALSE FROM bounds ORDER BY id",
    "SELECT NULL IS NULL, 0 IS NULL, '' IS NULL, (1 = NULL) IS NULL, (NULL AND 0) IS NULL, (NULL OR 1) IS NOT NULL, NULL IS NOT NULL, 0.5 IS TRUE, 0.0 IS FALSE, 'a' IS TRUE, '1' IS TRUE, NULL IS TRUE, NULL IS NOT TRUE, NULL IS UNKNOWN, 2 IS TRUE, -1 IS NOT FALSE, 1 IS NOT UNKNOWN, NULL IS FALSE, NULL IS NOT FALSE, NOT NULL IS UNKNOWN, 1 = 1 IS TRUE, (1 IS TRUE) = 1",
    "SELECT u.id, o.id IS NULL, o.total IS NOT NULL, o.status IS NULL, (o.total > 100) IS NOT TRUE, (o.total > 100) IS UNKNOWN FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.total > 60 ORDER BY u.id, o.id",
    "SELECT u.id FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id IS NULL ORDER BY u.id",
    "SELECT id FROM orders WHERE meta IS NULL ORDER BY id",
    "SELECT id, (meta->'$.missing') IS NULL, (meta->'$.tags') IS NOT NULL, JSON_EXTRACT(meta, '$.tags[0]') IS NULL, (meta->'$.score') IS TRUE, (meta->>'$.score') IS NOT NULL FROM orders ORDER BY id",
    "SELECT COUNT(*), SUM(note IS NULL), SUM(note IS NOT NULL), SUM(active IS TRUE), SUM(active IS NOT TRUE), SUM((note = 'beta') IS UNKNOWN), SUM((note = 'beta') IS NOT FALSE) FROM events",
    "SELECT id FROM bounds WHERE (n > 0) IS NOT TRUE ORDER BY id",
    "SELECT id FROM bounds WHERE (n < u) IS UNKNOWN OR approx IS FALSE ORDER BY id",
    "SELECT id FROM events WHERE note IS NOT NULL AND active IS FALSE ORDER BY id",
    "SELECT note IS NULL AS missing, COUNT(*), MIN(score), MAX(score) FROM events GROUP BY note IS NULL ORDER BY missing",
    "SELECT id, NULLIF(score, 50) IS NULL, COALESCE(note, name) IS NOT NULL, IF(active, NULL, 1) IS NULL, (CASE WHEN active THEN note END) IS NOT NULL FROM events ORDER BY id",
];

const EXISTS: &[&str] = &[
    "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY id",
    "SELECT id FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) ORDER BY id",
    "SELECT id, EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 100), NOT EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id AND o.status = 'pending') FROM users u ORDER BY id",
    "SELECT EXISTS (SELECT 1 FROM events WHERE 1 = 0), EXISTS (SELECT NULL), NOT EXISTS (SELECT NULL FROM events WHERE note IS NULL), EXISTS (SELECT 1 FROM bounds WHERE n IS NULL), EXISTS (SELECT COUNT(*) FROM events WHERE 1 = 0), NOT EXISTS (SELECT MAX(id) FROM events WHERE 1 = 0), EXISTS (SELECT 1 FROM events GROUP BY active HAVING COUNT(*) > 5), NOT EXISTS (SELECT 1 FROM events GROUP BY active HAVING COUNT(*) > 4)",
    "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id GROUP BY o.user_id HAVING SUM(o.total) > 100) ORDER BY id",
    "SELECT id FROM orders o WHERE NOT EXISTS (SELECT 1 FROM users u WHERE u.id = o.user_id) OR EXISTS (SELECT 1 FROM orders p WHERE p.user_id = o.user_id AND p.id > o.id) ORDER BY id",
    "SELECT id FROM events e WHERE EXISTS (SELECT 1 FROM events f WHERE f.note = e.note AND f.id <> e.id) ORDER BY id",
    "SELECT id FROM events e WHERE NOT EXISTS (SELECT 1 FROM events f WHERE f.note = e.note AND f.id <> e.id) ORDER BY id",
    "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id LIMIT 0) ORDER BY id",
    "SELECT id FROM users u WHERE EXISTS (SELECT o.id FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1 OFFSET 1) ORDER BY id",
    "SELECT id FROM bounds b WHERE EXISTS (SELECT 1 FROM bounds c WHERE c.n = b.u) OR NOT EXISTS (SELECT 1 FROM bounds c WHERE c.u <=> b.n) ORDER BY id",
    "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND EXISTS (SELECT 1 FROM orders p WHERE p.user_id = o.user_id AND p.status <> o.status)) ORDER BY id",
    "SELECT id FROM users u WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND NOT EXISTS (SELECT 1 FROM orders p WHERE p.user_id = o.user_id AND p.total > o.total)) ORDER BY id",
    "SELECT active, COUNT(*) FROM events e WHERE EXISTS (SELECT 1 FROM users u WHERE u.id = e.id) GROUP BY active HAVING EXISTS (SELECT 1 FROM orders WHERE total > 900) ORDER BY active",
    "SELECT id, CASE WHEN EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.meta IS NULL) THEN 'bare' WHEN NOT EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id) THEN 'none' ELSE 'full' END FROM users u ORDER BY id",
];

const ARITHMETIC: &[&str] = &[
    "SELECT id, score + 1, score - 100, score * 3, score / 3, score % 7, score DIV 7, -score, score + 0.5, score - 1.5e0, score * 0.1, score / 0, score % 0, score DIV 0 FROM events ORDER BY id",
    "SELECT id, total + 1, total - 0.005, total * total, total / 3, total % 7, total DIV 7, -total, total * 1.5, total / 0.3, total + user_id, total * id, total / id, total % 0.03, total DIV 0.03 FROM orders ORDER BY id",
    "SELECT id, frac + 0.0005, frac - whole, frac * frac, frac / 7, frac % 1, frac DIV 1, -frac, whole + 1, whole - 1, -whole, whole / 3, whole % 7, whole DIV 100000000000000000000 FROM bounds ORDER BY id",
    "SELECT id, approx + 1, approx - 0.1, approx * approx, approx / 3, approx % 1, approx DIV 1, -approx, approx / 0, approx % 0, approx + frac, approx * n FROM bounds ORDER BY id",
    "SELECT id, n DIV 2, n % 1000, n % -7, n / 2, n * 0, n + 0, n - 0, u DIV 3, u % 1000, u / 2, u * 1, u + 0 FROM bounds ORDER BY id",
    "SELECT id, CAST(n AS DECIMAL(30,0)) + 1, CAST(n AS DECIMAL(30,0)) - 1, CAST(n AS DECIMAL(30,0)) * 2, -CAST(n AS DECIMAL(30,0)), CAST(u AS DECIMAL(30,0)) + 1, CAST(u AS DECIMAL(30,0)) * 2, CAST(u AS DECIMAL(30,0)) - 1 FROM bounds ORDER BY id",
    "SELECT 1 + '1', '1' + '1', '1.5' + 1, 'a' + 1, '1a' * 2, 1 + NULL, NULL * 0, 7 / 2, 7 DIV 2, -7 DIV 2, 7 % -2, -7 % 2, 7.5 % 2, -7.5 DIV 2, 1 / 3, 1.0 / 3, 1e0 / 3, 2 * 3.50, 5 - 10, 0.1 + 0.2, 1 - -1, - - 1, -(-(1)), +1, 9223372036854775807 + 0, 18446744073709551615 - 1, 9223372036854775807 * 1.0, 1 / 0, 1 % 0, 1 DIV 0, 0 / 1, -0.0 + 0",
    "SELECT 1 + 2 * 3, (1 + 2) * 3, 10 - 2 - 3, 10 - (2 - 3), 2 * 3 % 4, 2 + 3 % 4, 20 / 4 / 2, 20 DIV 4 DIV 2, 20 DIV 3 * 3, 7 % 4 * 2, -2 * -3, 2 - -3 * 2, 10 / 4 * 2, 10 % 4 / 2, 1 + 2 DIV 2, - 5 % 3, -(5 % 3), 2 * -(3 + 1), 100 / 3 DIV 1, 5 % 3 + 1",
    "SELECT id, day + 0, day + 1, dt0 + 0, dt3 + 0, clock + 0, day - day, dt0 - 1, clock * 2, day / 2, day DIV 100, day % 100 FROM bounds ORDER BY id",
    "SELECT id, active + active, active * score, -active, score * -1, -(score - 50), - score % 7, -score DIV 7, score / (CAST(id AS SIGNED) - 5), score % (CAST(id AS SIGNED) - 5), score DIV (CAST(id AS SIGNED) - 5) FROM events ORDER BY id",
    "SELECT id, status + 0, status * 2, status - 1, status / 2, status % 2, status DIV 2, -status FROM orders ORDER BY id",
    "SELECT id, -n FROM bounds WHERE id > 1 ORDER BY id",
    "SELECT id, -u FROM bounds WHERE id IN (1, 2, 7, 8) ORDER BY id",
    "SELECT id, -approx, -frac, -whole, -(approx + frac), -(whole - 1) FROM bounds ORDER BY id",
    "SELECT id, -txt, -fixed, txt + 0, fixed * 1, txt - 1, fixed / 2 FROM bounds ORDER BY id",
    "SELECT user_id, SUM(total * 2) - SUM(total), AVG(total) / 2, MAX(total) % 10, COUNT(*) * 1.5, MIN(total) DIV 1, -SUM(total), SUM(total) / COUNT(*) FROM orders GROUP BY user_id ORDER BY user_id",
    "SELECT id, meta->'$.score' + 1, meta->'$.score' * 2, meta->'$.score' - 0.5, meta->'$.score' / 2, meta->'$.items[0]' % 2, meta->'$.items[0]' DIV 2, -(meta->'$.score') FROM orders ORDER BY id",
    "SELECT id FROM orders WHERE total * 2 > 100 AND total - 50 < 100 AND total / 2 <> 25 AND total % 1 = 0 AND total DIV 10 >= 5 AND -total < -60 ORDER BY id",
    "SELECT id, score + id, score - id, score * id, score / id, score % id, score DIV id, CAST(id AS SIGNED) - score, id - 1, id + 0, id * 1 FROM events ORDER BY id",
    "SELECT id, score MOD 7, score MOD -7, -score MOD 7, score MOD 0, score MOD 2.5, score MOD '7', score MOD NULL FROM events ORDER BY id",
    "SELECT id, n MOD 1000, n MOD -7, u MOD 1000, frac MOD 1, approx MOD 1, whole MOD 7 FROM bounds ORDER BY id",
    "SELECT 7 MOD 2, -7 MOD 2, 7 MOD -2, 7.5 MOD 2, 7 MOD 0, NULL MOD 2, 7 MOD NULL, '7' MOD '2', 5 MOD 3 + 1, 2 * 5 MOD 3, 10 MOD 4 MOD 3, 7 MOD 2 = MOD(7, 2), 7 MOD 2 = 7 % 2, 1e1 MOD 3, 18446744073709551615 MOD 10, -9223372036854775808 MOD 10",
    "SELECT id, total MOD 7, total MOD 0.03, total MOD user_id, status MOD 2, user_id MOD id FROM orders ORDER BY id",
    "SELECT id FROM events WHERE score MOD 20 = 0 AND id MOD 2 = 0 ORDER BY id",
    "SELECT score MOD 3 AS r, COUNT(*), SUM(score) FROM events GROUP BY r HAVING SUM(score) MOD 2 = 0 ORDER BY r",
    "SELECT id, day MOD 100, clock MOD 100, txt MOD 2 FROM bounds ORDER BY id",
];

const BITWISE: &[&str] = &[
    "SELECT id, score & 12, score | 3, score ^ 255, ~score, score << 2, score >> 1, score & ~15, score | score << 8, score ^ score FROM events ORDER BY id",
    "SELECT id, n & 255, n | 1, n ^ -1, ~n, n << 1, n >> 1, n >> 63, n << 63, n << 64, n >> 64, n & u, n | u, n ^ u, ~u, u << 1, u >> 60 FROM bounds ORDER BY id",
    "SELECT 5 & 3, 5 | 3, 5 ^ 3, ~5, 1 << 3, 256 >> 4, -1 & 255, -1 >> 1, -1 << 1, 1 << 64, 1 << -1, 1 >> -1, NULL & 1, 1 | NULL, ~NULL, NULL << 1, 1 >> NULL, 2.5 & 3, 2.4 | 0, 3.5 ^ 0, '5' & '3', 'a' | 1, ~0, ~18446744073709551615, 1 << 63, 18446744073709551615 >> 63, -1 ^ 0, 9223372036854775807 | 1, -9223372036854775808 & -1",
    "SELECT HEX(0x0F & 0xF0), HEX(0x0F | 0xF0), HEX(b'1010' ^ b'0110'), HEX(~0x0F), HEX(0x0F0F << 4), HEX(0xF0F0 >> 4), HEX(X'FF00' & X'0FF0'), HEX(X'FF00' | X'0FF0'), HEX(X'FF00' ^ X'0FF0'), 0x0F & 15, 0xF0 | 1, 0x10 << 1",
    "SELECT id, frac & 3, frac | 0, frac ^ 1, ~frac, frac << 1, frac >> 1 FROM bounds ORDER BY id",
    "SELECT id, approx & 3, approx | 0, approx ^ 1, ~approx, approx << 1, approx >> 1 FROM bounds ORDER BY id",
    "SELECT id, whole & 255, whole | 0, whole ^ 0, ~whole, whole >> 1 FROM bounds WHERE id > 2 ORDER BY id",
    "SELECT id, HEX(raw & raw), HEX(raw | raw), HEX(raw ^ raw), HEX(~raw), HEX(raw << 1), HEX(raw >> 1) FROM bounds ORDER BY id",
    "SELECT id, txt | 0, txt & 1, fixed ^ 0, ~txt, fixed << 1 FROM bounds ORDER BY id",
    "SELECT id, day & 255, dt0 | 0, clock & 255, day << 1, day >> 4, ~day FROM bounds ORDER BY id",
    "SELECT id FROM events WHERE score & 16 ORDER BY id",
    "SELECT id FROM events WHERE (score >> 2) % 2 = 1 OR score | 1 = score OR id << 2 > 36 AND ~id & 1 ORDER BY id",
    "SELECT id FROM events WHERE score ^ 255 > 200 AND NOT score & 2 ORDER BY id",
    "SELECT 1 | 2 & 3, 1 ^ 2 | 3, 2 << 1 + 1, 1 + 2 << 1, 6 & 3 = 2, 1 | 0 = 1, ~1 + 1, ~-1, 2 ^ 3 * 2, 8 >> 1 << 1, 1 << 2 >> 1, 5 & 3 | 2 ^ 1, !1 | 1, - 2 ^ 2, 2 * 3 ^ 1, 7 & 3 << 1, 1 | 2 << 1, 12 / 4 & 3, 4 & 6 % 4",
    "SELECT id, status & 1, status | 8, status ^ 1, ~status & 7, status << 1, status >> 1, user_id & id, user_id | id, user_id ^ id, total & 7, total | 0, total >> 1, total << 1 FROM orders ORDER BY id",
    "SELECT active, BIT_AND(score), BIT_OR(score), BIT_XOR(score), BIT_OR(score & 15), BIT_XOR(score | 1) ^ 1, BIT_AND(~score) >> 60, SUM(score << 1), MAX(score >> 1) FROM events GROUP BY active ORDER BY active",
    "SELECT id, meta->'$.items[0]' & 1, meta->'$.items[0]' | 8, meta->'$.items[0]' ^ 1, meta->'$.items[0]' << 2, meta->'$.items[0]' >> 1 FROM orders WHERE meta IS NOT NULL ORDER BY id",
];

const CASE_CONSTRUCT: &[&str] = &[
    "SELECT id, CASE status WHEN 'shipped' THEN 1 WHEN 'pending' THEN 2 ELSE 0 END, CASE status WHEN 3 THEN 'three' END, CASE WHEN total > 100 THEN 'big' WHEN total > 10 THEN total END, CASE user_id WHEN 1 THEN total WHEN 2 THEN placed_at ELSE status END FROM orders ORDER BY id",
    "SELECT id, CASE n WHEN u THEN 'eq' WHEN 0 THEN 'zero' WHEN NULL THEN 'null' ELSE 'other' END, CASE WHEN n IS NULL THEN 'null' WHEN n < 0 THEN 'neg' ELSE 'pos' END, CASE WHEN n > 0 THEN n ELSE u END, CASE WHEN id < 4 THEN frac ELSE approx END, CASE WHEN id < 4 THEN n ELSE frac END, CASE WHEN id < 4 THEN whole ELSE u END FROM bounds ORDER BY id",
    "SELECT id, CASE id WHEN 1 THEN day WHEN 2 THEN dt6 WHEN 3 THEN clock END, CASE WHEN id < 3 THEN day ELSE dt0 END, CASE WHEN id < 3 THEN dt0 ELSE dt6 END, CASE WHEN id < 3 THEN day ELSE txt END FROM bounds ORDER BY id",
    "SELECT id, CASE WHEN id < 3 THEN raw ELSE txt END FROM bounds ORDER BY id",
    "SELECT CASE WHEN NULL THEN 1 ELSE 2 END, CASE NULL WHEN NULL THEN 1 ELSE 2 END, CASE 1 WHEN '1' THEN 'a' END, CASE 'a' WHEN 'A' THEN 1 ELSE 0 END, CASE WHEN 0 THEN 1 END, CASE 2 WHEN 1 THEN 'x' WHEN 2.0 THEN 'y' END, CASE WHEN 1 THEN 1 ELSE 'a' END, CASE WHEN 0 THEN 1 ELSE 2.50 END, CASE WHEN 1 THEN 1 ELSE 2.50 END, CASE WHEN 1 THEN 1 ELSE 2e0 END, CASE WHEN 1 THEN NULL ELSE 1 END, CASE 1.0 WHEN 1 THEN 'dec' END, CASE WHEN 'a' THEN 1 ELSE 0 END, CASE WHEN 0.1 THEN 1 ELSE 0 END, CASE WHEN 1 THEN NULL END, CASE 'x' WHEN 'x ' THEN 1 ELSE 0 END, CASE WHEN 1 THEN 18446744073709551615 ELSE -1 END",
    "SELECT CASE WHEN active THEN 'on' ELSE 'off' END AS state, SUM(CASE WHEN note IS NULL THEN 1 ELSE 0 END), COUNT(CASE WHEN score > 50 THEN 1 END), MAX(CASE note WHEN 'Alpha' THEN score END), AVG(CASE WHEN score > 30 THEN score END) FROM events GROUP BY state ORDER BY state",
    "SELECT id, CASE WHEN score < 30 THEN CASE WHEN active THEN 'a' ELSE 'b' END WHEN score < 70 THEN CASE note WHEN 'beta' THEN 'c' ELSE COALESCE(note, 'd') END ELSE 'e' END FROM events ORDER BY CASE WHEN active THEN 0 ELSE 1 END, CASE WHEN note IS NULL THEN score ELSE -score END, id",
    "SELECT id FROM orders WHERE CASE status WHEN 'cancelled' THEN total = 0 WHEN 'shipped' THEN total > 10 ELSE NULL END ORDER BY id",
    "SELECT id, CASE WHEN meta IS NULL THEN 'none' WHEN JSON_LENGTH(meta->'$.items') > 2 THEN 'many' ELSE 'few' END, CASE meta->>'$.tags[0]' WHEN 'premium' THEN 1 WHEN 'PREMIUM' THEN 2 ELSE 0 END, CASE WHEN id > 5 THEN meta->'$.score' ELSE total END FROM orders ORDER BY id",
    "SELECT id, CASE tag WHEN 'RED' THEN 1 WHEN 'blue' THEN 2 END, CASE label WHEN 'STRASSE' THEN 1 WHEN 'A' THEN 2 END, CASE note WHEN 'ALPHA' THEN 'a' WHEN NULL THEN 'n' END, CASE WHEN note = 'beta' THEN tag ELSE label END FROM events ORDER BY id",
    "SELECT CASE WHEN total < 10 THEN 'small' WHEN total < 100 THEN 'medium' ELSE 'large' END AS size, COUNT(*), SUM(total), MIN(CASE status WHEN 'shipped' THEN placed_at END) FROM orders GROUP BY size HAVING CASE WHEN COUNT(*) > 2 THEN 1 ELSE SUM(total) > 1000 END ORDER BY CASE size WHEN 'small' THEN 1 WHEN 'medium' THEN 2 ELSE 3 END",
    "SELECT u.id, CASE WHEN o.id IS NULL THEN 'no order' WHEN o.total > 100 THEN CONCAT(u.name, ':big') ELSE o.status END, CASE o.status WHEN 'shipped' THEN o.total ELSE 0 END + 1 FROM users u LEFT JOIN orders o ON o.user_id = u.id ORDER BY u.id, o.id",
    "SELECT id, CASE WHEN n > 0 THEN 1 WHEN n > 0 THEN 2 END, CASE n WHEN n THEN 'self' ELSE 'null' END, CASE WHEN n < 0 THEN n END, CASE WHEN n < 0 THEN NULL ELSE approx END, CASE u WHEN 1 THEN 'one' WHEN 1.0 THEN 'one again' WHEN '10' THEN 'ten' END FROM bounds ORDER BY id",
];

const PATTERN_MATCH: &[&str] = &[
    "SELECT id, name LIKE 'event-0%', name LIKE '%1_', name NOT LIKE '%0', note LIKE 'a%', note NOT LIKE 'A%', note LIKE NULL, tag LIKE 'red', tag LIKE 'RED%', tag LIKE 'red ', label LIKE 'stra%e', label LIKE 'a', label LIKE 'a_', label NOT LIKE '%r%' FROM events ORDER BY id",
    r"SELECT 'abc' LIKE 'ABC', 'abc' LIKE 'a_c', 'abc' LIKE 'a%', 'abc' LIKE '', '' LIKE '', '' LIKE '%', 'a%c' LIKE 'a\%c', 'abc' LIKE 'a\%c', 'a_c' LIKE 'a\_c', 'abc' LIKE 'a\_c', 'a\\c' LIKE 'a\\\\c', 'a%c' LIKE 'a|%c' ESCAPE '|', 'abc' LIKE 'a|%c' ESCAPE '|', 'abc' NOT LIKE 'abd', NULL LIKE 'a', 'a' LIKE NULL, NULL NOT LIKE NULL, 'abc' LIKE '%%%', 'abc' LIKE '___', 'abc' LIKE '____', 'ab' LIKE '_%_', 'é' LIKE 'e', 'É' LIKE 'é', 'ß' LIKE 'ss', 'a ' LIKE 'a', 'a' LIKE 'a ', 'abc' NOT LIKE '%b%', 10 LIKE '1%', 1.50 LIKE '1.5', 'ABC' LIKE BINARY 'abc'",
    "SELECT id, total LIKE '1%', total NOT LIKE '%.00', placed_at LIKE '2024-%', placed_at NOT LIKE '%00:00', status LIKE 'ship%', status NOT LIKE '%ed', user_id LIKE 1, id LIKE '1_' FROM orders ORDER BY id",
    "SELECT id, n LIKE '-%', u LIKE '%5', frac LIKE '%.__5', approx LIKE '%.5', txt LIKE 'a%', txt LIKE 'A _', fixed LIKE 'a', fixed LIKE 'a ', txt LIKE '%\\t%', raw LIKE 'a%', raw LIKE 'N%', raw NOT LIKE '%l%', day LIKE '2024%', clock LIKE '-%' FROM bounds ORDER BY id",
    "SELECT id FROM events WHERE name LIKE CONCAT('%-0', id) ORDER BY id",
    "SELECT e.id, f.id FROM events e JOIN events f ON e.note LIKE f.note AND e.id < f.id ORDER BY e.id, f.id",
    "SELECT id FROM events WHERE note NOT LIKE 'alpha' ORDER BY id",
    "SELECT COUNT(*), SUM(label LIKE '%s%'), SUM(label NOT LIKE '%s%'), SUM(tag LIKE '% '), SUM(note LIKE '%') FROM events",
    "SELECT id FROM orders WHERE (meta->>'$.tags[0]') LIKE 'p%' AND status NOT LIKE 'deliver%' ORDER BY id",
    "SELECT id FROM events WHERE name REGEXP CONCAT('0', id, '$') ORDER BY id",
    "SELECT 'new\\nline' REGEXP 'new.line', 'new\\nline' REGEXP '^line', 'tab\\there' REGEXP '[[:space:]]'",
    "SELECT id FROM events WHERE note NOT REGEXP '^[A-Z]' OR label REGEXP '^[^a-z]+$' ORDER BY id",
    "SELECT active, SUM(name REGEXP '[13579]$'), SUM(note NOT REGEXP '^b'), COUNT(*) FROM events GROUP BY active HAVING SUM(tag REGEXP 'e') > 0 ORDER BY active",
];

const JSON_PATH: &[&str] = &[
    "SELECT id, meta->'$.tags', meta->'$.tags[0]', meta->>'$.tags[0]', meta->'$.score', meta->>'$.score', meta->'$.items[1]', meta->>'$.items[last]', meta->'$.missing', meta->>'$.missing', meta->>'$.tags' FROM orders ORDER BY id",
    "SELECT id FROM orders WHERE meta->>'$.tags[0]' = 'premium' ORDER BY id",
    "SELECT id FROM orders WHERE meta->'$.score' > 2 ORDER BY meta->'$.score', id",
    "SELECT meta->>'$.tags[0]' AS t, COUNT(*), SUM(meta->'$.score') FROM orders GROUP BY t ORDER BY t",
    "SELECT id, JSON_TYPE(meta->'$.score'), JSON_LENGTH(meta->'$.items'), CAST(meta->'$.score' AS DECIMAL(5,2)), meta->>'$.score' + 1, meta->'$.items[0]' * 2, CHAR_LENGTH(meta->>'$.tags[0]'), CHAR_LENGTH(meta->'$.tags[0]'), UPPER(meta->>'$.tags[0]') FROM orders ORDER BY id",
    "SELECT o.id, u.name FROM orders o JOIN users u ON o.meta->'$.items[0]' = u.id ORDER BY o.id, u.id",
    "SELECT id, meta->'$.items[last - 1]', meta->'$.*', meta->'$.tags[*]', meta->>'$.items[*]', meta->'$**.score' FROM orders ORDER BY id",
    "SELECT id, meta->>'$.score' = 1.5, meta->>'$.tags[0]' = 'PREMIUM', meta->'$.tags[0]' = 'PREMIUM', meta->'$.items' = JSON_ARRAY(1, 2), meta->'$.tags' = CAST('[\"premium\"]' AS JSON), meta->>'$.tags' = '[\"premium\"]' FROM orders ORDER BY id",
    "SELECT id, 1 MEMBER OF(meta->'$.items'), 'premium' MEMBER OF(meta->'$.tags'), id MEMBER OF(meta->'$.items'), user_id MEMBER OF(meta->'$.items'), NULL MEMBER OF(meta->'$.items'), 9.5e0 MEMBER OF(meta->'$.score'), 'PREMIUM' MEMBER OF(meta->'$.tags'), total MEMBER OF(meta->'$.items') FROM orders ORDER BY id",
    "SELECT 1 MEMBER OF('[1,2]'), '1' MEMBER OF('[1,2]'), 1 MEMBER OF('[\"1\"]'), 'a' MEMBER OF('[\"A\"]'), 1 MEMBER OF('1'), 1 MEMBER OF('{\"a\":1}'), NULL MEMBER OF('[null]'), 1 MEMBER OF(NULL), 1e0 MEMBER OF('[1]'), 2 MEMBER OF('[[2]]'), CAST('[2]' AS JSON) MEMBER OF('[[2]]'), CAST('null' AS JSON) MEMBER OF('[null]'), TRUE MEMBER OF('[true]'), 1 MEMBER OF('[true]'), CAST('true' AS JSON) MEMBER OF('[true]'), 'x' MEMBER OF('[]'), 1.5e0 MEMBER OF('[1.5]'), 'a' MEMBER OF('\"a\"'), CAST('{\"a\":1}' AS JSON) MEMBER OF('[{\"a\":1}]')",
    "SELECT id FROM orders WHERE 'premium' MEMBER OF(meta->'$.tags') AND NOT 5 MEMBER OF(meta->'$.items') ORDER BY id",
    "SELECT u.id, COUNT(o.id), SUM(u.id MEMBER OF(o.meta->'$.items')) FROM users u LEFT JOIN orders o ON o.user_id = u.id GROUP BY u.id ORDER BY u.id",
];

const VOLATILE: &[&str] = &[
    "SELECT CURRENT_DATE() = CURDATE(), CURRENT_TIMESTAMP() = NOW(), LOCALTIME() = NOW(), LOCALTIMESTAMP() = NOW(), UTC_TIMESTAMP() = NOW(), UTC_DATE() = CURDATE(), UTC_TIME() = CURTIME(), SYSDATE() >= NOW(), DATE(CURRENT_TIMESTAMP()) = CURRENT_DATE(), DATE(UTC_TIMESTAMP()) = UTC_DATE(), TIME(UTC_TIMESTAMP()) = UTC_TIME(), DATE(LOCALTIME()) = CURRENT_DATE(), DATE(LOCALTIMESTAMP()) = UTC_DATE()",
    "SELECT LENGTH(CURRENT_DATE()), LENGTH(CURRENT_TIMESTAMP()), LENGTH(LOCALTIME()), LENGTH(LOCALTIMESTAMP()), LENGTH(UTC_DATE()), LENGTH(UTC_TIME()), LENGTH(UTC_TIMESTAMP()), LENGTH(SYSDATE())",
    "SELECT CURRENT_DATE() REGEXP '^[0-9]{4}-[0-9]{2}-[0-9]{2}$', UTC_TIME() REGEXP '^[0-9]{2}:[0-9]{2}:[0-9]{2}$', SYSDATE() REGEXP '^[0-9]{4}-[0-9]{2}-[0-9]{2} [0-9]{2}:[0-9]{2}:[0-9]{2}$'",
    "SELECT UTC_TIMESTAMP() REGEXP '^[0-9]{4}(-[0-9]{2}){2} [0-9]{2}(:[0-9]{2}){2}$', LOCALTIME() REGEXP '^[0-9 :-]{19}$', UTC_DATE() REGEXP '^[0-9-]{10}$'",
    "SELECT CURRENT_DATE() > '2024-01-01', CURRENT_DATE() + 0 > 20240101, UTC_DATE() + 0 = CURDATE() + 0, CURRENT_TIMESTAMP() + 0 > 20240101000000, UTC_TIME() + 0 = CURTIME() + 0, YEAR(CURRENT_DATE()) = YEAR(NOW()), YEAR(UTC_DATE()) >= 2024, DATEDIFF(CURRENT_DATE(), UTC_DATE()), TIMESTAMPDIFF(SECOND, UTC_TIMESTAMP(), LOCALTIMESTAMP()), TIMESTAMPDIFF(SECOND, CURRENT_TIMESTAMP(), LOCALTIME()), TIMESTAMPDIFF(HOUR, NOW(), SYSDATE()), CURRENT_DATE() IS NOT NULL, SYSDATE() IS NULL, UTC_TIMESTAMP() + 0 = LOCALTIMESTAMP() + 0, LOCALTIME() + 0 = CURRENT_TIMESTAMP() + 0",
    "SELECT id, placed_at < CURRENT_TIMESTAMP(), placed_at < LOCALTIME(), placed_at < LOCALTIMESTAMP(), placed_at < UTC_TIMESTAMP(), placed_at < SYSDATE(), DATE(placed_at) < CURRENT_DATE(), DATE(placed_at) < UTC_DATE(), DATEDIFF(CURRENT_DATE(), placed_at) > 300, TIME(placed_at) < UTC_TIME() OR TIME(placed_at) >= UTC_TIME() FROM orders ORDER BY id",
    "SELECT COUNT(*) FROM orders WHERE placed_at BETWEEN '2024-01-01' AND CURRENT_TIMESTAMP() AND placed_at <= UTC_TIMESTAMP() AND DATE(placed_at) <> UTC_DATE() AND placed_at < SYSDATE() AND placed_at < LOCALTIME() AND placed_at < LOCALTIMESTAMP() AND DATE(placed_at) < CURRENT_DATE()",
    "SELECT id, day < CURRENT_DATE(), day < UTC_DATE(), dt6 < UTC_TIMESTAMP(), dt3 < LOCALTIME(), dt0 < LOCALTIMESTAMP(), stamp < CURRENT_TIMESTAMP(), dt6 < SYSDATE(), DATEDIFF(UTC_DATE(), day) > 0 FROM bounds ORDER BY id",
    "SELECT LENGTH(UUID()), CHAR_LENGTH(UUID()), IS_UUID(UUID()), UUID() REGEXP '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$', UUID() = UUID(), UUID() IS NULL, LENGTH(UUID_TO_BIN(UUID())), LENGTH(REPLACE(UUID(), '-', ''))",
    "SELECT SUBSTRING(UUID(), 9, 1), SUBSTRING(UUID(), 14, 1), SUBSTRING(UUID(), 19, 1), SUBSTRING(UUID(), 24, 1)",
    "SELECT COUNT(*), SUM(UUID_SHORT() > 0), MIN(LENGTH(UUID())), MAX(LENGTH(UUID())), SUM(IS_UUID(UUID())) FROM events",
    "SELECT UUID_SHORT() > 0, UUID_SHORT() <> UUID_SHORT(), UUID_SHORT() IS NOT NULL, UUID_SHORT() < 18446744073709551615, UUID_SHORT() = NULL",
    "SELECT id, UUID_SHORT() > id, LENGTH(UUID()) + id, UUID() <> name FROM events ORDER BY id",
    "SELECT id FROM events WHERE UUID() <> name AND UUID_SHORT() > 0 AND IS_UUID(UUID()) ORDER BY id",
];

const ALIASES: &[&str] = &[
    "SELECT INSERT('Quadratic', 3, 4, 'What'), INSERT('Quadratic', -1, 4, 'What'), INSERT('Quadratic', 3, 100, 'What'), INSERT('Quadratic', 0, 1, 'X'), INSERT('Quadratic', 10, 1, 'X'), INSERT('Quadratic', 9, 0, 'X'), INSERT('Quadratic', 9, -1, 'X'), INSERT('Quadratic', 1, 0, ''), INSERT('', 1, 0, 'X'), INSERT(NULL, 1, 1, 'X'), INSERT('abc', NULL, 1, 'X'), INSERT('abc', 1, NULL, 'X'), INSERT('abc', 1, 1, NULL), INSERT('héllo', 2, 1, 'e'), INSERT('日本語', 2, 1, '_'), INSERT(12345, 2, 2, 0), INSERT('abc', 2, 1, 123), INSERT('abc', '2', '1', 'X'), INSERT('abc', 4, 1, 'X'), INSERT('abc', 3, 5, 'XYZ')",
    "SELECT id, INSERT(name, 7, 2, note), INSERT(name, id, 1, '#'), INSERT(tag, 2, id, UPPER(tag)), INSERT(label, 1, 1, ''), HEX(INSERT(label, 2, 0, 'ß')), CHAR_LENGTH(INSERT(name, 1, 0, label)) FROM events ORDER BY id",
    "SELECT id, INSERT(txt, 1, 1, 'Z'), INSERT(fixed, 2, 1, txt), HEX(INSERT(raw, 1, 1, X'ff')), INSERT(n, 1, 1, '+'), INSERT(day, 1, 4, 'YYYY'), INSERT(frac, 2, 0, '_') FROM bounds ORDER BY id",
    "SELECT id FROM events WHERE INSERT(name, 1, 6, '') = CONCAT('0', id) OR INSERT(note, 1, 1, 'B') = 'beta' ORDER BY id",
    "SELECT SHA('abc'), SHA(''), SHA(NULL), SHA(123), SHA(1.50), SHA('héllo'), SHA(X'00ff'), SHA(SHA('abc')), SHA('abc') = SHA1('abc'), LENGTH(SHA('abc')), SHA(CONCAT('a', 'bc')), UPPER(SHA('a')), SHA(TRUE), SHA(1e0), SHA(-0.0)",
    "SELECT id, SHA(name), SHA(note), SHA(score), SHA(label), SHA(active) FROM events ORDER BY id",
    "SELECT id, SHA(n), SHA(u), SHA(frac), SHA(txt), SHA(raw), SHA(day), SHA(dt6), SHA(clock), SHA(whole) FROM bounds ORDER BY id",
    "SELECT id, SHA(total), SHA(placed_at), SHA(status), SHA(meta->'$.tags'), SHA(meta->>'$.tags[0]') FROM orders ORDER BY id",
    "SELECT id FROM events WHERE SHA(name) < SHA(note) ORDER BY SHA(name)",
    "SELECT id, ADDDATE(day, 1), ADDDATE(day, INTERVAL 1 MONTH), SUBDATE(day, 1), SUBDATE(day, INTERVAL 1 YEAR), ADDDATE(dt6, INTERVAL '1.5' SECOND_MICROSECOND), SUBDATE(dt3, INTERVAL 1 MICROSECOND), ADDDATE(dt0, -1), SUBDATE(dt0, -1), ADDDATE(day, NULL), SUBDATE(stamp, INTERVAL 1 DAY) FROM bounds ORDER BY id",
    "SELECT id, ADDDATE(clock, INTERVAL 1 HOUR), SUBDATE(clock, INTERVAL 1 MICROSECOND), ADDDATE(clock, 1), SUBDATE(clock, 1) FROM bounds ORDER BY id",
    "SELECT ADDDATE('2024-01-31', INTERVAL 1 MONTH), ADDDATE('2024-02-29', INTERVAL 1 YEAR), SUBDATE('2024-03-31', INTERVAL 1 MONTH), ADDDATE('2024-12-31 23:59:59', INTERVAL 1 SECOND), SUBDATE('2024-01-01', 1), ADDDATE('2024-01-01', 365), ADDDATE('2024-01-01', '1'), ADDDATE('2024-01-01', 1.5), ADDDATE('9999-12-31', 1), SUBDATE('0001-01-01', 1), ADDDATE('2024-02-30', 1), ADDDATE('not a date', 1), ADDDATE(20240101, 1), ADDDATE('2024-01-01', INTERVAL '1 2' DAY_HOUR), SUBDATE('2024-01-01 00:00:00', INTERVAL '1:30' HOUR_MINUTE), ADDDATE('2024-01-01', INTERVAL -1 WEEK), ADDDATE('2024-01-01', INTERVAL 1 QUARTER), ADDDATE(ADDDATE('2024-01-31', INTERVAL 1 MONTH), INTERVAL -1 MONTH), SUBDATE(NULL, 1), ADDDATE('2024-01-01', NULL), SUBDATE('2024-03-01', INTERVAL 1 DAY), SUBDATE('2023-03-01', 1)",
    "SELECT id, ADDDATE(placed_at, user_id), SUBDATE(placed_at, INTERVAL id MONTH), DATEDIFF(ADDDATE(placed_at, 30), SUBDATE(placed_at, 30)), ADDDATE(placed_at, INTERVAL total SECOND), SUBDATE(DATE(placed_at), id) FROM orders ORDER BY id",
    "SELECT id FROM orders WHERE ADDDATE(placed_at, 365) > '2025-06-01' AND SUBDATE(placed_at, INTERVAL 1 MONTH) < '2025-01-01' ORDER BY id",
    "SELECT MID('abcdef', 2, 3), MID('abcdef', -3, 2), MID('abcdef', 0, 2), MID('abcdef', 7, 1), MID('abcdef', 2, 0), MID('abcdef', 2, -1), MID(NULL, 1, 1), MID('abc', NULL, 1), MID('abc', 1, NULL), MID('héllo', 2, 3), MID('日本語', 2, 1), MID(12345, 2, 2), MID('abcdef', '2', '3'), MID('abcdef', 6, 10), MID('abcdef', -6, 6), MID('abcdef', -7, 2)",
    "SELECT MID('abcdef', 2), MID('abcdef', -2), MID('abcdef', 7), MID(NULL, 2), MID('日本語', 2)",
    "SELECT POSITION('b' IN 'abc'), POSITION('B' IN 'abc'), POSITION('' IN 'abc'), POSITION('d' IN 'abc'), POSITION(NULL IN 'abc'), POSITION('a' IN NULL), POSITION('é' IN 'cafe'), POSITION('語' IN '日本語'), POSITION(2 IN 123), POSITION('bc' IN 'abcabc'), POSITION('ss' IN 'straße'), POSITION('abc' IN ''), POSITION('' IN ''), POSITION('c' IN CONCAT('ab', 'cd')), POSITION(X'62' IN X'616263')",
    "SELECT id, POSITION('-' IN name), POSITION(note IN 'alphabet'), POSITION('e' IN label), POSITION('RED' IN tag), MID(name, 7, 2), MID(name, id, 2), MID(label, 2, 3), MID(note, -2, 2) FROM events ORDER BY id",
    "SELECT id, POSITION('a' IN txt), POSITION(X'62' IN raw), MID(txt, 2, 1), HEX(MID(raw, 2, 2)), MID(n, 1, 3), MID(day, 6, 2), POSITION('-' IN day), POSITION('.' IN frac), MID(clock, 1, 4) FROM bounds ORDER BY id",
    "SELECT id FROM events WHERE POSITION('0' IN name) = 7 AND MID(name, 8, 1) > '4' ORDER BY id",
    "SELECT INTERVAL(23, 1, 15, 17, 30, 44, 200), INTERVAL(10, 1, 10, 100, 1000), INTERVAL(22, 23, 30, 44, 200), INTERVAL(NULL, 1, 2), INTERVAL(5, NULL, 3, 10), INTERVAL(1.5, 1, 2), INTERVAL('5', 1, 10), INTERVAL(5, 1), INTERVAL(0, 1), INTERVAL(5, 5), INTERVAL(-1, -5, -3, 0), INTERVAL(5, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10), INTERVAL(2.5, 1.5, 2.5, 3.5), INTERVAL(1e2, 99.99, 100, 100.01), INTERVAL(10, 30, 20, 5), INTERVAL(5, 1, NULL, 10), INTERVAL(18446744073709551615, 1, 9223372036854775807)",
    "SELECT id, INTERVAL(score, 25, 50, 75), INTERVAL(score, id * 10, 1000), INTERVAL(id, 3, 6, 9), INTERVAL(score, 100), INTERVAL(active, 1) FROM events ORDER BY id",
    "SELECT id, INTERVAL(n, -1, 0, 1), INTERVAL(u, 1, 10, 100), INTERVAL(frac, -1, 0, 1), INTERVAL(approx, -1, 0, 1), INTERVAL(n, u), INTERVAL(0, n, u), INTERVAL(whole, 0, 2) FROM bounds ORDER BY id",
    "SELECT INTERVAL(total, 10, 100, 1000) AS bucket, COUNT(*), MIN(total), MAX(total) FROM orders GROUP BY bucket ORDER BY bucket",
];

const ROLLUP: &[&str] = &[
    "SELECT active, note, COUNT(*), SUM(score), GROUPING(active) AS ga, GROUPING(note) AS gn, GROUPING(active, note) FROM events GROUP BY active, note WITH ROLLUP ORDER BY ga, active, gn, note",
    "SELECT IF(GROUPING(status), 'all', status) AS s, COUNT(*), SUM(total), AVG(total), MIN(placed_at), MAX(total), GROUPING(status) AS g FROM orders GROUP BY status WITH ROLLUP ORDER BY g, status",
    "SELECT user_id, status, COUNT(*) AS c, SUM(total), GROUPING(user_id) AS g FROM orders GROUP BY user_id, status WITH ROLLUP HAVING GROUPING(status) = 1 ORDER BY g, user_id",
    "SELECT user_id, status, COUNT(*), GROUPING(user_id) AS g FROM orders GROUP BY user_id, status WITH ROLLUP HAVING user_id IS NULL OR status IS NULL ORDER BY g, user_id",
    "SELECT YEAR(placed_at) AS y, MONTH(placed_at) AS m, status, COUNT(*), SUM(total), GROUPING(YEAR(placed_at), MONTH(placed_at), status) AS g FROM orders GROUP BY YEAR(placed_at), MONTH(placed_at), status WITH ROLLUP ORDER BY g, y, m, status",
    "SELECT note, GROUPING(note) AS g, COUNT(*), COUNT(note) FROM events GROUP BY note WITH ROLLUP ORDER BY g, note",
    "SELECT n IS NULL AS missing, COUNT(*), GROUPING(n IS NULL) FROM bounds GROUP BY n IS NULL WITH ROLLUP ORDER BY 3, 1",
    "SELECT active, COUNT(*), GROUPING(active) FROM events WHERE id > 100 GROUP BY active WITH ROLLUP",
    "SELECT active, COUNT(DISTINCT note), SUM(DISTINCT score), COUNT(DISTINCT tag), GROUPING(active) AS g FROM events GROUP BY active WITH ROLLUP ORDER BY g, active",
    "SELECT * FROM (SELECT status, COUNT(*) AS c FROM orders GROUP BY status WITH ROLLUP) t WHERE status IS NULL",
    "WITH r AS (SELECT user_id, SUM(total) AS s FROM orders GROUP BY user_id WITH ROLLUP) SELECT COUNT(*), SUM(s), MAX(s) FROM r",
    "SELECT u.name, o.status, SUM(o.total), GROUPING(u.name) AS gn, GROUPING(o.status) AS gs FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.name, o.status WITH ROLLUP ORDER BY gn, u.name, gs, o.status",
    "SELECT status, COUNT(*), GROUPING(status) AS g FROM orders GROUP BY status WITH ROLLUP ORDER BY g DESC, status LIMIT 3",
    "SELECT CASE WHEN GROUPING(active) THEN 'total' WHEN active THEN 'on' ELSE 'off' END, COUNT(*), MIN(score), MAX(score), GROUPING(active) AS g FROM events GROUP BY active WITH ROLLUP ORDER BY g, active",
    "SELECT user_id, status, GROUPING(user_id) + GROUPING(status) AS depth, SUM(total) FROM orders WHERE total < 1000 GROUP BY user_id, status WITH ROLLUP HAVING SUM(total) > 50 ORDER BY depth, user_id, status",
    "SELECT day IS NULL, YEAR(day), COUNT(*), GROUPING(day IS NULL) AS g1, GROUPING(YEAR(day)) AS g2 FROM bounds GROUP BY day IS NULL, YEAR(day) WITH ROLLUP ORDER BY g1, 1, g2, 2",
    "SELECT active, COUNT(*), SUM(score), AVG(score), MIN(name), MAX(name), GROUP_CONCAT(id ORDER BY id), GROUPING(active) AS g FROM events GROUP BY active WITH ROLLUP ORDER BY g, active",
    "SELECT status, GROUPING(status) + 1, IF(GROUPING(status), 'all', status), CASE WHEN GROUPING(status) THEN 1 ELSE 0 END, COUNT(*) FROM orders GROUP BY status WITH ROLLUP ORDER BY 2, 1",
];

const TABLE_STATEMENT: &[&str] = &[
    "SELECT * FROM (TABLE users) t ORDER BY id",
    "TABLE users ORDER BY id DESC LIMIT 3",
    "TABLE users ORDER BY name LIMIT 2 OFFSET 1",
    "TABLE users LIMIT 0",
    "TABLE users UNION ALL TABLE users ORDER BY 1, 2",
    "TABLE users INTERSECT TABLE users ORDER BY id",
    "TABLE events ORDER BY score DESC, id LIMIT 4",
    "SELECT * FROM (TABLE users) t WHERE id > 5 ORDER BY id",
    "SELECT COUNT(*) FROM (TABLE orders) o JOIN (TABLE users) u ON u.id = o.user_id",
    "SELECT (SELECT COUNT(*) FROM (TABLE events) e)",
    "SELECT id FROM users WHERE id IN (SELECT id FROM (TABLE events) e) ORDER BY id",
    "WITH t AS (TABLE users) SELECT COUNT(*), MAX(name) FROM t",
];

const VALUES_CONSTRUCTOR: &[&str] = &[
    "VALUES ROW(1, 'a'), ROW(2, 'b') ORDER BY 1",
    "VALUES ROW(1, 2), ROW(3, 4) ORDER BY column_1 DESC LIMIT 1",
    "VALUES ROW(1 + 1, CONCAT('a', 'b'), NULL, 1.50, 2e0)",
    "SELECT * FROM (VALUES ROW(1, 'a'), ROW(2, NULL)) AS v (n, s) ORDER BY n",
    "SELECT column_0, column_1 FROM (VALUES ROW(1, 2.5), ROW(NULL, 3)) AS v ORDER BY column_1",
    "SELECT column_0 FROM (VALUES ROW(1), ROW('a'), ROW(2.5)) AS v ORDER BY column_0",
    "VALUES ROW(1, 2) UNION ALL SELECT 3, 4 ORDER BY 1",
    "SELECT 0, 0 UNION VALUES ROW(1, 2), ROW(1, 2) ORDER BY 1",
    "SELECT id FROM users WHERE id IN (VALUES ROW(1), ROW(3), ROW(NULL)) ORDER BY id",
    "SELECT u.id, v.label FROM users u JOIN (VALUES ROW(1, 'one'), ROW(2, 'two')) AS v (id, label) ON v.id = u.id ORDER BY u.id",
    "SELECT u.id, v.label FROM users u LEFT JOIN (VALUES ROW(1, 'one'), ROW(9, 'nine')) AS v (id, label) ON v.id = u.id ORDER BY u.id",
    "SELECT SUM(n), COUNT(*), MAX(s) FROM (VALUES ROW(1, 'x'), ROW(2, 'y'), ROW(NULL, NULL)) AS v (n, s)",
    "WITH v (n) AS (VALUES ROW(3), ROW(1), ROW(2)) SELECT n, n * 2 FROM v ORDER BY n DESC",
];

/// Regular expressions, three to a statement: every compiled pattern
/// reserves memory before the statement starts, and the corpus runs under a
/// small budget.
const REGEX: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "'abc' REGEXP 'ABC'",
            "'abc' REGEXP '^a.c$'",
            "'abc' REGEXP 'b|z'",
            "'abc' NOT REGEXP 'd'",
            r"'a.c' REGEXP 'a\\.c'",
            r"'abc' REGEXP 'a\\.c'",
            "'abc' REGEXP '[[:alpha:]]+'",
            "'123' REGEXP '^[0-9]{3}$'",
            "'ab' REGEXP '^(ab)+$'",
            "'' REGEXP '^$'",
            "NULL REGEXP 'a'",
            "NULL NOT REGEXP 'a'",
            "'aXb' REGEXP 'a[[:upper:]]b'",
            "'é' REGEXP 'e'",
            "'É' REGEXP 'é'",
            "'abc' RLIKE 'c$'",
            "'abc' NOT RLIKE '^b'",
            r"'a+b' REGEXP 'a\\+b'",
            "'ab' REGEXP 'a{0}b'",
            "'abab' REGEXP '(ab){2}'",
            r"'foo.bar' REGEXP '\\w+\\.\\w+'",
            r"'2024-01-15' REGEXP '^\\d{4}-\\d{2}-\\d{2}$'",
            r"'abc' REGEXP '\\bab'",
            "123 REGEXP '^1'",
            "1.5 REGEXP '5$'",
            "'a' REGEXP 'a|'",
            "'ab' NOT REGEXP '^a$'",
        ],
    ),
    (
        " FROM events ORDER BY id",
        &[
            "name REGEXP '^event-0[1-5]$'",
            "name NOT REGEXP '[02468]$'",
            "note REGEXP '^a'",
            "note REGEXP 'A$'",
            "note NOT REGEXP 'eta'",
            "note RLIKE 'l.h'",
            "tag REGEXP '^red$'",
            "tag REGEXP ' $'",
            "label REGEXP 'ger$'",
            "label REGEXP '^.$'",
            "label NOT REGEXP '[a-z]'",
        ],
    ),
    (
        " FROM orders ORDER BY id",
        &[
            r"total REGEXP '^[0-9]+\\.[05]0$'",
            "placed_at REGEXP '^2024'",
            "status REGEXP 'ed$'",
            "status NOT REGEXP '^p'",
            "user_id REGEXP '^[1-3]$'",
            "(meta->>'$.tags[0]') REGEXP '^p'",
            "(meta->>'$.tags[0]') NOT REGEXP 'u'",
        ],
    ),
    (
        " FROM bounds ORDER BY id",
        &[
            "txt REGEXP '^$'",
            r"txt REGEXP '\\t'",
            r"txt REGEXP '^a\\s'",
            "fixed REGEXP '^a$'",
            "fixed NOT REGEXP ' '",
            "n REGEXP '^-'",
            "day REGEXP '-02-29$'",
            "u NOT REGEXP '^[0-9]+$'",
            "frac RLIKE '5$'",
        ],
    ),
];

const TRUTH_OF_TIME_VALUES: &[&str] = &[
    "SELECT id, NOT clock, clock OR 0, clock AND 1, clock XOR 0, clock IS TRUE, clock IS FALSE, clock IS NOT TRUE, IF(clock, 'y', 'n'), CASE WHEN clock THEN 'y' ELSE 'n' END FROM bounds ORDER BY id",
    "SELECT id FROM bounds WHERE clock ORDER BY id",
    "SELECT id FROM bounds WHERE NOT clock ORDER BY id",
    "SELECT TIME'00:00:00.5' IS TRUE, NOT TIME'00:00:00.500000', TIME'00:00:00.5' AND 1, IF(TIME'00:00:00.5', 'y', 'n'), TIME'00:30:00' IS TRUE, NOT TIME'00:00:01', IF(TIME'00:00:00', 'y', 'n'), TIME'-00:00:01' OR 0, CASE WHEN TIME'00:00:59' THEN 1 ELSE 0 END",
    "SELECT COUNT(*), SUM(clock IS TRUE), SUM(NOT clock) FROM bounds HAVING MAX(clock)",
    "SELECT b.id, c.id FROM bounds b JOIN bounds c ON c.clock AND b.id = c.id ORDER BY b.id",
    "SELECT id, SEC_TO_TIME(id * 30) IS TRUE, NOT SEC_TO_TIME(id - 3), IF(TIMEDIFF('10:00:00', '09:59:30'), 1, 0), NOT MAKETIME(0, 0, id - 1), NOT CAST('00:00:30' AS TIME) FROM users ORDER BY id",
];

const MEMBER_OF_EXACT_DECIMALS: &[&str] = &[
    r#"SELECT 1.5 MEMBER OF('[1.5]'), 1.0 MEMBER OF('[1]'), 1.50 MEMBER OF('[1.5]'), 2.5 MEMBER OF('[1.5]'), 1.5 MEMBER OF('["1.5"]'), CAST(2 AS DECIMAL(5,2)) MEMBER OF('[2]'), -0.0 MEMBER OF('[0]'), 0.1 MEMBER OF('[0.1]'), 1.5 MEMBER OF('1.5'), 1.5 MEMBER OF(NULL), 99999999999999999999.5 MEMBER OF('[1]')"#,
    "SELECT id, total MEMBER OF(meta->'$.items'), total MEMBER OF('[10.5, 50, 100.00, 7]'), 9.9 MEMBER OF(meta->'$.score'), (total * 2) MEMBER OF('[21, 100]') FROM orders ORDER BY id",
    "SELECT id, frac MEMBER OF('[0, 1.5, 2.5, -0.005]'), whole MEMBER OF('[0, 1, 2, 3, -1]') FROM bounds ORDER BY id",
];

const FRACTIONAL_POSITIONS_AND_COUNTS: &[&str] = &[
    "SELECT MID('abcdef', 1.5, 2), SUBSTRING('abcdef', 2.5), SUBSTRING('abcdef', 1.4, 1.5), SUBSTR('abcdef', 1, 1.5), LEFT('abcdef', 1.5), LEFT('abcdef', 2.4), RIGHT('abcdef', 2.5), RIGHT('abcdef', 0.4), INSERT('abc', 1.5, 1.5, 'X'), INSERT('abc', 2.5, 1, 'X'), INSERT('abc', 1, 1.5, 'X'), SUBSTRING('abcdef', -1.5), SUBSTRING('abcdef', -2.4, 1.6), LEFT('abcdef', 0.5), LEFT('abcdef', -0.5)",
    "SELECT MID('abcdef', 1.5e0, 2), SUBSTRING('abcdef', 2.5e0), LEFT('abcdef', 1.5e0), LEFT('abcdef', 2.5e0), LEFT('abcdef', 0.5e0), RIGHT('abcdef', 2.5e0), RIGHT('abcdef', 3.5e0), INSERT('abc', 1.5e0, 1.5e0, 'X'), INSERT('abc', 2.5e0, 2.5e0, 'X'), SUBSTRING('abcdef', -1.5e0), SUBSTRING('abcdef', -2.5e0)",
    "SELECT MID('abcdef', '1.5', 2), SUBSTRING('abcdef', '2.5'), LEFT('abcdef', '1.5'), RIGHT('abcdef', '2.9'), INSERT('abc', '1.5', '1.5', 'X'), LEFT('abcdef', '2x')",
    "SELECT id, LEFT(name, frac), RIGHT(name, frac), SUBSTRING(name, frac), SUBSTRING(name, 1, frac), INSERT(name, frac, frac, '#'), LEFT(name, approx), SUBSTRING(name, approx, 2), RIGHT(name, approx) FROM (SELECT b.id, b.frac, b.approx, 'abcdefgh' AS name FROM bounds b) t ORDER BY id",
    "SELECT id, LEFT(name, total / 10), SUBSTRING(name, total / 20), RIGHT(name, id / 2), INSERT(name, id / 4, id / 3, '#') FROM (SELECT o.id, o.total, 'abcdefghijklmnop' AS name FROM orders o) t ORDER BY id",
    "SELECT LEFT('abcdef', 18446744073709551616), LEFT('abcdef', 99999999999999999999.5), SUBSTRING('abcdef', 2, 99999999999999999999999), RIGHT('abcdef', 1e30), SUBSTRING('abcdef', -99999999999999999999)",
];

const JSON_PATH_OPERANDS: &[&str] = &[
    "SELECT id, 1 + meta->'$.score', meta->'$.score' + 1, 2 * meta->'$.items[0]' - 1, -meta->'$.score' FROM orders ORDER BY id",
    "SELECT id, user_id = meta->'$.items[0]', meta->'$.score' >= meta->'$.items[0]', meta->'$.score' < meta->'$.items[0]', meta->>'$.score' = meta->'$.score', meta->>'$.tags[0]' = meta->'$.tags[0]', meta->'$.tags' = meta->'$.tags' FROM orders ORDER BY id",
    "SELECT id, meta->>'$.tags[0]' LIKE 'p%', meta->>'$.tags[0]' NOT LIKE '%u%', meta->'$.missing' IS NULL, meta->'$.tags' IS NOT NULL, meta->'$.score' IS TRUE, meta->>'$.score' BETWEEN 1 AND 3, meta->>'$.tags[0]' IN ('premium', 'gift'), meta->'$.score' AND 1, NOT meta->'$.score' FROM orders ORDER BY id",
    "SELECT id FROM orders WHERE meta->>'$.tags[0]' LIKE 'p%' AND 1 < meta->'$.score' ORDER BY id",
    "SELECT o.id, u.name FROM orders o JOIN users u ON u.id = o.meta->'$.items[0]' ORDER BY o.id, u.id",
    "SELECT o.id FROM orders o WHERE o.user_id + o.meta->'$.score' > 5 OR o.meta->>'$.tags[0]' REGEXP '^g' ORDER BY o.id",
    "SELECT id, CONCAT(meta->>'$.tags[0]', '-', id), meta->'$.items[0]' DIV 2, meta->'$.items[0]' % 2 = 1, CASE meta->>'$.tags[0]' WHEN 'premium' THEN 1 ELSE 0 END, meta->'$.score' <=> 1.5 FROM orders ORDER BY id",
];

const JSON_COMPARED_WITH_NULL: &[&str] = &[
    "SELECT id, meta <=> NULL, meta = NULL, NULL <=> meta, meta <> NULL, NULL = meta, meta->'$.score' <=> NULL, meta->'$.missing' <=> NULL, meta < NULL FROM orders ORDER BY id",
    "SELECT id FROM orders WHERE meta <=> NULL ORDER BY id",
    "SELECT id FROM orders WHERE NOT (meta <=> NULL) AND meta->'$.missing' <=> NULL ORDER BY id",
];

const MID_WITH_FROM_AND_FOR: &[&str] = &[
    "SELECT MID('abcdef' FROM 3), MID('abcdef' FROM 2 FOR 2), MID('abcdef' FROM -4 FOR 2), MID('abcdef' FROM 7), MID(NULL FROM 1), MID('日本語' FROM 2 FOR 1)",
    "SELECT id, MID(name FROM 7), MID(name FROM id FOR 2), MID(note FROM -2) FROM events ORDER BY id",
];

const BARE_UTC_CLOCKS: &[&str] = &[
    "SELECT UTC_DATE = CURDATE(), UTC_TIME = CURTIME(), UTC_TIMESTAMP = NOW(), LENGTH(UTC_DATE), LENGTH(utc_time), LENGTH(Utc_Timestamp), UTC_DATE = UTC_DATE(), UTC_TIMESTAMP + 0 = UTC_TIMESTAMP() + 0",
    "SELECT id, placed_at < UTC_TIMESTAMP, DATE(placed_at) < UTC_DATE FROM orders WHERE DATE(placed_at) <> UTC_DATE ORDER BY id",
];

const REGEXP_WITH_A_NULL_PATTERN: &[&str] = &[
    "SELECT 'a' REGEXP NULL",
    "SELECT 'a' NOT RLIKE NULL",
    "SELECT id, note REGEXP NULL FROM events ORDER BY id",
    "SELECT id FROM events WHERE note REGEXP NULL OR id = 3 ORDER BY id",
];

const JSON_RANGES_OVER_EMPTY_ARRAYS: &[&str] = &[
    r#"SELECT JSON_EXTRACT('[]', '$[0 to 1]'), JSON_EXTRACT('{"a":[]}', '$.a[0 to 1]'), JSON_EXTRACT('[1]', '$[1 to 2]'), JSON_EXTRACT('[1,2,3]', '$[1 to 5]'), JSON_EXTRACT('[1,2,3]', '$[0 to 0]'), JSON_EXTRACT('[1,2,3]', '$[last to last]'), JSON_EXTRACT('[1,2,3]', '$[5 to 9]'), JSON_EXTRACT('[]', '$[*]'), JSON_EXTRACT('[[]]', '$[0][0 to 1]'), JSON_EXTRACT('{}', '$.*'), JSON_EXTRACT('[1,2,3]', '$[last-1 to last]'), JSON_EXTRACT('5', '$[0 to 1]'), JSON_EXTRACT('5', '$[1 to 2]'), JSON_EXTRACT('[]', '$**.a'), JSON_EXTRACT('[1,[2]]', '$[0 to 1][0]')"#,
    "SELECT id, meta->'$.items[0 to 1]', meta->'$.items[1 to 9]', meta->'$.tags[0 to 0]', meta->'$.tags[*]', meta->'$.items[last to last]' FROM orders ORDER BY id",
];

const EXISTS_OVER_SEVERAL_COLUMNS: &[&str] = &[
    "SELECT EXISTS (SELECT * FROM users), EXISTS (SELECT * FROM users WHERE id > 100), NOT EXISTS (SELECT id, name FROM users), EXISTS (SELECT 1, 2), EXISTS (TABLE users), EXISTS (SELECT * FROM users LIMIT 0), EXISTS (SELECT id, name FROM users UNION SELECT id, name FROM users)",
    "SELECT id FROM users u WHERE EXISTS (TABLE events) ORDER BY id",
    "SELECT id FROM users u WHERE NOT EXISTS (SELECT * FROM orders) OR EXISTS (SELECT id, total FROM orders WHERE total > 900) ORDER BY id",
    "SELECT id, EXISTS (SELECT * FROM orders o WHERE o.user_id = u.id), EXISTS (SELECT o.id, o.total FROM orders o WHERE o.user_id = u.id AND o.total > 100) FROM users u ORDER BY id",
    "SELECT active, COUNT(*) FROM events GROUP BY active HAVING EXISTS (SELECT * FROM orders WHERE total > 900) ORDER BY active",
];

const STORED_JSON_AS_TEXT: &[&str] = &[
    "SELECT id, meta FROM orders ORDER BY id",
    r#"SELECT id, SHA(meta), MD5(meta), CONCAT(meta, ''), CAST(meta AS CHAR), LENGTH(meta), CHAR_LENGTH(meta), UPPER(meta), LEFT(meta, 12), meta LIKE '%"tags": [%' FROM orders ORDER BY id"#,
];

pub fn cases() -> Vec<OracleCase> {
    let mut cases = Vec::new();
    for (family, statements) in [
        ("operators comparison", COMPARISON),
        ("operators logical", LOGICAL),
        ("operators range and membership", RANGE_AND_MEMBERSHIP),
        ("operators null and truth tests", NULL_TESTS),
        ("operators exists", EXISTS),
        ("operators arithmetic", ARITHMETIC),
        ("operators bitwise", BITWISE),
        ("operators case", CASE_CONSTRUCT),
        ("operators pattern match", PATTERN_MATCH),
        ("operators json path and membership", JSON_PATH),
        ("clock and identifier functions", VOLATILE),
        ("function spellings across types", ALIASES),
        ("rollup and grouping", ROLLUP),
        ("table statements", TABLE_STATEMENT),
        ("values statements", VALUES_CONSTRUCTOR),
        ("stored json as text", STORED_JSON_AS_TEXT),
        ("exists over several columns", EXISTS_OVER_SEVERAL_COLUMNS),
        (
            "json ranges over empty arrays",
            JSON_RANGES_OVER_EMPTY_ARRAYS,
        ),
        ("regexp with a null pattern", REGEXP_WITH_A_NULL_PATTERN),
        ("bare utc clocks", BARE_UTC_CLOCKS),
        ("mid with from and for", MID_WITH_FROM_AND_FOR),
        ("json compared with null", JSON_COMPARED_WITH_NULL),
        ("json path operands", JSON_PATH_OPERANDS),
        (
            "fractional positions and counts",
            FRACTIONAL_POSITIONS_AND_COUNTS,
        ),
        ("member of exact decimals", MEMBER_OF_EXACT_DECIMALS),
        ("truth of time values", TRUTH_OF_TIME_VALUES),
    ] {
        cases.extend(statements.iter().map(|sql| OracleCase {
            sql_mode: "",
            family,
            sql: (*sql).to_owned(),
            ordered: true,
        }));
    }
    for (tail, expressions) in REGEX {
        for chunk in expressions.chunks(3) {
            let id = if tail.is_empty() { "" } else { "id, " };
            cases.push(OracleCase {
                sql_mode: "",
                family: "operators pattern match",
                sql: format!("SELECT {id}{}{tail}", chunk.join(", ")),
                ordered: true,
            });
        }
    }
    cases
}

/// Statement forms and clauses that stay refused (docs/limitations.md), as
/// `(family, sql, needle)`: each must fail by name, never run as something
/// narrower.
pub const REFUSED: &[(&str, &str, &str)] = &[
    (
        "reject row compared with a subquery that selects every column",
        "SELECT id FROM events WHERE (id, name) IN (TABLE users) ORDER BY id",
        "unsupported subquery",
    ),
    (
        "reject locking read for update",
        "SELECT id FROM users WHERE id < 3 ORDER BY id FOR UPDATE",
        "unsupported query clause",
    ),
    (
        "reject locking read for share",
        "SELECT id FROM users u WHERE id < 3 ORDER BY id FOR SHARE OF u",
        "unsupported query clause",
    ),
    (
        "reject locking read that skips locked rows",
        "SELECT id FROM users WHERE id < 3 ORDER BY id FOR UPDATE SKIP LOCKED",
        "unsupported query clause",
    ),
    (
        "reject locking read that does not wait",
        "SELECT id FROM users WHERE id < 3 ORDER BY id FOR SHARE NOWAIT",
        "unsupported query clause",
    ),
    (
        "reject lock in share mode",
        "SELECT id FROM users WHERE id < 3 ORDER BY id LOCK IN SHARE MODE",
        "parse",
    ),
    (
        "reject select into outfile",
        "SELECT id FROM users INTO OUTFILE '/tmp/users.txt'",
        "parse",
    ),
    (
        "reject select into a user variable",
        "SELECT id INTO @first FROM users WHERE id = 1",
        "unsupported query clause",
    ),
    (
        "reject create view",
        "CREATE VIEW recent AS SELECT id FROM users",
        "unsupported statement",
    ),
    (
        "reject drop view",
        "DROP VIEW recent",
        "unsupported statement",
    ),
    (
        "reject distinct over rollup",
        "SELECT DISTINCT active, COUNT(*) FROM events GROUP BY active WITH ROLLUP",
        "unsupported query clause",
    ),
    (
        "reject window function over rollup",
        "SELECT user_id, COUNT(*), SUM(COUNT(*)) OVER () FROM orders GROUP BY user_id WITH ROLLUP",
        "unsupported query clause",
    ),
    (
        "reject grouping in order by",
        "SELECT status, COUNT(*) FROM orders GROUP BY status WITH ROLLUP ORDER BY GROUPING(status), status",
        "unsupported expression",
    ),
    (
        // MySQL refuses this one too (error 1111).
        "reject grouping without rollup",
        "SELECT active, GROUPING(active) FROM events GROUP BY active",
        "unsupported expression",
    ),
    (
        "reject grouping of an expression that is not grouped",
        "SELECT active, GROUPING(score) FROM events GROUP BY active WITH ROLLUP",
        "not in GROUP BY",
    ),
    (
        "reject connection function beside another item",
        "SELECT DATABASE(), id FROM users",
        "unsupported expression",
    ),
    (
        "reject server version inside an expression",
        "SELECT LENGTH(VERSION())",
        "unsupported expression",
    ),
];

#[test]
fn every_statement_is_listed_once() {
    let cases = cases();
    let distinct = cases
        .iter()
        .map(|case| case.sql.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(distinct.len(), cases.len());
}

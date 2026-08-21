-- BI-tool SQL shapes, modeled from the documented generation behavior of
-- Metabase, Superset, Looker and Tableau against a MySQL source.
--
-- PROVENANCE, stated plainly: these are reconstructed shapes, not a captured
-- query log. They encode how these tools are documented to compile a time
-- series, a cohort, a filtered dimension and a symmetric aggregate — which is
-- enough to rank the missing surface, and not enough to claim a measured
-- frequency. When a real log exists, feed it to the same harness and the
-- ranking below should be replaced by its output, not merged with it.

-- ---------------------------------------------------------------------------
-- Metabase: time series with a unit grain. Metabase compiles every "group by
-- month/week/quarter" breakout into DATE_FORMAT or a truncation expression,
-- and wraps the source column in CONVERT_TZ when a report timezone is set.
-- ---------------------------------------------------------------------------
SELECT DATE_FORMAT(CONVERT_TZ(o.created_at, 'UTC', 'America/New_York'), '%Y-%m-01') AS grain,
       COUNT(*) AS n,
       SUM(o.total) AS revenue
FROM orders o
GROUP BY DATE_FORMAT(CONVERT_TZ(o.created_at, 'UTC', 'America/New_York'), '%Y-%m-01')
ORDER BY grain;

SELECT DATE_FORMAT(o.created_at, '%x-%v') AS iso_week, COUNT(*) AS n
FROM orders o
GROUP BY DATE_FORMAT(o.created_at, '%x-%v');

SELECT DATE_FORMAT(o.created_at, '%W') AS weekday_name,
       DATE_FORMAT(o.created_at, '%b %e, %Y') AS pretty,
       DATE_FORMAT(o.created_at, '%r') AS clock
FROM orders o;

-- Metabase "previous period" comparison.
SELECT SUM(o.total) AS revenue
FROM orders o
WHERE o.created_at >= DATE_ADD(CURDATE(), INTERVAL -1 MONTH)
  AND o.created_at <  CURDATE();

-- ---------------------------------------------------------------------------
-- Superset: time grain expressions plus moving averages. Superset's MySQL
-- time grains lean on DATE_ADD with compound and single units, and its
-- "rolling window" post-processing compiles to explicit window frames.
-- ---------------------------------------------------------------------------
SELECT DATE_ADD(DATE(o.created_at), INTERVAL -WEEKDAY(o.created_at) DAY) AS week_start,
       SUM(o.total) AS revenue,
       AVG(SUM(o.total)) OVER (ORDER BY DATE_ADD(DATE(o.created_at), INTERVAL -WEEKDAY(o.created_at) DAY)
                               ROWS BETWEEN 6 PRECEDING AND CURRENT ROW) AS rolling_7
FROM orders o
GROUP BY week_start;

SELECT o.tenant_id,
       SUM(o.total) AS revenue,
       SUM(SUM(o.total)) OVER (PARTITION BY o.tenant_id ORDER BY MIN(o.created_at)
                               ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running_total
FROM orders o
GROUP BY o.tenant_id;

SELECT o.created_at,
       o.total,
       LAG(o.total, 1, 0) OVER (PARTITION BY o.tenant_id ORDER BY o.created_at) AS prev_total,
       LEAD(o.total) OVER (PARTITION BY o.tenant_id ORDER BY o.created_at) AS next_total,
       o.total - LAG(o.total) OVER (PARTITION BY o.tenant_id ORDER BY o.created_at) AS delta
FROM orders o;

SELECT NTILE(4) OVER (ORDER BY o.total) AS quartile, COUNT(*) AS n
FROM orders o
GROUP BY quartile;

SELECT FIRST_VALUE(o.status) OVER w AS first_status,
       LAST_VALUE(o.status) OVER w AS last_status
FROM orders o
WINDOW w AS (PARTITION BY o.tenant_id ORDER BY o.created_at
             ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING);

-- Superset compound-interval grain.
SELECT DATE_ADD(o.created_at, INTERVAL '1-2' YEAR_MONTH) AS shifted FROM orders o;
SELECT DATE_SUB(o.created_at, INTERVAL '3 4:00:00' DAY_SECOND) AS backdated FROM orders o;

-- ---------------------------------------------------------------------------
-- Looker: symmetric aggregates. Looker's documented technique for computing a
-- correct SUM across a fanned-out join hashes the primary key with MD5 and
-- reassembles the value from a CAST'd substring, which is why MD5 and CAST
-- appear together in nearly every Looker-generated measure.
-- ---------------------------------------------------------------------------
SELECT COALESCE(CAST(SUM(DISTINCT CAST(CONV(SUBSTR(MD5(o.id), 1, 15), 16, 10) AS DECIMAL(38,0)) * 1000000000
                      + CAST(o.total * 100 AS DECIMAL(38,0)))
                     - SUM(DISTINCT CAST(CONV(SUBSTR(MD5(o.id), 1, 15), 16, 10) AS DECIMAL(38,0)) * 1000000000)
                     AS DECIMAL(38,0)) / 100, 0) AS total_revenue
FROM orders o
LEFT JOIN order_items i ON i.order_id = o.id;

SELECT ANY_VALUE(o.status) AS a_status, o.tenant_id, COUNT(*) AS n
FROM orders o
GROUP BY o.tenant_id;

-- ---------------------------------------------------------------------------
-- Tableau: heavy explicit casting and statistical measures.
-- ---------------------------------------------------------------------------
SELECT CAST(o.total AS DECIMAL(18,4)) AS amt,
       CAST(o.created_at AS DATE) AS d,
       CAST(o.tenant_id AS CHAR(32)) AS t,
       CONVERT(o.status, CHAR) AS s
FROM orders o;

SELECT STDDEV(o.total) AS sd,
       STDDEV_POP(o.total) AS sdp,
       STDDEV_SAMP(o.total) AS sds,
       VARIANCE(o.total) AS v,
       VAR_POP(o.total) AS vp,
       VAR_SAMP(o.total) AS vs
FROM orders o;

SELECT BIT_OR(o.flags) AS any_flag, BIT_AND(o.flags) AS all_flags, BIT_XOR(o.flags) AS parity
FROM orders o;

-- ---------------------------------------------------------------------------
-- Dimension cleanup and JSON filtering, common to all four tools.
-- ---------------------------------------------------------------------------
SELECT SUBSTRING_INDEX(o.source_url, '/', -1) AS last_segment,
       SUBSTRING_INDEX(SUBSTRING_INDEX(o.utm, '&', 1), '=', -1) AS utm_source
FROM orders o;

SELECT o.id
FROM orders o
WHERE JSON_CONTAINS(o.attributes, '"premium"', '$.tags')
  AND JSON_LENGTH(o.attributes, '$.items') > 3
  AND JSON_TYPE(JSON_EXTRACT(o.attributes, '$.score')) = 'DOUBLE'
  AND JSON_VALID(o.attributes);

SELECT JSON_KEYS(o.attributes) AS ks FROM orders o;

SELECT MAKETIME(o.hour, o.minute, 0) AS t FROM shifts o;

SELECT EXTRACT(YEAR_MONTH FROM o.created_at) AS ym FROM orders o;

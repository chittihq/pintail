SHOW DATABASES;

SHOW FULL TABLES;

SHOW FULL COLUMNS FROM events;

SHOW KEYS FROM events;

SELECT c.table_name, c.column_name, c.ordinal_position, t.table_type
FROM information_schema.columns AS c
JOIN information_schema.tables AS t
  ON c.table_schema = t.table_schema
 AND c.table_name = t.table_name
WHERE c.table_schema = 'analytics'
ORDER BY c.table_name, c.ordinal_position;

SELECT c.table_name, COUNT(c.column_name) AS column_count
FROM information_schema.columns AS c
WHERE c.table_schema = 'analytics'
GROUP BY c.table_name
ORDER BY c.table_name;

SELECT COUNT(*) AS view_count
FROM information_schema.views
WHERE table_schema = 'analytics';

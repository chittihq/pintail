//! `JSON_TABLE` against the oracle: constant documents, documents read from
//! a column of an earlier relation, every supported column type with the
//! values it stores and the values it refuses, `EXISTS PATH`, `ON EMPTY` and
//! `ON ERROR`, nested and sibling-nested column lists, and the table
//! function inside the query shapes around it.
//!
//! A lone table function returns its rows in document order on both sides,
//! so those cases compare in order. Anything joined is either ordered by a
//! total key or compared as a multiset.

use super::OracleCase;

const CONSTANT: &str = "json_table constant document";
const TYPES: &str = "json_table column types";
const FALLBACK: &str = "json_table on empty and on error";
const NESTED: &str = "json_table nested paths";
const SHAPES: &str = "json_table in query shapes";
const LATERAL: &str = "json_table over a column";
const COMPOSED: &str = "json_table beside a lateral derived table";

#[allow(clippy::too_many_lines, clippy::needless_raw_string_hashes)] // one table of shapes
pub fn cases() -> Vec<OracleCase> {
    let mut cases = Vec::new();
    let mut ordered = |family: &'static str, sql: &str| {
        cases.push(OracleCase {
            sql_mode: "",
            family,
            sql: sql.to_owned(),
            ordered: true,
        });
    };

    // Row paths and what a document that is not an array yields.
    for sql in [
        r#"SELECT * FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"a":1}', '$' COLUMNS (v INT PATH '$.a')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"a":1}', '$[*]' COLUMNS (v INT PATH '$.a')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('5', '$' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,2]', '$' COLUMNS (v JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(NULL, '$' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT COUNT(*) FROM JSON_TABLE(NULL, '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[]', '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,[2,3],{"a":[4]}]', '$**[*]' COLUMNS (o FOR ORDINALITY, j JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,2,3,4]', '$[1 to 2]' COLUMNS (o FOR ORDINALITY, j JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,2,3,4]', '$[last]' COLUMNS (o FOR ORDINALITY, j JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,2,3,4]', '$[last-2 to last]' COLUMNS (o FOR ORDINALITY, v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"a":{"x":1},"b":{"x":2}}', '$.*' COLUMNS (o FOR ORDINALITY, x INT PATH '$.x')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"rows":[{"k":"a"},{"k":"b"}]}', '$.rows[*]' COLUMNS (o FOR ORDINALITY, k VARCHAR(4) PATH '$.k')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1]', '$**[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(JSON_ARRAY(1, 2, 'x'), '$[*]' COLUMNS (v VARCHAR(5) PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(CONCAT('[', '1,2', ']'), '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(CAST('[1]' AS JSON), '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(JSON_EXTRACT('{"a":[1,2]}', '$.a'), '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE(JSON_OBJECT('a', JSON_ARRAY(7, 8)), '$.a[*]' COLUMNS (o FOR ORDINALITY, v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1}]', '$[*]' COLUMNS (`a b` INT PATH '$.a', `SELECT` INT PATH '$.a')) AS jt"#,
        r#"SELECT jt.v, v FROM JSON_TABLE('[1]', '$[*]' COLUMNS (V INT PATH '$')) AS jt"#,
        // A column path with several matches is an array: a document for a
        // JSON column, nothing a scalar column can hold.
        r#"SELECT * FROM JSON_TABLE('[[1],[1,2],{"a":1},1]', '$[*]' COLUMNS (o FOR ORDINALITY, j JSON PATH '$[*]', k JSON PATH '$.*', i INT PATH '$[*]', l JSON PATH '$[0]', m JSON PATH '$**.a')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1,2]', '$' COLUMNS (v INT PATH '$[*]', w VARCHAR(20) PATH '$[*]', x VARCHAR(20) PATH '$', y JSON PATH '$[*]')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1]', '$' COLUMNS (v INT PATH '$[*]')) AS jt"#,
    ] {
        ordered(CONSTANT, sql);
    }

    // What each column type stores, and what it refuses and leaves NULL.
    for sql in [
        r#"SELECT * FROM JSON_TABLE('[1.5, 2.5, -1.5, 1e2, "1e2", "1.5", true, false, null, 99999999999, "abc", ""]', '$[*]' COLUMNS (o FOR ORDINALITY, v INT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1.5, 2.5, -1.5, 1e2, "1e2", "1.5", true, false, null, 99999999999, -1, 18446744073709551615, 18446744073709551616]', '$[*]' COLUMNS (o FOR ORDINALITY, v BIGINT UNSIGNED PATH '$', w BIGINT PATH '$', t TINYINT PATH '$', u TINYINT UNSIGNED PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["+5","-5","007","5.0","0x10","99999999999999999999","-0"," 5","5 ","1e0", 5.0, 1e30, -1e30, 9223372036854775807, 9223372036854775808, -9223372036854775808, 0.4, -0.5, 0.5, 1.5e0, 3.5]', '$[*]' COLUMNS (o FOR ORDINALITY, i INT PATH '$', b BIGINT PATH '$', u BIGINT UNSIGNED PATH '$', s SMALLINT PATH '$', m MEDIUMINT UNSIGNED PATH '$', iu INT UNSIGNED PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[127, 128, -128, -129, 255, 256, 32767, 32768, 65535, 65536, 8388607, 8388608, 16777215, 16777216, 2147483647, 2147483648, 4294967295, 4294967296]', '$[*]' COLUMNS (o FOR ORDINALITY, t TINYINT PATH '$', tu TINYINT UNSIGNED PATH '$', s SMALLINT PATH '$', su SMALLINT UNSIGNED PATH '$', m MEDIUMINT PATH '$', mu MEDIUMINT UNSIGNED PATH '$', i INT PATH '$', iu INT UNSIGNED PATH '$', bo BOOLEAN PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1.5, 2.555, -1.5, 1e2, "1e2", "1.5", true, false, null, 99999999999, "abc", 123.456]', '$[*]' COLUMNS (o FOR ORDINALITY, v DECIMAL(5,2) PATH '$', d DOUBLE PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["1.5x"," 1.5","","1.5 ","+1.5","-.5","1.","1e-2", 2.345, 2.335, 0.005, -0.005, 1e-10, 999.994, 999.995, 1000, -999.995, 12345678901234567890, "0.125", ".5e1"]', '$[*]' COLUMNS (o FOR ORDINALITY, d DECIMAL(5,2) PATH '$', e DECIMAL PATH '$', f DECIMAL(20,3) PATH '$', g NUMERIC(3,0) PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["1.5x"," 1.5","","1.5 ","+1.5","-.5","1.","1e-2", 2.345, 1e40, -1e40, 12345678901234567890, 0.1, 16777217, "inf", "nan", "1e400"]', '$[*]' COLUMNS (o FOR ORDINALITY, d DOUBLE PATH '$', r REAL PATH '$', p DOUBLE PRECISION PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1.5, 0.25, -100, "2.5", true, 1e40, "x", null]', '$[*]' COLUMNS (o FOR ORDINALITY, f FLOAT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["abc", "abcdef", 1, 1.50, true, null, {"a":1}, [1], "é", 1e10]', '$[*]' COLUMNS (o FOR ORDINALITY, v VARCHAR(3) PATH '$', c CHAR(5) PATH '$', t TEXT PATH '$', j JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["", "a", "ab", false, -12, 0.5]', '$[*]' COLUMNS (o FOR ORDINALITY, c CHAR PATH '$', t TINYTEXT PATH '$', m MEDIUMTEXT PATH '$', l LONGTEXT PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":true},{"a":"x"},{"a":null},{"a":1.0},{"a":"1"}]', '$[*]' COLUMNS (j JSON PATH '$.a', e TINYINT EXISTS PATH '$.a', v VARCHAR(10) PATH '$.a', i INT PATH '$.a')) AS jt"#,
        r#"SELECT j, JSON_TYPE(j), j = 1, j = '1' FROM JSON_TABLE('[1,"1",true]', '$[*]' COLUMNS (j JSON PATH '$')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"a":"2024-01-02"}', '$' COLUMNS (d DATE PATH '$.a', dt DATETIME PATH '$.a', dt6 DATETIME(6) PATH '$.a')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('["2024-01-02","2024-02-30","2024-1-2","20240102","2024-01-02T03:04:05","2024-01-02 03:04:05.5","0000-00-00","2024-01-02 25:00:00", 1, null, "x", true, 20240102]', '$[*]' COLUMNS (o FOR ORDINALITY, d DATE PATH '$', dt DATETIME PATH '$', dt3 DATETIME(3) PATH '$')) AS jt"#,
        // EXISTS answers 1 or 0 in the column's own type.
        r#"SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (e VARCHAR(5) EXISTS PATH '$', g DECIMAL(4,1) EXISTS PATH '$.x', f DOUBLE EXISTS PATH '$', b BIGINT UNSIGNED EXISTS PATH '$', t TEXT EXISTS PATH '$.q')) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1},{"a":null},{}]', '$[*]' COLUMNS (o FOR ORDINALITY, e INT EXISTS PATH '$.a', n INT EXISTS PATH '$.a.b', w INT EXISTS PATH '$' NULL ON EMPTY)) AS jt"#,
    ] {
        ordered(TYPES, sql);
    }

    // ON EMPTY answers a path with no match; ON ERROR a value the column
    // cannot hold. A JSON null is neither: it is NULL.
    for sql in [
        r#"SELECT * FROM JSON_TABLE('[{"a":"3"},{"a":2},{"b":1},{"a":0},{"a":[1,2]}]', '$[*]' COLUMNS (rowid FOR ORDINALITY, ac VARCHAR(100) PATH '$.a' DEFAULT '111' ON EMPTY DEFAULT '999' ON ERROR, aj JSON PATH '$.a' DEFAULT '{"x": 333}' ON EMPTY, bx INT EXISTS PATH '$.b')) AS tt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1},{"a":null},{}]', '$[*]' COLUMNS (o FOR ORDINALITY, v INT PATH '$.a' DEFAULT '7' ON EMPTY, w INT PATH '$.a' DEFAULT '8' ON ERROR, x VARCHAR(10) PATH '$.a' NULL ON EMPTY NULL ON ERROR)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{}]', '$[*]' COLUMNS (v INT PATH '$.a' DEFAULT '"12"' ON EMPTY, w VARCHAR(10) PATH '$.a' DEFAULT '"q"' ON EMPTY, y JSON PATH '$.a' DEFAULT '[1]' ON EMPTY, z VARCHAR(10) PATH '$.a' DEFAULT '12' ON EMPTY)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{}]', '$[*]' COLUMNS (v INT PATH '$.a' DEFAULT '1.5' ON EMPTY, w INT PATH '$.a' DEFAULT 'true' ON EMPTY, y DECIMAL(4,2) PATH '$.a' DEFAULT '"1.239"' ON EMPTY, z JSON PATH '$.a' DEFAULT '"s"' ON EMPTY, zz VARCHAR(9) PATH '$.a' DEFAULT 'true' ON EMPTY)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":"abcdef"},{"a":[1]},{"a":"x"}]', '$[*]' COLUMNS (v VARCHAR(3) PATH '$.a' DEFAULT '"df"' ON ERROR, w INT PATH '$.a' DEFAULT '-1' ON ERROR, x JSON PATH '$.a' DEFAULT '{"e":1}' ON ERROR)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":300},{"a":"x"},{"a":5},{}]', '$[*]' COLUMNS (t TINYINT PATH '$.a' DEFAULT '-1' ON ERROR DEFAULT '-2' ON EMPTY, d DECIMAL(3,1) PATH '$.a' DEFAULT '0.5' ON ERROR, dt DATE PATH '$.a' DEFAULT '"2000-01-01"' ON ERROR)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1]', '$[*]' COLUMNS (v INT PATH '$' ERROR ON EMPTY ERROR ON ERROR, w INT PATH '$' NULL ON ERROR NULL ON EMPTY)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[1.5]', '$[*]' COLUMNS (v INT PATH '$' ERROR ON ERROR)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":[1]}]', '$[*]' COLUMNS (v INT PATH '$.a[0]' ERROR ON ERROR, w INT PATH '$.a[1]' DEFAULT '9' ON EMPTY)) AS jt"#,
    ] {
        ordered(FALLBACK, sql);
    }

    // A nested list adds rows beside its parent's; sibling lists take
    // turns, the others NULL, and a parent with none keeps one row.
    for sql in [
        r#"SELECT * FROM JSON_TABLE('[{"a":1,"b":[10,20]},{"a":2,"b":[]},{"a":3}]', '$[*]' COLUMNS (o FOR ORDINALITY, a INT PATH '$.a', NESTED PATH '$.b[*]' COLUMNS (bo FOR ORDINALITY, b INT PATH '$'))) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1,"b":[10,20],"c":[100,200,300]},{"a":2,"b":[],"c":[7]},{"a":3},{"a":4,"b":[5]}]', '$[*]' COLUMNS (o FOR ORDINALITY, a INT PATH '$.a', NESTED PATH '$.b[*]' COLUMNS (bo FOR ORDINALITY, b INT PATH '$'), NESTED PATH '$.c[*]' COLUMNS (co FOR ORDINALITY, c INT PATH '$'))) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1,"b":[{"x":1,"y":[5,6]},{"x":2,"y":[]}]}]', '$[*]' COLUMNS (a INT PATH '$.a', NESTED PATH '$.b[*]' COLUMNS (x INT PATH '$.x', NESTED PATH '$.y[*]' COLUMNS (y INT PATH '$', yo FOR ORDINALITY)))) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"a":1,"b":[10,20],"c":[100,200,300]}]', '$[*]' COLUMNS (NESTED PATH '$.b[*]' COLUMNS (b INT PATH '$'), a INT PATH '$.a', NESTED PATH '$.c[*]' COLUMNS (c INT PATH '$', NESTED PATH '$' COLUMNS (z INT PATH '$')), o FOR ORDINALITY)) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"b":[{"x":[1,2],"y":[3]},{"x":[],"y":[4,5]}],"c":[9]}]', '$[*]' COLUMNS (NESTED PATH '$.b[*]' COLUMNS (bo FOR ORDINALITY, NESTED PATH '$.x[*]' COLUMNS (x INT PATH '$'), NESTED PATH '$.y[*]' COLUMNS (y INT PATH '$')), NESTED PATH '$.c[*]' COLUMNS (c INT PATH '$'))) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('{"k":[1,2],"m":{"p":"x","q":"y"}}', '$' COLUMNS (NESTED PATH '$.k[*]' COLUMNS (k INT PATH '$'), NESTED PATH '$.m.*' COLUMNS (mo FOR ORDINALITY, m VARCHAR(3) PATH '$'))) AS jt"#,
        r#"SELECT * FROM JSON_TABLE('[{"n":[]},{"n":[1]},{}]', '$[*]' COLUMNS (o FOR ORDINALITY, NESTED PATH '$.n[*]' COLUMNS (n INT PATH '$' DEFAULT '0' ON EMPTY, e INT EXISTS PATH '$'))) AS jt"#,
        r#"SELECT o, COUNT(b), SUM(b) FROM JSON_TABLE('[{"b":[10,20]},{"b":[]},{"b":[5]}]', '$[*]' COLUMNS (o FOR ORDINALITY, NESTED PATH '$.b[*]' COLUMNS (b INT PATH '$'))) AS jt GROUP BY o ORDER BY o"#,
    ] {
        ordered(NESTED, sql);
    }

    // The table function as a relation among the others of a query.
    for sql in [
        r#"SELECT COUNT(*), SUM(v), MIN(v), MAX(v) FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS jt"#,
        r#"SELECT v, COUNT(*) FROM JSON_TABLE('[1,2,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt GROUP BY v ORDER BY v"#,
        r#"SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt WHERE v > 1"#,
        r#"SELECT v + 1, CONCAT('n', v) FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt WHERE v IS NOT NULL ORDER BY v DESC"#,
        r#"SELECT * FROM (SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt) AS x WHERE v > 1"#,
        r#"WITH c AS (SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt) SELECT * FROM c ORDER BY v DESC"#,
        r#"SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt UNION ALL SELECT 9 ORDER BY 1"#,
        r#"SELECT * FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS jt ORDER BY v LIMIT 2"#,
        r#"SELECT * FROM JSON_TABLE('[3,1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS jt ORDER BY v DESC LIMIT 2"#,
        r#"SELECT DISTINCT v FROM JSON_TABLE('[2,1,2,1]', '$[*]' COLUMNS (v INT PATH '$')) AS jt ORDER BY v"#,
        r#"SELECT * FROM JSON_TABLE('[1,2]', '$[*]' COLUMNS (v INT PATH '$')) AS a, JSON_TABLE('[3,4]', '$[*]' COLUMNS (w INT PATH '$')) AS b ORDER BY 1, 2"#,
        r#"SELECT a.v, b.w FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS a JOIN JSON_TABLE('[2,3,4]', '$[*]' COLUMNS (w INT PATH '$')) AS b ON a.v = b.w ORDER BY 1"#,
        r#"SELECT a.v, b.w FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS a LEFT JOIN JSON_TABLE('[2,3,4]', '$[*]' COLUMNS (w INT PATH '$')) AS b ON a.v = b.w ORDER BY 1"#,
        r#"SELECT u.id, x.v FROM users u JOIN JSON_TABLE('[1,4,9]', '$[*]' COLUMNS (v INT PATH '$')) AS x ON x.v = u.id ORDER BY 1"#,
        r#"SELECT u.id, x.v FROM users u LEFT JOIN JSON_TABLE('[1,4,9]', '$[*]' COLUMNS (v INT PATH '$')) AS x ON x.v = u.id ORDER BY 1"#,
        r#"SELECT x.v, u.name FROM JSON_TABLE('[1,4,9]', '$[*]' COLUMNS (v INT PATH '$')) AS x LEFT JOIN users u ON x.v = u.id ORDER BY 1"#,
        r#"SELECT x.v, u.name FROM JSON_TABLE('[1,4,9]', '$[*]' COLUMNS (v INT PATH '$')) AS x, users u WHERE u.id = x.v ORDER BY 1"#,
        r#"SELECT id FROM users WHERE id IN (SELECT v FROM JSON_TABLE('[1,4]', '$[*]' COLUMNS (v INT PATH '$')) AS jt) ORDER BY 1"#,
        r#"SELECT id FROM users WHERE id NOT IN (SELECT v FROM JSON_TABLE('[1,4]', '$[*]' COLUMNS (v INT PATH '$')) AS jt) ORDER BY 1"#,
        r#"SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM JSON_TABLE('[1,4]', '$[*]' COLUMNS (v INT PATH '$')) AS jt WHERE jt.v = u.id) ORDER BY 1"#,
        r#"SELECT id, (SELECT COUNT(*) FROM JSON_TABLE('[1,4,4]', '$[*]' COLUMNS (v INT PATH '$')) AS jt WHERE jt.v = u.id) FROM users u ORDER BY 1"#,
        r#"SELECT (SELECT MAX(v) FROM JSON_TABLE('[1,4]', '$[*]' COLUMNS (v INT PATH '$')) AS jt)"#,
        // A text column is utf8mb4_0900_ai_ci, as any declared VARCHAR is.
        r#"SELECT v FROM JSON_TABLE('["a","A","b"]', '$[*]' COLUMNS (v VARCHAR(5) PATH '$')) AS jt WHERE v = 'A'"#,
        r#"SELECT LOWER(v), COUNT(*) FROM JSON_TABLE('["a","A","b"]', '$[*]' COLUMNS (v VARCHAR(5) PATH '$')) AS jt GROUP BY v ORDER BY 1"#,
        r#"SELECT COUNT(DISTINCT v) FROM JSON_TABLE('["a","A","b"]', '$[*]' COLUMNS (v TEXT PATH '$')) AS jt"#,
        r#"SELECT o, ROW_NUMBER() OVER (ORDER BY v DESC) FROM JSON_TABLE('[3,1,2]', '$[*]' COLUMNS (o FOR ORDINALITY, v INT PATH '$')) AS jt ORDER BY o"#,
        r#"SELECT SUM(d), AVG(d) FROM JSON_TABLE('[1.25, 2.5, "3.75"]', '$[*]' COLUMNS (d DECIMAL(6,2) PATH '$')) AS jt"#,
    ] {
        ordered(SHAPES, sql);
    }

    // The document read from a column of a relation written before the
    // table function: computed once per row of what precedes it.
    for sql in [
        r#"SELECT o.id, jt.pos, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (pos FOR ORDINALITY, item INT PATH '$')) AS jt ORDER BY o.id, jt.pos"#,
        r#"SELECT o.id, jt.tag FROM orders o, JSON_TABLE(o.meta, '$.tags[*]' COLUMNS (tag VARCHAR(16) PATH '$')) AS jt ORDER BY o.id, jt.tag"#,
        r#"SELECT o.id, jt.* FROM orders o JOIN JSON_TABLE(o.meta, '$' COLUMNS (score DECIMAL(4,2) PATH '$.score', first_tag VARCHAR(16) PATH '$.tags[0]', n JSON PATH '$.items')) AS jt ORDER BY o.id"#,
        r#"SELECT o.id, jt.pos, jt.item FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (pos FOR ORDINALITY, item INT PATH '$')) AS jt ON TRUE ORDER BY o.id, jt.pos"#,
        r#"SELECT o.id, jt.pos, jt.item FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (pos FOR ORDINALITY, item INT PATH '$')) AS jt ON jt.item > 3 ORDER BY o.id, jt.pos"#,
        r#"SELECT o.id, jt.pos, jt.item FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (pos FOR ORDINALITY, item INT PATH '$')) AS jt ON jt.item = o.user_id ORDER BY o.id, jt.pos"#,
        r#"SELECT o.id, jt.item FROM orders o JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON jt.item = o.user_id ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.id = 5 ORDER BY jt.item"#,
        r#"SELECT o.id, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.status = 'shipped' AND jt.item >= 3 ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, COUNT(jt.item), SUM(jt.item) FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON TRUE GROUP BY o.id ORDER BY o.id"#,
        r#"SELECT jt.tag, COUNT(*), SUM(o.total) FROM orders o, JSON_TABLE(o.meta, '$.tags[*]' COLUMNS (tag VARCHAR(16) PATH '$')) AS jt GROUP BY jt.tag ORDER BY jt.tag"#,
        r#"SELECT u.name, o.id, jt.item FROM users u JOIN orders o ON o.user_id = u.id JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ORDER BY o.id, jt.item"#,
        r#"SELECT u.id, o.id, jt.item FROM users u, orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.user_id = u.id AND jt.item > 4 ORDER BY o.id, jt.item"#,
        r#"SELECT u.id, o.id, jt.item FROM users u JOIN orders o ON o.id = 1 LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON jt.item = u.id ORDER BY u.id, jt.item"#,
        r#"SELECT o.id, u.id, jt.item FROM orders o, users u LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON jt.item = u.id WHERE o.id = 1 ORDER BY u.id, jt.item"#,
        r#"SELECT o.id, u.id, jt.item FROM orders o, users u JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON jt.item = u.id WHERE o.id IN (1, 5) ORDER BY o.id, u.id"#,
        r#"SELECT o.id, jt.item, u.name FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON TRUE LEFT JOIN users u ON u.id = jt.item ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, jt.item, u.name FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt, users u WHERE u.id = jt.item AND o.id < 6 ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, a.pos, b.w FROM orders o JOIN JSON_TABLE(o.meta, '$' COLUMNS (pos FOR ORDINALITY, j JSON PATH '$.items')) AS a JOIN JSON_TABLE(a.j, '$[*]' COLUMNS (w INT PATH '$')) AS b ORDER BY 1, 2, 3"#,
        r#"SELECT a.o, a.j, b.w FROM JSON_TABLE('[[1,2],[3]]', '$[*]' COLUMNS (o FOR ORDINALITY, j JSON PATH '$')) AS a, JSON_TABLE(a.j, '$[*]' COLUMNS (w INT PATH '$')) AS b ORDER BY 1, 3"#,
        r#"SELECT o.id, jt.tag, jt.item FROM orders o, JSON_TABLE(o.meta, '$' COLUMNS (NESTED PATH '$.tags[*]' COLUMNS (tag VARCHAR(16) PATH '$'), NESTED PATH '$.items[*]' COLUMNS (item INT PATH '$'))) AS jt WHERE o.id IN (1, 4, 5) ORDER BY o.id, jt.tag, jt.item"#,
        r#"SELECT o.id, jt.item FROM orders o, JSON_TABLE(JSON_EXTRACT(o.meta, '$.items'), '$[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.id <= 2 ORDER BY o.id, jt.item"#,
        r#"SELECT e.id, jt.v FROM events e, JSON_TABLE(CONCAT('["', e.note, '", "', e.name, '"]'), '$[*]' COLUMNS (v VARCHAR(16) PATH '$')) AS jt ORDER BY e.id, jt.v"#,
        r#"SELECT e.id, jt.v FROM events e LEFT JOIN JSON_TABLE(CONCAT('[', e.score, ']'), '$[*]' COLUMNS (v INT PATH '$')) AS jt ON jt.v > 50 ORDER BY e.id"#,
        r#"SELECT o.id, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ORDER BY jt.item DESC, o.id LIMIT 5"#,
        r#"SELECT COUNT(*) FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt"#,
        r#"SELECT COUNT(*) FROM orders o LEFT JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt ON TRUE"#,
        r#"SELECT d.id, d.item FROM (SELECT o.id, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt) AS d WHERE d.item = 1 ORDER BY d.id"#,
        r#"WITH x AS (SELECT o.user_id, jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt) SELECT user_id, SUM(item) FROM x GROUP BY user_id ORDER BY user_id"#,
        r#"SELECT u.id FROM users u WHERE u.id IN (SELECT jt.item FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt) ORDER BY u.id"#,
        r#"SELECT o.id, jt.item, ROW_NUMBER() OVER (PARTITION BY o.id ORDER BY jt.item DESC) FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.id IN (1, 6) ORDER BY o.id, jt.item"#,
    ] {
        ordered(LATERAL, sql);
    }

    // Beside a LATERAL derived table, on either side of it: each reads
    // what is written before it, the other included.
    for sql in [
        r#"SELECT o.id, jt.item, d.n FROM orders o, JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt, LATERAL (SELECT COUNT(*) AS n FROM users u WHERE u.id <= jt.item) AS d WHERE o.id IN (1, 6) ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, d.doc, jt.item FROM orders o, LATERAL (SELECT JSON_EXTRACT(o.meta, '$.items') AS doc) AS d, JSON_TABLE(d.doc, '$[*]' COLUMNS (item INT PATH '$')) AS jt WHERE o.id <= 2 ORDER BY o.id, jt.item"#,
        r#"SELECT o.id, jt.item, d.name FROM orders o JOIN JSON_TABLE(o.meta, '$.items[*]' COLUMNS (item INT PATH '$')) AS jt LEFT JOIN LATERAL (SELECT u.name FROM users u WHERE u.id = jt.item + 3) AS d ON TRUE WHERE o.id = 12 ORDER BY jt.item"#,
        r#"SELECT u.id, jt.tag FROM users u LEFT JOIN LATERAL (SELECT o.meta FROM orders o WHERE o.user_id = u.id ORDER BY o.id LIMIT 1) AS d ON TRUE LEFT JOIN JSON_TABLE(d.meta, '$.tags[*]' COLUMNS (tag VARCHAR(16) PATH '$')) AS jt ON TRUE ORDER BY u.id, jt.tag"#,
        r#"SELECT u.id, d.n FROM users u, LATERAL (SELECT COUNT(*) AS n FROM JSON_TABLE('[1,4,4,9]', '$[*]' COLUMNS (v INT PATH '$')) AS jt WHERE jt.v = u.id) AS d ORDER BY u.id"#,
    ] {
        ordered(COMPOSED, sql);
    }
    cases
}

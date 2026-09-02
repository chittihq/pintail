# MySQL's regression suite against Pintail

Measured 2026-09-02T09:36:42.067Z: `mysql-test/t` from mysql/mysql-server at `8.4`, oracle MySQL 8.4.11, 114 files.

**1,902 of 2,360 compared SELECTs match MySQL byte-for-byte** (80.6%). 394 differ in rows, 64 in column names only. 1,407 SELECTs Pintail could not run, 1,449 were not compared because their tables were changed by statements a local database cannot follow, 38 failed on MySQL itself. Fixtures: 4,644 accepted, 360 rejected by Pintail, 1,780 outside the replayed subset.

Column names are compared with rows. Row order is compared when the outer query has ORDER BY and the test did not ask for sorted results; otherwise rows are compared as multisets.

| File | Statements | Exact | Mismatch | Names | Pintail error | Tainted | MySQL error | Setup ok | Setup rejected | Unsupported | Session | Skipped |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| case | 140 | 23 | 7 | 0 | 3 | 15 | 0 | 47 | 5 | 13 | 5 | 22 |
| derived | 40 | 15 | 0 | 0 | 2 | 0 | 0 | 12 | 0 | 1 | 0 | 10 |
| derived_ci | 10 | 2 | 0 | 0 | 0 | 0 | 5 | 0 | 0 | 0 | 0 | 3 |
| derived_condition_pushdown | 256 | 15 | 0 | 0 | 6 | 9 | 0 | 79 | 1 | 38 | 31 | 77 |
| derived_correlated | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 1 | 2 | 1 |
| derived_correlated_hypergraph | 8 | 0 | 0 | 0 | 1 | 0 | 0 | 5 | 0 | 0 | 0 | 2 |
| derived_cs | 10 | 2 | 0 | 0 | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 5 |
| derived_limit | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 3 | 0 | 0 | 0 | 1 |
| distinct | 69 | 2 | 0 | 0 | 0 | 22 | 0 | 13 | 2 | 22 | 0 | 8 |
| distinct_innodb | 16 | 0 | 0 | 0 | 2 | 0 | 0 | 6 | 1 | 2 | 0 | 5 |
| func_at_time_zone | 22 | 0 | 1 | 0 | 5 | 0 | 0 | 5 | 0 | 2 | 3 | 6 |
| func_bitwise_ops | 387 | 6 | 2 | 0 | 5 | 264 | 0 | 19 | 6 | 20 | 0 | 65 |
| func_comparison | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| func_concat | 48 | 9 | 1 | 0 | 7 | 1 | 0 | 14 | 2 | 4 | 0 | 10 |
| func_date_add | 62 | 5 | 0 | 0 | 20 | 1 | 0 | 5 | 1 | 5 | 2 | 23 |
| func_default | 13 | 0 | 0 | 0 | 0 | 2 | 0 | 5 | 1 | 1 | 0 | 4 |
| func_equal | 29 | 12 | 1 | 0 | 0 | 0 | 0 | 12 | 0 | 1 | 0 | 3 |
| func_gconcat | 261 | 53 | 8 | 0 | 28 | 22 | 0 | 109 | 9 | 8 | 10 | 14 |
| func_group | 725 | 144 | 17 | 0 | 31 | 61 | 3 | 257 | 17 | 50 | 12 | 133 |
| func_if | 99 | 20 | 6 | 1 | 11 | 1 | 0 | 41 | 2 | 3 | 0 | 14 |
| func_in_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| func_in_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| func_isnull | 35 | 2 | 0 | 0 | 4 | 2 | 0 | 19 | 2 | 3 | 0 | 3 |
| func_like | 175 | 58 | 1 | 0 | 23 | 3 | 0 | 46 | 2 | 6 | 12 | 24 |
| func_math | 568 | 109 | 30 | 0 | 51 | 52 | 1 | 100 | 10 | 51 | 20 | 144 |
| func_op | 18 | 8 | 0 | 0 | 2 | 0 | 0 | 6 | 0 | 0 | 0 | 2 |
| func_regexp | 90 | 5 | 0 | 4 | 26 | 0 | 0 | 16 | 0 | 4 | 1 | 34 |
| func_sapdb | 88 | 21 | 9 | 0 | 47 | 3 | 0 | 4 | 0 | 3 | 0 | 1 |
| func_set | 77 | 16 | 3 | 0 | 23 | 0 | 2 | 25 | 0 | 1 | 0 | 7 |
| func_str | 834 | 249 | 24 | 9 | 258 | 17 | 0 | 140 | 6 | 23 | 11 | 97 |
| func_test | 221 | 61 | 32 | 3 | 32 | 3 | 0 | 43 | 3 | 8 | 8 | 28 |
| func_time | 599 | 167 | 27 | 0 | 192 | 9 | 1 | 98 | 2 | 24 | 33 | 46 |
| func_timestamp | 7 | 0 | 1 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 2 | 0 |
| func_unixtime | 31 | 0 | 20 | 0 | 4 | 0 | 0 | 0 | 0 | 0 | 7 | 0 |
| func_weight_string | 84 | 1 | 0 | 0 | 17 | 7 | 0 | 16 | 1 | 11 | 5 | 26 |
| group_by | 1235 | 119 | 7 | 0 | 85 | 104 | 14 | 443 | 23 | 119 | 25 | 296 |
| group_by_fd_no_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_fd_ps_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_hypergraph | 76 | 3 | 0 | 0 | 6 | 0 | 0 | 13 | 0 | 3 | 0 | 51 |
| having | 324 | 51 | 1 | 0 | 24 | 19 | 7 | 152 | 5 | 23 | 7 | 35 |
| having_myisam | 19 | 1 | 0 | 0 | 2 | 0 | 0 | 13 | 0 | 1 | 0 | 2 |
| limit | 177 | 48 | 7 | 0 | 0 | 12 | 0 | 40 | 3 | 32 | 2 | 33 |
| limit_myisam | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 3 | 0 | 0 | 0 | 1 |
| negation_elimination | 70 | 33 | 1 | 0 | 0 | 1 | 0 | 4 | 0 | 1 | 0 | 30 |
| null | 133 | 19 | 1 | 0 | 3 | 5 | 0 | 27 | 5 | 33 | 6 | 34 |
| null_key_all_innodb | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| null_key_all_myisam | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| null_key_icp_innodb | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| null_key_icp_myisam | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| null_key_none_innodb | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| null_key_none_myisam | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| order_by_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| order_by_icp_mrr | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| order_by_limit | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 1 |
| order_by_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| order_by_sortkey | 36 | 0 | 0 | 0 | 0 | 2 | 0 | 8 | 2 | 17 | 1 | 6 |
| round | 133 | 0 | 0 | 0 | 1 | 9 | 0 | 18 | 9 | 94 | 2 | 0 |
| select_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_all_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_all_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_count | 74 | 4 | 0 | 0 | 4 | 2 | 0 | 13 | 1 | 7 | 2 | 41 |
| select_distinct_debug | 15 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 7 | 2 | 3 |
| select_for_update | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 3 | 0 | 1 | 0 | 2 |
| select_found | 32 | 0 | 0 | 0 | 12 | 8 | 0 | 6 | 3 | 1 | 0 | 2 |
| select_icp_mrr | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| select_icp_mrr_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_icp_mrr_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| select_none_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_none_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_safe | 150 | 1 | 0 | 0 | 2 | 29 | 0 | 17 | 1 | 19 | 39 | 42 |
| subselect | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 | 0 | 3 |
| subselect_debug | 35 | 0 | 0 | 0 | 0 | 4 | 0 | 14 | 1 | 2 | 10 | 4 |
| subselect_gis | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 3 | 1 | 0 | 0 | 0 |
| subselect_innodb | 181 | 11 | 0 | 0 | 1 | 7 | 0 | 71 | 12 | 62 | 2 | 15 |
| type_binary | 109 | 1 | 6 | 0 | 13 | 27 | 1 | 27 | 11 | 14 | 0 | 9 |
| type_bit_innodb | 100 | 23 | 4 | 0 | 6 | 7 | 0 | 31 | 6 | 8 | 0 | 15 |
| type_bit_myisam | 263 | 43 | 4 | 0 | 8 | 30 | 0 | 106 | 17 | 34 | 0 | 21 |
| type_blob | 410 | 6 | 0 | 0 | 17 | 97 | 1 | 81 | 17 | 49 | 6 | 136 |
| type_blob_myisam | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 | 1 | 1 |
| type_date | 88 | 7 | 3 | 0 | 8 | 4 | 2 | 55 | 0 | 7 | 2 | 0 |
| type_datetime | 76 | 3 | 3 | 0 | 5 | 8 | 0 | 20 | 2 | 22 | 4 | 9 |
| type_datetime_myisam | 13 | 0 | 0 | 0 | 0 | 3 | 0 | 3 | 2 | 3 | 1 | 1 |
| type_decimal | 370 | 21 | 11 | 0 | 4 | 23 | 0 | 103 | 17 | 171 | 5 | 15 |
| type_enum | 151 | 3 | 5 | 0 | 6 | 11 | 0 | 65 | 14 | 14 | 10 | 23 |
| type_float | 262 | 14 | 12 | 0 | 2 | 23 | 0 | 103 | 13 | 61 | 2 | 32 |
| type_float_myisam | 8 | 4 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 0 | 0 |
| type_nchar | 22 | 0 | 0 | 0 | 0 | 0 | 0 | 10 | 5 | 0 | 0 | 7 |
| type_newdecimal | 349 | 138 | 27 | 0 | 19 | 13 | 0 | 85 | 11 | 22 | 8 | 26 |
| type_newdecimal-big | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 3 |
| type_newdecimal_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 2 | 0 | 1 | 1 | 0 |
| type_ranges | 76 | 2 | 0 | 0 | 0 | 15 | 0 | 18 | 1 | 24 | 3 | 13 |
| type_set | 44 | 1 | 2 | 0 | 2 | 2 | 0 | 22 | 2 | 5 | 3 | 5 |
| type_set_myisam | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 2 | 0 | 0 | 1 |
| type_string | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| type_temporal_fractional | 1195 | 20 | 53 | 0 | 93 | 97 | 0 | 622 | 16 | 190 | 11 | 93 |
| type_time | 134 | 30 | 21 | 0 | 3 | 7 | 0 | 53 | 4 | 10 | 4 | 2 |
| type_timestamp | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 1 | 0 | 3 | 0 |
| type_timestamp_explicit | 71 | 0 | 1 | 0 | 0 | 3 | 0 | 15 | 3 | 23 | 6 | 20 |
| type_timestamp_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 0 | 1 | 1 |
| type_uint | 8 | 1 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 2 | 1 | 0 |
| type_varchar | 39 | 2 | 2 | 0 | 0 | 0 | 0 | 31 | 0 | 1 | 0 | 3 |
| type_year | 151 | 4 | 4 | 0 | 9 | 40 | 0 | 45 | 9 | 14 | 2 | 24 |
| union | 931 | 76 | 15 | 1 | 64 | 57 | 0 | 293 | 22 | 179 | 14 | 210 |
| union_myisam | 30 | 0 | 0 | 0 | 0 | 10 | 0 | 4 | 4 | 10 | 0 | 2 |
| varbinary | 159 | 5 | 1 | 2 | 42 | 43 | 0 | 15 | 4 | 10 | 1 | 36 |
| window_functions | 1665 | 177 | 13 | 41 | 134 | 216 | 1 | 577 | 21 | 118 | 32 | 335 |
| window_functions_big | 35 | 0 | 0 | 0 | 0 | 3 | 0 | 5 | 3 | 11 | 12 | 1 |
| window_functions_bugs | 217 | 25 | 0 | 3 | 19 | 9 | 0 | 108 | 4 | 5 | 8 | 36 |
| window_functions_explain | 502 | 1 | 0 | 0 | 1 | 1 | 0 | 116 | 6 | 37 | 11 | 329 |
| window_functions_in2exists | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_in2exists_hypergraph | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_interesting_orders | 31 | 0 | 0 | 0 | 0 | 0 | 0 | 6 | 0 | 6 | 0 | 19 |
| window_functions_myisam | 15 | 0 | 0 | 0 | 9 | 0 | 0 | 4 | 0 | 1 | 0 | 1 |

† the parser dropped trailing lines it could not delimit.

## What Pintail could not run, by message shape

| Count | Message |
|---:|---|
| 152 | Error: numeric expression overflow |
| 108 | DDL: Error: column option DEFAULT _ is not supported on a local table |
| 100 | INSERT: Error: only literal values are supported in INSERT, got _ |
| 79 | Error: query engine failed: invalid MySQL date/time value |
| 53 | Error: sql parser error: INTERVAL requires a unit after the literal value at Line: _, Colu |
| 47 | Error: query engine failed: bound expression has an invalid physical type |
| 33 | INSERT: Error: value _ is not supported in INSERT |
| 33 | Error: sql parser error: Expected: end of statement, found: WITH at Line: _, Column: _ |
| 30 | Error: query engine failed: binary value is not valid UTF-_ for numeric coercion |
| 24 | Error: unsupported expression: found_rows() |
| 21 | INSERT: Error: column _ is AUTO_INCREMENT; a local table needs its value supplied |
| 19 | Error: HAVING requires GROUP BY or an aggregate |
| 16 | Error: unsupported expression: COLLATE latin1_bin is unsupported; supported: utf8mb4_0900_ |
| 14 | Error: unsupported expression: ADDTIME(TIME _, TIME _) |
| 14 | Error: unsupported expression: RAND(_) |
| 14 | Error: unsupported query clause: SELECT SQL_CALC_FOUND_ROWS * FROM t1__5 |
| 13 | Error: unsupported expression: TIMEDIFF(TIME _, TIME _) |
| 13 | Error: unsupported expression: SUBTIME(TIME _, TIME _) |
| 12 | Error: unknown column @g1 |
| 11 | Error: unknown column @a |
| 11 | Error: unsupported expression: pi() |
| 11 | Error: unsupported expression: FOUND_ROWS() |
| 10 | Error: query engine failed: invalid physical plan: unresolved subquery reached expression  |
| 10 | Error: UNION ALL column _ has types Some(Utf8) and Some(Int64) |
| 10 | Error: unsupported expression: f(_) |

Per-file diffs for mismatches are written to `tests/mtr/diffs/` (not committed).

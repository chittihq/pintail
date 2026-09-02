# MySQL's regression suite against Pintail

Measured 2026-09-02T11:10:45.462Z: `mysql-test/t` from mysql/mysql-server at `8.4`, oracle MySQL 8.4.11, 114 files.

**2,052 of 2,582 compared SELECTs match MySQL byte-for-byte** (79.5%). 457 differ in rows, 73 in column names only. 1,167 SELECTs Pintail could not run, 1,467 were not compared because their tables were changed by statements a local database cannot follow, 38 failed on MySQL itself. Fixtures: 4,608 accepted, 378 rejected by Pintail, 1,798 outside the replayed subset.

Column names are compared with rows. Row order is compared when the outer query has ORDER BY and the test did not ask for sorted results; otherwise rows are compared as multisets.

| File | Statements | Exact | Mismatch | Names | Pintail error | Tainted | MySQL error | Setup ok | Setup rejected | Unsupported | Session | Skipped |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| case | 140 | 24 | 7 | 0 | 2 | 15 | 0 | 47 | 5 | 13 | 5 | 22 |
| derived | 40 | 17 | 0 | 0 | 0 | 0 | 0 | 12 | 0 | 1 | 0 | 10 |
| derived_ci | 10 | 2 | 0 | 0 | 0 | 0 | 5 | 0 | 0 | 0 | 0 | 3 |
| derived_condition_pushdown | 256 | 15 | 0 | 0 | 6 | 9 | 0 | 79 | 1 | 38 | 31 | 77 |
| derived_correlated | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 1 | 2 | 1 |
| derived_correlated_hypergraph | 8 | 0 | 0 | 0 | 1 | 0 | 0 | 5 | 0 | 0 | 0 | 2 |
| derived_cs | 10 | 2 | 0 | 0 | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 5 |
| derived_limit | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 3 | 0 | 0 | 0 | 1 |
| distinct | 69 | 2 | 0 | 0 | 0 | 22 | 0 | 13 | 2 | 22 | 0 | 8 |
| distinct_innodb | 16 | 0 | 0 | 0 | 2 | 0 | 0 | 6 | 1 | 2 | 0 | 5 |
| func_at_time_zone | 22 | 0 | 0 | 0 | 3 | 3 | 0 | 4 | 1 | 2 | 3 | 6 |
| func_bitwise_ops | 387 | 6 | 2 | 0 | 5 | 264 | 0 | 19 | 6 | 20 | 0 | 65 |
| func_comparison | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| func_concat | 48 | 9 | 1 | 0 | 7 | 1 | 0 | 14 | 2 | 4 | 0 | 10 |
| func_date_add | 62 | 10 | 1 | 0 | 14 | 1 | 0 | 5 | 1 | 5 | 2 | 23 |
| func_default | 13 | 0 | 0 | 0 | 0 | 2 | 0 | 5 | 1 | 1 | 0 | 4 |
| func_equal | 29 | 13 | 0 | 0 | 0 | 0 | 0 | 12 | 0 | 1 | 0 | 3 |
| func_gconcat | 261 | 53 | 8 | 0 | 28 | 22 | 0 | 109 | 9 | 8 | 10 | 14 |
| func_group | 725 | 145 | 18 | 0 | 29 | 61 | 3 | 257 | 17 | 50 | 12 | 133 |
| func_if | 99 | 20 | 6 | 1 | 11 | 1 | 0 | 41 | 2 | 3 | 0 | 14 |
| func_in_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| func_in_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| func_isnull | 35 | 2 | 0 | 0 | 4 | 2 | 0 | 19 | 2 | 3 | 0 | 3 |
| func_like | 175 | 60 | 1 | 0 | 21 | 3 | 0 | 46 | 2 | 6 | 12 | 24 |
| func_math | 568 | 122 | 31 | 0 | 37 | 52 | 1 | 100 | 10 | 51 | 20 | 144 |
| func_op | 18 | 8 | 0 | 0 | 2 | 0 | 0 | 6 | 0 | 0 | 0 | 2 |
| func_regexp | 90 | 5 | 1 | 5 | 24 | 0 | 0 | 16 | 0 | 4 | 1 | 34 |
| func_sapdb | 88 | 35 | 14 | 0 | 28 | 3 | 0 | 4 | 0 | 3 | 0 | 1 |
| func_set | 77 | 16 | 3 | 0 | 23 | 0 | 2 | 25 | 0 | 1 | 0 | 7 |
| func_str | 834 | 274 | 18 | 9 | 239 | 17 | 0 | 140 | 6 | 23 | 11 | 97 |
| func_test | 221 | 72 | 29 | 3 | 24 | 3 | 0 | 43 | 3 | 8 | 8 | 28 |
| func_time | 599 | 190 | 62 | 0 | 131 | 12 | 1 | 96 | 4 | 24 | 33 | 46 |
| func_timestamp | 7 | 0 | 1 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 2 | 0 |
| func_unixtime | 31 | 7 | 15 | 0 | 2 | 0 | 0 | 0 | 0 | 0 | 7 | 0 |
| func_weight_string | 84 | 1 | 0 | 0 | 17 | 7 | 0 | 16 | 1 | 11 | 5 | 26 |
| group_by | 1235 | 117 | 16 | 0 | 76 | 106 | 14 | 438 | 25 | 122 | 25 | 296 |
| group_by_fd_no_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_fd_ps_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_hypergraph | 76 | 3 | 0 | 0 | 6 | 0 | 0 | 13 | 0 | 3 | 0 | 51 |
| having | 324 | 59 | 1 | 0 | 16 | 19 | 7 | 152 | 5 | 23 | 7 | 35 |
| having_myisam | 19 | 2 | 0 | 0 | 1 | 0 | 0 | 13 | 0 | 1 | 0 | 2 |
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
| round | 133 | 1 | 0 | 0 | 0 | 9 | 0 | 18 | 9 | 94 | 2 | 0 |
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
| type_date | 88 | 8 | 2 | 0 | 7 | 5 | 2 | 54 | 1 | 7 | 2 | 0 |
| type_datetime | 76 | 3 | 0 | 0 | 5 | 11 | 0 | 16 | 5 | 23 | 4 | 9 |
| type_datetime_myisam | 13 | 0 | 0 | 0 | 0 | 3 | 0 | 3 | 2 | 3 | 1 | 1 |
| type_decimal | 370 | 21 | 11 | 0 | 4 | 23 | 0 | 103 | 17 | 171 | 5 | 15 |
| type_enum | 151 | 3 | 5 | 0 | 6 | 11 | 0 | 65 | 14 | 14 | 10 | 23 |
| type_float | 262 | 14 | 11 | 0 | 3 | 23 | 0 | 103 | 13 | 61 | 2 | 32 |
| type_float_myisam | 8 | 4 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 0 | 0 |
| type_nchar | 22 | 0 | 0 | 0 | 0 | 0 | 0 | 10 | 5 | 0 | 0 | 7 |
| type_newdecimal | 349 | 138 | 27 | 0 | 19 | 13 | 0 | 85 | 11 | 22 | 8 | 26 |
| type_newdecimal-big | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 3 |
| type_newdecimal_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 2 | 0 | 1 | 1 | 0 |
| type_ranges | 76 | 2 | 0 | 0 | 0 | 15 | 0 | 18 | 1 | 24 | 3 | 13 |
| type_set | 44 | 1 | 2 | 0 | 2 | 2 | 0 | 22 | 2 | 5 | 3 | 5 |
| type_set_myisam | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 2 | 0 | 0 | 1 |
| type_string | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| type_temporal_fractional | 1195 | 45 | 92 | 0 | 27 | 99 | 0 | 606 | 18 | 204 | 11 | 93 |
| type_time | 134 | 33 | 16 | 0 | 3 | 9 | 0 | 51 | 6 | 10 | 4 | 2 |
| type_timestamp | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 1 | 0 | 3 | 0 |
| type_timestamp_explicit | 71 | 0 | 0 | 0 | 0 | 4 | 0 | 14 | 4 | 23 | 6 | 20 |
| type_timestamp_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 0 | 1 | 1 |
| type_uint | 8 | 1 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 2 | 1 | 0 |
| type_varchar | 39 | 2 | 2 | 0 | 0 | 0 | 0 | 31 | 0 | 1 | 0 | 3 |
| type_year | 151 | 5 | 5 | 0 | 7 | 40 | 0 | 45 | 9 | 14 | 2 | 24 |
| union | 931 | 77 | 15 | 1 | 63 | 57 | 0 | 293 | 22 | 179 | 14 | 210 |
| union_myisam | 30 | 0 | 0 | 0 | 0 | 10 | 0 | 4 | 4 | 10 | 0 | 2 |
| varbinary | 159 | 7 | 7 | 2 | 34 | 43 | 0 | 15 | 4 | 10 | 1 | 36 |
| window_functions | 1665 | 180 | 4 | 49 | 131 | 217 | 1 | 573 | 25 | 118 | 32 | 335 |
| window_functions_big | 35 | 0 | 0 | 0 | 0 | 3 | 0 | 5 | 3 | 11 | 12 | 1 |
| window_functions_bugs | 217 | 26 | 0 | 3 | 18 | 9 | 0 | 108 | 4 | 5 | 8 | 36 |
| window_functions_explain | 502 | 1 | 0 | 0 | 1 | 1 | 0 | 116 | 6 | 37 | 11 | 329 |
| window_functions_in2exists | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_in2exists_hypergraph | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_interesting_orders | 31 | 0 | 0 | 0 | 0 | 0 | 0 | 6 | 0 | 6 | 0 | 19 |
| window_functions_myisam | 15 | 0 | 0 | 0 | 9 | 0 | 0 | 4 | 0 | 1 | 0 | 1 |

† the parser dropped trailing lines it could not delimit.

## What Pintail could not run, by message shape

| Count | Message |
|---:|---|
| 144 | Error: numeric expression overflow |
| 108 | DDL: Error: column option DEFAULT _ is not supported on a local table |
| 97 | INSERT: Error: only literal values are supported in INSERT, got _ |
| 53 | Error: sql parser error: INTERVAL requires a unit after the literal value at Line: _, Colu |
| 49 | Error: query engine failed: bound expression has an invalid physical type |
| 33 | INSERT: Error: value _ is not supported in INSERT |
| 33 | Error: sql parser error: Expected: end of statement, found: WITH at Line: _, Column: _ |
| 30 | Error: query engine failed: binary value is not valid UTF-_ for numeric coercion |
| 24 | Error: unsupported expression: found_rows() |
| 21 | INSERT: Error: column _ is AUTO_INCREMENT; a local table needs its value supplied |
| 14 | Error: unsupported query clause: SELECT SQL_CALC_FOUND_ROWS * FROM t1__5 |
| 12 | Error: unknown column @g1 |
| 11 | Error: unknown column @a |
| 11 | Error: unsupported expression: FOUND_ROWS() |
| 10 | INSERT: Error: Incorrect value _ for column _: expected a datetime |
| 10 | Error: query engine failed: invalid physical plan: unresolved subquery reached expression  |
| 10 | Error: UNION ALL column _ has types Some(Utf8) and Some(Int64) |
| 10 | Error: unsupported expression: f(_) |
| 9 | INSERT: Error: Column _ cannot be null |
| 9 | Error: sql parser error: INTERVAL requires a unit after the literal value |
| 9 | Error: unsupported expression: format(_._, _, _) |
| 9 | Error: sql parser error: Expected: end of statement, found: _ at Line: _, Column: _ |
| 9 | DDL: Error: sql parser error: Expected: _ or _ after column definition, found: zerofill at Line |
| 9 | Error: unsupported expression: CURRENT_TIMESTAMP(_) |
| 8 | Error: unsupported expression: INTERVAL _ HOUR |

Per-file diffs for mismatches are written to `tests/mtr/diffs/` (not committed).

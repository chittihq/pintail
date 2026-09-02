# MySQL's regression suite against Pintail

Measured 2026-09-02T08:17:12.063Z: `mysql-test/t` from mysql/mysql-server at `8.4`, oracle MySQL 8.4.11, 114 files.

**344 of 762 compared SELECTs match MySQL byte-for-byte** (45.1%). 103 differ in rows, 315 in column names only. 1,155 SELECTs Pintail could not run, 3,265 were not compared because their tables were changed by statements a local database cannot follow, 72 failed on MySQL itself. Fixtures: 1,705 accepted, 1,438 rejected by Pintail, 3,641 outside the replayed subset.

Column names are compared with rows. Row order is compared when the outer query has ORDER BY and the test did not ask for sorted results; otherwise rows are compared as multisets.

| File | Statements | Exact | Mismatch | Names | Pintail error | Tainted | MySQL error | Setup ok | Setup rejected | Unsupported | Session | Skipped |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| case | 140 | 1 | 0 | 2 | 18 | 27 | 0 | 20 | 17 | 28 | 5 | 22 |
| derived | 40 | 3 | 0 | 0 | 3 | 11 | 0 | 3 | 5 | 5 | 0 | 10 |
| derived_ci | 10 | 2 | 0 | 0 | 0 | 0 | 5 | 0 | 0 | 0 | 0 | 3 |
| derived_condition_pushdown | 256 | 3 | 0 | 0 | 2 | 25 | 0 | 30 | 32 | 56 | 31 | 77 |
| derived_correlated | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 2 | 2 | 1 |
| derived_correlated_hypergraph | 8 | 0 | 0 | 0 | 0 | 1 | 0 | 3 | 1 | 1 | 0 | 2 |
| derived_cs | 10 | 2 | 0 | 0 | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 5 |
| derived_limit | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 1 | 0 | 1 |
| distinct | 69 | 0 | 0 | 0 | 0 | 24 | 0 | 4 | 5 | 28 | 0 | 8 |
| distinct_innodb | 16 | 0 | 0 | 0 | 0 | 2 | 0 | 2 | 3 | 4 | 0 | 5 |
| func_at_time_zone | 22 | 0 | 0 | 0 | 3 | 3 | 0 | 2 | 2 | 3 | 3 | 6 |
| func_bitwise_ops | 387 | 0 | 0 | 0 | 5 | 272 | 0 | 8 | 8 | 29 | 0 | 65 |
| func_comparison | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| func_concat | 48 | 0 | 0 | 5 | 3 | 10 | 0 | 6 | 6 | 8 | 0 | 10 |
| func_date_add | 62 | 3 | 0 | 0 | 20 | 3 | 0 | 2 | 2 | 7 | 2 | 23 |
| func_default | 13 | 0 | 0 | 0 | 0 | 2 | 0 | 3 | 2 | 2 | 0 | 4 |
| func_equal | 29 | 0 | 0 | 3 | 3 | 7 | 0 | 4 | 4 | 5 | 0 | 3 |
| func_gconcat | 261 | 1 | 0 | 0 | 2 | 108 | 0 | 42 | 37 | 47 | 10 | 14 |
| func_group | 725 | 13 | 1 | 0 | 8 | 234 | 0 | 91 | 83 | 150 | 12 | 133 |
| func_if | 99 | 3 | 1 | 6 | 7 | 22 | 0 | 16 | 16 | 14 | 0 | 14 |
| func_in_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| func_in_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| func_isnull | 35 | 0 | 0 | 0 | 1 | 7 | 0 | 9 | 7 | 8 | 0 | 3 |
| func_like | 175 | 31 | 0 | 1 | 17 | 36 | 0 | 17 | 15 | 22 | 12 | 24 |
| func_math | 568 | 57 | 15 | 34 | 42 | 94 | 1 | 47 | 34 | 80 | 20 | 144 |
| func_op | 18 | 0 | 0 | 0 | 9 | 1 | 0 | 2 | 2 | 2 | 0 | 2 |
| func_regexp | 90 | 1 | 0 | 4 | 26 | 4 | 0 | 8 | 4 | 8 | 1 | 34 |
| func_sapdb | 88 | 0 | 3 | 12 | 62 | 3 | 0 | 2 | 1 | 4 | 0 | 1 |
| func_set | 77 | 0 | 3 | 2 | 20 | 19 | 0 | 11 | 8 | 7 | 0 | 7 |
| func_str | 834 | 60 | 12 | 107 | 319 | 59 | 0 | 65 | 40 | 64 | 11 | 97 |
| func_test | 221 | 29 | 8 | 6 | 70 | 18 | 0 | 21 | 14 | 19 | 8 | 28 |
| func_time | 599 | 24 | 7 | 45 | 172 | 86 | 62 | 41 | 19 | 64 | 33 | 46 |
| func_timestamp | 7 | 0 | 0 | 0 | 0 | 1 | 0 | 2 | 1 | 1 | 2 | 0 |
| func_unixtime | 31 | 0 | 18 | 0 | 6 | 0 | 0 | 0 | 0 | 0 | 7 | 0 |
| func_weight_string | 84 | 1 | 0 | 0 | 14 | 10 | 0 | 7 | 4 | 17 | 5 | 26 |
| group_by | 1235 | 5 | 0 | 0 | 6 | 317 | 1 | 185 | 146 | 254 | 25 | 296 |
| group_by_fd_no_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_fd_ps_prot | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| group_by_hypergraph | 76 | 0 | 0 | 0 | 0 | 9 | 0 | 5 | 5 | 6 | 0 | 51 |
| having | 324 | 6 | 0 | 0 | 1 | 93 | 2 | 54 | 50 | 76 | 7 | 35 |
| having_myisam | 19 | 0 | 0 | 0 | 0 | 3 | 0 | 5 | 4 | 5 | 0 | 2 |
| limit | 177 | 3 | 0 | 0 | 0 | 64 | 0 | 18 | 13 | 44 | 2 | 33 |
| limit_myisam | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 1 | 0 | 1 |
| negation_elimination | 70 | 0 | 0 | 0 | 0 | 35 | 0 | 2 | 1 | 2 | 0 | 30 |
| null | 133 | 4 | 1 | 3 | 8 | 12 | 0 | 15 | 11 | 39 | 6 | 34 |
| null_key_all_innodb | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| null_key_all_myisam | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| null_key_icp_innodb | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| null_key_icp_myisam | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| null_key_none_innodb | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| null_key_none_myisam | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| order_by_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| order_by_icp_mrr | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| order_by_limit | 3 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 1 |
| order_by_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| order_by_sortkey | 36 | 0 | 0 | 0 | 0 | 2 | 0 | 3 | 4 | 20 | 1 | 6 |
| round | 133 | 0 | 0 | 0 | 1 | 9 | 0 | 10 | 9 | 102 | 2 | 0 |
| select_all | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_all_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_all_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_count | 74 | 0 | 0 | 0 | 0 | 10 | 0 | 6 | 6 | 9 | 2 | 41 |
| select_distinct_debug | 15 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 7 | 2 | 3 |
| select_for_update | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 2 | 0 | 2 |
| select_found | 32 | 0 | 0 | 0 | 12 | 8 | 0 | 5 | 3 | 2 | 0 | 2 |
| select_icp_mrr | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| select_icp_mrr_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_icp_mrr_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_none | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| select_none_bka | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_none_bka_nobnl | 2 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 |
| select_safe | 150 | 1 | 0 | 0 | 1 | 30 | 0 | 8 | 5 | 24 | 39 | 42 |
| subselect | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 3 |
| subselect_debug | 35 | 0 | 0 | 0 | 0 | 4 | 0 | 5 | 5 | 7 | 10 | 4 |
| subselect_gis | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 2 | 1 | 1 | 0 | 0 |
| subselect_innodb | 181 | 1 | 0 | 0 | 0 | 18 | 0 | 29 | 28 | 88 | 2 | 15 |
| type_binary | 109 | 0 | 1 | 0 | 18 | 28 | 1 | 16 | 14 | 22 | 0 | 9 |
| type_bit_innodb | 100 | 0 | 0 | 0 | 12 | 28 | 0 | 14 | 14 | 17 | 0 | 15 |
| type_bit_myisam | 263 | 0 | 0 | 0 | 13 | 72 | 0 | 42 | 44 | 71 | 0 | 21 |
| type_blob | 410 | 0 | 0 | 0 | 15 | 106 | 0 | 38 | 39 | 70 | 6 | 136 |
| type_blob_myisam | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 0 | 0 | 1 | 1 |
| type_date | 88 | 0 | 0 | 0 | 6 | 18 | 0 | 14 | 16 | 32 | 2 | 0 |
| type_datetime | 76 | 0 | 0 | 0 | 5 | 14 | 0 | 9 | 8 | 27 | 4 | 9 |
| type_datetime_myisam | 13 | 0 | 0 | 0 | 0 | 3 | 0 | 2 | 2 | 4 | 1 | 1 |
| type_decimal | 370 | 1 | 1 | 0 | 4 | 53 | 0 | 35 | 43 | 213 | 5 | 15 |
| type_enum | 151 | 1 | 1 | 0 | 0 | 23 | 0 | 44 | 25 | 24 | 10 | 23 |
| type_float | 262 | 3 | 5 | 0 | 3 | 40 | 0 | 46 | 45 | 86 | 2 | 32 |
| type_float_myisam | 8 | 4 | 0 | 0 | 0 | 0 | 0 | 4 | 0 | 0 | 0 | 0 |
| type_nchar | 22 | 0 | 0 | 0 | 0 | 0 | 0 | 8 | 7 | 0 | 0 | 7 |
| type_newdecimal | 349 | 31 | 3 | 72 | 21 | 70 | 0 | 31 | 26 | 61 | 8 | 26 |
| type_newdecimal-big | 4 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 3 |
| type_newdecimal_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 1 | 1 | 0 |
| type_ranges | 76 | 2 | 0 | 0 | 0 | 15 | 0 | 13 | 5 | 25 | 3 | 13 |
| type_set | 44 | 0 | 0 | 0 | 0 | 7 | 0 | 9 | 9 | 11 | 3 | 5 |
| type_set_myisam | 5 | 0 | 0 | 0 | 0 | 0 | 0 | 2 | 2 | 0 | 0 | 1 |
| type_string | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| type_temporal_fractional | 1195 | 3 | 16 | 0 | 66 | 178 | 0 | 120 | 86 | 622 | 11 | 93 |
| type_time | 134 | 11 | 4 | 4 | 16 | 26 | 0 | 18 | 19 | 30 | 4 | 2 |
| type_timestamp | 6 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 2 | 0 | 3 | 0 |
| type_timestamp_explicit | 71 | 0 | 1 | 0 | 0 | 3 | 0 | 9 | 8 | 24 | 6 | 20 |
| type_timestamp_myisam | 5 | 0 | 0 | 0 | 0 | 1 | 0 | 1 | 1 | 0 | 1 | 1 |
| type_uint | 8 | 0 | 0 | 0 | 0 | 1 | 0 | 2 | 1 | 3 | 1 | 0 |
| type_varchar | 39 | 0 | 0 | 0 | 0 | 4 | 0 | 4 | 4 | 24 | 0 | 3 |
| type_year | 151 | 0 | 0 | 0 | 5 | 52 | 0 | 20 | 20 | 28 | 2 | 24 |
| union | 931 | 9 | 2 | 0 | 38 | 164 | 0 | 158 | 93 | 243 | 14 | 210 |
| union_myisam | 30 | 0 | 0 | 0 | 0 | 10 | 0 | 3 | 5 | 10 | 0 | 2 |
| varbinary | 159 | 0 | 0 | 1 | 49 | 43 | 0 | 10 | 6 | 13 | 1 | 36 |
| window_functions | 1665 | 24 | 0 | 8 | 18 | 532 | 0 | 137 | 143 | 436 | 32 | 335 |
| window_functions_big | 35 | 0 | 0 | 0 | 0 | 3 | 0 | 2 | 5 | 12 | 12 | 1 |
| window_functions_bugs | 217 | 1 | 0 | 0 | 2 | 53 | 0 | 36 | 38 | 43 | 8 | 36 |
| window_functions_explain | 502 | 0 | 0 | 0 | 0 | 3 | 0 | 24 | 34 | 101 | 11 | 329 |
| window_functions_in2exists | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_in2exists_hypergraph | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| window_functions_interesting_orders | 31 | 0 | 0 | 0 | 0 | 0 | 0 | 4 | 1 | 7 | 0 | 19 |
| window_functions_myisam | 15 | 0 | 0 | 0 | 0 | 9 | 0 | 1 | 1 | 3 | 0 | 1 |

† the parser dropped trailing lines it could not delimit.

## What Pintail could not run, by message shape

| Count | Message |
|---:|---|
| 1017 | DDL: Error: table _ needs a PRIMARY KEY; a local table without one has no row identity |
| 169 | Error: unsupported literal: _ |
| 89 | DDL: Error: column option AUTO_INCREMENT is not supported on a local table |
| 81 | DDL: Error: column option DEFAULT _ is not supported on a local table |
| 75 | Error: numeric literal is out of range: _ |
| 58 | Error: unsupported expression: TIME _ |
| 56 | Error: numeric expression overflow |
| 42 | DDL: Error: column option DEFAULT NULL is not supported on a local table |
| 41 | Error: unsupported literal: X_ |
| 36 | Error: sql parser error: INTERVAL requires a unit after the literal value at Line: _, Colu |
| 30 | Error: query engine failed: invalid MySQL date/time value |
| 29 | Error: unsupported expression: _latin2 _ |
| 26 | Error: unsupported literal: B_ |
| 24 | Error: unsupported expression: found_rows() |
| 20 | Error: unknown table mtr_func_str.DUAL |
| 19 | DDL: Error: table constraint KEY (a) is not supported on a local table |
| 19 | Error: unsupported expression: _latin1 _ |
| 17 | Error: unsupported expression: DATE _ |
| 13 | Error: unsupported expression: insert(_, _, _, _) |
| 12 | Error: operator \| does not accept Some(Int64) and Some(Int64) |
| 12 | Error: unsupported expression: TIME(_) |
| 12 | Error: unknown column @g1 |
| 11 | DDL: Error: column option CHARACTER SET utf8mb3 is not supported on a local table |
| 11 | Error: unsupported expression: FOUND_ROWS() |
| 10 | Error: unsupported expression: pi() |

Per-file diffs for mismatches are written to `tests/mtr/diffs/` (not committed).

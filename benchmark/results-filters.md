# Filter-level benchmark: Pintail against MySQL

Measured 2026-09-10T10:18:39.068Z at `47cfd1e5` with 20,000,000 rows.

Both engines run on the docker host under identical limits (8 CPUs, 8 GB). MySQL 8.4
has a 4 GB buffer pool and secondary indexes on `created_at`, `scheduled_at`,
`account_id` and `state`, the way a production schema carries them; Pintail replicates
the same table over CDC and runs every statement (its settled-answer memo is off).

`created_at` rises with the primary key, as it does in a table an application appends
to; `scheduled_at` is scattered across the same span. After the snapshot the source
updated one row in a hundred, deleted one in a thousand and appended
100,000 rows, and the replica converged before timing began, so
Pintail answers from the layered state a replica keeps while its source is written to.

Each filter runs 5 distinct constants; each engine answers each statement twice
and the second is timed, over the MySQL wire protocol from the same client host. Every
answer is compared row for row.

| Filter | Rows back | MySQL access | MySQL median | MySQL min | Pintail median | Pintail min | Pintail vs MySQL | Exact |
|---|---:|---|---:|---:|---:|---:|---:|:--|
| created_at, one hour: count | 1 | idx_activity_created (range) | 1.0 ms | 0.9 ms | 466 ms | 460 ms | 0.00× | yes |
| created_at, one day: count and sum | 1 | idx_activity_created (range) | 50 ms | 49 ms | 872 ms | 864 ms | 0.06× | yes |
| created_at, one day: newest 50 rows | 50 | idx_activity_created (range) | 1.3 ms | 1.0 ms | 1,272 ms | 1,264 ms | 0.00× | yes |
| created_at, one day: every row, three columns | 43,156 | idx_activity_created (range) | 70 ms | 66 ms | 1,030 ms | 1,015 ms | 0.07× | yes |
| created_at, thirty days: per state | 6 | idx_activity_created (range) | 1,888 ms | 1,875 ms | 3,913 ms | 3,857 ms | 0.48× | yes |
| created_at, one day, and a state | 1 | idx_activity_created (range) | 49 ms | 48 ms | 701 ms | 696 ms | 0.07× | yes |
| created_at from a moment: first 100 rows | 100 | idx_activity_created (range) | 1.2 ms | 1.0 ms | 31,401 ms | 10,973 ms | 0.00× | yes |
| scheduled_at (scattered), one day: count | 1 | idx_activity_scheduled (range) | 6.7 ms | 6.5 ms | 48,033 ms | 47,917 ms | 0.00× | yes |
| account_id point: every row | 100 | idx_activity_account (ref) | 1.0 ms | 0.7 ms | 66,490 ms | 29,027 ms | 0.00× | yes |
| account_id IN 100 ids: count | 1 | idx_activity_account (range) | 2.3 ms | 2.2 ms | 80,944 ms | 80,836 ms | 0.00× | yes |
| primary key point | 1 | PRIMARY (const) | 0.4 ms | 0.3 ms | 6.7 ms | 6.2 ms | 0.06× | yes |
| primary key range of 10,000: sum | 1 | PRIMARY (range) | 2.4 ms | 2.4 ms | 23 ms | 20 ms | 0.11× | yes |
| state = an updated value: count | 1 | idx_activity_state (ref) | 303 ms | 301 ms | 27,727 ms | 27,635 ms | 0.01× | yes |
| amount range, no index anywhere: count | 1 | full scan | 2,469 ms | 2,435 ms | 33,566 ms | 33,400 ms | 0.07× | yes |
| DATE(created_at) = a day: count | 1 | idx_activity_created (index) | 2,114 ms | 2,110 ms | 605 ms | 455 ms | 3.50× | yes |
| note LIKE a prefix, and a channel: count | 1 | full scan | 3,074 ms | 3,068 ms | 36,245 ms | 36,168 ms | 0.08× | yes |
| note IS NULL within a week: count | 1 | idx_activity_created (range) | 328 ms | 326 ms | 1,084 ms | 1,072 ms | 0.30× | yes |
| states, amount and a month together: count | 1 | idx_activity_created (range) | 1,518 ms | 1,505 ms | 4,212 ms | 4,187 ms | 0.36× | yes |

Every answer exact: yes. A ratio above 1 means Pintail answered faster.

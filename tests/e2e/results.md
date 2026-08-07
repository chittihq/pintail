# Pintail end-to-end differential gate

Measured 2026-08-07T14:28:27.762Z.

**441 passed, 0 failed, 3 documented-gap warnings.**

| Phase | Check | Status | Detail |
|---|---|---|---|
| snapshot | converge:audit_log | PASS |  |
| snapshot | converge:counters | PASS |  |
| snapshot | converge:customers | PASS |  |
| snapshot | converge:order_items | PASS |  |
| snapshot | converge:orders | PASS |  |
| snapshot | converge:information_schema.columns | PASS |  |
| snapshot | query:point lookup by key | PASS |  |
| snapshot | query:range scan with compound predicate | PASS |  |
| snapshot | query:inner join with aggregation | PASS |  |
| snapshot | query:left join preserves unmatched rows | PASS |  |
| snapshot | query:right join preserves unmatched rows | PASS |  |
| snapshot | query:three-way join through items | PASS |  |
| snapshot | query:union all across sources | PASS |  |
| snapshot | query:intersect customer identifiers | PASS |  |
| snapshot | query:except customer identifiers | PASS |  |
| snapshot | query:group by with having | PASS |  |
| snapshot | query:conditional decimal sum keeps the fraction | PASS |  |
| snapshot | query:distinct count and min max | PASS |  |
| snapshot | query:uncorrelated in-subquery | PASS |  |
| snapshot | query:correlated exists with inner predicate | PASS |  |
| snapshot | query:correlated scalar aggregate | PASS |  |
| snapshot | query:correlated scalar unique lookup | PASS |  |
| snapshot | query:scalar subquery threshold | PASS |  |
| snapshot | query:non-recursive cte | PASS |  |
| snapshot | query:bounded recursive cte | PASS |  |
| snapshot | query:date bucketing | PASS |  |
| snapshot | query:string functions and like | PASS |  |
| snapshot | query:looker symmetric key helpers | PASS |  |
| snapshot | query:json constructor preserves json versus text | PASS |  |
| snapshot | query:json aggregate embeds documents | PASS |  |
| snapshot | query:regular expression read transforms | PASS |  |
| snapshot | query:case expression buckets | PASS |  |
| snapshot | query:null handling | PASS |  |
| snapshot | query:coalesce and ifnull | PASS |  |
| snapshot | query:enum and set filters | PASS |  |
| snapshot | query:unsigned boundary readback | PASS |  |
| snapshot | query:derived table | PASS |  |
| snapshot | query:group_concat single expression | PASS |  |
| snapshot | query:window ranking per group | PASS |  |
| snapshot | query:window share of total over grouped output | PASS |  |
| snapshot | query:window running total | PASS |  |
| crud | converge:audit_log | PASS |  |
| crud | converge:counters | PASS |  |
| crud | converge:customers | PASS |  |
| crud | converge:order_items | PASS |  |
| crud | converge:orders | PASS |  |
| crud | converge:information_schema.columns | PASS |  |
| crud | query:point lookup by key | PASS |  |
| crud | query:range scan with compound predicate | PASS |  |
| crud | query:inner join with aggregation | PASS |  |
| crud | query:left join preserves unmatched rows | PASS |  |
| crud | query:right join preserves unmatched rows | PASS |  |
| crud | query:three-way join through items | PASS |  |
| crud | query:union all across sources | PASS |  |
| crud | query:intersect customer identifiers | PASS |  |
| crud | query:except customer identifiers | PASS |  |
| crud | query:group by with having | PASS |  |
| crud | query:conditional decimal sum keeps the fraction | PASS |  |
| crud | query:distinct count and min max | PASS |  |
| crud | query:uncorrelated in-subquery | PASS |  |
| crud | query:correlated exists with inner predicate | PASS |  |
| crud | query:correlated scalar aggregate | PASS |  |
| crud | query:correlated scalar unique lookup | PASS |  |
| crud | query:scalar subquery threshold | PASS |  |
| crud | query:non-recursive cte | PASS |  |
| crud | query:bounded recursive cte | PASS |  |
| crud | query:date bucketing | PASS |  |
| crud | query:string functions and like | PASS |  |
| crud | query:looker symmetric key helpers | PASS |  |
| crud | query:json constructor preserves json versus text | PASS |  |
| crud | query:json aggregate embeds documents | PASS |  |
| crud | query:regular expression read transforms | PASS |  |
| crud | query:case expression buckets | PASS |  |
| crud | query:null handling | PASS |  |
| crud | query:coalesce and ifnull | PASS |  |
| crud | query:enum and set filters | PASS |  |
| crud | query:unsigned boundary readback | PASS |  |
| crud | query:derived table | PASS |  |
| crud | query:group_concat single expression | PASS |  |
| crud | query:window ranking per group | PASS |  |
| crud | query:window share of total over grouped output | PASS |  |
| crud | query:window running total | PASS |  |
| type-edges | converge:audit_log | PASS |  |
| type-edges | converge:counters | PASS |  |
| type-edges | converge:customers | PASS |  |
| type-edges | converge:order_items | PASS |  |
| type-edges | converge:orders | PASS |  |
| type-edges | converge:information_schema.columns | PASS |  |
| type-edges | query:point lookup by key | PASS |  |
| type-edges | query:range scan with compound predicate | PASS |  |
| type-edges | query:inner join with aggregation | PASS |  |
| type-edges | query:left join preserves unmatched rows | PASS |  |
| type-edges | query:right join preserves unmatched rows | PASS |  |
| type-edges | query:three-way join through items | PASS |  |
| type-edges | query:union all across sources | PASS |  |
| type-edges | query:intersect customer identifiers | PASS |  |
| type-edges | query:except customer identifiers | PASS |  |
| type-edges | query:group by with having | PASS |  |
| type-edges | query:conditional decimal sum keeps the fraction | PASS |  |
| type-edges | query:distinct count and min max | PASS |  |
| type-edges | query:uncorrelated in-subquery | PASS |  |
| type-edges | query:correlated exists with inner predicate | PASS |  |
| type-edges | query:correlated scalar aggregate | PASS |  |
| type-edges | query:correlated scalar unique lookup | PASS |  |
| type-edges | query:scalar subquery threshold | PASS |  |
| type-edges | query:non-recursive cte | PASS |  |
| type-edges | query:bounded recursive cte | PASS |  |
| type-edges | query:date bucketing | PASS |  |
| type-edges | query:string functions and like | PASS |  |
| type-edges | query:looker symmetric key helpers | PASS |  |
| type-edges | query:json constructor preserves json versus text | PASS |  |
| type-edges | query:json aggregate embeds documents | PASS |  |
| type-edges | query:regular expression read transforms | PASS |  |
| type-edges | query:case expression buckets | PASS |  |
| type-edges | query:null handling | PASS |  |
| type-edges | query:coalesce and ifnull | PASS |  |
| type-edges | query:enum and set filters | PASS |  |
| type-edges | query:unsigned boundary readback | PASS |  |
| type-edges | query:derived table | PASS |  |
| type-edges | query:group_concat single expression | PASS |  |
| type-edges | query:window ranking per group | PASS |  |
| type-edges | query:window share of total over grouped output | PASS |  |
| type-edges | query:window running total | PASS |  |
| ddl | converge:audit_log | PASS |  |
| ddl | converge:counters | PASS |  |
| ddl | converge:customers | PASS |  |
| ddl | converge:order_items | PASS |  |
| ddl | converge:orders | PASS |  |
| ddl | converge:shipments | PASS |  |
| ddl | converge:information_schema.columns | PASS |  |
| ddl | query:point lookup by key | PASS |  |
| ddl | query:range scan with compound predicate | PASS |  |
| ddl | query:inner join with aggregation | PASS |  |
| ddl | query:left join preserves unmatched rows | PASS |  |
| ddl | query:right join preserves unmatched rows | PASS |  |
| ddl | query:three-way join through items | PASS |  |
| ddl | query:union all across sources | PASS |  |
| ddl | query:intersect customer identifiers | PASS |  |
| ddl | query:except customer identifiers | PASS |  |
| ddl | query:group by with having | PASS |  |
| ddl | query:conditional decimal sum keeps the fraction | PASS |  |
| ddl | query:distinct count and min max | PASS |  |
| ddl | query:uncorrelated in-subquery | PASS |  |
| ddl | query:correlated exists with inner predicate | PASS |  |
| ddl | query:correlated scalar aggregate | PASS |  |
| ddl | query:correlated scalar unique lookup | PASS |  |
| ddl | query:scalar subquery threshold | PASS |  |
| ddl | query:non-recursive cte | PASS |  |
| ddl | query:bounded recursive cte | PASS |  |
| ddl | query:date bucketing | PASS |  |
| ddl | query:string functions and like | PASS |  |
| ddl | query:looker symmetric key helpers | PASS |  |
| ddl | query:json constructor preserves json versus text | PASS |  |
| ddl | query:json aggregate embeds documents | PASS |  |
| ddl | query:regular expression read transforms | PASS |  |
| ddl | query:case expression buckets | PASS |  |
| ddl | query:null handling | PASS |  |
| ddl | query:coalesce and ifnull | PASS |  |
| ddl | query:enum and set filters | PASS |  |
| ddl | query:unsigned boundary readback | PASS |  |
| ddl | query:derived table | PASS |  |
| ddl | query:group_concat single expression | PASS |  |
| ddl | query:window ranking per group | PASS |  |
| ddl | query:window share of total over grouped output | PASS |  |
| ddl | query:window running total | PASS |  |
| churn-live | live:point lookup by key | PASS |  |
| churn-live | live:range scan with compound predicate | PASS |  |
| churn-live | live:inner join with aggregation | PASS |  |
| churn-live | live:left join preserves unmatched rows | PASS |  |
| churn-live | live:right join preserves unmatched rows | PASS |  |
| churn-live | live:three-way join through items | PASS |  |
| churn | converge:audit_log | PASS |  |
| churn | converge:counters | PASS |  |
| churn | converge:customers | PASS |  |
| churn | converge:order_items | PASS |  |
| churn | converge:orders | PASS |  |
| churn | converge:shipments | PASS |  |
| churn | converge:information_schema.columns | PASS |  |
| churn | query:point lookup by key | PASS |  |
| churn | query:range scan with compound predicate | PASS |  |
| churn | query:inner join with aggregation | PASS |  |
| churn | query:left join preserves unmatched rows | PASS |  |
| churn | query:right join preserves unmatched rows | PASS |  |
| churn | query:three-way join through items | PASS |  |
| churn | query:union all across sources | PASS |  |
| churn | query:intersect customer identifiers | PASS |  |
| churn | query:except customer identifiers | PASS |  |
| churn | query:group by with having | PASS |  |
| churn | query:conditional decimal sum keeps the fraction | PASS |  |
| churn | query:distinct count and min max | PASS |  |
| churn | query:uncorrelated in-subquery | PASS |  |
| churn | query:correlated exists with inner predicate | PASS |  |
| churn | query:correlated scalar aggregate | PASS |  |
| churn | query:correlated scalar unique lookup | PASS |  |
| churn | query:scalar subquery threshold | PASS |  |
| churn | query:non-recursive cte | PASS |  |
| churn | query:bounded recursive cte | PASS |  |
| churn | query:date bucketing | PASS |  |
| churn | query:string functions and like | PASS |  |
| churn | query:looker symmetric key helpers | PASS |  |
| churn | query:json constructor preserves json versus text | PASS |  |
| churn | query:json aggregate embeds documents | PASS |  |
| churn | query:regular expression read transforms | PASS |  |
| churn | query:case expression buckets | PASS |  |
| churn | query:null handling | PASS |  |
| churn | query:coalesce and ifnull | PASS |  |
| churn | query:enum and set filters | PASS |  |
| churn | query:unsigned boundary readback | PASS |  |
| churn | query:derived table | PASS |  |
| churn | query:group_concat single expression | PASS |  |
| churn | query:window ranking per group | PASS |  |
| churn | query:window share of total over grouped output | PASS |  |
| churn | query:window running total | PASS |  |
| spill | forced-spill:sort | PASS |  |
| spill | forced-spill:aggregate | PASS |  |
| spill | forced-spill:distinct | PASS |  |
| spill | forced-spill:join | PASS |  |
| spill | converge:audit_log | PASS |  |
| spill | converge:counters | PASS |  |
| spill | converge:customers | PASS |  |
| spill | converge:order_items | PASS |  |
| spill | converge:orders | PASS |  |
| spill | converge:shipments | PASS |  |
| spill | converge:information_schema.columns | PASS |  |
| spill | query:point lookup by key | PASS |  |
| spill | query:range scan with compound predicate | PASS |  |
| spill | query:inner join with aggregation | PASS |  |
| spill | query:left join preserves unmatched rows | PASS |  |
| spill | query:right join preserves unmatched rows | PASS |  |
| spill | query:three-way join through items | PASS |  |
| spill | query:union all across sources | PASS |  |
| spill | query:intersect customer identifiers | PASS |  |
| spill | query:except customer identifiers | PASS |  |
| spill | query:group by with having | PASS |  |
| spill | query:conditional decimal sum keeps the fraction | PASS |  |
| spill | query:distinct count and min max | PASS |  |
| spill | query:uncorrelated in-subquery | PASS |  |
| spill | query:correlated exists with inner predicate | PASS |  |
| spill | query:correlated scalar aggregate | PASS |  |
| spill | query:correlated scalar unique lookup | PASS |  |
| spill | query:scalar subquery threshold | PASS |  |
| spill | query:non-recursive cte | PASS |  |
| spill | query:bounded recursive cte | PASS |  |
| spill | query:date bucketing | PASS |  |
| spill | query:string functions and like | PASS |  |
| spill | query:looker symmetric key helpers | PASS |  |
| spill | query:json constructor preserves json versus text | PASS |  |
| spill | query:json aggregate embeds documents | PASS |  |
| spill | query:regular expression read transforms | PASS |  |
| spill | query:case expression buckets | PASS |  |
| spill | query:null handling | PASS |  |
| spill | query:coalesce and ifnull | PASS |  |
| spill | query:enum and set filters | PASS |  |
| spill | query:unsigned boundary readback | PASS |  |
| spill | query:derived table | PASS |  |
| spill | query:group_concat single expression | PASS |  |
| spill | query:window ranking per group | PASS |  |
| spill | query:window share of total over grouped output | PASS |  |
| spill | query:window running total | PASS |  |
| pooling | pool:concurrent-borrows(40 over 4) | PASS |  |
| pooling | pool:prepared-statements | PASS |  |
| pooling | pool:session-reset-between-borrows | WARN | time_zone leaked across borrows as +05:30 |
| pooling | converge:audit_log | PASS |  |
| pooling | converge:counters | PASS |  |
| pooling | converge:customers | PASS |  |
| pooling | converge:order_items | PASS |  |
| pooling | converge:orders | PASS |  |
| pooling | converge:shipments | PASS |  |
| pooling | converge:information_schema.columns | PASS |  |
| pooling | query:point lookup by key | PASS |  |
| pooling | query:range scan with compound predicate | PASS |  |
| pooling | query:inner join with aggregation | PASS |  |
| pooling | query:left join preserves unmatched rows | PASS |  |
| pooling | query:right join preserves unmatched rows | PASS |  |
| pooling | query:three-way join through items | PASS |  |
| pooling | query:union all across sources | PASS |  |
| pooling | query:intersect customer identifiers | PASS |  |
| pooling | query:except customer identifiers | PASS |  |
| pooling | query:group by with having | PASS |  |
| pooling | query:conditional decimal sum keeps the fraction | PASS |  |
| pooling | query:distinct count and min max | PASS |  |
| pooling | query:uncorrelated in-subquery | PASS |  |
| pooling | query:correlated exists with inner predicate | PASS |  |
| pooling | query:correlated scalar aggregate | PASS |  |
| pooling | query:correlated scalar unique lookup | PASS |  |
| pooling | query:scalar subquery threshold | PASS |  |
| pooling | query:non-recursive cte | PASS |  |
| pooling | query:bounded recursive cte | PASS |  |
| pooling | query:date bucketing | PASS |  |
| pooling | query:string functions and like | PASS |  |
| pooling | query:looker symmetric key helpers | PASS |  |
| pooling | query:json constructor preserves json versus text | PASS |  |
| pooling | query:json aggregate embeds documents | PASS |  |
| pooling | query:regular expression read transforms | PASS |  |
| pooling | query:case expression buckets | PASS |  |
| pooling | query:null handling | PASS |  |
| pooling | query:coalesce and ifnull | PASS |  |
| pooling | query:enum and set filters | PASS |  |
| pooling | query:unsigned boundary readback | PASS |  |
| pooling | query:derived table | PASS |  |
| pooling | query:group_concat single expression | PASS |  |
| pooling | query:window ranking per group | PASS |  |
| pooling | query:window share of total over grouped output | PASS |  |
| pooling | query:window running total | PASS |  |
| restart | converge:audit_log | PASS |  |
| restart | converge:counters | PASS |  |
| restart | converge:customers | PASS |  |
| restart | converge:order_items | PASS |  |
| restart | converge:orders | PASS |  |
| restart | converge:shipments | PASS |  |
| restart | converge:information_schema.columns | PASS |  |
| restart | query:point lookup by key | PASS |  |
| restart | query:range scan with compound predicate | PASS |  |
| restart | query:inner join with aggregation | PASS |  |
| restart | query:left join preserves unmatched rows | PASS |  |
| restart | query:right join preserves unmatched rows | PASS |  |
| restart | query:three-way join through items | PASS |  |
| restart | query:union all across sources | PASS |  |
| restart | query:intersect customer identifiers | PASS |  |
| restart | query:except customer identifiers | PASS |  |
| restart | query:group by with having | PASS |  |
| restart | query:conditional decimal sum keeps the fraction | PASS |  |
| restart | query:distinct count and min max | PASS |  |
| restart | query:uncorrelated in-subquery | PASS |  |
| restart | query:correlated exists with inner predicate | PASS |  |
| restart | query:correlated scalar aggregate | PASS |  |
| restart | query:correlated scalar unique lookup | PASS |  |
| restart | query:scalar subquery threshold | PASS |  |
| restart | query:non-recursive cte | PASS |  |
| restart | query:bounded recursive cte | PASS |  |
| restart | query:date bucketing | PASS |  |
| restart | query:string functions and like | PASS |  |
| restart | query:looker symmetric key helpers | PASS |  |
| restart | query:json constructor preserves json versus text | PASS |  |
| restart | query:json aggregate embeds documents | PASS |  |
| restart | query:regular expression read transforms | PASS |  |
| restart | query:case expression buckets | PASS |  |
| restart | query:null handling | PASS |  |
| restart | query:coalesce and ifnull | PASS |  |
| restart | query:enum and set filters | PASS |  |
| restart | query:unsigned boundary readback | PASS |  |
| restart | query:derived table | PASS |  |
| restart | query:group_concat single expression | PASS |  |
| restart | query:window ranking per group | PASS |  |
| restart | query:window share of total over grouped output | PASS |  |
| restart | query:window running total | PASS |  |
| control-plane | api:auth login issues a fresh token | PASS |  |
| control-plane | api:auth setup status responds | PASS |  |
| control-plane | api:health, status, and metrics respond | PASS |  |
| control-plane | api:databases list and detail agree | PASS |  |
| control-plane | api:connection test succeeds | PASS |  |
| control-plane | api:activity and dlq respond | PASS |  |
| control-plane | api:table metadata routes match the source | PASS |  |
| control-plane | api:api key disable blocks the wire, enable restores it | PASS |  |
| control-plane | api:sse event stream connects | PASS |  |
| control-plane | api:mode switches to polling and back with exact counts | PASS |  |
| control-plane | api:resync and reconcile are accepted | PASS |  |
| control-plane | api:keyless policy: ambiguity quarantines and exact multiplicity repairs | PASS |  |
| control-plane | api:throwaway database lifecycle: create, update, delete | PASS |  |
| control-plane | converge:audit_log | PASS |  |
| control-plane | converge:counters | PASS |  |
| control-plane | converge:customers | PASS |  |
| control-plane | converge:keyless_log | PASS |  |
| control-plane | converge:order_items | PASS |  |
| control-plane | converge:orders | PASS |  |
| control-plane | converge:shipments | PASS |  |
| control-plane | converge:information_schema.columns | PASS |  |
| control-plane | query:point lookup by key | PASS |  |
| control-plane | query:range scan with compound predicate | PASS |  |
| control-plane | query:inner join with aggregation | PASS |  |
| control-plane | query:left join preserves unmatched rows | PASS |  |
| control-plane | query:right join preserves unmatched rows | PASS |  |
| control-plane | query:three-way join through items | PASS |  |
| control-plane | query:union all across sources | PASS |  |
| control-plane | query:intersect customer identifiers | PASS |  |
| control-plane | query:except customer identifiers | PASS |  |
| control-plane | query:group by with having | PASS |  |
| control-plane | query:conditional decimal sum keeps the fraction | PASS |  |
| control-plane | query:distinct count and min max | PASS |  |
| control-plane | query:uncorrelated in-subquery | PASS |  |
| control-plane | query:correlated exists with inner predicate | PASS |  |
| control-plane | query:correlated scalar aggregate | PASS |  |
| control-plane | query:correlated scalar unique lookup | PASS |  |
| control-plane | query:scalar subquery threshold | PASS |  |
| control-plane | query:non-recursive cte | PASS |  |
| control-plane | query:bounded recursive cte | PASS |  |
| control-plane | query:date bucketing | PASS |  |
| control-plane | query:string functions and like | PASS |  |
| control-plane | query:looker symmetric key helpers | PASS |  |
| control-plane | query:json constructor preserves json versus text | PASS |  |
| control-plane | query:json aggregate embeds documents | PASS |  |
| control-plane | query:regular expression read transforms | PASS |  |
| control-plane | query:case expression buckets | PASS |  |
| control-plane | query:null handling | PASS |  |
| control-plane | query:coalesce and ifnull | PASS |  |
| control-plane | query:enum and set filters | PASS |  |
| control-plane | query:unsigned boundary readback | PASS |  |
| control-plane | query:derived table | PASS |  |
| control-plane | query:group_concat single expression | PASS |  |
| control-plane | query:window ranking per group | PASS |  |
| control-plane | query:window share of total over grouped output | PASS |  |
| control-plane | query:window running total | PASS |  |
| ddl-documented-gaps | converge:audit_history | WARN | pintail query failed: Error: unknown table e2e_db.audit_history |
| ddl-documented-gaps | converge:counters | PASS |  |
| ddl-documented-gaps | converge:customers | PASS |  |
| ddl-documented-gaps | converge:keyless_log | PASS |  |
| ddl-documented-gaps | converge:order_items | PASS |  |
| ddl-documented-gaps | converge:orders | PASS |  |
| ddl-documented-gaps | converge:shipments | PASS |  |
| ddl-documented-gaps | converge:information_schema.columns | WARN | row 0: |
| ddl-documented-gaps | query:point lookup by key | PASS |  |
| ddl-documented-gaps | query:range scan with compound predicate | PASS |  |
| ddl-documented-gaps | query:inner join with aggregation | PASS |  |
| ddl-documented-gaps | query:left join preserves unmatched rows | PASS |  |
| ddl-documented-gaps | query:right join preserves unmatched rows | PASS |  |
| ddl-documented-gaps | query:three-way join through items | SKIP |  |
| ddl-documented-gaps | query:union all across sources | PASS |  |
| ddl-documented-gaps | query:intersect customer identifiers | PASS |  |
| ddl-documented-gaps | query:except customer identifiers | PASS |  |
| ddl-documented-gaps | query:group by with having | PASS |  |
| ddl-documented-gaps | query:conditional decimal sum keeps the fraction | PASS |  |
| ddl-documented-gaps | query:distinct count and min max | PASS |  |
| ddl-documented-gaps | query:uncorrelated in-subquery | PASS |  |
| ddl-documented-gaps | query:correlated exists with inner predicate | PASS |  |
| ddl-documented-gaps | query:correlated scalar aggregate | PASS |  |
| ddl-documented-gaps | query:correlated scalar unique lookup | PASS |  |
| ddl-documented-gaps | query:scalar subquery threshold | PASS |  |
| ddl-documented-gaps | query:non-recursive cte | PASS |  |
| ddl-documented-gaps | query:bounded recursive cte | PASS |  |
| ddl-documented-gaps | query:date bucketing | PASS |  |
| ddl-documented-gaps | query:string functions and like | PASS |  |
| ddl-documented-gaps | query:looker symmetric key helpers | PASS |  |
| ddl-documented-gaps | query:json constructor preserves json versus text | PASS |  |
| ddl-documented-gaps | query:json aggregate embeds documents | PASS |  |
| ddl-documented-gaps | query:regular expression read transforms | PASS |  |
| ddl-documented-gaps | query:case expression buckets | PASS |  |
| ddl-documented-gaps | query:null handling | PASS |  |
| ddl-documented-gaps | query:coalesce and ifnull | PASS |  |
| ddl-documented-gaps | query:enum and set filters | PASS |  |
| ddl-documented-gaps | query:unsigned boundary readback | PASS |  |
| ddl-documented-gaps | query:derived table | PASS |  |
| ddl-documented-gaps | query:group_concat single expression | PASS |  |
| ddl-documented-gaps | query:window ranking per group | PASS |  |
| ddl-documented-gaps | query:window share of total over grouped output | PASS |  |
| ddl-documented-gaps | query:window running total | PASS |  |

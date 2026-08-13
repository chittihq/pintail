# Pintail end-to-end differential gate

Measured 2026-08-13T05:41:56.890Z.

**736 passed, 0 failed, 13 documented-gap warnings.**

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
| snapshot | query:decimal column average beyond simple sum | PASS |  |
| snapshot | query:json extract filter on customer meta | PASS |  |
| snapshot | query:fan-out join group concat line products | PASS |  |
| snapshot | query:outer join customers without recent orders | PASS |  |
| snapshot | query:set op union distinct tiers and statuses | PASS |  |
| snapshot | query:temporal convert and date_format grain | PASS |  |
| snapshot | query:correlated not exists open orders | PASS |  |
| snapshot | query:window lag payment-shaped totals | PASS |  |
| snapshot | query:multi-key join items to orders | PASS |  |
| snapshot | query:between and null-safe coalesce on balance | PASS |  |
| snapshot | query:intersect all-style customer buyers | PASS |  |
| snapshot | query:derived table status revenue share | PASS |  |
| snapshot | query:general_ci: equality folds ASCII case | PASS |  |
| snapshot | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| snapshot | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| snapshot | query:general_ci: every supplementary character compares equal | PASS |  |
| snapshot | query:general_ci: grouping partitions by collated equality | PASS |  |
| snapshot | query:general_ci: ordering follows the collation, not code points | PASS |  |
| snapshot | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| snapshot | query:general_ci: joining on a collated column | PASS |  |
| snapshot | query:general_ci: representative spelling of a collated group | PASS |  |
| snapshot | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
| orm-compat | sequelize:metadata:result | PASS |  |
| orm-compat | sequelize:metadata:generated-sql | PASS |  |
| orm-compat | sequelize:point-and-filtered-reads:result | PASS |  |
| orm-compat | sequelize:point-and-filtered-reads:generated-sql | PASS |  |
| orm-compat | sequelize:relation-read:result | PASS |  |
| orm-compat | sequelize:relation-read:generated-sql | PASS |  |
| orm-compat | sequelize:grouped-aggregate:result | PASS |  |
| orm-compat | sequelize:grouped-aggregate:generated-sql | PASS |  |
| orm-compat | drizzle:introspection:result | PASS |  |
| orm-compat | drizzle:introspection:generated-sql | PASS |  |
| orm-compat | drizzle:point-and-filtered-reads:result | PASS |  |
| orm-compat | drizzle:point-and-filtered-reads:generated-sql | PASS |  |
| orm-compat | drizzle:relation-read:result | PASS |  |
| orm-compat | drizzle:relation-read:generated-sql | PASS |  |
| orm-compat | drizzle:grouped-aggregate:result | PASS |  |
| orm-compat | drizzle:grouped-aggregate:generated-sql | PASS |  |
| orm-compat | prisma:introspection:result | PASS |  |
| orm-compat | prisma:introspection:generated-sql | PASS |  |
| orm-compat | prisma:point-and-filtered-reads:result | PASS |  |
| orm-compat | prisma:point-and-filtered-reads:generated-sql | PASS |  |
| orm-compat | prisma:relation-read:result | PASS |  |
| orm-compat | prisma:relation-read:generated-sql | PASS |  |
| orm-compat | prisma:grouped-aggregate:result | PASS |  |
| orm-compat | prisma:grouped-aggregate:generated-sql | PASS |  |
| orm-compat | converge:audit_log | PASS |  |
| orm-compat | converge:counters | PASS |  |
| orm-compat | converge:customers | PASS |  |
| orm-compat | converge:order_items | PASS |  |
| orm-compat | converge:orders | PASS |  |
| orm-compat | converge:information_schema.columns | PASS |  |
| orm-compat | query:point lookup by key | PASS |  |
| orm-compat | query:range scan with compound predicate | PASS |  |
| orm-compat | query:inner join with aggregation | PASS |  |
| orm-compat | query:left join preserves unmatched rows | PASS |  |
| orm-compat | query:right join preserves unmatched rows | PASS |  |
| orm-compat | query:three-way join through items | PASS |  |
| orm-compat | query:union all across sources | PASS |  |
| orm-compat | query:intersect customer identifiers | PASS |  |
| orm-compat | query:except customer identifiers | PASS |  |
| orm-compat | query:group by with having | PASS |  |
| orm-compat | query:conditional decimal sum keeps the fraction | PASS |  |
| orm-compat | query:distinct count and min max | PASS |  |
| orm-compat | query:uncorrelated in-subquery | PASS |  |
| orm-compat | query:correlated exists with inner predicate | PASS |  |
| orm-compat | query:correlated scalar aggregate | PASS |  |
| orm-compat | query:correlated scalar unique lookup | PASS |  |
| orm-compat | query:scalar subquery threshold | PASS |  |
| orm-compat | query:non-recursive cte | PASS |  |
| orm-compat | query:bounded recursive cte | PASS |  |
| orm-compat | query:date bucketing | PASS |  |
| orm-compat | query:string functions and like | PASS |  |
| orm-compat | query:looker symmetric key helpers | PASS |  |
| orm-compat | query:json constructor preserves json versus text | PASS |  |
| orm-compat | query:json aggregate embeds documents | PASS |  |
| orm-compat | query:regular expression read transforms | PASS |  |
| orm-compat | query:case expression buckets | PASS |  |
| orm-compat | query:null handling | PASS |  |
| orm-compat | query:coalesce and ifnull | PASS |  |
| orm-compat | query:enum and set filters | PASS |  |
| orm-compat | query:unsigned boundary readback | PASS |  |
| orm-compat | query:derived table | PASS |  |
| orm-compat | query:group_concat single expression | PASS |  |
| orm-compat | query:window ranking per group | PASS |  |
| orm-compat | query:window share of total over grouped output | PASS |  |
| orm-compat | query:window running total | PASS |  |
| orm-compat | query:decimal column average beyond simple sum | PASS |  |
| orm-compat | query:json extract filter on customer meta | PASS |  |
| orm-compat | query:fan-out join group concat line products | PASS |  |
| orm-compat | query:outer join customers without recent orders | PASS |  |
| orm-compat | query:set op union distinct tiers and statuses | PASS |  |
| orm-compat | query:temporal convert and date_format grain | PASS |  |
| orm-compat | query:correlated not exists open orders | PASS |  |
| orm-compat | query:window lag payment-shaped totals | PASS |  |
| orm-compat | query:multi-key join items to orders | PASS |  |
| orm-compat | query:between and null-safe coalesce on balance | PASS |  |
| orm-compat | query:intersect all-style customer buyers | PASS |  |
| orm-compat | query:derived table status revenue share | PASS |  |
| orm-compat | query:general_ci: equality folds ASCII case | PASS |  |
| orm-compat | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| orm-compat | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| orm-compat | query:general_ci: every supplementary character compares equal | PASS |  |
| orm-compat | query:general_ci: grouping partitions by collated equality | PASS |  |
| orm-compat | query:general_ci: ordering follows the collation, not code points | PASS |  |
| orm-compat | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| orm-compat | query:general_ci: joining on a collated column | PASS |  |
| orm-compat | query:general_ci: representative spelling of a collated group | PASS |  |
| orm-compat | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| crud | query:decimal column average beyond simple sum | PASS |  |
| crud | query:json extract filter on customer meta | PASS |  |
| crud | query:fan-out join group concat line products | PASS |  |
| crud | query:outer join customers without recent orders | PASS |  |
| crud | query:set op union distinct tiers and statuses | PASS |  |
| crud | query:temporal convert and date_format grain | PASS |  |
| crud | query:correlated not exists open orders | PASS |  |
| crud | query:window lag payment-shaped totals | PASS |  |
| crud | query:multi-key join items to orders | PASS |  |
| crud | query:between and null-safe coalesce on balance | PASS |  |
| crud | query:intersect all-style customer buyers | PASS |  |
| crud | query:derived table status revenue share | PASS |  |
| crud | query:general_ci: equality folds ASCII case | PASS |  |
| crud | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| crud | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| crud | query:general_ci: every supplementary character compares equal | PASS |  |
| crud | query:general_ci: grouping partitions by collated equality | PASS |  |
| crud | query:general_ci: ordering follows the collation, not code points | PASS |  |
| crud | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| crud | query:general_ci: joining on a collated column | PASS |  |
| crud | query:general_ci: representative spelling of a collated group | PASS |  |
| crud | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| type-edges | query:decimal column average beyond simple sum | PASS |  |
| type-edges | query:json extract filter on customer meta | PASS |  |
| type-edges | query:fan-out join group concat line products | PASS |  |
| type-edges | query:outer join customers without recent orders | PASS |  |
| type-edges | query:set op union distinct tiers and statuses | PASS |  |
| type-edges | query:temporal convert and date_format grain | PASS |  |
| type-edges | query:correlated not exists open orders | PASS |  |
| type-edges | query:window lag payment-shaped totals | PASS |  |
| type-edges | query:multi-key join items to orders | PASS |  |
| type-edges | query:between and null-safe coalesce on balance | PASS |  |
| type-edges | query:intersect all-style customer buyers | PASS |  |
| type-edges | query:derived table status revenue share | PASS |  |
| type-edges | query:general_ci: equality folds ASCII case | PASS |  |
| type-edges | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| type-edges | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| type-edges | query:general_ci: every supplementary character compares equal | PASS |  |
| type-edges | query:general_ci: grouping partitions by collated equality | PASS |  |
| type-edges | query:general_ci: ordering follows the collation, not code points | PASS |  |
| type-edges | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| type-edges | query:general_ci: joining on a collated column | PASS |  |
| type-edges | query:general_ci: representative spelling of a collated group | PASS |  |
| type-edges | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| ddl | query:decimal column average beyond simple sum | PASS |  |
| ddl | query:json extract filter on customer meta | PASS |  |
| ddl | query:fan-out join group concat line products | PASS |  |
| ddl | query:outer join customers without recent orders | PASS |  |
| ddl | query:set op union distinct tiers and statuses | PASS |  |
| ddl | query:temporal convert and date_format grain | PASS |  |
| ddl | query:correlated not exists open orders | PASS |  |
| ddl | query:window lag payment-shaped totals | PASS |  |
| ddl | query:multi-key join items to orders | PASS |  |
| ddl | query:between and null-safe coalesce on balance | PASS |  |
| ddl | query:intersect all-style customer buyers | PASS |  |
| ddl | query:derived table status revenue share | PASS |  |
| ddl | query:general_ci: equality folds ASCII case | PASS |  |
| ddl | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| ddl | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| ddl | query:general_ci: every supplementary character compares equal | PASS |  |
| ddl | query:general_ci: grouping partitions by collated equality | PASS |  |
| ddl | query:general_ci: ordering follows the collation, not code points | PASS |  |
| ddl | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| ddl | query:general_ci: joining on a collated column | PASS |  |
| ddl | query:general_ci: representative spelling of a collated group | PASS |  |
| ddl | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| churn | query:decimal column average beyond simple sum | PASS |  |
| churn | query:json extract filter on customer meta | PASS |  |
| churn | query:fan-out join group concat line products | PASS |  |
| churn | query:outer join customers without recent orders | PASS |  |
| churn | query:set op union distinct tiers and statuses | PASS |  |
| churn | query:temporal convert and date_format grain | PASS |  |
| churn | query:correlated not exists open orders | PASS |  |
| churn | query:window lag payment-shaped totals | PASS |  |
| churn | query:multi-key join items to orders | PASS |  |
| churn | query:between and null-safe coalesce on balance | PASS |  |
| churn | query:intersect all-style customer buyers | PASS |  |
| churn | query:derived table status revenue share | PASS |  |
| churn | query:general_ci: equality folds ASCII case | PASS |  |
| churn | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| churn | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| churn | query:general_ci: every supplementary character compares equal | PASS |  |
| churn | query:general_ci: grouping partitions by collated equality | PASS |  |
| churn | query:general_ci: ordering follows the collation, not code points | PASS |  |
| churn | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| churn | query:general_ci: joining on a collated column | PASS |  |
| churn | query:general_ci: representative spelling of a collated group | PASS |  |
| churn | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| spill | query:decimal column average beyond simple sum | PASS |  |
| spill | query:json extract filter on customer meta | PASS |  |
| spill | query:fan-out join group concat line products | PASS |  |
| spill | query:outer join customers without recent orders | PASS |  |
| spill | query:set op union distinct tiers and statuses | PASS |  |
| spill | query:temporal convert and date_format grain | PASS |  |
| spill | query:correlated not exists open orders | PASS |  |
| spill | query:window lag payment-shaped totals | PASS |  |
| spill | query:multi-key join items to orders | PASS |  |
| spill | query:between and null-safe coalesce on balance | PASS |  |
| spill | query:intersect all-style customer buyers | PASS |  |
| spill | query:derived table status revenue share | PASS |  |
| spill | query:general_ci: equality folds ASCII case | PASS |  |
| spill | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| spill | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| spill | query:general_ci: every supplementary character compares equal | PASS |  |
| spill | query:general_ci: grouping partitions by collated equality | PASS |  |
| spill | query:general_ci: ordering follows the collation, not code points | PASS |  |
| spill | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| spill | query:general_ci: joining on a collated column | PASS |  |
| spill | query:general_ci: representative spelling of a collated group | PASS |  |
| spill | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
| pooling | pool:concurrent-borrows(40 over 4) | PASS |  |
| pooling | pool:prepared-statements | PASS |  |
| pooling | pool:session-state-survives-borrow-like-mysql | PASS |  |
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
| pooling | query:decimal column average beyond simple sum | PASS |  |
| pooling | query:json extract filter on customer meta | PASS |  |
| pooling | query:fan-out join group concat line products | PASS |  |
| pooling | query:outer join customers without recent orders | PASS |  |
| pooling | query:set op union distinct tiers and statuses | PASS |  |
| pooling | query:temporal convert and date_format grain | PASS |  |
| pooling | query:correlated not exists open orders | PASS |  |
| pooling | query:window lag payment-shaped totals | PASS |  |
| pooling | query:multi-key join items to orders | PASS |  |
| pooling | query:between and null-safe coalesce on balance | PASS |  |
| pooling | query:intersect all-style customer buyers | PASS |  |
| pooling | query:derived table status revenue share | PASS |  |
| pooling | query:general_ci: equality folds ASCII case | PASS |  |
| pooling | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| pooling | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| pooling | query:general_ci: every supplementary character compares equal | PASS |  |
| pooling | query:general_ci: grouping partitions by collated equality | PASS |  |
| pooling | query:general_ci: ordering follows the collation, not code points | PASS |  |
| pooling | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| pooling | query:general_ci: joining on a collated column | PASS |  |
| pooling | query:general_ci: representative spelling of a collated group | PASS |  |
| pooling | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| restart | query:decimal column average beyond simple sum | PASS |  |
| restart | query:json extract filter on customer meta | PASS |  |
| restart | query:fan-out join group concat line products | PASS |  |
| restart | query:outer join customers without recent orders | PASS |  |
| restart | query:set op union distinct tiers and statuses | PASS |  |
| restart | query:temporal convert and date_format grain | PASS |  |
| restart | query:correlated not exists open orders | PASS |  |
| restart | query:window lag payment-shaped totals | PASS |  |
| restart | query:multi-key join items to orders | PASS |  |
| restart | query:between and null-safe coalesce on balance | PASS |  |
| restart | query:intersect all-style customer buyers | PASS |  |
| restart | query:derived table status revenue share | PASS |  |
| restart | query:general_ci: equality folds ASCII case | PASS |  |
| restart | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| restart | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| restart | query:general_ci: every supplementary character compares equal | PASS |  |
| restart | query:general_ci: grouping partitions by collated equality | PASS |  |
| restart | query:general_ci: ordering follows the collation, not code points | PASS |  |
| restart | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| restart | query:general_ci: joining on a collated column | PASS |  |
| restart | query:general_ci: representative spelling of a collated group | PASS |  |
| restart | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| control-plane | query:decimal column average beyond simple sum | PASS |  |
| control-plane | query:json extract filter on customer meta | PASS |  |
| control-plane | query:fan-out join group concat line products | PASS |  |
| control-plane | query:outer join customers without recent orders | PASS |  |
| control-plane | query:set op union distinct tiers and statuses | PASS |  |
| control-plane | query:temporal convert and date_format grain | PASS |  |
| control-plane | query:correlated not exists open orders | PASS |  |
| control-plane | query:window lag payment-shaped totals | PASS |  |
| control-plane | query:multi-key join items to orders | PASS |  |
| control-plane | query:between and null-safe coalesce on balance | PASS |  |
| control-plane | query:intersect all-style customer buyers | PASS |  |
| control-plane | query:derived table status revenue share | PASS |  |
| control-plane | query:general_ci: equality folds ASCII case | PASS |  |
| control-plane | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| control-plane | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| control-plane | query:general_ci: every supplementary character compares equal | PASS |  |
| control-plane | query:general_ci: grouping partitions by collated equality | PASS |  |
| control-plane | query:general_ci: ordering follows the collation, not code points | PASS |  |
| control-plane | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| control-plane | query:general_ci: joining on a collated column | PASS |  |
| control-plane | query:general_ci: representative spelling of a collated group | PASS |  |
| control-plane | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |
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
| ddl-documented-gaps | query:decimal column average beyond simple sum | PASS |  |
| ddl-documented-gaps | query:json extract filter on customer meta | PASS |  |
| ddl-documented-gaps | query:fan-out join group concat line products | SKIP |  |
| ddl-documented-gaps | query:outer join customers without recent orders | PASS |  |
| ddl-documented-gaps | query:set op union distinct tiers and statuses | PASS |  |
| ddl-documented-gaps | query:temporal convert and date_format grain | PASS |  |
| ddl-documented-gaps | query:correlated not exists open orders | PASS |  |
| ddl-documented-gaps | query:window lag payment-shaped totals | PASS |  |
| ddl-documented-gaps | query:multi-key join items to orders | SKIP |  |
| ddl-documented-gaps | query:between and null-safe coalesce on balance | PASS |  |
| ddl-documented-gaps | query:intersect all-style customer buyers | PASS |  |
| ddl-documented-gaps | query:derived table status revenue share | PASS |  |
| ddl-documented-gaps | query:general_ci: equality folds ASCII case | PASS |  |
| ddl-documented-gaps | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| ddl-documented-gaps | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| ddl-documented-gaps | query:general_ci: every supplementary character compares equal | PASS |  |
| ddl-documented-gaps | query:general_ci: grouping partitions by collated equality | PASS |  |
| ddl-documented-gaps | query:general_ci: ordering follows the collation, not code points | PASS |  |
| ddl-documented-gaps | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| ddl-documented-gaps | query:general_ci: joining on a collated column | PASS |  |
| ddl-documented-gaps | query:general_ci: representative spelling of a collated group | PASS |  |
| ddl-documented-gaps | query:general_ci: mixing collations across separate comparisons | WARN | a query whose comparisons use different collations is refused; pintail resolves one collation per query (#10) |

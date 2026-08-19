# Pintail end-to-end differential gate

Measured 2026-08-19T13:22:06.888Z.

**1826 passed, 0 failed, 6 documented-gap warnings.**

| Phase | Check | Status | Detail |
|---|---|---|---|
| snapshot | converge:audit_log | PASS |  |
| snapshot | converge:counters | PASS |  |
| snapshot | converge:customers | PASS |  |
| snapshot | converge:order_items | PASS |  |
| snapshot | converge:orders | PASS |  |
| snapshot | converge:staff | PASS |  |
| snapshot | converge:information_schema.columns | PASS |  |
| snapshot | query:point lookup by key | PASS |  |
| snapshot | query:range scan with compound predicate | PASS |  |
| snapshot | query:inner join with aggregation | PASS |  |
| snapshot | query:join with a residual comparison between both inputs | PASS |  |
| snapshot | query:left join keeps rows whose only matches fail the residual | PASS |  |
| snapshot | query:residual comparison through coalesce on a nullable column | PASS |  |
| snapshot | query:created-by and updated-by resolve through separate aliases | PASS |  |
| snapshot | query:alias pair with the join order reversed | PASS |  |
| snapshot | query:four aliases of one table joined in a chain | PASS |  |
| snapshot | query:self-join with a single-side predicate in the ON clause | PASS |  |
| snapshot | query:self-join manager chain preserves the roots | PASS |  |
| snapshot | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| snapshot | query:aliases stay distinct when the empty side joins first | PASS |  |
| snapshot | query:left join preserves unmatched rows | PASS |  |
| snapshot | query:right join preserves unmatched rows | PASS |  |
| snapshot | query:three-way join through items | PASS |  |
| snapshot | query:union all across sources | PASS |  |
| snapshot | query:intersect customer identifiers | PASS |  |
| snapshot | query:except customer identifiers | PASS |  |
| snapshot | query:order by an expression over an aggregate | PASS |  |
| snapshot | query:order by a tree over several aggregates | PASS |  |
| snapshot | query:order by an aggregate absent from the select list | PASS |  |
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
| snapshot | query:general_ci: mixing collations across separate comparisons | PASS |  |
| snapshot | query:enum: order by ascends by declared ordinal | PASS |  |
| snapshot | query:enum: order by descends by declared ordinal | PASS |  |
| snapshot | query:enum: min and max compare as strings | PASS |  |
| snapshot | query:enum: a greater-than range compares as strings | PASS |  |
| snapshot | query:enum: a less-than range compares as strings | PASS |  |
| snapshot | query:enum: between compares as strings | PASS |  |
| snapshot | query:enum: distinct orders by ordinal | PASS |  |
| snapshot | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| snapshot | query:enum: a window order walks the ordinal | PASS |  |
| snapshot | query:collation: mixed grouping answers with per-key folds | PASS |  |
| snapshot | query:collation: distinct counts fold per column collation | PASS |  |
| snapshot | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| snapshot | query:set: order by walks the member bitmask | SKIP |  |
| snapshot | query:set: grouping orders groups by bitmask | SKIP |  |
| snapshot | query:geometry: hex round-trips the internal format | SKIP |  |
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
| orm-compat | converge:staff | PASS |  |
| orm-compat | converge:information_schema.columns | PASS |  |
| orm-compat | query:point lookup by key | PASS |  |
| orm-compat | query:range scan with compound predicate | PASS |  |
| orm-compat | query:inner join with aggregation | PASS |  |
| orm-compat | query:join with a residual comparison between both inputs | PASS |  |
| orm-compat | query:left join keeps rows whose only matches fail the residual | PASS |  |
| orm-compat | query:residual comparison through coalesce on a nullable column | PASS |  |
| orm-compat | query:created-by and updated-by resolve through separate aliases | PASS |  |
| orm-compat | query:alias pair with the join order reversed | PASS |  |
| orm-compat | query:four aliases of one table joined in a chain | PASS |  |
| orm-compat | query:self-join with a single-side predicate in the ON clause | PASS |  |
| orm-compat | query:self-join manager chain preserves the roots | PASS |  |
| orm-compat | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| orm-compat | query:aliases stay distinct when the empty side joins first | PASS |  |
| orm-compat | query:left join preserves unmatched rows | PASS |  |
| orm-compat | query:right join preserves unmatched rows | PASS |  |
| orm-compat | query:three-way join through items | PASS |  |
| orm-compat | query:union all across sources | PASS |  |
| orm-compat | query:intersect customer identifiers | PASS |  |
| orm-compat | query:except customer identifiers | PASS |  |
| orm-compat | query:order by an expression over an aggregate | PASS |  |
| orm-compat | query:order by a tree over several aggregates | PASS |  |
| orm-compat | query:order by an aggregate absent from the select list | PASS |  |
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
| orm-compat | query:general_ci: mixing collations across separate comparisons | PASS |  |
| orm-compat | query:enum: order by ascends by declared ordinal | PASS |  |
| orm-compat | query:enum: order by descends by declared ordinal | PASS |  |
| orm-compat | query:enum: min and max compare as strings | PASS |  |
| orm-compat | query:enum: a greater-than range compares as strings | PASS |  |
| orm-compat | query:enum: a less-than range compares as strings | PASS |  |
| orm-compat | query:enum: between compares as strings | PASS |  |
| orm-compat | query:enum: distinct orders by ordinal | PASS |  |
| orm-compat | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| orm-compat | query:enum: a window order walks the ordinal | PASS |  |
| orm-compat | query:collation: mixed grouping answers with per-key folds | PASS |  |
| orm-compat | query:collation: distinct counts fold per column collation | PASS |  |
| orm-compat | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| orm-compat | query:set: order by walks the member bitmask | SKIP |  |
| orm-compat | query:set: grouping orders groups by bitmask | SKIP |  |
| orm-compat | query:geometry: hex round-trips the internal format | SKIP |  |
| crud | converge:audit_log | PASS |  |
| crud | converge:counters | PASS |  |
| crud | converge:customers | PASS |  |
| crud | converge:order_items | PASS |  |
| crud | converge:orders | PASS |  |
| crud | converge:staff | PASS |  |
| crud | converge:information_schema.columns | PASS |  |
| crud | query:point lookup by key | PASS |  |
| crud | query:range scan with compound predicate | PASS |  |
| crud | query:inner join with aggregation | PASS |  |
| crud | query:join with a residual comparison between both inputs | PASS |  |
| crud | query:left join keeps rows whose only matches fail the residual | PASS |  |
| crud | query:residual comparison through coalesce on a nullable column | PASS |  |
| crud | query:created-by and updated-by resolve through separate aliases | PASS |  |
| crud | query:alias pair with the join order reversed | PASS |  |
| crud | query:four aliases of one table joined in a chain | PASS |  |
| crud | query:self-join with a single-side predicate in the ON clause | PASS |  |
| crud | query:self-join manager chain preserves the roots | PASS |  |
| crud | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| crud | query:aliases stay distinct when the empty side joins first | PASS |  |
| crud | query:left join preserves unmatched rows | PASS |  |
| crud | query:right join preserves unmatched rows | PASS |  |
| crud | query:three-way join through items | PASS |  |
| crud | query:union all across sources | PASS |  |
| crud | query:intersect customer identifiers | PASS |  |
| crud | query:except customer identifiers | PASS |  |
| crud | query:order by an expression over an aggregate | PASS |  |
| crud | query:order by a tree over several aggregates | PASS |  |
| crud | query:order by an aggregate absent from the select list | PASS |  |
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
| crud | query:general_ci: mixing collations across separate comparisons | PASS |  |
| crud | query:enum: order by ascends by declared ordinal | PASS |  |
| crud | query:enum: order by descends by declared ordinal | PASS |  |
| crud | query:enum: min and max compare as strings | PASS |  |
| crud | query:enum: a greater-than range compares as strings | PASS |  |
| crud | query:enum: a less-than range compares as strings | PASS |  |
| crud | query:enum: between compares as strings | PASS |  |
| crud | query:enum: distinct orders by ordinal | PASS |  |
| crud | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| crud | query:enum: a window order walks the ordinal | PASS |  |
| crud | query:collation: mixed grouping answers with per-key folds | PASS |  |
| crud | query:collation: distinct counts fold per column collation | PASS |  |
| crud | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| crud | query:set: order by walks the member bitmask | SKIP |  |
| crud | query:set: grouping orders groups by bitmask | SKIP |  |
| crud | query:geometry: hex round-trips the internal format | SKIP |  |
| type-edges | converge:audit_log | PASS |  |
| type-edges | converge:counters | PASS |  |
| type-edges | converge:customers | PASS |  |
| type-edges | converge:order_items | PASS |  |
| type-edges | converge:orders | PASS |  |
| type-edges | converge:staff | PASS |  |
| type-edges | converge:information_schema.columns | PASS |  |
| type-edges | query:point lookup by key | PASS |  |
| type-edges | query:range scan with compound predicate | PASS |  |
| type-edges | query:inner join with aggregation | PASS |  |
| type-edges | query:join with a residual comparison between both inputs | PASS |  |
| type-edges | query:left join keeps rows whose only matches fail the residual | PASS |  |
| type-edges | query:residual comparison through coalesce on a nullable column | PASS |  |
| type-edges | query:created-by and updated-by resolve through separate aliases | PASS |  |
| type-edges | query:alias pair with the join order reversed | PASS |  |
| type-edges | query:four aliases of one table joined in a chain | PASS |  |
| type-edges | query:self-join with a single-side predicate in the ON clause | PASS |  |
| type-edges | query:self-join manager chain preserves the roots | PASS |  |
| type-edges | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| type-edges | query:aliases stay distinct when the empty side joins first | PASS |  |
| type-edges | query:left join preserves unmatched rows | PASS |  |
| type-edges | query:right join preserves unmatched rows | PASS |  |
| type-edges | query:three-way join through items | PASS |  |
| type-edges | query:union all across sources | PASS |  |
| type-edges | query:intersect customer identifiers | PASS |  |
| type-edges | query:except customer identifiers | PASS |  |
| type-edges | query:order by an expression over an aggregate | PASS |  |
| type-edges | query:order by a tree over several aggregates | PASS |  |
| type-edges | query:order by an aggregate absent from the select list | PASS |  |
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
| type-edges | query:general_ci: mixing collations across separate comparisons | PASS |  |
| type-edges | query:enum: order by ascends by declared ordinal | PASS |  |
| type-edges | query:enum: order by descends by declared ordinal | PASS |  |
| type-edges | query:enum: min and max compare as strings | PASS |  |
| type-edges | query:enum: a greater-than range compares as strings | PASS |  |
| type-edges | query:enum: a less-than range compares as strings | PASS |  |
| type-edges | query:enum: between compares as strings | PASS |  |
| type-edges | query:enum: distinct orders by ordinal | PASS |  |
| type-edges | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| type-edges | query:enum: a window order walks the ordinal | PASS |  |
| type-edges | query:collation: mixed grouping answers with per-key folds | PASS |  |
| type-edges | query:collation: distinct counts fold per column collation | PASS |  |
| type-edges | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| type-edges | query:set: order by walks the member bitmask | SKIP |  |
| type-edges | query:set: grouping orders groups by bitmask | SKIP |  |
| type-edges | query:geometry: hex round-trips the internal format | SKIP |  |
| ddl | converge:audit_log | PASS |  |
| ddl | converge:counters | PASS |  |
| ddl | converge:customers | PASS |  |
| ddl | converge:order_items | PASS |  |
| ddl | converge:orders | PASS |  |
| ddl | converge:shipments | PASS |  |
| ddl | converge:staff | PASS |  |
| ddl | converge:information_schema.columns | PASS |  |
| ddl | query:point lookup by key | PASS |  |
| ddl | query:range scan with compound predicate | PASS |  |
| ddl | query:inner join with aggregation | PASS |  |
| ddl | query:join with a residual comparison between both inputs | PASS |  |
| ddl | query:left join keeps rows whose only matches fail the residual | PASS |  |
| ddl | query:residual comparison through coalesce on a nullable column | PASS |  |
| ddl | query:created-by and updated-by resolve through separate aliases | PASS |  |
| ddl | query:alias pair with the join order reversed | PASS |  |
| ddl | query:four aliases of one table joined in a chain | PASS |  |
| ddl | query:self-join with a single-side predicate in the ON clause | PASS |  |
| ddl | query:self-join manager chain preserves the roots | PASS |  |
| ddl | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| ddl | query:aliases stay distinct when the empty side joins first | PASS |  |
| ddl | query:left join preserves unmatched rows | PASS |  |
| ddl | query:right join preserves unmatched rows | PASS |  |
| ddl | query:three-way join through items | PASS |  |
| ddl | query:union all across sources | PASS |  |
| ddl | query:intersect customer identifiers | PASS |  |
| ddl | query:except customer identifiers | PASS |  |
| ddl | query:order by an expression over an aggregate | PASS |  |
| ddl | query:order by a tree over several aggregates | PASS |  |
| ddl | query:order by an aggregate absent from the select list | PASS |  |
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
| ddl | query:general_ci: mixing collations across separate comparisons | PASS |  |
| ddl | query:enum: order by ascends by declared ordinal | PASS |  |
| ddl | query:enum: order by descends by declared ordinal | PASS |  |
| ddl | query:enum: min and max compare as strings | PASS |  |
| ddl | query:enum: a greater-than range compares as strings | PASS |  |
| ddl | query:enum: a less-than range compares as strings | PASS |  |
| ddl | query:enum: between compares as strings | PASS |  |
| ddl | query:enum: distinct orders by ordinal | PASS |  |
| ddl | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| ddl | query:enum: a window order walks the ordinal | PASS |  |
| ddl | query:collation: mixed grouping answers with per-key folds | PASS |  |
| ddl | query:collation: distinct counts fold per column collation | PASS |  |
| ddl | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| ddl | query:set: order by walks the member bitmask | PASS |  |
| ddl | query:set: grouping orders groups by bitmask | PASS |  |
| ddl | query:geometry: hex round-trips the internal format | PASS |  |
| schema-drift-minimal | converge:audit_log | PASS |  |
| schema-drift-minimal | converge:counters | PASS |  |
| schema-drift-minimal | converge:customers | PASS |  |
| schema-drift-minimal | converge:order_items | PASS |  |
| schema-drift-minimal | converge:orders | PASS |  |
| schema-drift-minimal | converge:shipments | PASS |  |
| schema-drift-minimal | converge:staff | PASS |  |
| schema-drift-minimal | converge:information_schema.columns | PASS |  |
| schema-drift-minimal | query:point lookup by key | PASS |  |
| schema-drift-minimal | query:range scan with compound predicate | PASS |  |
| schema-drift-minimal | query:inner join with aggregation | PASS |  |
| schema-drift-minimal | query:join with a residual comparison between both inputs | PASS |  |
| schema-drift-minimal | query:left join keeps rows whose only matches fail the residual | PASS |  |
| schema-drift-minimal | query:residual comparison through coalesce on a nullable column | PASS |  |
| schema-drift-minimal | query:created-by and updated-by resolve through separate aliases | PASS |  |
| schema-drift-minimal | query:alias pair with the join order reversed | PASS |  |
| schema-drift-minimal | query:four aliases of one table joined in a chain | PASS |  |
| schema-drift-minimal | query:self-join with a single-side predicate in the ON clause | PASS |  |
| schema-drift-minimal | query:self-join manager chain preserves the roots | PASS |  |
| schema-drift-minimal | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| schema-drift-minimal | query:aliases stay distinct when the empty side joins first | PASS |  |
| schema-drift-minimal | query:left join preserves unmatched rows | PASS |  |
| schema-drift-minimal | query:right join preserves unmatched rows | PASS |  |
| schema-drift-minimal | query:three-way join through items | PASS |  |
| schema-drift-minimal | query:union all across sources | PASS |  |
| schema-drift-minimal | query:intersect customer identifiers | PASS |  |
| schema-drift-minimal | query:except customer identifiers | PASS |  |
| schema-drift-minimal | query:order by an expression over an aggregate | PASS |  |
| schema-drift-minimal | query:order by a tree over several aggregates | PASS |  |
| schema-drift-minimal | query:order by an aggregate absent from the select list | PASS |  |
| schema-drift-minimal | query:group by with having | PASS |  |
| schema-drift-minimal | query:conditional decimal sum keeps the fraction | PASS |  |
| schema-drift-minimal | query:distinct count and min max | PASS |  |
| schema-drift-minimal | query:uncorrelated in-subquery | PASS |  |
| schema-drift-minimal | query:correlated exists with inner predicate | PASS |  |
| schema-drift-minimal | query:correlated scalar aggregate | PASS |  |
| schema-drift-minimal | query:correlated scalar unique lookup | PASS |  |
| schema-drift-minimal | query:scalar subquery threshold | PASS |  |
| schema-drift-minimal | query:non-recursive cte | PASS |  |
| schema-drift-minimal | query:bounded recursive cte | PASS |  |
| schema-drift-minimal | query:date bucketing | PASS |  |
| schema-drift-minimal | query:string functions and like | PASS |  |
| schema-drift-minimal | query:looker symmetric key helpers | PASS |  |
| schema-drift-minimal | query:json constructor preserves json versus text | PASS |  |
| schema-drift-minimal | query:json aggregate embeds documents | PASS |  |
| schema-drift-minimal | query:regular expression read transforms | PASS |  |
| schema-drift-minimal | query:case expression buckets | PASS |  |
| schema-drift-minimal | query:null handling | PASS |  |
| schema-drift-minimal | query:coalesce and ifnull | PASS |  |
| schema-drift-minimal | query:enum and set filters | PASS |  |
| schema-drift-minimal | query:unsigned boundary readback | PASS |  |
| schema-drift-minimal | query:derived table | PASS |  |
| schema-drift-minimal | query:group_concat single expression | PASS |  |
| schema-drift-minimal | query:window ranking per group | PASS |  |
| schema-drift-minimal | query:window share of total over grouped output | PASS |  |
| schema-drift-minimal | query:window running total | PASS |  |
| schema-drift-minimal | query:decimal column average beyond simple sum | PASS |  |
| schema-drift-minimal | query:json extract filter on customer meta | PASS |  |
| schema-drift-minimal | query:fan-out join group concat line products | PASS |  |
| schema-drift-minimal | query:outer join customers without recent orders | PASS |  |
| schema-drift-minimal | query:set op union distinct tiers and statuses | PASS |  |
| schema-drift-minimal | query:temporal convert and date_format grain | PASS |  |
| schema-drift-minimal | query:correlated not exists open orders | PASS |  |
| schema-drift-minimal | query:window lag payment-shaped totals | PASS |  |
| schema-drift-minimal | query:multi-key join items to orders | PASS |  |
| schema-drift-minimal | query:between and null-safe coalesce on balance | PASS |  |
| schema-drift-minimal | query:intersect all-style customer buyers | PASS |  |
| schema-drift-minimal | query:derived table status revenue share | PASS |  |
| schema-drift-minimal | query:general_ci: equality folds ASCII case | PASS |  |
| schema-drift-minimal | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| schema-drift-minimal | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| schema-drift-minimal | query:general_ci: every supplementary character compares equal | PASS |  |
| schema-drift-minimal | query:general_ci: grouping partitions by collated equality | PASS |  |
| schema-drift-minimal | query:general_ci: ordering follows the collation, not code points | PASS |  |
| schema-drift-minimal | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| schema-drift-minimal | query:general_ci: joining on a collated column | PASS |  |
| schema-drift-minimal | query:general_ci: representative spelling of a collated group | PASS |  |
| schema-drift-minimal | query:general_ci: mixing collations across separate comparisons | PASS |  |
| schema-drift-minimal | query:enum: order by ascends by declared ordinal | PASS |  |
| schema-drift-minimal | query:enum: order by descends by declared ordinal | PASS |  |
| schema-drift-minimal | query:enum: min and max compare as strings | PASS |  |
| schema-drift-minimal | query:enum: a greater-than range compares as strings | PASS |  |
| schema-drift-minimal | query:enum: a less-than range compares as strings | PASS |  |
| schema-drift-minimal | query:enum: between compares as strings | PASS |  |
| schema-drift-minimal | query:enum: distinct orders by ordinal | PASS |  |
| schema-drift-minimal | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| schema-drift-minimal | query:enum: a window order walks the ordinal | PASS |  |
| schema-drift-minimal | query:collation: mixed grouping answers with per-key folds | PASS |  |
| schema-drift-minimal | query:collation: distinct counts fold per column collation | PASS |  |
| schema-drift-minimal | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| schema-drift-minimal | query:set: order by walks the member bitmask | PASS |  |
| schema-drift-minimal | query:set: grouping orders groups by bitmask | PASS |  |
| schema-drift-minimal | query:geometry: hex round-trips the internal format | PASS |  |
| schema-drift-unseen | converge:audit_log | PASS |  |
| schema-drift-unseen | converge:counters | PASS |  |
| schema-drift-unseen | converge:customers | PASS |  |
| schema-drift-unseen | converge:order_items | PASS |  |
| schema-drift-unseen | converge:orders | PASS |  |
| schema-drift-unseen | converge:shipments | PASS |  |
| schema-drift-unseen | converge:staff | PASS |  |
| schema-drift-unseen | converge:information_schema.columns | PASS |  |
| schema-drift-unseen | query:point lookup by key | PASS |  |
| schema-drift-unseen | query:range scan with compound predicate | PASS |  |
| schema-drift-unseen | query:inner join with aggregation | PASS |  |
| schema-drift-unseen | query:join with a residual comparison between both inputs | PASS |  |
| schema-drift-unseen | query:left join keeps rows whose only matches fail the residual | PASS |  |
| schema-drift-unseen | query:residual comparison through coalesce on a nullable column | PASS |  |
| schema-drift-unseen | query:created-by and updated-by resolve through separate aliases | PASS |  |
| schema-drift-unseen | query:alias pair with the join order reversed | PASS |  |
| schema-drift-unseen | query:four aliases of one table joined in a chain | PASS |  |
| schema-drift-unseen | query:self-join with a single-side predicate in the ON clause | PASS |  |
| schema-drift-unseen | query:self-join manager chain preserves the roots | PASS |  |
| schema-drift-unseen | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| schema-drift-unseen | query:aliases stay distinct when the empty side joins first | PASS |  |
| schema-drift-unseen | query:left join preserves unmatched rows | PASS |  |
| schema-drift-unseen | query:right join preserves unmatched rows | PASS |  |
| schema-drift-unseen | query:three-way join through items | PASS |  |
| schema-drift-unseen | query:union all across sources | PASS |  |
| schema-drift-unseen | query:intersect customer identifiers | PASS |  |
| schema-drift-unseen | query:except customer identifiers | PASS |  |
| schema-drift-unseen | query:order by an expression over an aggregate | PASS |  |
| schema-drift-unseen | query:order by a tree over several aggregates | PASS |  |
| schema-drift-unseen | query:order by an aggregate absent from the select list | PASS |  |
| schema-drift-unseen | query:group by with having | PASS |  |
| schema-drift-unseen | query:conditional decimal sum keeps the fraction | PASS |  |
| schema-drift-unseen | query:distinct count and min max | PASS |  |
| schema-drift-unseen | query:uncorrelated in-subquery | PASS |  |
| schema-drift-unseen | query:correlated exists with inner predicate | PASS |  |
| schema-drift-unseen | query:correlated scalar aggregate | PASS |  |
| schema-drift-unseen | query:correlated scalar unique lookup | PASS |  |
| schema-drift-unseen | query:scalar subquery threshold | PASS |  |
| schema-drift-unseen | query:non-recursive cte | PASS |  |
| schema-drift-unseen | query:bounded recursive cte | PASS |  |
| schema-drift-unseen | query:date bucketing | PASS |  |
| schema-drift-unseen | query:string functions and like | PASS |  |
| schema-drift-unseen | query:looker symmetric key helpers | PASS |  |
| schema-drift-unseen | query:json constructor preserves json versus text | PASS |  |
| schema-drift-unseen | query:json aggregate embeds documents | PASS |  |
| schema-drift-unseen | query:regular expression read transforms | PASS |  |
| schema-drift-unseen | query:case expression buckets | PASS |  |
| schema-drift-unseen | query:null handling | PASS |  |
| schema-drift-unseen | query:coalesce and ifnull | PASS |  |
| schema-drift-unseen | query:enum and set filters | PASS |  |
| schema-drift-unseen | query:unsigned boundary readback | PASS |  |
| schema-drift-unseen | query:derived table | PASS |  |
| schema-drift-unseen | query:group_concat single expression | PASS |  |
| schema-drift-unseen | query:window ranking per group | PASS |  |
| schema-drift-unseen | query:window share of total over grouped output | PASS |  |
| schema-drift-unseen | query:window running total | PASS |  |
| schema-drift-unseen | query:decimal column average beyond simple sum | PASS |  |
| schema-drift-unseen | query:json extract filter on customer meta | PASS |  |
| schema-drift-unseen | query:fan-out join group concat line products | PASS |  |
| schema-drift-unseen | query:outer join customers without recent orders | PASS |  |
| schema-drift-unseen | query:set op union distinct tiers and statuses | PASS |  |
| schema-drift-unseen | query:temporal convert and date_format grain | PASS |  |
| schema-drift-unseen | query:correlated not exists open orders | PASS |  |
| schema-drift-unseen | query:window lag payment-shaped totals | PASS |  |
| schema-drift-unseen | query:multi-key join items to orders | PASS |  |
| schema-drift-unseen | query:between and null-safe coalesce on balance | PASS |  |
| schema-drift-unseen | query:intersect all-style customer buyers | PASS |  |
| schema-drift-unseen | query:derived table status revenue share | PASS |  |
| schema-drift-unseen | query:general_ci: equality folds ASCII case | PASS |  |
| schema-drift-unseen | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| schema-drift-unseen | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| schema-drift-unseen | query:general_ci: every supplementary character compares equal | PASS |  |
| schema-drift-unseen | query:general_ci: grouping partitions by collated equality | PASS |  |
| schema-drift-unseen | query:general_ci: ordering follows the collation, not code points | PASS |  |
| schema-drift-unseen | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| schema-drift-unseen | query:general_ci: joining on a collated column | PASS |  |
| schema-drift-unseen | query:general_ci: representative spelling of a collated group | PASS |  |
| schema-drift-unseen | query:general_ci: mixing collations across separate comparisons | PASS |  |
| schema-drift-unseen | query:enum: order by ascends by declared ordinal | PASS |  |
| schema-drift-unseen | query:enum: order by descends by declared ordinal | PASS |  |
| schema-drift-unseen | query:enum: min and max compare as strings | PASS |  |
| schema-drift-unseen | query:enum: a greater-than range compares as strings | PASS |  |
| schema-drift-unseen | query:enum: a less-than range compares as strings | PASS |  |
| schema-drift-unseen | query:enum: between compares as strings | PASS |  |
| schema-drift-unseen | query:enum: distinct orders by ordinal | PASS |  |
| schema-drift-unseen | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| schema-drift-unseen | query:enum: a window order walks the ordinal | PASS |  |
| schema-drift-unseen | query:collation: mixed grouping answers with per-key folds | PASS |  |
| schema-drift-unseen | query:collation: distinct counts fold per column collation | PASS |  |
| schema-drift-unseen | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| schema-drift-unseen | query:set: order by walks the member bitmask | PASS |  |
| schema-drift-unseen | query:set: grouping orders groups by bitmask | PASS |  |
| schema-drift-unseen | query:geometry: hex round-trips the internal format | PASS |  |
| churn-live | live:point lookup by key | PASS |  |
| churn-live | live:range scan with compound predicate | PASS |  |
| churn-live | live:inner join with aggregation | PASS |  |
| churn-live | live:join with a residual comparison between both inputs | PASS |  |
| churn-live | live:left join keeps rows whose only matches fail the residual | PASS |  |
| churn-live | live:residual comparison through coalesce on a nullable column | PASS |  |
| churn | converge:audit_log | PASS |  |
| churn | converge:counters | PASS |  |
| churn | converge:customers | PASS |  |
| churn | converge:order_items | PASS |  |
| churn | converge:orders | PASS |  |
| churn | converge:shipments | PASS |  |
| churn | converge:staff | PASS |  |
| churn | converge:information_schema.columns | PASS |  |
| churn | query:point lookup by key | PASS |  |
| churn | query:range scan with compound predicate | PASS |  |
| churn | query:inner join with aggregation | PASS |  |
| churn | query:join with a residual comparison between both inputs | PASS |  |
| churn | query:left join keeps rows whose only matches fail the residual | PASS |  |
| churn | query:residual comparison through coalesce on a nullable column | PASS |  |
| churn | query:created-by and updated-by resolve through separate aliases | PASS |  |
| churn | query:alias pair with the join order reversed | PASS |  |
| churn | query:four aliases of one table joined in a chain | PASS |  |
| churn | query:self-join with a single-side predicate in the ON clause | PASS |  |
| churn | query:self-join manager chain preserves the roots | PASS |  |
| churn | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| churn | query:aliases stay distinct when the empty side joins first | PASS |  |
| churn | query:left join preserves unmatched rows | PASS |  |
| churn | query:right join preserves unmatched rows | PASS |  |
| churn | query:three-way join through items | PASS |  |
| churn | query:union all across sources | PASS |  |
| churn | query:intersect customer identifiers | PASS |  |
| churn | query:except customer identifiers | PASS |  |
| churn | query:order by an expression over an aggregate | PASS |  |
| churn | query:order by a tree over several aggregates | PASS |  |
| churn | query:order by an aggregate absent from the select list | PASS |  |
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
| churn | query:general_ci: mixing collations across separate comparisons | PASS |  |
| churn | query:enum: order by ascends by declared ordinal | PASS |  |
| churn | query:enum: order by descends by declared ordinal | PASS |  |
| churn | query:enum: min and max compare as strings | PASS |  |
| churn | query:enum: a greater-than range compares as strings | PASS |  |
| churn | query:enum: a less-than range compares as strings | PASS |  |
| churn | query:enum: between compares as strings | PASS |  |
| churn | query:enum: distinct orders by ordinal | PASS |  |
| churn | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| churn | query:enum: a window order walks the ordinal | PASS |  |
| churn | query:collation: mixed grouping answers with per-key folds | PASS |  |
| churn | query:collation: distinct counts fold per column collation | PASS |  |
| churn | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| churn | query:set: order by walks the member bitmask | PASS |  |
| churn | query:set: grouping orders groups by bitmask | PASS |  |
| churn | query:geometry: hex round-trips the internal format | PASS |  |
| execution-budget | hint:interrupts a runaway join | PASS |  |
| execution-budget | hint:interrupts promptly | PASS |  |
| execution-budget | hint:a generous budget runs to completion | PASS |  |
| execution-budget | hint:cannot loosen the session ceiling | PASS |  |
| execution-budget | hint:an unimplemented hint rejects | PASS |  |
| execution-budget | converge:audit_log | PASS |  |
| execution-budget | converge:counters | PASS |  |
| execution-budget | converge:customers | PASS |  |
| execution-budget | converge:order_items | PASS |  |
| execution-budget | converge:orders | PASS |  |
| execution-budget | converge:shipments | PASS |  |
| execution-budget | converge:staff | PASS |  |
| execution-budget | converge:information_schema.columns | PASS |  |
| execution-budget | query:point lookup by key | PASS |  |
| execution-budget | query:range scan with compound predicate | PASS |  |
| execution-budget | query:inner join with aggregation | PASS |  |
| execution-budget | query:join with a residual comparison between both inputs | PASS |  |
| execution-budget | query:left join keeps rows whose only matches fail the residual | PASS |  |
| execution-budget | query:residual comparison through coalesce on a nullable column | PASS |  |
| execution-budget | query:created-by and updated-by resolve through separate aliases | PASS |  |
| execution-budget | query:alias pair with the join order reversed | PASS |  |
| execution-budget | query:four aliases of one table joined in a chain | PASS |  |
| execution-budget | query:self-join with a single-side predicate in the ON clause | PASS |  |
| execution-budget | query:self-join manager chain preserves the roots | PASS |  |
| execution-budget | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| execution-budget | query:aliases stay distinct when the empty side joins first | PASS |  |
| execution-budget | query:left join preserves unmatched rows | PASS |  |
| execution-budget | query:right join preserves unmatched rows | PASS |  |
| execution-budget | query:three-way join through items | PASS |  |
| execution-budget | query:union all across sources | PASS |  |
| execution-budget | query:intersect customer identifiers | PASS |  |
| execution-budget | query:except customer identifiers | PASS |  |
| execution-budget | query:order by an expression over an aggregate | PASS |  |
| execution-budget | query:order by a tree over several aggregates | PASS |  |
| execution-budget | query:order by an aggregate absent from the select list | PASS |  |
| execution-budget | query:group by with having | PASS |  |
| execution-budget | query:conditional decimal sum keeps the fraction | PASS |  |
| execution-budget | query:distinct count and min max | PASS |  |
| execution-budget | query:uncorrelated in-subquery | PASS |  |
| execution-budget | query:correlated exists with inner predicate | PASS |  |
| execution-budget | query:correlated scalar aggregate | PASS |  |
| execution-budget | query:correlated scalar unique lookup | PASS |  |
| execution-budget | query:scalar subquery threshold | PASS |  |
| execution-budget | query:non-recursive cte | PASS |  |
| execution-budget | query:bounded recursive cte | PASS |  |
| execution-budget | query:date bucketing | PASS |  |
| execution-budget | query:string functions and like | PASS |  |
| execution-budget | query:looker symmetric key helpers | PASS |  |
| execution-budget | query:json constructor preserves json versus text | PASS |  |
| execution-budget | query:json aggregate embeds documents | PASS |  |
| execution-budget | query:regular expression read transforms | PASS |  |
| execution-budget | query:case expression buckets | PASS |  |
| execution-budget | query:null handling | PASS |  |
| execution-budget | query:coalesce and ifnull | PASS |  |
| execution-budget | query:enum and set filters | PASS |  |
| execution-budget | query:unsigned boundary readback | PASS |  |
| execution-budget | query:derived table | PASS |  |
| execution-budget | query:group_concat single expression | PASS |  |
| execution-budget | query:window ranking per group | PASS |  |
| execution-budget | query:window share of total over grouped output | PASS |  |
| execution-budget | query:window running total | PASS |  |
| execution-budget | query:decimal column average beyond simple sum | PASS |  |
| execution-budget | query:json extract filter on customer meta | PASS |  |
| execution-budget | query:fan-out join group concat line products | PASS |  |
| execution-budget | query:outer join customers without recent orders | PASS |  |
| execution-budget | query:set op union distinct tiers and statuses | PASS |  |
| execution-budget | query:temporal convert and date_format grain | PASS |  |
| execution-budget | query:correlated not exists open orders | PASS |  |
| execution-budget | query:window lag payment-shaped totals | PASS |  |
| execution-budget | query:multi-key join items to orders | PASS |  |
| execution-budget | query:between and null-safe coalesce on balance | PASS |  |
| execution-budget | query:intersect all-style customer buyers | PASS |  |
| execution-budget | query:derived table status revenue share | PASS |  |
| execution-budget | query:general_ci: equality folds ASCII case | PASS |  |
| execution-budget | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| execution-budget | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| execution-budget | query:general_ci: every supplementary character compares equal | PASS |  |
| execution-budget | query:general_ci: grouping partitions by collated equality | PASS |  |
| execution-budget | query:general_ci: ordering follows the collation, not code points | PASS |  |
| execution-budget | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| execution-budget | query:general_ci: joining on a collated column | PASS |  |
| execution-budget | query:general_ci: representative spelling of a collated group | PASS |  |
| execution-budget | query:general_ci: mixing collations across separate comparisons | PASS |  |
| execution-budget | query:enum: order by ascends by declared ordinal | PASS |  |
| execution-budget | query:enum: order by descends by declared ordinal | PASS |  |
| execution-budget | query:enum: min and max compare as strings | PASS |  |
| execution-budget | query:enum: a greater-than range compares as strings | PASS |  |
| execution-budget | query:enum: a less-than range compares as strings | PASS |  |
| execution-budget | query:enum: between compares as strings | PASS |  |
| execution-budget | query:enum: distinct orders by ordinal | PASS |  |
| execution-budget | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| execution-budget | query:enum: a window order walks the ordinal | PASS |  |
| execution-budget | query:collation: mixed grouping answers with per-key folds | PASS |  |
| execution-budget | query:collation: distinct counts fold per column collation | PASS |  |
| execution-budget | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| execution-budget | query:set: order by walks the member bitmask | PASS |  |
| execution-budget | query:set: grouping orders groups by bitmask | PASS |  |
| execution-budget | query:geometry: hex round-trips the internal format | PASS |  |
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
| spill | converge:staff | PASS |  |
| spill | converge:information_schema.columns | PASS |  |
| spill | query:point lookup by key | PASS |  |
| spill | query:range scan with compound predicate | PASS |  |
| spill | query:inner join with aggregation | PASS |  |
| spill | query:join with a residual comparison between both inputs | PASS |  |
| spill | query:left join keeps rows whose only matches fail the residual | PASS |  |
| spill | query:residual comparison through coalesce on a nullable column | PASS |  |
| spill | query:created-by and updated-by resolve through separate aliases | PASS |  |
| spill | query:alias pair with the join order reversed | PASS |  |
| spill | query:four aliases of one table joined in a chain | PASS |  |
| spill | query:self-join with a single-side predicate in the ON clause | PASS |  |
| spill | query:self-join manager chain preserves the roots | PASS |  |
| spill | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| spill | query:aliases stay distinct when the empty side joins first | PASS |  |
| spill | query:left join preserves unmatched rows | PASS |  |
| spill | query:right join preserves unmatched rows | PASS |  |
| spill | query:three-way join through items | PASS |  |
| spill | query:union all across sources | PASS |  |
| spill | query:intersect customer identifiers | PASS |  |
| spill | query:except customer identifiers | PASS |  |
| spill | query:order by an expression over an aggregate | PASS |  |
| spill | query:order by a tree over several aggregates | PASS |  |
| spill | query:order by an aggregate absent from the select list | PASS |  |
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
| spill | query:general_ci: mixing collations across separate comparisons | PASS |  |
| spill | query:enum: order by ascends by declared ordinal | PASS |  |
| spill | query:enum: order by descends by declared ordinal | PASS |  |
| spill | query:enum: min and max compare as strings | PASS |  |
| spill | query:enum: a greater-than range compares as strings | PASS |  |
| spill | query:enum: a less-than range compares as strings | PASS |  |
| spill | query:enum: between compares as strings | PASS |  |
| spill | query:enum: distinct orders by ordinal | PASS |  |
| spill | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| spill | query:enum: a window order walks the ordinal | PASS |  |
| spill | query:collation: mixed grouping answers with per-key folds | PASS |  |
| spill | query:collation: distinct counts fold per column collation | PASS |  |
| spill | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| spill | query:set: order by walks the member bitmask | PASS |  |
| spill | query:set: grouping orders groups by bitmask | PASS |  |
| spill | query:geometry: hex round-trips the internal format | PASS |  |
| pooling | pool:concurrent-borrows(40 over 4) | PASS |  |
| pooling | pool:prepared-statements | PASS |  |
| pooling | pool:session-state-survives-borrow-like-mysql | PASS |  |
| pooling | converge:audit_log | PASS |  |
| pooling | converge:counters | PASS |  |
| pooling | converge:customers | PASS |  |
| pooling | converge:order_items | PASS |  |
| pooling | converge:orders | PASS |  |
| pooling | converge:shipments | PASS |  |
| pooling | converge:staff | PASS |  |
| pooling | converge:information_schema.columns | PASS |  |
| pooling | query:point lookup by key | PASS |  |
| pooling | query:range scan with compound predicate | PASS |  |
| pooling | query:inner join with aggregation | PASS |  |
| pooling | query:join with a residual comparison between both inputs | PASS |  |
| pooling | query:left join keeps rows whose only matches fail the residual | PASS |  |
| pooling | query:residual comparison through coalesce on a nullable column | PASS |  |
| pooling | query:created-by and updated-by resolve through separate aliases | PASS |  |
| pooling | query:alias pair with the join order reversed | PASS |  |
| pooling | query:four aliases of one table joined in a chain | PASS |  |
| pooling | query:self-join with a single-side predicate in the ON clause | PASS |  |
| pooling | query:self-join manager chain preserves the roots | PASS |  |
| pooling | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| pooling | query:aliases stay distinct when the empty side joins first | PASS |  |
| pooling | query:left join preserves unmatched rows | PASS |  |
| pooling | query:right join preserves unmatched rows | PASS |  |
| pooling | query:three-way join through items | PASS |  |
| pooling | query:union all across sources | PASS |  |
| pooling | query:intersect customer identifiers | PASS |  |
| pooling | query:except customer identifiers | PASS |  |
| pooling | query:order by an expression over an aggregate | PASS |  |
| pooling | query:order by a tree over several aggregates | PASS |  |
| pooling | query:order by an aggregate absent from the select list | PASS |  |
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
| pooling | query:general_ci: mixing collations across separate comparisons | PASS |  |
| pooling | query:enum: order by ascends by declared ordinal | PASS |  |
| pooling | query:enum: order by descends by declared ordinal | PASS |  |
| pooling | query:enum: min and max compare as strings | PASS |  |
| pooling | query:enum: a greater-than range compares as strings | PASS |  |
| pooling | query:enum: a less-than range compares as strings | PASS |  |
| pooling | query:enum: between compares as strings | PASS |  |
| pooling | query:enum: distinct orders by ordinal | PASS |  |
| pooling | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| pooling | query:enum: a window order walks the ordinal | PASS |  |
| pooling | query:collation: mixed grouping answers with per-key folds | PASS |  |
| pooling | query:collation: distinct counts fold per column collation | PASS |  |
| pooling | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| pooling | query:set: order by walks the member bitmask | PASS |  |
| pooling | query:set: grouping orders groups by bitmask | PASS |  |
| pooling | query:geometry: hex round-trips the internal format | PASS |  |
| restart | converge:audit_log | PASS |  |
| restart | converge:counters | PASS |  |
| restart | converge:customers | PASS |  |
| restart | converge:order_items | PASS |  |
| restart | converge:orders | PASS |  |
| restart | converge:shipments | PASS |  |
| restart | converge:staff | PASS |  |
| restart | converge:information_schema.columns | PASS |  |
| restart | query:point lookup by key | PASS |  |
| restart | query:range scan with compound predicate | PASS |  |
| restart | query:inner join with aggregation | PASS |  |
| restart | query:join with a residual comparison between both inputs | PASS |  |
| restart | query:left join keeps rows whose only matches fail the residual | PASS |  |
| restart | query:residual comparison through coalesce on a nullable column | PASS |  |
| restart | query:created-by and updated-by resolve through separate aliases | PASS |  |
| restart | query:alias pair with the join order reversed | PASS |  |
| restart | query:four aliases of one table joined in a chain | PASS |  |
| restart | query:self-join with a single-side predicate in the ON clause | PASS |  |
| restart | query:self-join manager chain preserves the roots | PASS |  |
| restart | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| restart | query:aliases stay distinct when the empty side joins first | PASS |  |
| restart | query:left join preserves unmatched rows | PASS |  |
| restart | query:right join preserves unmatched rows | PASS |  |
| restart | query:three-way join through items | PASS |  |
| restart | query:union all across sources | PASS |  |
| restart | query:intersect customer identifiers | PASS |  |
| restart | query:except customer identifiers | PASS |  |
| restart | query:order by an expression over an aggregate | PASS |  |
| restart | query:order by a tree over several aggregates | PASS |  |
| restart | query:order by an aggregate absent from the select list | PASS |  |
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
| restart | query:general_ci: mixing collations across separate comparisons | PASS |  |
| restart | query:enum: order by ascends by declared ordinal | PASS |  |
| restart | query:enum: order by descends by declared ordinal | PASS |  |
| restart | query:enum: min and max compare as strings | PASS |  |
| restart | query:enum: a greater-than range compares as strings | PASS |  |
| restart | query:enum: a less-than range compares as strings | PASS |  |
| restart | query:enum: between compares as strings | PASS |  |
| restart | query:enum: distinct orders by ordinal | PASS |  |
| restart | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| restart | query:enum: a window order walks the ordinal | PASS |  |
| restart | query:collation: mixed grouping answers with per-key folds | PASS |  |
| restart | query:collation: distinct counts fold per column collation | PASS |  |
| restart | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| restart | query:set: order by walks the member bitmask | PASS |  |
| restart | query:set: grouping orders groups by bitmask | PASS |  |
| restart | query:geometry: hex round-trips the internal format | PASS |  |
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
| control-plane | api:resync recopies only the table it names | PASS |  |
| control-plane | api:keyless policy: ambiguity quarantines and exact multiplicity repairs | PASS |  |
| control-plane | api:a connection string carrying client driver options registers | PASS |  |
| control-plane | api:throwaway database lifecycle: create, update, delete | PASS |  |
| control-plane | converge:audit_log | PASS |  |
| control-plane | converge:counters | PASS |  |
| control-plane | converge:customers | PASS |  |
| control-plane | converge:keyless_log | PASS |  |
| control-plane | converge:order_items | PASS |  |
| control-plane | converge:orders | PASS |  |
| control-plane | converge:shipments | PASS |  |
| control-plane | converge:staff | PASS |  |
| control-plane | converge:information_schema.columns | PASS |  |
| control-plane | query:point lookup by key | PASS |  |
| control-plane | query:range scan with compound predicate | PASS |  |
| control-plane | query:inner join with aggregation | PASS |  |
| control-plane | query:join with a residual comparison between both inputs | PASS |  |
| control-plane | query:left join keeps rows whose only matches fail the residual | PASS |  |
| control-plane | query:residual comparison through coalesce on a nullable column | PASS |  |
| control-plane | query:created-by and updated-by resolve through separate aliases | PASS |  |
| control-plane | query:alias pair with the join order reversed | PASS |  |
| control-plane | query:four aliases of one table joined in a chain | PASS |  |
| control-plane | query:self-join with a single-side predicate in the ON clause | PASS |  |
| control-plane | query:self-join manager chain preserves the roots | PASS |  |
| control-plane | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| control-plane | query:aliases stay distinct when the empty side joins first | PASS |  |
| control-plane | query:left join preserves unmatched rows | PASS |  |
| control-plane | query:right join preserves unmatched rows | PASS |  |
| control-plane | query:three-way join through items | PASS |  |
| control-plane | query:union all across sources | PASS |  |
| control-plane | query:intersect customer identifiers | PASS |  |
| control-plane | query:except customer identifiers | PASS |  |
| control-plane | query:order by an expression over an aggregate | PASS |  |
| control-plane | query:order by a tree over several aggregates | PASS |  |
| control-plane | query:order by an aggregate absent from the select list | PASS |  |
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
| control-plane | query:general_ci: mixing collations across separate comparisons | PASS |  |
| control-plane | query:enum: order by ascends by declared ordinal | PASS |  |
| control-plane | query:enum: order by descends by declared ordinal | PASS |  |
| control-plane | query:enum: min and max compare as strings | PASS |  |
| control-plane | query:enum: a greater-than range compares as strings | PASS |  |
| control-plane | query:enum: a less-than range compares as strings | PASS |  |
| control-plane | query:enum: between compares as strings | PASS |  |
| control-plane | query:enum: distinct orders by ordinal | PASS |  |
| control-plane | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| control-plane | query:enum: a window order walks the ordinal | PASS |  |
| control-plane | query:collation: mixed grouping answers with per-key folds | PASS |  |
| control-plane | query:collation: distinct counts fold per column collation | PASS |  |
| control-plane | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| control-plane | query:set: order by walks the member bitmask | PASS |  |
| control-plane | query:set: grouping orders groups by bitmask | PASS |  |
| control-plane | query:geometry: hex round-trips the internal format | PASS |  |
| snapshot-ddl-window | a table created just before a forced snapshot is still adopted | PASS |  |
| snapshot-ddl-window | converge:audit_log | PASS |  |
| snapshot-ddl-window | converge:counters | PASS |  |
| snapshot-ddl-window | converge:customers | PASS |  |
| snapshot-ddl-window | converge:keyless_log | PASS |  |
| snapshot-ddl-window | converge:order_items | PASS |  |
| snapshot-ddl-window | converge:orders | PASS |  |
| snapshot-ddl-window | converge:shipments | PASS |  |
| snapshot-ddl-window | converge:staff | PASS |  |
| snapshot-ddl-window | converge:information_schema.columns | PASS |  |
| snapshot-ddl-window | query:point lookup by key | PASS |  |
| snapshot-ddl-window | query:range scan with compound predicate | PASS |  |
| snapshot-ddl-window | query:inner join with aggregation | PASS |  |
| snapshot-ddl-window | query:join with a residual comparison between both inputs | PASS |  |
| snapshot-ddl-window | query:left join keeps rows whose only matches fail the residual | PASS |  |
| snapshot-ddl-window | query:residual comparison through coalesce on a nullable column | PASS |  |
| snapshot-ddl-window | query:created-by and updated-by resolve through separate aliases | PASS |  |
| snapshot-ddl-window | query:alias pair with the join order reversed | PASS |  |
| snapshot-ddl-window | query:four aliases of one table joined in a chain | PASS |  |
| snapshot-ddl-window | query:self-join with a single-side predicate in the ON clause | PASS |  |
| snapshot-ddl-window | query:self-join manager chain preserves the roots | PASS |  |
| snapshot-ddl-window | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| snapshot-ddl-window | query:aliases stay distinct when the empty side joins first | PASS |  |
| snapshot-ddl-window | query:left join preserves unmatched rows | PASS |  |
| snapshot-ddl-window | query:right join preserves unmatched rows | PASS |  |
| snapshot-ddl-window | query:three-way join through items | PASS |  |
| snapshot-ddl-window | query:union all across sources | PASS |  |
| snapshot-ddl-window | query:intersect customer identifiers | PASS |  |
| snapshot-ddl-window | query:except customer identifiers | PASS |  |
| snapshot-ddl-window | query:order by an expression over an aggregate | PASS |  |
| snapshot-ddl-window | query:order by a tree over several aggregates | PASS |  |
| snapshot-ddl-window | query:order by an aggregate absent from the select list | PASS |  |
| snapshot-ddl-window | query:group by with having | PASS |  |
| snapshot-ddl-window | query:conditional decimal sum keeps the fraction | PASS |  |
| snapshot-ddl-window | query:distinct count and min max | PASS |  |
| snapshot-ddl-window | query:uncorrelated in-subquery | PASS |  |
| snapshot-ddl-window | query:correlated exists with inner predicate | PASS |  |
| snapshot-ddl-window | query:correlated scalar aggregate | PASS |  |
| snapshot-ddl-window | query:correlated scalar unique lookup | PASS |  |
| snapshot-ddl-window | query:scalar subquery threshold | PASS |  |
| snapshot-ddl-window | query:non-recursive cte | PASS |  |
| snapshot-ddl-window | query:bounded recursive cte | PASS |  |
| snapshot-ddl-window | query:date bucketing | PASS |  |
| snapshot-ddl-window | query:string functions and like | PASS |  |
| snapshot-ddl-window | query:looker symmetric key helpers | PASS |  |
| snapshot-ddl-window | query:json constructor preserves json versus text | PASS |  |
| snapshot-ddl-window | query:json aggregate embeds documents | PASS |  |
| snapshot-ddl-window | query:regular expression read transforms | PASS |  |
| snapshot-ddl-window | query:case expression buckets | PASS |  |
| snapshot-ddl-window | query:null handling | PASS |  |
| snapshot-ddl-window | query:coalesce and ifnull | PASS |  |
| snapshot-ddl-window | query:enum and set filters | PASS |  |
| snapshot-ddl-window | query:unsigned boundary readback | PASS |  |
| snapshot-ddl-window | query:derived table | PASS |  |
| snapshot-ddl-window | query:group_concat single expression | PASS |  |
| snapshot-ddl-window | query:window ranking per group | PASS |  |
| snapshot-ddl-window | query:window share of total over grouped output | PASS |  |
| snapshot-ddl-window | query:window running total | PASS |  |
| snapshot-ddl-window | query:decimal column average beyond simple sum | PASS |  |
| snapshot-ddl-window | query:json extract filter on customer meta | PASS |  |
| snapshot-ddl-window | query:fan-out join group concat line products | PASS |  |
| snapshot-ddl-window | query:outer join customers without recent orders | PASS |  |
| snapshot-ddl-window | query:set op union distinct tiers and statuses | PASS |  |
| snapshot-ddl-window | query:temporal convert and date_format grain | PASS |  |
| snapshot-ddl-window | query:correlated not exists open orders | PASS |  |
| snapshot-ddl-window | query:window lag payment-shaped totals | PASS |  |
| snapshot-ddl-window | query:multi-key join items to orders | PASS |  |
| snapshot-ddl-window | query:between and null-safe coalesce on balance | PASS |  |
| snapshot-ddl-window | query:intersect all-style customer buyers | PASS |  |
| snapshot-ddl-window | query:derived table status revenue share | PASS |  |
| snapshot-ddl-window | query:general_ci: equality folds ASCII case | PASS |  |
| snapshot-ddl-window | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| snapshot-ddl-window | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| snapshot-ddl-window | query:general_ci: every supplementary character compares equal | PASS |  |
| snapshot-ddl-window | query:general_ci: grouping partitions by collated equality | PASS |  |
| snapshot-ddl-window | query:general_ci: ordering follows the collation, not code points | PASS |  |
| snapshot-ddl-window | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| snapshot-ddl-window | query:general_ci: joining on a collated column | PASS |  |
| snapshot-ddl-window | query:general_ci: representative spelling of a collated group | PASS |  |
| snapshot-ddl-window | query:general_ci: mixing collations across separate comparisons | PASS |  |
| snapshot-ddl-window | query:enum: order by ascends by declared ordinal | PASS |  |
| snapshot-ddl-window | query:enum: order by descends by declared ordinal | PASS |  |
| snapshot-ddl-window | query:enum: min and max compare as strings | PASS |  |
| snapshot-ddl-window | query:enum: a greater-than range compares as strings | PASS |  |
| snapshot-ddl-window | query:enum: a less-than range compares as strings | PASS |  |
| snapshot-ddl-window | query:enum: between compares as strings | PASS |  |
| snapshot-ddl-window | query:enum: distinct orders by ordinal | PASS |  |
| snapshot-ddl-window | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| snapshot-ddl-window | query:enum: a window order walks the ordinal | PASS |  |
| snapshot-ddl-window | query:collation: mixed grouping answers with per-key folds | PASS |  |
| snapshot-ddl-window | query:collation: distinct counts fold per column collation | PASS |  |
| snapshot-ddl-window | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| snapshot-ddl-window | query:set: order by walks the member bitmask | PASS |  |
| snapshot-ddl-window | query:set: grouping orders groups by bitmask | PASS |  |
| snapshot-ddl-window | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-cdc | drop-table:replicates before the drop | PASS |  |
| drop-table-cdc | drop-table:source drop marks the table orphaned | PASS |  |
| drop-table-cdc | drop-table:the rest of the database keeps replicating | PASS |  |
| drop-table-cdc | drop-table:orphan is retired without an operator re-probe | WARN | DROP TABLE retains the replica as an orphan and does not refresh the stored probe report, so the table stays in the replica catalog until an operator re-probes (3 rows still served) |
| drop-table-cdc | drop-table:re-probe retires the orphan from the catalog | PASS |  |
| drop-table-cdc | converge:audit_log | PASS |  |
| drop-table-cdc | converge:counters | PASS |  |
| drop-table-cdc | converge:customers | PASS |  |
| drop-table-cdc | converge:keyless_log | PASS |  |
| drop-table-cdc | converge:order_items | PASS |  |
| drop-table-cdc | converge:orders | PASS |  |
| drop-table-cdc | converge:shipments | PASS |  |
| drop-table-cdc | converge:staff | PASS |  |
| drop-table-cdc | converge:information_schema.columns | PASS |  |
| drop-table-cdc | query:point lookup by key | PASS |  |
| drop-table-cdc | query:range scan with compound predicate | PASS |  |
| drop-table-cdc | query:inner join with aggregation | PASS |  |
| drop-table-cdc | query:join with a residual comparison between both inputs | PASS |  |
| drop-table-cdc | query:left join keeps rows whose only matches fail the residual | PASS |  |
| drop-table-cdc | query:residual comparison through coalesce on a nullable column | PASS |  |
| drop-table-cdc | query:created-by and updated-by resolve through separate aliases | PASS |  |
| drop-table-cdc | query:alias pair with the join order reversed | PASS |  |
| drop-table-cdc | query:four aliases of one table joined in a chain | PASS |  |
| drop-table-cdc | query:self-join with a single-side predicate in the ON clause | PASS |  |
| drop-table-cdc | query:self-join manager chain preserves the roots | PASS |  |
| drop-table-cdc | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| drop-table-cdc | query:aliases stay distinct when the empty side joins first | PASS |  |
| drop-table-cdc | query:left join preserves unmatched rows | PASS |  |
| drop-table-cdc | query:right join preserves unmatched rows | PASS |  |
| drop-table-cdc | query:three-way join through items | PASS |  |
| drop-table-cdc | query:union all across sources | PASS |  |
| drop-table-cdc | query:intersect customer identifiers | PASS |  |
| drop-table-cdc | query:except customer identifiers | PASS |  |
| drop-table-cdc | query:order by an expression over an aggregate | PASS |  |
| drop-table-cdc | query:order by a tree over several aggregates | PASS |  |
| drop-table-cdc | query:order by an aggregate absent from the select list | PASS |  |
| drop-table-cdc | query:group by with having | PASS |  |
| drop-table-cdc | query:conditional decimal sum keeps the fraction | PASS |  |
| drop-table-cdc | query:distinct count and min max | PASS |  |
| drop-table-cdc | query:uncorrelated in-subquery | PASS |  |
| drop-table-cdc | query:correlated exists with inner predicate | PASS |  |
| drop-table-cdc | query:correlated scalar aggregate | PASS |  |
| drop-table-cdc | query:correlated scalar unique lookup | PASS |  |
| drop-table-cdc | query:scalar subquery threshold | PASS |  |
| drop-table-cdc | query:non-recursive cte | PASS |  |
| drop-table-cdc | query:bounded recursive cte | PASS |  |
| drop-table-cdc | query:date bucketing | PASS |  |
| drop-table-cdc | query:string functions and like | PASS |  |
| drop-table-cdc | query:looker symmetric key helpers | PASS |  |
| drop-table-cdc | query:json constructor preserves json versus text | PASS |  |
| drop-table-cdc | query:json aggregate embeds documents | PASS |  |
| drop-table-cdc | query:regular expression read transforms | PASS |  |
| drop-table-cdc | query:case expression buckets | PASS |  |
| drop-table-cdc | query:null handling | PASS |  |
| drop-table-cdc | query:coalesce and ifnull | PASS |  |
| drop-table-cdc | query:enum and set filters | PASS |  |
| drop-table-cdc | query:unsigned boundary readback | PASS |  |
| drop-table-cdc | query:derived table | PASS |  |
| drop-table-cdc | query:group_concat single expression | PASS |  |
| drop-table-cdc | query:window ranking per group | PASS |  |
| drop-table-cdc | query:window share of total over grouped output | PASS |  |
| drop-table-cdc | query:window running total | PASS |  |
| drop-table-cdc | query:decimal column average beyond simple sum | PASS |  |
| drop-table-cdc | query:json extract filter on customer meta | PASS |  |
| drop-table-cdc | query:fan-out join group concat line products | PASS |  |
| drop-table-cdc | query:outer join customers without recent orders | PASS |  |
| drop-table-cdc | query:set op union distinct tiers and statuses | PASS |  |
| drop-table-cdc | query:temporal convert and date_format grain | PASS |  |
| drop-table-cdc | query:correlated not exists open orders | PASS |  |
| drop-table-cdc | query:window lag payment-shaped totals | PASS |  |
| drop-table-cdc | query:multi-key join items to orders | PASS |  |
| drop-table-cdc | query:between and null-safe coalesce on balance | PASS |  |
| drop-table-cdc | query:intersect all-style customer buyers | PASS |  |
| drop-table-cdc | query:derived table status revenue share | PASS |  |
| drop-table-cdc | query:general_ci: equality folds ASCII case | PASS |  |
| drop-table-cdc | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| drop-table-cdc | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| drop-table-cdc | query:general_ci: every supplementary character compares equal | PASS |  |
| drop-table-cdc | query:general_ci: grouping partitions by collated equality | PASS |  |
| drop-table-cdc | query:general_ci: ordering follows the collation, not code points | PASS |  |
| drop-table-cdc | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| drop-table-cdc | query:general_ci: joining on a collated column | PASS |  |
| drop-table-cdc | query:general_ci: representative spelling of a collated group | PASS |  |
| drop-table-cdc | query:general_ci: mixing collations across separate comparisons | PASS |  |
| drop-table-cdc | query:enum: order by ascends by declared ordinal | PASS |  |
| drop-table-cdc | query:enum: order by descends by declared ordinal | PASS |  |
| drop-table-cdc | query:enum: min and max compare as strings | PASS |  |
| drop-table-cdc | query:enum: a greater-than range compares as strings | PASS |  |
| drop-table-cdc | query:enum: a less-than range compares as strings | PASS |  |
| drop-table-cdc | query:enum: between compares as strings | PASS |  |
| drop-table-cdc | query:enum: distinct orders by ordinal | PASS |  |
| drop-table-cdc | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| drop-table-cdc | query:enum: a window order walks the ordinal | PASS |  |
| drop-table-cdc | query:collation: mixed grouping answers with per-key folds | PASS |  |
| drop-table-cdc | query:collation: distinct counts fold per column collation | PASS |  |
| drop-table-cdc | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| drop-table-cdc | query:set: order by walks the member bitmask | PASS |  |
| drop-table-cdc | query:set: grouping orders groups by bitmask | PASS |  |
| drop-table-cdc | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-recreate | recreate:first generation replicates | PASS |  |
| drop-table-recreate | recreate:a table recreated under the same name replicates as a new table | WARN | the source has 2 rows and the replica 4: the orphaned store is reused instead of being resnapshotted, because the CREATE handler skips any name it already tracks |
| drop-table-recreate | recreate:the rest of the database keeps replicating | PASS |  |
| drop-table-recreate | converge:audit_log | PASS |  |
| drop-table-recreate | converge:counters | PASS |  |
| drop-table-recreate | converge:customers | PASS |  |
| drop-table-recreate | converge:keyless_log | PASS |  |
| drop-table-recreate | converge:order_items | PASS |  |
| drop-table-recreate | converge:orders | PASS |  |
| drop-table-recreate | converge:shipments | PASS |  |
| drop-table-recreate | converge:staff | PASS |  |
| drop-table-recreate | converge:information_schema.columns | PASS |  |
| drop-table-recreate | query:point lookup by key | PASS |  |
| drop-table-recreate | query:range scan with compound predicate | PASS |  |
| drop-table-recreate | query:inner join with aggregation | PASS |  |
| drop-table-recreate | query:join with a residual comparison between both inputs | PASS |  |
| drop-table-recreate | query:left join keeps rows whose only matches fail the residual | PASS |  |
| drop-table-recreate | query:residual comparison through coalesce on a nullable column | PASS |  |
| drop-table-recreate | query:created-by and updated-by resolve through separate aliases | PASS |  |
| drop-table-recreate | query:alias pair with the join order reversed | PASS |  |
| drop-table-recreate | query:four aliases of one table joined in a chain | PASS |  |
| drop-table-recreate | query:self-join with a single-side predicate in the ON clause | PASS |  |
| drop-table-recreate | query:self-join manager chain preserves the roots | PASS |  |
| drop-table-recreate | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| drop-table-recreate | query:aliases stay distinct when the empty side joins first | PASS |  |
| drop-table-recreate | query:left join preserves unmatched rows | PASS |  |
| drop-table-recreate | query:right join preserves unmatched rows | PASS |  |
| drop-table-recreate | query:three-way join through items | PASS |  |
| drop-table-recreate | query:union all across sources | PASS |  |
| drop-table-recreate | query:intersect customer identifiers | PASS |  |
| drop-table-recreate | query:except customer identifiers | PASS |  |
| drop-table-recreate | query:order by an expression over an aggregate | PASS |  |
| drop-table-recreate | query:order by a tree over several aggregates | PASS |  |
| drop-table-recreate | query:order by an aggregate absent from the select list | PASS |  |
| drop-table-recreate | query:group by with having | PASS |  |
| drop-table-recreate | query:conditional decimal sum keeps the fraction | PASS |  |
| drop-table-recreate | query:distinct count and min max | PASS |  |
| drop-table-recreate | query:uncorrelated in-subquery | PASS |  |
| drop-table-recreate | query:correlated exists with inner predicate | PASS |  |
| drop-table-recreate | query:correlated scalar aggregate | PASS |  |
| drop-table-recreate | query:correlated scalar unique lookup | PASS |  |
| drop-table-recreate | query:scalar subquery threshold | PASS |  |
| drop-table-recreate | query:non-recursive cte | PASS |  |
| drop-table-recreate | query:bounded recursive cte | PASS |  |
| drop-table-recreate | query:date bucketing | PASS |  |
| drop-table-recreate | query:string functions and like | PASS |  |
| drop-table-recreate | query:looker symmetric key helpers | PASS |  |
| drop-table-recreate | query:json constructor preserves json versus text | PASS |  |
| drop-table-recreate | query:json aggregate embeds documents | PASS |  |
| drop-table-recreate | query:regular expression read transforms | PASS |  |
| drop-table-recreate | query:case expression buckets | PASS |  |
| drop-table-recreate | query:null handling | PASS |  |
| drop-table-recreate | query:coalesce and ifnull | PASS |  |
| drop-table-recreate | query:enum and set filters | PASS |  |
| drop-table-recreate | query:unsigned boundary readback | PASS |  |
| drop-table-recreate | query:derived table | PASS |  |
| drop-table-recreate | query:group_concat single expression | PASS |  |
| drop-table-recreate | query:window ranking per group | PASS |  |
| drop-table-recreate | query:window share of total over grouped output | PASS |  |
| drop-table-recreate | query:window running total | PASS |  |
| drop-table-recreate | query:decimal column average beyond simple sum | PASS |  |
| drop-table-recreate | query:json extract filter on customer meta | PASS |  |
| drop-table-recreate | query:fan-out join group concat line products | PASS |  |
| drop-table-recreate | query:outer join customers without recent orders | PASS |  |
| drop-table-recreate | query:set op union distinct tiers and statuses | PASS |  |
| drop-table-recreate | query:temporal convert and date_format grain | PASS |  |
| drop-table-recreate | query:correlated not exists open orders | PASS |  |
| drop-table-recreate | query:window lag payment-shaped totals | PASS |  |
| drop-table-recreate | query:multi-key join items to orders | PASS |  |
| drop-table-recreate | query:between and null-safe coalesce on balance | PASS |  |
| drop-table-recreate | query:intersect all-style customer buyers | PASS |  |
| drop-table-recreate | query:derived table status revenue share | PASS |  |
| drop-table-recreate | query:general_ci: equality folds ASCII case | PASS |  |
| drop-table-recreate | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| drop-table-recreate | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| drop-table-recreate | query:general_ci: every supplementary character compares equal | PASS |  |
| drop-table-recreate | query:general_ci: grouping partitions by collated equality | PASS |  |
| drop-table-recreate | query:general_ci: ordering follows the collation, not code points | PASS |  |
| drop-table-recreate | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| drop-table-recreate | query:general_ci: joining on a collated column | PASS |  |
| drop-table-recreate | query:general_ci: representative spelling of a collated group | PASS |  |
| drop-table-recreate | query:general_ci: mixing collations across separate comparisons | PASS |  |
| drop-table-recreate | query:enum: order by ascends by declared ordinal | PASS |  |
| drop-table-recreate | query:enum: order by descends by declared ordinal | PASS |  |
| drop-table-recreate | query:enum: min and max compare as strings | PASS |  |
| drop-table-recreate | query:enum: a greater-than range compares as strings | PASS |  |
| drop-table-recreate | query:enum: a less-than range compares as strings | PASS |  |
| drop-table-recreate | query:enum: between compares as strings | PASS |  |
| drop-table-recreate | query:enum: distinct orders by ordinal | PASS |  |
| drop-table-recreate | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| drop-table-recreate | query:enum: a window order walks the ordinal | PASS |  |
| drop-table-recreate | query:collation: mixed grouping answers with per-key folds | PASS |  |
| drop-table-recreate | query:collation: distinct counts fold per column collation | PASS |  |
| drop-table-recreate | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| drop-table-recreate | query:set: order by walks the member bitmask | PASS |  |
| drop-table-recreate | query:set: grouping orders groups by bitmask | PASS |  |
| drop-table-recreate | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-polling | polling:fixtures replicate before the mode switch | PASS |  |
| drop-table-polling | polling:database is healthy before the drop | PASS |  |
| drop-table-polling | polling:TRUNCATE empties the replica | PASS |  |
| drop-table-polling | polling:one dropped table does not stop the other tables | WARN | the whole poll cycle aborts on the first table that fails, so every other table stops replicating too: {"database":{"id":"db_4598c7b25dd1b8ac2b32e70347c1bc07","name":"e2e_db","mode":"polling","effective_mode":"polling","state":"error","include_tables":[],"exclude_tables":[],"poll_interval_seconds":5,"reconcile_interval_seconds":600,"keyless_policy":"quarantine","created_at":"2026-08-19T13:08:05.890824+00:00","updated_at":"2026-08-19T13:15:33.077933+00:00"},"tables":13,"rows":794} |
| drop-table-polling | polling:re-probe restores replication for the surviving tables | PASS |  |
| drop-table-polling | converge:audit_log | PASS |  |
| drop-table-polling | converge:counters | PASS |  |
| drop-table-polling | converge:customers | PASS |  |
| drop-table-polling | converge:keyless_log | PASS |  |
| drop-table-polling | converge:order_items | PASS |  |
| drop-table-polling | converge:orders | PASS |  |
| drop-table-polling | converge:shipments | PASS |  |
| drop-table-polling | converge:staff | PASS |  |
| drop-table-polling | converge:information_schema.columns | PASS |  |
| drop-table-polling | query:point lookup by key | PASS |  |
| drop-table-polling | query:range scan with compound predicate | PASS |  |
| drop-table-polling | query:inner join with aggregation | PASS |  |
| drop-table-polling | query:join with a residual comparison between both inputs | PASS |  |
| drop-table-polling | query:left join keeps rows whose only matches fail the residual | PASS |  |
| drop-table-polling | query:residual comparison through coalesce on a nullable column | PASS |  |
| drop-table-polling | query:created-by and updated-by resolve through separate aliases | PASS |  |
| drop-table-polling | query:alias pair with the join order reversed | PASS |  |
| drop-table-polling | query:four aliases of one table joined in a chain | PASS |  |
| drop-table-polling | query:self-join with a single-side predicate in the ON clause | PASS |  |
| drop-table-polling | query:self-join manager chain preserves the roots | PASS |  |
| drop-table-polling | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| drop-table-polling | query:aliases stay distinct when the empty side joins first | PASS |  |
| drop-table-polling | query:left join preserves unmatched rows | PASS |  |
| drop-table-polling | query:right join preserves unmatched rows | PASS |  |
| drop-table-polling | query:three-way join through items | PASS |  |
| drop-table-polling | query:union all across sources | PASS |  |
| drop-table-polling | query:intersect customer identifiers | PASS |  |
| drop-table-polling | query:except customer identifiers | PASS |  |
| drop-table-polling | query:order by an expression over an aggregate | PASS |  |
| drop-table-polling | query:order by a tree over several aggregates | PASS |  |
| drop-table-polling | query:order by an aggregate absent from the select list | PASS |  |
| drop-table-polling | query:group by with having | PASS |  |
| drop-table-polling | query:conditional decimal sum keeps the fraction | PASS |  |
| drop-table-polling | query:distinct count and min max | PASS |  |
| drop-table-polling | query:uncorrelated in-subquery | PASS |  |
| drop-table-polling | query:correlated exists with inner predicate | PASS |  |
| drop-table-polling | query:correlated scalar aggregate | PASS |  |
| drop-table-polling | query:correlated scalar unique lookup | PASS |  |
| drop-table-polling | query:scalar subquery threshold | PASS |  |
| drop-table-polling | query:non-recursive cte | PASS |  |
| drop-table-polling | query:bounded recursive cte | PASS |  |
| drop-table-polling | query:date bucketing | PASS |  |
| drop-table-polling | query:string functions and like | PASS |  |
| drop-table-polling | query:looker symmetric key helpers | PASS |  |
| drop-table-polling | query:json constructor preserves json versus text | PASS |  |
| drop-table-polling | query:json aggregate embeds documents | PASS |  |
| drop-table-polling | query:regular expression read transforms | PASS |  |
| drop-table-polling | query:case expression buckets | PASS |  |
| drop-table-polling | query:null handling | PASS |  |
| drop-table-polling | query:coalesce and ifnull | PASS |  |
| drop-table-polling | query:enum and set filters | PASS |  |
| drop-table-polling | query:unsigned boundary readback | PASS |  |
| drop-table-polling | query:derived table | PASS |  |
| drop-table-polling | query:group_concat single expression | PASS |  |
| drop-table-polling | query:window ranking per group | PASS |  |
| drop-table-polling | query:window share of total over grouped output | PASS |  |
| drop-table-polling | query:window running total | PASS |  |
| drop-table-polling | query:decimal column average beyond simple sum | PASS |  |
| drop-table-polling | query:json extract filter on customer meta | PASS |  |
| drop-table-polling | query:fan-out join group concat line products | PASS |  |
| drop-table-polling | query:outer join customers without recent orders | PASS |  |
| drop-table-polling | query:set op union distinct tiers and statuses | PASS |  |
| drop-table-polling | query:temporal convert and date_format grain | PASS |  |
| drop-table-polling | query:correlated not exists open orders | PASS |  |
| drop-table-polling | query:window lag payment-shaped totals | PASS |  |
| drop-table-polling | query:multi-key join items to orders | PASS |  |
| drop-table-polling | query:between and null-safe coalesce on balance | PASS |  |
| drop-table-polling | query:intersect all-style customer buyers | PASS |  |
| drop-table-polling | query:derived table status revenue share | PASS |  |
| drop-table-polling | query:general_ci: equality folds ASCII case | PASS |  |
| drop-table-polling | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| drop-table-polling | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| drop-table-polling | query:general_ci: every supplementary character compares equal | PASS |  |
| drop-table-polling | query:general_ci: grouping partitions by collated equality | PASS |  |
| drop-table-polling | query:general_ci: ordering follows the collation, not code points | PASS |  |
| drop-table-polling | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| drop-table-polling | query:general_ci: joining on a collated column | PASS |  |
| drop-table-polling | query:general_ci: representative spelling of a collated group | PASS |  |
| drop-table-polling | query:general_ci: mixing collations across separate comparisons | PASS |  |
| drop-table-polling | query:enum: order by ascends by declared ordinal | PASS |  |
| drop-table-polling | query:enum: order by descends by declared ordinal | PASS |  |
| drop-table-polling | query:enum: min and max compare as strings | PASS |  |
| drop-table-polling | query:enum: a greater-than range compares as strings | PASS |  |
| drop-table-polling | query:enum: a less-than range compares as strings | PASS |  |
| drop-table-polling | query:enum: between compares as strings | PASS |  |
| drop-table-polling | query:enum: distinct orders by ordinal | PASS |  |
| drop-table-polling | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| drop-table-polling | query:enum: a window order walks the ordinal | PASS |  |
| drop-table-polling | query:collation: mixed grouping answers with per-key folds | PASS |  |
| drop-table-polling | query:collation: distinct counts fold per column collation | PASS |  |
| drop-table-polling | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| drop-table-polling | query:set: order by walks the member bitmask | PASS |  |
| drop-table-polling | query:set: grouping orders groups by bitmask | PASS |  |
| drop-table-polling | query:geometry: hex round-trips the internal format | PASS |  |
| drop-database | cross-schema:same-named table replicates first | PASS |  |
| drop-database | cross-schema:dropping another schema's table leaves this one replicating | PASS |  |
| drop-database | drop-database:second database snapshots | PASS |  |
| drop-database | drop-database:second database serves its rows | PASS |  |
| drop-database | drop-database:the deleted source is surfaced, not served silently | PASS |  |
| drop-database | drop-database:re-probing a deleted source fails loudly | PASS |  |
| drop-database | drop-database:polling reports the deleted source as an error | PASS |  |
| drop-database | drop-database:reads against a deleted source do not claim to be current | WARN | 3 rows are still served from the replica of a database MySQL no longer has, with nothing on the read path marking them stale |
| drop-database | converge:audit_log | PASS |  |
| drop-database | converge:counters | PASS |  |
| drop-database | converge:customers | PASS |  |
| drop-database | converge:keyless_log | PASS |  |
| drop-database | converge:order_items | PASS |  |
| drop-database | converge:orders | PASS |  |
| drop-database | converge:shipments | PASS |  |
| drop-database | converge:staff | PASS |  |
| drop-database | converge:information_schema.columns | PASS |  |
| drop-database | query:point lookup by key | PASS |  |
| drop-database | query:range scan with compound predicate | PASS |  |
| drop-database | query:inner join with aggregation | PASS |  |
| drop-database | query:join with a residual comparison between both inputs | PASS |  |
| drop-database | query:left join keeps rows whose only matches fail the residual | PASS |  |
| drop-database | query:residual comparison through coalesce on a nullable column | PASS |  |
| drop-database | query:created-by and updated-by resolve through separate aliases | PASS |  |
| drop-database | query:alias pair with the join order reversed | PASS |  |
| drop-database | query:four aliases of one table joined in a chain | PASS |  |
| drop-database | query:self-join with a single-side predicate in the ON clause | PASS |  |
| drop-database | query:self-join manager chain preserves the roots | PASS |  |
| drop-database | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| drop-database | query:aliases stay distinct when the empty side joins first | PASS |  |
| drop-database | query:left join preserves unmatched rows | PASS |  |
| drop-database | query:right join preserves unmatched rows | PASS |  |
| drop-database | query:three-way join through items | PASS |  |
| drop-database | query:union all across sources | PASS |  |
| drop-database | query:intersect customer identifiers | PASS |  |
| drop-database | query:except customer identifiers | PASS |  |
| drop-database | query:order by an expression over an aggregate | PASS |  |
| drop-database | query:order by a tree over several aggregates | PASS |  |
| drop-database | query:order by an aggregate absent from the select list | PASS |  |
| drop-database | query:group by with having | PASS |  |
| drop-database | query:conditional decimal sum keeps the fraction | PASS |  |
| drop-database | query:distinct count and min max | PASS |  |
| drop-database | query:uncorrelated in-subquery | PASS |  |
| drop-database | query:correlated exists with inner predicate | PASS |  |
| drop-database | query:correlated scalar aggregate | PASS |  |
| drop-database | query:correlated scalar unique lookup | PASS |  |
| drop-database | query:scalar subquery threshold | PASS |  |
| drop-database | query:non-recursive cte | PASS |  |
| drop-database | query:bounded recursive cte | PASS |  |
| drop-database | query:date bucketing | PASS |  |
| drop-database | query:string functions and like | PASS |  |
| drop-database | query:looker symmetric key helpers | PASS |  |
| drop-database | query:json constructor preserves json versus text | PASS |  |
| drop-database | query:json aggregate embeds documents | PASS |  |
| drop-database | query:regular expression read transforms | PASS |  |
| drop-database | query:case expression buckets | PASS |  |
| drop-database | query:null handling | PASS |  |
| drop-database | query:coalesce and ifnull | PASS |  |
| drop-database | query:enum and set filters | PASS |  |
| drop-database | query:unsigned boundary readback | PASS |  |
| drop-database | query:derived table | PASS |  |
| drop-database | query:group_concat single expression | PASS |  |
| drop-database | query:window ranking per group | PASS |  |
| drop-database | query:window share of total over grouped output | PASS |  |
| drop-database | query:window running total | PASS |  |
| drop-database | query:decimal column average beyond simple sum | PASS |  |
| drop-database | query:json extract filter on customer meta | PASS |  |
| drop-database | query:fan-out join group concat line products | PASS |  |
| drop-database | query:outer join customers without recent orders | PASS |  |
| drop-database | query:set op union distinct tiers and statuses | PASS |  |
| drop-database | query:temporal convert and date_format grain | PASS |  |
| drop-database | query:correlated not exists open orders | PASS |  |
| drop-database | query:window lag payment-shaped totals | PASS |  |
| drop-database | query:multi-key join items to orders | PASS |  |
| drop-database | query:between and null-safe coalesce on balance | PASS |  |
| drop-database | query:intersect all-style customer buyers | PASS |  |
| drop-database | query:derived table status revenue share | PASS |  |
| drop-database | query:general_ci: equality folds ASCII case | PASS |  |
| drop-database | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| drop-database | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| drop-database | query:general_ci: every supplementary character compares equal | PASS |  |
| drop-database | query:general_ci: grouping partitions by collated equality | PASS |  |
| drop-database | query:general_ci: ordering follows the collation, not code points | PASS |  |
| drop-database | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| drop-database | query:general_ci: joining on a collated column | PASS |  |
| drop-database | query:general_ci: representative spelling of a collated group | PASS |  |
| drop-database | query:general_ci: mixing collations across separate comparisons | PASS |  |
| drop-database | query:enum: order by ascends by declared ordinal | PASS |  |
| drop-database | query:enum: order by descends by declared ordinal | PASS |  |
| drop-database | query:enum: min and max compare as strings | PASS |  |
| drop-database | query:enum: a greater-than range compares as strings | PASS |  |
| drop-database | query:enum: a less-than range compares as strings | PASS |  |
| drop-database | query:enum: between compares as strings | PASS |  |
| drop-database | query:enum: distinct orders by ordinal | PASS |  |
| drop-database | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| drop-database | query:enum: a window order walks the ordinal | PASS |  |
| drop-database | query:collation: mixed grouping answers with per-key folds | PASS |  |
| drop-database | query:collation: distinct counts fold per column collation | PASS |  |
| drop-database | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| drop-database | query:set: order by walks the member bitmask | PASS |  |
| drop-database | query:set: grouping orders groups by bitmask | PASS |  |
| drop-database | query:geometry: hex round-trips the internal format | PASS |  |
| ddl-documented-gaps | converge:audit_history | WARN | pintail query failed: Error: unknown table e2e_db.audit_history |
| ddl-documented-gaps | converge:counters | PASS |  |
| ddl-documented-gaps | converge:customers | PASS |  |
| ddl-documented-gaps | converge:keyless_log | PASS |  |
| ddl-documented-gaps | converge:order_items | PASS |  |
| ddl-documented-gaps | converge:orders | PASS |  |
| ddl-documented-gaps | converge:shipments | PASS |  |
| ddl-documented-gaps | converge:staff | PASS |  |
| ddl-documented-gaps | converge:information_schema.columns | WARN | row 0: |
| ddl-documented-gaps | query:point lookup by key | PASS |  |
| ddl-documented-gaps | query:range scan with compound predicate | PASS |  |
| ddl-documented-gaps | query:inner join with aggregation | PASS |  |
| ddl-documented-gaps | query:join with a residual comparison between both inputs | SKIP |  |
| ddl-documented-gaps | query:left join keeps rows whose only matches fail the residual | SKIP |  |
| ddl-documented-gaps | query:residual comparison through coalesce on a nullable column | PASS |  |
| ddl-documented-gaps | query:created-by and updated-by resolve through separate aliases | PASS |  |
| ddl-documented-gaps | query:alias pair with the join order reversed | PASS |  |
| ddl-documented-gaps | query:four aliases of one table joined in a chain | PASS |  |
| ddl-documented-gaps | query:self-join with a single-side predicate in the ON clause | PASS |  |
| ddl-documented-gaps | query:self-join manager chain preserves the roots | PASS |  |
| ddl-documented-gaps | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| ddl-documented-gaps | query:aliases stay distinct when the empty side joins first | PASS |  |
| ddl-documented-gaps | query:left join preserves unmatched rows | PASS |  |
| ddl-documented-gaps | query:right join preserves unmatched rows | PASS |  |
| ddl-documented-gaps | query:three-way join through items | SKIP |  |
| ddl-documented-gaps | query:union all across sources | PASS |  |
| ddl-documented-gaps | query:intersect customer identifiers | PASS |  |
| ddl-documented-gaps | query:except customer identifiers | PASS |  |
| ddl-documented-gaps | query:order by an expression over an aggregate | PASS |  |
| ddl-documented-gaps | query:order by a tree over several aggregates | PASS |  |
| ddl-documented-gaps | query:order by an aggregate absent from the select list | PASS |  |
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
| ddl-documented-gaps | query:general_ci: mixing collations across separate comparisons | PASS |  |
| ddl-documented-gaps | query:enum: order by ascends by declared ordinal | PASS |  |
| ddl-documented-gaps | query:enum: order by descends by declared ordinal | PASS |  |
| ddl-documented-gaps | query:enum: min and max compare as strings | PASS |  |
| ddl-documented-gaps | query:enum: a greater-than range compares as strings | PASS |  |
| ddl-documented-gaps | query:enum: a less-than range compares as strings | PASS |  |
| ddl-documented-gaps | query:enum: between compares as strings | PASS |  |
| ddl-documented-gaps | query:enum: distinct orders by ordinal | PASS |  |
| ddl-documented-gaps | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| ddl-documented-gaps | query:enum: a window order walks the ordinal | PASS |  |
| ddl-documented-gaps | query:collation: mixed grouping answers with per-key folds | PASS |  |
| ddl-documented-gaps | query:collation: distinct counts fold per column collation | PASS |  |
| ddl-documented-gaps | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| ddl-documented-gaps | query:set: order by walks the member bitmask | PASS |  |
| ddl-documented-gaps | query:set: grouping orders groups by bitmask | PASS |  |
| ddl-documented-gaps | query:geometry: hex round-trips the internal format | PASS |  |

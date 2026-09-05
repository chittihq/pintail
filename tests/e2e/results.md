# Pintail end-to-end differential gate

Measured 2026-09-05T09:57:49.829Z.

Source: `mysql:8.4` (server 8.4.11), `binlog_row_metadata=MINIMAL`, fresh container.

**4913 passed, 0 failed, 56 documented-gap warnings, 37 skipped.**

167 unique corpus queries produced 4509 corpus checks across phases; the remaining checks are convergence, battery, and control-plane assertions.

| Phase | Check | Status | Detail |
|---|---|---|---|
| snapshot | converge:Dim | PASS |  |
| snapshot | converge:Event | PASS |  |
| snapshot | converge:Fact | PASS |  |
| snapshot | converge:Person | PASS |  |
| snapshot | converge:audit_log | PASS |  |
| snapshot | converge:badges | PASS |  |
| snapshot | converge:counters | PASS |  |
| snapshot | converge:customers | PASS |  |
| snapshot | converge:order_items | PASS |  |
| snapshot | converge:orders | PASS |  |
| snapshot | converge:staff | PASS |  |
| snapshot | converge:information_schema.columns | PASS |  |
| snapshot | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| snapshot | query:conformance: mixed-collation double grouping | PASS |  |
| snapshot | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| snapshot | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| snapshot | query:conformance: case-variant code grouping | PASS |  |
| snapshot | query:conformance: anti-join finds the event-less dimension | PASS |  |
| snapshot | query:conformance: nullable join key NULL-extends | PASS |  |
| snapshot | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| snapshot | query:conformance: date bucketing over the fact table | PASS |  |
| snapshot | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| snapshot | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| snapshot | query:enum: the empty member groups by its ordinal | PASS |  |
| snapshot | query:enum: the empty member sorts by its ordinal | PASS |  |
| snapshot | query:enum: the empty member is selectable by text | PASS |  |
| snapshot | query:geometry: hex round-trips the internal format | SKIP |  |
| snapshot | query:geometry: byte length includes the srid prefix | SKIP |  |
| snapshot | query:geometry: null routes filter and count | SKIP |  |
| snapshot | query:geometry: spatial functions are a documented gap | SKIP |  |
| snapshot | query:set: find_in_set filters by membership | PASS |  |
| snapshot | query:set: equality is literal, not member-normalized | PASS |  |
| snapshot | query:set: distinct values walk the bitmask including empty | PASS |  |
| snapshot | query:set: grouped counts order by bitmask not text | PASS |  |
| snapshot | query:set: a range predicate compares the bitmask | PASS |  |
| snapshot | query:star: fact with dimension and two audit persons | PASS |  |
| snapshot | query:star: five-alias chain fans out through events | PASS |  |
| snapshot | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| snapshot | query:star: five tables bridge the shop and the star | PASS |  |
| snapshot | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| snapshot | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| snapshot | query:json: length and keys survive null documents | PASS |  |
| snapshot | query:json: contains_path filters the documented rows | PASS |  |
| snapshot | query:json: json_value reads a scalar with sql semantics | PASS |  |
| snapshot | query:json: object construction embeds an extracted scalar | PASS |  |
| snapshot | query:json: search locates a literal value | PASS |  |
| snapshot | query:json: grouping by an extracted scalar | PASS |  |
| snapshot | query:json: merge_patch overlays and reads back | PASS |  |
| snapshot | query:temporal: quarter, weekday and name grains agree | PASS |  |
| snapshot | query:temporal: month-end bucketing via last_day | PASS |  |
| snapshot | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| snapshot | query:temporal: datetime range keeps the year window | PASS |  |
| snapshot | query:temporal: date_sub bound in the predicate | PASS |  |
| snapshot | query:temporal: year-month split grouping | PASS |  |
| snapshot | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| snapshot | query:regex: substr extracts the mail domain | PASS |  |
| snapshot | query:regex: the REGEXP operator anchors a class | PASS |  |
| snapshot | query:regex: replace folds suffix classes before grouping | PASS |  |
| snapshot | query:bi metabase: month grain through convert_tz | PASS |  |
| snapshot | query:bi metabase: iso week bucketing | PASS |  |
| snapshot | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| snapshot | query:bi metabase: previous-period revenue window | PASS |  |
| snapshot | query:bi superset: week-start grain with a rolling average | PASS |  |
| snapshot | query:bi superset: running total over grouped revenue | PASS |  |
| snapshot | query:bi superset: lag and lead against a named window | PASS |  |
| snapshot | query:bi superset: quartile counts from ntile | PASS |  |
| snapshot | query:bi superset: first and last value over an unbounded frame | PASS |  |
| snapshot | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| snapshot | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| snapshot | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| snapshot | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| snapshot | query:bi looker: the grouped primary key determines the row | PASS |  |
| snapshot | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| snapshot | query:bi tableau: explicit cast ladder | PASS |  |
| snapshot | query:bi tableau: the stddev and variance family | PASS |  |
| snapshot | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| snapshot | query:bi shared: substring_index dimension cleanup | PASS |  |
| snapshot | query:bi shared: json validity and typed path filter | PASS |  |
| snapshot | query:bi shared: contains_path over several paths at once | PASS |  |
| snapshot | query:bi shared: maketime from extracted parts | PASS |  |
| snapshot | query:bi shared: extract year_month grouping | PASS |  |
| snapshot | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| snapshot | query:staff: three-level management chain with an inactive tail | PASS |  |
| snapshot | query:staff: active split with id extremes | PASS |  |
| snapshot | query:counters: full unsigned ladder readback | PASS |  |
| snapshot | query:counters: greatest and least across widths | PASS |  |
| snapshot | query:dim: enum status split | PASS |  |
| snapshot | query:dim: pattern filter across collated columns | PASS |  |
| snapshot | query:person: anti-join finds owners without facts | PASS |  |
| snapshot | query:person: created-fact counts through a scalar subquery | PASS |  |
| snapshot | query:event: lag over per-dimension timelines | PASS |  |
| snapshot | query:event: daily grain per dimension code | PASS |  |
| snapshot | query:order_items: product rollup without the orders table | PASS |  |
| snapshot | query:shipments: carrier value through the items bridge | SKIP |  |
| snapshot | query:json: distinct case variants survive a derived table | PASS |  |
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
| orm-compat | converge:Dim | PASS |  |
| orm-compat | converge:Event | PASS |  |
| orm-compat | converge:Fact | PASS |  |
| orm-compat | converge:Person | PASS |  |
| orm-compat | converge:audit_log | PASS |  |
| orm-compat | converge:badges | PASS |  |
| orm-compat | converge:counters | PASS |  |
| orm-compat | converge:customers | PASS |  |
| orm-compat | converge:order_items | PASS |  |
| orm-compat | converge:orders | PASS |  |
| orm-compat | converge:staff | PASS |  |
| orm-compat | converge:information_schema.columns | PASS |  |
| orm-compat | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| orm-compat | query:conformance: mixed-collation double grouping | PASS |  |
| orm-compat | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| orm-compat | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| orm-compat | query:conformance: case-variant code grouping | PASS |  |
| orm-compat | query:conformance: anti-join finds the event-less dimension | PASS |  |
| orm-compat | query:conformance: nullable join key NULL-extends | PASS |  |
| orm-compat | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| orm-compat | query:conformance: date bucketing over the fact table | PASS |  |
| orm-compat | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| orm-compat | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| orm-compat | query:enum: the empty member groups by its ordinal | PASS |  |
| orm-compat | query:enum: the empty member sorts by its ordinal | PASS |  |
| orm-compat | query:enum: the empty member is selectable by text | PASS |  |
| orm-compat | query:geometry: hex round-trips the internal format | SKIP |  |
| orm-compat | query:geometry: byte length includes the srid prefix | SKIP |  |
| orm-compat | query:geometry: null routes filter and count | SKIP |  |
| orm-compat | query:geometry: spatial functions are a documented gap | SKIP |  |
| orm-compat | query:set: find_in_set filters by membership | PASS |  |
| orm-compat | query:set: equality is literal, not member-normalized | PASS |  |
| orm-compat | query:set: distinct values walk the bitmask including empty | PASS |  |
| orm-compat | query:set: grouped counts order by bitmask not text | PASS |  |
| orm-compat | query:set: a range predicate compares the bitmask | PASS |  |
| orm-compat | query:star: fact with dimension and two audit persons | PASS |  |
| orm-compat | query:star: five-alias chain fans out through events | PASS |  |
| orm-compat | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| orm-compat | query:star: five tables bridge the shop and the star | PASS |  |
| orm-compat | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| orm-compat | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| orm-compat | query:json: length and keys survive null documents | PASS |  |
| orm-compat | query:json: contains_path filters the documented rows | PASS |  |
| orm-compat | query:json: json_value reads a scalar with sql semantics | PASS |  |
| orm-compat | query:json: object construction embeds an extracted scalar | PASS |  |
| orm-compat | query:json: search locates a literal value | PASS |  |
| orm-compat | query:json: grouping by an extracted scalar | PASS |  |
| orm-compat | query:json: merge_patch overlays and reads back | PASS |  |
| orm-compat | query:temporal: quarter, weekday and name grains agree | PASS |  |
| orm-compat | query:temporal: month-end bucketing via last_day | PASS |  |
| orm-compat | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| orm-compat | query:temporal: datetime range keeps the year window | PASS |  |
| orm-compat | query:temporal: date_sub bound in the predicate | PASS |  |
| orm-compat | query:temporal: year-month split grouping | PASS |  |
| orm-compat | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| orm-compat | query:regex: substr extracts the mail domain | PASS |  |
| orm-compat | query:regex: the REGEXP operator anchors a class | PASS |  |
| orm-compat | query:regex: replace folds suffix classes before grouping | PASS |  |
| orm-compat | query:bi metabase: month grain through convert_tz | PASS |  |
| orm-compat | query:bi metabase: iso week bucketing | PASS |  |
| orm-compat | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| orm-compat | query:bi metabase: previous-period revenue window | PASS |  |
| orm-compat | query:bi superset: week-start grain with a rolling average | PASS |  |
| orm-compat | query:bi superset: running total over grouped revenue | PASS |  |
| orm-compat | query:bi superset: lag and lead against a named window | PASS |  |
| orm-compat | query:bi superset: quartile counts from ntile | PASS |  |
| orm-compat | query:bi superset: first and last value over an unbounded frame | PASS |  |
| orm-compat | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| orm-compat | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| orm-compat | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| orm-compat | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| orm-compat | query:bi looker: the grouped primary key determines the row | PASS |  |
| orm-compat | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| orm-compat | query:bi tableau: explicit cast ladder | PASS |  |
| orm-compat | query:bi tableau: the stddev and variance family | PASS |  |
| orm-compat | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| orm-compat | query:bi shared: substring_index dimension cleanup | PASS |  |
| orm-compat | query:bi shared: json validity and typed path filter | PASS |  |
| orm-compat | query:bi shared: contains_path over several paths at once | PASS |  |
| orm-compat | query:bi shared: maketime from extracted parts | PASS |  |
| orm-compat | query:bi shared: extract year_month grouping | PASS |  |
| orm-compat | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| orm-compat | query:staff: three-level management chain with an inactive tail | PASS |  |
| orm-compat | query:staff: active split with id extremes | PASS |  |
| orm-compat | query:counters: full unsigned ladder readback | PASS |  |
| orm-compat | query:counters: greatest and least across widths | PASS |  |
| orm-compat | query:dim: enum status split | PASS |  |
| orm-compat | query:dim: pattern filter across collated columns | PASS |  |
| orm-compat | query:person: anti-join finds owners without facts | PASS |  |
| orm-compat | query:person: created-fact counts through a scalar subquery | PASS |  |
| orm-compat | query:event: lag over per-dimension timelines | PASS |  |
| orm-compat | query:event: daily grain per dimension code | PASS |  |
| orm-compat | query:order_items: product rollup without the orders table | PASS |  |
| orm-compat | query:shipments: carrier value through the items bridge | SKIP |  |
| orm-compat | query:json: distinct case variants survive a derived table | PASS |  |
| crud | converge:Dim | PASS |  |
| crud | converge:Event | PASS |  |
| crud | converge:Fact | PASS |  |
| crud | converge:Person | PASS |  |
| crud | converge:audit_log | PASS |  |
| crud | converge:badges | PASS |  |
| crud | converge:counters | PASS |  |
| crud | converge:customers | PASS |  |
| crud | converge:order_items | PASS |  |
| crud | converge:orders | PASS |  |
| crud | converge:staff | PASS |  |
| crud | converge:information_schema.columns | PASS |  |
| crud | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| crud | query:conformance: mixed-collation double grouping | PASS |  |
| crud | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| crud | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| crud | query:conformance: case-variant code grouping | PASS |  |
| crud | query:conformance: anti-join finds the event-less dimension | PASS |  |
| crud | query:conformance: nullable join key NULL-extends | PASS |  |
| crud | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| crud | query:conformance: date bucketing over the fact table | PASS |  |
| crud | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| crud | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| crud | query:enum: the empty member groups by its ordinal | PASS |  |
| crud | query:enum: the empty member sorts by its ordinal | PASS |  |
| crud | query:enum: the empty member is selectable by text | PASS |  |
| crud | query:geometry: hex round-trips the internal format | SKIP |  |
| crud | query:geometry: byte length includes the srid prefix | SKIP |  |
| crud | query:geometry: null routes filter and count | SKIP |  |
| crud | query:geometry: spatial functions are a documented gap | SKIP |  |
| crud | query:set: find_in_set filters by membership | PASS |  |
| crud | query:set: equality is literal, not member-normalized | PASS |  |
| crud | query:set: distinct values walk the bitmask including empty | PASS |  |
| crud | query:set: grouped counts order by bitmask not text | PASS |  |
| crud | query:set: a range predicate compares the bitmask | PASS |  |
| crud | query:star: fact with dimension and two audit persons | PASS |  |
| crud | query:star: five-alias chain fans out through events | PASS |  |
| crud | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| crud | query:star: five tables bridge the shop and the star | PASS |  |
| crud | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| crud | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| crud | query:json: length and keys survive null documents | PASS |  |
| crud | query:json: contains_path filters the documented rows | PASS |  |
| crud | query:json: json_value reads a scalar with sql semantics | PASS |  |
| crud | query:json: object construction embeds an extracted scalar | PASS |  |
| crud | query:json: search locates a literal value | PASS |  |
| crud | query:json: grouping by an extracted scalar | PASS |  |
| crud | query:json: merge_patch overlays and reads back | PASS |  |
| crud | query:temporal: quarter, weekday and name grains agree | PASS |  |
| crud | query:temporal: month-end bucketing via last_day | PASS |  |
| crud | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| crud | query:temporal: datetime range keeps the year window | PASS |  |
| crud | query:temporal: date_sub bound in the predicate | PASS |  |
| crud | query:temporal: year-month split grouping | PASS |  |
| crud | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| crud | query:regex: substr extracts the mail domain | PASS |  |
| crud | query:regex: the REGEXP operator anchors a class | PASS |  |
| crud | query:regex: replace folds suffix classes before grouping | PASS |  |
| crud | query:bi metabase: month grain through convert_tz | PASS |  |
| crud | query:bi metabase: iso week bucketing | PASS |  |
| crud | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| crud | query:bi metabase: previous-period revenue window | PASS |  |
| crud | query:bi superset: week-start grain with a rolling average | PASS |  |
| crud | query:bi superset: running total over grouped revenue | PASS |  |
| crud | query:bi superset: lag and lead against a named window | PASS |  |
| crud | query:bi superset: quartile counts from ntile | PASS |  |
| crud | query:bi superset: first and last value over an unbounded frame | PASS |  |
| crud | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| crud | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| crud | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| crud | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| crud | query:bi looker: the grouped primary key determines the row | PASS |  |
| crud | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| crud | query:bi tableau: explicit cast ladder | PASS |  |
| crud | query:bi tableau: the stddev and variance family | PASS |  |
| crud | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| crud | query:bi shared: substring_index dimension cleanup | PASS |  |
| crud | query:bi shared: json validity and typed path filter | PASS |  |
| crud | query:bi shared: contains_path over several paths at once | PASS |  |
| crud | query:bi shared: maketime from extracted parts | PASS |  |
| crud | query:bi shared: extract year_month grouping | PASS |  |
| crud | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| crud | query:staff: three-level management chain with an inactive tail | PASS |  |
| crud | query:staff: active split with id extremes | PASS |  |
| crud | query:counters: full unsigned ladder readback | PASS |  |
| crud | query:counters: greatest and least across widths | PASS |  |
| crud | query:dim: enum status split | PASS |  |
| crud | query:dim: pattern filter across collated columns | PASS |  |
| crud | query:person: anti-join finds owners without facts | PASS |  |
| crud | query:person: created-fact counts through a scalar subquery | PASS |  |
| crud | query:event: lag over per-dimension timelines | PASS |  |
| crud | query:event: daily grain per dimension code | PASS |  |
| crud | query:order_items: product rollup without the orders table | PASS |  |
| crud | query:shipments: carrier value through the items bridge | SKIP |  |
| crud | query:json: distinct case variants survive a derived table | PASS |  |
| type-edges | converge:Dim | PASS |  |
| type-edges | converge:Event | PASS |  |
| type-edges | converge:Fact | PASS |  |
| type-edges | converge:Person | PASS |  |
| type-edges | converge:audit_log | PASS |  |
| type-edges | converge:badges | PASS |  |
| type-edges | converge:counters | PASS |  |
| type-edges | converge:customers | PASS |  |
| type-edges | converge:order_items | PASS |  |
| type-edges | converge:orders | PASS |  |
| type-edges | converge:staff | PASS |  |
| type-edges | converge:information_schema.columns | PASS |  |
| type-edges | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| type-edges | query:conformance: mixed-collation double grouping | PASS |  |
| type-edges | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| type-edges | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| type-edges | query:conformance: case-variant code grouping | PASS |  |
| type-edges | query:conformance: anti-join finds the event-less dimension | PASS |  |
| type-edges | query:conformance: nullable join key NULL-extends | PASS |  |
| type-edges | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| type-edges | query:conformance: date bucketing over the fact table | PASS |  |
| type-edges | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| type-edges | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| type-edges | query:enum: the empty member groups by its ordinal | PASS |  |
| type-edges | query:enum: the empty member sorts by its ordinal | PASS |  |
| type-edges | query:enum: the empty member is selectable by text | PASS |  |
| type-edges | query:geometry: hex round-trips the internal format | SKIP |  |
| type-edges | query:geometry: byte length includes the srid prefix | SKIP |  |
| type-edges | query:geometry: null routes filter and count | SKIP |  |
| type-edges | query:geometry: spatial functions are a documented gap | SKIP |  |
| type-edges | query:set: find_in_set filters by membership | PASS |  |
| type-edges | query:set: equality is literal, not member-normalized | PASS |  |
| type-edges | query:set: distinct values walk the bitmask including empty | PASS |  |
| type-edges | query:set: grouped counts order by bitmask not text | PASS |  |
| type-edges | query:set: a range predicate compares the bitmask | PASS |  |
| type-edges | query:star: fact with dimension and two audit persons | PASS |  |
| type-edges | query:star: five-alias chain fans out through events | PASS |  |
| type-edges | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| type-edges | query:star: five tables bridge the shop and the star | PASS |  |
| type-edges | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| type-edges | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| type-edges | query:json: length and keys survive null documents | PASS |  |
| type-edges | query:json: contains_path filters the documented rows | PASS |  |
| type-edges | query:json: json_value reads a scalar with sql semantics | PASS |  |
| type-edges | query:json: object construction embeds an extracted scalar | PASS |  |
| type-edges | query:json: search locates a literal value | PASS |  |
| type-edges | query:json: grouping by an extracted scalar | PASS |  |
| type-edges | query:json: merge_patch overlays and reads back | PASS |  |
| type-edges | query:temporal: quarter, weekday and name grains agree | PASS |  |
| type-edges | query:temporal: month-end bucketing via last_day | PASS |  |
| type-edges | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| type-edges | query:temporal: datetime range keeps the year window | PASS |  |
| type-edges | query:temporal: date_sub bound in the predicate | PASS |  |
| type-edges | query:temporal: year-month split grouping | PASS |  |
| type-edges | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| type-edges | query:regex: substr extracts the mail domain | PASS |  |
| type-edges | query:regex: the REGEXP operator anchors a class | PASS |  |
| type-edges | query:regex: replace folds suffix classes before grouping | PASS |  |
| type-edges | query:bi metabase: month grain through convert_tz | PASS |  |
| type-edges | query:bi metabase: iso week bucketing | PASS |  |
| type-edges | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| type-edges | query:bi metabase: previous-period revenue window | PASS |  |
| type-edges | query:bi superset: week-start grain with a rolling average | PASS |  |
| type-edges | query:bi superset: running total over grouped revenue | PASS |  |
| type-edges | query:bi superset: lag and lead against a named window | PASS |  |
| type-edges | query:bi superset: quartile counts from ntile | PASS |  |
| type-edges | query:bi superset: first and last value over an unbounded frame | PASS |  |
| type-edges | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| type-edges | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| type-edges | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| type-edges | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| type-edges | query:bi looker: the grouped primary key determines the row | PASS |  |
| type-edges | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| type-edges | query:bi tableau: explicit cast ladder | PASS |  |
| type-edges | query:bi tableau: the stddev and variance family | PASS |  |
| type-edges | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| type-edges | query:bi shared: substring_index dimension cleanup | PASS |  |
| type-edges | query:bi shared: json validity and typed path filter | PASS |  |
| type-edges | query:bi shared: contains_path over several paths at once | PASS |  |
| type-edges | query:bi shared: maketime from extracted parts | PASS |  |
| type-edges | query:bi shared: extract year_month grouping | PASS |  |
| type-edges | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| type-edges | query:staff: three-level management chain with an inactive tail | PASS |  |
| type-edges | query:staff: active split with id extremes | PASS |  |
| type-edges | query:counters: full unsigned ladder readback | PASS |  |
| type-edges | query:counters: greatest and least across widths | PASS |  |
| type-edges | query:dim: enum status split | PASS |  |
| type-edges | query:dim: pattern filter across collated columns | PASS |  |
| type-edges | query:person: anti-join finds owners without facts | PASS |  |
| type-edges | query:person: created-fact counts through a scalar subquery | PASS |  |
| type-edges | query:event: lag over per-dimension timelines | PASS |  |
| type-edges | query:event: daily grain per dimension code | PASS |  |
| type-edges | query:order_items: product rollup without the orders table | PASS |  |
| type-edges | query:shipments: carrier value through the items bridge | SKIP |  |
| type-edges | query:json: distinct case variants survive a derived table | PASS |  |
| ddl | a virtual column added mid-stream is recopied with its values | PASS |  |
| ddl | converge:Dim | PASS |  |
| ddl | converge:Event | PASS |  |
| ddl | converge:Fact | PASS |  |
| ddl | converge:Person | PASS |  |
| ddl | converge:audit_log | PASS |  |
| ddl | converge:badges | PASS |  |
| ddl | converge:counters | PASS |  |
| ddl | converge:customers | PASS |  |
| ddl | converge:order_items | PASS |  |
| ddl | converge:orders | PASS |  |
| ddl | converge:shipments | PASS |  |
| ddl | converge:staff | PASS |  |
| ddl | converge:information_schema.columns | PASS |  |
| ddl | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| ddl | query:conformance: mixed-collation double grouping | PASS |  |
| ddl | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| ddl | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| ddl | query:conformance: case-variant code grouping | PASS |  |
| ddl | query:conformance: anti-join finds the event-less dimension | PASS |  |
| ddl | query:conformance: nullable join key NULL-extends | PASS |  |
| ddl | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| ddl | query:conformance: date bucketing over the fact table | PASS |  |
| ddl | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| ddl | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| ddl | query:enum: the empty member groups by its ordinal | PASS |  |
| ddl | query:enum: the empty member sorts by its ordinal | PASS |  |
| ddl | query:enum: the empty member is selectable by text | PASS |  |
| ddl | query:geometry: hex round-trips the internal format | PASS |  |
| ddl | query:geometry: byte length includes the srid prefix | PASS |  |
| ddl | query:geometry: null routes filter and count | PASS |  |
| ddl | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| ddl | query:set: find_in_set filters by membership | PASS |  |
| ddl | query:set: equality is literal, not member-normalized | PASS |  |
| ddl | query:set: distinct values walk the bitmask including empty | PASS |  |
| ddl | query:set: grouped counts order by bitmask not text | PASS |  |
| ddl | query:set: a range predicate compares the bitmask | PASS |  |
| ddl | query:star: fact with dimension and two audit persons | PASS |  |
| ddl | query:star: five-alias chain fans out through events | PASS |  |
| ddl | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| ddl | query:star: five tables bridge the shop and the star | PASS |  |
| ddl | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| ddl | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| ddl | query:json: length and keys survive null documents | PASS |  |
| ddl | query:json: contains_path filters the documented rows | PASS |  |
| ddl | query:json: json_value reads a scalar with sql semantics | PASS |  |
| ddl | query:json: object construction embeds an extracted scalar | PASS |  |
| ddl | query:json: search locates a literal value | PASS |  |
| ddl | query:json: grouping by an extracted scalar | PASS |  |
| ddl | query:json: merge_patch overlays and reads back | PASS |  |
| ddl | query:temporal: quarter, weekday and name grains agree | PASS |  |
| ddl | query:temporal: month-end bucketing via last_day | PASS |  |
| ddl | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| ddl | query:temporal: datetime range keeps the year window | PASS |  |
| ddl | query:temporal: date_sub bound in the predicate | PASS |  |
| ddl | query:temporal: year-month split grouping | PASS |  |
| ddl | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| ddl | query:regex: substr extracts the mail domain | PASS |  |
| ddl | query:regex: the REGEXP operator anchors a class | PASS |  |
| ddl | query:regex: replace folds suffix classes before grouping | PASS |  |
| ddl | query:bi metabase: month grain through convert_tz | PASS |  |
| ddl | query:bi metabase: iso week bucketing | PASS |  |
| ddl | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| ddl | query:bi metabase: previous-period revenue window | PASS |  |
| ddl | query:bi superset: week-start grain with a rolling average | PASS |  |
| ddl | query:bi superset: running total over grouped revenue | PASS |  |
| ddl | query:bi superset: lag and lead against a named window | PASS |  |
| ddl | query:bi superset: quartile counts from ntile | PASS |  |
| ddl | query:bi superset: first and last value over an unbounded frame | PASS |  |
| ddl | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| ddl | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| ddl | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| ddl | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| ddl | query:bi looker: the grouped primary key determines the row | PASS |  |
| ddl | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| ddl | query:bi tableau: explicit cast ladder | PASS |  |
| ddl | query:bi tableau: the stddev and variance family | PASS |  |
| ddl | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| ddl | query:bi shared: substring_index dimension cleanup | PASS |  |
| ddl | query:bi shared: json validity and typed path filter | PASS |  |
| ddl | query:bi shared: contains_path over several paths at once | PASS |  |
| ddl | query:bi shared: maketime from extracted parts | PASS |  |
| ddl | query:bi shared: extract year_month grouping | PASS |  |
| ddl | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| ddl | query:staff: three-level management chain with an inactive tail | PASS |  |
| ddl | query:staff: active split with id extremes | PASS |  |
| ddl | query:counters: full unsigned ladder readback | PASS |  |
| ddl | query:counters: greatest and least across widths | PASS |  |
| ddl | query:dim: enum status split | PASS |  |
| ddl | query:dim: pattern filter across collated columns | PASS |  |
| ddl | query:person: anti-join finds owners without facts | PASS |  |
| ddl | query:person: created-fact counts through a scalar subquery | PASS |  |
| ddl | query:event: lag over per-dimension timelines | PASS |  |
| ddl | query:event: daily grain per dimension code | PASS |  |
| ddl | query:order_items: product rollup without the orders table | PASS |  |
| ddl | query:shipments: carrier value through the items bridge | PASS |  |
| ddl | query:json: distinct case variants survive a derived table | PASS |  |
| schema-drift-minimal | converge:Dim | PASS |  |
| schema-drift-minimal | converge:Event | PASS |  |
| schema-drift-minimal | converge:Fact | PASS |  |
| schema-drift-minimal | converge:Person | PASS |  |
| schema-drift-minimal | converge:audit_log | PASS |  |
| schema-drift-minimal | converge:badges | PASS |  |
| schema-drift-minimal | converge:counters | PASS |  |
| schema-drift-minimal | converge:customers | PASS |  |
| schema-drift-minimal | converge:order_items | PASS |  |
| schema-drift-minimal | converge:orders | PASS |  |
| schema-drift-minimal | converge:shipments | PASS |  |
| schema-drift-minimal | converge:staff | PASS |  |
| schema-drift-minimal | converge:information_schema.columns | PASS |  |
| schema-drift-minimal | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| schema-drift-minimal | query:conformance: mixed-collation double grouping | PASS |  |
| schema-drift-minimal | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| schema-drift-minimal | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| schema-drift-minimal | query:conformance: case-variant code grouping | PASS |  |
| schema-drift-minimal | query:conformance: anti-join finds the event-less dimension | PASS |  |
| schema-drift-minimal | query:conformance: nullable join key NULL-extends | PASS |  |
| schema-drift-minimal | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| schema-drift-minimal | query:conformance: date bucketing over the fact table | PASS |  |
| schema-drift-minimal | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| schema-drift-minimal | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| schema-drift-minimal | query:enum: the empty member groups by its ordinal | PASS |  |
| schema-drift-minimal | query:enum: the empty member sorts by its ordinal | PASS |  |
| schema-drift-minimal | query:enum: the empty member is selectable by text | PASS |  |
| schema-drift-minimal | query:geometry: hex round-trips the internal format | PASS |  |
| schema-drift-minimal | query:geometry: byte length includes the srid prefix | PASS |  |
| schema-drift-minimal | query:geometry: null routes filter and count | PASS |  |
| schema-drift-minimal | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| schema-drift-minimal | query:set: find_in_set filters by membership | PASS |  |
| schema-drift-minimal | query:set: equality is literal, not member-normalized | PASS |  |
| schema-drift-minimal | query:set: distinct values walk the bitmask including empty | PASS |  |
| schema-drift-minimal | query:set: grouped counts order by bitmask not text | PASS |  |
| schema-drift-minimal | query:set: a range predicate compares the bitmask | PASS |  |
| schema-drift-minimal | query:star: fact with dimension and two audit persons | PASS |  |
| schema-drift-minimal | query:star: five-alias chain fans out through events | PASS |  |
| schema-drift-minimal | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| schema-drift-minimal | query:star: five tables bridge the shop and the star | PASS |  |
| schema-drift-minimal | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| schema-drift-minimal | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| schema-drift-minimal | query:json: length and keys survive null documents | PASS |  |
| schema-drift-minimal | query:json: contains_path filters the documented rows | PASS |  |
| schema-drift-minimal | query:json: json_value reads a scalar with sql semantics | PASS |  |
| schema-drift-minimal | query:json: object construction embeds an extracted scalar | PASS |  |
| schema-drift-minimal | query:json: search locates a literal value | PASS |  |
| schema-drift-minimal | query:json: grouping by an extracted scalar | PASS |  |
| schema-drift-minimal | query:json: merge_patch overlays and reads back | PASS |  |
| schema-drift-minimal | query:temporal: quarter, weekday and name grains agree | PASS |  |
| schema-drift-minimal | query:temporal: month-end bucketing via last_day | PASS |  |
| schema-drift-minimal | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| schema-drift-minimal | query:temporal: datetime range keeps the year window | PASS |  |
| schema-drift-minimal | query:temporal: date_sub bound in the predicate | PASS |  |
| schema-drift-minimal | query:temporal: year-month split grouping | PASS |  |
| schema-drift-minimal | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| schema-drift-minimal | query:regex: substr extracts the mail domain | PASS |  |
| schema-drift-minimal | query:regex: the REGEXP operator anchors a class | PASS |  |
| schema-drift-minimal | query:regex: replace folds suffix classes before grouping | PASS |  |
| schema-drift-minimal | query:bi metabase: month grain through convert_tz | PASS |  |
| schema-drift-minimal | query:bi metabase: iso week bucketing | PASS |  |
| schema-drift-minimal | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| schema-drift-minimal | query:bi metabase: previous-period revenue window | PASS |  |
| schema-drift-minimal | query:bi superset: week-start grain with a rolling average | PASS |  |
| schema-drift-minimal | query:bi superset: running total over grouped revenue | PASS |  |
| schema-drift-minimal | query:bi superset: lag and lead against a named window | PASS |  |
| schema-drift-minimal | query:bi superset: quartile counts from ntile | PASS |  |
| schema-drift-minimal | query:bi superset: first and last value over an unbounded frame | PASS |  |
| schema-drift-minimal | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| schema-drift-minimal | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| schema-drift-minimal | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| schema-drift-minimal | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| schema-drift-minimal | query:bi looker: the grouped primary key determines the row | PASS |  |
| schema-drift-minimal | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| schema-drift-minimal | query:bi tableau: explicit cast ladder | PASS |  |
| schema-drift-minimal | query:bi tableau: the stddev and variance family | PASS |  |
| schema-drift-minimal | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| schema-drift-minimal | query:bi shared: substring_index dimension cleanup | PASS |  |
| schema-drift-minimal | query:bi shared: json validity and typed path filter | PASS |  |
| schema-drift-minimal | query:bi shared: contains_path over several paths at once | PASS |  |
| schema-drift-minimal | query:bi shared: maketime from extracted parts | PASS |  |
| schema-drift-minimal | query:bi shared: extract year_month grouping | PASS |  |
| schema-drift-minimal | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| schema-drift-minimal | query:staff: three-level management chain with an inactive tail | PASS |  |
| schema-drift-minimal | query:staff: active split with id extremes | PASS |  |
| schema-drift-minimal | query:counters: full unsigned ladder readback | PASS |  |
| schema-drift-minimal | query:counters: greatest and least across widths | PASS |  |
| schema-drift-minimal | query:dim: enum status split | PASS |  |
| schema-drift-minimal | query:dim: pattern filter across collated columns | PASS |  |
| schema-drift-minimal | query:person: anti-join finds owners without facts | PASS |  |
| schema-drift-minimal | query:person: created-fact counts through a scalar subquery | PASS |  |
| schema-drift-minimal | query:event: lag over per-dimension timelines | PASS |  |
| schema-drift-minimal | query:event: daily grain per dimension code | PASS |  |
| schema-drift-minimal | query:order_items: product rollup without the orders table | PASS |  |
| schema-drift-minimal | query:shipments: carrier value through the items bridge | PASS |  |
| schema-drift-minimal | query:json: distinct case variants survive a derived table | PASS |  |
| schema-drift-unseen | converge:Dim | PASS |  |
| schema-drift-unseen | converge:Event | PASS |  |
| schema-drift-unseen | converge:Fact | PASS |  |
| schema-drift-unseen | converge:Person | PASS |  |
| schema-drift-unseen | converge:audit_log | PASS |  |
| schema-drift-unseen | converge:badges | PASS |  |
| schema-drift-unseen | converge:counters | PASS |  |
| schema-drift-unseen | converge:customers | PASS |  |
| schema-drift-unseen | converge:order_items | PASS |  |
| schema-drift-unseen | converge:orders | PASS |  |
| schema-drift-unseen | converge:shipments | PASS |  |
| schema-drift-unseen | converge:staff | PASS |  |
| schema-drift-unseen | converge:information_schema.columns | PASS |  |
| schema-drift-unseen | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| schema-drift-unseen | query:conformance: mixed-collation double grouping | PASS |  |
| schema-drift-unseen | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| schema-drift-unseen | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| schema-drift-unseen | query:conformance: case-variant code grouping | PASS |  |
| schema-drift-unseen | query:conformance: anti-join finds the event-less dimension | PASS |  |
| schema-drift-unseen | query:conformance: nullable join key NULL-extends | PASS |  |
| schema-drift-unseen | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| schema-drift-unseen | query:conformance: date bucketing over the fact table | PASS |  |
| schema-drift-unseen | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| schema-drift-unseen | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| schema-drift-unseen | query:enum: the empty member groups by its ordinal | PASS |  |
| schema-drift-unseen | query:enum: the empty member sorts by its ordinal | PASS |  |
| schema-drift-unseen | query:enum: the empty member is selectable by text | PASS |  |
| schema-drift-unseen | query:geometry: hex round-trips the internal format | PASS |  |
| schema-drift-unseen | query:geometry: byte length includes the srid prefix | PASS |  |
| schema-drift-unseen | query:geometry: null routes filter and count | PASS |  |
| schema-drift-unseen | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| schema-drift-unseen | query:set: find_in_set filters by membership | PASS |  |
| schema-drift-unseen | query:set: equality is literal, not member-normalized | PASS |  |
| schema-drift-unseen | query:set: distinct values walk the bitmask including empty | PASS |  |
| schema-drift-unseen | query:set: grouped counts order by bitmask not text | PASS |  |
| schema-drift-unseen | query:set: a range predicate compares the bitmask | PASS |  |
| schema-drift-unseen | query:star: fact with dimension and two audit persons | PASS |  |
| schema-drift-unseen | query:star: five-alias chain fans out through events | PASS |  |
| schema-drift-unseen | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| schema-drift-unseen | query:star: five tables bridge the shop and the star | PASS |  |
| schema-drift-unseen | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| schema-drift-unseen | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| schema-drift-unseen | query:json: length and keys survive null documents | PASS |  |
| schema-drift-unseen | query:json: contains_path filters the documented rows | PASS |  |
| schema-drift-unseen | query:json: json_value reads a scalar with sql semantics | PASS |  |
| schema-drift-unseen | query:json: object construction embeds an extracted scalar | PASS |  |
| schema-drift-unseen | query:json: search locates a literal value | PASS |  |
| schema-drift-unseen | query:json: grouping by an extracted scalar | PASS |  |
| schema-drift-unseen | query:json: merge_patch overlays and reads back | PASS |  |
| schema-drift-unseen | query:temporal: quarter, weekday and name grains agree | PASS |  |
| schema-drift-unseen | query:temporal: month-end bucketing via last_day | PASS |  |
| schema-drift-unseen | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| schema-drift-unseen | query:temporal: datetime range keeps the year window | PASS |  |
| schema-drift-unseen | query:temporal: date_sub bound in the predicate | PASS |  |
| schema-drift-unseen | query:temporal: year-month split grouping | PASS |  |
| schema-drift-unseen | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| schema-drift-unseen | query:regex: substr extracts the mail domain | PASS |  |
| schema-drift-unseen | query:regex: the REGEXP operator anchors a class | PASS |  |
| schema-drift-unseen | query:regex: replace folds suffix classes before grouping | PASS |  |
| schema-drift-unseen | query:bi metabase: month grain through convert_tz | PASS |  |
| schema-drift-unseen | query:bi metabase: iso week bucketing | PASS |  |
| schema-drift-unseen | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| schema-drift-unseen | query:bi metabase: previous-period revenue window | PASS |  |
| schema-drift-unseen | query:bi superset: week-start grain with a rolling average | PASS |  |
| schema-drift-unseen | query:bi superset: running total over grouped revenue | PASS |  |
| schema-drift-unseen | query:bi superset: lag and lead against a named window | PASS |  |
| schema-drift-unseen | query:bi superset: quartile counts from ntile | PASS |  |
| schema-drift-unseen | query:bi superset: first and last value over an unbounded frame | PASS |  |
| schema-drift-unseen | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| schema-drift-unseen | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| schema-drift-unseen | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| schema-drift-unseen | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| schema-drift-unseen | query:bi looker: the grouped primary key determines the row | PASS |  |
| schema-drift-unseen | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| schema-drift-unseen | query:bi tableau: explicit cast ladder | PASS |  |
| schema-drift-unseen | query:bi tableau: the stddev and variance family | PASS |  |
| schema-drift-unseen | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| schema-drift-unseen | query:bi shared: substring_index dimension cleanup | PASS |  |
| schema-drift-unseen | query:bi shared: json validity and typed path filter | PASS |  |
| schema-drift-unseen | query:bi shared: contains_path over several paths at once | PASS |  |
| schema-drift-unseen | query:bi shared: maketime from extracted parts | PASS |  |
| schema-drift-unseen | query:bi shared: extract year_month grouping | PASS |  |
| schema-drift-unseen | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| schema-drift-unseen | query:staff: three-level management chain with an inactive tail | PASS |  |
| schema-drift-unseen | query:staff: active split with id extremes | PASS |  |
| schema-drift-unseen | query:counters: full unsigned ladder readback | PASS |  |
| schema-drift-unseen | query:counters: greatest and least across widths | PASS |  |
| schema-drift-unseen | query:dim: enum status split | PASS |  |
| schema-drift-unseen | query:dim: pattern filter across collated columns | PASS |  |
| schema-drift-unseen | query:person: anti-join finds owners without facts | PASS |  |
| schema-drift-unseen | query:person: created-fact counts through a scalar subquery | PASS |  |
| schema-drift-unseen | query:event: lag over per-dimension timelines | PASS |  |
| schema-drift-unseen | query:event: daily grain per dimension code | PASS |  |
| schema-drift-unseen | query:order_items: product rollup without the orders table | PASS |  |
| schema-drift-unseen | query:shipments: carrier value through the items bridge | PASS |  |
| schema-drift-unseen | query:json: distinct case variants survive a derived table | PASS |  |
| churn-live | live:conformance: triple-alias person join with a dangling FK | PASS |  |
| churn-live | live:conformance: mixed-collation double grouping | PASS |  |
| churn-live | live:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| churn-live | live:conformance: trailing-space grouping under PAD semantics | PASS |  |
| churn-live | live:conformance: case-variant code grouping | PASS |  |
| churn-live | live:conformance: anti-join finds the event-less dimension | PASS |  |
| churn | converge:Dim | PASS |  |
| churn | converge:Event | PASS |  |
| churn | converge:Fact | PASS |  |
| churn | converge:Person | PASS |  |
| churn | converge:audit_log | PASS |  |
| churn | converge:badges | PASS |  |
| churn | converge:counters | PASS |  |
| churn | converge:customers | PASS |  |
| churn | converge:order_items | PASS |  |
| churn | converge:orders | PASS |  |
| churn | converge:shipments | PASS |  |
| churn | converge:staff | PASS |  |
| churn | converge:information_schema.columns | PASS |  |
| churn | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| churn | query:conformance: mixed-collation double grouping | PASS |  |
| churn | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| churn | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| churn | query:conformance: case-variant code grouping | PASS |  |
| churn | query:conformance: anti-join finds the event-less dimension | PASS |  |
| churn | query:conformance: nullable join key NULL-extends | PASS |  |
| churn | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| churn | query:conformance: date bucketing over the fact table | PASS |  |
| churn | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| churn | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| churn | query:enum: the empty member groups by its ordinal | PASS |  |
| churn | query:enum: the empty member sorts by its ordinal | PASS |  |
| churn | query:enum: the empty member is selectable by text | PASS |  |
| churn | query:geometry: hex round-trips the internal format | PASS |  |
| churn | query:geometry: byte length includes the srid prefix | PASS |  |
| churn | query:geometry: null routes filter and count | PASS |  |
| churn | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| churn | query:set: find_in_set filters by membership | PASS |  |
| churn | query:set: equality is literal, not member-normalized | PASS |  |
| churn | query:set: distinct values walk the bitmask including empty | PASS |  |
| churn | query:set: grouped counts order by bitmask not text | PASS |  |
| churn | query:set: a range predicate compares the bitmask | PASS |  |
| churn | query:star: fact with dimension and two audit persons | PASS |  |
| churn | query:star: five-alias chain fans out through events | PASS |  |
| churn | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| churn | query:star: five tables bridge the shop and the star | PASS |  |
| churn | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| churn | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| churn | query:json: length and keys survive null documents | PASS |  |
| churn | query:json: contains_path filters the documented rows | PASS |  |
| churn | query:json: json_value reads a scalar with sql semantics | PASS |  |
| churn | query:json: object construction embeds an extracted scalar | PASS |  |
| churn | query:json: search locates a literal value | PASS |  |
| churn | query:json: grouping by an extracted scalar | PASS |  |
| churn | query:json: merge_patch overlays and reads back | PASS |  |
| churn | query:temporal: quarter, weekday and name grains agree | PASS |  |
| churn | query:temporal: month-end bucketing via last_day | PASS |  |
| churn | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| churn | query:temporal: datetime range keeps the year window | PASS |  |
| churn | query:temporal: date_sub bound in the predicate | PASS |  |
| churn | query:temporal: year-month split grouping | PASS |  |
| churn | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| churn | query:regex: substr extracts the mail domain | PASS |  |
| churn | query:regex: the REGEXP operator anchors a class | PASS |  |
| churn | query:regex: replace folds suffix classes before grouping | PASS |  |
| churn | query:bi metabase: month grain through convert_tz | PASS |  |
| churn | query:bi metabase: iso week bucketing | PASS |  |
| churn | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| churn | query:bi metabase: previous-period revenue window | PASS |  |
| churn | query:bi superset: week-start grain with a rolling average | PASS |  |
| churn | query:bi superset: running total over grouped revenue | PASS |  |
| churn | query:bi superset: lag and lead against a named window | PASS |  |
| churn | query:bi superset: quartile counts from ntile | PASS |  |
| churn | query:bi superset: first and last value over an unbounded frame | PASS |  |
| churn | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| churn | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| churn | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| churn | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| churn | query:bi looker: the grouped primary key determines the row | PASS |  |
| churn | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| churn | query:bi tableau: explicit cast ladder | PASS |  |
| churn | query:bi tableau: the stddev and variance family | PASS |  |
| churn | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| churn | query:bi shared: substring_index dimension cleanup | PASS |  |
| churn | query:bi shared: json validity and typed path filter | PASS |  |
| churn | query:bi shared: contains_path over several paths at once | PASS |  |
| churn | query:bi shared: maketime from extracted parts | PASS |  |
| churn | query:bi shared: extract year_month grouping | PASS |  |
| churn | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| churn | query:staff: three-level management chain with an inactive tail | PASS |  |
| churn | query:staff: active split with id extremes | PASS |  |
| churn | query:counters: full unsigned ladder readback | PASS |  |
| churn | query:counters: greatest and least across widths | PASS |  |
| churn | query:dim: enum status split | PASS |  |
| churn | query:dim: pattern filter across collated columns | PASS |  |
| churn | query:person: anti-join finds owners without facts | PASS |  |
| churn | query:person: created-fact counts through a scalar subquery | PASS |  |
| churn | query:event: lag over per-dimension timelines | PASS |  |
| churn | query:event: daily grain per dimension code | PASS |  |
| churn | query:order_items: product rollup without the orders table | PASS |  |
| churn | query:shipments: carrier value through the items bridge | PASS |  |
| churn | query:json: distinct case variants survive a derived table | PASS |  |
| contention | converge:Dim | PASS |  |
| contention | converge:Event | PASS |  |
| contention | converge:Fact | PASS |  |
| contention | converge:Person | PASS |  |
| contention | converge:audit_log | PASS |  |
| contention | converge:badges | PASS |  |
| contention | converge:counters | PASS |  |
| contention | converge:customers | PASS |  |
| contention | converge:order_items | PASS |  |
| contention | converge:orders | PASS |  |
| contention | converge:shipments | PASS |  |
| contention | converge:staff | PASS |  |
| contention | converge:information_schema.columns | PASS |  |
| contention | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| contention | query:conformance: mixed-collation double grouping | PASS |  |
| contention | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| contention | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| contention | query:conformance: case-variant code grouping | PASS |  |
| contention | query:conformance: anti-join finds the event-less dimension | PASS |  |
| contention | query:conformance: nullable join key NULL-extends | PASS |  |
| contention | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| contention | query:conformance: date bucketing over the fact table | PASS |  |
| contention | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| contention | query:point lookup by key | PASS |  |
| contention | query:range scan with compound predicate | PASS |  |
| contention | query:inner join with aggregation | PASS |  |
| contention | query:join with a residual comparison between both inputs | PASS |  |
| contention | query:left join keeps rows whose only matches fail the residual | PASS |  |
| contention | query:residual comparison through coalesce on a nullable column | PASS |  |
| contention | query:created-by and updated-by resolve through separate aliases | PASS |  |
| contention | query:alias pair with the join order reversed | PASS |  |
| contention | query:four aliases of one table joined in a chain | PASS |  |
| contention | query:self-join with a single-side predicate in the ON clause | PASS |  |
| contention | query:self-join manager chain preserves the roots | PASS |  |
| contention | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| contention | query:aliases stay distinct when the empty side joins first | PASS |  |
| contention | query:left join preserves unmatched rows | PASS |  |
| contention | query:right join preserves unmatched rows | PASS |  |
| contention | query:three-way join through items | PASS |  |
| contention | query:union all across sources | PASS |  |
| contention | query:intersect customer identifiers | PASS |  |
| contention | query:except customer identifiers | PASS |  |
| contention | query:order by an expression over an aggregate | PASS |  |
| contention | query:order by a tree over several aggregates | PASS |  |
| contention | query:order by an aggregate absent from the select list | PASS |  |
| contention | query:group by with having | PASS |  |
| contention | query:conditional decimal sum keeps the fraction | PASS |  |
| contention | query:distinct count and min max | PASS |  |
| contention | query:uncorrelated in-subquery | PASS |  |
| contention | query:correlated exists with inner predicate | PASS |  |
| contention | query:correlated scalar aggregate | PASS |  |
| contention | query:correlated scalar unique lookup | PASS |  |
| contention | query:scalar subquery threshold | PASS |  |
| contention | query:non-recursive cte | PASS |  |
| contention | query:bounded recursive cte | PASS |  |
| contention | query:date bucketing | PASS |  |
| contention | query:string functions and like | PASS |  |
| contention | query:looker symmetric key helpers | PASS |  |
| contention | query:json constructor preserves json versus text | PASS |  |
| contention | query:json aggregate embeds documents | PASS |  |
| contention | query:regular expression read transforms | PASS |  |
| contention | query:case expression buckets | PASS |  |
| contention | query:null handling | PASS |  |
| contention | query:coalesce and ifnull | PASS |  |
| contention | query:enum and set filters | PASS |  |
| contention | query:unsigned boundary readback | PASS |  |
| contention | query:derived table | PASS |  |
| contention | query:group_concat single expression | PASS |  |
| contention | query:window ranking per group | PASS |  |
| contention | query:window share of total over grouped output | PASS |  |
| contention | query:window running total | PASS |  |
| contention | query:decimal column average beyond simple sum | PASS |  |
| contention | query:computed decimal rounds negative digits half away from zero | PASS |  |
| contention | query:json extract filter on customer meta | PASS |  |
| contention | query:fan-out join group concat line products | PASS |  |
| contention | query:outer join customers without recent orders | PASS |  |
| contention | query:set op union distinct tiers and statuses | PASS |  |
| contention | query:temporal convert and date_format grain | PASS |  |
| contention | query:correlated not exists open orders | PASS |  |
| contention | query:window lag payment-shaped totals | PASS |  |
| contention | query:multi-key join items to orders | PASS |  |
| contention | query:between and null-safe coalesce on balance | PASS |  |
| contention | query:intersect all-style customer buyers | PASS |  |
| contention | query:derived table status revenue share | PASS |  |
| contention | query:general_ci: equality folds ASCII case | PASS |  |
| contention | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| contention | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| contention | query:general_ci: every supplementary character compares equal | PASS |  |
| contention | query:general_ci: grouping partitions by collated equality | PASS |  |
| contention | query:general_ci: ordering follows the collation, not code points | PASS |  |
| contention | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| contention | query:general_ci: joining on a collated column | PASS |  |
| contention | query:general_ci: representative spelling of a collated group | PASS |  |
| contention | query:general_ci: mixing collations across separate comparisons | PASS |  |
| contention | query:enum: order by ascends by declared ordinal | PASS |  |
| contention | query:enum: order by descends by declared ordinal | PASS |  |
| contention | query:enum: min and max compare as strings | PASS |  |
| contention | query:enum: a greater-than range compares as strings | PASS |  |
| contention | query:enum: a less-than range compares as strings | PASS |  |
| contention | query:enum: between compares as strings | PASS |  |
| contention | query:enum: distinct orders by ordinal | PASS |  |
| contention | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| contention | query:enum: a window order walks the ordinal | PASS |  |
| contention | query:collation: mixed grouping answers with per-key folds | PASS |  |
| contention | query:collation: distinct counts fold per column collation | PASS |  |
| contention | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| contention | query:set: order by walks the member bitmask | PASS |  |
| contention | query:set: grouping orders groups by bitmask | PASS |  |
| contention | query:enum: the empty member groups by its ordinal | PASS |  |
| contention | query:enum: the empty member sorts by its ordinal | PASS |  |
| contention | query:enum: the empty member is selectable by text | PASS |  |
| contention | query:geometry: hex round-trips the internal format | PASS |  |
| contention | query:geometry: byte length includes the srid prefix | PASS |  |
| contention | query:geometry: null routes filter and count | PASS |  |
| contention | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| contention | query:set: find_in_set filters by membership | PASS |  |
| contention | query:set: equality is literal, not member-normalized | PASS |  |
| contention | query:set: distinct values walk the bitmask including empty | PASS |  |
| contention | query:set: grouped counts order by bitmask not text | PASS |  |
| contention | query:set: a range predicate compares the bitmask | PASS |  |
| contention | query:star: fact with dimension and two audit persons | PASS |  |
| contention | query:star: five-alias chain fans out through events | PASS |  |
| contention | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| contention | query:star: five tables bridge the shop and the star | PASS |  |
| contention | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| contention | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| contention | query:json: length and keys survive null documents | PASS |  |
| contention | query:json: contains_path filters the documented rows | PASS |  |
| contention | query:json: json_value reads a scalar with sql semantics | PASS |  |
| contention | query:json: object construction embeds an extracted scalar | PASS |  |
| contention | query:json: search locates a literal value | PASS |  |
| contention | query:json: grouping by an extracted scalar | PASS |  |
| contention | query:json: merge_patch overlays and reads back | PASS |  |
| contention | query:temporal: quarter, weekday and name grains agree | PASS |  |
| contention | query:temporal: month-end bucketing via last_day | PASS |  |
| contention | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| contention | query:temporal: datetime range keeps the year window | PASS |  |
| contention | query:temporal: date_sub bound in the predicate | PASS |  |
| contention | query:temporal: year-month split grouping | PASS |  |
| contention | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| contention | query:regex: substr extracts the mail domain | PASS |  |
| contention | query:regex: the REGEXP operator anchors a class | PASS |  |
| contention | query:regex: replace folds suffix classes before grouping | PASS |  |
| contention | query:bi metabase: month grain through convert_tz | PASS |  |
| contention | query:bi metabase: iso week bucketing | PASS |  |
| contention | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| contention | query:bi metabase: previous-period revenue window | PASS |  |
| contention | query:bi superset: week-start grain with a rolling average | PASS |  |
| contention | query:bi superset: running total over grouped revenue | PASS |  |
| contention | query:bi superset: lag and lead against a named window | PASS |  |
| contention | query:bi superset: quartile counts from ntile | PASS |  |
| contention | query:bi superset: first and last value over an unbounded frame | PASS |  |
| contention | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| contention | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| contention | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| contention | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| contention | query:bi looker: the grouped primary key determines the row | PASS |  |
| contention | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| contention | query:bi tableau: explicit cast ladder | PASS |  |
| contention | query:bi tableau: the stddev and variance family | PASS |  |
| contention | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| contention | query:bi shared: substring_index dimension cleanup | PASS |  |
| contention | query:bi shared: json validity and typed path filter | PASS |  |
| contention | query:bi shared: contains_path over several paths at once | PASS |  |
| contention | query:bi shared: maketime from extracted parts | PASS |  |
| contention | query:bi shared: extract year_month grouping | PASS |  |
| contention | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| contention | query:staff: three-level management chain with an inactive tail | PASS |  |
| contention | query:staff: active split with id extremes | PASS |  |
| contention | query:counters: full unsigned ladder readback | PASS |  |
| contention | query:counters: greatest and least across widths | PASS |  |
| contention | query:dim: enum status split | PASS |  |
| contention | query:dim: pattern filter across collated columns | PASS |  |
| contention | query:person: anti-join finds owners without facts | PASS |  |
| contention | query:person: created-fact counts through a scalar subquery | PASS |  |
| contention | query:event: lag over per-dimension timelines | PASS |  |
| contention | query:event: daily grain per dimension code | PASS |  |
| contention | query:order_items: product rollup without the orders table | PASS |  |
| contention | query:shipments: carrier value through the items bridge | PASS |  |
| contention | query:json: distinct case variants survive a derived table | PASS |  |
| execution-budget | hint:interrupts a runaway join | PASS |  |
| execution-budget | hint:interrupts promptly | PASS |  |
| execution-budget | hint:a generous budget runs to completion | PASS |  |
| execution-budget | hint:cannot loosen the session ceiling | PASS |  |
| execution-budget | hint:an unimplemented hint rejects | PASS |  |
| execution-budget | converge:Dim | PASS |  |
| execution-budget | converge:Event | PASS |  |
| execution-budget | converge:Fact | PASS |  |
| execution-budget | converge:Person | PASS |  |
| execution-budget | converge:audit_log | PASS |  |
| execution-budget | converge:badges | PASS |  |
| execution-budget | converge:counters | PASS |  |
| execution-budget | converge:customers | PASS |  |
| execution-budget | converge:order_items | PASS |  |
| execution-budget | converge:orders | PASS |  |
| execution-budget | converge:shipments | PASS |  |
| execution-budget | converge:staff | PASS |  |
| execution-budget | converge:information_schema.columns | PASS |  |
| execution-budget | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| execution-budget | query:conformance: mixed-collation double grouping | PASS |  |
| execution-budget | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| execution-budget | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| execution-budget | query:conformance: case-variant code grouping | PASS |  |
| execution-budget | query:conformance: anti-join finds the event-less dimension | PASS |  |
| execution-budget | query:conformance: nullable join key NULL-extends | PASS |  |
| execution-budget | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| execution-budget | query:conformance: date bucketing over the fact table | PASS |  |
| execution-budget | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| execution-budget | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| execution-budget | query:enum: the empty member groups by its ordinal | PASS |  |
| execution-budget | query:enum: the empty member sorts by its ordinal | PASS |  |
| execution-budget | query:enum: the empty member is selectable by text | PASS |  |
| execution-budget | query:geometry: hex round-trips the internal format | PASS |  |
| execution-budget | query:geometry: byte length includes the srid prefix | PASS |  |
| execution-budget | query:geometry: null routes filter and count | PASS |  |
| execution-budget | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| execution-budget | query:set: find_in_set filters by membership | PASS |  |
| execution-budget | query:set: equality is literal, not member-normalized | PASS |  |
| execution-budget | query:set: distinct values walk the bitmask including empty | PASS |  |
| execution-budget | query:set: grouped counts order by bitmask not text | PASS |  |
| execution-budget | query:set: a range predicate compares the bitmask | PASS |  |
| execution-budget | query:star: fact with dimension and two audit persons | PASS |  |
| execution-budget | query:star: five-alias chain fans out through events | PASS |  |
| execution-budget | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| execution-budget | query:star: five tables bridge the shop and the star | PASS |  |
| execution-budget | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| execution-budget | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| execution-budget | query:json: length and keys survive null documents | PASS |  |
| execution-budget | query:json: contains_path filters the documented rows | PASS |  |
| execution-budget | query:json: json_value reads a scalar with sql semantics | PASS |  |
| execution-budget | query:json: object construction embeds an extracted scalar | PASS |  |
| execution-budget | query:json: search locates a literal value | PASS |  |
| execution-budget | query:json: grouping by an extracted scalar | PASS |  |
| execution-budget | query:json: merge_patch overlays and reads back | PASS |  |
| execution-budget | query:temporal: quarter, weekday and name grains agree | PASS |  |
| execution-budget | query:temporal: month-end bucketing via last_day | PASS |  |
| execution-budget | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| execution-budget | query:temporal: datetime range keeps the year window | PASS |  |
| execution-budget | query:temporal: date_sub bound in the predicate | PASS |  |
| execution-budget | query:temporal: year-month split grouping | PASS |  |
| execution-budget | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| execution-budget | query:regex: substr extracts the mail domain | PASS |  |
| execution-budget | query:regex: the REGEXP operator anchors a class | PASS |  |
| execution-budget | query:regex: replace folds suffix classes before grouping | PASS |  |
| execution-budget | query:bi metabase: month grain through convert_tz | PASS |  |
| execution-budget | query:bi metabase: iso week bucketing | PASS |  |
| execution-budget | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| execution-budget | query:bi metabase: previous-period revenue window | PASS |  |
| execution-budget | query:bi superset: week-start grain with a rolling average | PASS |  |
| execution-budget | query:bi superset: running total over grouped revenue | PASS |  |
| execution-budget | query:bi superset: lag and lead against a named window | PASS |  |
| execution-budget | query:bi superset: quartile counts from ntile | PASS |  |
| execution-budget | query:bi superset: first and last value over an unbounded frame | PASS |  |
| execution-budget | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| execution-budget | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| execution-budget | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| execution-budget | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| execution-budget | query:bi looker: the grouped primary key determines the row | PASS |  |
| execution-budget | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| execution-budget | query:bi tableau: explicit cast ladder | PASS |  |
| execution-budget | query:bi tableau: the stddev and variance family | PASS |  |
| execution-budget | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| execution-budget | query:bi shared: substring_index dimension cleanup | PASS |  |
| execution-budget | query:bi shared: json validity and typed path filter | PASS |  |
| execution-budget | query:bi shared: contains_path over several paths at once | PASS |  |
| execution-budget | query:bi shared: maketime from extracted parts | PASS |  |
| execution-budget | query:bi shared: extract year_month grouping | PASS |  |
| execution-budget | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| execution-budget | query:staff: three-level management chain with an inactive tail | PASS |  |
| execution-budget | query:staff: active split with id extremes | PASS |  |
| execution-budget | query:counters: full unsigned ladder readback | PASS |  |
| execution-budget | query:counters: greatest and least across widths | PASS |  |
| execution-budget | query:dim: enum status split | PASS |  |
| execution-budget | query:dim: pattern filter across collated columns | PASS |  |
| execution-budget | query:person: anti-join finds owners without facts | PASS |  |
| execution-budget | query:person: created-fact counts through a scalar subquery | PASS |  |
| execution-budget | query:event: lag over per-dimension timelines | PASS |  |
| execution-budget | query:event: daily grain per dimension code | PASS |  |
| execution-budget | query:order_items: product rollup without the orders table | PASS |  |
| execution-budget | query:shipments: carrier value through the items bridge | PASS |  |
| execution-budget | query:json: distinct case variants survive a derived table | PASS |  |
| spill | forced-spill:sort | PASS |  |
| spill | forced-spill:aggregate | PASS |  |
| spill | forced-spill:distinct | PASS |  |
| spill | forced-spill:join | PASS |  |
| spill | converge:Dim | PASS |  |
| spill | converge:Event | PASS |  |
| spill | converge:Fact | PASS |  |
| spill | converge:Person | PASS |  |
| spill | converge:audit_log | PASS |  |
| spill | converge:badges | PASS |  |
| spill | converge:counters | PASS |  |
| spill | converge:customers | PASS |  |
| spill | converge:order_items | PASS |  |
| spill | converge:orders | PASS |  |
| spill | converge:shipments | PASS |  |
| spill | converge:staff | PASS |  |
| spill | converge:information_schema.columns | PASS |  |
| spill | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| spill | query:conformance: mixed-collation double grouping | PASS |  |
| spill | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| spill | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| spill | query:conformance: case-variant code grouping | PASS |  |
| spill | query:conformance: anti-join finds the event-less dimension | PASS |  |
| spill | query:conformance: nullable join key NULL-extends | PASS |  |
| spill | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| spill | query:conformance: date bucketing over the fact table | PASS |  |
| spill | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| spill | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| spill | query:enum: the empty member groups by its ordinal | PASS |  |
| spill | query:enum: the empty member sorts by its ordinal | PASS |  |
| spill | query:enum: the empty member is selectable by text | PASS |  |
| spill | query:geometry: hex round-trips the internal format | PASS |  |
| spill | query:geometry: byte length includes the srid prefix | PASS |  |
| spill | query:geometry: null routes filter and count | PASS |  |
| spill | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| spill | query:set: find_in_set filters by membership | PASS |  |
| spill | query:set: equality is literal, not member-normalized | PASS |  |
| spill | query:set: distinct values walk the bitmask including empty | PASS |  |
| spill | query:set: grouped counts order by bitmask not text | PASS |  |
| spill | query:set: a range predicate compares the bitmask | PASS |  |
| spill | query:star: fact with dimension and two audit persons | PASS |  |
| spill | query:star: five-alias chain fans out through events | PASS |  |
| spill | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| spill | query:star: five tables bridge the shop and the star | PASS |  |
| spill | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| spill | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| spill | query:json: length and keys survive null documents | PASS |  |
| spill | query:json: contains_path filters the documented rows | PASS |  |
| spill | query:json: json_value reads a scalar with sql semantics | PASS |  |
| spill | query:json: object construction embeds an extracted scalar | PASS |  |
| spill | query:json: search locates a literal value | PASS |  |
| spill | query:json: grouping by an extracted scalar | PASS |  |
| spill | query:json: merge_patch overlays and reads back | PASS |  |
| spill | query:temporal: quarter, weekday and name grains agree | PASS |  |
| spill | query:temporal: month-end bucketing via last_day | PASS |  |
| spill | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| spill | query:temporal: datetime range keeps the year window | PASS |  |
| spill | query:temporal: date_sub bound in the predicate | PASS |  |
| spill | query:temporal: year-month split grouping | PASS |  |
| spill | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| spill | query:regex: substr extracts the mail domain | PASS |  |
| spill | query:regex: the REGEXP operator anchors a class | PASS |  |
| spill | query:regex: replace folds suffix classes before grouping | PASS |  |
| spill | query:bi metabase: month grain through convert_tz | PASS |  |
| spill | query:bi metabase: iso week bucketing | PASS |  |
| spill | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| spill | query:bi metabase: previous-period revenue window | PASS |  |
| spill | query:bi superset: week-start grain with a rolling average | PASS |  |
| spill | query:bi superset: running total over grouped revenue | PASS |  |
| spill | query:bi superset: lag and lead against a named window | PASS |  |
| spill | query:bi superset: quartile counts from ntile | PASS |  |
| spill | query:bi superset: first and last value over an unbounded frame | PASS |  |
| spill | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| spill | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| spill | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| spill | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| spill | query:bi looker: the grouped primary key determines the row | PASS |  |
| spill | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| spill | query:bi tableau: explicit cast ladder | PASS |  |
| spill | query:bi tableau: the stddev and variance family | PASS |  |
| spill | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| spill | query:bi shared: substring_index dimension cleanup | PASS |  |
| spill | query:bi shared: json validity and typed path filter | PASS |  |
| spill | query:bi shared: contains_path over several paths at once | PASS |  |
| spill | query:bi shared: maketime from extracted parts | PASS |  |
| spill | query:bi shared: extract year_month grouping | PASS |  |
| spill | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| spill | query:staff: three-level management chain with an inactive tail | PASS |  |
| spill | query:staff: active split with id extremes | PASS |  |
| spill | query:counters: full unsigned ladder readback | PASS |  |
| spill | query:counters: greatest and least across widths | PASS |  |
| spill | query:dim: enum status split | PASS |  |
| spill | query:dim: pattern filter across collated columns | PASS |  |
| spill | query:person: anti-join finds owners without facts | PASS |  |
| spill | query:person: created-fact counts through a scalar subquery | PASS |  |
| spill | query:event: lag over per-dimension timelines | PASS |  |
| spill | query:event: daily grain per dimension code | PASS |  |
| spill | query:order_items: product rollup without the orders table | PASS |  |
| spill | query:shipments: carrier value through the items bridge | PASS |  |
| spill | query:json: distinct case variants survive a derived table | PASS |  |
| pooling | pool:concurrent-borrows(40 over 4) | PASS |  |
| pooling | pool:prepared-statements | PASS |  |
| pooling | pool:session-state-survives-borrow-like-mysql | PASS |  |
| pooling | converge:Dim | PASS |  |
| pooling | converge:Event | PASS |  |
| pooling | converge:Fact | PASS |  |
| pooling | converge:Person | PASS |  |
| pooling | converge:audit_log | PASS |  |
| pooling | converge:badges | PASS |  |
| pooling | converge:counters | PASS |  |
| pooling | converge:customers | PASS |  |
| pooling | converge:order_items | PASS |  |
| pooling | converge:orders | PASS |  |
| pooling | converge:shipments | PASS |  |
| pooling | converge:staff | PASS |  |
| pooling | converge:information_schema.columns | PASS |  |
| pooling | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| pooling | query:conformance: mixed-collation double grouping | PASS |  |
| pooling | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| pooling | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| pooling | query:conformance: case-variant code grouping | PASS |  |
| pooling | query:conformance: anti-join finds the event-less dimension | PASS |  |
| pooling | query:conformance: nullable join key NULL-extends | PASS |  |
| pooling | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| pooling | query:conformance: date bucketing over the fact table | PASS |  |
| pooling | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| pooling | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| pooling | query:enum: the empty member groups by its ordinal | PASS |  |
| pooling | query:enum: the empty member sorts by its ordinal | PASS |  |
| pooling | query:enum: the empty member is selectable by text | PASS |  |
| pooling | query:geometry: hex round-trips the internal format | PASS |  |
| pooling | query:geometry: byte length includes the srid prefix | PASS |  |
| pooling | query:geometry: null routes filter and count | PASS |  |
| pooling | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| pooling | query:set: find_in_set filters by membership | PASS |  |
| pooling | query:set: equality is literal, not member-normalized | PASS |  |
| pooling | query:set: distinct values walk the bitmask including empty | PASS |  |
| pooling | query:set: grouped counts order by bitmask not text | PASS |  |
| pooling | query:set: a range predicate compares the bitmask | PASS |  |
| pooling | query:star: fact with dimension and two audit persons | PASS |  |
| pooling | query:star: five-alias chain fans out through events | PASS |  |
| pooling | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| pooling | query:star: five tables bridge the shop and the star | PASS |  |
| pooling | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| pooling | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| pooling | query:json: length and keys survive null documents | PASS |  |
| pooling | query:json: contains_path filters the documented rows | PASS |  |
| pooling | query:json: json_value reads a scalar with sql semantics | PASS |  |
| pooling | query:json: object construction embeds an extracted scalar | PASS |  |
| pooling | query:json: search locates a literal value | PASS |  |
| pooling | query:json: grouping by an extracted scalar | PASS |  |
| pooling | query:json: merge_patch overlays and reads back | PASS |  |
| pooling | query:temporal: quarter, weekday and name grains agree | PASS |  |
| pooling | query:temporal: month-end bucketing via last_day | PASS |  |
| pooling | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| pooling | query:temporal: datetime range keeps the year window | PASS |  |
| pooling | query:temporal: date_sub bound in the predicate | PASS |  |
| pooling | query:temporal: year-month split grouping | PASS |  |
| pooling | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| pooling | query:regex: substr extracts the mail domain | PASS |  |
| pooling | query:regex: the REGEXP operator anchors a class | PASS |  |
| pooling | query:regex: replace folds suffix classes before grouping | PASS |  |
| pooling | query:bi metabase: month grain through convert_tz | PASS |  |
| pooling | query:bi metabase: iso week bucketing | PASS |  |
| pooling | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| pooling | query:bi metabase: previous-period revenue window | PASS |  |
| pooling | query:bi superset: week-start grain with a rolling average | PASS |  |
| pooling | query:bi superset: running total over grouped revenue | PASS |  |
| pooling | query:bi superset: lag and lead against a named window | PASS |  |
| pooling | query:bi superset: quartile counts from ntile | PASS |  |
| pooling | query:bi superset: first and last value over an unbounded frame | PASS |  |
| pooling | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| pooling | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| pooling | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| pooling | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| pooling | query:bi looker: the grouped primary key determines the row | PASS |  |
| pooling | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| pooling | query:bi tableau: explicit cast ladder | PASS |  |
| pooling | query:bi tableau: the stddev and variance family | PASS |  |
| pooling | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| pooling | query:bi shared: substring_index dimension cleanup | PASS |  |
| pooling | query:bi shared: json validity and typed path filter | PASS |  |
| pooling | query:bi shared: contains_path over several paths at once | PASS |  |
| pooling | query:bi shared: maketime from extracted parts | PASS |  |
| pooling | query:bi shared: extract year_month grouping | PASS |  |
| pooling | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| pooling | query:staff: three-level management chain with an inactive tail | PASS |  |
| pooling | query:staff: active split with id extremes | PASS |  |
| pooling | query:counters: full unsigned ladder readback | PASS |  |
| pooling | query:counters: greatest and least across widths | PASS |  |
| pooling | query:dim: enum status split | PASS |  |
| pooling | query:dim: pattern filter across collated columns | PASS |  |
| pooling | query:person: anti-join finds owners without facts | PASS |  |
| pooling | query:person: created-fact counts through a scalar subquery | PASS |  |
| pooling | query:event: lag over per-dimension timelines | PASS |  |
| pooling | query:event: daily grain per dimension code | PASS |  |
| pooling | query:order_items: product rollup without the orders table | PASS |  |
| pooling | query:shipments: carrier value through the items bridge | PASS |  |
| pooling | query:json: distinct case variants survive a derived table | PASS |  |
| local-database | create table returns an OK packet | PASS |  |
| local-database | insert reports its affected rows | PASS |  |
| local-database | the rows read back through the same connection | PASS |  |
| local-database | aggregates and predicates work on a local table | PASS |  |
| local-database | duplicate key is 1062 | PASS |  |
| local-database | existing table is 1050 | PASS |  |
| local-database | not-null violation is 1048 | PASS |  |
| local-database | unknown table is 1146 | PASS |  |
| local-database | unknown column is 1054 | PASS |  |
| local-database | BEGIN is refused on a local database | PASS |  |
| local-database | START TRANSACTION is refused on a local database | PASS |  |
| local-database | COMMIT is refused on a local database | PASS |  |
| local-database | ROLLBACK is refused on a local database | PASS |  |
| local-database | SET autocommit = 0 is refused on a local database | PASS |  |
| local-database | the rows an autocommit committed survive a ROLLBACK | PASS |  |
| local-database | a replicated database keeps the transaction no-op | PASS |  |
| local-database | a refused write leaves the table unchanged | PASS |  |
| local-database | the replicated database still refuses writes | PASS |  |
| local-database | a local database is not scheduled for replication | PASS |  |
| local-database | converge:Dim | PASS |  |
| local-database | converge:Event | PASS |  |
| local-database | converge:Fact | PASS |  |
| local-database | converge:Person | PASS |  |
| local-database | converge:audit_log | PASS |  |
| local-database | converge:badges | PASS |  |
| local-database | converge:counters | PASS |  |
| local-database | converge:customers | PASS |  |
| local-database | converge:order_items | PASS |  |
| local-database | converge:orders | PASS |  |
| local-database | converge:shipments | PASS |  |
| local-database | converge:staff | PASS |  |
| local-database | converge:information_schema.columns | PASS |  |
| local-database | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| local-database | query:conformance: mixed-collation double grouping | PASS |  |
| local-database | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| local-database | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| local-database | query:conformance: case-variant code grouping | PASS |  |
| local-database | query:conformance: anti-join finds the event-less dimension | PASS |  |
| local-database | query:conformance: nullable join key NULL-extends | PASS |  |
| local-database | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| local-database | query:conformance: date bucketing over the fact table | PASS |  |
| local-database | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| local-database | query:point lookup by key | PASS |  |
| local-database | query:range scan with compound predicate | PASS |  |
| local-database | query:inner join with aggregation | PASS |  |
| local-database | query:join with a residual comparison between both inputs | PASS |  |
| local-database | query:left join keeps rows whose only matches fail the residual | PASS |  |
| local-database | query:residual comparison through coalesce on a nullable column | PASS |  |
| local-database | query:created-by and updated-by resolve through separate aliases | PASS |  |
| local-database | query:alias pair with the join order reversed | PASS |  |
| local-database | query:four aliases of one table joined in a chain | PASS |  |
| local-database | query:self-join with a single-side predicate in the ON clause | PASS |  |
| local-database | query:self-join manager chain preserves the roots | PASS |  |
| local-database | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| local-database | query:aliases stay distinct when the empty side joins first | PASS |  |
| local-database | query:left join preserves unmatched rows | PASS |  |
| local-database | query:right join preserves unmatched rows | PASS |  |
| local-database | query:three-way join through items | PASS |  |
| local-database | query:union all across sources | PASS |  |
| local-database | query:intersect customer identifiers | PASS |  |
| local-database | query:except customer identifiers | PASS |  |
| local-database | query:order by an expression over an aggregate | PASS |  |
| local-database | query:order by a tree over several aggregates | PASS |  |
| local-database | query:order by an aggregate absent from the select list | PASS |  |
| local-database | query:group by with having | PASS |  |
| local-database | query:conditional decimal sum keeps the fraction | PASS |  |
| local-database | query:distinct count and min max | PASS |  |
| local-database | query:uncorrelated in-subquery | PASS |  |
| local-database | query:correlated exists with inner predicate | PASS |  |
| local-database | query:correlated scalar aggregate | PASS |  |
| local-database | query:correlated scalar unique lookup | PASS |  |
| local-database | query:scalar subquery threshold | PASS |  |
| local-database | query:non-recursive cte | PASS |  |
| local-database | query:bounded recursive cte | PASS |  |
| local-database | query:date bucketing | PASS |  |
| local-database | query:string functions and like | PASS |  |
| local-database | query:looker symmetric key helpers | PASS |  |
| local-database | query:json constructor preserves json versus text | PASS |  |
| local-database | query:json aggregate embeds documents | PASS |  |
| local-database | query:regular expression read transforms | PASS |  |
| local-database | query:case expression buckets | PASS |  |
| local-database | query:null handling | PASS |  |
| local-database | query:coalesce and ifnull | PASS |  |
| local-database | query:enum and set filters | PASS |  |
| local-database | query:unsigned boundary readback | PASS |  |
| local-database | query:derived table | PASS |  |
| local-database | query:group_concat single expression | PASS |  |
| local-database | query:window ranking per group | PASS |  |
| local-database | query:window share of total over grouped output | PASS |  |
| local-database | query:window running total | PASS |  |
| local-database | query:decimal column average beyond simple sum | PASS |  |
| local-database | query:computed decimal rounds negative digits half away from zero | PASS |  |
| local-database | query:json extract filter on customer meta | PASS |  |
| local-database | query:fan-out join group concat line products | PASS |  |
| local-database | query:outer join customers without recent orders | PASS |  |
| local-database | query:set op union distinct tiers and statuses | PASS |  |
| local-database | query:temporal convert and date_format grain | PASS |  |
| local-database | query:correlated not exists open orders | PASS |  |
| local-database | query:window lag payment-shaped totals | PASS |  |
| local-database | query:multi-key join items to orders | PASS |  |
| local-database | query:between and null-safe coalesce on balance | PASS |  |
| local-database | query:intersect all-style customer buyers | PASS |  |
| local-database | query:derived table status revenue share | PASS |  |
| local-database | query:general_ci: equality folds ASCII case | PASS |  |
| local-database | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| local-database | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| local-database | query:general_ci: every supplementary character compares equal | PASS |  |
| local-database | query:general_ci: grouping partitions by collated equality | PASS |  |
| local-database | query:general_ci: ordering follows the collation, not code points | PASS |  |
| local-database | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| local-database | query:general_ci: joining on a collated column | PASS |  |
| local-database | query:general_ci: representative spelling of a collated group | PASS |  |
| local-database | query:general_ci: mixing collations across separate comparisons | PASS |  |
| local-database | query:enum: order by ascends by declared ordinal | PASS |  |
| local-database | query:enum: order by descends by declared ordinal | PASS |  |
| local-database | query:enum: min and max compare as strings | PASS |  |
| local-database | query:enum: a greater-than range compares as strings | PASS |  |
| local-database | query:enum: a less-than range compares as strings | PASS |  |
| local-database | query:enum: between compares as strings | PASS |  |
| local-database | query:enum: distinct orders by ordinal | PASS |  |
| local-database | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| local-database | query:enum: a window order walks the ordinal | PASS |  |
| local-database | query:collation: mixed grouping answers with per-key folds | PASS |  |
| local-database | query:collation: distinct counts fold per column collation | PASS |  |
| local-database | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| local-database | query:set: order by walks the member bitmask | PASS |  |
| local-database | query:set: grouping orders groups by bitmask | PASS |  |
| local-database | query:enum: the empty member groups by its ordinal | PASS |  |
| local-database | query:enum: the empty member sorts by its ordinal | PASS |  |
| local-database | query:enum: the empty member is selectable by text | PASS |  |
| local-database | query:geometry: hex round-trips the internal format | PASS |  |
| local-database | query:geometry: byte length includes the srid prefix | PASS |  |
| local-database | query:geometry: null routes filter and count | PASS |  |
| local-database | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| local-database | query:set: find_in_set filters by membership | PASS |  |
| local-database | query:set: equality is literal, not member-normalized | PASS |  |
| local-database | query:set: distinct values walk the bitmask including empty | PASS |  |
| local-database | query:set: grouped counts order by bitmask not text | PASS |  |
| local-database | query:set: a range predicate compares the bitmask | PASS |  |
| local-database | query:star: fact with dimension and two audit persons | PASS |  |
| local-database | query:star: five-alias chain fans out through events | PASS |  |
| local-database | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| local-database | query:star: five tables bridge the shop and the star | PASS |  |
| local-database | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| local-database | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| local-database | query:json: length and keys survive null documents | PASS |  |
| local-database | query:json: contains_path filters the documented rows | PASS |  |
| local-database | query:json: json_value reads a scalar with sql semantics | PASS |  |
| local-database | query:json: object construction embeds an extracted scalar | PASS |  |
| local-database | query:json: search locates a literal value | PASS |  |
| local-database | query:json: grouping by an extracted scalar | PASS |  |
| local-database | query:json: merge_patch overlays and reads back | PASS |  |
| local-database | query:temporal: quarter, weekday and name grains agree | PASS |  |
| local-database | query:temporal: month-end bucketing via last_day | PASS |  |
| local-database | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| local-database | query:temporal: datetime range keeps the year window | PASS |  |
| local-database | query:temporal: date_sub bound in the predicate | PASS |  |
| local-database | query:temporal: year-month split grouping | PASS |  |
| local-database | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| local-database | query:regex: substr extracts the mail domain | PASS |  |
| local-database | query:regex: the REGEXP operator anchors a class | PASS |  |
| local-database | query:regex: replace folds suffix classes before grouping | PASS |  |
| local-database | query:bi metabase: month grain through convert_tz | PASS |  |
| local-database | query:bi metabase: iso week bucketing | PASS |  |
| local-database | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| local-database | query:bi metabase: previous-period revenue window | PASS |  |
| local-database | query:bi superset: week-start grain with a rolling average | PASS |  |
| local-database | query:bi superset: running total over grouped revenue | PASS |  |
| local-database | query:bi superset: lag and lead against a named window | PASS |  |
| local-database | query:bi superset: quartile counts from ntile | PASS |  |
| local-database | query:bi superset: first and last value over an unbounded frame | PASS |  |
| local-database | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| local-database | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| local-database | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| local-database | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| local-database | query:bi looker: the grouped primary key determines the row | PASS |  |
| local-database | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| local-database | query:bi tableau: explicit cast ladder | PASS |  |
| local-database | query:bi tableau: the stddev and variance family | PASS |  |
| local-database | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| local-database | query:bi shared: substring_index dimension cleanup | PASS |  |
| local-database | query:bi shared: json validity and typed path filter | PASS |  |
| local-database | query:bi shared: contains_path over several paths at once | PASS |  |
| local-database | query:bi shared: maketime from extracted parts | PASS |  |
| local-database | query:bi shared: extract year_month grouping | PASS |  |
| local-database | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| local-database | query:staff: three-level management chain with an inactive tail | PASS |  |
| local-database | query:staff: active split with id extremes | PASS |  |
| local-database | query:counters: full unsigned ladder readback | PASS |  |
| local-database | query:counters: greatest and least across widths | PASS |  |
| local-database | query:dim: enum status split | PASS |  |
| local-database | query:dim: pattern filter across collated columns | PASS |  |
| local-database | query:person: anti-join finds owners without facts | PASS |  |
| local-database | query:person: created-fact counts through a scalar subquery | PASS |  |
| local-database | query:event: lag over per-dimension timelines | PASS |  |
| local-database | query:event: daily grain per dimension code | PASS |  |
| local-database | query:order_items: product rollup without the orders table | PASS |  |
| local-database | query:shipments: carrier value through the items bridge | PASS |  |
| local-database | query:json: distinct case variants survive a derived table | PASS |  |
| restart | local:rows survive a SIGKILL | PASS |  |
| restart | converge:Dim | PASS |  |
| restart | converge:Event | PASS |  |
| restart | converge:Fact | PASS |  |
| restart | converge:Person | PASS |  |
| restart | converge:audit_log | PASS |  |
| restart | converge:badges | PASS |  |
| restart | converge:counters | PASS |  |
| restart | converge:customers | PASS |  |
| restart | converge:order_items | PASS |  |
| restart | converge:orders | PASS |  |
| restart | converge:shipments | PASS |  |
| restart | converge:staff | PASS |  |
| restart | converge:information_schema.columns | PASS |  |
| restart | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| restart | query:conformance: mixed-collation double grouping | PASS |  |
| restart | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| restart | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| restart | query:conformance: case-variant code grouping | PASS |  |
| restart | query:conformance: anti-join finds the event-less dimension | PASS |  |
| restart | query:conformance: nullable join key NULL-extends | PASS |  |
| restart | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| restart | query:conformance: date bucketing over the fact table | PASS |  |
| restart | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| restart | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| restart | query:enum: the empty member groups by its ordinal | PASS |  |
| restart | query:enum: the empty member sorts by its ordinal | PASS |  |
| restart | query:enum: the empty member is selectable by text | PASS |  |
| restart | query:geometry: hex round-trips the internal format | PASS |  |
| restart | query:geometry: byte length includes the srid prefix | PASS |  |
| restart | query:geometry: null routes filter and count | PASS |  |
| restart | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| restart | query:set: find_in_set filters by membership | PASS |  |
| restart | query:set: equality is literal, not member-normalized | PASS |  |
| restart | query:set: distinct values walk the bitmask including empty | PASS |  |
| restart | query:set: grouped counts order by bitmask not text | PASS |  |
| restart | query:set: a range predicate compares the bitmask | PASS |  |
| restart | query:star: fact with dimension and two audit persons | PASS |  |
| restart | query:star: five-alias chain fans out through events | PASS |  |
| restart | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| restart | query:star: five tables bridge the shop and the star | PASS |  |
| restart | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| restart | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| restart | query:json: length and keys survive null documents | PASS |  |
| restart | query:json: contains_path filters the documented rows | PASS |  |
| restart | query:json: json_value reads a scalar with sql semantics | PASS |  |
| restart | query:json: object construction embeds an extracted scalar | PASS |  |
| restart | query:json: search locates a literal value | PASS |  |
| restart | query:json: grouping by an extracted scalar | PASS |  |
| restart | query:json: merge_patch overlays and reads back | PASS |  |
| restart | query:temporal: quarter, weekday and name grains agree | PASS |  |
| restart | query:temporal: month-end bucketing via last_day | PASS |  |
| restart | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| restart | query:temporal: datetime range keeps the year window | PASS |  |
| restart | query:temporal: date_sub bound in the predicate | PASS |  |
| restart | query:temporal: year-month split grouping | PASS |  |
| restart | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| restart | query:regex: substr extracts the mail domain | PASS |  |
| restart | query:regex: the REGEXP operator anchors a class | PASS |  |
| restart | query:regex: replace folds suffix classes before grouping | PASS |  |
| restart | query:bi metabase: month grain through convert_tz | PASS |  |
| restart | query:bi metabase: iso week bucketing | PASS |  |
| restart | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| restart | query:bi metabase: previous-period revenue window | PASS |  |
| restart | query:bi superset: week-start grain with a rolling average | PASS |  |
| restart | query:bi superset: running total over grouped revenue | PASS |  |
| restart | query:bi superset: lag and lead against a named window | PASS |  |
| restart | query:bi superset: quartile counts from ntile | PASS |  |
| restart | query:bi superset: first and last value over an unbounded frame | PASS |  |
| restart | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| restart | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| restart | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| restart | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| restart | query:bi looker: the grouped primary key determines the row | PASS |  |
| restart | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| restart | query:bi tableau: explicit cast ladder | PASS |  |
| restart | query:bi tableau: the stddev and variance family | PASS |  |
| restart | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| restart | query:bi shared: substring_index dimension cleanup | PASS |  |
| restart | query:bi shared: json validity and typed path filter | PASS |  |
| restart | query:bi shared: contains_path over several paths at once | PASS |  |
| restart | query:bi shared: maketime from extracted parts | PASS |  |
| restart | query:bi shared: extract year_month grouping | PASS |  |
| restart | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| restart | query:staff: three-level management chain with an inactive tail | PASS |  |
| restart | query:staff: active split with id extremes | PASS |  |
| restart | query:counters: full unsigned ladder readback | PASS |  |
| restart | query:counters: greatest and least across widths | PASS |  |
| restart | query:dim: enum status split | PASS |  |
| restart | query:dim: pattern filter across collated columns | PASS |  |
| restart | query:person: anti-join finds owners without facts | PASS |  |
| restart | query:person: created-fact counts through a scalar subquery | PASS |  |
| restart | query:event: lag over per-dimension timelines | PASS |  |
| restart | query:event: daily grain per dimension code | PASS |  |
| restart | query:order_items: product rollup without the orders table | PASS |  |
| restart | query:shipments: carrier value through the items bridge | PASS |  |
| restart | query:json: distinct case variants survive a derived table | PASS |  |
| activity-history | activity-history:the history is in the control plane pintail reads | PASS | 150049 sync_runs rows for db_5aabb588ee70288e4f8cb79e2ac963cd |
| activity-history | activity-history:the feed pages the full history | PASS | limit=200 returned 200 |
| activity-history | activity-history:scoped feed stays fast over a large history | PASS | p50 2ms p95 5ms over 150000 rows |
| activity-history | activity-history:workspace feed stays fast over a large history | PASS | p50 2ms p95 6ms |
| activity-history | activity-history:25 concurrent feed reads do not pile up | PASS | p50 30ms p99 48ms |
| activity-history | activity-history:health answers while the feed is hammered | PASS | health p95 37ms |
| activity-history | converge:Dim | PASS |  |
| activity-history | converge:Event | PASS |  |
| activity-history | converge:Fact | PASS |  |
| activity-history | converge:Person | PASS |  |
| activity-history | converge:audit_log | PASS |  |
| activity-history | converge:badges | PASS |  |
| activity-history | converge:counters | PASS |  |
| activity-history | converge:customers | PASS |  |
| activity-history | converge:order_items | PASS |  |
| activity-history | converge:orders | PASS |  |
| activity-history | converge:shipments | PASS |  |
| activity-history | converge:staff | PASS |  |
| activity-history | converge:information_schema.columns | PASS |  |
| activity-history | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| activity-history | query:conformance: mixed-collation double grouping | PASS |  |
| activity-history | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| activity-history | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| activity-history | query:conformance: case-variant code grouping | PASS |  |
| activity-history | query:conformance: anti-join finds the event-less dimension | PASS |  |
| activity-history | query:conformance: nullable join key NULL-extends | PASS |  |
| activity-history | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| activity-history | query:conformance: date bucketing over the fact table | PASS |  |
| activity-history | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| activity-history | query:point lookup by key | PASS |  |
| activity-history | query:range scan with compound predicate | PASS |  |
| activity-history | query:inner join with aggregation | PASS |  |
| activity-history | query:join with a residual comparison between both inputs | PASS |  |
| activity-history | query:left join keeps rows whose only matches fail the residual | PASS |  |
| activity-history | query:residual comparison through coalesce on a nullable column | PASS |  |
| activity-history | query:created-by and updated-by resolve through separate aliases | PASS |  |
| activity-history | query:alias pair with the join order reversed | PASS |  |
| activity-history | query:four aliases of one table joined in a chain | PASS |  |
| activity-history | query:self-join with a single-side predicate in the ON clause | PASS |  |
| activity-history | query:self-join manager chain preserves the roots | PASS |  |
| activity-history | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| activity-history | query:aliases stay distinct when the empty side joins first | PASS |  |
| activity-history | query:left join preserves unmatched rows | PASS |  |
| activity-history | query:right join preserves unmatched rows | PASS |  |
| activity-history | query:three-way join through items | PASS |  |
| activity-history | query:union all across sources | PASS |  |
| activity-history | query:intersect customer identifiers | PASS |  |
| activity-history | query:except customer identifiers | PASS |  |
| activity-history | query:order by an expression over an aggregate | PASS |  |
| activity-history | query:order by a tree over several aggregates | PASS |  |
| activity-history | query:order by an aggregate absent from the select list | PASS |  |
| activity-history | query:group by with having | PASS |  |
| activity-history | query:conditional decimal sum keeps the fraction | PASS |  |
| activity-history | query:distinct count and min max | PASS |  |
| activity-history | query:uncorrelated in-subquery | PASS |  |
| activity-history | query:correlated exists with inner predicate | PASS |  |
| activity-history | query:correlated scalar aggregate | PASS |  |
| activity-history | query:correlated scalar unique lookup | PASS |  |
| activity-history | query:scalar subquery threshold | PASS |  |
| activity-history | query:non-recursive cte | PASS |  |
| activity-history | query:bounded recursive cte | PASS |  |
| activity-history | query:date bucketing | PASS |  |
| activity-history | query:string functions and like | PASS |  |
| activity-history | query:looker symmetric key helpers | PASS |  |
| activity-history | query:json constructor preserves json versus text | PASS |  |
| activity-history | query:json aggregate embeds documents | PASS |  |
| activity-history | query:regular expression read transforms | PASS |  |
| activity-history | query:case expression buckets | PASS |  |
| activity-history | query:null handling | PASS |  |
| activity-history | query:coalesce and ifnull | PASS |  |
| activity-history | query:enum and set filters | PASS |  |
| activity-history | query:unsigned boundary readback | PASS |  |
| activity-history | query:derived table | PASS |  |
| activity-history | query:group_concat single expression | PASS |  |
| activity-history | query:window ranking per group | PASS |  |
| activity-history | query:window share of total over grouped output | PASS |  |
| activity-history | query:window running total | PASS |  |
| activity-history | query:decimal column average beyond simple sum | PASS |  |
| activity-history | query:computed decimal rounds negative digits half away from zero | PASS |  |
| activity-history | query:json extract filter on customer meta | PASS |  |
| activity-history | query:fan-out join group concat line products | PASS |  |
| activity-history | query:outer join customers without recent orders | PASS |  |
| activity-history | query:set op union distinct tiers and statuses | PASS |  |
| activity-history | query:temporal convert and date_format grain | PASS |  |
| activity-history | query:correlated not exists open orders | PASS |  |
| activity-history | query:window lag payment-shaped totals | PASS |  |
| activity-history | query:multi-key join items to orders | PASS |  |
| activity-history | query:between and null-safe coalesce on balance | PASS |  |
| activity-history | query:intersect all-style customer buyers | PASS |  |
| activity-history | query:derived table status revenue share | PASS |  |
| activity-history | query:general_ci: equality folds ASCII case | PASS |  |
| activity-history | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| activity-history | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| activity-history | query:general_ci: every supplementary character compares equal | PASS |  |
| activity-history | query:general_ci: grouping partitions by collated equality | PASS |  |
| activity-history | query:general_ci: ordering follows the collation, not code points | PASS |  |
| activity-history | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| activity-history | query:general_ci: joining on a collated column | PASS |  |
| activity-history | query:general_ci: representative spelling of a collated group | PASS |  |
| activity-history | query:general_ci: mixing collations across separate comparisons | PASS |  |
| activity-history | query:enum: order by ascends by declared ordinal | PASS |  |
| activity-history | query:enum: order by descends by declared ordinal | PASS |  |
| activity-history | query:enum: min and max compare as strings | PASS |  |
| activity-history | query:enum: a greater-than range compares as strings | PASS |  |
| activity-history | query:enum: a less-than range compares as strings | PASS |  |
| activity-history | query:enum: between compares as strings | PASS |  |
| activity-history | query:enum: distinct orders by ordinal | PASS |  |
| activity-history | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| activity-history | query:enum: a window order walks the ordinal | PASS |  |
| activity-history | query:collation: mixed grouping answers with per-key folds | PASS |  |
| activity-history | query:collation: distinct counts fold per column collation | PASS |  |
| activity-history | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| activity-history | query:set: order by walks the member bitmask | PASS |  |
| activity-history | query:set: grouping orders groups by bitmask | PASS |  |
| activity-history | query:enum: the empty member groups by its ordinal | PASS |  |
| activity-history | query:enum: the empty member sorts by its ordinal | PASS |  |
| activity-history | query:enum: the empty member is selectable by text | PASS |  |
| activity-history | query:geometry: hex round-trips the internal format | PASS |  |
| activity-history | query:geometry: byte length includes the srid prefix | PASS |  |
| activity-history | query:geometry: null routes filter and count | PASS |  |
| activity-history | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| activity-history | query:set: find_in_set filters by membership | PASS |  |
| activity-history | query:set: equality is literal, not member-normalized | PASS |  |
| activity-history | query:set: distinct values walk the bitmask including empty | PASS |  |
| activity-history | query:set: grouped counts order by bitmask not text | PASS |  |
| activity-history | query:set: a range predicate compares the bitmask | PASS |  |
| activity-history | query:star: fact with dimension and two audit persons | PASS |  |
| activity-history | query:star: five-alias chain fans out through events | PASS |  |
| activity-history | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| activity-history | query:star: five tables bridge the shop and the star | PASS |  |
| activity-history | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| activity-history | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| activity-history | query:json: length and keys survive null documents | PASS |  |
| activity-history | query:json: contains_path filters the documented rows | PASS |  |
| activity-history | query:json: json_value reads a scalar with sql semantics | PASS |  |
| activity-history | query:json: object construction embeds an extracted scalar | PASS |  |
| activity-history | query:json: search locates a literal value | PASS |  |
| activity-history | query:json: grouping by an extracted scalar | PASS |  |
| activity-history | query:json: merge_patch overlays and reads back | PASS |  |
| activity-history | query:temporal: quarter, weekday and name grains agree | PASS |  |
| activity-history | query:temporal: month-end bucketing via last_day | PASS |  |
| activity-history | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| activity-history | query:temporal: datetime range keeps the year window | PASS |  |
| activity-history | query:temporal: date_sub bound in the predicate | PASS |  |
| activity-history | query:temporal: year-month split grouping | PASS |  |
| activity-history | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| activity-history | query:regex: substr extracts the mail domain | PASS |  |
| activity-history | query:regex: the REGEXP operator anchors a class | PASS |  |
| activity-history | query:regex: replace folds suffix classes before grouping | PASS |  |
| activity-history | query:bi metabase: month grain through convert_tz | PASS |  |
| activity-history | query:bi metabase: iso week bucketing | PASS |  |
| activity-history | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| activity-history | query:bi metabase: previous-period revenue window | PASS |  |
| activity-history | query:bi superset: week-start grain with a rolling average | PASS |  |
| activity-history | query:bi superset: running total over grouped revenue | PASS |  |
| activity-history | query:bi superset: lag and lead against a named window | PASS |  |
| activity-history | query:bi superset: quartile counts from ntile | PASS |  |
| activity-history | query:bi superset: first and last value over an unbounded frame | PASS |  |
| activity-history | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| activity-history | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| activity-history | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| activity-history | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| activity-history | query:bi looker: the grouped primary key determines the row | PASS |  |
| activity-history | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| activity-history | query:bi tableau: explicit cast ladder | PASS |  |
| activity-history | query:bi tableau: the stddev and variance family | PASS |  |
| activity-history | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| activity-history | query:bi shared: substring_index dimension cleanup | PASS |  |
| activity-history | query:bi shared: json validity and typed path filter | PASS |  |
| activity-history | query:bi shared: contains_path over several paths at once | PASS |  |
| activity-history | query:bi shared: maketime from extracted parts | PASS |  |
| activity-history | query:bi shared: extract year_month grouping | PASS |  |
| activity-history | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| activity-history | query:staff: three-level management chain with an inactive tail | PASS |  |
| activity-history | query:staff: active split with id extremes | PASS |  |
| activity-history | query:counters: full unsigned ladder readback | PASS |  |
| activity-history | query:counters: greatest and least across widths | PASS |  |
| activity-history | query:dim: enum status split | PASS |  |
| activity-history | query:dim: pattern filter across collated columns | PASS |  |
| activity-history | query:person: anti-join finds owners without facts | PASS |  |
| activity-history | query:person: created-fact counts through a scalar subquery | PASS |  |
| activity-history | query:event: lag over per-dimension timelines | PASS |  |
| activity-history | query:event: daily grain per dimension code | PASS |  |
| activity-history | query:order_items: product rollup without the orders table | PASS |  |
| activity-history | query:shipments: carrier value through the items bridge | PASS |  |
| activity-history | query:json: distinct case variants survive a derived table | PASS |  |
| poll-storm | poll-storm:no request fails under 25 open dashboards | PASS | 0 failed of 4753 |
| poll-storm | poll-storm:latency stays bounded | PASS | 4753 requests: p50 3ms p99 20ms |
| poll-storm | poll-storm:health never stalls | PASS | health p99 21ms |
| poll-storm | poll-storm:replication keeps pace under the storm | PASS | orders replica 620 vs source 620 |
| poll-storm | converge:Dim | PASS |  |
| poll-storm | converge:Event | PASS |  |
| poll-storm | converge:Fact | PASS |  |
| poll-storm | converge:Person | PASS |  |
| poll-storm | converge:audit_log | PASS |  |
| poll-storm | converge:badges | PASS |  |
| poll-storm | converge:counters | PASS |  |
| poll-storm | converge:customers | PASS |  |
| poll-storm | converge:order_items | PASS |  |
| poll-storm | converge:orders | PASS |  |
| poll-storm | converge:shipments | PASS |  |
| poll-storm | converge:staff | PASS |  |
| poll-storm | converge:information_schema.columns | PASS |  |
| poll-storm | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| poll-storm | query:conformance: mixed-collation double grouping | PASS |  |
| poll-storm | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| poll-storm | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| poll-storm | query:conformance: case-variant code grouping | PASS |  |
| poll-storm | query:conformance: anti-join finds the event-less dimension | PASS |  |
| poll-storm | query:conformance: nullable join key NULL-extends | PASS |  |
| poll-storm | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| poll-storm | query:conformance: date bucketing over the fact table | PASS |  |
| poll-storm | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| poll-storm | query:point lookup by key | PASS |  |
| poll-storm | query:range scan with compound predicate | PASS |  |
| poll-storm | query:inner join with aggregation | PASS |  |
| poll-storm | query:join with a residual comparison between both inputs | PASS |  |
| poll-storm | query:left join keeps rows whose only matches fail the residual | PASS |  |
| poll-storm | query:residual comparison through coalesce on a nullable column | PASS |  |
| poll-storm | query:created-by and updated-by resolve through separate aliases | PASS |  |
| poll-storm | query:alias pair with the join order reversed | PASS |  |
| poll-storm | query:four aliases of one table joined in a chain | PASS |  |
| poll-storm | query:self-join with a single-side predicate in the ON clause | PASS |  |
| poll-storm | query:self-join manager chain preserves the roots | PASS |  |
| poll-storm | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| poll-storm | query:aliases stay distinct when the empty side joins first | PASS |  |
| poll-storm | query:left join preserves unmatched rows | PASS |  |
| poll-storm | query:right join preserves unmatched rows | PASS |  |
| poll-storm | query:three-way join through items | PASS |  |
| poll-storm | query:union all across sources | PASS |  |
| poll-storm | query:intersect customer identifiers | PASS |  |
| poll-storm | query:except customer identifiers | PASS |  |
| poll-storm | query:order by an expression over an aggregate | PASS |  |
| poll-storm | query:order by a tree over several aggregates | PASS |  |
| poll-storm | query:order by an aggregate absent from the select list | PASS |  |
| poll-storm | query:group by with having | PASS |  |
| poll-storm | query:conditional decimal sum keeps the fraction | PASS |  |
| poll-storm | query:distinct count and min max | PASS |  |
| poll-storm | query:uncorrelated in-subquery | PASS |  |
| poll-storm | query:correlated exists with inner predicate | PASS |  |
| poll-storm | query:correlated scalar aggregate | PASS |  |
| poll-storm | query:correlated scalar unique lookup | PASS |  |
| poll-storm | query:scalar subquery threshold | PASS |  |
| poll-storm | query:non-recursive cte | PASS |  |
| poll-storm | query:bounded recursive cte | PASS |  |
| poll-storm | query:date bucketing | PASS |  |
| poll-storm | query:string functions and like | PASS |  |
| poll-storm | query:looker symmetric key helpers | PASS |  |
| poll-storm | query:json constructor preserves json versus text | PASS |  |
| poll-storm | query:json aggregate embeds documents | PASS |  |
| poll-storm | query:regular expression read transforms | PASS |  |
| poll-storm | query:case expression buckets | PASS |  |
| poll-storm | query:null handling | PASS |  |
| poll-storm | query:coalesce and ifnull | PASS |  |
| poll-storm | query:enum and set filters | PASS |  |
| poll-storm | query:unsigned boundary readback | PASS |  |
| poll-storm | query:derived table | PASS |  |
| poll-storm | query:group_concat single expression | PASS |  |
| poll-storm | query:window ranking per group | PASS |  |
| poll-storm | query:window share of total over grouped output | PASS |  |
| poll-storm | query:window running total | PASS |  |
| poll-storm | query:decimal column average beyond simple sum | PASS |  |
| poll-storm | query:computed decimal rounds negative digits half away from zero | PASS |  |
| poll-storm | query:json extract filter on customer meta | PASS |  |
| poll-storm | query:fan-out join group concat line products | PASS |  |
| poll-storm | query:outer join customers without recent orders | PASS |  |
| poll-storm | query:set op union distinct tiers and statuses | PASS |  |
| poll-storm | query:temporal convert and date_format grain | PASS |  |
| poll-storm | query:correlated not exists open orders | PASS |  |
| poll-storm | query:window lag payment-shaped totals | PASS |  |
| poll-storm | query:multi-key join items to orders | PASS |  |
| poll-storm | query:between and null-safe coalesce on balance | PASS |  |
| poll-storm | query:intersect all-style customer buyers | PASS |  |
| poll-storm | query:derived table status revenue share | PASS |  |
| poll-storm | query:general_ci: equality folds ASCII case | PASS |  |
| poll-storm | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| poll-storm | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| poll-storm | query:general_ci: every supplementary character compares equal | PASS |  |
| poll-storm | query:general_ci: grouping partitions by collated equality | PASS |  |
| poll-storm | query:general_ci: ordering follows the collation, not code points | PASS |  |
| poll-storm | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| poll-storm | query:general_ci: joining on a collated column | PASS |  |
| poll-storm | query:general_ci: representative spelling of a collated group | PASS |  |
| poll-storm | query:general_ci: mixing collations across separate comparisons | PASS |  |
| poll-storm | query:enum: order by ascends by declared ordinal | PASS |  |
| poll-storm | query:enum: order by descends by declared ordinal | PASS |  |
| poll-storm | query:enum: min and max compare as strings | PASS |  |
| poll-storm | query:enum: a greater-than range compares as strings | PASS |  |
| poll-storm | query:enum: a less-than range compares as strings | PASS |  |
| poll-storm | query:enum: between compares as strings | PASS |  |
| poll-storm | query:enum: distinct orders by ordinal | PASS |  |
| poll-storm | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| poll-storm | query:enum: a window order walks the ordinal | PASS |  |
| poll-storm | query:collation: mixed grouping answers with per-key folds | PASS |  |
| poll-storm | query:collation: distinct counts fold per column collation | PASS |  |
| poll-storm | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| poll-storm | query:set: order by walks the member bitmask | PASS |  |
| poll-storm | query:set: grouping orders groups by bitmask | PASS |  |
| poll-storm | query:enum: the empty member groups by its ordinal | PASS |  |
| poll-storm | query:enum: the empty member sorts by its ordinal | PASS |  |
| poll-storm | query:enum: the empty member is selectable by text | PASS |  |
| poll-storm | query:geometry: hex round-trips the internal format | PASS |  |
| poll-storm | query:geometry: byte length includes the srid prefix | PASS |  |
| poll-storm | query:geometry: null routes filter and count | PASS |  |
| poll-storm | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| poll-storm | query:set: find_in_set filters by membership | PASS |  |
| poll-storm | query:set: equality is literal, not member-normalized | PASS |  |
| poll-storm | query:set: distinct values walk the bitmask including empty | PASS |  |
| poll-storm | query:set: grouped counts order by bitmask not text | PASS |  |
| poll-storm | query:set: a range predicate compares the bitmask | PASS |  |
| poll-storm | query:star: fact with dimension and two audit persons | PASS |  |
| poll-storm | query:star: five-alias chain fans out through events | PASS |  |
| poll-storm | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| poll-storm | query:star: five tables bridge the shop and the star | PASS |  |
| poll-storm | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| poll-storm | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| poll-storm | query:json: length and keys survive null documents | PASS |  |
| poll-storm | query:json: contains_path filters the documented rows | PASS |  |
| poll-storm | query:json: json_value reads a scalar with sql semantics | PASS |  |
| poll-storm | query:json: object construction embeds an extracted scalar | PASS |  |
| poll-storm | query:json: search locates a literal value | PASS |  |
| poll-storm | query:json: grouping by an extracted scalar | PASS |  |
| poll-storm | query:json: merge_patch overlays and reads back | PASS |  |
| poll-storm | query:temporal: quarter, weekday and name grains agree | PASS |  |
| poll-storm | query:temporal: month-end bucketing via last_day | PASS |  |
| poll-storm | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| poll-storm | query:temporal: datetime range keeps the year window | PASS |  |
| poll-storm | query:temporal: date_sub bound in the predicate | PASS |  |
| poll-storm | query:temporal: year-month split grouping | PASS |  |
| poll-storm | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| poll-storm | query:regex: substr extracts the mail domain | PASS |  |
| poll-storm | query:regex: the REGEXP operator anchors a class | PASS |  |
| poll-storm | query:regex: replace folds suffix classes before grouping | PASS |  |
| poll-storm | query:bi metabase: month grain through convert_tz | PASS |  |
| poll-storm | query:bi metabase: iso week bucketing | PASS |  |
| poll-storm | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| poll-storm | query:bi metabase: previous-period revenue window | PASS |  |
| poll-storm | query:bi superset: week-start grain with a rolling average | PASS |  |
| poll-storm | query:bi superset: running total over grouped revenue | PASS |  |
| poll-storm | query:bi superset: lag and lead against a named window | PASS |  |
| poll-storm | query:bi superset: quartile counts from ntile | PASS |  |
| poll-storm | query:bi superset: first and last value over an unbounded frame | PASS |  |
| poll-storm | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| poll-storm | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| poll-storm | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| poll-storm | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| poll-storm | query:bi looker: the grouped primary key determines the row | PASS |  |
| poll-storm | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| poll-storm | query:bi tableau: explicit cast ladder | PASS |  |
| poll-storm | query:bi tableau: the stddev and variance family | PASS |  |
| poll-storm | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| poll-storm | query:bi shared: substring_index dimension cleanup | PASS |  |
| poll-storm | query:bi shared: json validity and typed path filter | PASS |  |
| poll-storm | query:bi shared: contains_path over several paths at once | PASS |  |
| poll-storm | query:bi shared: maketime from extracted parts | PASS |  |
| poll-storm | query:bi shared: extract year_month grouping | PASS |  |
| poll-storm | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| poll-storm | query:staff: three-level management chain with an inactive tail | PASS |  |
| poll-storm | query:staff: active split with id extremes | PASS |  |
| poll-storm | query:counters: full unsigned ladder readback | PASS |  |
| poll-storm | query:counters: greatest and least across widths | PASS |  |
| poll-storm | query:dim: enum status split | PASS |  |
| poll-storm | query:dim: pattern filter across collated columns | PASS |  |
| poll-storm | query:person: anti-join finds owners without facts | PASS |  |
| poll-storm | query:person: created-fact counts through a scalar subquery | PASS |  |
| poll-storm | query:event: lag over per-dimension timelines | PASS |  |
| poll-storm | query:event: daily grain per dimension code | PASS |  |
| poll-storm | query:order_items: product rollup without the orders table | PASS |  |
| poll-storm | query:shipments: carrier value through the items bridge | PASS |  |
| poll-storm | query:json: distinct case variants survive a derived table | PASS |  |
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
| control-plane | api:wire column types: temporal expressions advertise what MySQL advertises | PASS |  |
| control-plane | api:erroring queries carry MySQL errno and SQLSTATE | PASS |  |
| control-plane | api:the audit trail records the network peer of every action | PASS |  |
| control-plane | api:resync and reconcile are accepted | PASS |  |
| control-plane | api:resync recopies only the table it names | PASS |  |
| control-plane | api:schema drift during downtime: purged DDL recovers by re-probe | PASS |  |
| control-plane | api:reset starts the mirror over with the saved connection | PASS |  |
| control-plane | api:keyless policy: ambiguity quarantines and exact multiplicity repairs | PASS |  |
| control-plane | api:a connection string carrying client driver options registers | PASS |  |
| control-plane | api:throwaway database lifecycle: create, update, delete | PASS |  |
| control-plane | converge:Dim | PASS |  |
| control-plane | converge:Event | PASS |  |
| control-plane | converge:Fact | PASS |  |
| control-plane | converge:Person | PASS |  |
| control-plane | converge:audit_log | PASS |  |
| control-plane | converge:badges | PASS |  |
| control-plane | converge:counters | PASS |  |
| control-plane | converge:customers | PASS |  |
| control-plane | converge:keyless_log | PASS |  |
| control-plane | converge:order_items | PASS |  |
| control-plane | converge:orders | PASS |  |
| control-plane | converge:shipments | PASS |  |
| control-plane | converge:staff | PASS |  |
| control-plane | converge:information_schema.columns | PASS |  |
| control-plane | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| control-plane | query:conformance: mixed-collation double grouping | PASS |  |
| control-plane | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| control-plane | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| control-plane | query:conformance: case-variant code grouping | PASS |  |
| control-plane | query:conformance: anti-join finds the event-less dimension | PASS |  |
| control-plane | query:conformance: nullable join key NULL-extends | PASS |  |
| control-plane | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| control-plane | query:conformance: date bucketing over the fact table | PASS |  |
| control-plane | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| control-plane | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| control-plane | query:enum: the empty member groups by its ordinal | PASS |  |
| control-plane | query:enum: the empty member sorts by its ordinal | PASS |  |
| control-plane | query:enum: the empty member is selectable by text | PASS |  |
| control-plane | query:geometry: hex round-trips the internal format | PASS |  |
| control-plane | query:geometry: byte length includes the srid prefix | PASS |  |
| control-plane | query:geometry: null routes filter and count | PASS |  |
| control-plane | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| control-plane | query:set: find_in_set filters by membership | PASS |  |
| control-plane | query:set: equality is literal, not member-normalized | PASS |  |
| control-plane | query:set: distinct values walk the bitmask including empty | PASS |  |
| control-plane | query:set: grouped counts order by bitmask not text | PASS |  |
| control-plane | query:set: a range predicate compares the bitmask | PASS |  |
| control-plane | query:star: fact with dimension and two audit persons | PASS |  |
| control-plane | query:star: five-alias chain fans out through events | PASS |  |
| control-plane | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| control-plane | query:star: five tables bridge the shop and the star | PASS |  |
| control-plane | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| control-plane | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| control-plane | query:json: length and keys survive null documents | PASS |  |
| control-plane | query:json: contains_path filters the documented rows | PASS |  |
| control-plane | query:json: json_value reads a scalar with sql semantics | PASS |  |
| control-plane | query:json: object construction embeds an extracted scalar | PASS |  |
| control-plane | query:json: search locates a literal value | PASS |  |
| control-plane | query:json: grouping by an extracted scalar | PASS |  |
| control-plane | query:json: merge_patch overlays and reads back | PASS |  |
| control-plane | query:temporal: quarter, weekday and name grains agree | PASS |  |
| control-plane | query:temporal: month-end bucketing via last_day | PASS |  |
| control-plane | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| control-plane | query:temporal: datetime range keeps the year window | PASS |  |
| control-plane | query:temporal: date_sub bound in the predicate | PASS |  |
| control-plane | query:temporal: year-month split grouping | PASS |  |
| control-plane | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| control-plane | query:regex: substr extracts the mail domain | PASS |  |
| control-plane | query:regex: the REGEXP operator anchors a class | PASS |  |
| control-plane | query:regex: replace folds suffix classes before grouping | PASS |  |
| control-plane | query:bi metabase: month grain through convert_tz | PASS |  |
| control-plane | query:bi metabase: iso week bucketing | PASS |  |
| control-plane | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| control-plane | query:bi metabase: previous-period revenue window | PASS |  |
| control-plane | query:bi superset: week-start grain with a rolling average | PASS |  |
| control-plane | query:bi superset: running total over grouped revenue | PASS |  |
| control-plane | query:bi superset: lag and lead against a named window | PASS |  |
| control-plane | query:bi superset: quartile counts from ntile | PASS |  |
| control-plane | query:bi superset: first and last value over an unbounded frame | PASS |  |
| control-plane | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| control-plane | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| control-plane | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| control-plane | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| control-plane | query:bi looker: the grouped primary key determines the row | PASS |  |
| control-plane | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| control-plane | query:bi tableau: explicit cast ladder | PASS |  |
| control-plane | query:bi tableau: the stddev and variance family | PASS |  |
| control-plane | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| control-plane | query:bi shared: substring_index dimension cleanup | PASS |  |
| control-plane | query:bi shared: json validity and typed path filter | PASS |  |
| control-plane | query:bi shared: contains_path over several paths at once | PASS |  |
| control-plane | query:bi shared: maketime from extracted parts | PASS |  |
| control-plane | query:bi shared: extract year_month grouping | PASS |  |
| control-plane | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| control-plane | query:staff: three-level management chain with an inactive tail | PASS |  |
| control-plane | query:staff: active split with id extremes | PASS |  |
| control-plane | query:counters: full unsigned ladder readback | PASS |  |
| control-plane | query:counters: greatest and least across widths | PASS |  |
| control-plane | query:dim: enum status split | PASS |  |
| control-plane | query:dim: pattern filter across collated columns | PASS |  |
| control-plane | query:person: anti-join finds owners without facts | PASS |  |
| control-plane | query:person: created-fact counts through a scalar subquery | PASS |  |
| control-plane | query:event: lag over per-dimension timelines | PASS |  |
| control-plane | query:event: daily grain per dimension code | PASS |  |
| control-plane | query:order_items: product rollup without the orders table | PASS |  |
| control-plane | query:shipments: carrier value through the items bridge | PASS |  |
| control-plane | query:json: distinct case variants survive a derived table | PASS |  |
| snapshot-ddl-window | a table created just before a forced snapshot is still adopted | PASS |  |
| snapshot-ddl-window | converge:Dim | PASS |  |
| snapshot-ddl-window | converge:Event | PASS |  |
| snapshot-ddl-window | converge:Fact | PASS |  |
| snapshot-ddl-window | converge:Person | PASS |  |
| snapshot-ddl-window | converge:audit_log | PASS |  |
| snapshot-ddl-window | converge:badges | PASS |  |
| snapshot-ddl-window | converge:counters | PASS |  |
| snapshot-ddl-window | converge:customers | PASS |  |
| snapshot-ddl-window | converge:keyless_log | PASS |  |
| snapshot-ddl-window | converge:order_items | PASS |  |
| snapshot-ddl-window | converge:orders | PASS |  |
| snapshot-ddl-window | converge:shipments | PASS |  |
| snapshot-ddl-window | converge:staff | PASS |  |
| snapshot-ddl-window | converge:information_schema.columns | PASS |  |
| snapshot-ddl-window | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| snapshot-ddl-window | query:conformance: mixed-collation double grouping | PASS |  |
| snapshot-ddl-window | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| snapshot-ddl-window | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| snapshot-ddl-window | query:conformance: case-variant code grouping | PASS |  |
| snapshot-ddl-window | query:conformance: anti-join finds the event-less dimension | PASS |  |
| snapshot-ddl-window | query:conformance: nullable join key NULL-extends | PASS |  |
| snapshot-ddl-window | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| snapshot-ddl-window | query:conformance: date bucketing over the fact table | PASS |  |
| snapshot-ddl-window | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| snapshot-ddl-window | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| snapshot-ddl-window | query:enum: the empty member groups by its ordinal | PASS |  |
| snapshot-ddl-window | query:enum: the empty member sorts by its ordinal | PASS |  |
| snapshot-ddl-window | query:enum: the empty member is selectable by text | PASS |  |
| snapshot-ddl-window | query:geometry: hex round-trips the internal format | PASS |  |
| snapshot-ddl-window | query:geometry: byte length includes the srid prefix | PASS |  |
| snapshot-ddl-window | query:geometry: null routes filter and count | PASS |  |
| snapshot-ddl-window | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| snapshot-ddl-window | query:set: find_in_set filters by membership | PASS |  |
| snapshot-ddl-window | query:set: equality is literal, not member-normalized | PASS |  |
| snapshot-ddl-window | query:set: distinct values walk the bitmask including empty | PASS |  |
| snapshot-ddl-window | query:set: grouped counts order by bitmask not text | PASS |  |
| snapshot-ddl-window | query:set: a range predicate compares the bitmask | PASS |  |
| snapshot-ddl-window | query:star: fact with dimension and two audit persons | PASS |  |
| snapshot-ddl-window | query:star: five-alias chain fans out through events | PASS |  |
| snapshot-ddl-window | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| snapshot-ddl-window | query:star: five tables bridge the shop and the star | PASS |  |
| snapshot-ddl-window | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| snapshot-ddl-window | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| snapshot-ddl-window | query:json: length and keys survive null documents | PASS |  |
| snapshot-ddl-window | query:json: contains_path filters the documented rows | PASS |  |
| snapshot-ddl-window | query:json: json_value reads a scalar with sql semantics | PASS |  |
| snapshot-ddl-window | query:json: object construction embeds an extracted scalar | PASS |  |
| snapshot-ddl-window | query:json: search locates a literal value | PASS |  |
| snapshot-ddl-window | query:json: grouping by an extracted scalar | PASS |  |
| snapshot-ddl-window | query:json: merge_patch overlays and reads back | PASS |  |
| snapshot-ddl-window | query:temporal: quarter, weekday and name grains agree | PASS |  |
| snapshot-ddl-window | query:temporal: month-end bucketing via last_day | PASS |  |
| snapshot-ddl-window | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| snapshot-ddl-window | query:temporal: datetime range keeps the year window | PASS |  |
| snapshot-ddl-window | query:temporal: date_sub bound in the predicate | PASS |  |
| snapshot-ddl-window | query:temporal: year-month split grouping | PASS |  |
| snapshot-ddl-window | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| snapshot-ddl-window | query:regex: substr extracts the mail domain | PASS |  |
| snapshot-ddl-window | query:regex: the REGEXP operator anchors a class | PASS |  |
| snapshot-ddl-window | query:regex: replace folds suffix classes before grouping | PASS |  |
| snapshot-ddl-window | query:bi metabase: month grain through convert_tz | PASS |  |
| snapshot-ddl-window | query:bi metabase: iso week bucketing | PASS |  |
| snapshot-ddl-window | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| snapshot-ddl-window | query:bi metabase: previous-period revenue window | PASS |  |
| snapshot-ddl-window | query:bi superset: week-start grain with a rolling average | PASS |  |
| snapshot-ddl-window | query:bi superset: running total over grouped revenue | PASS |  |
| snapshot-ddl-window | query:bi superset: lag and lead against a named window | PASS |  |
| snapshot-ddl-window | query:bi superset: quartile counts from ntile | PASS |  |
| snapshot-ddl-window | query:bi superset: first and last value over an unbounded frame | PASS |  |
| snapshot-ddl-window | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| snapshot-ddl-window | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| snapshot-ddl-window | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| snapshot-ddl-window | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| snapshot-ddl-window | query:bi looker: the grouped primary key determines the row | PASS |  |
| snapshot-ddl-window | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| snapshot-ddl-window | query:bi tableau: explicit cast ladder | PASS |  |
| snapshot-ddl-window | query:bi tableau: the stddev and variance family | PASS |  |
| snapshot-ddl-window | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| snapshot-ddl-window | query:bi shared: substring_index dimension cleanup | PASS |  |
| snapshot-ddl-window | query:bi shared: json validity and typed path filter | PASS |  |
| snapshot-ddl-window | query:bi shared: contains_path over several paths at once | PASS |  |
| snapshot-ddl-window | query:bi shared: maketime from extracted parts | PASS |  |
| snapshot-ddl-window | query:bi shared: extract year_month grouping | PASS |  |
| snapshot-ddl-window | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| snapshot-ddl-window | query:staff: three-level management chain with an inactive tail | PASS |  |
| snapshot-ddl-window | query:staff: active split with id extremes | PASS |  |
| snapshot-ddl-window | query:counters: full unsigned ladder readback | PASS |  |
| snapshot-ddl-window | query:counters: greatest and least across widths | PASS |  |
| snapshot-ddl-window | query:dim: enum status split | PASS |  |
| snapshot-ddl-window | query:dim: pattern filter across collated columns | PASS |  |
| snapshot-ddl-window | query:person: anti-join finds owners without facts | PASS |  |
| snapshot-ddl-window | query:person: created-fact counts through a scalar subquery | PASS |  |
| snapshot-ddl-window | query:event: lag over per-dimension timelines | PASS |  |
| snapshot-ddl-window | query:event: daily grain per dimension code | PASS |  |
| snapshot-ddl-window | query:order_items: product rollup without the orders table | PASS |  |
| snapshot-ddl-window | query:shipments: carrier value through the items bridge | PASS |  |
| snapshot-ddl-window | query:json: distinct case variants survive a derived table | PASS |  |
| drop-table-cdc | drop-table:replicates before the drop | PASS |  |
| drop-table-cdc | drop-table:source drop marks the table orphaned | PASS |  |
| drop-table-cdc | drop-table:the rest of the database keeps replicating | PASS |  |
| drop-table-cdc | drop-table:orphan is retired without an operator re-probe | WARN | DROP TABLE retains the replica as an orphan and does not refresh the stored probe report, so the table stays in the replica catalog until an operator re-probes (3 rows still served) |
| drop-table-cdc | drop-table:re-probe retires the orphan from the catalog | PASS |  |
| drop-table-cdc | converge:Dim | PASS |  |
| drop-table-cdc | converge:Event | PASS |  |
| drop-table-cdc | converge:Fact | PASS |  |
| drop-table-cdc | converge:Person | PASS |  |
| drop-table-cdc | converge:audit_log | PASS |  |
| drop-table-cdc | converge:badges | PASS |  |
| drop-table-cdc | converge:counters | PASS |  |
| drop-table-cdc | converge:customers | PASS |  |
| drop-table-cdc | converge:keyless_log | PASS |  |
| drop-table-cdc | converge:order_items | PASS |  |
| drop-table-cdc | converge:orders | PASS |  |
| drop-table-cdc | converge:shipments | PASS |  |
| drop-table-cdc | converge:staff | PASS |  |
| drop-table-cdc | converge:information_schema.columns | PASS |  |
| drop-table-cdc | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| drop-table-cdc | query:conformance: mixed-collation double grouping | PASS |  |
| drop-table-cdc | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| drop-table-cdc | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| drop-table-cdc | query:conformance: case-variant code grouping | PASS |  |
| drop-table-cdc | query:conformance: anti-join finds the event-less dimension | PASS |  |
| drop-table-cdc | query:conformance: nullable join key NULL-extends | PASS |  |
| drop-table-cdc | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| drop-table-cdc | query:conformance: date bucketing over the fact table | PASS |  |
| drop-table-cdc | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| drop-table-cdc | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| drop-table-cdc | query:enum: the empty member groups by its ordinal | PASS |  |
| drop-table-cdc | query:enum: the empty member sorts by its ordinal | PASS |  |
| drop-table-cdc | query:enum: the empty member is selectable by text | PASS |  |
| drop-table-cdc | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-cdc | query:geometry: byte length includes the srid prefix | PASS |  |
| drop-table-cdc | query:geometry: null routes filter and count | PASS |  |
| drop-table-cdc | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| drop-table-cdc | query:set: find_in_set filters by membership | PASS |  |
| drop-table-cdc | query:set: equality is literal, not member-normalized | PASS |  |
| drop-table-cdc | query:set: distinct values walk the bitmask including empty | PASS |  |
| drop-table-cdc | query:set: grouped counts order by bitmask not text | PASS |  |
| drop-table-cdc | query:set: a range predicate compares the bitmask | PASS |  |
| drop-table-cdc | query:star: fact with dimension and two audit persons | PASS |  |
| drop-table-cdc | query:star: five-alias chain fans out through events | PASS |  |
| drop-table-cdc | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| drop-table-cdc | query:star: five tables bridge the shop and the star | PASS |  |
| drop-table-cdc | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| drop-table-cdc | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| drop-table-cdc | query:json: length and keys survive null documents | PASS |  |
| drop-table-cdc | query:json: contains_path filters the documented rows | PASS |  |
| drop-table-cdc | query:json: json_value reads a scalar with sql semantics | PASS |  |
| drop-table-cdc | query:json: object construction embeds an extracted scalar | PASS |  |
| drop-table-cdc | query:json: search locates a literal value | PASS |  |
| drop-table-cdc | query:json: grouping by an extracted scalar | PASS |  |
| drop-table-cdc | query:json: merge_patch overlays and reads back | PASS |  |
| drop-table-cdc | query:temporal: quarter, weekday and name grains agree | PASS |  |
| drop-table-cdc | query:temporal: month-end bucketing via last_day | PASS |  |
| drop-table-cdc | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| drop-table-cdc | query:temporal: datetime range keeps the year window | PASS |  |
| drop-table-cdc | query:temporal: date_sub bound in the predicate | PASS |  |
| drop-table-cdc | query:temporal: year-month split grouping | PASS |  |
| drop-table-cdc | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| drop-table-cdc | query:regex: substr extracts the mail domain | PASS |  |
| drop-table-cdc | query:regex: the REGEXP operator anchors a class | PASS |  |
| drop-table-cdc | query:regex: replace folds suffix classes before grouping | PASS |  |
| drop-table-cdc | query:bi metabase: month grain through convert_tz | PASS |  |
| drop-table-cdc | query:bi metabase: iso week bucketing | PASS |  |
| drop-table-cdc | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| drop-table-cdc | query:bi metabase: previous-period revenue window | PASS |  |
| drop-table-cdc | query:bi superset: week-start grain with a rolling average | PASS |  |
| drop-table-cdc | query:bi superset: running total over grouped revenue | PASS |  |
| drop-table-cdc | query:bi superset: lag and lead against a named window | PASS |  |
| drop-table-cdc | query:bi superset: quartile counts from ntile | PASS |  |
| drop-table-cdc | query:bi superset: first and last value over an unbounded frame | PASS |  |
| drop-table-cdc | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| drop-table-cdc | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| drop-table-cdc | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| drop-table-cdc | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| drop-table-cdc | query:bi looker: the grouped primary key determines the row | PASS |  |
| drop-table-cdc | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| drop-table-cdc | query:bi tableau: explicit cast ladder | PASS |  |
| drop-table-cdc | query:bi tableau: the stddev and variance family | PASS |  |
| drop-table-cdc | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| drop-table-cdc | query:bi shared: substring_index dimension cleanup | PASS |  |
| drop-table-cdc | query:bi shared: json validity and typed path filter | PASS |  |
| drop-table-cdc | query:bi shared: contains_path over several paths at once | PASS |  |
| drop-table-cdc | query:bi shared: maketime from extracted parts | PASS |  |
| drop-table-cdc | query:bi shared: extract year_month grouping | PASS |  |
| drop-table-cdc | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| drop-table-cdc | query:staff: three-level management chain with an inactive tail | PASS |  |
| drop-table-cdc | query:staff: active split with id extremes | PASS |  |
| drop-table-cdc | query:counters: full unsigned ladder readback | PASS |  |
| drop-table-cdc | query:counters: greatest and least across widths | PASS |  |
| drop-table-cdc | query:dim: enum status split | PASS |  |
| drop-table-cdc | query:dim: pattern filter across collated columns | PASS |  |
| drop-table-cdc | query:person: anti-join finds owners without facts | PASS |  |
| drop-table-cdc | query:person: created-fact counts through a scalar subquery | PASS |  |
| drop-table-cdc | query:event: lag over per-dimension timelines | PASS |  |
| drop-table-cdc | query:event: daily grain per dimension code | PASS |  |
| drop-table-cdc | query:order_items: product rollup without the orders table | PASS |  |
| drop-table-cdc | query:shipments: carrier value through the items bridge | PASS |  |
| drop-table-cdc | query:json: distinct case variants survive a derived table | PASS |  |
| drop-table-recreate | recreate:first generation replicates | PASS |  |
| drop-table-recreate | recreate:a table recreated under the same name replicates as a new table | WARN | the source has 2 rows and the replica 4: the orphaned store is reused instead of being resnapshotted, because the CREATE handler skips any name it already tracks |
| drop-table-recreate | recreate:the rest of the database keeps replicating | PASS |  |
| drop-table-recreate | converge:Dim | PASS |  |
| drop-table-recreate | converge:Event | PASS |  |
| drop-table-recreate | converge:Fact | PASS |  |
| drop-table-recreate | converge:Person | PASS |  |
| drop-table-recreate | converge:audit_log | PASS |  |
| drop-table-recreate | converge:badges | PASS |  |
| drop-table-recreate | converge:counters | PASS |  |
| drop-table-recreate | converge:customers | PASS |  |
| drop-table-recreate | converge:keyless_log | PASS |  |
| drop-table-recreate | converge:order_items | PASS |  |
| drop-table-recreate | converge:orders | PASS |  |
| drop-table-recreate | converge:shipments | PASS |  |
| drop-table-recreate | converge:staff | PASS |  |
| drop-table-recreate | converge:information_schema.columns | PASS |  |
| drop-table-recreate | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| drop-table-recreate | query:conformance: mixed-collation double grouping | PASS |  |
| drop-table-recreate | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| drop-table-recreate | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| drop-table-recreate | query:conformance: case-variant code grouping | PASS |  |
| drop-table-recreate | query:conformance: anti-join finds the event-less dimension | PASS |  |
| drop-table-recreate | query:conformance: nullable join key NULL-extends | PASS |  |
| drop-table-recreate | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| drop-table-recreate | query:conformance: date bucketing over the fact table | PASS |  |
| drop-table-recreate | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| drop-table-recreate | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| drop-table-recreate | query:enum: the empty member groups by its ordinal | PASS |  |
| drop-table-recreate | query:enum: the empty member sorts by its ordinal | PASS |  |
| drop-table-recreate | query:enum: the empty member is selectable by text | PASS |  |
| drop-table-recreate | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-recreate | query:geometry: byte length includes the srid prefix | PASS |  |
| drop-table-recreate | query:geometry: null routes filter and count | PASS |  |
| drop-table-recreate | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| drop-table-recreate | query:set: find_in_set filters by membership | PASS |  |
| drop-table-recreate | query:set: equality is literal, not member-normalized | PASS |  |
| drop-table-recreate | query:set: distinct values walk the bitmask including empty | PASS |  |
| drop-table-recreate | query:set: grouped counts order by bitmask not text | PASS |  |
| drop-table-recreate | query:set: a range predicate compares the bitmask | PASS |  |
| drop-table-recreate | query:star: fact with dimension and two audit persons | PASS |  |
| drop-table-recreate | query:star: five-alias chain fans out through events | PASS |  |
| drop-table-recreate | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| drop-table-recreate | query:star: five tables bridge the shop and the star | PASS |  |
| drop-table-recreate | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| drop-table-recreate | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| drop-table-recreate | query:json: length and keys survive null documents | PASS |  |
| drop-table-recreate | query:json: contains_path filters the documented rows | PASS |  |
| drop-table-recreate | query:json: json_value reads a scalar with sql semantics | PASS |  |
| drop-table-recreate | query:json: object construction embeds an extracted scalar | PASS |  |
| drop-table-recreate | query:json: search locates a literal value | PASS |  |
| drop-table-recreate | query:json: grouping by an extracted scalar | PASS |  |
| drop-table-recreate | query:json: merge_patch overlays and reads back | PASS |  |
| drop-table-recreate | query:temporal: quarter, weekday and name grains agree | PASS |  |
| drop-table-recreate | query:temporal: month-end bucketing via last_day | PASS |  |
| drop-table-recreate | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| drop-table-recreate | query:temporal: datetime range keeps the year window | PASS |  |
| drop-table-recreate | query:temporal: date_sub bound in the predicate | PASS |  |
| drop-table-recreate | query:temporal: year-month split grouping | PASS |  |
| drop-table-recreate | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| drop-table-recreate | query:regex: substr extracts the mail domain | PASS |  |
| drop-table-recreate | query:regex: the REGEXP operator anchors a class | PASS |  |
| drop-table-recreate | query:regex: replace folds suffix classes before grouping | PASS |  |
| drop-table-recreate | query:bi metabase: month grain through convert_tz | PASS |  |
| drop-table-recreate | query:bi metabase: iso week bucketing | PASS |  |
| drop-table-recreate | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| drop-table-recreate | query:bi metabase: previous-period revenue window | PASS |  |
| drop-table-recreate | query:bi superset: week-start grain with a rolling average | PASS |  |
| drop-table-recreate | query:bi superset: running total over grouped revenue | PASS |  |
| drop-table-recreate | query:bi superset: lag and lead against a named window | PASS |  |
| drop-table-recreate | query:bi superset: quartile counts from ntile | PASS |  |
| drop-table-recreate | query:bi superset: first and last value over an unbounded frame | PASS |  |
| drop-table-recreate | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| drop-table-recreate | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| drop-table-recreate | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| drop-table-recreate | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| drop-table-recreate | query:bi looker: the grouped primary key determines the row | PASS |  |
| drop-table-recreate | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| drop-table-recreate | query:bi tableau: explicit cast ladder | PASS |  |
| drop-table-recreate | query:bi tableau: the stddev and variance family | PASS |  |
| drop-table-recreate | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| drop-table-recreate | query:bi shared: substring_index dimension cleanup | PASS |  |
| drop-table-recreate | query:bi shared: json validity and typed path filter | PASS |  |
| drop-table-recreate | query:bi shared: contains_path over several paths at once | PASS |  |
| drop-table-recreate | query:bi shared: maketime from extracted parts | PASS |  |
| drop-table-recreate | query:bi shared: extract year_month grouping | PASS |  |
| drop-table-recreate | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| drop-table-recreate | query:staff: three-level management chain with an inactive tail | PASS |  |
| drop-table-recreate | query:staff: active split with id extremes | PASS |  |
| drop-table-recreate | query:counters: full unsigned ladder readback | PASS |  |
| drop-table-recreate | query:counters: greatest and least across widths | PASS |  |
| drop-table-recreate | query:dim: enum status split | PASS |  |
| drop-table-recreate | query:dim: pattern filter across collated columns | PASS |  |
| drop-table-recreate | query:person: anti-join finds owners without facts | PASS |  |
| drop-table-recreate | query:person: created-fact counts through a scalar subquery | PASS |  |
| drop-table-recreate | query:event: lag over per-dimension timelines | PASS |  |
| drop-table-recreate | query:event: daily grain per dimension code | PASS |  |
| drop-table-recreate | query:order_items: product rollup without the orders table | PASS |  |
| drop-table-recreate | query:shipments: carrier value through the items bridge | PASS |  |
| drop-table-recreate | query:json: distinct case variants survive a derived table | PASS |  |
| drop-table-polling | polling:fixtures replicate before the mode switch | PASS |  |
| drop-table-polling | polling:database is healthy before the drop | PASS |  |
| drop-table-polling | polling:TRUNCATE empties the replica | PASS |  |
| drop-table-polling | polling:one dropped table does not stop the other tables | PASS |  |
| drop-table-polling | polling:re-probe restores replication for the surviving tables | PASS |  |
| drop-table-polling | converge:Dim | PASS |  |
| drop-table-polling | converge:Event | PASS |  |
| drop-table-polling | converge:Fact | PASS |  |
| drop-table-polling | converge:Person | PASS |  |
| drop-table-polling | converge:audit_log | PASS |  |
| drop-table-polling | converge:badges | PASS |  |
| drop-table-polling | converge:counters | PASS |  |
| drop-table-polling | converge:customers | PASS |  |
| drop-table-polling | converge:keyless_log | PASS |  |
| drop-table-polling | converge:order_items | PASS |  |
| drop-table-polling | converge:orders | PASS |  |
| drop-table-polling | converge:shipments | PASS |  |
| drop-table-polling | converge:staff | PASS |  |
| drop-table-polling | converge:information_schema.columns | PASS |  |
| drop-table-polling | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| drop-table-polling | query:conformance: mixed-collation double grouping | PASS |  |
| drop-table-polling | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| drop-table-polling | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| drop-table-polling | query:conformance: case-variant code grouping | PASS |  |
| drop-table-polling | query:conformance: anti-join finds the event-less dimension | PASS |  |
| drop-table-polling | query:conformance: nullable join key NULL-extends | PASS |  |
| drop-table-polling | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| drop-table-polling | query:conformance: date bucketing over the fact table | PASS |  |
| drop-table-polling | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| drop-table-polling | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| drop-table-polling | query:enum: the empty member groups by its ordinal | PASS |  |
| drop-table-polling | query:enum: the empty member sorts by its ordinal | PASS |  |
| drop-table-polling | query:enum: the empty member is selectable by text | PASS |  |
| drop-table-polling | query:geometry: hex round-trips the internal format | PASS |  |
| drop-table-polling | query:geometry: byte length includes the srid prefix | PASS |  |
| drop-table-polling | query:geometry: null routes filter and count | PASS |  |
| drop-table-polling | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| drop-table-polling | query:set: find_in_set filters by membership | PASS |  |
| drop-table-polling | query:set: equality is literal, not member-normalized | PASS |  |
| drop-table-polling | query:set: distinct values walk the bitmask including empty | PASS |  |
| drop-table-polling | query:set: grouped counts order by bitmask not text | PASS |  |
| drop-table-polling | query:set: a range predicate compares the bitmask | PASS |  |
| drop-table-polling | query:star: fact with dimension and two audit persons | PASS |  |
| drop-table-polling | query:star: five-alias chain fans out through events | PASS |  |
| drop-table-polling | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| drop-table-polling | query:star: five tables bridge the shop and the star | PASS |  |
| drop-table-polling | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| drop-table-polling | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| drop-table-polling | query:json: length and keys survive null documents | PASS |  |
| drop-table-polling | query:json: contains_path filters the documented rows | PASS |  |
| drop-table-polling | query:json: json_value reads a scalar with sql semantics | PASS |  |
| drop-table-polling | query:json: object construction embeds an extracted scalar | PASS |  |
| drop-table-polling | query:json: search locates a literal value | PASS |  |
| drop-table-polling | query:json: grouping by an extracted scalar | PASS |  |
| drop-table-polling | query:json: merge_patch overlays and reads back | PASS |  |
| drop-table-polling | query:temporal: quarter, weekday and name grains agree | PASS |  |
| drop-table-polling | query:temporal: month-end bucketing via last_day | PASS |  |
| drop-table-polling | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| drop-table-polling | query:temporal: datetime range keeps the year window | PASS |  |
| drop-table-polling | query:temporal: date_sub bound in the predicate | PASS |  |
| drop-table-polling | query:temporal: year-month split grouping | PASS |  |
| drop-table-polling | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| drop-table-polling | query:regex: substr extracts the mail domain | PASS |  |
| drop-table-polling | query:regex: the REGEXP operator anchors a class | PASS |  |
| drop-table-polling | query:regex: replace folds suffix classes before grouping | PASS |  |
| drop-table-polling | query:bi metabase: month grain through convert_tz | PASS |  |
| drop-table-polling | query:bi metabase: iso week bucketing | PASS |  |
| drop-table-polling | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| drop-table-polling | query:bi metabase: previous-period revenue window | PASS |  |
| drop-table-polling | query:bi superset: week-start grain with a rolling average | PASS |  |
| drop-table-polling | query:bi superset: running total over grouped revenue | PASS |  |
| drop-table-polling | query:bi superset: lag and lead against a named window | PASS |  |
| drop-table-polling | query:bi superset: quartile counts from ntile | PASS |  |
| drop-table-polling | query:bi superset: first and last value over an unbounded frame | PASS |  |
| drop-table-polling | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| drop-table-polling | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| drop-table-polling | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| drop-table-polling | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| drop-table-polling | query:bi looker: the grouped primary key determines the row | PASS |  |
| drop-table-polling | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| drop-table-polling | query:bi tableau: explicit cast ladder | PASS |  |
| drop-table-polling | query:bi tableau: the stddev and variance family | PASS |  |
| drop-table-polling | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| drop-table-polling | query:bi shared: substring_index dimension cleanup | PASS |  |
| drop-table-polling | query:bi shared: json validity and typed path filter | PASS |  |
| drop-table-polling | query:bi shared: contains_path over several paths at once | PASS |  |
| drop-table-polling | query:bi shared: maketime from extracted parts | PASS |  |
| drop-table-polling | query:bi shared: extract year_month grouping | PASS |  |
| drop-table-polling | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| drop-table-polling | query:staff: three-level management chain with an inactive tail | PASS |  |
| drop-table-polling | query:staff: active split with id extremes | PASS |  |
| drop-table-polling | query:counters: full unsigned ladder readback | PASS |  |
| drop-table-polling | query:counters: greatest and least across widths | PASS |  |
| drop-table-polling | query:dim: enum status split | PASS |  |
| drop-table-polling | query:dim: pattern filter across collated columns | PASS |  |
| drop-table-polling | query:person: anti-join finds owners without facts | PASS |  |
| drop-table-polling | query:person: created-fact counts through a scalar subquery | PASS |  |
| drop-table-polling | query:event: lag over per-dimension timelines | PASS |  |
| drop-table-polling | query:event: daily grain per dimension code | PASS |  |
| drop-table-polling | query:order_items: product rollup without the orders table | PASS |  |
| drop-table-polling | query:shipments: carrier value through the items bridge | PASS |  |
| drop-table-polling | query:json: distinct case variants survive a derived table | PASS |  |
| restart-during-snapshot | restart-during-snapshot:interrupts a copy in flight | PASS |  |
| restart-during-snapshot | restart-during-snapshot:the database resumes replicating on its own | PASS |  |
| restart-during-snapshot | restart-during-snapshot:every row arrives after the resume | PASS | big: 300000 of 300000 |
| restart-during-snapshot | restart-during-snapshot:no table is left quarantined | PASS |  |
| restart-during-snapshot | converge:Dim | PASS |  |
| restart-during-snapshot | converge:Event | PASS |  |
| restart-during-snapshot | converge:Fact | PASS |  |
| restart-during-snapshot | converge:Person | PASS |  |
| restart-during-snapshot | converge:audit_log | PASS |  |
| restart-during-snapshot | converge:badges | PASS |  |
| restart-during-snapshot | converge:counters | PASS |  |
| restart-during-snapshot | converge:customers | PASS |  |
| restart-during-snapshot | converge:keyless_log | PASS |  |
| restart-during-snapshot | converge:order_items | PASS |  |
| restart-during-snapshot | converge:orders | PASS |  |
| restart-during-snapshot | converge:shipments | PASS |  |
| restart-during-snapshot | converge:staff | PASS |  |
| restart-during-snapshot | converge:information_schema.columns | PASS |  |
| restart-during-snapshot | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| restart-during-snapshot | query:conformance: mixed-collation double grouping | PASS |  |
| restart-during-snapshot | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| restart-during-snapshot | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| restart-during-snapshot | query:conformance: case-variant code grouping | PASS |  |
| restart-during-snapshot | query:conformance: anti-join finds the event-less dimension | PASS |  |
| restart-during-snapshot | query:conformance: nullable join key NULL-extends | PASS |  |
| restart-during-snapshot | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| restart-during-snapshot | query:conformance: date bucketing over the fact table | PASS |  |
| restart-during-snapshot | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| restart-during-snapshot | query:point lookup by key | PASS |  |
| restart-during-snapshot | query:range scan with compound predicate | PASS |  |
| restart-during-snapshot | query:inner join with aggregation | PASS |  |
| restart-during-snapshot | query:join with a residual comparison between both inputs | PASS |  |
| restart-during-snapshot | query:left join keeps rows whose only matches fail the residual | PASS |  |
| restart-during-snapshot | query:residual comparison through coalesce on a nullable column | PASS |  |
| restart-during-snapshot | query:created-by and updated-by resolve through separate aliases | PASS |  |
| restart-during-snapshot | query:alias pair with the join order reversed | PASS |  |
| restart-during-snapshot | query:four aliases of one table joined in a chain | PASS |  |
| restart-during-snapshot | query:self-join with a single-side predicate in the ON clause | PASS |  |
| restart-during-snapshot | query:self-join manager chain preserves the roots | PASS |  |
| restart-during-snapshot | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| restart-during-snapshot | query:aliases stay distinct when the empty side joins first | PASS |  |
| restart-during-snapshot | query:left join preserves unmatched rows | PASS |  |
| restart-during-snapshot | query:right join preserves unmatched rows | PASS |  |
| restart-during-snapshot | query:three-way join through items | PASS |  |
| restart-during-snapshot | query:union all across sources | PASS |  |
| restart-during-snapshot | query:intersect customer identifiers | PASS |  |
| restart-during-snapshot | query:except customer identifiers | PASS |  |
| restart-during-snapshot | query:order by an expression over an aggregate | PASS |  |
| restart-during-snapshot | query:order by a tree over several aggregates | PASS |  |
| restart-during-snapshot | query:order by an aggregate absent from the select list | PASS |  |
| restart-during-snapshot | query:group by with having | PASS |  |
| restart-during-snapshot | query:conditional decimal sum keeps the fraction | PASS |  |
| restart-during-snapshot | query:distinct count and min max | PASS |  |
| restart-during-snapshot | query:uncorrelated in-subquery | PASS |  |
| restart-during-snapshot | query:correlated exists with inner predicate | PASS |  |
| restart-during-snapshot | query:correlated scalar aggregate | PASS |  |
| restart-during-snapshot | query:correlated scalar unique lookup | PASS |  |
| restart-during-snapshot | query:scalar subquery threshold | PASS |  |
| restart-during-snapshot | query:non-recursive cte | PASS |  |
| restart-during-snapshot | query:bounded recursive cte | PASS |  |
| restart-during-snapshot | query:date bucketing | PASS |  |
| restart-during-snapshot | query:string functions and like | PASS |  |
| restart-during-snapshot | query:looker symmetric key helpers | PASS |  |
| restart-during-snapshot | query:json constructor preserves json versus text | PASS |  |
| restart-during-snapshot | query:json aggregate embeds documents | PASS |  |
| restart-during-snapshot | query:regular expression read transforms | PASS |  |
| restart-during-snapshot | query:case expression buckets | PASS |  |
| restart-during-snapshot | query:null handling | PASS |  |
| restart-during-snapshot | query:coalesce and ifnull | PASS |  |
| restart-during-snapshot | query:enum and set filters | PASS |  |
| restart-during-snapshot | query:unsigned boundary readback | PASS |  |
| restart-during-snapshot | query:derived table | PASS |  |
| restart-during-snapshot | query:group_concat single expression | PASS |  |
| restart-during-snapshot | query:window ranking per group | PASS |  |
| restart-during-snapshot | query:window share of total over grouped output | PASS |  |
| restart-during-snapshot | query:window running total | PASS |  |
| restart-during-snapshot | query:decimal column average beyond simple sum | PASS |  |
| restart-during-snapshot | query:computed decimal rounds negative digits half away from zero | PASS |  |
| restart-during-snapshot | query:json extract filter on customer meta | PASS |  |
| restart-during-snapshot | query:fan-out join group concat line products | PASS |  |
| restart-during-snapshot | query:outer join customers without recent orders | PASS |  |
| restart-during-snapshot | query:set op union distinct tiers and statuses | PASS |  |
| restart-during-snapshot | query:temporal convert and date_format grain | PASS |  |
| restart-during-snapshot | query:correlated not exists open orders | PASS |  |
| restart-during-snapshot | query:window lag payment-shaped totals | PASS |  |
| restart-during-snapshot | query:multi-key join items to orders | PASS |  |
| restart-during-snapshot | query:between and null-safe coalesce on balance | PASS |  |
| restart-during-snapshot | query:intersect all-style customer buyers | PASS |  |
| restart-during-snapshot | query:derived table status revenue share | PASS |  |
| restart-during-snapshot | query:general_ci: equality folds ASCII case | PASS |  |
| restart-during-snapshot | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| restart-during-snapshot | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| restart-during-snapshot | query:general_ci: every supplementary character compares equal | PASS |  |
| restart-during-snapshot | query:general_ci: grouping partitions by collated equality | PASS |  |
| restart-during-snapshot | query:general_ci: ordering follows the collation, not code points | PASS |  |
| restart-during-snapshot | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| restart-during-snapshot | query:general_ci: joining on a collated column | PASS |  |
| restart-during-snapshot | query:general_ci: representative spelling of a collated group | PASS |  |
| restart-during-snapshot | query:general_ci: mixing collations across separate comparisons | PASS |  |
| restart-during-snapshot | query:enum: order by ascends by declared ordinal | PASS |  |
| restart-during-snapshot | query:enum: order by descends by declared ordinal | PASS |  |
| restart-during-snapshot | query:enum: min and max compare as strings | PASS |  |
| restart-during-snapshot | query:enum: a greater-than range compares as strings | PASS |  |
| restart-during-snapshot | query:enum: a less-than range compares as strings | PASS |  |
| restart-during-snapshot | query:enum: between compares as strings | PASS |  |
| restart-during-snapshot | query:enum: distinct orders by ordinal | PASS |  |
| restart-during-snapshot | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| restart-during-snapshot | query:enum: a window order walks the ordinal | PASS |  |
| restart-during-snapshot | query:collation: mixed grouping answers with per-key folds | PASS |  |
| restart-during-snapshot | query:collation: distinct counts fold per column collation | PASS |  |
| restart-during-snapshot | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| restart-during-snapshot | query:set: order by walks the member bitmask | PASS |  |
| restart-during-snapshot | query:set: grouping orders groups by bitmask | PASS |  |
| restart-during-snapshot | query:enum: the empty member groups by its ordinal | PASS |  |
| restart-during-snapshot | query:enum: the empty member sorts by its ordinal | PASS |  |
| restart-during-snapshot | query:enum: the empty member is selectable by text | PASS |  |
| restart-during-snapshot | query:geometry: hex round-trips the internal format | PASS |  |
| restart-during-snapshot | query:geometry: byte length includes the srid prefix | PASS |  |
| restart-during-snapshot | query:geometry: null routes filter and count | PASS |  |
| restart-during-snapshot | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| restart-during-snapshot | query:set: find_in_set filters by membership | PASS |  |
| restart-during-snapshot | query:set: equality is literal, not member-normalized | PASS |  |
| restart-during-snapshot | query:set: distinct values walk the bitmask including empty | PASS |  |
| restart-during-snapshot | query:set: grouped counts order by bitmask not text | PASS |  |
| restart-during-snapshot | query:set: a range predicate compares the bitmask | PASS |  |
| restart-during-snapshot | query:star: fact with dimension and two audit persons | PASS |  |
| restart-during-snapshot | query:star: five-alias chain fans out through events | PASS |  |
| restart-during-snapshot | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| restart-during-snapshot | query:star: five tables bridge the shop and the star | PASS |  |
| restart-during-snapshot | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| restart-during-snapshot | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| restart-during-snapshot | query:json: length and keys survive null documents | PASS |  |
| restart-during-snapshot | query:json: contains_path filters the documented rows | PASS |  |
| restart-during-snapshot | query:json: json_value reads a scalar with sql semantics | PASS |  |
| restart-during-snapshot | query:json: object construction embeds an extracted scalar | PASS |  |
| restart-during-snapshot | query:json: search locates a literal value | PASS |  |
| restart-during-snapshot | query:json: grouping by an extracted scalar | PASS |  |
| restart-during-snapshot | query:json: merge_patch overlays and reads back | PASS |  |
| restart-during-snapshot | query:temporal: quarter, weekday and name grains agree | PASS |  |
| restart-during-snapshot | query:temporal: month-end bucketing via last_day | PASS |  |
| restart-during-snapshot | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| restart-during-snapshot | query:temporal: datetime range keeps the year window | PASS |  |
| restart-during-snapshot | query:temporal: date_sub bound in the predicate | PASS |  |
| restart-during-snapshot | query:temporal: year-month split grouping | PASS |  |
| restart-during-snapshot | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| restart-during-snapshot | query:regex: substr extracts the mail domain | PASS |  |
| restart-during-snapshot | query:regex: the REGEXP operator anchors a class | PASS |  |
| restart-during-snapshot | query:regex: replace folds suffix classes before grouping | PASS |  |
| restart-during-snapshot | query:bi metabase: month grain through convert_tz | PASS |  |
| restart-during-snapshot | query:bi metabase: iso week bucketing | PASS |  |
| restart-during-snapshot | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| restart-during-snapshot | query:bi metabase: previous-period revenue window | PASS |  |
| restart-during-snapshot | query:bi superset: week-start grain with a rolling average | PASS |  |
| restart-during-snapshot | query:bi superset: running total over grouped revenue | PASS |  |
| restart-during-snapshot | query:bi superset: lag and lead against a named window | PASS |  |
| restart-during-snapshot | query:bi superset: quartile counts from ntile | PASS |  |
| restart-during-snapshot | query:bi superset: first and last value over an unbounded frame | PASS |  |
| restart-during-snapshot | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| restart-during-snapshot | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| restart-during-snapshot | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| restart-during-snapshot | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| restart-during-snapshot | query:bi looker: the grouped primary key determines the row | PASS |  |
| restart-during-snapshot | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| restart-during-snapshot | query:bi tableau: explicit cast ladder | PASS |  |
| restart-during-snapshot | query:bi tableau: the stddev and variance family | PASS |  |
| restart-during-snapshot | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| restart-during-snapshot | query:bi shared: substring_index dimension cleanup | PASS |  |
| restart-during-snapshot | query:bi shared: json validity and typed path filter | PASS |  |
| restart-during-snapshot | query:bi shared: contains_path over several paths at once | PASS |  |
| restart-during-snapshot | query:bi shared: maketime from extracted parts | PASS |  |
| restart-during-snapshot | query:bi shared: extract year_month grouping | PASS |  |
| restart-during-snapshot | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| restart-during-snapshot | query:staff: three-level management chain with an inactive tail | PASS |  |
| restart-during-snapshot | query:staff: active split with id extremes | PASS |  |
| restart-during-snapshot | query:counters: full unsigned ladder readback | PASS |  |
| restart-during-snapshot | query:counters: greatest and least across widths | PASS |  |
| restart-during-snapshot | query:dim: enum status split | PASS |  |
| restart-during-snapshot | query:dim: pattern filter across collated columns | PASS |  |
| restart-during-snapshot | query:person: anti-join finds owners without facts | PASS |  |
| restart-during-snapshot | query:person: created-fact counts through a scalar subquery | PASS |  |
| restart-during-snapshot | query:event: lag over per-dimension timelines | PASS |  |
| restart-during-snapshot | query:event: daily grain per dimension code | PASS |  |
| restart-during-snapshot | query:order_items: product rollup without the orders table | PASS |  |
| restart-during-snapshot | query:shipments: carrier value through the items bridge | PASS |  |
| restart-during-snapshot | query:json: distinct case variants survive a derived table | PASS |  |
| restart-during-resync | restart-during-resync:interrupts a resync in flight | PASS |  |
| restart-during-resync | restart-during-resync:the interrupted table comes back on its own | PASS |  |
| restart-during-resync | restart-during-resync:the other table never leaves streaming | PASS |  |
| restart-during-resync | restart-during-resync:no whole-database snapshot ran for a one-table repair | PASS | 1 snapshot run(s) recorded; the initial copy is the only one allowed |
| restart-during-resync | restart-during-resync:the stream keeps applying while the table is repaired | PASS |  |
| restart-during-resync | restart-during-resync:every row of the repaired table arrives | PASS | big: 200000 of 200000 |
| restart-during-resync | converge:Dim | PASS |  |
| restart-during-resync | converge:Event | PASS |  |
| restart-during-resync | converge:Fact | PASS |  |
| restart-during-resync | converge:Person | PASS |  |
| restart-during-resync | converge:audit_log | PASS |  |
| restart-during-resync | converge:badges | PASS |  |
| restart-during-resync | converge:counters | PASS |  |
| restart-during-resync | converge:customers | PASS |  |
| restart-during-resync | converge:keyless_log | PASS |  |
| restart-during-resync | converge:order_items | PASS |  |
| restart-during-resync | converge:orders | PASS |  |
| restart-during-resync | converge:shipments | PASS |  |
| restart-during-resync | converge:staff | PASS |  |
| restart-during-resync | converge:information_schema.columns | PASS |  |
| restart-during-resync | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| restart-during-resync | query:conformance: mixed-collation double grouping | PASS |  |
| restart-during-resync | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| restart-during-resync | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| restart-during-resync | query:conformance: case-variant code grouping | PASS |  |
| restart-during-resync | query:conformance: anti-join finds the event-less dimension | PASS |  |
| restart-during-resync | query:conformance: nullable join key NULL-extends | PASS |  |
| restart-during-resync | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| restart-during-resync | query:conformance: date bucketing over the fact table | PASS |  |
| restart-during-resync | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| restart-during-resync | query:point lookup by key | PASS |  |
| restart-during-resync | query:range scan with compound predicate | PASS |  |
| restart-during-resync | query:inner join with aggregation | PASS |  |
| restart-during-resync | query:join with a residual comparison between both inputs | PASS |  |
| restart-during-resync | query:left join keeps rows whose only matches fail the residual | PASS |  |
| restart-during-resync | query:residual comparison through coalesce on a nullable column | PASS |  |
| restart-during-resync | query:created-by and updated-by resolve through separate aliases | PASS |  |
| restart-during-resync | query:alias pair with the join order reversed | PASS |  |
| restart-during-resync | query:four aliases of one table joined in a chain | PASS |  |
| restart-during-resync | query:self-join with a single-side predicate in the ON clause | PASS |  |
| restart-during-resync | query:self-join manager chain preserves the roots | PASS |  |
| restart-during-resync | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| restart-during-resync | query:aliases stay distinct when the empty side joins first | PASS |  |
| restart-during-resync | query:left join preserves unmatched rows | PASS |  |
| restart-during-resync | query:right join preserves unmatched rows | PASS |  |
| restart-during-resync | query:three-way join through items | PASS |  |
| restart-during-resync | query:union all across sources | PASS |  |
| restart-during-resync | query:intersect customer identifiers | PASS |  |
| restart-during-resync | query:except customer identifiers | PASS |  |
| restart-during-resync | query:order by an expression over an aggregate | PASS |  |
| restart-during-resync | query:order by a tree over several aggregates | PASS |  |
| restart-during-resync | query:order by an aggregate absent from the select list | PASS |  |
| restart-during-resync | query:group by with having | PASS |  |
| restart-during-resync | query:conditional decimal sum keeps the fraction | PASS |  |
| restart-during-resync | query:distinct count and min max | PASS |  |
| restart-during-resync | query:uncorrelated in-subquery | PASS |  |
| restart-during-resync | query:correlated exists with inner predicate | PASS |  |
| restart-during-resync | query:correlated scalar aggregate | PASS |  |
| restart-during-resync | query:correlated scalar unique lookup | PASS |  |
| restart-during-resync | query:scalar subquery threshold | PASS |  |
| restart-during-resync | query:non-recursive cte | PASS |  |
| restart-during-resync | query:bounded recursive cte | PASS |  |
| restart-during-resync | query:date bucketing | PASS |  |
| restart-during-resync | query:string functions and like | PASS |  |
| restart-during-resync | query:looker symmetric key helpers | PASS |  |
| restart-during-resync | query:json constructor preserves json versus text | PASS |  |
| restart-during-resync | query:json aggregate embeds documents | PASS |  |
| restart-during-resync | query:regular expression read transforms | PASS |  |
| restart-during-resync | query:case expression buckets | PASS |  |
| restart-during-resync | query:null handling | PASS |  |
| restart-during-resync | query:coalesce and ifnull | PASS |  |
| restart-during-resync | query:enum and set filters | PASS |  |
| restart-during-resync | query:unsigned boundary readback | PASS |  |
| restart-during-resync | query:derived table | PASS |  |
| restart-during-resync | query:group_concat single expression | PASS |  |
| restart-during-resync | query:window ranking per group | PASS |  |
| restart-during-resync | query:window share of total over grouped output | PASS |  |
| restart-during-resync | query:window running total | PASS |  |
| restart-during-resync | query:decimal column average beyond simple sum | PASS |  |
| restart-during-resync | query:computed decimal rounds negative digits half away from zero | PASS |  |
| restart-during-resync | query:json extract filter on customer meta | PASS |  |
| restart-during-resync | query:fan-out join group concat line products | PASS |  |
| restart-during-resync | query:outer join customers without recent orders | PASS |  |
| restart-during-resync | query:set op union distinct tiers and statuses | PASS |  |
| restart-during-resync | query:temporal convert and date_format grain | PASS |  |
| restart-during-resync | query:correlated not exists open orders | PASS |  |
| restart-during-resync | query:window lag payment-shaped totals | PASS |  |
| restart-during-resync | query:multi-key join items to orders | PASS |  |
| restart-during-resync | query:between and null-safe coalesce on balance | PASS |  |
| restart-during-resync | query:intersect all-style customer buyers | PASS |  |
| restart-during-resync | query:derived table status revenue share | PASS |  |
| restart-during-resync | query:general_ci: equality folds ASCII case | PASS |  |
| restart-during-resync | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| restart-during-resync | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| restart-during-resync | query:general_ci: every supplementary character compares equal | PASS |  |
| restart-during-resync | query:general_ci: grouping partitions by collated equality | PASS |  |
| restart-during-resync | query:general_ci: ordering follows the collation, not code points | PASS |  |
| restart-during-resync | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| restart-during-resync | query:general_ci: joining on a collated column | PASS |  |
| restart-during-resync | query:general_ci: representative spelling of a collated group | PASS |  |
| restart-during-resync | query:general_ci: mixing collations across separate comparisons | PASS |  |
| restart-during-resync | query:enum: order by ascends by declared ordinal | PASS |  |
| restart-during-resync | query:enum: order by descends by declared ordinal | PASS |  |
| restart-during-resync | query:enum: min and max compare as strings | PASS |  |
| restart-during-resync | query:enum: a greater-than range compares as strings | PASS |  |
| restart-during-resync | query:enum: a less-than range compares as strings | PASS |  |
| restart-during-resync | query:enum: between compares as strings | PASS |  |
| restart-during-resync | query:enum: distinct orders by ordinal | PASS |  |
| restart-during-resync | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| restart-during-resync | query:enum: a window order walks the ordinal | PASS |  |
| restart-during-resync | query:collation: mixed grouping answers with per-key folds | PASS |  |
| restart-during-resync | query:collation: distinct counts fold per column collation | PASS |  |
| restart-during-resync | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| restart-during-resync | query:set: order by walks the member bitmask | PASS |  |
| restart-during-resync | query:set: grouping orders groups by bitmask | PASS |  |
| restart-during-resync | query:enum: the empty member groups by its ordinal | PASS |  |
| restart-during-resync | query:enum: the empty member sorts by its ordinal | PASS |  |
| restart-during-resync | query:enum: the empty member is selectable by text | PASS |  |
| restart-during-resync | query:geometry: hex round-trips the internal format | PASS |  |
| restart-during-resync | query:geometry: byte length includes the srid prefix | PASS |  |
| restart-during-resync | query:geometry: null routes filter and count | PASS |  |
| restart-during-resync | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| restart-during-resync | query:set: find_in_set filters by membership | PASS |  |
| restart-during-resync | query:set: equality is literal, not member-normalized | PASS |  |
| restart-during-resync | query:set: distinct values walk the bitmask including empty | PASS |  |
| restart-during-resync | query:set: grouped counts order by bitmask not text | PASS |  |
| restart-during-resync | query:set: a range predicate compares the bitmask | PASS |  |
| restart-during-resync | query:star: fact with dimension and two audit persons | PASS |  |
| restart-during-resync | query:star: five-alias chain fans out through events | PASS |  |
| restart-during-resync | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| restart-during-resync | query:star: five tables bridge the shop and the star | PASS |  |
| restart-during-resync | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| restart-during-resync | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| restart-during-resync | query:json: length and keys survive null documents | PASS |  |
| restart-during-resync | query:json: contains_path filters the documented rows | PASS |  |
| restart-during-resync | query:json: json_value reads a scalar with sql semantics | PASS |  |
| restart-during-resync | query:json: object construction embeds an extracted scalar | PASS |  |
| restart-during-resync | query:json: search locates a literal value | PASS |  |
| restart-during-resync | query:json: grouping by an extracted scalar | PASS |  |
| restart-during-resync | query:json: merge_patch overlays and reads back | PASS |  |
| restart-during-resync | query:temporal: quarter, weekday and name grains agree | PASS |  |
| restart-during-resync | query:temporal: month-end bucketing via last_day | PASS |  |
| restart-during-resync | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| restart-during-resync | query:temporal: datetime range keeps the year window | PASS |  |
| restart-during-resync | query:temporal: date_sub bound in the predicate | PASS |  |
| restart-during-resync | query:temporal: year-month split grouping | PASS |  |
| restart-during-resync | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| restart-during-resync | query:regex: substr extracts the mail domain | PASS |  |
| restart-during-resync | query:regex: the REGEXP operator anchors a class | PASS |  |
| restart-during-resync | query:regex: replace folds suffix classes before grouping | PASS |  |
| restart-during-resync | query:bi metabase: month grain through convert_tz | PASS |  |
| restart-during-resync | query:bi metabase: iso week bucketing | PASS |  |
| restart-during-resync | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| restart-during-resync | query:bi metabase: previous-period revenue window | PASS |  |
| restart-during-resync | query:bi superset: week-start grain with a rolling average | PASS |  |
| restart-during-resync | query:bi superset: running total over grouped revenue | PASS |  |
| restart-during-resync | query:bi superset: lag and lead against a named window | PASS |  |
| restart-during-resync | query:bi superset: quartile counts from ntile | PASS |  |
| restart-during-resync | query:bi superset: first and last value over an unbounded frame | PASS |  |
| restart-during-resync | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| restart-during-resync | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| restart-during-resync | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| restart-during-resync | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| restart-during-resync | query:bi looker: the grouped primary key determines the row | PASS |  |
| restart-during-resync | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| restart-during-resync | query:bi tableau: explicit cast ladder | PASS |  |
| restart-during-resync | query:bi tableau: the stddev and variance family | PASS |  |
| restart-during-resync | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| restart-during-resync | query:bi shared: substring_index dimension cleanup | PASS |  |
| restart-during-resync | query:bi shared: json validity and typed path filter | PASS |  |
| restart-during-resync | query:bi shared: contains_path over several paths at once | PASS |  |
| restart-during-resync | query:bi shared: maketime from extracted parts | PASS |  |
| restart-during-resync | query:bi shared: extract year_month grouping | PASS |  |
| restart-during-resync | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| restart-during-resync | query:staff: three-level management chain with an inactive tail | PASS |  |
| restart-during-resync | query:staff: active split with id extremes | PASS |  |
| restart-during-resync | query:counters: full unsigned ladder readback | PASS |  |
| restart-during-resync | query:counters: greatest and least across widths | PASS |  |
| restart-during-resync | query:dim: enum status split | PASS |  |
| restart-during-resync | query:dim: pattern filter across collated columns | PASS |  |
| restart-during-resync | query:person: anti-join finds owners without facts | PASS |  |
| restart-during-resync | query:person: created-fact counts through a scalar subquery | PASS |  |
| restart-during-resync | query:event: lag over per-dimension timelines | PASS |  |
| restart-during-resync | query:event: daily grain per dimension code | PASS |  |
| restart-during-resync | query:order_items: product rollup without the orders table | PASS |  |
| restart-during-resync | query:shipments: carrier value through the items bridge | PASS |  |
| restart-during-resync | query:json: distinct case variants survive a derived table | PASS |  |
| memory-pressure | memory-pressure:a CDC table with a secondary UNIQUE key streams under the ceiling | PASS | pintail 40, source 40 |
| memory-pressure | memory-pressure:the process survives the storm | PASS | wire 240 ok, http 62 ok, dashboards 157 ok; no errors |
| memory-pressure | memory-pressure:every failure is a designed refusal | PASS | only refusals; 0 dashboard requests failed |
| memory-pressure | memory-pressure:work still gets done | PASS | wire 240 of 240, http 62 |
| memory-pressure | memory-pressure:wire queries are not starved by the HTTP surface | PASS | wire p50 449ms p99 832ms over 240 queries |
| memory-pressure | memory-pressure:health never stalls | PASS | health p99 9ms over 11 samples |
| memory-pressure | memory-pressure:the process stays inside its ceiling | PASS | peak RSS 340MB with a 256MB budget |
| memory-pressure | memory-pressure:the replica catches up after the storm | PASS | big 201000 vs source 201000 |
| memory-pressure | memory-pressure:queries recover once the storm passes | PASS | 3 of 3 sequential queries succeeded |
| memory-pressure | converge:Dim | PASS |  |
| memory-pressure | converge:Event | PASS |  |
| memory-pressure | converge:Fact | PASS |  |
| memory-pressure | converge:Person | PASS |  |
| memory-pressure | converge:audit_log | PASS |  |
| memory-pressure | converge:badges | PASS |  |
| memory-pressure | converge:counters | PASS |  |
| memory-pressure | converge:customers | PASS |  |
| memory-pressure | converge:keyless_log | PASS |  |
| memory-pressure | converge:order_items | PASS |  |
| memory-pressure | converge:orders | PASS |  |
| memory-pressure | converge:shipments | PASS |  |
| memory-pressure | converge:staff | PASS |  |
| memory-pressure | converge:information_schema.columns | PASS |  |
| memory-pressure | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| memory-pressure | query:conformance: mixed-collation double grouping | PASS |  |
| memory-pressure | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| memory-pressure | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| memory-pressure | query:conformance: case-variant code grouping | PASS |  |
| memory-pressure | query:conformance: anti-join finds the event-less dimension | PASS |  |
| memory-pressure | query:conformance: nullable join key NULL-extends | PASS |  |
| memory-pressure | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| memory-pressure | query:conformance: date bucketing over the fact table | PASS |  |
| memory-pressure | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| memory-pressure | query:point lookup by key | PASS |  |
| memory-pressure | query:range scan with compound predicate | PASS |  |
| memory-pressure | query:inner join with aggregation | PASS |  |
| memory-pressure | query:join with a residual comparison between both inputs | PASS |  |
| memory-pressure | query:left join keeps rows whose only matches fail the residual | PASS |  |
| memory-pressure | query:residual comparison through coalesce on a nullable column | PASS |  |
| memory-pressure | query:created-by and updated-by resolve through separate aliases | PASS |  |
| memory-pressure | query:alias pair with the join order reversed | PASS |  |
| memory-pressure | query:four aliases of one table joined in a chain | PASS |  |
| memory-pressure | query:self-join with a single-side predicate in the ON clause | PASS |  |
| memory-pressure | query:self-join manager chain preserves the roots | PASS |  |
| memory-pressure | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| memory-pressure | query:aliases stay distinct when the empty side joins first | PASS |  |
| memory-pressure | query:left join preserves unmatched rows | PASS |  |
| memory-pressure | query:right join preserves unmatched rows | PASS |  |
| memory-pressure | query:three-way join through items | PASS |  |
| memory-pressure | query:union all across sources | PASS |  |
| memory-pressure | query:intersect customer identifiers | PASS |  |
| memory-pressure | query:except customer identifiers | PASS |  |
| memory-pressure | query:order by an expression over an aggregate | PASS |  |
| memory-pressure | query:order by a tree over several aggregates | PASS |  |
| memory-pressure | query:order by an aggregate absent from the select list | PASS |  |
| memory-pressure | query:group by with having | PASS |  |
| memory-pressure | query:conditional decimal sum keeps the fraction | PASS |  |
| memory-pressure | query:distinct count and min max | PASS |  |
| memory-pressure | query:uncorrelated in-subquery | PASS |  |
| memory-pressure | query:correlated exists with inner predicate | PASS |  |
| memory-pressure | query:correlated scalar aggregate | PASS |  |
| memory-pressure | query:correlated scalar unique lookup | PASS |  |
| memory-pressure | query:scalar subquery threshold | PASS |  |
| memory-pressure | query:non-recursive cte | PASS |  |
| memory-pressure | query:bounded recursive cte | PASS |  |
| memory-pressure | query:date bucketing | PASS |  |
| memory-pressure | query:string functions and like | PASS |  |
| memory-pressure | query:looker symmetric key helpers | PASS |  |
| memory-pressure | query:json constructor preserves json versus text | PASS |  |
| memory-pressure | query:json aggregate embeds documents | PASS |  |
| memory-pressure | query:regular expression read transforms | PASS |  |
| memory-pressure | query:case expression buckets | PASS |  |
| memory-pressure | query:null handling | PASS |  |
| memory-pressure | query:coalesce and ifnull | PASS |  |
| memory-pressure | query:enum and set filters | PASS |  |
| memory-pressure | query:unsigned boundary readback | PASS |  |
| memory-pressure | query:derived table | PASS |  |
| memory-pressure | query:group_concat single expression | PASS |  |
| memory-pressure | query:window ranking per group | PASS |  |
| memory-pressure | query:window share of total over grouped output | PASS |  |
| memory-pressure | query:window running total | PASS |  |
| memory-pressure | query:decimal column average beyond simple sum | PASS |  |
| memory-pressure | query:computed decimal rounds negative digits half away from zero | PASS |  |
| memory-pressure | query:json extract filter on customer meta | PASS |  |
| memory-pressure | query:fan-out join group concat line products | PASS |  |
| memory-pressure | query:outer join customers without recent orders | PASS |  |
| memory-pressure | query:set op union distinct tiers and statuses | PASS |  |
| memory-pressure | query:temporal convert and date_format grain | PASS |  |
| memory-pressure | query:correlated not exists open orders | PASS |  |
| memory-pressure | query:window lag payment-shaped totals | PASS |  |
| memory-pressure | query:multi-key join items to orders | PASS |  |
| memory-pressure | query:between and null-safe coalesce on balance | PASS |  |
| memory-pressure | query:intersect all-style customer buyers | PASS |  |
| memory-pressure | query:derived table status revenue share | PASS |  |
| memory-pressure | query:general_ci: equality folds ASCII case | PASS |  |
| memory-pressure | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| memory-pressure | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| memory-pressure | query:general_ci: every supplementary character compares equal | PASS |  |
| memory-pressure | query:general_ci: grouping partitions by collated equality | PASS |  |
| memory-pressure | query:general_ci: ordering follows the collation, not code points | PASS |  |
| memory-pressure | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| memory-pressure | query:general_ci: joining on a collated column | PASS |  |
| memory-pressure | query:general_ci: representative spelling of a collated group | PASS |  |
| memory-pressure | query:general_ci: mixing collations across separate comparisons | PASS |  |
| memory-pressure | query:enum: order by ascends by declared ordinal | PASS |  |
| memory-pressure | query:enum: order by descends by declared ordinal | PASS |  |
| memory-pressure | query:enum: min and max compare as strings | PASS |  |
| memory-pressure | query:enum: a greater-than range compares as strings | PASS |  |
| memory-pressure | query:enum: a less-than range compares as strings | PASS |  |
| memory-pressure | query:enum: between compares as strings | PASS |  |
| memory-pressure | query:enum: distinct orders by ordinal | PASS |  |
| memory-pressure | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| memory-pressure | query:enum: a window order walks the ordinal | PASS |  |
| memory-pressure | query:collation: mixed grouping answers with per-key folds | PASS |  |
| memory-pressure | query:collation: distinct counts fold per column collation | PASS |  |
| memory-pressure | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| memory-pressure | query:set: order by walks the member bitmask | PASS |  |
| memory-pressure | query:set: grouping orders groups by bitmask | PASS |  |
| memory-pressure | query:enum: the empty member groups by its ordinal | PASS |  |
| memory-pressure | query:enum: the empty member sorts by its ordinal | PASS |  |
| memory-pressure | query:enum: the empty member is selectable by text | PASS |  |
| memory-pressure | query:geometry: hex round-trips the internal format | PASS |  |
| memory-pressure | query:geometry: byte length includes the srid prefix | PASS |  |
| memory-pressure | query:geometry: null routes filter and count | PASS |  |
| memory-pressure | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| memory-pressure | query:set: find_in_set filters by membership | PASS |  |
| memory-pressure | query:set: equality is literal, not member-normalized | PASS |  |
| memory-pressure | query:set: distinct values walk the bitmask including empty | PASS |  |
| memory-pressure | query:set: grouped counts order by bitmask not text | PASS |  |
| memory-pressure | query:set: a range predicate compares the bitmask | PASS |  |
| memory-pressure | query:star: fact with dimension and two audit persons | PASS |  |
| memory-pressure | query:star: five-alias chain fans out through events | PASS |  |
| memory-pressure | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| memory-pressure | query:star: five tables bridge the shop and the star | PASS |  |
| memory-pressure | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| memory-pressure | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| memory-pressure | query:json: length and keys survive null documents | PASS |  |
| memory-pressure | query:json: contains_path filters the documented rows | PASS |  |
| memory-pressure | query:json: json_value reads a scalar with sql semantics | PASS |  |
| memory-pressure | query:json: object construction embeds an extracted scalar | PASS |  |
| memory-pressure | query:json: search locates a literal value | PASS |  |
| memory-pressure | query:json: grouping by an extracted scalar | PASS |  |
| memory-pressure | query:json: merge_patch overlays and reads back | PASS |  |
| memory-pressure | query:temporal: quarter, weekday and name grains agree | PASS |  |
| memory-pressure | query:temporal: month-end bucketing via last_day | PASS |  |
| memory-pressure | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| memory-pressure | query:temporal: datetime range keeps the year window | PASS |  |
| memory-pressure | query:temporal: date_sub bound in the predicate | PASS |  |
| memory-pressure | query:temporal: year-month split grouping | PASS |  |
| memory-pressure | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| memory-pressure | query:regex: substr extracts the mail domain | PASS |  |
| memory-pressure | query:regex: the REGEXP operator anchors a class | PASS |  |
| memory-pressure | query:regex: replace folds suffix classes before grouping | PASS |  |
| memory-pressure | query:bi metabase: month grain through convert_tz | PASS |  |
| memory-pressure | query:bi metabase: iso week bucketing | PASS |  |
| memory-pressure | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| memory-pressure | query:bi metabase: previous-period revenue window | PASS |  |
| memory-pressure | query:bi superset: week-start grain with a rolling average | PASS |  |
| memory-pressure | query:bi superset: running total over grouped revenue | PASS |  |
| memory-pressure | query:bi superset: lag and lead against a named window | PASS |  |
| memory-pressure | query:bi superset: quartile counts from ntile | PASS |  |
| memory-pressure | query:bi superset: first and last value over an unbounded frame | PASS |  |
| memory-pressure | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| memory-pressure | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| memory-pressure | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| memory-pressure | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| memory-pressure | query:bi looker: the grouped primary key determines the row | PASS |  |
| memory-pressure | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| memory-pressure | query:bi tableau: explicit cast ladder | PASS |  |
| memory-pressure | query:bi tableau: the stddev and variance family | PASS |  |
| memory-pressure | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| memory-pressure | query:bi shared: substring_index dimension cleanup | PASS |  |
| memory-pressure | query:bi shared: json validity and typed path filter | PASS |  |
| memory-pressure | query:bi shared: contains_path over several paths at once | PASS |  |
| memory-pressure | query:bi shared: maketime from extracted parts | PASS |  |
| memory-pressure | query:bi shared: extract year_month grouping | PASS |  |
| memory-pressure | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| memory-pressure | query:staff: three-level management chain with an inactive tail | PASS |  |
| memory-pressure | query:staff: active split with id extremes | PASS |  |
| memory-pressure | query:counters: full unsigned ladder readback | PASS |  |
| memory-pressure | query:counters: greatest and least across widths | PASS |  |
| memory-pressure | query:dim: enum status split | PASS |  |
| memory-pressure | query:dim: pattern filter across collated columns | PASS |  |
| memory-pressure | query:person: anti-join finds owners without facts | PASS |  |
| memory-pressure | query:person: created-fact counts through a scalar subquery | PASS |  |
| memory-pressure | query:event: lag over per-dimension timelines | PASS |  |
| memory-pressure | query:event: daily grain per dimension code | PASS |  |
| memory-pressure | query:order_items: product rollup without the orders table | PASS |  |
| memory-pressure | query:shipments: carrier value through the items bridge | PASS |  |
| memory-pressure | query:json: distinct case variants survive a derived table | PASS |  |
| reconcile-memory | reconcile-memory:the source holds the large child table | PASS | 2000000 rows |
| reconcile-memory | reconcile-memory:every child row arrives | PASS | 2000000 of 2000000 |
| reconcile-memory | reconcile-memory:the cascade removed the deleted parents' children | PASS | 1800000 remain |
| reconcile-memory | reconcile-memory:reconciliation converges the replica on the source | PASS | child 1800000 vs source 1800000 after 11.4s |
| reconcile-memory | reconcile-memory:reconciliation is bounded in memory | PASS | RSS 30MB before, peak 133MB during (margin 768MB) |
| reconcile-memory | converge:Dim | PASS |  |
| reconcile-memory | converge:Event | PASS |  |
| reconcile-memory | converge:Fact | PASS |  |
| reconcile-memory | converge:Person | PASS |  |
| reconcile-memory | converge:audit_log | PASS |  |
| reconcile-memory | converge:badges | PASS |  |
| reconcile-memory | converge:counters | PASS |  |
| reconcile-memory | converge:customers | PASS |  |
| reconcile-memory | converge:keyless_log | PASS |  |
| reconcile-memory | converge:order_items | PASS |  |
| reconcile-memory | converge:orders | PASS |  |
| reconcile-memory | converge:shipments | PASS |  |
| reconcile-memory | converge:staff | PASS |  |
| reconcile-memory | converge:information_schema.columns | PASS |  |
| reconcile-memory | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| reconcile-memory | query:conformance: mixed-collation double grouping | PASS |  |
| reconcile-memory | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| reconcile-memory | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| reconcile-memory | query:conformance: case-variant code grouping | PASS |  |
| reconcile-memory | query:conformance: anti-join finds the event-less dimension | PASS |  |
| reconcile-memory | query:conformance: nullable join key NULL-extends | PASS |  |
| reconcile-memory | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| reconcile-memory | query:conformance: date bucketing over the fact table | PASS |  |
| reconcile-memory | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
| reconcile-memory | query:point lookup by key | PASS |  |
| reconcile-memory | query:range scan with compound predicate | PASS |  |
| reconcile-memory | query:inner join with aggregation | PASS |  |
| reconcile-memory | query:join with a residual comparison between both inputs | PASS |  |
| reconcile-memory | query:left join keeps rows whose only matches fail the residual | PASS |  |
| reconcile-memory | query:residual comparison through coalesce on a nullable column | PASS |  |
| reconcile-memory | query:created-by and updated-by resolve through separate aliases | PASS |  |
| reconcile-memory | query:alias pair with the join order reversed | PASS |  |
| reconcile-memory | query:four aliases of one table joined in a chain | PASS |  |
| reconcile-memory | query:self-join with a single-side predicate in the ON clause | PASS |  |
| reconcile-memory | query:self-join manager chain preserves the roots | PASS |  |
| reconcile-memory | query:a table joined twice under two aliases keeps them distinct | PASS |  |
| reconcile-memory | query:aliases stay distinct when the empty side joins first | PASS |  |
| reconcile-memory | query:left join preserves unmatched rows | PASS |  |
| reconcile-memory | query:right join preserves unmatched rows | PASS |  |
| reconcile-memory | query:three-way join through items | PASS |  |
| reconcile-memory | query:union all across sources | PASS |  |
| reconcile-memory | query:intersect customer identifiers | PASS |  |
| reconcile-memory | query:except customer identifiers | PASS |  |
| reconcile-memory | query:order by an expression over an aggregate | PASS |  |
| reconcile-memory | query:order by a tree over several aggregates | PASS |  |
| reconcile-memory | query:order by an aggregate absent from the select list | PASS |  |
| reconcile-memory | query:group by with having | PASS |  |
| reconcile-memory | query:conditional decimal sum keeps the fraction | PASS |  |
| reconcile-memory | query:distinct count and min max | PASS |  |
| reconcile-memory | query:uncorrelated in-subquery | PASS |  |
| reconcile-memory | query:correlated exists with inner predicate | PASS |  |
| reconcile-memory | query:correlated scalar aggregate | PASS |  |
| reconcile-memory | query:correlated scalar unique lookup | PASS |  |
| reconcile-memory | query:scalar subquery threshold | PASS |  |
| reconcile-memory | query:non-recursive cte | PASS |  |
| reconcile-memory | query:bounded recursive cte | PASS |  |
| reconcile-memory | query:date bucketing | PASS |  |
| reconcile-memory | query:string functions and like | PASS |  |
| reconcile-memory | query:looker symmetric key helpers | PASS |  |
| reconcile-memory | query:json constructor preserves json versus text | PASS |  |
| reconcile-memory | query:json aggregate embeds documents | PASS |  |
| reconcile-memory | query:regular expression read transforms | PASS |  |
| reconcile-memory | query:case expression buckets | PASS |  |
| reconcile-memory | query:null handling | PASS |  |
| reconcile-memory | query:coalesce and ifnull | PASS |  |
| reconcile-memory | query:enum and set filters | PASS |  |
| reconcile-memory | query:unsigned boundary readback | PASS |  |
| reconcile-memory | query:derived table | PASS |  |
| reconcile-memory | query:group_concat single expression | PASS |  |
| reconcile-memory | query:window ranking per group | PASS |  |
| reconcile-memory | query:window share of total over grouped output | PASS |  |
| reconcile-memory | query:window running total | PASS |  |
| reconcile-memory | query:decimal column average beyond simple sum | PASS |  |
| reconcile-memory | query:computed decimal rounds negative digits half away from zero | PASS |  |
| reconcile-memory | query:json extract filter on customer meta | PASS |  |
| reconcile-memory | query:fan-out join group concat line products | PASS |  |
| reconcile-memory | query:outer join customers without recent orders | PASS |  |
| reconcile-memory | query:set op union distinct tiers and statuses | PASS |  |
| reconcile-memory | query:temporal convert and date_format grain | PASS |  |
| reconcile-memory | query:correlated not exists open orders | PASS |  |
| reconcile-memory | query:window lag payment-shaped totals | PASS |  |
| reconcile-memory | query:multi-key join items to orders | PASS |  |
| reconcile-memory | query:between and null-safe coalesce on balance | PASS |  |
| reconcile-memory | query:intersect all-style customer buyers | PASS |  |
| reconcile-memory | query:derived table status revenue share | PASS |  |
| reconcile-memory | query:general_ci: equality folds ASCII case | PASS |  |
| reconcile-memory | query:general_ci: equality folds Latin-1 accents onto the base letter | PASS |  |
| reconcile-memory | query:general_ci: trailing spaces are insignificant (PAD SPACE) | PASS |  |
| reconcile-memory | query:general_ci: every supplementary character compares equal | PASS |  |
| reconcile-memory | query:general_ci: grouping partitions by collated equality | PASS |  |
| reconcile-memory | query:general_ci: ordering follows the collation, not code points | PASS |  |
| reconcile-memory | query:general_ci: DISTINCT collapses collation-equal values | PASS |  |
| reconcile-memory | query:general_ci: joining on a collated column | PASS |  |
| reconcile-memory | query:general_ci: representative spelling of a collated group | PASS |  |
| reconcile-memory | query:general_ci: mixing collations across separate comparisons | PASS |  |
| reconcile-memory | query:enum: order by ascends by declared ordinal | PASS |  |
| reconcile-memory | query:enum: order by descends by declared ordinal | PASS |  |
| reconcile-memory | query:enum: min and max compare as strings | PASS |  |
| reconcile-memory | query:enum: a greater-than range compares as strings | PASS |  |
| reconcile-memory | query:enum: a less-than range compares as strings | PASS |  |
| reconcile-memory | query:enum: between compares as strings | PASS |  |
| reconcile-memory | query:enum: distinct orders by ordinal | PASS |  |
| reconcile-memory | query:enum: a limited sort keeps the lowest ordinals | PASS |  |
| reconcile-memory | query:enum: a window order walks the ordinal | PASS |  |
| reconcile-memory | query:collation: mixed grouping answers with per-key folds | PASS |  |
| reconcile-memory | query:collation: distinct counts fold per column collation | PASS |  |
| reconcile-memory | query:collation: regrouping a mixed grouping stays exact | PASS |  |
| reconcile-memory | query:set: order by walks the member bitmask | PASS |  |
| reconcile-memory | query:set: grouping orders groups by bitmask | PASS |  |
| reconcile-memory | query:enum: the empty member groups by its ordinal | PASS |  |
| reconcile-memory | query:enum: the empty member sorts by its ordinal | PASS |  |
| reconcile-memory | query:enum: the empty member is selectable by text | PASS |  |
| reconcile-memory | query:geometry: hex round-trips the internal format | PASS |  |
| reconcile-memory | query:geometry: byte length includes the srid prefix | PASS |  |
| reconcile-memory | query:geometry: null routes filter and count | PASS |  |
| reconcile-memory | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| reconcile-memory | query:set: find_in_set filters by membership | PASS |  |
| reconcile-memory | query:set: equality is literal, not member-normalized | PASS |  |
| reconcile-memory | query:set: distinct values walk the bitmask including empty | PASS |  |
| reconcile-memory | query:set: grouped counts order by bitmask not text | PASS |  |
| reconcile-memory | query:set: a range predicate compares the bitmask | PASS |  |
| reconcile-memory | query:star: fact with dimension and two audit persons | PASS |  |
| reconcile-memory | query:star: five-alias chain fans out through events | PASS |  |
| reconcile-memory | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| reconcile-memory | query:star: five tables bridge the shop and the star | PASS |  |
| reconcile-memory | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| reconcile-memory | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| reconcile-memory | query:json: length and keys survive null documents | PASS |  |
| reconcile-memory | query:json: contains_path filters the documented rows | PASS |  |
| reconcile-memory | query:json: json_value reads a scalar with sql semantics | PASS |  |
| reconcile-memory | query:json: object construction embeds an extracted scalar | PASS |  |
| reconcile-memory | query:json: search locates a literal value | PASS |  |
| reconcile-memory | query:json: grouping by an extracted scalar | PASS |  |
| reconcile-memory | query:json: merge_patch overlays and reads back | PASS |  |
| reconcile-memory | query:temporal: quarter, weekday and name grains agree | PASS |  |
| reconcile-memory | query:temporal: month-end bucketing via last_day | PASS |  |
| reconcile-memory | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| reconcile-memory | query:temporal: datetime range keeps the year window | PASS |  |
| reconcile-memory | query:temporal: date_sub bound in the predicate | PASS |  |
| reconcile-memory | query:temporal: year-month split grouping | PASS |  |
| reconcile-memory | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| reconcile-memory | query:regex: substr extracts the mail domain | PASS |  |
| reconcile-memory | query:regex: the REGEXP operator anchors a class | PASS |  |
| reconcile-memory | query:regex: replace folds suffix classes before grouping | PASS |  |
| reconcile-memory | query:bi metabase: month grain through convert_tz | PASS |  |
| reconcile-memory | query:bi metabase: iso week bucketing | PASS |  |
| reconcile-memory | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| reconcile-memory | query:bi metabase: previous-period revenue window | PASS |  |
| reconcile-memory | query:bi superset: week-start grain with a rolling average | PASS |  |
| reconcile-memory | query:bi superset: running total over grouped revenue | PASS |  |
| reconcile-memory | query:bi superset: lag and lead against a named window | PASS |  |
| reconcile-memory | query:bi superset: quartile counts from ntile | PASS |  |
| reconcile-memory | query:bi superset: first and last value over an unbounded frame | PASS |  |
| reconcile-memory | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| reconcile-memory | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| reconcile-memory | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| reconcile-memory | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| reconcile-memory | query:bi looker: the grouped primary key determines the row | PASS |  |
| reconcile-memory | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| reconcile-memory | query:bi tableau: explicit cast ladder | PASS |  |
| reconcile-memory | query:bi tableau: the stddev and variance family | PASS |  |
| reconcile-memory | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| reconcile-memory | query:bi shared: substring_index dimension cleanup | PASS |  |
| reconcile-memory | query:bi shared: json validity and typed path filter | PASS |  |
| reconcile-memory | query:bi shared: contains_path over several paths at once | PASS |  |
| reconcile-memory | query:bi shared: maketime from extracted parts | PASS |  |
| reconcile-memory | query:bi shared: extract year_month grouping | PASS |  |
| reconcile-memory | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| reconcile-memory | query:staff: three-level management chain with an inactive tail | PASS |  |
| reconcile-memory | query:staff: active split with id extremes | PASS |  |
| reconcile-memory | query:counters: full unsigned ladder readback | PASS |  |
| reconcile-memory | query:counters: greatest and least across widths | PASS |  |
| reconcile-memory | query:dim: enum status split | PASS |  |
| reconcile-memory | query:dim: pattern filter across collated columns | PASS |  |
| reconcile-memory | query:person: anti-join finds owners without facts | PASS |  |
| reconcile-memory | query:person: created-fact counts through a scalar subquery | PASS |  |
| reconcile-memory | query:event: lag over per-dimension timelines | PASS |  |
| reconcile-memory | query:event: daily grain per dimension code | PASS |  |
| reconcile-memory | query:order_items: product rollup without the orders table | PASS |  |
| reconcile-memory | query:shipments: carrier value through the items bridge | PASS |  |
| reconcile-memory | query:json: distinct case variants survive a derived table | PASS |  |
| drop-database | cross-schema:same-named table replicates first | PASS |  |
| drop-database | cross-schema:dropping another schema's table leaves this one replicating | PASS |  |
| drop-database | drop-database:second database snapshots | PASS |  |
| drop-database | drop-database:second database serves its rows | PASS |  |
| drop-database | drop-database:the deleted source is surfaced, not served silently | PASS |  |
| drop-database | drop-database:re-probing a deleted source fails loudly | PASS |  |
| drop-database | drop-database:polling reports the deleted source as an error | PASS |  |
| drop-database | drop-database:reads against a deleted source do not claim to be current | WARN | 3 rows are still served from the replica of a database MySQL no longer has, with nothing on the read path marking them stale |
| drop-database | converge:Dim | PASS |  |
| drop-database | converge:Event | PASS |  |
| drop-database | converge:Fact | PASS |  |
| drop-database | converge:Person | PASS |  |
| drop-database | converge:audit_log | PASS |  |
| drop-database | converge:badges | PASS |  |
| drop-database | converge:counters | PASS |  |
| drop-database | converge:customers | PASS |  |
| drop-database | converge:keyless_log | PASS |  |
| drop-database | converge:order_items | PASS |  |
| drop-database | converge:orders | PASS |  |
| drop-database | converge:shipments | PASS |  |
| drop-database | converge:staff | PASS |  |
| drop-database | converge:information_schema.columns | PASS |  |
| drop-database | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| drop-database | query:conformance: mixed-collation double grouping | PASS |  |
| drop-database | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| drop-database | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| drop-database | query:conformance: case-variant code grouping | PASS |  |
| drop-database | query:conformance: anti-join finds the event-less dimension | PASS |  |
| drop-database | query:conformance: nullable join key NULL-extends | PASS |  |
| drop-database | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| drop-database | query:conformance: date bucketing over the fact table | PASS |  |
| drop-database | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| drop-database | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| drop-database | query:enum: the empty member groups by its ordinal | PASS |  |
| drop-database | query:enum: the empty member sorts by its ordinal | PASS |  |
| drop-database | query:enum: the empty member is selectable by text | PASS |  |
| drop-database | query:geometry: hex round-trips the internal format | PASS |  |
| drop-database | query:geometry: byte length includes the srid prefix | PASS |  |
| drop-database | query:geometry: null routes filter and count | PASS |  |
| drop-database | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| drop-database | query:set: find_in_set filters by membership | PASS |  |
| drop-database | query:set: equality is literal, not member-normalized | PASS |  |
| drop-database | query:set: distinct values walk the bitmask including empty | PASS |  |
| drop-database | query:set: grouped counts order by bitmask not text | PASS |  |
| drop-database | query:set: a range predicate compares the bitmask | PASS |  |
| drop-database | query:star: fact with dimension and two audit persons | PASS |  |
| drop-database | query:star: five-alias chain fans out through events | PASS |  |
| drop-database | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| drop-database | query:star: five tables bridge the shop and the star | PASS |  |
| drop-database | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| drop-database | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| drop-database | query:json: length and keys survive null documents | PASS |  |
| drop-database | query:json: contains_path filters the documented rows | PASS |  |
| drop-database | query:json: json_value reads a scalar with sql semantics | PASS |  |
| drop-database | query:json: object construction embeds an extracted scalar | PASS |  |
| drop-database | query:json: search locates a literal value | PASS |  |
| drop-database | query:json: grouping by an extracted scalar | PASS |  |
| drop-database | query:json: merge_patch overlays and reads back | PASS |  |
| drop-database | query:temporal: quarter, weekday and name grains agree | PASS |  |
| drop-database | query:temporal: month-end bucketing via last_day | PASS |  |
| drop-database | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| drop-database | query:temporal: datetime range keeps the year window | PASS |  |
| drop-database | query:temporal: date_sub bound in the predicate | PASS |  |
| drop-database | query:temporal: year-month split grouping | PASS |  |
| drop-database | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| drop-database | query:regex: substr extracts the mail domain | PASS |  |
| drop-database | query:regex: the REGEXP operator anchors a class | PASS |  |
| drop-database | query:regex: replace folds suffix classes before grouping | PASS |  |
| drop-database | query:bi metabase: month grain through convert_tz | PASS |  |
| drop-database | query:bi metabase: iso week bucketing | PASS |  |
| drop-database | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| drop-database | query:bi metabase: previous-period revenue window | PASS |  |
| drop-database | query:bi superset: week-start grain with a rolling average | PASS |  |
| drop-database | query:bi superset: running total over grouped revenue | PASS |  |
| drop-database | query:bi superset: lag and lead against a named window | PASS |  |
| drop-database | query:bi superset: quartile counts from ntile | PASS |  |
| drop-database | query:bi superset: first and last value over an unbounded frame | PASS |  |
| drop-database | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| drop-database | query:bi looker: symmetric aggregate across a fanned-out join | PASS |  |
| drop-database | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| drop-database | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| drop-database | query:bi looker: the grouped primary key determines the row | PASS |  |
| drop-database | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| drop-database | query:bi tableau: explicit cast ladder | PASS |  |
| drop-database | query:bi tableau: the stddev and variance family | PASS |  |
| drop-database | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| drop-database | query:bi shared: substring_index dimension cleanup | PASS |  |
| drop-database | query:bi shared: json validity and typed path filter | PASS |  |
| drop-database | query:bi shared: contains_path over several paths at once | PASS |  |
| drop-database | query:bi shared: maketime from extracted parts | PASS |  |
| drop-database | query:bi shared: extract year_month grouping | PASS |  |
| drop-database | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| drop-database | query:staff: three-level management chain with an inactive tail | PASS |  |
| drop-database | query:staff: active split with id extremes | PASS |  |
| drop-database | query:counters: full unsigned ladder readback | PASS |  |
| drop-database | query:counters: greatest and least across widths | PASS |  |
| drop-database | query:dim: enum status split | PASS |  |
| drop-database | query:dim: pattern filter across collated columns | PASS |  |
| drop-database | query:person: anti-join finds owners without facts | PASS |  |
| drop-database | query:person: created-fact counts through a scalar subquery | PASS |  |
| drop-database | query:event: lag over per-dimension timelines | PASS |  |
| drop-database | query:event: daily grain per dimension code | PASS |  |
| drop-database | query:order_items: product rollup without the orders table | PASS |  |
| drop-database | query:shipments: carrier value through the items bridge | PASS |  |
| drop-database | query:json: distinct case variants survive a derived table | PASS |  |
| ddl-documented-gaps | converge:Dim | PASS |  |
| ddl-documented-gaps | converge:Event | PASS |  |
| ddl-documented-gaps | converge:Fact | PASS |  |
| ddl-documented-gaps | converge:Person | PASS |  |
| ddl-documented-gaps | converge:audit_history | WARN | pintail query failed: Error: unknown table e2e_db.audit_history |
| ddl-documented-gaps | converge:badges | PASS |  |
| ddl-documented-gaps | converge:counters | PASS |  |
| ddl-documented-gaps | converge:customers | PASS |  |
| ddl-documented-gaps | converge:keyless_log | PASS |  |
| ddl-documented-gaps | converge:order_items | WARN | row 0: |
| ddl-documented-gaps | converge:orders | PASS |  |
| ddl-documented-gaps | converge:shipments | PASS |  |
| ddl-documented-gaps | converge:staff | PASS |  |
| ddl-documented-gaps | converge:information_schema.columns | WARN | row 20: |
| ddl-documented-gaps | query:conformance: triple-alias person join with a dangling FK | PASS |  |
| ddl-documented-gaps | query:conformance: mixed-collation double grouping | PASS |  |
| ddl-documented-gaps | query:conformance: enum ordinal ordering disagrees with labels | PASS |  |
| ddl-documented-gaps | query:conformance: trailing-space grouping under PAD semantics | PASS |  |
| ddl-documented-gaps | query:conformance: case-variant code grouping | PASS |  |
| ddl-documented-gaps | query:conformance: anti-join finds the event-less dimension | PASS |  |
| ddl-documented-gaps | query:conformance: nullable join key NULL-extends | PASS |  |
| ddl-documented-gaps | query:conformance: timestamp ties page deterministically with a tiebreaker | PASS |  |
| ddl-documented-gaps | query:conformance: date bucketing over the fact table | PASS |  |
| ddl-documented-gaps | query:conformance: decimal aggregate spanning negatives and zero | PASS |  |
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
| ddl-documented-gaps | query:computed decimal rounds negative digits half away from zero | PASS |  |
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
| ddl-documented-gaps | query:enum: the empty member groups by its ordinal | PASS |  |
| ddl-documented-gaps | query:enum: the empty member sorts by its ordinal | PASS |  |
| ddl-documented-gaps | query:enum: the empty member is selectable by text | PASS |  |
| ddl-documented-gaps | query:geometry: hex round-trips the internal format | PASS |  |
| ddl-documented-gaps | query:geometry: byte length includes the srid prefix | PASS |  |
| ddl-documented-gaps | query:geometry: null routes filter and count | PASS |  |
| ddl-documented-gaps | query:geometry: spatial functions are a documented gap | WARN | spatial query functions are not implemented; geometry is carried as bytes only |
| ddl-documented-gaps | query:set: find_in_set filters by membership | PASS |  |
| ddl-documented-gaps | query:set: equality is literal, not member-normalized | PASS |  |
| ddl-documented-gaps | query:set: distinct values walk the bitmask including empty | PASS |  |
| ddl-documented-gaps | query:set: grouped counts order by bitmask not text | PASS |  |
| ddl-documented-gaps | query:set: a range predicate compares the bitmask | PASS |  |
| ddl-documented-gaps | query:star: fact with dimension and two audit persons | PASS |  |
| ddl-documented-gaps | query:star: five-alias chain fans out through events | PASS |  |
| ddl-documented-gaps | query:star: grouped rollup counts facts and events per dimension | PASS |  |
| ddl-documented-gaps | query:star: five tables bridge the shop and the star | SKIP |  |
| ddl-documented-gaps | query:star: null join keys stay unmatched through a four-table chain | PASS |  |
| ddl-documented-gaps | query:star: date-windowed join keeps only overlapping activity | PASS |  |
| ddl-documented-gaps | query:json: length and keys survive null documents | PASS |  |
| ddl-documented-gaps | query:json: contains_path filters the documented rows | PASS |  |
| ddl-documented-gaps | query:json: json_value reads a scalar with sql semantics | PASS |  |
| ddl-documented-gaps | query:json: object construction embeds an extracted scalar | PASS |  |
| ddl-documented-gaps | query:json: search locates a literal value | PASS |  |
| ddl-documented-gaps | query:json: grouping by an extracted scalar | PASS |  |
| ddl-documented-gaps | query:json: merge_patch overlays and reads back | PASS |  |
| ddl-documented-gaps | query:temporal: quarter, weekday and name grains agree | PASS |  |
| ddl-documented-gaps | query:temporal: month-end bucketing via last_day | PASS |  |
| ddl-documented-gaps | query:temporal: timestampdiff spans date and datetime operands | PASS |  |
| ddl-documented-gaps | query:temporal: datetime range keeps the year window | PASS |  |
| ddl-documented-gaps | query:temporal: date_sub bound in the predicate | PASS |  |
| ddl-documented-gaps | query:temporal: year-month split grouping | PASS |  |
| ddl-documented-gaps | query:temporal: sub-day grains on a microsecond timestamp | PASS |  |
| ddl-documented-gaps | query:regex: substr extracts the mail domain | PASS |  |
| ddl-documented-gaps | query:regex: the REGEXP operator anchors a class | PASS |  |
| ddl-documented-gaps | query:regex: replace folds suffix classes before grouping | PASS |  |
| ddl-documented-gaps | query:bi metabase: month grain through convert_tz | PASS |  |
| ddl-documented-gaps | query:bi metabase: iso week bucketing | PASS |  |
| ddl-documented-gaps | query:bi metabase: display formats for weekday, pretty date and clock | PASS |  |
| ddl-documented-gaps | query:bi metabase: previous-period revenue window | PASS |  |
| ddl-documented-gaps | query:bi superset: week-start grain with a rolling average | PASS |  |
| ddl-documented-gaps | query:bi superset: running total over grouped revenue | PASS |  |
| ddl-documented-gaps | query:bi superset: lag and lead against a named window | PASS |  |
| ddl-documented-gaps | query:bi superset: quartile counts from ntile | PASS |  |
| ddl-documented-gaps | query:bi superset: first and last value over an unbounded frame | PASS |  |
| ddl-documented-gaps | query:bi superset: compound interval grains | WARN | compound interval units (YEAR_MONTH, DAY_SECOND) are not parsed; sqlparser-rs has no qualifier for them |
| ddl-documented-gaps | query:bi looker: symmetric aggregate across a fanned-out join | SKIP |  |
| ddl-documented-gaps | query:bi looker: any_value reads a functionally dependent column | PASS |  |
| ddl-documented-gaps | query:bi looker: a grouped foreign key reads the joined dimension | PASS |  |
| ddl-documented-gaps | query:bi looker: the grouped primary key determines the row | PASS |  |
| ddl-documented-gaps | query:bi looker: a grouped self-join key reads the managers row | PASS |  |
| ddl-documented-gaps | query:bi tableau: explicit cast ladder | PASS |  |
| ddl-documented-gaps | query:bi tableau: the stddev and variance family | PASS |  |
| ddl-documented-gaps | query:bi tableau: bit aggregates over an unsigned flag column | PASS |  |
| ddl-documented-gaps | query:bi shared: substring_index dimension cleanup | PASS |  |
| ddl-documented-gaps | query:bi shared: json validity and typed path filter | PASS |  |
| ddl-documented-gaps | query:bi shared: contains_path over several paths at once | PASS |  |
| ddl-documented-gaps | query:bi shared: maketime from extracted parts | PASS |  |
| ddl-documented-gaps | query:bi shared: extract year_month grouping | PASS |  |
| ddl-documented-gaps | query:bi shared: keyset-free pagination with limit offset | PASS |  |
| ddl-documented-gaps | query:staff: three-level management chain with an inactive tail | PASS |  |
| ddl-documented-gaps | query:staff: active split with id extremes | PASS |  |
| ddl-documented-gaps | query:counters: full unsigned ladder readback | PASS |  |
| ddl-documented-gaps | query:counters: greatest and least across widths | PASS |  |
| ddl-documented-gaps | query:dim: enum status split | PASS |  |
| ddl-documented-gaps | query:dim: pattern filter across collated columns | PASS |  |
| ddl-documented-gaps | query:person: anti-join finds owners without facts | PASS |  |
| ddl-documented-gaps | query:person: created-fact counts through a scalar subquery | PASS |  |
| ddl-documented-gaps | query:event: lag over per-dimension timelines | PASS |  |
| ddl-documented-gaps | query:event: daily grain per dimension code | PASS |  |
| ddl-documented-gaps | query:order_items: product rollup without the orders table | SKIP |  |
| ddl-documented-gaps | query:shipments: carrier value through the items bridge | SKIP |  |
| ddl-documented-gaps | query:json: distinct case variants survive a derived table | PASS |  |

## Timing

| Phase | run s | converge s | corpus s |
|---|---|---|---|
| snapshot | 0.0 | 1.9 | 1.7 |
| orm-compat | 13.7 | 0.5 | 1.7 |
| crud | 0.7 | 0.8 | 1.5 |
| type-edges | 0.2 | 1.3 | 1.7 |
| ddl | 21.2 | 34.1 | 1.8 |
| schema-drift-minimal | 0.3 | 5.4 | 1.9 |
| schema-drift-unseen | 2.4 | 18.9 | 1.4 |
| churn | 26.0 | 1.9 | 1.3 |
| contention | 13.0 | 1.5 | 1.7 |
| execution-budget | 0.0 | 1.0 | 1.4 |
| spill | 2.9 | 1.0 | 2.1 |
| pooling | 0.1 | 0.6 | 1.6 |
| local-database | 0.1 | 0.9 | 2.3 |
| restart | 0.8 | 1.0 | 1.7 |
| activity-history | 1.2 | 0.5 | 1.5 |
| poll-storm | 40.3 | 0.7 | 1.6 |
| control-plane | 131.2 | 3.1 | 2.6 |
| snapshot-ddl-window | 14.8 | 2.2 | 1.3 |
| drop-table-cdc | 30.1 | 2.3 | 1.8 |
| drop-table-recreate | 170.8 | 2.4 | 1.8 |
| drop-table-polling | 60.9 | 2.5 | 1.4 |
| restart-during-snapshot | 13.9 | 3.9 | 1.7 |
| restart-during-resync | 17.7 | 2.5 | 1.5 |
| memory-pressure | 23.3 | 2.9 | 2.0 |
| reconcile-memory | 61.8 | 2.1 | 2.6 |
| drop-database | 36.0 | 2.4 | 2.0 |
| ddl-documented-gaps | 0.3 | 3.7 | 1.4 |
| total | 683.6 | 101.7 | 47.0 |

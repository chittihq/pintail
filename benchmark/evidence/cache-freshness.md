# Cached reads against cached reads, and freshness after ingestion

Issue #36. The canonical benchmark table times a Pintail settled-memo hit
against a ClickHouse execution with its query cache off. This study runs the
comparisons that table cannot make: both result caches on, and what each
serves once the source changes. Machine-readable data, including every
served answer of the freshness phase, is in
[`cache-freshness.json`](cache-freshness.json).

## Method

- **Host:** one 8-vCPU cloud host (AMD Ryzen 9 9950X). MySQL 8.4, ClickHouse
  26.8 and Pintail each run in a container capped at 8 CPUs and 8 GB, as in
  the main benchmark.
- **Data:** `BENCHMARK_SCALE=0.1`, so 2,000,000 orders, 100,000 users and
  10,000 products from the benchmark's deterministic seed. The question is
  the shape of each cache's behaviour, not scan speed, so the smaller scale
  was chosen to keep the run short; absolute execution times are not
  comparable with the 20M-row tables.
- **Pintail:** a plain release build (not profile-guided) of the commit
  carrying the settled-memo counters, in a runtime image built from that
  binary (`PINTAIL_BENCHMARK_IMAGE`). The settled memo is on. It keys an
  entry by table, manifest generation and plan signature, holds at most 32
  entries of at most 131,072 rows, and cannot serve a table whose memtable
  holds rows.
- **ClickHouse:** RMT+FINAL, read with `use_query_cache = 1`,
  `query_cache_min_query_runs = 0`, `query_cache_min_query_duration = 0`, and
  `query_cache_ttl = 3600` for the static phase or 60 (ClickHouse's default)
  for the TTL freshness mode. Server limits: `max_entries` 1024,
  `max_entry_size_in_bytes` 1 MiB, `max_entry_size_in_rows` 30,000,000,
  `max_size_in_bytes` 1 GiB. Nothing in either phase came near them.
- **Proof of a hit:** Pintail's `pintail_settled_memo_hits_total` on
  `/metrics`, read before and after every answer, outside its timing;
  ClickHouse's `QueryCacheHits` profile event in `system.query_log`, looked
  up per query id.
- **Correctness:** every answer is compared with MySQL's (canonical multiset
  form, as in the main harness).
- **Command:** `BENCHMARK_SCALE=0.1 BENCHMARK_CACHE_STUDY=1
  BENCHMARK_CONCURRENCY= bun run run.ts`. The memo-off execution track ran in
  the same run, unchanged.

## 1. Static: both caches warm

Two warmups, then 15 timed answers per engine per query. Medians, with the
hits proved among the 15 timed answers.

| Query | Pintail memo hit | memo hits | CH query-cache hit | cache hits | CH RMT+FINAL executed | Exact |
|---|---:|---:|---:|---:|---:|:--|
| Full table count | 1.2 ms | 15/15 | 1 ms | 15/15 | 2.9 ms | yes |
| Filtered count | 1.2 ms | 15/15 | 1.1 ms | 15/15 | 5.7 ms | yes |
| Group by status | 1.5 ms | 15/15 | 1.2 ms | 15/15 | 8.8 ms | yes |
| Region × status breakdown | 1.5 ms | 15/15 | 1.1 ms | 15/15 | 16 ms | yes |
| Monthly revenue (2023) | 1.9 ms | 15/15 | 1.3 ms | 15/15 | 8.5 ms | yes |
| Top 10 spenders | 11 ms | 15/15 | 0.8 ms | 15/15 | 29 ms | yes |
| Regional analytics | 1.3 ms | 15/15 | 0.9 ms | 15/15 | 17 ms | yes |
| Join users + orders | 1 ms | 15/15 | 1 ms | 15/15 | 24 ms | yes |

Both caches served every timed answer, and every answer matched MySQL. With
both caches on, the two engines are at parity within about half a
millisecond on seven of the eight shapes, and ClickHouse is ahead on all
eight. The exception is Top 10 spenders: Pintail's memo
stores the grouped aggregate (100,000 per-user rows at this scale) and the
ORDER BY and LIMIT run on top of every hit, while ClickHouse caches the final
ten rows. So the canonical table's lead over ClickHouse
comes from comparing a cache hit with an execution, and does not hold
against ClickHouse's cache.

## 2. Freshness after ingestion

Each event commits one change of 1,000 rows of an invented key band of
`orders` to MySQL: an insert of new keys above the highest key, an update
that moves rows to a new status `returned` and adds 1.00 to their amount,
or a delete. MySQL's answers after the change are taken from a rehearsal of
the same statement inside a transaction that is rolled back, so readers
start the moment the real commit returns. Two served queries change (count
and total, group by status). A control on the unchanged `users` table does
not. Readers poll every 100 ms. *Visible* is the first poll where a
row-level read of the changed keys (no result cache serves it) matches
MySQL, so it carries up to one poll of error. ClickHouse gets the change
from the harness into a `ReplacingMergeTree(_version, _is_deleted)` copy
the moment MySQL commits (5.6-12 ms to apply). That is a floor for any real
pipeline, not a replication measurement.

Per event, the two affected queries summed. Stale answers are counted after
the change was visible in that engine's data; every one of ClickHouse's
stale answers in TTL mode was a proved cache hit.

| Mode | Change | Engine | Cache warm before | Visible | First correct | Stale after visible | Old before visible | Wrong | First correct took | Steady after (hits) |
|---|---|---|:-:|---:|---:|---:|---:|---:|---:|---:|
| TTL | insert | Pintail | yes | 407 ms | 408 / 422 ms | 0 | 8 | 0 | 1.7 / 6.2 ms | 1.3 / 4.6 ms (0/10) |
| TTL | insert | ClickHouse | yes | 105 ms | 60,063 / 60,070 ms | 1,200 | 2 | 0 | 5.5 / 6.8 ms | 1.8 / 1.2 ms (10/10) |
| TTL | update | Pintail | no | 1,509 ms | 1,513 / 1,523 ms | 0 | 30 | 0 | 3.3 / 4.6 ms | 2.8 / 4.6 ms (0/10) |
| TTL | update | ClickHouse | yes | 106 ms | 60,013 / 60,023 ms | 1,198 | 2 | 0 | 6.2 / 10 ms | 1.7 / 1.1 ms (10/10) |
| TTL | delete | Pintail | no | 2,611 ms | 2,615 / 2,627 ms | 0 | 52 | 0 | 4 / 6.1 ms | 3.2 / 5.5 ms (0/10) |
| TTL | delete | ClickHouse | yes | 105 ms | 60,015 / 60,031 ms | 1,200 | 2 | 0 | 13 / 16 ms | 2 / 1.2 ms (10/10) |
| drop | insert | Pintail | no | 3,105 ms | 3,109 / 3,119 ms | 0 | 62 | 0 | 3.5 / 4.9 ms | 2.7 / 5.3 ms (0/10) |
| drop | insert | ClickHouse | yes | 120 ms | 130 / 142 ms | 0 | 2 | 0 | 10 / 13 ms | 1.7 / 1 ms (10/10) |
| drop | update | Pintail | no | 1,103 ms | 1,105 / 1,018 ms | 0 | 21 | 0 | 2.4 / 9.6 ms | 2.7 / 5.1 ms (0/11) |
| drop | update | ClickHouse | yes | 116 ms | 125 / 137 ms | 0 | 2 | 0 | 9.1 / 12 ms | 1.6 / 1.3 ms (10/10) |
| drop | delete | Pintail | no | 810 ms | 814 / 824 ms | 0 | 16 | 0 | 3.9 / 5.4 ms | 2.8 / 4.9 ms (0/10) |
| drop | delete | ClickHouse | yes | 116 ms | 133 / 153 ms | 0 | 2 | 0 | 17 / 19 ms | 1.5 / 1.4 ms (10/10) |

(Pairs are count-and-total / group-by-status. In the drop-mode update,
Pintail's group-by answer turned correct within the poll whose key probe
had just missed the change, so its first correct answer precedes the
recorded visibility.)

The control query on `users` stayed correct and cached throughout in both
engines: Pintail served it from the memo on every counted poll (9/9 to
36/36 per event), because a change to `orders` changes only `orders`' manifest
generation. ClickHouse also kept serving it from the cache.

ClickHouse drop-mode coordination, charged to that mode:

| Change | CH rows applied | Visible to coordinator | Probes | Probe time | Drop | Cache dropped at |
|---|---:|---:|---:|---:|---:|---:|
| insert | 12 ms | 16 ms | 1 | 2.8 ms | 0.5 ms | 16 ms |
| update | 11 ms | 15 ms | 1 | 3.6 ms | 0.6 ms | 16 ms |
| delete | 5.9 ms | 9.7 ms | 1 | 3.7 ms | 0.4 ms | 10 ms |

## 3. Reading it

- **Stale reads.** ClickHouse with TTL expiry served the pre-change answer,
  as a proved cache hit, until the entry expired: 599-600 stale answers per
  query per event, ~60 s after its own data already showed the change. That
  window is the TTL less the entry's age (32-34 ms at the commit here). With
  an explicit drop after a visibility check it served no stale answer. The
  coordination cost was one cache-off probe and a sub-millisecond drop,
  ~10-16 ms after commit. Note that `SYSTEM DROP QUERY CACHE` empties the
  whole cache, the unchanged-table control's entry included. Pintail served
  no stale answer and no wrong answer in any event.
- **Replication delay is the other half.** Pintail's own CDC showed the
  change 0.4-3.1 s after commit (the supervisor's cycle). Every old answer
  Pintail gave came before that point. ClickHouse's ~0.1 s is the harness
  writing directly, plus one poll; a real pipeline would add its own delay
  on top.
- **Recomputation and steady state.** After the change, ClickHouse's first
  correct answer was an execution of 5-19 ms, then hits of 1-2 ms. Pintail's
  memo did not come back. The memo serves only a table with an empty
  memtable, and after 1,000 changed rows `orders` did not settle again
  within the 60 s the harness waits before each event (`no` in *cache warm
  before*, 5 of 6 events; inferred from zero memo hits, not from a flush
  log). Pintail answered every post-change query by execution: 1.3-3.2 ms
  for count and total, 4.6-5.5 ms for group by status. For a low-volume change stream
  this is the honest Pintail number. It is always fresh, but it is not a
  cache hit.
- **What this does not show.** One 2M-row dataset, one change size, one
  poll interval. ClickHouse's replication is simulated, so its visibility is
  a floor. The freshness rows compare a Pintail that was mostly executing
  with a ClickHouse that was caching; the *cache warm before* column says
  which, per event.

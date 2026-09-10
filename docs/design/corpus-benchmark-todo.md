# Corpus benchmark follow-up todo

Source: the three-engine run of the full oracle corpus,
[`benchmark/corpus/results.csv`](../../benchmark/corpus/results.csv): 1,895
cases at scale 1 (8 to 13 rows per table) and scale 10,000 (80k to 130k rows),
against MySQL 8.4.11, ClickHouse 26.8 and Pintail v0.1.5-rc2 with its result
memo off.

| | Scale 1 | Scale 10,000 |
|---|---|---|
| Pintail ÷ MySQL (geometric mean) | 2.24× slower | 2.83× slower |
| Pintail ÷ ClickHouse | 2.4× faster | 3.44× slower |
| Pintail failures where MySQL answered | 3 | 50 (37 timeouts, 13 errors) |

## Decisions (owner, 2026-09-10)

- The cross-join safety estimate goes: runaway joins are stopped by the
  per-query memory ceiling and `max_execution_time`, as in MySQL.
- The join-proof work (native batches through unique integer joins,
  match-sized buckets, nested-join headroom) is folded into this pass. The
  parallel probe stays on its own branch.
- One branch, `perf/corpus-fixes`: every item is its own tested commit, then
  one full rc gate, then a merge to `dev`.
- After the merge, the corpus benchmark is rerun and its CSV committed. No
  release is cut from this pass.

## Items

Each item gets a reproduction in the in-process harness or the wire tests
first, then the fix, clippy and the touched crates' unit tests, and a commit.

1. [x] **Session collation on the wire.** `SET NAMES utf8mb4 COLLATE …` is
   accepted but literal comparisons ignore it: 7 cases (`'A' = 'a'`,
   `'É' = 'e'`, `'ß' = 'ss'`) answer differently over the MySQL protocol
   than in-process.
2. [ ] **Uncorrelated subqueries.** `SELECT id, (SELECT MAX(id) FROM users),
   id IN (SELECT id FROM users …) FROM events WHERE id = 1` takes about 6 s
   against 0.4 ms on MySQL, for one outer row: 77.5 s of Pintail's 154.7 s
   total excess.
3. [ ] **Early stop for ORDER BY … LIMIT.** `events JOIN users ON id … ORDER
   BY e.id LIMIT 5` takes 134 to 242 ms against 0.6 ms: everything is joined
   and sorted before the limit applies.
4. [ ] **Joins MySQL answers.** Remove the cross-join safety estimate (10
   refusals) and fix the theta, null-safe and quantified-subquery join
   timeouts (most of 37). Fold in join-proof.
5. [ ] **Fixed per-query cost.** `COUNT(*) … WHERE status BETWEEN …` takes
   30 ms against 17 ms on MySQL and 1.5 to 3 ms on ClickHouse; `DATE(day)`
   over 80k rows takes 169 ms against 19 ms; a query that reads no table
   costs 0.35 ms more than on MySQL.
6. [ ] **Confirm the expected differences.** 26 differences are `LIMIT` over
   tied rows and 25 are `GROUP_CONCAT` without `ORDER BY`; capture the rows
   for differing cases to prove it.
7. [ ] **Parity gaps.** `CASE` decimal branch scale, `GROUP BY` with a
   trailing no-break space, DECIMAL wider than 38 digits.
8. [ ] **Lifecycle replay.** Rerun `tests/e2e/parity-replay.ts`; 33 wire and
   session checks were red (prepared statements, `SHOW WARNINGS`, TIMESTAMP
   under time zones, multi-row scalar subquery errors, invalid JSON paths,
   `@@session.time_zone`).
9. [ ] **Gate and remeasure.** Full rc gate on the branch, merge to `dev`,
   rerun the corpus benchmark and commit the CSV.

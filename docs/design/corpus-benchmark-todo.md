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

The rerun on `dev` at 5256ec63, after items 1 to 8, on an idle docker host:
Pintail ÷ MySQL 2.29× and 2.79× slower, Pintail ÷ ClickHouse 2.4× faster
and 3.37× slower. Pintail fails 0 and 4 cases where MySQL answers, and
answers 12 at scale 10,000 that MySQL times out on.

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
2. [x] **Uncorrelated subqueries.** `SELECT id, (SELECT MAX(id) FROM users),
   id IN (SELECT id FROM users …) FROM events WHERE id = 1` takes about 6 s
   against 0.4 ms on MySQL, for one outer row: 77.5 s of Pintail's 154.7 s
   total excess.
3. [x] **Early stop for ORDER BY … LIMIT.** `events JOIN users ON id … ORDER
   BY e.id LIMIT 5` takes 134 to 242 ms against 0.6 ms: everything is joined
   and sorted before the limit applies.
4. [ ] **Joins MySQL answers.** Remove the cross-join safety estimate (10
   refusals) and fix the theta, null-safe and quantified-subquery join
   timeouts (most of 37). Fold in join-proof.
   Done: the estimate is gone (c3ff1de6), and constant ON conjuncts,
   exact-decimal IN keys, self-join and null-safe EXISTS decorrelation and
   the anti join's first-match stop cleared 16 of the 19 join and
   subquery timeouts at scale 10,000. Open: a correlated IN inside an ON
   clause, a doubly nested scalar subquery, and a second NOT EXISTS reusing
   the first's alias; join-proof waits on its own qualification.
5. [x] **Window frames.** Six window queries time out at scale 10,000:
   `ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING`, `RANGE` peer
   groups over a low-cardinality key, and numeric and temporal `RANGE`
   offsets, each recomputing its frame per row.
6. [ ] **Fixed per-query cost.** `COUNT(*) … WHERE status BETWEEN …` takes
   30 ms against 17 ms on MySQL and 1.5 to 3 ms on ClickHouse; `DATE(day)`
   over 80k rows takes 169 ms against 19 ms; a query that reads no table
   costs 0.35 ms more than on MySQL.
7. [x] **Confirm the expected differences.** 26 differences are `LIMIT` over
   tied rows and 25 are `GROUP_CONCAT` without `ORDER BY`; capture the rows
   for differing cases to prove it.
   Confirmed on the rerun: of 61 differences at scale 10,000, 26 are
   `LIMIT` over tied rows and 34 are orders MySQL leaves unspecified
   (`GROUP_CONCAT`, `JSON_ARRAYAGG` and `JSON_OBJECTAGG` without
   `ORDER BY`, and `ROW_NUMBER`, `FIRST_VALUE` and `LAST_VALUE` over tied
   sort keys). The one real difference was a DOUBLE printed without
   MySQL's exponent notation (`POW` over large ids), fixed in 565ff396.
8. [x] **Parity gaps.** `CASE` decimal branch scale, `GROUP BY` with a
   trailing no-break space, DECIMAL wider than 38 digits.
9. [ ] **Lifecycle replay.** Rerun `tests/e2e/parity-replay.ts`; 33 wire and
   session checks were red (prepared statements, `SHOW WARNINGS`, TIMESTAMP
   under time zones, multi-row scalar subquery errors, invalid JSON paths,
   `@@session.time_zone`).
   Done on `fix/wire-session-parity`: a text-stored DECIMAL widens its scale
   in place; prepared-statement results carry MySQL's nullability and
   parameter widths, a string cast to an integer parameter saturates, and
   a floating-point LIMIT is refused; `SHOW WARNINGS` lists the last
   statement's error or its division-by-zero and GROUP_CONCAT warnings,
   with MySQL's out-of-range message; `NO_UNSIGNED_SUBTRACTION` is
   implemented; a TIMESTAMP reads in the session zone; 1242 and 3143 are
   answered; a session starts in the source's global zone. The replay rerun
   follows the branch's gate.
10. [x] **Gate and remeasure.** Full rc gate on the branch, merge to `dev`,
   rerun the corpus benchmark and commit the CSV. The gate passed at
   d787d3a6, the branch merged as 5256ec63, and the rerun's CSV is
   3bc2054b.

# What the calendar copy check costs when its proof fails

Under the date-validation modes (MySQL 8.4's default `NO_ZERO_DATE` and
`NO_ZERO_IN_DATE` included), a DATE or DATETIME copied into a grouping or
deduplication result is validated. A column is spared the check only when
its statistics prove every stored value is a real calendar date. This
measures what is lost when that proof fails.

- Arms: base `ded2943e` (no copy check), head `f6094dc6` (copy check), and
  head2, a second copy of head, as the same-commit noise floor. Release
  builds, one replica copy per arm, `PINTAIL_DISABLE_SETTLED_MEMO=1`,
  `PINTAIL_COMPACTION_INPUT_ROWS=1`.
- Host: 8-vCPU cloud host (AMD Ryzen 9 9950X).
- Data: invented table `(id BIGINT PK, d DATE, dt DATETIME, x INT)` with
  20M rows over 1,096 days (2022-2024). Variant a has only real dates.
  Variant b has the same rows, but every 1000th row has zero `d` and `dt`
  (20,000 rows, 0.1%, loaded with zeros allowed). That spread puts a zero
  in every batch and every segment.
- Method: over the wire, the default session mode, 6 rounds × 15 cycles,
  with the arms interleaved and their order rotating. n = 90 per cell, and
  every answer agreed across the arms. CPU is the server process's
  utime+stime.

| query | var | base median / min / cpu ms | head median / min / cpu ms | head vs base (median) | head2 vs head |
|---|---|---|---|---|---|
| `GROUP BY d` with COUNT, SUM | a | 12.8 / 10.7 / 75 | 13.1 / 10.5 / 70 | +2% | 0% |
| | b | 1,470 / 1,313 / 5,380 | 2,090 / 1,799 / 10,780 | **+42%**, CPU ×2.0 | +0.4% |
| `GROUP BY DATE(dt)` | a | 17.0 / 13.6 / 90 | 16.3 / 13.4 / 90 | -4% | -1% |
| | b | 1,731 / 1,522 / 9,300 | 1,904 / 1,675 / 10,350 | **+10%** | -0.2% |
| `COUNT(DISTINCT d)` | a | 77.1 / 70.2 / 120 | 76.4 / 68.2 / 120 | -1% | 0% |
| | b | 5,823 / 5,589 / 7,105 | 7,273 / 7,015 / 8,520 | **+25%** | +0.3% |
| derived `GROUP BY d` with `x > 0` | a | 16.4 / 11.8 / 90 | 16.9 / 12.5 / 90 | +3% | -4% |
| | b | 1,137 / 1,010 / 4,610 | 1,682 / 1,503 / 9,215 | **+48%**, CPU ×2.0 | +0.7% |

## Reading

- When the proof holds (variant a), the check costs nothing. Every
  difference is within the floor.
- When the proof fails (variant b), head is 10-48% slower than base, and
  on two queries it uses twice the CPU. The check then runs row by row,
  over batches that are already text.
- The larger cost is in base itself. The same 0.1% of zero dates make base
  70-115× slower than on clean data (`GROUP BY d`: 12.8 ms → 1,470 ms).
  A batch holding a zero date does not stay packed, so it loses the packed
  and column folds whether or not the copy check runs.

## Recommendation

A per-segment "all dates valid" flag is not worth a format change. With
zeros spread like this, every segment would carry the flag as false, so it
would recover none of the 10-48%. It would help only when invalid dates
sit together in a few segments, and even then it would leave base's
100× loss in place for those segments.

A per-batch proof from the packed values recovers nothing here either,
because these batches are not packed. The fix that removes both costs is
to keep a batch packed when some of its rows hold the zero date or an
invalid date: a packed spelling for the zero date, or a side mask of
non-civil rows. The copy check then becomes a vectorized rewrite of the
masked rows, and a batch with an empty mask passes as it does today.
Whether storage needs a new encoding for this, or whether only the batch
decode does, has not been examined. A cheaper interim step is a single
vectorized pass of the check over text batches, with no per-row `Value`
round trip. That targets the CPU doubling.

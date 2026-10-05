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

## After: the zero date kept packed

The zero date now packs as the day before 0000-01-01 (the zero datetime
as that day's midnight). That unit orders before every real date, as
MySQL orders the zero date. Two changes use it:

- **Decode only.** The executor packs a stored text column that holds
  the zero date, and the integer-range fold gives the zero date a slot of
  its own. Without that slot, the zero date would widen a range of days
  by about 740,000 days. The copy check passes a packed batch whole. It
  reads a batch still held as text in one pass over its bytes.
- **Format 8.** The segment writer stores such a column as native units,
  zero date included. The decoder does no text parse and no copy.

Same data, same queries and same method as above. The arms are base
`3213a825`, the decode-only build on base's format 7 replica, and head
(format 8) on a replica that head snapshotted. head2, a second copy of
head, gives the same-commit floor. Host: 8-vCPU cloud host (AMD Ryzen 9
9950X). n = 90 per cell, and every answer agreed across the arms.

| query | var | base median / min / cpu ms | decode-only median / min / cpu | head median / min / cpu | head b vs head a | head2 vs head |
|---|---|---|---|---|---|---|
| `GROUP BY d` with COUNT, SUM | a | 12.7 / 10.0 / 70 | 12.7 / 10.6 / 70 | 12.7 / 9.9 / 70 | | -2% |
| | b | 2,060 / 1,803 / 11,135 | 262 / 233 / 1,795 | **15.3** / 11.8 / 90 | **1.20×** | -1% |
| `GROUP BY DATE(dt)` | a | 15.0 / 12.7 / 90 | 14.9 / 12.7 / 80 | 15.3 / 12.9 / 90 | | -2% |
| | b | 1,925 / 1,641 / 10,855 | 139 / 118 / 945 | **19.9** / 16.4 / 120 | **1.30×** | -4% |
| `COUNT(DISTINCT d)` | a | 74.1 / 67.5 / 110 | 77.2 / 68.8 / 110 | 75.9 / 69.6 / 110 | | -2% |
| | b | 7,443 / 7,077 / 8,795 | 343 / 280 / 1,960 | **76.3** / 67.6 / 110 | **1.01×** | -1% |
| derived `GROUP BY d` with `x > 0` | a | 15.0 / 11.0 / 80 | 14.4 / 11.6 / 80 | 14.2 / 11.6 / 80 | | -2% |
| | b | 1,738 / 1,487 / 9,760 | 273 / 235 / 1,840 | **13.9** / 11.4 / 80 | **0.98×** | +1% |

- Variant b is now within 1.3× of variant a on every query, against
  70-115× before. Base itself measured somewhat slower here than in the
  run above (`GROUP BY d`, b: 2,060 ms against 1,470 ms), because the box
  ran in its slower CPU state. The arms were measured together, so the
  ratios stand.
- Variant a moves by -5% to +2% against base, inside the ±4% floor (the
  smallest same-binary difference above is 1-4%).
- Decode-only gets variant b 6-22× faster than base, but it stays 4-21×
  slower than variant a. Each scan still parses 20M stored strings, which
  is 950-1,960 ms of CPU against 80-110 ms. Format 8 removes the parse,
  and with it a further 4.5-20× (`GROUP BY d` 262 → 15.3 ms, `DATE(dt)`
  139 → 19.9 ms, `COUNT(DISTINCT d)` 343 → 76.3 ms, derived 273 → 13.9
  ms).
- The replica is also smaller: 415 MB against 505 MB for the same data,
  because both calendar columns of the zero-date table are stored as
  units.
- Before the range fold had its zero slot, head's b variant still took
  the hash path on `GROUP BY d` and `GROUP BY DATE(dt)` (112 and 95 ms,
  7-10× of variant a). The range of days then spanned the zero date and
  went past the fold's bound.

//! Row-range work units for the parallel aggregate rounds.
//!
//! A round used to hand each worker one whole batch, so its parallel width
//! was the number of batches it held: a round cut short by the memory
//! ceiling, the last round of a scan, or a table of two batches ran on two
//! threads of a sixteen-thread pool. Cutting the round's batches into
//! bounded row ranges instead lets the width come from the pool while the
//! rows in flight stay whatever the round can afford; an idle worker takes
//! the next range rather than waiting for a whole batch to be its own.

use std::ops::Range;

use crate::{RecordBatch, batch::SelectedRows};

/// Fewest physical rows a morsel is cut to. Below this the per-unit costs -
/// a partial group map, its merge, the dictionary tables - outweigh the
/// balance a finer cut buys.
const MIN_MORSEL_ROWS: usize = 4_096;

/// Morsels a round aims for per pool thread. Several per thread lets a
/// worker that finishes early take another instead of idling until the
/// slowest range completes.
const MORSELS_PER_THREAD: usize = 2;

/// One contiguous physical row range of a batch. Rows outside the batch's
/// selection are skipped exactly as a whole-batch pass would skip them.
#[derive(Clone, Debug)]
pub(super) struct Morsel<'a> {
    pub(super) batch: &'a RecordBatch,
    pub(super) rows: Range<usize>,
}

impl<'a> Morsel<'a> {
    /// The whole batch as a single morsel.
    pub(super) fn whole(batch: &'a RecordBatch) -> Self {
        Self {
            batch,
            rows: 0..batch.row_count(),
        }
    }

    /// Selected physical rows of this range, ascending.
    pub(super) fn selected_rows(&self) -> SelectedRows<'a> {
        self.batch.selection().selected_rows_in(self.rows.clone())
    }

    /// Number of selected rows in this range.
    pub(super) fn selected_count(&self) -> usize {
        self.batch.selection().count_in(self.rows.clone())
    }
}

/// The largest morsel count a round should produce for the pool it runs on.
pub(super) fn default_morsel_limit() -> usize {
    rayon::current_num_threads()
        .max(1)
        .saturating_mul(MORSELS_PER_THREAD)
}

/// Cuts a sequence of batch row counts into at most `max_morsels` ranges of
/// at least [`MIN_MORSEL_ROWS`] rows each (a batch shorter than that is one
/// morsel). Each entry is `(batch index, physical row range)`; empty
/// batches produce nothing.
pub(super) fn morsel_plan(
    row_counts: impl IntoIterator<Item = usize>,
    max_morsels: usize,
) -> Vec<(usize, Range<usize>)> {
    let row_counts: Vec<usize> = row_counts.into_iter().collect();
    let total: usize = row_counts.iter().sum();
    let target = total
        .div_ceil(max_morsels.max(1))
        .max(MIN_MORSEL_ROWS)
        .max(1);
    let mut plan = Vec::with_capacity(total.div_ceil(target).max(row_counts.len()));
    for (index, rows) in row_counts.into_iter().enumerate() {
        if rows == 0 {
            continue;
        }
        // Equal cuts rather than `target`-sized ones with a short remainder:
        // a 70K batch at a 64K target becomes two 35K morsels, not 64K + 6K.
        let pieces = rows.div_ceil(target).max(1);
        let piece = rows.div_ceil(pieces);
        let mut start = 0;
        while start < rows {
            let end = start.saturating_add(piece).min(rows);
            plan.push((index, start..end));
            start = end;
        }
    }
    plan
}

/// Cuts a round's batches into morsels for the pool.
pub(super) fn split_into_morsels(batches: &[RecordBatch]) -> Vec<Morsel<'_>> {
    split_into_morsels_bounded(batches, default_morsel_limit())
}

/// Cuts a round's batches into at most `max_morsels` morsels.
pub(super) fn split_into_morsels_bounded(
    batches: &[RecordBatch],
    max_morsels: usize,
) -> Vec<Morsel<'_>> {
    morsel_plan(batches.iter().map(RecordBatch::row_count), max_morsels)
        .into_iter()
        .map(|(index, rows)| Morsel {
            batch: &batches[index],
            rows,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{MIN_MORSEL_ROWS, morsel_plan};

    #[test]
    fn a_short_batch_is_one_morsel() {
        assert_eq!(morsel_plan([100], 64), vec![(0, 0..100)]);
    }

    #[test]
    fn empty_batches_produce_nothing() {
        assert_eq!(morsel_plan([0, 10, 0], 64), vec![(1, 0..10)]);
    }

    #[test]
    fn a_large_round_is_cut_to_the_limit_in_equal_pieces() {
        let plan = morsel_plan([65_536, 65_536], 8);
        assert_eq!(plan.len(), 8);
        assert!(plan.iter().all(|(_, rows)| rows.len() == 16_384));
        assert_eq!(plan[0], (0, 0..16_384));
        assert_eq!(plan[7], (1, 49_152..65_536));
    }

    #[test]
    fn morsels_never_fall_below_the_floor() {
        let plan = morsel_plan([65_536], 1_000);
        assert!(plan.iter().all(|(_, rows)| rows.len() >= MIN_MORSEL_ROWS));
        assert_eq!(plan.len(), 16);
    }

    #[test]
    fn a_remainder_spreads_across_equal_cuts() {
        // 130,000 rows over two morsels is a 65,000 target: the 70,000-row
        // batch becomes two cuts of 35,000, not 65,000 + 5,000.
        let plan = morsel_plan([70_000, 60_000], 2);
        assert_eq!(
            plan,
            vec![(0, 0..35_000), (0, 35_000..70_000), (1, 0..60_000)]
        );
    }

    #[test]
    fn every_row_is_covered_exactly_once() {
        let counts = [65_536, 12_345, 1, 4_096, 100_000];
        let plan = morsel_plan(counts, 16);
        let mut covered = vec![0_usize; counts.len()];
        let mut expected_start = vec![0_usize; counts.len()];
        for (index, rows) in plan {
            assert_eq!(rows.start, expected_start[index], "ranges are contiguous");
            expected_start[index] = rows.end;
            covered[index] += rows.len();
        }
        assert_eq!(covered, counts.to_vec());
    }
}

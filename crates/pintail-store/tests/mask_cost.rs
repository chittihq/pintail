//! What building the overlay's superseded-row mask costs, as a linear walk
//! of the segment's keys against a search driven by the changed keys. The
//! question is whether a table taking a steady trickle of updates pays for
//! its size or for its changes. Ignored: a measurement, not a gate. Run
//! with `cargo test --release -p pintail-store --test mask_cost --
//! --ignored --nocapture`.
use std::time::Instant;

const SEGMENT_ROWS: usize = 10_000_000;
const BLOCK_ROWS: usize = 8_192;

/// The linear merge walk the overlay uses now: both sides sorted, advance
/// whichever is behind. Cost follows the segment, whatever changed.
fn linear_mask(keys: &[i64], changed: &[i64]) -> Vec<usize> {
    let mut excluded = Vec::new();
    let mut row = 0_usize;
    let mut next = 0_usize;
    while row < keys.len() && next < changed.len() {
        match keys[row].cmp(&changed[next]) {
            std::cmp::Ordering::Less => row += 1,
            std::cmp::Ordering::Equal => {
                excluded.push(row);
                row += 1;
                next += 1;
            }
            std::cmp::Ordering::Greater => next += 1,
        }
    }
    excluded
}

/// Block by block, take the changed keys that fall inside the block's range
/// and look each one up. A block whose range holds no changed key is not
/// examined at all. Cost follows the changes.
fn searched_mask(keys: &[i64], changed: &[i64]) -> Vec<usize> {
    let mut excluded = Vec::new();
    let mut cursor = 0_usize;
    for (block, rows) in keys.chunks(BLOCK_ROWS).enumerate() {
        let base = block * BLOCK_ROWS;
        let (first, last) = (rows[0], rows[rows.len() - 1]);
        while cursor < changed.len() && changed[cursor] < first {
            cursor += 1;
        }
        if cursor >= changed.len() || changed[cursor] > last {
            // Nothing changed inside this block's key range.
            continue;
        }
        let mut probe = cursor;
        while probe < changed.len() && changed[probe] <= last {
            if let Ok(offset) = rows.binary_search(&changed[probe]) {
                excluded.push(base + offset);
            }
            probe += 1;
        }
    }
    excluded
}

#[test]
#[ignore = "a measurement over a large key column, not a gate"]
fn the_masks_agree_and_one_of_them_follows_the_changes() {
    let keys: Vec<i64> = (0..SEGMENT_ROWS as i64).collect();
    println!("{SEGMENT_ROWS} segment rows, {BLOCK_ROWS}-row blocks, minimum of 5 runs");
    println!(
        "{:>12}  {:>12}  {:>12}  {:>8}",
        "changed", "linear ms", "searched ms", "ratio"
    );
    for changed_rows in [2_usize, 2_000, 20_000, 200_000, 2_000_000] {
        // Scattered the way an UPDATE ... WHERE scatters them.
        let stride = SEGMENT_ROWS / changed_rows;
        let changed: Vec<i64> = (0..changed_rows)
            .map(|n| i64::try_from(n * stride).expect("small"))
            .collect();
        let mut linear = f64::MAX;
        let mut searched = f64::MAX;
        let mut left = Vec::new();
        let mut right = Vec::new();
        for _ in 0..5 {
            let clock = Instant::now();
            left = linear_mask(&keys, &changed);
            linear = linear.min(clock.elapsed().as_secs_f64());
            let clock = Instant::now();
            right = searched_mask(&keys, &changed);
            searched = searched.min(clock.elapsed().as_secs_f64());
        }
        assert_eq!(left, right, "the two masks must agree at {changed_rows}");
        println!(
            "{changed_rows:>12}  {:>12.3}  {:>12.3}  {:>7.1}x",
            linear * 1e3,
            searched * 1e3,
            linear / searched
        );
    }
}

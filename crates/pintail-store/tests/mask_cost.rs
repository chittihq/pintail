//! What building the overlay's superseded-row mask costs, as a linear walk
//! of the segment's keys against a search driven by the changed keys. The
//! question is whether a table taking a steady trickle of updates pays for
//! its size or for its changes. Ignored: a measurement, not a gate. Run
//! with `cargo test --release -p pintail-store --test mask_cost --
//! --ignored --nocapture`.
//!
//! Two things the first version of this measurement left out, either of
//! which could have carried the result:
//!
//! - it changed only keys the segment already held, so the insert path was
//!   never measured. A changed key with no row to supersede has to be
//!   placed after the survivors below it and after the inserts already
//!   placed, and getting that count wrong is the mistake this shape is
//!   here to catch as well as to time;
//! - it scattered the changed keys evenly, which is the shape a search
//!   does worst on: every part of the key column is touched and no lookup
//!   reuses a cache line the last one warmed. Updates in practice cluster
//!   towards recent rows, so the clustered arm is here to show the spread
//!   rather than let its worst end stand for the whole.
//!
//! One thing deliberately not varied: how dense the key VALUES are. The
//! keys live in one contiguous array whatever they hold, so a binary
//! search over sparse values costs what it costs over dense ones. What
//! sparseness actually changes is whether a changed key is found at all,
//! and that is the insert arms.
use std::time::Instant;

const SEGMENT_ROWS: usize = 10_000_000;
/// Keys step by two, so an odd value is a key the segment does not hold
/// and the insert arms have somewhere to land.
const STEP: i64 = 2;

/// The positions a mask has to produce: rows of the segment that a changed
/// key supersedes, and where in the merged output a changed key that
/// supersedes nothing has to be placed.
type Mask = (Vec<usize>, Vec<usize>);

/// The linear merge walk the overlay used to do: both sides sorted,
/// advance whichever is behind. Cost follows the segment, whatever
/// changed.
fn linear_mask(keys: &[i64], changed: &[i64]) -> Mask {
    let mut excluded = Vec::new();
    let mut inserts = Vec::new();
    let mut row = 0_usize;
    let mut survivors = 0_usize;
    for key in changed {
        while row < keys.len() && keys[row] < *key {
            row += 1;
            survivors += 1;
        }
        if row < keys.len() && keys[row] == *key {
            // Superseded, so it is not one of the survivors the next
            // insert counts.
            excluded.push(row);
            row += 1;
        } else {
            inserts.push(survivors + inserts.len());
        }
    }
    (excluded, inserts)
}

/// Look each changed key up instead of walking to it. Cost follows the
/// changes.
///
/// The insert position is the subtle half. A changed key with no row of
/// its own sits after every segment row below it that survived, and after
/// every insert already placed. The rows below it that did NOT survive are
/// exactly the exclusions found so far, because the changed keys are
/// sorted - and an exclusion for the current key, if there is one, is
/// counted after this and never before it.
fn searched_mask(keys: &[i64], changed: &[i64]) -> Mask {
    let mut excluded = Vec::new();
    let mut inserts = Vec::new();
    for key in changed {
        match keys.binary_search(key) {
            Ok(found) => excluded.push(found),
            Err(insertion) => inserts.push(insertion - excluded.len() + inserts.len()),
        }
    }
    (excluded, inserts)
}

/// The same search, but each lookup starts where the last one landed.
///
/// The changed keys are sorted, so nothing below the previous match can
/// hold the next one. Searching the remaining slice turns a walk of the
/// whole key column into a walk of the gap between two changes, which is
/// both a shorter search and one that stays in cache the dense case has
/// already warmed.
fn narrowed_mask(keys: &[i64], changed: &[i64]) -> Mask {
    let mut excluded = Vec::new();
    let mut inserts = Vec::new();
    let mut base = 0_usize;
    for key in changed {
        match keys[base..].binary_search(key) {
            Ok(found) => {
                excluded.push(base + found);
                base += found + 1;
            }
            Err(offset) => {
                let insertion = base + offset;
                inserts.push(insertion - excluded.len() + inserts.len());
                base = insertion;
            }
        }
    }
    (excluded, inserts)
}

/// How the changed keys sit against the segment's.
#[derive(Clone, Copy)]
enum Shape {
    /// Evenly spread over the whole segment; every part of it is touched.
    Scattered,
    /// A contiguous run at the end, as a burst of recent activity leaves.
    Clustered,
    /// Evenly spread, and every one supersedes a row that is there.
    ScatteredInserts,
    /// Evenly spread, and none of them supersedes anything.
    Mixed,
}

impl Shape {
    const fn label(self) -> &'static str {
        match self {
            Self::Scattered => "scattered",
            Self::Clustered => "clustered",
            Self::ScatteredInserts => "all inserts",
            Self::Mixed => "half inserts",
        }
    }
}

/// The changed keys for one shape, sorted.
fn changed_keys(shape: Shape, count: usize) -> Vec<i64> {
    let stride = i64::try_from(SEGMENT_ROWS / count).expect("small") * STEP;
    let last = i64::try_from(SEGMENT_ROWS - 1).expect("small") * STEP;
    (0..count)
        .map(|n| {
            let n = i64::try_from(n).expect("small");
            match shape {
                Shape::Scattered => n * stride,
                Shape::Clustered => last - (i64::try_from(count).expect("small") - 1 - n) * STEP,
                // An odd key is one the segment does not hold.
                Shape::ScatteredInserts => n * stride + 1,
                Shape::Mixed => n * stride + (n % 2),
            }
        })
        .collect()
}

#[test]
#[ignore = "a measurement over a large key column, not a gate"]
fn the_masks_agree_and_one_of_them_follows_the_changes() {
    let keys: Vec<i64> = (0..SEGMENT_ROWS)
        .map(|n| i64::try_from(n).expect("small") * STEP)
        .collect();
    println!(
        "{SEGMENT_ROWS} segment rows, minimum of 5 runs, all three masks compared for equality"
    );
    println!(
        "{:>14}  {:>10}  {:>12}  {:>12}  {:>12}  {:>8}  {:>8}",
        "shape", "changed", "linear ms", "searched ms", "narrowed ms", "search", "narrow"
    );
    for shape in [
        Shape::Scattered,
        Shape::Clustered,
        Shape::Mixed,
        Shape::ScatteredInserts,
    ] {
        for count in [
            2_usize, 2_000, 20_000, 50_000, 100_000, 150_000, 200_000, 500_000, 2_000_000,
        ] {
            let changed = changed_keys(shape, count);
            let mut linear = f64::MAX;
            let mut searched = f64::MAX;
            let mut narrowed = f64::MAX;
            let mut walked = (Vec::new(), Vec::new());
            let mut looked_up = (Vec::new(), Vec::new());
            let mut resumed = (Vec::new(), Vec::new());
            for _ in 0..5 {
                let clock = Instant::now();
                walked = linear_mask(&keys, &changed);
                linear = linear.min(clock.elapsed().as_secs_f64());
                let clock = Instant::now();
                looked_up = searched_mask(&keys, &changed);
                searched = searched.min(clock.elapsed().as_secs_f64());
                let clock = Instant::now();
                resumed = narrowed_mask(&keys, &changed);
                narrowed = narrowed.min(clock.elapsed().as_secs_f64());
            }
            assert_eq!(
                walked,
                looked_up,
                "the walk and the search must agree: {} keys, {count} changed",
                shape.label()
            );
            assert_eq!(
                walked,
                resumed,
                "the walk and the narrowed search must agree: {} keys, {count} changed",
                shape.label()
            );
            println!(
                "{:>14}  {count:>10}  {:>12.3}  {:>12.3}  {:>12.3}  {:>7.1}x  {:>7.1}x",
                shape.label(),
                linear * 1e3,
                searched * 1e3,
                narrowed * 1e3,
                linear / searched,
                linear / narrowed
            );
        }
    }
}

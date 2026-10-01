//! A flat hash table for integer join keys too widely spread for a
//! direct-address table: auto-increment ids with long gaps, ids minted from
//! a clock and a counter, hashes stored as BIGINT.
//!
//! The general build keeps such keys in sixty-four hash maps of
//! `Vec`-per-key buckets, filled on one thread: an allocation per key, a
//! tagged key compared per probe, and a second lookup by bucket address to
//! learn which group a fused aggregate folds the row into. Here the keys
//! are split by the top bits of their hash into regions small enough to
//! stay in cache while they are written, every region is laid out on its
//! own worker, and a probe is one hash and - at half load - about one slot
//! read. A key's rows sit side by side in the order they were read, exactly
//! as the direct-address table keeps them.

use rayon::prelude::*;

use crate::batch::mix64;

/// Keys a region is sized for: its slots then fit comfortably in L2.
const REGION_KEYS: usize = 8_192;

/// Most regions a table is split into.
const MAX_REGIONS: usize = 1 << 14;

/// One slot: a key's bits, and one more than its entry - zero where the
/// slot is empty.
#[derive(Clone, Copy, Default)]
struct Slot {
    key: u64,
    entry: u32,
}

/// One region's slots within the table, and what its entries count from.
#[derive(Clone, Copy)]
struct Region {
    offset: usize,
    mask: usize,
    base: u32,
}

/// Integer keys of one signedness, each resolving to an entry number.
pub(super) struct SparseKeyTable {
    signed: bool,
    /// A hash's region is its bits above this; 64 when there is one region.
    shift: u32,
    regions: Vec<Region>,
    slots: Vec<Slot>,
}

/// A table laid out from a build side's keys, with each entry's rows.
pub(super) struct SparseLayout<R> {
    pub(super) table: SparseKeyTable,
    /// Entry `e` holds `rows[starts[e]..starts[e + 1]]`.
    pub(super) starts: Vec<usize>,
    pub(super) rows: Vec<R>,
}

/// The bits of `key` among keys of one signedness, `None` when it is
/// outside that type and so equals none of them.
#[inline]
fn key_bits(key: i128, signed: bool) -> Option<u64> {
    if signed {
        i64::try_from(key).ok().map(i64::cast_unsigned)
    } else {
        u64::try_from(key).ok()
    }
}

impl SparseKeyTable {
    /// Bytes a table of `keys` keys holds, at most.
    pub(super) fn bytes_for(keys: usize) -> usize {
        // Each region rounds twice its keys up to a power of two.
        keys.saturating_mul(4)
            .saturating_add(MAX_REGIONS * 2)
            .saturating_mul(size_of::<Slot>())
            .saturating_add(MAX_REGIONS.saturating_mul(size_of::<Region>()))
    }

    /// Bytes this table holds.
    pub(super) fn bytes(&self) -> usize {
        self.slots
            .len()
            .saturating_mul(size_of::<Slot>())
            .saturating_add(self.regions.len().saturating_mul(size_of::<Region>()))
    }

    #[inline]
    fn region(&self, hash: u64) -> Region {
        self.regions[usize::try_from(hash.checked_shr(self.shift).unwrap_or(0)).unwrap_or(0)]
    }

    /// The entry `key` names, if any.
    #[inline]
    pub(super) fn find(&self, key: i128) -> Option<usize> {
        let bits = key_bits(key, self.signed)?;
        let hash = mix64(bits);
        let region = self.region(hash);
        #[allow(clippy::cast_possible_truncation)] // masked to the region
        let mut index = hash as usize & region.mask;
        loop {
            let slot = self.slots[region.offset + index];
            if slot.entry == 0 {
                return None;
            }
            if slot.key == bits {
                return Some((region.base + slot.entry - 1) as usize);
            }
            index = (index + 1) & region.mask;
        }
    }

    /// The same keys, each resolving to `entry(its entry here)`.
    pub(super) fn remapped<E>(
        &self,
        mut entry: impl FnMut(usize) -> Result<u32, E>,
    ) -> Result<Self, E> {
        let mut slots = vec![Slot::default(); self.slots.len()];
        for region in &self.regions {
            let range = region.offset..=region.offset + region.mask;
            for (mine, theirs) in slots[range.clone()].iter_mut().zip(&self.slots[range]) {
                if theirs.entry != 0 {
                    *mine = Slot {
                        key: theirs.key,
                        entry: entry((region.base + theirs.entry - 1) as usize)? + 1,
                    };
                }
            }
        }
        Ok(Self {
            signed: self.signed,
            shift: self.shift,
            regions: self
                .regions
                .iter()
                .map(|region| Region { base: 0, ..*region })
                .collect(),
            slots,
        })
    }

    /// Lays `keys` out with the row each came with. `None` when there are
    /// more keys than entries can number or a key is outside the type
    /// `signed` names.
    pub(super) fn lay_out<R: Copy + Default + Send + Sync>(
        keys: &[i128],
        rows: &[R],
        signed: bool,
    ) -> Option<SparseLayout<R>> {
        if keys.len() != rows.len() || u32::try_from(keys.len()).is_err() {
            return None;
        }
        let count = (keys.len() / REGION_KEYS)
            .max(1)
            .next_power_of_two()
            .min(MAX_REGIONS);
        let shift = 64 - count.trailing_zeros();
        let region_of = |hash: u64| {
            usize::try_from(hash.checked_shr(shift).unwrap_or(0)).expect("a region index")
        };
        // Keys and rows gathered region by region, each region's in the
        // order they were read.
        let mut ends = vec![0_usize; count];
        let mut bits = Vec::with_capacity(keys.len());
        for key in keys {
            let key = key_bits(*key, signed)?;
            ends[region_of(mix64(key))] += 1;
            bits.push(key);
        }
        let mut next = 0_usize;
        let sizes = ends.clone();
        for end in &mut ends {
            let size = *end;
            *end = next;
            next += size;
        }
        let mut region_keys = vec![0_u64; keys.len()];
        let mut region_rows = vec![R::default(); keys.len()];
        for (key, row) in bits.iter().zip(rows) {
            let cursor = &mut ends[region_of(mix64(*key))];
            region_keys[*cursor] = *key;
            region_rows[*cursor] = *row;
            *cursor += 1;
        }
        drop(bits);
        let mut regions = Vec::with_capacity(count);
        let mut offset = 0_usize;
        for size in &sizes {
            let capacity = size.saturating_mul(2).next_power_of_two().max(2);
            regions.push(Region {
                offset,
                mask: capacity - 1,
                base: 0,
            });
            offset += capacity;
        }
        let mut slots = vec![Slot::default(); offset];
        let mut placed = vec![R::default(); keys.len()];
        // Every region gets its own stretch of the slots and of the rows.
        let mut work = Vec::with_capacity(count);
        let (mut slots_left, mut placed_left) = (slots.as_mut_slice(), placed.as_mut_slice());
        let (mut keys_left, mut rows_left) = (region_keys.as_slice(), region_rows.as_slice());
        for (region, size) in regions.iter().zip(&sizes) {
            let (region_slots, rest) = slots_left.split_at_mut(region.mask + 1);
            slots_left = rest;
            let (region_placed, rest) = placed_left.split_at_mut(*size);
            placed_left = rest;
            let (keys, rest) = keys_left.split_at(*size);
            keys_left = rest;
            let (rows, rest) = rows_left.split_at(*size);
            rows_left = rest;
            work.push((region_slots, region_placed, keys, rows));
        }
        let entry_sizes = work
            .into_par_iter()
            .map(|(slots, placed, keys, rows)| lay_out_region(slots, placed, keys, rows))
            .collect::<Vec<_>>();
        let mut starts = Vec::with_capacity(keys.len() + 1);
        let mut row = 0_usize;
        for (region, sizes) in regions.iter_mut().zip(&entry_sizes) {
            region.base = u32::try_from(starts.len()).ok()?;
            for size in sizes {
                starts.push(row);
                row += *size as usize;
            }
        }
        starts.push(row);
        Some(SparseLayout {
            table: Self {
                signed,
                shift,
                regions,
                slots,
            },
            starts,
            rows: placed,
        })
    }
}

/// Fills one region's slots from its keys and writes its rows entry after
/// entry: each entry's row count, in entry order.
fn lay_out_region<R: Copy>(
    slots: &mut [Slot],
    placed: &mut [R],
    keys: &[u64],
    rows: &[R],
) -> Vec<u32> {
    let mask = slots.len() - 1;
    let mut sizes = Vec::<u32>::new();
    let mut entries = Vec::with_capacity(keys.len());
    for key in keys {
        #[allow(clippy::cast_possible_truncation)] // masked to the region
        let mut index = mix64(*key) as usize & mask;
        let entry = loop {
            let slot = &mut slots[index];
            if slot.entry == 0 {
                sizes.push(0);
                *slot = Slot {
                    key: *key,
                    entry: u32::try_from(sizes.len()).expect("a region's keys fit u32"),
                };
                break sizes.len() - 1;
            }
            if slot.key == *key {
                break slot.entry as usize - 1;
            }
            index = (index + 1) & mask;
        };
        sizes[entry] += 1;
        entries.push(entry);
    }
    let mut cursors = Vec::with_capacity(sizes.len());
    let mut next = 0_usize;
    for size in &sizes {
        cursors.push(next);
        next += *size as usize;
    }
    for (entry, row) in entries.iter().zip(rows) {
        placed[cursors[*entry]] = *row;
        cursors[*entry] += 1;
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::SparseKeyTable;

    fn spread(index: u64) -> i128 {
        i128::from(index.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1)
    }

    #[test]
    fn every_key_finds_its_rows_in_the_order_they_were_read() {
        // Enough keys for several regions, every third one repeated.
        let mut keys = Vec::new();
        let mut rows = Vec::new();
        for index in 0..60_000_u64 {
            keys.push(spread(index));
            rows.push(u32::try_from(index).expect("small"));
            if index % 3 == 0 {
                keys.push(spread(index));
                rows.push(u32::try_from(index + 1_000_000).expect("small"));
            }
        }
        let layout = SparseKeyTable::lay_out(&keys, &rows, true).expect("laid out");
        assert_eq!(layout.starts.len(), 60_001);
        assert_eq!(layout.rows.len(), rows.len());
        let mut seen = vec![false; 60_000];
        for index in 0..60_000_u64 {
            let entry = layout.table.find(spread(index)).expect("present");
            assert!(
                !std::mem::replace(&mut seen[entry], true),
                "one entry per key"
            );
            let found = &layout.rows[layout.starts[entry]..layout.starts[entry + 1]];
            let id = u32::try_from(index).expect("small");
            if index % 3 == 0 {
                assert_eq!(found, [id, id + 1_000_000]);
            } else {
                assert_eq!(found, [id]);
            }
        }
        assert_eq!(layout.table.find(spread(60_001)), None);
        assert_eq!(layout.table.find(-1), None);
        assert_eq!(layout.table.find(i128::from(u64::MAX)), None);
    }

    #[test]
    fn signed_and_unsigned_keys_only_match_inside_their_type() {
        let signed = SparseKeyTable::lay_out(&[-5, 7, i128::from(i64::MIN)], &[0_u8, 1, 2], true)
            .expect("laid out");
        assert!(signed.table.find(-5).is_some());
        assert!(signed.table.find(i128::from(i64::MIN)).is_some());
        // The unsigned value with -5's bit pattern is another number.
        assert_eq!(
            signed.table.find(i128::from((-5_i64).cast_unsigned())),
            None
        );
        let unsigned =
            SparseKeyTable::lay_out(&[i128::from(u64::MAX), 7], &[0_u8, 1], false).expect("laid");
        assert!(unsigned.table.find(i128::from(u64::MAX)).is_some());
        assert_eq!(unsigned.table.find(-1), None);
        assert!(SparseKeyTable::lay_out(&[-1], &[0_u8], false).is_none());
    }

    #[test]
    fn a_remapped_table_answers_with_the_new_entries() {
        let keys = (0..20_000_u64).map(spread).collect::<Vec<_>>();
        let rows = vec![0_u8; keys.len()];
        let layout = SparseKeyTable::lay_out(&keys, &rows, true).expect("laid out");
        let doubled = layout
            .table
            .remapped(|entry| Ok::<_, ()>(u32::try_from(entry * 2).expect("small")))
            .expect("remapped");
        for key in &keys {
            assert_eq!(
                doubled.find(*key),
                layout.table.find(*key).map(|entry| entry * 2)
            );
        }
        assert_eq!(doubled.find(spread(20_001)), None);
    }
}

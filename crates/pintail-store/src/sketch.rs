//! Distinct-value sketches kept with each segment's column statistics.
//!
//! A sketch is a fixed array of small registers. Every value is hashed; the
//! hash's low bits pick a register and the register keeps the longest run of
//! leading zeros seen in the rest. Many distinct values make long runs
//! likely, so the registers together estimate how many distinct values
//! went in, within a few tens of percent, in a size that does not grow with
//! the data. A segment column's sketch is the merge of the registers each of
//! its blocks already stores, so keeping it costs no extra hashing. Two sketches combine by keeping each register's larger value,
//! which is what lets segments written at different times, and the rows not
//! yet flushed, answer for the whole table without rereading anything.

use pintail_types::Value;
use xxhash_rust::xxh3::xxh3_64;

/// Registers per sketch.
pub(crate) const SKETCH_REGISTERS: usize = 64;
const INDEX_BITS: u32 = SKETCH_REGISTERS.trailing_zeros();

/// A mergeable estimate of how many distinct values a column holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DistinctSketch {
    registers: [u8; SKETCH_REGISTERS],
}

impl Default for DistinctSketch {
    fn default() -> Self {
        Self {
            registers: [0; SKETCH_REGISTERS],
        }
    }
}

impl DistinctSketch {
    pub(crate) const fn from_registers(registers: [u8; SKETCH_REGISTERS]) -> Self {
        Self { registers }
    }

    pub(crate) const fn registers(&self) -> &[u8; SKETCH_REGISTERS] {
        &self.registers
    }

    /// Counts one value; NULL is not a value and is skipped.
    pub(crate) fn insert(&mut self, value: &Value) {
        if let Some(hash) = value_hash(value) {
            self.insert_hash(hash);
        }
    }

    fn insert_hash(&mut self, hash: u64) {
        let index = usize::try_from(hash & (SKETCH_REGISTERS as u64 - 1)).unwrap_or(0);
        // The remaining bits start at bit `INDEX_BITS`; the rank is one more
        // than their leading zeros, so an all-zero remainder ranks highest.
        let rest = hash >> INDEX_BITS;
        let rank = u8::try_from(rest.leading_zeros() - INDEX_BITS + 1).unwrap_or(u8::MAX);
        self.registers[index] = self.registers[index].max(rank);
    }

    /// Folds another sketch into this one.
    pub(crate) fn merge(&mut self, other: &Self) {
        for (mine, theirs) in self.registers.iter_mut().zip(other.registers) {
            *mine = (*mine).max(theirs);
        }
    }

    /// The estimated number of distinct values counted.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn estimate(&self) -> u64 {
        let registers = SKETCH_REGISTERS as f64;
        let empty: usize = self
            .registers
            .iter()
            .map(|rank| usize::from(*rank == 0))
            .sum();
        if empty == SKETCH_REGISTERS {
            return 0;
        }
        let harmonic: f64 = self
            .registers
            .iter()
            .map(|rank| (-f64::from(*rank)).exp2())
            .sum();
        // The bias constant for this register count.
        let raw = 0.709 * registers * registers / harmonic;
        let estimate = if raw <= 2.5 * registers && empty > 0 {
            // With registers still empty, how many are empty is the better
            // guide: it counts small sets almost exactly.
            registers * (registers / empty as f64).ln()
        } else {
            raw
        };
        estimate.round().max(1.0) as u64
    }
}

/// A value's hash for counting distinct values: `xxh3` of the bytes a
/// segment block's statistics hash for it, so unflushed rows land in the
/// same registers as the flushed ones. A column a segment stores in native
/// units (dates, decimals) hashes those units there and its text here;
/// the only effect is that the few sampled unflushed rows can count again.
fn value_hash(value: &Value) -> Option<u64> {
    let text = |bytes: &[u8]| {
        let mut framed = Vec::with_capacity(bytes.len() + 4);
        framed.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
        framed.extend_from_slice(bytes);
        xxh3_64(&framed)
    };
    match value {
        Value::Null => None,
        Value::Boolean(value) => Some(xxh3_64(&[u8::from(*value)])),
        Value::Int64(value) => Some(xxh3_64(&value.to_le_bytes())),
        Value::UInt64(value) => Some(xxh3_64(&value.to_le_bytes())),
        Value::Float64(value) => Some(xxh3_64(&value.key_bits().to_le_bytes())),
        Value::Utf8(value) => Some(text(value.as_bytes())),
        Value::Binary(bytes) => Some(text(bytes)),
        other => other.text().map(|value| text(value.as_bytes())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sketch_of(values: impl IntoIterator<Item = Value>) -> DistinctSketch {
        let mut sketch = DistinctSketch::default();
        for value in values {
            sketch.insert(&value);
        }
        sketch
    }

    #[test]
    fn small_sets_count_almost_exactly() {
        assert_eq!(DistinctSketch::default().estimate(), 0);
        assert_eq!(sketch_of([Value::Int64(7), Value::Int64(7)]).estimate(), 1);
        let ten = sketch_of((0..1_000).map(|value| Value::Int64(value % 10)));
        assert!((9..=11).contains(&ten.estimate()), "{}", ten.estimate());
        assert_eq!(sketch_of([Value::Null]).estimate(), 0);
    }

    #[test]
    fn large_sets_land_within_a_third() {
        for distinct in [1_000_u64, 50_000, 400_000] {
            let sketch = sketch_of((0..distinct * 2).map(|value| Value::UInt64(value % distinct)));
            let estimate = sketch.estimate();
            assert!(
                estimate > distinct * 2 / 3 && estimate < distinct * 4 / 3,
                "{distinct}: {estimate}"
            );
            let text = sketch_of((0..distinct).map(|value| Value::Utf8(format!("v{value}"))));
            let estimate = text.estimate();
            assert!(
                estimate > distinct * 2 / 3 && estimate < distinct * 4 / 3,
                "text {distinct}: {estimate}"
            );
        }
    }

    #[test]
    fn merging_counts_the_union() {
        let mut left = sketch_of((0..5_000).map(Value::Int64));
        let right = sketch_of((2_500..7_500).map(Value::Int64));
        left.merge(&right);
        let estimate = left.estimate();
        assert!((5_000..10_000).contains(&estimate), "{estimate}");
    }
}

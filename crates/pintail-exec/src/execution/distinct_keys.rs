//! Compact keys for a DISTINCT set.
//!
//! A DISTINCT set keyed every text value as a `Value` holding the hex of
//! its collation sort key, and a DATETIME or DECIMAL column reached it as
//! text: per row the value was cloned or formatted, weighed by the
//! collator, hex-encoded, hashed and allocated. `COUNT(DISTINCT)` over two
//! million datetimes spent 1.4 µs a row there.
//!
//! Two key forms replace that, each an exact stand-in for the normalized
//! value, so a set can move to normalized values at any point - when a
//! value arrives that has no compact form, when two sets merge, when a
//! state spills - and hold what it would have held all along:
//!
//! - [`UnitKind`]: a DECIMAL, DATE or DATETIME column's packed units. Two
//!   values written canonically at the column's own scale are the same
//!   text exactly when their units are equal, so the units are the key. A
//!   value that arrives as text is read back to units when it is canonical
//!   and otherwise sends the set to normalized values.
//! - [`TextKeys`]: a text value's collation weight, without the `Value`
//!   and the hex. Under `utf8mb4_0900_ai_ci` a printable ASCII string
//!   weighs as its characters' weights in sequence, so the key is the
//!   string with every character replaced by the first printable ASCII
//!   character of the same weight: one table lookup a byte. The first
//!   value outside printable ASCII moves the set to sort-key bytes.

use std::collections::HashSet;
use std::hash::BuildHasherDefault;

use pintail_types::DataType;

use super::join::{collation_sort_key, normalized_collation_text};
use crate::collation::Collation;

/// What a DISTINCT set's integer keys stand for when they are a column's
/// packed units rather than integers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnitKind {
    /// Units of `10^-scale`.
    Decimal { scale: u8 },
    /// Days.
    Date,
    /// Units of `10^-fsp` seconds: the column's microseconds without the
    /// digits its precision never holds.
    DateTime { fsp: u8 },
}

impl UnitKind {
    /// The kind a plain column of `data_type` keys by, if it has one.
    pub(crate) const fn of_type(data_type: DataType) -> Option<Self> {
        match data_type {
            DataType::Decimal { scale, .. } => Some(Self::Decimal { scale }),
            DataType::Date32 => Some(Self::Date),
            DataType::DateTime64 { fsp } if fsp <= 6 => Some(Self::DateTime { fsp }),
            _ => None,
        }
    }

    /// Microseconds per key unit of a DATETIME at `fsp`.
    fn datetime_step(fsp: u8) -> i64 {
        10_i64.pow(u32::from(6_u8.saturating_sub(fsp)))
    }

    /// Packed units per key unit: microseconds per `10^-fsp` seconds for a
    /// DATETIME, one for the other kinds.
    pub(crate) fn step(self) -> i64 {
        match self {
            Self::DateTime { fsp } => Self::datetime_step(fsp),
            Self::Decimal { .. } | Self::Date => 1,
        }
    }

    /// The key of a value whose packed units are `units`.
    #[inline]
    pub(crate) fn key_of_units(self, units: i128) -> i128 {
        match self {
            Self::DateTime { fsp } => units.div_euclid(i128::from(Self::datetime_step(fsp))),
            Self::Decimal { .. } | Self::Date => units,
        }
    }

    /// The key of a value that arrived as text, when the text is exactly
    /// what its units format to; `None` for any other spelling, which is a
    /// different value to a set that compares text.
    pub(crate) fn key_of_text(self, text: &str) -> Option<i128> {
        match self {
            Self::Decimal { scale } => crate::batch::canonical_decimal_text(text, scale)
                .then(|| crate::batch::parse_decimal_scaled(text, scale))
                .flatten(),
            Self::Date => {
                let days = crate::batch::parse_date_days(text)?;
                (pintail_types::format_date_days(days)? == text).then_some(i128::from(days))
            }
            Self::DateTime { fsp } => {
                let micros = crate::batch::parse_datetime_micros(text)?;
                (pintail_types::format_datetime_micros(micros, fsp)? == text)
                    .then(|| self.key_of_units(i128::from(micros)))
            }
        }
    }

    /// The canonical text of the value `key` stands for.
    pub(crate) fn text_of_key(self, key: i128) -> Option<String> {
        match self {
            Self::Decimal { scale } => Some(pintail_types::format_decimal_scaled(key, scale)),
            Self::Date => pintail_types::format_date_days(i64::try_from(key).ok()?),
            Self::DateTime { fsp } => pintail_types::format_datetime_micros(
                i64::try_from(key)
                    .ok()?
                    .checked_mul(Self::datetime_step(fsp))?,
                fsp,
            ),
        }
    }
}

/// The hash of a weight-byte key: per-query column data in a private set,
/// so a keyed hash's flood resistance buys nothing.
fn hash_key_bytes(bytes: &[u8]) -> u64 {
    const STEP: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut state = (bytes.len() as u64).wrapping_mul(STEP);
    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        state = (state.rotate_left(23) ^ word).wrapping_mul(STEP);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut word = [0_u8; 8];
        word[..rest.len()].copy_from_slice(rest);
        state = (state.rotate_left(23) ^ u64::from_le_bytes(word)).wrapping_mul(STEP);
    }
    crate::batch::mix64(state)
}

/// Bytes a key holds in its own entry.
const INLINE_KEY_BYTES: usize = 22;

/// A key's bytes: in the entry when short, on the heap otherwise.
#[derive(Clone)]
enum KeyBytes {
    Inline {
        len: u8,
        bytes: [u8; INLINE_KEY_BYTES],
    },
    Heap(Box<[u8]>),
}

/// One key with its hash beside it. Growing the set rehashes every entry;
/// with the hash stored that reads the entries alone, where hashing the
/// bytes again fetched each key's allocation - most of what a set of
/// millions of distinct strings cost.
#[derive(Clone)]
struct TextKey {
    hash: u64,
    bytes: KeyBytes,
}

impl TextKey {
    fn new(hash: u64, bytes: &[u8]) -> Self {
        let bytes = match u8::try_from(bytes.len()) {
            Ok(len) if bytes.len() <= INLINE_KEY_BYTES => {
                let mut inline = [0_u8; INLINE_KEY_BYTES];
                inline[..bytes.len()].copy_from_slice(bytes);
                KeyBytes::Inline { len, bytes: inline }
            }
            _ => KeyBytes::Heap(Box::from(bytes)),
        };
        Self { hash, bytes }
    }

    /// Bytes this key holds outside its entry.
    fn heap_bytes(&self) -> usize {
        match &self.bytes {
            KeyBytes::Inline { .. } => 0,
            KeyBytes::Heap(bytes) => bytes.len(),
        }
    }
}

/// A key as the set compares it: a stored entry, or the bytes being looked
/// up with their hash, so a lookup builds no entry.
trait Keyed {
    fn hash_value(&self) -> u64;
    fn key_bytes(&self) -> &[u8];
}

impl Keyed for TextKey {
    fn hash_value(&self) -> u64 {
        self.hash
    }

    fn key_bytes(&self) -> &[u8] {
        match &self.bytes {
            KeyBytes::Inline { len, bytes } => &bytes[..usize::from(*len)],
            KeyBytes::Heap(bytes) => bytes,
        }
    }
}

struct Probe<'a> {
    hash: u64,
    bytes: &'a [u8],
}

impl Keyed for Probe<'_> {
    fn hash_value(&self) -> u64 {
        self.hash
    }

    fn key_bytes(&self) -> &[u8] {
        self.bytes
    }
}

impl<'a> std::borrow::Borrow<dyn Keyed + 'a> for TextKey {
    fn borrow(&self) -> &(dyn Keyed + 'a) {
        self
    }
}

impl std::hash::Hash for dyn Keyed + '_ {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash_value());
    }
}

impl PartialEq for dyn Keyed + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.hash_value() == other.hash_value() && self.key_bytes() == other.key_bytes()
    }
}

impl Eq for dyn Keyed + '_ {}

impl std::hash::Hash for TextKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.hash);
    }
}

impl PartialEq for TextKey {
    fn eq(&self, other: &Self) -> bool {
        self.hash == other.hash && self.key_bytes() == other.key_bytes()
    }
}

impl Eq for TextKey {}

/// Hands a key's stored hash to the set.
#[derive(Default)]
pub(super) struct TextKeyHasher(u64);

impl std::hash::Hasher for TextKeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _bytes: &[u8]) {
        unreachable!("text keys hash through write_u64");
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = value;
    }
}

/// How a [`TextKeys`] set spells its keys.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TextForm {
    /// Printable ASCII under `utf8mb4_0900_ai_ci`, each character replaced
    /// by the first printable ASCII character of the same weight.
    Folded,
    /// The collation's sort-key bytes.
    SortKey,
}

/// Each printable ASCII byte's stand-in under `collation` (zero for every
/// other byte), when the collation weighs printable ASCII strings
/// character by character.
fn ascii_fold(collation: Collation) -> Option<&'static [u8; 256]> {
    static AI_CI: std::sync::OnceLock<Option<[u8; 256]>> = std::sync::OnceLock::new();
    if collation != Collation::Utf8mb40900AiCi {
        return None;
    }
    AI_CI
        .get_or_init(|| {
            let ranks = super::ungrouped_fold::printable_ascii_ranks(collation)?;
            let mut first_of_rank = [0_u8; 128];
            let mut fold = [0_u8; 256];
            for byte in 0x20_u8..0x7f {
                let slot = &mut first_of_rank[usize::from(ranks[usize::from(byte)])];
                if *slot == 0 {
                    *slot = byte;
                }
                fold[usize::from(byte)] = *slot;
            }
            Some(fold)
        })
        .as_ref()
}

/// A DISTINCT set of text values, keyed by collation weight.
#[derive(Clone)]
pub(super) struct TextKeys {
    set: HashSet<TextKey, BuildHasherDefault<TextKeyHasher>>,
    form: TextForm,
    /// The key being built, kept to reuse its allocation.
    scratch: Vec<u8>,
    /// A batch's keys waiting for [`Self::flush`]: their bytes end to end,
    /// and where each one lies.
    arena: Vec<u8>,
    staged: Vec<Staged>,
    /// The second buffer the batch is reordered through.
    reordered: Vec<Staged>,
    /// Keys a batch added before the rest of it was flushed.
    staged_added: u64,
}

/// One staged key: its hash and its bytes' place in the arena.
#[derive(Clone, Copy)]
struct Staged {
    hash: u64,
    start: usize,
    len: usize,
}

/// Members a set holds before a batch of keys is put in table order.
const ORDERED_INSERT_MIN_MEMBERS: usize = 1 << 14;

/// Bytes a key's set entry is charged beside its own length.
const TEXT_KEY_ENTRY: usize = size_of::<TextKey>() + super::HASH_ENTRY_OVERHEAD;

impl TextKeys {
    pub(super) fn new(collation: Collation) -> Self {
        Self {
            set: HashSet::default(),
            form: if ascii_fold(collation).is_some() {
                TextForm::Folded
            } else {
                TextForm::SortKey
            },
            scratch: Vec::new(),
            arena: Vec::new(),
            staged: Vec::new(),
            reordered: Vec::new(),
            staged_added: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.set.len()
    }

    /// Holds `text`'s key for the next [`Self::flush`], which must come
    /// before the set is read or changed any other way.
    pub(super) fn stage(
        &mut self,
        text: &str,
        collation: Collation,
        memory: &super::MemoryTracker,
    ) -> Result<(), super::ExecError> {
        let start = self.arena.len();
        if self.form == TextForm::Folded {
            let fold = ascii_fold(collation).expect("a folded set's collation folds");
            let mut folds = true;
            self.arena.extend(text.bytes().map(|byte| {
                let folded = fold[usize::from(byte)];
                folds &= folded != 0;
                folded
            }));
            if !folds {
                // The keys staged so far are folded ones: they go in
                // before the set respells what it holds.
                self.arena.truncate(start);
                self.staged_added += self.insert_staged(memory)?;
                self.leave_folded(collation, memory)?;
                return self.stage(text, collation, memory);
            }
        } else {
            self.arena
                .extend_from_slice(&collation_sort_key(text, collation));
        }
        self.staged.push(Staged {
            hash: hash_key_bytes(&self.arena[start..]),
            start,
            len: self.arena.len() - start,
        });
        Ok(())
    }

    /// Inserts every staged key, answering how many keys the batch added.
    pub(super) fn flush(&mut self, memory: &super::MemoryTracker) -> Result<u64, super::ExecError> {
        let added = self.insert_staged(memory)?;
        Ok(added + std::mem::take(&mut self.staged_added))
    }

    /// Inserts the staged keys - in table order once the set is large (see
    /// `order_by_table_position`) - and answers how many were new.
    fn insert_staged(&mut self, memory: &super::MemoryTracker) -> Result<u64, super::ExecError> {
        if self.set.len() >= ORDERED_INSERT_MIN_MEMBERS {
            super::reserve_hash_set_entries(
                &mut self.set,
                self.staged.len(),
                TEXT_KEY_ENTRY,
                0,
                memory,
            )?;
            super::aggregate::order_by_table_position(
                &mut self.staged,
                &mut self.reordered,
                self.set.capacity(),
                |staged| staged.hash,
            );
        }
        let mut added = 0_u64;
        for index in 0..self.staged.len() {
            let Staged { hash, start, len } = self.staged[index];
            let bytes = &self.arena[start..start + len];
            if self.set.contains(&Probe { hash, bytes } as &dyn Keyed) {
                continue;
            }
            let key = TextKey::new(hash, bytes);
            self.insert_new(key, memory)?;
            added += 1;
        }
        self.staged.clear();
        self.arena.clear();
        Ok(added)
    }

    /// Inserts `text`, answering whether it was new.
    pub(super) fn insert(
        &mut self,
        text: &str,
        collation: Collation,
        memory: &super::MemoryTracker,
    ) -> Result<bool, super::ExecError> {
        self.scratch.clear();
        if self.form == TextForm::Folded {
            let fold = ascii_fold(collation).expect("a folded set's collation folds");
            let mut folds = true;
            self.scratch.extend(text.bytes().map(|byte| {
                let folded = fold[usize::from(byte)];
                folds &= folded != 0;
                folded
            }));
            if !folds {
                self.leave_folded(collation, memory)?;
                self.scratch.clear();
            }
        }
        if self.form == TextForm::SortKey {
            self.scratch = collation_sort_key(text, collation);
        }
        self.insert_scratch(memory)
    }

    fn insert_scratch(&mut self, memory: &super::MemoryTracker) -> Result<bool, super::ExecError> {
        let hash = hash_key_bytes(&self.scratch);
        let probe = Probe {
            hash,
            bytes: &self.scratch,
        };
        if self.set.contains(&probe as &dyn Keyed) {
            return Ok(false);
        }
        let key = TextKey::new(hash, &self.scratch);
        self.insert_new(key, memory)?;
        Ok(true)
    }

    /// Inserts a key the set does not hold.
    fn insert_new(
        &mut self,
        key: TextKey,
        memory: &super::MemoryTracker,
    ) -> Result<(), super::ExecError> {
        super::reserve_hash_set_entries(&mut self.set, 1, TEXT_KEY_ENTRY, 0, memory)?;
        memory.reserve(key.heap_bytes())?;
        self.set.insert(key);
        Ok(())
    }

    /// Respells folded keys as sort keys: a folded key is a string equal,
    /// under the collation, to every value it stands for, so its sort key
    /// is theirs.
    fn leave_folded(
        &mut self,
        collation: Collation,
        memory: &super::MemoryTracker,
    ) -> Result<(), super::ExecError> {
        let folded = std::mem::take(&mut self.set);
        self.form = TextForm::SortKey;
        for key in folded {
            let text =
                std::str::from_utf8(key.key_bytes()).expect("folded keys are printable ASCII");
            let weights = collation_sort_key(text, collation);
            let key = TextKey::new(hash_key_bytes(&weights), &weights);
            if !self.set.contains(&key) {
                self.insert_new(key, memory)?;
            }
        }
        Ok(())
    }

    /// Takes every key of `other`, a set under the same collation,
    /// answering how many were new.
    pub(super) fn union(
        &mut self,
        mut other: Self,
        collation: Collation,
        memory: &super::MemoryTracker,
    ) -> Result<u64, super::ExecError> {
        if self.form != other.form {
            if self.form == TextForm::Folded {
                self.leave_folded(collation, memory)?;
            } else {
                other.leave_folded(collation, memory)?;
            }
        }
        let mut added = 0_u64;
        for key in other.set {
            if self.set.contains(&key) {
                continue;
            }
            self.insert_new(key, memory)?;
            added += 1;
        }
        Ok(added)
    }

    /// Every key as the normalized text a set of values holds for it.
    pub(super) fn into_normalized(self, collation: Collation) -> impl Iterator<Item = String> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let form = self.form;
        self.set.into_iter().map(move |key| match form {
            TextForm::Folded => normalized_collation_text(
                std::str::from_utf8(key.key_bytes()).expect("folded keys are printable ASCII"),
                collation,
            ),
            TextForm::SortKey => {
                let key = key.key_bytes();
                let mut encoded = String::with_capacity(key.len().saturating_mul(2));
                for byte in key {
                    encoded.push(char::from(HEX[usize::from(byte >> 4)]));
                    encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
                }
                encoded
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Collation, TextKeys, UnitKind, normalized_collation_text};
    use crate::execution::MemoryTracker;

    /// Whatever mix of values a set takes, and whenever it leaves the
    /// folded form, it holds exactly the normalized texts a set of values
    /// would hold.
    #[test]
    fn text_keys_stand_for_the_normalized_values() {
        let memory = MemoryTracker::new(1 << 30);
        let words = [
            "amber",
            "Amber",
            "AMBER",
            "amber ",
            "ámber",
            "a-b",
            "a b",
            "A B",
            "",
            " ",
            "zinc",
            "Zínc",
            "ZINC",
            "x~y",
            "x~Y",
            "tab\there",
            "naïve",
            "NAIVE",
            "naive",
            "東京",
            "Ω",
        ];
        for collation in [
            Collation::Utf8mb40900AiCi,
            Collation::Utf8mb4Bin,
            Collation::Latin1SwedishCi,
            Collation::Utf8mb4GeneralCi,
            Collation::Utf8mb40900AsCs,
        ] {
            for split in 0..=words.len() {
                // Rotating the list moves the first non-ASCII value, and
                // so the point where a folded set respells its keys.
                let mut rotated = words.to_vec();
                rotated.rotate_left(split);
                let mut keys = TextKeys::new(collation);
                let mut expected = std::collections::BTreeSet::new();
                for word in &rotated {
                    let new = keys.insert(word, collation, &memory).expect("insert");
                    assert_eq!(
                        new,
                        expected.insert(normalized_collation_text(word, collation)),
                        "{word:?} under {collation:?}"
                    );
                }
                assert_eq!(keys.len(), expected.len());
                let held: std::collections::BTreeSet<String> =
                    keys.into_normalized(collation).collect();
                assert_eq!(held, expected, "{collation:?} rotated by {split}");
            }
        }
    }

    /// Batches staged and flushed - small ones, ones past the size where
    /// keys go in table order, and one that meets its first non-ASCII
    /// value part way through - count what inserting one at a time counts.
    #[test]
    fn staged_batches_count_what_single_inserts_count() {
        let memory = MemoryTracker::new(1 << 30);
        for collation in [Collation::Utf8mb40900AiCi, Collation::Utf8mb4Bin] {
            let mut staged = TextKeys::new(collation);
            let mut single = TextKeys::new(collation);
            let mut total = 0_u64;
            let mut seed = 7_u64;
            for batch in 0..12_u64 {
                let mut expected = 0_u64;
                for _ in 0..9_000 {
                    seed = crate::batch::mix64(seed);
                    let stem = seed % 60_000;
                    let word = match (seed >> 40) % 4 {
                        0 => format!("Kite-{stem:05}"),
                        1 => format!("kite-{stem:05}"),
                        2 if batch >= 7 => format!("kíte-{stem:05}"),
                        _ => format!("KITE-{stem:05} "),
                    };
                    staged.stage(&word, collation, &memory).expect("stage");
                    expected +=
                        u64::from(single.insert(&word, collation, &memory).expect("insert"));
                }
                let added = staged.flush(&memory).expect("flush");
                assert_eq!(added, expected, "{collation:?} batch {batch}");
                total += added;
            }
            assert_eq!(staged.len() as u64, total);
            let left: std::collections::BTreeSet<String> =
                staged.into_normalized(collation).collect();
            let right: std::collections::BTreeSet<String> =
                single.into_normalized(collation).collect();
            assert_eq!(left, right, "{collation:?}");
        }
    }

    /// Two sets in different forms merge into what one set would hold.
    #[test]
    fn text_keys_union_across_forms() {
        let memory = MemoryTracker::new(1 << 30);
        let collation = Collation::Utf8mb40900AiCi;
        let mut ascii = TextKeys::new(collation);
        for word in ["amber", "birch", "Cedar"] {
            ascii.insert(word, collation, &memory).expect("insert");
        }
        let mut accented = TextKeys::new(collation);
        for word in ["ámber", "CEDAR", "delta", "écho"] {
            accented.insert(word, collation, &memory).expect("insert");
        }
        let added = ascii
            .clone()
            .union(accented.clone(), collation, &memory)
            .expect("union");
        assert_eq!(added, 2, "delta and écho are new; ámber and CEDAR are not");
        let added = accented.union(ascii, collation, &memory).expect("union");
        assert_eq!(added, 1, "only birch is new the other way round");
    }

    /// A unit key reads back from exactly the text its units format to.
    #[test]
    fn unit_keys_round_trip_canonical_text_only() {
        let decimal = UnitKind::Decimal { scale: 2 };
        assert_eq!(decimal.key_of_text("-12.50"), Some(-1250));
        assert_eq!(decimal.text_of_key(-1250).as_deref(), Some("-12.50"));
        for spelling in ["-12.5", "012.50", "12.500", "-0.00", " 1.00", "1e2"] {
            assert_eq!(decimal.key_of_text(spelling), None, "{spelling:?}");
        }
        let seconds = UnitKind::DateTime { fsp: 0 };
        let key = seconds
            .key_of_text("2024-02-29 23:59:59")
            .expect("canonical");
        assert_eq!(
            seconds.text_of_key(key).as_deref(),
            Some("2024-02-29 23:59:59")
        );
        assert_eq!(seconds.key_of_text("2024-02-29 23:59:59.5"), None);
        assert_eq!(seconds.key_of_text("2024-02-29"), None);
        let millis = UnitKind::DateTime { fsp: 3 };
        let key = millis
            .key_of_text("2024-02-29 23:59:59.250")
            .expect("canonical");
        assert_eq!(
            millis.text_of_key(key).as_deref(),
            Some("2024-02-29 23:59:59.250")
        );
        assert_eq!(millis.key_of_units(1_250_999), 1_250);
        assert_eq!(millis.key_of_text("2024-02-29 23:59:59"), None);
        let date = UnitKind::Date;
        let key = date.key_of_text("1999-12-31").expect("canonical");
        assert_eq!(date.text_of_key(key).as_deref(), Some("1999-12-31"));
        assert_eq!(date.key_of_text("1999-12-31 00:00:00"), None);
    }
}

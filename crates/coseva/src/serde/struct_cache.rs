use core::mem;
use core::str::Utf8Error;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

use crate::byte_record::ByteRecord;

/// Identity of a `&'static` value, held as an address and length pair.
///
/// The cache only ever asks whether the current key is the same static as the
/// one it recorded, so the referent is never dereferenced. Comparing addresses
/// rather than contents is strictly conservative: distinct statics that happen
/// to hold equal contents are treated as different keys, which costs a cache
/// reset and never reuses one struct's learned set for another.
///
/// Atomics rather than `Cell` keep [`StructCache`] `Sync`, so enabling the
/// `serde` feature does not take `Sync` away from the parsers. Access is
/// logically single-threaded, so `Relaxed` is sufficient.
#[derive(Debug)]
struct AtomicKey {
    address: AtomicUsize,
    len: AtomicUsize,
}

impl AtomicKey {
    /// A key matching nothing; no static ever lives at address zero.
    const fn unset() -> Self {
        Self {
            address: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
        }
    }

    fn clear(&self) {
        self.store(0, 0);
    }

    fn store(&self, address: usize, len: usize) {
        self.address.store(address, Relaxed);
        self.len.store(len, Relaxed);
    }

    fn holds(&self, address: usize, len: usize) -> bool {
        self.address.load(Relaxed) == address && self.len.load(Relaxed) == len
    }

    fn set_str(&self, value: &'static str) {
        self.store(value.as_ptr() as usize, value.len());
    }

    fn holds_str(&self, value: &'static str) -> bool {
        self.holds(value.as_ptr() as usize, value.len())
    }

    fn set_slice(&self, value: &'static [&'static str]) {
        self.store(value.as_ptr() as usize, value.len());
    }

    fn holds_slice(&self, value: &'static [&'static str]) -> bool {
        self.holds(value.as_ptr() as usize, value.len())
    }
}

/// Per-parser cache for the header-aware Serde struct path.
///
/// Two things are cached across records:
///
/// * **Header names.** Header bytes are validated as UTF-8 once instead of on
///   every record. When a header is not valid UTF-8 the offending column and
///   its error are recorded so the original per-record error is reproduced
///   verbatim at the same point in the key sequence.
/// * **Ignored columns.** Serde asks for a column it does not want via
///   `deserialize_ignored_any`, which lets the deserializer observe exactly
///   which columns the visitor discards. Once a record deserializes
///   *successfully* those columns can be skipped entirely on later records.
///   Success is the critical precondition: a struct using
///   `#[serde(deny_unknown_fields)]` fails on the first unknown column, so
///   nothing is ever committed and every column keeps being yielded.
///   Columns matched via `#[serde(alias)]` are by definition not ignored, so
///   they are never skipped.
#[derive(Debug)]
pub(crate) struct StructCache {
    /// Header names validated as UTF-8, in CSV column order. Truncated at
    /// `invalid` when a header is not valid UTF-8.
    pub(super) names: Vec<Box<str>>,
    /// The first column whose header is not valid UTF-8, with its error.
    pub(super) invalid: Option<(usize, Utf8Error)>,
    /// Total header column count, including any invalid tail.
    pub(super) columns: usize,
    /// The struct whose ignored-column set `ignored` describes, identified by
    /// its `deserialize_struct` field-name slice.
    key_fields: AtomicKey,
    /// The struct type name paired with `key`. Two distinct structs can share
    /// an identical field-name list (which the compiler may even merge into a
    /// single `&'static` constant), so the type name is required to tell them
    /// apart and stop one struct's learned skip-set from being reused for
    /// another — most importantly a `#[serde(deny_unknown_fields)]` struct that
    /// must keep rejecting the columns a lax sibling learned to ignore.
    key_name: AtomicKey,
    /// Bit `i` set means column `i` was ignored by the visitor, for `i < 64`.
    ignored: AtomicU64,
    /// Ignored-column bits observed for the record currently in flight, for the
    /// first 64 columns.
    observing: AtomicU64,
    /// The learned and in-flight ignored-column bits for columns 64 and beyond.
    ///
    /// Allocated only when the header actually reaches past 64 columns, so an
    /// ordinary header keeps the single-word fast path and never allocates.
    /// `None` while the header fits in one word; otherwise one atomic word per
    /// 64 columns past the first. Kept atomic for the same reason `ignored` is:
    /// so [`StructCache`] stays `Sync` without a lock, with `Relaxed` access
    /// because it is logically single-threaded.
    wide: Option<WideBitsets>,
    /// Whether `ignored` reflects a completed, successful record.
    learned: AtomicBool,
    /// Whether a struct record is currently being observed.
    active: AtomicBool,
}

/// The learned and in-flight ignored-column bits for columns at or beyond 64.
///
/// One boxed run of atomic words per set, sized to the header once and never
/// resized while it stands. Bit `j` of word `w` is column `64 + w * 64 + j`.
#[derive(Debug)]
struct WideBitsets {
    /// Learned ignored columns beyond the first word.
    learned: Box<[AtomicU64]>,
    /// In-flight observed ignored columns beyond the first word.
    observing: Box<[AtomicU64]>,
}

impl WideBitsets {
    /// Allocate cleared bitsets covering `columns` total columns, which must be
    /// more than [`MAX_LEARNED_COLUMNS`].
    fn new(columns: usize) -> Self {
        let words = (columns - MAX_LEARNED_COLUMNS).div_ceil(MAX_LEARNED_COLUMNS);
        Self {
            learned: cleared_words(words),
            observing: cleared_words(words),
        }
    }
}

/// A boxed run of `words` zeroed atomic words, without going through a value
/// that would need `AtomicU64: Clone`.
fn cleared_words(words: usize) -> Box<[AtomicU64]> {
    let mut run = Vec::with_capacity(words);
    run.resize_with(words, || AtomicU64::new(0));
    run.into_boxed_slice()
}

/// Zero every word in a wide bitset.
fn clear_words(words: &[AtomicU64]) {
    for word in words {
        word.store(0, Relaxed);
    }
}

/// Columns 0 through 63 live in a single word; a header wider than this
/// allocates a [`WideBitsets`] so columns at every index can still be learned.
const MAX_LEARNED_COLUMNS: usize = 64;

/// Whether `column` is set in a learned single-word ignored-column mask.
///
/// Columns at 64 or beyond are never in the word and are resolved through
/// [`StructCache::wide_ignored`] instead.
pub(super) fn mask_contains(mask: u64, column: usize) -> bool {
    u32::try_from(column)
        .ok()
        .and_then(|shift| mask.checked_shr(shift))
        .is_some_and(|shifted| shifted & 1 != 0)
}

/// The word and bit within a wide bitset that hold `column`, for a column at or
/// beyond [`MAX_LEARNED_COLUMNS`].
const fn wide_position(column: usize) -> (usize, u64) {
    let offset = column - MAX_LEARNED_COLUMNS;
    (
        offset / MAX_LEARNED_COLUMNS,
        1 << (offset % MAX_LEARNED_COLUMNS),
    )
}

/// Whether `column` is set in a run of learned wide ignored-column words.
///
/// Returns `false` for the first 64 columns, which live in the single-word mask
/// instead, and for a header with no wide words at all.
pub(super) fn wide_contains(words: &[AtomicU64], column: usize) -> bool {
    column
        .checked_sub(MAX_LEARNED_COLUMNS)
        .is_some_and(|offset| {
            let word = offset / MAX_LEARNED_COLUMNS;
            let bit = 1 << (offset % MAX_LEARNED_COLUMNS);
            words
                .get(word)
                .is_some_and(|word| word.load(Relaxed) & bit != 0)
        })
}

impl StructCache {
    pub(crate) const fn new() -> Self {
        Self {
            names: Vec::new(),
            invalid: None,
            columns: 0,
            key_fields: AtomicKey::unset(),
            key_name: AtomicKey::unset(),
            ignored: AtomicU64::new(0),
            observing: AtomicU64::new(0),
            wide: None,
            learned: AtomicBool::new(false),
            active: AtomicBool::new(false),
        }
    }

    /// Rebuild the validated header names when the header record changes.
    pub(crate) fn sync(&mut self, headers: Option<&ByteRecord>) {
        let Some(headers) = headers else {
            self.reset();
            return;
        };

        if self.columns == headers.len()
            && self.invalid.is_none()
            && self
                .names
                .iter()
                .zip(headers.iter())
                .all(|(name, raw)| name.as_bytes() == raw)
        {
            return;
        }

        self.reset();
        self.columns = headers.len();
        // Only a header past the single-word boundary allocates a wide bitset,
        // so an ordinary header keeps the fast path and never allocates here.
        if self.columns > MAX_LEARNED_COLUMNS {
            self.wide = Some(WideBitsets::new(self.columns));
        }
        for (column, raw) in headers.iter().enumerate() {
            match str::from_utf8(raw) {
                Ok(name) => self.names.push(name.into()),
                Err(error) => {
                    self.invalid = Some((column, error));
                    break;
                }
            }
        }
    }

    pub(crate) fn reset(&mut self) {
        self.names.clear();
        self.invalid.take();
        let _ = mem::take(&mut self.columns);
        self.key_fields.clear();
        self.key_name.clear();
        self.ignored.store(0, Relaxed);
        self.observing.store(0, Relaxed);
        self.wide.take();
        self.learned.store(false, Relaxed);
        self.active.store(false, Relaxed);
    }

    /// Begin a struct record, returning whether learned columns may be skipped.
    ///
    /// Skipping is disabled while a header is not valid UTF-8 so the original
    /// error still surfaces.
    pub(super) fn begin_struct(&self, name: &'static str, fields: &'static [&'static str]) -> bool {
        if !self.key_name.holds_str(name) || !self.key_fields.holds_slice(fields) {
            self.key_name.set_str(name);
            self.key_fields.set_slice(fields);
            self.ignored.store(0, Relaxed);
            if let Some(wide) = &self.wide {
                clear_words(&wide.learned);
            }
            self.learned.store(false, Relaxed);
        }
        if self.learned.load(Relaxed) && self.invalid.is_none() {
            return true;
        }
        self.observing.store(0, Relaxed);
        if let Some(wide) = &self.wide {
            clear_words(&wide.observing);
        }
        self.active.store(true, Relaxed);
        false
    }

    pub(super) fn note_ignored(&self, column: usize) {
        if column < MAX_LEARNED_COLUMNS {
            self.observing
                .store(self.observing.load(Relaxed) | (1 << column), Relaxed);
        } else if let Some(wide) = &self.wide {
            let (word, bit) = wide_position(column);
            if let Some(word) = wide.observing.get(word) {
                word.store(word.load(Relaxed) | bit, Relaxed);
            }
        }
    }

    /// The learned ignored-column set for the first 64 columns, read once per
    /// record so the per-column test stays a register bit test.
    pub(super) fn ignored_mask(&self) -> u64 {
        self.ignored.load(Relaxed)
    }

    /// The learned ignored columns at or beyond 64, empty for an ordinary
    /// header, so the wide test is skipped entirely on the fast path.
    pub(super) fn wide_ignored(&self) -> &[AtomicU64] {
        self.wide.as_ref().map_or(&[], |wide| &wide.learned)
    }

    /// Promote the in-flight observation to the learned set.
    ///
    /// Called only after a record deserializes successfully.
    pub(crate) fn commit(&self) {
        if self.active.load(Relaxed) {
            self.active.store(false, Relaxed);
            self.ignored.store(self.observing.load(Relaxed), Relaxed);
            if let Some(wide) = &self.wide {
                for (learned, observed) in wide.learned.iter().zip(wide.observing.iter()) {
                    learned.store(observed.load(Relaxed), Relaxed);
                }
            }
            self.learned.store(true, Relaxed);
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_struct_cache_wide_and_sync() {
        let mut cache = StructCache::new();
        let mut headers = ByteRecord::new();
        for i in 0..80 {
            headers.append_field(format!("col_{i}").as_bytes());
        }
        cache.sync(Some(&headers));
        assert_eq!(cache.columns, 80);
        assert!(cache.wide.is_some());

        // Second sync with identical headers hits early return (line 295)
        cache.sync(Some(&headers));
        assert_eq!(cache.columns, 80);
        cache.note_ignored(70);
        cache.note_ignored(1000);

        // Third sync with same column count but different names returns false from .all()
        let mut diff_headers = ByteRecord::new();
        for i in 0..80 {
            diff_headers.append_field(format!("diff_{i}").as_bytes());
        }
        cache.sync(Some(&diff_headers));
        assert_eq!(cache.columns, 80);

        // note_ignored with column >= 64 when wide is None
        let cache_narrow = StructCache::new();
        cache_narrow.note_ignored(100);

        assert!(!cache.begin_struct("TestStruct", &["col_0"]));
        cache.note_ignored(0);
        cache.note_ignored(70);
        cache.commit();

        assert_eq!(cache.ignored_mask() & 1, 1);
        assert!(wide_contains(cache.wide_ignored(), 70));
        assert!(!wide_contains(cache.wide_ignored(), 69));
        assert!(!wide_contains(cache.wide_ignored(), 0));

        // sync(None) resets the cache.
        cache.sync(None);
        assert_eq!(cache.columns, 0);
        assert!(cache.wide.is_none());
    }

    #[test]
    fn atomic_keys_clear_both_identity_components() {
        let key = AtomicKey::unset();
        key.set_str("alpha");
        assert!(key.holds_str("alpha"));
        key.clear();
        assert_eq!(key.address.load(Relaxed), 0);
        assert_eq!(key.len.load(Relaxed), 0);
        assert!(!key.holds_str("alpha"));

        let first: &'static [&'static str] = &["a"];
        let second: &'static [&'static str] = &["a", "b"];
        key.set_slice(first);
        assert!(key.holds_slice(first));
        assert!(!key.holds_slice(second));
    }

    #[test]
    fn wide_word_counts_cover_the_exact_boundary() {
        let one = WideBitsets::new(65);
        assert_eq!(one.learned.len(), 1);
        assert_eq!(one.observing.len(), 1);
        assert!(one.learned.iter().all(|word| word.load(Relaxed) == 0));
        assert!(one.observing.iter().all(|word| word.load(Relaxed) == 0));

        let still_one = WideBitsets::new(128);
        assert_eq!(still_one.learned.len(), 1);

        let two = WideBitsets::new(129);
        assert_eq!(two.learned.len(), 2);
    }

    #[test]
    fn low_word_masks_stop_before_column_sixty_four() {
        assert!(mask_contains(1, 0));
        assert!(mask_contains(1_u64 << 63, 63));
        assert!(!mask_contains(u64::MAX, 64));
        assert!(!mask_contains(u64::MAX, 65));
    }

    #[test]
    fn syncing_identical_headers_preserves_learning_and_changes_reset_it() {
        let mut headers = ByteRecord::new();
        headers.push_field("a");
        headers.push_field("b");
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let name = "Pair";
        let fields: &'static [&'static str] = &["b"];
        assert!(!cache.begin_struct(name, fields));
        cache.note_ignored(0);
        cache.commit();
        assert!(cache.begin_struct(name, fields));

        cache.sync(Some(&headers));
        assert!(
            cache.begin_struct(name, fields),
            "an identical header must preserve the learned set"
        );

        let mut one_changed = ByteRecord::new();
        one_changed.push_field("a");
        one_changed.push_field("c");
        cache.sync(Some(&one_changed));
        assert_eq!(
            cache.names.iter().map(Box::as_ref).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        assert!(
            !cache.begin_struct(name, fields),
            "one changed column must invalidate learning"
        );

        let mut all_changed = ByteRecord::new();
        all_changed.push_field("x");
        all_changed.push_field("y");
        cache.sync(Some(&all_changed));
        assert_eq!(
            cache.names.iter().map(Box::as_ref).collect::<Vec<_>>(),
            vec!["x", "y"]
        );
    }

    #[test]
    fn invalid_headers_stop_at_the_first_invalid_column() {
        let mut headers = ByteRecord::new();
        headers.push_field("valid");
        headers.push_field(b"\xff");
        headers.push_field("must-not-be-cached");
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));

        assert_eq!(cache.columns, 3);
        assert_eq!(
            cache.names.iter().map(Box::as_ref).collect::<Vec<_>>(),
            vec!["valid"]
        );
        assert_eq!(cache.invalid.map(|(column, _)| column), Some(1));
    }

    #[test]
    fn sync_allocates_wide_storage_only_for_columns_beyond_the_edge() {
        let mut cache = StructCache::new();

        let mut sixty_four = ByteRecord::new();
        for column in 0..64 {
            sixty_four.push_field(format!("c{column}"));
        }
        cache.sync(Some(&sixty_four));
        assert!(cache.wide.is_none());

        let mut one_twenty_eight = ByteRecord::new();
        for column in 0..128 {
            one_twenty_eight.push_field(format!("c{column}"));
        }
        cache.sync(Some(&one_twenty_eight));
        assert_eq!(cache.wide.as_ref().map(|wide| wide.learned.len()), Some(1));
    }

    #[test]
    fn reset_clears_every_cached_and_in_flight_value() {
        let mut headers = ByteRecord::new();
        for column in 0..65 {
            headers.push_field(format!("c{column}"));
        }
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        assert!(!cache.begin_struct("Primed", &["c0"]));
        let invalid = String::from_utf8(vec![0xff]).unwrap_err().utf8_error();
        cache.invalid = Some((3, invalid));
        cache.ignored.store(7, Relaxed);
        cache.observing.store(11, Relaxed);
        cache.learned.store(true, Relaxed);
        cache.active.store(true, Relaxed);
        if let Some(wide) = &cache.wide {
            wide.learned[0].store(13, Relaxed);
            wide.observing[0].store(17, Relaxed);
        }

        cache.reset();

        assert!(cache.names.is_empty());
        assert!(cache.invalid.is_none());
        assert_eq!(cache.columns, 0);
        assert_eq!(cache.key_fields.address.load(Relaxed), 0);
        assert_eq!(cache.key_fields.len.load(Relaxed), 0);
        assert_eq!(cache.key_name.address.load(Relaxed), 0);
        assert_eq!(cache.key_name.len.load(Relaxed), 0);
        assert_eq!(cache.ignored.load(Relaxed), 0);
        assert_eq!(cache.observing.load(Relaxed), 0);
        assert!(cache.wide.is_none());
        assert!(!cache.learned.load(Relaxed));
        assert!(!cache.active.load(Relaxed));
    }

    #[test]
    fn a_new_observation_discards_uncommitted_low_and_wide_bits() {
        let mut headers = ByteRecord::new();
        for column in 0..66 {
            headers.push_field(format!("c{column}"));
        }
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let fields: &'static [&'static str] = &["c1", "c65"];

        assert!(!cache.begin_struct("Boundary", fields));
        cache.note_ignored(0);
        cache.note_ignored(64);

        assert!(!cache.begin_struct("Boundary", fields));
        cache.note_ignored(1);
        cache.note_ignored(65);
        cache.commit();

        assert!(!mask_contains(cache.ignored_mask(), 0));
        assert!(mask_contains(cache.ignored_mask(), 1));
        assert!(!wide_contains(cache.wide_ignored(), 64));
        assert!(wide_contains(cache.wide_ignored(), 65));
    }

    #[test]
    fn commit_closes_the_in_flight_observation() {
        let mut headers = ByteRecord::new();
        headers.push_field("a");
        headers.push_field("b");
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let fields: &'static [&'static str] = &["b"];

        assert!(!cache.begin_struct("Committed", fields));
        cache.note_ignored(0);
        cache.commit();
        assert_eq!(cache.ignored_mask(), 1);
        assert!(cache.learned.load(Relaxed));

        cache.note_ignored(1);
        cache.commit();
        assert_eq!(
            cache.ignored_mask(),
            1,
            "a second commit without begin must not promote later observations"
        );
    }
}

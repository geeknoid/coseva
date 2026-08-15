//! An independently owned CSV record whose fields are valid UTF-8.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::{borrow::ToOwned, string::String, vec::Vec};
use core::hash::{Hash, Hasher};
use core::ops::{Index, Range};
use core::str::{self, FromStr};

use crate::byte_record::ByteRecord;
use crate::error::{Error, Location};
use crate::field_ends::{end_is_null, end_offset};
use crate::projection::{FieldProjection, ProjectedTextFields};
use crate::record::Record;
use coseva_unsafe::storage::{RecordStorage, Utf8RecordError, Utf8RecordStorage};

/// An owned CSV record whose fields are guaranteed to be valid UTF-8.
///
/// The same owned record as [`ByteRecord`], with UTF-8 validated once when the
/// record is read. Use it when the fields are text and you would otherwise
/// check them repeatedly: [`get`](Self::get) returns `&str` directly, with no
/// per-access validation and no error to handle.
///
/// Reuse one record across a read loop and steady-state reads do not allocate:
/// [`Line::read_text_record_into`](crate::Line::read_text_record_into) refills
/// it in place, keeping the capacity it already has.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::{SliceParser, TextRecord};
///
/// let mut parser = SliceParser::<Csv>::new("city,country\nKøbenhavn,DK\n".as_bytes(), ParseOptions::new())?;
///
/// let mut record = TextRecord::new();
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
/// line.read_text_record_into(&mut record)?;
///
/// assert_eq!(record.get(0), Some("København"));
/// assert_eq!(record.get(1), Some("DK"));
///
/// // Invalid UTF-8 is rejected when the record is read, not when it is used.
/// let mut parser = SliceParser::<Csv>::new(b"header\nh\xffi\n", ParseOptions::new())?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
/// assert!(line.read_text_record_into(&mut record).is_err());
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
/// The record uses the same compact byte and endpoint storage as [`ByteRecord`],
/// with a UTF-8 invariant established when bytes enter the record. String
/// accessors therefore require no per-access validation.
#[derive(Clone, Debug, Default)]
pub struct TextRecord {
    inner: Utf8RecordStorage,
}

/// Compares field content, not the parser metadata a record happens to carry.
///
/// Two records with the same fields are equal even when they were read from
/// different positions, so a parsed record can be compared against a literal.
/// The wrapped [`ByteRecord`] compares only `ends` and `bytes`, so delegating
/// preserves that exact semantics.
impl PartialEq for TextRecord {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for TextRecord {}

impl Hash for TextRecord {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.inner.hash(state);
    }
}

#[cold]
#[inline(never)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn unreachable_valid_utf8() -> ! {
    unreachable!("a record of individually valid fields concatenates to valid UTF-8")
}

impl TextRecord {
    /// Construct an empty record without allocating.
    ///
    /// ```
    /// use coseva::TextRecord;
    ///
    /// let record = TextRecord::new();
    /// assert_eq!(record.len(), 0);
    /// assert!(record.is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: Utf8RecordStorage::new(),
        }
    }

    /// Construct an empty record with reusable capacity.
    ///
    /// ```
    /// use coseva::TextRecord;
    ///
    /// let record = TextRecord::with_capacity(4, 64);
    /// assert_eq!(record.len(), 0);
    /// assert!(record.is_empty());
    /// ```
    #[must_use]
    pub fn with_capacity(fields: usize, bytes: usize) -> Self {
        Self {
            inner: Utf8RecordStorage::with_capacity(fields, bytes),
        }
    }

    /// Number of fields.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the record has no fields.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Return one field.
    ///
    /// Fields that are an explicit NULL yield an empty slice, matching a
    /// non-NULL empty field. Use [`Self::is_null`] to distinguish the two.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        self.inner.get(index)
    }

    /// Return the decoded-storage range occupied by one field.
    ///
    /// The returned range indexes [`Self::as_slice`].
    #[must_use]
    pub fn range(&self, index: usize) -> Option<Range<usize>> {
        self.inner.range(index)
    }

    /// Whether a field is an explicit NULL rather than merely empty.
    #[must_use]
    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.inner.is_null(index)
    }

    /// Return all decoded fields as one contiguous string slice.
    ///
    /// Field boundaries are available through [`Self::range`].
    #[must_use]
    pub fn as_slice(&self) -> &str {
        self.inner.as_str()
    }

    /// Return one field as bytes.
    #[must_use]
    pub fn get_bytes(&self, index: usize) -> Option<&[u8]> {
        self.get(index).map(str::as_bytes)
    }

    /// Parse one field using [`FromStr`].
    ///
    /// An explicit NULL field yields `Ok(None)` without attempting to parse
    /// its (empty) contents.
    ///
    /// ```
    /// use coseva::TextRecord;
    ///
    /// let mut record = TextRecord::new();
    /// record.push_field("650706");
    /// assert_eq!(record.parse::<u64>(0), Ok(Some(650_706)));
    /// assert_eq!(record.parse::<u64>(1), Ok(None));
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the target parser's error.
    pub fn parse<T: FromStr>(&self, index: usize) -> Result<Option<T>, T::Err> {
        if self.is_null(index) == Some(true) {
            return Ok(None);
        }
        self.get(index).map(str::parse).transpose()
    }

    /// Iterate over string fields.
    #[must_use]
    pub const fn iter(&self) -> TextRecordIter<'_> {
        TextRecordIter {
            record: self,
            index: 0,
        }
    }

    /// Iterate over the fields selected by a projection, in projection order.
    ///
    /// Positions past the end of this record yield `None`.
    #[must_use]
    pub fn project<'projection, 'record>(
        &'record self,
        projection: &'projection FieldProjection,
    ) -> ProjectedTextFields<'projection, 'record> {
        ProjectedTextFields::new(projection, self)
    }

    /// Append a field.
    ///
    /// # Panics
    ///
    /// Panics if the record's total field bytes would exceed
    /// `usize::MAX / 2`, because a field endpoint that large would alias the
    /// bit used to mark explicit NULLs and silently corrupt every later field
    /// boundary. On 64-bit targets this is beyond any possible allocation; on
    /// 32-bit targets it is 2 GiB, reachable only by a record grown this far
    /// by hand.
    pub fn push_field(&mut self, field: impl AsRef<str>) {
        self.inner
            .push_field(field.as_ref(), crate::field_ends::max_field_offset());
        self.invalidate_source_metadata();
    }

    /// Append an explicit NULL field.
    ///
    /// NULL fields carry no bytes. [`Self::get`] and [`Self::iter`] continue
    /// to yield an empty slice for them; use [`Self::is_null`] to
    /// distinguish the two.
    pub fn push_null(&mut self) {
        self.inner.push_null(crate::field_ends::max_field_offset());
        self.invalidate_source_metadata();
    }

    /// Replace one field.
    ///
    /// The field is no longer NULL after this call, even when `field` is
    /// empty. Returns `false` when `index` is outside the record.
    ///
    /// # Panics
    ///
    /// Panics if the record's total field bytes would exceed
    /// `usize::MAX / 2`, because a field endpoint that large would alias the
    /// bit used to mark explicit NULLs and silently corrupt every later field
    /// boundary. On 64-bit targets this is beyond any possible allocation; on
    /// 32-bit targets it is 2 GiB, reachable only by a record grown this far
    /// by hand.
    pub fn set_field(&mut self, index: usize, field: impl AsRef<str>) -> bool {
        let field = field.as_ref();
        match self.inner.try_set_field_equal(index, field) {
            None => return false,
            Some(true) => {
                self.invalidate_source_metadata();
                return true;
            }
            Some(false) => {}
        }
        let updated = self
            .inner
            .set_field(index, field, crate::field_ends::max_field_offset());
        debug_assert!(updated);
        self.invalidate_source_metadata();
        true
    }

    /// Mark an existing field as an explicit NULL, discarding its bytes.
    ///
    /// Returns `false` when `index` is outside the record.
    pub fn set_null(&mut self, index: usize) -> bool {
        if !self
            .inner
            .set_null(index, crate::field_ends::max_field_offset())
        {
            return false;
        }
        self.invalidate_source_metadata();
        true
    }

    /// Retain only the first `len` fields.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }
        self.inner.truncate(len);
        self.invalidate_source_metadata();
    }

    /// Remove all fields while retaining allocated capacity.
    pub fn clear(&mut self) {
        self.inner.clear();
    }

    /// Reserve capacity for additional fields and decoded UTF-8 bytes.
    pub fn reserve(&mut self, additional_fields: usize, additional_bytes: usize) {
        self.inner.reserve(additional_fields, additional_bytes);
    }

    /// Release capacity not needed by the current record.
    pub fn shrink_to_fit(&mut self) {
        self.inner.shrink_to_fit();
    }

    /// Current decoded-byte capacity.
    #[must_use]
    pub const fn byte_capacity(&self) -> usize {
        self.inner.byte_capacity()
    }

    /// Current field-endpoint capacity.
    #[must_use]
    pub const fn field_capacity(&self) -> usize {
        self.inner.field_capacity()
    }

    /// Raw byte range occupied by this record when parser-produced.
    #[must_use]
    #[inline]
    pub fn byte_range(&self) -> Range<usize> {
        self.inner.byte_range()
    }

    /// Zero-based record index.
    #[must_use]
    #[inline]
    pub const fn index(&self) -> u64 {
        self.inner.index()
    }

    /// Replace parser-origin metadata without changing field data.
    ///
    /// Ordinary field mutation clears this metadata.
    #[inline]
    pub fn set_location(&mut self, byte_range: Range<usize>, index: u64) {
        self.inner.set_location(byte_range, index);
    }

    #[inline]
    pub(crate) fn rebase_location(&mut self, consumed: usize) {
        let range = self.inner.byte_range();
        let index = self.inner.index();
        let rebased = match range.end.checked_add(consumed) {
            Some(end) => range.start + consumed..end,
            None => range.start.saturating_add(consumed)..range.end.saturating_add(consumed),
        };
        self.inner.set_location(rebased, index);
    }

    /// Lossily convert an owned byte record to UTF-8.
    ///
    /// Invalid sequences are replaced independently within each field using
    /// `U+FFFD`. Valid input reuses the byte record's allocation without
    /// copying. Parser-origin metadata is preserved.
    ///
    /// ```
    /// use coseva::{ByteRecord, TextRecord};
    ///
    /// let mut bytes = ByteRecord::new();
    /// bytes.push_field(&b"Bost\xffn"[..]);
    ///
    /// let text = TextRecord::from_byte_record_lossy(bytes);
    /// assert_eq!(text.get(0), Some("Bost\u{FFFD}n"));
    /// ```
    #[must_use]
    pub fn from_byte_record_lossy(record: ByteRecord) -> Self {
        let (storage, byte_range, index) = record.into_storage();
        let storage = match Utf8RecordStorage::try_from_storage(storage) {
            Ok(inner) => return Self { inner },
            Err((storage, _error)) => storage,
        };
        let bytes = storage.bytes();
        let ends = storage.ends();
        let mut output = Self::with_capacity(ends.len(), bytes.len());
        let mut start = 0;
        for &raw_end in ends {
            let end = end_offset(raw_end);
            output.inner.push_field(
                &String::from_utf8_lossy(&bytes[start..end]),
                crate::field_ends::max_field_offset(),
            );
            if end_is_null(raw_end) {
                let field = output.len() - 1;
                output
                    .inner
                    .set_null(field, crate::field_ends::max_field_offset());
            }
            start = end;
        }
        output.inner.set_null_aware(storage.null_aware());
        output.set_location(byte_range, index);
        output
    }

    /// Whether any field of this record is an explicit NULL.
    #[must_use]
    pub(crate) const fn null_aware(&self) -> bool {
        self.inner.null_aware()
    }

    /// Copy this record into byte-oriented storage.
    ///
    /// ```
    /// use coseva::TextRecord;
    ///
    /// let mut text = TextRecord::new();
    /// text.push_field("Boston");
    ///
    /// let bytes = text.to_byte_record();
    /// assert_eq!(bytes.get(0), Some(&b"Boston"[..]));
    /// ```
    #[must_use]
    pub fn to_byte_record(&self) -> ByteRecord {
        ByteRecord::from_storage(
            self.inner.clone().into_storage(),
            self.inner.byte_range(),
            self.inner.index(),
        )
    }

    pub(crate) fn replace_from_byte_record(&mut self, source: &ByteRecord) -> Result<(), Error> {
        let storage = source.storage.clone();
        match Utf8RecordStorage::try_from_storage(storage) {
            Ok(inner) => {
                self.inner = inner;
                Ok(())
            }
            Err((_storage, _error)) => Err(Self::field_utf8_error(source)),
        }
    }

    /// Attribute an already-detected UTF-8 failure to the field holding it.
    ///
    /// Only reached once the concatenated buffer has failed validation or a
    /// field boundary has been found mid-sequence, so the cost of walking the
    /// fields is paid by the error path alone.
    #[cold]
    #[inline(never)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn field_utf8_error(source: &ByteRecord) -> Error {
        for (index, field) in source.iter().enumerate() {
            if let Err(error) = str::from_utf8(field) {
                return Error::utf8(error, index, Location::UNKNOWN);
            }
        }
        unreachable_valid_utf8()
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the refill closure must disappear from the owned-record hot path"
    )]
    pub(crate) fn refill_with_validity<E>(
        &mut self,
        refill: impl FnOnce(&mut RecordStorage) -> Result<coseva_unsafe::storage::TextValidity, E>,
    ) -> Result<Result<(), E>, Utf8RecordError> {
        self.inner.refill_with_validity(refill)
    }

    #[cfg(test)]
    pub(crate) fn refill_with<E>(
        &mut self,
        refill: impl FnOnce(&mut RecordStorage) -> Result<(), E>,
    ) -> Result<Result<(), E>, Utf8RecordError> {
        self.inner.refill_with(refill)
    }

    fn invalidate_source_metadata(&mut self) {
        self.inner.set_location(0..0, 0);
    }
}

/// Borrows a field by position.
///
/// # Panics
///
/// Panics if `index` is out of range. Use [`TextRecord::get`] to handle a
/// missing field without panicking.
impl Index<usize> for TextRecord {
    type Output = str;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("field index out of bounds")
    }
}

impl From<Vec<String>> for TextRecord {
    fn from(fields: Vec<String>) -> Self {
        let field_count = fields.len();
        let bytes = fields.iter().map(String::len).sum();
        let mut record = Self::with_capacity(field_count, bytes);
        for field in fields {
            record.push_field(field);
        }
        record
    }
}

impl<T: AsRef<str>> FromIterator<T> for TextRecord {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut record = Self::new();
        record.extend(iter);
        record
    }
}

impl<T: AsRef<str>> Extend<T> for TextRecord {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let fields = iter.into_iter();
        self.reserve(fields.size_hint().0, 0);
        for field in fields {
            self.push_field(field);
        }
    }
}

impl From<TextRecord> for Vec<String> {
    fn from(record: TextRecord) -> Self {
        record.iter().map(str::to_owned).collect()
    }
}

/// Converts a byte record to validated UTF-8 storage.
///
/// ```
/// use coseva::{ByteRecord, TextRecord};
///
/// let mut record = ByteRecord::new();
/// record.push_field("Boston");
/// let text = TextRecord::try_from(&record)?;
/// assert_eq!(text.get(0), Some("Boston"));
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns the first field that is not valid UTF-8.
impl TryFrom<&ByteRecord> for TextRecord {
    type Error = Error;

    fn try_from(record: &ByteRecord) -> Result<Self, Self::Error> {
        let mut output = Self::with_capacity(record.len(), record.bytes_len());
        output.replace_from_byte_record(record)?;
        Ok(output)
    }
}

impl TryFrom<ByteRecord> for TextRecord {
    type Error = Error;

    fn try_from(record: ByteRecord) -> Result<Self, Self::Error> {
        let (storage, byte_range, index) = record.into_storage();
        match Utf8RecordStorage::try_from_storage(storage) {
            Ok(inner) => Ok(Self { inner }),
            Err((storage, _error)) => {
                let record = ByteRecord::from_storage(storage, byte_range, index);
                Err(Self::field_utf8_error(&record))
            }
        }
    }
}

impl TryFrom<&Record<'_>> for TextRecord {
    type Error = Error;

    fn try_from(record: &Record<'_>) -> Result<Self, Self::Error> {
        let mut output = Self::with_capacity(record.len(), record.byte_range.len());
        for (index, field) in record.iter().enumerate() {
            if record.is_null(index) == Some(true) {
                output
                    .inner
                    .push_null(crate::field_ends::max_field_offset());
                continue;
            }
            let field = str::from_utf8(field)
                .map_err(|error| Error::utf8(error, index, Location::UNKNOWN))?;
            output
                .inner
                .push_field(field, crate::field_ends::max_field_offset());
        }
        output.inner.set_null_aware(record.null_aware);
        output.set_location(record.byte_range(), record.index());
        Ok(output)
    }
}

/// Iterator over fields in a [`TextRecord`].
///
/// For a worked example, see [`TextRecord`].
#[derive(Clone, Debug)]
pub struct TextRecordIter<'record> {
    record: &'record TextRecord,
    index: usize,
}

impl<'record> Iterator for TextRecordIter<'record> {
    type Item = &'record str;

    // gamma::skip(fn_value.some, reason = "returning a value without consuming an index made collection unbounded and exceeded the memory limit")
    fn next(&mut self) -> Option<Self::Item> {
        let field = self.record.get(self.index)?;
        // gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "not advancing repeats one field indefinitely and exceeded the memory limit")
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TextRecordIter<'_> {}

impl<'record> IntoIterator for &'record TextRecord {
    type Item = &'record str;
    type IntoIter = TextRecordIter<'record>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Owning iterator over the fields of a [`TextRecord`].
///
/// Created by `into_iter` on a [`TextRecord`] taken by value. The record's
/// buffer moves into the iterator, so consuming a record this way never copies
/// the record as a whole. Each field is still handed out as its own `String`,
/// which is one allocation per field — inherent to yielding owned fields, and
/// the reason to prefer iterating a `&TextRecord` when borrowed fields will do.
/// For a worked example, see [`TextRecord`].
#[derive(Debug)]
pub struct TextRecordIntoIter {
    record: TextRecord,
    index: usize,
}

impl Iterator for TextRecordIntoIter {
    type Item = String;

    fn next(&mut self) -> Option<Self::Item> {
        let field = self.record.get(self.index)?.to_owned();
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for TextRecordIntoIter {}

impl IntoIterator for TextRecord {
    type Item = String;
    type IntoIter = TextRecordIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        TextRecordIntoIter {
            record: self,
            index: 0,
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Source, Span, SpanSet};

    #[derive(Default)]
    struct WriteCounter(usize);

    impl Hasher for WriteCounter {
        fn finish(&self) -> u64 {
            self.0 as u64
        }

        fn write(&mut self, bytes: &[u8]) {
            self.0 += bytes.len().max(1);
        }
    }

    #[test]
    fn hashing_delegates_to_the_record_storage() {
        let record = TextRecord::from(vec!["alpha".to_owned(), "beta".to_owned()]);
        let mut hasher = WriteCounter::default();
        record.hash(&mut hasher);
        assert!(hasher.finish() > 0);
    }

    #[test]
    fn every_field_mutation_invalidates_source_metadata_but_noop_truncate_does_not() {
        fn set_source(record: &mut TextRecord) {
            record.set_location(10..20, 7);
        }

        fn assert_invalidated(record: &TextRecord) {
            assert_eq!(record.byte_range(), 0..0);
            assert_eq!(record.index(), 0);
        }

        let mut record = TextRecord::from(vec!["a".to_owned(), "bb".to_owned()]);

        set_source(&mut record);
        record.push_field("ccc");
        assert_invalidated(&record);

        set_source(&mut record);
        record.push_null();
        assert_invalidated(&record);

        set_source(&mut record);
        assert!(record.set_field(0, "z"));
        assert_invalidated(&record);

        set_source(&mut record);
        assert!(record.set_field(0, "longer"));
        assert_invalidated(&record);

        set_source(&mut record);
        assert!(record.set_null(1));
        assert_invalidated(&record);

        set_source(&mut record);
        record.truncate(1);
        assert_invalidated(&record);

        set_source(&mut record);
        record.clear();
        assert_invalidated(&record);

        record.push_field("same");
        set_source(&mut record);
        record.truncate(record.len());
        assert_eq!(record.byte_range(), 10..20);
        assert_eq!(record.index(), 7);

        record.invalidate_source_metadata();
        assert_invalidated(&record);
    }

    #[test]
    fn reserve_shrink_and_extend_hints_control_capacity_directly() {
        let mut record = TextRecord::new();
        record.reserve(7, 101);
        assert!(record.field_capacity() >= 7);
        assert!(record.byte_capacity() >= 101);

        let mut shrink = TextRecord::with_capacity(32, 256);
        shrink.push_field("x");
        shrink.shrink_to_fit();
        assert!(shrink.field_capacity() < 32);
        assert!(shrink.byte_capacity() < 256);

        struct HintOnly;
        impl Iterator for HintOnly {
            type Item = &'static str;

            fn next(&mut self) -> Option<Self::Item> {
                None
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                (9, Some(9))
            }
        }

        let mut hinted = TextRecord::new();
        hinted.extend(HintOnly);
        assert!(hinted.is_empty());
        assert!(hinted.field_capacity() >= 9);
        assert_eq!(hinted.byte_capacity(), 0);
    }

    #[test]
    fn borrowed_and_owned_iterators_report_every_exact_remaining_bound() {
        let record = TextRecord::from(vec!["a".to_owned(), "bb".to_owned(), "ccc".to_owned()]);
        let mut borrowed = record.iter();
        assert_eq!(borrowed.size_hint(), (3, Some(3)));
        assert_eq!(borrowed.next(), Some("a"));
        assert_eq!(borrowed.size_hint(), (2, Some(2)));
        assert_eq!(borrowed.next(), Some("bb"));
        assert_eq!(borrowed.next(), Some("ccc"));
        assert_eq!(borrowed.next(), None);
        assert_eq!(borrowed.size_hint(), (0, Some(0)));

        let mut owned = record.into_iter();
        assert_eq!(owned.size_hint(), (3, Some(3)));
        assert_eq!(owned.next().as_deref(), Some("a"));
        assert_eq!(owned.size_hint(), (2, Some(2)));
        assert_eq!(owned.next().as_deref(), Some("bb"));
        assert_eq!(owned.next().as_deref(), Some("ccc"));
        assert_eq!(owned.next(), None);
        assert_eq!(owned.size_hint(), (0, Some(0)));
    }

    /// The owned form must move the record's buffer into the iterator rather
    /// than copy the record. Comparing the buffer address before and after
    /// proves the transfer directly: a clone would land somewhere else.
    #[test]
    fn owned_iteration_transfers_the_buffer_instead_of_cloning_it() {
        let mut record = TextRecord::new();
        record.push_field("alpha");
        record.push_field("beta");

        let before = record.as_slice().as_ptr();
        let iter = record.into_iter();
        assert_eq!(
            iter.record.as_slice().as_ptr(),
            before,
            "the field bytes were reallocated, so the record was copied"
        );

        let fields: Vec<String> = iter.collect();
        assert_eq!(fields, vec!["alpha".to_owned(), "beta".to_owned()]);
    }

    /// A multi-byte UTF-8 sequence split across a field boundary must still
    /// be rejected as invalid in each half, proving the fused whole-buffer
    /// check carries the char-boundary guard: without it, the two halves
    /// concatenate into a valid `€` (E2 82 AC) and would wrongly pass through
    /// unmodified instead of being replaced with `U+FFFD`.
    #[test]
    fn lossy_conversion_rejects_a_multibyte_sequence_split_across_fields() {
        let mut bytes = ByteRecord::new();
        bytes.push_field(&[0xE2, 0x82][..]);
        bytes.push_field(&[0xAC][..]);

        let text = TextRecord::from_byte_record_lossy(bytes);
        assert_eq!(text.get(0), Some("\u{FFFD}"));
        assert_eq!(text.get(1), Some("\u{FFFD}"));
    }

    #[test]
    fn lossy_conversion_preserves_null_awareness_without_a_null_field() {
        let mut bytes = ByteRecord::new();
        bytes.push_null();
        assert!(bytes.set_field(0, b"\xff"));
        assert_eq!(bytes.is_null(0), Some(false));
        assert!(bytes.null_aware());

        let text = TextRecord::from_byte_record_lossy(bytes);
        assert_eq!(text.get(0), Some("\u{FFFD}"));
        assert_eq!(text.is_null(0), Some(false));
        assert!(text.null_aware());
    }

    /// Every `IntoIterator` form the type offers has to work in a `for` loop,
    /// which is the position the trait exists to serve.
    #[test]
    fn every_iteration_form_drives_a_for_loop() {
        let mut record = TextRecord::new();
        record.push_field("a");
        record.push_field("b");

        let mut borrowed = Vec::new();
        for field in &record {
            borrowed.push(field.to_owned());
        }

        let mut owned = Vec::new();
        for field in record {
            owned.push(field);
        }

        assert_eq!(borrowed, owned);
        assert_eq!(owned, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn try_from_record_for_string_record_preserves_null_flags() {
        let scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Input, 0..1, false),
            Span::from_valid_null(Source::Input, 1),
            Span::from_valid_range(Source::Input, 1..2, false),
        ]);
        let input = b"ab";
        let record =
            super::Record::new(spans.resolved(input, &scratch), 10..20, 7).with_null_aware(true);

        let strings = TextRecord::try_from(&record).expect("valid UTF-8");
        assert!(strings.null_aware());
        assert_eq!(strings.len(), 3);
        assert_eq!(strings.is_null(0), Some(false));
        assert_eq!(strings.is_null(1), Some(true));
        assert_eq!(strings.is_null(2), Some(false));
        assert_eq!(strings.get(1), Some(""));
        assert_eq!(strings.get(2), Some("b"));
        assert_eq!(strings.byte_range(), 10..20);
        assert_eq!(strings.index(), 7);
    }

    #[test]
    fn try_from_record_preserves_null_awareness_without_a_null_field() {
        let scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Input, 0..1, false),
            Span::from_valid_range(Source::Input, 1..2, false),
        ]);
        let record =
            super::Record::new(spans.resolved(b"ab", &scratch), 10..20, 7).with_null_aware(true);

        let strings = TextRecord::try_from(&record).expect("valid UTF-8");
        assert_eq!(strings.is_null(0), Some(false));
        assert_eq!(strings.is_null(1), Some(false));
        assert!(strings.null_aware());
    }

    #[test]
    fn borrowed_byte_conversion_preserves_fields_and_source_metadata() {
        let mut bytes = ByteRecord::new();
        bytes.push_field(b"alpha");
        bytes.push_null();
        bytes.push_field(b"omega");
        bytes.set_location(30..50, 11);

        let text = TextRecord::try_from(&bytes).expect("valid UTF-8");
        assert_eq!(text.iter().collect::<Vec<_>>(), ["alpha", "", "omega"]);
        assert_eq!(text.is_null(1), Some(true));
        assert_eq!(text.byte_range(), 30..50);
        assert_eq!(text.index(), 11);
    }

    #[test]
    #[expect(
        clippy::panic,
        reason = "the test exercises transactional rollback during unwinding"
    )]
    fn panicking_refill_cannot_publish_invalid_utf8() {
        let mut record = TextRecord::with_capacity(2, 32);
        let capacity = record.byte_capacity();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = record.refill_with::<()>(|storage| {
                storage.extend_bytes(&[0xff]);
                panic!("abort refill");
            });
        }));

        assert!(panic.is_err());
        assert_eq!(record.as_slice(), "");
        assert!(record.is_empty());
        assert_eq!(record.byte_capacity(), capacity);
    }

    #[test]
    fn text_record_set_field_and_null_lossy() {
        let mut record = TextRecord::new();
        record.push_field("foo");
        assert!(record.set_field(0, "bar")); // equal length
        assert!(record.set_field(0, "bar")); // same exact content
        assert!(record.set_field(0, "longer_than_before")); // different length
        assert!(record.set_field(0, "longer_than_before")); // same exact content again
        assert_eq!(record.get(0), Some("longer_than_before"));
        assert!(!record.set_field(10, "baz")); // out of bounds

        let mut byte_rec = ByteRecord::new();
        byte_rec.push_field(&b"foo\xff"[..]);
        byte_rec.push_null();
        byte_rec.set_location(40..60, 13);
        let text_rec = TextRecord::from_byte_record_lossy(byte_rec);
        assert_eq!(text_rec.len(), 2);
        assert!(text_rec.is_null(1).unwrap());
        assert!(text_rec.null_aware());
        assert_eq!(text_rec.byte_range(), 40..60);
        assert_eq!(text_rec.index(), 13);
    }
}

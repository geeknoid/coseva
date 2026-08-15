//! An independently owned byte-oriented CSV record.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::error::Error as StdError;
use core::hash::{Hash, Hasher};
use core::ops::{Index, Range};
use core::str::{self, FromStr};

use crate::error::Error;
#[cfg(feature = "serde")]
use crate::field_ends::EndpointNullFlags;
use crate::from_bytes::FromBytes;
use crate::projection::{ByteSource, FieldProjection, ProjectedFields};
use crate::record::Record;
#[cfg(feature = "serde")]
use crate::serde::deserialize_byte_record;
use crate::text_record::TextRecord;
use coseva_unsafe::storage::RecordStorage;

/// An owned CSV record holding raw field bytes.
///
/// Use this when a record has to outlive the parser position: to collect
/// records into a `Vec`, hand one to another thread, or keep one across
/// further reads. Fields are not required to be UTF-8, which makes this the
/// right choice for binary or unknown-encoding data — see
/// [`TextRecord`] when the fields are text.
///
/// Reuse one record across a read loop and steady-state reads do not allocate:
/// [`Line::read_byte_record_into`](crate::Line::read_byte_record_into) refills
/// it in place, keeping the capacity it already has.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::{ByteRecord, SliceParser};
///
/// let mut parser = SliceParser::<Csv>::new(b"city,population\nBoston,650706\nDenver,715522\n", ParseOptions::new())?;
///
/// // One record, reused for every row.
/// let mut record = ByteRecord::new();
/// let mut total = 0;
/// while let Some(mut line) = parser.next_line()? {
///     line.read_byte_record_into(&mut record)?;
///     total += record.parse::<u64>(1)?.unwrap_or(0);
/// }
/// assert_eq!(total, 1_366_228);
///
/// // Records can also be built by hand, for encoding.
/// let mut built = ByteRecord::new();
/// built.push_field("Boston");
/// built.push_field("650706");
/// assert_eq!(built.get(0), Some(&b"Boston"[..]));
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Debug, Default)]
pub struct ByteRecord {
    pub(crate) storage: RecordStorage,
}

/// Compares field content, not the parser metadata a record happens to carry.
///
/// Two records with the same fields are equal even when they were read from
/// different positions, so a parsed record can be compared against a literal.
impl PartialEq for ByteRecord {
    fn eq(&self, other: &Self) -> bool {
        self.storage == other.storage
    }
}

impl Eq for ByteRecord {}

impl Hash for ByteRecord {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.storage.hash(state);
    }
}

impl Clone for ByteRecord {
    fn clone(&self) -> Self {
        Self {
            storage: self.storage.clone(),
        }
    }

    /// Overwrite this record in place, reusing its existing allocations.
    ///
    /// The derived implementation would assign a fresh clone, freeing and
    /// reallocating both buffers on every record. Copying into the existing
    /// buffers keeps a reused `ByteRecord` allocation-free in steady state.
    fn clone_from(&mut self, source: &Self) {
        self.storage.clone_from(&source.storage);
    }
}

impl ByteRecord {
    pub(crate) fn from_storage(
        storage: RecordStorage,
        byte_range: Range<usize>,
        index: u64,
    ) -> Self {
        let mut storage = storage;
        storage.set_location(byte_range, index);
        Self { storage }
    }

    pub(crate) fn into_storage(self) -> (RecordStorage, Range<usize>, u64) {
        let byte_range = self.storage.byte_range();
        let index = self.storage.index();
        (self.storage, byte_range, index)
    }

    pub(crate) const fn null_aware(&self) -> bool {
        self.storage.null_aware()
    }

    /// Construct an empty record without allocating.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// let record = ByteRecord::new();
    /// assert_eq!(record.len(), 0);
    /// assert!(record.is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            storage: RecordStorage::new(),
        }
    }

    /// Construct an empty record with reusable capacity.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// let record = ByteRecord::with_capacity(4, 64);
    /// assert_eq!(record.len(), 0);
    /// assert!(record.is_empty());
    /// ```
    #[must_use]
    pub fn with_capacity(fields: usize, bytes: usize) -> Self {
        Self {
            storage: RecordStorage::with_capacity(fields, bytes),
        }
    }

    /// Number of fields.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.storage.len()
    }

    /// Whether the record has no fields.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    /// Number of decoded bytes stored by all fields.
    #[must_use]
    pub const fn bytes_len(&self) -> usize {
        self.storage.bytes_len()
    }

    /// Current decoded-byte capacity.
    #[must_use]
    pub const fn byte_capacity(&self) -> usize {
        self.storage.byte_capacity()
    }

    /// Current field-endpoint capacity.
    #[must_use]
    pub const fn field_capacity(&self) -> usize {
        self.storage.field_capacity()
    }

    /// Reserve capacity for additional fields and decoded bytes.
    pub fn reserve(&mut self, additional_fields: usize, additional_bytes: usize) {
        self.storage.reserve(additional_fields, additional_bytes);
    }

    /// Remove all fields while retaining allocated capacity.
    pub fn clear(&mut self) {
        self.clear_fields();
        self.invalidate_source_metadata();
    }

    pub(crate) fn clear_fields(&mut self) {
        self.storage.clear_fields();
    }

    /// Release capacity not needed by the current record.
    pub fn shrink_to_fit(&mut self) {
        self.storage.shrink_to_fit();
    }

    /// Hand back capacity grown by an outlier record, when there is enough of
    /// it to pay for the reallocation.
    ///
    /// Unlike [`Self::shrink_to_fit`] this is free to do nothing, which is what
    /// makes it safe to call from a reused emitter's scratch storage.
    pub(crate) fn reclaim(&mut self) {
        self.storage.reclaim();
    }

    /// Return one field.
    ///
    /// Fields that are an explicit NULL yield an empty slice, matching a
    /// non-NULL empty field. Use [`Self::is_null`] to distinguish the two.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        self.storage.get(index)
    }

    /// Return the decoded-storage range occupied by one field.
    ///
    /// The returned range indexes [`Self::as_slice`].
    #[must_use]
    pub fn range(&self, index: usize) -> Option<Range<usize>> {
        self.storage.range(index)
    }

    /// Whether a field is an explicit NULL rather than merely empty.
    #[must_use]
    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.storage.is_null(index)
    }

    #[cfg(feature = "serde")]
    pub(crate) fn null_flags(&self) -> EndpointNullFlags<'_> {
        EndpointNullFlags {
            ends: self.storage.ends().iter(),
        }
    }

    /// Return all decoded fields as one contiguous byte slice.
    ///
    /// Field boundaries are available through [`Self::range`].
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.storage.bytes()
    }

    /// Validate one field as UTF-8.
    ///
    /// An explicit NULL field yields `Ok(None)` without inspecting its
    /// (empty) bytes. A non-NULL empty field yields `Ok(Some(""))`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected field is not valid UTF-8.
    pub fn get_str(&self, index: usize) -> Result<Option<&str>, Error> {
        if self.is_null(index) == Some(true) {
            return Ok(None);
        }
        crate::field_value::get_str(self.get(index), index)
    }

    /// Parse one field directly from its bytes using [`FromBytes`].
    ///
    /// Integer and float targets parse straight from the raw bytes, so no
    /// intermediate UTF-8 validation is performed. Use
    /// [`Self::parse_from_str`] for types that only implement [`FromStr`].
    ///
    /// An explicit NULL field yields `Ok(None)` without attempting to parse
    /// its (empty) bytes.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// let mut record = ByteRecord::new();
    /// record.push_field("650706");
    /// assert_eq!(record.parse::<u64>(0)?, Some(650_706));
    /// assert_eq!(record.parse::<u64>(1)?, None);
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error located only by field index when the target parser
    /// rejects the field; a parser fills in the rest of the location when the
    /// error passes through it.
    pub fn parse<T: FromBytes>(&self, index: usize) -> Result<Option<T>, Error> {
        if self.is_null(index) == Some(true) {
            return Ok(None);
        }
        crate::field_value::parse(self.get(index), index)
    }

    /// Parse one UTF-8 field using [`FromStr`].
    ///
    /// Prefer [`Self::parse`] when the target implements [`FromBytes`]; this
    /// method validates the field as UTF-8 first and is intended for types
    /// that only provide a [`FromStr`] implementation.
    ///
    /// An explicit NULL field yields `Ok(None)` without attempting to parse
    /// its (empty) bytes.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// let mut record = ByteRecord::new();
    /// record.push_field("true");
    /// assert_eq!(record.parse_from_str::<bool>(0)?, Some(true));
    /// assert_eq!(record.parse_from_str::<bool>(1)?, None);
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the field is not UTF-8 or parsing fails.
    pub fn parse_from_str<T: FromStr>(&self, index: usize) -> Result<Option<T>, Error>
    where
        T::Err: StdError + Send + Sync + 'static,
    {
        if self.is_null(index) == Some(true) {
            return Ok(None);
        }
        crate::field_value::parse_from_str(self.get(index), index)
    }

    /// Iterate over decoded fields.
    #[must_use]
    pub const fn iter(&self) -> ByteRecordIter<'_> {
        ByteRecordIter {
            record: self,
            index: 0,
            start: 0,
        }
    }

    /// Iterate over the fields selected by a projection, in projection order.
    ///
    /// Positions past the end of this record yield `None`.
    #[must_use]
    pub fn project<'projection, 'record>(
        &'record self,
        projection: &'projection FieldProjection,
    ) -> ProjectedFields<'projection, 'record> {
        ProjectedFields::new(projection, ByteSource::Record(self))
    }

    /// Deserialize this record into `T` using Serde.
    ///
    /// `T` may borrow from this `ByteRecord` with the lifetime `'de`. For
    /// example, `T` can be a struct containing `&'de str` or `&'de [u8]`
    /// fields.
    ///
    /// Fields are accessed **positionally** (no header mapping). Use
    /// [`crate::Line::deserialized`] for header-aware struct
    /// deserialization.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct City<'row> {
    ///     name: &'row str,
    ///     population: u64,
    /// }
    ///
    /// let mut record = ByteRecord::new();
    /// record.push_field("Boston");
    /// record.push_field("650706");
    ///
    /// let city: City<'_> = record.deserialize()?;
    /// assert_eq!(city.name, "Boston");
    /// assert_eq!(city.population, 650_706);
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a [`crate::Error`] when `T` cannot be constructed from
    /// this record's fields.
    #[cfg(feature = "serde")]
    pub fn deserialize<'de, T: ::serde::Deserialize<'de>>(&'de self) -> Result<T, crate::Error> {
        deserialize_byte_record(self, None)
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
    pub fn push_field(&mut self, field: impl AsRef<[u8]>) {
        self.storage
            .push_field(field.as_ref(), crate::field_ends::max_field_offset());
        self.invalidate_source_metadata();
    }

    /// Append an explicit NULL field.
    ///
    /// NULL fields carry no bytes. [`Self::get`], [`Self::get_str`], and
    /// [`Self::iter`] continue to yield an empty slice for them, matching a
    /// non-NULL empty field; use [`Self::is_null`] to distinguish the two.
    pub fn push_null(&mut self) {
        self.storage
            .push_null(crate::field_ends::max_field_offset());
        self.invalidate_source_metadata();
    }

    /// Replace one field.
    ///
    /// The field is no longer NULL after this call, even when `field` is
    /// empty. Returns `false` when `index` is outside the record.
    ///
    /// Replacing a field with one of the same length is O(1) in the number of
    /// fields. A replacement of a different length shifts the bytes after it
    /// and rewrites every later field endpoint, so it costs O(fields after
    /// `index`); rewriting a whole record one field at a time this way is
    /// quadratic in field count. To rewrite a whole record, prefer
    /// [`Self::clear`] followed by [`Self::push_field`], which is linear in
    /// the record's total size.
    ///
    /// # Panics
    ///
    /// Panics if the record's total field bytes would exceed
    /// `usize::MAX / 2`, because a field endpoint that large would alias the
    /// bit used to mark explicit NULLs and silently corrupt every later field
    /// boundary. On 64-bit targets this is beyond any possible allocation; on
    /// 32-bit targets it is 2 GiB, reachable only by a record grown this far
    /// by hand.
    pub fn set_field(&mut self, index: usize, field: impl AsRef<[u8]>) -> bool {
        let field = field.as_ref();
        match self.storage.try_set_field_equal(index, field) {
            None => return false,
            Some(true) => {
                self.invalidate_source_metadata();
                return true;
            }
            Some(false) => {}
        }
        let updated = self
            .storage
            .set_field(index, field, crate::field_ends::max_field_offset());
        debug_assert!(updated);
        self.invalidate_source_metadata();
        true
    }

    /// Mark an existing field as an explicit NULL, discarding its bytes.
    ///
    /// Returns `false` when `index` is outside the record.
    ///
    /// ```
    /// use coseva::ByteRecord;
    ///
    /// let mut record = ByteRecord::new();
    /// record.push_field("Boston");
    /// record.push_field("650706");
    ///
    /// assert!(record.set_null(1));
    /// assert_eq!(record.is_null(1), Some(true));
    /// assert_eq!(record.get(1), Some(&b""[..]));
    ///
    /// // Out of range.
    /// assert!(!record.set_null(5));
    /// ```
    pub fn set_null(&mut self, index: usize) -> bool {
        if !self
            .storage
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
        self.storage.truncate(len);
        self.invalidate_source_metadata();
    }

    /// Raw byte range occupied by this record.
    #[must_use]
    pub fn byte_range(&self) -> Range<usize> {
        self.storage.byte_range()
    }

    /// Zero-based record index.
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.storage.index()
    }

    /// Copy a borrowed view into a fresh owned record.
    ///
    /// Reuse an existing record with
    /// [`Line::read_byte_record_into`](crate::Line::read_byte_record_into)
    /// instead when parsing a sequence of records.
    pub(crate) fn copied_from(source: &Record<'_>) -> Self {
        let mut record = Self::with_capacity(source.len(), source.byte_range().len());
        record.replace_from(source);
        record
    }

    pub(crate) fn replace_from(&mut self, source: &Record<'_>) {
        self.storage.clear();

        // Growing a fresh record field by field reallocates both buffers
        // several times per record, which costs more than the parse itself.
        // Reserving once removes that churn, and is a cheap capacity check
        // when the caller reuses a record that is already large enough.
        // The raw extent bounds the decoded bytes from above, because
        // unescaping and dropping delimiters only ever remove bytes.
        self.storage
            .reserve(source.spans.len(), source.byte_range().len());

        // Each span already carries its own NULL bit, so this reads it
        // directly rather than walking the spans a second time through a
        // parallel flag iterator.
        for index in 0..source.spans.len() {
            let (field, is_null) = source.spans.get_entry(index).expect("index is in range");
            if is_null {
                self.storage.append_null_field();
            } else {
                self.storage
                    .push_field(field, crate::field_ends::max_field_offset());
            }
        }
        self.storage
            .set_location(source.byte_range(), source.index());
        self.storage.set_null_aware(source.null_aware);
    }

    #[cfg(feature = "serde")]
    pub(crate) fn extend_bytes(&mut self, bytes: &[u8]) {
        self.storage.extend_bytes(bytes);
    }

    #[cfg(feature = "serde")]
    pub(crate) fn append_field(&mut self, field: &[u8]) {
        self.storage
            .push_field(field, crate::field_ends::max_field_offset());
    }

    pub(crate) fn storage_mut(&mut self) -> &mut RecordStorage {
        &mut self.storage
    }

    /// Append an explicit NULL field without touching parser-origin
    /// metadata.
    ///
    /// Used by trusted, source-aware callers (parsers, batch adapters)
    /// filling a record during parsing.
    #[cfg(feature = "serde")]
    pub(crate) fn append_null_field(&mut self) {
        self.storage.append_null_field();
    }

    #[cfg(feature = "serde")]
    pub(crate) fn finish_field(&mut self) {
        self.storage.finish_field();
    }

    /// Close out the current field as an explicit NULL.
    ///
    /// Equivalent to [`Self::append_null_field`]; provided so parser call
    /// sites can mirror the `append_field`/`finish_field` naming convention
    /// used for ordinary fields.
    #[cfg(feature = "serde")]
    pub(crate) fn finish_null_field(&mut self) {
        self.append_null_field();
    }

    /// Re-mark the fields of a freshly parsed plain record whose dialect
    /// spells some of them NULL.
    ///
    /// A pass over the finished record keeps NULL policy out of the parse
    /// kernel; inlining the test costs 3.2% on ordinary `Nulls::None` input.
    ///
    /// Only sound for records the plain kernel produced, since it bails to the
    /// general parser on the first quote and only unquoted fields can be NULL.
    /// The record is not yet `null_aware` on entry, so `ends` still holds bare
    /// offsets.
    #[cfg(test)]
    pub(crate) fn trim_fields_ascii(&mut self) {
        self.storage.trim_fields_ascii();
    }

    /// Set the source position this record came from, leaving its fields
    /// alone.
    ///
    /// A record read from a parser already knows its byte range and record
    /// number, reported by [`Self::byte_range`] and [`Self::index`]. Use this
    /// when you build a record yourself and want those to reflect a position
    /// in some original document — for example when reassembling records from
    /// a separate store. Mutating any field clears the position again, since
    /// it is no longer the record that was read.
    pub fn set_location(&mut self, byte_range: Range<usize>, index: u64) {
        self.storage.set_location(byte_range, index);
    }

    pub(crate) fn invalidate_source_metadata(&mut self) {
        self.storage.invalidate_source_metadata();
    }
}

/// Borrows a field by position.
///
/// # Panics
///
/// Panics if `index` is out of range. Use [`ByteRecord::get`] to handle a
/// missing field without panicking.
impl Index<usize> for ByteRecord {
    type Output = [u8];

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("field index out of bounds")
    }
}

impl From<Vec<Vec<u8>>> for ByteRecord {
    fn from(fields: Vec<Vec<u8>>) -> Self {
        let field_count = fields.len();
        let bytes = fields.iter().map(Vec::len).sum();
        let mut record = Self::with_capacity(field_count, bytes);
        for field in fields {
            record.push_field(field);
        }
        record
    }
}

impl<T: AsRef<[u8]>> FromIterator<T> for ByteRecord {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut record = Self::new();
        record.extend(iter);
        record
    }
}

impl<T: AsRef<[u8]>> Extend<T> for ByteRecord {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        let fields = iter.into_iter();
        self.reserve(fields.size_hint().0, 0);
        for field in fields {
            self.push_field(field);
        }
    }
}

impl From<&TextRecord> for ByteRecord {
    fn from(record: &TextRecord) -> Self {
        record.to_byte_record()
    }
}

impl From<ByteRecord> for Vec<Vec<u8>> {
    fn from(record: ByteRecord) -> Self {
        record.iter().map(<[u8]>::to_vec).collect()
    }
}

/// Iterator over fields in a [`ByteRecord`].
///
/// For a worked example, see [`ByteRecord`].
#[derive(Clone, Debug)]
pub struct ByteRecordIter<'record> {
    record: &'record ByteRecord,
    index: usize,
    start: usize,
}

impl<'record> Iterator for ByteRecordIter<'record> {
    type Item = &'record [u8];

    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    fn next(&mut self) -> Option<Self::Item> {
        let end = self.record.range(self.index)?.end;
        let field = &self.record.as_slice()[self.start..end];
        self.start = end;
        // gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ByteRecordIter<'_> {}

impl<'record> IntoIterator for &'record ByteRecord {
    type Item = &'record [u8];
    type IntoIter = ByteRecordIter<'record>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Owning iterator over the fields of a [`ByteRecord`].
///
/// Created by `into_iter` on a [`ByteRecord`] taken by value. The record's
/// buffers move into the iterator, so consuming a record this way never copies
/// the record as a whole. Each field is still handed out as its own `Vec<u8>`,
/// which is one allocation per field — inherent to yielding owned fields, and
/// the reason to prefer iterating a `&ByteRecord` when borrowed fields will do.
/// For a worked example, see [`ByteRecord`].
#[derive(Debug)]
pub struct ByteRecordIntoIter {
    record: ByteRecord,
    index: usize,
}

impl Iterator for ByteRecordIntoIter {
    type Item = Vec<u8>;

    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    fn next(&mut self) -> Option<Self::Item> {
        let field = self.record.get(self.index)?.to_vec();
        // gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
        self.index += 1;
        Some(field)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.record.len() - self.index;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ByteRecordIntoIter {}

impl IntoIterator for ByteRecord {
    type Item = Vec<u8>;
    type IntoIter = ByteRecordIntoIter;

    fn into_iter(self) -> Self::IntoIter {
        ByteRecordIntoIter {
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
    struct RecordingHasher {
        writes: Vec<u8>,
    }

    impl Hasher for RecordingHasher {
        fn finish(&self) -> u64 {
            self.writes.len() as u64
        }

        fn write(&mut self, bytes: &[u8]) {
            self.writes.extend_from_slice(bytes);
        }
    }

    /// The owned form must move the record's buffer into the iterator rather
    /// than copy the record. Comparing the buffer address before and after
    /// proves the transfer directly: a clone would land somewhere else.
    #[test]
    fn owned_iteration_transfers_the_buffer_instead_of_cloning_it() {
        let mut record = ByteRecord::new();
        record.push_field("alpha");
        record.push_field("beta");
        record.push_field("gamma");

        let before = record.as_slice().as_ptr();
        let iter = record.into_iter();
        assert_eq!(
            iter.record.as_slice().as_ptr(),
            before,
            "the field bytes were reallocated, so the record was copied"
        );

        let fields: Vec<Vec<u8>> = iter.collect();
        assert_eq!(
            fields,
            vec![b"alpha".to_vec(), b"beta".to_vec(), b"gamma".to_vec()]
        );
    }

    /// Every `IntoIterator` form the type offers has to work in a `for` loop,
    /// which is the position the trait exists to serve.
    #[test]
    fn every_iteration_form_drives_a_for_loop() {
        let mut record = ByteRecord::new();
        record.push_field("a");
        record.push_field("b");

        let mut borrowed = Vec::new();
        for field in &record {
            borrowed.push(field.to_vec());
        }

        let mut owned = Vec::new();
        for field in record {
            owned.push(field);
        }

        assert_eq!(borrowed, owned);
        assert_eq!(owned, vec![b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn byte_record_truncate_and_trim_preserve_null_flags() {
        let mut record = ByteRecord::new();
        record.push_field("  a  ");
        record.push_null();
        record.push_field("  b  ");

        record.trim_fields_ascii();
        assert_eq!(record.get(0), Some(b"a".as_slice()));
        assert_eq!(record.is_null(1), Some(true));
        assert_eq!(record.get(2), Some(b"b".as_slice()));

        record.truncate(2);
        assert_eq!(record.len(), 2);
        assert_eq!(record.is_null(1), Some(true));
    }

    #[test]
    fn byte_record_replace_from_propagates_null_metadata() {
        let mut scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Scratch, 0..0, false),
            Span::from_valid_null(Source::Scratch, 0),
        ]);
        scratch.extend_from_slice(b"");
        let source =
            super::Record::new(spans.resolved(b"", &scratch), 0..0, 0).with_null_aware(true);

        let mut dest = ByteRecord::new();
        dest.replace_from(&source);

        assert!(dest.null_aware());
        assert_eq!(dest.len(), 2);
        assert_eq!(dest.is_null(0), Some(false));
        assert_eq!(dest.is_null(1), Some(true));
    }

    #[test]
    fn byte_record_additional_methods() {
        let mut record = ByteRecord::new();
        record.push_field("foo");
        record.shrink_to_fit();
        assert!(!record.set_field(10, "bar")); // index out of bounds
        assert!(record.set_field(0, "baz")); // equal length
        assert!(record.set_field(0, "longer_than_before")); // different length
        assert_eq!(record.get(0), Some(b"longer_than_before".as_slice()));
    }

    #[test]
    fn equality_hashing_and_storage_location_are_observable() {
        let left: ByteRecord = [b"alpha".as_slice(), b"beta"].into_iter().collect();
        let equal: ByteRecord = [b"alpha".as_slice(), b"beta"].into_iter().collect();
        let different: ByteRecord = [b"alpha".as_slice(), b"gamma"].into_iter().collect();

        assert_eq!(left, equal);
        assert_ne!(left, different);

        let mut hasher = RecordingHasher::default();
        left.hash(&mut hasher);
        assert!(
            !hasher.writes.is_empty(),
            "hashing a populated record must feed its field layout and bytes to the hasher"
        );

        let mut storage = RecordStorage::new();
        storage.push_field(b"value", crate::field_ends::MAX_FIELD_OFFSET);
        let record = ByteRecord::from_storage(storage, 17..29, 41);
        assert_eq!(record.byte_range(), 17..29);
        assert_eq!(record.index(), 41);

        let (storage, byte_range, index) = record.into_storage();
        assert_eq!(storage.get(0), Some(b"value".as_slice()));
        assert_eq!(byte_range, 17..29);
        assert_eq!(index, 41);
    }

    #[test]
    fn capacity_controls_change_both_record_buffers() {
        let mut reserved = ByteRecord::new();
        reserved.reserve(10, 100);
        assert!(reserved.field_capacity() >= 10);
        assert!(reserved.byte_capacity() >= 100);

        let collected: ByteRecord = [b"".as_slice(); 10].into_iter().collect();
        assert_eq!(collected.len(), 10);
        assert_eq!(
            collected.field_capacity(),
            10,
            "the exact iterator lower bound should be reserved before pushing endpoints"
        );
        assert_eq!(
            collected.byte_capacity(),
            0,
            "reserving endpoint capacity must not allocate a byte buffer for empty fields"
        );

        let mut shrunk = ByteRecord::with_capacity(64, 256);
        shrunk.push_field(b"x");
        let before_shrink = (shrunk.field_capacity(), shrunk.byte_capacity());
        shrunk.shrink_to_fit();
        assert!(shrunk.field_capacity() < before_shrink.0);
        assert!(shrunk.byte_capacity() < before_shrink.1);
        assert!(shrunk.field_capacity() >= shrunk.len());
        assert!(shrunk.byte_capacity() >= shrunk.bytes_len());

        let mut reclaimed = ByteRecord::with_capacity(100_000, 100_000);
        reclaimed.push_field(b"x");
        let before_reclaim = (reclaimed.field_capacity(), reclaimed.byte_capacity());
        reclaimed.reclaim();
        assert!(reclaimed.field_capacity() < before_reclaim.0);
        assert!(reclaimed.byte_capacity() < before_reclaim.1);
        assert!(reclaimed.field_capacity() >= reclaimed.len());
        assert!(reclaimed.byte_capacity() >= reclaimed.bytes_len());
    }

    fn assert_location_cleared(record: &ByteRecord) {
        assert_eq!(record.byte_range(), 0..0);
        assert_eq!(record.index(), 0);
    }

    fn located_record() -> ByteRecord {
        let mut record: ByteRecord = [b"abc".as_slice(), b"def"].into_iter().collect();
        record.set_location(100..120, 9);
        record
    }

    #[test]
    fn successful_content_mutations_invalidate_source_metadata() {
        let mut record = located_record();
        record.clear();
        assert!(record.is_empty());
        assert_location_cleared(&record);

        let mut record = located_record();
        record.push_field(b"ghi");
        assert_location_cleared(&record);

        let mut record = located_record();
        record.push_null();
        assert_location_cleared(&record);

        let mut record = located_record();
        assert!(record.set_field(0, b"xyz"));
        assert_location_cleared(&record);

        let mut record = located_record();
        assert!(record.set_field(0, b"longer"));
        assert_location_cleared(&record);

        let mut record = located_record();
        assert!(record.set_null(1));
        assert_location_cleared(&record);

        let mut record = located_record();
        record.truncate(1);
        assert_eq!(record.len(), 1);
        assert_location_cleared(&record);

        let mut record = located_record();
        record.invalidate_source_metadata();
        assert_location_cleared(&record);
    }

    #[test]
    fn no_op_mutations_preserve_source_metadata() {
        let mut record = located_record();
        record.truncate(record.len());
        assert_eq!(record.byte_range(), 100..120);
        assert_eq!(record.index(), 9);

        assert!(!record.set_field(record.len(), b"missing"));
        assert!(!record.set_null(record.len()));
        assert_eq!(record.byte_range(), 100..120);
        assert_eq!(record.index(), 9);
    }

    #[test]
    fn conversion_errors_retain_the_requested_field_index() {
        let mut utf8 = ByteRecord::new();
        utf8.push_field(b"valid");
        utf8.push_field(b"\xFF");
        assert_eq!(
            utf8.get_str(1).expect_err("invalid UTF-8").location().field,
            1
        );

        let mut number = ByteRecord::new();
        number.push_field(b"10");
        number.push_field(b"not-a-number");
        assert_eq!(
            number
                .parse::<u64>(1)
                .expect_err("invalid integer")
                .location()
                .field,
            1
        );

        let mut boolean = ByteRecord::new();
        boolean.push_field(b"true");
        boolean.push_field(b"not-a-bool");
        assert_eq!(
            boolean
                .parse_from_str::<bool>(1)
                .expect_err("invalid boolean")
                .location()
                .field,
            1
        );
    }

    fn ten_field_spans() -> SpanSet {
        SpanSet::from([
            Span::from_valid_range(Source::Scratch, 0..1, false),
            Span::from_valid_range(Source::Scratch, 1..2, false),
            Span::from_valid_range(Source::Scratch, 2..3, false),
            Span::from_valid_range(Source::Scratch, 3..4, false),
            Span::from_valid_range(Source::Scratch, 4..5, false),
            Span::from_valid_range(Source::Scratch, 5..6, false),
            Span::from_valid_range(Source::Scratch, 6..7, false),
            Span::from_valid_range(Source::Scratch, 7..8, false),
            Span::from_valid_range(Source::Scratch, 8..9, false),
            Span::from_valid_range(Source::Scratch, 9..10, false),
        ])
    }

    #[test]
    fn replace_from_reserves_exact_storage_and_copies_location() {
        let scratch = *b"abcdefghij";
        let spans = ten_field_spans();
        let source = Record::new(spans.resolved(b"", &scratch), 200..210, 12).with_null_aware(true);
        let mut destination = ByteRecord::new();
        destination.replace_from(&source);

        assert_eq!(destination.len(), 10);
        assert_eq!(destination.as_slice(), b"abcdefghij");
        assert_eq!(destination.field_capacity(), 10);
        assert_eq!(destination.byte_capacity(), 10);
        assert_eq!(destination.byte_range(), 200..210);
        assert_eq!(destination.index(), 12);
        assert!(destination.null_aware());
    }

    struct MaxFieldOffsetGuard(Option<usize>);

    impl MaxFieldOffsetGuard {
        fn shrink_to(bound: usize) -> Self {
            let previous =
                crate::field_ends::TEST_MAX_FIELD_OFFSET.with(|cell| cell.replace(Some(bound)));
            Self(previous)
        }
    }

    impl Drop for MaxFieldOffsetGuard {
        fn drop(&mut self) {
            crate::field_ends::TEST_MAX_FIELD_OFFSET.with(|cell| cell.set(self.0));
        }
    }

    #[test]
    fn replace_from_honors_the_testable_field_offset_bound() {
        let scratch = *b"ab";
        let spans = SpanSet::from([Span::from_valid_range(Source::Scratch, 0..2, false)]);
        let source = Record::new(spans.resolved(b"", &scratch), 0..2, 0);
        let _guard = MaxFieldOffsetGuard::shrink_to(1);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut destination = ByteRecord::new();
            destination.replace_from(&source);
        }));
        assert!(
            result.is_err(),
            "copying a field past the endpoint bound must not corrupt the endpoint"
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn append_field_honors_the_testable_field_offset_bound() {
        let _guard = MaxFieldOffsetGuard::shrink_to(1);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut record = ByteRecord::new();
            record.append_field(b"ab");
        }));
        assert!(
            result.is_err(),
            "appending a field past the endpoint bound must not corrupt the endpoint"
        );
    }

    #[test]
    fn borrowed_and_owned_iterators_report_exact_remaining_lengths() {
        let record: ByteRecord = [
            b"a".as_slice(),
            b"bc".as_slice(),
            b"".as_slice(),
            b"d".as_slice(),
        ]
        .into_iter()
        .collect();

        let mut borrowed = record.iter();
        assert_eq!(borrowed.size_hint(), (4, Some(4)));
        assert_eq!(borrowed.len(), 4);
        assert_eq!(borrowed.next(), Some(b"a".as_slice()));
        assert_eq!(borrowed.size_hint(), (3, Some(3)));
        assert_eq!(borrowed.next(), Some(b"bc".as_slice()));
        assert_eq!(borrowed.next(), Some(b"".as_slice()));
        assert_eq!(borrowed.len(), 1);
        assert_eq!(borrowed.next(), Some(b"d".as_slice()));
        assert_eq!(borrowed.size_hint(), (0, Some(0)));
        assert_eq!(borrowed.next(), None);

        let mut owned = record.into_iter();
        assert_eq!(owned.size_hint(), (4, Some(4)));
        assert_eq!(owned.next(), Some(b"a".to_vec()));
        assert_eq!(owned.len(), 3);
        assert_eq!(owned.next(), Some(b"bc".to_vec()));
        assert_eq!(owned.next(), Some(Vec::new()));
        assert_eq!(owned.size_hint(), (1, Some(1)));
        assert_eq!(owned.next(), Some(b"d".to_vec()));
        assert_eq!(owned.size_hint(), (0, Some(0)));
        assert_eq!(owned.next(), None);
    }
}

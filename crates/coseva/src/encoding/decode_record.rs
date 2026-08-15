use crate::byte_record::ByteRecord;
use crate::record::Record;
use crate::span::ResolvedSpans;

/// Byte-oriented field access used by native typed decoders.
///
/// Both lending [`Record`] views and owned [`ByteRecord`] values implement
/// this trait, allowing generated decoding to share one implementation.
/// For a worked example, see [`super::CsvDecode`].
pub trait DecodeRecord<'record> {
    /// Return one decoded field.
    #[must_use]
    fn get_field(&self, index: usize) -> Option<&'record [u8]>;

    /// Report whether this record distinguishes explicit NULL fields from
    /// present-but-empty fields.
    ///
    /// The default implementation returns `false`, matching ordinary CSV
    /// sources where an empty field is never treated as NULL. Record types
    /// that can carry explicit NULL metadata (for example database-flavored
    /// [`ByteRecord`]/[`Record`] sources) override this to report `true` once
    /// configured accordingly.
    #[must_use]
    fn is_null_aware(&self) -> bool {
        false
    }

    /// Report whether the field at `index` is an explicit NULL.
    ///
    /// The default implementation returns `false`. This is only meaningful
    /// when [`Self::is_null_aware`] returns `true`; non-NULL-aware sources
    /// always report `false` here regardless of the field's content.
    #[must_use]
    fn is_field_null(&self, _index: usize) -> bool {
        false
    }
}

impl<'record> DecodeRecord<'record> for Record<'record> {
    fn get_field(&self, index: usize) -> Option<&'record [u8]> {
        self.get(index)
    }

    fn is_null_aware(&self) -> bool {
        self.null_aware
    }

    fn is_field_null(&self, index: usize) -> bool {
        self.is_null(index).unwrap_or(false)
    }
}

/// Zero-indirection field view handed to fused decoding.
///
/// Unlike [`Record`], this carries only what a decode needs: the two buffers a
/// field can live in and the record's spans. There is no byte range, no record
/// index and no header permutation, because the fused path only runs once the
/// parser has established that the file's columns are already in the target
/// type's declared order.
///
/// Constructed by the parser; [`super::CsvDecode::fused_decode`] receives it.
///
/// Hidden from the docs: its constructor is crate-private, so this type exists
/// in the public signature of a hidden trait method and nowhere else. Callers
/// decode through [`Record`] instead.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct FusedFields<'record> {
    spans: ResolvedSpans<'record>,
    null_aware: bool,
}

impl<'record> FusedFields<'record> {
    pub(crate) const fn new(spans: ResolvedSpans<'record>, null_aware: bool) -> Self {
        Self { spans, null_aware }
    }

    /// Number of fields the record actually held.
    #[must_use]
    #[inline]
    pub const fn len(&self) -> usize {
        self.spans.len()
    }

    /// Report whether the record held no fields at all.
    #[must_use]
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

impl<'record> DecodeRecord<'record> for FusedFields<'record> {
    #[inline]
    fn get_field(&self, index: usize) -> Option<&'record [u8]> {
        self.spans.get(index)
    }

    #[inline]
    fn is_null_aware(&self) -> bool {
        self.null_aware
    }

    #[inline]
    fn is_field_null(&self, index: usize) -> bool {
        self.spans.field_is_null(index)
    }
}

/// Borrowed field-access adapter for an owned [`ByteRecord`].
///
/// For a worked example, see [`super::CsvDecode`].
#[derive(Clone, Copy, Debug)]
pub struct ByteRecordRef<'record> {
    record: &'record ByteRecord,
}

impl<'record> ByteRecordRef<'record> {
    /// Borrow an owned record for native typed decoding.
    #[must_use]
    pub const fn new(record: &'record ByteRecord) -> Self {
        Self { record }
    }
}

impl<'record> DecodeRecord<'record> for ByteRecordRef<'record> {
    fn get_field(&self, index: usize) -> Option<&'record [u8]> {
        self.record.get(index)
    }

    fn is_null_aware(&self) -> bool {
        self.record.null_aware()
    }

    fn is_field_null(&self, index: usize) -> bool {
        self.record.is_null(index).unwrap_or(false)
    }
}

/// Field-access adapter that applies a resolved header permutation.
///
/// For a worked example, see [`super::CsvDecode`].
#[derive(Clone, Copy, Debug)]
pub struct MappedRecord<'record, 'mapping, R: ?Sized> {
    record: &'record R,
    mapping: &'mapping [usize],
}

impl<'record, 'mapping, R: ?Sized> MappedRecord<'record, 'mapping, R> {
    /// Wrap a record with target-field-to-source-field positions.
    #[must_use]
    pub const fn new(record: &'record R, mapping: &'mapping [usize]) -> Self {
        Self { record, mapping }
    }
}

impl<'record, R> DecodeRecord<'record> for MappedRecord<'_, '_, R>
where
    R: DecodeRecord<'record> + ?Sized,
{
    fn get_field(&self, index: usize) -> Option<&'record [u8]> {
        self.mapping
            .get(index)
            .and_then(|&source| self.record.get_field(source))
    }

    fn is_null_aware(&self) -> bool {
        self.record.is_null_aware()
    }

    fn is_field_null(&self, index: usize) -> bool {
        self.mapping
            .get(index)
            .is_some_and(|&source| self.record.is_field_null(source))
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{ByteRecordRef, DecodeRecord, FusedFields, MappedRecord};
    use crate::byte_record::ByteRecord;
    use crate::record::Record;
    use crate::span::{Source, Span, SpanSet};

    fn spans() -> SpanSet {
        SpanSet::from([
            Span::new(Source::Input, 0..2, false).expect("in range"),
            Span::from_valid_null(Source::Input, 2),
        ])
    }

    #[test]
    fn a_null_aware_record_reports_its_null_field_through_the_decode_trait() {
        let spans = spans();
        let record = Record::new(spans.resolved(b"ab", b""), 0..2, 0).with_null_aware(true);
        assert!(record.is_null_aware());
        assert!(!record.is_field_null(0));
        assert!(record.is_field_null(1));
        // Past the end there is no field to be null.
        assert!(!record.is_field_null(2));
    }

    #[test]
    fn a_record_that_does_not_track_nulls_reports_none() {
        // A parser with no NULL syntax never marks a span, so every field is
        // present-but-empty rather than NULL.
        let spans = SpanSet::from([
            Span::new(Source::Input, 0..2, false).expect("in range"),
            Span::new(Source::Input, 2..2, false).expect("in range"),
        ]);
        let record = Record::new(spans.resolved(b"ab", b""), 0..2, 0);
        assert!(!record.is_null_aware());
        assert!(!record.is_field_null(0));
        assert!(!record.is_field_null(1));
    }

    #[test]
    fn fused_fields_report_their_length() {
        let spans = spans();
        let fields = FusedFields::new(spans.resolved(b"ab", b""), false);
        assert_eq!(fields.len(), 2);
        assert!(!fields.is_empty());

        let empty_spans = SpanSet::new();
        let empty = FusedFields::new(empty_spans.resolved(b"", b""), false);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn fused_and_owned_adapters_report_null_metadata_exactly() {
        let spans = spans();
        let fields = FusedFields::new(spans.resolved(b"ab", b""), true);
        assert!(fields.is_null_aware());
        assert!(!fields.is_field_null(0));
        assert!(fields.is_field_null(1));

        let mut record = ByteRecord::new();
        record.push_field(b"value");
        record.push_null();
        let borrowed = ByteRecordRef::new(&record);
        assert!(borrowed.is_null_aware());
        assert!(!borrowed.is_field_null(0));
        assert!(borrowed.is_field_null(1));
        assert!(!borrowed.is_field_null(99));
    }

    #[test]
    fn mapped_records_forward_null_awareness_without_inventing_it() {
        let mut plain = ByteRecord::new();
        plain.push_field(b"value");
        let plain = ByteRecordRef::new(&plain);
        let mapped = MappedRecord::new(&plain, &[0]);
        assert!(!mapped.is_null_aware());
        assert!(!mapped.is_field_null(0));

        let mut nullable = ByteRecord::new();
        nullable.push_null();
        let nullable = ByteRecordRef::new(&nullable);
        let mapped = MappedRecord::new(&nullable, &[0]);
        assert!(mapped.is_null_aware());
        assert!(mapped.is_field_null(0));
    }
}

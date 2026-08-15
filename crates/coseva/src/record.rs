//! A zero-copy view of one parsed CSV record.

use core::error::Error as StdError;
use core::ops::Range;
#[cfg(feature = "serde")]
use core::slice;
use core::str::{self, FromStr};

use crate::error::Error;
use crate::from_bytes::FromBytes;
use crate::projection::{ByteSource, FieldProjection, ProjectedFields};
#[cfg(feature = "serde")]
use crate::serde::deserialize_record;
#[cfg(test)]
use crate::span::Source;
#[cfg(any(feature = "serde", test))]
use crate::span::Span;
#[cfg(test)]
use crate::span::SpanSet;
use crate::span::{ResolvedSpanIter, ResolvedSpans};

/// A view of one CSV record, borrowed from the parser.
///
/// This is what [`Line::record`](crate::Line::record) hands back, and the
/// cheapest way to read a record: its fields are slices of the input rather
/// than copies. It borrows the parser, so it is valid until the next record is
/// requested — see [`ByteRecord`](crate::ByteRecord) and
/// [`TextRecord`](crate::TextRecord) for owned records that outlive the
/// parser position.
///
/// Fields can be taken as raw bytes ([`get`](Self::get)), as validated UTF-8
/// ([`get_str`](Self::get_str)), or parsed into a Rust type
/// ([`parse`](Self::parse)).
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::SliceParser;
///
/// let mut parser = SliceParser::<Csv>::new(b"city,population,coastal\nBoston,650706,true\n", ParseOptions::new())?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
/// let record = line.record()?;
///
/// assert_eq!(record.len(), 3);
/// assert_eq!(record.get(0), Some(&b"Boston"[..]));
/// assert_eq!(record.get_str(0)?, Some("Boston"));
/// assert_eq!(record.parse::<u64>(1)?, Some(650_706));
/// assert_eq!(record.parse::<bool>(2)?, Some(true));
///
/// // Out-of-range access is `None`, never a panic.
/// assert_eq!(record.get(9), None);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Debug)]
pub struct Record<'record> {
    pub(crate) spans: ResolvedSpans<'record>,
    pub(crate) byte_range: Range<usize>,
    index: u64,
    pub(crate) null_aware: bool,
}

impl<'record> Record<'record> {
    pub(crate) const fn new(
        spans: ResolvedSpans<'record>,
        byte_range: Range<usize>,
        index: u64,
    ) -> Self {
        Self {
            spans,
            byte_range,
            index,
            null_aware: false,
        }
    }

    /// Mark whether this view distinguishes explicit NULL fields from empty
    /// ones.
    ///
    /// Parsers configured with a database [`crate::config::Nulls`] should set
    /// this before handing the record to typed/Serde decoding so that
    /// `Option<T>` fields observe explicit NULLs instead of the ordinary
    /// empty-means-`None` rule.
    #[must_use]
    pub(crate) const fn with_null_aware(mut self, null_aware: bool) -> Self {
        self.null_aware = null_aware;
        self
    }

    /// Number of fields.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether the record has no fields.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// Zero-based record index.
    #[must_use]
    pub const fn index(&self) -> u64 {
        self.index
    }

    /// Raw byte range occupied by this record.
    #[must_use]
    pub fn byte_range(&self) -> Range<usize> {
        self.byte_range.clone()
    }

    /// Shift the reported range by the bytes already dropped from the front of
    /// the window the record was parsed from.
    ///
    /// The range is only ever reported, never used to reach the fields, so
    /// moving it onto stream coordinates cannot disturb field access.
    pub(crate) fn rebase(&mut self, consumed: usize) {
        self.byte_range = self.byte_range.start.saturating_add(consumed)
            ..self.byte_range.end.saturating_add(consumed);
    }

    /// Return one decoded field.
    ///
    /// Fields that are an explicit NULL yield an empty slice, matching a
    /// non-NULL empty field. Use [`Self::is_null`] to distinguish the two.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&'record [u8]> {
        self.spans.get(index)
    }

    /// Iterate over the fields selected by a projection, in projection order.
    ///
    /// The yielded fields borrow from the parsed input, not from this record
    /// view, so they remain usable for the whole input lifetime. Positions past
    /// the end of this record yield `None`.
    #[must_use]
    pub fn project<'projection>(
        &self,
        projection: &'projection FieldProjection,
    ) -> ProjectedFields<'projection, 'record> {
        ProjectedFields::new(projection, ByteSource::Spans(self.spans))
    }

    /// Whether a field had to be copied while unescaping.
    #[cfg(test)]
    fn is_copied(&self, index: usize) -> Option<bool> {
        self.spans
            .source(index)
            .map(|source| source == Source::Scratch)
    }

    /// Whether a field is an explicit NULL rather than merely empty.
    #[must_use]
    pub fn is_null(&self, index: usize) -> Option<bool> {
        self.spans.is_null(index)
    }

    #[cfg(feature = "serde")]
    pub(crate) fn null_flags(&self) -> SpanNullFlags<'record> {
        SpanNullFlags {
            spans: self.spans.span_iter(),
        }
    }

    /// Validate one field as UTF-8.
    ///
    /// An explicit NULL field yields `Ok(None)` without inspecting its
    /// (empty) bytes. A non-NULL empty field yields `Ok(Some(""))`.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected field is not valid UTF-8.
    pub fn get_str(&self, index: usize) -> Result<Option<&'record str>, Error> {
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
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::SliceParser;
    ///
    /// let mut parser = SliceParser::<Csv>::new(b"pop\n650706\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let record = line.record()?;
    ///
    /// assert_eq!(record.parse::<u64>(0)?, Some(650_706));
    /// assert_eq!(record.parse::<u64>(1)?, None);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
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
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::SliceParser;
    ///
    /// let mut parser = SliceParser::<Csv>::new(b"coastal\ntrue\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let record = line.record()?;
    ///
    /// assert_eq!(record.parse_from_str::<bool>(0)?, Some(true));
    /// assert_eq!(record.parse_from_str::<bool>(1)?, None);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the field is not UTF-8 or the target parser
    /// rejects it.
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
    pub const fn iter(&self) -> RecordIter<'record> {
        RecordIter {
            fields: self.spans.fields(),
        }
    }

    /// Deserialize this record into `T` using Serde.
    ///
    /// Fields are accessed **positionally** (no header mapping). Use
    /// [`crate::Line::deserialized`] for header-aware struct
    /// deserialization.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::SliceParser;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct City<'row> {
    ///     name: &'row str,
    ///     population: u64,
    /// }
    ///
    /// let mut parser =
    ///     SliceParser::<Csv>::new(b"Boston,650706\n", ParseOptions::new().headers(coseva::config::Headers::None))?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let record = line.record()?;
    ///
    /// let city: City<'_> = record.deserialize()?;
    /// assert_eq!(city.name, "Boston");
    /// assert_eq!(city.population, 650_706);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a [`crate::Error`] when `T` cannot be constructed from
    /// this record's fields.
    #[cfg(feature = "serde")]
    pub fn deserialize<T: ::serde::Deserialize<'record>>(&self) -> Result<T, crate::Error> {
        deserialize_record(self)
    }
}

/// Iterator over fields in a [`Record`].
///
/// For a worked example, see [`Record`].
#[derive(Clone, Debug)]
pub struct RecordIter<'record> {
    fields: ResolvedSpanIter<'record>,
}

impl<'record> Iterator for RecordIter<'record> {
    type Item = &'record [u8];

    // gamma::skip(fn_value.some, reason = "the logged mutation never consumes the underlying field iterator and exceeded the Gamma memory limit")
    fn next(&mut self) -> Option<Self::Item> {
        self.fields.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.fields.size_hint()
    }
}

impl ExactSizeIterator for RecordIter<'_> {}

impl<'record> IntoIterator for &Record<'record> {
    type Item = &'record [u8];
    type IntoIter = RecordIter<'record>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over whether each field of a [`Record`] is an explicit NULL.
#[cfg(feature = "serde")]
#[derive(Clone, Debug)]
pub(crate) struct SpanNullFlags<'record> {
    spans: slice::Iter<'record, Span>,
}

#[cfg(feature = "serde")]
impl Iterator for SpanNullFlags<'_> {
    type Item = bool;

    fn next(&mut self) -> Option<Self::Item> {
        self.spans.next().map(|span| span.is_null())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.spans.size_hint()
    }
}

#[cfg(feature = "serde")]
impl ExactSizeIterator for SpanNullFlags<'_> {}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FormatOptions, Headers, ParseOptions};
    use crate::error::ErrorKind;
    use crate::slice_parser::SliceParser;

    /// Parse every record as data, bypassing the default header policy.
    fn unheaded_parser(input: &str) -> SliceParser<'_> {
        SliceParser::with_options(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options")
    }

    #[test]
    fn plain_fields_are_never_copied() {
        let mut parser = unheaded_parser("city,population\nBoston,650706\n");
        let mut line = parser.next_line().expect("headers parse").expect("headers");
        let headers = line.record().expect("headers parse");
        assert_eq!(headers.is_copied(0), Some(false));
        assert_eq!(headers.is_copied(1), Some(false));
        let mut line = parser.next_line().expect("record parses").expect("record");
        let record = line.record().expect("record parses");
        assert_eq!(record.is_copied(0), Some(false));
        assert_eq!(record.is_copied(1), Some(false));
    }

    #[test]
    fn record_fields_and_iterator_bounds_are_exact() {
        let mut parser = unheaded_parser("a,bb,ccc\n");
        let mut line = parser.next_line().expect("line parses").expect("one line");
        let record = line.record().expect("record parses");

        assert_eq!(record.len(), 3);
        assert_eq!(record.get(0), Some(b"a".as_slice()));
        assert_eq!(record.get(1), Some(b"bb".as_slice()));
        assert_eq!(record.get(2), Some(b"ccc".as_slice()));
        assert_eq!(record.get(3), None);

        let mut fields = record.iter();
        assert_eq!(fields.size_hint(), (3, Some(3)));
        assert_eq!(fields.next(), Some(b"a".as_slice()));
        assert_eq!(fields.size_hint(), (2, Some(2)));
        assert_eq!(fields.next(), Some(b"bb".as_slice()));
        assert_eq!(fields.next(), Some(b"ccc".as_slice()));
        assert_eq!(fields.next(), None);
        assert_eq!(fields.size_hint(), (0, Some(0)));
    }

    #[test]
    fn only_escaped_quotes_force_a_copy() {
        let mut parser = unheaded_parser(r#""plain","say ""hello""","tail""#);
        let mut line = parser.next_line().expect("record parses").expect("record");
        let record = line.record().expect("record parses");
        assert_eq!(record.get(0), Some(&b"plain"[..]));
        assert_eq!(record.get(1), Some(&b"say \"hello\""[..]));
        assert_eq!(record.get(2), Some(&b"tail"[..]));
        // Quoting alone stays zero-copy; only unescaping uses the scratch
        // buffer.
        assert_eq!(record.is_copied(0), Some(false));
        assert_eq!(record.is_copied(1), Some(true));
        assert_eq!(record.is_copied(2), Some(false));
    }

    #[test]
    fn record_view_reports_null_fields_and_awareness() {
        let scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Input, 0..1, false),
            Span::from_valid_null(Source::Input, 1),
        ]);
        let input = b"a";
        let record = super::Record::new(spans.resolved(input, &scratch), 0..1, 0);
        assert!(!record.null_aware);
        assert_eq!(record.is_null(0), Some(false));
        assert_eq!(record.is_null(1), Some(true));
        assert_eq!(record.get(1), Some(b"".as_slice()));

        let aware = record.with_null_aware(true);
        assert!(aware.null_aware);
    }

    #[test]
    fn record_view_parse_and_get_str_treat_null_as_none() {
        let scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Input, 0..2, false),
            Span::from_valid_null(Source::Input, 2),
            Span::from_valid_range(Source::Input, 2..2, false),
        ]);
        let input = b"42";
        let record = super::Record::new(spans.resolved(input, &scratch), 0..2, 0);

        // Non-NULL present field parses normally.
        assert_eq!(
            record.parse::<u32>(0).expect("valid numeric field"),
            Some(42)
        );
        assert_eq!(record.get_str(0).expect("valid UTF-8 field"), Some("42"));

        // Explicit NULL short-circuits to `None` without attempting to parse
        // its (empty) bytes, even for a numeric type that would otherwise
        // error on an empty string.
        assert_eq!(record.parse::<u32>(1).expect("NULL field"), None);
        assert_eq!(record.get_str(1).expect("NULL field"), None);

        // A non-NULL empty field: parsing an empty string as a number still
        // errors, and get_str still yields `Some("")`.
        record
            .parse::<u32>(2)
            .expect_err("empty numeric field should fail");
        assert_eq!(record.get_str(2).expect("valid UTF-8 field"), Some(""));

        // Out-of-range indices remain `Ok(None)`.
        assert_eq!(record.parse::<u32>(3).expect("absent field"), None);
        assert_eq!(record.get_str(3).expect("absent field"), None);
    }

    #[test]
    fn parse_is_byte_native_and_parse_from_str_validates_utf8() {
        use core::net::Ipv4Addr;

        let scratch = Vec::new();
        let spans = SpanSet::from([
            Span::from_valid_range(Source::Input, 0..2, false),
            Span::from_valid_range(Source::Input, 2..3, false),
            Span::from_valid_range(Source::Input, 3..12, false),
        ]);
        let input = b"42\xFF127.0.0.1";
        let record = super::Record::new(spans.resolved(input, &scratch), 0..12, 0);

        // `parse` goes through `FromBytes`, so integers never touch UTF-8
        // validation.
        assert_eq!(record.parse::<u32>(0).expect("valid digits"), Some(42));

        // `parse_from_str` validates UTF-8 first, so it reports the failure
        // as such. `parse` is byte-native and never gets that far: it
        // rejects the first byte that is not a digit. Both locate the
        // failure by field index alone.
        let utf8 = record
            .parse_from_str::<u32>(1)
            .expect_err("field 1 is not UTF-8");
        assert!(matches!(utf8.kind(), ErrorKind::InvalidUtf8(_)));
        assert_eq!(utf8.location().field, 1);

        let digit = record.parse::<u32>(1).expect_err("field 1 is not a number");
        assert_eq!(digit.kind(), ErrorKind::InvalidDigit);
        assert_eq!(digit.location().field, 1);

        // Both spellings agree for a type reachable either way.
        assert_eq!(
            record.parse::<Ipv4Addr>(2).expect("valid address"),
            Some(Ipv4Addr::LOCALHOST)
        );
        assert_eq!(
            record.parse_from_str::<Ipv4Addr>(2).expect("valid address"),
            Some(Ipv4Addr::LOCALHOST)
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn span_null_flags_report_an_exact_size_hint() {
        // `SpanNullFlags` is an `ExactSizeIterator`, so its `size_hint` must
        // stay exact as the iterator is drained.
        let mut parser = unheaded_parser("a,b,c\n");
        let mut line = parser.next_line().expect("record parses").expect("record");
        let record = line.record().expect("record parses");

        let mut flags = record.null_flags();
        assert_eq!(flags.size_hint(), (3, Some(3)));
        assert_eq!(flags.len(), 3);
        assert_eq!(flags.next(), Some(false));
        assert_eq!(flags.size_hint(), (2, Some(2)));
        assert_eq!(flags.count(), 2);
    }
}

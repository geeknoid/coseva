//! Zero-copy parser for an in-memory CSV input.

use crate::byte_record::ByteRecord;
use crate::config::{FormatOptions, ParseOptions, ReadBom};
use crate::encoding::DecodeSink;
use crate::engine::{Engine, FieldAliases, TypedMapping};
use crate::error::{Error, ErrorKind, Location};
use crate::filter::Predicate;
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::line::{Line, LineSource};
use core::marker::PhantomData;

/// CSV parser for an input already held in memory.
///
/// This is the reader to reach for when you have the whole document as a
/// `&[u8]` or `&str` — a file you read in one go, an HTTP body, a literal in
/// a test. Because the input is all present, fields are handed back as slices
/// of it: nothing is copied and nothing is allocated, except to unescape a
/// field that needed it. Use [`IoParser`](crate::IoParser) when
/// the document arrives a chunk at a time or does not fit in memory.
///
/// The first record is taken as headers by default; see
/// [`Headers`](crate::config::Headers) to change that.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::SliceParser;
///
/// let mut parser = SliceParser::<Csv>::new(b"city,population\nBoston,650706\nDenver,715522\n", ParseOptions::new())?;
///
/// // Headers enable lookup by name.
/// assert_eq!(parser.header_index("population")?, Some(1));
///
/// // A cursor over the records...
/// let mut total = 0;
/// while let Some(mut line) = parser.next_line()? {
///     total += line.record()?.parse::<u64>(1)?.unwrap_or(0);
/// }
/// assert_eq!(total, 1_366_228);
///
/// // ...or an iterator, over a fresh parser.
/// let mut parser = SliceParser::<Csv>::new(b"city,population\nBoston,650706\nDenver,715522\n", ParseOptions::new())?;
/// assert_eq!(parser.byte_records().count(), 2);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct SliceParser<'input, F: CsvFormat = Dynamic> {
    input: &'input [u8],
    pub(crate) core: Engine,
    marker: PhantomData<fn() -> F>,
}

impl<'input> SliceParser<'input, Dynamic> {
    /// Create a parser for an explicit format and parse options.
    ///
    /// ```
    /// use coseva::config::{FormatOptions, ParseOptions};
    /// use coseva::SliceParser;
    ///
    /// let mut parser = SliceParser::with_options(
    ///     b"city;pop\nBoston;650706\n",
    ///     FormatOptions::CSV.delimiter(b';'),
    ///     ParseOptions::new(),
    /// )?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid format or rejected leading BOM.
    pub fn with_options<S: AsRef<[u8]> + ?Sized>(
        input: &'input S,
        format: FormatOptions,
        options: ParseOptions,
    ) -> Result<Self, Error> {
        Self::build(input.as_ref(), format, options)
    }
}

impl<'input, F: StaticFormat> SliceParser<'input, F> {
    /// Parse with the format named by `F`.
    ///
    /// The kernel is specialized for `F`, so its structural bytes become
    /// immediates and the branches its settings rule out are removed. The
    /// format comes from the type, so there is no format argument; use
    /// [`SliceParser::new`] or [`SliceParser::with_options`] when the format
    /// is only known at run time.
    ///
    /// ```
    /// use coseva::config::ParseOptions;
    /// use coseva::format::Tsv;
    /// use coseva::SliceParser;
    ///
    /// let mut parser = SliceParser::<Tsv>::new(b"city\tpop\nBoston\t650706\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for invalid parse options or a rejected leading BOM.
    pub fn new<S: AsRef<[u8]> + ?Sized>(
        input: &'input S,
        options: ParseOptions,
    ) -> Result<Self, Error> {
        Self::build(input.as_ref(), F::FORMAT, options)
    }
}

impl<'input, F: CsvFormat> SliceParser<'input, F> {
    pub(crate) fn build(
        input: &'input [u8],
        format: FormatOptions,
        options: ParseOptions,
    ) -> Result<Self, Error> {
        let settings = options.into_settings(format)?;
        if settings.bom == ReadBom::Reject && input.starts_with(b"\xEF\xBB\xBF") {
            // The whole input is in hand, so the mark is refused eagerly at
            // construction. Streaming front ends cannot see input this early,
            // so all three agree on the kind (`RejectedBom`) and location
            // (`Location::START`); only the stage differs, as their APIs
            // require.
            return Err(Error::new(ErrorKind::RejectedBom, Location::START));
        }
        Ok(Self {
            input,
            core: Engine::from_config(input, settings),
            marker: PhantomData,
        })
    }

    #[inline]
    fn resolve_optional_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        self.core
            .resolve_optional_typed_mapping(self.input, names, aliases)
    }

    /// Return the configured or discovered headers.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    #[inline]
    pub fn headers(&mut self) -> Result<Option<&ByteRecord>, Error> {
        self.core.headers(self.input)
    }

    /// Resolve the first header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    #[inline]
    pub fn header_index(&mut self, name: impl AsRef<[u8]>) -> Result<Option<usize>, Error> {
        self.core.header_index(self.input, name)
    }

    /// Resolve every duplicate header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns a parse error when discovering first-record headers fails.
    #[inline]
    pub fn header_indices(&mut self, name: impl AsRef<[u8]>) -> Result<&[usize], Error> {
        self.core.header_indices(self.input, name)
    }

    /// Whether this parser uses discovered or caller-provided headers.
    #[must_use]
    #[inline]
    pub fn has_headers(&self) -> bool {
        self.core.has_headers()
    }

    /// Replace the header record without consuming input.
    ///
    /// Subsequent named decoding uses this record, and the next input record
    /// is treated as data.
    #[inline]
    pub fn set_headers(&mut self, headers: ByteRecord) {
        self.core.set_headers(headers);
    }

    /// Move to the next record, without parsing it.
    ///
    /// Returns `false` once the input is exhausted, and keeps returning
    /// `false` on further calls. The record is parsed lazily by whichever
    /// view is asked for it, so that a view only materializes the fields it
    /// actually needs; syntax errors are therefore reported by the view
    /// rather than here.
    ///
    /// # Errors
    ///
    /// Returns a parse error raised while discovering headers, or when the
    /// parser has already failed.
    #[inline]
    pub(crate) fn advance(&mut self) -> Result<bool, Error> {
        self.core.advance::<F>(self.input)
    }

    /// Move to the next record satisfying `predicate`, skipping the rest.
    ///
    /// The predicate's literal is searched for directly in the raw input with
    /// the SIMD byte scanner, so records that cannot possibly match are never
    /// split into fields or unescaped. Matching itself is always exact: a
    /// candidate located by the scan is fully parsed and evaluated before it
    /// is accepted, so the result is identical to filtering the output of
    /// [`Self::advance`] with [`Predicate::matches_field`].
    ///
    /// A predicate naming a header that does not exist matches no records.
    ///
    /// Unlike [`Self::advance`], the accepted record has already been parsed
    /// in full, because evaluating the predicate requires its fields.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline]
    pub(crate) fn advance_with_filter(&mut self, predicate: &Predicate) -> Result<bool, Error> {
        self.core.advance_with_filter::<F>(self.input, predicate)
    }

    /// View the located record as a [`Line`].
    ///
    /// The whole input is always present and nothing is ever dropped from the
    /// front of it, so the line needs no offset, no poison flag, and no
    /// deferred byte-order-mark report.
    #[inline]
    pub(crate) fn current_line(&mut self) -> Line<'_, F> {
        Line::new(&mut self.core, self.input, 0, None, false)
    }

    /// Decode the current record through `mapping`.
    ///
    /// The record is parsed here rather than by `advance`, so a projected
    /// mapping materializes only the fields the target type names.
    #[inline]
    fn decode_with_mapping<'record, S>(
        &'record mut self,
        mapping: &TypedMapping,
        sink: S,
    ) -> Result<S::Output, Error>
    where
        S: DecodeSink<'record>,
    {
        self.core
            .decode_with_mapping::<_, F>(self.input, mapping, sink)
    }

    /// Current parser location.
    #[must_use]
    #[inline]
    pub fn location(&self) -> Location {
        self.core.location(self.input)
    }

    /// Seek to a previously observed record boundary.
    ///
    /// Discovered or provided headers and the established first-record field
    /// count are preserved. The target's byte offset, physical line, and
    /// record index become the parser's new location, so positions and errors
    /// reported afterwards stay absolute against the whole input.
    ///
    /// The location should come from [`Self::location`] immediately before
    /// reading a record, from a record's
    /// [`byte_range`](crate::Record::byte_range), or from equivalent validated
    /// index metadata. Seeking to an arbitrary byte inside a record is not
    /// supported. `location.field` must be zero.
    ///
    /// A successful seek clears any earlier parse failure.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::SliceParser;
    ///
    /// let mut parser = SliceParser::<Csv>::new("city,pop\nParis,2\nLyon,1\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected the first record"))?;
    /// assert_eq!(line.record()?.get(0), Some(b"Paris".as_slice()));
    ///
    /// // Bookmark the next record before reading it.
    /// let bookmark = parser.location();
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected the second record"))?;
    /// assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    /// assert_eq!(bookmark.line, 3);
    ///
    /// // Returning restores the byte, line, and record counters exactly.
    /// parser.seek(bookmark)?;
    /// assert_eq!(parser.location(), bookmark);
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected the second record again"))?;
    /// assert_eq!(line.record()?.get(0), Some(b"Lyon".as_slice()));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error while initially discovering headers or field width,
    /// for a nonzero field location, or for a byte past the end of the input.
    pub fn seek(&mut self, location: Location) -> Result<(), Error> {
        if location.field != 0 {
            return Err(Error::detailed(
                ErrorKind::Configuration,
                "CSV seek positions must identify a record boundary with field 0",
            ));
        }
        if location.byte > self.input.len() {
            return Err(Error::detailed(
                ErrorKind::Configuration,
                "CSV seek positions must lie within the input",
            ));
        }
        // Settle the headers against the original position, so a seek past the
        // header record still leaves the mapping and field width established.
        self.core.ensure_headers(self.input)?;
        self.core
            .seek_to(location.byte, location.line, location.record);
        Ok(())
    }

    #[cfg(feature = "index")]
    #[inline]
    pub(crate) fn line_for_offset(&self, byte: usize) -> u64 {
        self.core.line_for_offset(self.input, byte)
    }

    // gamma::skip(fn_value.unit, reason = "mutation causes index traversal to lose progress and time out")
    /// Move the physical-line origin onto an already-numbered record boundary.
    #[cfg(feature = "index")]
    #[inline]
    pub(crate) fn advance_line_origin(&mut self, byte: usize, line: u64) {
        // gamma::skip(stmt.delete_call, reason = "mutation causes index traversal to lose progress and time out")
        self.core.advance_line_origin(byte, line);
    }

    /// Whether parsing has reached EOF or stopped after an error.
    #[must_use]
    #[inline]
    pub const fn is_done(&self) -> bool {
        self.core.is_done(self.input)
    }
}

impl<F: CsvFormat> LineSource for SliceParser<'_, F> {
    type Format = F;

    #[inline]
    fn advance_line(&mut self, predicate: Option<&Predicate>) -> Result<bool, Error> {
        match predicate {
            Some(predicate) => self.advance_with_filter(predicate),
            None => self.advance(),
        }
    }

    #[inline]
    fn line_view(&mut self) -> Line<'_, F> {
        self.current_line()
    }

    #[inline]
    fn resolve_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        self.resolve_optional_typed_mapping(names, aliases)
    }

    #[inline]
    fn decode_through<'record, S>(
        &'record mut self,
        mapping: &TypedMapping,
        sink: S,
    ) -> Result<S::Output, Error>
    where
        S: DecodeSink<'record>,
    {
        self.decode_with_mapping(mapping, sink)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{Csv, Tsv};

    #[test]
    fn slice_parser_methods_and_errors() {
        let input = b"\xEF\xBB\xBFcity,pop\nBoston,650706\n";
        // Rejected BOM on build across formats
        assert!(
            SliceParser::<Csv>::build(
                input,
                FormatOptions::CSV.read_bom(ReadBom::Reject),
                ParseOptions::new()
            )
            .is_err()
        );
        assert!(
            SliceParser::<Tsv>::build(
                input,
                FormatOptions::TSV.read_bom(ReadBom::Reject),
                ParseOptions::new()
            )
            .is_err()
        );
        assert!(
            SliceParser::with_options(
                input,
                FormatOptions::CSV.read_bom(ReadBom::Reject),
                ParseOptions::new()
            )
            .is_err()
        );

        // Invalid format options in build
        assert!(
            SliceParser::with_options(b"a,b", FormatOptions::CSV.quote(b','), ParseOptions::new())
                .is_err()
        );

        let mut parser =
            SliceParser::<Csv>::new(b"city,pop,city\nBoston,100,Boston\n", ParseOptions::new())
                .unwrap();
        assert!(parser.has_headers());
        assert!(!parser.is_done());
        assert_eq!(parser.header_indices("city").unwrap(), &[0, 2]);

        // Seek validation errors
        let mut bad_field = parser.location();
        bad_field.field = 1;
        assert!(parser.seek(bad_field).is_err());

        let mut bad_byte = parser.location();
        bad_byte.byte = 9999;
        assert!(parser.seek(bad_byte).is_err());

        // set_headers
        let mut custom = ByteRecord::new();
        custom.push_field("c1");
        custom.push_field("c2");
        custom.push_field("c3");
        parser.set_headers(custom);
        assert_eq!(parser.header_index("c1").unwrap(), Some(0));

        #[cfg(feature = "index")]
        {
            assert_eq!(parser.line_for_offset(0), 1);
            parser.advance_line_origin(0, 1);
        }
    }

    #[test]
    fn seek_validates_exact_boundaries_and_restores_location() {
        let input = b"head,value\nfirst,1\nsecond,2\n";
        let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new()).expect("parser");

        let mut invalid_field = Location::START;
        invalid_field.field = 1;
        assert_eq!(
            parser
                .seek(invalid_field)
                .expect_err("field must be zero")
                .to_string(),
            "CSV seek positions must identify a record boundary with field 0",
        );

        let mut past_end = Location::START;
        past_end.byte = input.len() + 1;
        assert_eq!(
            parser
                .seek(past_end)
                .expect_err("byte must be in range")
                .to_string(),
            "CSV seek positions must lie within the input",
        );

        let second = Location {
            byte: b"head,value\nfirst,1\n".len(),
            line: 3,
            record: 2,
            field: 0,
        };
        parser.seek(second).expect("valid record boundary");
        assert_eq!(parser.location(), second);
        let mut line = parser.next_line().expect("advance").expect("second row");
        assert_eq!(
            line.record().expect("record").iter().collect::<Vec<_>>(),
            [b"second".as_slice(), b"2"],
        );
    }

    #[cfg(feature = "index")]
    #[test]
    fn index_origin_uses_the_requested_byte_without_adjustment() {
        let input = b"a\nb\nc\n";
        let mut parser = SliceParser::<Csv>::new(
            input,
            ParseOptions::new().headers(crate::config::Headers::None),
        )
        .expect("parser");
        assert_eq!(
            parser.line_for_offset(1),
            1,
            "the newline byte still belongs to its physical line",
        );
        parser.advance_line_origin(1, 10);
        assert_eq!(parser.line_for_offset(1), 10);
        assert_eq!(parser.line_for_offset(2), 11);

        let mut parser = SliceParser::<Csv>::new(
            input,
            ParseOptions::new().headers(crate::config::Headers::None),
        )
        .expect("parser");
        parser.advance_line_origin(2, 10);
        assert_eq!(parser.line_for_offset(2), 10);
        assert_eq!(parser.line_for_offset(4), 11);
    }
}

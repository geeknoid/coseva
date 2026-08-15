//! Cursor-free record access shared by every parser.
//!
//! Each parser hands out records one at a time through its `next_line` and
//! `next_matching_line` methods. A [`Line`] proves that a record is current,
//! so the views that read it cannot be called out of order and none of them
//! panic. The views stay lazy: a line is only split into fields by the view
//! that asks for it, so a line that is looked at cheaply, or not at all,
//! costs no more than the scan that found it.

use crate::byte_record::ByteRecord;
use crate::encoding::{CsvDecode, CsvDecodeOwned, DecodeSink};
use crate::engine::{Engine, FieldAliases, TypedMapping};
use crate::error::{Error, ErrorKind, Location};
use crate::filter::Predicate;
use crate::format::{CsvFormat, Dynamic};
#[cfg(feature = "std")]
use crate::io_parser::IoParser;
use crate::push::Chunk;
use crate::record::Record;
use crate::slice_parser::SliceParser;
use crate::text_record::TextRecord;
use core::marker::PhantomData;
#[cfg(feature = "std")]
use std::io::Read;

/// The cursor motion and typed-decode hooks a record iterator needs.
///
/// Each parser reaches its next record differently, and the record iterators
/// are written once against this rather than once per parser.
pub(crate) trait LineSource {
    /// The compile-time format this source was specialized for.
    type Format: CsvFormat;

    /// Move to the next record, optionally skipping the ones `predicate`
    /// rejects, and report whether one was reached.
    fn advance_line(&mut self, predicate: Option<&Predicate>) -> Result<bool, Error>;

    /// View the record the last successful [`Self::advance_line`] reached.
    fn line_view(&mut self) -> Line<'_, Self::Format>;

    /// Advance under `predicate` and expose the record when one was reached.
    #[inline]
    fn next_line_view(
        &mut self,
        predicate: Option<&Predicate>,
    ) -> Result<Option<Line<'_, Self::Format>>, Error> {
        if self.advance_line(predicate)? {
            Ok(Some(self.line_view()))
        } else {
            Ok(None)
        }
    }

    /// Resolve `names` against the header record, once per iterator run.
    ///
    /// `aliases` carries the alternate spellings each name also accepts.
    fn resolve_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error>;

    /// Decode the current record through a mapping resolved earlier.
    fn decode_through<'record, S>(
        &'record mut self,
        mapping: &TypedMapping,
        sink: S,
    ) -> Result<S::Output, Error>
    where
        S: DecodeSink<'record>;
}

/// A record the parser has reached but not yet interpreted.
///
/// Holding a `Line` is the proof that a record is current: it is only handed
/// out after the parser has positioned itself on one, and it borrows the
/// parser for as long as it lives, so the parser cannot move on while a view
/// of the record is still outstanding.
///
/// Every view is repeatable and mixable, because the record stays resident in
/// the parser's window until the line is dropped and the parser advances.
///
/// The type is the same whatever produced it, so a helper written against
/// `Line` works for every parser:
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::format::CsvFormat;
/// use coseva::{Error, Line, PushParser, SliceParser};
///
/// fn first_field<F: CsvFormat>(line: &mut Line<'_, F>) -> Result<Vec<u8>, Error> {
///     Ok(line.record()?.get(0).unwrap_or_default().to_vec())
/// }
///
/// let mut slice = SliceParser::<Csv>::new(b"city\nBoston\n", ParseOptions::new())?;
/// let mut push = PushParser::<Csv>::new(ParseOptions::new())?;
/// push.finish();
/// let mut chunk = push.chunk(b"city\nBoston\n");
///
/// let from_slice = first_field(
///     &mut slice
///         .next_line()?
///         .ok_or_else(|| std::io::Error::other("expected slice record"))?,
/// )?;
/// let from_push = first_field(
///     &mut chunk
///         .next_line()?
///         .ok_or_else(|| std::io::Error::other("expected pushed record"))?,
/// )?;
/// assert_eq!(from_slice, from_push);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct Line<'parser, F: CsvFormat = Dynamic> {
    /// The engine holding the span of the located record.
    core: &'parser mut Engine,
    /// The bytes the located record's spans point into.
    input: &'parser [u8],
    /// Stream bytes dropped from the front of `input`, added back to the
    /// positions a view reports. Zero for a parser that never drops any.
    consumed: usize,
    /// The producing parser's poison flag, for parsers that have one.
    failed: Option<&'parser mut bool>,
    /// Whether a leading mark was rejected, which the views report rather than
    /// the refill that discovered it.
    bom_rejected: bool,
    /// The compile-time format, if any, the producing parser was built for.
    marker: PhantomData<fn() -> F>,
}

#[inline]
fn line_fail_with(failed: Option<&mut bool>, consumed: usize, error: Error) -> Error {
    if let Some(failed) = failed {
        *failed = true;
    }
    rebase(error, consumed)
}

#[inline]
fn line_check_bom(
    bom_rejected: bool,
    failed: Option<&mut bool>,
    consumed: usize,
) -> Result<(), Error> {
    if bom_rejected {
        return Err(line_fail_with(failed, consumed, rejected_bom()));
    }
    Ok(())
}

impl<'parser, F: CsvFormat> Line<'parser, F> {
    /// Build a line over a located record.
    pub(crate) fn new(
        core: &'parser mut Engine,
        input: &'parser [u8],
        consumed: usize,
        failed: Option<&'parser mut bool>,
        bom_rejected: bool,
    ) -> Self {
        Self {
            core,
            input,
            consumed,
            failed,
            bom_rejected,
            marker: PhantomData,
        }
    }

    /// Poison the producing parser, if it tracks failure, and hand back the
    /// reason positioned against the stream.
    #[inline]
    fn fail(&mut self, error: Error) -> Error {
        line_fail_with(self.failed.as_deref_mut(), self.consumed, error)
    }

    /// Refuse every view of a record that opens with a rejected mark.
    #[inline]
    fn check_bom(&mut self) -> Result<(), Error> {
        line_check_bom(self.bom_rejected, self.failed.as_deref_mut(), self.consumed)
    }

    /// Borrow the line as a record, without copying its fields.
    ///
    /// This is the cheapest way to read a record. Fields point straight into
    /// the input, so nothing is copied and nothing is allocated; only a field
    /// that needed unescaping is materialized, into storage the parser reuses
    /// across records.
    ///
    /// The returned [`Record`] borrows the parser, so it is valid until the
    /// next record is requested. To keep a record beyond that, use
    /// [`Self::read_byte_record_into`], [`Self::read_text_record_into`], or
    /// [`Self::decoded`].
    ///
    /// Byte ranges reported by the record are absolute positions in the input
    /// or stream, so they agree with the owned views and with
    /// [`Error::location`](crate::Error::location).
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the validated span view must scalarize into borrowed record iteration"
    )]
    pub fn record(&mut self) -> Result<Record<'_>, Error> {
        self.check_bom()?;
        let consumed = self.consumed;
        let failed = self.failed.as_deref_mut();
        match self.core.record::<F>(self.input) {
            Ok(mut record) => {
                record.rebase(consumed);
                Ok(record)
            }
            Err(error) => Err(line_fail_with(failed, consumed, error)),
        }
    }

    /// Read the line into a reusable owned record.
    ///
    /// Use this when a record has to outlive the parser position — to collect
    /// records, to hand one to another thread, or to keep one across further
    /// reads. Reuse a single [`ByteRecord`] across the loop and steady-state
    /// reads do not allocate: `output` is refilled in place, keeping the
    /// capacity it already has.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::{ByteRecord, SliceParser};
    ///
    /// let mut parser = SliceParser::<Csv>::new(b"city\nBoston\nDenver\n", ParseOptions::new())?;
    /// let mut record = ByteRecord::new();
    ///
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected first record"))?;
    /// line.read_byte_record_into(&mut record)?;
    /// assert_eq!(record.get(0), Some(&b"Boston"[..]));
    ///
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected second record"))?;
    /// line.read_byte_record_into(&mut record)?;
    /// assert_eq!(record.get(0), Some(&b"Denver"[..]));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline]
    pub fn read_byte_record_into(&mut self, output: &mut ByteRecord) -> Result<(), Error> {
        self.check_bom()?;
        self.core
            .read_byte_record_into::<F>(self.input, output)
            .map_err(|error| self.fail(error))?;
        rebase_record(output, self.consumed);
        Ok(())
    }

    /// Read the line into a reusable owned record, validating UTF-8.
    ///
    /// The same reuse rules as [`Self::read_byte_record_into`] apply; this
    /// additionally rejects fields that are not valid UTF-8, so the resulting
    /// [`TextRecord`] hands out `&str` without further checks.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::{SliceParser, TextRecord};
    ///
    /// let mut parser = SliceParser::<Csv>::new("city\nKøbenhavn\n".as_bytes(), ParseOptions::new())?;
    /// let mut record = TextRecord::new();
    ///
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// line.read_text_record_into(&mut record)?;
    /// assert_eq!(record.get(0), Some("København"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a parse error or the first invalid UTF-8 field.
    #[inline]
    pub fn read_text_record_into(&mut self, output: &mut TextRecord) -> Result<(), Error> {
        self.check_bom()?;
        self.core
            .read_text_record_into::<F>(self.input, output)
            .map_err(|error| self.fail(error))?;
        let range = output.byte_range();
        let consumed = self.consumed;
        output.set_location(
            range.start.saturating_add(consumed)..range.end.saturating_add(consumed),
            output.index(),
        );
        Ok(())
    }

    /// Decode the line, permitting fields borrowed from parser storage.
    ///
    /// Only the fields the target type names are materialized, so a projected
    /// type never pays for the columns it ignores.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// # #[cfg(feature = "derive")] {
    /// use coseva::SliceParser;
    /// use coseva::encoding::CsvDecode;
    ///
    /// #[derive(CsvDecode)]
    /// struct City<'row> {
    ///     name: &'row str,
    ///     population: u64,
    /// }
    ///
    /// let mut parser = SliceParser::<Csv>::new(b"name,population\nBoston,650706\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let city: City<'_> = line.decoded()?;
    /// assert_eq!(city.name, "Boston");
    /// assert_eq!(city.population, 650_706);
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a parse or typed conversion error.
    #[inline]
    pub fn decoded<'record, T>(&'record mut self) -> Result<T, Error>
    where
        T: CsvDecode<'record>,
    {
        self.check_bom()?;
        let consumed = self.consumed;
        let failed = self.failed.as_deref_mut();
        self.core
            .decoded::<_, F>(self.input)
            .map_err(|error| line_fail_with(failed, consumed, error))
    }

    /// Decode the line into a caller-owned value, reusing its allocations.
    ///
    /// Types deriving [`CsvDecode`] decode each field in place, so heap-bearing
    /// field types such as `String` and `Vec<u8>` overwrite their existing
    /// buffers instead of allocating per record.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// # #[cfg(feature = "derive")] {
    /// use coseva::SliceParser;
    /// use coseva::encoding::CsvDecode;
    ///
    /// #[derive(CsvDecode, Default)]
    /// struct City {
    ///     name: String,
    ///     population: u64,
    /// }
    ///
    /// let mut parser = SliceParser::<Csv>::new(
    ///     b"name,population\nBoston,650706\nDenver,715522\n",
    ///     ParseOptions::new(),
    /// )?;
    /// let mut city = City::default();
    ///
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected first record"))?;
    /// line.decode_into(&mut city)?;
    /// assert_eq!(city.name, "Boston");
    ///
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected second record"))?;
    /// line.decode_into(&mut city)?;
    /// assert_eq!(city.name, "Denver");
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a parse or typed conversion error. On error `output` is left in
    /// an unspecified but valid state, because fields are decoded in
    /// declaration order and already-decoded fields are not rolled back.
    #[inline]
    pub fn decode_into<T>(&mut self, output: &mut T) -> Result<(), Error>
    where
        T: CsvDecodeOwned,
    {
        self.check_bom()?;
        self.core
            .decode_into::<_, F>(self.input, output)
            .map_err(|error| self.fail(error))
    }

    /// Deserialize the line with Serde, permitting fields borrowed from parser
    /// storage.
    ///
    /// The returned value may contain `&str` or `&[u8]` references into the
    /// parser's storage, so it cannot outlive the line.
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
    /// let mut parser = SliceParser::<Csv>::new(b"name,population\nBoston,650706\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let city: City<'_> = line.deserialized()?;
    /// assert_eq!(city.name, "Boston");
    /// assert_eq!(city.population, 650_706);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be deserialized into `T`, or
    /// propagates a parse error from the underlying engine.
    #[cfg(feature = "serde")]
    #[inline]
    pub fn deserialized<'record, T>(&'record mut self) -> Result<T, Error>
    where
        T: ::serde::Deserialize<'record>,
    {
        let consumed = self.consumed;
        let failed = self.failed.as_deref_mut();
        self.core
            .deserialized_line::<_, F>(self.input, self.bom_rejected)
            .map_err(|error| line_fail_with(failed, consumed, error))
    }
}

/// The report for a stream that opens with a rejected byte-order mark.
fn rejected_bom() -> Error {
    Error::new(ErrorKind::RejectedBom, Location::START)
}

/// Convert a window-relative error into a stream-relative one.
///
/// Line numbers are already stream-absolute, because the newlines of every
/// dropped prefix were folded into the engine's line base as it was dropped.
/// A parser that never drops a prefix passes zero, making this the identity.
fn rebase(mut error: Error, consumed: usize) -> Error {
    error.rebase_stream_window(consumed);
    error
}

/// Convert an owned record's window-relative extent into a stream-relative one.
pub(crate) fn rebase_record(record: &mut ByteRecord, consumed: usize) {
    let range = record.byte_range();
    let index = record.index();
    record.set_location(
        range.start.saturating_add(consumed)..range.end.saturating_add(consumed),
        index,
    );
}

impl<F: CsvFormat> SliceParser<'_, F> {
    /// Move to the next line, without parsing it.
    ///
    /// Returns `None` at end of input, after which further calls keep
    /// returning `None`. Parsing is deferred to whichever view of the line is
    /// called next, so each view runs only the work it needs.
    ///
    /// # Errors
    ///
    /// Returns a parse error raised while discovering headers, or when the
    /// parser has already failed.
    #[inline]
    pub fn next_line(&mut self) -> Result<Option<Line<'_, F>>, Error> {
        self.next_line_view(None)
    }

    /// Move to the next line satisfying `predicate`, without parsing it.
    ///
    /// A record that does not match is skipped without being split into
    /// fields, so most of the document costs almost nothing when matches are
    /// rare. Returns `None` once no matching record remains. See
    /// [`Predicate`].
    ///
    /// # When it pays
    ///
    /// This is an optimization for selective predicates, not a free one.
    /// Rejecting a record costs about a seventh of what parsing it costs, but
    /// accepting one costs slightly more than reading it with
    /// [`Self::next_line`] and testing the field yourself, because the filter
    /// locates the record before handing it over. Measured against a hand
    /// written loop over the same bytes, the two break even at about 89%
    /// selectivity: below that this wins, and by a wide margin as matches get
    /// rare, while a predicate that accepts nearly everything costs about 11%
    /// more than not filtering at all. `benches/filter.rs` is the measurement.
    ///
    /// # Errors
    ///
    /// Returns a parse error raised while discovering headers or while walking
    /// the records the scan passes over, or an error if the parser has already
    /// failed. Skipping a record never suppresses its errors.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::{Predicate, SliceParser};
    ///
    /// let predicate = Predicate::equals("city", "Boston");
    /// let mut parser = SliceParser::<Csv>::new(b"city,pop\nBoston,650706\nLondon,8982000\n", ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_matching_line(&predicate)?
    ///     .ok_or_else(|| std::io::Error::other("expected a matching record"))?;
    /// assert_eq!(line.record()?.get(1), Some(&b"650706"[..]));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    #[inline]
    pub fn next_matching_line(
        &mut self,
        predicate: &Predicate,
    ) -> Result<Option<Line<'_, F>>, Error> {
        self.next_line_view(Some(predicate))
    }
}

#[cfg(feature = "std")]
impl<R: Read, F: CsvFormat> IoParser<R, F> {
    /// Move to the next line, without parsing it.
    ///
    /// Returns `None` at end of input, after which further calls keep
    /// returning `None`.
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, or width error.
    #[inline]
    pub fn next_line(&mut self) -> Result<Option<Line<'_, F>>, Error> {
        self.next_line_view(None)
    }

    /// Move to the next line satisfying `predicate`, without parsing it.
    ///
    /// A record that does not match is skipped without being split into
    /// fields, so most of the document costs almost nothing when matches are
    /// rare. Returns `None` once no matching record remains. See
    /// [`Predicate`].
    ///
    /// # When it pays
    ///
    /// This is an optimization for selective predicates, not a free one.
    /// Rejecting a record costs about a seventh of what parsing it costs, but
    /// accepting one costs slightly more than reading it with
    /// [`Self::next_line`] and testing the field yourself, because the filter
    /// locates the record before handing it over. Measured against a hand
    /// written loop over the same bytes, the two break even at about 89%
    /// selectivity: below that this wins, and by a wide margin as matches get
    /// rare, while a predicate that accepts nearly everything costs about 11%
    /// more than not filtering at all. `benches/filter.rs` is the measurement.
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, or width error.
    #[inline]
    pub fn next_matching_line(
        &mut self,
        predicate: &Predicate,
    ) -> Result<Option<Line<'_, F>>, Error> {
        self.next_line_view(Some(predicate))
    }
}

impl<F: CsvFormat> Chunk<'_, '_, F> {
    /// Move to the next line the chunk holds, without parsing it.
    ///
    /// Returns `None` once the chunk completes no further record, which is a
    /// pause rather than an end of input unless [`crate::PushParser::finish`] has
    /// declared the stream over. A line reached this way usually borrows the
    /// chunk directly, so reading it copies nothing.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits, or
    /// when the parser has already failed.
    #[inline]
    pub fn next_line(&mut self) -> Result<Option<Line<'_, F>>, Error> {
        // Once the chunk is borrowed every record comes from the same slice
        // under the same parser, so reaching one and viewing it are fused
        // here: split across `advance` and `current_line` the shared state is
        // re-read and the borrow re-tested for every record.
        match self.borrowed() {
            true => self.next_borrowed_line(),
            false => {
                if self.advance()? {
                    Ok(Some(self.current_line()))
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// Move to the next line the chunk holds that satisfies `predicate`.
    ///
    /// A record that does not match is skipped without being kept; see
    /// [`Predicate`]. As with [`Self::next_line`], `None` is a pause rather
    /// than an end of input, so a later chunk can still produce a match.
    ///
    /// # When it pays
    ///
    /// This is an optimization for selective predicates, not a free one.
    /// Rejecting a record costs about a seventh of what parsing it costs, but
    /// accepting one costs slightly more than reading it with
    /// [`Self::next_line`] and testing the field yourself, because the filter
    /// locates the record before handing it over. Measured against a hand
    /// written loop over the same bytes, the two break even at about 89%
    /// selectivity: below that this wins, and by a wide margin as matches get
    /// rare, while a predicate that accepts nearly everything costs about 11%
    /// more than not filtering at all. `benches/filter.rs` is the measurement.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits, or
    /// when the parser has already failed.
    #[inline]
    pub fn next_matching_line(
        &mut self,
        predicate: &Predicate,
    ) -> Result<Option<Line<'_, F>>, Error> {
        if self.advance_with_filter(predicate)? {
            Ok(Some(self.current_line()))
        } else {
            Ok(None)
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FormatOptions, Headers, ParseOptions, ReadBom};

    #[test]
    fn test_line_bom_rejected_views() {
        let bom_data = b"\xEF\xBB\xBFa,b\n1,2\n";
        let mut parser = IoParser::with_options(
            &bom_data[..],
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();

        let mut line = parser.next_line().unwrap().unwrap();
        assert!(line.record().is_err());

        let mut parser = IoParser::with_options(
            &bom_data[..],
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut line = parser.next_line().unwrap().unwrap();
        let mut br = ByteRecord::new();
        assert!(line.read_byte_record_into(&mut br).is_err());

        let mut parser = IoParser::with_options(
            &bom_data[..],
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut line = parser.next_line().unwrap().unwrap();
        let mut tr = TextRecord::new();
        assert!(line.read_text_record_into(&mut tr).is_err());

        #[cfg(feature = "serde")]
        {
            let mut parser = IoParser::with_options(
                &bom_data[..],
                FormatOptions::CSV.read_bom(ReadBom::Reject),
                ParseOptions::new().headers(Headers::None),
            )
            .unwrap();
            let mut line = parser.next_line().unwrap().unwrap();
            #[derive(serde::Deserialize)]
            struct Row(
                #[expect(dead_code, reason = "test struct fields")] String,
                #[expect(dead_code, reason = "test struct fields")] String,
            );
            assert!(line.deserialized::<Row>().is_err());
        }

        // decode_into error path with IoParser line
        struct FailDecode;
        impl<'record> crate::encoding::CsvDecode<'record> for FailDecode {
            fn csv_decode<R>(_record: &R) -> Result<Self, Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                Err(Error::detailed(ErrorKind::Decode, "decode error"))
            }
            fn field_names() -> &'static [&'static str] {
                &["a"]
            }
        }
        let data = b"not_a_num\n";
        let mut parser_num = IoParser::with_options(
            &data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut line = parser_num.next_line().unwrap().unwrap();
        let mut fail_val = FailDecode;
        assert!(line.decode_into(&mut fail_val).is_err());
        assert!(parser_num.is_done());
    }

    #[test]
    fn rebasing_changes_only_the_window_relative_byte_and_record() {
        let error = Error::new(
            ErrorKind::UnexpectedQuote,
            Location {
                byte: 3,
                line: 4,
                record: 9,
                field: 2,
            },
        );
        let rebased = rebase(error, 11);
        assert_eq!(
            rebased.location(),
            Location {
                byte: 14,
                line: 4,
                record: 9,
                field: 2,
            }
        );
    }

    #[test]
    fn a_borrowed_push_chunk_yields_each_record() {
        let mut parser = crate::PushParser::<crate::format::Csv>::new(
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options");
        let mut chunk = parser.chunk(b"a,b\nc,d\n");
        assert!(chunk.borrowed());

        let mut first = chunk.next_line().expect("advance").expect("first");
        assert_eq!(first.record().expect("record").get(0), Some(&b"a"[..]));
        let mut second = chunk.next_line().expect("advance").expect("second");
        assert_eq!(second.record().expect("record").get(0), Some(&b"c"[..]));
        assert!(chunk.next_line().expect("advance").is_none());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn streaming_deserialization_rebases_conversion_errors_exactly() {
        #[derive(Debug, serde::Deserialize)]
        struct Row {
            value: u32,
        }

        let input = b"value\n1\n2\nx\n";
        let mut parser = IoParser::with_options(
            &input[..],
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(1),
        )
        .expect("valid options");

        for expected in [1_u32, 2] {
            let mut line = parser.next_line().expect("advance").expect("record");
            let row: Row = line.deserialized().expect("valid row");
            assert_eq!(row.value, expected);
        }
        let mut line = parser.next_line().expect("advance").expect("invalid row");
        let error = line
            .deserialized::<Row>()
            .expect_err("invalid integer must fail");
        assert_eq!(error.location().byte, 10);
    }
}

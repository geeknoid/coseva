//! Buffered CSV parser over an arbitrary reader.

use core::cmp::Ordering;
use core::marker::PhantomData;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::byte_record::ByteRecord;
use crate::config::{BlankRecords, FormatOptions, Headers, ParseOptions, ParserSettings, ReadBom};
use crate::encoding::DecodeSink;
use crate::engine::{Advance, BOM, Engine, FILTER_BACKOFF, FieldAliases, TypedMapping};
use crate::error::{Error, ErrorKind, Location};
use crate::filter::{Column, Predicate};
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::line::{Line, LineSource, rebase_record};
use crate::reclaim::should_reclaim;
use crate::search::{count1, find_literal, find1, rfind1};
use crate::text_record::TextRecord;

/// Buffered CSV parser over an arbitrary [`Read`] source.
///
/// This is the reader for files, sockets, pipes, and anything else that
/// arrives a chunk at a time. It reads into a bounded buffer and hands you
/// records that borrow from it directly, so reading a document of any size
/// uses a fixed amount of memory and does not copy field data.
///
/// Because a record borrows from that buffer, it is valid only until the next
/// record is requested. Use [`Self::read_byte_record_into`] or
/// [`Self::read_text_record_into`] for the fastest reusable owned-record loop,
/// or a [`Line`] view when the representation varies by record.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::IoParser;
///
/// let mut parser = IoParser::<_, Csv>::new(&b"city,pop\nBoston,650706\n"[..], ParseOptions::new())?;
/// let mut cities = Vec::new();
/// while let Some(mut line) = parser.next_line()? {
///     let record = line.record()?.get(0).unwrap_or_default().to_vec();
///     cities.push(record);
/// }
/// assert_eq!(cities, [b"Boston".to_vec()]);
/// # Ok::<(), coseva::Error>(())
/// ```
#[expect(
    clippy::struct_excessive_bools,
    reason = "cached stream-policy flags avoid repeated work in the record hot path"
)]
#[cfg(feature = "std")]
#[derive(Debug)]
pub struct IoParser<R, F: CsvFormat = Dynamic> {
    input: R,
    marker: PhantomData<fn() -> F>,
    /// The read buffer. `window[..filled]` is stream data the engine scans;
    /// the rest is spare capacity that has already been initialized, so a
    /// refill reads straight into it instead of staging through a temporary.
    window: Vec<u8>,
    core: Engine,
    /// Bytes of `window` holding stream data.
    filled: usize,
    /// Stream bytes already dropped from the front of the window.
    consumed: usize,
    eof: bool,
    failed: bool,
    /// Whether [`IoParser::advance`] has reported the end of input.
    done: bool,
    /// Whether record boundaries need parsing to be recognized, which makes
    /// the scan report parse errors itself rather than leave them to a view.
    eager: bool,
    /// Spare capacity a refill makes room for when the window is full.
    read_size: usize,
    /// The configured byte-order-mark policy.
    bom: ReadBom,
    /// Whether a leading byte-order mark has been decided on.
    bom_resolved: bool,
    /// Whether a leading mark was found under [`ReadBom::Reject`], which the
    /// views report rather than the refill that discovered it.
    bom_rejected: bool,
    /// Whether the header record comes from the first record of the stream.
    discovers_headers: bool,
    /// Whether the engine has settled the header record, so the header views
    /// can read it without parsing a window that may still grow.
    headers_resolved: bool,
    /// Stream offset through which a filter scan proved its literal absent.
    filter_scanned_to: usize,
    /// Records left to walk before the filter scan is probed again.
    filter_backoff: u32,
}

#[cfg(feature = "std")]
impl<R: Read> IoParser<R, Dynamic> {
    /// Create a buffered parser for an explicit format and parse options.
    ///
    /// ```
    /// use coseva::config::{FormatOptions, ParseOptions};
    /// use coseva::IoParser;
    ///
    /// let mut parser = IoParser::with_options(
    ///     &b"city;pop\nBoston;650706\n"[..],
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
    /// # Borrowing the reader
    ///
    /// `input` is taken by value, and [`into_inner`](Self::into_inner) hands it back. A
    /// caller that must keep the reader can pass `&mut reader` instead, since
    /// `&mut R` implements [`Read`] wherever `R: Read` does.
    ///
    /// # Errors
    ///
    /// Returns an error when the format or buffer capacity is invalid.
    pub fn with_options(
        input: R,
        format: FormatOptions,
        options: ParseOptions,
    ) -> Result<Self, Error> {
        Ok(Self::from_config(input, options.into_settings(format)?))
    }

    pub(crate) fn from_config(input: R, options: ParserSettings) -> Self {
        Self::from_config_for(input, options)
    }
}

#[cfg(feature = "std")]
impl<R: Read, F: StaticFormat> IoParser<R, F> {
    /// Create a buffered parser specialized to the format named by `F`.
    ///
    /// The format bytes fold to immediates, so the parser avoids reloading
    /// them on every field. The format comes from the type, so there is no
    /// format argument.
    ///
    /// ```
    /// use coseva::config::ParseOptions;
    /// use coseva::format::Tsv;
    /// use coseva::IoParser;
    ///
    /// let mut parser = IoParser::<_, Tsv>::new(&b"city\tpop\nBoston\t650706\n"[..], ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the reader
    ///
    /// `input` is taken by value, and [`into_inner`](Self::into_inner) hands it
    /// back. A caller that cannot give the reader up can pass `&mut reader`
    /// instead, since `&mut R` implements [`Read`] wherever `R: Read` does.
    ///
    /// ```
    /// use std::io::Cursor;
    ///
    /// use coseva::config::ParseOptions;
    /// use coseva::format::Tsv;
    /// use coseva::IoParser;
    ///
    /// let mut input = Cursor::new(b"city\tpop\nBoston\t650706\n".to_vec());
    ///
    /// // The parser borrows the reader rather than consuming it.
    /// let mut parser = IoParser::<_, Tsv>::new(&mut input, ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// drop(parser);
    ///
    /// // `input` was never surrendered, so it is still usable here.
    /// assert_eq!(input.into_inner().len(), 23);
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the parse options or buffer capacity are invalid.
    pub fn new(input: R, options: ParseOptions) -> Result<Self, Error> {
        Ok(Self::from_config_for(
            input,
            options.into_settings(F::FORMAT)?,
        ))
    }
}

#[cfg(feature = "std")]
impl<R: Read, F: CsvFormat> IoParser<R, F> {
    fn from_config_for(input: R, options: ParserSettings) -> Self {
        let read_size = options.buffer_capacity;
        let bom = options.bom;
        let discovers_headers = options.headers == Headers::FirstRecord;
        let dialect = options.dialect;
        let blank_records = options.blank_records;
        Self {
            input,
            marker: PhantomData,
            // Eager allocation keeps setup out of the first record and lets
            // refills read directly into the window.
            window: vec![0; read_size],
            core: Engine::from_config_windowed(&[], options),
            filled: 0,
            consumed: 0,
            eof: false,
            failed: false,
            done: false,
            eager: dialect.comment.is_some() || blank_records == BlankRecords::Skip,
            read_size,
            bom,
            bom_resolved: bom == ReadBom::Preserve,
            bom_rejected: false,
            discovers_headers,
            headers_resolved: false,
            filter_scanned_to: 0,
            filter_backoff: 0,
        }
    }

    /// Move to the next record, without parsing it.
    ///
    /// Returns `false` at end of input, after which further calls keep
    /// returning `false`. Parsing is deferred to whichever view is called
    /// next, so each view runs only the work it needs.
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, or width error.
    #[inline]
    pub(crate) fn advance(&mut self) -> Result<bool, Error> {
        if !self.headers_resolved || !self.bom_resolved || self.failed {
            self.ensure_headers()?;
        }
        loop {
            let filled = self.filled;
            let consumed = self.consumed;
            let window = &self.window[..filled];
            // A rejected leading mark is refused by the view, but the eager
            // scan parses the record to skip comments and blank lines and
            // could raise a syntax error from inside the mark first. Positioning
            // the mark's record lazily leaves the refusal to the view, so every
            // path reports `RejectedBom` before any downstream syntax error.
            let scanned = if self.eager && !self.bom_rejected {
                self.core.advance_window_eagerly::<F>(window, self.eof)
            } else {
                self.core.advance_window_lazily::<F>(window, self.eof)
            };
            match scanned {
                Ok(Advance::Record) => return Ok(true),
                Ok(Advance::Done) => {
                    self.done = true;
                    return Ok(false);
                }
                Ok(Advance::NeedMore) => self.refill()?,
                Err(error) => return Err(self.fail(rebase(error, consumed))),
            }
        }
    }

    /// Read the next record directly into reusable owned byte storage.
    ///
    /// Returns `false` at end of input. Reusing one record keeps steady-state
    /// reads allocation-free and avoids constructing an intermediate
    /// [`Line`] or staging a second [`ByteRecord`].
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, width, or byte-order-mark error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn read_byte_record_into(&mut self, output: &mut ByteRecord) -> Result<bool, Error> {
        self.ensure_headers()?;
        if self.bom_rejected {
            return Err(self.fail(rejected_bom()));
        }
        loop {
            let filled = self.filled;
            let consumed = self.consumed;
            match self
                .core
                .read_window_owned::<F>(&self.window[..filled], self.eof, output)
            {
                Ok(Advance::Record) => {
                    rebase_record(output, consumed);
                    return Ok(true);
                }
                Ok(Advance::Done) => {
                    self.done = true;
                    return Ok(false);
                }
                Ok(Advance::NeedMore) => self.refill()?,
                Err(error) => return Err(self.fail(rebase(error, consumed))),
            }
        }
    }

    /// Read the next record directly into reusable validated UTF-8 storage.
    ///
    /// Returns `false` at end of input. This is the text counterpart to
    /// [`Self::read_byte_record_into`]: advancement, owned materialization,
    /// and UTF-8 validation are fused without staging an intermediate record.
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, width, byte-order-mark, or UTF-8 error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[inline]
    pub fn read_text_record_into(&mut self, output: &mut TextRecord) -> Result<bool, Error> {
        if !self.headers_resolved || !self.bom_resolved || self.failed {
            self.ensure_headers()?;
        }
        if self.bom_rejected {
            return Err(self.fail(rejected_bom()));
        }
        loop {
            let filled = self.filled;
            let consumed = self.consumed;
            match self
                .core
                .read_window_text::<F>(&self.window[..filled], self.eof, output)
            {
                Ok(Advance::Record) => {
                    output.rebase_location(consumed);
                    return Ok(true);
                }
                Ok(Advance::Done) => {
                    self.done = true;
                    return Ok(false);
                }
                Ok(Advance::NeedMore) => self.refill()?,
                Err(error) => return Err(self.fail(rebase(error, consumed))),
            }
        }
    }

    /// Parse the next record satisfying `predicate`, skipping the rest.
    ///
    /// This is the I/O counterpart to [`SliceParser::next_matching_line`].
    /// The literal is searched for directly in the read buffer, so records
    /// that cannot match are never split into fields. Matching is exact: a
    /// candidate located by the scan is fully parsed and evaluated before it
    /// is returned.
    ///
    /// A predicate naming a header that does not exist yields no records.
    ///
    /// # Errors
    ///
    /// Returns an I/O, syntax, limit, or width error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(crate) fn advance_with_filter(&mut self, predicate: &Predicate) -> Result<bool, Error> {
        self.check_failed()?;
        self.ensure_headers()?;

        let column = match predicate.column() {
            Column::Index(index) => Some(*index),
            Column::Name(name) => {
                let name = name.as_bytes();
                if let cached @ Some(_) = self.core.cached_filter_column(name) {
                    cached
                } else {
                    let column = self.header_index(name)?;
                    if let Some(column) = column {
                        self.core.store_filter_column(name, column);
                    }
                    column
                }
            }
        };
        let Some(column) = column else {
            return Ok(false);
        };

        let literal = self.core.skip_literal_for::<F>(predicate);
        loop {
            if let Some(literal) = literal {
                self.skip_to_candidate(literal);
            }
            if !self.advance()? {
                return Ok(false);
            }
            let consumed = self.consumed;
            let filled = self.filled;
            let matched = match self.core.field::<F>(&self.window[..filled], column) {
                Ok(field) => predicate.matches_field(field),
                Err(error) => {
                    self.failed = true;
                    return Err(rebase(error, consumed));
                }
            };
            if matched {
                return Ok(true);
            }
        }
    }

    /// Return the configured or discovered headers.
    ///
    /// # Errors
    ///
    /// Returns an I/O or parse error while discovering first-record headers.
    pub fn headers(&mut self) -> Result<Option<&ByteRecord>, Error> {
        self.ensure_headers()?;
        Ok(self
            .core
            .headers(&self.window[..self.filled])
            .ok()
            .flatten())
    }

    /// Resolve the first header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns an error while discovering first-record headers.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn header_index(&mut self, name: impl AsRef<[u8]>) -> Result<Option<usize>, Error> {
        self.ensure_headers()?;
        Ok(self
            .core
            .header_index(&[], name)
            .expect("headers were initialized by ensure_headers"))
    }

    /// Resolve every duplicate header with the requested name.
    ///
    /// # Errors
    ///
    /// Returns an error while discovering first-record headers.
    pub fn header_indices(&mut self, name: impl AsRef<[u8]>) -> Result<&[usize], Error> {
        self.ensure_headers()?;
        Ok(self
            .core
            .header_indices(&[], name)
            .expect("headers were initialized by ensure_headers"))
    }

    /// Whether this parser uses discovered or caller-provided headers.
    #[must_use]
    pub fn has_headers(&self) -> bool {
        self.core.has_headers()
    }

    /// Replace the header record without consuming input.
    ///
    /// Subsequent named decoding uses this record, and the next input record
    /// is treated as data.
    pub fn set_headers(&mut self, headers: ByteRecord) {
        self.core.set_headers(headers);
        self.discovers_headers = bool::default();
    }

    /// Current position in the stream.
    ///
    /// The byte offset counts from the start of the whole stream, not from the
    /// start of the buffer, so it stays meaningful for a stream far larger
    /// than memory and can be handed back to [`Self::seek`].
    ///
    /// After a record has been read, the offset sits just past that record.
    /// Between [`Self::next_line`] and reading the record it positioned on,
    /// the offset may already have advanced past it.
    #[must_use]
    pub fn location(&self) -> Location {
        let mut location = self.core.location(&self.window[..self.filled]);
        location.byte = self.consumed.saturating_add(location.byte);
        location
    }

    /// Whether parsing has reached EOF or stopped after an error.
    ///
    /// EOF becomes observable after an operation attempts to read beyond the
    /// final record.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        match (self.failed, self.core.has_failed(), self.done) {
            (false, false, false) => false,
            _ => true,
        }
    }

    /// Borrow the underlying input.
    #[must_use]
    pub const fn get_ref(&self) -> &R {
        &self.input
    }

    /// Mutably borrow the underlying input.
    ///
    /// The caller must not read from it directly.
    pub const fn get_mut(&mut self) -> &mut R {
        &mut self.input
    }

    /// Consume the parser and return its input.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.input
    }

    #[cfg(feature = "index")]
    pub(crate) fn line_for_offset(&self, byte: usize) -> u64 {
        self.core.line_for_offset(
            &self.window[..self.filled],
            byte.saturating_sub(self.consumed),
        )
    }

    /// Move the physical-line origin onto an already-numbered record boundary.
    #[cfg(feature = "index")]
    pub(crate) fn advance_line_origin(&mut self, byte: usize, line: u64) {
        self.core
            .advance_line_origin(byte.saturating_sub(self.consumed), line);
    }

    /// Settle the header record, reading as much input as that takes.
    fn ensure_headers(&mut self) -> Result<(), Error> {
        self.check_failed()?;
        self.resolve_bom()?;
        match self.headers_resolved {
            true => return Ok(()),
            false => {}
        }
        // A rejected mark sits in the header record, so discovering headers
        // cannot get past it; every other policy leaves the report to a view.
        if self.bom_rejected && self.discovers_headers {
            return Err(self.fail(rejected_bom()));
        }
        loop {
            let filled = self.filled;
            let consumed = self.consumed;
            match self.core.headers_window(&self.window[..filled], self.eof) {
                Ok(true) => {
                    self.headers_resolved = true;
                    return Ok(());
                }
                Ok(false) => self.refill()?,
                Err(error) => return Err(self.fail(rebase(error, consumed))),
            }
        }
    }

    #[inline]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn resolve_optional_typed_mapping(
        &mut self,
        names: &'static [&'static str],
        aliases: FieldAliases,
    ) -> Result<TypedMapping, Error> {
        self.ensure_headers()?;
        let consumed = self.consumed;
        self.core
            .resolve_optional_typed_mapping(&[], names, aliases)
            .map_err(|error| self.fail(rebase(error, consumed)))
    }

    /// Decode the current record through `mapping`.
    #[inline]
    fn decode_with_mapping<'record, S>(
        &'record mut self,
        mapping: &TypedMapping,
        sink: S,
    ) -> Result<S::Output, Error>
    where
        S: DecodeSink<'record>,
    {
        let consumed = self.consumed;
        let filled = self.filled;
        match self
            .core
            .decode_with_mapping::<_, F>(&self.window[..filled], mapping, sink)
        {
            Ok(output) => Ok(output),
            Err(error) => {
                self.failed = true;
                Err(rebase(error, consumed))
            }
        }
    }

    /// Drop the window prefix the engine no longer needs.
    ///
    /// The record most recently reported is retained, because a view may still
    /// borrow it, so the window holds at most that record plus the one being
    /// assembled.
    fn compact(&mut self) {
        let anchor = self.core.io_window_anchor();
        self.core.shift_window(&self.window[..self.filled], anchor);
        self.window.copy_within(anchor..self.filled, 0);
        self.filled -= anchor;
        self.consumed = self.consumed.saturating_add(anchor);
    }

    // gamma::skip(fn_value.ok, reason = "mutation prevents refill progress and causes non-termination")
    /// Widen the window with more of the stream.
    ///
    /// The window is compacted first, then read into directly: the buffer
    /// keeps its spare capacity initialized, so bytes land where they are
    /// parsed instead of being copied in from a staging buffer. A read of zero
    /// bytes ends the stream, which lets the next scan terminate the final
    /// record.
    ///
    /// Records are not bounded here. The engine reports
    /// [`ErrorKind::RecordTooLarge`] and [`ErrorKind::FieldTooLarge`] against
    /// the window as it grows, exactly as it does for the slice parser, so the
    /// window never outgrows the configured limits by more than one read.
    #[cold]
    fn refill(&mut self) -> Result<(), Error> {
        // gamma::skip(cond.always_true, cond.negate, reason = "mutation prevents refill progress and causes non-termination")
        if self.eof {
            return Ok(());
        }
        self.compact();
        // Compaction has just moved the live bytes to the front, so this is
        // where a window blown up by one outsized record is at its emptiest
        // and cheapest to hand back.
        //
        // The test comes first and the window is only disturbed when it
        // passes. Truncating unconditionally would drop the window's
        // initialized spare capacity, and the resize below would then have to
        // re-zero a full read's worth of bytes on every single refill.
        let keep = self.read_size.saturating_add(self.filled);
        if should_reclaim(self.window.capacity(), keep) {
            self.window.truncate(self.filled);
            self.window.shrink_to(keep);
            self.core.reclaim_scratch();
        }
        // Grow when compaction has left too little room to read into, not only
        // when the window is completely full: compaction can free an
        // arbitrarily small sliver, and reading into just that sliver would
        // shrink every subsequent read to far less than the caller asked for.
        //
        // The threshold is half a read rather than a whole one because
        // compaction always keeps the record being assembled, so the room it
        // frees is a read short by exactly that record. Demanding a full read
        // would therefore grow the window on the second refill of every stream
        // and every one after it, reallocating and copying the whole window to
        // win back a few bytes of read size. Half a read keeps the guard
        // against genuine slivers while letting the steady state settle at the
        // capacity the caller configured.
        let read_size = self.read_size;
        if self.window.len() - self.filled < read_size.div_ceil(2) {
            let want = self.filled.saturating_add(read_size);
            self.window.resize(want, u8::default());
        }
        let room = self.window.len() - self.filled;
        loop {
            match self.input.read(&mut self.window[self.filled..]) {
                Ok(0) => {
                    // gamma::skip(stmt.delete_assign, assign_value.default, literal.bool_flip, reason = "mutation prevents EOF progress and causes non-termination")
                    self.eof = true;
                    return Ok(());
                }
                Ok(read) if read <= room => {
                    self.filled += read;
                    return Ok(());
                }
                Ok(_) => {
                    self.failed = true;
                    // gamma::skip(result.err_to_ok, reason = "mutation suppresses a progress error and causes non-termination")
                    return Err(Error::io(
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "Read implementation returned more bytes than the buffer holds",
                        ),
                        self.location(),
                    ));
                }
                // gamma::skip(match_guard.always_true, match_guard.negate, relational.eq_to_ne, reason = "mutation retries permanent I/O errors and causes non-termination")
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    self.failed = true;
                    // gamma::skip(result.err_to_ok, reason = "mutation suppresses a progress error and causes non-termination")
                    return Err(Error::io(error, self.location()));
                }
            }
        }
    }

    /// Strip or note a byte-order mark at the very start of the stream.
    fn resolve_bom(&mut self) -> Result<(), Error> {
        if self.bom_resolved {
            return Ok(());
        }
        // gamma::skip(logical.and_to_or, reason = "mutation continues refilling after EOF and causes non-termination")
        while self.filled < BOM.len() && !self.eof {
            self.refill()?;
        }
        self.bom_resolved = true;
        if !self.window[..self.filled].starts_with(BOM) {
            return Ok(());
        }
        if self.bom == ReadBom::Reject {
            // Reported by the views rather than here, so that positioning on
            // the record carrying the mark still succeeds.
            self.bom_rejected = true;
            return Ok(());
        }
        let filled = self.filled;
        self.core.skip_detected_bom(&self.window[..filled]);
        Ok(())
    }

    /// Advance the read window to a record that may contain `literal`.
    ///
    /// Only whole records already sitting in the window are skipped. Refilling
    /// is left to the parse path, so at most one record per refill is parsed
    /// that the scan could have rejected.
    fn skip_to_candidate(&mut self, literal: &[u8]) {
        // A span already proven free of the literal is never rescanned, which
        // keeps walking a quoted region linear rather than quadratic.
        let start = self.core.byte_offset();
        let absolute = self.consumed.saturating_add(start);
        match (
            self.failed,
            absolute.cmp(&self.filter_scanned_to),
            start.cmp(&self.filled),
        ) {
            (true, _, _) | (_, Ordering::Less, _) | (_, _, Ordering::Greater | Ordering::Equal) => {
                return;
            }
            (false, Ordering::Greater | Ordering::Equal, Ordering::Less) => {}
        }

        // Matches dense enough that scanning never skips anything would make
        // every record pay for a scan, so retry only periodically.
        if self.filter_backoff > 0 {
            self.filter_backoff -= 1;
            return;
        }

        let window = &self.window[start..self.filled];
        let searched = match find_literal(literal, window) {
            // Records before the first occurrence cannot match.
            Some(hit) => &window[..hit],
            // Nothing in this window matches, so every whole record in it goes.
            None => window,
        };
        self.filter_scanned_to = absolute.saturating_add(searched.len());

        // A quote makes record terminators ambiguous, so the parser walks.
        let terminator = self.core.fmt_terminator::<F>();
        let target = if find1(self.core.fmt_quote::<F>(), searched).is_some() {
            None
        } else {
            rfind1(terminator, searched).map(|at| (at, count1(terminator, searched)))
        };

        let Some((at, records)) = target else {
            self.filter_backoff = FILTER_BACKOFF;
            return;
        };
        let through_ending = searched
            .get(..=at)
            .expect("rfind returned an index within searched")
            .len();
        self.core.skip_records(through_ending, records as u64);
    }

    /// View the located record as a [`Line`].
    ///
    /// The record sits in the retained window, so the line carries the count
    /// of dropped stream bytes to report positions against the stream.
    #[inline]
    pub(crate) fn current_line(&mut self) -> Line<'_, F> {
        let filled = self.filled;
        Line::new(
            &mut self.core,
            &self.window[..filled],
            self.consumed,
            Some(&mut self.failed),
            self.bom_rejected,
        )
    }

    /// Refuse further work once the parser has failed.
    fn check_failed(&self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::new(ErrorKind::ParserFailed, self.location()));
        }
        Ok(())
    }

    /// Record that the parser has failed and hand back the reason.
    fn fail(&mut self, error: Error) -> Error {
        self.failed = true;
        error
    }
}

/// The report for a stream that opens with a rejected byte-order mark.
#[cfg(feature = "std")]
fn rejected_bom() -> Error {
    Error::new(ErrorKind::RejectedBom, Location::START)
}

/// Convert a window-relative error into a stream-relative one.
///
/// Line numbers are already stream-absolute, because the newlines of every
/// dropped prefix were folded into the engine's line base as it was dropped.
#[cfg(feature = "std")]
fn rebase(mut error: Error, consumed: usize) -> Error {
    let record = error.location().record;
    error.relocate(consumed, Location::START.line, record);
    error
}

#[cfg(feature = "std")]
impl<R: Read + io::Seek, F: CsvFormat> IoParser<R, F> {
    /// Seek to a previously observed record boundary.
    ///
    /// Discovered or provided headers and the established first-record field count
    /// are preserved. The target's byte offset, physical line, and record
    /// index become the parser's new location. `location.field` must be zero.
    ///
    /// The location should come from [`Self::location`] immediately before
    /// reading a record, or from equivalent validated index metadata. Seeking
    /// to an arbitrary byte inside a record is not supported.
    ///
    /// A successful seek clears EOF and any earlier parse or I/O failure.
    ///
    /// # Errors
    ///
    /// Returns an error while initially discovering headers or field width,
    /// for a nonzero field location, or when the underlying seek fails.
    pub fn seek(&mut self, location: Location) -> Result<(), Error> {
        if location.field != 0 {
            return Err(Error::io(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CSV seek positions must identify a record boundary with field 0",
                ),
                location,
            ));
        }
        self.prepare_seek_state()?;
        let previous = self.location();
        let offset = location.byte as u64;
        let actual = self
            .input
            .seek(io::SeekFrom::Start(offset))
            .map_err(|error| Error::io(error, previous))?;
        if actual != offset {
            self.failed = true;
            return Err(Error::io(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Seek implementation returned an unexpected absolute location",
                ),
                previous,
            ));
        }
        self.reset_stream_position(location);
        Ok(())
    }

    /// Perform a raw seek and restore the supplied logical record location.
    ///
    /// Unlike [`Self::seek`], this always invokes the underlying seek with the
    /// supplied [`io::SeekFrom`]. The resulting physical offset must match
    /// `location.byte`; this prevents silently corrupting subsequent location
    /// metadata. The supplied one-based `location.line` is restored without
    /// rescanning the source. `location.field` must be zero.
    ///
    /// # Errors
    ///
    /// Returns an error while initially discovering headers or field width,
    /// for inconsistent location metadata, or when the underlying seek fails.
    pub fn seek_raw(&mut self, seek_from: io::SeekFrom, location: Location) -> Result<(), Error> {
        if location.field != 0 {
            return Err(Error::io(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "CSV seek positions must identify a record boundary with field 0",
                ),
                location,
            ));
        }
        self.prepare_seek_state()?;
        let previous = self.location();
        let actual = self
            .input
            .seek(seek_from)
            .map_err(|error| Error::io(error, previous))?;
        if actual != location.byte as u64 {
            self.failed = true;
            return Err(Error::io(
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "raw seek reached byte {actual}, but location metadata specified byte {}",
                        location.byte
                    ),
                ),
                previous,
            ));
        }
        self.reset_stream_position(location);
        Ok(())
    }

    /// Rewind to the beginning and reapply the configured header policy.
    ///
    /// First-record headers are rediscovered on the next header or record
    /// operation. Provided headers remain installed.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying input cannot seek to byte zero.
    pub fn rewind(&mut self) -> Result<(), Error> {
        let previous = self.location();
        let actual = self
            .input
            .seek(io::SeekFrom::Start(0))
            .map_err(|error| Error::io(error, previous))?;
        if actual != 0 {
            self.failed = true;
            return Err(Error::io(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Seek implementation did not rewind to byte zero",
                ),
                previous,
            ));
        }

        self.reset_stream_position(Location::START);
        self.core.reset_headers();
        Ok(())
    }

    /// Settle everything a seek must carry across, before the position moves.
    ///
    /// A `MatchFirst` width is established by the first record of the stream,
    /// so one record is read here when the width is not yet known.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn prepare_seek_state(&mut self) -> Result<(), Error> {
        if self.failed {
            if self.headers_resolved && !self.core.needs_first_record_width() {
                return Ok(());
            }
            return self.check_failed();
        }
        self.ensure_headers()?;
        if !self.core.needs_first_record_width() {
            return Ok(());
        }
        if self.advance()? {
            let mut record = ByteRecord::new();
            let filled = self.filled;
            let window = self
                .window
                .get(..filled)
                .expect("filled bytes always lie within the read window");
            self.core.read_byte_record_into::<F>(window, &mut record)?;
        }
        Ok(())
    }

    /// Restart the window at a new stream position.
    fn reset_stream_position(&mut self, location: Location) {
        self.filled = usize::default();
        self.consumed = location.byte;
        self.eof = bool::default();
        self.failed = bool::default();
        self.bom_resolved = location.byte != 0 || self.bom == ReadBom::Preserve;
        self.bom_rejected = bool::default();
        self.filter_scanned_to = usize::default();
        self.filter_backoff = u32::default();
        self.headers_resolved = bool::default();
        self.done = bool::default();
        self.core.reset_position(location.line, location.record);
    }
}

#[cfg(feature = "std")]
impl IoParser<File> {
    /// Open a file for an explicit format and parse options.
    ///
    /// ```
    /// use coseva::config::{FormatOptions, ParseOptions};
    /// use coseva::IoParser;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("cities.csv");
    /// std::fs::write(&path, b"city,pop\nBoston,650706\n")?;
    ///
    /// let mut parser = IoParser::from_path(&path, FormatOptions::CSV, ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    ///
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, or an I/O error when the file cannot be
    /// opened.
    pub fn from_path(
        path: impl AsRef<Path>,
        format: FormatOptions,
        options: ParseOptions,
    ) -> Result<Self, Error> {
        let settings = options.into_settings(format)?;
        let input = File::open(path).map_err(Error::io_at_start)?;
        Ok(Self::from_config(input, settings))
    }
}

#[cfg(feature = "std")]
impl<F: StaticFormat> IoParser<File, F> {
    /// Open a file, specialized to the format named by `F`.
    ///
    /// This is [`IoParser::from_path`] with the format taken from the type
    /// rather than an argument.
    ///
    /// ```
    /// use coseva::config::ParseOptions;
    /// use coseva::format::Csv;
    /// use coseva::IoParser;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("cities.csv");
    /// std::fs::write(&path, b"city,pop\nBoston,650706\n")?;
    ///
    /// let mut parser = IoParser::<_, Csv>::new_path(&path, ParseOptions::new())?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    ///
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, or an I/O error when the file cannot be
    /// opened.
    pub fn new_path(path: impl AsRef<Path>, options: ParseOptions) -> Result<Self, Error> {
        let settings = options.into_settings(F::FORMAT)?;
        let input = File::open(path).map_err(Error::io_at_start)?;
        Ok(Self::from_config_for(input, settings))
    }
}

#[cfg(feature = "std")]
impl<R: Read, F: CsvFormat> LineSource for IoParser<R, F> {
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
#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::config::FieldCount;
    use std::error::Error as _;

    #[test]
    fn test_io_parser_edge_cases() {
        use std::io::Cursor;

        // 1. advance_with_filter for nonexistent header column
        let data = b"a,b\n1,2\n3,4\n";
        let mut parser = IoParser::with_options(
            &data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(
            !parser
                .advance_with_filter(&Predicate::equals("nonexistent", "1"))
                .unwrap()
        );

        // 2. advance_with_filter error when field parsing fails
        let bad_data = b"a,b\n\"unclosed,2\n";
        let mut parser2 = IoParser::with_options(
            &bad_data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(
            parser2
                .advance_with_filter(&Predicate::equals("a", "val"))
                .is_err()
        );

        // 3. advance_line_origin and line_for_offset
        #[cfg(feature = "index")]
        {
            let mut p = IoParser::with_options(
                &b"a\nb\n"[..],
                FormatOptions::CSV,
                ParseOptions::new().headers(Headers::None),
            )
            .unwrap();
            p.advance().unwrap();
            p.advance_line_origin(0, 10);
            assert_eq!(p.line_for_offset(0), 10);
        }

        // 4. decode_with_mapping error path
        #[cfg(feature = "serde")]
        {
            let data3 = b"a,b\nnot_a_num,2\n";
            let mut parser3 = IoParser::with_options(
                &data3[..],
                FormatOptions::CSV,
                ParseOptions::new().headers(Headers::FirstRecord),
            )
            .unwrap();
            let mut line = parser3.next_line().unwrap().unwrap();
            #[derive(serde::Deserialize)]
            struct NumRow {
                #[expect(dead_code, reason = "test struct")]
                a: u32,
            }
            assert!(line.deserialized::<NumRow>().is_err());
            assert!(parser3.is_done());
        }

        // 5. refill with bad Read implementation returning read > room
        struct OverflowReader;
        impl io::Read for OverflowReader {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                Ok(buf.len() + 10)
            }
        }
        let mut parser_overflow = IoParser::with_options(
            OverflowReader,
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(16),
        )
        .unwrap();
        let mut rec = ByteRecord::new();
        assert!(parser_overflow.read_byte_record_into(&mut rec).is_err());

        // 6. seek / seek_raw with location.field != 0
        let mut seek_p = IoParser::with_options(
            Cursor::new(b"a,b\n1,2\n3,4\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new(),
        )
        .unwrap();
        assert!(
            seek_p
                .seek(Location {
                    byte: 0,
                    line: 1,
                    record: 1,
                    field: 1
                })
                .is_err()
        );
        assert!(
            seek_p
                .seek_raw(
                    io::SeekFrom::Start(0),
                    Location {
                        byte: 0,
                        line: 1,
                        record: 1,
                        field: 1
                    }
                )
                .is_err()
        );

        // 7. prepare_seek_state after fail
        seek_p.fail(Error::new(ErrorKind::ParserFailed, Location::START));
        assert!(
            seek_p
                .seek(Location {
                    byte: 0,
                    line: 1,
                    record: 1,
                    field: 0
                })
                .is_err()
        );

        // 8. read_byte_record_into with ReadBom::Reject
        let bom_data = b"\xEF\xBB\xBFa,b\n1,2\n";
        let mut bom_p = IoParser::with_options(
            &bom_data[..],
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut bom_rec = ByteRecord::new();
        assert!(bom_p.read_byte_record_into(&mut bom_rec).is_err());

        // 9. read_byte_record_into with syntax error
        let mut bad_rec_p = IoParser::with_options(
            &b"\"unclosed\n"[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        assert!(bad_rec_p.read_byte_record_into(&mut bom_rec).is_err());

        // 10. headers / header_indices with syntax error
        let mut bad_headers_parser = IoParser::with_options(
            &b"\"unclosed\n"[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(bad_headers_parser.headers().is_err());

        let mut bad_header_indices_parser = IoParser::with_options(
            &b"\"unclosed\n"[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(bad_header_indices_parser.header_indices("a").is_err());

        // 11. seek with MatchFirst preparing field count
        let mut mf_seek = IoParser::with_options(
            Cursor::new(b"a,b\n1,2\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert!(mf_seek.seek(Location::START).is_ok());

        // 12. refill when eof is true
        mf_seek.eof = true;
        assert!(mf_seek.refill().is_ok());

        // 13. headers and header_indices when core is failed
        let mut h_fail_p = IoParser::with_options(
            &b"a,b\n1,2\n"[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        h_fail_p.failed = true;
        assert!(h_fail_p.headers().is_err());
        assert!(h_fail_p.header_indices("a").is_err());

        // 14. seek with bad seek returning unexpected offset
        struct BadSeek;
        impl Read for BadSeek {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl io::Seek for BadSeek {
            fn seek(&mut self, _pos: io::SeekFrom) -> std::io::Result<u64> {
                Ok(999)
            }
        }
        let mut bad_seek_p =
            IoParser::with_options(BadSeek, FormatOptions::CSV, ParseOptions::new()).unwrap();
        assert!(bad_seek_p.seek(Location::START).is_err());

        let mut bad_rewind_p =
            IoParser::with_options(BadSeek, FormatOptions::CSV, ParseOptions::new()).unwrap();
        assert!(bad_rewind_p.rewind().is_err());

        let mut bad_raw_seek_p =
            IoParser::with_options(BadSeek, FormatOptions::CSV, ParseOptions::new()).unwrap();
        assert!(
            bad_raw_seek_p
                .seek_raw(io::SeekFrom::Start(0), Location::START)
                .is_err()
        );

        // 15. get_ref, get_mut, set_headers, has_headers, into_inner
        let mut get_p = IoParser::with_options(
            Cursor::new(b"a,b\n1,2\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        assert_eq!(get_p.get_ref().get_ref(), b"a,b\n1,2\n");
        assert_eq!(get_p.get_mut().get_ref(), b"a,b\n1,2\n");
        assert!(!get_p.has_headers());
        let mut custom_headers = ByteRecord::new();
        custom_headers.push_field(b"h1");
        get_p.set_headers(custom_headers);
        assert!(get_p.has_headers());
        let _inner = get_p.into_inner();

        // 16. rewind on stream with non-zero start
        let mut rew_p = IoParser::with_options(
            Cursor::new(b"a,b\n1,2\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(rew_p.advance().unwrap());
        assert!(rew_p.rewind().is_ok());

        // advance_window_eagerly with BlankRecords::Skip and comments
        let commented_data = b"# comment\n\na,b\n";
        let mut eager_p = IoParser::with_options(
            &commented_data[..],
            FormatOptions::CSV
                .comment(Some(b'#'))
                .blank_records(BlankRecords::Skip),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        assert!(eager_p.advance().unwrap());

        // read_byte_record_into reaching EOF
        let empty_data = b"";
        let mut empty_p = IoParser::with_options(
            &empty_data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut r_empty = ByteRecord::new();
        assert!(!empty_p.read_byte_record_into(&mut r_empty).unwrap());

        // Invalid buffer capacity in new and with_options
        assert!(
            IoParser::<_, crate::format::Csv>::new(
                &b""[..],
                ParseOptions::new().buffer_capacity(0)
            )
            .is_err()
        );
        assert!(
            IoParser::with_options(
                &b""[..],
                FormatOptions::CSV,
                ParseOptions::new().buffer_capacity(0)
            )
            .is_err()
        );

        // Multi-refill read_byte_record_into
        let small_buf_data = b"first_chunk_field,second_chunk_field\n";
        let mut p_chunk = IoParser::with_options(
            &small_buf_data[..],
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(8),
        )
        .unwrap();
        let mut rec_chunk = ByteRecord::new();
        assert!(p_chunk.read_byte_record_into(&mut rec_chunk).unwrap());

        // advance_with_filter error on syntax error headers
        let bad_hdr_data = b"\"unclosed\n1,2\n";
        let mut p_filter_err = IoParser::with_options(
            &bad_hdr_data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(
            p_filter_err
                .advance_with_filter(&Predicate::equals("col", "val"))
                .is_err()
        );

        // resolve_optional_typed_mapping error on bad headers
        let mut p_map_err = IoParser::with_options(
            &bad_hdr_data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(
            p_map_err
                .resolve_optional_typed_mapping(&["a"], &[])
                .is_err()
        );

        // seek_raw on failed parser
        let mut p_seek_err = IoParser::with_options(
            std::io::Cursor::new(b"a,b\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new(),
        )
        .unwrap();
        p_seek_err.failed = true;
        assert!(
            p_seek_err
                .seek_raw(io::SeekFrom::Start(0), Location::START)
                .is_err()
        );

        // seek with MatchFirst and headers FirstRecord
        let mut mf_seek2 = IoParser::with_options(
            Cursor::new(b"col1,col2\n1,2\n3,4\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::FirstRecord)
                .field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert!(
            mf_seek2
                .seek(Location {
                    byte: 10,
                    line: 2,
                    record: 1,
                    field: 0
                })
                .is_ok()
        );

        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing").join("file.csv");

        // from_path and new_path error when nonexistent
        assert!(IoParser::from_path(&missing, FormatOptions::CSV, ParseOptions::new()).is_err());
        assert!(
            IoParser::<_, crate::format::Csv>::new_path(&missing, ParseOptions::new()).is_err()
        );

        // from_path and new_path invalid settings error
        assert!(
            IoParser::from_path(
                directory.path(),
                FormatOptions::CSV,
                ParseOptions::new().buffer_capacity(0)
            )
            .is_err()
        );
        assert!(
            IoParser::<_, crate::format::Csv>::new_path(
                directory.path(),
                ParseOptions::new().buffer_capacity(0)
            )
            .is_err()
        );
        assert!(
            IoParser::<_, crate::format::Csv>::new(
                Cursor::new(b"a,b\n"),
                ParseOptions::new().buffer_capacity(0)
            )
            .is_err()
        );

        // new_path valid file
        let temp = directory.path().join("parser.csv");
        use std::io::Write as _;
        let mut f = std::fs::File::create(&temp).unwrap();
        f.write_all(b"a,b\n1,2\n").unwrap();
        drop(f);
        let mut valid_np =
            IoParser::<_, crate::format::Csv>::new_path(&temp, ParseOptions::new()).unwrap();
        assert!(valid_np.advance().unwrap());

        // advance with small buffer needing refill
        let mut p_refill = IoParser::with_options(
            Cursor::new(b"very_long_field_1234567890\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(8),
        )
        .unwrap();
        assert!(p_refill.advance().unwrap());

        // advance_with_filter with header caching and end of iteration
        let mut p_filter = IoParser::with_options(
            Cursor::new(b"name,age\nalice,30\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        let pred = Predicate::equals("name", "alice");
        assert!(p_filter.advance_with_filter(&pred).unwrap());
        assert!(!p_filter.advance_with_filter(&pred).unwrap());

        // prepare_first_record_width with unheaded and MatchFirst
        let mut p_first_width = IoParser::<_, crate::format::Csv>::new(
            Cursor::new(b"1,2,3\n4,5,6\n".to_vec()),
            ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert!(p_first_width.advance().unwrap());

        // header_index and resolve_optional_typed_mapping error on malformed header
        let mut mal_head = IoParser::with_options(
            Cursor::new(b"\"unclosed\n1,2\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(mal_head.header_index("col").is_err());

        let mut mal_head2 = IoParser::with_options(
            Cursor::new(b"\"unclosed\n1,2\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::FirstRecord),
        )
        .unwrap();
        assert!(
            mal_head2
                .resolve_optional_typed_mapping(&["col"], &[])
                .is_err()
        );

        // failing seeker
        struct FailingSeeker(std::io::Cursor<Vec<u8>>);
        impl std::io::Read for FailingSeeker {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl std::io::Seek for FailingSeeker {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "seek failed",
                ))
            }
        }
        let mut fail_seek = IoParser::with_options(
            FailingSeeker(Cursor::new(b"1,2\n3,4\n".to_vec())),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        assert!(
            fail_seek
                .seek(Location {
                    byte: 4,
                    line: 2,
                    record: 1,
                    field: 0
                })
                .is_err()
        );
    }

    #[test]
    fn refill_reclaims_a_window_grown_by_one_huge_record() {
        let read_size = 4096;
        let huge_len = 512 * 1024;
        let mut input = Vec::new();
        input.extend(std::iter::repeat_n(b'x', huge_len));
        input.push(b'\n');
        for _ in 0..10000 {
            input.extend_from_slice(b"a\n");
        }

        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(read_size),
        )
        .expect("valid options");
        let mut record = ByteRecord::new();

        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("huge record")
        );
        let field = record.get(0).expect("huge field");
        assert_eq!(field.len(), huge_len);
        assert!(field.iter().all(|&byte| byte == b'x'));
        let grown = parser.window.capacity();
        assert!(
            grown >= huge_len,
            "the window must grow enough for the outlier record"
        );

        let mut reclaimed = None;
        for _ in 0..10000 {
            assert!(
                parser
                    .read_byte_record_into(&mut record)
                    .expect("small record")
            );
            assert_eq!(record.get(0), Some(b"a".as_slice()));
            let capacity = parser.window.capacity();
            if capacity < grown {
                reclaimed = Some(capacity);
                break;
            }
        }

        let reclaimed = reclaimed.expect("the refill path must hand back the outlier capacity");
        assert!(
            reclaimed <= read_size * 4,
            "reclaimed capacity {reclaimed} should return near the read size {read_size}"
        );
    }

    #[test]
    fn refill_reclaims_engine_scratch_with_the_outlier_window() {
        let mut input = Vec::new();
        input.push(b'"');
        for _ in 0..128 * 1024 {
            input.extend_from_slice(b"x\"\"");
        }
        input.extend_from_slice(b"tail\"\n");
        for _ in 0..10000 {
            input.extend_from_slice(b"a\n");
        }

        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(1024),
        )
        .expect("valid parser");
        let mut line = parser.next_line().expect("advance").expect("outlier");
        assert_eq!(
            line.record().expect("record").get(0).expect("field").len(),
            128 * 1024 * 2 + 4,
        );
        drop(line);
        let grown_window = parser.window.capacity();
        let (_, grown_scratch) = parser.core.buffer_capacities();
        assert!(grown_scratch >= 128 * 1024);

        let mut reclaimed_scratch = None;
        for _ in 0..10000 {
            let mut line = parser.next_line().expect("advance").expect("small record");
            assert_eq!(line.record().expect("record").get(0), Some(b"a".as_slice()));
            drop(line);
            if parser.window.capacity() < grown_window {
                reclaimed_scratch = Some(parser.core.buffer_capacities().1);
                break;
            }
        }
        assert!(
            reclaimed_scratch.expect("window reclaimed") < grown_scratch,
            "scratch must be reclaimed with the window",
        );
    }

    #[test]
    fn test_io_parser_filter_seek_and_decode_edges() {
        let data = b"name,age\nalice,30\nbob,25\ncharlie,35\n";
        let mut parser = IoParser::with_options(
            &data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(crate::config::Headers::FirstRecord),
        )
        .unwrap();

        // advance_with_filter by Column::Name
        let pred = crate::filter::Predicate::equals("name", "bob");
        assert!(parser.advance_with_filter(&pred).unwrap());
        assert!(!parser.advance_with_filter(&pred).unwrap());

        // advance_with_filter reaching EOF without match
        let pred_none = crate::filter::Predicate::equals("name", "nonexistent");
        assert!(!parser.advance_with_filter(&pred_none).unwrap());

        // header_index on IoParser
        let mut parser2 = IoParser::with_options(
            &data[..],
            FormatOptions::CSV,
            ParseOptions::new().headers(crate::config::Headers::FirstRecord),
        )
        .unwrap();
        assert_eq!(parser2.header_index("name").unwrap(), Some(0));
        assert_eq!(parser2.header_index("nonexistent").unwrap(), None);

        // seek before reading when field count is MatchFirst
        let mut cursor_data = std::io::Cursor::new(b"a,b\n1,2\n3,4\n");
        let mut parser3 = IoParser::with_options(
            &mut cursor_data,
            FormatOptions::CSV,
            ParseOptions::new().field_count(crate::config::FieldCount::MatchFirst),
        )
        .unwrap();
        assert!(
            parser3
                .seek(Location {
                    byte: 4,
                    line: 2,
                    record: 1,
                    field: 0
                })
                .is_ok()
        );
    }

    #[test]
    fn owned_records_rebase_ranges_and_errors_after_refills() {
        let input = b"aa,bb\ncc,dd\nbad\"quote,x\n";
        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(3),
        )
        .expect("valid parser");
        let mut record = ByteRecord::new();

        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("first record")
        );
        assert_eq!(record.byte_range(), 0..6);
        assert_eq!(record.index(), 0);
        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("second record")
        );
        assert_eq!(record.byte_range(), 6..12);
        assert_eq!(record.index(), 1);

        let error = parser
            .read_byte_record_into(&mut record)
            .expect_err("third record is malformed");
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        assert_eq!(error.location().byte, 15);
        assert_eq!(error.location().line, 3);
        assert_eq!(error.location().record, 2);
        assert_eq!(
            parser
                .read_byte_record_into(&mut record)
                .expect_err("a failed parser stays failed")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }

    #[test]
    fn header_state_and_eager_blank_skipping_are_exact() {
        let input = b"left,right,left\none,two,three\n";
        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(2),
        )
        .expect("valid parser");

        assert_eq!(
            parser
                .headers()
                .expect("headers")
                .expect("configured headers")
                .iter()
                .collect::<Vec<_>>(),
            [b"left".as_slice(), b"right", b"left"],
        );
        assert!(parser.headers_resolved);
        assert_eq!(parser.header_indices("left").expect("indices"), [0, 2]);
        assert_eq!(parser.header_index("right").expect("index"), Some(1));
        assert_eq!(
            parser
                .headers()
                .expect("headers remain settled")
                .expect("configured headers")
                .get(1),
            Some(b"right".as_slice()),
        );

        let mut record = ByteRecord::new();
        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("data record")
        );
        assert_eq!(record.byte_range(), 16..30);
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"one".as_slice(), b"two", b"three"]
        );

        let mut provided = IoParser::with_options(
            b"data,value\n".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new(),
        )
        .expect("valid parser");
        provided.set_headers(["name", "amount"].into_iter().collect());
        assert!(!provided.discovers_headers);
        assert!(
            provided
                .read_byte_record_into(&mut record)
                .expect("first input record remains data")
        );
        assert_eq!(
            record.iter().collect::<Vec<_>>(),
            [b"data".as_slice(), b"value"],
        );

        let mut blanks = IoParser::with_options(
            b"\n\nx,y\n".as_slice(),
            FormatOptions::CSV.blank_records(BlankRecords::Skip),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(1),
        )
        .expect("valid parser");
        assert!(blanks.eager);
        assert!(
            blanks
                .read_byte_record_into(&mut record)
                .expect("nonblank record")
        );
        assert_eq!(record.iter().collect::<Vec<_>>(), [b"x", b"y"]);
        assert_eq!(record.byte_range(), 2..6);
    }

    #[test]
    fn filter_scan_skips_non_candidates_and_tracks_backoff() {
        let input = b"skip,0\nbad,too,many\nwanted,1\n";
        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::Exact(2))
                .buffer_capacity(64),
        )
        .expect("valid parser");
        let predicate = Predicate::equals(1, "1");
        assert!(
            parser
                .advance_with_filter(&predicate)
                .expect("non-candidates are skipped")
        );
        let mut line = parser.current_line();
        assert_eq!(
            line.record()
                .expect("matching record")
                .iter()
                .collect::<Vec<_>>(),
            [b"wanted".as_slice(), b"1"],
        );

        let mut named = IoParser::with_options(
            b"name,value\nskip,0\nwanted,1\n".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(64),
        )
        .expect("valid parser");
        assert!(
            named
                .advance_with_filter(&Predicate::equals("name", "wanted"))
                .expect("named filter")
        );
        assert_eq!(named.core.cached_filter_column(b"name"), Some(0));

        let quoted = b"a,\"quoted\"\nnext,row\n";
        let mut backoff = IoParser::with_options(
            quoted.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(quoted.len()),
        )
        .expect("valid parser");
        backoff.resolve_bom().expect("BOM state");
        assert_eq!(backoff.filled, quoted.len());
        backoff.skip_to_candidate(b"absent");
        assert_eq!(backoff.filter_scanned_to, quoted.len());
        assert_eq!(backoff.filter_backoff, FILTER_BACKOFF);
        backoff.filter_scanned_to = 0;
        backoff.skip_to_candidate(b"absent");
        assert_eq!(backoff.filter_backoff, FILTER_BACKOFF - 1);
        assert_eq!(backoff.filter_scanned_to, 0);

        let mut candidate_error = IoParser::with_options(
            b"bad\"quote,needle\n".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(64),
        )
        .expect("valid parser");
        let error = candidate_error
            .advance_with_filter(&Predicate::equals(1, "needle"))
            .expect_err("candidate syntax error");
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        assert!(candidate_error.failed);
        assert_eq!(
            candidate_error
                .advance_with_filter(&Predicate::equals(1, "needle"))
                .expect_err("filter failure is latched")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }

    #[test]
    fn refill_uses_the_configured_room_and_terminates_after_eof() {
        #[derive(Debug)]
        struct ExactRoomReader {
            expected: usize,
            calls: usize,
        }

        impl Read for ExactRoomReader {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                self.calls += 1;
                assert_eq!(buffer.len(), self.expected, "unexpected refill room");
                buffer[0] = b'x';
                Ok(1)
            }
        }

        let mut parser = IoParser::with_options(
            ExactRoomReader {
                expected: 4,
                calls: 0,
            },
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        assert_eq!(parser.read_size, 4);
        assert_eq!(parser.window.len(), 4);
        parser.window = vec![b'a'; 3];
        parser.filled = 3;
        parser
            .refill()
            .expect("window grows by one configured read");
        assert_eq!(parser.window.len(), 7);
        assert_eq!(parser.filled, 4);
        assert_eq!(parser.get_ref().calls, 1);

        #[derive(Debug)]
        struct ExactHalfEof;

        impl Read for ExactHalfEof {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert_eq!(buffer.len(), 2, "half a read is sufficient room");
                Ok(0)
            }
        }

        let mut equality = IoParser::with_options(
            ExactHalfEof,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        equality.window = vec![b'x'; 4];
        equality.filled = 2;
        equality.refill().expect("equality does not grow");
        assert_eq!(equality.window.len(), 4);
        assert!(equality.eof);

        #[derive(Debug)]
        struct FillRoom;

        impl Read for FillRoom {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }

        let mut full = IoParser::with_options(
            FillRoom,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        full.refill().expect("filling the complete room is valid");
        assert_eq!(full.filled, 4);
        assert!(!full.failed);

        #[derive(Debug)]
        struct BoundedEof {
            bytes: &'static [u8],
            offset: usize,
            eof_reads: usize,
            interrupted: bool,
        }

        impl Read for BoundedEof {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if !self.interrupted {
                    self.interrupted = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                if self.offset == self.bytes.len() {
                    self.eof_reads += 1;
                    assert_eq!(self.eof_reads, 1, "EOF must be latched");
                    return Ok(0);
                }
                buffer[0] = self.bytes[self.offset];
                self.offset += 1;
                Ok(1)
            }
        }

        let mut bounded = IoParser::with_options(
            BoundedEof {
                bytes: b"\xEF\xBB\xBFa,b\n",
                offset: 0,
                eof_reads: 0,
                interrupted: false,
            },
            FormatOptions::CSV.read_bom(ReadBom::Detect),
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(1),
        )
        .expect("valid parser");
        let mut record = ByteRecord::new();
        assert!(
            bounded
                .read_byte_record_into(&mut record)
                .expect("record after split BOM")
        );
        assert_eq!(record.iter().collect::<Vec<_>>(), [b"a", b"b"]);
        assert!(
            !bounded
                .read_byte_record_into(&mut record)
                .expect("bounded EOF")
        );
        assert_eq!(bounded.get_ref().eof_reads, 1);
        assert!(bounded.eof);
    }

    #[test]
    fn refill_reports_exact_progress_errors_and_poisoning() {
        struct Overrun;

        impl Read for Overrun {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                Ok(buffer.len() + 1)
            }
        }

        let mut parser = IoParser::with_options(
            Overrun,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        let error = parser.advance().expect_err("overrun is rejected");
        assert_eq!(
            error.source().expect("I/O source").to_string(),
            "Read implementation returned more bytes than the buffer holds",
        );
        assert_eq!(
            parser.advance().expect_err("parser is poisoned").kind(),
            ErrorKind::ParserFailed,
        );

        struct Broken;

        impl Read for Broken {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "permanent"))
            }
        }

        let mut parser = IoParser::with_options(
            Broken,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        let error = parser.advance().expect_err("permanent I/O failure");
        assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::BrokenPipe));
        assert!(parser.failed);
        assert_eq!(
            parser
                .advance()
                .expect_err("failure remains latched")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }

    #[test]
    fn refill_reclaim_boundary_uses_exact_capacity_and_keep_values() {
        struct Eof;

        impl Read for Eof {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }

        let read_size = 8 * 1024 + 1;
        let boundary = read_size * 4;
        for (capacity, reclaimed) in [(boundary, false), (boundary + 1, true)] {
            let mut parser = IoParser::with_options(
                Eof,
                FormatOptions::CSV,
                ParseOptions::new()
                    .headers(Headers::None)
                    .buffer_capacity(read_size),
            )
            .expect("valid parser");
            parser.window = Vec::with_capacity(capacity);
            parser.window.resize(read_size, 0);
            assert_eq!(parser.window.capacity(), capacity);
            parser.refill().expect("EOF refill");
            assert_eq!(
                parser.window.capacity() < boundary,
                reclaimed,
                "capacity {capacity}",
            );
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn typed_decode_errors_rebase_and_poison_the_stream() {
        #[derive(Debug, PartialEq, Eq)]
        struct Number(u32);

        impl<'record> crate::encoding::CsvDecode<'record> for Number {
            fn csv_decode<R>(record: &R) -> Result<Self, Error>
            where
                R: crate::encoding::DecodeRecord<'record> + ?Sized,
            {
                let text = core::str::from_utf8(record.get_field(0).unwrap_or_default())
                    .map_err(|error| Error::from_field_conversion(error, 0, "value"))?;
                let value = text
                    .parse::<u32>()
                    .map_err(|error| Error::from_field_conversion(error, 0, "value"))?;
                Ok(Self(value))
            }

            fn field_names() -> &'static [&'static str] {
                &["value"]
            }
        }

        let mut parser = IoParser::with_options(
            b"value\n7\nbad\n".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(2),
        )
        .expect("valid parser");
        {
            let mut records = parser.decoded_records::<Number>();
            assert_eq!(records.next().expect("first").expect("number"), Number(7));
            let error = records.next().expect("second").expect_err("invalid number");
            assert_eq!(error.kind(), ErrorKind::InvalidValue);
            assert_eq!(error.location().byte, 8);
            assert_eq!(error.location().line, 3);
            assert_eq!(error.location().record, 2);
            assert_eq!(error.location().field, 0);
        }
        assert_eq!(
            parser
                .advance()
                .expect_err("decode failure is latched")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }

    #[test]
    fn reset_stream_position_clears_every_stream_local_state() {
        let mut parser = IoParser::with_options(
            std::io::Cursor::new(b"a,b\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        parser.filled = 3;
        parser.consumed = 7;
        parser.eof = true;
        parser.failed = true;
        parser.bom_resolved = false;
        parser.bom_rejected = true;
        parser.filter_scanned_to = 99;
        parser.filter_backoff = 5;
        parser.headers_resolved = true;
        parser.done = true;

        let location = Location {
            byte: 12,
            line: 8,
            record: 6,
            field: 0,
        };
        parser.reset_stream_position(location);
        assert_eq!(parser.filled, 0);
        assert_eq!(parser.consumed, 12);
        assert!(!parser.eof);
        assert!(!parser.failed);
        assert!(parser.bom_resolved);
        assert!(!parser.bom_rejected);
        assert_eq!(parser.filter_scanned_to, 0);
        assert_eq!(parser.filter_backoff, 0);
        assert!(!parser.headers_resolved);
        assert!(!parser.done);
        assert_eq!(parser.location(), location);

        parser.bom_resolved = true;
        parser.reset_stream_position(Location::START);
        assert!(!parser.bom_resolved);
    }

    #[test]
    fn seek_failures_have_exact_messages_and_latch_failure() {
        #[derive(Debug)]
        struct LyingSeek;

        impl Read for LyingSeek {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }

        impl io::Seek for LyingSeek {
            fn seek(&mut self, position: io::SeekFrom) -> io::Result<u64> {
                Ok(match position {
                    io::SeekFrom::Start(0) => 1,
                    io::SeekFrom::Start(_) => 0,
                    _ => 0,
                })
            }
        }

        let mut parser = IoParser::with_options(
            LyingSeek,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        let mut invalid = Location::START;
        invalid.field = 1;
        let error = parser.seek(invalid).expect_err("field must be zero");
        assert_eq!(
            error.source().expect("I/O source").to_string(),
            "CSV seek positions must identify a record boundary with field 0",
        );
        let error = parser
            .seek_raw(io::SeekFrom::Start(0), invalid)
            .expect_err("raw seek field must be zero");
        assert_eq!(
            error.source().expect("I/O source").to_string(),
            "CSV seek positions must identify a record boundary with field 0",
        );

        let error = parser
            .seek(Location {
                byte: 4,
                line: 2,
                record: 1,
                field: 0,
            })
            .expect_err("reported position differs");
        assert_eq!(
            error.source().expect("I/O source").to_string(),
            "Seek implementation returned an unexpected absolute location",
        );
        assert!(parser.failed);

        let mut rewind = IoParser::with_options(
            LyingSeek,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        let error = rewind.rewind().expect_err("rewind must reach zero");
        assert_eq!(
            error.source().expect("I/O source").to_string(),
            "Seek implementation did not rewind to byte zero",
        );
        assert!(rewind.failed);
    }

    #[test]
    fn round_four_stream_windows_and_state_boundaries_are_exact() {
        use std::io::Cursor;

        static MISSING: &[&str] = &["missing"];
        let mut mapping = IoParser::with_options(
            b"".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        mapping.set_headers(["present"].into_iter().collect());
        mapping.consumed = 7;
        let error = mapping
            .resolve_optional_typed_mapping(MISSING, &[])
            .expect_err("missing mapped header");
        assert_eq!(error.kind(), ErrorKind::Decode);
        assert_eq!(error.location().byte, 7);

        let input = b"a,b\nc,d\n".to_vec();
        let mut seeking = IoParser::with_options(
            Cursor::new(input.clone()),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::MatchFirst)
                .buffer_capacity(input.len()),
        )
        .expect("valid parser");
        seeking.seek(Location::START).expect("settle first width");
        let mut record = ByteRecord::new();
        assert!(
            seeking
                .read_byte_record_into(&mut record)
                .expect("record after seek")
        );
        assert_eq!(record.iter().collect::<Vec<_>>(), [b"a".as_slice(), b"b"],);

        let mut rewind = IoParser::with_options(
            Cursor::new(b"left,right\none,two\n".to_vec()),
            FormatOptions::CSV,
            ParseOptions::new().buffer_capacity(4),
        )
        .expect("valid parser");
        assert!(rewind.headers().expect("headers").is_some());
        assert!(rewind.headers_resolved);
        rewind.rewind().expect("rewind");
        assert!(!rewind.headers_resolved);
        assert_eq!(
            rewind
                .headers()
                .expect("rediscovered headers")
                .expect("headers")
                .iter()
                .collect::<Vec<_>>(),
            [b"left".as_slice(), b"right"],
        );

        struct PanicReader;

        impl Read for PanicReader {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                panic!("latched EOF must not read")
            }
        }

        let mut eof = IoParser::with_options(
            PanicReader,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        eof.window = vec![b'x', b'y'];
        eof.filled = 1;
        eof.consumed = 5;
        eof.eof = true;
        eof.refill().expect("latched EOF");
        assert_eq!(eof.window, [b'x', b'y']);
        assert_eq!(eof.filled, 1);
        assert_eq!(eof.consumed, 5);

        struct ExpectFive;

        impl Read for ExpectFive {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert_eq!(buffer.len(), 5, "five-byte reads grow at room two");
                Ok(0)
            }
        }

        let mut growth = IoParser::with_options(
            ExpectFive,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(5),
        )
        .expect("valid parser");
        growth.window = vec![b'x'; 4];
        growth.filled = 2;
        growth.refill().expect("grow at exact threshold");
        assert_eq!(growth.window.len(), 7);

        struct ThreeForTwo;

        impl Read for ThreeForTwo {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert_eq!(buffer.len(), 2);
                Ok(3)
            }
        }

        let mut overrun = IoParser::with_options(
            ThreeForTwo,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(4),
        )
        .expect("valid parser");
        overrun.window = vec![b'x'; 4];
        overrun.filled = 2;
        assert_eq!(
            overrun.refill().expect_err("three bytes do not fit").kind(),
            ErrorKind::Io(io::ErrorKind::InvalidData),
        );
        assert!(overrun.failed);

        struct Eof;

        impl Read for Eof {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }

        let read_size = 8 * 1024 + 1;
        let keep = read_size + 1;
        let mut reclaimed = IoParser::with_options(
            Eof,
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(read_size),
        )
        .expect("valid parser");
        reclaimed.window = Vec::with_capacity(keep * 4 + 1);
        reclaimed.window.resize(read_size, b'y');
        reclaimed.window[0] = b'x';
        reclaimed.filled = 1;
        reclaimed.refill().expect("reclaim");
        assert_eq!(reclaimed.window[0], b'x');
        assert_eq!(
            reclaimed.window[1], 0,
            "discarded initialized spare is zeroed",
        );

        let mut bom = IoParser::with_options(
            b"x".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(1),
        )
        .expect("valid parser");
        bom.resolve_bom().expect("resolve non-BOM input");
        assert!(bom.bom_resolved);

        #[derive(Debug, Default)]
        struct ExactBom {
            calls: usize,
        }

        impl Read for ExactBom {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                assert_eq!(self.calls, 0, "an exact BOM is not read past");
                self.calls += 1;
                buffer[..BOM.len()].copy_from_slice(BOM);
                Ok(BOM.len())
            }
        }

        let mut exact_bom = IoParser::with_options(
            ExactBom::default(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(BOM.len()),
        )
        .expect("valid parser");
        exact_bom.resolve_bom().expect("exact BOM");
        assert!(exact_bom.bom_resolved);
        assert_eq!(exact_bom.get_ref().calls, 1);

        let bytes = b"skip\nneedle,1\n";
        let mut filter = IoParser::with_options(
            b"".as_slice(),
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        filter.window = bytes.to_vec();
        filter.filled = bytes.len();
        filter.filter_backoff = 1;
        filter.skip_to_candidate(b"needle");
        assert_eq!(filter.filter_backoff, 0);
        assert_eq!(filter.filter_scanned_to, 0);
        filter.skip_to_candidate(b"needle");
        assert_eq!(filter.filter_scanned_to, 5);
        assert_eq!(filter.core.byte_offset(), 5);
    }

    #[cfg(feature = "index")]
    #[test]
    fn index_line_helpers_use_stream_relative_offsets() {
        let input = b"a\nb\nc\n";
        let mut parser = IoParser::with_options(
            input.as_slice(),
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(2),
        )
        .expect("valid parser");
        let mut record = ByteRecord::new();
        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("first record")
        );
        assert!(
            parser
                .read_byte_record_into(&mut record)
                .expect("second record")
        );
        let origin = parser.consumed;
        assert!(parser.filled > 0);
        parser.advance_line_origin(origin, 10);
        assert_eq!(parser.line_for_offset(origin), 10);
        assert_eq!(parser.line_for_offset(origin + 2), 11);

        parser.consumed = 7;
        parser.core.reset_position(10, 0);
        parser.window = b"a\n".to_vec();
        parser.filled = 2;
        assert_eq!(
            parser.line_for_offset(9),
            11,
            "the last filled newline remains visible",
        );
    }
}

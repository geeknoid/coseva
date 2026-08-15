//! Push-based CSV parsing for chunked and asynchronous sources.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::marker::PhantomData;
use core::mem;

use crate::byte_record::ByteRecord;
use crate::config::{FormatOptions, Headers, ParseOptions, ParserSettings, ReadBom};
use crate::engine::{Advance, Engine};
use crate::error::{Error, ErrorKind, Location};
use crate::filter::{Column, Predicate};
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::line::{Line, rebase_record};
use crate::reclaim::reclaim;
use crate::search::{count1, find1};
use crate::text_record::TextRecord;

/// The mark stripped from the start of a stream by [`ReadBom::Detect`].
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Incremental parser for chunked and asynchronous sources.
///
/// [`SliceParser`](crate::SliceParser) needs the whole input up front and
/// [`IoParser`](crate::IoParser) pulls from a reader. Neither
/// works when something else owns the read loop: an async socket, a WASM or
/// FFI callback, or a decompressor emitting blocks. This parser inverts the
/// control flow — you lend it bytes with [`Self::chunk`], and it yields the
/// records those bytes completed through the same cursor API the other
/// parsers expose.
///
/// The loan is what keeps the parser cheap. A record lying wholly inside a
/// chunk is reported straight out of the caller's memory, so only a record a
/// chunk cut in half is ever copied, and only that record outlives the chunk
/// it arrived in. Memory therefore stays bounded by the configured record
/// limit however long the stream runs.
///
/// [`Chunk::next_line`] returning `None` means "no further record from the
/// bytes lent so far". That is a pause, not an end of input; use
/// [`Self::is_done`] to tell the two apart.
///
/// # Chunk size
///
/// Chunks smaller than a record cost real time, because a record split across
/// chunks is copied into the parser's window and re-parsed from its start each
/// time the window grows. `benches/window.rs` sweeps a 51-byte record: chunks
/// of 8192, 1024 and 256 bytes cost 964k, 1076k and 1458k instructions for
/// 1000 records, and 32-byte chunks cost 3819k — four times the default.
///
/// The parser does not choose this number, the caller's transport does, so
/// coalescing small reads before lending them is worth doing when the source
/// delivers less than a record at a time. There is no penalty for large
/// chunks; a chunk holding many whole records is the case the borrowed path
/// is built for.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::PushParser;
///
/// let chunks: [&[u8]; 3] = [b"city,pop\nBos", b"ton,650706\nLond", b"on,8982000"];
/// let mut parser = PushParser::<Csv>::new(ParseOptions::new())?;
/// let mut cities = Vec::new();
///
/// for bytes in chunks {
///     let mut offset = 0;
///     while offset < bytes.len() {
///         let mut chunk = parser.chunk(&bytes[offset..]);
///         while let Some(mut line) = chunk.next_line()? {
///             cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
///         }
///         offset += chunk.done();
///     }
/// }
///
/// // The last record has no terminator, so it stays pending until the stream
/// // is declared complete.
/// parser.finish();
/// let mut chunk = parser.chunk(b"");
/// while let Some(mut line) = chunk.next_line()? {
///     cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
/// }
/// drop(chunk);
///
/// assert_eq!(cities, [b"Boston".to_vec(), b"London".to_vec()]);
/// assert!(parser.is_done());
/// # Ok::<(), coseva::Error>(())
/// ```
#[expect(
    clippy::struct_excessive_bools,
    reason = "cached stream-policy flags avoid repeated work in the record hot path"
)]
#[derive(Debug)]
pub struct PushParser<F: CsvFormat = Dynamic> {
    marker: PhantomData<fn() -> F>,
    /// Bytes accepted but not yet dropped, scanned in place by the engine.
    window: Vec<u8>,
    core: Engine,
    /// Stream bytes already dropped from the front of the window.
    consumed: usize,
    finished: bool,
    failed: bool,
    /// An error raised where it could not be returned, held for the next
    /// fallible call.
    ///
    /// `Chunk::settle` runs from `Drop` and can only report how many bytes it
    /// took, so a record-limit breach discovered while it drains the chunk tail
    /// has nowhere to go. Holding it here surfaces the actionable limit and
    /// location at the next fallible boundary instead of the generic
    /// [`ErrorKind::ParserFailed`] the latched flag alone would produce.
    ///
    /// Set only in company with `failed`, so the checks that consume it can
    /// stay under that flag rather than testing for it separately.
    deferred: Option<Error>,
    /// The settings the window sessions run under, retained for [`Self::reset`].
    settings: ParserSettings,
    /// Whether a leading byte-order mark has been decided on.
    bom_resolved: bool,
    /// Whether the engine has settled the header record, so that the header
    /// views can read it without parsing a window that may still grow.
    headers_resolved: bool,
    /// Window length at which the engine last asked for more bytes.
    ///
    /// A window that held no whole record still holds none at the same length,
    /// so a caller feeding chunks smaller than a record would otherwise pay a
    /// full parse of the partial record for every chunk that fails to complete
    /// it. [`NO_REFUSAL`] means no such answer is cached.
    need_more_len: usize,
}

/// No cached [`Advance::NeedMore`] answer, since no window has this length.
const NO_REFUSAL: usize = usize::MAX;

impl PushParser<Dynamic> {
    /// Create a parser for an explicit format and parse options.
    ///
    /// ```
    /// use coseva::config::{FormatOptions, ParseOptions};
    /// use coseva::PushParser;
    ///
    /// let mut parser = PushParser::with_options(
    ///     FormatOptions::CSV.delimiter(b';'),
    ///     ParseOptions::new(),
    /// )?;
    /// let mut chunk = parser.chunk(b"city;pop\nBoston;650706\n");
    /// let mut line = chunk
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid format or rejected buffer capacity.
    pub fn with_options(format: FormatOptions, options: ParseOptions) -> Result<Self, Error> {
        Ok(Self::from_settings(options.into_settings(format)?))
    }
}

impl<F: StaticFormat> PushParser<F> {
    /// Create a parser specialized to the format named by `F`.
    ///
    /// The format bytes fold to immediates, so the parser avoids reloading
    /// them on every field. The format comes from the type, so there is no
    /// format argument.
    ///
    /// ```
    /// use coseva::config::ParseOptions;
    /// use coseva::format::Tsv;
    /// use coseva::PushParser;
    ///
    /// let mut parser = PushParser::<Tsv>::new(ParseOptions::new())?;
    /// let mut chunk = parser.chunk(b"city\tpop\nBoston\t650706\n");
    /// let mut line = chunk
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for invalid parse options or a rejected buffer
    /// capacity.
    pub fn new(options: ParseOptions) -> Result<Self, Error> {
        Ok(Self::from_settings(options.into_settings(F::FORMAT)?))
    }
}

impl<F: CsvFormat> PushParser<F> {
    /// Create a parser for `settings` over an initially empty window.
    fn from_settings(settings: ParserSettings) -> Self {
        let bom_resolved = settings.bom == ReadBom::Preserve;
        // Provided headers need no input to settle, so the header views can
        // read them before a single byte is fed.
        let headers_resolved = matches!(settings.headers, Headers::Provided(_));
        Self {
            marker: PhantomData,
            window: Vec::new(),
            core: Engine::from_config_windowed(&[], settings.clone()),
            consumed: 0,
            finished: false,
            failed: false,
            deferred: None,
            settings,
            bom_resolved,
            headers_resolved,
            need_more_len: NO_REFUSAL,
        }
    }

    /// Hand the parser a chunk of the stream to read records out of directly.
    ///
    /// The returned guard borrows the chunk, so records lying wholly inside it
    /// are reported straight out of the caller's memory the way
    /// [`SliceParser`](crate::SliceParser) reports them, and only a record the
    /// chunk cut in half is ever copied. Drain it with
    /// [`Chunk::next_line`] until that yields `None`, then call
    /// [`Chunk::done`] to learn how much of the chunk was taken and to leave
    /// the straddling tail with the parser. Dropping the guard without calling
    /// [`Chunk::done`] settles the parser the same way; only the count is lost.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::PushParser;
    ///
    /// let chunks: [&[u8]; 2] = [b"city,pop\nBos", b"ton,650706\nLondon,8982000\n"];
    /// let mut parser = PushParser::<Csv>::new(ParseOptions::new())?;
    /// let mut cities = Vec::new();
    ///
    /// for (index, bytes) in chunks.iter().enumerate() {
    ///     // The record ending the last chunk can only terminate once the
    ///     // stream is known to be over.
    ///     if index + 1 == chunks.len() {
    ///         parser.finish();
    ///     }
    ///     let mut offset = 0;
    ///     while offset < bytes.len() {
    ///         let mut chunk = parser.chunk(&bytes[offset..]);
    ///         while let Some(mut line) = chunk.next_line()? {
    ///             cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
    ///         }
    ///         offset += chunk.done();
    ///     }
    /// }
    ///
    /// assert_eq!(cities, [b"Boston".to_vec(), b"London".to_vec()]);
    /// assert!(parser.is_done());
    /// # Ok::<(), coseva::Error>(())
    /// ```
    pub fn chunk<'parser, 'input>(
        &'parser mut self,
        input: &'input [u8],
    ) -> Chunk<'parser, 'input, F> {
        // A chunk offered after a whole record has nothing carried over, so
        // its records come straight out of the caller's slice. Saying so here
        // skips an absorbing round that can only parse an empty window, fail
        // to find a record in it, and hand the chunk over anyway.
        let borrowed = self.window.is_empty();
        let direct_text = borrowed && self.headers_resolved && self.bom_resolved && !self.failed;
        Chunk {
            parser: self,
            input,
            absorbed: 0,
            stride: 0,
            borrowed,
            direct_text,
            region: input,
            settled: false,
        }
    }

    /// Declare the stream complete, so the final record can terminate.
    ///
    /// A record that has arrived without a record ending is only reported
    /// after this call, because until then it may still grow.
    pub fn finish(&mut self) {
        self.finished = true;
    }

    /// Return the configured or discovered headers.
    ///
    /// Unlike the other parsers this cannot fetch input, so first-record
    /// headers are only available once the record carrying them has been lent
    /// to the parser and read out of a [`Chunk`]; until then this yields
    /// `None`.
    #[must_use]
    #[inline]
    pub fn headers(&mut self) -> Option<&ByteRecord> {
        match self.headers_settled() {
            false => None,
            true => {
                // Settled headers make this a pure read: the engine has
                // already parsed them, so it cannot fail or consume input.
                self.core.headers(&self.window).ok().flatten()
            }
        }
    }

    /// Resolve the first header with the requested name.
    ///
    /// Yields `None` until the headers are available, as [`Self::headers`]
    /// describes.
    #[must_use]
    #[inline]
    pub fn header_index(&mut self, name: impl AsRef<[u8]>) -> Option<usize> {
        match self.headers_settled() {
            false => None,
            true => self.core.header_index(&self.window, name).ok().flatten(),
        }
    }

    /// Resolve every duplicate header with the requested name.
    ///
    /// Yields an empty slice until the headers are available, as
    /// [`Self::headers`] describes.
    #[must_use]
    #[inline]
    pub fn header_indices(&mut self, name: impl AsRef<[u8]>) -> &[usize] {
        match self.headers_settled() {
            false => &[],
            true => self
                .core
                .header_indices(&self.window, name)
                .unwrap_or_default(),
        }
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
        self.settings.headers = Headers::Provided(headers.clone());
        self.core.set_headers(headers);
        self.headers_resolved = self.core.has_headers();
    }

    /// Current parser location, reported against the stream.
    ///
    /// Because a chunk that may be cut mid-record has to be parsed eagerly,
    /// [`Chunk::next_line`] can leave the byte at the end of the record it
    /// positioned on rather than at its start; the line and record counters
    /// are always exact. Read a record's own
    /// [`byte_range`](crate::Record::byte_range) for its exact extent.
    #[must_use]
    #[inline]
    pub fn location(&self) -> Location {
        self.location_in(&self.window)
    }

    /// Whether the stream is complete and holds no further record, or parsing
    /// stopped after an error.
    #[must_use]
    #[inline]
    pub fn is_done(&self) -> bool {
        match (self.failed, self.finished) {
            (true, _) => true,
            (false, false) => false,
            (false, true) => self.core.is_done(&self.window),
        }
    }

    /// Reset the parser for an unrelated stream while retaining capacity.
    ///
    /// The configuration is retained, so this cannot fail and the caller need
    /// not hold on to the original options. Headers installed with
    /// [`Self::set_headers`] stay installed; discovered headers are
    /// rediscovered from the next stream. Every buffer the parser has grown is
    /// reused rather than reallocated.
    pub fn reset(&mut self) {
        self.window.clear();
        self.need_more_len = NO_REFUSAL;
        self.core.reset_for(&self.settings);
        self.consumed = usize::default();
        self.finished = bool::default();
        self.failed = bool::default();
        self.deferred = Option::default();
        self.bom_resolved = matches!(self.settings.bom, ReadBom::Preserve);
        self.headers_resolved = matches!(self.settings.headers, Headers::Provided(_));
    }

    /// Position the engine on the next record of the window.
    #[inline]
    fn window_advance(&mut self, at_eof: bool) -> Result<Advance, Error> {
        let outcome = self.core.advance_window::<F>(&self.window, at_eof);
        let outcome = self.note_advance(outcome)?;
        // At end of input the same window can settle a record it refused
        // while more bytes were still possible, so nothing is cached then.
        //
        // Only the refusal is recorded, never a reset: between the resets
        // below the window only ever grows, and a length that refused a
        // record is never revisited with the parse offset moved on, so a
        // stale entry cannot match.
        match (outcome, at_eof) {
            (Advance::NeedMore, false) => self.need_more_len = self.window.len(),
            _ => {}
        }
        Ok(outcome)
    }

    /// Fold an engine outcome into the parser's own stream state.
    #[inline]
    fn note_advance(&mut self, outcome: Result<Advance, Error>) -> Result<Advance, Error> {
        match outcome {
            Ok(outcome) => {
                match outcome {
                    Advance::NeedMore => {}
                    // Both other outcomes prove the engine settled the header
                    // record against bytes it was allowed to treat as whole.
                    _ => self.headers_resolved = true,
                }
                Ok(outcome)
            }
            Err(error) => {
                let consumed = self.consumed;
                Err(self.fail(rebase(error, consumed)))
            }
        }
    }

    /// Strip or reject a byte-order mark at the very start of the stream.
    fn resolve_bom(&mut self, at_eof: bool) -> Result<(), Error> {
        match self.bom_resolved {
            true => return Ok(()),
            false => {}
        }
        // The mark sits in the window, which cannot be read while the parser
        // is borrowed mutably, so it is lent out and handed straight back.
        let window = mem::take(&mut self.window);
        let result = self.resolve_bom_in(&window, at_eof);
        self.window = window;
        result
    }

    /// Strip or reject a byte-order mark at the start of `buffer`.
    ///
    /// A mark can be split across chunks, so a buffer holding only a prefix of
    /// one waits for more input rather than deciding, unless `at_eof` says
    /// nothing further is coming and the prefix is all there will ever be.
    #[cold]
    #[inline(never)]
    fn resolve_bom_in(&mut self, buffer: &[u8], at_eof: bool) -> Result<(), Error> {
        if buffer.len() < BOM.len() {
            if !at_eof && BOM.starts_with(buffer) {
                return Ok(());
            }
            self.bom_resolved = true;
            return Ok(());
        }
        self.bom_resolved = true;
        if !buffer.starts_with(BOM) {
            return Ok(());
        }
        if self.settings.bom == ReadBom::Reject {
            self.failed = true;
            return Err(Error::new(ErrorKind::RejectedBom, Location::START));
        }
        self.core.skip_detected_bom(buffer);
        Ok(())
    }

    /// Whether the header record has been settled against whole input.
    #[inline]
    fn headers_settled(&self) -> bool {
        match (self.headers_resolved, self.core.has_headers()) {
            (true, _) | (_, false) => true,
            (false, true) => false,
        }
    }

    /// Report the limit failure for a record that can no longer grow.
    ///
    /// Re-running the engine against the full bounded window exposes a
    /// narrower field or syntax error before the record-limit fallback, as it
    /// does in the other parsers.
    #[cold]
    #[inline(never)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn oversized_record_error(&mut self, buffer: &[u8]) -> Error {
        let consumed = self.consumed;
        let error = self.core.advance_window::<F>(buffer, false).map_or_else(
            |error| rebase(error, consumed),
            |_| self.record_limit_error(buffer),
        );
        self.fail(error)
    }

    /// The limit report for the record `buffer` is holding.
    #[cold]
    #[inline(never)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn record_limit_error(&self, buffer: &[u8]) -> Error {
        let mut location = self.location_in(buffer);
        // The engine can be positioned one past the caller's buffer. Clamping
        // leaves an empty range to count, which is right: no retained byte is
        // left to hold a newline.
        let requested = self.core.byte_offset();
        let (start, remaining) = match buffer.get(requested..) {
            Some(remaining) => (requested, remaining),
            None => (buffer.len(), &[][..]),
        };
        let retained = remaining
            .get(..=self.settings.limits.max_record_bytes)
            .unwrap_or(remaining);
        let cut = start.saturating_add(retained.len());
        location.line = location
            .line
            .saturating_add(count1(b'\n', &buffer[start..cut]) as u64);
        location.byte = self.consumed.saturating_add(cut);
        Error::new(
            ErrorKind::RecordTooLarge {
                limit: self.settings.limits.max_record_bytes,
            },
            location,
        )
    }

    /// Refuse further work once the parser has failed.
    ///
    /// A deferred reason is only ever recorded together with the latched flag,
    /// so it is looked for under it and the succeeding path keeps the single
    /// test it costs on every record.
    fn check_failed(&mut self) -> Result<(), Error> {
        if self.failed {
            if let Some(error) = self.deferred.take() {
                return Err(error);
            }
            return Err(Error::new(
                ErrorKind::ParserFailed,
                self.location_in(&self.window),
            ));
        }
        Ok(())
    }

    /// Refuse further work once the parser has failed, positioning the report
    /// against the buffer the engine's offsets currently refer to.
    fn check_failed_in(&mut self, buffer: &[u8]) -> Result<(), Error> {
        if self.failed {
            if let Some(error) = self.deferred.take() {
                return Err(error);
            }
            return Err(Error::new(
                ErrorKind::ParserFailed,
                self.location_in(buffer),
            ));
        }
        Ok(())
    }

    /// The stream-relative location of the engine within `buffer`.
    fn location_in(&self, buffer: &[u8]) -> Location {
        let mut location = self.core.location(buffer);
        location.byte = self.consumed.saturating_add(location.byte);
        location
    }

    /// Record that the parser has failed and hand back the reason.
    fn fail(&mut self, error: Error) -> Error {
        self.failed = true;
        error
    }
}

/// A chunk of the stream lent to a [`PushParser`] for the length of a drain.
///
/// The guard exists so the parser can point at the caller's bytes rather than
/// at a copy of them. Because it borrows the chunk for as long as the records
/// in it are being read, a record lying wholly inside the chunk is reported
/// straight out of the caller's memory; only a record the chunk cut in half
/// has to be assembled in storage the parser owns, and it is the only thing
/// that survives [`Self::done`].
///
/// Reading stops at the last record the chunk completes, so [`Self::next_line`]
/// returning `None` is a pause rather than an end of input unless
/// [`PushParser::finish`] has been called. Settling happens on drop as well as
/// on [`Self::done`], so a guard abandoned early — after an error, say — still
/// leaves the parser ready for the next chunk.
///
/// Create one with [`PushParser::chunk`].
/// For a worked example, see [`PushParser`].
#[derive(Debug)]
pub struct Chunk<'parser, 'input, F: CsvFormat = Dynamic> {
    parser: &'parser mut PushParser<F>,
    input: &'input [u8],
    /// How much of `input` has been copied into the parser's window, and where
    /// the borrowed region begins once the window is gone.
    absorbed: usize,
    /// How far past `absorbed` the next absorption reaches before it starts
    /// looking for a record ending.
    ///
    /// Every absorption is followed by a re-parse of the record being
    /// assembled, so growing the step geometrically keeps a record carrying
    /// many embedded record endings from costing a pass per ending.
    stride: usize,
    /// Whether records now come out of `input` rather than out of the window.
    borrowed: bool,
    /// Whether the borrowed text path has resolved all stream-level guards.
    direct_text: bool,
    /// The part of `input` records are read out of once `borrowed` is set.
    ///
    /// This is always `input[absorbed..]`, kept alongside it so the record
    /// loop reaches the bytes without re-slicing on every record.
    region: &'input [u8],
    /// Whether the tail has already been handed back to the parser.
    settled: bool,
}

impl<F: CsvFormat> Chunk<'_, '_, F> {
    /// How many bytes of the chunk the parser took, ending the loan.
    ///
    /// Anything left over is a record the chunk cut short, which the parser
    /// keeps so the next chunk can finish it. A caller offers the chunk again
    /// from the returned offset until it is exhausted, which only takes more
    /// than one round when a record hits
    /// [`crate::config::Limits::max_record_bytes`].
    #[must_use]
    pub fn done(mut self) -> usize {
        self.settle()
    }

    /// Read the next record completed by this chunk into reusable byte storage.
    ///
    /// Returns `false` when the chunk completes no further record. That is a
    /// pause rather than end of input unless [`PushParser::finish`] has closed
    /// the stream. This is the push counterpart to
    /// [`IoParser::read_byte_record_into`](crate::IoParser::read_byte_record_into).
    ///
    /// # Errors
    ///
    /// Returns a positioned syntax, limit, width, byte-order-mark, or parser
    /// state error.

    #[inline]
    pub fn read_byte_record_into(&mut self, output: &mut ByteRecord) -> Result<bool, Error> {
        if self.borrowed && self.parser.headers_settled() {
            let input = self.region;
            let parser = &mut *self.parser;
            parser.check_failed_in(input)?;
            let at_eof = parser.finished;
            if !parser.bom_resolved {
                parser.resolve_bom_in(input, at_eof)?;
            }
            let outcome = parser.core.read_window_owned::<F>(input, at_eof, output);
            match parser.note_advance(outcome)? {
                Advance::Record => {
                    rebase_record(output, parser.consumed);
                    return Ok(true);
                }
                Advance::NeedMore | Advance::Done => return Ok(false),
            }
        }
        if !self.advance()? {
            return Ok(false);
        }
        let borrowed = self.borrowed;
        let region = self.region;
        let parser = &mut *self.parser;
        let input = match borrowed {
            true => region,
            false => parser.window.as_slice(),
        };
        parser
            .core
            .read_byte_record_into::<F>(input, output)
            .map_err(|error| parser.fail(error))?;
        rebase_record(output, parser.consumed);
        Ok(true)
    }

    /// Read the next record completed by this chunk into reusable UTF-8 storage.
    ///
    /// Returns `false` when the chunk completes no further record. Validation
    /// happens once while the record is refilled, so subsequent field access
    /// yields `&str` without per-field validation.
    ///
    /// # Errors
    ///
    /// Returns a positioned syntax, limit, width, byte-order-mark, UTF-8, or
    /// parser state error.
    #[inline]
    pub fn read_text_record_into(&mut self, output: &mut TextRecord) -> Result<bool, Error> {
        if self.direct_text {
            let input = self.region;
            let parser = &mut *self.parser;
            match parser
                .core
                .read_window_text::<F>(input, parser.finished, output)
            {
                Ok(Advance::Record) => {
                    output.rebase_location(parser.consumed);
                    return Ok(true);
                }
                Ok(Advance::NeedMore | Advance::Done) => return Ok(false),
                Err(error) => {
                    let consumed = parser.consumed;
                    return Err(parser.fail(rebase(error, consumed)));
                }
            }
        }
        if self.borrowed && self.parser.headers_settled() {
            let input = self.region;
            let parser = &mut *self.parser;
            parser.check_failed_in(input)?;
            let at_eof = parser.finished;
            if !parser.bom_resolved {
                parser.resolve_bom_in(input, at_eof)?;
            }
            let outcome = parser.core.read_window_text::<F>(input, at_eof, output);
            match parser.note_advance(outcome)? {
                Advance::Record => {
                    self.direct_text =
                        parser.headers_resolved && parser.bom_resolved && !parser.failed;
                    output.rebase_location(parser.consumed);
                    return Ok(true);
                }
                Advance::NeedMore | Advance::Done => return Ok(false),
            }
        }
        if !self.advance()? {
            return Ok(false);
        }
        let borrowed = self.borrowed;
        let region = self.region;
        let parser = &mut *self.parser;
        let input = match borrowed {
            true => region,
            false => parser.window.as_slice(),
        };
        parser
            .core
            .read_text_record_into::<F>(input, output)
            .map_err(|error| parser.fail(error))?;
        self.direct_text =
            borrowed && parser.headers_resolved && parser.bom_resolved && !parser.failed;
        output.rebase_location(parser.consumed);
        Ok(true)
    }

    /// Move to the next record the chunk holds, reporting whether one exists.
    ///
    /// Once the chunk is borrowed every further record comes straight out of
    /// the caller's slice, so that path is kept inline and the absorbing one,
    /// which runs at most once per chunk boundary, is left out of line.
    #[inline]
    pub(crate) fn advance(&mut self) -> Result<bool, Error> {
        match self.borrowed {
            true => self.advance_borrowed(),
            false => self.advance_absorbing(),
        }
    }

    /// Move to the next record while the window still holds a partial one.
    #[inline(never)]
    fn advance_absorbing(&mut self) -> Result<bool, Error> {
        loop {
            self.parser.check_failed()?;
            // The window can only be the whole of what is left when the chunk
            // has been drained into it and the caller has closed the stream.
            let at_eof = matches!(
                (self.parser.finished, self.absorbed.cmp(&self.input.len()),),
                (true, Ordering::Equal),
            );
            self.parser.resolve_bom(at_eof)?;
            // Re-parsing a window the engine already refused at this exact
            // length cannot produce a record, and for a caller whose chunks
            // are smaller than a record that is most of the work.
            let cached_refusal = matches!(
                (
                    at_eof,
                    self.parser.window.len().cmp(&self.parser.need_more_len),
                ),
                (false, Ordering::Equal),
            );
            match cached_refusal {
                false if self.parser.window_advance(at_eof)? == Advance::Record => {
                    return Ok(true);
                }
                _ => {}
            }
            match self.pending_window_bytes().cmp(&self.absorbed) {
                Ordering::Less | Ordering::Equal => {
                    self.enter_borrowed();
                    return self.advance_borrowed();
                }
                Ordering::Greater => {}
            }
            match self.absorb()? {
                Absorbed::Drained => return Ok(false),
                Absorbed::Boundary => {}
                Absorbed::Interior => {
                    // The whole chunk arrived without a record terminator, so
                    // the record still being assembled cannot have ended:
                    // every record ending carries that byte. Skip the re-parse
                    // this loop would otherwise run and, unless the stream has
                    // now closed and this partial record is the last there
                    // will ever be, report no record without consulting the
                    // engine at all. Recording the refusal at the grown length
                    // lets the next chunk's memo skip its parse too, so a
                    // record delivered in tiny chunks is scanned once here
                    // rather than re-parsed from its start for every chunk.
                    match self.parser.finished {
                        false => {
                            self.parser.need_more_len = self.parser.window.len();
                            return Ok(false);
                        }
                        true => {}
                    }
                }
            }
        }
    }

    /// Whether records now come out of the caller's slice.
    #[inline]
    pub(crate) const fn borrowed(&self) -> bool {
        self.borrowed
    }

    /// Reach the next record of the borrowed region and view it in one step.
    ///
    /// The region and the parser are loaded once and the record is built from
    /// the same slice the engine was just advanced over, which is what going
    /// back through `current_line` costs: it has to re-test the borrow and
    /// re-derive the buffer that this already has in hand.
    #[inline]
    pub(crate) fn next_borrowed_line(&mut self) -> Result<Option<Line<'_, F>>, Error> {
        let input = self.region;
        let parser = &mut *self.parser;
        parser.check_failed_in(input)?;
        let at_eof = parser.finished;
        match parser.bom_resolved {
            false => parser.resolve_bom_in(input, at_eof)?,
            true => {}
        }
        let outcome = parser.core.advance_window::<F>(input, at_eof);
        match parser.note_advance(outcome)? {
            Advance::Record => {}
            _ => return Ok(None),
        }
        Ok(Some(Line::new(
            &mut parser.core,
            input,
            parser.consumed,
            Some(&mut parser.failed),
            false,
        )))
    }

    /// Move to the next record of the borrowed region of the chunk.
    ///
    /// The engine keeps only offsets, so bytes the parser does not own serve
    /// as its window once the offsets have been made to agree with them.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "a borrowed chunk advances once per record; folding the fixed parser and region state into direct owned reads removes the incremental hot-path call frame"
    )]
    fn advance_borrowed(&mut self) -> Result<bool, Error> {
        let input = self.region;
        let parser = &mut *self.parser;
        parser.check_failed_in(input)?;
        let at_eof = parser.finished;
        match parser.bom_resolved {
            false => parser.resolve_bom_in(input, at_eof)?,
            true => {}
        }
        let outcome = parser.core.advance_window::<F>(input, at_eof);
        Ok(parser.note_advance(outcome)? == Advance::Record)
    }

    /// View the record the last successful [`Self::advance`] reached.
    #[inline]
    pub(crate) fn current_line(&mut self) -> Line<'_, F> {
        let region = self.region;
        let borrowed = self.borrowed;
        let parser = &mut *self.parser;
        let buffer = match borrowed {
            true => region,
            false => parser.window.as_slice(),
        };
        Line::new(
            &mut parser.core,
            buffer,
            parser.consumed,
            Some(&mut parser.failed),
            false,
        )
    }

    /// Move to the next record satisfying `predicate`, skipping the rest.
    ///
    /// The literal skip only ever runs once the chunk has been borrowed:
    /// while it is still being absorbed the window may not yet hold a whole
    /// record, and [`Self::advance`] already has to re-parse a
    /// not-yet-terminated tail as more of the chunk arrives, so a skip ahead
    /// of that has nothing settled to skip past. Once borrowed, the region is
    /// this call's whole remaining input and [`Engine::skip_toward_literal`]
    /// only ever advances past a record ending it has already proven
    /// complete, so it stays correct across a chunk boundary the same way the
    /// pull front ends' skip already does.
    pub(crate) fn advance_with_filter(&mut self, predicate: &Predicate) -> Result<bool, Error> {
        let literal = self.parser.core.skip_literal_for::<F>(predicate);
        let mut pending = Option::default();
        while {
            match (self.borrowed, literal) {
                (true, Some(literal)) => {
                    let region = self.region;
                    self.parser
                        .core
                        .skip_toward_literal(region, literal, &mut pending);
                }
                _ => {}
            }
            self.advance()?
        } {
            // Still resolved inside the loop, because on this path the headers
            // can arrive with any chunk; the cache is what makes repeating it
            // cheap.
            let column = match predicate.column() {
                Column::Index(index) => *index,
                Column::Name(name) => {
                    let name = name.as_bytes();
                    match self.parser.core.cached_filter_column(name) {
                        Some(index) => index,
                        None => match self.header_index(name) {
                            Some(index) => {
                                self.parser.core.store_filter_column(name, index);
                                index
                            }
                            None => return Ok(false),
                        },
                    }
                }
            };
            let matched = {
                let region = self.region;
                let borrowed = self.borrowed;
                let parser = &mut *self.parser;
                let buffer = match borrowed {
                    true => region,
                    false => parser.window.as_slice(),
                };
                let field = parser.core.field::<F>(buffer, column).ok().flatten();
                predicate.matches_field(field)
            };
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Resolve a header name against the buffer the engine is reading.
    fn header_index(&mut self, name: &[u8]) -> Option<usize> {
        let region = self.region;
        let borrowed = self.borrowed;
        let parser = &mut *self.parser;
        let buffer = match borrowed {
            true => region,
            false => parser.window.as_slice(),
        };
        parser.core.header_index(buffer, name).ok().flatten()
    }

    /// Copy more of the chunk into the window, reporting whether any was left.
    ///
    /// Only as far as the byte after the next record ending is taken, because
    /// that is the least that lets the engine call the record whole, and a
    /// record ending that turns out to sit inside a quoted field simply brings
    /// the next absorption round.
    fn absorb(&mut self) -> Result<Absorbed, Error> {
        let rest = &self.input[self.absorbed..];
        // gamma::skip(cond.always_false, reason = "mutation prevents exhausted chunks from terminating")
        if rest.is_empty() {
            return Ok(Absorbed::Drained);
        }
        // Only the record being assembled is bounded: bytes the engine has
        // already reported are behind it, and one byte more than the limit
        // allows is admitted so an over-long record is recognized rather than
        // waited on.
        let room = self
            .parser
            .settings
            .limits
            .max_record_bytes
            .saturating_add(1)
            .saturating_sub(self.pending_window_bytes());
        // gamma::skip(cond.always_false, literal.int_increment, reason = "mutation prevents bounded overflow termination")
        if room == 0 {
            let window = mem::take(&mut self.parser.window);
            let error = self.parser.oversized_record_error(&window);
            self.parser.window = window;
            return Err(error);
        }
        // The first absorption of a chunk scans it whole; later ones step past
        // a growing stride so a record carrying many endings is not re-scanned
        // per ending. Only the whole-chunk scan can prove a terminator absent.
        let scanned_whole = self.stride == 0;
        let (scanned, unscanned) = match rest.get(..self.stride) {
            Some(scanned) => (scanned, &rest[self.stride..]),
            None => (rest, &[][..]),
        };
        let scan_from = scanned.len();
        let terminator = self.parser.core.record_terminator();
        let found = find1(terminator, unscanned);
        // A multi-byte record ending can straddle the chunk boundary: its lead
        // byte may already be in the window with only its tail arriving here.
        // The scan below would then find no lead and wrongly conclude the
        // record cannot have ended, so the completed record would be withheld
        // until the next lead byte turned up — never, on a stream that stalls.
        let tail_len = self.parser.core.record_ending_tail_len();
        let window = &self.parser.window;
        let tail_from = window.len().checked_sub(tail_len).unwrap_or_default();
        let straddling = window[tail_from..].contains(&terminator);
        let available = available_absorption_bytes(rest);
        let boundary = found.map_or(boundary_absorption_bytes(rest), |at| {
            scan_from
                .saturating_add(at)
                .saturating_add(2)
                .min(available)
        });
        let bounded = bounded_absorption_bytes(&room);
        let take = boundary.min(bounded);
        self.stride = self.stride.saturating_mul(2).max(ABSORB_STRIDE);
        self.parser.window.extend_from_slice(&rest[..take]);
        self.absorbed = self.absorbed.saturating_add(take);
        // A whole-chunk scan that found no terminator and was not cut short by
        // the record limit proves the record cannot have ended in this window.
        if scanned_whole && found.is_none() && take == rest.len() && !straddling {
            Ok(Absorbed::Interior)
        } else {
            Ok(Absorbed::Boundary)
        }
    }

    /// How much of the window the engine has not parsed past yet.
    fn pending_window_bytes(&self) -> usize {
        self.parser
            .window
            .len()
            .saturating_sub(self.parser.core.byte_offset())
    }

    /// Drop the window and read the rest of the chunk in place.
    ///
    /// The unparsed tail of the window is by then a copy of chunk bytes the
    /// absorption overshot into, so it is given back by rewinding the chunk
    /// offset rather than kept.
    fn enter_borrowed(&mut self) {
        let leftover = self.pending_window_bytes();
        let parser = &mut *self.parser;
        // Nothing can borrow the window here: the chunk holds the parser
        // mutably, so any line over it has already been dropped.
        parser.core.release_positioned_record();
        let parsed = parser.window.len().saturating_sub(leftover);
        parser.core.shift_window(&parser.window, parsed);
        parser.consumed = parser.consumed.saturating_add(parsed);
        parser.window.clear();
        parser.need_more_len = NO_REFUSAL;
        self.absorbed = self.absorbed.saturating_sub(leftover);
        self.region = &self.input[self.absorbed..];
        self.borrowed = true;
        self.direct_text = parser.headers_resolved && parser.bom_resolved && !parser.failed;
    }

    // gamma::skip(fn_value.zero, reason = "mutation prevents caller progress and times out")
    /// Hand the unread tail of the chunk back to the parser.
    fn settle(&mut self) -> usize {
        if self.settled {
            return self.absorbed;
        }
        self.settled = true;
        if self.borrowed {
            let buffer = self.region;
            let parser = &mut *self.parser;
            parser.core.release_positioned_record();
            let anchor = parser.core.window_anchor();
            debug_assert!(anchor <= buffer.len());
            parser.core.shift_window(buffer, anchor);
            parser.consumed = parser.consumed.saturating_add(anchor);
            parser.window.clear();
            // A caller that stops reading part way through leaves a tail that
            // is no longer bounded by a record, so it is taken a record limit
            // at a time rather than whole. Keeping the whole of it would let
            // the chunk size, which the caller picks, decide how much the
            // parser holds, when that is what the record limit is for.
            let tail = &buffer[anchor..];
            let take = tail
                .len()
                .min(parser.settings.limits.max_record_bytes.saturating_add(1));
            parser.window.extend_from_slice(&tail[..take]);
            parser.need_more_len = NO_REFUSAL;
            reclaim_to_len(&mut parser.window);
            parser.core.reclaim_scratch();
            self.absorbed = self.absorbed.saturating_add(anchor).saturating_add(take);
            self.region = &self.input[self.absorbed..];
        } else if !self.parser.failed
            && self
                .input
                .get(self.absorbed..)
                .is_some_and(|remaining| !remaining.is_empty())
        {
            // A caller that never drained, or that stopped at a record
            // boundary, leaves chunk bytes unread; the records in them can only
            // be reached from the window, so take what the record limit still
            // has room for. A drained chunk whose tail was already absorbed
            // whole -- the common short-chunk case -- has nothing left and
            // skips the scan entirely.
            loop {
                match self.absorb() {
                    Ok(Absorbed::Interior | Absorbed::Boundary) => {}
                    // gamma::skip(loop.break_to_continue, reason = "mutation loops forever after the chunk is drained")
                    Ok(_) => break,
                    // Settling cannot report, so the reason is kept for the
                    // next fallible call rather than reduced to the latched
                    // flag, which would lose the limit and the location.
                    Err(error) => {
                        debug_assert!(self.parser.failed);
                        self.parser.deferred.get_or_insert(error);
                        // gamma::skip(loop.break_to_continue, loop.delete_break, reason = "mutation loops forever after a deferred terminal error")
                        break;
                    }
                }
            }
        }
        self.absorbed
    }
}

// gamma::skip(fn_value.zero, reason = "mutation prevents absorption progress and times out")
fn available_absorption_bytes(input: &[u8]) -> usize {
    input.len()
}

// gamma::skip(fn_value.zero, expr.decrement, reason = "mutation prevents absorption progress and times out")
fn boundary_absorption_bytes(input: &[u8]) -> usize {
    input.len()
}

// gamma::skip(fn_value.zero, expr.decrement, reason = "mutation prevents bounded absorption progress and times out")
fn bounded_absorption_bytes(room: &usize) -> usize {
    let bounded = *room;
    bounded
}

fn reclaim_to_len<T>(buffer: &mut Vec<T>) {
    reclaim(buffer, buffer.as_slice().len());
}

impl<F: CsvFormat> Drop for Chunk<'_, '_, F> {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}

/// The smallest distance a repeated absorption reaches past the last one.
const ABSORB_STRIDE: usize = 64;

/// What one [`Chunk::absorb`] round did, so the caller can decide whether a
/// re-parse of the grown window could possibly find a record.
enum Absorbed {
    /// Nothing was left of the chunk to copy.
    Drained,
    /// Bytes were copied and a record ending may now sit in the window, so the
    /// window is worth re-parsing.
    Boundary,
    /// The whole chunk was copied and provably carries no record terminator,
    /// so the record being assembled cannot have ended and the re-parse can be
    /// skipped.
    Interior,
}

/// Convert a window-relative error into a stream-relative one.
///
/// Line numbers are already stream-absolute, because the newlines of every
/// dropped prefix were folded into the engine's line base as it was dropped.
fn rebase(mut error: Error, consumed: usize) -> Error {
    error.rebase_stream_window(consumed);
    error
}

impl Default for PushParser<Dynamic> {
    fn default() -> Self {
        Self::with_options(FormatOptions::CSV, ParseOptions::new())
            .expect("the default format and options are valid")
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod push_parser_tests {
    use super::{ABSORB_STRIDE, Absorbed, Advance, BOM, NO_REFUSAL, PushParser, reclaim_to_len};
    use crate::byte_record::ByteRecord;
    use crate::config::{FormatOptions, Headers, Limits, ParseOptions, ReadBom, RecordEnding};
    use crate::error::{Error, ErrorKind, Location};
    use crate::filter::Predicate;

    #[test]
    fn test_push_parser_static_new_and_borrowed_bom() {
        use crate::format::Csv;
        let mut parser =
            PushParser::<Csv>::new(ParseOptions::new().headers(Headers::None)).unwrap();
        let mut chunk = parser.chunk(b"col1,col2\nval1,val2\n");
        let mut line1 = chunk.next_line().unwrap().unwrap();
        assert_eq!(line1.record().unwrap().get_str(0).unwrap(), Some("col1"));
        let mut line2 = chunk.next_line().unwrap().unwrap();
        assert_eq!(line2.record().unwrap().get_str(0).unwrap(), Some("val1"));
        drop(chunk);
    }

    #[test]
    fn shrink_reclaims_outlier_window_capacity() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options");

        // One outsized record grows the window; without reclamation it would
        // stay that size for as long as the parser is alive.
        let huge = vec![b'q'; 512 * 1024];
        drain(&mut parser, &huge);
        let grown = parser.window.capacity();
        assert!(grown >= 512 * 1024);

        // Ordinary chunks afterwards must bring it back down on their own.
        drain(&mut parser, b",1\n");
        for index in 0..8 {
            drain(&mut parser, format!("small{index},2\n").as_bytes());
        }
        assert!(
            parser.window.capacity() < grown,
            "chunking should have reclaimed the outlier record's window"
        );
    }

    #[test]
    fn shrink_reclaims_engine_scratch_with_the_outlier_window() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options");

        let mut first = Vec::new();
        first.push(b'"');
        for _ in 0..128 * 1024 {
            first.extend_from_slice(b"x\"\"");
        }
        drain(&mut parser, &first);
        let mut chunk = parser.chunk(b"tail\"\nsmall\n");
        let mut line = chunk.next_line().expect("advance").expect("outlier");
        assert_eq!(
            line.record().expect("record").get(0).expect("field").len(),
            128 * 1024 * 2 + 4,
        );
        drop(line);
        let mut line = chunk.next_line().expect("advance").expect("small record");
        assert_eq!(
            line.record().expect("record").get(0),
            Some(b"small".as_slice()),
        );
        drop(line);
        assert!(chunk.next_line().expect("drained").is_none());
        assert!(chunk.borrowed());
        let grown_window = chunk.parser.window.capacity();
        let (_, grown_scratch) = chunk.parser.core.buffer_capacities();
        assert!(grown_scratch >= 128 * 1024);
        drop(chunk);

        let (_, reclaimed_scratch) = parser.core.buffer_capacities();
        assert!(
            parser.window.capacity() < grown_window,
            "borrowed settlement must reclaim the outlier window",
        );
        assert!(
            reclaimed_scratch < grown_scratch,
            "scratch must be reclaimed with the window",
        );
    }

    /// Lend `input` to `parser` in whole and read every record it completes.
    fn drain(parser: &mut PushParser, input: &[u8]) {
        let mut offset = 0;
        while offset < input.len() {
            let mut chunk = parser.chunk(&input[offset..]);
            while let Some(mut line) = chunk.next_line().expect("line") {
                line.record().expect("record");
            }
            offset += chunk.done();
        }
    }

    #[test]
    fn reset_recycles_the_parse_buffers_and_keeps_provided_headers() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid options");
        let mut headers = ByteRecord::new();
        headers.push_field(b"city");
        headers.push_field(b"country");
        parser.set_headers(headers);

        // Grow the span and scratch buffers with a wide, escape-heavy record.
        let mut input = Vec::new();
        for index in 0..64 {
            if index != 0 {
                input.push(b',');
            }
            input.extend_from_slice(b"\"escaped\"\"quote\"");
        }
        input.push(b'\n');
        parser.finish();
        let mut fed = 0;
        while fed < input.len() {
            let mut chunk = parser.chunk(&input[fed..]);
            let mut line = chunk
                .next_line()
                .expect("record parses")
                .expect("one record");
            assert_eq!(line.record().expect("record parses").len(), 64);
            fed += chunk.done();
        }

        let window = parser.window.capacity();
        let (spans, scratch) = parser.core.buffer_capacities();
        assert!(spans > 0 && scratch > 0, "the record grew both buffers");

        parser.reset();

        let (reset_spans, reset_scratch) = parser.core.buffer_capacities();
        assert_eq!(reset_spans, spans, "span capacity survives a reset");
        assert_eq!(reset_scratch, scratch, "scratch capacity survives a reset");
        assert_eq!(parser.window.capacity(), window, "the window survives too");

        // A reset restarts the stream but keeps the installed headers.
        assert_eq!(parser.location().byte, 0);
        assert!(!parser.is_done());
        assert!(parser.has_headers());
        assert_eq!(parser.header_index(b"country"), Some(1));
    }

    #[test]
    fn oversized_unterminated_chunk_is_bounded_before_failure() {
        let limits = Limits::new(8, 8, 8);
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None).limits(limits),
        )
        .expect("valid options");
        let input = vec![b'x'; 1024 * 1024];
        let mut fed = 0;
        let error = loop {
            let mut chunk = parser.chunk(&input[fed..]);
            let outcome = match chunk.next_line() {
                Ok(line) => {
                    assert!(line.is_none(), "no record can be completed");
                    Ok(())
                }
                Err(error) => Err(error),
            };
            fed += chunk.done();
            match outcome {
                Ok(()) => assert!(parser.window.len() <= 9, "window outgrew the record limit"),
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 8 });
        assert!(parser.window.len() <= 9);
        assert!(parser.window.len() < input.len());
        assert!(parser.window.capacity() < input.len());
    }

    #[test]
    fn push_parser_edge_cases_and_filter() {
        use crate::filter::Predicate;

        let mut p = PushParser::default();
        let mut chunk = p.chunk(b"a,b,a\n1,2,3\n4,5,6\n");
        let _ = chunk.next_line();
        // advance_with_filter for nonexistent column
        assert!(
            !chunk
                .advance_with_filter(&Predicate::equals("nonexistent", "val"))
                .unwrap()
        );
        let _ = chunk.done();

        let mut p_filter = PushParser::default();
        let mut c_filter = p_filter.chunk(b"name,city\nalice,boston\nbob,denver\n");
        let _ = c_filter.next_line(); // header
        assert!(
            c_filter
                .advance_with_filter(&Predicate::equals("city", "denver"))
                .unwrap()
        );
        assert!(
            !c_filter
                .advance_with_filter(&Predicate::equals("city", "austin"))
                .unwrap()
        );
        drop(c_filter);

        assert_eq!(p.header_indices(b"a"), &[0, 2]);
        assert_eq!(p.header_indices(b"missing"), &[] as &[usize]);

        // Straddling multi-byte record ending
        #[cfg(feature = "multibyte")]
        {
            let mut p_mb = PushParser::with_options(
                FormatOptions::CSV.record_ending_sequence(b";--"),
                ParseOptions::new().headers(Headers::None),
            )
            .unwrap();
            let mut c1 = p_mb.chunk(b"foo,bar;");
            let _ = c1.next_line();
            let _ = c1.done();
            let mut c2 = p_mb.chunk(b"--baz,qux;--");
            let mut line = c2.next_line().unwrap().unwrap();
            assert_eq!(line.record().unwrap().get_str(0).unwrap(), Some("foo"));
            drop(c2);
        }
    }

    #[test]
    fn push_parser_deferred_error_on_settle() {
        let limits = Limits::new(4, 4, 4);
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None).limits(limits),
        )
        .expect("valid options");
        let mut chunk1 = parser.chunk(b"foo,");
        let _ = chunk1.next_line();
        let _ = chunk1.done();

        let chunk2 = parser.chunk(b"toolongfieldthatcannotfitinlimit");
        drop(chunk2);
        let mut chunk3 = parser.chunk(b"");
        let res = chunk3.next_line();
        assert!(res.is_err());
        drop(chunk3);

        // Test record_limit_error directly
        let err = parser.record_limit_error(b"foo\nbar\n");
        assert!(matches!(err.kind(), ErrorKind::RecordTooLarge { .. }));

        // ReadBom::Reject on PushParser
        let mut p_bom = PushParser::with_options(
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut c_bom = p_bom.chunk(b"\xEF\xBB\xBFa,b\n");
        assert!(c_bom.next_line().is_err());
        drop(c_bom);

        // Invalid format options in with_options
        assert!(
            PushParser::with_options(FormatOptions::CSV.quote(b','), ParseOptions::new(),).is_err()
        );

        // Drop chunk with multiple unread lines so settle loop absorbs boundaries
        let mut p_settle_loop = PushParser::default();
        let c_sl = p_settle_loop.chunk(b"a,b\n1,2\n3,4\n5,6\n");
        drop(c_sl);
        assert_eq!(p_settle_loop.location().record, 0);

        // check_failed_in when failed is true
        let mut c_failed = p_bom.chunk(b"a,b\n");
        assert!(c_failed.next_line().is_err());
        drop(c_failed);

        // check_failed_in when failed is true with deferred error
        let mut p_def = PushParser::default();
        p_def.failed = true;
        p_def.deferred = Some(crate::Error::new(
            ErrorKind::ParserFailed,
            crate::Location::START,
        ));
        let mut c_def = p_def.chunk(b"a,b\n");
        assert!(c_def.next_line().is_err());
        drop(c_def);

        // advance_with_filter error on corrupted field
        let mut p_bad = PushParser::default();
        let mut c_bad = p_bad.chunk(b"a,b\na\"b,1\n");
        let _ = c_bad.next_line(); // header
        assert!(
            c_bad
                .advance_with_filter(&Predicate::equals("a", "val"))
                .is_err()
        );
        drop(c_bad);

        // advance_with_filter where advance() succeeds but field::<F> fails due to limits
        let mut p_field_err = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::FirstRecord)
                .limits(Limits::new(100, 2, 10)),
        )
        .unwrap();
        let mut c_fe = p_field_err.chunk(b"h1,h2\n\"toolong\",1\n");
        let _ = c_fe.next_line(); // read header
        assert!(
            c_fe.advance_with_filter(&Predicate::equals("h1", "val"))
                .is_err()
        );
        drop(c_fe);

        // advance_absorbing error propagation when window has syntax error
        let mut p_syntax = PushParser::default();
        let mut c_syn1 = p_syntax.chunk(b"foo,");
        let _ = c_syn1.next_line();
        let _ = c_syn1.done();
        let mut c_syn2 = p_syntax.chunk(b"\"bad\"quote\n");
        assert!(c_syn2.next_line().is_err());
        drop(c_syn2);

        // next_borrowed_line error with failed parser or ReadBom::Reject
        let mut p_nb_fail = PushParser::default();
        p_nb_fail.failed = true;
        let mut c_nb = p_nb_fail.chunk(b"a,b\n");
        assert!(c_nb.next_borrowed_line().is_err());
        drop(c_nb);

        let mut p_nb_bom = PushParser::with_options(
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut c_nb_bom = p_nb_bom.chunk(b"\xEF\xBB\xBFa,b\n");
        assert!(c_nb_bom.next_borrowed_line().is_err());
        drop(c_nb_bom);

        // PushParser::new with invalid options
        assert!(
            PushParser::<crate::format::Csv>::new(ParseOptions::new().buffer_capacity(0)).is_err()
        );

        // chunk.advance() with ReadBom::Reject
        let mut p_adv_bom = PushParser::with_options(
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .unwrap();
        let mut c_adv = p_adv_bom.chunk(b"\xEF\xBB\xBFa,b\n");
        assert!(c_adv.advance().is_err());
        drop(c_adv);
    }

    #[test]
    fn header_and_reset_state_are_exact() {
        let mut parser = PushParser::with_options(FormatOptions::CSV, ParseOptions::new())
            .expect("valid parser");
        assert!(parser.headers().is_none());
        assert_eq!(parser.header_index("city"), None);
        assert!(parser.header_indices("city").is_empty());
        assert!(parser.has_headers());

        let mut provided = ByteRecord::new();
        provided.push_field(b"city");
        provided.push_field(b"country");
        parser.set_headers(provided);
        assert_eq!(parser.header_index("country"), Some(1));
        assert!(parser.headers_resolved);

        parser.window.extend_from_slice(b"partial");
        parser.need_more_len = 3;
        parser.consumed = 9;
        parser.finished = true;
        parser.failed = true;
        parser.deferred = Some(Error::new(ErrorKind::ParserFailed, Location::START));
        parser.bom_resolved = false;
        parser.headers_resolved = false;
        parser.reset();

        assert!(parser.window.is_empty());
        assert_eq!(parser.need_more_len, NO_REFUSAL);
        assert_eq!(parser.consumed, 0);
        assert!(!parser.finished);
        assert!(!parser.failed);
        assert!(parser.deferred.is_none());
        assert!(!parser.bom_resolved);
        assert!(parser.headers_resolved);
        assert_eq!(parser.location(), Location::START);
        assert_eq!(parser.header_index("country"), Some(1));

        let unheaded = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        assert!(!unheaded.has_headers());

        let mut failed = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        failed.failed = true;
        assert!(failed.is_done(), "a failed parser is immediately done");

        let mut preserved = PushParser::with_options(
            FormatOptions::CSV.read_bom(ReadBom::Preserve),
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        preserved.bom_resolved = false;
        preserved.reset();
        assert!(preserved.bom_resolved);

        let mut unsettled = PushParser::with_options(FormatOptions::CSV, ParseOptions::new())
            .expect("valid parser");
        {
            let chunk = unsettled.chunk(b"name,value\n");
            drop(chunk);
        }
        assert_eq!(unsettled.window, b"name,value\n");
        assert!(!unsettled.headers_resolved);
        assert!(!unsettled.headers_settled());
        assert!(
            unsettled.headers().is_none(),
            "a whole but unparsed header remains unsettled",
        );
    }

    #[test]
    fn need_more_cache_and_bom_resolution_track_progress() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        parser.window.extend_from_slice(b"partial");
        assert_eq!(
            parser.window_advance(false).expect("partial parse"),
            Advance::NeedMore,
        );
        assert_eq!(parser.need_more_len, b"partial".len());
        assert!(!parser.headers_resolved);

        let mut prefix = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        prefix.resolve_bom_in(b"\xEF", false).expect("BOM prefix");
        assert!(!prefix.bom_resolved);
        prefix
            .resolve_bom_in(b"\xEF", true)
            .expect("BOM prefix at EOF");
        assert!(prefix.bom_resolved);

        let mut non_bom = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        non_bom.resolve_bom_in(b"x", false).expect("not a BOM");
        assert!(non_bom.bom_resolved);

        let mut rejected = PushParser::with_options(
            FormatOptions::CSV.read_bom(ReadBom::Reject),
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        let error = rejected
            .resolve_bom_in(BOM, false)
            .expect_err("configured rejection");
        assert_eq!(error.kind(), ErrorKind::RejectedBom);
        assert!(rejected.failed);
        assert!(rejected.bom_resolved);

        let mut chunked = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        chunked.window.extend_from_slice(b"pre");
        {
            let mut chunk = chunked.chunk(b"partial");
            assert!(!chunk.advance().expect("partial chunk"));
            assert_eq!(chunk.parser.need_more_len, b"prepartial".len());
            assert_eq!(chunk.parser.window.len(), b"prepartial".len());
        }
    }

    #[test]
    fn absorb_advances_by_the_exact_boundary_and_stride() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        parser.window.extend_from_slice(b"prefix");
        let mut chunk = parser.chunk(b"x\nrest");
        assert!(!chunk.borrowed());
        assert!(matches!(
            chunk.absorb().expect("absorb"),
            Absorbed::Boundary
        ));
        assert_eq!(chunk.absorbed, 3);
        assert_eq!(chunk.stride, ABSORB_STRIDE);
        assert_eq!(chunk.parser.window, b"prefixx\nr");
        assert!(matches!(
            chunk.absorb().expect("absorb tail"),
            Absorbed::Boundary
        ));
        assert_eq!(chunk.absorbed, b"x\nrest".len());
        assert_eq!(chunk.stride, ABSORB_STRIDE * 2);
        drop(chunk);

        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        parser.window.push(b'p');
        let mut chunk = parser.chunk(b"abcdef");
        assert!(matches!(
            chunk.absorb().expect("whole interior"),
            Absorbed::Interior
        ));
        assert_eq!(chunk.absorbed, 6);
        assert_eq!(chunk.stride, ABSORB_STRIDE);
        assert_eq!(chunk.parser.window, b"pabcdef");

        let mut limited = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(3, 32, 8)),
        )
        .expect("valid parser");
        limited.window.extend_from_slice(b"abcd");
        let mut chunk = limited.chunk(b"x");
        let error = match chunk.absorb() {
            Ok(_) => panic!("no room remains"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(chunk.parser.window, b"abcd");

        let mut crlf = PushParser::with_options(
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        crlf.window.extend_from_slice(b"value\r");
        let mut chunk = crlf.chunk(b"\n");
        assert!(matches!(
            chunk.absorb().expect("straddling ending"),
            Absorbed::Boundary,
        ));
        assert_eq!(chunk.parser.window, b"value\r\n");

        let mut newline = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        newline.window.extend_from_slice(b"done\npartial");
        let mut chunk = newline.chunk(b"more");
        assert!(matches!(
            chunk.absorb().expect("single-byte ending has no tail"),
            Absorbed::Interior,
        ));
    }

    #[test]
    fn absorbing_and_borrowed_transitions_preserve_every_record() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        {
            let mut first = parser.chunk(b"part");
            assert!(first.next_line().expect("partial").is_none());
            assert_eq!(first.done(), 4);
        }
        assert_eq!(parser.window, b"part");
        assert_eq!(parser.need_more_len, NO_REFUSAL);

        let mut chunk = parser.chunk(b"ial,1\nnext,2\n");
        let mut first = chunk.next_line().expect("advance").expect("first record");
        assert_eq!(
            first.record().expect("record").iter().collect::<Vec<_>>(),
            [b"partial".as_slice(), b"1"],
        );
        drop(first);
        let mut second = chunk.next_line().expect("advance").expect("second record");
        let second_fields = second
            .record()
            .expect("record")
            .iter()
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        drop(second);
        assert!(chunk.borrowed());
        assert_eq!(second_fields, [b"next".to_vec(), b"2".to_vec()]);
        assert!(chunk.next_line().expect("drained").is_none());
        assert_eq!(chunk.done(), b"ial,1\nnext,2\n".len());
        assert!(parser.window.is_empty());
        assert_eq!(parser.need_more_len, NO_REFUSAL);
    }

    #[test]
    fn dropping_a_partially_read_borrowed_chunk_keeps_its_tail() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        {
            let mut chunk = parser.chunk(b"a,1\nb,2\nc,3\n");
            let mut line = chunk.next_line().expect("advance").expect("first");
            assert_eq!(line.record().expect("record").get(0), Some(b"a".as_slice()));
        }

        assert_eq!(parser.consumed, 4);
        assert_eq!(parser.window, b"b,2\nc,3\n");
        assert_eq!(parser.need_more_len, NO_REFUSAL);
        parser.finish();
        let mut chunk = parser.chunk(b"");
        let mut values = Vec::new();
        while let Some(mut line) = chunk.next_line().expect("advance") {
            values.push(
                line.record()
                    .expect("record")
                    .get(0)
                    .expect("first field")
                    .to_vec(),
            );
        }
        assert_eq!(values, [b"b".to_vec(), b"c".to_vec()]);
    }

    #[test]
    fn filter_skip_avoids_parsing_non_candidate_width_errors() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .field_count(crate::config::FieldCount::Exact(2)),
        )
        .expect("valid parser");
        parser.finish();
        let mut chunk = parser.chunk(b"skip,0\nbad,too,many\nwanted,1\n");
        let predicate = Predicate::equals(1, "1");
        let mut line = chunk
            .next_matching_line(&predicate)
            .expect("filter")
            .expect("matching record");
        assert_eq!(
            line.record().expect("record").iter().collect::<Vec<_>>(),
            [b"wanted".as_slice(), b"1"],
        );

        drop(line);
        drop(chunk);
        let mut named = PushParser::with_options(FormatOptions::CSV, ParseOptions::new())
            .expect("valid parser");
        named.finish();
        let mut chunk = named.chunk(b"name,value\nskip,0\nwanted,1\n");
        assert!(
            chunk
                .advance_with_filter(&Predicate::equals("name", "wanted"))
                .expect("named filter")
        );
        assert_eq!(chunk.parser.core.cached_filter_column(b"name"), Some(0));
    }

    #[test]
    fn record_limit_error_reports_the_exact_cut_and_line() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(3, 32, 8)),
        )
        .expect("valid parser");
        let error = parser.record_limit_error(b"ab\ncd");
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(error.location().byte, 4);
        assert_eq!(error.location().line, 2);
        assert_eq!(error.location().record, 0);

        parser.consumed = 7;
        let error = parser.record_limit_error(b"ab\ncd");
        assert_eq!(error.location().byte, 11);
        assert_eq!(error.location().line, 2);

        parser.consumed = 0;
        let error = parser.record_limit_error(b"\nabcd");
        assert_eq!(error.location().byte, 4);
        assert_eq!(
            error.location().line,
            2,
            "a newline exactly at the retained start is counted",
        );

        let error = parser.record_limit_error(b"abc\nd");
        assert_eq!(error.location().byte, 4);
        assert_eq!(
            error.location().line,
            2,
            "a newline exactly at the retained end is counted",
        );
    }

    #[test]
    fn oversized_record_reports_the_narrower_field_limit() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(12, 6, 8)),
        )
        .expect("valid parser");
        let error = parser.oversized_record_error(b"\"multi\r\nline\"");
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 6 });
        assert_eq!(error.location().byte, 8);
    }

    #[test]
    fn equality_borrows_and_settlement_keeps_exact_bounded_tail() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(3, 32, 8)),
        )
        .expect("valid parser");
        {
            let mut chunk = parser.chunk(b"a\nb\nc\n");
            assert_eq!(chunk.pending_window_bytes(), chunk.absorbed);
            assert!(chunk.advance().expect("first record"));
            assert!(chunk.borrowed(), "equality enters borrowed mode");
            let mut line = chunk.current_line();
            assert_eq!(line.record().expect("record").get(0), Some(b"a".as_slice()));
        }
        assert_eq!(parser.consumed, 2);
        assert_eq!(parser.window, b"b\nc\n");
        assert_eq!(parser.window.len(), 4, "the limit keeps one probe byte");
        assert_eq!(parser.need_more_len, NO_REFUSAL);
        assert!(!parser.failed);
    }

    #[test]
    fn borrowed_parse_errors_poison_the_next_view() {
        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        parser.finish();
        let mut chunk = parser.chunk(b"bad\"quote,value\n");
        assert!(chunk.advance().expect("position malformed record"));
        let mut line = chunk.current_line();
        let error = line.record().expect_err("malformed borrowed record");
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        drop(line);
        assert!(chunk.parser.failed);
        assert_eq!(
            chunk
                .next_borrowed_line()
                .expect_err("failure remains latched")
                .kind(),
            ErrorKind::ParserFailed,
        );
    }

    #[test]
    fn entering_and_settling_chunks_publish_exact_state() {
        let mut positioned = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        positioned.window.extend_from_slice(b"a\n");
        assert_eq!(
            positioned.window_advance(false).expect("position record"),
            Advance::Record,
        );
        positioned.need_more_len = 7;
        {
            let mut chunk = positioned.chunk(b"b\n");
            chunk.enter_borrowed();
            assert!(chunk.borrowed);
            assert_eq!(chunk.parser.consumed, 2);
            assert!(chunk.parser.window.is_empty());
            assert_eq!(chunk.parser.need_more_len, NO_REFUSAL);
            assert_eq!(chunk.absorbed, 0);
            assert_eq!(chunk.region, b"b\n");
            assert!(chunk.advance_borrowed().expect("borrowed record"));
            let mut line = chunk.current_line();
            assert_eq!(line.record().expect("record").get(0), Some(b"b".as_slice()));
        }

        let mut parser = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(2, 32, 8)),
        )
        .expect("valid parser");
        let input = b"tail";
        {
            let mut chunk = parser.chunk(input);
            chunk.borrowed = true;
            chunk.region = input;
            chunk.parser.window.extend_from_slice(b"stale");
            chunk.parser.need_more_len = 3;
            assert_eq!(chunk.settle(), 3);
            assert_eq!(chunk.settle(), 3, "settling twice reports no progress");
            assert_eq!(chunk.parser.window, b"tai");
            assert_eq!(chunk.parser.need_more_len, NO_REFUSAL);
            assert_eq!(chunk.region, b"l");
        }

        let mut single = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid parser");
        {
            let chunk = single.chunk(b"x");
            drop(chunk);
        }
        assert_eq!(single.window, b"x", "the final unread byte is absorbed");

        let mut deferred = PushParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(Limits::new(3, 32, 8)),
        )
        .expect("valid parser");
        deferred.window.extend_from_slice(b"abcd");
        {
            let chunk = deferred.chunk(b"x");
            drop(chunk);
        }
        assert!(deferred.failed);
        assert!(deferred.deferred.is_some());
        assert_eq!(
            deferred
                .chunk(b"")
                .advance()
                .expect_err("deferred limit")
                .kind(),
            ErrorKind::RecordTooLarge { limit: 3 },
        );

        let live = 9 * 1024;
        let mut window = Vec::with_capacity(live * 5);
        window.resize(live, b'x');
        reclaim_to_len(&mut window);
        assert_eq!(window.capacity(), live);
    }
}

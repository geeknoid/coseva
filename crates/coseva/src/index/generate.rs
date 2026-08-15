use super::*;
use crate::config::{BlankRecords, Escape, Quoting, RecordEnding};
use crate::search::{find1_near, find2_near, find3_near, find4_near};

const fn should_flush_buffer(buffered: usize, threshold: usize) -> bool {
    buffered >= threshold
}

impl CsvIndex {
    /// Encode records into `output` while streaming an index of them into
    /// `index`.
    ///
    /// Record positions are already known at write time. Each encoded record
    /// is parsed before it is committed so the stored parser limits and
    /// parser-visible record boundaries are guaranteed to match a later
    /// [`CsvIndex::build`], without rereading the output or holding the whole
    /// document in memory.
    ///
    /// Both the document and the index are written incrementally, so this holds
    /// neither in memory. The resulting index is byte-for-byte what indexing
    /// the generated document afterwards would have produced.
    ///
    /// A header record is written first when
    /// [`EmitOptions::has_headers`] is set, and it is indexed as record zero,
    /// exactly as [`CsvIndex::build`] would index it.
    ///
    /// ```
    /// use std::io::Cursor;
    /// use coseva::config::EmitOptions;
    /// use coseva::index::{CsvIndex, IndexOptions};
    /// # #[cfg(feature = "derive")] {
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     population: u32,
    /// }
    ///
    /// let cities = [
    ///     City { name: "Boston", population: 650_706 },
    ///     City { name: "Denver", population: 715_522 },
    /// ];
    ///
    /// let mut output = Vec::new();
    /// let mut reader = CsvIndex::generate(
    ///     &mut output,
    ///     Cursor::new(Vec::new()),
    ///     cities,
    ///     IndexOptions::default(),
    ///     EmitOptions::new(),
    /// )?;
    /// assert_eq!(reader.len(), 3);
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_reader(Cursor::new(&output), 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the writers
    ///
    /// `output` and `index` are taken by value, and the returned reader hands
    /// the index back through [`CsvIndexReader::into_inner`]. A caller that
    /// must keep either can pass `&mut writer`, since `&mut W` implements
    /// [`Write`] and [`Seek`] wherever `W` does.
    ///
    /// # Errors
    ///
    /// Returns a configuration error, the first typed encoding or field-count
    /// error, or an error writing either the document or the index.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn generate<W, X, T, I>(
        output: W,
        index: X,
        values: I,
        options: IndexOptions,
        encode: EmitOptions,
    ) -> Result<CsvIndexReader<X>, Error>
    where
        W: Write,
        X: Read + Write + Seek,
        T: CsvEncode,
        I: IntoIterator<Item = T>,
    {
        encode.validate_buffered(options.format)?;
        let mut index = index;
        index_seek(&mut index, SeekFrom::Start(0))?;
        let mut entries = BufWriter::new(index);
        entries
            .write_all(&[0; FIXED_HEADER_BYTES])
            .map_err(Error::io_at_start)?;

        let mut sink = HashingWriter::new(output);
        // The emitter splices a byte-order mark in front of the first record it
        // accepts, which would shift every offset already measured, so it is
        // written here instead and accounted for before any record exists.
        if options.format.emits_bom() {
            sink.write_all(b"\xEF\xBB\xBF")
                .map_err(Error::io_at_start)?;
        }
        let format = options.format.write_bom(WriteBom::Omit);
        let threshold = encode.capacity();
        let mut core = PushEmitter::with_options(format, encode)?;
        let mut validator = RecordValidator::new(format, options.limits)?;
        let fast = FastValidator::for_format(format);

        let mut count: u64 = 0;
        let mut newlines: u64 = 0;
        let mut flushed = sink.len;
        let mut entries_hasher = Xxh3::new();

        if encode.writes_headers() {
            core.encode_header::<T>()?;
            let record_newlines =
                validate_record(fast.as_ref(), &mut validator, core.buffer(), options.limits)?;
            count = record_entry(
                &mut entries,
                &mut entries_hasher,
                flushed,
                &mut newlines,
                record_newlines,
                count,
            )?;
        }

        for value in values {
            let start = core.len();
            core.encode(&value)?;
            let offset = flushed + start as u64;
            let record_newlines = validate_record(
                fast.as_ref(),
                &mut validator,
                &core.buffer()[start..],
                options.limits,
            )?;
            count = record_entry(
                &mut entries,
                &mut entries_hasher,
                offset,
                &mut newlines,
                record_newlines,
                count,
            )?;
            if should_flush_buffer(core.len(), threshold) {
                sink.write_all(core.buffer()).map_err(Error::io_at_start)?;
                flushed = sink.len;
                core.clear();
            }
        }

        sink.write_all(core.buffer()).map_err(Error::io_at_start)?;
        sink.flush().map_err(Error::io_at_start)?;

        let mut index = entries
            .into_inner()
            .map_err(|error| Error::io(error.into_error(), Location::START))?;
        let header = encode_header(
            sink.len,
            sink.hasher.digest128().to_le_bytes(),
            options.format,
            options.limits,
            count,
        );
        index_seek(&mut index, SeekFrom::Start(0))?;
        index.write_all(&header).map_err(Error::io_at_start)?;

        // Entries were hashed as they were written above, and the header
        // checksum is a one-shot hash of the header now sitting in memory,
        // so neither authentication needs the payload read back.
        let entries_checksum = entries_hasher.digest128().to_le_bytes();
        let header_checksum = hash_bytes(&header);
        index_seek(&mut index, SeekFrom::End(0))?;
        index
            .write_all(&entries_checksum)
            .map_err(Error::io_at_start)?;
        index
            .write_all(&header_checksum)
            .map_err(Error::io_at_start)?;
        index.flush().map_err(Error::io_at_start)?;
        CsvIndexReader::new(index)
    }

    /// Generate a CSV file and its index together, in constant memory.
    ///
    /// See [`CsvIndex::generate`]. This is the entry point for producing a file
    /// larger than memory that is randomly addressable the moment it exists.
    ///
    /// ```
    /// use coseva::config::EmitOptions;
    /// use coseva::index::{CsvIndex, IndexOptions};
    /// # #[cfg(feature = "derive")] {
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     population: u32,
    /// }
    ///
    /// let cities = [
    ///     City { name: "Boston", population: 650_706 },
    ///     City { name: "Denver", population: 715_522 },
    /// ];
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// let index_path = directory.path().join("cities.idx");
    ///
    /// let mut reader = CsvIndex::generate_path(
    ///     &source_path,
    ///     &index_path,
    ///     cities,
    ///     IndexOptions::default(),
    ///     EmitOptions::new(),
    /// )?;
    /// assert_eq!(reader.len(), 3);
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_path(&source_path, 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    ///
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, an error when either file cannot be
    /// created, or the first typed encoding, field-count, or I/O error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn generate_path<T, I>(
        source_path: impl AsRef<Path>,
        index_path: impl AsRef<Path>,
        values: I,
        options: IndexOptions,
        encode: EmitOptions,
    ) -> Result<CsvIndexReader<File>, Error>
    where
        T: CsvEncode,
        I: IntoIterator<Item = T>,
    {
        let source = File::create(source_path).map_err(Error::io_at_start)?;
        let index = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(index_path)
            .map_err(Error::io_at_start)?;
        Self::generate(source, index, values, options, encode)
    }
}

/// Confirm one record with [`FastValidator`] where possible, falling back to
/// [`RecordValidator`] otherwise.
///
/// [`FastValidator::for_format`] is computed once per call to
/// [`CsvIndex::generate`], not per record, and [`FastValidator::validate`]
/// itself never reparses anything: it is `None` up front for a format the
/// scan can never soundly cover (such as
/// [`Quoting::Raw`](crate::config::Quoting::Raw)), and otherwise reports
/// `Ok(false)` for the rare record whose bytes it cannot confidently account
/// for by scanning alone, at which point `RecordValidator` settles it exactly
/// as it always has.
fn validate_record(
    fast: Option<&FastValidator>,
    validator: &mut RecordValidator,
    record: &[u8],
    limits: Limits,
) -> Result<u64, Error> {
    let fast_result = match fast {
        Some(fast) => fast.validate(record, limits)?,
        None => None,
    };
    if let Some(newlines) = fast_result {
        return Ok(newlines);
    }
    validator.validate(record)?;
    Ok(count_newlines(record))
}

/// Reused per-record validator for [`CsvIndex::generate`].
///
/// Constructing a fresh [`SliceParser`] for every record made parser
/// construction and its buffer allocations the dominant cost of generating
/// small records, since each one was discarded right after a single record
/// was checked. This keeps one [`PushParser`] alive for the whole call to
/// [`CsvIndex::generate`], which reuses every buffer the parser has already
/// grown instead of reallocating.
///
/// `field_count` is deliberately pinned to [`FieldCount::MatchFirst`] rather
/// than following the caller's configured field-count policy. A validation
/// session never holds a second record to compare a first one against, so
/// `MatchFirst` and `Flexible` validate identically here — but `MatchFirst`
/// also disqualifies the engine's specialized whole-record fast path. Forcing
/// the general path keeps every record checked against the caller's configured
/// limits exactly as [`CsvIndex::build`] would.
///
/// [`FastValidator`] now handles the overwhelming majority of records at a
/// fraction of the cost, so this is reached only for a format
/// [`FastValidator::for_format`] declines up front (such as
/// [`Quoting::Raw`](crate::config::Quoting::Raw), whose encoded output can be
/// structurally ambiguous, or a multi-byte dialect) or for the rare record a
/// sound scan cannot confidently settle on its own.
struct RecordValidator {
    parser: PushParser<Dynamic>,
}

impl RecordValidator {
    fn new(format: FormatOptions, limits: Limits) -> Result<Self, Error> {
        let parser = PushParser::with_options(
            format.read_bom(ReadBom::Preserve),
            ParseOptions::new()
                .headers(Headers::None)
                .limits(limits)
                .field_count(FieldCount::MatchFirst),
        )?;
        Ok(Self { parser })
    }

    /// Confirm `record` is exactly one parser-visible record within the
    /// configured limits, exactly as a fresh parser over `record` alone
    /// would report, without constructing or allocating a new parser.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn validate(&mut self, record: &[u8]) -> Result<(), Error> {
        // The whole record is already in hand, so the stream is complete the
        // moment it is handed over; this settles a final record lacking a
        // terminator exactly as a fresh one-shot parser would.
        self.parser.finish();
        let mut chunk = self.parser.chunk(record);
        let Some(mut line) = chunk.next_line()? else {
            return Err(Error::detailed(ErrorKind::Encode, ENCODED_VALUE_NO_RECORD));
        };
        line.record()?;
        if chunk.next_line()?.is_some() {
            return Err(Error::detailed(
                ErrorKind::Encode,
                ENCODED_VALUE_MULTIPLE_RECORDS,
            ));
        }
        Ok(())
    }
}

/// Which structural byte a scanned field ended on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FieldEnd {
    /// The field ended at a delimiter, at this content-relative offset; a
    /// following field starts immediately after it.
    Delimiter(usize),
    /// The field ran to the end of the record's content; nothing follows.
    Record,
}

/// How a quoted field's closing quote is distinguished from a doubled or
/// escaped one, mirroring [`emit::write_quoted`](crate::emit::write_quoted)'s
/// two quoted-escape styles.
#[derive(Clone, Copy)]
enum FrameEscape {
    /// [`Escape::DoubleQuote`]: a literal quote is written twice.
    Doubled,
    /// [`Escape::Backslash`]: a literal quote or the escape byte itself is
    /// written after this byte.
    Backslash(u8),
}

/// The two mutually exclusive families [`FastValidator`] scans, matching the
/// two field writers in `emit.rs`.
#[derive(Clone, Copy)]
enum FastMode {
    /// [`Quoting::Necessary`], [`Quoting::NonNumeric`], [`Quoting::Always`] or
    /// [`Quoting::Never`], paired with [`Escape::DoubleQuote`] or
    /// [`Escape::Backslash`]. A field either opens with the quote byte, or by
    /// construction of `needs_quotes`/`needs_csv_quotes` contains none of the
    /// delimiter, the quote, the record ending byte, or (in a newline-based
    /// dialect) a bare `\r`.
    Framed(FrameEscape),
    /// [`Escape::Mysql`] or [`Escape::Unquoted`], always paired with
    /// [`Quoting::Never`]. Nothing is ever quoted; every structural byte,
    /// including the escape byte itself, is individually escaped, so a bare
    /// delimiter always marks a genuine field boundary.
    Escaped(u8),
}

/// A byte-level validator for [`CsvIndex::generate`] that confirms a record's
/// boundaries and the caller's configured [`Limits`] without driving a
/// parser.
///
/// [`RecordValidator`] removed the per-record allocation of reparsing, but
/// still walked every byte of every record through the general parsing
/// engine, which measured as the dominant remaining cost of `generate`. Most
/// formats do not need that: a field is either quoted or, by construction of
/// the emitter's own `needs_quotes`/`needs_csv_quotes` safety check, free of
/// every byte that could be mistaken for structure, and an escaped-unquoted
/// field escapes every such byte individually. Those two shapes are exactly
/// what [`FastMode::Framed`] and [`FastMode::Escaped`] scan for directly, so
/// this never re-derives a field's *value* the way a parser would (no
/// unescaping, no NULL handling, no trimming) — it only needs to agree with a
/// parser about where each field starts and ends, and whether the configured
/// [`Limits`] are satisfied, both of which are properties of the raw bytes
/// alone.
///
/// This is deliberately conservative rather than exhaustive: the moment a
/// record contains anything this scan cannot confidently explain from the
/// safety contract above — an unterminated quote, a dangling escape, or a
/// structural byte where none of the safe encodings should ever place one —
/// [`Self::validate`] returns `Ok(false)` instead of guessing, and the caller
/// falls back to [`RecordValidator`], which settles it by actually
/// reparsing. A record this scan does confirm is therefore never one it
/// merely assumed was fine; every accept is a proof, not a shortcut around
/// one.
///
/// Every field boundary is found directly with this crate's own
/// [`find1_near`]/[`find2_near`]/[`find3_near`]/[`find4_near`] — the same
/// SIMD-accelerated search `borrowed_parser.rs` uses when reading a stored
/// source. Benchmarking across `benches/index.rs` confirmed calling
/// `find*_near` directly is faster than using a scalar prefix on short fields.
struct FastValidator {
    delimiter: u8,
    quote: u8,
    /// [`RecordEnding::byte`](crate::config::RecordEnding::byte): `\n` for
    /// [`RecordEnding::Newline`](crate::config::RecordEnding::Newline) and
    /// [`RecordEnding::CrLf`](crate::config::RecordEnding::CrLf), or the
    /// configured byte otherwise.
    terminator: u8,
    /// Whether the terminator is `\r\n` rather than one byte.
    crlf: bool,
    /// Whether a bare `\r` outside a quoted field is itself structural, as it
    /// is for [`RecordEnding::Newline`] and [`RecordEnding::CrLf`].
    newline: bool,
    /// Whether [`BlankRecords::Skip`] makes a zero-content record ambiguous.
    blank_skip: bool,
    mode: FastMode,
}

impl FastValidator {
    /// Build a scanner for `format`, or `None` when no scan can soundly cover
    /// it and every record must keep going through [`RecordValidator`].
    ///
    /// Declines [`Quoting::Raw`] (no escaping at all, so encoded output can be
    /// structurally ambiguous by design) and any multi-byte dialect (outside
    /// the single-byte matching every other fast path in this crate relies
    /// on). For [`FastMode::Framed`] shapes it also declines a
    /// [`Syntax`](crate::config::Syntax) that disables quote syntax on read
    /// — [`Recovery::NONE`](crate::config::Recovery::NONE) among others —
    /// since this scan's field boundaries depend on a leading quote byte
    /// meaning the same thing to a parser that it does here.
    fn for_format(format: FormatOptions) -> Option<Self> {
        if format.quoting == Quoting::Raw || format.dialect.multibyte() || format.skip_initial_space
        {
            return None;
        }
        let dialect = format.dialect;
        let mode = if let Some(escape) = dialect.escape.unquoted_byte() {
            FastMode::Escaped(escape)
        } else {
            if !format.syntax.quoting_enabled() {
                return None;
            }
            let escape = match dialect.escape {
                Escape::Backslash(escape) => FrameEscape::Backslash(escape),
                _ => FrameEscape::Doubled,
            };
            FastMode::Framed(escape)
        };
        Some(Self {
            delimiter: dialect.delimiter,
            quote: dialect.quote,
            terminator: dialect.record_ending.byte(),
            crlf: dialect.record_ending == RecordEnding::CrLf,
            newline: matches!(
                dialect.record_ending,
                RecordEnding::Newline | RecordEnding::CrLf
            ),
            blank_skip: format.blank_records == BlankRecords::Skip,
            mode,
        })
    }

    /// Confirm `record` (the encoded record, including its terminator) is
    /// exactly one parser-visible record within `limits`, purely by scanning
    /// its bytes.
    ///
    /// Returns `Ok(Some(_))` with the record's line-feed count once every field
    /// has been accounted for and `limits` is confirmed satisfied, `Ok(None)`
    /// the moment the record
    /// holds something this scan cannot confidently interpret (see
    /// [`FastValidator`]'s own documentation), and `Err` for a record that
    /// unambiguously violates `limits` or disappears as a blank line.
    fn validate(&self, record: &[u8], limits: Limits) -> Result<Option<u64>, Error> {
        let terminator_len = if self.crlf { 2 } else { 1 };
        let Some(content_len) = record.len().checked_sub(terminator_len) else {
            return Ok(None);
        };
        let (content, terminator) = record.split_at(content_len);
        let terminator_matches = if self.crlf {
            terminator == b"\r\n"
        } else {
            terminator == [self.terminator]
        };
        if !terminator_matches {
            return Ok(None);
        }
        let mut newlines = u64::from(self.crlf || self.terminator == b'\n');

        // A blank-skipping format never turns this record into one the
        // caller can address: a fresh parser would skip straight past it
        // looking for the next one, exactly as `RecordValidator` already
        // reports for it below. This is checked before the record-size limit
        // because a parser's own blank-line skip runs unconditionally,
        // before any limit is ever consulted.
        if content_len == 0 {
            return if self.blank_skip {
                Err(Error::detailed(ErrorKind::Encode, ENCODED_VALUE_NO_RECORD))
            } else if limits.max_fields == 0 {
                // `BlankRecords::Preserve` reads a zero-content record back as
                // one empty field, so a caller who allows no fields at all
                // cannot accept even this one.
                Err(Error::new(
                    ErrorKind::TooManyFields { limit: 0 },
                    Location::UNKNOWN,
                ))
            } else {
                Ok(Some(newlines))
            };
        }

        if record.len() > limits.max_record_bytes {
            return Err(Error::new(
                ErrorKind::RecordTooLarge {
                    limit: limits.max_record_bytes,
                },
                Location::UNKNOWN,
            ));
        }

        let mut field_count: usize = 0;
        let mut pos = 0;
        loop {
            if field_count == limits.max_fields {
                return Err(Error::new(
                    ErrorKind::TooManyFields {
                        limit: limits.max_fields,
                    },
                    Location::UNKNOWN,
                ));
            }
            field_count += 1;
            let Some((field_bytes, end, field_newlines)) = self.scan_field(content, pos) else {
                return Ok(None);
            };
            newlines += field_newlines;
            if field_bytes > limits.max_field_bytes {
                return Err(Error::new(
                    ErrorKind::FieldTooLarge {
                        limit: limits.max_field_bytes,
                    },
                    Location::UNKNOWN,
                ));
            }
            match end {
                FieldEnd::Record => return Ok(Some(newlines)),
                FieldEnd::Delimiter(at) => pos = at + 1,
            }
        }
    }

    /// Scan one field starting at `pos` in `content`, returning its raw byte
    /// span for [`Limits::max_field_bytes`] and where it ended.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: forcing this into `validate`'s loop, alongside `scan_unquoted_field`, saved 1.5%-4.2% across `benches/index.rs`'s `generate`, `generate_narrow`, `generate_wide` and `generate_long` cases; the same hint on `scan_quoted_field`, `scan_escaped_field` or `validate` itself measured worse, so it is not applied there"
    )]
    fn scan_field(&self, content: &[u8], pos: usize) -> Option<(usize, FieldEnd, u64)> {
        match self.mode {
            FastMode::Framed(escape) if content.get(pos) == Some(&self.quote) => {
                self.scan_quoted_field(content, pos, escape)
            }
            FastMode::Framed(_) => self.scan_unquoted_field(content, pos),
            FastMode::Escaped(escape) => self.scan_escaped_field(content, pos, escape),
        }
    }

    /// Scan a field opening with the quote byte at `pos`, to its closing
    /// quote.
    ///
    /// The field's raw byte span for [`Limits::max_field_bytes`] is the
    /// content strictly between the two quotes — escape or doubling bytes
    /// counted, the quotes themselves excluded — matching
    /// `content_start = self.location + 1` in
    /// `borrowed_parser.rs::parse_quoted_field`.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn scan_quoted_field(
        &self,
        content: &[u8],
        pos: usize,
        escape: FrameEscape,
    ) -> Option<(usize, FieldEnd, u64)> {
        let content_start = pos + 1;
        let mut cursor = content_start;
        let close = loop {
            match escape {
                FrameEscape::Doubled => {
                    let at = cursor + find1_near(self.quote, &content[cursor..])?;
                    if content.get(at + 1) == Some(&self.quote) {
                        let [_, _, tail @ ..] = &content[at..] else {
                            unreachable!("a confirmed doubled quote has two bytes")
                        };
                        // gamma::skip(stmt.delete_assign, arith.sub_to_div, reason = "without advancing beyond the doubled quote, the scanner repeatedly finds the same escape pair")
                        cursor = content.len() - tail.len();
                    } else {
                        break at;
                    }
                }
                FrameEscape::Backslash(escape) => {
                    let at = cursor + find2_near(self.quote, escape, &content[cursor..])?;
                    if content[at] == escape {
                        // The escape always consumes the byte after it,
                        // whatever it is: `write_quoted`'s `Backslash` arm
                        // only ever escapes a literal quote or a literal
                        // escape byte, so a real parser reading this back
                        // always treats the pair as one escaped byte. A
                        // dangling escape with nothing after it is not
                        // something this scan resolves on its own.
                        let [_, _, tail @ ..] = &content[at..] else {
                            return None;
                        };
                        // gamma::skip(stmt.delete_assign, arith.sub_to_div, reason = "without advancing beyond the backslash pair, the scanner repeatedly finds the same escape pair")
                        cursor = content.len() - tail.len();
                    } else {
                        break at;
                    }
                }
            }
        };
        let (before_close, _) = content.split_at(close);
        let (_, field) = before_close.split_at(content_start);
        debug_assert_eq!(before_close.len(), close);
        debug_assert_eq!(field.len(), close - content_start);
        let newlines = count_newlines(field);
        let field_bytes = close - content_start;
        let after = close + 1;
        if after == content.len() {
            Some((field_bytes, FieldEnd::Record, newlines))
        } else if content.get(after) == Some(&self.delimiter) {
            Some((field_bytes, FieldEnd::Delimiter(after), newlines))
        } else {
            // Nothing the emitter writes follows a closing quote other than a
            // delimiter or the record's end.
            None
        }
    }

    /// Scan a field not opening with the quote byte, to the next delimiter.
    ///
    /// By construction of `needs_quotes` this field holds none of the
    /// delimiter, the quote, the record ending byte, or (in a newline-based
    /// dialect) a bare `\r`, so the first of those found is always this
    /// field's true end. Finding the quote, the terminator byte, or a bare
    /// `\r` here instead means that guarantee was somehow violated, which
    /// this scan defers rather than guesses past.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "see `scan_field`, whose doc comment cites the measurement covering this function too"
    )]
    fn scan_unquoted_field(&self, content: &[u8], pos: usize) -> Option<(usize, FieldEnd, u64)> {
        let remaining = &content[pos..];
        let hit = if self.newline {
            find4_near(
                self.delimiter,
                self.quote,
                self.terminator,
                b'\r',
                remaining,
            )
        } else {
            find3_near(self.delimiter, self.quote, self.terminator, remaining)
        };
        let Some(rel) = hit else {
            let newlines = count_newlines(remaining);
            return Some((remaining.len(), FieldEnd::Record, newlines));
        };
        (content[pos + rel] == self.delimiter).then(|| {
            let newlines = count_newlines(remaining.split_at(rel).0);
            (rel, FieldEnd::Delimiter(pos + rel), newlines)
        })
    }

    /// Scan an escaped-unquoted field (`Escape::Mysql` or
    /// `Escape::Unquoted`), to the next unescaped delimiter.
    ///
    /// Every structural byte `write_unquoted_escaped_field` writes —
    /// including the escape byte itself, the quote (so a later parser never
    /// mistakes it for opening one) and the terminator byte — is escaped
    /// individually, so a bare occurrence of any of them here means that
    /// guarantee was somehow violated. A bare `\r` is not included: nothing
    /// this mode writes treats it as structural on its own (only `MySQL`'s
    /// letter-coded escapes ever touch it, and only when it is already
    /// preceded by the escape byte), and the terminator is always anchored on
    /// its lead byte regardless.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn scan_escaped_field(
        &self,
        content: &[u8],
        pos: usize,
        escape: u8,
    ) -> Option<(usize, FieldEnd, u64)> {
        let mut cursor = pos;
        loop {
            let remaining = &content[cursor..];
            let Some(rel) = find4_near(
                self.delimiter,
                self.terminator,
                self.quote,
                escape,
                remaining,
            ) else {
                return Some((
                    content.len() - pos,
                    FieldEnd::Record,
                    count_newlines(&content[pos..]),
                ));
            };
            let at = cursor + rel;
            if content[at] == escape {
                let [_, _, tail @ ..] = &content[at..] else {
                    return None;
                };
                // gamma::skip(stmt.delete_assign, assign_value.default, arith.sub_to_div, reason = "without advancing beyond the escaped pair, the scanner repeatedly finds the same escape byte")
                cursor = content.len() - tail.len();
                continue;
            }
            return (content[at] == self.delimiter).then(|| {
                (at - pos, FieldEnd::Delimiter(at), {
                    let (before_delimiter, _) = content.split_at(at);
                    debug_assert_eq!(before_delimiter.len(), at);
                    let (_, field) = before_delimiter.split_at(pos);
                    count_newlines(field)
                })
            });
        }
    }
}

/// Append one record's index entry and advance the running line number.
///
/// `record` is the encoded record including its terminator, so counting the
/// line feeds it contains is what keeps physical line numbers correct for
/// fields carrying embedded newlines.
#[cfg_attr(coverage_nightly, coverage(off))]
fn record_entry<X: Write>(
    entries: &mut X,
    entries_hasher: &mut Xxh3,
    offset: u64,
    newlines: &mut u64,
    record_newlines: u64,
    count: u64,
) -> Result<u64, Error> {
    let encoded = offset.to_le_bytes();
    entries_hasher.update(&encoded);
    entries.write_all(&encoded).map_err(Error::io_at_start)?;
    let encoded = (*newlines + 1).to_le_bytes();
    entries_hasher.update(&encoded);
    entries.write_all(&encoded).map_err(Error::io_at_start)?;
    *newlines += record_newlines;
    count
        .checked_add(1)
        .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, TOO_MANY_RECORDS))
}

/// Count line feeds in one encoded record.
#[expect(
    clippy::naive_bytecount,
    reason = "records are short and a byte-count dependency is not worth taking"
)]
fn count_newlines(record: &[u8]) -> u64 {
    record.iter().filter(|&&byte| byte == b'\n').count() as u64
}

/// A writer that hashes and measures everything passing through it.
///
/// The index binds itself to the exact bytes of its source, so generation has
/// to produce the same identity the reparsing path would have computed.
struct HashingWriter<W> {
    inner: W,
    hasher: Xxh3,
    len: u64,
}

impl<W> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Xxh3::new(),
            len: 0,
        }
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        let accepted = buf
            .get(..written)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, WRITE_OVERRAN_BUFFER))?;
        self.hasher.update(accepted);
        self.len = self
            .len
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, OUTPUT_LENGTH_OVERFLOW))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Dialect, Recovery, Syntax};

    #[derive(Clone, Copy)]
    struct SimpleRow {
        a: &'static str,
        b: u32,
    }

    impl CsvEncode for SimpleRow {
        fn csv_encode<V: crate::encoding::EncodeVisitor>(
            &self,
            visitor: &mut V,
        ) -> Result<(), Error> {
            visitor.visit_field(0, "a", self.a.as_bytes())?;
            visitor.visit_field(1, "b", self.b.to_string().as_bytes())?;
            Ok(())
        }
        fn field_names() -> &'static [&'static str] {
            &["a", "b"]
        }
    }

    #[test]
    fn generation_buffer_boundaries_and_path_truncation_are_exact() {
        assert!(!should_flush_buffer(7, 8));
        assert!(should_flush_buffer(8, 8));
        assert!(should_flush_buffer(9, 8));

        let directory = tempfile::tempdir().expect("temporary directory");
        let source_path = directory.path().join("generated.csv");
        let index_path = directory.path().join("generated.idx");
        std::fs::write(&index_path, vec![0xAA; 4096]).expect("preexisting long index");
        let reader = CsvIndex::generate_path(
            &source_path,
            &index_path,
            [SimpleRow { a: "x", b: 1 }],
            IndexOptions::default(),
            EmitOptions::new(),
        )
        .expect("generation truncates the previous index");
        assert_eq!(reader.len(), 2);
        assert_eq!(
            std::fs::metadata(&index_path)
                .expect("index metadata")
                .len(),
            (FIXED_HEADER_BYTES + 2 * 16 + 2 * CHECKSUM_BYTES) as u64
        );
    }

    #[test]
    fn generation_flushes_at_both_sides_of_the_exact_capacity_boundary() {
        #[derive(Default)]
        struct WriteTrace {
            writes: Vec<usize>,
        }

        impl Write for WriteTrace {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.writes.push(buf.len());
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        fn writes_at(capacity: usize) -> Vec<usize> {
            let mut trace = WriteTrace::default();
            CsvIndex::generate(
                &mut trace,
                std::io::Cursor::new(Vec::new()),
                [SimpleRow { a: "test", b: 1 }, SimpleRow { a: "test", b: 2 }],
                IndexOptions::default(),
                EmitOptions::new()
                    .has_headers(false)
                    .buffer_capacity(capacity),
            )
            .expect("generation");
            trace.writes
        }

        assert_eq!(writes_at(7), [7, 7]);
        assert_eq!(writes_at(8), [14]);
    }

    #[test]
    fn record_validator_finishes_each_independent_record() {
        let mut validator =
            RecordValidator::new(FormatOptions::CSV, Limits::DEFAULT).expect("validator");
        validator.validate(b"a,b\n").expect("first record");
        validator.validate(b"c,d\n").expect("state resets");
        validator
            .validate(b"unterminated,final")
            .expect("finish settles a final record without a terminator");

        let mut validator =
            RecordValidator::new(FormatOptions::CSV, Limits::DEFAULT).expect("validator");
        let error = validator
            .validate(b"")
            .expect_err("empty input produces no record");
        assert!(
            error
                .to_string()
                .contains("encoded value does not produce a parser-visible record"),
            "{error}"
        );

        let mut validator =
            RecordValidator::new(FormatOptions::CSV, Limits::DEFAULT).expect("validator");
        let error = validator
            .validate(b"a,b\nc,d\n")
            .expect_err("two records are not one encoded value");
        assert!(
            error
                .to_string()
                .contains("encoded value produces more than one parser-visible record"),
            "{error}"
        );
    }

    #[test]
    fn fast_validator_boundaries_and_scanners_are_exact() {
        let csv = FastValidator::for_format(FormatOptions::CSV).expect("CSV fast validator");
        assert_eq!(csv.delimiter, b',');
        assert_eq!(csv.quote, b'"');
        assert_eq!(csv.terminator, b'\n');
        assert!(!csv.crlf);
        assert!(csv.newline);
        assert!(!csv.blank_skip);
        assert!(matches!(csv.mode, FastMode::Framed(FrameEscape::Doubled)));

        assert_eq!(
            csv.validate(b"a,b\n", Limits::new(4, 1, 2))
                .expect("limits are inclusive"),
            Some(1)
        );
        assert_eq!(
            csv.validate(b"ab,c\n", Limits::new(5, 2, 2))
                .expect("field width is inclusive"),
            Some(1)
        );
        assert_eq!(
            csv.validate(b"a,zz\n", Limits::new(5, 1, 2))
                .expect_err("the second field exceeds the exact limit")
                .kind(),
            ErrorKind::FieldTooLarge { limit: 1 }
        );
        let error = csv
            .validate(b"\n", Limits::new(1, 1, 0))
            .expect_err("a preserved blank record still has one field");
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });

        assert_eq!(
            csv.scan_quoted_field(b"\"a\"\"b\nc\",z", 0, FrameEscape::Doubled),
            Some((6, FieldEnd::Delimiter(8), 1))
        );
        assert_eq!(
            csv.scan_quoted_field(b"\"\"\"\"", 0, FrameEscape::Doubled),
            Some((2, FieldEnd::Record, 0))
        );
        assert_eq!(
            csv.scan_quoted_field(b"\"\n\"", 0, FrameEscape::Doubled),
            Some((1, FieldEnd::Record, 1))
        );
        assert_eq!(
            csv.scan_quoted_field(b"xx\"ab\"", 2, FrameEscape::Doubled),
            Some((2, FieldEnd::Record, 0))
        );
        assert_eq!(
            csv.scan_field(b"xx\"ab\"", 2),
            Some((2, FieldEnd::Record, 0))
        );
        assert_eq!(
            csv.scan_quoted_field(b"\"ab\"x", 0, FrameEscape::Doubled),
            None
        );

        let backslash_format = FormatOptions::from_dialect(
            Dialect::new(b',', b'"', RecordEnding::Newline, Escape::Backslash(b'\\'))
                .expect("backslash dialect"),
        );
        let backslash =
            FastValidator::for_format(backslash_format).expect("backslash fast validator");
        assert_eq!(
            backslash.scan_quoted_field(b"\"a\\\"\n\\\\b\",z", 0, FrameEscape::Backslash(b'\\')),
            Some((7, FieldEnd::Delimiter(9), 1))
        );
        assert_eq!(
            backslash.scan_quoted_field(b"\"\\\"\"", 0, FrameEscape::Backslash(b'\\')),
            Some((2, FieldEnd::Record, 0))
        );

        assert_eq!(
            csv.scan_unquoted_field(b"ab,cd", 0),
            Some((2, FieldEnd::Delimiter(2), 0))
        );
        assert_eq!(
            csv.scan_field(b"xxab,cd", 2),
            Some((2, FieldEnd::Delimiter(4), 0))
        );
        assert_eq!(csv.scan_unquoted_field(b"a\"b", 0), None);
        assert_eq!(csv.scan_unquoted_field(b"a\nb", 0), None);
        assert_eq!(csv.scan_unquoted_field(b"ab\rcd", 0), None);

        let byte_ending = FormatOptions::from_dialect(
            Dialect::new(b',', b'"', RecordEnding::Byte(b';'), Escape::DoubleQuote)
                .expect("byte-ending dialect"),
        );
        let byte_ending =
            FastValidator::for_format(byte_ending).expect("byte-ending fast validator");
        assert!(!byte_ending.newline);
        assert_eq!(
            byte_ending.scan_unquoted_field(b"a\n,b", 0),
            Some((2, FieldEnd::Delimiter(2), 1))
        );
        assert_eq!(
            byte_ending.scan_unquoted_field(b"xxa\nb", 2),
            Some((3, FieldEnd::Record, 1))
        );
        assert_eq!(
            byte_ending.scan_unquoted_field(b"a\rb", 0),
            Some((3, FieldEnd::Record, 0))
        );
        assert_eq!(byte_ending.scan_unquoted_field(b"a\"b", 0), None);
        assert_eq!(byte_ending.scan_unquoted_field(b"a;b", 0), None);

        let escaped_format = FormatOptions::from_dialect(
            Dialect::new(
                b',',
                b'"',
                RecordEnding::Byte(b';'),
                Escape::Unquoted(b'\\'),
            )
            .expect("escaped dialect"),
        )
        .quoting(Quoting::Never);
        let escaped = FastValidator::for_format(escaped_format).expect("escaped fast validator");
        assert_eq!(
            escaped.scan_escaped_field(b"a\\\n\\,b,c", 0, b'\\'),
            Some((6, FieldEnd::Delimiter(6), 1))
        );
        assert_eq!(
            escaped.scan_escaped_field(b"xxa\nb", 2, b'\\'),
            Some((3, FieldEnd::Record, 1))
        );
        assert_eq!(
            escaped.scan_escaped_field(b"xx\nb", 2, b'\\'),
            Some((2, FieldEnd::Record, 1))
        );
        assert_eq!(
            escaped.scan_escaped_field(b"xx\n\\\n,", 2, b'\\'),
            Some((3, FieldEnd::Delimiter(5), 2))
        );
        assert_eq!(
            escaped.scan_field(b"xxa\\\n\\,b,c", 2),
            Some((6, FieldEnd::Delimiter(8), 1))
        );
        assert_eq!(escaped.scan_escaped_field(b"a\\", 0, b'\\'), None);
        assert_eq!(escaped.scan_escaped_field(b"a\"b", 0, b'\\'), None);
        assert_eq!(escaped.scan_escaped_field(b"a;b", 0, b'\\'), None);
    }

    #[test]
    fn record_entry_and_hashing_writer_report_exact_bytes_and_errors() {
        let mut entries = Vec::new();
        let mut hasher = Xxh3::new();
        let mut newlines = 2;
        let count = record_entry(&mut entries, &mut hasher, 5, &mut newlines, 3, 7).expect("entry");
        let mut expected = Vec::new();
        expected.extend_from_slice(&5_u64.to_le_bytes());
        expected.extend_from_slice(&3_u64.to_le_bytes());
        assert_eq!(entries, expected);
        assert_eq!(hasher.digest128().to_le_bytes(), hash_bytes(&expected));
        assert_eq!(newlines, 5);
        assert_eq!(count, 8);

        struct Liar;
        impl Write for Liar {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len() + 1)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let error = HashingWriter::new(Liar)
            .write(b"abc")
            .expect_err("writer over-reporting is invalid");
        assert_eq!(
            error.to_string(),
            "Write implementation reported more bytes than the buffer holds"
        );

        let mut writer = HashingWriter::new(Vec::new());
        writer.len = u64::MAX;
        let error = writer
            .write(b"x")
            .expect_err("output length cannot exceed u64");
        assert_eq!(error.to_string(), "output length exceeds u64");
    }

    #[test]
    fn test_fast_validator_for_format() {
        let raw = FormatOptions::CSV.quoting(Quoting::Raw);
        assert!(FastValidator::for_format(raw).is_none());

        let skip_space = FormatOptions::CSV.skip_initial_space(true);
        assert!(FastValidator::for_format(skip_space).is_none());

        let no_quote_syntax = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE));
        assert!(FastValidator::for_format(no_quote_syntax).is_none());

        let backslash = FormatOptions::from_dialect(
            Dialect::new(b',', b'"', RecordEnding::Newline, Escape::Backslash(b'\\')).unwrap(),
        );
        let val = FastValidator::for_format(backslash).expect("backslash validator");
        assert!(matches!(
            val.mode,
            FastMode::Framed(FrameEscape::Backslash(b'\\'))
        ));

        let mysql = FormatOptions::MYSQL;
        let val_mysql = FastValidator::for_format(mysql).expect("mysql validator");
        assert!(matches!(val_mysql.mode, FastMode::Escaped(b'\\')));
    }

    #[test]
    fn test_fast_validator_validate_branches() {
        let fmt = FormatOptions::CSV;
        let val = FastValidator::for_format(fmt).unwrap();

        // Mismatched terminator
        assert_eq!(val.validate(b"foo,bar", Limits::DEFAULT).unwrap(), None);

        // CRLF terminator
        let val_crlf = FastValidator::for_format(FormatOptions::RFC4180).unwrap();
        assert!(
            val_crlf
                .validate(b"a,b\r\n", Limits::DEFAULT)
                .unwrap()
                .is_some()
        );
        assert_eq!(val_crlf.validate(b"a,b\n", Limits::DEFAULT).unwrap(), None);

        // Blank line preserve vs skip
        let blank_skip =
            FastValidator::for_format(FormatOptions::CSV.blank_records(BlankRecords::Skip))
                .unwrap();
        assert!(blank_skip.validate(b"\n", Limits::DEFAULT).is_err());

        let blank_zero_fields = val.validate(b"\n", Limits::new(100, 100, 0));
        assert!(blank_zero_fields.is_err());

        let blank_valid = val.validate(b"\n", Limits::DEFAULT).unwrap();
        assert_eq!(blank_valid, Some(1));

        // Record too large
        assert!(
            val.validate(b"hello,world\n", Limits::new(4, 100, 10))
                .is_err()
        );

        // Field count limit
        assert!(val.validate(b"a,b,c\n", Limits::new(100, 100, 2)).is_err());

        // Field byte limit
        assert!(
            val.validate(b"toolong,b\n", Limits::new(100, 3, 10))
                .is_err()
        );

        // Quoted field with doubled quote and newlines
        let res = val
            .validate(b"\"line1\nline2\"\"continued\",val\n", Limits::DEFAULT)
            .unwrap();
        assert_eq!(res, Some(2));

        // Quoted field with closing quote followed by non-delimiter
        assert_eq!(
            val.validate(b"\"hello\"world,val\n", Limits::DEFAULT)
                .unwrap(),
            None
        );

        // Backslash quoted escape mode
        let backslash = FormatOptions::from_dialect(
            Dialect::new(b',', b'"', RecordEnding::Newline, Escape::Backslash(b'\\')).unwrap(),
        );
        let val_b = FastValidator::for_format(backslash).unwrap();
        assert!(
            val_b
                .validate(b"\"a\\\"b\",c\n", Limits::DEFAULT)
                .unwrap()
                .is_some()
        );
        assert!(
            val_b
                .validate(b"\"a\\\nb\"\n", Limits::DEFAULT)
                .unwrap()
                .is_some()
        );
        assert_eq!(val_b.validate(b"\"a\\", Limits::DEFAULT).unwrap(), None);
        assert_eq!(val_b.validate(b"\"a\\\n", Limits::DEFAULT).unwrap(), None);

        // Non-newline ending
        let custom_ending = FormatOptions::from_dialect(
            Dialect::new(b',', b'"', RecordEnding::Byte(b';'), Escape::DoubleQuote).unwrap(),
        );
        let custom_val = FastValidator::for_format(custom_ending).unwrap();
        assert!(custom_val.validate(b"a,b;", Limits::DEFAULT).is_ok());

        // Escaped format validations
        let val_mysql = FastValidator::for_format(FormatOptions::MYSQL).unwrap();
        // MySQL uses \n as record ending
        let valid_mysql = val_mysql
            .validate(b"hello\\,world,second\n", Limits::DEFAULT)
            .unwrap();
        assert!(valid_mysql.is_some());

        // Dangling escape in escaped mode
        assert_eq!(
            val_mysql.validate(b"hello\\", Limits::DEFAULT).unwrap(),
            None
        );
        assert_eq!(
            val_mysql.validate(b"hello\\\n", Limits::DEFAULT).unwrap(),
            None
        );
        // Bare quote in escaped mode returns None
        assert_eq!(
            val_mysql
                .validate(b"hello\"world\n", Limits::DEFAULT)
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_record_validator() {
        let invalid_fmt = FormatOptions::CSV.delimiter(b'"');
        assert!(RecordValidator::new(invalid_fmt, Limits::DEFAULT).is_err());

        let mut rv = RecordValidator::new(FormatOptions::CSV, Limits::DEFAULT).unwrap();
        assert!(rv.validate(b"a,b\n").is_ok());
        // Empty produce no record error
        assert!(rv.validate(b"").is_err());
        // Multiple records error
        assert!(rv.validate(b"a,b\nc,d\n").is_err());

        // validate_record fallback
        let raw = FormatOptions::CSV.quoting(Quoting::Raw);
        let mut rv_raw = RecordValidator::new(raw, Limits::DEFAULT).unwrap();
        assert_eq!(
            super::validate_record(None, &mut rv_raw, b"a,b\n", Limits::DEFAULT).unwrap(),
            1
        );

        // FastValidator validate with record shorter than terminator
        let val = FastValidator::for_format(FormatOptions::CSV).unwrap();
        assert_eq!(val.validate(b"", Limits::DEFAULT).unwrap(), None);
    }

    #[test]
    fn test_generate_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source_path = directory.path().join("generated.csv");
        let index_path = directory.path().join("generated.idx");

        let rows = [
            SimpleRow { a: "boston", b: 10 },
            SimpleRow { a: "austin", b: 20 },
            SimpleRow { a: "denver", b: 30 },
        ];
        let reader = CsvIndex::generate_path(
            &source_path,
            &index_path,
            rows,
            IndexOptions {
                format: FormatOptions::CSV.write_bom(WriteBom::Emit),
                limits: Limits::DEFAULT,
            },
            EmitOptions::new().buffer_capacity(16).has_headers(true),
        )
        .expect("generate_path succeeds");

        assert_eq!(reader.len(), 4);
    }

    #[test]
    fn test_generate_errors() {
        let rows = [SimpleRow { a: "boston", b: 10 }];
        let invalid_opts = IndexOptions {
            format: FormatOptions::CSV.delimiter(b'"'),
            limits: Limits::DEFAULT,
        };
        assert!(
            CsvIndex::generate(
                Vec::new(),
                std::io::Cursor::new(Vec::new()),
                rows,
                invalid_opts,
                EmitOptions::default()
            )
            .is_err()
        );

        let non_existent_source = std::path::PathBuf::from("/non_existent_dir_12345/source.csv");
        let non_existent_idx = std::path::PathBuf::from("/non_existent_dir_12345/index.idx");
        assert!(
            CsvIndex::generate_path(
                &non_existent_source,
                &non_existent_idx,
                rows,
                IndexOptions::default(),
                EmitOptions::default(),
            )
            .is_err()
        );

        struct FailingReaderWriter;
        impl std::io::Read for FailingReaderWriter {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for FailingReaderWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
        }
        impl std::io::Seek for FailingReaderWriter {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                Ok(0)
            }
        }
        let rows = [SimpleRow { a: "b", b: 1 }];
        let index = std::io::Cursor::new(Vec::new());
        assert!(
            CsvIndex::generate(
                FailingReaderWriter,
                index,
                rows,
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .is_err()
        );

        // Header emission with failing index write covers record_entry error on line 121
        assert!(
            CsvIndex::generate(
                Vec::new(),
                FailingReaderWriter,
                rows,
                IndexOptions::default(),
                EmitOptions::new().buffer_capacity(16).has_headers(true),
            )
            .is_err()
        );

        // Failing sink with BOM
        assert!(
            CsvIndex::generate(
                FailingReaderWriter,
                std::io::Cursor::new(Vec::new()),
                rows,
                IndexOptions {
                    format: FormatOptions::CSV.write_bom(WriteBom::Emit),
                    limits: Limits::DEFAULT,
                },
                EmitOptions::new(),
            )
            .is_err()
        );

        // Encoding error in generate
        struct ErrorRow;
        impl CsvEncode for ErrorRow {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                _visitor: &mut V,
            ) -> Result<(), Error> {
                Err(Error::detailed(ErrorKind::Encode, "custom error"))
            }
            fn field_names() -> &'static [&'static str] {
                &["a"]
            }
        }
        assert!(
            CsvIndex::generate(
                Vec::new(),
                std::io::Cursor::new(Vec::new()),
                [ErrorRow],
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .is_err()
        );

        // Large rows to trigger core.len() >= threshold flushing
        let many_rows: Vec<_> = (0..50)
            .map(|i| SimpleRow {
                a: "testing_threshold_flushing",
                b: i,
            })
            .collect();
        assert!(
            CsvIndex::generate(
                Vec::new(),
                std::io::Cursor::new(Vec::new()),
                many_rows,
                IndexOptions::default(),
                EmitOptions::new().buffer_capacity(32),
            )
            .is_ok()
        );

        // Test record_entry with count == u64::MAX
        let mut entry_buf = Vec::new();
        let mut h = Xxh3::new();
        let mut nl = 0;
        assert!(record_entry(&mut entry_buf, &mut h, 0, &mut nl, 1, u64::MAX).is_err());

        // Test sink failure on threshold flush and final flush
        struct CountingWriter {
            writes_until_fail: usize,
        }
        impl Write for CountingWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.writes_until_fail == 0 {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    self.writes_until_fail -= 1;
                    Ok(buf.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                if self.writes_until_fail == 0 {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    Ok(())
                }
            }
        }
        impl Read for CountingWriter {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Seek for CountingWriter {
            fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
                Ok(0)
            }
        }

        // Sink write threshold failure
        let rows = [
            SimpleRow {
                a: "test_flush",
                b: 1,
            },
            SimpleRow {
                a: "test_flush_2",
                b: 2,
            },
        ];
        assert!(
            CsvIndex::generate(
                CountingWriter {
                    writes_until_fail: 0
                },
                std::io::Cursor::new(Vec::new()),
                rows,
                IndexOptions::default(),
                EmitOptions::new().buffer_capacity(8),
            )
            .is_err()
        );

        // Sink flush failure
        struct FlushFailWriter;
        impl Write for FlushFailWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
        }
        assert!(
            CsvIndex::generate(
                FlushFailWriter,
                std::io::Cursor::new(Vec::new()),
                rows,
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .is_err()
        );

        // Index write header failure (e.g. seek fails on header write)
        struct SeekFailIndex {
            seeks: usize,
        }
        impl Read for SeekFailIndex {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Ok(0)
            }
        }
        impl Write for SeekFailIndex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl Seek for SeekFailIndex {
            fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
                if self.seeks == 0 {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    self.seeks -= 1;
                    Ok(0)
                }
            }
        }
        assert!(
            CsvIndex::generate(
                Vec::new(),
                SeekFailIndex { seeks: 1 },
                rows,
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .is_err()
        );
        assert!(
            CsvIndex::generate(
                Vec::new(),
                SeekFailIndex { seeks: 2 },
                rows,
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn late_index_finalization_preserves_each_io_error() {
        #[derive(Clone, Copy, Debug)]
        enum FinalizationFailure {
            EntriesChecksum(io::ErrorKind),
            HeaderChecksum(io::ErrorKind),
            Flush(io::ErrorKind),
        }

        #[derive(Debug)]
        struct StageFailIndex {
            inner: io::Cursor<Vec<u8>>,
            failure: FinalizationFailure,
            writing_checksums: bool,
            checksum_writes: usize,
        }

        impl StageFailIndex {
            fn new(failure: FinalizationFailure) -> Self {
                Self {
                    inner: io::Cursor::new(Vec::new()),
                    failure,
                    writing_checksums: false,
                    checksum_writes: 0,
                }
            }
        }

        impl Read for StageFailIndex {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.inner.read(buf)
            }
        }

        impl Write for StageFailIndex {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if self.writing_checksums {
                    self.checksum_writes += 1;
                    match (self.failure, self.checksum_writes) {
                        (FinalizationFailure::EntriesChecksum(kind), 1)
                        | (FinalizationFailure::HeaderChecksum(kind), 2) => {
                            return Err(io::Error::from(kind));
                        }
                        _ => {}
                    }
                }
                self.inner.write(buf)
            }

            fn flush(&mut self) -> io::Result<()> {
                match (self.failure, self.writing_checksums) {
                    (FinalizationFailure::Flush(kind), true) => Err(io::Error::from(kind)),
                    _ => Ok(()),
                }
            }
        }

        impl Seek for StageFailIndex {
            fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
                let begins_checksums = matches!(position, SeekFrom::End(0));
                let offset = self.inner.seek(position)?;
                self.writing_checksums |= begins_checksums;
                Ok(offset)
            }
        }

        let rows = [SimpleRow { a: "row", b: 1 }];
        for failure in [
            FinalizationFailure::EntriesChecksum(io::ErrorKind::BrokenPipe),
            FinalizationFailure::HeaderChecksum(io::ErrorKind::PermissionDenied),
        ] {
            let kind = match failure {
                FinalizationFailure::EntriesChecksum(kind)
                | FinalizationFailure::HeaderChecksum(kind)
                | FinalizationFailure::Flush(kind) => kind,
            };
            let error = CsvIndex::generate(
                Vec::new(),
                StageFailIndex::new(failure),
                rows,
                IndexOptions::default(),
                EmitOptions::new(),
            )
            .expect_err("the selected checksum write must fail");
            assert_eq!(error.kind(), ErrorKind::Io(kind));
        }

        let kind = io::ErrorKind::Interrupted;
        let error = CsvIndex::generate(
            Vec::new(),
            StageFailIndex::new(FinalizationFailure::Flush(kind)),
            rows,
            IndexOptions::default(),
            EmitOptions::new(),
        )
        .expect_err("the final index flush must fail");
        assert_eq!(error.kind(), ErrorKind::Io(kind));
    }

    #[test]
    fn test_hashing_writer_overflow() {
        let mut writer = HashingWriter::new(Vec::new());
        writer.len = u64::MAX - 2;
        let buf = [0u8; 8];
        assert!(writer.write(&buf).is_err());
        assert!(writer.flush().is_ok());

        // CsvIndex::generate_path and CsvIndex::generate with headers
        let directory = tempfile::tempdir().unwrap();
        let src_p = directory.path().join("generated.csv");
        let idx_p = directory.path().join("generated.idx");
        let rows = [
            SimpleRow { a: "boston", b: 1 },
            SimpleRow { a: "austin", b: 2 },
        ];
        CsvIndex::generate_path(
            &src_p,
            &idx_p,
            rows,
            IndexOptions::default(),
            EmitOptions::new().has_headers(true).buffer_capacity(16),
        )
        .unwrap();
        let loaded = CsvIndex::load(&idx_p).unwrap();
        assert_eq!(loaded.len(), 3);

        // CsvIndex::generate with BOM and headers
        let mut out_writer = Vec::new();
        let mut idx_writer = std::io::Cursor::new(Vec::new());
        let reader = CsvIndex::generate(
            &mut out_writer,
            &mut idx_writer,
            rows,
            IndexOptions {
                format: FormatOptions::CSV.write_bom(WriteBom::Emit),
                ..IndexOptions::default()
            },
            EmitOptions::new().has_headers(true),
        )
        .unwrap();
        assert_eq!(reader.len(), 3);

        // CsvIndex::generate with TSV format (RecordValidator)
        let tsv_rows = [
            SimpleRow { a: "boston", b: 1 },
            SimpleRow { a: "austin", b: 2 },
        ];
        let tsv_res = CsvIndex::generate(
            Vec::new(),
            std::io::Cursor::new(Vec::new()),
            tsv_rows,
            IndexOptions {
                format: FormatOptions::TSV,
                ..IndexOptions::default()
            },
            EmitOptions::new(),
        );
        assert!(tsv_res.is_ok());

        // CsvIndex::generate with Backslash escape and quoted strings
        let esc_rows = [
            SimpleRow {
                a: "hello \"world\"",
                b: 1,
            },
            SimpleRow { a: "plain", b: 2 },
        ];
        let esc_res = CsvIndex::generate(
            Vec::new(),
            std::io::Cursor::new(Vec::new()),
            esc_rows,
            IndexOptions {
                format: FormatOptions::CSV.escape(crate::config::Escape::Backslash(b'\\')),
                ..IndexOptions::default()
            },
            EmitOptions::new(),
        );
        assert!(esc_res.is_ok());
    }
}

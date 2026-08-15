//! The owned parse path, which copies fields into engine storage.

use super::*;

/// How many records may follow a bailed one before the prediction re-tests.
///
/// The wrong prediction costs about twice what the right one saves, so this
/// trades a sixteenth of the win for a file that stops quoting after a while
/// correcting itself within sixteen records rather than never.
const INTERIOR_QUOTE_RUN: u8 = 16;
const WIDE_RECORD_FIELDS: usize = 16;

#[inline]
fn use_short_owned_fields(field_capacity: usize) -> bool {
    field_capacity >= WIDE_RECORD_FIELDS
}

#[cold]
#[inline(never)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn unreachable_owned_unquoted_escape() -> ! {
    unreachable!("unquoted-escape dialects bypass the owned-bytes fast path")
}

impl Engine {
    #[inline]
    fn owned_resume_window<'a>(&self, input: &'a [u8], record_start: usize) -> Option<&'a [u8]> {
        let window_end = cmp::min(
            input.len(),
            record_start.saturating_add(self.limits.max_record_bytes),
        );
        input
            .get(
                // #[gamma::skip(expr.decrement, reason = "moving a resumed quote window one byte before its settled field reparses prior output and makes repeated resume attempts grow storage without bound")]
                // #[gamma::skip(expr.increment, reason = "moving a resumed quote window past its opening quote prevents cursor progress and makes the caller retry the same record until timeout")]
                self.location..window_end,
            )
            .filter(|window| !window.is_empty())
    }

    #[inline]
    fn try_parse_default_csv_owned_plain<const CERTIFY_ASCII: bool>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
    ) -> Option<bool> {
        #[cfg(target_arch = "x86_64")]
        {
            if self.owned_parser.is_some()
                && let Some(consumed) = if CERTIFY_ASCII {
                    try_parse_default_plain_packed_ascii(&input[record_start..], output)
                } else {
                    try_parse_default_plain_packed(&input[record_start..], output)
                }
            {
                // gamma::skip(assign_value.default, reason = "resetting the packed-parser location to zero reports a completed record without consuming it, so callers repeatedly append the same record")
                self.location = record_start + consumed;
                return Some(true);
            }
        }
        None
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(super) fn parse_owned_record<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        self.parse_owned_record_mode::<F, false>(input, output, record_start, header)
    }

    pub(super) fn parse_owned_text_record<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        self.parse_owned_record_mode::<F, true>(input, output, record_start, header)
    }

    fn parse_owned_record_mode<F: CsvFormat, const CERTIFY_ASCII: bool>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        let record_origin = record_start;
        if self.fmt_plain_kernel::<F>() {
            // The second test is the shape prediction. It is deliberately all
            // this adds to the unquoted path -- one load and one branch -- with
            // the bookkeeping inside the branch it guards. Hoisting the quote
            // comparison into a local ahead of the guard, so that only a
            // predicted record spends from the count, measured worse: it costs
            // the unquoted corpora 1.3% rather than 0.7%, for a 1% saving on
            // the leading-quoted ones. Leave the ordering to the compiler.
            let opens_quoted = input[record_start] == self.fmt_quote::<F>();
            if self.fmt_quoting_enabled::<F>() && (opens_quoted || self.interior_quotes != 0) {
                self.interior_quotes = self.interior_quotes.saturating_sub(1);
                if CERTIFY_ASCII && !opens_quoted {
                    if self.ascii_structural_backoff != 0 {
                        self.ascii_structural_backoff -= 1;
                    } else {
                        match try_parse_default_interior_record_structural_ascii(
                            &input[record_start..],
                            output,
                        ) {
                            Some((consumed, true)) => {
                                self.ascii_structural_succeeded = true;
                                self.location = record_start + consumed;
                                return Ok(());
                            }
                            _ => {
                                self.ascii_structural_backoff = if self.ascii_structural_succeeded {
                                    1
                                } else {
                                    3
                                };
                            }
                        }
                    }
                }
                // A record that really opens with a quote has a quoted head the
                // leading-quote parser splits off inline below. A record that
                // got here on the prediction alone does not, so an out-of-line
                // The active helper either hands a single quoted run back to
                // the kernel or reads a repeatedly alternating shape whole.
                // Keeping both behind one function pointer leaves this
                // dispatch and the unquoted path's frame unchanged.
                match opens_quoted {
                    true => {
                        if let Some(parse) = self.quoted_prefix_parser {
                            match parse(&input[record_start..], output) {
                                // The record was quoted the whole way, so there was no
                                // unquoted tail to hand back.
                                Some((consumed, true)) => {
                                    // gamma::skip(assign_value.default, reason = "resetting the quoted-prefix location to zero reports success without consuming the record and causes unbounded caller retries")
                                    self.location = record_start + consumed;
                                    return Ok(());
                                }
                                // The quoted head is parsed and the tail is plain, which
                                // is what the kernel is best at. It resumes at a field
                                // boundary with the head's fields already in `output`,
                                // so it appends to them exactly as it would its own.
                                Some((consumed, false)) => {
                                    if self.try_parse_owned_plain_from::<F>(
                                        input,
                                        output,
                                        record_origin,
                                        record_start + consumed,
                                    )? {
                                        self.mark_owned_nulls::<F>(output, header);
                                        self.validate_field_count(input, output.len())?;
                                        return Ok(());
                                    }
                                    self.interior_quotes = INTERIOR_QUOTE_RUN;
                                    self.interior_prefix_parser = self.multi_quote_parser;
                                    let () = self.mark_owned_nulls::<F>(output, header);
                                    // A further interior quote in the tail goes back to
                                    // the kernel one quoted field at a time, the same
                                    // resume the predicted and armed records use.
                                    match self.resume_owned_after_interior_quote::<F>(
                                        input,
                                        output,
                                        record_origin,
                                    )? {
                                        true => return Ok(()),
                                        false => {}
                                    }
                                }
                                None => output.clear_fields(),
                            }
                        } else if let Some(parse) = self.owned_parser {
                            if let Some(consumed) = parse(&input[record_start..], output) {
                                self.location = record_start + consumed;
                                return Ok(());
                            }
                            output.clear_fields();
                        }
                    }
                    false => {
                        if self.parse_owned_interior_prefix::<F>(
                            input,
                            output,
                            record_origin,
                            header,
                        )? {
                            return Ok(());
                        }
                    }
                }
            } else {
                let parsed = if CERTIFY_ASCII {
                    self.try_parse_owned_plain_ascii::<F>(input, output, record_start)?
                } else {
                    self.try_parse_owned_plain::<F>(input, output, record_start)?
                };
                if parsed {
                    self.mark_owned_nulls::<F>(output, header);
                    self.validate_field_count(input, output.len())?;
                    return Ok(());
                }
                // This record held a quote the scan had to bail at, so predict
                // the next few have one too. No reset is needed when the scan
                // instead runs to the end: the count only ever decays, and a
                // record that reaches here is the only thing that renews it.
                self.interior_quotes = INTERIOR_QUOTE_RUN;
                self.interior_prefix_parser
                    .clone_from(&self.interior_handoff_parser);
                // Unlike the borrowed kernel, this one keeps the unquoted
                // fields it parsed before it hit the quote that made it bail,
                // so that prefix needs the same NULL pass. Anything the resume
                // alternation appends afterwards marks itself.
                self.mark_owned_nulls::<F>(output, header);
                // The tail after the quote goes back to the kernel one quoted
                // field at a time rather than being read whole scalar, the same
                // handoff the predicted records make, so a plain run inside it
                // is vectorized here too.
                match self.resume_owned_after_interior_quote::<F>(input, output, record_origin)? {
                    true => return Ok(()),
                    false => {}
                }
            }
        }
        loop {
            self.skip_delimiter_spaces::<F>(input, record_start)?;
            let record_end = if input.get(self.location) == Some(&self.fmt_quote::<F>()) {
                if self.fmt_quoting_enabled::<F>() {
                    self.parse_owned_quoted_field(input, output, record_origin)?
                } else {
                    self.parse_owned_unquoted_field(input, output, record_start, header)?
                }
            } else {
                self.parse_owned_unquoted_field(input, output, record_origin, header)?
            };
            if record_end {
                self.validate_field_count(input, output.len())?;
                return Ok(());
            }
        }
    }

    /// Parse a predicted record whose first byte is not a quote through the
    /// active parser for the observed row shape.
    ///
    /// Reached only through the predicted branch and held out of line, so the
    /// hot unquoted path that dispatches here, and the leading-quote path beside
    /// it, keep the exact register allocation and code shape they had when this
    /// record was still parsed whole scalar. The default dialect initially
    /// reads the head and one interior quoted field before returning the plain
    /// tail to the kernel. A second separated quote run switches the function
    /// pointer to a whole-record AVX2 parser for short, simply quoted rows and
    /// otherwise to the established scalar parser. Any other dialect reads the
    /// whole predicted record scalar, as it did before.
    ///
    /// Returns `Ok(true)` when the record is fully parsed. `Ok(false)` leaves
    /// `output` and `self.location` as the general loop needs them -- either to
    /// finish a record no scalar parser is configured for, or to reproduce a
    /// malformed record's exact error from where it began.
    #[inline(never)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_owned_interior_prefix<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        header: bool,
    ) -> Result<bool, Error> {
        let record_origin = record_start;
        let Some(parse) = self.interior_prefix_parser else {
            // A dialect with a whole-record scalar parser but no interior one
            // reads the predicted record whole, the scan the prediction routed
            // it here to take.
            if let Some(parse) = self.owned_parser {
                if let Some(consumed) = parse(&input[record_start..], output) {
                    self.location = record_start + consumed;
                    return Ok(true);
                }
                output.clear_fields();
            }
            return Ok(false);
        };
        // The interior parser caps its own scan at the record-length limit
        // measured from the slice start, which is `record_start`, so a deep
        // interior quote cannot buy the record extra length and no outer window
        // is needed here.
        match parse(&input[record_start..], output) {
            // The predicted record turned out plain and was read to its end; the
            // misprediction cost one scalar pass and nothing more. The eligible
            // config marks no nulls and counts flexibly, so neither pass runs.
            Some((consumed, true)) => {
                self.location = record_start + consumed;
                Ok(true)
            }
            // The head and its interior quoted field are read. The kernel takes
            // the plain tail from the field boundary they left; if that tail
            // bails at a further interior quote, the resume alternation covers
            // it, reading each quoted field scalar and the plain runs between
            // them with the kernel.
            Some((consumed, false)) => {
                self.location = record_start + consumed;
                if self.try_parse_owned_plain_from::<F>(
                    input,
                    output,
                    record_start,
                    self.location,
                )? {
                    self.mark_owned_nulls::<F>(output, header);
                    self.validate_field_count(input, output.len())?;
                    return Ok(true);
                }
                self.interior_quotes = INTERIOR_QUOTE_RUN;
                self.interior_prefix_parser = self.multi_quote_parser;
                match self.resume_owned_after_interior_quote::<F>(input, output, record_origin)? {
                    true => {
                        let () = self.mark_owned_nulls::<F>(output, header);
                        self.validate_field_count(input, output.len())?;
                        return Ok(true);
                    }
                    false => {}
                }
                self.mark_owned_nulls::<F>(output, header);
                Ok(false)
            }
            // The record was malformed; the general loop reproduces its error
            // from `record_start`, where `self.location` still points.
            None => {
                output.clear_fields();
                Ok(false)
            }
        }
    }

    /// Finish a record whose quoted head is parsed and whose plain tail bailed
    /// at a further interior quote, by alternating the scalar quoted-field
    /// parser and the kernel across what remains.
    ///
    /// On entry `self.location` is the field-start quote the kernel stopped at
    /// and `output` holds the fields settled before it. Each round reads the
    /// quoted field with the scalar parser -- its one search over the run is
    /// what the kernel cannot vectorize -- and hands any plain tail straight
    /// back to the kernel, so a record that quotes several interior columns is
    /// still parsed mostly by the kernel rather than wholly scalar.
    ///
    /// Returns `Ok(true)` when the record is finished. `Ok(false)` leaves
    /// `output` and `self.location` exactly as the general loop needs to
    /// reproduce the record's error from the bail point.
    #[inline(never)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn resume_owned_after_interior_quote<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
    ) -> Result<bool, Error> {
        let record_origin = record_start;
        let Some(parse) = self.quoted_prefix_parser else {
            return Ok(false);
        };
        loop {
            let Some(window) = self.owned_resume_window(input, record_start) else {
                return Ok(bool::default());
            };
            let quote_start = self.location;
            let settled_fields = output.len();
            let settled_bytes = output.bytes_len();
            match parse(window, output) {
                // The quoted field ran to the end of the record.
                Some((consumed, true)) => {
                    // gamma::skip(assign_value.default, reason = "resetting the resumed quoted-field location to zero reports completion without consuming the record and causes repeated output growth")
                    self.location = quote_start + consumed;
                    return Ok(true);
                }
                // A plain tail follows the quoted field; the kernel takes it and
                // either finishes the record or bails at a further interior
                // quote, in which case another round reads that one too.
                Some((consumed, false)) => {
                    self.location = quote_start + consumed;
                    // gamma::skip(cond.always_false, reason = "discarding a completed plain tail leaves the resume loop on an already-settled record and makes it append that tail repeatedly")
                    if self.try_parse_owned_plain_from::<F>(
                        input,
                        output,
                        record_origin,
                        self.location,
                    )? {
                        return Ok(true);
                    }
                }
                // The scalar parser could not read a quoted field here. Restore
                // the fields settled before this quote and leave the cursor on
                // it so the general loop reproduces the record's exact error.
                None => {
                    output.truncate_storage(settled_fields, settled_bytes);
                    return Ok(false);
                }
            }
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the hot call site passes `resume == record_start`, and only inlining lets that fold back into the single-variable loop the kernel had before the resume parameter existed"
    )]
    fn try_parse_owned_plain<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
    ) -> Result<bool, Error> {
        #[cfg(target_arch = "x86_64")]
        if self.is_csv_format::<F>()
            && let Some(res) =
                self.try_parse_default_csv_owned_plain::<false>(input, output, record_start)
        {
            return Ok(res);
        }
        // A reusable record that has grown past a narrow row predicts enough
        // fields for per-field memcpy calls to dominate. Select that
        // monomorphization once per record; keeping the choice out of the field
        // loop leaves ordinary rows on their original kernel.
        match use_short_owned_fields(output.field_capacity()) {
            false => match use_short_owned_fields(output.field_capacity()) {
                false => {
                    self.try_parse_owned_plain_from::<F>(input, output, record_start, record_start)
                }
                true => self.try_parse_owned_plain_from_mode::<F, true>(
                    input,
                    output,
                    record_start,
                    record_start,
                ),
            },
            true => self.try_parse_owned_plain_from_mode::<F, true>(
                input,
                output,
                record_start,
                record_start,
            ),
        }
    }

    #[inline(always)]
    fn try_parse_owned_plain_ascii<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
    ) -> Result<bool, Error> {
        #[cfg(target_arch = "x86_64")]
        if self.is_csv_format::<F>()
            && let Some(res) =
                self.try_parse_default_csv_owned_plain::<true>(input, output, record_start)
        {
            return Ok(res);
        }
        self.try_parse_owned_plain_from::<F>(input, output, record_start, record_start)
    }

    // #[gamma::skip(fn_value.ok, reason = "returning Ok(false) from the plain-resume adapter discards successful cursor progress and makes the caller retry the same record")]
    /// Run the plain kernel from `resume`, which need not be where the record
    /// began.
    ///
    /// `record_start` is still what the record-length limit is measured from,
    /// so a record whose quoted head is already parsed cannot buy itself extra
    /// length by handing the rest over.
    #[inline(always)]
    #[expect(clippy::inline_always, reason = "see `try_parse_owned_plain`")]
    fn try_parse_owned_plain_from<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        resume: usize,
    ) -> Result<bool, Error> {
        self.try_parse_owned_plain_from_mode::<F, false>(input, output, record_start, resume)
    }

    // gamma::skip(fn_value.ok, reason = "returning Ok(false) from the structural record kernel suppresses every successful record boundary and drives unbounded fallback retries")
    #[inline(always)]
    #[expect(clippy::inline_always, reason = "see `try_parse_owned_plain`")]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn try_parse_owned_plain_from_mode<F: CsvFormat, const SHORT: bool>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        resume: usize,
    ) -> Result<bool, Error> {
        let scan_end = cmp::min(
            input.len(),
            record_start.saturating_add(self.limits.max_record_bytes.saturating_add(1)),
        );
        let mut structural = StructuralScanner::resume(
            &input[..scan_end],
            self.fmt_delimiter::<F>(),
            self.fmt_quote::<F>(),
            self.fmt_terminator::<F>(),
            resume,
            self.block_cache,
        );
        let mut field_start = resume;
        for mut block in &mut structural {
            while let Some((cursor, byte)) = block.next_match() {
                if byte == self.fmt_delimiter::<F>() {
                    match cursor.cmp(&field_start) {
                        cmp::Ordering::Equal => {
                            let mut after_run = cursor;
                            while input.get(after_run) == Some(&self.fmt_delimiter::<F>()) {
                                // gamma::skip(stmt.delete_assign, reason = "not advancing through an empty-field delimiter run leaves the inner loop on the same delimiter forever")
                                // gamma::skip(literal.int_decrement, reason = "a zero delimiter-run increment leaves the scanner on the same delimiter forever")
                                after_run += 1;
                            }
                            let count = after_run - cursor;
                            let available = self.limits.max_fields - output.len();
                            if count > available {
                                return Err(self.error_for(
                                    input,
                                    ErrorKind::TooManyFields {
                                        limit: self.limits.max_fields,
                                    },
                                    cursor + available,
                                    self.limits.max_fields,
                                ));
                            }
                            output.append_empty_fields(count);
                            field_start = after_run;
                        }
                        cmp::Ordering::Less => continue,
                        cmp::Ordering::Greater => {
                            self.push_structural_owned_field::<SHORT>(
                                input,
                                output,
                                field_start..cursor,
                                cursor,
                            )?;
                            field_start = cursor
                                .checked_add(1)
                                .expect("a structural cursor is always inside the input");
                        }
                    }
                } else if byte == self.fmt_quote::<F>()
                    && self.fmt_quoting_enabled::<F>()
                    && (cursor == field_start || !self.fmt_permits_unquoted_quotes::<F>())
                {
                    if cursor != field_start {
                        // gamma::skip(result.err_to_ok, reason = "turning an unexpected interior quote into success leaves the parser cursor on that quote and causes repeated fallback attempts")
                        return Err(self.error_for(
                            input,
                            ErrorKind::UnexpectedQuote,
                            cursor,
                            output.len(),
                        ));
                    }
                    self.location = cursor;
                    return Ok(false);
                } else if self.is_terminator(byte) {
                    let field_end = if self.fmt_record_ending::<F>() == RecordEnding::Newline
                        && cursor > field_start
                        && input[cursor - 1] == b'\r'
                    {
                        cursor - 1
                    } else {
                        cursor
                    };
                    self.push_structural_owned_field::<SHORT>(
                        input,
                        output,
                        field_start..field_end,
                        cursor,
                    )?;
                    self.location = cursor + 1;
                    self.check_record_limit_for(input, record_start, self.location, output.len())?;
                    return Ok(true);
                }
            }
        }
        if scan_end < input.len() {
            return Err(self.error_for(
                input,
                ErrorKind::RecordTooLarge {
                    limit: self.limits.max_record_bytes,
                },
                scan_end,
                output.len(),
            ));
        }
        self.push_structural_owned_field::<SHORT>(input, output, field_start..scan_end, scan_end)?;
        self.location = scan_end;
        self.check_record_limit_for(input, record_start, self.location, output.len())?;
        Ok(true)
    }

    #[expect(
        clippy::inline_always,
        reason = "the SHORT mode must fold before the structural record loop is emitted"
    )]
    #[inline(always)]
    fn push_structural_owned_field<const SHORT: bool>(
        &self,
        input: &[u8],
        output: &mut RecordStorage,
        range: Range<usize>,
        at: usize,
    ) -> Result<(), Error> {
        match SHORT {
            true => self.push_owned_short_field(input, output, range, at),
            false => self.push_owned_field(input, output, range, at),
        }
    }

    /// Mark the NULL fields of a record the owned plain kernel produced.
    ///
    /// Resolved here rather than once per record higher up: doing it up front
    /// measured +2.9% on the runtime-configured dialect benchmark, where the
    /// dialect is a real load instead of a constant, which is more than the
    /// NULL support costs in total.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "for a static Nulls::None format the guard folds to nothing, which is the whole point of taking the format as a parameter"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn mark_owned_nulls<F: CsvFormat>(&self, output: &mut RecordStorage, header: bool) {
        if header {
            return;
        }
        let nulls = self.fmt_nulls::<F>();
        match nulls {
            Nulls::None => {}
            Nulls::PostgresCsv => output.mark_null_fields(|field| field.is_empty()),
            Nulls::Mysql => output.mark_null_fields(|field| field == b"\\N"),
        }
    }

    /// Push one unquoted owned field from the scalar fallback, as a NULL when
    /// the dialect says so.
    ///
    /// The plain kernel gets this from [`ByteRecord::mark_null_fields`]
    /// instead, but the fallback also parses records containing quoted fields,
    /// which are never NULL and which a pass over the finished record could no
    /// longer tell apart.
    fn push_owned_unquoted(
        &self,
        input: &[u8],
        output: &mut RecordStorage,
        range: Range<usize>,
        at: usize,
        nulls: Nulls,
    ) -> Result<(), Error> {
        if raw_field_is_null(nulls, &input[range.clone()]) {
            if output.len() == self.limits.max_fields {
                return Err(self.error_for(
                    input,
                    ErrorKind::TooManyFields {
                        limit: self.limits.max_fields,
                    },
                    at,
                    output.len(),
                ));
            }
            output.append_null_field();
            return Ok(());
        }
        self.push_owned_field(input, output, range, at)
    }

    // gamma::skip(fn_value.ok, reason = "forcing every unquoted field parse to report a non-terminal field leaves EOF and terminator records in an unbounded outer field loop")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_owned_unquoted_field(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
        header: bool,
    ) -> Result<bool, Error> {
        let nulls = if header { Nulls::None } else { self.nulls };
        let field_start = self.location;
        let scan_end = self.scan_end(input, record_start, field_start);
        let remaining = &input[self.location..scan_end];
        let special = if self.syntax.quoting_enabled() && !self.syntax.permits_unquoted_quotes() {
            find3_near(
                self.dialect.delimiter,
                self.dialect.quote,
                self.dialect.record_ending.byte(),
                remaining,
            )
        } else {
            find2_near(
                self.dialect.delimiter,
                self.dialect.record_ending.byte(),
                remaining,
            )
        }
        .map(|relative| self.location + relative);

        let Some(at) = special else {
            let range = field_start..scan_end;
            self.check_scan_end_for(input, scan_end, record_start, range.start, output.len())?;
            self.push_owned_unquoted(input, output, range, scan_end, nulls)?;
            self.location = scan_end;
            return Ok(true);
        };

        let byte = input[at];
        if byte == self.dialect.quote {
            // gamma::skip(result.err_to_ok, reason = "accepting an unexpected quote without moving the cursor makes the outer owned parser retry the same field forever")
            return Err(self.error_for(input, ErrorKind::UnexpectedQuote, at, output.len()));
        }
        let field_end = if self.is_terminator(byte)
            && self.dialect.record_ending == RecordEnding::Newline
            && at > field_start
            && input[at - 1] == b'\r'
        {
            at - 1
        } else {
            at
        };
        self.push_owned_unquoted(input, output, field_start..field_end, at, nulls)?;
        self.location = at + 1;
        self.check_record_limit_for(input, record_start, self.location, output.len())?;
        Ok(self.is_terminator(byte))
    }

    // gamma::skip(fn_value.ok, reason = "forcing every quoted field to report a non-terminal field leaves completed records in an unbounded outer field loop")
    fn parse_owned_quoted_field(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        record_start: usize,
    ) -> Result<bool, Error> {
        if output.len() == self.limits.max_fields {
            return Err(self.error_for(
                input,
                ErrorKind::TooManyFields {
                    limit: self.limits.max_fields,
                },
                self.location,
                output.len(),
            ));
        }

        let content_start = self.location + 1;
        let mut segment_start = content_start;
        let mut cursor = content_start;
        loop {
            let scan_end = self.scan_end(input, record_start, content_start);
            if !matches!(cursor.cmp(&scan_end), cmp::Ordering::Less) {
                self.check_scan_end_for(
                    input,
                    scan_end,
                    record_start,
                    content_start,
                    output.len(),
                )?;
                // gamma::skip(result.err_to_ok, reason = "accepting a quote that reaches the scan boundary without moving location makes the outer parser retry the same unterminated field")
                return Err(self.error_for(
                    input,
                    ErrorKind::UnterminatedQuotedField,
                    scan_end,
                    output.len(),
                ));
            }
            let found = match self.dialect.escape {
                Escape::DoubleQuote => find1_near(self.dialect.quote, &input[cursor..scan_end]),
                Escape::Backslash(escape) => {
                    find2_near(self.dialect.quote, escape, &input[cursor..scan_end])
                }
                Escape::Mysql | Escape::Unquoted(_) => unreachable_owned_unquoted_escape(),
            }
            .map(|relative| cursor + relative);

            let Some(at) = found else {
                self.check_scan_end_for(
                    input,
                    scan_end,
                    record_start,
                    content_start,
                    output.len(),
                )?;
                // gamma::skip(result.err_to_ok, reason = "accepting a missing closing quote without moving location leaves the parser retrying the same unterminated field")
                return Err(self.error_for(
                    input,
                    ErrorKind::UnterminatedQuotedField,
                    scan_end,
                    output.len(),
                ));
            };
            match self.dialect.escape {
                Escape::DoubleQuote if input.get(at + 1) == Some(&self.dialect.quote) => {
                    // Search bounds prove `segment_start <= at <= input.len()`.
                    let segment = &input[segment_start..at];
                    append_owned_segment(output, segment);
                    output.push_byte(self.dialect.quote);
                    // gamma::skip(stmt.delete_assign, reason = "not advancing past a doubled quote makes the quote search rediscover the same escape and append bytes without bound")
                    // gamma::skip(arith.add_to_sub, reason = "moving backward after a doubled quote repeatedly reparses the same quoted segment and exhausts memory")
                    // gamma::skip(literal.int_to_zero, reason = "a zero doubled-quote width leaves the cursor on the same quote pair forever")
                    cursor = at + 2;
                    segment_start = cursor;
                }
                Escape::Backslash(escape) if input[at] == escape => {
                    let Some(&escaped) = input.get(at + 1) else {
                        // gamma::skip(result.err_to_ok, reason = "accepting a trailing escape without moving the cursor makes the quoted-field loop retry it forever")
                        return Err(self.error_for(
                            input,
                            ErrorKind::InvalidEscape(escape),
                            at,
                            output.len(),
                        ));
                    };
                    if escaped != self.dialect.quote
                        && escaped != escape
                        && !self.syntax.permits_any_backslash_escape()
                    {
                        // gamma::skip(result.err_to_ok, reason = "accepting an invalid escape without moving the cursor leaves the parser repeatedly examining the same escape")
                        return Err(self.error_for(
                            input,
                            ErrorKind::InvalidEscape(escaped),
                            at + 1,
                            output.len(),
                        ));
                    }
                    // Search bounds prove `segment_start <= at <= input.len()`.
                    let segment = &input[segment_start..at];
                    append_owned_segment(output, segment);
                    output.push_byte(escaped);
                    // gamma::skip(stmt.delete_assign, reason = "not advancing past a backslash escape makes the quote search rediscover it and append decoded bytes without bound")
                    // gamma::skip(arith.add_to_sub, reason = "moving backward after a backslash escape repeatedly reparses the same segment and exhausts memory")
                    // gamma::skip(literal.int_to_zero, reason = "a zero escape width leaves the quoted-field cursor on the same escape forever")
                    cursor = at + 2;
                    segment_start = cursor;
                }
                _ => {
                    // Search bounds prove `segment_start <= at <= input.len()`.
                    let segment = &input[segment_start..at];
                    append_owned_segment(output, segment);
                    output.finish_field();
                    return self.finish_owned_quoted_field(
                        input,
                        output.len(),
                        record_start,
                        at + 1,
                    );
                }
            }
        }
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn finish_owned_quoted_field(
        &mut self,
        input: &[u8],
        field_count: usize,
        record_start: usize,
        after_quote: usize,
    ) -> Result<bool, Error> {
        let after_quote = self.skip_compatible_post_quote_whitespace::<Dynamic>(input, after_quote);
        let Some(&next) = input.get(after_quote) else {
            self.location = after_quote;
            self.check_record_limit_for(input, record_start, self.location, field_count)?;
            return Ok(true);
        };
        if next == self.dialect.delimiter {
            self.location = after_quote + 1;
            return Ok(false);
        }
        if self.is_terminator(next) {
            self.location = after_quote + 1;
            self.check_record_limit_for(input, record_start, self.location, field_count)?;
            return Ok(true);
        }
        if self.dialect.record_ending == RecordEnding::Newline
            && next == b'\r'
            && input.get(after_quote + 1) == Some(&b'\n')
        {
            self.location = after_quote + 2;
            self.check_record_limit_for(input, record_start, self.location, field_count)?;
            return Ok(true);
        }
        Err(self.error_for(
            input,
            ErrorKind::UnexpectedByteAfterQuote(next),
            after_quote,
            field_count,
        ))
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_arch = "x86_64")]
    use crate::engine::record_parser::default_plain_packed_available;
    use crate::engine::record_parser::try_parse_default_record_prefix;

    fn assert_owned_error(error: Error, kind: ErrorKind, byte: usize, field: usize) {
        assert_eq!(error.kind(), kind);
        assert_eq!(error.location().byte, byte);
        assert_eq!(error.location().field, field);
    }

    fn fields(output: &RecordStorage) -> Vec<&[u8]> {
        output.iter().collect()
    }

    fn assert_exact_resume_window(
        input: &[u8],
        _output: &mut RecordStorage,
    ) -> Option<(usize, bool)> {
        assert_eq!(input, b"\"abc");
        None
    }

    fn configure_prediction_parsers(engine: &mut Engine) {
        engine.plain_kernel = true;
        engine.owned_parser = Some(try_parse_default_record::<false>);
        engine.quoted_prefix_parser = Some(try_parse_default_quoted_prefix::<false>);
        engine.interior_prefix_parser = Some(try_parse_default_interior_prefix::<false>);
        engine.interior_handoff_parser = engine.interior_prefix_parser;
        engine.multi_quote_parser = Some(try_parse_default_record_prefix::<false>);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn packed_owned_plain_fast_path_reports_exact_progress() {
        if !default_plain_packed_available() {
            return;
        }
        let record = b"a,b\n";
        let mut input = record.to_vec();
        input.resize(32, b'x');
        let mut engine = Engine::from_config(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut output = RecordStorage::new();

        assert_eq!(
            engine.try_parse_default_csv_owned_plain::<false>(&input, &mut output, 0),
            Some(true)
        );
        assert_eq!(engine.location, record.len());
        assert_eq!(fields(&output), [b"a".as_slice(), b"b".as_slice()]);
    }

    #[test]
    fn prediction_count_and_parser_selection_are_exact() {
        assert!(!use_short_owned_fields(WIDE_RECORD_FIELDS - 1));
        assert!(use_short_owned_fields(WIDE_RECORD_FIELDS));

        let quoted = b"\"a\"\n";
        let mut countdown = Engine::from_config(
            quoted,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut countdown);
        countdown.interior_quotes = 2;
        let mut countdown_output = RecordStorage::new();
        countdown
            .parse_owned_record::<Dynamic>(quoted, &mut countdown_output, 0, false)
            .expect("valid quoted record");
        assert_eq!(countdown.interior_quotes, 1);
        assert_eq!(countdown.location, quoted.len());

        let interior = b"a,\"b\",c\n";
        let mut predicted = Engine::from_config(
            interior,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut predicted);
        predicted.interior_quotes = 2;
        let mut predicted_output = RecordStorage::new();
        predicted
            .parse_owned_record::<Dynamic>(interior, &mut predicted_output, 0, false)
            .expect("the predicted interior parser completes the record");
        assert_eq!(predicted.interior_quotes, 1);
        assert_eq!(predicted.location, interior.len());
        assert_eq!(
            fields(&predicted_output),
            [b"a".as_slice(), b"b".as_slice(), b"c".as_slice()]
        );

        let mut first_bail = Engine::from_config(
            interior,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut first_bail);
        let expected_handoff = first_bail
            .interior_handoff_parser
            .map(|parser| parser as usize);
        first_bail.interior_prefix_parser = first_bail.multi_quote_parser;
        assert_ne!(
            first_bail
                .interior_prefix_parser
                .map(|parser| parser as usize),
            expected_handoff
        );
        let mut first_bail_output = RecordStorage::new();
        first_bail
            .parse_owned_record::<Dynamic>(interior, &mut first_bail_output, 0, false)
            .expect("the first interior quote resumes successfully");
        assert_eq!(first_bail.interior_quotes, INTERIOR_QUOTE_RUN);
        assert_eq!(
            first_bail
                .interior_prefix_parser
                .map(|parser| parser as usize),
            expected_handoff
        );
        assert_eq!(first_bail.location, interior.len());
    }

    #[test]
    fn repeated_interior_quotes_select_the_multi_quote_parser_and_mark_nulls() {
        let input = b"\"a\",\\N,\"c\",d\n";
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        let mut engine = Engine::from_config(input, settings);
        configure_prediction_parsers(&mut engine);
        let expected_multi = engine.multi_quote_parser.map(|parser| parser as usize);
        let initial = engine.interior_prefix_parser.map(|parser| parser as usize);
        assert_ne!(initial, expected_multi);
        let mut output = RecordStorage::new();

        engine
            .parse_owned_record::<Dynamic>(input, &mut output, 0, false)
            .expect("valid alternating quoted record");

        assert_eq!(engine.location, input.len());
        assert_eq!(engine.interior_quotes, INTERIOR_QUOTE_RUN);
        assert_eq!(
            engine.interior_prefix_parser.map(|parser| parser as usize),
            expected_multi
        );
        assert_eq!(
            fields(&output),
            [
                b"a".as_slice(),
                b"".as_slice(),
                b"c".as_slice(),
                b"d".as_slice(),
            ]
        );
        assert_eq!(output.is_null(0), Some(false));
        assert_eq!(output.is_null(1), Some(true));
        assert_eq!(output.is_null(2), Some(false));

        let leading = b"\"a\",\\N\n";
        let mut leading_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        leading_settings.nulls = Nulls::Mysql;
        let mut leading_engine = Engine::from_config(leading, leading_settings);
        configure_prediction_parsers(&mut leading_engine);
        let mut leading_output = RecordStorage::new();
        leading_engine
            .parse_owned_record::<Dynamic>(leading, &mut leading_output, 0, false)
            .expect("the plain tail after a quoted head is valid");
        assert_eq!(fields(&leading_output), [b"a".as_slice(), b"".as_slice()]);
        assert_eq!(leading_output.is_null(1), Some(true));

        let interior = b"a,\"b\",\\N\n";
        let mut interior_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        interior_settings.nulls = Nulls::Mysql;
        let mut interior_engine = Engine::from_config(interior, interior_settings);
        configure_prediction_parsers(&mut interior_engine);
        interior_engine.interior_quotes = 1;
        let mut interior_output = RecordStorage::new();
        interior_engine
            .parse_owned_record::<Dynamic>(interior, &mut interior_output, 0, false)
            .expect("the predicted interior quoted field and plain tail are valid");
        assert_eq!(
            fields(&interior_output),
            [b"a".as_slice(), b"b".as_slice(), b"".as_slice()]
        );
        assert_eq!(interior_output.is_null(2), Some(true));
    }

    #[test]
    fn interior_prefix_field_count_errors_report_the_concrete_actual_count() {
        let input = b"a,\"b\",c\n";
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        settings.field_count = FieldCount::Exact(2);
        let mut engine = Engine::from_config(input, settings);
        configure_prediction_parsers(&mut engine);
        engine.interior_quotes = 2;
        let mut output = RecordStorage::new();

        let error = engine
            .parse_owned_record::<Dynamic>(input, &mut output, 0, false)
            .expect_err("three fields violate the exact count of two");
        assert_eq!(
            error.kind(),
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 3,
            }
        );

        let repeated = b"a,\"b\",c,\"d\",e\n";
        let mut repeated_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        repeated_settings.field_count = FieldCount::Exact(4);
        let mut repeated_engine = Engine::from_config(repeated, repeated_settings);
        configure_prediction_parsers(&mut repeated_engine);
        repeated_engine.interior_quotes = 2;
        let mut repeated_output = RecordStorage::new();
        let error = repeated_engine
            .parse_owned_record::<Dynamic>(repeated, &mut repeated_output, 0, false)
            .expect_err("five fields violate the exact count of four");
        assert_eq!(
            error.kind(),
            ErrorKind::FieldCountMismatch {
                expected: 4,
                actual: 5,
            }
        );
        assert_eq!(repeated_engine.interior_quotes, INTERIOR_QUOTE_RUN);
        assert_eq!(
            repeated_engine
                .interior_prefix_parser
                .map(|parser| parser as usize),
            repeated_engine
                .multi_quote_parser
                .map(|parser| parser as usize)
        );

        let nullable = b"a,\"b\",\\N,\"d\",e\n";
        let mut nullable_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        nullable_settings.nulls = Nulls::Mysql;
        let mut nullable_engine = Engine::from_config(nullable, nullable_settings);
        configure_prediction_parsers(&mut nullable_engine);
        nullable_engine.interior_quotes = 2;
        let mut nullable_output = RecordStorage::new();
        nullable_engine
            .parse_owned_record::<Dynamic>(nullable, &mut nullable_output, 0, false)
            .expect("the predicted multi-quote record is valid");
        assert_eq!(nullable_output.is_null(2), Some(true));

        let malformed = b"a,\"b\",\\N,\"bad\"x";
        let mut malformed_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        malformed_settings.nulls = Nulls::Mysql;
        let mut malformed_engine = Engine::from_config(malformed, malformed_settings);
        configure_prediction_parsers(&mut malformed_engine);
        let mut malformed_output = RecordStorage::new();
        assert!(
            !malformed_engine
                .parse_owned_interior_prefix::<Dynamic>(malformed, &mut malformed_output, 0, false,)
                .expect("malformed input falls back")
        );
        assert_eq!(malformed_output.is_null(2), Some(true));
    }

    #[test]
    fn quoted_resume_window_obeys_both_sides_of_the_record_limit() {
        let exact = b"xx\"a\"\n";
        let mut exact_engine = Engine::from_config(
            exact,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 10)),
        );
        configure_prediction_parsers(&mut exact_engine);
        exact_engine.location = 2;
        assert_eq!(
            exact_engine.owned_resume_window(exact, 2),
            Some(&exact[2..6])
        );
        exact_engine.location = 6;
        assert_eq!(exact_engine.owned_resume_window(exact, 2), None);
        exact_engine.location = 7;
        assert_eq!(exact_engine.owned_resume_window(exact, 2), None);
        exact_engine.location = 2;
        let mut exact_output = RecordStorage::new();
        assert!(
            exact_engine
                .resume_owned_after_interior_quote::<Dynamic>(exact, &mut exact_output, 2,)
                .expect("exact record window")
        );
        assert_eq!(exact_engine.location, exact.len());
        assert_eq!(exact_output.get(0), Some(&b"a"[..]));

        let over = b"xx\"abc\"";
        let mut over_engine = Engine::from_config(
            over,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 10)),
        );
        configure_prediction_parsers(&mut over_engine);
        over_engine.location = 2;
        assert_eq!(over_engine.owned_resume_window(over, 2), Some(&over[2..6]));
        let mut over_output = RecordStorage::new();
        assert!(
            !over_engine
                .resume_owned_after_interior_quote::<Dynamic>(over, &mut over_output, 2)
                .expect("the over-limit closing quote is outside the parser window")
        );
        assert_eq!(over_engine.location, 2);
        assert!(over_output.is_empty());

        let mut call_site = Engine::from_config(
            over,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 10)),
        );
        call_site.quoted_prefix_parser = Some(assert_exact_resume_window);
        call_site.location = 2;
        let mut call_site_output = RecordStorage::new();
        assert!(
            !call_site
                .resume_owned_after_interior_quote::<Dynamic>(over, &mut call_site_output, 2,)
                .expect("the exact resume window falls back")
        );
    }

    #[test]
    fn every_owned_path_measures_record_limits_from_the_original_start() {
        let unquoted_quote = b"xx\"ab\"\n";
        let mut unquoted_quote_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 10));
        unquoted_quote_settings.syntax =
            Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut unquoted_quote_engine =
            Engine::from_config(unquoted_quote, unquoted_quote_settings);
        unquoted_quote_engine.plain_kernel = false;
        unquoted_quote_engine.location = 2;
        let mut unquoted_quote_output = RecordStorage::new();
        let error = unquoted_quote_engine
            .parse_owned_record::<Dynamic>(unquoted_quote, &mut unquoted_quote_output, 2, false)
            .expect_err("a literal leading quote cannot move the record origin");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 4 }, 7, 1);

        let leading_repeated = b"xx\"a\",b,\"c\",d\n";
        let mut leading_repeated_engine = Engine::from_config(
            leading_repeated,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut leading_repeated_engine);
        leading_repeated_engine.limits = Limits::new(11, 100, 10);
        leading_repeated_engine.location = 2;
        let mut leading_repeated_output = RecordStorage::new();
        let error = leading_repeated_engine
            .parse_owned_record::<Dynamic>(leading_repeated, &mut leading_repeated_output, 2, false)
            .expect_err("the second quoted field cannot move the record origin");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 11 }, 14, 4);

        let quoted_tail = b"xx\"a\",b\n";
        let mut quoted_tail_engine = Engine::from_config(
            quoted_tail,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut quoted_tail_engine);
        quoted_tail_engine.limits = Limits::new(5, 100, 10);
        quoted_tail_engine.location = 2;
        let mut quoted_tail_output = RecordStorage::new();
        let error = quoted_tail_engine
            .parse_owned_record::<Dynamic>(quoted_tail, &mut quoted_tail_output, 2, false)
            .expect_err("the quoted-head plain tail is six bytes long");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 5 }, 8, 2);

        let interior = b"xxa,\"b\",c\n";
        let mut interior_engine = Engine::from_config(
            interior,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut interior_engine);
        interior_engine.limits = Limits::new(7, 100, 10);
        interior_engine.location = 2;
        let mut interior_output = RecordStorage::new();
        let error = interior_engine
            .parse_owned_record::<Dynamic>(interior, &mut interior_output, 2, false)
            .expect_err("the interior-quote record is eight bytes long");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 7 }, 10, 3);

        let mut predicted_interior = Engine::from_config(
            interior,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut predicted_interior);
        predicted_interior.interior_quotes = 1;
        predicted_interior.limits = Limits::new(7, 100, 10);
        predicted_interior.location = 2;
        let mut predicted_interior_output = RecordStorage::new();
        let error = predicted_interior
            .parse_owned_record::<Dynamic>(interior, &mut predicted_interior_output, 2, false)
            .expect_err("the predicted plain tail cannot move the record origin");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 7 }, 10, 3);

        let repeated_interior = b"xxa,\"b\",c,\"d\",e\n";
        let mut repeated_interior_engine = Engine::from_config(
            repeated_interior,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut repeated_interior_engine);
        repeated_interior_engine.interior_quotes = 1;
        repeated_interior_engine.limits = Limits::new(13, 100, 10);
        repeated_interior_engine.location = 2;
        let mut repeated_interior_output = RecordStorage::new();
        let error = repeated_interior_engine
            .parse_owned_record::<Dynamic>(
                repeated_interior,
                &mut repeated_interior_output,
                2,
                false,
            )
            .expect_err("a resumed second quote cannot move the record origin");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 13 }, 16, 5);

        let mut direct_resume = Engine::from_config(
            quoted_tail,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut direct_resume);
        direct_resume.limits = Limits::new(5, 100, 10);
        direct_resume.location = 2;
        let mut direct_resume_output = RecordStorage::new();
        let error = direct_resume
            .resume_owned_after_interior_quote::<Dynamic>(quoted_tail, &mut direct_resume_output, 2)
            .expect_err("the resumed quoted-head record is six bytes long");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 5 }, 8, 2);

        for (input, quoted) in [(&b"xxabc\n"[..], false), (&b"xx\"a\"\n"[..], true)] {
            let mut general = Engine::from_config(
                input,
                ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
            );
            general.plain_kernel = false;
            general.limits = Limits::new(3, 100, 10);
            general.location = 2;
            let mut output = RecordStorage::new();
            let error = general
                .parse_owned_record::<Dynamic>(input, &mut output, 2, false)
                .expect_err("the general owned path is four bytes long");
            assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 3 }, 6, 1);
            assert_eq!(
                output.get(0),
                Some(if quoted {
                    b"a".as_slice()
                } else {
                    b"abc".as_slice()
                })
            );
        }

        let spaced = b"xxa,   b\n";
        let mut spaced_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10));
        spaced_settings.skip_initial_space = true;
        let mut spaced_engine = Engine::from_config(spaced, spaced_settings);
        spaced_engine.plain_kernel = false;
        spaced_engine.location = 2;
        let mut spaced_output = RecordStorage::new();
        let error = spaced_engine
            .parse_owned_record::<Dynamic>(spaced, &mut spaced_output, 2, false)
            .expect_err("delimiter spaces remain measured from the original record start");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 3 }, 6, 0);

        let wide = b"xxa,b\n";
        let mut wide_engine = Engine::from_config(
            wide,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        wide_engine.owned_parser = None;
        wide_engine.limits = Limits::new(3, 100, 10);
        wide_engine.location = 2;
        let mut wide_output = RecordStorage::with_capacity(16, 16);
        let error = wide_engine
            .try_parse_owned_plain::<Dynamic>(wide, &mut wide_output, 2)
            .expect_err("wide reusable storage still measures from byte two");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 3 }, 6, 2);
    }

    #[test]
    fn failed_quoted_resume_restores_exact_storage_and_location() {
        let input = b"xx\"bad\"x";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        configure_prediction_parsers(&mut engine);
        engine.location = 2;
        let mut output = RecordStorage::new();
        output.append_field(b"settled");

        assert!(
            !engine
                .resume_owned_after_interior_quote::<Dynamic>(input, &mut output, 2)
                .expect("malformed quoted field falls back")
        );
        assert_eq!(engine.location, 2);
        assert_eq!(fields(&output), [b"settled".as_slice()]);
    }

    #[test]
    fn structural_plain_kernel_reports_exact_empty_runs_and_limit_errors() {
        let exact = b"xxa,,,\n";
        let mut exact_engine = Engine::from_config(
            exact,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 4)),
        );
        exact_engine.owned_parser = None;
        let mut exact_output = RecordStorage::new();
        assert!(
            exact_engine
                .try_parse_owned_plain_from_mode::<Dynamic, false>(exact, &mut exact_output, 2, 2,)
                .expect("the exact field limit is accepted")
        );
        assert_eq!(
            fields(&exact_output),
            [
                b"a".as_slice(),
                b"".as_slice(),
                b"".as_slice(),
                b"".as_slice(),
            ]
        );
        assert_eq!(exact_engine.location, exact.len());

        let separated = b"xxa,b\n";
        let mut separated_engine = Engine::from_config(
            separated,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        separated_engine.owned_parser = None;
        let mut separated_output = RecordStorage::new();
        assert!(
            separated_engine
                .try_parse_owned_plain_from_mode::<Dynamic, false>(
                    separated,
                    &mut separated_output,
                    2,
                    2,
                )
                .expect("a single delimiter separates both exact fields")
        );
        assert_eq!(
            fields(&separated_output),
            [b"a".as_slice(), b"b".as_slice()]
        );

        let mut limited = Engine::from_config(
            exact,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 2)),
        );
        limited.owned_parser = None;
        let mut limited_output = RecordStorage::new();
        let error = limited
            .try_parse_owned_plain_from_mode::<Dynamic, false>(exact, &mut limited_output, 2, 2)
            .expect_err("the delimiter run creates one field too many");
        assert_owned_error(error, ErrorKind::TooManyFields { limit: 2 }, 5, 2);

        let equality = b"xxa,,,z\n";
        let mut equality_engine = Engine::from_config(
            equality,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 3)),
        );
        equality_engine.owned_parser = None;
        let mut equality_output = RecordStorage::new();
        let error = equality_engine
            .try_parse_owned_plain_from_mode::<Dynamic, false>(equality, &mut equality_output, 2, 2)
            .expect_err("the field after an exactly fitting delimiter run is too many");
        assert_owned_error(error, ErrorKind::TooManyFields { limit: 3 }, 7, 3);

        let terminated = b"xxa,b\n";
        let mut record_limited = Engine::from_config(
            terminated,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10)),
        );
        record_limited.owned_parser = None;
        let mut record_output = RecordStorage::new();
        let error = record_limited
            .try_parse_owned_plain_from_mode::<Dynamic, false>(terminated, &mut record_output, 2, 2)
            .expect_err("the terminator crosses the record limit");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 3 }, 6, 2);

        let eof = b"xxa,b";
        let mut eof_limited = Engine::from_config(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        eof_limited.owned_parser = None;
        let mut eof_output = RecordStorage::new();
        let error = eof_limited
            .try_parse_owned_plain_from_mode::<Dynamic, false>(eof, &mut eof_output, 2, 2)
            .expect_err("EOF crosses the record limit");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 2 }, 5, 2);

        let truncated = b"xxabcdef";
        let mut truncated_engine = Engine::from_config(
            truncated,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10)),
        );
        truncated_engine.owned_parser = None;
        let mut truncated_output = RecordStorage::new();
        let error = truncated_engine
            .try_parse_owned_plain_from_mode::<Dynamic, false>(
                truncated,
                &mut truncated_output,
                2,
                2,
            )
            .expect_err("the scan window ends before the input");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 3 }, 6, 0);
    }

    #[test]
    fn owned_null_and_field_push_helpers_preserve_exact_metadata() {
        let mut none_engine = Engine::from_config(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        none_engine.nulls = Nulls::None;
        let mut none_output = RecordStorage::new();
        none_output.append_field(b"");
        none_engine.mark_owned_nulls::<Dynamic>(&mut none_output, false);
        assert!(!none_output.null_aware());
        assert_eq!(none_output.is_null(0), Some(false));

        none_engine.nulls = Nulls::PostgresCsv;
        let mut header_output = RecordStorage::new();
        header_output.append_field(b"");
        none_engine.mark_owned_nulls::<Dynamic>(&mut header_output, true);
        assert!(!header_output.null_aware());
        assert_eq!(header_output.is_null(0), Some(false));

        let input = b"\\N";
        let limited = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1)),
        );
        let mut full = RecordStorage::new();
        full.append_field(b"first");
        let error = limited
            .push_owned_unquoted(input, &mut full, 0..2, 2, Nulls::Mysql)
            .expect_err("the NULL field would exceed the field-count limit");
        assert_owned_error(error, ErrorKind::TooManyFields { limit: 1 }, 2, 1);

        let direct = b"abcd";
        let direct_limited = Engine::from_config(
            direct,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        let mut direct_output = RecordStorage::new();
        let error = direct_limited
            .push_owned_unquoted(direct, &mut direct_output, 0..4, 4, Nulls::None)
            .expect_err("the helper reports the supplied field boundary");
        assert_owned_error(error, ErrorKind::FieldTooLarge { limit: 3 }, 4, 0);

        let too_long = b"xxabcd\n";
        let mut unquoted = Engine::from_config(
            too_long,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        unquoted.location = 2;
        let mut unquoted_output = RecordStorage::new();
        let error = unquoted
            .parse_owned_unquoted_field(too_long, &mut unquoted_output, 2, false)
            .expect_err("four bytes exceed the field limit");
        assert_owned_error(error, ErrorKind::FieldTooLarge { limit: 3 }, 6, 0);

        let scan_bound = b"xxabcde";
        let mut scan_bound_engine = Engine::from_config(
            scan_bound,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        scan_bound_engine.location = 2;
        let mut scan_bound_output = RecordStorage::new();
        let error = scan_bound_engine
            .parse_owned_unquoted_field(scan_bound, &mut scan_bound_output, 2, false)
            .expect_err("the scan ends exactly one byte past the field limit");
        assert_owned_error(error, ErrorKind::FieldTooLarge { limit: 3 }, 6, 0);

        let null_boundary = b"xx\\N,\n";
        let mut null_boundary_engine = Engine::from_config(
            null_boundary,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0)),
        );
        null_boundary_engine.nulls = Nulls::Mysql;
        null_boundary_engine.location = 2;
        let mut null_boundary_output = RecordStorage::new();
        let error = null_boundary_engine
            .parse_owned_unquoted_field(null_boundary, &mut null_boundary_output, 2, false)
            .expect_err("the NULL field reports its delimiter boundary");
        assert_owned_error(error, ErrorKind::TooManyFields { limit: 0 }, 4, 0);

        let unexpected = b"xxa\"b\n";
        let mut quote_engine = Engine::from_config(
            unexpected,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        quote_engine.location = 2;
        let mut quote_output = RecordStorage::new();
        quote_output.append_field(b"settled");
        let error = quote_engine
            .parse_owned_unquoted_field(unexpected, &mut quote_output, 2, false)
            .expect_err("an interior quote is invalid in strict CSV");
        assert_owned_error(error, ErrorKind::UnexpectedQuote, 3, 1);

        let header = b"\\N\n";
        let mut header_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        header_settings.nulls = Nulls::Mysql;
        let mut header_engine = Engine::from_config(header, header_settings);
        header_engine.plain_kernel = false;
        let mut header_output = RecordStorage::new();
        header_engine
            .parse_owned_record::<Dynamic>(header, &mut header_output, 0, true)
            .expect("headers never apply the NULL policy");
        assert_eq!(header_output.get(0), Some(&b"\\N"[..]));
        assert_eq!(header_output.is_null(0), Some(false));
    }

    #[test]
    fn quoted_field_errors_and_finishes_report_exact_metadata() {
        let full = b"xx\"a\"\n";
        let mut full_engine = Engine::from_config(
            full,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1)),
        );
        full_engine.location = 2;
        let mut full_output = RecordStorage::new();
        full_output.append_field(b"settled");
        let error = full_engine
            .parse_owned_quoted_field(full, &mut full_output, 2)
            .expect_err("the quoted field exceeds the field-count limit");
        assert_owned_error(error, ErrorKind::TooManyFields { limit: 1 }, 2, 1);

        let input = b"xx\"abc";
        let mut unterminated = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        unterminated.location = 2;
        let mut output = RecordStorage::new();
        let error = unterminated
            .parse_owned_quoted_field(input, &mut output, 2)
            .expect_err("the closing quote is missing");
        assert_owned_error(error, ErrorKind::UnterminatedQuotedField, 6, 0);

        let escaped_to_edge = b"xx\"a\"\"";
        let mut edge = Engine::from_config(
            escaped_to_edge,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 10)),
        );
        edge.location = 2;
        let mut edge_output = RecordStorage::new();
        edge_output.append_field(b"settled");
        let error = edge
            .parse_owned_quoted_field(escaped_to_edge, &mut edge_output, 2)
            .expect_err("the doubled quote crosses the field limit at the scan edge");
        assert_owned_error(error, ErrorKind::FieldTooLarge { limit: 2 }, 6, 1);

        let mut exact_edge = Engine::from_config(
            escaped_to_edge,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        exact_edge.location = 2;
        let mut exact_edge_output = RecordStorage::new();
        exact_edge_output.append_field(b"settled");
        let error = exact_edge
            .parse_owned_quoted_field(escaped_to_edge, &mut exact_edge_output, 2)
            .expect_err("the exact raw-byte boundary remains an unterminated field");
        assert_owned_error(error, ErrorKind::UnterminatedQuotedField, 6, 1);

        let invalid = b"xx\"a\\x\"\n";
        let dialect = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let mut invalid_engine =
            Engine::from_config(invalid, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        invalid_engine.location = 2;
        let mut invalid_output = RecordStorage::new();
        invalid_output.append_field(b"settled");
        let error = invalid_engine
            .parse_owned_quoted_field(invalid, &mut invalid_output, 2)
            .expect_err("x is not a compatible backslash escape");
        assert_owned_error(error, ErrorKind::InvalidEscape(b'x'), 5, 1);

        for (input, after_quote, expected_byte) in [
            (&b"xx\"a\""[..], 5, 5),
            (&b"xx\"a\"\n"[..], 5, 6),
            (&b"xx\"a\"\r\n"[..], 5, 7),
        ] {
            let mut engine = Engine::from_config(
                input,
                ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
            );
            let error = engine
                .finish_owned_quoted_field(input, 5, 2, after_quote)
                .expect_err("the finished field crosses the record limit");
            assert_owned_error(
                error,
                ErrorKind::RecordTooLarge { limit: 2 },
                expected_byte,
                5,
            );
        }

        let crlf_boundary = b"xx\"a\"\r\n";
        let mut crlf_boundary_engine = Engine::from_config(
            crlf_boundary,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 10)),
        );
        let error = crlf_boundary_engine
            .finish_owned_quoted_field(crlf_boundary, 1, 2, 5)
            .expect_err("the CRLF crosses the record limit by exactly one byte");
        assert_owned_error(error, ErrorKind::RecordTooLarge { limit: 4 }, 7, 1);
    }

    #[test]
    fn structural_field_push_reports_the_supplied_error_offset() {
        let input = b"xxab";
        for short in [false, true] {
            let engine = Engine::from_config(
                input,
                ParserSettings::unheaded(Dialect::default(), Limits::new(100, 1, 10)),
            );
            let mut output = RecordStorage::new();
            output.append_field(b"settled");
            let result = if short {
                engine.push_structural_owned_field::<true>(input, &mut output, 2..4, 7)
            } else {
                engine.push_structural_owned_field::<false>(input, &mut output, 2..4, 7)
            };
            let error = result.expect_err("the two-byte field exceeds the limit");
            assert_owned_error(error, ErrorKind::FieldTooLarge { limit: 1 }, 7, 1);
        }
    }

    #[test]
    fn test_owned_parser_coverage_paths() {
        // field_capacity >= 16
        let input1 = b"a,b,c\n";
        let mut engine = Engine::from_config(
            input1,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut storage = RecordStorage::new();
        storage.reserve(20, 100);
        assert!(
            engine
                .parse_owned_record::<Dynamic>(input1, &mut storage, 0, false)
                .is_ok()
        );

        // TooManyFields in delimiter run
        let input2 = b",,,,\n";
        let settings_limit = ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 2));
        let mut engine_limit = Engine::from_config(input2, settings_limit);
        let mut storage_limit = RecordStorage::new();
        assert!(
            engine_limit
                .parse_owned_record::<Dynamic>(input2, &mut storage_limit, 0, false)
                .is_err()
        );

        // TooManyFields in push_owned_unquoted (with Nulls::PostgresCsv)
        let input3 = b",\n";
        let mut settings_nulls =
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1));
        settings_nulls.nulls = Nulls::PostgresCsv;
        let mut engine_nulls = Engine::from_config(input3, settings_nulls);
        let mut storage_nulls = RecordStorage::new();
        assert!(
            engine_nulls
                .parse_owned_record::<Dynamic>(input3, &mut storage_nulls, 0, false)
                .is_err()
        );

        // TooManyFields in parse_owned_quoted_field
        let mut storage_q = RecordStorage::new();
        storage_q.append_field(b"f1");
        let settings_q = ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1));
        let mut engine_q = Engine::from_config(b"\"abc\",\n", settings_q);
        engine_q.location = 0;
        assert!(
            engine_q
                .parse_owned_quoted_field(b"\"abc\",\n", &mut storage_q, 0)
                .is_err()
        );

        // Multiple interior quotes and alternating shapes: "foo",bar,"baz"\n
        let input_alt = b"\"foo\",bar,\"baz\"\n";
        let mut engine_alt = Engine::from_config(
            input_alt,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut storage_alt = RecordStorage::new();
        assert!(
            engine_alt
                .parse_owned_record::<Dynamic>(input_alt, &mut storage_alt, 0, false)
                .is_ok()
        );

        // resume_owned_after_interior_quote quote_start >= window_end
        let mut engine_win = Engine::from_config(
            input_alt,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine_win.location = 100;
        let mut storage_win = RecordStorage::new();
        assert_eq!(
            engine_win
                .resume_owned_after_interior_quote::<Dynamic>(input_alt, &mut storage_win, 0)
                .unwrap(),
            false
        );
        assert_eq!(
            engine_win
                .resume_owned_after_interior_quote::<crate::format::Csv>(
                    input_alt,
                    &mut storage_win,
                    0
                )
                .unwrap(),
            false
        );

        // mark_owned_nulls with Mysql
        let mut storage_nulls_m = RecordStorage::new();
        storage_nulls_m.append_field(b"\\N");
        storage_nulls_m.append_field(b"regular");
        let mut engine_m = Engine::from_config(
            b"\\N,regular\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine_m.nulls = Nulls::Mysql;
        engine_m.mark_owned_nulls::<Dynamic>(&mut storage_nulls_m, false);
        assert_eq!(storage_nulls_m.is_null(0), Some(true));
        assert_eq!(storage_nulls_m.is_null(1), Some(false));

        // push_owned_unquoted with Mysql null and max_fields
        let mut storage_nulls_limit = RecordStorage::new();
        storage_nulls_limit.append_field(b"f1");
        let engine_m_limit = Engine::from_config(
            b"\\N\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1)),
        );
        assert!(
            engine_m_limit
                .push_owned_unquoted(b"\\N\n", &mut storage_nulls_limit, 0..2, 0, Nulls::Mysql)
                .is_err()
        );

        // FieldTooLarge in parse_owned_quoted_field
        let mut storage_large = RecordStorage::new();
        let custom_settings = ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 10));
        let mut custom_engine = Engine::from_config(b"\"abc\"\n", custom_settings);
        custom_engine.location = 0;
        assert!(
            custom_engine
                .parse_owned_quoted_field(b"\"abc\"\n", &mut storage_large, 0)
                .is_err()
        );

        // resume_owned_after_interior_quote when quoted_prefix_parser is None
        let custom_dialect = Dialect {
            quote: b'#',
            ..Dialect::default()
        };
        let mut no_prefix_engine = Engine::from_config(
            b"#abc#\n",
            ParserSettings::unheaded(custom_dialect, Limits::DEFAULT),
        );
        let mut no_prefix_storage = RecordStorage::new();
        assert_eq!(
            no_prefix_engine
                .resume_owned_after_interior_quote::<Dynamic>(b"#abc#\n", &mut no_prefix_storage, 0)
                .unwrap(),
            false
        );

        // parse_owned_interior_prefix with interior_prefix_parser = None and owned_parser = Some
        let mut engine_no_interior = Engine::from_config(
            b"a,\"b\",c\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine_no_interior.interior_prefix_parser = None;
        let mut storage_no_interior = RecordStorage::new();
        assert!(
            engine_no_interior
                .parse_owned_interior_prefix::<Dynamic>(
                    b"a,\"b\",c\n",
                    &mut storage_no_interior,
                    0,
                    false
                )
                .unwrap()
        );

        // parse_owned_interior_prefix with interior_prefix_parser = None and owned_parser = None
        let mut engine_none = Engine::from_config(
            b"a,\"b\",c\n",
            ParserSettings::unheaded(custom_dialect, Limits::DEFAULT),
        );
        engine_none.interior_prefix_parser = None;
        let mut storage_none = RecordStorage::new();
        assert_eq!(
            engine_none
                .parse_owned_interior_prefix::<Dynamic>(b"a,\"b\",c\n", &mut storage_none, 0, false)
                .unwrap(),
            false
        );

        // parse_owned_interior_prefix when parse returns None (malformed)
        let mut engine_malformed = Engine::from_config(
            b"a,\"b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut storage_malformed = RecordStorage::new();
        assert_eq!(
            engine_malformed
                .parse_owned_interior_prefix::<Dynamic>(
                    b"a,\"b\n",
                    &mut storage_malformed,
                    0,
                    false
                )
                .unwrap(),
            false
        );

        // parse_owned_interior_prefix with multiple interior quotes returning false
        let mut engine_multi_fail = Engine::from_config(
            b"a,\"b\",c,\"d\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut storage_multi_fail = RecordStorage::new();
        assert_eq!(
            engine_multi_fail
                .parse_owned_interior_prefix::<Dynamic>(
                    b"a,\"b\",c,\"d\n",
                    &mut storage_multi_fail,
                    0,
                    false
                )
                .unwrap(),
            false
        );

        // parse_owned_record with quoting disabled where field starts with quote char
        let mut noq_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        noq_settings.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        noq_settings.format_tag = FormatTag::Custom;
        let mut noq_engine = Engine::from_config(b"\"quoted\"\n", noq_settings.clone());
        let mut noq_storage = RecordStorage::new();
        assert!(
            noq_engine
                .parse_owned_record::<Dynamic>(b"\"quoted\"\n", &mut noq_storage, 0, false)
                .is_ok()
        );

        // finish_owned_quoted_field at EOF (no delimiter or terminator) and with CRLF
        let mut eof_storage = RecordStorage::new();
        let mut eof_engine = Engine::from_config(
            b"\"abc\"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        eof_engine.location = 0;
        assert!(
            eof_engine
                .parse_owned_quoted_field(b"\"abc\"", &mut eof_storage, 0)
                .unwrap()
        );

        let mut crlf_storage = RecordStorage::new();
        let mut crlf_engine = Engine::from_config(
            b"\"abc\"\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        crlf_engine.location = 0;
        assert!(
            crlf_engine
                .parse_owned_quoted_field(b"\"abc\"\r\n", &mut crlf_storage, 0)
                .unwrap()
        );

        // Escaped quotes exceeding max_field_bytes (lines 703-710)
        let mut esc_storage = RecordStorage::new();
        let esc_settings = ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10));
        let mut esc_engine = Engine::from_config(b"\"a\"\"b\"\"c\"\n", esc_settings);
        esc_engine.location = 0;
        assert!(
            esc_engine
                .parse_owned_quoted_field(b"\"a\"\"b\"\"c\"\n", &mut esc_storage, 0)
                .is_err()
        );

        // Quoted head + plain tail + interior quote in parse_owned_record (line 84)
        let q_head_input = b"\"head\",plain,\"second\",tail\n";
        let mut q_head_engine = Engine::from_config(
            q_head_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut q_head_storage = RecordStorage::new();
        assert!(
            q_head_engine
                .parse_owned_record::<Dynamic>(q_head_input, &mut q_head_storage, 0, false)
                .is_ok()
        );
        let mut q_head_engine_csv = Engine::from_config(
            q_head_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut q_head_storage_csv = RecordStorage::new();
        assert!(
            q_head_engine_csv
                .parse_owned_record::<crate::format::Csv>(
                    q_head_input,
                    &mut q_head_storage_csv,
                    0,
                    false
                )
                .is_ok()
        );

        // resume_owned_after_interior_quote when scalar parse returns None (line 261)
        let mut mal_tail_engine = Engine::from_config(
            b"head,\"bad\"x,tail\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        mal_tail_engine.location = 5;
        let mut mal_tail_storage = RecordStorage::new();
        assert_eq!(
            mal_tail_engine
                .resume_owned_after_interior_quote::<Dynamic>(
                    b"head,\"bad\"x,tail\n",
                    &mut mal_tail_storage,
                    0
                )
                .unwrap(),
            false
        );
        assert_eq!(
            mal_tail_engine
                .resume_owned_after_interior_quote::<crate::format::Csv>(
                    b"head,\"bad\"x,tail\n",
                    &mut mal_tail_storage,
                    0
                )
                .unwrap(),
            false
        );

        // parse_owned_record error in resume_owned_after_interior_quote (line 84 Err arm)
        let mut q_err_engine = Engine::from_config(
            b"\"head\",plain,\"second\",third,fourth\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        q_err_engine.limits = Limits::new(100, 100, 3);
        let mut q_err_storage = RecordStorage::new();
        assert!(
            q_err_engine
                .parse_owned_record::<Dynamic>(
                    b"\"head\",plain,\"second\",third,fourth\n",
                    &mut q_err_storage,
                    0,
                    false
                )
                .is_err()
        );
        let mut q_err_engine_csv = Engine::from_config(
            b"\"head\",plain,\"second\",third,fourth\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        q_err_engine_csv.limits = Limits::new(100, 100, 3);
        let mut q_err_storage_csv = RecordStorage::new();
        assert!(
            q_err_engine_csv
                .parse_owned_record::<crate::format::Csv>(
                    b"\"head\",plain,\"second\",third,fourth\n",
                    &mut q_err_storage_csv,
                    0,
                    false
                )
                .is_err()
        );

        // parse_owned_quoted_field with Escape::Mysql (line 643)
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut storage = RecordStorage::new();
            let mut engine = Engine::from_config(
                b"\"abc\"\n",
                ParserSettings::unheaded(Dialect::MYSQL, Limits::DEFAULT),
            );
            engine.location = 0;
            let _ = engine.parse_owned_quoted_field(b"\"abc\"\n", &mut storage, 0);
        }));

        // parse_owned_record with quoted_prefix_parser = None and malformed starting quote (line 98)
        let mut engine_without_qprefix = Engine::from_config(
            b"\"bad\"quote\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine_without_qprefix.quoted_prefix_parser = None;
        let mut storage_without_qprefix = RecordStorage::new();
        assert!(
            engine_without_qprefix
                .parse_owned_record::<Dynamic>(
                    b"\"bad\"quote\n",
                    &mut storage_without_qprefix,
                    0,
                    false
                )
                .is_err()
        );

        // resume_owned_after_interior_quote alternating plain and multiple quotes (line 275)
        let multi_q_input = b"\"first\",plain,\"second\",tail,\"third\",end\n";
        let mut multi_q_engine = Engine::from_config(
            multi_q_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        multi_q_engine.location = 14; // Start of "second"
        let mut multi_q_storage = RecordStorage::new();
        multi_q_storage.append_field(b"first");
        multi_q_storage.append_field(b"plain");
        assert!(
            multi_q_engine
                .resume_owned_after_interior_quote::<Dynamic>(
                    multi_q_input,
                    &mut multi_q_storage,
                    0
                )
                .unwrap()
        );

        // parse_owned_quoted_field with invalid backslash escape (line 717)
        let bslash_dialect = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let mut bslash_engine = Engine::from_config(
            b"\"invalid \\x escape\"\n",
            ParserSettings::unheaded(bslash_dialect, Limits::DEFAULT),
        );
        bslash_engine.location = 0;
        let mut bslash_storage = RecordStorage::new();
        assert!(
            bslash_engine
                .parse_owned_quoted_field(b"\"invalid \\x escape\"\n", &mut bslash_storage, 0)
                .is_err()
        );

        // parse_owned_interior_prefix error propagation and fallback (lines 52, 227)
        let input_pred_err = b"head,\"interior\",extra,tail\n";
        let mut pred_err_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        pred_err_settings.field_count = FieldCount::Exact(3);
        let mut pred_err_engine = Engine::from_config(input_pred_err, pred_err_settings);
        pred_err_engine.interior_quotes = 16;
        let mut pred_err_storage = RecordStorage::new();
        assert!(
            pred_err_engine
                .parse_owned_record::<Dynamic>(input_pred_err, &mut pred_err_storage, 0, false)
                .is_err()
        );

        let mut interior_fail_engine = Engine::from_config(
            b"head,\"interior\",plain,\"bad\"quote\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        interior_fail_engine.interior_prefix_parser = interior_fail_engine.interior_handoff_parser;
        let mut interior_fail_storage = RecordStorage::new();
        assert_eq!(
            interior_fail_engine
                .parse_owned_interior_prefix::<Dynamic>(
                    b"head,\"interior\",plain,\"bad\"quote\n",
                    &mut interior_fail_storage,
                    0,
                    false
                )
                .unwrap(),
            false
        );

        // parse_owned_record::<Csv> with quoted head and malformed interior quote (covers line 84 and line 261)
        let mal_interior_input = b"\"head\",plain,\"bad\"x\n";
        let mut mal_interior_engine = Engine::from_config(
            mal_interior_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut mal_interior_storage = RecordStorage::new();
        assert!(
            mal_interior_engine
                .parse_owned_record::<crate::format::Csv>(
                    mal_interior_input,
                    &mut mal_interior_storage,
                    0,
                    false
                )
                .is_err()
        );

        // parse_owned_record::<Csv> with skip_initial_space
        let mut csv_sp_owned = Engine::from_config(
            b"\"quoted\",  plain\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        csv_sp_owned.skip_initial_space = true;
        let mut sp_storage = RecordStorage::new();
        assert!(
            csv_sp_owned
                .parse_owned_record::<crate::format::Csv>(
                    b"\"quoted\",  plain\n",
                    &mut sp_storage,
                    0,
                    false
                )
                .is_ok()
        );

        // parse_owned_record::<Csv> with Exact field count
        let mut csv_fc_owned = Engine::from_config(
            b"\"a\",b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        csv_fc_owned.field_count = FieldCount::Exact(2);
        let mut fc_storage = RecordStorage::new();
        assert!(
            csv_fc_owned
                .parse_owned_record::<crate::format::Csv>(b"\"a\",b\n", &mut fc_storage, 0, false)
                .is_ok()
        );

        let mut csv_fc_bad = Engine::from_config(
            b"\"a\",b,c\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        csv_fc_bad.field_count = FieldCount::Exact(2);
        let mut fc_bad_storage = RecordStorage::new();
        assert!(
            csv_fc_bad
                .parse_owned_record::<crate::format::Csv>(
                    b"\"a\",b,c\n",
                    &mut fc_bad_storage,
                    0,
                    false
                )
                .is_err()
        );

        // parse_owned_record with record limit in unquoted field
        let mut own_lim_engine = Engine::from_config(
            b"a,b,c\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        let mut lim_storage = RecordStorage::new();
        assert!(
            own_lim_engine
                .parse_owned_record::<crate::format::Csv>(b"a,b,c\n", &mut lim_storage, 0, false)
                .is_err()
        );

        // parse_owned_unquoted_field with header = true
        let mut hdr_engine = Engine::from_config(
            b"col1,col2\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut hdr_storage = RecordStorage::new();
        assert!(
            hdr_engine
                .parse_owned_record::<Dynamic>(b"col1,col2\n", &mut hdr_storage, 0, true)
                .is_ok()
        );

        // skip_delimiter_spaces limit error in general loop of parse_owned_record
        let mut sp_err_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10));
        sp_err_settings.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        sp_err_settings.format_tag = FormatTag::Custom;
        let mut sp_err_engine = Engine::from_config(b"a,   b\n", sp_err_settings);
        sp_err_engine.skip_initial_space = true;
        sp_err_engine.location = 2;
        let mut sp_err_storage = RecordStorage::new();
        assert!(
            sp_err_engine
                .parse_owned_record::<Dynamic>(b"a,   b\n", &mut sp_err_storage, 0, false)
                .is_err()
        );

        // check_record_limit_for at terminator in try_parse_owned_plain_from_mode
        let mut term_lim_engine = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 10, 10)),
        );
        let mut term_lim_storage = RecordStorage::new();
        assert!(
            term_lim_engine
                .try_parse_owned_plain::<Dynamic>(b"a,b\n", &mut term_lim_storage, 0)
                .is_err()
        );

        // check_record_limit_for at EOF in try_parse_owned_plain_from_mode
        let mut eof_lim_engine = Engine::from_config(
            b"a,b",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 10, 10)),
        );
        let mut eof_lim_storage = RecordStorage::new();
        assert!(
            eof_lim_engine
                .try_parse_owned_plain::<Dynamic>(b"a,b", &mut eof_lim_storage, 0)
                .is_err()
        );

        // check_scan_end_for and check_record_limit_for in parse_owned_unquoted_field
        let mut unq_lim_engine = Engine::from_config(
            b"toolongfieldhere\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        unq_lim_engine.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut unq_lim_storage = RecordStorage::new();
        assert!(
            unq_lim_engine
                .parse_owned_unquoted_field(b"toolongfieldhere\n", &mut unq_lim_storage, 0, false)
                .is_err()
        );

        // validate_field_count errors in parse_owned_interior_prefix (lines 205 and 212)
        let mut int_fc_engine = Engine::from_config(
            b"head,\"interior\",tail\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        int_fc_engine.field_count = FieldCount::Exact(2);
        int_fc_engine.interior_quotes = 16;
        let mut int_fc_storage = RecordStorage::new();
        assert!(
            int_fc_engine
                .parse_owned_record::<Dynamic>(
                    b"head,\"interior\",tail\n",
                    &mut int_fc_storage,
                    0,
                    false
                )
                .is_err()
        );

        let mut int_fc_engine2 = Engine::from_config(
            b"head,\"interior\",tail,\"second\",end\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        int_fc_engine2.field_count = FieldCount::Exact(2);
        int_fc_engine2.interior_quotes = 16;
        let mut int_fc_storage2 = RecordStorage::new();
        assert!(
            int_fc_engine2
                .parse_owned_record::<Dynamic>(
                    b"head,\"interior\",tail,\"second\",end\n",
                    &mut int_fc_storage2,
                    0,
                    false
                )
                .is_err()
        );

        // EOF push_owned_unquoted limit error in parse_owned_unquoted_field (line 572)
        let mut eof_unq_engine = Engine::from_config(
            b"toolongfieldwithoutending",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 5, 10)),
        );
        eof_unq_engine.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut eof_unq_storage = RecordStorage::new();
        assert!(
            eof_unq_engine
                .parse_owned_unquoted_field(
                    b"toolongfieldwithoutending",
                    &mut eof_unq_storage,
                    0,
                    false
                )
                .is_err()
        );

        // check_record_limit_for at EOF in finish_owned_quoted_field (line 719)
        let mut eof_q_lim = Engine::from_config(
            b"\"toolong\"",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        eof_q_lim.location = 0;
        let mut eof_q_storage = RecordStorage::new();
        assert!(
            eof_q_lim
                .parse_owned_quoted_field(b"\"toolong\"", &mut eof_q_storage, 0)
                .is_err()
        );

        // check_record_limit_for at CRLF in finish_owned_quoted_field (line 736)
        let mut crlf_q_lim = Engine::from_config(
            b"\"toolong\"\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        crlf_q_lim.location = 0;
        let mut crlf_q_storage = RecordStorage::new();
        assert!(
            crlf_q_lim
                .parse_owned_quoted_field(b"\"toolong\"\r\n", &mut crlf_q_storage, 0)
                .is_err()
        );

        // push_owned_unquoted field_too_large error (line 550)
        let mut unq_field_large_engine = Engine::from_config(
            b"toolong\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 10)),
        );
        unq_field_large_engine.syntax =
            Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut unq_field_large_storage = RecordStorage::new();
        assert!(
            unq_field_large_engine
                .parse_owned_record::<Dynamic>(b"toolong\n", &mut unq_field_large_storage, 0, false)
                .is_err()
        );

        // General loop in parse_owned_record with both quoted and unquoted fields
        let mut custom_general_eng = Engine::from_config(b"\"q\",unq\n", noq_settings);
        custom_general_eng.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(true));
        let mut cg_storage = RecordStorage::new();
        assert!(
            custom_general_eng
                .parse_owned_record::<Dynamic>(b"\"q\",unq\n", &mut cg_storage, 0, false)
                .is_ok()
        );

        // Test fully quoted record through quoted_prefix_parser in parse_owned_record (line 55)
        let all_quoted = b"\"a\",\"b\"\n";
        let mut all_q_engine = Engine::from_config(
            all_quoted,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut all_q_storage = RecordStorage::new();
        assert!(
            all_q_engine
                .parse_owned_record::<Dynamic>(all_quoted, &mut all_q_storage, 0, false)
                .is_ok()
        );

        // skip_initial_space for CSV and TSV in parse_owned_record
        let mut sis_csv_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        sis_csv_settings.skip_initial_space = true;
        let mut sis_own_csv = Engine::from_config(b" a , b \n", sis_csv_settings);
        let mut sis_own_csv_store = RecordStorage::new();
        assert!(
            sis_own_csv
                .parse_owned_record::<crate::format::Csv>(
                    b" a , b \n",
                    &mut sis_own_csv_store,
                    0,
                    false
                )
                .is_ok()
        );

        let mut sis_tsv_settings = ParserSettings::unheaded(Dialect::TSV, Limits::DEFAULT);
        sis_tsv_settings.skip_initial_space = true;
        let mut sis_own_tsv = Engine::from_config(b" a \t b \n", sis_tsv_settings);
        let mut sis_own_tsv_store = RecordStorage::new();
        assert!(
            sis_own_tsv
                .parse_owned_record::<crate::format::Tsv>(
                    b" a \t b \n",
                    &mut sis_own_tsv_store,
                    0,
                    false
                )
                .is_ok()
        );

        // header with nulls != Nulls::None
        let mut null_hdr_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        null_hdr_settings.nulls = Nulls::PostgresCsv;
        let mut null_hdr_eng = Engine::from_config(b"col1,col2\n", null_hdr_settings);
        null_hdr_eng.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut null_hdr_store = RecordStorage::new();
        assert!(
            null_hdr_eng
                .parse_owned_record::<Dynamic>(b"col1,col2\n", &mut null_hdr_store, 0, true)
                .is_ok()
        );

        // Limits error during skip_delimiter_spaces in owned mode
        let mut sis_lim_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10));
        sis_lim_settings.skip_initial_space = true;
        let mut sis_own_lim = Engine::from_config(b"    a\n", sis_lim_settings);
        let mut sis_own_store = RecordStorage::new();
        assert!(
            sis_own_lim
                .parse_owned_record::<Dynamic>(b"    a\n", &mut sis_own_store, 0, false)
                .is_err()
        );

        // Limits error during unquoted field with quote char when quoting is disabled in owned mode
        let mut noq_lim_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10));
        noq_lim_settings.syntax =
            crate::config::Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut noq_own_lim = Engine::from_config(b"\"abcdef\"\n", noq_lim_settings);
        let mut noq_own_store = RecordStorage::new();
        assert!(
            noq_own_lim
                .parse_owned_record::<Dynamic>(b"\"abcdef\"\n", &mut noq_own_store, 0, false)
                .is_err()
        );

        // Limits error on CRLF in unquoted owned field
        let mut unq_crlf_lim = Engine::from_config(
            b"ab\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        let mut unq_crlf_store = RecordStorage::new();
        assert!(
            unq_crlf_lim
                .parse_owned_record::<Dynamic>(b"ab\r\n", &mut unq_crlf_store, 0, false)
                .is_err()
        );

        // Limits error at EOF and CRLF after quote in owned mode
        let mut q_eof_own = Engine::from_config(
            b"\"abc\"",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        let mut q_eof_store = RecordStorage::new();
        assert!(
            q_eof_own
                .parse_owned_record::<Dynamic>(b"\"abc\"", &mut q_eof_store, 0, false)
                .is_err()
        );

        let mut q_crlf_own = Engine::from_config(
            b"\"abc\"\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        let mut q_crlf_store = RecordStorage::new();
        assert!(
            q_crlf_own
                .parse_owned_record::<Dynamic>(b"\"abc\"\r\n", &mut q_crlf_store, 0, false)
                .is_err()
        );

        // General unquoted field limit error before delimiter and EOF
        let mut gen_delim_lim = Engine::from_config(
            b"toolong,b\n",
            ParserSettings::unheaded(Dialect::TSV, Limits::new(100, 3, 10)),
        );
        let mut gen_delim_store = RecordStorage::new();
        assert!(
            gen_delim_lim
                .parse_owned_record::<Dynamic>(b"toolong,b\n", &mut gen_delim_store, 0, false)
                .is_err()
        );

        let mut gen_eof_lim = Engine::from_config(
            b"toolong",
            ParserSettings::unheaded(Dialect::TSV, Limits::new(100, 3, 10)),
        );
        let mut gen_eof_store = RecordStorage::new();
        assert!(
            gen_eof_lim
                .parse_owned_record::<Dynamic>(b"toolong", &mut gen_eof_store, 0, false)
                .is_err()
        );
    }
}

//! The borrowed parse path, which spans fields in the input.

use super::*;
use coseva_unsafe::record::BorrowedQuoted;

struct QuotedFieldConfig {
    escape: Escape,
    quote: u8,
    delimiter: u8,
    delimiter_tail: crate::config::Tail,
    terminator: u8,
    ending_tail: crate::config::Tail,
    record_ending: RecordEnding,
    permits_any_backslash: bool,
    permits_trailing_ws: bool,
}

const DEFAULT_BORROWED_FIELDS: usize = 16;
const DEFAULT_BORROWED_PROBE: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlainFinishKind {
    None,
    Required,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FormatDispatchKind {
    Static,
    Dynamic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnquotedParserKind {
    Runtime,
    General,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OwnedWriteKind {
    Short,
    General,
}

impl Engine {
    #[inline]
    fn default_plain_borrowed_candidate(&self, input: &[u8], record_start: usize) -> bool {
        self.spans.capacity() >= DEFAULT_BORROWED_FIELDS
            && input.len().saturating_sub(record_start) >= DEFAULT_BORROWED_PROBE
    }

    #[inline]
    fn default_quoted_probe(input: &[u8], record_start: usize) -> Option<&[u8]> {
        if input.len().saturating_sub(record_start) < DEFAULT_BORROWED_PROBE {
            return None;
        }
        let end = record_start + DEFAULT_BORROWED_PROBE;
        Some(&input[record_start..end])
    }

    #[inline]
    fn plain_record_candidate<F: CsvFormat>(&self, first: u8) -> bool {
        self.fmt_plain_kernel::<F>()
            && (!self.fmt_quoting_enabled::<F>() || first != self.fmt_quote::<F>())
    }

    #[inline]
    fn plain_finish_kind<F: CsvFormat>(&self) -> PlainFinishKind {
        if self.fmt_record_pass::<F>() {
            PlainFinishKind::Required
        } else {
            PlainFinishKind::None
        }
    }

    #[inline]
    fn format_dispatch_kind<F: CsvFormat>() -> FormatDispatchKind {
        if F::OPTIONS.is_some() {
            FormatDispatchKind::Static
        } else {
            FormatDispatchKind::Dynamic
        }
    }

    #[inline]
    fn borrowed_scan_end(&self, input_len: usize, record_start: usize) -> usize {
        cmp::min(
            input_len,
            record_start.saturating_add(self.limits.max_record_bytes.saturating_add(1)),
        )
    }

    #[inline]
    fn unquoted_parser_kind<F: CsvFormat>(&self) -> UnquotedParserKind {
        if self.fmt_general_parsing::<F>() {
            UnquotedParserKind::General
        } else {
            UnquotedParserKind::Runtime
        }
    }

    #[inline]
    fn owned_write_kind<const SHORT: bool>(field: &[u8]) -> OwnedWriteKind {
        if SHORT && field.len() <= 3 {
            OwnedWriteKind::Short
        } else {
            OwnedWriteKind::General
        }
    }

    #[inline]
    fn resume_after_unquoted_escape(input: &[u8], at: usize) -> usize {
        // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
        if input.get(at + 1).is_some() {
            at + 2
        } else {
            at + 1
        }
    }

    // gamma::skip(fn_value.some, reason = "mutation causes non-termination or unbounded resource use")
    #[inline]
    fn try_parse_default_csv_borrowed(
        &mut self,
        input: &[u8],
        record_start: usize,
    ) -> Option<bool> {
        if self.owned_parser.is_none() {
            return None;
        }
        #[cfg(target_arch = "x86_64")]
        if self.default_plain_borrowed_candidate(input, record_start)
            && let Some(consumed) =
                try_parse_default_borrowed_plain(input, record_start, &mut self.spans)
        {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = record_start + consumed;
            // The marker is consulted only at the window edge.
            if self.location == input.len() {
                self.note_terminated();
            }
            self.fold_plain_record_terminator(record_start, true, b'\n');
            return Some(true);
        }

        if Self::default_quoted_probe(input, record_start)
            .is_some_and(|probe| find1_near(b'"', probe).is_some())
            && let BorrowedQuoted::Parsed {
                consumed,
                terminated,
            } =
                try_parse_default_borrowed_record(input, record_start, &mut self.spans, self.limits)
        {
            self.location = record_start + consumed;
            if terminated {
                self.note_terminated();
            } else {
                self.clear_terminated();
            }
            return Some(true);
        }
        None
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[inline]
    pub(super) fn parse_record<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(logical.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
        if self.plain_record_candidate::<F>(input[record_start])
            && self.try_parse_borrowed_plain::<F>(input, record_start)?
        {
            match self.plain_finish_kind::<F>() {
                PlainFinishKind::None => {}
                PlainFinishKind::Required => {
                    self.finish_plain_record::<F>(input, record_start, header)?;
                }
            }
            self.validate_field_count(input, self.spans.len())?;
            return Ok(());
        }
        // Only records the plain kernel declines get here. Folding dispatch
        // here keeps plain input on its single body, which measured 25
        // instructions per record cheaper.
        match Self::format_dispatch_kind::<F>() {
            FormatDispatchKind::Static => {
                self.parse_record_fields::<F>(input, record_start, header)
            }
            FormatDispatchKind::Dynamic => match self.format_kind {
                FormatKind::Csv => self.parse_record_fields::<Csv>(input, record_start, header),
                FormatKind::Tsv => self.parse_record_fields::<Tsv>(input, record_start, header),
                FormatKind::Other => {
                    self.parse_record_fields::<Dynamic>(input, record_start, header)
                }
            },
        }
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_record_fields<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        loop {
            self.skip_delimiter_spaces::<F>(input, record_start)?;
            let record_end = if input.get(self.location) == Some(&self.fmt_quote::<F>())
                && self.fmt_quoting_enabled::<F>()
            {
                self.parse_quoted_field::<F>(input, record_start)?
            } else {
                self.parse_unquoted_field::<F>(input, record_start, header)?
            };
            if record_end {
                self.mark_null_spans::<F>(input, header);
                self.validate_field_count(input, self.spans.len())?;
                return Ok(());
            }
        }
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[inline]
    fn try_parse_borrowed_plain<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
    ) -> Result<bool, Error> {
        #[cfg(target_arch = "x86_64")]
        if self.is_csv_format::<F>()
            && let Some(res) = self.try_parse_default_csv_borrowed(input, record_start)
        {
            return Ok(res);
        }

        // A limit at least as large as the window can never bite: the record
        // starts inside the window, so `record_start + limit + 1` already
        // exceeds it. Testing that first spares the hot path two saturating
        // additions and a minimum on every record.
        let scan_end = self.borrowed_scan_end(input.len(), record_start);
        let mut structural = StructuralScanner::resume(
            &input[..scan_end],
            self.fmt_delimiter::<F>(),
            self.fmt_quote::<F>(),
            self.fmt_terminator::<F>(),
            record_start,
            self.block_cache,
        );
        let mut field_start = record_start;
        for mut block in &mut structural {
            while let Some((cursor, byte)) = block.next_match() {
                if byte == self.fmt_delimiter::<F>() {
                    // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                    self.push_span(input, Source::Input, field_start..cursor, cursor, false)?;
                    field_start = cursor + 1;
                } else if byte == self.fmt_quote::<F>()
                    && self.fmt_quoting_enabled::<F>()
                    && (cursor == field_start || !self.fmt_permits_unquoted_quotes::<F>())
                {
                    self.spans.clear_spans();
                    // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                    return Ok(false);
                } else if byte == self.fmt_terminator::<F>() {
                    // The one commit point, and so the one place this dialect
                    // can be judged. An escape byte anywhere in the record
                    // means the kernel's answer may be wrong -- it escapes the
                    // next byte, which can be a delimiter the kernel split on,
                    // a terminator it stopped at, or neither, in which case the
                    // field still has to be unescaped rather than borrowed.
                    // Bailing before anything is committed is what keeps this
                    // to a `spans.clear()`, exactly as the quote case above.
                    if let Some(escape) = self.fmt_unquoted_escape::<F>()
                        && find1(escape, &input[record_start..cursor]).is_some()
                    {
                        self.spans.clear_spans();
                        // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                        return Ok(false);
                    }
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    let field_end = if self.fmt_strips_cr::<F>()
                        && cursor > field_start
                        && input[cursor - 1] == b'\r'
                    {
                        cursor - 1
                    } else {
                        cursor
                    };
                    // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                    self.push_span(input, Source::Input, field_start..field_end, cursor, false)?;
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    self.location = cursor + 1;
                    self.note_terminated();
                    self.check_record_limit(input, record_start, self.location)?;
                    self.fold_plain_record::<F>(record_start, true);
                    return Ok(true);
                }
            }
        }
        if scan_end < input.len() {
            return Err(self.error(
                input,
                ErrorKind::RecordTooLarge {
                    limit: self.limits.max_record_bytes,
                },
                scan_end,
            ));
        }
        // The second commit point: a final record the input ends in the middle
        // of, which never reaches the terminator branch above and so needs the
        // same escape test.
        if let Some(escape) = self.fmt_unquoted_escape::<F>()
            && find1(escape, &input[record_start..scan_end]).is_some()
        {
            self.spans.clear_spans();
            // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
            return Ok(false);
        }
        // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
        self.push_span(input, Source::Input, field_start..scan_end, scan_end, false)?;
        self.location = scan_end;
        self.clear_terminated();
        self.check_record_limit(input, record_start, self.location)?;
        self.fold_plain_record::<F>(record_start, false);
        Ok(true)
    }

    /// Re-mark the spans of a plain record whose dialect spells some fields
    /// NULL.
    ///
    /// A pass over the finished record keeps NULL policy out of the kernel.
    /// Inlining the test costs 3.2% on ordinary `Nulls::None` input by
    /// perturbing LLVM's scan-loop inlining.
    ///
    /// Only unquoted fields can be NULL, so a quoted `""` or `\N` stays an
    /// ordinary value. Spans carry that distinction, which is what lets this
    /// run after the fact rather than inside either parser.
    ///
    /// Spans resolving into the scratch buffer are skipped: those fields have
    /// been unescaped, so their bytes are no longer the raw ones the NULL
    /// sentinel is spelled in, and `\\N` would otherwise read as the NULL
    /// `\N`. Their NULL status was already settled from the raw bytes by
    /// [`Self::push_general_unquoted_span`], which is the only thing that
    /// unescapes an unquoted field.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "for a static Nulls::None format the guard folds to nothing, which is the whole point of taking the format as a parameter"
    )]
    fn mark_null_spans<F: CsvFormat>(&mut self, input: &[u8], header: bool) {
        // Ordered so that a format known at compile time to have no NULLs
        // drops the whole call, and a runtime-configured one settles it with
        // one compare rather than reading `header` first.
        let nulls = self.fmt_nulls::<F>();
        if matches!(nulls, Nulls::None) || header {
            return;
        }
        self.mark_null_spans_cold(input, nulls);
    }

    /// Enforce [`RecordEnding::CrLf`] over a record the plain kernel produced.
    ///
    /// The kernel stops at `\n` and strips a `\r` before it for either newline
    /// dialect, so a well-formed `CrLf` record comes out of it already correct.
    /// What is left is the rejection this dialect exists for: a record ended by
    /// a bare `\n`, and a `\r` anywhere other than immediately before that
    /// `\n`. Both are decided by looking at the finished record, which is what
    /// keeps the kernel byte-identical -- the same reason
    /// [`Self::mark_null_spans`] is a pass rather than a test inside the scan.
    ///
    /// One search over the record answers both questions, because a stray `\r`
    /// always precedes the terminator and so is always the earlier complaint,
    /// which is the order the general parser reports them in too.
    #[cold]
    #[inline(never)]
    fn finish_plain_record_runtime(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
        is_crlf: bool,
        nulls: Nulls,
    ) -> Result<(), Error> {
        if is_crlf {
            self.validate_crlf(input, record_start)?;
        }
        if !matches!(nulls, Nulls::None) && !header {
            self.mark_null_spans_cold(input, nulls);
        }
        Ok(())
    }

    #[inline]
    fn finish_plain_record<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<(), Error> {
        self.finish_plain_record_runtime(
            input,
            record_start,
            header,
            self.fmt_record_ending::<F>() == RecordEnding::CrLf,
            self.fmt_nulls::<F>(),
        )
    }

    fn validate_crlf(&self, input: &[u8], record_start: usize) -> Result<(), Error> {
        let end = self.location;
        // A record the kernel ran off the end of has no terminator to judge;
        // one it stopped at ends with the `\n` it stopped on.
        let terminated = end > record_start && input.get(end - 1) == Some(&b'\n');
        let (search_end, lf) = if terminated {
            let lf = end - 1;
            let carriage = lf > record_start && input[lf - 1] == b'\r';
            (if carriage { lf - 1 } else { lf }, Some(lf))
        } else {
            (end, None)
        };
        if let Some(offset) = find1(b'\r', &input[record_start..search_end]) {
            let at = record_start + offset;
            // The general parser blames the field it was scanning, so the
            // stray byte's own field is the one to name -- not the count of
            // fields the kernel went on to finish, which is what the plain
            // `error` helper would use.
            let field = self
                .spans
                .iter()
                .position(|span| span.range().end > at)
                .unwrap_or(0);
            return Err(self.error_for(input, ErrorKind::InvalidRecordEnding(b'\r'), at, field));
        }
        match lf {
            // A record ended by a bare `\n` is blamed on its last field, which
            // is the one the general parser was scanning when it arrived.
            Some(lf) if search_end == lf => {
                let field = self.spans.len().saturating_sub(1);
                Err(self.error_for(input, ErrorKind::InvalidRecordEnding(b'\n'), lf, field))
            }
            _ => Ok(()),
        }
    }

    #[cold]
    fn mark_null_spans_cold(&mut self, input: &[u8], nulls: Nulls) {
        self.spans
            .mark_input_nulls(input, |field| raw_field_is_null(nulls, field));
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_unquoted_field_runtime(
        &mut self,
        input: &[u8],
        record_start: usize,
        delimiter: u8,
        quote: u8,
        terminator: u8,
        quoting_enabled: bool,
        permits_unquoted_quotes: bool,
        strips_cr: bool,
    ) -> Result<bool, Error> {
        let field_start = self.location;
        let scan_end = self.scan_end(input, record_start, field_start);
        let remaining = &input[self.location..scan_end];
        let special = if quoting_enabled && !permits_unquoted_quotes {
            find3_near(delimiter, quote, terminator, remaining)
        } else {
            find2_near(delimiter, terminator, remaining)
        }
        .map(|relative| self.location + relative);

        let Some(at) = special else {
            self.check_scan_end(input, scan_end, record_start, field_start)?;
            self.push_span(input, Source::Input, field_start..scan_end, scan_end, false)?;
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = scan_end;
            self.clear_terminated();
            // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
            return Ok(true);
        };

        let byte = input[at];
        if byte == quote {
            // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
            return Err(self.error(input, ErrorKind::UnexpectedQuote, at));
        }
        let terminated = byte == terminator;
        let field_end = if terminated && strips_cr && at > field_start && input[at - 1] == b'\r' {
            at - 1
        } else {
            at
        };
        self.push_span(input, Source::Input, field_start..field_end, at, false)?;
        self.location = at + 1;
        if terminated {
            self.note_terminated();
        }
        self.check_record_limit(input, record_start, self.location)?;
        Ok(terminated)
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_unquoted_field<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<bool, Error> {
        match self.unquoted_parser_kind::<F>() {
            UnquotedParserKind::General => {
                self.parse_general_unquoted_field(input, record_start, header)
            }
            UnquotedParserKind::Runtime => self.parse_unquoted_field_runtime(
                input,
                record_start,
                self.fmt_delimiter::<F>(),
                self.fmt_quote::<F>(),
                self.fmt_terminator::<F>(),
                self.fmt_quoting_enabled::<F>(),
                self.fmt_permits_unquoted_quotes::<F>(),
                self.fmt_strips_cr::<F>(),
            ),
        }
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    /// General-purpose unquoted field scan for [`RecordEnding::CrLf`], the
    /// escape styles that apply outside quotes, and non-default [`Nulls`].
    ///
    /// Resolve once which extra stop bytes are needed so each dialect class
    /// gets a single-pass loop with only its required needles.
    fn parse_general_unquoted_field(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<bool, Error> {
        if matches!(self.dialect.record_ending, RecordEnding::CrLf)
            || self.dialect.escape.escapes_unquoted()
        {
            self.general_unquoted_field::<true>(input, record_start, header)
        } else {
            self.general_unquoted_field::<false>(input, record_start, header)
        }
    }

    /// The next byte in `remaining` that could end the field being scanned.
    ///
    /// Fold the dialect's extra stop byte into the same comparison chain
    /// rather than making a second pass over the field. Only a `CrLf` dialect
    /// that also escapes outside quotes needs five needles, and that
    /// combination composes the last one here as `also`.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "this is one iteration of the scan loop's body, lifted out for readability only, and must fold back into it"
    )]
    fn find_stop_byte<const EXTRA: bool>(
        &self,
        remaining: &[u8],
        allow_quote_search: bool,
        record_ending: u8,
        extra: u8,
        also: Option<u8>,
    ) -> Option<usize> {
        let found = match (EXTRA, allow_quote_search) {
            (true, true) => find4_near(
                self.dialect.delimiter,
                self.dialect.quote,
                record_ending,
                extra,
                remaining,
            ),
            (true, false) => find3_near(self.dialect.delimiter, record_ending, extra, remaining),
            (false, true) => find3_near(
                self.dialect.delimiter,
                self.dialect.quote,
                record_ending,
                remaining,
            ),
            (false, false) => find2_near(self.dialect.delimiter, record_ending, remaining),
        };
        match also {
            Some(escape) => earliest(found, find1_near(escape, remaining)),
            None => found,
        }
    }

    /// One dialect class of [`Self::parse_general_unquoted_field`].
    ///
    /// `EXTRA` selects whether the scan carries the additional bare-`\r` or
    /// `\` stop byte. The rare dialect that wants both fuses the first and
    /// composes the second.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn general_unquoted_field<const EXTRA: bool>(
        &mut self,
        input: &[u8],
        record_start: usize,
        header: bool,
    ) -> Result<bool, Error> {
        let field_start = self.location;
        let mut cursor = field_start;
        let allow_quote_search =
            self.syntax.quoting_enabled() && !self.syntax.permits_unquoted_quotes();
        let record_ending = self.dialect.record_ending.byte();
        let crlf = self.dialect.record_ending == RecordEnding::CrLf;
        let unquoted_escape = self.dialect.escape.unquoted_byte();
        let extra = if crlf {
            b'\r'
        } else {
            unquoted_escape.unwrap_or(b'\\')
        };
        let both_extras = crlf && unquoted_escape.is_some();
        let bare_lf = self.dialect.record_ending == RecordEnding::Newline;
        // What confirms a separator whose lead byte the scan found. Both are
        // empty for every single-byte dialect, so `confirms` is a constant
        // `true` there and the `width` below is a constant one.
        let delimiter_tail = self.dialect.delimiter_tail();
        let ending_tail = self.dialect.ending_tail();
        loop {
            let scan_end = self.scan_end(input, record_start, field_start);
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(relational.ge_to_gt, reason = "mutation causes non-termination or unbounded resource use")
            if cursor >= scan_end {
                self.check_scan_end(input, scan_end, record_start, field_start)?;
                self.push_general_unquoted_span(input, field_start, scan_end, scan_end, header)?;
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                self.location = scan_end;
                self.clear_terminated();
                return Ok(true);
            }
            let remaining = &input[cursor..scan_end];
            let found = self.find_stop_byte::<EXTRA>(
                remaining,
                allow_quote_search,
                record_ending,
                extra,
                both_extras.then(|| unquoted_escape.unwrap_or(b'\\')),
            );

            let Some(relative) = found else {
                cursor = scan_end;
                continue;
            };
            let at = cursor + relative;
            let byte = input[at];

            if unquoted_escape == Some(byte) {
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                cursor = Self::resume_after_unquoted_escape(input, at);
                continue;
            }
            if allow_quote_search && byte == self.dialect.quote {
                // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                return Err(self.error(input, ErrorKind::UnexpectedQuote, at));
            }
            if crlf && byte == b'\r' {
                if input.get(at + 1) == Some(&b'\n') {
                    self.push_general_unquoted_span(input, field_start, at, at, header)?;
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    self.location = at + 2;
                    self.note_terminated();
                    self.check_record_limit(input, record_start, self.location)?;
                    return Ok(true);
                }
                // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                return Err(self.error(input, ErrorKind::InvalidRecordEnding(b'\r'), at));
            }
            if byte == record_ending {
                if crlf {
                    // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                    return Err(self.error(input, ErrorKind::InvalidRecordEnding(b'\n'), at));
                }
                if !ending_tail.confirms(&input[at + 1..]) {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 1;
                    continue;
                }
                let field_end = if bare_lf && at > field_start && input[at - 1] == b'\r' {
                    at - 1
                } else {
                    at
                };
                self.push_general_unquoted_span(input, field_start, field_end, at, header)?;
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                self.location = at + ending_tail.width();
                self.note_terminated();
                self.check_record_limit(input, record_start, self.location)?;
                return Ok(true);
            }
            debug_assert_eq!(byte, self.dialect.delimiter);
            // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
            if !delimiter_tail.confirms(&input[at + 1..]) {
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                cursor = at + 1;
                continue;
            }
            self.push_general_unquoted_span(input, field_start, at, at, header)?;
            self.location = at + delimiter_tail.width();
            self.check_record_limit(input, record_start, self.location)?;
            return Ok(false);
        }
    }

    /// Finalize a general-path unquoted field, applying NULL detection and
    /// `MySQL` escape decoding.
    ///
    /// [`Nulls::Mysql`] checks the raw field bytes, so headers and escaped
    /// `\\N` are not mistaken for explicit NULLs.
    fn push_general_unquoted_span(
        &mut self,
        input: &[u8],
        field_start: usize,
        field_end: usize,
        at: usize,
        header: bool,
    ) -> Result<(), Error> {
        let raw = &input[field_start..field_end];
        if !header {
            if self.nulls == Nulls::Mysql && raw == b"\\N" {
                return self.push_null_span(input, field_start);
            }
            if self.nulls == Nulls::PostgresCsv && raw.is_empty() {
                return self.push_null_span(input, field_start);
            }
        }
        if let Some(escape) = self.dialect.escape.unquoted_byte()
            && raw.contains(&escape)
        {
            return self.push_unescaped_span(input, escape, field_start, field_end, at);
        }
        self.push_span(input, Source::Input, field_start..field_end, at, false)
    }

    /// Decode the unquoted-field escapes in `field_start..field_end` into
    /// `self.scratch` and push a `Scratch` span.
    ///
    /// A trailing lone escape byte is copied through literally, matching
    /// `MySQL`'s tolerant text-import behavior and Python's, which both treat
    /// an escape with nothing after it as an ordinary byte.
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn push_unescaped_span(
        &mut self,
        input: &[u8],
        escape: u8,
        field_start: usize,
        field_end: usize,
        at: usize,
    ) -> Result<(), Error> {
        let mysql = self.dialect.escape == Escape::Mysql;
        let scratch_start = self.spans.scratch_len();
        let mut segment_start = field_start;
        let mut cursor = field_start;
        while let Some(relative) = find1(escape, &input[cursor..field_end]) {
            let backslash = cursor + relative;
            self.spans
                .scratch_extend_from_slice(&input[segment_start..backslash]);
            if backslash + 1 < field_end {
                let escaped = input[backslash + 1];
                self.spans.scratch_push(if mysql {
                    mysql_unescape_byte(escaped)
                } else {
                    escaped
                });
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                cursor = backslash + 2;
            } else {
                self.spans.scratch_push(escape);
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                cursor = backslash + 1;
            }
            segment_start = cursor;
        }
        self.spans
            .scratch_extend_from_slice(&input[segment_start..field_end]);
        self.check_scratch_limit(input, scratch_start, at)?;
        self.push_span(
            input,
            Source::Scratch,
            scratch_start..self.spans.scratch_len(),
            at,
            false,
        )
    }

    /// Push a zero-length span marking an explicit NULL field.
    ///
    /// NULL fields carry no bytes, so only the field-count limit applies.
    fn push_null_span(&mut self, input: &[u8], at: usize) -> Result<(), Error> {
        if self.spans.len() == self.limits.max_fields {
            return Err(self.error(
                input,
                ErrorKind::TooManyFields {
                    limit: self.limits.max_fields,
                },
                at,
            ));
        }
        self.spans.push_null(at);
        Ok(())
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_quoted_field<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
    ) -> Result<bool, Error> {
        let config = QuotedFieldConfig {
            escape: self.fmt_escape::<F>(),
            quote: self.fmt_quote::<F>(),
            delimiter: self.fmt_delimiter::<F>(),
            delimiter_tail: self.fmt_delimiter_tail::<F>(),
            terminator: self.fmt_terminator::<F>(),
            ending_tail: self.fmt_ending_tail::<F>(),
            record_ending: self.fmt_record_ending::<F>(),
            permits_any_backslash: self.fmt_permits_any_backslash_escape::<F>(),
            permits_trailing_ws: self.fmt_permits_trailing_whitespace::<F>(),
        };
        self.parse_quoted_field_runtime(input, record_start, config)
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parse_quoted_field_runtime(
        &mut self,
        input: &[u8],
        record_start: usize,
        config: QuotedFieldConfig,
    ) -> Result<bool, Error> {
        let field_index = self.spans.len();
        let content_start = self.location + 1;
        let mut segment_start = content_start;
        let mut cursor = content_start;
        let scratch_start = self.spans.scratch_len();
        let mut copied = false;

        loop {
            let scan_end = self.scan_end(input, record_start, content_start);
            // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
            let Some(remaining) = input.get(cursor..scan_end) else {
                let error = self
                    .check_record_limit(input, record_start, scan_end)
                    .expect_err("quoted cursor can overrun only a record-limited scan");
                return Err(error);
            };
            let found = match config.escape {
                Escape::DoubleQuote => find1_near(config.quote, remaining),
                Escape::Backslash(escape) | Escape::Unquoted(escape) => {
                    find2_near(config.quote, escape, remaining)
                }
                Escape::Mysql => find2_near(config.quote, b'\\', remaining),
            }
            .map(|relative| cursor + relative);

            let Some(at) = found else {
                self.check_scan_end(input, scan_end, record_start, content_start)?;
                // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                return Err(self.error(input, ErrorKind::UnterminatedQuotedField, scan_end));
            };
            let byte = input[at];
            match config.escape {
                Escape::DoubleQuote if input.get(at + 1) == Some(&config.quote) => {
                    self.copy_segment(input, &mut copied, content_start, segment_start..at);
                    self.spans.scratch_push(config.quote);
                    self.check_scratch_limit(input, scratch_start, at)?;
                    cursor = at + 2;
                    segment_start = cursor;
                }
                Escape::Backslash(escape) if byte == escape => {
                    let Some(&escaped) = input.get(at + 1) else {
                        // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                        return Err(self.error(input, ErrorKind::InvalidEscape(escape), at));
                    };
                    if escaped != config.quote && escaped != escape && !config.permits_any_backslash
                    {
                        // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                        return Err(self.error(input, ErrorKind::InvalidEscape(escaped), at + 1));
                    }
                    self.copy_segment(input, &mut copied, content_start, segment_start..at);
                    self.spans.scratch_push(escaped);
                    self.check_scratch_limit(input, scratch_start, at)?;
                    cursor = at + 2;
                    segment_start = cursor;
                }
                // The two styles that also escape outside quotes accept any
                // byte after the escape, unlike `Backslash` above. They differ
                // only in whether that byte goes through `MySQL`'s alphabet.
                escape_style if escape_style.unquoted_byte() == Some(byte) => {
                    let Some(&escaped) = input.get(at + 1) else {
                        // gamma::skip(result.err_to_ok, reason = "mutation causes non-termination or unbounded resource use")
                        return Err(self.error(
                            input,
                            ErrorKind::UnterminatedQuotedField,
                            scan_end,
                        ));
                    };
                    self.copy_segment(input, &mut copied, content_start, segment_start..at);
                    self.spans.scratch_push(if escape_style == Escape::Mysql {
                        mysql_unescape_byte(escaped)
                    } else {
                        escaped
                    });
                    self.check_scratch_limit(input, scratch_start, at)?;
                    cursor = at + 2;
                    segment_start = cursor;
                }
                _ => {
                    if copied {
                        // Quote-search bounds prove this segment is in the input.
                        let segment = &input[segment_start..at];
                        self.spans.scratch_extend_from_slice(segment);
                    }
                    let span = if copied {
                        Source::Scratch
                    } else {
                        Source::Input
                    };
                    let range = if copied {
                        scratch_start..self.spans.scratch_len()
                    } else {
                        content_start..at
                    };
                    self.push_span(input, span, range, at, true)?;
                    return self.finish_quoted_field_runtime(input, record_start, at + 1, config);
                }
            }

            debug_assert_eq!(self.spans.len(), field_index);
        }
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn finish_quoted_field_runtime(
        &mut self,
        input: &[u8],
        record_start: usize,
        after_quote: usize,
        config: QuotedFieldConfig,
    ) -> Result<bool, Error> {
        let after_quote =
            self.skip_post_quote_whitespace_runtime(input, after_quote, config.permits_trailing_ws);
        let Some(&next) = input.get(after_quote) else {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = after_quote;
            self.clear_terminated();
            self.check_record_limit(input, record_start, self.location)?;
            return Ok(true);
        };
        let delimiter_tail = config.delimiter_tail;
        if next == config.delimiter && delimiter_tail.confirms(&input[after_quote + 1..]) {
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = after_quote + delimiter_tail.width();
            return Ok(false);
        }
        let ending_tail = config.ending_tail;
        if config.record_ending != RecordEnding::CrLf
            && next == config.terminator
            && ending_tail.confirms(&input[after_quote + 1..])
        {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = after_quote + ending_tail.width();
            self.note_terminated();
            self.check_record_limit(input, record_start, self.location)?;
            return Ok(true);
        }
        if matches!(
            config.record_ending,
            RecordEnding::Newline | RecordEnding::CrLf
        ) && next == b'\r'
            && input.get(after_quote + 1) == Some(&b'\n')
        {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
            self.location = after_quote + 2;
            self.note_terminated();
            self.check_record_limit(input, record_start, self.location)?;
            return Ok(true);
        }
        Err(self.error(
            input,
            ErrorKind::UnexpectedByteAfterQuote(next),
            after_quote,
        ))
    }

    fn skip_post_quote_whitespace_runtime(
        &self,
        input: &[u8],
        mut location: usize,
        permits_trailing_ws: bool,
    ) -> usize {
        if !permits_trailing_ws {
            return location;
        }
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        while matches!(input.get(location), Some(b' ' | b'\t')) {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            location += 1;
        }
        location
    }

    pub(super) fn skip_compatible_post_quote_whitespace<F: CsvFormat>(
        &self,
        input: &[u8],
        location: usize,
    ) -> usize {
        self.skip_post_quote_whitespace_runtime(
            input,
            location,
            self.fmt_permits_trailing_whitespace::<F>(),
        )
    }

    fn copy_segment(
        &mut self,
        input: &[u8],
        copied: &mut bool,
        content_start: usize,
        segment: Range<usize>,
    ) {
        // The offsets come from callers rather than from this function, so the
        // bounds required by the low-level slice helper are asserted here.
        debug_assert!(content_start <= segment.start, "segment precedes content");
        debug_assert!(segment.start <= segment.end, "segment range is inverted");
        debug_assert!(segment.end <= input.len(), "segment overruns the input");

        if !*copied {
            // Quote-search bounds prove the prefix is in the input.
            let prefix = &input[content_start..segment.start];
            self.spans.scratch_extend_from_slice(prefix);
            *copied = true;
        }
        let segment = &input[segment];
        self.spans.scratch_extend_from_slice(segment);
    }

    /// Record one field, unless it would breach a width or count limit.
    ///
    /// The accepting path stays straight-line and rare limit breaches are
    /// outlined to a cold helper. `Range::len` is avoided because callers
    /// already guarantee `start <= end`.
    #[inline]
    fn push_span(
        &mut self,
        input: &[u8],
        source: Source,
        range: Range<usize>,
        at: usize,
        quoted: bool,
    ) -> Result<(), Error> {
        debug_assert!(
            range.end
                <= match source {
                    Source::Input => input.len(),
                    Source::Scratch => self.spans.scratch_len(),
                }
        );
        let pushed = match source {
            Source::Input => self.spans.try_push_input_bounded(
                range,
                quoted,
                self.limits.max_fields,
                self.limits.max_field_bytes,
            ),
            Source::Scratch => self.spans.try_push_scratch_bounded(
                range,
                quoted,
                self.limits.max_fields,
                self.limits.max_field_bytes,
            ),
        };
        if pushed {
            return Ok(());
        }
        Err(self.span_limit_error(input, at))
    }

    /// Build the error for a field that breached a limit in [`Self::push_span`].
    #[cold]
    fn span_limit_error(&self, input: &[u8], at: usize) -> Error {
        let kind = if self.spans.len() >= self.limits.max_fields {
            ErrorKind::TooManyFields {
                limit: self.limits.max_fields,
            }
        } else {
            ErrorKind::FieldTooLarge {
                limit: self.limits.max_field_bytes,
            }
        };
        self.error(input, kind, at)
    }
}

impl Engine {
    #[expect(
        clippy::inline_always,
        reason = "Callgrind shows double-digit instruction savings for short and empty fields"
    )]
    #[inline(always)]
    pub(super) fn push_owned_field(
        &self,
        input: &[u8],
        output: &mut RecordStorage,
        range: Range<usize>,
        at: usize,
    ) -> Result<(), Error> {
        self.push_owned_field_with::<false>(input, output, range, at)
    }

    #[expect(
        clippy::inline_always,
        reason = "the SHORT mode must fold before the structural record loop is emitted"
    )]
    #[inline(always)]
    pub(super) fn push_owned_short_field(
        &self,
        input: &[u8],
        output: &mut RecordStorage,
        range: Range<usize>,
        at: usize,
    ) -> Result<(), Error> {
        self.push_owned_field_with::<true>(input, output, range, at)
    }

    #[expect(
        clippy::inline_always,
        reason = "the SHORT mode must fold before the structural record loop is emitted"
    )]
    #[inline(always)]
    fn push_owned_field_with<const SHORT: bool>(
        &self,
        input: &[u8],
        output: &mut RecordStorage,
        range: Range<usize>,
        at: usize,
    ) -> Result<(), Error> {
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
        // `Range::len` guards against an inverted range with a checked
        // subtraction; every caller builds `start..end` with `start <= end`,
        // so the guard is pure overhead on the hottest field path.
        debug_assert!(
            range.start <= range.end,
            "inverted field range; the unchecked length arithmetic that follows \
             assumes the parser produced start <= end"
        );
        if range.end - range.start > self.limits.max_field_bytes {
            return Err(self.error_for(
                input,
                ErrorKind::FieldTooLarge {
                    limit: self.limits.max_field_bytes,
                },
                at,
                output.len(),
            ));
        }
        let field = &input[range];
        match Self::owned_write_kind::<SHORT>(field) {
            OwnedWriteKind::Short => output.append_short_field(field),
            OwnedWriteKind::General => output.append_field(field),
        }
        Ok(())
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_borrowed_parser_coverage_paths() {
        let esc_dialect = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let esc_settings = ParserSettings::unheaded(esc_dialect, Limits::new(100, 2, 10));

        let mut null_dialect = Dialect::default();
        null_dialect.escape = Escape::Mysql;

        // parse_record<Dynamic> with FormatKind::Csv on quoted record
        let csv_q_input = b"\"csv_quoted\",val\n";
        let mut csv_q_engine = Engine::from_config(
            csv_q_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        csv_q_engine.spans.begin(csv_q_input, csv_q_input.len());
        csv_q_engine.format_kind = FormatKind::Csv;
        assert!(
            csv_q_engine
                .parse_record::<Dynamic>(csv_q_input, 0, false)
                .is_ok()
        );

        // Lone trailing escape in unquoted field
        let esc_tail_input = b"a\\,b\n";
        let mut esc_tail_engine = Engine::from_config(esc_tail_input, esc_settings.clone());
        esc_tail_engine
            .spans
            .begin(esc_tail_input, esc_tail_input.len());
        assert!(
            esc_tail_engine
                .parse_record::<Dynamic>(esc_tail_input, 0, false)
                .is_ok()
        );

        // Lone trailing escape at end of input
        let esc_eof_input = b"a\\";
        let mut esc_eof_engine = Engine::from_config(esc_eof_input, esc_settings.clone());
        esc_eof_engine
            .spans
            .begin(esc_eof_input, esc_eof_input.len());
        assert!(
            esc_eof_engine
                .parse_record::<Dynamic>(esc_eof_input, 0, false)
                .is_ok()
        );

        // Invalid backslash escape in quoted field (strict)
        let bad_esc_input = b"\"a\\xb\"\n";
        let mut bad_esc_engine = Engine::from_config(
            bad_esc_input,
            ParserSettings::unheaded(esc_dialect, Limits::DEFAULT),
        );
        bad_esc_engine
            .spans
            .begin(bad_esc_input, bad_esc_input.len());
        assert!(
            bad_esc_engine
                .parse_record::<Dynamic>(bad_esc_input, 0, false)
                .is_err()
        );

        // Backslash at EOF in quoted field
        let eof_bs_input = b"\"a\\";
        let mut eof_bs_engine = Engine::from_config(
            eof_bs_input,
            ParserSettings::unheaded(esc_dialect, Limits::DEFAULT),
        );
        eof_bs_engine.spans.begin(eof_bs_input, eof_bs_input.len());
        assert!(
            eof_bs_engine
                .parse_record::<Dynamic>(eof_bs_input, 0, false)
                .is_err()
        );

        // MySQL escape at EOF in quoted field
        let mut eof_mysql_engine = Engine::from_config(
            eof_bs_input,
            ParserSettings::unheaded(null_dialect, Limits::DEFAULT),
        );
        eof_mysql_engine
            .spans
            .begin(eof_bs_input, eof_bs_input.len());
        assert!(
            eof_mysql_engine
                .parse_record::<Dynamic>(eof_bs_input, 0, false)
                .is_err()
        );

        // format_kind Tsv & Other in parse_record with Dynamic
        let tsv_input = b"a\t\"b\"\n";
        let mut tsv_settings = ParserSettings::unheaded(Dialect::TSV, Limits::DEFAULT);
        tsv_settings.format_tag = FormatTag::Custom;
        let mut tsv_engine = Engine::from_config(tsv_input, tsv_settings);
        tsv_engine.spans.begin(tsv_input, tsv_input.len());
        tsv_engine.format_kind = FormatKind::Tsv;
        assert!(
            tsv_engine
                .parse_record::<Dynamic>(tsv_input, 0, false)
                .is_ok()
        );

        let other_input = b"a;\"b\"\n";
        let mut other_settings = ParserSettings::unheaded(Dialect::SEMICOLON, Limits::DEFAULT);
        other_settings.format_tag = FormatTag::Custom;
        let mut other_engine = Engine::from_config(other_input, other_settings);
        other_engine.spans.begin(other_input, other_input.len());
        other_engine.format_kind = FormatKind::Other;
        assert!(
            other_engine
                .parse_record::<Dynamic>(other_input, 0, false)
                .is_ok()
        );

        // push_null_span limit error
        let null_input = b"\\N,\\N\n";
        let mut null_dialect = Dialect::default();
        null_dialect.escape = Escape::Mysql;
        let mut null_settings = ParserSettings::unheaded(null_dialect, Limits::new(100, 10, 1));
        null_settings.nulls = Nulls::Mysql;
        null_settings.format_tag = FormatTag::Custom;
        let mut null_engine = Engine::from_config(null_input, null_settings.clone());
        null_engine.spans.begin(null_input, null_input.len());
        assert!(
            null_engine
                .parse_record::<Dynamic>(null_input, 0, false)
                .is_err()
        );

        // push_span with Source::Scratch limit error
        let esc_input = b"\"a\\\"b\\\"c\"\n";
        let esc_dialect = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let esc_settings = ParserSettings::unheaded(esc_dialect, Limits::new(100, 2, 10));
        let mut esc_engine = Engine::from_config(esc_input, esc_settings);
        esc_engine.spans.begin(esc_input, esc_input.len());
        assert!(
            esc_engine
                .parse_record::<Dynamic>(esc_input, 0, false)
                .is_err()
        );

        // finish_quoted_field with trailing whitespace under Compatible syntax
        let py_input = b"\"a\" \t , \"b\"\n";
        let mut py_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        py_settings.syntax =
            Syntax::Compatible(crate::config::Recovery::NONE.trailing_whitespace_after_quote(true));
        let mut py_engine = Engine::from_config(py_input, py_settings);
        py_engine.spans.begin(py_input, py_input.len());
        assert!(
            py_engine
                .parse_record::<Dynamic>(py_input, 0, false)
                .is_ok()
        );

        // Quoting disabled in parse_record_fields
        let mut noq_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        noq_settings.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        noq_settings.format_tag = FormatTag::Custom;
        let mut noq_engine = Engine::from_config(b"\"quoted\"\n", noq_settings);
        noq_engine.spans.begin(b"\"quoted\"\n", 9);
        assert!(
            noq_engine
                .parse_record::<Dynamic>(b"\"quoted\"\n", 0, false)
                .is_ok()
        );

        #[cfg(feature = "multibyte")]
        {
            // general_unquoted_field with multi-byte delimiter and CRLF
            let mb_dialect = Dialect {
                delimiter: b'-',
                delimiter_tail: Tail::of(b"->"),
                record_ending: RecordEnding::CrLf,
                ..Dialect::default()
            };
            let mut mb_engine = Engine::from_config(
                b"field1->field2\r\n",
                ParserSettings::unheaded(mb_dialect, Limits::DEFAULT),
            );
            mb_engine.spans.begin(b"field1->field2\r\n", 16);
            assert!(
                mb_engine
                    .parse_record::<Dynamic>(b"field1->field2\r\n", 0, false)
                    .is_ok()
            );
        }

        // skip_delimiter_spaces error when record limit exceeded
        let mut space_lim_engine = Engine::from_config(
            b"a,          b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(5, 10, 10)),
        );
        space_lim_engine.skip_initial_space = true;
        space_lim_engine.spans.begin(b"a,          b\n", 20);
        assert!(
            space_lim_engine
                .parse_record::<crate::format::Csv>(b"a,          b\n", 0, false)
                .is_err()
        );

        // push_span error at EOF without newline in try_parse_borrowed_plain
        let mut eof_field_lim_engine = Engine::from_config(
            b"toolongfieldwithoutnewline",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 5, 10)),
        );
        eof_field_lim_engine
            .spans
            .begin(b"toolongfieldwithoutnewline", 30);
        assert!(
            eof_field_lim_engine
                .parse_record::<crate::format::Csv>(b"toolongfieldwithoutnewline", 0, false)
                .is_err()
        );

        // general_unquoted_field check_record_limit at delimiter and at record_ending
        let mut gen_rec_delim_lim = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(null_dialect, Limits::new(2, 10, 10)),
        );
        gen_rec_delim_lim.spans.begin(b"a,b\n", 10);
        assert!(
            gen_rec_delim_lim
                .parse_record::<Dynamic>(b"a,b\n", 0, false)
                .is_err()
        );

        let mut gen_rec_end_lim = Engine::from_config(
            b"ab\n",
            ParserSettings::unheaded(null_dialect, Limits::new(2, 10, 10)),
        );
        gen_rec_end_lim.spans.begin(b"ab\n", 10);
        assert!(
            gen_rec_end_lim
                .parse_record::<Dynamic>(b"ab\n", 0, false)
                .is_err()
        );

        // check_scratch_limit in doubled quote
        let mut dquote_engine = Engine::from_config(
            b"\"a\"\"b\"\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 10)),
        );
        dquote_engine.spans.begin(b"\"a\"\"b\"\n", 20);
        assert!(
            dquote_engine
                .parse_record::<crate::format::Csv>(b"\"a\"\"b\"\n", 0, false)
                .is_err()
        );

        // check_record_limit in unquoted field
        let mut rec_lim_engine = Engine::from_config(
            b"a,b,c\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        rec_lim_engine.spans.begin(b"a,b,c\n", 10);
        assert!(
            rec_lim_engine
                .parse_record::<crate::format::Csv>(b"a,b,c\n", 0, false)
                .is_err()
        );

        // finish_plain_record error with CRLF validation
        let crlf_dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut crlf_engine = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(crlf_dialect, Limits::DEFAULT),
        );
        crlf_engine.spans.begin(b"a,b\n", 4);
        assert!(
            crlf_engine
                .parse_record::<Dynamic>(b"a,b\n", 0, false)
                .is_err()
        );

        // validate_field_count error on plain record
        let mut exact_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        exact_settings.field_count = FieldCount::Exact(5);
        let mut exact_engine = Engine::from_config(b"a,b\n", exact_settings);
        exact_engine.spans.begin(b"a,b\n", 4);
        assert!(
            exact_engine
                .parse_record::<Dynamic>(b"a,b\n", 0, false)
                .is_err()
        );

        // check_record_limit in finish_quoted_field (EOF and CRLF)
        let mut q_eof_engine = Engine::from_config(
            b"\"quoted\"",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        q_eof_engine.spans.begin(b"\"quoted\"", 10);
        assert!(
            q_eof_engine
                .parse_record::<Dynamic>(b"\"quoted\"", 0, false)
                .is_err()
        );

        let mut q_crlf_engine = Engine::from_config(
            b"\"quoted\"\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        q_crlf_engine.spans.begin(b"\"quoted\"\r\n", 10);
        assert!(
            q_crlf_engine
                .parse_record::<Dynamic>(b"\"quoted\"\r\n", 0, false)
                .is_err()
        );

        // Stray \r in general_unquoted_field
        let mut stray_r_engine = Engine::from_config(
            b"a\rb\r\n",
            ParserSettings::unheaded(crlf_dialect, Limits::DEFAULT),
        );
        stray_r_engine.spans.begin(b"a\rb\r\n", 6);
        assert!(
            stray_r_engine
                .parse_record::<Dynamic>(b"a\rb\r\n", 0, false)
                .is_err()
        );

        // Scratch limit error in backslash and mysql quoted escapes
        let mut esc_bs_engine = Engine::from_config(
            b"\"a\\\\b\\\\c\"\n",
            ParserSettings::unheaded(esc_dialect, Limits::new(100, 2, 10)),
        );
        esc_bs_engine.spans.begin(b"\"a\\\\b\\\\c\"\n", 20);
        assert!(
            esc_bs_engine
                .parse_record::<Dynamic>(b"\"a\\\\b\\\\c\"\n", 0, false)
                .is_err()
        );

        let mut mysql_q_engine = Engine::from_config(
            b"\"a\\nb\\nc\"\n",
            ParserSettings::unheaded(null_dialect, Limits::new(100, 2, 10)),
        );
        mysql_q_engine.spans.begin(b"\"a\\nb\\nc\"\n", 20);
        assert!(
            mysql_q_engine
                .parse_record::<Dynamic>(b"\"a\\nb\\nc\"\n", 0, false)
                .is_err()
        );

        // Test parse_record with StaticFormat (Csv) on quoted record (F::OPTIONS.is_some())
        let mut static_engine = Engine::from_config(
            b"\"quoted\",val\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        static_engine.spans.begin(b"\"quoted\",val\n", 20);
        assert!(
            static_engine
                .parse_record::<crate::format::Csv>(b"\"quoted\",val\n", 0, false)
                .is_ok()
        );

        // Test parse_record with CRLF plain record exercising finish_plain_record
        let mut crlf_plain_engine = Engine::from_config(
            b"a,b\r\n",
            ParserSettings::unheaded(crlf_dialect, Limits::DEFAULT),
        );
        crlf_plain_engine.spans.begin(b"a,b\r\n", 5);
        assert!(
            crlf_plain_engine
                .parse_record::<Dynamic>(b"a,b\r\n", 0, false)
                .is_ok()
        );

        // Test parse_record with MySQL null plain record exercising finish_plain_record and mark_null_spans_cold
        let mut null_plain_engine = Engine::from_config(
            b"\\N,b\n",
            ParserSettings::unheaded(null_dialect, Limits::DEFAULT),
        );
        null_plain_engine.spans.begin(b"\\N,b\n", 5);
        assert!(
            null_plain_engine
                .parse_record::<Dynamic>(b"\\N,b\n", 0, false)
                .is_ok()
        );

        // Test push_owned_field and push_owned_short_field
        let mut store = RecordStorage::new();
        let norm_eng = Engine::from_config(
            b"abc",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            norm_eng
                .push_owned_field(b"abc", &mut store, 0..3, 3)
                .is_ok()
        );
        assert!(
            norm_eng
                .push_owned_short_field(b"abc", &mut store, 0..3, 3)
                .is_ok()
        );
        assert!(
            norm_eng
                .push_owned_short_field(b"abcd", &mut store, 0..4, 4)
                .is_ok()
        );

        // Limits error on push_owned_field
        let lim_eng = Engine::from_config(
            b"abcdef",
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 2)),
        );
        let mut lim_store = RecordStorage::new();
        assert!(
            lim_eng
                .push_owned_field(b"abcdef", &mut lim_store, 0..5, 5)
                .is_err()
        );
        assert!(
            lim_eng
                .push_owned_field(b"a", &mut lim_store, 0..1, 1)
                .is_ok()
        );
        assert!(
            lim_eng
                .push_owned_field(b"b", &mut lim_store, 0..1, 1)
                .is_ok()
        );
        assert!(
            lim_eng
                .push_owned_field(b"c", &mut lim_store, 0..1, 1)
                .is_err()
        ); // too many fields

        // skip_initial_space in borrowed parser for CSV and TSV
        let mut sis_csv_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        sis_csv_settings.skip_initial_space = true;
        let mut sis_csv_engine = Engine::from_config(b" a , b \n", sis_csv_settings);
        sis_csv_engine.spans.begin(b" a , b \n", 10);
        assert!(
            sis_csv_engine
                .parse_record::<crate::format::Csv>(b" a , b \n", 0, false)
                .is_ok()
        );

        let mut sis_tsv_settings = ParserSettings::unheaded(Dialect::TSV, Limits::DEFAULT);
        sis_tsv_settings.skip_initial_space = true;
        let mut sis_tsv_engine = Engine::from_config(b" a \t b \n", sis_tsv_settings);
        sis_tsv_engine.spans.begin(b" a \t b \n", 10);
        assert!(
            sis_tsv_engine
                .parse_record::<crate::format::Tsv>(b" a \t b \n", 0, false)
                .is_ok()
        );

        // Header record with quoted field
        let mut hdr_q_engine = Engine::from_config(
            b"\"quoted_header\",h2\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        hdr_q_engine.spans.begin(b"\"quoted_header\",h2\n", 20);
        assert!(
            hdr_q_engine
                .parse_record::<crate::format::Csv>(b"\"quoted_header\",h2\n", 0, true)
                .is_ok()
        );

        // Limits error during skip_delimiter_spaces
        let mut sis_lim_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10));
        sis_lim_settings.skip_initial_space = true;
        let mut sis_lim_eng = Engine::from_config(b"    a\n", sis_lim_settings);
        sis_lim_eng.spans.begin(b"    a\n", 10);
        assert!(
            sis_lim_eng
                .parse_record::<Dynamic>(b"    a\n", 0, false)
                .is_err()
        );

        // Limits error during unquoted field with quote char when quoting is disabled
        let mut no_q_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 10));
        no_q_settings.syntax =
            crate::config::Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let mut no_q_eng = Engine::from_config(b"\"abcdef\"\n", no_q_settings);
        no_q_eng.spans.begin(b"\"abcdef\"\n", 10);
        assert!(
            no_q_eng
                .parse_record::<Dynamic>(b"\"abcdef\"\n", 0, false)
                .is_err()
        );

        // Limits errors in general unquoted before CRLF
        let mut tsv_crlf_lim = Engine::from_config(
            b"abcdef\r\n",
            ParserSettings::unheaded(Dialect::TSV, Limits::new(100, 2, 10)),
        );
        tsv_crlf_lim.spans.begin(b"abcdef\r\n", 10);
        assert!(
            tsv_crlf_lim
                .parse_record::<Dynamic>(b"abcdef\r\n", 0, false)
                .is_err()
        );

        let mut tsv_crlf_rec_lim = Engine::from_config(
            b"ab\r\n",
            ParserSettings::unheaded(Dialect::TSV, Limits::new(2, 100, 10)),
        );
        tsv_crlf_rec_lim.spans.begin(b"ab\r\n", 10);
        assert!(
            tsv_crlf_rec_lim
                .parse_record::<Dynamic>(b"ab\r\n", 0, false)
                .is_err()
        );

        // Limits error at EOF and CRLF after quote
        let mut q_eof_lim = Engine::from_config(
            b"\"abc\"",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        q_eof_lim.spans.begin(b"\"abc\"", 10);
        assert!(
            q_eof_lim
                .parse_record::<Dynamic>(b"\"abc\"", 0, false)
                .is_err()
        );

        let mut q_crlf_lim = Engine::from_config(
            b"\"abc\"\r\n",
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 10)),
        );
        q_crlf_lim.spans.begin(b"\"abc\"\r\n", 10);
        assert!(
            q_crlf_lim
                .parse_record::<Dynamic>(b"\"abc\"\r\n", 0, false)
                .is_err()
        );
    }

    fn engine_for(input: &[u8], settings: ParserSettings) -> Engine {
        let mut engine = Engine::from_config(input, settings);
        assert!(engine.spans.begin(input, input.len()));
        engine
    }

    fn fields(engine: &Engine, input: &[u8]) -> Vec<Vec<u8>> {
        let spans = engine.spans.resolved(input);
        (0..spans.len())
            .map(|index| spans.get(index).unwrap().to_vec())
            .collect()
    }

    fn points_into(field: &[u8], input: &[u8]) -> bool {
        let start = input.as_ptr() as usize;
        let end = start + input.len();
        let field = field.as_ptr() as usize;
        (start..end).contains(&field)
    }

    #[test]
    fn default_borrowed_gate_observes_capacity_length_and_record_start() {
        let mut short = vec![b'a'; 63];
        short[62] = b'\n';
        let mut engine = engine_for(
            &short,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.spans.reserve(16);
        assert_eq!(engine.try_parse_default_csv_borrowed(&short, 0), None);
        assert_eq!(engine.location, 0);

        let mut full = vec![b'a'; 64];
        full[63] = b'\n';
        let mut no_capacity = engine_for(
            &full,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        no_capacity.spans = SpanStorage::with_capacity(0);
        assert!(no_capacity.spans.begin(&full, full.len()));
        assert_eq!(no_capacity.try_parse_default_csv_borrowed(&full, 0), None);

        let mut disabled = engine_for(
            &full,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        disabled.spans.reserve(16);
        disabled.owned_parser = None;
        assert_eq!(disabled.try_parse_default_csv_borrowed(&full, 0), None);

        let prefix = b"skip:";
        let mut prefixed = prefix.to_vec();
        prefixed.extend_from_slice(&full);
        let mut engine = engine_for(
            &prefixed,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.spans.reserve(16);
        engine.folded_upto = prefix.len();
        assert_eq!(
            engine.try_parse_default_csv_borrowed(&prefixed, prefix.len()),
            Some(true)
        );
        assert_eq!(engine.location, prefixed.len());
        assert!(engine.terminated);
        assert_eq!(engine.folded_upto, prefixed.len());
        assert_eq!(engine.folded_lines, 1);
        assert_eq!(fields(&engine, &prefixed), [vec![b'a'; 63]]);

        let mut quoted = vec![b'x'; 64];
        quoted[..6].copy_from_slice(b"\"a\",b\n");
        let mut engine = engine_for(
            &quoted,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            engine.try_parse_default_csv_borrowed(&quoted, 0),
            Some(true)
        );
        assert_eq!(engine.location, 6);
        assert!(engine.terminated);
        assert_eq!(fields(&engine, &quoted), [b"a".to_vec(), b"b".to_vec()]);
    }

    #[test]
    fn plain_records_preserve_fields_boundaries_and_borrowed_identity() {
        let cases: &[(&[u8], &[&[u8]], usize, bool)] = &[
            (b"a\n", &[b"a"], 2, true),
            (b"a,b", &[b"a", b"b"], 3, false),
            (b",\n", &[b"", b""], 2, true),
            (b"a,b\r\n", &[b"a", b"b"], 5, true),
        ];

        for &(input, expected, location, terminated) in cases {
            let mut engine = engine_for(
                input,
                ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
            );
            engine.parse_record::<Dynamic>(input, 0, false).unwrap();

            assert_eq!(engine.location, location, "{input:?}");
            assert_eq!(engine.terminated, terminated, "{input:?}");
            assert_eq!(
                fields(&engine, input),
                expected
                    .iter()
                    .map(|field| field.to_vec())
                    .collect::<Vec<_>>(),
                "{input:?}"
            );
            let spans = engine.spans.resolved(input);
            for index in 0..spans.len() {
                assert!(
                    points_into(spans.get(index).unwrap(), input),
                    "plain field {index} in {input:?} must borrow the input"
                );
            }
        }
    }

    #[test]
    fn general_unquoted_parsing_decodes_escapes_nulls_and_crlf() {
        let mut mysql = Dialect::default();
        mysql.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(mysql, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let input = b"a\\,b,\\N,x\\t\n";
        let mut engine = engine_for(input, settings.clone());
        engine.parse_record::<Dynamic>(input, 0, false).unwrap();
        assert_eq!(engine.location, input.len());
        assert!(engine.terminated);
        assert_eq!(
            fields(&engine, input),
            [b"a,b".to_vec(), Vec::new(), b"x\t".to_vec()]
        );
        let spans = engine.spans.resolved(input);
        assert!(!points_into(spans.get(0).unwrap(), input));
        assert_eq!(spans.is_null(0), Some(false));
        assert_eq!(spans.is_null(1), Some(true));
        assert_eq!(spans.is_null(2), Some(false));

        let mut header = engine_for(b"\\N\n", settings);
        header.parse_record::<Dynamic>(b"\\N\n", 0, true).unwrap();
        assert_eq!(fields(&header, b"\\N\n"), [b"N".to_vec()]);
        assert_eq!(header.spans.resolved(b"\\N\n").is_null(0), Some(false));

        let crlf = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        for (input, expected, offset, field) in [
            (
                b"a\rb\r\n".as_slice(),
                ErrorKind::InvalidRecordEnding(b'\r'),
                1,
                0,
            ),
            (
                b"a,b\n".as_slice(),
                ErrorKind::InvalidRecordEnding(b'\n'),
                3,
                1,
            ),
        ] {
            let mut engine = engine_for(input, ParserSettings::unheaded(crlf, Limits::DEFAULT));
            let error = engine.parse_record::<Dynamic>(input, 0, false).unwrap_err();
            assert_eq!(error.kind(), expected);
            assert_eq!(error.location().byte, offset);
            assert_eq!(error.location().field, field);
        }

        let input = b"a,b\r\n";
        let mut engine = engine_for(input, ParserSettings::unheaded(crlf, Limits::DEFAULT));
        engine.parse_record::<Dynamic>(input, 0, false).unwrap();
        assert_eq!(fields(&engine, input), [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(engine.location, input.len());
        assert!(engine.terminated);
    }

    #[test]
    fn quoted_fields_cover_borrowing_decoding_boundaries_and_errors() {
        let cases = [
            (
                Dialect::default(),
                b"\"alpha\",z\n".as_slice(),
                b"alpha".as_slice(),
                true,
            ),
            (
                Dialect::default(),
                b"\"a\"\"b\",z\n".as_slice(),
                b"a\"b".as_slice(),
                false,
            ),
            (
                Dialect {
                    escape: Escape::Backslash(b'\\'),
                    ..Dialect::default()
                },
                b"\"a\\\"b\",z\n".as_slice(),
                b"a\"b".as_slice(),
                false,
            ),
            (
                Dialect {
                    escape: Escape::Mysql,
                    ..Dialect::default()
                },
                b"\"a\\nb\",z\n".as_slice(),
                b"a\nb".as_slice(),
                false,
            ),
        ];

        for (dialect, input, expected, borrowed) in cases {
            let mut engine = engine_for(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
            engine.parse_record::<Dynamic>(input, 0, false).unwrap();
            let spans = engine.spans.resolved(input);
            assert_eq!(spans.get(0), Some(expected));
            assert_eq!(spans.get(1), Some(b"z".as_slice()));
            assert_eq!(points_into(spans.get(0).unwrap(), input), borrowed);
            assert_eq!(engine.location, input.len());
            assert!(engine.terminated);
        }

        let strict_backslash = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let errors = [
            (
                Dialect::default(),
                b"\"unterminated".as_slice(),
                ErrorKind::UnterminatedQuotedField,
                13,
                0,
            ),
            (
                strict_backslash,
                b"\"a\\".as_slice(),
                ErrorKind::InvalidEscape(b'\\'),
                2,
                0,
            ),
            (
                strict_backslash,
                b"\"a\\x\"".as_slice(),
                ErrorKind::InvalidEscape(b'x'),
                3,
                0,
            ),
            (
                Dialect::default(),
                b"\"a\"x".as_slice(),
                ErrorKind::UnexpectedByteAfterQuote(b'x'),
                3,
                1,
            ),
        ];

        for (dialect, input, expected, offset, field) in errors {
            let mut engine = engine_for(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
            let error = engine.parse_record::<Dynamic>(input, 0, false).unwrap_err();
            assert_eq!(error.kind(), expected, "{input:?}");
            assert_eq!(error.location().byte, offset, "{input:?}");
            assert_eq!(error.location().field, field, "{input:?}");
        }
    }

    #[test]
    fn post_quote_whitespace_and_owned_short_writes_are_bounded() {
        let input = b"\"a\" \t,b\n";
        let strict = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut engine = engine_for(input, strict);
        let error = engine.parse_record::<Dynamic>(input, 0, false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedByteAfterQuote(b' '));
        assert_eq!(error.location().byte, 3);

        let mut compatible = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        compatible.syntax = Syntax::Compatible(
            crate::config::Recovery::default().trailing_whitespace_after_quote(true),
        );
        let mut engine = engine_for(input, compatible);
        assert_eq!(
            engine.skip_compatible_post_quote_whitespace::<Dynamic>(input, 3),
            5
        );
        engine.parse_record::<Dynamic>(input, 0, false).unwrap();
        assert_eq!(fields(&engine, input), [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(engine.location, input.len());

        let input = b"012345";
        let engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        for len in 0..=5 {
            let mut output = RecordStorage::new();
            engine
                .push_owned_short_field(input, &mut output, 0..len, len)
                .unwrap();
            assert_eq!(output.len(), 1);
            assert_eq!(output.get(0), Some(&input[..len]));
            assert_eq!(output.bytes_len(), len);
        }
    }

    #[cfg(feature = "multibyte")]
    #[test]
    fn multibyte_separator_tails_confirm_complete_boundaries_only() {
        let dialect = Dialect {
            delimiter: b'-',
            delimiter_tail: Tail::of(b"->"),
            record_ending: RecordEnding::Byte(b'!'),
            ending_tail: Tail::of(b"!!"),
            ..Dialect::default()
        };
        let settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);

        let input = b"a-b->\"c-d\"!!ignored";
        let mut engine = engine_for(input, settings.clone());
        engine.parse_record::<Dynamic>(input, 0, false).unwrap();
        assert_eq!(fields(&engine, input), [b"a-b".to_vec(), b"c-d".to_vec()]);
        assert_eq!(engine.location, 12);
        assert!(engine.terminated);

        for (input, expected) in [
            (
                b"a->b!".as_slice(),
                [b"a".as_slice(), b"b!".as_slice()].as_slice(),
            ),
            (b"a-".as_slice(), [b"a-".as_slice()].as_slice()),
        ] {
            let mut engine = engine_for(input, settings.clone());
            engine.parse_record::<Dynamic>(input, 0, false).unwrap();
            assert_eq!(
                fields(&engine, input),
                expected
                    .iter()
                    .map(|field| field.to_vec())
                    .collect::<Vec<_>>()
            );
            assert_eq!(engine.location, input.len());
            assert!(!engine.terminated);
        }
    }

    fn quoted_config(dialect: Dialect, permits_trailing_ws: bool) -> QuotedFieldConfig {
        QuotedFieldConfig {
            escape: dialect.escape,
            quote: dialect.quote,
            delimiter: dialect.delimiter,
            delimiter_tail: dialect.delimiter_tail(),
            terminator: dialect.record_ending.byte(),
            ending_tail: dialect.ending_tail(),
            record_ending: dialect.record_ending,
            permits_any_backslash: false,
            permits_trailing_ws,
        }
    }

    #[test]
    fn borrowed_path_selection_helpers_have_exact_boundaries() {
        let input = vec![b'x'; 80];
        let mut engine = engine_for(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );

        for (capacity, expected) in [(1, false), (15, false), (16, true)] {
            engine.spans = SpanStorage::with_capacity(capacity);
            assert_eq!(
                engine.default_plain_borrowed_candidate(&input[..64], 0),
                expected,
                "capacity {capacity}"
            );
        }
        assert!(!engine.default_plain_borrowed_candidate(&input[..63], 0));
        assert!(engine.default_plain_borrowed_candidate(&input[..69], 5));
        assert!(!engine.default_plain_borrowed_candidate(&input[..68], 5));

        assert_eq!(Engine::default_quoted_probe(&input[..63], 0), None);
        let probe = Engine::default_quoted_probe(&input, 5).unwrap();
        assert_eq!(probe.len(), DEFAULT_BORROWED_PROBE);
        assert_eq!(probe.as_ptr(), input[5..].as_ptr());
        assert_eq!(
            Engine::default_quoted_probe(&input, 16).unwrap().len(),
            DEFAULT_BORROWED_PROBE
        );
        assert_eq!(Engine::default_quoted_probe(&input, 17), None);

        let default = Engine::from_config(
            b"\"x\"\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(!default.plain_record_candidate::<Dynamic>(b'"'));
        assert!(default.plain_record_candidate::<Dynamic>(b'x'));
        assert_eq!(
            default.plain_finish_kind::<Dynamic>(),
            PlainFinishKind::None
        );
        assert_eq!(
            Engine::format_dispatch_kind::<Csv>(),
            FormatDispatchKind::Static
        );
        assert_eq!(
            Engine::format_dispatch_kind::<Dynamic>(),
            FormatDispatchKind::Dynamic
        );

        let mut no_quotes = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        no_quotes.syntax = Syntax::Compatible(crate::config::Recovery::NONE.quoting(false));
        let engine = Engine::from_config(b"\"x\"\n", no_quotes);
        assert!(engine.plain_record_candidate::<Dynamic>(b'"'));

        let mut mysql = Dialect::default();
        mysql.escape = Escape::Mysql;
        let mut settings = ParserSettings::unheaded(mysql, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        settings.format_tag = FormatTag::Custom;
        let engine = Engine::from_config(b"\\N\n", settings);
        assert_eq!(
            engine.plain_finish_kind::<Dynamic>(),
            PlainFinishKind::Required
        );

        let limited = Engine::from_config(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(5, 100, 100)),
        );
        assert_eq!(limited.borrowed_scan_end(80, 2), 8);
        assert_eq!(limited.borrowed_scan_end(5, 2), 5);

        for (short, len, expected) in [
            (false, 0, false),
            (false, 3, false),
            (true, 0, true),
            (true, 2, true),
            (true, 3, true),
            (true, 4, false),
        ] {
            let field = vec![b'x'; len];
            let actual = if short {
                Engine::owned_write_kind::<true>(&field)
            } else {
                Engine::owned_write_kind::<false>(&field)
            };
            assert_eq!(
                actual,
                if expected {
                    OwnedWriteKind::Short
                } else {
                    OwnedWriteKind::General
                },
                "short={short}, len={len}"
            );
        }
    }

    #[test]
    fn optimized_plain_parser_advances_without_marking_an_internal_boundary() {
        let mut input = vec![b'x'; 64];
        input[..2].copy_from_slice(b"a\n");
        let mut engine = engine_for(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.spans.reserve(DEFAULT_BORROWED_FIELDS);

        assert_eq!(engine.try_parse_default_csv_borrowed(&input, 0), Some(true));
        assert_eq!(engine.location, 2);
        assert!(!engine.terminated);
        assert_eq!(fields(&engine, &input), [b"a".to_vec()]);
    }

    #[test]
    fn prefixed_records_keep_all_absolute_offsets_and_fold_boundaries() {
        let input = b"skip:a,b\nrest";
        let start = 5;
        let mut engine = engine_for(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.location = start;
        engine.folded_upto = start;
        engine.parse_record::<Dynamic>(input, start, false).unwrap();
        assert_eq!(fields(&engine, input), [b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(engine.location, 9);
        assert_eq!(engine.folded_upto, 9);
        assert_eq!(engine.folded_lines, 1);

        let quoted = b"skip:\"a\"\"b\",z\n";
        let mut engine = engine_for(
            quoted,
            ParserSettings::unheaded(
                Dialect {
                    delimiter: b';',
                    ..Dialect::default()
                },
                Limits::DEFAULT,
            ),
        );
        engine.location = start;
        let error = engine
            .parse_record::<Dynamic>(quoted, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedByteAfterQuote(b','));
        assert_eq!(error.location().byte, 11);
        assert_eq!(error.location().field, 1);
    }

    #[test]
    fn plain_kernel_quote_and_crlf_decisions_are_directly_observable() {
        let input = b"a\"b\n";
        let mut strict = engine_for(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            !strict
                .try_parse_borrowed_plain::<Dynamic>(input, 0)
                .unwrap()
        );
        assert_eq!(strict.spans.len(), 0);

        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        settings.syntax = Syntax::Compatible(crate::config::Recovery::NONE.unquoted_quotes(true));
        settings.format_tag = FormatTag::Custom;
        let mut compatible = engine_for(input, settings);
        assert!(
            compatible
                .try_parse_borrowed_plain::<Dynamic>(input, 0)
                .unwrap()
        );
        assert_eq!(fields(&compatible, input), [b"a\"b".to_vec()]);

        let input = b"a\rb";
        let crlf = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut engine = engine_for(input, ParserSettings::unheaded(crlf, Limits::DEFAULT));
        assert!(engine.spans.try_push_input_bounded(0..1, false, 4, 10));
        assert!(engine.spans.try_push_input_bounded(1..3, false, 4, 10));
        engine.location = input.len();
        let error = engine.validate_crlf(input, 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRecordEnding(b'\r'));
        assert_eq!(error.location().byte, 1);
        assert_eq!(error.location().field, 1);

        let empty = b"";
        let engine = engine_for(empty, ParserSettings::unheaded(crlf, Limits::DEFAULT));
        assert!(engine.validate_crlf(empty, 0).is_ok());
    }

    #[test]
    fn general_parser_preserves_prefixed_escapes_nulls_and_style() {
        let start = 5;
        let mut mysql = Dialect::default();
        mysql.escape = Escape::Mysql;
        let mut mysql_settings = ParserSettings::unheaded(mysql, Limits::DEFAULT);
        mysql_settings.nulls = Nulls::Mysql;
        mysql_settings.format_tag = FormatTag::Custom;
        let input = b"skip:a\\,b,\\N,x\\t\n";
        let mut engine = engine_for(input, mysql_settings.clone());
        engine.location = start;
        engine.parse_record::<Dynamic>(input, start, false).unwrap();
        assert_eq!(
            fields(&engine, input),
            [b"a,b".to_vec(), Vec::new(), b"x\t".to_vec()]
        );
        assert_eq!(engine.location, input.len());
        assert!(engine.terminated);
        let spans = engine.spans.resolved(input);
        assert_eq!(spans.is_null(1), Some(true));

        let postgres_input = b",x\n";
        let mut postgres_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        postgres_settings.nulls = Nulls::PostgresCsv;
        postgres_settings.format_tag = FormatTag::Custom;
        let mut postgres = engine_for(postgres_input, postgres_settings.clone());
        postgres
            .parse_record::<Dynamic>(postgres_input, 0, false)
            .unwrap();
        assert_eq!(
            postgres.spans.resolved(postgres_input).is_null(0),
            Some(true)
        );
        let mut header = engine_for(postgres_input, postgres_settings);
        header
            .parse_record::<Dynamic>(postgres_input, 0, true)
            .unwrap();
        assert_eq!(
            header.spans.resolved(postgres_input).is_null(0),
            Some(false)
        );

        let escaped = b"a\\nb\n";
        let mut unquoted_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Unquoted(b'\\'),
                ..Dialect::default()
            },
            Limits::DEFAULT,
        );
        unquoted_settings.format_tag = FormatTag::Custom;
        let mut unquoted = engine_for(escaped, unquoted_settings);
        unquoted.parse_record::<Dynamic>(escaped, 0, false).unwrap();
        assert_eq!(fields(&unquoted, escaped), [b"anb".to_vec()]);

        let mut mysql = engine_for(escaped, mysql_settings);
        mysql.parse_record::<Dynamic>(escaped, 0, false).unwrap();
        assert_eq!(fields(&mysql, escaped), [b"a\nb".to_vec()]);
    }

    #[test]
    fn quoted_runtime_exposes_copy_ranges_and_finish_states() {
        let input = b"xx\"a\\\"b\";z\n";
        let dialect = Dialect {
            delimiter: b';',
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let mut engine = engine_for(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        engine.location = 2;
        assert!(
            !engine
                .parse_quoted_field_runtime(input, 2, quoted_config(dialect, false))
                .unwrap()
        );
        assert_eq!(engine.location, 9);
        assert_eq!(fields(&engine, input), [b"a\"b".to_vec()]);
        assert_eq!(engine.spans.scratch_len(), 3);

        let borrowed_input = b"\"alpha\";z\n";
        let borrowed_dialect = Dialect {
            delimiter: b';',
            ..Dialect::default()
        };
        let mut borrowed = engine_for(
            borrowed_input,
            ParserSettings::unheaded(borrowed_dialect, Limits::DEFAULT),
        );
        borrowed
            .parse_record::<Dynamic>(borrowed_input, 0, false)
            .unwrap();
        assert_eq!(borrowed.spans.scratch_len(), 0);
        assert!(points_into(
            borrowed.spans.resolved(borrowed_input).get(0).unwrap(),
            borrowed_input
        ));

        let segment_input = b"xxabcdef";
        let mut segment = engine_for(
            segment_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut copied = false;
        segment.copy_segment(segment_input, &mut copied, 2, 4..6);
        assert!(copied);
        assert_eq!(segment.spans.scratch_len(), 4);
        assert!(segment.spans.try_push_scratch_bounded(0..4, false, 1, 4));
        assert_eq!(
            segment.spans.resolved(segment_input).get(0),
            Some(b"abcd".as_slice())
        );

        for (input, after_quote, expected_end, terminated, record_end) in [
            (b"xx".as_slice(), 2, 2, false, true),
            (b"xx,z".as_slice(), 2, 3, false, false),
            (b"xx\n".as_slice(), 2, 3, true, true),
            (b"xx\r\n".as_slice(), 2, 4, true, true),
        ] {
            let mut engine = engine_for(
                input,
                ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
            );
            let actual = engine
                .finish_quoted_field_runtime(
                    input,
                    0,
                    after_quote,
                    quoted_config(Dialect::default(), false),
                )
                .unwrap();
            assert_eq!(actual, record_end, "{input:?}");
            assert_eq!(engine.location, expected_end, "{input:?}");
            assert_eq!(engine.terminated, terminated, "{input:?}");
        }
    }

    #[test]
    fn third_round_probe_candidate_and_scan_boundaries_are_exact() {
        let input = vec![b'x'; DEFAULT_BORROWED_PROBE + 5];
        let mut engine = engine_for(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.spans = SpanStorage::with_capacity(DEFAULT_BORROWED_FIELDS);
        assert!(engine.default_plain_borrowed_candidate(&input[..DEFAULT_BORROWED_PROBE], 0));
        assert!(!engine.default_plain_borrowed_candidate(&input[..DEFAULT_BORROWED_PROBE - 1], 0));
        assert_eq!(
            Engine::default_quoted_probe(&input, 5).unwrap(),
            &input[5..5 + DEFAULT_BORROWED_PROBE]
        );

        let whole = Engine::from_config(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(input.len(), 100, 100)),
        );
        assert_eq!(whole.borrowed_scan_end(input.len(), 5), input.len());

        let limited = Engine::from_config(
            &input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(10, 100, 100)),
        );
        assert_eq!(limited.borrowed_scan_end(input.len(), 5), 16);
    }

    #[test]
    fn third_round_prefixed_tsv_dispatch_and_space_limit_use_record_start() {
        let tsv_input = b"xx\"a\"\tb\n";
        let start = 2;
        let mut tsv = engine_for(
            tsv_input,
            ParserSettings::unheaded(Dialect::TSV, Limits::new(5, 100, 100)),
        );
        tsv.location = start;
        let error = tsv
            .parse_record::<Dynamic>(tsv_input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 5 });
        assert_eq!(error.location().byte, tsv_input.len());

        let spaced_input = b"xxa,  b\n";
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 100));
        settings.skip_initial_space = true;
        settings.format_tag = FormatTag::Custom;
        let mut spaced = engine_for(spaced_input, settings);
        spaced.location = start;
        let error = spaced
            .parse_record_fields::<Dynamic>(spaced_input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(error.location().byte, 6);
        assert_eq!(error.location().field, 1);
    }

    #[test]
    fn third_round_plain_escape_scan_excludes_the_terminator() {
        let input = b"xxa\n";
        let start = 2;
        let mut settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Unquoted(b'\n'),
                ..Dialect::default()
            },
            Limits::DEFAULT,
        );
        settings.format_tag = FormatTag::Custom;
        let mut engine = engine_for(input, settings);
        engine.location = start;

        assert!(
            engine
                .try_parse_borrowed_plain::<Dynamic>(input, start)
                .unwrap()
        );
        assert_eq!(fields(&engine, input), [b"a".to_vec()]);
        assert_eq!(engine.location, input.len());
        assert!(engine.terminated);
    }

    #[test]
    fn final_plain_eof_dispatch_null_and_crlf_values_are_exact() {
        let prefixed = b"xxa\n";
        let start = 2;
        let mut runtime = engine_for(
            prefixed,
            ParserSettings::unheaded(Dialect::default(), Limits::new(1, 100, 100)),
        );
        runtime.location = start;
        let error = runtime
            .parse_record_fields::<Dynamic>(prefixed, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, prefixed.len());

        let eof = b"xxabc";
        let mut limited = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        limited.location = start;
        let error = limited
            .try_parse_borrowed_plain::<Dynamic>(eof, start)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 2 });
        assert_eq!(error.location().byte, eof.len());

        let mut folded = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        folded.location = start;
        folded.folded_upto = start;
        assert!(
            folded
                .try_parse_borrowed_plain::<Dynamic>(eof, start)
                .unwrap()
        );
        assert_eq!(folded.folded_upto, eof.len());
        assert_eq!(folded.folded_lines, 0);

        let empty = b"";
        let mut null_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        null_settings.nulls = Nulls::PostgresCsv;
        null_settings.format_tag = FormatTag::Custom;
        let mut header = engine_for(empty, null_settings);
        assert!(header.spans.try_push_input_bounded(0..0, false, 1, 0));
        header.mark_null_spans::<Dynamic>(empty, true);
        assert_eq!(header.spans.resolved(empty).is_null(0), Some(false));

        let cr = b"\r";
        let mut crlf = engine_for(
            cr,
            ParserSettings::unheaded(
                Dialect {
                    record_ending: RecordEnding::CrLf,
                    ..Dialect::default()
                },
                Limits::DEFAULT,
            ),
        );
        crlf.location = 1;
        let error = crlf.validate_crlf(cr, 0).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidRecordEnding(b'\r'));
        assert_eq!(error.location().field, 0);

        let default = engine_for(
            b"a\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            default.plain_finish_kind::<Dynamic>(),
            PlainFinishKind::None
        );
        assert_eq!(
            Engine::format_dispatch_kind::<Csv>(),
            FormatDispatchKind::Static
        );
        assert_eq!(
            Engine::format_dispatch_kind::<Dynamic>(),
            FormatDispatchKind::Dynamic
        );
    }

    #[test]
    fn final_runtime_unquoted_bounds_offsets_and_flags_are_exact() {
        let start = 2;
        let long = b"xxabcd";
        let mut scan_bound = engine_for(
            long,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        scan_bound.location = start;
        let error = scan_bound
            .parse_unquoted_field_runtime(long, start, b',', b'"', b'\n', true, false, true)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let eof = b"xxabc";
        let mut checked = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 0)),
        );
        checked.location = start;
        let error = checked
            .parse_unquoted_field_runtime(eof, start, b',', b'"', b'\n', true, false, true)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });

        let plain = b"xxab";
        let mut flags = engine_for(
            plain,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        flags.location = start;
        assert!(
            flags
                .parse_unquoted_field_runtime(plain, start, b',', b'"', b'\n', true, false, true)
                .unwrap()
        );
        assert!(!flags.spans.iter().next().unwrap().is_quoted());

        let delimiter = b"xxabc,";
        let mut offset = engine_for(
            delimiter,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        offset.location = start;
        let error = offset
            .parse_unquoted_field_runtime(delimiter, start, b',', b'"', b'\n', true, false, true)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let split = b"xxa,b";
        let mut terminated = engine_for(
            split,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        terminated.location = start;
        assert!(
            !terminated
                .parse_unquoted_field_runtime(split, start, b',', b'"', b'\n', true, false, true)
                .unwrap()
        );
        assert!(!terminated.terminated);
        assert_eq!(
            terminated.unquoted_parser_kind::<Dynamic>(),
            UnquotedParserKind::Runtime
        );
    }

    #[test]
    fn final_general_dispatch_scan_bounds_and_quote_policy_are_exact() {
        let start = 2;
        let input = b"xxa\n";

        let mut mysql_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Mysql,
                ..Dialect::default()
            },
            Limits::new(1, 100, 100),
        );
        mysql_settings.format_tag = FormatTag::Custom;
        let mut extra = engine_for(input, mysql_settings);
        extra.location = start;
        let error = extra
            .parse_general_unquoted_field(input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, input.len());
        assert_eq!(
            extra.unquoted_parser_kind::<Dynamic>(),
            UnquotedParserKind::General
        );

        let mut postgres_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(1, 100, 100));
        postgres_settings.nulls = Nulls::PostgresCsv;
        postgres_settings.format_tag = FormatTag::Custom;
        let mut no_extra = engine_for(input, postgres_settings.clone());
        no_extra.location = start;
        let error = no_extra
            .parse_general_unquoted_field(input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, input.len());

        let quote = b"xxa\"b\n";
        let mut quote_settings = postgres_settings;
        quote_settings.limits = Limits::DEFAULT;
        let mut strict = engine_for(quote, quote_settings);
        strict.location = start;
        let error = strict
            .general_unquoted_field::<false>(quote, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
        assert_eq!(error.location().byte, 3);

        let record_bound = b"xxabcdef";
        let mut record = engine_for(
            record_bound,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        record.location = start;
        let error = record
            .general_unquoted_field::<false>(record_bound, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let mut field = engine_for(
            record_bound,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        field.location = start;
        let error = field
            .general_unquoted_field::<false>(record_bound, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let eof = b"xxabc";
        let mut checked_record = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        checked_record.location = start;
        let error = checked_record
            .general_unquoted_field::<false>(eof, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 2 });

        let mut checked_field = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 0)),
        );
        checked_field.location = start;
        let error = checked_field
            .general_unquoted_field::<false>(eof, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });

        assert_eq!(Engine::resume_after_unquoted_escape(b"a\\b", 1), 3);
        assert_eq!(Engine::resume_after_unquoted_escape(b"a\\", 1), 2);
    }

    #[test]
    fn final_general_special_bytes_use_exact_offsets_and_record_extents() {
        let start = 2;

        let crlf_field = b"xxabc\r\n";
        let crlf = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut field = engine_for(
            crlf_field,
            ParserSettings::unheaded(crlf, Limits::new(100, 2, 100)),
        );
        field.location = start;
        let error = field
            .general_unquoted_field::<true>(crlf_field, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let crlf_record = b"xxa\r\n";
        let mut record = engine_for(
            crlf_record,
            ParserSettings::unheaded(crlf, Limits::new(2, 100, 100)),
        );
        record.location = start;
        let error = record
            .general_unquoted_field::<true>(crlf_record, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 2 });
        assert_eq!(error.location().byte, crlf_record.len());

        let mut record_exact = engine_for(
            crlf_record,
            ParserSettings::unheaded(crlf, Limits::new(3, 100, 100)),
        );
        record_exact.location = start;
        assert!(
            record_exact
                .general_unquoted_field::<true>(crlf_record, start, false)
                .unwrap()
        );

        let terminated_field = b"xxabc\n";
        let mut field = engine_for(
            terminated_field,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        field.location = start;
        let error = field
            .general_unquoted_field::<false>(terminated_field, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let terminated_record = b"xxa\n";
        let mut record = engine_for(
            terminated_record,
            ParserSettings::unheaded(Dialect::default(), Limits::new(1, 100, 100)),
        );
        record.location = start;
        let error = record
            .general_unquoted_field::<false>(terminated_record, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, terminated_record.len());

        let mut record_exact = engine_for(
            terminated_record,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        record_exact.location = start;
        assert!(
            record_exact
                .general_unquoted_field::<false>(terminated_record, start, false)
                .unwrap()
        );

        let delimiter_field = b"xxabc,";
        let mut field = engine_for(
            delimiter_field,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        field.location = start;
        let error = field
            .general_unquoted_field::<false>(delimiter_field, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let delimiter_record = b"xxa,b";
        let mut record = engine_for(
            delimiter_record,
            ParserSettings::unheaded(Dialect::default(), Limits::new(1, 100, 100)),
        );
        record.location = start;
        let error = record
            .general_unquoted_field::<false>(delimiter_record, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, 4);

        let mut record_exact = engine_for(
            delimiter_record,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        record_exact.location = start;
        assert!(
            !record_exact
                .general_unquoted_field::<false>(delimiter_record, start, false)
                .unwrap()
        );
        assert_eq!(record_exact.location, 4);

        #[cfg(feature = "multibyte")]
        {
            let ending = Dialect {
                record_ending: RecordEnding::Byte(b'!'),
                ending_tail: Tail::of(b"!XY"),
                ..Dialect::default()
            };
            let input = b"a!!XY";
            let mut engine = engine_for(input, ParserSettings::unheaded(ending, Limits::DEFAULT));
            assert!(
                engine
                    .general_unquoted_field::<false>(input, 0, false)
                    .unwrap()
            );
            assert_eq!(fields(&engine, input), [b"a!".to_vec()]);
            assert_eq!(engine.location, input.len());

            let delimiter = Dialect {
                delimiter: b'-',
                delimiter_tail: Tail::of(b"-XY"),
                ..Dialect::default()
            };
            let input = b"a--XYb\n";
            let mut engine =
                engine_for(input, ParserSettings::unheaded(delimiter, Limits::DEFAULT));
            engine.parse_record::<Dynamic>(input, 0, false).unwrap();
            assert_eq!(fields(&engine, input), [b"a-".to_vec(), b"b".to_vec()]);
            assert_eq!(engine.location, input.len());
        }
    }

    #[test]
    fn final_general_span_null_escape_and_limit_offsets_are_exact() {
        let mysql_input = b"xx\\N";
        let mut mysql_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Mysql,
                ..Dialect::default()
            },
            Limits::new(100, 100, 0),
        );
        mysql_settings.nulls = Nulls::Mysql;
        mysql_settings.format_tag = FormatTag::Custom;
        let mut mysql = engine_for(mysql_input, mysql_settings);
        let error = mysql
            .push_general_unquoted_span(mysql_input, 2, 4, 4, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 2);

        let empty = b"xx";
        let mut postgres_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0));
        postgres_settings.nulls = Nulls::PostgresCsv;
        postgres_settings.format_tag = FormatTag::Custom;
        let mut postgres = engine_for(empty, postgres_settings);
        let error = postgres
            .push_general_unquoted_span(empty, 2, 2, 2, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 2);

        let mut postgres_settings =
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1));
        postgres_settings.nulls = Nulls::PostgresCsv;
        postgres_settings.format_tag = FormatTag::Custom;
        let mut postgres = engine_for(empty, postgres_settings);
        postgres
            .push_general_unquoted_span(empty, 2, 2, 2, false)
            .unwrap();
        assert_eq!(postgres.spans.resolved(empty).is_null(0), Some(true));
        assert_eq!(postgres.spans.iter().next().unwrap().range(), 2..2);

        let escaped = b"xxa\\b";
        let mut escape_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Unquoted(b'\\'),
                ..Dialect::default()
            },
            Limits::new(100, 1, 100),
        );
        escape_settings.format_tag = FormatTag::Custom;
        let mut scratch_limit = engine_for(escaped, escape_settings);
        let error = scratch_limit
            .push_general_unquoted_span(escaped, 2, 5, 5, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 1 });
        assert_eq!(error.location().byte, 5);

        let mut escape_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Unquoted(b'\\'),
                ..Dialect::default()
            },
            Limits::new(100, 100, 0),
        );
        escape_settings.format_tag = FormatTag::Custom;
        let mut span_limit = engine_for(escaped, escape_settings);
        let error = span_limit
            .push_general_unquoted_span(escaped, 2, 5, 5, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let mut escape_settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Unquoted(b'\\'),
                ..Dialect::default()
            },
            Limits::DEFAULT,
        );
        escape_settings.format_tag = FormatTag::Custom;
        let mut escaped_ok = engine_for(escaped, escape_settings);
        escaped_ok
            .push_general_unquoted_span(escaped, 2, 5, 5, false)
            .unwrap();
        assert_eq!(fields(&escaped_ok, escaped), [b"ab".to_vec()]);
        assert!(!escaped_ok.spans.iter().next().unwrap().is_quoted());

        let plain = b"xxabc";
        let mut plain_limit = engine_for(
            plain,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 100)),
        );
        let error = plain_limit
            .push_general_unquoted_span(plain, 2, 5, 5, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
        assert_eq!(error.location().byte, 5);

        let mut null_limit = engine_for(
            empty,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0)),
        );
        let error = null_limit.push_null_span(empty, 2).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 2);

        let mut null_ok = engine_for(
            empty,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 1)),
        );
        null_ok.push_null_span(empty, 2).unwrap();
        assert_eq!(null_ok.spans.iter().next().unwrap().range(), 2..2);
    }

    #[test]
    fn final_quoted_scan_scratch_span_and_record_offsets_are_exact() {
        let start = 2;
        let prefixed = b"xx\"abc";
        let mut record_bound = engine_for(
            prefixed,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 100)),
        );
        record_bound.location = start;
        let error = record_bound
            .parse_quoted_field_runtime(prefixed, start, quoted_config(Dialect::default(), false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(error.location().byte, prefixed.len());

        let unterminated = b"\"abc";
        let mut content_bound = engine_for(
            unterminated,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 3, 100)),
        );
        let error = content_bound
            .parse_quoted_field_runtime(unterminated, 0, quoted_config(Dialect::default(), false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::UnterminatedQuotedField);
        assert_eq!(error.location().byte, unterminated.len());

        let doubled = b"\"a\"\"b\"";
        let mut doubled_limit = engine_for(
            doubled,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 1, 100)),
        );
        let error = doubled_limit
            .parse_quoted_field_runtime(doubled, 0, quoted_config(Dialect::default(), false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 1 });
        assert_eq!(error.location().byte, 2);

        let backslash_dialect = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::default()
        };
        let escaped = b"\"a\\\"b\"";
        let mut backslash_limit = engine_for(
            escaped,
            ParserSettings::unheaded(backslash_dialect, Limits::new(100, 1, 100)),
        );
        let error = backslash_limit
            .parse_quoted_field_runtime(escaped, 0, quoted_config(backslash_dialect, false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 1 });
        assert_eq!(error.location().byte, 2);

        let unquoted_dialect = Dialect {
            escape: Escape::Unquoted(b'\\'),
            ..Dialect::default()
        };
        let escaped_text = b"\"a\\n\"";
        let mut unquoted = engine_for(
            escaped_text,
            ParserSettings::unheaded(unquoted_dialect, Limits::DEFAULT),
        );
        assert!(
            unquoted
                .parse_quoted_field_runtime(
                    escaped_text,
                    0,
                    quoted_config(unquoted_dialect, false),
                )
                .unwrap()
        );
        assert_eq!(fields(&unquoted, escaped_text), [b"an".to_vec()]);

        let mut unquoted_limit = engine_for(
            escaped_text,
            ParserSettings::unheaded(unquoted_dialect, Limits::new(100, 1, 100)),
        );
        let error = unquoted_limit
            .parse_quoted_field_runtime(escaped_text, 0, quoted_config(unquoted_dialect, false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 1 });
        assert_eq!(error.location().byte, 2);

        let mut span_limit = engine_for(
            escaped,
            ParserSettings::unheaded(backslash_dialect, Limits::new(100, 100, 0)),
        );
        let error = span_limit
            .parse_quoted_field_runtime(escaped, 0, quoted_config(backslash_dialect, false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let eof = b"xx\"a\"";
        let mut eof_limit = engine_for(
            eof,
            ParserSettings::unheaded(Dialect::default(), Limits::new(2, 100, 100)),
        );
        eof_limit.location = start;
        let error = eof_limit
            .parse_quoted_field_runtime(eof, start, quoted_config(Dialect::default(), false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 2 });
        assert_eq!(error.location().byte, eof.len());

        let terminated = b"xx\"a\"\n";
        let mut terminated_limit = engine_for(
            terminated,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 100, 100)),
        );
        terminated_limit.location = start;
        let error = terminated_limit
            .parse_quoted_field_runtime(terminated, start, quoted_config(Dialect::default(), false))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(error.location().byte, terminated.len());

        let mut terminated_exact = engine_for(
            terminated,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 100)),
        );
        terminated_exact.location = start;
        assert!(
            terminated_exact
                .parse_quoted_field_runtime(
                    terminated,
                    start,
                    quoted_config(Dialect::default(), false),
                )
                .unwrap()
        );
        assert_eq!(terminated_exact.location, terminated.len());
    }

    #[test]
    fn round_four_dispatch_span_offsets_and_crlf_extent_are_exact() {
        let start = 2;
        let input = b"xxa\n";
        let mut settings = ParserSettings::unheaded(
            Dialect {
                escape: Escape::Mysql,
                ..Dialect::default()
            },
            Limits::new(1, 100, 100),
        );
        settings.format_tag = FormatTag::Custom;
        let mut dispatched = engine_for(input, settings);
        dispatched.location = start;
        let error = dispatched
            .parse_unquoted_field::<Dynamic>(input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 1 });
        assert_eq!(error.location().byte, input.len());

        let runtime_input = b"xxabc,";
        let mut runtime = engine_for(
            runtime_input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0)),
        );
        runtime.location = start;
        let error = runtime
            .parse_unquoted_field_runtime(
                runtime_input,
                start,
                b',',
                b'"',
                b'\n',
                true,
                false,
                true,
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let crlf_input = b"xxabc\r\n";
        let crlf = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut crlf_field = engine_for(
            crlf_input,
            ParserSettings::unheaded(crlf, Limits::new(100, 100, 0)),
        );
        crlf_field.location = start;
        let error = crlf_field
            .general_unquoted_field::<true>(crlf_input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let terminated_input = b"xxabc\n";
        let mut terminated = engine_for(
            terminated_input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0)),
        );
        terminated.location = start;
        let error = terminated
            .general_unquoted_field::<false>(terminated_input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let delimiter_input = b"xxabc,";
        let mut delimiter = engine_for(
            delimiter_input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 100, 0)),
        );
        delimiter.location = start;
        let error = delimiter
            .general_unquoted_field::<false>(delimiter_input, start, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 0 });
        assert_eq!(error.location().byte, 5);

        let quoted_crlf = b"xx\"a\"\r\n";
        let mut too_long = engine_for(
            quoted_crlf,
            ParserSettings::unheaded(Dialect::default(), Limits::new(4, 100, 100)),
        );
        too_long.location = start;
        let error = too_long
            .parse_quoted_field_runtime(
                quoted_crlf,
                start,
                quoted_config(Dialect::default(), false),
            )
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 4 });
        assert_eq!(error.location().byte, quoted_crlf.len());

        let mut exact = engine_for(
            quoted_crlf,
            ParserSettings::unheaded(Dialect::default(), Limits::new(5, 100, 100)),
        );
        exact.location = start;
        assert!(
            exact
                .parse_quoted_field_runtime(
                    quoted_crlf,
                    start,
                    quoted_config(Dialect::default(), false),
                )
                .unwrap()
        );
        assert_eq!(exact.location, quoted_crlf.len());
    }

    #[test]
    fn round_four_quoted_record_limit_stops_before_the_next_field() {
        let input = b"\"a\",\"b\"\n";
        let mut engine = engine_for(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 8, 8)),
        );
        let error = engine.parse_record::<Dynamic>(input, 0, false).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 3 });
        assert_eq!(error.location().byte, 4);
        assert_eq!(error.location().field, 1);
    }
}

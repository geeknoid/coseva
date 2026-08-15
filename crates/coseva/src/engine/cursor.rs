//! Cursor and window movement, including rewind and staging.

use super::*;

#[cfg_attr(test, derive(Clone, Copy))]
struct WindowScanConfig {
    quoting: bool,
    delimiter: u8,
    delimiter_tail: crate::config::Tail,
    quote: u8,
    terminator: u8,
    ending_tail: crate::config::Tail,
    crlf: bool,
    escape: Escape,
    permits_unquoted_quotes: bool,
    permits_any_backslash: bool,
    permits_trailing_ws: bool,
    skip_initial_space: bool,
    max_record: usize,
    max_field: usize,
}

impl Engine {
    #[inline]
    fn prepare_advance(&mut self, input: &[u8]) -> Result<(), Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        self.ensure_headers(input)?;
        Ok(())
    }

    fn resolve_filter_column(&mut self, column: &Column) -> Option<usize> {
        match column {
            Column::Index(index) => Some(*index),
            Column::Name(name) => {
                let name = name.as_bytes();
                if let cached @ Some(_) = self.cached_filter_column(name) {
                    cached
                } else {
                    let column = self.header_slots(name).map(HeaderSlots::first);
                    if let Some(column) = column {
                        self.store_filter_column(name, column);
                    }
                    column
                }
            }
        }
    }

    /// Move to the next record without parsing it.
    ///
    /// Returns `false` after EOF. Views parse lazily, so syntax errors are
    /// reported when a view materializes the record.
    ///
    /// # Errors
    ///
    /// Returns a parse error from header discovery or a prior parser failure.
    #[inline]
    pub(crate) fn advance<F: CsvFormat>(&mut self, input: &[u8]) -> Result<bool, Error> {
        self.prepare_advance(input)?;
        self.settle::<F>(input)?;
        Ok(self.position(input))
    }

    /// Move to the next record satisfying `predicate`, skipping the rest.
    ///
    /// The literal is SIMD-scanned in raw input, but candidates are still fully
    /// parsed before acceptance, so results match filtering [`Self::advance`].
    /// Missing headers match no records, and the accepted record is already
    /// parsed.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    pub(crate) fn advance_with_filter<F: CsvFormat>(
        &mut self,
        input: &[u8],
        predicate: &Predicate,
    ) -> Result<bool, Error> {
        self.prepare_advance(input)?;
        self.settle::<F>(input)?;
        self.cursor_start = NO_OFFSET;

        let Some(column) = self.resolve_filter_column(predicate.column()) else {
            return Ok(false);
        };

        let literal = self.skip_literal_for::<F>(predicate);
        match literal {
            Some(literal) => {
                self.advance_with_literal_filter_loop::<F>(input, predicate, column, literal)
            }
            None => self.advance_with_scanning_filter_loop::<F>(input, predicate, column),
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the literal-present specialization must eliminate unavailable skip-record work from the candidate loop"
    )]
    fn advance_with_literal_filter_loop<F: CsvFormat>(
        &mut self,
        input: &[u8],
        predicate: &Predicate,
        column: usize,
        literal: &[u8],
    ) -> Result<bool, Error> {
        // The offset of a located candidate we have not yet parsed past.
        let mut pending: Option<usize> = Default::default();
        loop {
            if self.skip_toward_literal(input, literal, &mut pending) == input.len() {
                return Ok(false);
            }
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            if self.filter_candidate_matches::<F>(input, predicate, column)? {
                return Ok(true);
            }
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the no-literal filter loop is hot and this preserves the prior monomorphized candidate body"
    )]
    fn advance_with_scanning_filter_loop<F: CsvFormat>(
        &mut self,
        input: &[u8],
        predicate: &Predicate,
        column: usize,
    ) -> Result<bool, Error> {
        loop {
            self.skip_ignored_records(input);
            if self.location == input.len() {
                return Ok(false);
            }
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            if self.filter_candidate_matches::<F>(input, predicate, column)? {
                return Ok(true);
            }
        }
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "both filter loops call this once per candidate and rely on it folding into their hot path"
    )]
    fn filter_candidate_matches<F: CsvFormat>(
        &mut self,
        input: &[u8],
        predicate: &Predicate,
        column: usize,
    ) -> Result<bool, Error> {
        // Failure was checked before the loop and any parse error returns
        // immediately, so the general fill helper's per-candidate poison
        // check would be redundant here.
        let (range, index) = self.parse_positioned_record::<F>(input, false)?;
        let field = self.spans.resolved(input).get(column);
        if !predicate.matches_field(field) {
            return Ok(false);
        }

        // Use the parsed record start: skipped comments and blank records may
        // have moved past the earlier offset.
        self.cursor_start = range.start;
        self.cursor_index = index;
        self.cursor_end = range.end;
        self.staged_valid ^= self.staged_valid;
        Ok(true)
    }

    /// Skip ahead toward the next occurrence of `literal`, the same shortcut
    /// [`Self::advance_with_filter`] takes inline.
    ///
    /// Exposed separately for the push front end, which interleaves the skip
    /// with its own per-chunk absorb/borrow bookkeeping rather than owning
    /// the whole filter loop the way the pull front ends do. Only ever moves
    /// `self.location` past a record ending [`Self::seek_candidate`] has
    /// already proven complete, so it is safe to call between any two calls
    /// to [`Self::advance`] regardless of whether more input may still
    /// arrive — except before headers are known, when a call this early would
    /// have nothing to tell the still-unconsumed header record apart from a
    /// rejected data one, and could skip straight over it. The pull front
    /// ends never reach that state because they call `ensure_headers` before
    /// this shortcut runs at all, so the caller checks it instead.
    ///
    /// `pending` is the caller's own loop-local memo of the last located
    /// candidate, reset once per outer filter call exactly as the local of
    /// the same name in [`Self::advance_with_filter`] is; `filter_backoff`
    /// persists on the engine across calls so repeated scans that skip
    /// nothing still back off.
    pub(crate) fn skip_toward_literal(
        &mut self,
        input: &[u8],
        literal: &[u8],
        pending: &mut Option<usize>,
    ) -> usize {
        if !self.headers_initialized {
            return self.location;
        }
        match *pending {
            Some(hit) if self.location <= hit => {}
            _ if self.filter_backoff > 0 => self.filter_backoff -= 1,
            _ => {
                let (hit, skipped) = self.seek_candidate(input, literal);
                self.filter_backoff = if skipped { 0 } else { FILTER_BACKOFF };
                *pending = Some(hit);
            }
        }
        self.location
    }

    /// Borrow one field of the current record without building a [`Record`].
    ///
    /// The io and push filter loops test a single column, so they need the
    /// span table but not the [`Record`] wrapper built over it. Resolving the
    /// one span directly is what this saves; the spans themselves still have
    /// to be materialized, since the field's extent is not known before that.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::advance`] has not reported a record.
    ///
    /// # Errors
    ///
    /// Returns a positioned error for malformed input or exceeded limits.
    #[inline]
    pub(crate) fn field<'a, F: CsvFormat>(
        &'a mut self,
        input: &'a [u8],
        column: usize,
    ) -> Result<Option<&'a [u8]>, Error> {
        self.materialize_full::<F>(input)?;
        Ok(self.spans.get(input, column))
    }

    /// Consume a record that was advanced to but never viewed.
    ///
    /// Skipping only needs the record extent, not full materialization.
    #[inline]
    pub(crate) fn settle<F: CsvFormat>(&mut self, input: &[u8]) -> Result<(), Error> {
        // Re-parse only when the cursor still points at an untouched record.
        if self.location == self.cursor_start {
            self.record_index = self.cursor_index;
            let _ = self.parse_positioned_record::<F>(input, false)?;
        }
        Ok(())
    }

    /// Place the cursor on the next record, reporting whether one exists.
    pub(crate) fn position(&mut self, input: &[u8]) -> bool {
        if self.skips_records {
            // gamma::skip(logical.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
            if !self.resume.ignored && self.resume.record_start <= input.len() {
                // A refused data record may sit after comments or blank lines.
                // Rewinding restores the pre-skip cursor, but the checkpoint
                // proves exactly where that same data record starts.
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // #[gamma::skip(iter.max_to_min, reason = "mutation causes non-termination or unbounded resource use")]
                self.location = self.location.max(self.resume.record_start);
            }
            self.skip_ignored_records(input);
        }
        if self.location == input.len() {
            self.cursor_start = NO_OFFSET;
            return false;
        }
        self.cursor_start = self.location;
        self.cursor_index = self.record_index;
        // `staged_valid` is only read with `cursor_end != NO_OFFSET`, so
        // leaving it stale avoids an extra hot-path store.
        self.cursor_end = NO_OFFSET;
        true
    }

    /// Rewind to the start of the current record if a view already advanced.
    pub(super) fn rewind_to_current(&mut self) {
        if self.cursor_start == NO_OFFSET {
            not_positioned();
        }
        // Untouched records are already at the right offset.
        if self.location != self.cursor_start {
            self.location = self.cursor_start;
            self.record_index = self.cursor_index;
        }
    }

    /// Position on the next record of a window that may not end the stream.
    ///
    /// Only a record ending inside the window is known to be whole; otherwise
    /// the cursor is restored and [`Advance::NeedMore`] is returned.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: the body is over the inlining threshold, and leaving it out of line costs the chunk parser 29 instructions per record in call frame alone"
    )]
    pub(crate) fn advance_window<F: CsvFormat>(
        &mut self,
        input: &[u8],
        at_eof: bool,
    ) -> Result<Advance, Error> {
        if at_eof {
            return self.advance::<F>(input).map(|found| {
                if found {
                    Advance::Record
                } else {
                    Advance::Done
                }
            });
        }

        let saved = self.chunk_cursor_state();
        match self.try_advance_window::<F>(input) {
            Ok(Some(end)) if self.window_settled(input, end) => Ok(Advance::Record),
            Ok(_) => {
                self.rewind_chunk(saved);
                Ok(Advance::NeedMore)
            }
            Err(error) => self.rewind_chunk_or_fail(saved, error, input.len()),
        }
    }

    #[cold]
    #[inline(never)]
    fn rewind_chunk(&mut self, saved: ChunkCursorState) {
        self.restore_chunk_cursor(saved);
    }

    #[cold]
    #[inline(never)]
    fn rewind_chunk_or_fail(
        &mut self,
        saved: ChunkCursorState,
        error: Error,
        len: usize,
    ) -> Result<Advance, Error> {
        if truncated_by_window(&error, len, self.separator_lookahead()) {
            self.restore_chunk_cursor(saved);
            return Ok(Advance::NeedMore);
        }
        Err(error)
    }

    /// How many bytes from an offending byte onward a verdict can still depend
    /// on: the widest separator this dialect can be in the middle of.
    ///
    /// One for a single-byte dialect, so this is exactly the previous constant
    /// wherever no separator has a tail.
    fn separator_lookahead(&self) -> usize {
        let delimiter = self.dialect.delimiter_tail().as_slice().len();
        let ending = self.dialect.ending_tail().as_slice().len();
        1 + delimiter.max(ending)
    }

    #[cold]
    #[inline(never)]
    fn rewind_to(&mut self, saved: CursorState) {
        self.restore_cursor(saved);
    }

    #[cold]
    #[inline(never)]
    fn rewind_or_fail(
        &mut self,
        saved: CursorState,
        error: Error,
        len: usize,
    ) -> Result<Advance, Error> {
        if truncated_by_window(&error, len, self.separator_lookahead()) {
            self.restore_cursor(saved);
            return Ok(Advance::NeedMore);
        }
        Err(error)
    }

    /// Position on and parse the next record, reporting where it ends.
    #[expect(
        clippy::inline_always,
        reason = "three window callers share this body; leaving it out of line costs a call per record"
    )]
    #[inline(always)]
    pub(super) fn try_advance_window<F: CsvFormat>(
        &mut self,
        input: &[u8],
    ) -> Result<Option<usize>, Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        if !self.headers_initialized && !self.ensure_headers_window(input)? {
            return Ok(None);
        }
        self.settle::<F>(input)?;
        if !self.position(input) {
            return Ok(None);
        }
        let record_start = self.location;
        // Fast resume: when a live checkpoint already proves this window holds
        // no whole record, skip re-parsing the settled prefix and ask for more
        // input directly. A checkpoint only exists for a record a narrower
        // window already refused, so the common case of a record that fits the
        // window it first appears in never reaches the scan.
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        if self.resume.record_start == record_start && self.window_lacks_record::<F>(input) {
            return Ok(None);
        }
        let outcome = if self.staged_form_owned && self.can_stage_owned() {
            self.stage_owned::<F>(input)
        } else {
            self.materialize_full::<F>(input)
                .map(|(range, _)| range.end)
        };
        // gamma::skip(stmt.delete_call, reason = "mutation causes non-termination or unbounded resource use")
        self.checkpoint_after_parse(input, record_start, &outcome);
        outcome.map(Some)
    }

    // gamma::skip(fn_value.unit, reason = "mutation causes non-termination or unbounded resource use")
    /// Record, or keep, a resume checkpoint whenever a full parse turned out to
    /// leave the record incomplete, so the next wider window resumes instead of
    /// re-parsing the prefix.
    ///
    /// This only seeds the checkpoint when there is not already one for this
    /// record: a checkpoint the scan itself advanced must survive the rewind
    /// that follows, or resuming would restart from the record's beginning
    /// every time and stay quadratic.
    #[inline]
    fn checkpoint_after_parse(
        &mut self,
        input: &[u8],
        record_start: usize,
        outcome: &Result<usize, Error>,
    ) {
        let incomplete = match outcome {
            // gamma::skip(unary.remove_not, reason = "mutation causes non-termination or unbounded resource use")
            Ok(end) => !self.window_settled(input, *end),
            Err(error) => truncated_by_window(error, input.len(), self.separator_lookahead()),
        };
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        if incomplete {
            // gamma::skip(stmt.delete_call, reason = "mutation causes non-termination or unbounded resource use")
            self.seed_resume(record_start);
        }
    }

    // gamma::skip(fn_value.unit, reason = "mutation causes non-termination or unbounded resource use")
    /// Seed a fresh resume checkpoint for `record_start` unless an eligible one
    /// is already in place.
    #[inline]
    fn seed_resume(&mut self, record_start: usize) {
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(relational.ne_to_eq, reason = "mutation causes non-termination or unbounded resource use")
        if self.resume.record_start != record_start {
            // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
            self.resume = ResumeState::fresh(record_start);
        }
    }

    /// Whether a window may parse records straight into owned form.
    ///
    /// General parsing and `Compatible` are excluded for correctness. Comment
    /// and trim dialects are excluded on measurement: field-by-field owned
    /// copies cost more than copying the whole record once from spans.
    const fn can_stage_owned(&self) -> bool {
        !self.general_parsing
            && matches!(self.syntax, Syntax::Strict)
            && self.dialect.comment.is_none()
            && !self.trim.applies_to_scope(false)
    }

    // gamma::skip(fn_value.bool_false, reason = "mutation causes non-termination or unbounded resource use")
    /// Whether the current window provably holds no whole record starting at
    /// the checkpointed offset, resuming the boundary scan from where a
    /// narrower window left off.
    ///
    /// Returns `true` only when the answer is certain, in which case the caller
    /// may skip the full parse and ask for more input; `false` means "not
    /// proven", so the full parse still runs and stays authoritative for record
    /// contents, limits, errors, and recovery. Deferring is always sound, so
    /// every boundary that could complete a record or raise a definite error
    /// returns `false`; only a field or quote the window genuinely truncates
    /// pauses. The scan advances [`Self::resume`] so each byte is examined once
    /// across the growing windows, keeping the total work linear in the record
    /// length.
    ///
    /// The caller guarantees `self.resume.record_start == self.location` and
    /// that `self.location < input.len()`.
    #[inline]
    fn window_lacks_record<F: CsvFormat>(&mut self, input: &[u8]) -> bool {
        let config = WindowScanConfig {
            quoting: self.fmt_quoting_enabled::<F>(),
            delimiter: self.fmt_delimiter::<F>(),
            delimiter_tail: self.dialect.delimiter_tail(),
            quote: self.fmt_quote::<F>(),
            terminator: self.fmt_terminator::<F>(),
            ending_tail: self.dialect.ending_tail(),
            crlf: matches!(self.fmt_record_ending::<F>(), RecordEnding::CrLf),
            escape: self.fmt_escape::<F>(),
            permits_unquoted_quotes: self.fmt_permits_unquoted_quotes::<F>(),
            permits_any_backslash: self.fmt_permits_any_backslash_escape::<F>(),
            permits_trailing_ws: self.fmt_permits_trailing_whitespace::<F>(),
            skip_initial_space: self.fmt_skip_initial_space::<F>(),
            max_record: self.limits.max_record_bytes,
            max_field: self.limits.max_field_bytes,
        };
        self.window_lacks_record_runtime(input, config)
    }

    // gamma::skip(fn_value.bool_false, reason = "mutation causes non-termination or unbounded resource use")
    #[expect(
        clippy::too_many_lines,
        reason = "one contiguous state machine mirrors the parser's framing rules exactly"
    )]
    fn window_lacks_record_runtime(&mut self, input: &[u8], config: WindowScanConfig) -> bool {
        debug_assert!(!self.resume.ignored);
        let record_start = self.resume.record_start;
        let quoting = config.quoting;
        let delimiter = config.delimiter;
        let delimiter_tail = config.delimiter_tail;
        let quote = config.quote;
        let terminator = config.terminator;
        let ending_tail = config.ending_tail;
        let crlf = config.crlf;
        let escape = config.escape;
        let unquoted_escape = escape.unquoted_byte();
        let permits_unquoted_quotes = config.permits_unquoted_quotes;
        let permits_any_backslash = config.permits_any_backslash;
        let permits_trailing_ws = config.permits_trailing_ws;
        let skip_initial_space = config.skip_initial_space;
        let max_record = config.max_record;
        let max_field = config.max_field;

        // How a quoted field escapes a quote: `DoubleQuote` writes it twice,
        // the others prefix an escape byte the in-quotes scan also stops on.
        let doubled = matches!(escape, Escape::DoubleQuote);
        let backslash = matches!(escape, Escape::Backslash(_));
        let quoted_escape = match escape {
            Escape::DoubleQuote => quote,
            Escape::Backslash(byte) | Escape::Unquoted(byte) => byte,
            Escape::Mysql => b'\\',
        };

        let mut cursor = self.resume.scanned_to;
        let mut field_start = self.resume.field_start;
        let mut in_quotes = self.resume.in_quotes;

        loop {
            // Mirror the full parser's scan bound so a record or field that
            // has already overrun its limit falls through to the parse, which
            // is the one place a limit error is reported.
            let bound = record_start
                .saturating_add(max_record)
                .min(field_start.saturating_add(max_field))
                .saturating_add(1);
            let scan_end = bound.min(
                // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                input.len(),
            );
            // gamma::skip(relational.lt_to_le, reason = "mutation causes non-termination or unbounded resource use")
            let limited = scan_end < input.len();

            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
            if in_quotes {
                // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                let slice = &input[cursor..scan_end];
                // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                let found = if doubled {
                    find1(quote, slice)
                } else {
                    find2(quote, quoted_escape, slice)
                };
                let Some(rel) = found else {
                    // No closing quote or escape in reach: either the limit
                    // will bite (defer to the parser) or the field genuinely
                    // needs more input.
                    return self.pause_scan(
                        record_start,
                        // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                        scan_end,
                        field_start,
                        // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                        true,
                        limited,
                    );
                };
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                let at = cursor + rel;
                // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(relational.eq_to_ne, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                if !doubled && input[at] == quoted_escape {
                    if at + 1 >= scan_end {
                        // The escaped byte is not in the window yet.
                        return self.pause_scan(
                            record_start,
                            // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                            // gamma::skip(expr.increment, reason = "mutation causes non-termination or unbounded resource use")
                            at,
                            field_start,
                            // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                            true,
                            limited,
                        );
                    }
                    if backslash {
                        // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                        let escaped = input[at + 1];
                        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(logical.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(logical.and_to_or, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(relational.ne_to_eq, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(relational.ne_to_eq, reason = "mutation causes non-termination or unbounded resource use")
                        if escaped != quote && escaped != quoted_escape && !permits_any_backslash {
                            // A backslash escaping neither a quote nor itself is
                            // an error the full parse reports.
                            return false;
                        }
                    }
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 2;
                    // gamma::skip(loop.delete_continue, reason = "mutation causes non-termination or unbounded resource use")
                    continue;
                }
                if at + 1 >= scan_end {
                    // The byte after the quote decides close versus doubled,
                    // and it is not in the window yet.
                    // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                    return self.pause_scan(record_start, at, field_start, true, limited);
                }
                // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(relational.eq_to_ne, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                if doubled && input[at + 1] == quote {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 2;
                    // gamma::skip(loop.delete_continue, reason = "mutation causes non-termination or unbounded resource use")
                    continue;
                }
                // A closing quote: resolve what, if anything, follows it.
                let mut after = at + 1;
                if permits_trailing_ws {
                    // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                    while after < scan_end && matches!(input[after], b' ' | b'\t') {
                        // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                        // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                        after += 1;
                    }
                    if after >= scan_end {
                        // Trailing whitespace runs to the window edge; let the
                        // full parse settle what follows once it arrives.
                        return false;
                    }
                }
                if input[after] == delimiter {
                    let tail_start = after + 1;
                    if scan_end - tail_start < delimiter_tail.as_slice().len() {
                        return self.pause_scan(record_start, at, field_start, true, limited);
                    }
                    if !delimiter_tail.confirms(&input[tail_start..scan_end]) {
                        return false;
                    }
                    let next = after + delimiter_tail.width();
                    field_start = next;
                    cursor = next;
                    in_quotes ^= in_quotes;
                    continue;
                }
                // A record ending or a stray byte after the quote: the full
                // parse owns both the boundary and any error.
                return false;
            }

            let slice = &input[cursor..scan_end];
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
            let mut found = if quoting {
                find3(
                    delimiter,
                    // #[gamma::skip(expr.decrement, expr.increment, reason = "mutation causes non-termination or unbounded resource use")]
                    quote, terminator, slice,
                )
            } else {
                find2(delimiter, terminator, slice)
            };
            if crlf {
                // A `CrLf` dialect must judge every `\r`, so the scan stops on
                // it and hands the decision to the full parse.
                found = earliest(found, find1(b'\r', slice));
            }
            if let Some(escape_byte) = unquoted_escape {
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                found = earliest(found, find1(escape_byte, slice));
            }
            let Some(rel) = found else {
                return self.pause_scan(
                    record_start,
                    // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                    scan_end,
                    field_start,
                    false,
                    limited,
                );
            };
            // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
            let at = cursor + rel;
            let byte = input[at];
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            if unquoted_escape == Some(byte) {
                // An unquoted-field escape hides the next byte's meaning.
                if at + 1 >= scan_end {
                    return self.pause_scan(
                        // gamma::skip(expr.increment, reason = "mutation causes non-termination or unbounded resource use")
                        record_start,
                        // gamma::skip(expr.increment, reason = "mutation causes non-termination or unbounded resource use")
                        at,
                        field_start,
                        false,
                        limited,
                    );
                }
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_to_zero, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                cursor = at + 2;
                // gamma::skip(loop.delete_continue, reason = "mutation causes non-termination or unbounded resource use")
                continue;
            }
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            if byte == delimiter {
                let tail_start = at + 1;
                if scan_end - tail_start < delimiter_tail.as_slice().len() {
                    return self.pause_scan(record_start, at, field_start, false, limited);
                }
                // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(unary.remove_not, reason = "mutation causes non-termination or unbounded resource use")
                if !delimiter_tail.confirms(&input[tail_start..scan_end]) {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = tail_start;
                    continue;
                }
                // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                let next = at + delimiter_tail.width();
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                field_start = next;
                // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                cursor = next;
                continue;
            }
            // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
            // gamma::skip(relational.eq_to_ne, reason = "mutation causes non-termination or unbounded resource use")
            if quoting && byte == quote {
                // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(logical.or_to_and, reason = "mutation causes non-termination or unbounded resource use")
                // gamma::skip(relational.eq_to_ne, reason = "mutation causes non-termination or unbounded resource use")
                if at == field_start
                    || (skip_initial_space
                        // gamma::skip(relational.ne_to_eq, reason = "mutation causes non-termination or unbounded resource use")
                        && field_start != record_start
                        // gamma::skip(expr.decrement, reason = "mutation causes non-termination or unbounded resource use")
                        && input[field_start..at].iter().all(|&b| b == b' '))
                {
                    // Opens a quoted field, possibly past skipped leading
                    // spaces. The bound tracks the quote just as the strict
                    // scan did before it generalized.
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
                    in_quotes = true;
                    field_start = at;
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 1;
                    // gamma::skip(loop.delete_continue, reason = "mutation causes non-termination or unbounded resource use")
                    continue;
                }
                if permits_unquoted_quotes {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_sub, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    // A stray quote is literal content in `Compatible` mode.
                    cursor = at + 1;
                    continue;
                }
                // A quote away from a field start is an error the full parse
                // reports.
                return false;
            }
            // A multi-byte terminator lead can be ordinary field data. Confirm
            // its tail before handing a real boundary to the full parser.
            if byte == terminator && !ending_tail.is_empty() {
                let tail_start = at + 1;
                if scan_end - tail_start < ending_tail.as_slice().len() {
                    return self.pause_scan(record_start, at, field_start, false, limited);
                }
                if !ending_tail.confirms(&input[tail_start..scan_end]) {
                    // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(arith.add_to_mul, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(assign_value.default, reason = "mutation causes non-termination or unbounded resource use")
                    // gamma::skip(literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                    cursor = at + 1;
                    continue;
                }
            }
            // A terminator, or a `\r` a `CrLf` dialect must judge: a record
            // boundary or an error, both for the full parse to resolve.
            return false;
        }
    }

    // gamma::skip(fn_value.bool_false, reason = "mutation causes non-termination or unbounded resource use")
    /// Save the boundary scan's progress and report whether the window still
    /// lacks a whole record.
    ///
    /// When a limit has already been crossed the scan stops proving anything
    /// and hands off to the full parse, which owns limit reporting.
    fn pause_scan(
        &mut self,
        record_start: usize,
        scanned_to: usize,
        field_start: usize,
        in_quotes: bool,
        limited: bool,
    ) -> bool {
        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        if limited {
            return false;
        }
        // gamma::skip(stmt.delete_assign, reason = "mutation causes non-termination or unbounded resource use")
        self.resume = ResumeState {
            record_start,
            scanned_to,
            field_start,
            in_quotes,
            ignored: false,
        };
        // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
        true
    }

    /// Parse the positioned record straight into [`Self::staged_record`].
    ///
    /// `cursor_end` is published normally; `staged_valid` marks that spans
    /// were not built, so borrowed views re-parse through
    /// [`Self::materialize_full`].
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "incremental owned readers stage every settled record; folding the state handoff into the window advance removes a per-record call frame"
    )]
    fn stage_owned<F: CsvFormat>(&mut self, input: &[u8]) -> Result<usize, Error> {
        self.invalidate_staged_record();
        self.clear_terminated();

        // `position` just cleared `cursor_end`, so the owned kernel applies
        // directly.
        let mut staged = self
            .staged_record
            .take()
            .unwrap_or_else(|| Box::new(ByteRecord::new()));
        let result = self.read_owned::<F>(input, &mut staged);
        self.staged_record = Some(staged);
        result?;

        if self.location == input.len() && self.ended_on_terminator::<F>(input) {
            self.note_terminated();
        }

        self.cursor_end = self.location;
        self.staged_valid = true;
        Ok(self.location)
    }

    /// Capture the header record from a window that may not end the stream.
    ///
    /// Returns whether the headers were resolved; `false` asks for more input.
    fn ensure_headers_window(&mut self, input: &[u8]) -> Result<bool, Error> {
        if !self.consume_first_record {
            self.headers_initialized = true;
            return Ok(true);
        }
        let Some((range, _)) = self.fill_record_spans::<Dynamic>(input, true)? else {
            // No record yet; an empty window says nothing about the headers.
            return Ok(false);
        };
        if range.end == input.len() {
            return Ok(false);
        }
        self.header_record = Some(ByteRecord::copied_from(&Record::new(
            self.spans.resolved(input),
            range,
            0,
        )));
        self.on_headers_changed();
        self.headers_initialized = true;
        Ok(true)
    }

    /// Everything the cursor must forget to retry a record on a wider window.
    fn cursor_state(&self) -> CursorState {
        CursorState {
            location: self.location,
            folded_upto: self.folded_upto,
            folded_lines: self.folded_lines,
            record_index: self.record_index,
            expected_fields: self.expected_fields,
            headers_initialized: self.headers_initialized,
            cursor_start: self.cursor_start,
            cursor_end: self.cursor_end,
            cursor_index: self.cursor_index,
            failed: self.failed,
        }
    }

    /// Everything a chunked retry on a wider window must forget.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: taken on every chunked record, and spilling it costs 19 instructions each"
    )]
    fn chunk_cursor_state(&self) -> ChunkCursorState {
        ChunkCursorState {
            location: self.location,
            record_index: self.record_index,
            folded_lines: self.folded_lines,
            expected_fields: self.expected_fields,
            headers_initialized: self.headers_initialized,
        }
    }

    /// Put the cursor back where [`Self::chunk_cursor_state`] found it.
    fn restore_chunk_cursor(&mut self, saved: ChunkCursorState) {
        self.record_index = saved.record_index;
        // The fold tracks parsed records, so a speculative parse that is
        // rewound must give back the lines it counted along with the bytes.
        // Folding only advances from the record start, so anything past the
        // restored location belongs to the record being undone.
        self.folded_upto = self.folded_upto.min(saved.location);
        self.folded_lines = saved.folded_lines;
        self.location = saved.location;
        // A truncated speculative parse must not latch the first record width.
        self.expected_fields = saved.expected_fields;
        self.headers_initialized = saved.headers_initialized;
        // Rewinding may put the headers back out of reach, and `serde_ready`
        // stands in for `headers_initialized` on the Serde path.
        #[cfg(feature = "serde")]
        {
            self.serde_ready ^= self.serde_ready;
        };
        self.cursor_start = NO_OFFSET;
        self.cursor_end = NO_OFFSET;
        self.failed ^= self.failed;
        // Restoring the cursor invalidates any staged record for it.
        self.invalidate_staged_record();
    }

    /// Put the cursor back where [`Self::cursor_state`] found it.
    fn restore_cursor(&mut self, saved: CursorState) {
        self.location = saved.location;
        self.record_index = saved.record_index;
        // The fold tracks parsed records, so a speculative parse that is
        // rewound must give back the lines it counted along with the bytes.
        self.folded_upto = saved.folded_upto;
        self.folded_lines = saved.folded_lines;
        // A truncated speculative parse must not latch the first record width.
        self.expected_fields = saved.expected_fields;
        self.headers_initialized = saved.headers_initialized;
        // Rewinding may put the headers back out of reach, and `serde_ready`
        // stands in for `headers_initialized` on the Serde path.
        #[cfg(feature = "serde")]
        {
            self.serde_ready ^= self.serde_ready;
        };
        // The cursor fields need no snapshot: a rewind leaves no positioned
        // record, which is exactly what clearing the cursor states. Nor does
        // `failed`, which the caller checks before parsing and which no
        // rewindable outcome can have set.
        self.cursor_start = saved.cursor_start;
        self.cursor_end = saved.cursor_end;
        self.cursor_index = saved.cursor_index;
        self.failed = saved.failed;
        // Restoring the cursor invalidates any staged record for it.
        self.invalidate_staged_record();
    }

    /// The earliest window offset the cursor still needs.
    ///
    /// Earlier bytes may be dropped after correcting the remaining offsets with
    /// [`Self::shift_window`].
    pub(crate) fn window_anchor(&self) -> usize {
        let cursor = self.cursor_start.min(self.location);
        if self.resume.ignored {
            cursor.min(self.resume.scanned_to)
        } else {
            cursor
        }
    }

    /// The earliest byte an owned I/O window still needs.
    ///
    /// Unlike a push loan, an I/O refill has no caller-held tail. A checkpoint
    /// that already crossed ignored records may therefore drop the restored
    /// pre-skip location as well.
    pub(crate) fn io_window_anchor(&self) -> usize {
        let anchor = self.window_anchor();
        if self.cursor_start == NO_OFFSET
            && !self.resume.ignored
            && self.resume.record_start != NO_OFFSET
        {
            anchor.max(self.resume.record_start)
        } else {
            anchor
        }
    }

    /// Give up the record the cursor is sitting on so its bytes may be dropped.
    ///
    /// [`Self::window_anchor`] deliberately holds a reported record back
    /// because a view may still be borrowing it. A front end that is about to
    /// swap the whole backing buffer has no way to keep those bytes, so it
    /// says here that nothing borrows them any more and the anchor collapses
    /// to the parse position. Calling this while a view is outstanding would
    /// hand that view a record the buffer no longer contains, so it is only
    /// sound where the borrow checker proves none is alive.
    pub(crate) fn release_positioned_record(&mut self) {
        self.cursor_start = NO_OFFSET;
        self.cursor_end = NO_OFFSET;
        self.invalidate_staged_record();
    }

    /// The byte a record ends on under the configured dialect.
    pub(crate) const fn record_terminator(&self) -> u8 {
        self.dialect.record_ending.byte()
    }

    /// How many bytes follow the record ending's lead byte.
    ///
    /// Zero for every single-byte ending, including `CrLf`, whose lead is its
    /// final `\n`.
    pub(crate) const fn record_ending_tail_len(&self) -> usize {
        self.dialect.ending_tail().as_slice().len()
    }

    /// Position on the next record of a window that may not end the stream,
    /// deferring settled errors to the view.
    ///
    /// If the window already proves the offending record whole, position on it
    /// and let the view report the error; otherwise ask for more input.
    #[cfg(feature = "std")]
    pub(crate) fn advance_window_lazily<F: CsvFormat>(
        &mut self,
        input: &[u8],
        at_eof: bool,
    ) -> Result<Advance, Error> {
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        if at_eof {
            return self.position_lazily::<F>(input);
        }

        let saved = self.cursor_state();
        match self.try_advance_window::<F>(input) {
            Ok(Some(end)) if self.window_settled(input, end) => Ok(Advance::Record),
            Ok(_) => {
                self.rewind_to(saved);
                Ok(Advance::NeedMore)
            }
            Err(error) => self.rewind_or_position_lazily::<F>(saved, &error, input),
        }
    }

    /// Read the next windowed record directly into caller-owned storage.
    pub(crate) fn read_window_owned<F: CsvFormat>(
        &mut self,
        input: &[u8],
        at_eof: bool,
        output: &mut ByteRecord,
    ) -> Result<Advance, Error> {
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        if at_eof {
            if !self.advance::<F>(input)? {
                return Ok(Advance::Done);
            }
            self.read_owned::<F>(input, output)?;
            self.release_positioned_record();
            return Ok(Advance::Record);
        }

        let saved = self.chunk_cursor_state();
        match self.try_read_window_owned::<F>(input, output) {
            Ok(Some(end)) if self.window_settled(input, end) => {
                self.release_positioned_record();
                Ok(Advance::Record)
            }
            Ok(_) => {
                self.rewind_chunk(saved);
                Ok(Advance::NeedMore)
            }
            Err(error) => self.rewind_chunk_or_fail(saved, error, input.len()),
        }
    }

    pub(crate) fn read_window_text<F: CsvFormat>(
        &mut self,
        input: &[u8],
        at_eof: bool,
        output: &mut TextRecord,
    ) -> Result<Advance, Error> {
        if at_eof {
            if !self.advance::<F>(input)? {
                return Ok(Advance::Done);
            }
            self.read_text_record_into::<F>(input, output)?;
            self.release_positioned_record();
            return Ok(Advance::Record);
        }

        let saved = self.chunk_cursor_state();
        match self.try_read_window_text::<F>(input, output) {
            Ok(Some(end)) if self.window_settled(input, end) => {
                self.release_positioned_record();
                Ok(Advance::Record)
            }
            Ok(_) => {
                self.rewind_chunk(saved);
                Ok(Advance::NeedMore)
            }
            Err(error) => self.rewind_chunk_or_fail(saved, error, input.len()),
        }
    }

    fn try_read_window_owned<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut ByteRecord,
    ) -> Result<Option<usize>, Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        self.settle::<F>(input)?;
        // gamma::skip(option.none_to_some, reason = "mutation causes non-termination or unbounded resource use")
        if !self.position(input) {
            return Ok(None);
        }
        let record_start = self.location;
        if self.resume.record_start == record_start && self.window_lacks_record::<F>(input) {
            return Ok(None);
        }

        self.invalidate_staged_record();
        self.clear_terminated();
        let outcome = self
            .read_owned_positioned::<F>(input, output)
            .map(|()| self.location);
        if self.location == input.len() && self.ended_on_terminator::<F>(input) {
            self.note_terminated();
        }
        self.checkpoint_after_parse(input, record_start, &outcome);
        outcome.map(Some)
    }

    fn try_read_window_text<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut TextRecord,
    ) -> Result<Option<usize>, Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        self.settle::<F>(input)?;
        if !self.position(input) {
            return Ok(None);
        }
        let record_start = self.location;
        if self.resume.record_start == record_start && self.window_lacks_record::<F>(input) {
            return Ok(None);
        }

        self.invalidate_staged_record();
        self.clear_terminated();
        let outcome = self
            .read_text_record_into::<F>(input, output)
            .map(|()| self.location);
        if self.location == input.len() && self.ended_on_terminator::<F>(input) {
            self.note_terminated();
        }
        self.checkpoint_after_parse(input, record_start, &outcome);
        outcome.map(Some)
    }

    /// The cold half of [`Self::advance_window_lazily`].
    #[cfg(feature = "std")]
    #[cold]
    #[inline(never)]
    fn rewind_or_position_lazily<F: CsvFormat>(
        &mut self,
        saved: CursorState,
        error: &Error,
        input: &[u8],
    ) -> Result<Advance, Error> {
        self.restore_cursor(saved);
        // #[gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")]
        if truncated_by_window(error, input.len(), self.separator_lookahead()) {
            return Ok(Advance::NeedMore);
        }
        // The window settled the error, so the record carrying it is whole;
        // position on it without parsing and let the view raise it, exactly as
        // the slice parser does.
        self.position_lazily::<F>(input)
    }

    /// Whether the record just parsed ended by consuming a terminator,
    /// recovered from the bytes immediately before the cursor.
    ///
    /// The whole separator has to be checked, not just its lead byte: a
    /// multi-byte record ending leaves its *tail* at the cursor, so comparing
    /// the final byte against the lead never matches and a properly terminated
    /// record is taken for an unterminated one. A record ending exactly at the
    /// window edge would then never be settled.
    ///
    /// This is only sound for `Strict`; `Compatible` can accept an
    /// unterminated quoted field ending in the terminator byte.
    fn ended_on_terminator<F: CsvFormat>(&self, input: &[u8]) -> bool {
        let tail = self.fmt_ending_tail::<F>();
        let Some(lead_at) = self.location.checked_sub(tail.width()) else {
            return false;
        };
        input.get(lead_at) == Some(&self.fmt_terminator::<F>())
            && tail.confirms(&input[lead_at + 1..self.location])
    }

    /// Marks the current record as terminated.
    #[inline]
    pub(super) fn note_terminated(&mut self) {
        self.terminated = true;
    }

    #[inline]
    fn invalidate_staged_record(&mut self) {
        self.staged_valid ^= self.staged_valid;
    }

    /// Forget how the previously parsed record ended.
    #[inline]
    pub(super) fn clear_terminated(&mut self) {
        self.terminated ^= self.terminated;
    }

    /// Whether a record ending at `end` is whole however the stream continues.
    ///
    /// A record ending inside the window is whole. One at the edge is whole
    /// only if it consumed a terminator, which `terminated` tracks for cases
    /// like [`RecordEnding::CrLf`] where the final byte alone is ambiguous.
    fn window_settled(&self, input: &[u8], end: usize) -> bool {
        end < input.len() || self.terminated
    }

    /// Position on the next record without parsing it.
    #[cfg(feature = "std")]
    fn position_lazily<F: CsvFormat>(&mut self, input: &[u8]) -> Result<Advance, Error> {
        self.advance::<F>(input).map(|found| {
            if found {
                Advance::Record
            } else {
                Advance::Done
            }
        })
    }

    /// Position on the next record of a window that may not end the stream,
    /// parsing it eagerly.
    ///
    /// Used when record boundaries are only known after parsing, such as with
    /// comments or skipped blank records.
    #[cfg(feature = "std")]
    pub(crate) fn advance_window_eagerly<F: CsvFormat>(
        &mut self,
        input: &[u8],
        at_eof: bool,
    ) -> Result<Advance, Error> {
        // gamma::skip(cond.always_true, reason = "mutation causes non-termination or unbounded resource use")
        if !at_eof {
            let saved = self.cursor_state();
            return match self.try_advance_window::<F>(input) {
                Ok(Some(end)) if self.window_settled(input, end) => Ok(Advance::Record),
                Ok(_) => {
                    self.rewind_to(saved);
                    Ok(Advance::NeedMore)
                }
                Err(error) => self.rewind_or_fail(saved, error, input.len()),
            };
        }
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        self.ensure_headers(input)?;
        self.settle::<F>(input)?;
        if !self.position(input) {
            return Ok(Advance::Done);
        }
        let _ = self.materialize_full::<F>(input)?;
        Ok(Advance::Record)
    }

    // gamma::skip(fn_value.ok, reason = "mutation causes non-termination or unbounded resource use")
    /// Settle the header record against a window that may not end the stream.
    ///
    /// Returns whether the headers are resolved; settled errors are replayed
    /// through the lazy path so the report matches the slice parser.
    #[cfg(feature = "std")]
    pub(crate) fn headers_window(&mut self, input: &[u8], at_eof: bool) -> Result<bool, Error> {
        if self.headers_initialized {
            // gamma::skip(literal.bool_flip, reason = "mutation causes non-termination or unbounded resource use")
            return Ok(true);
        }
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        if at_eof {
            self.ensure_headers(input)?;
            return Ok(self.headers_initialized);
        }

        let saved = self.cursor_state();
        match self.ensure_headers_window(input) {
            Ok(true) => Ok(self.headers_initialized),
            Ok(false) => {
                self.restore_cursor(saved);
                Ok(false)
            }
            Err(error) => {
                self.restore_cursor(saved);
                if truncated_by_window(&error, input.len(), self.separator_lookahead()) {
                    return Ok(false);
                }
                self.ensure_headers(input)?;
                Ok(self.headers_initialized)
            }
        }
    }

    /// Skip `bytes` of window covering `records` whole records.
    ///
    /// The engine cannot check that `bytes` lands on a record boundary, so
    /// this is only sound behind a scan that located unambiguous record
    /// terminators, the way filter pushdown does.
    #[cfg(feature = "std")]
    pub(crate) fn skip_records(&mut self, bytes: usize, records: u64) {
        self.location = self.location.saturating_add(bytes);
        self.record_index = self.record_index.saturating_add(records);
        self.cursor_start = NO_OFFSET;
        self.cursor_end = NO_OFFSET;
        self.resume = ResumeState::new();
    }

    /// Reposition onto a record boundary inside the current input.
    ///
    /// `line` and `record` are adopted verbatim, and the line origin moves to
    /// `byte` so later lookups extend that count.
    pub(crate) fn seek_to(&mut self, byte: usize, line: u64, record: u64) {
        self.seek_to_exact(byte, line.max(1), record);
    }

    pub(crate) fn seek_to_exact(&mut self, byte: usize, line: u64, record: u64) {
        debug_assert!(line >= 1);
        self.location = byte;
        self.line_base = line;
        self.line_origin = byte;
        self.folded_upto = byte;
        self.folded_lines -= self.folded_lines;
        self.record_index = record;
        self.cursor_start = NO_OFFSET;
        self.cursor_index -= self.cursor_index;
        self.cursor_end = NO_OFFSET;
        self.resume = ResumeState::new();
        self.failed ^= self.failed;
        self.filter_backoff -= self.filter_backoff;
        self.clear_terminated();
    }

    /// Restart the cursor over a fresh window at a new stream position.
    ///
    /// Seeks keep the header record, header lookup, and established field
    /// width.
    #[cfg(feature = "std")]
    pub(crate) fn reset_position(&mut self, line: u64, record: u64) {
        self.location -= self.location;
        self.line_base = line.max(1);
        self.line_origin -= self.line_origin;
        self.folded_upto -= self.folded_upto;
        self.folded_lines -= self.folded_lines;
        self.record_index = record;
        self.cursor_start = NO_OFFSET;
        self.cursor_index -= self.cursor_index;
        self.cursor_end = NO_OFFSET;
        self.resume = ResumeState::new();
        self.failed ^= self.failed;
        self.filter_backoff -= self.filter_backoff;
    }

    /// Reapply the configured header policy from the start of the stream.
    #[cfg(feature = "std")]
    pub(crate) fn reset_headers(&mut self) {
        if self.consume_first_record {
            self.header_record.take();
            self.headers_initialized ^= self.headers_initialized;
            self.on_headers_changed();
        }
        if self.field_count == FieldCount::MatchFirst {
            self.expected_fields = self.header_record.as_ref().map(ByteRecord::len);
        }
    }

    /// Whether a first-record field width still has to be established.
    #[cfg(feature = "std")]
    pub(crate) fn needs_first_record_width(&self) -> bool {
        self.field_count == FieldCount::MatchFirst && self.expected_fields.is_none()
    }

    /// Fold the lines counted so far into the physical-line origin.
    ///
    /// Advancing the origin at known record boundaries keeps later line lookups
    /// scanning only the remaining suffix.
    #[cfg(feature = "index")]
    pub(crate) fn advance_line_origin(&mut self, byte: usize, line: u64) {
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(relational.ge_to_le, reason = "mutation causes non-termination or unbounded resource use")
        if byte >= self.line_origin {
            self.line_base = line.max(1);
            self.line_origin = byte;
            self.folded_upto = byte;
            self.folded_lines -= self.folded_lines;
        }
    }

    /// Skip a leading byte-order mark once the start of the stream is in hand.
    ///
    /// Incremental windows cannot resolve the mark at construction time.
    pub(crate) fn skip_detected_bom(&mut self, input: &[u8]) {
        if self.location == 0 && input.starts_with(b"\xEF\xBB\xBF") {
            self.location = 3;
        }
    }

    /// Correct the cursor after `dropped` was removed from the window front.
    ///
    /// Fold counted newlines into `line_base` before dropping them; bytes
    /// before `line_origin` were never counted.
    pub(crate) fn shift_window(&mut self, window: &[u8], by: usize) {
        if by == 0 {
            return;
        }
        let len = window.len();
        debug_assert!(by <= len);
        // Records fold their own newlines in as they are parsed, so the tally
        // usually answers this without a scan. It reaches past the drop point
        // whenever a window is compacted around a record that is already
        // positioned, which is what the io parsers do on every refill, and the
        // overshoot is bounded by one record.
        //
        // Whatever it reached past is also what the kept bytes carry forward,
        // so splitting the tally in two serves both without scanning twice.
        // Restarting it at the new origin instead would leave it one partial
        // record short of every record start from here on, and it would never
        // fold again for the rest of the stream.
        let (dropped, carried) = if self.folded_upto >= by && self.folded_upto <= len {
            if by >= self.line_origin {
                let overshot = count1(b'\n', &window[by..self.folded_upto]) as u64;
                (self.folded_lines.saturating_sub(overshot), Some(overshot))
            } else {
                // The drop stops short of the line in progress, so it gives up
                // no lines and the tally survives untouched.
                (0, Some(self.folded_lines))
            }
        } else {
            let counted = count1(b'\n', &window[self.line_origin.min(by)..by]) as u64;
            (counted, None)
        };

        self.line_base = self.line_base.saturating_add(dropped);
        self.line_origin = self.line_origin.saturating_sub(by);
        // A forward resume checkpoint can prove that an earlier restored
        // location is no longer needed, so compaction may drop past it.
        self.location = self.location.saturating_sub(by);
        if self.cursor_start != NO_OFFSET {
            self.cursor_start -= by;
        }
        if self.cursor_end != NO_OFFSET {
            self.cursor_end -= by;
        }
        self.shift_resume(by);
        if let Some(lines) = carried {
            self.folded_upto -= by;
            self.folded_lines = lines;
        } else {
            self.folded_upto = self.line_origin;
            self.folded_lines -= self.folded_lines;
        }
    }

    /// Move the resume checkpoint to track a window compacted by `by` bytes.
    ///
    /// Data checkpoints keep their whole record. Ignored comments only keep
    /// the unconfirmed terminator overlap and can continue after their marker
    /// has been dropped.
    fn shift_resume(&mut self, by: usize) {
        if self.resume.record_start == NO_OFFSET {
            return;
        }
        if self.resume.ignored && self.resume.record_start < by {
            // The marker itself may be dropped while its unterminated comment
            // remains live. Keep the continuation at the new window front.
            self.resume.record_start -= self.resume.record_start;
            self.resume.scanned_to = self.resume.scanned_to.saturating_sub(by);
            self.resume.field_start -= self.resume.field_start;
            return;
        }
        if self.resume.record_start >= by
            && self.resume.scanned_to >= by
            && self.resume.field_start >= by
        {
            self.resume.record_start -= by;
            self.resume.scanned_to -= by;
            self.resume.field_start -= by;
        } else {
            self.resume = ResumeState::new();
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Recovery;

    #[test]
    fn test_cursor_coverage_paths() {
        let input = b"col1,col2\nval1,val2\nval3,val4\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        let mut pending = None;
        // skip_toward_literal before headers
        engine.skip_toward_literal(input, b"1", &mut pending);

        // advance_with_filter cached filter column
        let pred = Predicate::equals("col1", "val3");
        assert!(engine.advance_with_filter::<Dynamic>(input, &pred).unwrap());
        // second call with same name hits cached
        let pred2 = Predicate::equals("col1", "nonexistent");
        assert!(
            !engine
                .advance_with_filter::<Dynamic>(input, &pred2)
                .unwrap()
        );

        // advance_line_origin
        #[cfg(feature = "index")]
        {
            engine.advance_line_origin(10, 2);
        }

        // shift_window coverage
        engine.shift_window(input, 0); // by == 0
        engine.line_origin = 10;
        engine.folded_upto = 5;
        engine.shift_window(input, 3); // folded_upto <= len, by < line_origin
        engine.folded_upto = 2;
        engine.shift_window(input, 5); // folded_upto < by
        engine.resume = ResumeState {
            record_start: 2,
            scanned_to: 10,
            field_start: 2,
            in_quotes: false,
            ignored: true,
        };
        engine.shift_window(input, 5); // resume.ignored with record_start < by

        // advance twice in a row without viewing to hit settle (line 208)
        let mut engine_settle = Engine::from_config(
            b"a,b\nc,d\ne,f\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            engine_settle
                .advance::<crate::format::Csv>(b"a,b\nc,d\ne,f\n")
                .unwrap()
        );
        assert!(
            engine_settle
                .advance::<crate::format::Csv>(b"a,b\nc,d\ne,f\n")
                .unwrap()
        );
        assert!(
            engine_settle
                .advance::<crate::format::Csv>(b"a,b\nc,d\ne,f\n")
                .unwrap()
        );
        assert!(
            !engine_settle
                .advance::<crate::format::Csv>(b"a,b\nc,d\ne,f\n")
                .unwrap()
        );

        // advance_window_eagerly at_eof when not failed
        let mut eager_engine = Engine::from_config(
            b"a,b\nc,d\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(matches!(
            eager_engine.advance_window_eagerly::<Dynamic>(b"a,b\nc,d\n", true),
            Ok(Advance::Record)
        ));
        assert!(matches!(
            eager_engine.advance_window_eagerly::<Dynamic>(b"a,b\nc,d\n", true),
            Ok(Advance::Done)
        ));

        // read_window_owned at_eof when advance is false
        let mut eof_own = Engine::from_config_windowed(
            b"",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        let mut eof_rec = ByteRecord::new();
        assert!(matches!(
            eof_own.read_window_owned::<Dynamic>(b"", true, &mut eof_rec),
            Ok(Advance::Done)
        ));
    }

    #[test]
    #[should_panic(expected = "no current record")]
    fn test_rewind_to_current_unpositioned() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.cursor_start = NO_OFFSET;
        engine.rewind_to_current();
    }

    #[test]
    fn test_cursor_additional_coverage() {
        let input = b"col1,col2\nval1,val2\n";
        // ensure_headers_window when consume_first_record is false
        let mut unheaded = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        unheaded.headers_initialized = false;
        assert!(unheaded.ensure_headers_window(input).unwrap());

        // try_read_window_owned when failed
        let mut failed_engine = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        failed_engine.failed = true;
        let mut out = ByteRecord::new();
        assert!(
            failed_engine
                .try_read_window_owned::<Dynamic>(input, &mut out)
                .is_err()
        );

        // ended_on_terminator when location == 0
        let engine_loc0 = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(!engine_loc0.ended_on_terminator::<Dynamic>(input));

        // window_lacks_record with stray quote in strict mode
        let stray_input = b"abc\"def";
        let mut strict_engine = Engine::from_config_windowed(
            stray_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        strict_engine.resume = ResumeState::fresh(0);
        assert!(!strict_engine.window_lacks_record::<Dynamic>(stray_input));

        // window_lacks_record with trailing whitespace running to window edge
        let ws_input = b"\"abc\"   ";
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        settings.syntax = Syntax::Compatible(Recovery::PERMISSIVE);
        let mut ws_engine = Engine::from_config_windowed(ws_input, settings);
        ws_engine.resume = ResumeState::fresh(0);
        assert!(!ws_engine.window_lacks_record::<Dynamic>(ws_input));

        // skip_records, reset_position, reset_headers, needs_first_record_width, skip_detected_bom
        let mut engine = Engine::from_config(
            b"a,b\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        engine.skip_records(4, 1);
        assert_eq!(engine.location, 4);
        assert_eq!(engine.record_index, 1);

        engine.reset_position(2, 5);
        assert_eq!(engine.location, 0);
        assert_eq!(engine.record_index, 5);

        engine.field_count = FieldCount::MatchFirst;
        assert!(engine.needs_first_record_width());
        engine.reset_headers();
        assert_eq!(engine.headers_initialized, false);

        let bom_input = b"\xEF\xBB\xBFa,b\n";
        let mut bom_engine = Engine::from_config(
            bom_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        bom_engine.skip_detected_bom(bom_input);
        assert_eq!(bom_engine.location, 3);

        // advance_window_lazily, read_window_owned, headers_window
        let win_input = b"col1,col2\nval1,val2\n";
        let mut win_engine = Engine::from_config_windowed(
            win_input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(win_engine.headers_window(win_input, true).unwrap());
        assert!(matches!(
            win_engine.advance_window_lazily::<Dynamic>(win_input, true),
            Ok(Advance::Record)
        ));

        let mut out_rec = ByteRecord::new();
        let mut own_engine = Engine::from_config_windowed(
            b"v1,v2\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(matches!(
            own_engine.read_window_owned::<Dynamic>(b"v1,v2\n", true, &mut out_rec),
            Ok(Advance::Record)
        ));
        assert_eq!(out_rec.len(), 2);

        // seek_to
        own_engine.seek_to(0, 1, 0);

        #[cfg(all(feature = "index", feature = "multibyte"))]
        {
            let multi_del_input = b"\"abc\":x";
            let dialect = Dialect::new(b':', b'"', RecordEnding::Newline, Escape::DoubleQuote)
                .unwrap()
                .with_tails(crate::config::Tail::of(b"::"), crate::config::Tail::EMPTY)
                .unwrap();
            let mut multi_engine = Engine::from_config_windowed(
                multi_del_input,
                ParserSettings::unheaded(dialect, Limits::DEFAULT),
            );
            multi_engine.resume = ResumeState {
                record_start: 0,
                scanned_to: 1,
                field_start: 0,
                in_quotes: true,
                ignored: false,
            };
            assert!(!multi_engine.window_lacks_record::<Dynamic>(multi_del_input));
        }

        // Test failed engine in advance, advance_with_filter, try_advance_window, advance_window_eagerly
        let pred_fail = Predicate::equals("col1", "val1");
        let mut f_eng = Engine::from_config(
            input,
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        f_eng.failed = true;
        assert!(f_eng.advance::<Dynamic>(input).is_err());
        assert!(
            f_eng
                .advance_with_filter::<Dynamic>(input, &pred_fail)
                .is_err()
        );
        assert!(f_eng.try_advance_window::<Dynamic>(input).is_err());
        assert!(
            f_eng
                .advance_window_eagerly::<Dynamic>(input, true)
                .is_err()
        );

        // Test settle error propagation in advance, advance_with_filter, try_advance_window, try_read_window_owned, advance_window_eagerly
        let malformed_settle_input = b"ok1,ok2\n\"bad unterminated\n";
        let mut mal_eng = Engine::from_config(
            malformed_settle_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(mal_eng.advance::<Dynamic>(malformed_settle_input).unwrap()); // positions on ok1,ok2
        assert!(mal_eng.advance::<Dynamic>(malformed_settle_input).unwrap()); // positions on bad record
        assert!(mal_eng.advance::<Dynamic>(malformed_settle_input).is_err()); // settle re-parses bad record and fails!

        let mut mal_filt_eng = Engine::from_config(
            malformed_settle_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_filt_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_filt_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_filt_eng
                .advance_with_filter::<Dynamic>(malformed_settle_input, &pred_fail)
                .is_err()
        );

        let mut mal_win_eng = Engine::from_config_windowed(
            malformed_settle_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_win_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_win_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_win_eng
                .try_advance_window::<Dynamic>(malformed_settle_input)
                .is_err()
        );

        let mut mal_own_eng = Engine::from_config_windowed(
            malformed_settle_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_own_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_own_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        let mut mal_out = ByteRecord::new();
        assert!(
            mal_own_eng
                .try_read_window_owned::<Dynamic>(malformed_settle_input, &mut mal_out)
                .is_err()
        );

        let mut mal_eager_eng = Engine::from_config_windowed(
            malformed_settle_input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            mal_eager_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_eager_eng
                .advance::<Dynamic>(malformed_settle_input)
                .unwrap()
        );
        assert!(
            mal_eager_eng
                .advance_window_eagerly::<Dynamic>(malformed_settle_input, true)
                .is_err()
        );

        // Header discovery error in advance and advance_with_filter
        let mut bad_hdr_adv = Engine::from_config(
            b"\"bad header\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(bad_hdr_adv.advance::<Dynamic>(b"\"bad header\n").is_err());
        let mut bad_hdr_filt = Engine::from_config(
            b"\"bad header\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            bad_hdr_filt
                .advance_with_filter::<Dynamic>(b"\"bad header\n", &pred_fail)
                .is_err()
        );

        // advance_window_eagerly with malformed header at EOF (line 1075)
        let mut bad_hdr_eager = Engine::from_config_windowed(
            b"\"bad header\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        assert!(
            bad_hdr_eager
                .advance_window_eagerly::<Dynamic>(b"\"bad header\n", true)
                .is_err()
        );

        // read_window_owned at EOF with malformed header (line 909)
        let mut bad_hdr_own = Engine::from_config_windowed(
            b"\"bad header\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        let mut bad_hdr_rec = ByteRecord::new();
        assert!(
            bad_hdr_own
                .read_window_owned::<Dynamic>(b"\"bad header\n", true, &mut bad_hdr_rec)
                .is_err()
        );

        // reset_headers with header record present and MatchFirst
        let mut match_eng = Engine::from_config(
            b"h1,h2\nv1,v2\n",
            ParserSettings::headed(Dialect::default(), Limits::DEFAULT),
        );
        match_eng.field_count = FieldCount::MatchFirst;
        assert!(match_eng.advance::<Dynamic>(b"h1,h2\nv1,v2\n").unwrap());
        match_eng.reset_headers();
        assert_eq!(match_eng.headers_initialized, false);

        // reset_position, window_settled, record_ending_tail_len
        let mut test_eng = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        test_eng.reset_position(5, 10);
        assert_eq!(test_eng.record_index, 10);
        assert_eq!(test_eng.record_ending_tail_len(), 0);
        assert!(!test_eng.window_settled(b"a,b", 3)); // end == len, terminated == false
        test_eng.terminated = true;
        assert!(test_eng.window_settled(b"a,b", 3)); // end == len, terminated == true

        // advance_line_origin with byte < line_origin
        #[cfg(feature = "index")]
        {
            test_eng.advance_line_origin(10, 5);
            test_eng.advance_line_origin(5, 2); // byte < line_origin
        }
    }

    fn scan_config() -> WindowScanConfig {
        WindowScanConfig {
            quoting: true,
            delimiter: b',',
            delimiter_tail: crate::config::Tail::EMPTY,
            quote: b'"',
            terminator: b'\n',
            ending_tail: crate::config::Tail::EMPTY,
            crlf: false,
            escape: Escape::DoubleQuote,
            permits_unquoted_quotes: false,
            permits_any_backslash: false,
            permits_trailing_ws: false,
            skip_initial_space: false,
            max_record: 64,
            max_field: 64,
        }
    }

    fn scan(input: &[u8], config: WindowScanConfig) -> (bool, ResumeState) {
        scan_from(input, config, ResumeState::fresh(0))
    }

    fn scan_from(
        input: &[u8],
        config: WindowScanConfig,
        resume: ResumeState,
    ) -> (bool, ResumeState) {
        let mut engine = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.resume = resume;
        let lacks_record = engine.window_lacks_record_runtime(input, config);
        (lacks_record, engine.resume)
    }

    fn assert_resume(
        resume: ResumeState,
        record_start: usize,
        scanned_to: usize,
        field_start: usize,
        in_quotes: bool,
    ) {
        assert_eq!(resume.record_start, record_start);
        assert_eq!(resume.scanned_to, scanned_to);
        assert_eq!(resume.field_start, field_start);
        assert_eq!(resume.in_quotes, in_quotes);
        assert!(!resume.ignored);
    }

    #[test]
    fn boundary_scan_tracks_plain_fields_quotes_and_record_endings() {
        let (lacks, resume) = scan(b"abc", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 3, 0, false);

        let (lacks, resume) = scan(b"a,b", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 3, 2, false);

        let (lacks, resume) = scan(b"a,b\n", scan_config());
        assert!(!lacks);
        assert_resume(resume, 0, 0, 0, false);

        let (lacks, resume) = scan(b"\"abc", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, true);

        let (lacks, _) = scan(b"a\"bc", scan_config());
        assert!(!lacks);
        let mut compatible = scan_config();
        compatible.permits_unquoted_quotes = true;
        let (lacks, resume) = scan(b"a\"bc", compatible);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, false);

        let mut unquoted = scan_config();
        unquoted.quoting = false;
        let (lacks, resume) = scan(b"\"abc", unquoted);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, false);

        let mut custom = scan_config();
        custom.delimiter = b';';
        custom.quote = b'\'';
        custom.terminator = b'!';
        let (lacks, resume) = scan(b"a;b", custom);
        assert!(lacks);
        assert_resume(resume, 0, 3, 2, false);
        let (lacks, _) = scan(b"a;b!", custom);
        assert!(!lacks);

        let mut crlf = scan_config();
        crlf.crlf = true;
        let (lacks, _) = scan(b"a\rb", crlf);
        assert!(!lacks);
        let (lacks, resume) = scan(b"a\rb", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 3, 0, false);
    }

    #[test]
    fn boundary_scan_handles_escaped_and_closed_quotes_exactly() {
        let (lacks, resume) = scan(b"\"a\"\"b", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 5, 0, true);

        let (lacks, resume) = scan(b"\"a\",b", scan_config());
        assert!(lacks);
        assert_resume(resume, 0, 5, 4, false);

        let mut whitespace = scan_config();
        whitespace.permits_trailing_ws = true;
        let (lacks, resume) = scan(b"\"a\"  ,b", whitespace);
        assert!(lacks);
        assert_resume(resume, 0, 7, 6, false);
        let (lacks, resume) = scan(b"\"a\"   ,b", whitespace);
        assert!(lacks);
        assert_resume(resume, 0, 8, 7, false);
        let (lacks, _) = scan(b"\"a\"  ,b", scan_config());
        assert!(!lacks);
        let (lacks, _) = scan(b"\"a\"  ", whitespace);
        assert!(!lacks);

        let mut backslash = scan_config();
        backslash.escape = Escape::Backslash(b'\\');
        let (lacks, resume) = scan(b"\"a\\\"b", backslash);
        assert!(lacks);
        assert_resume(resume, 0, 5, 0, true);
        let (lacks, resume) = scan(b"\"a\\", backslash);
        assert!(lacks);
        assert_resume(resume, 0, 2, 0, true);
        let (lacks, _) = scan(b"\"a\\q", backslash);
        assert!(!lacks);
        backslash.permits_any_backslash = true;
        let (lacks, resume) = scan(b"\"a\\q", backslash);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, true);

        let mut unquoted_escape = scan_config();
        unquoted_escape.escape = Escape::Unquoted(b'\\');
        let (lacks, resume) = scan(b"a\\,b", unquoted_escape);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, false);
        let (lacks, resume) = scan(b"a\\", unquoted_escape);
        assert!(lacks);
        assert_resume(resume, 0, 1, 0, false);

        let (lacks, resume) = scan(b"\\x", unquoted_escape);
        assert!(lacks);
        assert_resume(resume, 0, 2, 0, false);

        let mut unquoted_in_quotes = scan_config();
        unquoted_in_quotes.escape = Escape::Unquoted(b'\\');
        let (lacks, resume) = scan(b"\"a\\q", unquoted_in_quotes);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, true);

        let mut custom_quote = scan_config();
        custom_quote.quote = b'\'';
        let (lacks, resume) = scan_from(
            b"xxa'",
            custom_quote,
            ResumeState {
                record_start: 2,
                scanned_to: 2,
                field_start: 2,
                in_quotes: true,
                ignored: false,
            },
        );
        assert!(lacks);
        assert_resume(resume, 2, 3, 2, true);

        custom_quote.escape = Escape::Backslash(b'\\');
        let (lacks, resume) = scan_from(
            b"xxa'",
            custom_quote,
            ResumeState {
                record_start: 2,
                scanned_to: 2,
                field_start: 2,
                in_quotes: true,
                ignored: false,
            },
        );
        assert!(lacks);
        assert_resume(resume, 2, 3, 2, true);

        let (lacks, resume) = scan_from(b"xx\"a\"", scan_config(), ResumeState::fresh(2));
        assert!(lacks);
        assert_resume(resume, 2, 4, 2, true);
    }

    #[test]
    fn boundary_scan_honors_record_and_field_limits_at_both_edges() {
        let mut record_limited = scan_config();
        record_limited.max_record = 3;
        record_limited.max_field = 64;
        let (lacks, resume) = scan(b"abcd", record_limited);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, false);
        let (lacks, resume) = scan(b"abcde", record_limited);
        assert!(!lacks);
        assert_resume(resume, 0, 0, 0, false);

        let mut field_limited = scan_config();
        field_limited.max_record = 64;
        field_limited.max_field = 2;
        let (lacks, resume) = scan(b"a,bcd", field_limited);
        assert!(lacks);
        assert_resume(resume, 0, 5, 2, false);
        let (lacks, resume) = scan(b"a,bcde", field_limited);
        assert!(!lacks);
        assert_resume(resume, 0, 0, 0, false);
    }

    #[test]
    fn skip_initial_space_only_opens_interior_fields_after_all_spaces() {
        let mut config = scan_config();
        config.skip_initial_space = true;

        let (lacks, resume) = scan(b"a,  \"b", config);
        assert!(lacks);
        assert_resume(resume, 0, 6, 4, true);

        let (lacks, _) = scan(b"  \"b", config);
        assert!(!lacks);
        let (lacks, _) = scan(b"a, \t\"b", config);
        assert!(!lacks);
        let (lacks, _) = scan(b"a,x  \"b", config);
        assert!(!lacks);

        let (lacks, resume) = scan_from(b"xxa,   \"b", config, ResumeState::fresh(2));
        assert!(lacks);
        assert_resume(resume, 2, 9, 7, true);
    }

    #[cfg(feature = "multibyte")]
    #[test]
    fn boundary_scan_confirms_complete_multibyte_separators() {
        let mut config = scan_config();
        config.delimiter = b':';
        config.delimiter_tail = crate::config::Tail::of(b"::");

        let (lacks, resume) = scan(b"a::b", config);
        assert!(lacks);
        assert_resume(resume, 0, 4, 3, false);
        let (lacks, resume) = scan(b"a:xb", config);
        assert!(lacks);
        assert_resume(resume, 0, 4, 0, false);
        let (lacks, _) = scan(b"a:\nX", config);
        assert!(!lacks);
        let (lacks, resume) = scan(b"a:", config);
        assert!(lacks);
        assert_resume(resume, 0, 1, 0, false);
        let (lacks, resume) = scan(b"a::", config);
        assert!(lacks);
        assert_resume(resume, 0, 3, 3, false);

        let (lacks, resume) = scan(b"\"a\"::b", config);
        assert!(lacks);
        assert_resume(resume, 0, 6, 5, false);
        let (lacks, resume) = scan(b"\"a\":", config);
        assert!(lacks);
        assert_resume(resume, 0, 2, 0, true);

        config.terminator = b'!';
        config.ending_tail = crate::config::Tail::of(b"!!!");
        let (lacks, _) = scan(b"a!!!b", config);
        assert!(!lacks);
        let (lacks, resume) = scan(b"a!!xb", config);
        assert!(lacks);
        assert_resume(resume, 0, 5, 0, false);

        config.crlf = true;
        config.terminator = b'\n';
        config.ending_tail = crate::config::Tail::of(b"\nX");
        let (lacks, _) = scan(b"a\r", config);
        assert!(!lacks);
        let (lacks, resume) = scan(b"a\n", config);
        assert!(lacks);
        assert_resume(resume, 0, 1, 0, false);

        config.crlf = false;
        config.delimiter = b',';
        config.delimiter_tail = crate::config::Tail::EMPTY;
        config.ending_tail = crate::config::Tail::of(b"\nx");
        let (lacks, resume) = scan(b"a\n,", config);
        assert!(lacks);
        assert_resume(resume, 0, 3, 3, false);
    }

    #[test]
    fn boundary_scan_without_quoting_uses_exact_structural_bytes() {
        let mut config = scan_config();
        config.quoting = false;
        config.delimiter = b';';
        config.terminator = b'!';

        let (lacks, resume) = scan(b"a;b", config);
        assert!(lacks);
        assert_resume(resume, 0, 3, 2, false);
        let (lacks, _) = scan(b"a!b", config);
        assert!(!lacks);
        let (lacks, resume) = scan(b"a,b", config);
        assert!(lacks);
        assert_resume(resume, 0, 3, 0, false);
    }

    #[test]
    fn cursor_position_rewind_and_settle_preserve_exact_indices() {
        let input = b" a \nnext\n";
        let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        settings.trim = Whitespace::HEADERS;
        let mut engine = Engine::from_config(input, settings);
        engine.cursor_start = 0;
        engine.cursor_index = 7;
        engine.location = 0;
        engine.record_index = 99;
        engine
            .settle::<Dynamic>(input)
            .expect("positioned record settles");
        assert_eq!(engine.record_index, 8);
        assert_eq!(engine.spans.get(input, 0), Some(&b" a "[..]));

        engine.cursor_start = engine.location;
        engine.cursor_index = 2;
        engine.record_index = 11;
        engine.rewind_to_current();
        assert_eq!(engine.record_index, 11);

        engine.cursor_start = 0;
        engine.cursor_index = 3;
        engine.location = 4;
        engine.record_index = 12;
        engine.rewind_to_current();
        assert_eq!(engine.location, 0);
        assert_eq!(engine.record_index, 3);
    }

    #[test]
    fn positioning_uses_valid_resume_points_and_clears_eof_cursor() {
        let input = b"first\nsecond\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.resume = ResumeState::fresh(6);
        assert!(engine.position(input));
        assert_eq!(engine.location, 0);

        engine.skips_records = true;
        engine.location = 0;
        engine.resume = ResumeState::fresh(6);
        assert!(engine.position(input));
        assert_eq!(engine.location, 6);
        assert_eq!(engine.cursor_start, 6);

        engine.location = 4;
        engine.resume = ResumeState::fresh(4);
        assert!(engine.position(input));
        assert_eq!(engine.location, 4);

        engine.location = 5;
        engine.resume = ResumeState::fresh(4);
        assert!(engine.position(input));
        assert_eq!(engine.location, 5);

        engine.location = 0;
        engine.resume = ResumeState::fresh(input.len());
        assert!(!engine.position(input));
        assert_eq!(engine.location, input.len());

        engine.location = 0;
        engine.resume = ResumeState::ignored(6, 8);
        assert!(engine.position(input));
        assert_eq!(engine.location, 0);

        engine.location = input.len();
        engine.cursor_start = 1;
        assert!(!engine.position(input));
        assert_eq!(engine.cursor_start, NO_OFFSET);
    }

    #[test]
    fn filter_resolution_skip_state_and_cursor_ranges_are_exact() {
        let input = b"left,right\nno,0\nyes,1\n";
        let mut engine =
            Engine::from_config(input, ParserSettings::headed(Dialect::CSV, Limits::DEFAULT));
        engine.staged_valid = true;
        let predicate = Predicate::equals("left", "yes");
        assert!(
            engine
                .advance_with_filter::<Dynamic>(input, &predicate)
                .expect("matching record is found")
        );
        assert_eq!(engine.cached_filter_column(b"left"), Some(0));
        assert_eq!(engine.cursor_start, 16);
        assert_eq!(engine.cursor_end, input.len());
        assert_eq!(engine.cursor_index, 2);
        assert!(!engine.staged_valid);
        assert_eq!(engine.field::<Dynamic>(input, 1).unwrap(), Some(&b"1"[..]));

        engine.cursor_start = 4;
        let missing = Predicate::equals("missing", "x");
        assert!(
            !engine
                .advance_with_filter::<Dynamic>(input, &missing)
                .expect("a missing header matches nothing")
        );
        assert_eq!(engine.cursor_start, NO_OFFSET);

        let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        settings.dialect.comment = Some(b'#');
        settings.blank_records = BlankRecords::Skip;
        let ignored = b"# comment\n\nno\nmatch\n";
        let mut engine = Engine::from_config(ignored, settings);
        let predicate = Predicate::equals(0, "match");
        assert!(
            engine
                .advance_with_filter::<Dynamic>(ignored, &predicate)
                .expect("ignored records are skipped before filtering")
        );
        assert_eq!(engine.cursor_start, 14);
        assert_eq!(engine.cursor_end, ignored.len());
        assert_eq!(engine.cursor_index, 1);
        assert_eq!(
            engine.field::<Dynamic>(ignored, 0).unwrap(),
            Some(&b"match"[..])
        );

        let padded = b" padded \n";
        let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        settings.trim = Whitespace::HEADERS;
        let mut engine = Engine::from_config(padded, settings);
        let predicate = Predicate::equals(0, " padded ");
        assert!(
            engine
                .advance_with_filter::<Dynamic>(padded, &predicate)
                .expect("data records must not use header trimming")
        );
        assert_eq!(
            engine.field::<Dynamic>(padded, 0).unwrap(),
            Some(&b" padded "[..])
        );
    }

    #[test]
    fn literal_skip_backoff_and_pending_boundaries_are_deterministic() {
        let input = b"a\nb\nneedle\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        let mut pending = None;
        engine.skip_toward_literal(input, b"needle", &mut pending);
        assert_eq!(engine.location, 4);
        assert_eq!(engine.record_index, 2);
        assert_eq!(engine.filter_backoff, 0);
        assert_eq!(pending, Some(4));

        engine.filter_backoff = 3;
        engine.skip_toward_literal(input, b"needle", &mut pending);
        assert_eq!(engine.filter_backoff, 3);

        engine.location = 5;
        engine.skip_toward_literal(input, b"needle", &mut pending);
        assert_eq!(engine.filter_backoff, 2);
        assert_eq!(pending, Some(4));
        engine.skip_toward_literal(input, b"needle", &mut pending);
        assert_eq!(engine.filter_backoff, 1);
        engine.skip_toward_literal(input, b"needle", &mut pending);
        assert_eq!(engine.filter_backoff, 0);
        assert_eq!(engine.location, 5);
        assert_eq!(pending, Some(4));

        let mut no_skip = Engine::from_config(
            b"needle\n",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        let mut no_skip_pending = None;
        no_skip.skip_toward_literal(b"needle\n", b"needle", &mut no_skip_pending);
        assert_eq!(no_skip.location, 0);
        assert_eq!(no_skip.filter_backoff, FILTER_BACKOFF);
        assert_eq!(no_skip_pending, Some(0));

        let mut headed = Engine::from_config(
            b"h\nneedle\n",
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        let mut blocked = None;
        let blocked_at = headed.skip_toward_literal(b"h\nneedle\n", b"needle", &mut blocked);
        assert_eq!(blocked_at, 0);
        assert_eq!(headed.location, 0);
        assert_eq!(headed.filter_backoff, 0);
        assert_eq!(blocked, None);
    }

    #[test]
    fn resume_checkpointing_distinguishes_complete_edge_and_interior_records() {
        let input = b"abc";
        let mut engine = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.resume = ResumeState::new();
        engine.checkpoint_after_parse(input, 0, &Ok(input.len()));
        assert_resume(engine.resume, 0, 0, 0, false);

        engine.resume.scanned_to = 2;
        engine.checkpoint_after_parse(input, 0, &Ok(input.len()));
        assert_eq!(engine.resume.scanned_to, 2);

        engine.resume = ResumeState::new();
        engine.checkpoint_after_parse(input, 0, &Ok(2));
        assert_eq!(engine.resume.record_start, NO_OFFSET);

        engine.terminated = true;
        engine.checkpoint_after_parse(input, 0, &Ok(input.len()));
        assert_eq!(engine.resume.record_start, NO_OFFSET);

        engine.resume = ResumeState::fresh(1);
        engine.seed_resume(1);
        assert_eq!(engine.resume.record_start, 1);
        assert_eq!(engine.resume.scanned_to, 1);
        engine.seed_resume(2);
        assert_resume(engine.resume, 2, 2, 2, false);

        let location = |byte| Location {
            byte,
            line: 1,
            record: 0,
            field: 0,
        };
        engine.terminated = false;
        engine.resume = ResumeState::new();
        let settled = Err(Error::new(ErrorKind::UnexpectedQuote, location(1)));
        engine.checkpoint_after_parse(input, 0, &settled);
        assert_eq!(engine.resume.record_start, NO_OFFSET);

        engine.resume = ResumeState::new();
        let edge = Err(Error::new(ErrorKind::UnexpectedQuote, location(2)));
        engine.checkpoint_after_parse(input, 0, &edge);
        assert_resume(engine.resume, 0, 0, 0, false);
    }

    #[test]
    fn staged_window_records_and_error_locations_use_exact_offsets() {
        let input = b"a,b\n";
        let mut engine = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.staged_form_owned = true;
        assert_eq!(
            engine.try_advance_window::<Dynamic>(input).unwrap(),
            Some(input.len())
        );
        assert!(engine.staged_valid);
        assert_eq!(engine.cursor_end, input.len());
        assert_eq!(
            engine
                .staged_record
                .as_ref()
                .and_then(|record| record.get(0)),
            Some(&b"a"[..])
        );

        let malformed = b"a\"x";
        let mut invalidated = Engine::from_config_windowed(
            malformed,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        assert!(invalidated.position(malformed));
        invalidated.staged_valid = true;
        assert!(invalidated.stage_owned::<Dynamic>(malformed).is_err());
        assert!(!invalidated.staged_valid);

        let mut failed = Engine::from_config_windowed(
            b"xx",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        failed.location = 1;
        failed.failed = true;
        let error = failed
            .try_advance_window::<Dynamic>(b"xx")
            .expect_err("poisoned cursor reports its exact location");
        assert_eq!(error.location().byte, 1);

        let error = failed
            .advance::<Dynamic>(b"xx")
            .expect_err("ordinary advance reports the same exact location");
        assert_eq!(error.location().byte, 1);
    }

    #[test]
    fn lazy_and_eager_windows_distinguish_edge_retries_and_interior_records() {
        let settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);

        let mut lazy_edge = Engine::from_config_windowed(b"a\"", settings.clone());
        assert_eq!(
            lazy_edge
                .advance_window_lazily::<Dynamic>(b"a\"", false)
                .unwrap(),
            Advance::NeedMore
        );
        assert_eq!(lazy_edge.location, 0);

        let mut eager_edge = Engine::from_config_windowed(b"a\"", settings.clone());
        assert_eq!(
            eager_edge
                .advance_window_eagerly::<Dynamic>(b"a\"", false)
                .unwrap(),
            Advance::NeedMore
        );
        assert_eq!(eager_edge.location, 0);

        let settled = b"a\"x";
        let mut lazy_settled = Engine::from_config_windowed(settled, settings.clone());
        assert_eq!(
            lazy_settled
                .advance_window_lazily::<Dynamic>(settled, false)
                .unwrap(),
            Advance::Record
        );
        assert_eq!(lazy_settled.cursor_start, 0);

        let mut eager_settled = Engine::from_config_windowed(settled, settings.clone());
        let error = eager_settled
            .advance_window_eagerly::<Dynamic>(settled, false)
            .expect_err("interior malformed byte is settled");
        assert_eq!(error.location().byte, 1);

        let interior = b"a\nb";
        let mut eager_record = Engine::from_config_windowed(interior, settings.clone());
        assert_eq!(
            eager_record
                .advance_window_eagerly::<Dynamic>(interior, false)
                .unwrap(),
            Advance::Record
        );
        assert_eq!(eager_record.cursor_start, 0);
        assert_eq!(eager_record.cursor_end, 2);

        let mut failed = Engine::from_config_windowed(b"xx", settings);
        failed.location = 1;
        failed.failed = true;
        let error = failed
            .advance_window_eagerly::<Dynamic>(b"xx", true)
            .expect_err("eager EOF reports the poisoned cursor");
        assert_eq!(error.location().byte, 1);
    }

    #[test]
    fn restore_helpers_replace_every_speculative_cursor_field() {
        let mut engine = Engine::from_config_windowed(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.location = 9;
        engine.record_index = 10;
        engine.folded_upto = 9;
        engine.folded_lines = 11;
        engine.expected_fields = Some(12);
        engine.headers_initialized = true;
        engine.cursor_start = 2;
        engine.cursor_end = 8;
        engine.failed = true;
        engine.staged_valid = true;
        #[cfg(feature = "serde")]
        {
            engine.serde_ready = true;
        }

        engine.restore_chunk_cursor(ChunkCursorState {
            location: 4,
            record_index: 5,
            folded_lines: 6,
            expected_fields: Some(3),
            headers_initialized: false,
        });
        assert_eq!(engine.location, 4);
        assert_eq!(engine.record_index, 5);
        assert_eq!(engine.folded_upto, 4);
        assert_eq!(engine.folded_lines, 6);
        assert_eq!(engine.expected_fields, Some(3));
        assert!(!engine.headers_initialized);
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert!(!engine.failed);
        assert!(!engine.staged_valid);
        #[cfg(feature = "serde")]
        assert!(!engine.serde_ready);

        engine.location = 20;
        engine.record_index = 21;
        engine.folded_upto = 22;
        engine.folded_lines = 23;
        engine.expected_fields = None;
        engine.headers_initialized = true;
        engine.cursor_start = 24;
        engine.cursor_end = 25;
        engine.cursor_index = 26;
        engine.failed = false;
        engine.staged_valid = true;
        #[cfg(feature = "serde")]
        {
            engine.serde_ready = true;
        }
        engine.restore_cursor(CursorState {
            location: 7,
            folded_upto: 8,
            folded_lines: 9,
            record_index: 10,
            expected_fields: Some(11),
            headers_initialized: false,
            cursor_start: 12,
            cursor_end: 13,
            cursor_index: 14,
            failed: true,
        });
        assert_eq!(engine.location, 7);
        assert_eq!(engine.folded_upto, 8);
        assert_eq!(engine.folded_lines, 9);
        assert_eq!(engine.record_index, 10);
        assert_eq!(engine.expected_fields, Some(11));
        assert!(!engine.headers_initialized);
        assert_eq!(engine.cursor_start, 12);
        assert_eq!(engine.cursor_end, 13);
        assert_eq!(engine.cursor_index, 14);
        assert!(engine.failed);
        assert!(!engine.staged_valid);
        #[cfg(feature = "serde")]
        assert!(!engine.serde_ready);
    }

    #[test]
    fn window_anchors_and_release_observe_every_live_offset() {
        let mut engine = Engine::from_config_windowed(
            b"0123456789",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.location = 7;
        assert_eq!(engine.window_anchor(), 7);

        engine.cursor_start = 9;
        assert_eq!(engine.window_anchor(), 7);
        engine.resume = ResumeState::ignored(5, 2);
        assert_eq!(engine.window_anchor(), 2);

        engine.cursor_start = NO_OFFSET;
        engine.resume = ResumeState::fresh(9);
        assert_eq!(engine.io_window_anchor(), 9);
        engine.resume.ignored = true;
        assert_eq!(engine.io_window_anchor(), 7);

        engine.cursor_start = 3;
        engine.cursor_end = 8;
        engine.staged_valid = true;
        engine.release_positioned_record();
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert!(!engine.staged_valid);
    }

    #[test]
    fn incremental_header_capture_preserves_offsets_and_invalidates_lookup() {
        let input = b"\xEF\xBB\xBFh1,h2\nv1,v2\n";
        let mut engine = Engine::from_config_windowed(
            input,
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        engine.skip_detected_bom(input);
        engine.store_filter_column(b"stale", 9);
        assert!(
            !engine
                .headers_window(&input[..8], false)
                .expect("header ending at the edge remains incomplete")
        );
        assert!(!engine.headers_initialized);

        assert!(
            engine
                .ensure_headers_window(input)
                .expect("wider window resolves the header")
        );
        assert!(engine.headers_initialized);
        assert_eq!(
            engine
                .header_record
                .as_ref()
                .and_then(|record| record.get(0)),
            Some(&b"h1"[..])
        );
        let (_, range, index) = engine
            .header_record
            .clone()
            .expect("captured header")
            .into_storage();
        assert_eq!(range, 3..9);
        assert_eq!(index, 0);
        assert!(engine.filter_column.is_none());

        let mut unheaded = Engine::from_config_windowed(
            b"x\n",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        unheaded.headers_initialized = false;
        assert!(unheaded.ensure_headers_window(b"x\n").unwrap());
        assert!(unheaded.headers_initialized);
    }

    #[test]
    fn position_resets_line_origins_and_bom_detection_are_exact() {
        let mut engine = Engine::from_config_windowed(
            b"",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.skip_detected_bom(b"\xEF\xBB");
        assert_eq!(engine.location, 0);
        engine.skip_detected_bom(b"\xEF\xBB\xBFa\n");
        assert_eq!(engine.location, 3);
        engine.skip_detected_bom(b"\xEF\xBB\xBFa\n");
        assert_eq!(engine.location, 3);

        engine.line_base = 9;
        engine.line_origin = 8;
        engine.folded_upto = 7;
        engine.folded_lines = 6;
        engine.record_index = 5;
        engine.cursor_start = 4;
        engine.cursor_index = 3;
        engine.cursor_end = 2;
        engine.resume = ResumeState::fresh(1);
        engine.failed = true;
        engine.filter_backoff = 10;
        engine.reset_position(0, 11);
        assert_eq!(engine.location, 0);
        assert_eq!(engine.line_base, 1);
        assert_eq!(engine.line_origin, 0);
        assert_eq!(engine.folded_upto, 0);
        assert_eq!(engine.folded_lines, 0);
        assert_eq!(engine.record_index, 11);
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_index, 0);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
        assert!(!engine.failed);
        assert_eq!(engine.filter_backoff, 0);

        #[cfg(feature = "index")]
        {
            engine.line_origin = 3;
            engine.line_base = 9;
            engine.folded_upto = 4;
            engine.folded_lines = 5;
            engine.advance_line_origin(3, 0);
            assert_eq!(engine.line_origin, 3);
            assert_eq!(engine.line_base, 1);
            assert_eq!(engine.folded_upto, 3);
            assert_eq!(engine.folded_lines, 0);

            engine.advance_line_origin(2, 8);
            assert_eq!(engine.line_origin, 3);
            assert_eq!(engine.line_base, 1);

            engine.advance_line_origin(7, 6);
            assert_eq!(engine.line_origin, 7);
            assert_eq!(engine.line_base, 6);
            assert_eq!(engine.folded_upto, 7);
        }
    }

    #[test]
    fn skip_and_seek_replace_every_position_coordinate() {
        let mut engine = Engine::from_config_windowed(
            b"0123456789",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.location = 2;
        engine.record_index = 3;
        engine.cursor_start = 4;
        engine.cursor_end = 5;
        engine.resume = ResumeState::fresh(6);
        engine.skip_records(7, 8);
        assert_eq!(engine.location, 9);
        assert_eq!(engine.record_index, 11);
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(engine.resume.record_start, NO_OFFSET);

        engine.line_base = 20;
        engine.line_origin = 21;
        engine.folded_upto = 22;
        engine.folded_lines = 23;
        engine.cursor_start = 24;
        engine.cursor_index = 25;
        engine.cursor_end = 26;
        engine.resume = ResumeState::fresh(27);
        engine.failed = true;
        engine.filter_backoff = 28;
        engine.terminated = true;
        engine.seek_to(7, 0, 9);
        assert_eq!(engine.location, 7);
        assert_eq!(engine.line_base, 1);
        assert_eq!(engine.line_origin, 7);
        assert_eq!(engine.folded_upto, 7);
        assert_eq!(engine.folded_lines, 0);
        assert_eq!(engine.record_index, 9);
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_index, 0);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
        assert!(!engine.failed);
        assert_eq!(engine.filter_backoff, 0);
        assert!(!engine.terminated);
    }

    #[test]
    fn header_reset_reapplies_both_header_and_width_policies() {
        let input = b"h1,h2\nv1,v2\n";
        let mut engine =
            Engine::from_config(input, ParserSettings::headed(Dialect::CSV, Limits::DEFAULT));
        assert!(engine.advance::<Dynamic>(input).unwrap());
        assert!(engine.header_record.is_some());
        engine.headers_initialized = true;
        engine.expected_fields = Some(99);
        engine.field_count = FieldCount::MatchFirst;
        engine.store_filter_column(b"h1", 0);
        engine.reset_headers();
        assert!(engine.header_record.is_none());
        assert!(!engine.headers_initialized);
        assert_eq!(engine.expected_fields, None);
        assert!(engine.filter_column.is_none());

        let mut unheaded = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        unheaded.headers_initialized = true;
        unheaded.expected_fields = Some(7);
        unheaded.field_count = FieldCount::Flexible;
        unheaded.reset_headers();
        assert!(unheaded.headers_initialized);
        assert_eq!(unheaded.expected_fields, Some(7));

        let mut provided = ByteRecord::new();
        provided.push_field(b"left");
        provided.push_field(b"right");
        let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        settings.headers = Headers::Provided(provided);
        settings.field_count = FieldCount::MatchFirst;
        let mut provided_engine = Engine::from_config(b"", settings);
        provided_engine.expected_fields = Some(99);
        provided_engine.reset_headers();
        assert_eq!(provided_engine.expected_fields, Some(2));
    }

    #[test]
    fn shift_window_preserves_exact_line_cursor_and_resume_coordinates() {
        let input = b"\n\n\n\n\n\n\n\n\n\n";
        let mut unchanged = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        unchanged.line_base = 9;
        unchanged.line_origin = 3;
        unchanged.folded_upto = 11;
        unchanged.folded_lines = 4;
        unchanged.location = 5;
        unchanged.shift_window(input, 0);
        assert_eq!(unchanged.line_base, 9);
        assert_eq!(unchanged.line_origin, 3);
        assert_eq!(unchanged.folded_upto, 11);
        assert_eq!(unchanged.folded_lines, 4);
        assert_eq!(unchanged.location, 5);

        let mut engine = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.line_base = 10;
        engine.line_origin = 2;
        engine.folded_upto = 8;
        engine.folded_lines = 10;
        engine.location = 9;
        engine.cursor_start = 6;
        engine.cursor_end = 9;
        engine.resume = ResumeState {
            record_start: 4,
            scanned_to: 6,
            field_start: 5,
            in_quotes: true,
            ignored: false,
        };
        engine.shift_window(input, 4);
        assert_eq!(engine.line_base, 16);
        assert_eq!(engine.line_origin, 0);
        assert_eq!(engine.folded_upto, 4);
        assert_eq!(engine.folded_lines, 4);
        assert_eq!(engine.location, 5);
        assert_eq!(engine.cursor_start, 2);
        assert_eq!(engine.cursor_end, 5);
        assert_resume(engine.resume, 0, 2, 1, true);

        let mut before_origin = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        before_origin.line_base = 10;
        before_origin.line_origin = 4;
        before_origin.folded_upto = 8;
        before_origin.folded_lines = 5;
        before_origin.shift_window(input, 2);
        assert_eq!(before_origin.line_base, 10);
        assert_eq!(before_origin.line_origin, 2);
        assert_eq!(before_origin.folded_upto, 6);
        assert_eq!(before_origin.folded_lines, 5);
        assert_eq!(before_origin.cursor_start, NO_OFFSET);
        assert_eq!(before_origin.cursor_end, NO_OFFSET);

        let mut equal_fold = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        equal_fold.line_base = 10;
        equal_fold.line_origin = 2;
        equal_fold.folded_upto = 4;
        equal_fold.folded_lines = 7;
        equal_fold.shift_window(input, 4);
        assert_eq!(equal_fold.line_base, 17);
        assert_eq!(equal_fold.folded_upto, 0);
        assert_eq!(equal_fold.folded_lines, 0);

        let mut equal_len = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        equal_len.line_base = 10;
        equal_len.line_origin = 2;
        equal_len.folded_upto = input.len();
        equal_len.folded_lines = 10;
        equal_len.shift_window(input, 4);
        assert_eq!(equal_len.line_base, 14);
        assert_eq!(equal_len.folded_upto, 6);
        assert_eq!(equal_len.folded_lines, 6);

        let mut equal_origin = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        equal_origin.line_base = 10;
        equal_origin.line_origin = 4;
        equal_origin.folded_upto = 8;
        equal_origin.folded_lines = 8;
        equal_origin.shift_window(input, 4);
        assert_eq!(equal_origin.line_base, 14);
        assert_eq!(equal_origin.line_origin, 0);
        assert_eq!(equal_origin.folded_upto, 4);
        assert_eq!(equal_origin.folded_lines, 4);

        let mut fallback = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        fallback.line_base = 10;
        fallback.line_origin = 1;
        fallback.folded_upto = 2;
        fallback.folded_lines = 8;
        fallback.shift_window(input, 4);
        assert_eq!(fallback.line_base, 13);
        assert_eq!(fallback.line_origin, 0);
        assert_eq!(fallback.folded_upto, 0);
        assert_eq!(fallback.folded_lines, 0);
        assert_eq!(fallback.cursor_start, NO_OFFSET);
        assert_eq!(fallback.cursor_end, NO_OFFSET);

        let mut beyond_window = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        beyond_window.line_base = 10;
        beyond_window.line_origin = 1;
        beyond_window.folded_upto = input.len() + 1;
        beyond_window.folded_lines = 8;
        beyond_window.shift_window(input, 4);
        assert_eq!(beyond_window.line_base, 13);
        assert_eq!(beyond_window.folded_upto, 0);
        assert_eq!(beyond_window.folded_lines, 0);

        let mut origin_past_drop = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        origin_past_drop.line_base = 10;
        origin_past_drop.line_origin = 5;
        origin_past_drop.folded_upto = 2;
        origin_past_drop.folded_lines = 8;
        origin_past_drop.shift_window(input, 4);
        assert_eq!(origin_past_drop.line_base, 10);
        assert_eq!(origin_past_drop.line_origin, 1);
        assert_eq!(origin_past_drop.folded_upto, 1);
        assert_eq!(origin_past_drop.folded_lines, 0);
    }

    #[test]
    fn shift_resume_handles_ignored_equal_and_invalid_boundaries() {
        let mut engine = Engine::from_config_windowed(
            b"0123456789",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        engine.resume = ResumeState::ignored(2, 7);
        engine.resume.field_start = 3;
        engine.shift_resume(4);
        assert_eq!(engine.resume.record_start, 0);
        assert_eq!(engine.resume.scanned_to, 3);
        assert_eq!(engine.resume.field_start, 0);
        assert!(engine.resume.ignored);

        engine.resume = ResumeState {
            record_start: 4,
            scanned_to: 6,
            field_start: 5,
            in_quotes: true,
            ignored: true,
        };
        engine.shift_resume(4);
        assert_eq!(engine.resume.record_start, 0);
        assert_eq!(engine.resume.scanned_to, 2);
        assert_eq!(engine.resume.field_start, 1);
        assert!(engine.resume.in_quotes);
        assert!(engine.resume.ignored);

        for resume in [
            ResumeState {
                record_start: 4,
                scanned_to: 5,
                field_start: 6,
                in_quotes: false,
                ignored: false,
            },
            ResumeState {
                record_start: 5,
                scanned_to: 4,
                field_start: 6,
                in_quotes: false,
                ignored: false,
            },
            ResumeState {
                record_start: 5,
                scanned_to: 6,
                field_start: 4,
                in_quotes: false,
                ignored: false,
            },
        ] {
            engine.resume = resume;
            engine.shift_resume(4);
            assert_resume(
                engine.resume,
                resume.record_start - 4,
                resume.scanned_to - 4,
                resume.field_start - 4,
                false,
            );
        }

        engine.resume = ResumeState {
            record_start: 4,
            scanned_to: 3,
            field_start: 4,
            in_quotes: true,
            ignored: false,
        };
        engine.shift_resume(4);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
        assert_eq!(engine.resume.scanned_to, NO_OFFSET);
        assert_eq!(engine.resume.field_start, NO_OFFSET);

        engine.resume = ResumeState::new();
        engine.shift_resume(4);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
    }

    #[test]
    fn owned_window_reads_release_cursors_and_preserve_exact_failures() {
        let input = b"a,b\n";
        for at_eof in [false, true] {
            let mut engine = Engine::from_config_windowed(
                input,
                ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
            );
            engine.staged_valid = true;
            let mut output = ByteRecord::new();
            assert_eq!(
                engine
                    .read_window_owned::<Dynamic>(input, at_eof, &mut output)
                    .unwrap(),
                Advance::Record
            );
            assert_eq!(output.get(0), Some(&b"a"[..]));
            assert_eq!(output.get(1), Some(&b"b"[..]));
            assert_eq!(engine.cursor_start, NO_OFFSET);
            assert_eq!(engine.cursor_end, NO_OFFSET);
            assert!(!engine.staged_valid);
        }

        let mut failed = Engine::from_config_windowed(
            b"xx",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        failed.location = 1;
        failed.failed = true;
        failed.staged_valid = true;
        failed.terminated = true;
        let mut output = ByteRecord::new();
        let error = failed
            .try_read_window_owned::<Dynamic>(b"xx", &mut output)
            .expect_err("poisoned owned read reports the exact cursor");
        assert_eq!(error.location().byte, 1);
        assert!(failed.staged_valid);
        assert!(failed.terminated);
        assert_eq!(failed.cursor_start, NO_OFFSET);

        let mut complete = Engine::from_config_windowed(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        complete.staged_valid = true;
        assert_eq!(
            complete
                .try_read_window_owned::<Dynamic>(input, &mut output)
                .unwrap(),
            Some(input.len())
        );
        assert!(complete.terminated);
        assert!(!complete.staged_valid);
        assert_eq!(complete.resume.record_start, NO_OFFSET);

        let mut resumed = Engine::from_config_windowed(
            b"abc",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        resumed.resume = ResumeState::fresh(0);
        resumed.staged_valid = true;
        let mut incomplete_output = ByteRecord::new();
        assert_eq!(
            resumed
                .try_read_window_owned::<Dynamic>(b"abc", &mut incomplete_output)
                .unwrap(),
            None
        );
        assert!(resumed.staged_valid);
        assert_eq!(resumed.location, 0);
        assert_eq!(resumed.resume.scanned_to, 3);

        let mut checkpointed = Engine::from_config_windowed(
            b"abc",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        checkpointed.resume = ResumeState::new();
        assert_eq!(
            checkpointed
                .try_read_window_owned::<Dynamic>(b"abc", &mut incomplete_output)
                .unwrap(),
            Some(3)
        );
        assert_eq!(checkpointed.resume.record_start, 0);

        let mut edge = Engine::from_config_windowed(
            b"a\"",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        assert_eq!(
            edge.read_window_owned::<Dynamic>(b"a\"", false, &mut incomplete_output)
                .unwrap(),
            Advance::NeedMore
        );
        assert_eq!(edge.location, 0);
    }

    #[test]
    fn header_windows_distinguish_complete_truncated_and_settled_errors() {
        let complete = b"h1,h2\nv1,v2\n";
        let mut initialized = Engine::from_config_windowed(
            complete,
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        initialized.headers_initialized = true;
        initialized.location = 7;
        initialized.record_index = 9;
        assert!(initialized.headers_window(complete, false).unwrap());
        assert_eq!(initialized.location, 7);
        assert_eq!(initialized.record_index, 9);

        let mut eof_header = Engine::from_config_windowed(
            b"header",
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        assert!(eof_header.headers_window(b"header", true).unwrap());
        assert!(eof_header.headers_initialized);

        let mut truncated = Engine::from_config_windowed(
            b"a\"",
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        truncated.location = 0;
        truncated.record_index = 0;
        assert!(!truncated.headers_window(b"a\"", false).unwrap());
        assert_eq!(truncated.location, 0);
        assert_eq!(truncated.record_index, 0);
        assert!(!truncated.headers_initialized);

        let mut settled = Engine::from_config_windowed(
            b"a\"x",
            ParserSettings::headed(Dialect::CSV, Limits::DEFAULT),
        );
        let error = settled
            .headers_window(b"a\"x", false)
            .expect_err("interior malformed byte is settled");
        assert_eq!(error.location().byte, 1);
    }

    #[test]
    fn terminator_detection_checks_the_exact_lead_and_tail_range() {
        let mut engine = Engine::from_config_windowed(
            b"a\n",
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        assert!(!engine.ended_on_terminator::<Dynamic>(b"a\n"));
        engine.location = 2;
        assert!(engine.ended_on_terminator::<Dynamic>(b"a\n"));
        assert!(!engine.ended_on_terminator::<Dynamic>(b"ax"));

        #[cfg(feature = "multibyte")]
        {
            let mut dialect = Dialect::CSV;
            dialect.ending_tail = crate::config::Tail::of(b"\nXY");
            let mut engine = Engine::from_config_windowed(
                b"aa\nXY",
                ParserSettings::unheaded(dialect, Limits::DEFAULT),
            );
            engine.location = 5;
            assert!(engine.ended_on_terminator::<Dynamic>(b"aa\nXY"));
            assert!(!engine.ended_on_terminator::<Dynamic>(b"aa\nXZ"));
            engine.location = 2;
            assert!(!engine.ended_on_terminator::<Dynamic>(b"aa\nXY"));
        }
    }

    #[test]
    fn settled_window_errors_are_not_misclassified_as_truncation() {
        let malformed = b"a\"x";
        let settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);

        let mut chunk = Engine::from_config_windowed(malformed, settings.clone());
        assert!(chunk.advance_window::<Dynamic>(malformed, false).is_err());

        let mut owned = Engine::from_config_windowed(malformed, settings);
        let mut output = ByteRecord::new();
        assert!(
            owned
                .read_window_owned::<Dynamic>(malformed, false, &mut output)
                .is_err()
        );
    }

    #[test]
    fn separator_lookahead_uses_the_widest_separator() {
        let engine =
            Engine::from_config(b"", ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT));
        assert_eq!(engine.separator_lookahead(), 1);

        #[cfg(feature = "multibyte")]
        {
            let mut dialect = Dialect::CSV;
            dialect.delimiter_tail = crate::config::Tail::of(b"::::");
            dialect.ending_tail = crate::config::Tail::of(b"...");
            let engine =
                Engine::from_config(b"", ParserSettings::unheaded(dialect, Limits::DEFAULT));
            assert_eq!(engine.separator_lookahead(), 4);

            dialect.delimiter_tail = crate::config::Tail::of(b"::");
            dialect.ending_tail = crate::config::Tail::of(b"....");
            let engine =
                Engine::from_config(b"", ParserSettings::unheaded(dialect, Limits::DEFAULT));
            assert_eq!(engine.separator_lookahead(), 4);
        }
    }
}

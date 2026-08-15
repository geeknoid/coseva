//! Record framing: skipping, trimming, and physical record reads.

#[cfg(test)]
use core::cell::Cell;

use super::*;
#[cfg(target_arch = "x86_64")]
// Test-only override of `Span::MAX_OFFSET`, so a unit test can exercise the
// `Engine::parse_positioned_record` guard below without allocating a real
// gigabyte-scale buffer to reach it. Production code always reads
// `Span::MAX_OFFSET` directly; this seam only exists under `#[cfg(test)]`.
#[cfg(test)]
std::thread_local! {
    static TEST_MAX_OFFSET: Cell<Option<usize>> = const { Cell::new(None) };
}

#[inline]
fn max_offset() -> usize {
    #[cfg(test)]
    {
        TEST_MAX_OFFSET.with(Cell::get).unwrap_or(Span::MAX_OFFSET)
    }
    #[cfg(not(test))]
    {
        Span::MAX_OFFSET
    }
}

impl Engine {
    // gamma::skip(fn_value.some, reason = "fabricating an ending width at a non-ending byte prevents the framing cursor from advancing to a real boundary and makes ignored-record loops unbounded")
    fn record_ending_width_at(&self, input: &[u8], at: usize) -> Option<usize> {
        match self.dialect.record_ending {
            // gamma::skip(match_guard.always_true, reason = "accepting every byte as LF makes blank-line skipping walk the full input as zero-field records")
            // gamma::skip(match_guard.negate, reason = "inverting the LF guard makes blank-line skipping consume non-newline input without a framing boundary")
            // gamma::skip(relational.eq_to_ne, reason = "treating non-LF bytes as LF makes blank-line skipping advance indefinitely across widening windows")
            // gamma::skip(option.some_to_none, reason = "removing the confirmed LF width leaves the ignored-record fixed-point loop unable to advance")
            RecordEnding::Newline if input.get(at) == Some(&b'\n') => {
                // gamma::skip(literal.int_decrement, reason = "a zero-width LF ending leaves skip_blank_lines on the same byte forever")
                Some(1)
            }
            // gamma::skip(match_guard.always_true, reason = "accepting every two-byte window as CRLF makes blank-line skipping consume arbitrary input as framing")
            // gamma::skip(match_guard.negate, reason = "inverting CRLF confirmation advances over non-endings and repeatedly reparses the remaining window")
            // gamma::skip(relational.eq_to_ne, reason = "treating non-CRLF pairs as endings makes ignored-record skipping consume the window without valid boundaries")
            // gamma::skip(option.some_to_none, reason = "removing the confirmed CRLF match prevents the ignored-record loop from reaching its next record")
            RecordEnding::Newline | RecordEnding::CrLf
                if input.get(at..at + 2) == Some(b"\r\n") =>
            {
                // gamma::skip(literal.int_to_zero, reason = "a zero-width CRLF ending leaves skip_blank_lines on the same byte forever")
                Some(2)
            }
            RecordEnding::Byte(byte) if input.get(at) == Some(&byte) => {
                let tail = self.dialect.ending_tail();
                tail.confirms(&input[at + 1..]).then(|| tail.width())
            }
            // gamma::skip(option.none_to_some, reason = "inventing an ending width for an unconfirmed lead byte makes ignored-record skipping advance through arbitrary input until resource limits are hit")
            RecordEnding::Newline | RecordEnding::CrLf | RecordEnding::Byte(_) => None,
        }
    }

    // gamma::skip(fn_value.some, reason = "fabricating a record end when no confirmed ending exists makes comment resumption report progress without consuming a boundary and grows the streaming window without bound")
    pub(super) fn find_record_ending(&self, input: &[u8], start: usize) -> Option<usize> {
        let lead = self.dialect.record_ending.byte();
        let tail = self.dialect.ending_tail();
        let mut search_start = start;
        loop {
            let remaining = input.get(search_start..)?;
            // gamma::skip(option.none_to_some, reason = "inventing a record end after an exhausted lead-byte search makes comment framing claim progress without consuming a confirmed boundary")
            let Some(relative) = find1(lead, remaining) else {
                return None;
            };
            let at = search_start + relative;
            let confirmed = match self.dialect.record_ending {
                RecordEnding::Newline => true,
                RecordEnding::CrLf => at > start && input[at - 1] == b'\r',
                RecordEnding::Byte(_) => tail.confirms(&input[at + 1..]),
            };
            if confirmed {
                return Some(at + tail.width());
            }
            // gamma::skip(stmt.delete_assign, reason = "not advancing past a rejected ending lead repeats the same search result forever")
            // gamma::skip(arith.add_to_mul, reason = "multiplying the rejected lead by one does not advance the search cursor, so the same candidate is retried forever")
            // gamma::skip(arith.add_to_sub, reason = "moving backward from a rejected ending lead repeatedly rediscovers that lead and prevents framing progress")
            // gamma::skip(assign_value.default, reason = "resetting the search cursor to zero makes every rejected lead restart the scan and prevents completion")
            // gamma::skip(literal.int_decrement, reason = "a zero cursor increment retries the rejected ending lead forever")
            search_start = at + 1;
        }
    }

    fn skip_blank_lines(&mut self, input: &[u8]) {
        if self.blank_records != BlankRecords::Skip {
            return;
        }
        while let Some(width) = self.record_ending_width_at(input, self.location) {
            // gamma::skip(assign.add_to_sub, reason = "moving the framing cursor backward after a blank line repeatedly rediscovers the same ending and expands work without bound")
            // gamma::skip(stmt.delete_assign, reason = "not consuming a confirmed blank-line ending leaves the loop on the same byte forever")
            self.location += width;
        }
    }

    pub(super) fn skip_ignored_records(&mut self, input: &[u8]) {
        let mut skipped = self.resume.ignored;
        loop {
            let before = self.location;
            self.skip_comments(input);
            self.skip_blank_lines(input);
            // gamma::skip(cond.always_false, reason = "removing the fixed-point exit keeps ignored-record skipping in an unconditional loop after progress stops")
            // gamma::skip(cond.negate, reason = "inverting the fixed-point check continues precisely when no cursor progress occurred")
            // gamma::skip(relational.eq_to_ne, reason = "breaking on progress and continuing on no progress leaves the loop spinning at an unchanged cursor")
            if self.location == before {
                // gamma::skip(loop.break_to_continue, reason = "continuing at the ignored-record fixed point retries the same no-progress iteration forever")
                // gamma::skip(loop.delete_break, reason = "deleting the fixed-point break leaves ignored-record skipping in a no-progress loop")
                break;
            }
            skipped = true;
        }
        if skipped && !self.resume.ignored {
            self.resume = ResumeState::fresh(self.location);
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: making the record path generic over the format perturbs LLVM's inlining order and spills this callee out of the hot loop, costing ~5% of parsing instructions"
    )]
    fn trim_spans(&mut self, input: &[u8], header: bool) {
        match self.trim.applies_to_scope(header) {
            true => self
                .spans
                .trim_ascii_where(input, |quoted| self.trim.applies(header, quoted)),
            false => {}
        }
    }

    /// Parse one physical record into `self.spans`/`self.scratch`.
    ///
    /// Returns the record byte range and index without borrowing the parser, so
    /// callers that own the input buffer can build a [`Record`] themselves.
    pub(super) fn fill_record_spans<F: CsvFormat>(
        &mut self,
        input: &[u8],
        header: bool,
    ) -> Result<Option<(Range<usize>, u64)>, Error> {
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        if self.skips_records {
            self.skip_ignored_records(input);
        }
        if self.location == input.len() {
            return Ok(None);
        }
        self.parse_positioned_record::<F>(input, header).map(Some)
    }

    #[inline]
    fn begin_spans(&mut self, input: &[u8]) -> Result<(), Error> {
        if !self.spans.begin(input, max_offset()) {
            self.failed = true;
            return Err(self.error(input, ErrorKind::LocationOverflow, self.location));
        }
        Ok(())
    }

    /// Parse the record already known to start at the current offset.
    ///
    /// The caller must have established that a record is there, which lets the
    /// cursor views skip the failure, comment, and end-of-input checks that
    /// positioning already performed.
    #[inline]
    pub(super) fn parse_positioned_record<F: CsvFormat>(
        &mut self,
        input: &[u8],
        header: bool,
    ) -> Result<(Range<usize>, u64), Error> {
        // `Span` packs its source and NULL flags into the high bits of each
        // offset, so a buffer longer than `Span::MAX_OFFSET` would produce
        // spans whose offsets alias a flag. Rejecting the buffer once per
        // record is the only check needed: every span this parser builds points
        // into `input` or into `scratch`, and `scratch` is cleared per record
        // and never grows beyond the record's own bytes. On 64-bit targets the
        // bound exceeds any possible allocation and the branch never taken.
        // `max_offset()` is `Span::MAX_OFFSET` outside tests; see its
        // definition above for the seam that lets a test shrink it.
        self.begin_spans(input)?;
        let record_start = self.location;
        if let Err(error) = self.parse_record::<F>(input, record_start, header) {
            self.failed = true;
            // gamma::skip(result.err_to_ok, reason = "turning a parse failure into a successful zero range leaves callers retrying the same malformed record without cursor progress")
            return Err(error);
        }
        self.trim_spans(input, header);
        let record_end = self.location;
        let index = self.record_index;
        self.record_index += 1;
        Ok((record_start..record_end, index))
    }

    pub(super) fn next_physical_record<'a>(
        &'a mut self,
        input: &'a [u8],
        header: bool,
    ) -> Result<Option<Record<'a>>, Error> {
        let Some((range, index)) = self.fill_record_spans::<Dynamic>(input, header)? else {
            return Ok(None);
        };
        Ok(Some(
            Record::new(self.spans.resolved(input), range, index)
                .with_null_aware(!header && self.nulls != Nulls::None),
        ))
    }

    pub(super) fn read_physical_storage<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        header: bool,
    ) -> Result<bool, Error> {
        self.read_physical_storage_mode::<F, false>(input, output, header)
    }

    pub(super) fn read_physical_text_storage<F: CsvFormat>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        header: bool,
    ) -> Result<bool, Error> {
        self.read_physical_storage_mode::<F, true>(input, output, header)
    }

    fn read_physical_storage_mode<F: CsvFormat, const CERTIFY_ASCII: bool>(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        header: bool,
    ) -> Result<bool, Error> {
        if self.fmt_general_parsing::<F>() {
            return self.read_physical_storage_general(input, output, header);
        }
        if self.failed {
            return Err(self.error(input, ErrorKind::ParserFailed, self.location));
        }
        if self.skips_records {
            self.skip_ignored_records(input);
        }
        if self.location == input.len() {
            return Ok(false);
        }

        let record_start = self.location;
        output.clear_fields();
        if CERTIFY_ASCII {
            output.reset_text_validity();
        }
        // Only a caller that supplies a brand new record every time needs the
        // pre-sizing, and only such a caller can teach the hint anything, so
        // both halves are gated on the same one-compare test.
        let unallocated = output.is_unallocated();
        if unallocated {
            presize_owned(output, self.owned_hint);
        }
        let parsed = if CERTIFY_ASCII {
            self.parse_owned_text_record::<F>(input, output, record_start, header)
        } else {
            self.parse_owned_record::<F>(input, output, record_start, header)
        };
        if let Err(error) = parsed {
            self.failed = true;
            output.invalidate_source_metadata();
            return Err(error);
        }
        if self.trim.applies_to_scope(header) {
            output.trim_fields_ascii();
        }
        let record_end = self.location;
        output.set_location(record_start..record_end, self.record_index);
        self.record_index += 1;
        if unallocated {
            self.owned_hint = (
                self.owned_hint.0.max(output.len()),
                self.owned_hint.1.max(output.bytes_len()),
            );
        }
        Ok(true)
    }

    fn read_physical_storage_general(
        &mut self,
        input: &[u8],
        output: &mut RecordStorage,
        header: bool,
    ) -> Result<bool, Error> {
        let Some(record) = self.next_physical_record(input, header)? else {
            return Ok(false);
        };
        output.clear();
        output.reserve(record.len(), record.byte_range().len());
        for index in 0..record.len() {
            let (field, is_null) = record.spans.get_entry(index).expect("index is in range");
            if is_null {
                output.append_null_field();
            } else {
                output.append_field(field);
            }
        }
        output.set_null_aware(record.null_aware);
        output.set_location(record.byte_range(), record.index());
        Ok(true)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{Engine, TEST_MAX_OFFSET, max_offset};
    use crate::config::{
        BlankRecords, Dialect, Limits, Nulls, ParserSettings, RecordEnding, Whitespace,
    };
    use crate::encoding::{DecodeField, DecodeRecord};
    use crate::engine::ResumeState;
    use crate::error::ErrorKind;
    use crate::format::Dynamic;
    use coseva_unsafe::span::Span;
    use coseva_unsafe::storage::RecordStorage;

    /// Restores the real `Span::MAX_OFFSET` when a test's override goes out
    /// of scope, even on panic, so a shrunk bound never leaks into another
    /// test sharing this thread.
    struct MaxOffsetGuard;

    impl MaxOffsetGuard {
        fn shrink_to(bound: usize) -> Self {
            TEST_MAX_OFFSET.with(|cell| cell.set(Some(bound)));
            Self
        }
    }

    impl Drop for MaxOffsetGuard {
        fn drop(&mut self) {
            TEST_MAX_OFFSET.with(|cell| cell.set(None));
        }
    }

    /// This test proves `parse_positioned_record`'s bound check is
    /// load-pin: with `max_offset()` shrunk to a value the input window
    /// exceeds, the record must be rejected with `LocationOverflow` rather
    /// than proceed to build a span whose offset would collide with a flag
    /// bit. A real `Span::MAX_OFFSET + 1`-byte input is gigabytes on a
    /// 64-bit target and unreachable in a unit test; shrinking the bound is
    /// the seam that reaches the same branch cheaply.
    #[test]
    fn parse_positioned_record_rejects_a_window_past_the_offset_bound() {
        let _guard = MaxOffsetGuard::shrink_to(8);
        assert_eq!(max_offset(), 8);

        let input = b"aaaaaaaaaa,b\n"; // 13 bytes, past the shrunk bound
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut engine = Engine::from_config(input, settings);

        let error = engine
            .parse_positioned_record::<Dynamic>(input, false)
            .expect_err("a window past the offset bound must be rejected");
        assert_eq!(error.kind(), ErrorKind::LocationOverflow);
    }

    /// A window of exactly `max_offset()` bytes must parse: the largest
    /// representable offset is `MAX_OFFSET` itself, so the guard rejects only
    /// what genuinely overflows. Sitting the input exactly on the boundary is
    /// what makes this test sensitive to the comparison widening to `>=`; a
    /// window comfortably inside the bound would pass either way.
    #[test]
    fn parse_positioned_record_accepts_a_window_exactly_on_the_offset_bound() {
        let input = b"a,b\n";
        let _guard = MaxOffsetGuard::shrink_to(input.len());
        assert_eq!(max_offset(), input.len());

        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut engine = Engine::from_config(input, settings);

        engine
            .parse_positioned_record::<Dynamic>(input, false)
            .expect("a window exactly on the offset bound must parse normally");
    }

    #[test]
    fn max_offset_fallback_is_the_exact_span_bound() {
        TEST_MAX_OFFSET.with(|cell| cell.set(None));
        assert_eq!(max_offset(), Span::MAX_OFFSET);
    }

    #[test]
    fn location_overflow_marks_the_engine_failed_for_later_reads() {
        let _guard = MaxOffsetGuard::shrink_to(3);
        let input = b"a,b\n";
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut engine = Engine::from_config(input, settings);

        let first = engine
            .fill_record_spans::<Dynamic>(input, false)
            .expect_err("the input exceeds the shrunk span bound");
        assert_eq!(first.kind(), ErrorKind::LocationOverflow);
        assert!(engine.failed);

        let second = engine
            .fill_record_spans::<Dynamic>(input, false)
            .expect_err("a failed parser must remain failed");
        assert_eq!(second.kind(), ErrorKind::ParserFailed);
    }

    /// `find_record_ending` is only ever called with `start` sitting on a
    /// candidate `\n` that the surrounding search has not yet looked behind,
    /// so `at == start` on the very first candidate is the boundary the
    /// `at > start` guard exists for. Under `RecordEnding::CrLf`, a bare `\n`
    /// with nothing preceding it in the window can never be a valid ending
    /// (there is no `\r` to confirm), and the guard must reject it without
    /// reading `input[at - 1]`, which is out of bounds when `at` is `0`.
    /// Widening the guard to `>=`, or loosening the `&&` to `||`, both let
    /// that read happen and panic on the underflowed index.
    #[test]
    fn find_record_ending_rejects_a_bare_newline_at_the_window_start() {
        let dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        let input = b"\n";
        let engine = Engine::from_config(input, settings);

        assert_eq!(engine.find_record_ending(input, 0), None);
    }

    #[cfg(feature = "multibyte")]
    #[test]
    fn find_record_ending_advances_one_byte_after_a_rejected_lead() {
        let dialect = Dialect::new(
            b',',
            b'"',
            RecordEnding::Byte(b';'),
            crate::engine::Escape::DoubleQuote,
        )
        .expect("valid dialect")
        .with_tails(crate::config::Tail::EMPTY, crate::config::Tail::of(b";x"))
        .expect("valid tails");
        let input = b";;x";
        let engine = Engine::from_config(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));

        assert_eq!(
            engine.find_record_ending(input, 0),
            Some(3),
            "the second adjacent lead is the start of the confirmed ending"
        );
    }

    #[test]
    fn ignored_record_skipping_sets_an_exact_fresh_resume_checkpoint() {
        let input = b"\na\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.blank_records = BlankRecords::Skip;
        engine.location = 0;
        engine.resume = ResumeState::new();

        engine.skip_ignored_records(input);

        assert_eq!(engine.location, 1);
        assert_eq!(engine.resume.record_start, 1);
        assert_eq!(engine.resume.scanned_to, 1);
        assert_eq!(engine.resume.field_start, 1);
        assert!(!engine.resume.in_quotes);
        assert!(!engine.resume.ignored);
    }

    #[test]
    fn fill_record_spans_honors_the_cached_skip_decision_exactly() {
        let input = b"\na\n";

        let mut do_not_skip = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        do_not_skip.blank_records = BlankRecords::Skip;
        do_not_skip.skips_records = false;
        let (range, _) = do_not_skip
            .fill_record_spans::<Dynamic>(input, false)
            .expect("valid blank record")
            .expect("record present");
        assert_eq!(range, 0..1);

        let mut do_skip = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        do_skip.blank_records = BlankRecords::Skip;
        do_skip.skips_records = true;
        let (range, _) = do_skip
            .fill_record_spans::<Dynamic>(input, false)
            .expect("valid data record")
            .expect("record present");
        assert_eq!(range, 1..3);
    }

    /// The header record is never null-aware, even when a NULL policy is
    /// configured: `with_null_aware` is gated on `!header`, so the header
    /// short-circuits to `false` before the policy is even asked about.
    #[test]
    fn next_physical_record_header_is_never_null_aware_under_a_null_policy() {
        let settings = ParserSettings {
            nulls: Nulls::Mysql,
            ..ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT)
        };
        let input = b"a,\n";
        let mut engine = Engine::from_config(input, settings);

        let record = engine
            .next_physical_record(input, true)
            .expect("valid record")
            .expect("a record is present");
        assert!(!record.is_null_aware());

        // Not null-aware means an empty field reads back as legacy `None`,
        // not as a present-but-empty value, even though the field itself
        // (an unescaped, non-`\N` empty string) is not an explicit NULL.
        let field =
            Option::<&[u8]>::decode_field_from_record(&record, 1, "field").expect("field decodes");
        assert_eq!(field, None);
    }

    /// A data record under a NULL policy (here `Nulls::Mysql`, whose sentinel
    /// is the literal `\N`, not emptiness) is null-aware, and that awareness
    /// is what lets a present-but-empty field be told apart from an absent
    /// one: `Option<&[u8]>` decodes it to `Some(&[])`, not `None`.
    #[test]
    fn next_physical_record_marks_data_records_null_aware_under_a_null_policy() {
        let settings = ParserSettings {
            nulls: Nulls::Mysql,
            ..ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT)
        };
        let input = b"a,\n";
        let mut engine = Engine::from_config(input, settings);

        let record = engine
            .next_physical_record(input, false)
            .expect("valid record")
            .expect("a record is present");
        assert!(record.is_null_aware());

        let field =
            Option::<&[u8]>::decode_field_from_record(&record, 1, "field").expect("field decodes");
        assert_eq!(field, Some(&b""[..]));
    }

    /// Without any NULL policy (`Nulls::None`), a data record stays
    /// legacy-not-null-aware: an empty field decodes to `None`, exactly as it
    /// would for ordinary CSV with no NULL concept at all.
    #[test]
    fn next_physical_record_leaves_data_records_not_null_aware_without_a_null_policy() {
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        assert_eq!(settings.nulls, Nulls::None);
        let input = b"a,\n";
        let mut engine = Engine::from_config(input, settings);

        let record = engine
            .next_physical_record(input, false)
            .expect("valid record")
            .expect("a record is present");
        assert!(!record.is_null_aware());

        let field =
            Option::<&[u8]>::decode_field_from_record(&record, 1, "field").expect("field decodes");
        assert_eq!(field, None);
    }

    #[test]
    fn owned_storage_skip_and_capacity_state_are_directly_observable() {
        let input = b"\na\n";
        let mut skipped = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        skipped.blank_records = BlankRecords::Skip;
        skipped.skips_records = true;
        let mut skipped_output = RecordStorage::new();
        assert!(
            skipped
                .read_physical_storage::<Dynamic>(input, &mut skipped_output, false)
                .expect("valid record")
        );
        assert_eq!(skipped_output.byte_range(), 1..3);
        assert_eq!(skipped_output.get(0), Some(&b"a"[..]));

        let mut preserved = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        preserved.blank_records = BlankRecords::Skip;
        preserved.skips_records = false;
        let mut preserved_output = RecordStorage::new();
        assert!(
            preserved
                .read_physical_storage::<Dynamic>(input, &mut preserved_output, false)
                .expect("valid blank record")
        );
        assert_eq!(preserved_output.byte_range(), 0..1);
        assert_eq!(preserved_output.get(0), Some(&b""[..]));

        let tiny = b"a\n";
        let mut hinted = Engine::from_config(
            tiny,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        hinted.owned_hint = (32, 128);
        let mut new_output = RecordStorage::new();
        assert!(
            hinted
                .read_physical_storage::<Dynamic>(tiny, &mut new_output, false)
                .expect("valid hinted record")
        );
        assert!(new_output.field_capacity() >= 32);
        assert!(new_output.byte_capacity() >= 128);
        assert_eq!(hinted.owned_hint, (32, 128));

        let mut reusable = Engine::from_config(
            tiny,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        reusable.owned_hint = (32, 128);
        let mut allocated = RecordStorage::with_capacity(1, 1);
        assert!(
            reusable
                .read_physical_storage::<Dynamic>(tiny, &mut allocated, false)
                .expect("valid reusable record")
        );
        assert!(allocated.field_capacity() < 32);
        assert!(allocated.byte_capacity() < 128);
        assert_eq!(reusable.owned_hint, (32, 128));

        let wide = b"aa,bb,cc,dd\n";
        let mut allocated_hint = Engine::from_config(
            wide,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        allocated_hint.owned_hint = (0, 0);
        let mut allocated_wide = RecordStorage::with_capacity(4, 8);
        assert!(
            allocated_hint
                .read_physical_storage::<Dynamic>(wide, &mut allocated_wide, false)
                .expect("valid allocated wide record")
        );
        assert_eq!(
            allocated_hint.owned_hint,
            (0, 0),
            "reusable storage must not teach the brand-new-record size hint"
        );

        let mut learning = Engine::from_config(
            wide,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        learning.owned_hint = (2, 3);
        let mut learning_output = RecordStorage::new();
        assert!(
            learning
                .read_physical_storage::<Dynamic>(wide, &mut learning_output, false)
                .expect("valid wide record")
        );
        assert_eq!(learning_output.len(), 4);
        assert_eq!(learning_output.bytes_len(), 8);
        assert_eq!(learning.owned_hint, (4, 8));
    }

    #[test]
    fn general_storage_preserves_reservation_and_null_awareness() {
        let mut raw = Vec::from(&b"\""[..]);
        for _ in 0..100 {
            raw.extend_from_slice(b"\"\"");
        }
        raw.extend_from_slice(b"\"\r\n");
        let dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut reserve_engine =
            Engine::from_config(&raw, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        let mut reserved = RecordStorage::new();
        assert!(
            reserve_engine
                .read_physical_storage::<Dynamic>(&raw, &mut reserved, false)
                .expect("valid escaped record")
        );
        assert_eq!(reserved.bytes_len(), 100);
        assert!(
            reserved.byte_capacity() >= raw.len(),
            "the general copy reserves the physical record width before decoding"
        );

        let input = b"a,\r\n";
        let mut settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
        settings.nulls = Nulls::Mysql;
        let mut null_engine = Engine::from_config(input, settings);
        let mut null_output = RecordStorage::new();
        assert!(
            null_engine
                .read_physical_storage::<Dynamic>(input, &mut null_output, false)
                .expect("valid null-aware record")
        );
        assert!(null_output.null_aware());
        assert_eq!(null_output.is_null(0), Some(false));
        assert_eq!(null_output.is_null(1), Some(false));
    }

    #[test]
    fn trim_scope_remains_behaviorally_exact_when_the_fast_guard_is_absent() {
        let input = b"  header  \n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        engine.trim = Whitespace::FIELDS;
        let record = engine
            .next_physical_record(input, true)
            .expect("valid header")
            .expect("header present");
        assert_eq!(record.get(0), Some(&b"  header  "[..]));
    }

    #[test]
    fn test_read_physical_storage_eof() {
        let input = b"";
        let mut storage = RecordStorage::new();

        // Line 203: non-general at EOF
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            engine.read_physical_storage::<Dynamic>(input, &mut storage, false),
            Ok(false)
        );

        // Line 180: general parsing at EOF
        let dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut gen_engine =
            Engine::from_config(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        assert_eq!(
            gen_engine.read_physical_storage::<Dynamic>(input, &mut storage, false),
            Ok(false)
        );

        // Test failed parser and general parse error in read_physical_storage
        let mut fail_eng = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        fail_eng.failed = true;
        assert!(
            fail_eng
                .read_physical_storage::<Dynamic>(b"a,b\n", &mut storage, false)
                .is_err()
        );

        let crlf_dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let mut gen_err_eng = Engine::from_config(
            b"\"bad unterminated\n",
            ParserSettings::unheaded(crlf_dialect, Limits::DEFAULT),
        );
        assert!(
            gen_err_eng
                .read_physical_storage::<Dynamic>(b"\"bad unterminated\n", &mut storage, false)
                .is_err()
        );

        // Test presize_owned when unallocated in read_physical_storage
        let mut unalloc_storage = RecordStorage::new();
        let mut norm_eng = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(
            norm_eng.read_physical_storage::<Dynamic>(b"a,b\n", &mut unalloc_storage, false),
            Ok(true)
        );
        let byte_dialect = Dialect {
            record_ending: RecordEnding::Byte(b';'),
            ..Dialect::default()
        };
        let engine_byte = Engine::from_config(
            b"a,b;c,d;",
            ParserSettings::unheaded(byte_dialect, Limits::DEFAULT),
        );
        assert_eq!(engine_byte.find_record_ending(b"a,b;c,d;", 0), Some(4));
        assert_eq!(engine_byte.record_ending_width_at(b";", 0), Some(1));
        assert_eq!(engine_byte.record_ending_width_at(b"x", 0), None);

        #[cfg(feature = "multibyte")]
        {
            let mb_end_dialect = Dialect::new(
                b',',
                b'"',
                RecordEnding::Byte(b';'),
                crate::engine::Escape::DoubleQuote,
            )
            .unwrap()
            .with_tails(crate::config::Tail::EMPTY, crate::config::Tail::of(b";;"))
            .unwrap();
            let mb_eng = Engine::from_config(
                b";;;",
                ParserSettings::unheaded(mb_end_dialect, Limits::DEFAULT),
            );
            assert_eq!(mb_eng.record_ending_width_at(b";;;", 0), Some(2));
            assert_eq!(mb_eng.record_ending_width_at(b";x", 0), None);
        }

        // RecordEnding::CrLf and Newline in record_ending_width_at and find_record_ending
        let crlf_dialect = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::default()
        };
        let engine_crlf = Engine::from_config(
            b"foo\r\n",
            ParserSettings::unheaded(crlf_dialect, Limits::DEFAULT),
        );
        assert_eq!(engine_crlf.find_record_ending(b"foo\r\n", 0), Some(5));
        assert_eq!(engine_crlf.record_ending_width_at(b"\r\n", 0), Some(2));
        assert_eq!(engine_crlf.record_ending_width_at(b"\n", 0), None);

        let nl_dialect = Dialect {
            record_ending: RecordEnding::Newline,
            ..Dialect::default()
        };
        let engine_nl = Engine::from_config(
            b"foo\r\n",
            ParserSettings::unheaded(nl_dialect, Limits::DEFAULT),
        );
        assert_eq!(engine_nl.record_ending_width_at(b"\r\n", 0), Some(2));
        assert_eq!(engine_nl.record_ending_width_at(b"\n", 0), Some(1));

        // fill_record_spans with skips_records (comment + blank lines) and trim_spans
        let skip_input = b"#comment\n\n a , b \n";
        let mut skip_settings = ParserSettings::unheaded(
            Dialect {
                comment: Some(b'#'),
                ..Dialect::default()
            },
            Limits::DEFAULT,
        );
        skip_settings.blank_records = crate::config::BlankRecords::Skip;
        skip_settings.trim = crate::config::Whitespace::ALL;
        let mut skip_eng = Engine::from_config(skip_input, skip_settings);
        let res = skip_eng.fill_record_spans::<Dynamic>(skip_input, false);
        assert!(res.unwrap().is_some());
        // second read reaches EOF
        assert_eq!(
            skip_eng
                .fill_record_spans::<Dynamic>(skip_input, false)
                .unwrap(),
            None
        );

        // fill_record_spans when failed is true
        let mut fail_eng = Engine::from_config(
            b"a,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        fail_eng.failed = true;
        assert!(
            fail_eng
                .fill_record_spans::<Dynamic>(b"a,b\n", false)
                .is_err()
        );

        // skip_blank_lines when blank_records != Skip
        let mut no_skip_eng = Engine::from_config(
            b"\n\na,b\n",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        no_skip_eng.skip_blank_lines(b"\n\na,b\n");
        assert_eq!(no_skip_eng.location, 0);
    }
}

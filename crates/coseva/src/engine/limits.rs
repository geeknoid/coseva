//! Field-count, scratch, and record limit checks, and error construction.

use super::*;

impl Engine {
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: making the record path generic over the format perturbs LLVM's inlining order and spills this callee out of the hot loop, costing ~5% of parsing instructions"
    )]
    pub(crate) fn validate_field_count(
        &mut self,
        input: &[u8],
        actual: usize,
    ) -> Result<(), Error> {
        let expected = match self.field_count {
            FieldCount::Flexible => return Ok(()),
            FieldCount::Exact(expected) => expected,
            FieldCount::MatchFirst => {
                if let Some(expected) = self.expected_fields {
                    expected
                } else {
                    self.expected_fields = Some(actual);
                    return Ok(());
                }
            }
        };
        if actual == expected {
            Ok(())
        } else {
            Err(self.error(
                input,
                ErrorKind::FieldCountMismatch { expected, actual },
                self.location,
            ))
        }
    }

    pub(crate) fn scan_end(&self, input: &[u8], record_start: usize, field_start: usize) -> usize {
        let record_end =
            record_start.saturating_add(self.limits.max_record_bytes.saturating_add(1));
        let field_end = field_start.saturating_add(self.limits.max_field_bytes.saturating_add(1));
        let input_end = input.len();
        // gamma::skip(expr.decrement, reason = "shrinking the combined scanner boundary before its limiting byte makes the parser repeatedly request a wider window without reaching that byte")
        cmp::min(input_end, cmp::min(record_end, field_end))
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: making the record path generic over the format perturbs LLVM's inlining order and spills this callee out of the hot loop, costing ~5% of parsing instructions"
    )]
    pub(super) fn skip_delimiter_spaces<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
    ) -> Result<(), Error> {
        self.skip_delimiter_spaces_for::<F>(input, record_start, self.spans.len())
    }

    pub(super) fn check_scan_end(
        &self,
        input: &[u8],
        at: usize,
        record_start: usize,
        field_start: usize,
    ) -> Result<(), Error> {
        self.check_scan_end_for(input, at, record_start, field_start, self.spans.len())
    }

    pub(super) fn skip_delimiter_spaces_for<F: CsvFormat>(
        &mut self,
        input: &[u8],
        record_start: usize,
        field: usize,
    ) -> Result<(), Error> {
        if !self.fmt_skip_initial_space::<F>() || self.location == record_start {
            return Ok(());
        }
        while input.get(self.location) == Some(&b' ') {
            // gamma::skip(stmt.delete_assign, reason = "not consuming a delimiter-adjacent space leaves the loop on the same byte forever")
            // gamma::skip(literal.int_decrement, reason = "a zero increment leaves delimiter-space skipping on the same byte forever")
            self.location += 1;
            self.check_record_limit_for(input, record_start, self.location, field)?;
        }
        Ok(())
    }

    pub(super) fn check_scan_end_for(
        &self,
        input: &[u8],
        at: usize,
        record_start: usize,
        field_start: usize,
        field: usize,
    ) -> Result<(), Error> {
        if at
            .checked_sub(field_start)
            .is_some_and(|field_bytes| field_bytes > self.limits.max_field_bytes)
        {
            return Err(self.error_for(
                input,
                ErrorKind::FieldTooLarge {
                    limit: self.limits.max_field_bytes,
                },
                at,
                field,
            ));
        }
        self.check_record_limit_for(input, record_start, at, field)
    }

    pub(super) fn check_scratch_limit_for(
        &self,
        input: &[u8],
        scratch_start: usize,
        scratch_len: usize,
        at: usize,
        field: usize,
    ) -> Result<(), Error> {
        if scratch_len - scratch_start > self.limits.max_field_bytes {
            Err(self.error_for(
                input,
                ErrorKind::FieldTooLarge {
                    limit: self.limits.max_field_bytes,
                },
                at,
                field,
            ))
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the wrapper must fold span-storage lengths into the general parser's hot field path"
    )]
    pub(super) fn check_scratch_limit(
        &self,
        input: &[u8],
        scratch_start: usize,
        at: usize,
    ) -> Result<(), Error> {
        self.check_scratch_limit_for(
            input,
            scratch_start,
            self.spans.scratch_len(),
            at,
            self.spans.len(),
        )
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "measured: making the record path generic over the format perturbs LLVM's inlining order and spills this callee out of the hot loop, costing ~5% of parsing instructions"
    )]
    pub(super) fn check_record_limit_for(
        &self,
        input: &[u8],
        record_start: usize,
        at: usize,
        field: usize,
    ) -> Result<(), Error> {
        if at - record_start > self.limits.max_record_bytes {
            Err(self.error_for(
                input,
                ErrorKind::RecordTooLarge {
                    limit: self.limits.max_record_bytes,
                },
                at,
                field,
            ))
        } else {
            Ok(())
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the wrapper must fold the current field count into the parser's hot record path"
    )]
    pub(super) fn check_record_limit(
        &self,
        input: &[u8],
        record_start: usize,
        at: usize,
    ) -> Result<(), Error> {
        self.check_record_limit_for(input, record_start, at, self.spans.len())
    }

    pub(super) fn skip_comments(&mut self, input: &[u8]) {
        let Some(comment) = self.dialect.comment else {
            return;
        };
        loop {
            let resuming = self.resume.ignored
                && self.resume.record_start == self.location
                && input.get(self.resume.scanned_to..).is_some();
            if !resuming && input.get(self.location) != Some(&comment) {
                return;
            }
            let comment_start = self.location;
            let scan_start = [comment_start, self.resume.scanned_to][usize::from(resuming)];
            if let Some(end) = self.find_record_ending(input, scan_start) {
                // gamma::skip(stmt.delete_assign, reason = "not moving to the confirmed comment ending makes the outer comment loop rescan the same ignored record forever")
                // gamma::skip(assign_value.default, reason = "resetting the cursor to byte zero after a comment makes successive iterations rediscover the same comment and exhaust memory")
                self.location = end;
                self.resume = ResumeState::new();
            } else {
                // Keep enough overlap to confirm a split CRLF or multi-byte
                // terminator tail on the next, wider window.
                let overlap = self.dialect.ending_tail().width().max(2);
                let scanned_to = input.len().saturating_sub(overlap).max(comment_start);
                self.resume = ResumeState::ignored(comment_start, scanned_to);
                // gamma::skip(assign_value.default, reason = "resetting an incomplete comment cursor to zero makes each wider streaming window rescan from the source start and grow without bound")
                self.location = input.len();
                return;
            }
        }
    }

    pub(super) const fn is_terminator(&self, byte: u8) -> bool {
        byte == self.dialect.record_ending.byte()
    }

    pub(crate) fn error(&self, input: &[u8], kind: ErrorKind, byte: usize) -> Error {
        self.error_for(input, kind, byte, self.spans.len())
    }

    pub(super) fn error_for(
        &self,
        input: &[u8],
        kind: ErrorKind,
        byte: usize,
        field: usize,
    ) -> Error {
        Error::new(
            kind,
            Location {
                byte,
                line: physical_line(input, self.line_base, self.line_origin, byte),
                record: self.record_index,
                field,
            },
        )
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn engine(input: &[u8], limits: Limits) -> Engine {
        Engine::from_config(input, ParserSettings::unheaded(Dialect::default(), limits))
    }

    fn assert_error(error: Error, kind: ErrorKind, byte: usize, field: usize) {
        assert_eq!(error.kind(), kind);
        assert_eq!(error.location().byte, byte);
        assert_eq!(error.location().field, field);
    }

    #[test]
    fn limit_helpers_report_exact_boundaries_and_locations() {
        let input = b"0123456789";

        let field_engine = engine(input, Limits::new(100, 3, 10));
        assert!(field_engine.check_scan_end_for(input, 6, 0, 3, 5).is_ok());
        assert_error(
            field_engine
                .check_scan_end_for(input, 7, 0, 3, 5)
                .expect_err("four bytes exceed the three-byte field limit"),
            ErrorKind::FieldTooLarge { limit: 3 },
            7,
            5,
        );
        assert_error(
            field_engine
                .check_scan_end(input, 7, 0, 3)
                .expect_err("the wrapper must report the engine's current field"),
            ErrorKind::FieldTooLarge { limit: 3 },
            7,
            0,
        );

        let record_first = engine(input, Limits::new(1, 100, 10));
        assert_error(
            record_first
                .check_scan_end_for(input, 2, 0, 3, 5)
                .expect_err("an offset before field_start still checks the record limit"),
            ErrorKind::RecordTooLarge { limit: 1 },
            2,
            5,
        );

        let scratch_engine = engine(input, Limits::new(100, 3, 10));
        assert!(
            scratch_engine
                .check_scratch_limit_for(input, 2, 5, 9, 7)
                .is_ok()
        );
        assert_error(
            scratch_engine
                .check_scratch_limit_for(input, 2, 6, 9, 7)
                .expect_err("four new scratch bytes exceed the limit"),
            ErrorKind::FieldTooLarge { limit: 3 },
            9,
            7,
        );

        let mut scratch_wrapper = engine(input, Limits::new(100, 3, 10));
        scratch_wrapper.spans.scratch_extend_from_slice(b"12345");
        assert_error(
            scratch_wrapper
                .check_scratch_limit(input, 1, 8)
                .expect_err("the wrapper must use the real scratch length and field count"),
            ErrorKind::FieldTooLarge { limit: 3 },
            8,
            0,
        );

        let record_engine = engine(input, Limits::new(3, 100, 10));
        assert_error(
            record_engine
                .check_record_limit_for(input, 1, 5, 7)
                .expect_err("four bytes exceed the record limit"),
            ErrorKind::RecordTooLarge { limit: 3 },
            5,
            7,
        );
        assert_error(
            record_engine
                .check_record_limit(input, 1, 5)
                .expect_err("the wrapper must report the current field"),
            ErrorKind::RecordTooLarge { limit: 3 },
            5,
            0,
        );
        let direct = record_engine.error(input, ErrorKind::UnexpectedQuote, 4);
        assert_eq!(direct.location().byte, 4);
        assert_eq!(direct.location().field, 0);
    }

    #[test]
    fn delimiter_space_limit_uses_the_advanced_cursor_and_requested_field() {
        let input = b"xxx x";
        let mut direct = engine(input, Limits::new(2, 100, 10));
        direct.skip_initial_space = true;
        direct.location = 3;
        assert_error(
            direct
                .skip_delimiter_spaces_for::<Dynamic>(input, 1, 7)
                .expect_err("the consumed space crosses the record limit"),
            ErrorKind::RecordTooLarge { limit: 2 },
            4,
            7,
        );

        let mut wrapped = engine(input, Limits::new(2, 100, 10));
        wrapped.skip_initial_space = true;
        wrapped.location = 3;
        assert_error(
            wrapped
                .skip_delimiter_spaces::<Dynamic>(input, 1)
                .expect_err("the wrapper reports its current field"),
            ErrorKind::RecordTooLarge { limit: 2 },
            4,
            0,
        );
    }

    #[test]
    fn incomplete_comments_keep_the_exact_resume_overlap() {
        let mut dialect = Dialect {
            comment: Some(b'#'),
            ..Dialect::default()
        };
        let input = b"#abcdef";
        let mut default_tail =
            Engine::from_config(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        default_tail.skip_comments(input);
        assert_eq!(default_tail.location, input.len());
        assert!(default_tail.resume.ignored);
        assert_eq!(default_tail.resume.record_start, 0);
        assert_eq!(default_tail.resume.scanned_to, input.len() - 2);

        let near_end = b"data\n#";
        let mut near_end_engine =
            Engine::from_config(near_end, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        near_end_engine.location = 5;
        near_end_engine.skip_comments(near_end);
        assert_eq!(near_end_engine.resume.record_start, 5);
        assert_eq!(near_end_engine.resume.scanned_to, 5);
        assert_eq!(near_end_engine.location, near_end.len());

        let complete = b"#x\nz";
        let mut completed_engine =
            Engine::from_config(complete, ParserSettings::unheaded(dialect, Limits::DEFAULT));
        completed_engine.resume = ResumeState::ignored(0, 1);
        completed_engine.skip_comments(complete);
        assert_eq!(completed_engine.location, 3);
        assert!(!completed_engine.resume.ignored);
        assert_eq!(completed_engine.resume.record_start, NO_OFFSET);
        assert_eq!(completed_engine.resume.scanned_to, NO_OFFSET);

        #[cfg(feature = "multibyte")]
        {
            dialect = Dialect::new(b',', b'"', RecordEnding::Byte(b';'), Escape::DoubleQuote)
                .expect("valid dialect")
                .with_tails(crate::config::Tail::EMPTY, crate::config::Tail::of(b";XYZ"))
                .expect("valid tails");
            dialect.comment = Some(b'#');
            let input = b"#123456789";
            let mut wide_tail =
                Engine::from_config(input, ParserSettings::unheaded(dialect, Limits::DEFAULT));
            wide_tail.skip_comments(input);
            assert_eq!(wide_tail.resume.record_start, 0);
            assert_eq!(
                wide_tail.resume.scanned_to,
                input.len() - 4,
                "the four-byte terminator needs four bytes of overlap"
            );
        }
    }

    #[test]
    fn test_limits_coverage() {
        let input = b"a,   b\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(3, 10, 10)),
        );
        engine.skip_initial_space = true;
        engine.location = 2; // on space
        assert!(
            engine
                .skip_delimiter_spaces_for::<Dynamic>(input, 0, 1)
                .is_err()
        );

        let engine2 = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::new(100, 2, 10)),
        );
        assert!(engine2.check_scan_end_for(input, 10, 0, 0, 0).is_err());
        assert!(engine2.check_scan_end_for(input, 0, 0, 5, 0).is_ok());
    }
}

//! Engine construction, configuration, and reset.

use super::*;

impl Engine {
    /// Create engine state for `options`, specializing the owned-record
    /// parser for the window when the configuration allows it.
    pub(crate) fn from_config(input: &[u8], options: ParserSettings) -> Self {
        let check_field_limit = input.len() > Limits::DEFAULT.max_field_bytes;
        Self::from_config_checking(input, options, check_field_limit)
    }

    /// Build a parser over a growing window, so field limits always apply.
    ///
    /// A slice knows its final width up front; a widening window does not.
    pub(crate) fn from_config_windowed(input: &[u8], options: ParserSettings) -> Self {
        let windowed_default =
            options.dialect == Dialect::CSV && Self::owned_parser_for(&options, true).is_some();
        let mut engine = Self::from_config_checking(input, options, true);
        if windowed_default {
            engine.owned_parser = Some(try_parse_default_record_windowed);
            engine.quoted_prefix_parser = Some(try_parse_default_quoted_record_structural_windowed);
            engine.interior_prefix_parser = Some(try_parse_default_interior_prefix_windowed);
            engine.interior_handoff_parser =
                Some(try_parse_default_quoted_record_structural_windowed);
            engine.multi_quote_parser = Some(try_parse_default_quoted_record_structural_windowed);
        }
        engine
    }

    fn from_config_checking(
        input: &[u8],
        options: ParserSettings,
        check_field_limit: bool,
    ) -> Self {
        let owned_parser = Self::owned_parser_for(&options, check_field_limit);
        Self::from_config_with_owned_parser(input, options, owned_parser, check_field_limit)
    }

    /// Select the specialized whole-record parser a configuration allows.
    fn owned_parser_for(
        options: &ParserSettings,
        check_field_limit: bool,
    ) -> Option<SliceOwnedParser> {
        let eligible = options.limits == Limits::DEFAULT
            && options.field_count == FieldCount::Flexible
            && options.trim == Whitespace::NONE
            && options.blank_records == BlankRecords::Preserve
            && options.syntax == Syntax::Strict
            && !options.skip_initial_space
            && options.nulls == Nulls::None;
        let owned_parser: Option<SliceOwnedParser> = if !eligible {
            None
        } else if options.dialect == Dialect::CSV {
            Some(if check_field_limit {
                try_parse_default_record::<true>
            } else {
                try_parse_default_record::<false>
            })
        } else {
            match options.dialect {
                Dialect::TSV => Some(if check_field_limit {
                    try_parse_named_dialect_record::<b'\t', false, true>
                } else {
                    try_parse_named_dialect_record::<b'\t', false, false>
                }),
                Dialect::SEMICOLON => Some(if check_field_limit {
                    try_parse_named_dialect_record::<b';', false, true>
                } else {
                    try_parse_named_dialect_record::<b';', false, false>
                }),
                Dialect::PIPE => Some(if check_field_limit {
                    try_parse_named_dialect_record::<b'|', false, true>
                } else {
                    try_parse_named_dialect_record::<b'|', false, false>
                }),
                Dialect::BACKSLASH_CSV => Some(if check_field_limit {
                    try_parse_named_dialect_record::<b',', true, true>
                } else {
                    try_parse_named_dialect_record::<b',', true, false>
                }),
                Dialect::BACKSLASH_TSV => Some(if check_field_limit {
                    try_parse_named_dialect_record::<b'\t', true, true>
                } else {
                    try_parse_named_dialect_record::<b'\t', true, false>
                }),
                _ => None,
            }
        };
        owned_parser
    }

    /// Select the quoted-head parser, which only the default dialect has.
    ///
    /// It shares every eligibility condition with the whole-record parser, so
    /// it is offered only where that one already is.
    fn quoted_prefix_parser_for(
        options: &ParserSettings,
        owned_parser: bool,
        check_field_limit: bool,
    ) -> Option<SliceQuotedPrefixParser> {
        // gamma::skip(cond.always_false, reason = "mutation causes non-termination or unbounded resource use")
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        if !owned_parser || options.dialect != Dialect::CSV {
            return None;
        }
        Some(if check_field_limit {
            try_parse_default_quoted_prefix::<true>
        } else {
            try_parse_default_quoted_prefix::<false>
        })
    }

    /// Select the interior-head parser, offered wherever the quoted-head one is.
    ///
    /// It reads a predicted interior-quoted record's plain prefix and the
    /// quoted field that follows, so the plain tail after that quote returns to
    /// the vectorized kernel instead of the whole record staying scalar.
    fn interior_prefix_parser_for(
        options: &ParserSettings,
        owned_parser: bool,
        check_field_limit: bool,
    ) -> Option<SliceInteriorPrefixParser> {
        if !owned_parser || options.dialect != Dialect::CSV {
            return None;
        }
        Some(if check_field_limit {
            try_parse_default_interior_prefix::<true>
        } else {
            try_parse_default_interior_prefix::<false>
        })
    }

    fn multi_quote_parser_for(
        options: &ParserSettings,
        owned_parser: bool,
        check_field_limit: bool,
    ) -> Option<SliceInteriorPrefixParser> {
        if !owned_parser || options.dialect != Dialect::CSV {
            return None;
        }
        Some(if check_field_limit {
            try_parse_default_quoted_record_structural_appending::<true>
        } else {
            try_parse_default_quoted_record_structural_appending::<false>
        })
    }

    pub(crate) fn from_config_with_owned_parser(
        input: &[u8],
        options: ParserSettings,
        owned_parser: Option<SliceOwnedParser>,
        check_field_limit: bool,
    ) -> Self {
        let general_parsing = needs_general_parsing(options.dialect, options.trim);
        let plain_kernel = plain_kernel(&options);
        let quoted_prefix_parser =
            Self::quoted_prefix_parser_for(&options, owned_parser.is_some(), check_field_limit);
        let interior_prefix_parser =
            Self::interior_prefix_parser_for(&options, owned_parser.is_some(), check_field_limit);
        let multi_quote_parser =
            Self::multi_quote_parser_for(&options, owned_parser.is_some(), check_field_limit);
        let format_kind = FormatKind::of(&options);
        let location =
            usize::from(options.bom == ReadBom::Detect && input.starts_with(b"\xEF\xBB\xBF")) * 3;
        let (consume_first_record, header_record, headers_initialized) = match options.headers {
            Headers::None => (false, None, true),
            Headers::FirstRecord => (true, None, false),
            Headers::Provided(record) => (false, Some(record), true),
        };
        let expected_fields = if options.field_count == FieldCount::MatchFirst {
            header_record.as_ref().map(ByteRecord::len)
        } else {
            None
        };
        let reader = Self {
            location,
            line_base: 1,
            line_origin: 0,
            folded_upto: 0,
            folded_lines: 0,
            record_index: 0,
            dialect: options.dialect,
            format_kind,
            limits: options.limits,
            field_count: options.field_count,
            expected_fields,
            consume_first_record,
            header_record,
            header_lookup: HeaderLookup::default(),
            header_lookup_ready: false,
            filter_column: None,
            typed_mapping: None,
            #[cfg(feature = "serde")]
            serde_cache: StructCache::new(),
            #[cfg(feature = "serde")]
            serde_ready: false,
            headers_initialized,
            trim: options.trim,
            blank_records: options.blank_records,
            syntax: options.syntax,
            skip_initial_space: options.skip_initial_space,
            nulls: options.nulls,
            general_parsing,
            plain_kernel,
            record_pass: needs_record_pass(options.dialect, options.nulls),
            interior_quotes: 0,
            ascii_structural_backoff: 0,
            ascii_structural_succeeded: false,
            block_cache: BlockCache::new(),
            skips_records: options.dialect.comment.is_some()
                || options.blank_records == BlankRecords::Skip,
            // Every record parsed through a span-building route fills this, so
            // leaving it empty makes the first record pay for two growth
            // reallocations before any parser can report anything. Reserving a
            // typical record's worth up front moves that off the parse path
            // and onto construction. `scratch` gets no such treatment on
            // purpose: only escaped fields touch it, so a document without any
            // should never allocate it at all.
            spans: SpanStorage::with_capacity(TYPICAL_FIELDS),
            owned_scratch: ByteRecord::new(),
            staged_record: None,
            staged_valid: false,
            staged_form_owned: false,
            owned_parser,
            quoted_prefix_parser,
            interior_prefix_parser,
            interior_handoff_parser: multi_quote_parser,
            multi_quote_parser,
            filter_backoff: 0,
            owned_hint: (0, 0),
            cursor_start: NO_OFFSET,
            cursor_index: 0,
            cursor_end: NO_OFFSET,
            resume: ResumeState::new(),
            failed: false,
            terminated: false,
        };
        reader
    }

    /// Capacities of the reusable parse buffers, for tests asserting that a
    /// reset recycles them rather than reallocating.
    #[cfg(test)]
    pub(crate) fn buffer_capacities(&self) -> (usize, usize) {
        (self.spans.capacity(), self.spans.scratch_capacity())
    }

    /// Restart over an empty window under `options`, reusing every buffer.
    ///
    /// Observable state matches a fresh parser; grown parse and cache
    /// allocations are retained.
    pub(crate) fn reset_for(&mut self, options: &ParserSettings) {
        // An empty window can neither carry a BOM nor exceed a field limit.
        self.owned_parser = Self::owned_parser_for(options, false);
        self.quoted_prefix_parser =
            Self::quoted_prefix_parser_for(options, self.owned_parser.is_some(), false);
        self.interior_prefix_parser =
            Self::interior_prefix_parser_for(options, self.owned_parser.is_some(), false);
        self.interior_handoff_parser = self.interior_prefix_parser;
        self.multi_quote_parser =
            Self::multi_quote_parser_for(options, self.owned_parser.is_some(), false);

        let (consume_first_record, header_record, headers_initialized) = match &options.headers {
            Headers::None => (false, None, true),
            Headers::FirstRecord => (true, None, false),
            Headers::Provided(record) => (false, Some(record.clone()), true),
        };
        self.expected_fields = if options.field_count == FieldCount::MatchFirst {
            header_record.as_ref().map(ByteRecord::len)
        } else {
            None
        };
        self.consume_first_record = consume_first_record;
        self.header_record = header_record;
        self.headers_initialized = headers_initialized;

        self.seek_to_exact(usize::MIN, 1, u64::MIN);
        self.dialect = options.dialect;
        self.limits = options.limits;
        self.field_count = options.field_count;
        self.trim = options.trim;
        self.blank_records = options.blank_records;
        self.syntax = options.syntax;
        self.skip_initial_space = options.skip_initial_space;
        self.nulls = options.nulls;
        self.general_parsing = needs_general_parsing(options.dialect, options.trim);
        // Deliberately not `!self.general_parsing`: see `needs_general_parsing`.
        self.format_kind = FormatKind::of(options);
        self.plain_kernel = plain_kernel(options);
        self.record_pass = needs_record_pass(options.dialect, options.nulls);
        self.interior_quotes -= self.interior_quotes;
        self.skips_records =
            options.dialect.comment.is_some() || options.blank_records == BlankRecords::Skip;

        self.spans.clear();
        self.owned_scratch.clear();
        #[cfg(feature = "serde")]
        self.serde_cache.reset();

        // Drops the header lookup and resyncs the Serde cache in place.
        self.on_headers_changed();
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Recovery;

    fn parser_accepts(parser: SliceOwnedParser, input: &[u8]) -> bool {
        let mut output = ByteRecord::new();
        parser(input, &mut output.storage).is_some()
    }

    fn prefix_accepts(parser: SliceInteriorPrefixParser, input: &[u8]) -> bool {
        let mut output = ByteRecord::new();
        parser(input, &mut output.storage).is_some()
    }

    #[test]
    fn specialized_parsers_match_every_named_dialect_and_limit_mode() {
        let cases = [
            (Dialect::CSV, &b"a,b\n"[..]),
            (Dialect::TSV, &b"a\tb\n"[..]),
            (Dialect::SEMICOLON, &b"a;b\n"[..]),
            (Dialect::PIPE, &b"a|b\n"[..]),
            (Dialect::BACKSLASH_CSV, &b"\"a\\\"b\",c\n"[..]),
            (Dialect::BACKSLASH_TSV, &b"\"a\\\"b\"\tc\n"[..]),
        ];
        let mut oversized = vec![b'a'; Limits::DEFAULT.max_field_bytes + 1];
        oversized.push(b'\n');

        for (dialect, sample) in cases {
            let settings = ParserSettings::unheaded(dialect, Limits::DEFAULT);
            let selected = Engine::owned_parser_for(&settings, false)
                .expect("every named dialect has an owned parser");
            assert!(parser_accepts(selected, sample));
            assert!(parser_accepts(selected, &oversized));

            let selected = Engine::owned_parser_for(&settings, true)
                .expect("every named dialect has a field-checking parser");
            assert!(parser_accepts(selected, sample));
            assert!(!parser_accepts(selected, &oversized));
        }
    }

    #[test]
    fn parser_selection_observes_eligibility_and_input_length_boundaries() {
        let settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        let exact = vec![b'a'; Limits::DEFAULT.max_field_bytes];
        let engine = Engine::from_config(&exact, settings.clone());
        let mut oversized = vec![b'a'; Limits::DEFAULT.max_field_bytes + 1];
        oversized.push(b'\n');
        assert!(parser_accepts(
            engine
                .owned_parser
                .expect("exact-bound input stays unchecked"),
            &oversized,
        ));

        let over = vec![b'a'; Limits::DEFAULT.max_field_bytes + 1];
        let engine = Engine::from_config(&over, settings.clone());
        assert!(!parser_accepts(
            engine.owned_parser.expect("over-bound input checks fields"),
            &oversized,
        ));

        let mut ineligible = settings.clone();
        let mut variants = Vec::new();
        ineligible.limits = Limits::new(32, 16, 4);
        variants.push(ineligible.clone());
        ineligible = settings.clone();
        ineligible.field_count = FieldCount::Exact(2);
        variants.push(ineligible.clone());
        ineligible = settings.clone();
        ineligible.trim = Whitespace::FIELDS;
        variants.push(ineligible.clone());
        ineligible = settings.clone();
        ineligible.blank_records = BlankRecords::Skip;
        variants.push(ineligible.clone());
        ineligible = settings.clone();
        ineligible.syntax = Syntax::Compatible(Recovery::PERMISSIVE);
        variants.push(ineligible.clone());
        ineligible = settings.clone();
        ineligible.skip_initial_space = true;
        variants.push(ineligible.clone());
        ineligible = settings;
        ineligible.nulls = Nulls::Mysql;
        variants.push(ineligible);

        for variant in variants {
            assert!(Engine::owned_parser_for(&variant, false).is_none());
            assert!(Engine::quoted_prefix_parser_for(&variant, true, false).is_some());
            assert!(Engine::quoted_prefix_parser_for(&variant, false, false).is_none());
        }

        let tsv = ParserSettings::unheaded(Dialect::TSV, Limits::DEFAULT);
        assert!(Engine::quoted_prefix_parser_for(&tsv, true, false).is_none());
        assert!(Engine::interior_prefix_parser_for(&tsv, true, false).is_none());
        assert!(Engine::multi_quote_parser_for(&tsv, true, false).is_none());
    }

    #[test]
    fn windowed_default_parsers_check_fields_only_when_the_window_can_exceed_the_limit() {
        let settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        let engine = Engine::from_config_windowed(b"", settings);
        let owned = engine.owned_parser.expect("windowed owned parser");
        let quoted = engine
            .quoted_prefix_parser
            .expect("windowed quoted-prefix parser");
        let interior = engine
            .interior_prefix_parser
            .expect("windowed interior-prefix parser");
        let structural = engine
            .multi_quote_parser
            .expect("windowed structural parser");

        assert!(parser_accepts(owned, b"a,b\n"));
        assert!(prefix_accepts(quoted, b"\"a\",b\n"));
        assert!(prefix_accepts(interior, b"a,\"b\",c\n"));
        assert!(prefix_accepts(structural, b"\"a\",\"b\"\n"));

        let mut oversized = vec![b'a'; Limits::DEFAULT.max_field_bytes + 1];
        oversized.push(b'\n');
        assert!(!parser_accepts(owned, &oversized));

        let mut quoted_oversized = Vec::with_capacity(oversized.len() + 2);
        quoted_oversized.push(b'"');
        quoted_oversized.extend_from_slice(&oversized[..oversized.len() - 1]);
        quoted_oversized.extend_from_slice(b"\"\n");
        assert!(!prefix_accepts(quoted, &quoted_oversized));

        let mut interior_oversized = b"x,".to_vec();
        interior_oversized.extend_from_slice(&quoted_oversized);
        assert!(!prefix_accepts(interior, &interior_oversized));
        assert!(!prefix_accepts(structural, &quoted_oversized));
    }

    #[test]
    fn csv_prefix_parsers_match_limit_mode_and_parser_presence() {
        let settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        let quoted_unchecked = Engine::quoted_prefix_parser_for(&settings, true, false)
            .expect("CSV has a quoted-prefix parser");
        let quoted_checked = Engine::quoted_prefix_parser_for(&settings, true, true)
            .expect("CSV has a checked quoted-prefix parser");
        let mut quoted_oversized = Vec::with_capacity(Limits::DEFAULT.max_field_bytes + 5);
        quoted_oversized.push(b'"');
        quoted_oversized.extend(std::iter::repeat_n(
            b'a',
            Limits::DEFAULT.max_field_bytes + 1,
        ));
        quoted_oversized.extend_from_slice(b"\"\n");
        assert!(prefix_accepts(quoted_unchecked, &quoted_oversized));
        assert!(!prefix_accepts(quoted_checked, &quoted_oversized));

        let interior_unchecked = Engine::interior_prefix_parser_for(&settings, true, false)
            .expect("CSV has an interior-prefix parser");
        let interior_checked = Engine::interior_prefix_parser_for(&settings, true, true)
            .expect("CSV has a checked interior-prefix parser");
        let mut interior_oversized = b"x,".to_vec();
        interior_oversized.extend_from_slice(&quoted_oversized);
        assert!(prefix_accepts(interior_unchecked, &interior_oversized));
        assert!(!prefix_accepts(interior_checked, &interior_oversized));

        assert!(Engine::quoted_prefix_parser_for(&settings, false, false).is_none());
        assert!(Engine::interior_prefix_parser_for(&settings, false, false).is_none());
        assert!(Engine::multi_quote_parser_for(&settings, false, false).is_none());

        let multi_unchecked = Engine::multi_quote_parser_for(&settings, true, false)
            .expect("CSV has a multi-quote parser");
        let multi_checked = Engine::multi_quote_parser_for(&settings, true, true)
            .expect("CSV has a checked multi-quote parser");
        assert!(prefix_accepts(multi_unchecked, &quoted_oversized));
        assert!(!prefix_accepts(multi_checked, &quoted_oversized));
    }

    #[test]
    fn construction_initializes_header_cursor_and_parser_state_exactly() {
        let mut provided = ByteRecord::new();
        provided.push_field(b"left");
        provided.push_field(b"right");

        let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        settings.headers = Headers::Provided(provided.clone());
        settings.field_count = FieldCount::MatchFirst;
        let input = b"\xEF\xBB\xBFa,b\n";
        let engine = Engine::from_config(input, settings);

        assert_eq!(engine.location, 3);
        assert_eq!(engine.line_base, 1);
        assert_eq!(engine.line_origin, 0);
        assert_eq!(engine.folded_upto, 0);
        assert_eq!(engine.folded_lines, 0);
        assert_eq!(engine.record_index, 0);
        assert_eq!(engine.dialect, Dialect::CSV);
        assert_eq!(
            engine.format_kind,
            FormatKind::of(&ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT))
        );
        assert_eq!(engine.limits, Limits::DEFAULT);
        assert_eq!(engine.field_count, FieldCount::MatchFirst);
        assert_eq!(engine.expected_fields, Some(2));
        assert!(!engine.consume_first_record);
        assert_eq!(engine.header_record, Some(provided));
        assert!(engine.headers_initialized);
        assert!(!engine.header_lookup_ready);
        assert!(engine.filter_column.is_none());
        assert!(engine.typed_mapping.is_none());
        assert_eq!(engine.trim, Whitespace::NONE);
        assert_eq!(engine.blank_records, BlankRecords::Preserve);
        assert_eq!(engine.syntax, Syntax::Strict);
        assert!(!engine.skip_initial_space);
        assert_eq!(engine.nulls, Nulls::None);
        assert!(!engine.general_parsing);
        assert!(engine.plain_kernel);
        assert!(!engine.record_pass);
        assert!(!engine.skips_records);
        assert!(engine.spans.capacity() >= TYPICAL_FIELDS);
        assert!(engine.owned_scratch.is_empty());
        assert!(engine.staged_record.is_none());
        assert!(!engine.staged_valid);
        assert!(!engine.staged_form_owned);
        assert!(engine.owned_parser.is_none());
        assert!(engine.quoted_prefix_parser.is_none());
        assert!(engine.interior_prefix_parser.is_none());
        assert!(engine.interior_handoff_parser.is_none());
        assert!(engine.multi_quote_parser.is_none());
        assert_eq!(engine.interior_quotes, 0);
        assert_eq!(engine.filter_backoff, 0);
        assert_eq!(engine.owned_hint, (0, 0));
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_index, 0);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
        assert!(!engine.failed);
        assert!(!engine.terminated);
    }

    #[test]
    fn construction_applies_each_header_and_skip_policy() {
        let mut provided = ByteRecord::new();
        provided.push_field(b"x");
        provided.push_field(b"y");

        for (headers, consume, initialized, width) in [
            (Headers::None, false, true, None),
            (Headers::FirstRecord, true, false, None),
            (Headers::Provided(provided.clone()), false, true, Some(2)),
        ] {
            let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
            settings.headers = headers;
            settings.field_count = FieldCount::MatchFirst;
            let engine = Engine::from_config(b"a,b\n", settings);
            assert_eq!(engine.consume_first_record, consume);
            assert_eq!(engine.headers_initialized, initialized);
            assert_eq!(engine.expected_fields, width);
        }

        let mut flexible = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        flexible.headers = Headers::Provided(provided);
        let engine = Engine::from_config(b"a,b\n", flexible);
        assert_eq!(engine.expected_fields, None);

        let mut skipping = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        skipping.dialect.comment = Some(b'#');
        assert!(Engine::from_config(b"", skipping.clone()).skips_records);
        skipping.dialect.comment = None;
        skipping.blank_records = BlankRecords::Skip;
        assert!(Engine::from_config(b"", skipping).skips_records);
    }

    #[test]
    fn reset_replaces_configuration_and_clears_all_observable_state() {
        let input = b"old,value\n";
        let mut engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT),
        );
        assert!(
            engine
                .advance::<Dynamic>(input)
                .expect("old record positions")
        );
        let _ = engine.record::<Dynamic>(input).expect("old record parses");
        engine.owned_scratch.push_field(b"stale");
        engine.store_filter_column(b"old", 7);
        engine.header_lookup_ready = true;
        engine.location = 8;
        engine.line_base = 9;
        engine.line_origin = 7;
        engine.folded_upto = 6;
        engine.folded_lines = 5;
        engine.record_index = 4;
        engine.interior_quotes = 3;
        engine.filter_backoff = 2;
        engine.cursor_start = 1;
        engine.cursor_index = 2;
        engine.cursor_end = 3;
        engine.resume = ResumeState::ignored(1, 3);
        engine.failed = true;
        engine.terminated = true;
        engine.staged_valid = true;
        #[cfg(feature = "serde")]
        {
            let mut cached_headers = ByteRecord::new();
            cached_headers.push_field(b"cached");
            engine.serde_cache.sync(Some(&cached_headers));
            engine.serde_ready = true;
            assert!(format!("{:?}", engine.serde_cache).contains("cached"));
        }

        let mut headers = ByteRecord::new();
        headers.push_field(b"a");
        headers.push_field(b"b");
        let mut settings =
            ParserSettings::unheaded(Dialect::PYTHON_ESCAPED, Limits::new(91, 17, 3));
        settings.dialect.comment = Some(b'#');
        settings.headers = Headers::Provided(headers.clone());
        settings.field_count = FieldCount::MatchFirst;
        settings.trim = Whitespace::FIELDS.unquoted_only();
        settings.blank_records = BlankRecords::Skip;
        settings.syntax = Syntax::Compatible(Recovery::PERMISSIVE);
        settings.skip_initial_space = true;
        settings.nulls = Nulls::Mysql;

        engine.reset_for(&settings);

        assert_eq!(engine.location, 0);
        assert_eq!(engine.line_base, 1);
        assert_eq!(engine.line_origin, 0);
        assert_eq!(engine.folded_upto, 0);
        assert_eq!(engine.folded_lines, 0);
        assert_eq!(engine.record_index, 0);
        assert_eq!(engine.dialect, settings.dialect);
        assert_eq!(engine.format_kind, FormatKind::of(&settings));
        assert_eq!(engine.limits, settings.limits);
        assert_eq!(engine.field_count, FieldCount::MatchFirst);
        assert_eq!(engine.expected_fields, Some(2));
        assert!(!engine.consume_first_record);
        assert_eq!(engine.header_record, Some(headers));
        assert!(engine.headers_initialized);
        assert_eq!(engine.trim, settings.trim);
        assert_eq!(engine.blank_records, BlankRecords::Skip);
        assert_eq!(engine.syntax, settings.syntax);
        assert!(engine.skip_initial_space);
        assert_eq!(engine.nulls, Nulls::Mysql);
        assert!(engine.general_parsing);
        assert!(!engine.plain_kernel);
        assert!(engine.record_pass);
        assert!(engine.skips_records);
        assert_eq!(engine.spans.len(), 0);
        assert!(engine.owned_scratch.is_empty());
        assert!(!engine.header_lookup_ready);
        assert!(engine.filter_column.is_none());
        assert_eq!(engine.interior_quotes, 0);
        assert_eq!(engine.filter_backoff, 0);
        assert_eq!(engine.cursor_start, NO_OFFSET);
        assert_eq!(engine.cursor_index, 0);
        assert_eq!(engine.cursor_end, NO_OFFSET);
        assert_eq!(engine.resume.record_start, NO_OFFSET);
        assert!(!engine.failed);
        assert!(!engine.terminated);
        assert!(engine.owned_parser.is_none());
        assert!(engine.quoted_prefix_parser.is_none());
        assert!(engine.interior_prefix_parser.is_none());
        assert!(engine.interior_handoff_parser.is_none());
        assert!(engine.multi_quote_parser.is_none());
        #[cfg(feature = "serde")]
        {
            assert!(!engine.serde_ready);
            assert!(!format!("{:?}", engine.serde_cache).contains("cached"));
        }

        let mut blank_only = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        blank_only.blank_records = BlankRecords::Skip;
        engine.reset_for(&blank_only);
        assert!(engine.skips_records);

        let mut comment_only = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        comment_only.dialect.comment = Some(b'#');
        engine.reset_for(&comment_only);
        assert!(engine.skips_records);

        let new_input = b"new,value\n";
        assert!(
            engine
                .advance::<Dynamic>(new_input)
                .expect("reset engine positions a new record")
        );
        let record = engine
            .record::<Dynamic>(new_input)
            .expect("reset engine parses with a fresh cache");
        assert_eq!(record.get(0), Some(&b"new"[..]));
        assert_eq!(record.get(1), Some(&b"value"[..]));

        let eligible = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        engine.reset_for(&eligible);
        assert!(engine.plain_kernel);
        let mut oversized = vec![b'a'; Limits::DEFAULT.max_field_bytes + 1];
        oversized.push(b'\n');
        assert!(parser_accepts(
            engine
                .owned_parser
                .expect("reset restores the owned parser"),
            &oversized,
        ));

        let mut quoted_oversized = Vec::with_capacity(Limits::DEFAULT.max_field_bytes + 5);
        quoted_oversized.push(b'"');
        quoted_oversized.extend(std::iter::repeat_n(
            b'a',
            Limits::DEFAULT.max_field_bytes + 1,
        ));
        quoted_oversized.extend_from_slice(b"\"\n");
        assert!(prefix_accepts(
            engine
                .quoted_prefix_parser
                .expect("reset restores the quoted-prefix parser"),
            &quoted_oversized,
        ));
        assert!(prefix_accepts(
            engine
                .interior_prefix_parser
                .expect("reset restores the interior-prefix parser"),
            &{
                let mut input = b"x,".to_vec();
                input.extend_from_slice(&quoted_oversized);
                input
            },
        ));
        assert!(engine.interior_handoff_parser.is_some());
        assert!(prefix_accepts(
            engine
                .multi_quote_parser
                .expect("reset restores the multi-quote parser"),
            &quoted_oversized,
        ));
    }

    #[test]
    fn reset_applies_each_header_policy_and_first_record_width_rule() {
        let mut engine =
            Engine::from_config(b"", ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT));
        let mut provided = ByteRecord::new();
        provided.push_field(b"x");
        provided.push_field(b"y");
        provided.push_field(b"z");

        for (headers, consume, initialized, width) in [
            (Headers::None, false, true, None),
            (Headers::FirstRecord, true, false, None),
            (Headers::Provided(provided.clone()), false, true, Some(3)),
        ] {
            let mut settings = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
            settings.headers = headers;
            settings.field_count = FieldCount::MatchFirst;
            engine.consume_first_record = !consume;
            engine.headers_initialized = !initialized;
            engine.expected_fields = Some(99);
            engine.reset_for(&settings);
            assert_eq!(engine.consume_first_record, consume);
            assert_eq!(engine.headers_initialized, initialized);
            assert_eq!(engine.expected_fields, width);
        }

        let mut flexible = ParserSettings::unheaded(Dialect::CSV, Limits::DEFAULT);
        flexible.headers = Headers::Provided(provided);
        flexible.field_count = FieldCount::Flexible;
        engine.expected_fields = Some(99);
        engine.reset_for(&flexible);
        assert_eq!(engine.expected_fields, None);
    }
}

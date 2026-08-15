#![cfg_attr(coverage_nightly, coverage(off))]

//! Format-parameter accessors, resolved statically or from the dialect.

use super::*;

#[cfg_attr(coverage_nightly, coverage(off))]
impl Engine {
    // ── Compile-time format accessors ───────────────────────────────────────
    //
    // Static formats fold `F::OPTIONS` to immediates; `Dynamic` keeps one
    // engine load. These must stay `#[inline(always)]` so the fold happens in
    // the calling kernel.

    #[inline(always)]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_delimiter<F: CsvFormat>(&self) -> u8 {
        match F::OPTIONS {
            Some(options) => options.dialect.delimiter,
            None => self.dialect.delimiter,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(crate) fn fmt_quote<F: CsvFormat>(&self) -> u8 {
        match F::OPTIONS {
            Some(options) => options.dialect.quote,
            None => self.dialect.quote,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(crate) fn fmt_terminator<F: CsvFormat>(&self) -> u8 {
        match F::OPTIONS {
            Some(options) => options.dialect.record_ending.byte(),
            None => self.dialect.record_ending.byte(),
        }
    }

    /// What must follow the delimiter byte for it to delimit.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_delimiter_tail<F: CsvFormat>(&self) -> Tail {
        match F::OPTIONS {
            Some(options) => options.dialect.delimiter_tail(),
            None => self.dialect.delimiter_tail(),
        }
    }

    /// What must follow the terminator byte for it to terminate.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_ending_tail<F: CsvFormat>(&self) -> Tail {
        match F::OPTIONS {
            Some(options) => options.dialect.ending_tail(),
            None => self.dialect.ending_tail(),
        }
    }

    /// Whether a `\r` before the terminator belongs to the terminator.    ///
    /// Compare `RecordEnding`, not its byte: `Byte(b'\n')` is explicit single-
    /// byte `\n`, so a preceding `\r` stays in the field.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_strips_cr<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => matches!(
                options.dialect.record_ending,
                RecordEnding::Newline | RecordEnding::CrLf
            ),
            None => matches!(
                self.dialect.record_ending,
                RecordEnding::Newline | RecordEnding::CrLf
            ),
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_quoting_enabled<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => options.syntax.quoting_enabled(),
            None => self.syntax.quoting_enabled(),
        }
    }

    /// Whether the plain borrowed kernel may run for this format.
    ///
    /// Only `skip_initial_space` rules it out outright; every dialect that once
    /// did is now either agreed with or declined per record. Static formats
    /// decide this at compile time, removing the per-record load and branch.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_plain_kernel<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => !options.skip_initial_space,
            None => self.plain_kernel,
        }
    }

    /// The byte that escapes inside an unquoted field, if this dialect has
    /// one.
    ///
    /// `None` for the dialects that escape only inside quotes, which is what
    /// lets the kernel skip the search entirely.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_unquoted_escape<F: CsvFormat>(&self) -> Option<u8> {
        match F::OPTIONS {
            Some(options) => options.dialect.escape.unquoted_byte(),
            None => self.dialect.escape.unquoted_byte(),
        }
    }

    /// Whether a record the plain kernel finished needs a pass over it.
    ///
    /// Static formats decide this at compile time, removing both passes and
    /// their guards from the kernel entirely.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_record_pass<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => needs_record_pass(options.dialect, options.nulls),
            None => self.record_pass,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_general_parsing<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => needs_general_parsing(options.dialect, options.trim),
            None => self.general_parsing,
        }
    }

    /// The configured NULL policy.
    ///
    /// A static format folds this to a constant, so the whole NULL check
    /// disappears from the kernel for the overwhelmingly common
    /// [`Nulls::None`] rather than costing a load and a compare per field.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_nulls<F: CsvFormat>(&self) -> Nulls {
        match F::OPTIONS {
            Some(options) => options.nulls,
            None => self.nulls,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_escape<F: CsvFormat>(&self) -> Escape {
        match F::OPTIONS {
            Some(options) => options.dialect.escape,
            None => self.dialect.escape,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_record_ending<F: CsvFormat>(&self) -> RecordEnding {
        match F::OPTIONS {
            Some(options) => options.dialect.record_ending,
            None => self.dialect.record_ending,
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_permits_unquoted_quotes<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => options.syntax.permits_unquoted_quotes(),
            None => self.syntax.permits_unquoted_quotes(),
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_permits_any_backslash_escape<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => options.syntax.permits_any_backslash_escape(),
            None => self.syntax.permits_any_backslash_escape(),
        }
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_permits_trailing_whitespace<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => options.syntax.permits_trailing_whitespace(),
            None => self.syntax.permits_trailing_whitespace(),
        }
    }

    /// Whether spaces after a delimiter belong to the next field.
    ///
    /// A static format folds this to a constant, which lets the whole of
    /// `skip_delimiter_spaces` fall out of the record path for the formats
    /// that do not ask for it.
    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
    )]
    pub(super) fn fmt_skip_initial_space<F: CsvFormat>(&self) -> bool {
        match F::OPTIONS {
            Some(options) => options.skip_initial_space,
            None => self.skip_initial_space,
        }
    }

    #[inline(always)]
    pub(super) fn is_csv_format<F: CsvFormat>(&self) -> bool {
        matches!(F::OPTIONS, Some(options) if matches!(options.tag, FormatTag::Csv))
            || (F::OPTIONS.is_none() && matches!(self.format_kind, FormatKind::Csv))
    }

    /// The literal whose absence lets whole records be skipped unparsed, or
    /// `None` when the dialect rules that shortcut out.
    ///
    /// This lives on the engine rather than being answered from a dialect the
    /// caller holds, so that a static format folds the decision to a constant
    /// instead of loading a copy the front end would otherwise have to keep.
    pub(crate) fn skip_literal_for<'predicate, F: CsvFormat>(
        &self,
        predicate: &'predicate Predicate,
    ) -> Option<&'predicate [u8]> {
        let (dialect, blank_records) = match F::OPTIONS {
            Some(options) => (options.dialect, options.blank_records),
            None => (self.dialect, self.blank_records),
        };
        skip_literal(dialect, blank_records, predicate)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FormatOptions, Quoting, WriteBom};

    #[test]
    fn test_format_accessors() {
        let input = b"a,b\n";
        let engine = Engine::from_config(
            input,
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );

        let _ = engine.fmt_delimiter::<Csv>();
        let _ = engine.fmt_delimiter::<Dynamic>();

        let _ = engine.fmt_quote::<Csv>();
        let _ = engine.fmt_quote::<Dynamic>();

        let _ = engine.fmt_terminator::<Csv>();
        let _ = engine.fmt_terminator::<Dynamic>();

        let _ = engine.fmt_delimiter_tail::<Csv>();
        let _ = engine.fmt_delimiter_tail::<Dynamic>();

        let _ = engine.fmt_ending_tail::<Csv>();
        let _ = engine.fmt_ending_tail::<Dynamic>();

        let _ = engine.fmt_strips_cr::<Csv>();
        let _ = engine.fmt_strips_cr::<Dynamic>();

        let _ = engine.fmt_quoting_enabled::<Csv>();
        let _ = engine.fmt_quoting_enabled::<Dynamic>();

        let _ = engine.fmt_plain_kernel::<Csv>();
        let _ = engine.fmt_plain_kernel::<Dynamic>();

        let _ = engine.fmt_unquoted_escape::<Csv>();
        let _ = engine.fmt_unquoted_escape::<Dynamic>();

        let _ = engine.fmt_record_pass::<Csv>();
        let _ = engine.fmt_record_pass::<Dynamic>();

        let _ = engine.fmt_general_parsing::<Csv>();
        let _ = engine.fmt_general_parsing::<Dynamic>();

        let _ = engine.fmt_nulls::<Csv>();
        let _ = engine.fmt_nulls::<Dynamic>();

        let _ = engine.fmt_escape::<Csv>();
        let _ = engine.fmt_escape::<Dynamic>();

        let _ = engine.fmt_record_ending::<Csv>();
        let _ = engine.fmt_record_ending::<Dynamic>();

        let _ = engine.fmt_permits_unquoted_quotes::<Csv>();
        let _ = engine.fmt_permits_unquoted_quotes::<Dynamic>();

        let _ = engine.fmt_permits_any_backslash_escape::<Csv>();
        let _ = engine.fmt_permits_any_backslash_escape::<Dynamic>();

        let _ = engine.fmt_permits_trailing_whitespace::<Csv>();
        let _ = engine.fmt_permits_trailing_whitespace::<Dynamic>();

        let _ = engine.fmt_skip_initial_space::<Csv>();
        let _ = engine.fmt_skip_initial_space::<Dynamic>();

        let pred = Predicate::equals(0, "a");
        let _ = engine.skip_literal_for::<Csv>(&pred);
        let _ = engine.skip_literal_for::<Dynamic>(&pred);

        // Exercise dialect with unquoted escape, non-matching delimiter tail, non-matching ending tail
        let esc_dialect = Dialect {
            escape: Escape::Unquoted(b'\\'),
            delimiter: b',',
            #[cfg(feature = "multibyte")]
            delimiter_tail: Tail::of(b",::"),
            record_ending: RecordEnding::Byte(b';'),
            #[cfg(feature = "multibyte")]
            ending_tail: Tail::of(b";;;"),
            quote: b'\'',
            ..Dialect::default()
        };
        let mut esc_settings = ParserSettings::unheaded(esc_dialect, Limits::DEFAULT);
        esc_settings.format_tag = FormatTag::Custom;
        let esc_engine = Engine::from_config(b"a\\,b;;c", esc_settings);
        assert_eq!(esc_engine.fmt_escape::<Dynamic>(), Escape::Unquoted(b'\\'));
        #[cfg(feature = "multibyte")]
        {
            assert_eq!(esc_engine.fmt_delimiter_tail::<Dynamic>().as_slice(), b"::");
            assert_eq!(esc_engine.fmt_ending_tail::<Dynamic>().as_slice(), b";;");
        }
        assert_eq!(esc_engine.fmt_quote::<Dynamic>(), b'\'');
        assert_eq!(esc_engine.fmt_unquoted_escape::<Dynamic>(), Some(b'\\'));
        assert_eq!(
            esc_engine.fmt_record_ending::<Dynamic>(),
            RecordEnding::Byte(b';')
        );
        assert_eq!(esc_engine.fmt_terminator::<Dynamic>(), b';');
        assert!(!esc_engine.fmt_strips_cr::<Dynamic>());

        // Dynamic with all Option<bool> flags as Some
        let mut full_rec_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        full_rec_settings.syntax = Syntax::Compatible(
            crate::config::Recovery::NONE
                .quoting(false)
                .unquoted_quotes(true)
                .trailing_whitespace_after_quote(true)
                .any_backslash_escape(true),
        );
        full_rec_settings.format_tag = FormatTag::Custom;
        full_rec_settings.skip_initial_space = true;
        let full_rec_engine = Engine::from_config(b"a,b\n", full_rec_settings);
        assert!(!full_rec_engine.fmt_quoting_enabled::<Dynamic>());
        assert!(full_rec_engine.fmt_permits_unquoted_quotes::<Dynamic>());
        assert!(full_rec_engine.fmt_skip_initial_space::<Dynamic>());
        assert!(full_rec_engine.fmt_permits_trailing_whitespace::<Dynamic>());
        assert!(full_rec_engine.fmt_permits_any_backslash_escape::<Dynamic>());

        // Static CsvFormat custom struct with all options
        struct MyCustomFmt;
        impl CsvFormat for MyCustomFmt {
            const OPTIONS: Option<FormatOptions> = Some(FormatOptions {
                dialect: Dialect {
                    delimiter: b'\t',
                    quote: b'\'',
                    record_ending: RecordEnding::Byte(b'\r'),
                    escape: Escape::Mysql,
                    comment: None,
                    #[cfg(feature = "multibyte")]
                    delimiter_tail: Tail::EMPTY,
                    #[cfg(feature = "multibyte")]
                    ending_tail: Tail::EMPTY,
                },
                syntax: Syntax::Strict,
                nulls: Nulls::None,
                trim: Whitespace::NONE,
                blank_records: BlankRecords::Preserve,
                read_bom: ReadBom::Detect,
                write_bom: WriteBom::Omit,
                quoting: Quoting::Necessary,
                skip_initial_space: true,
                tag: FormatTag::Custom,
            });
        }
        let static_engine = Engine::from_config(
            b"a\tb\r",
            ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT),
        );
        assert_eq!(static_engine.fmt_delimiter::<MyCustomFmt>(), b'\t');
        assert_eq!(static_engine.fmt_terminator::<MyCustomFmt>(), b'\r');
        assert_eq!(static_engine.fmt_quote::<MyCustomFmt>(), b'\'');
        assert_eq!(
            static_engine.fmt_record_ending::<MyCustomFmt>(),
            RecordEnding::Byte(b'\r')
        );
        assert_eq!(static_engine.fmt_escape::<MyCustomFmt>(), Escape::Mysql);
        assert_eq!(
            static_engine.fmt_unquoted_escape::<MyCustomFmt>(),
            Some(b'\\')
        );
        assert!(!static_engine.fmt_strips_cr::<MyCustomFmt>());
        assert!(!static_engine.fmt_plain_kernel::<MyCustomFmt>());
        assert!(static_engine.fmt_skip_initial_space::<MyCustomFmt>());
    }

    #[test]
    fn format_dependent_shortcuts_use_the_selected_static_or_dynamic_options() {
        struct Custom;
        impl CsvFormat for Custom {
            const OPTIONS: Option<FormatOptions> = Some(FormatOptions {
                dialect: Dialect {
                    delimiter: b'|',
                    quote: b'\'',
                    record_ending: RecordEnding::Byte(b';'),
                    escape: Escape::DoubleQuote,
                    comment: None,
                    #[cfg(feature = "multibyte")]
                    delimiter_tail: Tail::EMPTY,
                    #[cfg(feature = "multibyte")]
                    ending_tail: Tail::EMPTY,
                },
                syntax: Syntax::Strict,
                nulls: Nulls::None,
                trim: Whitespace::NONE,
                blank_records: BlankRecords::Preserve,
                read_bom: ReadBom::Detect,
                write_bom: WriteBom::Omit,
                quoting: Quoting::Necessary,
                skip_initial_space: false,
                tag: FormatTag::Custom,
            });
        }

        let predicate = Predicate::equals(0, "needle");
        let mut csv_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        csv_settings.format_tag = FormatTag::Csv;
        let csv_engine = Engine::from_config(b"", csv_settings);
        assert!(!csv_engine.fmt_record_pass::<Csv>());
        assert!(csv_engine.is_csv_format::<Csv>());
        assert!(csv_engine.is_csv_format::<Dynamic>());
        assert_eq!(
            csv_engine.skip_literal_for::<Csv>(&predicate),
            Some(&b"needle"[..])
        );
        assert_eq!(
            csv_engine.skip_literal_for::<Dynamic>(&predicate),
            Some(&b"needle"[..])
        );

        let mut custom_settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        custom_settings.format_tag = FormatTag::Custom;
        custom_settings.blank_records = BlankRecords::Skip;
        let custom_engine = Engine::from_config(b"", custom_settings);
        assert!(!custom_engine.is_csv_format::<Custom>());
        assert!(!custom_engine.is_csv_format::<Dynamic>());
        assert!(
            custom_engine
                .skip_literal_for::<Dynamic>(&predicate)
                .is_none()
        );
        assert_eq!(
            custom_engine.skip_literal_for::<Custom>(&predicate),
            Some(&b"needle"[..])
        );
    }
}

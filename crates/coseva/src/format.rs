//! Compile-time format specialization.
//!
//! A [`CsvFormat`] names the format a parser reads at the type level. The
//! trait carries an *optional* compile-time value, so one kernel body serves
//! both cases: [`Csv`] and friends supply `Some(..)`, folding every setting
//! the kernel reads to an immediate; [`Dynamic`] supplies `None`, so the
//! kernel loads each setting from the engine at run time.
//!
//! A parser built from run-time options classifies itself once, at
//! construction, and runs the specialized kernel for CSV and TSV
//! automatically — naming a format is only needed for a *custom* one the
//! classifier cannot recognize.

use crate::config::{FormatOptions, FormatTag, ParserSettings};

/// A CSV format that may be known at compile time.
///
/// This is the bound generic code writes against; it covers both regimes.
/// [`Dynamic`] implements it with `None`, and [`csv_format!`](crate::csv_format) declares types
/// that implement it with `Some`.
///
/// ```
/// use coseva::config::FormatOptions;
/// use coseva::csv_format;
///
/// csv_format! {
///     pub Pipes = FormatOptions::CSV.delimiter(b'|');
/// }
/// ```
pub trait CsvFormat {
    /// The format, when it is known at compile time.
    ///
    /// `None` selects run-time configuration: the parser reads its settings
    /// from the options it was built with, and nothing folds.
    const OPTIONS: Option<FormatOptions>;
}

/// A format whose value is known at compile time.
///
/// This is what separates a declared format from [`Dynamic`], so a
/// constructor can require one and take no format argument. It is sealed:
/// only [`csv_format!`](crate::csv_format) implements it, which is what guarantees that
/// [`FORMAT`](StaticFormat::FORMAT) was validated as it was declared.
/// For a worked example, see [`crate::csv_format`].
pub trait StaticFormat: CsvFormat + sealed::Sealed {
    /// The format itself, needed unconditionally rather than as an `Option`.
    const FORMAT: FormatOptions;
}

#[doc(hidden)]
pub mod sealed {
    /// Prevents `StaticFormat` from being implemented outside `csv_format!`.
    pub trait Sealed {}
}

/// Run-time configured format: settings come from the built parser.
///
/// This is a marker only. The options themselves already live in the engine,
/// so selecting it costs no space and generates the code the crate generates
/// today.
/// For a worked example, see [`crate::SliceParser::with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Dynamic;

impl CsvFormat for Dynamic {
    const OPTIONS: Option<FormatOptions> = None;
}

/// Declare a type naming a CSV format known at compile time.
///
/// The format is checked as it is declared: an unusable one, such as a
/// delimiter that is also the quote byte, fails to compile rather than
/// failing when a parser is built. That check is why this is the only way to
/// obtain a [`StaticFormat`].
///
/// ```
/// use coseva::config::{FormatOptions, ParseOptions};
/// use coseva::{SliceParser, csv_format};
///
/// csv_format! {
///     /// The export format our upstream system produces.
///     pub Upstream = FormatOptions::CSV.delimiter(b'|').quote(b'\'');
/// }
///
/// let mut parser = SliceParser::<Upstream>::new(
///     b"city|pop\n'Boston, MA'|650706\n",
///     ParseOptions::new(),
/// )?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected a record"))?;
/// assert_eq!(line.record()?.get(0), Some(&b"Boston, MA"[..]));
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// Declaring an unusable format is a compile error:
///
/// ```compile_fail
/// use coseva::config::FormatOptions;
/// use coseva::csv_format;
///
/// csv_format! {
///     pub Broken = FormatOptions::CSV.delimiter(b'"');
/// }
/// ```
#[macro_export]
macro_rules! csv_format {
    ($($(#[$meta:meta])* $vis:vis $name:ident = $options:expr;)+) => {$(
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis struct $name;

        impl $crate::format::CsvFormat for $name {
            const OPTIONS: ::core::option::Option<$crate::config::FormatOptions> =
                ::core::option::Option::Some($options);
        }

        impl $crate::format::sealed::Sealed for $name {}

        impl $crate::format::StaticFormat for $name {
            const FORMAT: $crate::config::FormatOptions = $options;
        }

        // A free `const` item is evaluated eagerly, where an associated one
        // would only be checked if something referenced it -- which nothing
        // does, so the format would go unvalidated.
        const _: () = match $options.invalidity() {
            ::core::option::Option::Some(reason) => panic!("{}", reason),
            ::core::option::Option::None => (),
        };
    )+};
}

csv_format! {
    /// Standard comma-separated values, specialized at compile time.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Csv = FormatOptions::CSV;
    /// Tab-separated values, specialized at compile time.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Tsv = FormatOptions::TSV;
    /// Semicolon-separated values, specialized at compile time.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Semicolon = FormatOptions::SEMICOLON;
    /// Pipe-delimited values, specialized at compile time.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Pipe = FormatOptions::PIPE;
    /// Comma-separated values with backslash escaping.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub BackslashCsv = FormatOptions::BACKSLASH_CSV;
    /// Tab-separated values with backslash escaping.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub BackslashTsv = FormatOptions::BACKSLASH_TSV;
    /// CSV with `#` comments and skipped physical blank lines.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub CommentedCsv = FormatOptions::COMMENTED_CSV;
    /// CSV with surrounding ASCII whitespace removed from all fields.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub TrimmedCsv = FormatOptions::TRIMMED_CSV;
    /// CSV that ignores spaces immediately following delimiters.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub PythonCsv = FormatOptions::PYTHON_CSV;
    /// Python `csv` with `quoting=QUOTE_NONE` and `escapechar='\\'`.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub PythonEscaped = FormatOptions::PYTHON_ESCAPED;
    /// Strict RFC 4180 with mandatory CRLF record terminators.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Rfc4180 = FormatOptions::RFC4180;
    /// Excel-compatible CRLF records.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Excel = FormatOptions::EXCEL;
    /// `PostgreSQL` `COPY ... CSV`, where an unquoted empty field is NULL.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub PostgresCopyCsv = FormatOptions::POSTGRES_COPY_CSV;
    /// `MySQL` text export with backslash escapes and `\N` NULL fields.
    /// For a worked example, see [`crate::SliceParser::new`].
    pub Mysql = FormatOptions::MYSQL;
}

// ── automatic specialization ────────────────────────────────────────────────

/// Which built-in format a parser's settings turn out to describe.
///
/// Computed once when a parser is built, so a caller who never mentions a
/// static format still gets the specialized kernel for the common formats.
/// `Other` covers everything else and runs the run-time-configured kernel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormatKind {
    Csv,
    Tsv,
    Other,
}

/// Whether `options` agrees with `settings` in everything parsing reads:
/// dialect, syntax, NULL policy, trim policy, blank-record handling, and the
/// initial-space rule. This is the soundness condition for substituting a
/// static format for the engine's own settings.
#[expect(
    clippy::unneeded_field_pattern,
    reason = "naming every field here makes a new `ParserSettings` field a compile error rather than a silently unchecked one; misclassification would cause a silently wrong parse"
)]
fn agrees(options: FormatOptions, settings: &ParserSettings) -> bool {
    let ParserSettings {
        dialect,
        syntax,
        nulls,
        trim,
        blank_records,
        skip_initial_space,
        // Answered by the tag when there is one; this is the fallback that
        // decides what an untagged format really is.
        format_tag: _,
        // Read by the engine directly, never through a `fmt_*` accessor, so
        // they are the same values whichever format parameter is in play.
        limits: _,
        field_count: _,
        headers: _,
        bom: _,
        buffer_capacity: _,
        // A static format folds the kernel decision at compile time, so it
        // cannot honour a request to force the general parser. Declining to
        // recognize the format sends the parser down the runtime-configured
        // path, which can.
        #[cfg(feature = "test-util")]
            force_general_parser: _,
    } = settings;

    #[cfg(feature = "test-util")]
    if settings.force_general_parser {
        return false;
    }

    options.dialect == *dialect
        && options.syntax == *syntax
        && options.nulls == *nulls
        && options.trim == *trim
        && options.blank_records == *blank_records
        && options.skip_initial_space == *skip_initial_space
}

impl FormatKind {
    /// Classify a parser's settings.
    ///
    /// Only CSV and TSV are recognized; anything else parses correctly but
    /// without folding.
    pub(crate) fn of(settings: &ParserSettings) -> Self {
        match settings.format_tag {
            // A built-in already knows what it is, so no comparison is needed.
            FormatTag::Csv => Self::Csv,
            FormatTag::Tsv => Self::Tsv,
            // Built-ins with no kernel of their own. Naming them individually
            // rather than using a wildcard makes a new built-in a compile
            // error here, so it must be classified deliberately.
            FormatTag::Semicolon
            | FormatTag::Pipe
            | FormatTag::BackslashCsv
            | FormatTag::BackslashTsv
            | FormatTag::CommentedCsv
            | FormatTag::TrimmedCsv
            | FormatTag::PythonCsv
            | FormatTag::PythonEscaped
            | FormatTag::Rfc4180
            | FormatTag::Excel
            | FormatTag::PostgresCopyCsv
            | FormatTag::Mysql => Self::Other,
            // Assembled by a caller, or a built-in that has been modified
            // since. Only the fields can say what it is now: a built-in
            // retargeted at another format's delimiter really is that format.
            FormatTag::Custom => Self::by_comparison(settings),
        }
    }

    /// Classify settings that carry no usable tag, by comparing every field
    /// that parsing reads.
    fn by_comparison(settings: &ParserSettings) -> Self {
        if Csv::OPTIONS.is_some_and(|options| agrees(options, settings)) {
            Self::Csv
        } else if Tsv::OPTIONS.is_some_and(|options| agrees(options, settings)) {
            Self::Tsv
        } else {
            Self::Other
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{FormatKind, agrees};
    use crate::config::FormatTag;
    use crate::config::{
        BlankRecords, Escape, FormatOptions, Limits, Nulls, ParserSettings, Recovery, Syntax,
        Whitespace,
    };

    /// Build the settings a parser configured with `options` would hold.
    fn settings(options: FormatOptions) -> ParserSettings {
        let mut settings = ParserSettings::headed(options.dialect, Limits::DEFAULT);
        settings.trim = options.trim;
        settings.blank_records = options.blank_records;
        settings.syntax = options.syntax;
        settings.nulls = options.nulls;
        settings.skip_initial_space = options.skip_initial_space;
        settings.format_tag = options.tag;
        settings
    }

    #[test]
    fn the_built_in_shapes_classify_as_themselves() {
        assert_eq!(
            FormatKind::of(&settings(FormatOptions::CSV)),
            FormatKind::Csv
        );
        assert_eq!(
            FormatKind::of(&settings(FormatOptions::TSV)),
            FormatKind::Tsv
        );
    }

    /// Anything one setting away from a built-in must fall back.
    ///
    /// This is the whole safety property. The substitution is silent, so a
    /// classifier that says `Csv` for settings that are not CSV produces a
    /// wrong parse with no error anywhere. Each case below differs from a
    /// built-in in exactly one field the comparison covers.
    #[test]
    fn one_setting_away_from_a_built_in_is_not_that_built_in() {
        let cases = [
            ("delimiter", FormatOptions::CSV.delimiter(b'^')),
            ("quote", FormatOptions::CSV.quote(b'\'')),
            (
                "escape",
                FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
            ),
            ("trim", FormatOptions::CSV.trim(Whitespace::ALL)),
            (
                "blank_records",
                FormatOptions::CSV.blank_records(BlankRecords::Skip),
            ),
            (
                "syntax",
                FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
            ),
            ("nulls", FormatOptions::CSV.nulls(Nulls::Mysql)),
            (
                "skip_initial_space",
                FormatOptions::CSV.skip_initial_space(true),
            ),
        ];

        for (field, options) in cases {
            assert_eq!(
                FormatKind::of(&settings(options)),
                FormatKind::Other,
                "settings differing from CSV in {field} were classified as a built-in format, \
                 so they would silently run that format's kernel"
            );
        }
    }

    #[test]
    fn one_setting_away_from_tsv_is_not_tsv() {
        for options in [
            FormatOptions::TSV.trim(Whitespace::ALL),
            FormatOptions::TSV.nulls(Nulls::Mysql),
            FormatOptions::TSV.quote(b'\''),
        ] {
            assert_eq!(FormatKind::of(&settings(options)), FormatKind::Other);
        }
    }

    /// Classification follows the settings, not the name they were built from.
    ///
    /// TSV differs from CSV only in its delimiter, so tab-separated options
    /// retargeted at a comma really are CSV and running the CSV kernel for
    /// them is correct rather than a leak.
    #[test]
    fn tsv_retargeted_at_a_comma_is_csv() {
        assert_eq!(
            FormatKind::of(&settings(FormatOptions::TSV.delimiter(b','))),
            FormatKind::Csv
        );
    }

    /// The formats the engine deliberately does not specialize.
    #[test]
    fn unspecialized_built_ins_fall_back_rather_than_borrowing_a_kernel() {
        for options in [FormatOptions::SEMICOLON, FormatOptions::PIPE] {
            assert_eq!(FormatKind::of(&settings(options)), FormatKind::Other);
        }
    }

    #[test]
    fn agreement_is_reflexive_for_every_built_in() {
        for options in [
            FormatOptions::CSV,
            FormatOptions::TSV,
            FormatOptions::SEMICOLON,
            FormatOptions::PIPE,
        ] {
            assert!(agrees(options, &settings(options)));
        }
    }

    /// Every built-in format, so the tests below cannot silently skip one.
    const BUILT_INS: [(&str, FormatOptions); 14] = [
        ("CSV", FormatOptions::CSV),
        ("TSV", FormatOptions::TSV),
        ("SEMICOLON", FormatOptions::SEMICOLON),
        ("PIPE", FormatOptions::PIPE),
        ("BACKSLASH_CSV", FormatOptions::BACKSLASH_CSV),
        ("BACKSLASH_TSV", FormatOptions::BACKSLASH_TSV),
        ("COMMENTED_CSV", FormatOptions::COMMENTED_CSV),
        ("TRIMMED_CSV", FormatOptions::TRIMMED_CSV),
        ("PYTHON_CSV", FormatOptions::PYTHON_CSV),
        ("PYTHON_ESCAPED", FormatOptions::PYTHON_ESCAPED),
        ("RFC4180", FormatOptions::RFC4180),
        ("EXCEL", FormatOptions::EXCEL),
        ("POSTGRES_COPY_CSV", FormatOptions::POSTGRES_COPY_CSV),
        ("MYSQL", FormatOptions::MYSQL),
    ];

    /// The tag is only ever an accelerator. If a built-in were tagged as a
    /// format it does not actually describe, it would silently run that
    /// format's kernel over input it does not match, so the fast answer must
    /// equal the answer the field comparison gives.
    #[test]
    fn every_built_ins_tag_agrees_with_comparing_its_fields() {
        for (name, options) in BUILT_INS {
            let settings = settings(options);
            assert_ne!(
                settings.format_tag,
                FormatTag::Custom,
                "{name} lost its tag, so it pays for classification it does not need"
            );
            assert_eq!(
                FormatKind::of(&settings),
                FormatKind::by_comparison(&settings),
                "the tag on {name} classifies it differently from its own fields,                  so it would run the wrong kernel"
            );
        }
    }

    /// A modified built-in is no longer the format it was named after, so it
    /// must fall back to the fields rather than keep a now-wrong tag.
    #[test]
    fn a_setter_drops_the_tag() {
        for (name, options) in BUILT_INS {
            assert_eq!(
                options.delimiter(b'~').tag,
                FormatTag::Custom,
                "{name} kept its tag after being retargeted at another delimiter"
            );
        }
    }

    /// The tag records how a value was spelled, not what it means, so it must
    /// not affect equality; formats rebuilt field by field, as when restored
    /// from an index, still have to equal the built-in they describe.
    #[test]
    fn the_tag_does_not_affect_equality() {
        for (name, options) in BUILT_INS {
            let rebuilt = options.delimiter(options.dialect.delimiter);
            assert_eq!(rebuilt.tag, FormatTag::Custom);
            assert_eq!(rebuilt, options, "{name} stopped equalling its own rebuild");
            assert_eq!(
                FormatKind::of(&settings(rebuilt)),
                FormatKind::of(&settings(options)),
                "{name} classified differently once rebuilt, so an equal format                  would parse by a different kernel"
            );
        }
    }

    crate::csv_format! {
        /// A marker declared exactly as a user would declare one.
        DerivedMarker = FormatOptions::CSV;
    }

    /// The markers are zero-sized, so every standard trait is free to derive.
    /// A caller that cannot compare, default-construct or hash one has to
    /// carry the format some other way, and the cost of fixing that is zero.
    #[test]
    #[expect(
        clippy::default_constructed_unit_structs,
        reason = "constructing through Default is the derive under test, not a \
                  roundabout way of naming the unit struct"
    )]
    fn a_declared_marker_derives_the_free_traits() {
        use core::mem::size_of;
        use std::collections::HashSet;

        let a = DerivedMarker;
        let b = DerivedMarker::default();

        assert_eq!(a, b);
        assert!(a <= b);

        let mut seen = HashSet::new();
        assert!(seen.insert(a));
        assert!(!seen.insert(b), "two markers of one type must hash alike");

        assert_eq!(size_of::<DerivedMarker>(), 0);
        assert_eq!(super::Dynamic, super::Dynamic::default());
    }

    #[test]
    #[cfg(feature = "test-util")]
    fn force_general_parser_disagrees() {
        let mut s = settings(FormatOptions::CSV);
        s.force_general_parser = true;
        assert!(!agrees(FormatOptions::CSV, &s));
    }
}

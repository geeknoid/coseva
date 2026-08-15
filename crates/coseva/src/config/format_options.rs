use core::fmt;

use crate::error::{Error, ErrorKind};

#[cfg(feature = "multibyte")]
use super::Tail;
use super::{
    BlankRecords, Dialect, Escape, Nulls, Quoting, ReadBom, RecordEnding, Recovery, Syntax,
    Whitespace, WriteBom,
};

/// Which built-in format a [`FormatOptions`] value was built from, if any.
///
/// Recognizing a format by comparing every field it affects costs more than
/// the parser construction it feeds, and every built-in already knows which
/// format it is. Carrying that answer along turns recognition into a single
/// match.
///
/// The tag is a hint that is allowed to be pessimistic but never optimistic:
/// any setter drops it to [`FormatTag::Custom`], because a retargeted
/// built-in is no longer the format it was named after. `Custom` means only
/// "ask the fields", so a value that loses its tag is still classified
/// correctly, just by the slower route. The tag is therefore not part of a
/// format's identity and takes no part in comparing two formats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormatTag {
    Csv,
    Tsv,
    Semicolon,
    Pipe,
    BackslashCsv,
    BackslashTsv,
    CommentedCsv,
    TrimmedCsv,
    PythonCsv,
    PythonEscaped,
    Rfc4180,
    Excel,
    PostgresCopyCsv,
    Mysql,
    /// Assembled by a caller, or a built-in that has since been modified.
    Custom,
}

/// A complete description of one CSV format, for both reading and writing.
///
/// A format captures everything that identifies a CSV flavor: its syntax
/// (delimiter, quote, record ending, escaping, comments) and the reading and
/// writing conventions that go with it. It deliberately does *not* carry
/// per-invocation concerns such as headers, resource limits, or buffer sizes;
/// those belong to [`ParseOptions`](super::ParseOptions) and [`EmitOptions`](super::EmitOptions). A format and a
/// matching options value are independent, and both are passed explicitly
/// when a parser or emitter is constructed.
///
/// Some options primarily affect reading (`trim`, `skip_initial_space`,
/// `syntax`, `read_bom`) and others primarily affect writing (`quoting`,
/// `write_bom`). Both live here because they must describe one coherent
/// format. Construction rejects combinations whose writer output cannot be
/// read back under the matching parser settings.
///
/// Every constructor and setter is `const`, so custom formats can be declared
/// as constants:
///
/// ```
/// use coseva::config::{EmitOptions, FormatOptions, ParseOptions, Whitespace};
///
/// const LOOSE_SEMICOLON: FormatOptions = FormatOptions::SEMICOLON
///     .trim(Whitespace::ALL)
///     .comment(Some(b'#'));
/// ```
///
/// Structural bytes are validated when a parser or emitter is built, not when
/// a format is declared, because `const` setters cannot report errors.
#[derive(Clone, Copy, Eq)]
pub struct FormatOptions {
    pub(crate) dialect: Dialect,
    pub(crate) trim: Whitespace,
    pub(crate) blank_records: BlankRecords,
    pub(crate) read_bom: ReadBom,
    pub(crate) write_bom: WriteBom,
    pub(crate) syntax: Syntax,
    pub(crate) nulls: Nulls,
    pub(crate) quoting: Quoting,
    pub(crate) skip_initial_space: bool,
    /// Which built-in this was built from; a construction-time hint only.
    pub(crate) tag: FormatTag,
}

/// Two formats are equal when they describe the same format.
///
/// The internal record of which built-in a value was named after captures how
/// it was spelled rather than what it means, so it is excluded: a format
/// reassembled field by field, such as one restored from an index, must still
/// equal the built-in it describes.
impl PartialEq for FormatOptions {
    #[expect(
        clippy::unneeded_field_pattern,
        reason = "naming every field makes a new one a compile error here rather than a field silently left out of equality"
    )]
    fn eq(&self, other: &Self) -> bool {
        let Self {
            dialect,
            trim,
            blank_records,
            read_bom,
            write_bom,
            syntax,
            nulls,
            quoting,
            skip_initial_space,
            tag: _,
        } = self;

        *dialect == other.dialect
            && *trim == other.trim
            && *blank_records == other.blank_records
            && *read_bom == other.read_bom
            && *write_bom == other.write_bom
            && *syntax == other.syntax
            && *nulls == other.nulls
            && *quoting == other.quoting
            && *skip_initial_space == other.skip_initial_space
    }
}

/// Prints what the format is.
///
/// The internal record of which built-in a value was named after is omitted,
/// because it describes how the value was built rather than how it parses.
#[expect(
    clippy::missing_fields_in_debug,
    reason = "`tag` is a construction-time hint about how the value was spelled, not part of the format it describes"
)]
impl fmt::Debug for FormatOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FormatOptions")
            .field("dialect", &self.dialect)
            .field("trim", &self.trim)
            .field("blank_records", &self.blank_records)
            .field("read_bom", &self.read_bom)
            .field("write_bom", &self.write_bom)
            .field("syntax", &self.syntax)
            .field("nulls", &self.nulls)
            .field("quoting", &self.quoting)
            .field("skip_initial_space", &self.skip_initial_space)
            .finish()
    }
}

impl FormatOptions {
    /// The reason this format is unusable, or `None` when it is sound.
    ///
    /// The same rules [`crate::SliceParser::with_options`] enforces, in a form
    /// a `const` block can check, so a format declared with
    /// [`crate::csv_format`] fails to compile rather than failing when a parser
    /// is built.
    ///
    /// ```
    /// use coseva::config::FormatOptions;
    ///
    /// assert!(FormatOptions::CSV.invalidity().is_none());
    /// // A delimiter cannot also be the quote byte.
    /// assert!(FormatOptions::CSV.delimiter(b'"').invalidity().is_some());
    /// ```
    #[must_use]
    pub const fn invalidity(self) -> Option<&'static str> {
        if let Some(reason) = self.dialect.invalidity() {
            return Some(reason);
        }
        if self.dialect.escape.escapes_unquoted() && !matches!(self.quoting, Quoting::Never) {
            return Some("unquoted escaping requires Quoting::Never");
        }
        if !self.syntax.quoting_enabled()
            && matches!(
                self.quoting,
                Quoting::Necessary | Quoting::Always | Quoting::NonNumeric
            )
        {
            return Some("quote-producing output requires parser quote syntax");
        }
        match self.nulls {
            Nulls::PostgresCsv
                if self.dialect.escape.escapes_unquoted()
                    || !self.syntax.quoting_enabled()
                    || matches!(self.quoting, Quoting::Never | Quoting::Raw) =>
            {
                Some("PostgreSQL CSV NULLs require protective quoting")
            }
            Nulls::Mysql
                if matches!(self.quoting, Quoting::Raw)
                    || (matches!(self.quoting, Quoting::Never)
                        && !self.dialect.escape.escapes_unquoted()) =>
            {
                Some("MySQL NULLs require quoting or unquoted escaping")
            }
            Nulls::None | Nulls::PostgresCsv | Nulls::Mysql => None,
        }
    }

    pub(crate) fn validate(self) -> Result<(), Error> {
        match self.invalidity() {
            Some(reason) => Err(Error::detailed(ErrorKind::Configuration, reason)),
            None => Ok(()),
        }
    }

    /// Standard comma-separated values.
    pub const CSV: Self = Self {
        tag: FormatTag::Csv,
        ..Self::with_dialect(Dialect::CSV)
    };
    /// Tab-separated values.
    pub const TSV: Self = Self {
        tag: FormatTag::Tsv,
        ..Self::with_dialect(Dialect::TSV)
    };
    /// Semicolon-separated values.
    pub const SEMICOLON: Self = Self {
        tag: FormatTag::Semicolon,
        ..Self::with_dialect(Dialect::SEMICOLON)
    };
    /// Pipe-delimited values.
    pub const PIPE: Self = Self {
        tag: FormatTag::Pipe,
        ..Self::with_dialect(Dialect::PIPE)
    };
    /// Comma-separated values with backslash escaping.
    pub const BACKSLASH_CSV: Self = Self {
        tag: FormatTag::BackslashCsv,
        ..Self::with_dialect(Dialect::BACKSLASH_CSV)
    };
    /// Tab-separated values with backslash escaping.
    pub const BACKSLASH_TSV: Self = Self {
        tag: FormatTag::BackslashTsv,
        ..Self::with_dialect(Dialect::BACKSLASH_TSV)
    };
    /// CSV with `#` comments and skipped physical blank lines.
    pub const COMMENTED_CSV: Self = Self {
        tag: FormatTag::CommentedCsv,
        blank_records: BlankRecords::Skip,
        ..Self::with_dialect(Dialect::COMMENTED_CSV)
    };
    /// CSV with leading and trailing ASCII whitespace removed from all fields.
    pub const TRIMMED_CSV: Self = Self {
        tag: FormatTag::TrimmedCsv,
        trim: Whitespace::ALL,
        ..Self::with_dialect(Dialect::CSV)
    };
    /// CSV that ignores spaces immediately following delimiters.
    ///
    /// This matches Python's `skipinitialspace=True`: the first field and
    /// trailing spaces are not trimmed.
    pub const PYTHON_CSV: Self = Self {
        tag: FormatTag::PythonCsv,
        skip_initial_space: true,
        ..Self::with_dialect(Dialect::CSV)
    };
    /// Python's `csv` with `quoting=QUOTE_NONE` and `escapechar='\\'`.
    ///
    /// Nothing is quoted; a delimiter, record terminator, quote or backslash
    /// inside a field is written with a backslash before it, and read back the
    /// same way. See [`Escape::Unquoted`].
    ///
    /// # Performance
    ///
    /// A record carrying no backslash stays on the vectorized path; one that
    /// does falls back to the general parser.
    pub const PYTHON_ESCAPED: Self = Self {
        tag: FormatTag::PythonEscaped,
        quoting: Quoting::Never,
        ..Self::with_dialect(Dialect::PYTHON_ESCAPED)
    };
    /// Strict RFC 4180 with mandatory CRLF record terminators.
    pub const RFC4180: Self = Self {
        tag: FormatTag::Rfc4180,
        read_bom: ReadBom::Reject,
        ..Self::with_dialect(Dialect::RFC4180)
    };
    /// Excel-compatible CRLF records, detecting a BOM on read and writing one.
    pub const EXCEL: Self = Self {
        tag: FormatTag::Excel,
        write_bom: WriteBom::Emit,
        ..Self::with_dialect(Dialect::EXCEL)
    };
    /// `PostgreSQL` `COPY ... CSV`, where an unquoted empty field is NULL.
    pub const POSTGRES_COPY_CSV: Self = Self {
        tag: FormatTag::PostgresCopyCsv,
        nulls: Nulls::PostgresCsv,
        ..Self::with_dialect(Dialect::POSTGRES_COPY_CSV)
    };
    /// `MySQL` text export with unquoted backslash escapes and `\N` NULL fields.
    ///
    /// # Performance
    ///
    /// See [`Escape::Mysql`]. Records carrying a backslash leave the
    /// vectorized path; records without one do not.
    pub const MYSQL: Self = Self {
        tag: FormatTag::Mysql,
        syntax: Syntax::Compatible(Recovery::NONE),
        nulls: Nulls::Mysql,
        quoting: Quoting::Never,
        ..Self::with_dialect(Dialect::MYSQL)
    };

    /// Construct the default CSV format.
    ///
    /// This is equivalent to [`FormatOptions::CSV`]: a comma delimiter, double
    /// quotes, LF record endings, no comment byte, no trimming, and no NULL
    /// convention.
    ///
    /// ```
    /// use coseva::config::FormatOptions;
    ///
    /// assert_eq!(FormatOptions::new(), FormatOptions::CSV);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self::CSV
    }

    const fn with_dialect(dialect: Dialect) -> Self {
        Self {
            dialect,
            trim: Whitespace::NONE,
            blank_records: BlankRecords::Preserve,
            read_bom: ReadBom::Detect,
            write_bom: WriteBom::Omit,
            syntax: Syntax::Strict,
            nulls: Nulls::None,
            quoting: Quoting::Necessary,
            skip_initial_space: false,
            tag: FormatTag::Custom,
        }
    }

    /// Set the field delimiter.
    #[must_use]
    pub const fn delimiter(mut self, delimiter: u8) -> Self {
        self.tag = FormatTag::Custom;
        self.dialect.delimiter = delimiter;
        #[cfg(feature = "multibyte")]
        {
            self.dialect.delimiter_tail = Tail::EMPTY;
        };
        self
    }

    /// Set a field delimiter of more than one byte.
    ///
    /// Files delimited by `||` or `\t|\t` exist, and pandas' `read_csv` accepts
    /// a multi-character separator, so this reads them. A one-byte sequence is
    /// exactly [`delimiter`](Self::delimiter); the sequence may be up to four
    /// bytes, and a longer or empty one is reported when a parser or emitter is
    /// built from this format.
    ///
    /// # Performance
    ///
    /// A multi-byte separator leaves the vectorized path, because every scan in
    /// the crate matches single bytes. It costs the dialects that do not use one
    /// nothing at all: the fast path is chosen by a test that is constant for
    /// them.
    ///
    /// ```
    /// use coseva::config::{FormatOptions, Headers, ParseOptions};
    /// use coseva::SliceParser;
    ///
    /// let format = FormatOptions::CSV.delimiter_sequence(b"||");
    /// let options = ParseOptions::new().headers(Headers::None);
    /// let mut parser = SliceParser::with_options(b"a|b||c\n", format, options)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let record = line.record()?;
    /// assert_eq!(record.get(0), Some(&b"a|b"[..]));
    /// assert_eq!(record.get(1), Some(&b"c"[..]));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    #[must_use]
    #[cfg(feature = "multibyte")]
    pub const fn delimiter_sequence(mut self, delimiter: &[u8]) -> Self {
        self.tag = FormatTag::Custom;
        if !delimiter.is_empty() {
            self.dialect.delimiter = delimiter[0];
        }
        self.dialect.delimiter_tail = Tail::of(delimiter);
        self
    }

    /// Set the quote byte.
    #[must_use]
    pub const fn quote(mut self, quote: u8) -> Self {
        self.tag = FormatTag::Custom;
        self.dialect.quote = quote;
        self
    }

    /// Set the record terminator.
    #[must_use]
    pub const fn record_ending(mut self, record_ending: RecordEnding) -> Self {
        self.tag = FormatTag::Custom;
        self.dialect.record_ending = record_ending;
        #[cfg(feature = "multibyte")]
        {
            self.dialect.ending_tail = Tail::EMPTY;
        };
        self
    }

    /// Set a record terminator of more than one byte.
    ///
    /// The same shape as [`delimiter_sequence`](Self::delimiter_sequence), and
    /// with the same cost: up to four bytes, checked when a parser or emitter
    /// is built, and off the vectorized path. A one-byte sequence is exactly
    /// [`RecordEnding::Byte`].
    ///
    /// ```
    /// use coseva::config::{FormatOptions, Headers, ParseOptions};
    /// use coseva::SliceParser;
    ///
    /// let format = FormatOptions::CSV.record_ending_sequence(b"@@");
    /// let options = ParseOptions::new().headers(Headers::None);
    /// let mut parser = SliceParser::with_options(b"a,b@@c,d@@", format, options)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// assert_eq!(line.record()?.get(1), Some(&b"b"[..]));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    #[must_use]
    #[cfg(feature = "multibyte")]
    pub const fn record_ending_sequence(mut self, record_ending: &[u8]) -> Self {
        self.tag = FormatTag::Custom;
        if !record_ending.is_empty() {
            self.dialect.record_ending = RecordEnding::Byte(record_ending[0]);
        }
        self.dialect.ending_tail = Tail::of(record_ending);
        self
    }

    /// Set quoted-field escaping.
    #[must_use]
    pub const fn escape(mut self, escape: Escape) -> Self {
        self.tag = FormatTag::Custom;
        self.dialect.escape = escape;
        self
    }

    /// Recognize comments beginning with this byte.
    #[must_use]
    pub const fn comment(mut self, comment: Option<u8>) -> Self {
        self.tag = FormatTag::Custom;
        self.dialect.comment = comment;
        self
    }

    /// Configure ASCII-whitespace trimming.
    #[must_use]
    pub const fn trim(mut self, trim: Whitespace) -> Self {
        self.tag = FormatTag::Custom;
        self.trim = trim;
        self
    }

    /// Configure physical blank-line handling.
    #[must_use]
    pub const fn blank_records(mut self, blank_records: BlankRecords) -> Self {
        self.tag = FormatTag::Custom;
        self.blank_records = blank_records;
        self
    }

    /// Configure leading UTF-8 BOM handling on read.
    #[must_use]
    pub const fn read_bom(mut self, read_bom: ReadBom) -> Self {
        self.tag = FormatTag::Custom;
        self.read_bom = read_bom;
        self
    }

    /// Whether a UTF-8 BOM is written at the start of the document.
    #[must_use]
    pub const fn emits_bom(self) -> bool {
        matches!(self.write_bom, WriteBom::Emit)
    }

    /// Configure UTF-8 BOM output on write.
    #[must_use]
    pub const fn write_bom(mut self, write_bom: WriteBom) -> Self {
        self.tag = FormatTag::Custom;
        self.write_bom = write_bom;
        self
    }

    /// Configure strict parsing or explicit compatibility recovery.
    #[must_use]
    pub const fn syntax(mut self, syntax: Syntax) -> Self {
        self.tag = FormatTag::Custom;
        self.syntax = syntax;
        self
    }

    /// Configure explicit database NULL recognition and encoding.
    #[must_use]
    pub const fn nulls(mut self, nulls: Nulls) -> Self {
        self.tag = FormatTag::Custom;
        self.nulls = nulls;
        self
    }

    /// Configure when the emitter quotes a field.
    #[must_use]
    pub const fn quoting(mut self, quoting: Quoting) -> Self {
        self.tag = FormatTag::Custom;
        self.quoting = quoting;
        self
    }

    /// Ignore spaces immediately following field delimiters.
    ///
    /// This does not trim the first field or trailing spaces.
    #[must_use]
    pub const fn skip_initial_space(mut self, skip: bool) -> Self {
        self.tag = FormatTag::Custom;
        self.skip_initial_space = skip;
        self
    }

    #[cfg(feature = "index")]
    pub(crate) const fn from_dialect(dialect: Dialect) -> Self {
        Self::with_dialect(dialect)
    }
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self::CSV
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_reports_the_parsing_behavior_and_omits_the_spelling_hint() {
        let rendered = format!("{:?}", FormatOptions::CSV);
        assert!(rendered.starts_with("FormatOptions {"), "{rendered}");
        // Every field that describes how the format parses must be present.
        for field in [
            "dialect",
            "trim",
            "blank_records",
            "read_bom",
            "write_bom",
            "syntax",
            "nulls",
            "quoting",
            "skip_initial_space",
        ] {
            assert!(rendered.contains(field), "missing `{field}` in {rendered}");
        }
        // `tag` records how the value was named, not how it parses.
        assert!(!rendered.contains("tag"), "{rendered}");

        #[cfg(feature = "multibyte")]
        {
            let _opt = FormatOptions::new().record_ending_sequence(b"");
        }
    }

    #[test]
    fn equality_observes_every_behavioral_field_but_not_the_spelling_hint() {
        let base = FormatOptions::CSV;
        let variants = [
            FormatOptions {
                dialect: Dialect::TSV,
                ..base
            },
            FormatOptions {
                trim: Whitespace::ALL,
                ..base
            },
            FormatOptions {
                blank_records: BlankRecords::Skip,
                ..base
            },
            FormatOptions {
                read_bom: ReadBom::Reject,
                ..base
            },
            FormatOptions {
                write_bom: WriteBom::Emit,
                ..base
            },
            FormatOptions {
                syntax: Syntax::Compatible(Recovery::NONE),
                ..base
            },
            FormatOptions {
                nulls: Nulls::Mysql,
                ..base
            },
            FormatOptions {
                quoting: Quoting::Always,
                ..base
            },
            FormatOptions {
                skip_initial_space: !base.skip_initial_space,
                ..base
            },
        ];
        for changed in variants {
            assert_ne!(base, changed, "{changed:?}");
        }

        assert_eq!(
            base,
            FormatOptions {
                tag: FormatTag::Custom,
                ..base
            }
        );
    }

    #[test]
    fn invalidity_forwards_the_dialects_own_verdict() {
        assert!(FormatOptions::CSV.invalidity().is_none());
        // A delimiter cannot also be the quote byte.
        assert!(FormatOptions::CSV.delimiter(b'"').invalidity().is_some());
    }

    #[test]
    fn every_builtin_is_coherent() {
        for format in [
            FormatOptions::CSV,
            FormatOptions::TSV,
            FormatOptions::SEMICOLON,
            FormatOptions::PIPE,
            FormatOptions::BACKSLASH_CSV,
            FormatOptions::BACKSLASH_TSV,
            FormatOptions::COMMENTED_CSV,
            FormatOptions::TRIMMED_CSV,
            FormatOptions::PYTHON_CSV,
            FormatOptions::PYTHON_ESCAPED,
            FormatOptions::RFC4180,
            FormatOptions::EXCEL,
            FormatOptions::POSTGRES_COPY_CSV,
            FormatOptions::MYSQL,
        ] {
            assert_eq!(format.invalidity(), None, "{format:?}");
        }
    }

    #[test]
    fn compatibility_validation_covers_quoting_syntax_escapes_comments_and_nulls() {
        let syntaxes = [Syntax::Strict, Syntax::Compatible(Recovery::NONE)];
        let quoting_modes = [
            Quoting::Necessary,
            Quoting::Always,
            Quoting::Never,
            Quoting::NonNumeric,
            Quoting::Raw,
        ];
        let escapes = [
            Escape::DoubleQuote,
            Escape::Backslash(b'!'),
            Escape::Mysql,
            Escape::Unquoted(b'!'),
        ];
        for syntax in syntaxes {
            for quoting in quoting_modes {
                for escape in escapes {
                    for comment in [None, Some(b'#')] {
                        for read_bom in [ReadBom::Detect, ReadBom::Preserve, ReadBom::Reject] {
                            for write_bom in [WriteBom::Omit, WriteBom::Emit] {
                                for nulls in [Nulls::None, Nulls::PostgresCsv, Nulls::Mysql] {
                                    let format = FormatOptions::CSV
                                        .syntax(syntax)
                                        .quoting(quoting)
                                        .escape(escape)
                                        .comment(comment)
                                        .read_bom(read_bom)
                                        .write_bom(write_bom)
                                        .nulls(nulls);
                                    let unquoted_escape = escape.escapes_unquoted();
                                    let quote_syntax = syntax.quoting_enabled();
                                    let quote_producing = matches!(
                                        quoting,
                                        Quoting::Necessary | Quoting::Always | Quoting::NonNumeric
                                    );
                                    let incompatible = (unquoted_escape
                                        && !matches!(quoting, Quoting::Never))
                                        || (!quote_syntax && quote_producing)
                                        || (matches!(nulls, Nulls::PostgresCsv)
                                            && (unquoted_escape
                                                || !quote_syntax
                                                || matches!(
                                                    quoting,
                                                    Quoting::Never | Quoting::Raw
                                                )))
                                        || (matches!(nulls, Nulls::Mysql)
                                            && (matches!(quoting, Quoting::Raw)
                                                || (matches!(quoting, Quoting::Never)
                                                    && !unquoted_escape)));
                                    assert_eq!(
                                        format.invalidity().is_some(),
                                        incompatible,
                                        "{format:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        assert!(
            FormatOptions::PYTHON_ESCAPED
                .comment(Some(b'\\'))
                .invalidity()
                .is_some()
        );
    }
}

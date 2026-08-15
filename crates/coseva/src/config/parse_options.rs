use crate::error::Error;

use super::format_options::FormatTag;
use super::{
    BlankRecords, DEFAULT_READ_BUFFER_BYTES, Dialect, FieldCount, FormatOptions, Headers, Limits,
    Nulls, ReadBom, Syntax, Whitespace, validate_buffer_capacity,
};

/// Parser settings that belong to the invocation rather than the format.
///
/// [`FormatOptions`] says what the bytes mean; this says how to read them —
/// header handling, field-count validation, resource limits, and buffer size.
/// The two are passed together when a parser is built.
///
/// ```
/// use coseva::SliceParser;
/// use coseva::config::{FieldCount, FormatOptions, Headers, Limits, ParseOptions};
///
/// let options = ParseOptions::new()
///     .headers(Headers::None)
///     .field_count(FieldCount::Exact(2))
///     .limits(Limits::new(64 * 1024, 8 * 1024, 256));
///
/// let mut parser = SliceParser::with_options(b"Boston,650706\n", FormatOptions::CSV, options)?;
/// assert_eq!(parser.byte_records().count(), 1);
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseOptions {
    limits: Limits,
    field_count: FieldCount,
    headers: Headers,
    buffer_capacity: usize,
    #[cfg(feature = "test-util")]
    force_general_parser: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ParserSettings {
    pub(crate) dialect: Dialect,
    pub(crate) limits: Limits,
    pub(crate) field_count: FieldCount,
    pub(crate) headers: Headers,
    pub(crate) trim: Whitespace,
    pub(crate) blank_records: BlankRecords,
    pub(crate) bom: ReadBom,
    pub(crate) syntax: Syntax,
    pub(crate) nulls: Nulls,
    pub(crate) skip_initial_space: bool,
    /// The format's built-in tag, carried through so the engine can recognize
    /// a well-known format without comparing every field it affects.
    pub(crate) format_tag: FormatTag,
    #[cfg_attr(
        not(feature = "std"),
        expect(dead_code, reason = "buffer capacity configures only std I/O readers")
    )]
    pub(crate) buffer_capacity: usize,
    #[cfg(feature = "test-util")]
    pub(crate) force_general_parser: bool,
}

impl ParserSettings {
    /// Settings that consume the first record as headers, matching
    /// [`ParseOptions::new`] and [`Headers::default`].
    #[cfg(test)]
    pub(crate) const fn headed(dialect: Dialect, limits: Limits) -> Self {
        Self {
            dialect,
            limits,
            field_count: FieldCount::Flexible,
            headers: Headers::FirstRecord,
            trim: Whitespace::NONE,
            blank_records: BlankRecords::Preserve,
            bom: ReadBom::Detect,
            syntax: Syntax::Strict,
            nulls: Nulls::None,
            skip_initial_space: false,
            format_tag: FormatTag::Custom,
            buffer_capacity: DEFAULT_READ_BUFFER_BYTES,
            #[cfg(feature = "test-util")]
            force_general_parser: false,
        }
    }

    /// Header-less settings used by internal tests.
    #[cfg(test)]
    pub(crate) const fn unheaded(dialect: Dialect, limits: Limits) -> Self {
        Self {
            dialect,
            limits,
            field_count: FieldCount::Flexible,
            headers: Headers::None,
            trim: Whitespace::NONE,
            blank_records: BlankRecords::Preserve,
            bom: ReadBom::Detect,
            syntax: Syntax::Strict,
            nulls: Nulls::None,
            skip_initial_space: false,
            format_tag: FormatTag::Custom,
            buffer_capacity: DEFAULT_READ_BUFFER_BYTES,
            #[cfg(feature = "test-util")]
            force_general_parser: false,
        }
    }
}

impl ParseOptions {
    /// Start with bounded limits and header detection enabled.
    ///
    /// ```
    /// use coseva::format::Csv;
    /// use coseva::config::ParseOptions;
    /// use coseva::SliceParser;
    ///
    /// // The default treats the first record as headers.
    /// let mut parser = SliceParser::<Csv>::new(b"city\nBoston\n", ParseOptions::new())?;
    /// assert_eq!(parser.headers()?.and_then(|h| h.get(0)), Some(&b"city"[..]));
    /// # Ok::<(), coseva::Error>(())
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            limits: Limits::DEFAULT,
            field_count: FieldCount::Flexible,
            headers: Headers::FirstRecord,
            buffer_capacity: DEFAULT_READ_BUFFER_BYTES,
            #[cfg(feature = "test-util")]
            force_general_parser: false,
        }
    }

    /// Set resource limits.
    #[must_use]
    pub const fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set field-count validation.
    #[must_use]
    pub const fn field_count(mut self, field_count: FieldCount) -> Self {
        self.field_count = field_count;
        self
    }

    /// Configure header handling.
    #[must_use]
    pub fn headers(mut self, headers: Headers) -> Self {
        self.headers = headers;
        self
    }

    /// Set the input-buffer capacity used when reading from a source.
    ///
    /// Buffered readers allocate this much when they are created, so the
    /// figure is a real memory commitment per parser and not just a tuning
    /// hint. Defaults to 8 KiB. A larger buffer refills less often, which pays off
    /// when the source is a slow one such as a file or a socket; a smaller one
    /// costs less to set up and holds less memory, which matters when many
    /// parsers are alive at once or when inputs are short. The capacity is a
    /// starting point rather than a ceiling: a record longer than the buffer
    /// grows it, and `Limits::max_record_bytes` is what bounds that growth.
    #[must_use]
    pub const fn buffer_capacity(mut self, capacity: usize) -> Self {
        self.buffer_capacity = capacity;
        self
    }

    /// Force every record onto the general parser, bypassing the vectorized
    /// kernel entirely.
    ///
    /// This exists so the test suite can use the general parser as an oracle
    /// for the kernel, which is only ever an optimization of it. It is not
    /// part of the public API, it is behind the off-by-default, test-only
    /// `test-util` feature, and it makes parsing several times slower. Nothing
    /// outside this crate's own tests should call it.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub const fn force_general_parser(mut self, force: bool) -> Self {
        self.force_general_parser = force;
        self
    }

    /// The field-count rule as configured, before any parser resolves it.
    #[cfg(feature = "parallel")]
    pub(crate) const fn requested_field_count(&self) -> FieldCount {
        self.field_count
    }

    pub(crate) fn into_settings(self, format: FormatOptions) -> Result<ParserSettings, Error> {
        format.validate()?;
        validate_buffer_capacity(self.buffer_capacity)?;
        Ok(ParserSettings {
            dialect: format.dialect,
            limits: self.limits,
            field_count: self.field_count,
            headers: self.headers,
            trim: format.trim,
            blank_records: format.blank_records,
            bom: format.read_bom,
            syntax: format.syntax,
            nulls: format.nulls,
            skip_initial_space: format.skip_initial_space,
            format_tag: format.tag,
            buffer_capacity: self.buffer_capacity,
            #[cfg(feature = "test-util")]
            force_general_parser: self.force_general_parser,
        })
    }
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self::new()
    }
}

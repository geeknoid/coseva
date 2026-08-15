use core::fmt;

/// Location in a CSV source.
///
/// A [`Location`] whose `line` is zero is unknown: it comes from an error
/// raised without a source to point into, such as converting a field
/// through [`crate::FromBytes`] directly. Parsers replace it with the real
/// position when such an error passes through them.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::SliceParser;
///
/// // A quoted field spanning three physical lines, then a malformed record.
/// let input = b"note\n\"one\ntwo\nthree\"\nbad\"record\n";
/// let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new())?;
/// let _ = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected first record"))?
///     .record()?;
///
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected second record"))?;
/// let result = line.record();
/// let error = match result {
///     Err(error) => error,
///     Ok(_) => return Err(std::io::Error::other("expected malformed record").into()),
/// };
/// let location = error.location();
///
/// // Lines count physical lines, including those inside quoted fields.
/// assert_eq!(location.line, 5);
/// assert_eq!(location.record, 2);
/// assert_eq!(location.field, 0);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Location {
    /// Byte offset in the input.
    pub byte: usize,
    /// One-based physical line containing `byte`.
    ///
    /// Lines are counted from occurrences of LF (`\n`), including LF bytes
    /// inside quoted fields and the LF half of CRLF.
    pub line: u64,
    /// Zero-based record index.
    pub record: u64,
    /// Zero-based field index.
    pub field: usize,
}

impl Location {
    /// Location of an error raised with no CSV source to point into.
    pub const UNKNOWN: Self = Self {
        byte: 0,
        line: 0,
        record: 0,
        field: 0,
    };

    /// Start of a source, before anything has been read.
    pub(crate) const START: Self = Self {
        byte: 0,
        line: 1,
        record: 0,
        field: 0,
    };

    /// Whether this location identifies a position in a CSV source.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        self.line != 0
    }
}

/// Renders a known location as `byte B, line L, record R, field F`.
///
/// An unknown location renders as `unknown location`.
impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_known() {
            write!(
                f,
                "byte {}, line {}, record {}, field {}",
                self.byte, self.line, self.record, self.field,
            )
        } else {
            f.write_str("unknown location")
        }
    }
}

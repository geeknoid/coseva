use core::error::Error as StdError;
use core::fmt;
use core::str;
#[cfg(feature = "std")]
use std::io;

/// Failure category.
///
/// This is both the category of a full [`Error`](super::Error) and the reason a single
/// field could not be converted, so it doubles as the error type of the
/// built-in [`crate::FromBytes`] implementations. Categories that only a
/// parser can raise, such as an I/O failure, never come from a conversion.
/// For a worked example, see [`super::Error`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The input or output operation failed.
    #[cfg(feature = "std")]
    Io(io::ErrorKind),
    /// A string-oriented API encountered invalid UTF-8.
    InvalidUtf8(str::Utf8Error),
    /// A conversion was given a field with no value.
    EmptyField,
    /// A numeric conversion found a byte that is not a valid digit.
    InvalidDigit,
    /// A numeric conversion produced a value the target type cannot hold.
    OutOfRange,
    /// The target type rejected an otherwise well-formed field.
    InvalidValue,
    /// Parser or emitter configuration was invalid.
    Configuration,
    /// A requested header name was absent.
    MissingHeader,
    /// A requested header name occurred more than once.
    DuplicateHeader,
    /// Native typed decoding failed.
    Decode,
    /// Optional Serde conversion failed.
    Serde,
    /// A leading UTF-8 byte-order mark was rejected by policy.
    RejectedBom,
    /// A quoted field reached end of input before its closing quote.
    UnterminatedQuotedField,
    /// A quote appeared inside an unquoted field.
    UnexpectedQuote,
    /// A non-structural byte followed a closing quote.
    UnexpectedByteAfterQuote(u8),
    /// A backslash escape targeted an unsupported byte.
    InvalidEscape(u8),
    /// A bare CR or LF appeared where [`crate::config::RecordEnding::CrLf`] requires an
    /// exact `\r\n` record boundary.
    InvalidRecordEnding(u8),
    /// A record exceeded its configured byte limit.
    RecordTooLarge {
        /// Configured byte limit.
        limit: usize,
    },
    /// A field exceeded its configured byte limit.
    FieldTooLarge {
        /// Configured byte limit.
        limit: usize,
    },
    /// A record exceeded its configured field-count limit.
    TooManyFields {
        /// Configured field-count limit.
        limit: usize,
    },
    /// A record had the wrong number of fields.
    FieldCountMismatch {
        /// Required number of fields.
        expected: usize,
        /// Parsed number of fields.
        actual: usize,
    },
    /// The parser cannot continue after an earlier syntax error.
    ParserFailed,
    /// A record cannot be represented under the configured encoding policy.
    Encode,
    /// The emitter cannot continue after an earlier failure.
    EmitterFailed,
    /// A file being appended to does not end with a record terminator.
    UnterminatedRecord,
    /// A byte or record location could not be represented.
    LocationOverflow,
    /// A persistent index was built for different source bytes.
    SourceMismatch,
    /// A requested record does not exist in a persistent index.
    RecordOutOfRange {
        /// Requested zero-based record.
        record: usize,
    },
    /// Persisted index bytes were malformed or unsupported.
    InvalidIndex,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            #[cfg(feature = "std")]
            Self::Io(kind) => write!(f, "input or output failed: {kind}"),
            Self::InvalidUtf8(error) => write!(f, "not UTF-8: {error}"),
            Self::EmptyField => f.write_str("cannot convert an empty field"),
            Self::InvalidDigit => f.write_str("field contains an invalid digit"),
            Self::OutOfRange => f.write_str("value does not fit in the target type"),
            Self::InvalidValue => f.write_str("field is not a valid value for the target type"),
            Self::Configuration => f.write_str("invalid configuration"),
            Self::MissingHeader => f.write_str("missing header"),
            Self::DuplicateHeader => f.write_str("duplicate header"),
            Self::Decode => f.write_str("typed decoding failed"),
            Self::Serde => f.write_str("Serde conversion failed"),
            Self::RejectedBom => f.write_str("leading UTF-8 BOM is not permitted"),
            Self::UnterminatedQuotedField => f.write_str("unterminated quoted field"),
            Self::UnexpectedQuote => f.write_str("quote in unquoted field"),
            Self::UnexpectedByteAfterQuote(byte) => {
                write!(f, "byte {byte:#04x} after closing quote")
            }
            Self::InvalidEscape(byte) => write!(f, "invalid escape target {byte:#04x}"),
            Self::InvalidRecordEnding(byte) => {
                write!(f, "byte {byte:#04x} is not a valid CRLF record ending")
            }
            Self::RecordTooLarge { limit } => write!(f, "record exceeds {limit} bytes"),
            Self::FieldTooLarge { limit } => write!(f, "field exceeds {limit} bytes"),
            Self::TooManyFields { limit } => write!(f, "record exceeds {limit} fields"),
            Self::FieldCountMismatch { expected, actual } => {
                write!(f, "expected {expected} fields but found {actual}")
            }
            Self::ParserFailed => f.write_str("parser stopped after an earlier error"),
            Self::Encode => f.write_str("record cannot be encoded under this policy"),
            Self::EmitterFailed => f.write_str("emitter stopped after an earlier error"),
            Self::UnterminatedRecord => {
                f.write_str("cannot append: file does not end with a record terminator")
            }
            Self::LocationOverflow => f.write_str("CSV location exceeds supported range"),
            Self::SourceMismatch => f.write_str("index belongs to different source bytes"),
            Self::RecordOutOfRange { record } => {
                write!(f, "record {record} is outside the index")
            }
            Self::InvalidIndex => f.write_str("index data is malformed or unsupported"),
        }
    }
}

impl StdError for ErrorKind {}

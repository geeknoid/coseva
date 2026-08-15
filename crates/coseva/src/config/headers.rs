use crate::ByteRecord;

/// How a parser obtains headers.
///
/// Headers drive name-based lookup and typed decoding. By default the first
/// record is consumed as headers; [`Headers::Provided`] supplies names for a
/// headerless document without consuming a record.
///
/// ```
/// use coseva::{ByteRecord, SliceParser};
/// use coseva::config::{FormatOptions, Headers, ParseOptions};
///
/// let names = ByteRecord::from_iter(["city", "population"]);
/// let options = ParseOptions::new().headers(Headers::Provided(names));
/// let mut parser = SliceParser::with_options(b"Boston,650706\n", FormatOptions::CSV, options)?;
///
/// assert_eq!(parser.header_index("population")?, Some(1));
/// assert_eq!(parser.byte_records().count(), 1);
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Headers {
    /// Treat every record as data.
    None,
    /// Consume the first record as headers.
    #[default]
    FirstRecord,
    /// Use caller-provided headers without consuming an input record.
    Provided(ByteRecord),
}

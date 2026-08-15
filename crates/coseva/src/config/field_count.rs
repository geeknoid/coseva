/// How the parser validates field counts.
///
/// ```
/// use coseva::SliceParser;
/// use coseva::config::{FieldCount, FormatOptions, Headers, ParseOptions};
///
/// let options = ParseOptions::new()
///     .headers(Headers::None)
///     .field_count(FieldCount::MatchFirst);
/// let mut parser = SliceParser::with_options(b"a,b,c\n1,2\n", FormatOptions::CSV, options)?;
///
/// parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected first record"))?
///     .record()?;
/// assert!(
///     parser
///         .next_line()?
///         .ok_or_else(|| std::io::Error::other("expected second record"))?
///         .record()
///         .is_err()
/// );
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldCount {
    /// Permit any number of fields.
    Flexible,
    /// Require every record to match the first record.
    MatchFirst,
    /// Require exactly this many fields.
    Exact(usize),
}

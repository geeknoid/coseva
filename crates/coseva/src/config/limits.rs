/// Resource limits enforced while scanning a record.
///
/// These bound the work and memory a single record can cost, and are checked
/// while scanning — before an oversized record can force a large allocation.
/// [`Limits::DEFAULT`] is generous enough for ordinary documents and still
/// safe for untrusted input; tighten it when you know your data.
///
/// ```
/// use coseva::SliceParser;
/// use coseva::config::{FormatOptions, Headers, Limits, ParseOptions};
///
/// let options = ParseOptions::new()
///     .headers(Headers::None)
///     .limits(Limits::new(1024, 8, 16));
/// let mut parser = SliceParser::with_options(
///     b"aaaaaaaaaaaaaaaaaaaa,b\n",
///     FormatOptions::CSV,
///     options,
/// )?;
///
/// // The first field exceeds `max_field_bytes`.
/// assert!(
///     parser
///         .next_line()?
///         .ok_or_else(|| std::io::Error::other("expected record"))?
///         .record()
///         .is_err()
/// );
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    /// Maximum raw bytes in one record.
    pub max_record_bytes: usize,
    /// Maximum raw bytes in one field.
    pub max_field_bytes: usize,
    /// Maximum fields in one record.
    pub max_fields: usize,
}

impl Limits {
    /// Conservative defaults suitable for untrusted inputs.
    pub const DEFAULT: Self = Self {
        max_record_bytes: 16 * 1024 * 1024,
        max_field_bytes: 4 * 1024 * 1024,
        max_fields: 16 * 1024,
    };

    /// Construct explicit resource limits.
    ///
    /// ```
    /// use coseva::config::Limits;
    ///
    /// let limits = Limits::new(1024, 8, 16);
    /// assert_eq!(limits.max_record_bytes, 1024);
    /// assert_eq!(limits.max_field_bytes, 8);
    /// assert_eq!(limits.max_fields, 16);
    /// ```
    #[must_use]
    pub const fn new(max_record_bytes: usize, max_field_bytes: usize, max_fields: usize) -> Self {
        Self {
            max_record_bytes,
            max_field_bytes,
            max_fields,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

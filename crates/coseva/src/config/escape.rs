/// How quotes inside quoted fields are escaped.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Escape {
    /// Escape a quote by writing it twice.
    DoubleQuote,
    /// Prefix quotes and the escape byte with this byte.
    Backslash(u8),
    /// `MySQL` text-export backslash escapes in unquoted fields.
    ///
    /// # Performance
    ///
    /// A record containing no backslash stays on the vectorized path; only a
    /// record carrying an escape falls back to the general parser. Escape-free
    /// input measures about 1.35x the instructions per record of a dialect
    /// without escaping. [`DoubleQuote`](Self::DoubleQuote) and
    /// [`Backslash`](Self::Backslash) never leave the fast path.
    Mysql,
    /// Prefix this byte before a structural byte in an unquoted field.
    ///
    /// This matches Python's `csv` module with `quoting=QUOTE_NONE` and an
    /// `escapechar`. Unlike [`Backslash`](Self::Backslash), it applies outside
    /// quoted fields. Unlike [`Mysql`](Self::Mysql), it takes the escaped byte
    /// literally.
    ///
    /// ```
    /// use coseva::config::{Escape, FormatOptions, Headers, ParseOptions, Quoting};
    /// use coseva::SliceParser;
    ///
    /// let format = FormatOptions::CSV
    ///     .escape(Escape::Unquoted(b'\\'))
    ///     .quoting(Quoting::Never);
    /// let options = ParseOptions::new().headers(Headers::None);
    /// let mut parser = SliceParser::with_options(b"a\\,b,c\n", format, options)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
    /// let record = line.record()?;
    /// assert_eq!(record.get(0), Some(&b"a,b"[..]));
    /// assert_eq!(record.get(1), Some(&b"c"[..]));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Performance
    ///
    /// Like [`Mysql`](Self::Mysql), only records carrying an escape byte leave
    /// the vectorized path.
    Unquoted(u8),
}

impl Escape {
    /// Returns the byte that escapes inside an unquoted field.
    pub(crate) const fn unquoted_byte(self) -> Option<u8> {
        match self {
            Self::DoubleQuote | Self::Backslash(_) => None,
            Self::Mysql => Some(b'\\'),
            Self::Unquoted(escape) => Some(escape),
        }
    }

    pub(crate) const fn escapes_unquoted(self) -> bool {
        self.unquoted_byte().is_some()
    }
}

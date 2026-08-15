/// ASCII-whitespace trimming policy.
///
/// Trimming is off by default, because padding around a field is data as far
/// as CSV is concerned. Turn it on for the exports that pad for readability.
/// [`unquoted_only`](Self::unquoted_only) leaves quoted fields alone, which is
/// what most tools mean by trimming: the quotes were there to protect the
/// padding.
///
/// ```
/// use coseva::SliceParser;
/// use coseva::config::{FormatOptions, Headers, ParseOptions, Whitespace};
///
/// let input = b"  a  ,\"  b  \"\n";
/// let format = FormatOptions::CSV.trim(Whitespace::ALL.unquoted_only());
/// let options = ParseOptions::new().headers(Headers::None);
/// let mut parser = SliceParser::with_options(input, format, options)?;
///
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected record"))?;
/// let record = line.record()?;
/// assert_eq!(record.get_str(0)?, Some("a"));
/// assert_eq!(record.get_str(1)?, Some("  b  "));
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Whitespace {
    fields: bool,
    headers: bool,
    quoted: bool,
}

impl Whitespace {
    /// Do not trim fields.
    pub const NONE: Self = Self {
        fields: false,
        headers: false,
        quoted: true,
    };

    /// Trim data fields but not headers.
    pub const FIELDS: Self = Self {
        fields: true,
        headers: false,
        quoted: true,
    };

    /// Trim headers but not data fields.
    pub const HEADERS: Self = Self {
        fields: false,
        headers: true,
        quoted: true,
    };

    /// Trim headers and data fields.
    pub const ALL: Self = Self {
        fields: true,
        headers: true,
        quoted: true,
    };

    /// Restrict trimming to unquoted fields.
    ///
    /// # Performance
    ///
    /// A record with no quoted field in it stays on the vectorized path, since
    /// the exemption cannot apply to a record that has nothing to exempt. A
    /// record that does contain one falls to the general parser, which is the
    /// path that knows which fields were quoted while it trims, and costs
    /// about 2.1x the instructions per record.
    #[must_use]
    pub const fn unquoted_only(mut self) -> Self {
        self.quoted = false;
        self
    }

    pub(crate) const fn applies(self, header: bool, quoted: bool) -> bool {
        (if header { self.headers } else { self.fields }) && (self.quoted || !quoted)
    }

    /// Whether this policy trims some scope but leaves quoted fields alone.
    ///
    /// Such a policy cannot be applied to a whole record at once, because the
    /// decision depends on how each field was written.
    pub(crate) const fn exempts_quoted(self) -> bool {
        !self.quoted && (self.fields || self.headers)
    }

    /// Whether this policy trims the given scope, ignoring the quoted axis.
    ///
    /// Only sound where quoted fields cannot be exempt, which the parser
    /// guarantees by routing [`Self::exempts_quoted`] policies to the path that
    /// tracks each field's quoting.
    pub(crate) const fn applies_to_scope(self, header: bool) -> bool {
        if header { self.headers } else { self.fields }
    }

    #[cfg(feature = "index")]
    pub(crate) const fn bits(self) -> u8 {
        (self.fields as u8) | ((self.headers as u8) << 1) | ((self.quoted as u8) << 2)
    }

    #[cfg(feature = "index")]
    pub(crate) const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !0b111 != 0 {
            return None;
        }
        Some(Self {
            fields: bits & 1 != 0,
            headers: bits & 0b10 != 0,
            quoted: bits & 0b100 != 0,
        })
    }
}

impl Default for Whitespace {
    fn default() -> Self {
        Self::NONE
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_methods() {
        let w = Whitespace::FIELDS.unquoted_only();
        assert!(w.exempts_quoted());
        assert!(w.applies_to_scope(false));
        assert!(!w.applies_to_scope(true));
        assert!(w.applies(false, false));
        assert!(!w.applies(false, true));
        assert!(!w.applies(true, false));

        let h = Whitespace::HEADERS;
        assert!(h.applies(true, false));
        assert!(h.applies(true, true));
        assert!(!h.applies(false, false));

        let all = Whitespace::ALL;
        assert!(all.applies_to_scope(true));
        assert!(all.applies_to_scope(false));
        assert!(!Whitespace::NONE.exempts_quoted());
        assert!(Whitespace::HEADERS.unquoted_only().exempts_quoted());

        #[cfg(feature = "index")]
        {
            let bits = w.bits();
            assert_eq!(Whitespace::from_bits(bits), Some(w));
            assert_eq!(Whitespace::from_bits(0xFF), None);
        }
    }
}

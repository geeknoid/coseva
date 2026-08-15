use super::Recovery;

/// Strict parsing, or explicitly selected compatibility relaxations.
///
/// Parsing is strict by default. When a source produces input a strict parser
/// rejects, relax exactly the rule it violates rather than turning off
/// checking wholesale — [`Recovery::PERMISSIVE`] enables all of them, and
/// [`Recovery::NONE`] plus one setter enables just one.
///
/// ```
/// use coseva::SliceParser;
/// use coseva::config::{FormatOptions, Headers, ParseOptions, Quoting, Recovery, Syntax};
///
/// // A bare quote inside an unquoted field: rejected by default.
/// let input = b"a,b\"c\n";
/// let options = ParseOptions::new().headers(Headers::None);
/// let mut parser = SliceParser::with_options(input, FormatOptions::CSV, options.clone())?;
/// assert!(
///     parser
///         .next_line()?
///         .ok_or_else(|| std::io::Error::other("expected malformed record"))?
///         .record()
///         .is_err()
/// );
///
/// // Accept just that one deviation, leaving every other check strict.
/// let recovery = Recovery::NONE.unquoted_quotes(true);
/// let format = FormatOptions::CSV
///     .syntax(Syntax::Compatible(recovery))
///     .quoting(Quoting::Never);
/// let mut parser = SliceParser::with_options(input, format, options)?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected compatible record"))?;
/// assert_eq!(line.record()?.get_str(1)?, Some("b\"c"));
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Syntax {
    /// Reject malformed or ambiguous syntax.
    #[default]
    Strict,
    /// Apply only the selected deterministic recovery switches.
    Compatible(Recovery),
}

impl Syntax {
    pub(crate) const fn quoting_enabled(self) -> bool {
        match self {
            Self::Strict => true,
            Self::Compatible(rules) => rules.quoting_enabled(),
        }
    }

    pub(crate) const fn permits_unquoted_quotes(self) -> bool {
        match self {
            Self::Strict => false,
            Self::Compatible(rules) => rules.permits_unquoted_quotes(),
        }
    }

    pub(crate) const fn permits_any_backslash_escape(self) -> bool {
        match self {
            Self::Strict => false,
            Self::Compatible(rules) => rules.permits_any_backslash_escape(),
        }
    }

    pub(crate) const fn permits_trailing_whitespace(self) -> bool {
        match self {
            Self::Strict => false,
            Self::Compatible(rules) => rules.permits_trailing_whitespace(),
        }
    }
}

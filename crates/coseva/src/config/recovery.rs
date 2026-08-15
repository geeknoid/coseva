/// Explicit recovery switches for non-strict CSV inputs.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Recovery {
    flags: u8,
}

impl Recovery {
    const QUOTING: u8 = 1 << 0;
    const UNQUOTED_QUOTES: u8 = 1 << 1;
    const ANY_BACKSLASH_ESCAPE: u8 = 1 << 2;
    const TRAILING_WHITESPACE_AFTER_QUOTE: u8 = 1 << 3;

    /// A broadly compatible, deterministic policy.
    pub const PERMISSIVE: Self = Self {
        flags: Self::QUOTING
            | Self::UNQUOTED_QUOTES
            | Self::ANY_BACKSLASH_ESCAPE
            | Self::TRAILING_WHITESPACE_AFTER_QUOTE,
    };

    /// No recovery switches, so quoting is disabled entirely.
    pub const NONE: Self = Self { flags: 0 };

    #[cfg(feature = "index")]
    const ALL_FLAGS: u8 = Self::PERMISSIVE.flags;

    /// Enable or disable quote syntax.
    #[must_use]
    pub const fn quoting(mut self, enabled: bool) -> Self {
        self.set(Self::QUOTING, enabled);
        self
    }

    /// Permit quote bytes inside unquoted fields.
    #[must_use]
    pub const fn unquoted_quotes(mut self, enabled: bool) -> Self {
        self.set(Self::UNQUOTED_QUOTES, enabled);
        self
    }

    /// Permit a backslash to escape any following byte in quoted fields.
    #[must_use]
    pub const fn any_backslash_escape(mut self, enabled: bool) -> Self {
        self.set(Self::ANY_BACKSLASH_ESCAPE, enabled);
        self
    }

    /// Permit ASCII whitespace between a closing quote and the next separator.
    #[must_use]
    pub const fn trailing_whitespace_after_quote(mut self, enabled: bool) -> Self {
        self.set(Self::TRAILING_WHITESPACE_AFTER_QUOTE, enabled);
        self
    }

    pub(crate) const fn quoting_enabled(self) -> bool {
        self.contains(Self::QUOTING)
    }

    pub(crate) const fn permits_unquoted_quotes(self) -> bool {
        self.contains(Self::UNQUOTED_QUOTES)
    }

    pub(crate) const fn permits_any_backslash_escape(self) -> bool {
        self.contains(Self::ANY_BACKSLASH_ESCAPE)
    }

    pub(crate) const fn permits_trailing_whitespace(self) -> bool {
        self.contains(Self::TRAILING_WHITESPACE_AFTER_QUOTE)
    }

    const fn contains(self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    #[cfg(feature = "index")]
    pub(crate) const fn bits(self) -> u8 {
        self.flags
    }

    #[cfg(feature = "index")]
    pub(crate) const fn from_bits(flags: u8) -> Option<Self> {
        if flags & !Self::ALL_FLAGS == 0 {
            Some(Self { flags })
        } else {
            None
        }
    }

    const fn set(&mut self, flag: u8, enabled: bool) {
        if enabled {
            self.flags |= flag;
        } else {
            self.flags &= !flag;
        }
    }
}

impl Default for Recovery {
    fn default() -> Self {
        Self::PERMISSIVE
    }
}

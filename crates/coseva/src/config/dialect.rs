#[cfg(feature = "index")]
use crate::error::{Error, ErrorKind};

use super::{Escape, RecordEnding, Tail};

#[cfg(all(feature = "index", any(not(feature = "multibyte"), test)))]
fn reject_unsupported_tails(delimiter: Tail, ending: Tail) -> Result<(), Error> {
    if delimiter.is_empty() {
        if ending.is_empty() {
            return Ok(());
        }
    }
    Err(Error::detailed(
        ErrorKind::Configuration,
        "the index records a multi-byte separator, which needs the `multibyte` feature",
    ))
}

/// CSV syntax shared by parsers and emitters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Dialect {
    pub(crate) delimiter: u8,
    pub(crate) quote: u8,
    pub(crate) record_ending: RecordEnding,
    pub(crate) escape: Escape,
    pub(crate) comment: Option<u8>,
    /// What must follow `delimiter` for it to delimit, empty when nothing must.
    #[cfg(feature = "multibyte")]
    pub(crate) delimiter_tail: Tail,
    /// What must follow the record ending's byte, empty when nothing must.
    #[cfg(feature = "multibyte")]
    pub(crate) ending_tail: Tail,
}

impl Dialect {
    /// What must follow the delimiter byte for it to delimit.
    ///
    /// Always present, and always empty without the `multibyte` feature, so
    /// that the parse and emit paths ask the same question either way and fold
    /// the answer away when the feature is off.
    #[cfg_attr(
        not(feature = "multibyte"),
        expect(
            clippy::unused_self,
            reason = "the field it reads only exists with the `multibyte` feature, and callers must not have to know that"
        )
    )]
    pub(crate) const fn delimiter_tail(self) -> Tail {
        #[cfg(feature = "multibyte")]
        {
            self.delimiter_tail
        }
        #[cfg(not(feature = "multibyte"))]
        {
            Tail::EMPTY
        }
    }

    /// What must follow the record ending's byte for it to terminate.
    #[cfg_attr(
        not(feature = "multibyte"),
        expect(
            clippy::unused_self,
            reason = "the field it reads only exists with the `multibyte` feature, and callers must not have to know that"
        )
    )]
    pub(crate) const fn ending_tail(self) -> Tail {
        #[cfg(feature = "multibyte")]
        {
            self.ending_tail
        }
        #[cfg(not(feature = "multibyte"))]
        {
            Tail::EMPTY
        }
    }

    /// Whether either separator is longer than one byte.
    ///
    /// This is the one question the rest of the crate asks about multi-byte
    /// support, and it is what takes such a dialect off every path built on
    /// single-byte matching.
    pub(crate) const fn multibyte(self) -> bool {
        !self.delimiter_tail().is_empty() || !self.ending_tail().is_empty()
    }

    /// Standard comma-separated values with doubled-quote escaping.
    pub(crate) const CSV: Self = Self {
        delimiter: b',',
        quote: b'"',
        record_ending: RecordEnding::Newline,
        escape: Escape::DoubleQuote,
        comment: None,
        #[cfg(feature = "multibyte")]
        delimiter_tail: Tail::EMPTY,
        #[cfg(feature = "multibyte")]
        ending_tail: Tail::EMPTY,
    };

    /// Tab-separated values with doubled-quote escaping.
    pub(crate) const TSV: Self = Self {
        delimiter: b'\t',
        ..Self::CSV
    };

    /// Semicolon-separated values with doubled-quote escaping.
    pub(crate) const SEMICOLON: Self = Self {
        delimiter: b';',
        ..Self::CSV
    };

    /// Pipe-delimited values with doubled-quote escaping.
    pub(crate) const PIPE: Self = Self {
        delimiter: b'|',
        ..Self::CSV
    };

    /// Comma-separated values with backslash escaping inside quoted fields.
    pub(crate) const BACKSLASH_CSV: Self = Self {
        escape: Escape::Backslash(b'\\'),
        ..Self::CSV
    };

    /// Tab-separated values with backslash escaping inside quoted fields.
    pub(crate) const BACKSLASH_TSV: Self = Self {
        delimiter: b'\t',
        escape: Escape::Backslash(b'\\'),
        ..Self::CSV
    };

    /// CSV with `#` comments recognized at the start of records.
    pub(super) const COMMENTED_CSV: Self = Self {
        comment: Some(b'#'),
        ..Self::CSV
    };

    /// Strict RFC 4180 CSV with mandatory CRLF record terminators.
    pub(super) const RFC4180: Self = Self {
        record_ending: RecordEnding::CrLf,
        ..Self::CSV
    };

    /// Excel-compatible CSV syntax.
    pub(crate) const EXCEL: Self = Self::RFC4180;

    /// `PostgreSQL` `COPY ... CSV` syntax.
    pub(crate) const POSTGRES_COPY_CSV: Self = Self::CSV;

    /// `MySQL` text-export syntax.
    pub(crate) const MYSQL: Self = Self {
        delimiter: b'\t',
        escape: Escape::Mysql,
        ..Self::CSV
    };

    /// Python `csv` with `quoting=QUOTE_NONE` and `escapechar='\\'`.
    pub(crate) const PYTHON_ESCAPED: Self = Self {
        escape: Escape::Unquoted(b'\\'),
        ..Self::CSV
    };

    /// Construct and validate a dialect.
    ///
    /// # Errors
    ///
    /// Returns an error when structural bytes are ambiguous.
    #[cfg(feature = "index")]
    pub(crate) fn new(
        delimiter: u8,
        quote: u8,
        record_ending: RecordEnding,
        escape: Escape,
    ) -> Result<Self, Error> {
        let dialect = Self {
            delimiter,
            quote,
            record_ending,
            escape,
            comment: None,
            #[cfg(feature = "multibyte")]
            delimiter_tail: Tail::EMPTY,
            #[cfg(feature = "multibyte")]
            ending_tail: Tail::EMPTY,
        };
        dialect.validate()?;
        Ok(dialect)
    }

    /// Add a comment byte recognized at the start of records.
    ///
    /// # Errors
    ///
    /// Returns an error when the comment byte conflicts with the dialect.
    #[cfg(feature = "index")]
    pub(crate) fn with_comment(mut self, comment: u8) -> Result<Self, Error> {
        self.comment = Some(comment);
        self.validate()?;
        Ok(self)
    }

    /// Field delimiter.
    #[must_use]
    #[cfg(feature = "index")]
    pub(crate) const fn delimiter(self) -> u8 {
        self.delimiter
    }

    /// Quote byte.
    #[must_use]
    #[cfg(feature = "index")]
    pub(crate) const fn quote(self) -> u8 {
        self.quote
    }

    /// Record record ending.
    #[must_use]
    #[cfg(feature = "index")]
    pub(crate) const fn record_ending(self) -> RecordEnding {
        self.record_ending
    }

    /// Escape style.
    #[must_use]
    #[cfg(feature = "index")]
    pub(crate) const fn escape(self) -> Escape {
        self.escape
    }

    /// Optional comment byte.
    #[must_use]
    #[cfg(feature = "index")]
    pub(crate) const fn comment(self) -> Option<u8> {
        self.comment
    }

    /// Attach separator tails, restoring a multi-byte dialect.
    ///
    /// # Errors
    ///
    /// Returns an error when either sequence is unusable or ambiguous.
    #[cfg(feature = "index")]
    #[cfg_attr(
        not(feature = "multibyte"),
        expect(
            unused_mut,
            reason = "the fields it would set only exist with the `multibyte` feature"
        )
    )]
    pub(crate) fn with_tails(mut self, delimiter: Tail, ending: Tail) -> Result<Self, Error> {
        #[cfg(feature = "multibyte")]
        {
            self.delimiter_tail = delimiter;
            self.ending_tail = ending;
        };
        #[cfg(not(feature = "multibyte"))]
        reject_unsupported_tails(delimiter, ending)?;
        self.validate()?;
        Ok(self)
    }

    /// The reason this dialect is unusable, or `None` when it is sound.
    ///
    /// This is the `const` half of [`Self::validate`], so that a format
    /// declared with [`crate::csv_format`] is rejected at compile time rather
    /// than when a parser is built. `validate` defers to it, so the two cannot
    /// disagree about what is valid.
    pub(crate) const fn invalidity(self) -> Option<&'static str> {
        let record_ending = self.record_ending.byte();
        let strict_crlf = matches!(self.record_ending, RecordEnding::CrLf);
        if self.delimiter == self.quote {
            return Some("delimiter and quote must be distinct");
        }
        if record_ending == self.delimiter
            || record_ending == self.quote
            || (strict_crlf && (self.delimiter == b'\r' || self.quote == b'\r'))
        {
            return Some("record_ending must differ from delimiter and quote");
        }
        match self.escape {
            Escape::Backslash(escape)
                if escape == self.delimiter || escape == self.quote || escape == record_ending =>
            {
                return Some("escape must differ from structural bytes");
            }
            Escape::Mysql
                if self.delimiter == b'\\' || self.quote == b'\\' || record_ending == b'\\' =>
            {
                return Some("MySQL escape byte must differ from structural bytes");
            }
            Escape::Unquoted(escape)
                if escape == self.delimiter || escape == self.quote || escape == record_ending =>
            {
                return Some("escape must differ from structural bytes");
            }
            Escape::DoubleQuote | Escape::Backslash(_) | Escape::Mysql | Escape::Unquoted(_) => {}
        }
        if let Some(comment) = self.comment
            && (comment == self.delimiter
                || comment == self.quote
                || comment == record_ending
                || match self.escape.unquoted_byte() {
                    Some(escape) => escape == comment,
                    None => false,
                }
                || (strict_crlf && comment == b'\r'))
        {
            return Some("comment must differ from structural bytes");
        }
        self.multibyte_invalidity()
    }

    /// The reason the multi-byte part of this dialect is unusable, if any.
    ///
    /// Split out of [`Self::invalidity`] because the single-byte rules above
    /// are what almost every dialect is judged by, and reading them should not
    /// mean reading these.
    const fn multibyte_invalidity(self) -> Option<&'static str> {
        // Nothing below can fire for a single-byte dialect, and this runs
        // whenever one is built, so leave before the loop rather than walk two
        // empty tails to reach the same answer.
        if !self.multibyte() {
            return None;
        }
        if self.delimiter_tail().unusable() || self.ending_tail().unusable() {
            return Some("a delimiter or record ending sequence must be 1 to 4 bytes");
        }
        if !self.ending_tail().is_empty() && !matches!(self.record_ending, RecordEnding::Byte(_)) {
            return Some("a multi-byte record ending must be spelled as a byte sequence");
        }
        // A tail byte may repeat its own lead -- `||` is the motivating case --
        // but a quote or escape inside a separator would make the scan's other
        // branches fire in the middle of it.
        let quote = self.quote;
        let escape = match self.escape {
            Escape::DoubleQuote => quote,
            Escape::Backslash(byte) | Escape::Unquoted(byte) => byte,
            Escape::Mysql => b'\\',
        };
        let tails = [self.delimiter_tail(), self.ending_tail()];
        let mut which = 0;
        while which < tails.len() {
            let bytes = tails[which].as_slice();
            let mut at = 0;
            while at < bytes.len() {
                if bytes[at] == quote || bytes[at] == escape {
                    return Some(
                        "a delimiter or record ending sequence must not contain the quote or escape byte",
                    );
                }
                at += 1;
            }
            which += 1;
        }
        None
    }

    #[cfg(feature = "index")]
    pub(crate) fn validate(self) -> Result<(), Error> {
        match self.invalidity() {
            Some(reason) => Err(Error::detailed(ErrorKind::Configuration, reason)),
            None => Ok(()),
        }
    }
}

impl Default for Dialect {
    fn default() -> Self {
        Self::CSV
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(all(test, feature = "index"))]
mod tests {
    use super::*;

    #[test]
    fn restoring_tails_without_multibyte_rejects_either_nonempty_sequence() {
        let message =
            "the index records a multi-byte separator, which needs the `multibyte` feature";
        let nonempty = Tail::from_parts(1, [b':', 0, 0]);

        reject_unsupported_tails(Tail::EMPTY, Tail::EMPTY)
            .expect("empty tails preserve a single-byte dialect");
        for (delimiter, ending) in [
            (nonempty, Tail::EMPTY),
            (Tail::EMPTY, nonempty),
            (nonempty, nonempty),
        ] {
            let error = reject_unsupported_tails(delimiter, ending)
                .expect_err("a nonempty tail requires multibyte support");
            assert_eq!(error.to_string(), message);
        }
    }
}

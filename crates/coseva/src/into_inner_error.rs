use core::error::Error as StdError;
use core::fmt;

use crate::error::Error;

/// Failure returned when flushing a consumed [`IoEmitter`](crate::IoEmitter).
///
/// The complete emitter is retained so callers can inspect it, recover the
/// underlying sink, or retry through sink-specific recovery mechanisms.
/// For a worked example, see [`crate::IoEmitter`].
pub struct IntoInnerError<W> {
    writer: Box<W>,
    error: Error,
}

impl<W> IntoInnerError<W> {
    pub(crate) fn new(writer: W, error: Error) -> Self {
        Self {
            writer: Box::new(writer),
            error,
        }
    }

    /// Borrow the flush error.
    #[must_use]
    pub const fn error(&self) -> &Error {
        &self.error
    }

    /// Consume this value and return the flush error.
    #[must_use]
    pub fn into_error(self) -> Error {
        self.error
    }

    /// Consume this value and recover the unconsumed emitter.
    #[must_use]
    pub fn into_inner(self) -> W {
        *self.writer
    }
}

impl<W> fmt::Debug for IntoInnerError<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IntoInnerError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<W> fmt::Display for IntoInnerError<W> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<W> StdError for IntoInnerError<W> {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(&self.error)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[test]
    fn debug_output_keeps_the_wrapper_and_error_field_labels() {
        let wrapped =
            IntoInnerError::new(17_u8, Error::detailed(ErrorKind::Decode, "flush failed"));
        let rendered = format!("{wrapped:?}");
        assert!(rendered.starts_with("IntoInnerError {"), "{rendered}");
        assert!(rendered.contains("error: Error"), "{rendered}");
    }
}

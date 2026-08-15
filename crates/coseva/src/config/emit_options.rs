#[cfg(feature = "std")]
use crate::error::Error;

use super::{DEFAULT_WRITE_BUFFER_BYTES, FieldCount};
#[cfg(feature = "std")]
use super::{FormatOptions, validate_buffer_capacity};

/// Per-session emitter settings, independent of the CSV format.
///
/// Pair this with a [`FormatOptions`](super::FormatOptions) when constructing an emitter, for example
/// with [`crate::VecEmitter::with_options`].
///
/// ```
/// use coseva::config::{EmitOptions, FieldCount, FormatOptions};
/// use coseva::VecEmitter;
///
/// // Reject any record that does not have exactly two fields.
/// let options = EmitOptions::new().field_count(FieldCount::Exact(2));
/// let mut emitter = VecEmitter::with_options(Vec::new(), FormatOptions::TSV, options)?;
///
/// emitter.emit_record(["Boston", "650706"])?;
/// assert!(emitter.emit_record(["too", "many", "fields"]).is_err());
///
/// assert_eq!(emitter.as_bytes(), b"Boston\t650706\n");
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmitOptions {
    field_count: FieldCount,
    has_headers: bool,
    buffer_capacity: usize,
}

impl EmitOptions {
    /// Start with flexible field counts and automatic Serde headers.
    ///
    /// ```
    /// use coseva::config::{EmitOptions, FormatOptions};
    /// use coseva::VecEmitter;
    ///
    /// let mut emitter = VecEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            field_count: FieldCount::Flexible,
            has_headers: true,
            buffer_capacity: DEFAULT_WRITE_BUFFER_BYTES,
        }
    }

    /// Configure field-count validation.
    #[must_use]
    pub const fn field_count(mut self, field_count: FieldCount) -> Self {
        self.field_count = field_count;
        self
    }

    /// Configure automatic headers for the first Serde-serialized struct, and
    /// for the whole-document generation entry points.
    ///
    /// [`crate::encode_to_vec`] and its siblings write a header record from
    /// [`crate::encoding::CsvEncode::field_names`] when this is set. It has no
    /// effect on the per-record native or untyped emitter methods, which never
    /// write a header unless asked. Enabled by default.
    #[must_use]
    pub const fn has_headers(mut self, has_headers: bool) -> Self {
        self.has_headers = has_headers;
        self
    }

    /// Set the buffered output threshold, in bytes.
    ///
    /// [`crate::IoEmitter`] buffers records and writes to the sink once
    /// this many bytes have accumulated, so wrapping the sink in a
    /// [`std::io::BufWriter`] is redundant. A record larger than the
    /// threshold is still appended and drained immediately, so resident
    /// memory is the threshold plus the largest recent record, never the
    /// whole document. Records only reach the sink once the threshold is
    /// crossed, so [`crate::IoEmitter::flush`] is what surfaces their
    /// I/O errors.
    #[must_use]
    pub const fn buffer_capacity(mut self, capacity: usize) -> Self {
        self.buffer_capacity = capacity;
        self
    }

    pub(crate) const fn field_count_policy(self) -> FieldCount {
        self.field_count
    }

    pub(crate) const fn writes_headers(self) -> bool {
        self.has_headers
    }

    #[cfg(feature = "std")]
    pub(crate) const fn capacity(self) -> usize {
        self.buffer_capacity
    }

    #[cfg(feature = "std")]
    pub(crate) fn validate_buffered(self, format: FormatOptions) -> Result<(), Error> {
        format.validate()?;
        validate_buffer_capacity(self.buffer_capacity)
    }
}

impl Default for EmitOptions {
    fn default() -> Self {
        Self::new()
    }
}

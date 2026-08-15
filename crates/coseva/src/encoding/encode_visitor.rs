#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;

use crate::error::Error;

/// Callback interface for receiving encoded CSV fields.
///
/// [`CsvEncode::csv_encode`](super::CsvEncode::csv_encode) calls [`visit_field`](EncodeVisitor::visit_field)
/// once for each output field in declaration order. Implementations may write
/// to a CSV destination, collect fields in memory, or inspect the encoded
/// bytes without copying them.
/// For a worked example, see [`CollectVisitor`].
pub trait EncodeVisitor {
    /// Receive one encoded field.
    ///
    /// `index` is the zero-based location of this field in the encoded record.
    /// `name` is the static CSV field name. `bytes` are the raw encoded
    /// bytes for this field, **not** including any quoting or escaping.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the field cannot be accepted, for
    /// example because the underlying I/O sink failed.
    fn visit_field(&mut self, index: usize, name: &'static str, bytes: &[u8]) -> Result<(), Error>;

    /// Receive an explicit NULL field.
    ///
    /// `index` and `name` identify the field exactly as in [`Self::visit_field`].
    ///
    /// The default implementation forwards to [`Self::visit_field`] with an
    /// empty byte slice, so visitors that do not distinguish NULL from empty
    /// (for example plain in-memory collectors) keep their existing
    /// behavior unchanged. Visitors that can represent NULL distinctly
    /// (for example a [`crate::ByteRecord`]-backed collector destined for a
    /// database export) should override this method instead.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] under the same conditions as
    /// [`Self::visit_field`].
    fn visit_null(&mut self, index: usize, name: &'static str) -> Result<(), Error> {
        self.visit_field(index, name, b"")
    }
}

// ── CollectVisitor ─────────────────────────────────────────────────────────────

/// An [`EncodeVisitor`] that collects encoded fields into a [`Vec`].
///
/// Suitable for testing and any use case where you need to materialize
/// the encoded fields of a [`CsvEncode`](super::CsvEncode) record without involving an emitter.
///
/// ```
/// # use coseva::encoding::{CollectVisitor, CsvEncode};
/// # struct MyRecord { val: u8 }
/// # impl CsvEncode for MyRecord {
/// #     fn csv_encode<V: coseva::encoding::EncodeVisitor>(&self, v: &mut V) -> Result<(), coseva::Error> {
/// #         v.visit_field(0, "val", &[self.val])
/// #     }
/// #     fn field_names() -> &'static [&'static str] { &["val"] }
/// # }
/// let record = MyRecord { val: 42 };
/// let mut visitor = CollectVisitor::new();
/// record.csv_encode(&mut visitor)?;
/// assert_eq!(visitor.fields(), &[vec![42u8]]);
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct CollectVisitor {
    fields: Vec<Vec<u8>>,
}

impl CollectVisitor {
    /// Construct a new empty visitor.
    #[must_use]
    pub const fn new() -> Self {
        Self { fields: Vec::new() }
    }

    /// Return a slice of the collected fields.
    #[must_use]
    pub fn fields(&self) -> &[Vec<u8>] {
        &self.fields
    }

    /// Consume the visitor and return the collected fields.
    #[must_use]
    pub fn into_fields(self) -> Vec<Vec<u8>> {
        self.fields
    }

    /// Number of fields collected so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether no fields have been collected yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl EncodeVisitor for CollectVisitor {
    fn visit_field(
        &mut self,
        _index: usize,
        _name: &'static str,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.fields.push(bytes.to_vec());
        Ok(())
    }
}

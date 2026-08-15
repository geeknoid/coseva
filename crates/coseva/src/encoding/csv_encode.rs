use crate::error::Error;

use super::EncodeVisitor;

/// Write your own type out as one CSV record.
///
/// Derive it with `#[derive(CsvEncode)]` (feature `derive`). Fields become
/// columns in declaration order, and [`field_names`](CsvEncode::field_names)
/// supplies the header row, so the header cannot drift out of sync with the
/// values written under it. See the [module docs](super) for the
/// `#[csv(...)]` attributes that control the mapping.
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::encoding::CsvEncode;
/// use coseva::VecEmitter;
///
/// #[derive(CsvEncode)]
/// struct City {
///     name: &'static str,
///     population: u64,
/// }
///
/// let mut emitter = VecEmitter::default();
/// emitter.encode_header::<City>()?;
/// emitter.encode_all([
///     City { name: "Boston", population: 650_706 },
///     City { name: "Denver", population: 715_522 },
/// ])?;
///
/// assert_eq!(
///     emitter.as_bytes(),
///     b"name,population\nBoston,650706\nDenver,715522\n",
/// );
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
pub trait CsvEncode {
    /// Encode `self` by calling the visitor once per output field.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the visitor rejects a field or a
    /// `format_with` function fails.
    fn csv_encode<V: EncodeVisitor>(&self, visitor: &mut V) -> Result<(), Error>;

    /// Static CSV field names in the same order as the encoded fields.
    fn field_names() -> &'static [&'static str];
}

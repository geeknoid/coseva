use super::*;

/// Options used to build a [`CsvIndex`].
///
/// These are stored in the index and reused whenever it hands back a parser,
/// so a record reached by seeking is always interpreted with the same format
/// used to find it.
///
/// ```
/// use coseva::config::{FormatOptions, Limits};
/// use coseva::index::{CsvIndex, IndexOptions};
///
/// let options = IndexOptions {
///     format: FormatOptions::TSV,
///     limits: Limits::DEFAULT,
/// };
/// let index = CsvIndex::build(b"a\tb\nc\td\n", options)?;
///
/// assert_eq!(index.len(), 2);
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IndexOptions {
    /// CSV format used to find record boundaries.
    ///
    /// Must match the document, or record boundaries will be found in the
    /// wrong places.
    pub format: FormatOptions,
    /// Parsing limits enforced while indexing.
    pub limits: Limits,
}

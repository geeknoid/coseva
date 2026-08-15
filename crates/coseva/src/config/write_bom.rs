/// Whether an emitter emits a UTF-8 byte-order mark.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WriteBom {
    /// Do not emit a BOM.
    #[default]
    Omit,
    /// Emit one BOM before the first successfully encoded record.
    Emit,
}

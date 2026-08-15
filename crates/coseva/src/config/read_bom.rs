/// How leading UTF-8 byte-order marks are handled.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReadBom {
    /// Strip one leading UTF-8 BOM.
    #[default]
    Detect,
    /// Preserve a leading BOM as field data.
    Preserve,
    /// Reject an input beginning with a UTF-8 BOM.
    Reject,
}

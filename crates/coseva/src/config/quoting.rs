/// When the emitter quotes a field.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Quoting {
    /// Quote only fields containing structural bytes.
    Necessary,
    /// Quote every field.
    Always,
    /// Never quote fields. Structurally ambiguous fields are rejected.
    Never,
    /// Quote fields that are not valid Rust floating-point spellings.
    NonNumeric,
    /// Write fields verbatim even when the result is structurally ambiguous.
    Raw,
}

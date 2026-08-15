/// How physical blank lines are handled.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BlankRecords {
    /// Return a blank line as a record containing one empty field.
    #[default]
    Preserve,
    /// Ignore physical blank lines.
    Skip,
}

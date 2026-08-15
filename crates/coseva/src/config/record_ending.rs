/// How records are terminated.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordEnding {
    /// A line feed, optionally preceded by a carriage return.
    Newline,
    /// A carriage return followed by a line feed.
    ///
    /// # Performance
    ///
    /// Both endings scan for the same byte, so this one stays on the vectorized
    /// path: the extra strictness is enforced by a pass over each finished
    /// record, which costs roughly 1.4x the instructions per record of
    /// [`Newline`](Self::Newline). Prefer `Newline` unless a lone `\n` must be
    /// rejected; it already accepts `\r\n` and strips the carriage return.
    CrLf,
    /// A specific byte.
    Byte(u8),
}

impl RecordEnding {
    pub(crate) const fn byte(self) -> u8 {
        match self {
            Self::Newline | Self::CrLf => b'\n',
            Self::Byte(byte) => byte,
        }
    }
}

/// How an explicit database NULL is represented.
///
/// # Performance
///
/// NULL detection runs over each finished record rather than inside the parse
/// kernels. It costs roughly 1.5x the instructions per non-header record of a
/// dialect without it.
///
/// For a worked example, see [`crate::config::FormatOptions`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Nulls {
    /// The format has no distinguished NULL representation.
    #[default]
    None,
    /// An unquoted empty field is NULL, as in `PostgreSQL` `COPY ... CSV`.
    PostgresCsv,
    /// An unescaped `\N` field is NULL, as in `MySQL` text exports.
    Mysql,
}

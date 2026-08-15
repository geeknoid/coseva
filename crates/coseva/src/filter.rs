//! Column predicates for filtering records while reading.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::borrow::Cow;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::{boxed::Box, string::String, vec::Vec};
#[cfg(any(feature = "std", test))]
use std::borrow::Cow;

use crate::search::find_literal;

/// How a field value is compared against the literal.
///
/// Chosen by the [`Predicate`] constructor you call — [`Predicate::equals`],
/// [`Predicate::contains`], [`Predicate::starts_with`], or
/// [`Predicate::ends_with`]. Comparison is on raw bytes, so it needs no UTF-8
/// validation and is case-sensitive.
/// For a worked example, see [`Predicate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchKind {
    /// The field equals the literal exactly.
    Equals,
    /// The field contains the literal.
    Contains,
    /// The field begins with the literal.
    StartsWith,
    /// The field ends with the literal.
    EndsWith,
}

/// Which column a [`Predicate`] applies to.
///
/// Built from a `&str` for a header name or a `usize` for a position, so the
/// explicit form is rarely needed:
///
/// ```
/// use coseva::{Column, Predicate};
///
/// let by_name = Predicate::equals("country", "US");
/// let by_index = Predicate::equals(1, "US");
///
/// assert_eq!(by_name.column(), &Column::Name("country".into()));
/// assert_eq!(by_index.column(), &Column::Index(1));
/// ```
///
/// A predicate naming a header the document does not have matches no records.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Column {
    /// A zero-based field position.
    Index(usize),
    /// A header name, resolved against the parser's headers.
    Name(Cow<'static, str>),
}

impl Column {
    /// Name a column without copying it.
    ///
    /// The conversion from `&str` has to allocate, because it accepts a name
    /// of any lifetime and the column outlives it. This does not, so the
    /// common case of a literal column name costs nothing:
    ///
    /// ```
    /// use coseva::{Column, Predicate};
    ///
    /// let predicate = Predicate::equals(Column::borrowed("country"), "US");
    /// assert_eq!(predicate.column(), &Column::borrowed("country"));
    /// ```
    ///
    /// The two forms compare and behave identically; only the allocation
    /// differs. It is a separate constructor rather than another `From`
    /// because `From<&'static str>` and `From<&'a str>` overlap, and coherence
    /// permits only one of them.
    #[must_use]
    pub const fn borrowed(name: &'static str) -> Self {
        Self::Name(Cow::Borrowed(name))
    }

    /// The header name this column matches, if it is named rather than
    /// positional.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Index(_) => None,
            Self::Name(name) => Some(name),
        }
    }
}

impl From<usize> for Column {
    fn from(index: usize) -> Self {
        Self::Index(index)
    }
}

impl From<&str> for Column {
    /// Copies `name`, since a column of any lifetime must outlive it. Use
    /// [`Column::borrowed`] for a `&'static str`.
    fn from(name: &str) -> Self {
        Self::Name(Cow::Owned(name.into()))
    }
}

impl From<String> for Column {
    /// Takes ownership of `name` without copying it.
    fn from(name: String) -> Self {
        Self::Name(Cow::Owned(name))
    }
}

/// A single-column match against a byte literal.
///
/// A predicate is handed to the reader rather than applied after the fact:
/// because it is an inspectable value rather than an opaque closure, the
/// reader can use the literal to skip non-matching records without splitting
/// them into fields, unescaping them, or allocating. The saving grows as
/// matches get rarer; the reader detects and bounds the small extra cost when
/// nearly everything matches.
///
/// Matching is on raw bytes, so it needs no UTF-8 validation and works on
/// binary field values.
///
/// # Examples
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::{Predicate, SliceParser};
///
/// let mut parser = SliceParser::<Csv>::new(b"city,country\nBoston,US\nParis,FR\nDenver,US\n", ParseOptions::new())?;
///
/// let predicate = Predicate::equals("country", "US");
/// let mut cities = Vec::new();
/// while let Some(mut line) = parser.next_matching_line(&predicate)? {
///     let city = line.record()?.get_str(0)?.unwrap_or_default().to_owned();
///     cities.push(city);
/// }
/// assert_eq!(cities, ["Boston", "Denver"]);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// The same predicates drive the filtering iterators, such as
/// [`matching_byte_records`](crate::SliceParser::matching_byte_records) and
/// [`matching_decoded_records`](crate::SliceParser::matching_decoded_records).
///
/// # Correctness
///
/// A predicate always yields exactly the records that match it. Escaped input
/// where the literal could straddle an escape sequence is handled by inspecting
/// every record instead of skipping, so quoting and escaping never change the
/// result — only how quickly it is reached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Predicate {
    column: Column,
    kind: MatchKind,
    literal: Box<[u8]>,
    /// Which byte values occur in `literal`, one bit each.
    ///
    /// [`Self::is_skippable`] is asked once per record but its answer depends
    /// only on the literal and the dialect, so the literal's half of the work
    /// is done here instead of being rescanned on every record.
    literal_bytes: ByteSet,
}

impl Predicate {
    /// Match records whose column equals `literal`.
    #[must_use]
    pub fn equals(column: impl Into<Column>, literal: impl Into<Vec<u8>>) -> Self {
        Self::new(column, MatchKind::Equals, literal)
    }

    /// Match records whose column contains `literal`.
    #[must_use]
    pub fn contains(column: impl Into<Column>, literal: impl Into<Vec<u8>>) -> Self {
        Self::new(column, MatchKind::Contains, literal)
    }

    /// Match records whose column starts with `literal`.
    #[must_use]
    pub fn starts_with(column: impl Into<Column>, literal: impl Into<Vec<u8>>) -> Self {
        Self::new(column, MatchKind::StartsWith, literal)
    }

    /// Match records whose column ends with `literal`.
    #[must_use]
    pub fn ends_with(column: impl Into<Column>, literal: impl Into<Vec<u8>>) -> Self {
        Self::new(column, MatchKind::EndsWith, literal)
    }

    fn new(column: impl Into<Column>, kind: MatchKind, literal: impl Into<Vec<u8>>) -> Self {
        let literal = literal.into().into_boxed_slice();
        let mut literal_bytes = ByteSet::EMPTY;
        for &byte in &literal {
            literal_bytes.insert(byte);
        }
        Self {
            column: column.into(),
            kind,
            literal,
            literal_bytes,
        }
    }

    /// The column this predicate applies to.
    #[must_use]
    pub const fn column(&self) -> &Column {
        &self.column
    }

    /// How the field is compared.
    #[must_use]
    pub const fn kind(&self) -> MatchKind {
        self.kind
    }

    /// The literal being matched.
    #[must_use]
    pub const fn literal(&self) -> &[u8] {
        &self.literal
    }

    /// Evaluate this predicate against an already-decoded field value.
    ///
    /// An absent field never matches, except for an `Equals` or `Contains`
    /// test against an empty literal, which an empty field satisfies.
    #[must_use]
    #[inline]
    pub fn matches_field(&self, field: Option<&[u8]>) -> bool {
        let Some(field) = field else {
            return false;
        };
        match self.kind {
            MatchKind::Equals => field == &*self.literal,
            MatchKind::Contains => contains(field, &self.literal),
            MatchKind::StartsWith => field.starts_with(&self.literal),
            MatchKind::EndsWith => field.ends_with(&self.literal),
        }
    }

    /// Whether the literal can be searched for directly in the raw input.
    ///
    /// Returns `false` for an empty literal, which every record trivially
    /// admits, and for literals containing a byte that escaping could split
    /// or synthesize.
    pub(crate) fn is_skippable(&self, structural: &[u8]) -> bool {
        !self.literal.is_empty()
            && !structural
                .iter()
                .chain(b"\r\n")
                .any(|&byte| self.literal_bytes.contains(byte))
    }
}

/// A set of byte values held as a 256-bit mask.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ByteSet([u64; 4]);

impl ByteSet {
    const EMPTY: Self = Self([0; 4]);

    fn insert(&mut self, byte: u8) {
        self.0[usize::from(byte >> 6)] |= 1 << (byte & 63);
    }

    fn contains(self, byte: u8) -> bool {
        self.0[usize::from(byte >> 6)] & (1 << (byte & 63)) != 0
    }
}

/// Whether `haystack` contains `needle`.
///
/// Shares [`find_literal`]'s two-way search rather than a naive `windows`
/// scan, so a decoded field is checked with the same worst-case-linear
/// guarantee that the raw-input pushdown in [`Predicate::is_skippable`]
/// relies on, instead of each carrying its own search with its own
/// worst case.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_literal(needle, haystack).is_some()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::Predicate;

    #[test]
    fn literals_touching_structural_bytes_are_not_skippable() {
        let structural = *b",\"";
        assert!(Predicate::equals(0, "US").is_skippable(&structural));
        // Empty literals match everything, so skipping would be pointless.
        assert!(!Predicate::equals(0, "").is_skippable(&structural));
        // A quote could be written as a doubled escape in the source.
        assert!(!Predicate::equals(0, "a\"b").is_skippable(&structural));
        // A delimiter only appears inside a quoted field.
        assert!(!Predicate::equals(0, "a,b").is_skippable(&structural));
        // Record endings cannot be searched for across records.
        assert!(!Predicate::equals(0, "a\nb").is_skippable(&structural));
    }

    #[test]
    fn high_byte_structural_literals_are_not_skippable() {
        let structural = [b'|', 0x80, 0xFF];

        assert!(!Predicate::equals(0, "a|b").is_skippable(&structural));
        assert!(!Predicate::equals(0, b"a\x80b".to_vec()).is_skippable(&structural));
        assert!(!Predicate::equals(0, b"a\xFFb".to_vec()).is_skippable(&structural));
    }
}

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::borrow::Cow;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::boxed::Box;
use core::error::Error as StdError;
use core::fmt;
use core::result;
use core::str;
#[cfg(any(feature = "std", test))]
use std::borrow::Cow;
#[cfg(feature = "std")]
use std::io;

use super::{ErrorKind, Location};

#[derive(Debug)]
enum ErrorSource {
    None,
    #[cfg(feature = "std")]
    Io(io::Error),
    /// Human-readable detail with no underlying error to point at.
    ///
    /// Carries a `Cow` so a crate-authored explanation costs nothing while a
    /// message assembled at runtime, such as Serde's `custom`, can still own
    /// its text.
    Detail(Cow<'static, str>),
    Custom(Box<dyn StdError + Send + Sync>),
}

/// The error type for every fallible operation in this crate.
///
/// An error says what went wrong through [`kind`](Self::kind) and where
/// through [`location`](Self::location) — byte offset, line, record, and
/// field. When a typed decode fails it also names the target field, through
/// [`field_name`](Self::field_name).
///
/// Match on [`ErrorKind`] when the response depends on the failure; use the
/// [`Display`](core::fmt::Display) form, which already includes the position,
/// when it only needs reporting.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// use coseva::{ErrorKind, SliceParser};
///
/// let mut parser = SliceParser::<Csv>::new(b"city\nBos\"ton\n", ParseOptions::new())?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
/// let result = line.record();
/// let error = match result {
///     Err(error) => error,
///     Ok(_) => {
///         return Err(std::io::Error::other(
///             "expected a quote inside an unquoted field to be rejected",
///         )
///         .into());
///     }
/// };
///
/// assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
/// assert_eq!(error.location().line, 2);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
pub struct Error(Box<ErrorRepr>);

#[derive(Debug)]
struct ErrorRepr {
    kind: ErrorKind,
    location: Location,
    /// Name of the target field, when a typed conversion knows one.
    field_name: Option<Cow<'static, str>>,
    source: ErrorSource,
}

impl Error {
    fn from_parts(kind: ErrorKind, location: Location, source: ErrorSource) -> Self {
        Self(Box::new(ErrorRepr {
            kind,
            location,
            field_name: None,
            source,
        }))
    }

    /// Build an error against a field that no source position is known for.
    ///
    /// Typed decoding raises these from a record, which knows the field
    /// index and the target's name but not where the record sat in the
    /// input. A parser supplies the rest through [`Self::at`].
    #[must_use]
    pub(crate) fn field(kind: ErrorKind, index: usize, name: Option<&'static str>) -> Self {
        let mut error = Self::from_parts(
            kind,
            Location {
                field: index,
                ..Location::UNKNOWN
            },
            ErrorSource::None,
        );
        error.0.field_name = name.map(Cow::Borrowed);
        error
    }

    /// Name the target field this error is about.
    #[must_use]
    pub(crate) fn with_field_name(mut self, name: impl Into<Cow<'static, str>>) -> Self {
        self.0.field_name = Some(name.into());
        self
    }

    pub(crate) fn new(kind: ErrorKind, location: Location) -> Self {
        Self::from_parts(kind, location, ErrorSource::None)
    }

    /// Build an error explained by a message rather than by a nested error.
    #[must_use]
    pub(crate) fn detailed(kind: ErrorKind, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::from_parts(kind, Location::UNKNOWN, ErrorSource::Detail(detail.into()))
    }

    #[cfg(feature = "std")]
    pub(crate) fn io(error: io::Error, location: Location) -> Self {
        Self::from_parts(
            ErrorKind::Io(error.kind()),
            location,
            ErrorSource::Io(error),
        )
    }

    #[cfg(feature = "std")]
    #[inline]
    pub(crate) fn io_at_start(error: io::Error) -> Self {
        Self::io(error, Location::START)
    }

    pub(crate) fn utf8(error: str::Utf8Error, index: usize, mut location: Location) -> Self {
        location.field = index;
        Self::from_parts(ErrorKind::InvalidUtf8(error), location, ErrorSource::None)
    }

    /// Adopt an arbitrary [`crate::FromBytes`] failure at `location`.
    ///
    /// A conversion that reported an [`ErrorKind`] keeps it, which is what
    /// every built-in conversion does. Anything else is a target type's own
    /// error, so it is preserved as the source and categorized as a value
    /// the target rejected.
    pub(crate) fn from_conversion<E>(error: E, mut location: Location, index: usize) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        location.field = index;
        match Box::<dyn StdError + Send + Sync>::from(error).downcast::<ErrorKind>() {
            Ok(kind) => Self::from_parts(*kind, location, ErrorSource::None),
            Err(source) => Self::from_parts(
                ErrorKind::InvalidValue,
                location,
                ErrorSource::Custom(source),
            ),
        }
    }

    /// Adopt a `parse_with` conversion failure from generated decode code.
    ///
    /// This is the constructor `#[derive(CsvDecode)]` generates for a
    /// `#[csv(parse_with = "...")]` field: the custom parser's error is
    /// preserved as a boxed, downcastable source (or unwrapped into an
    /// [`ErrorKind`] when it already is one, which never allocates), and the
    /// target field's index and static name are recorded. It is not part of
    /// the stable API.
    ///
    /// ```
    /// use coseva::Error;
    ///
    /// #[derive(Debug)]
    /// struct BadColor;
    /// impl core::fmt::Display for BadColor {
    ///     fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    ///         f.write_str("not a recognized color")
    ///     }
    /// }
    /// impl core::error::Error for BadColor {}
    ///
    /// let error = Error::from_field_conversion(BadColor, 2, "color");
    /// assert_eq!(error.field_name(), Some("color"));
    /// assert_eq!(error.location().field, 2);
    /// ```
    #[doc(hidden)]
    #[must_use]
    pub fn from_field_conversion<E>(error: E, index: usize, name: &'static str) -> Self
    where
        E: StdError + Send + Sync + 'static,
    {
        Self::from_conversion(error, Location::UNKNOWN, index).with_field_name(name)
    }

    /// Record the field index this error refers to, leaving the rest of the
    /// location unknown.
    ///
    /// Serde reports failures against a positional field before any parser has
    /// supplied the record's real byte and line position; [`Self::at`] fills
    /// those in later while preserving this index.
    #[cfg(feature = "serde")]
    #[must_use]
    pub(crate) fn with_field_index(mut self, index: usize) -> Self {
        self.0.location.field = index;
        self
    }

    /// Give a field-only error the position of the record carrying it.
    ///
    /// The field index the conversion reported is kept; everything else
    /// comes from `location`.
    pub(crate) fn at(mut self, location: Location) -> Self {
        let field = self.0.location.field;
        self.0.location = Location { field, ..location };
        self
    }

    pub(crate) fn relocate(&mut self, byte_offset: usize, line: u64, record: u64) {
        let location = &mut self.0.location;
        location.byte = location.byte.saturating_add(byte_offset);
        location.line = location.line.saturating_add(line.saturating_sub(1));
        location.record = record;
    }

    pub(crate) fn rebase_stream_window(&mut self, byte_offset: usize) {
        let location = &mut self.0.location;
        location.byte = location.byte.saturating_add(byte_offset);
    }

    /// Error category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// Error location.
    #[must_use]
    pub const fn location(&self) -> Location {
        self.0.location
    }

    /// Typed target field name, when available.
    #[must_use]
    pub fn field_name(&self) -> Option<&str> {
        match &self.0.field_name {
            Some(name) => Some(name.as_ref()),
            None => None,
        }
    }

    /// Recover the underlying I/O error, if this was an I/O failure.
    #[must_use]
    #[cfg(feature = "std")]
    pub fn into_io_error(self) -> Option<io::Error> {
        match self.0.source {
            ErrorSource::Io(error) => Some(error),
            ErrorSource::None | ErrorSource::Custom(_) | ErrorSource::Detail(_) => None,
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Flattened, so boxing the payload does not show up in output.
        f.debug_struct("Error")
            .field("kind", &self.0.kind)
            .field("location", &self.0.location)
            .field("source", &self.0.source)
            .finish()
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        self.0.kind == other.0.kind && self.0.location == other.0.location
    }
}

impl Eq for Error {}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.location.is_known() {
            write!(f, "CSV error at {}: ", self.0.location)?;
        }
        if let Some(name) = &self.0.field_name {
            write!(f, "field {name}: ")?;
        }
        match &self.0.source {
            #[cfg(feature = "std")]
            ErrorSource::Io(error) => return write!(f, "{error}"),
            ErrorSource::Detail(detail) => return f.write_str(detail),
            ErrorSource::Custom(error) => return write!(f, "{error}"),
            ErrorSource::None => {}
        }
        write!(f, "{}", self.0.kind)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &self.0.source {
            ErrorSource::None => None,
            #[cfg(feature = "std")]
            ErrorSource::Io(error) => Some(error),
            ErrorSource::Detail(_) => None,
            ErrorSource::Custom(error) => Some(error.as_ref()),
        }
    }
}

/// Result type used by CSV operations.
/// Convenience alias for a [`result::Result`] whose default error is [`Error`].
///
/// For a worked example, see [`Error`].
pub type Result<T, E = Error> = result::Result<T, E>;

#[cfg(feature = "serde")]
impl serde::de::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::detailed(ErrorKind::Serde, msg.to_string())
    }

    fn missing_field(field: &'static str) -> Self {
        Self::detailed(ErrorKind::Serde, "missing record field").with_field_name(field)
    }

    fn unknown_field(field: &str, _expected: &'static [&'static str]) -> Self {
        Self::detailed(ErrorKind::Serde, format!("unknown field `{field}`"))
    }
}

#[cfg(feature = "serde")]
impl serde::ser::Error for Error {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        Self::detailed(ErrorKind::Serde, msg.to_string())
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_methods() {
        let err = Error::from_conversion(ErrorKind::InvalidValue, Location::UNKNOWN, 1);
        assert_eq!(err.kind(), ErrorKind::InvalidValue);
        assert_eq!(err.location().field, 1);

        let err_field = Error::field(ErrorKind::InvalidValue, 2, Some("f1"));
        assert_eq!(err_field.field_name(), Some("f1"));
        let err_field_none = Error::field(ErrorKind::InvalidValue, 2, None);
        assert_eq!(err_field_none.field_name(), None);

        #[cfg(feature = "std")]
        {
            let io_err = Error::io_at_start(io::Error::new(io::ErrorKind::NotFound, "not found"));
            assert_eq!(io_err.location(), Location::START);
            assert!(format!("{io_err}").contains("not found"));
            assert!(io_err.into_io_error().is_some());
            let io_err2 = Error::io(
                io::Error::new(io::ErrorKind::NotFound, "not found"),
                Location::UNKNOWN,
            );
            assert!(std::error::Error::source(&io_err2).is_some());
        }

        let named =
            Error::new(ErrorKind::InvalidValue, Location::UNKNOWN).with_field_name("my_field");
        assert_eq!(named.field_name(), Some("my_field"));
        let named_str = Error::new(ErrorKind::InvalidValue, Location::UNKNOWN)
            .with_field_name(String::from("owned_field"));
        assert_eq!(named_str.field_name(), Some("owned_field"));

        // Error fmt and equality
        let err2 = Error::new(ErrorKind::InvalidValue, Location::UNKNOWN);
        let err3 = Error::new(ErrorKind::InvalidValue, Location::UNKNOWN);
        assert_eq!(err2, err3);
        assert!(std::error::Error::source(&err2).is_none());
        let _ = format!("{err2:?}");
        let _ = format!("{err2}");
        let _ = format!("{named}");

        // Error source
        let custom_err =
            Error::from_conversion(std::io::Error::other("custom"), Location::UNKNOWN, 0);
        assert!(std::error::Error::source(&custom_err).is_some());
        assert!(format!("{custom_err}").contains("custom"));
        let detailed_err = Error::detailed(ErrorKind::Serde, "detailed error");
        assert!(std::error::Error::source(&detailed_err).is_none());
        assert!(format!("{detailed_err}").contains("detailed error"));
        #[cfg(feature = "std")]
        assert!(detailed_err.into_io_error().is_none());

        // from_field_conversion
        let field_conv_err =
            Error::from_field_conversion(std::io::Error::other("custom"), 2, "my_field");
        assert_eq!(field_conv_err.field_name(), Some("my_field"));

        // utf8 constructor
        let invalid_bytes = vec![0xff];
        let utf8_err = core::str::from_utf8(&invalid_bytes).unwrap_err();
        let err_utf8 = Error::utf8(utf8_err, 1, Location::UNKNOWN);
        assert_eq!(err_utf8.location().field, 1);

        // with_field_index and relocate
        #[cfg(feature = "serde")]
        {
            let indexed = Error::new(ErrorKind::Serde, Location::UNKNOWN).with_field_index(3);
            assert_eq!(indexed.location().field, 3);
            use serde::de::Error as DeError;
            use serde::ser::Error as SerError;
            let missing = Error::missing_field("foo");
            assert_eq!(missing.field_name(), Some("foo"));
            let unknown = Error::unknown_field("bar", &["foo"]);
            assert!(format!("{unknown}").contains("unknown field `bar`"));
            let de_custom_args = <Error as DeError>::custom(format_args!("de err {}", 123));
            assert!(format!("{de_custom_args}").contains("de err 123"));
            let de_custom_str = <Error as DeError>::custom("de err str");
            assert!(format!("{de_custom_str}").contains("de err str"));
            let ser_custom = <Error as SerError>::custom("ser err");
            assert!(format!("{ser_custom}").contains("ser err"));
        }

        let mut relocated = Error::new(
            ErrorKind::UnexpectedQuote,
            Location {
                byte: 5,
                line: 2,
                record: 1,
                field: 0,
            },
        );
        relocated.relocate(10, 3, 2);
        assert_eq!(relocated.location().byte, 15);
        assert_eq!(relocated.location().line, 4);
        assert_eq!(relocated.location().record, 2);

        let relocated_at = relocated.at(Location {
            byte: 100,
            line: 10,
            record: 5,
            field: 0,
        });
        assert_eq!(relocated_at.location().byte, 100);
    }

    #[test]
    fn debug_labels_and_equality_components_are_exact() {
        let error = Error::new(ErrorKind::InvalidValue, Location::UNKNOWN);
        let rendered = format!("{error:?}");
        assert!(rendered.starts_with("Error {"), "{rendered}");
        assert!(rendered.contains("kind: InvalidValue"), "{rendered}");
        assert!(rendered.contains("location: Location"), "{rendered}");
        assert!(rendered.contains("source: None"), "{rendered}");

        let different_kind = Error::new(ErrorKind::EmptyField, Location::UNKNOWN);
        assert_ne!(error, different_kind);
        let different_location = Error::new(
            ErrorKind::InvalidValue,
            Location {
                byte: 1,
                ..Location::UNKNOWN
            },
        );
        assert_ne!(error, different_location);

        let detailed = Error::detailed(ErrorKind::InvalidValue, "detail");
        assert_eq!(error, detailed);
    }
}

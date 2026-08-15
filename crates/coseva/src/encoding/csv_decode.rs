use core::marker::PhantomData;

use crate::error::Error;

use super::{DecodeRecord, FusedFields};

static NO_FIELD_ALIASES_STORAGE: [&[&str]; 1] = [&[]];

/// Read one CSV record into your own type.
///
/// Derive it with `#[derive(CsvDecode)]` (feature `derive`) and columns are
/// matched to fields by name when the document has headers, and positionally
/// when it does not. See the [module docs](super) for the `#[csv(...)]`
/// attributes that control that mapping.
///
/// The `'record` lifetime lets a decoded type borrow its fields straight out
/// of the input rather than copying them, which is the cheapest way to decode.
/// A type that borrows can only be used with [`Line::decoded`], since it
/// cannot outlive the record; use [`CsvDecodeOwned`] as the bound — and an
/// owning type such as `String` — when it must, as the
/// [`decoded_records`] iterator requires.
///
/// ```
/// use coseva::format::Csv;
/// use coseva::config::ParseOptions;
/// # #[cfg(feature = "derive")] {
/// use coseva::SliceParser;
/// use coseva::encoding::CsvDecode;
///
/// // Borrowing: `name` points into the input, nothing is copied.
/// #[derive(CsvDecode)]
/// struct CityRef<'row> {
///     name: &'row str,
///     population: u64,
/// }
///
/// // Owning: can be collected, sent across threads, or kept.
/// #[derive(CsvDecode)]
/// struct City {
///     name: String,
///     population: u64,
/// }
///
/// let input = b"name,population\nBoston,650706\n";
///
/// let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new())?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected one record"))?;
/// let borrowed: CityRef<'_> = line.decoded()?;
/// assert_eq!(borrowed.name, "Boston");
///
/// let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new())?;
/// let owned: Vec<City> = parser.decoded_records::<City>().collect::<Result<_, _>>()?;
/// assert_eq!(owned[0].population, 650_706);
/// # }
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// [`Line::decoded`]: crate::Line::decoded
/// [`decoded_records`]: crate::SliceParser::decoded_records
pub trait CsvDecode<'record>: Sized {
    /// Decode `self` from the next record.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when a field is missing, is not valid UTF-8,
    /// or cannot be converted to the target type.
    fn csv_decode<R>(record: &R) -> Result<Self, Error>
    where
        R: DecodeRecord<'record> + ?Sized;

    /// Decode the next record into an existing value, reusing its allocations.
    ///
    /// The default implementation is equivalent to
    /// `*self = Self::csv_decode(record)?`, so hand-written implementations
    /// need not override it. `#[derive(CsvDecode)]` generates a field-wise
    /// override that decodes each field in place, which lets heap-bearing
    /// field types such as [`String`] and [`Vec<u8>`] reuse their buffers
    /// instead of allocating a fresh one per record.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] under the same conditions as
    /// [`Self::csv_decode`]. On failure `self` is left in an unspecified but
    /// valid state: the field-wise override decodes in declaration order and
    /// does not roll back fields that were already updated.
    fn csv_decode_into<R>(&mut self, record: &R) -> Result<(), Error>
    where
        R: DecodeRecord<'record> + ?Sized,
    {
        *self = Self::csv_decode(record)?;
        Ok(())
    }

    /// Static CSV field names in the same order as the decoded fields.
    ///
    /// The parser layer uses these names to resolve a header record to a
    /// positional permutation before the first data record.
    fn field_names() -> &'static [&'static str];

    /// Alternate header spellings accepted for each field, parallel to
    /// [`field_names`](Self::field_names).
    ///
    /// An empty slice means no field has alternates, which is the common case
    /// and lets header resolution skip alias matching entirely. Otherwise the
    /// outer slice has one entry per field, each holding that field's
    /// alternates.
    #[must_use]
    fn field_aliases() -> &'static [&'static [&'static str]] {
        &NO_FIELD_ALIASES_STORAGE[..0]
    }

    /// Number of CSV fields [`Self::fused_decode`] consumes, when this type
    /// supports fused decoding.
    ///
    /// `None` — the default — opts out, and the parser then always takes the
    /// general path. `#[derive(CsvDecode)]` sets it to the count of
    /// non-skipped fields.
    ///
    /// Hidden because it is an implementation detail shared between the derive
    /// and the parser: [`FusedFields`] cannot be constructed outside this
    /// crate, so there is nothing a hand-written implementation can usefully do
    /// with it. Leaving the default is always correct.
    #[doc(hidden)]
    const FUSED_ARITY: Option<usize> = None;

    /// Decode from a record whose columns are already in declaration order.
    ///
    /// The parser calls this instead of [`Self::csv_decode`] once it has
    /// established that no header permutation is needed, which lets it skip
    /// per-record mapping resolution entirely. Derived implementations expand
    /// to straight-line per-field conversions with the field count fixed at
    /// compile time.
    ///
    /// The default implementation forwards to [`Self::csv_decode`], so
    /// hand-written implementations need not override it and behave
    /// identically either way.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] under the same conditions as
    /// [`Self::csv_decode`].
    #[doc(hidden)]
    fn fused_decode(fields: &FusedFields<'record>) -> Result<Self, Error> {
        Self::csv_decode(fields)
    }

    /// Fused counterpart to [`Self::csv_decode_into`].
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] under the same conditions as
    /// [`Self::csv_decode_into`].
    #[doc(hidden)]
    fn fused_decode_into(&mut self, fields: &FusedFields<'record>) -> Result<(), Error> {
        self.csv_decode_into(fields)
    }
}

// ── CsvDecodeOwned ─────────────────────────────────────────────────────────────

/// A [`CsvDecode`] type that does not borrow from the record it came from.
///
/// Use this as the bound when a decoded value must outlive the record — to
/// collect values into a `Vec`, send them elsewhere, or keep them across
/// further reads. It is what [`decoded_records`](crate::SliceParser::decoded_records)
/// requires.
///
/// Every type is covered automatically: any `T` satisfying
/// `for<'record> CsvDecode<'record>` implements this, so a derived type whose
/// fields all own their data (`String` rather than `&str`) qualifies with no
/// extra work. It exists only to spare you writing the higher-ranked bound.
/// For a worked example, see [`CsvDecode`].
pub trait CsvDecodeOwned: for<'record> CsvDecode<'record> {}

impl<T: for<'record> CsvDecode<'record>> CsvDecodeOwned for T {}

// ── DecodeSink ─────────────────────────────────────────────────────────────────

/// Where a decoded record is deposited.
///
/// This lets the parsers share one record-production path between
/// `decoded`, which yields a fresh value, and `decode_into`, which
/// overwrites a caller-owned value in place.
pub(crate) trait DecodeSink<'record> {
    /// What the parser returns once the record has been absorbed.
    type Output;

    /// Decode `record` into this sink.
    fn absorb<R>(self, record: &R) -> Result<Self::Output, Error>
    where
        R: DecodeRecord<'record> + ?Sized;
}

/// A sink that constructs a fresh `T` per record.
pub(crate) struct DecodeNew<T>(PhantomData<fn() -> T>);

impl<T> DecodeNew<T> {
    pub(crate) const fn new() -> Self {
        Self(PhantomData)
    }
}

impl<'record, T> DecodeSink<'record> for DecodeNew<T>
where
    T: CsvDecode<'record>,
{
    type Output = T;

    #[inline]
    fn absorb<R>(self, record: &R) -> Result<T, Error>
    where
        R: DecodeRecord<'record> + ?Sized,
    {
        T::csv_decode(record)
    }
}

/// A sink that overwrites an existing `T` in place.
impl<'record, T> DecodeSink<'record> for &mut T
where
    T: CsvDecode<'record>,
{
    type Output = ();

    #[inline]
    fn absorb<R>(self, record: &R) -> Result<(), Error>
    where
        R: DecodeRecord<'record> + ?Sized,
    {
        self.csv_decode_into(record)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{CsvDecode, NO_FIELD_ALIASES_STORAGE};
    use crate::encoding::decode_record::{DecodeRecord, FusedFields};
    use crate::error::Error;
    use crate::span::{Source, Span, SpanSet};

    /// A hand-written implementation, which leaves `FUSED_ARITY` at `None`
    /// and so never overrides the fused forwarders.
    #[derive(Debug, Default, PartialEq)]
    struct Handwritten {
        city: String,
    }

    impl<'record> CsvDecode<'record> for Handwritten {
        fn csv_decode<R>(record: &R) -> Result<Self, Error>
        where
            R: DecodeRecord<'record> + ?Sized,
        {
            let city = record.get_field(0).unwrap_or_default();
            Ok(Self {
                city: String::from_utf8_lossy(city).into_owned(),
            })
        }

        fn field_names() -> &'static [&'static str] {
            &["city"]
        }
    }

    /// The parser only builds a `FusedFields` for types that opt into fusion,
    /// so the defaults are exercised here rather than through a parse.
    #[test]
    fn the_default_fused_forwarders_match_the_general_path() {
        let spans = SpanSet::from([Span::new(Source::Input, 0..6, false).expect("in range")]);
        let fields = FusedFields::new(spans.resolved(b"Boston", b""), false);

        let decoded = Handwritten::fused_decode(&fields).expect("decodes");
        assert_eq!(decoded.city, "Boston");

        let mut reused = Handwritten::default();
        reused.fused_decode_into(&fields).expect("decodes");
        assert_eq!(reused, decoded);
    }

    #[test]
    fn the_default_alias_table_is_empty() {
        let aliases = Handwritten::field_aliases();
        assert!(aliases.is_empty());
        assert_eq!(aliases.as_ptr(), NO_FIELD_ALIASES_STORAGE.as_ptr());
    }
}

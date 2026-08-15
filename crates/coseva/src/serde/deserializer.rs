use std::sync::atomic::AtomicU64;
use std::{fmt, str};

use serde::de::{self, IntoDeserializer as _};

use super::struct_cache::{StructCache, mask_contains, wide_contains};
use super::{parse_error, utf8_error};
use crate::error::{Error, ErrorKind};
use crate::field_ends::EndpointNullFlags;
use crate::from_bytes::{
    parse_i8, parse_i16, parse_i32, parse_i64, parse_i128, parse_u8, parse_u16, parse_u32,
    parse_u64, parse_u128,
};
use crate::record::SpanNullFlags;

/// Explicit-NULL flag source paired with a field-byte iterator.
///
/// `Disabled` matches ordinary CSV, where no field is ever treated as an
/// explicit NULL. `Endpoints`/`Spans` supply real flags sourced
/// from a NULL-aware [`ByteRecord`]/[`Record`], allowing the deserializer to
/// distinguish an explicit NULL from a present-but-empty field.
pub(super) enum FieldNulls<'de> {
    Disabled,
    Endpoints(EndpointNullFlags<'de>),
    Spans(SpanNullFlags<'de>),
}

impl FieldNulls<'_> {
    fn next(&mut self) -> bool {
        match self {
            Self::Disabled => false,
            Self::Endpoints(flags) => flags.next().unwrap_or_default(),
            Self::Spans(flags) => flags.next().unwrap_or_default(),
        }
    }

    /// Whether the next field is an explicit NULL, without consuming it.
    ///
    /// Both flag iterators are cheap `slice::Iter` wrappers, so cloning one to
    /// look ahead costs nothing and keeps the real iterator untouched.
    fn peek(&self) -> bool {
        match self {
            Self::Disabled => false,
            Self::Endpoints(flags) => flags.clone().next().unwrap_or_default(),
            Self::Spans(flags) => flags.clone().next().unwrap_or_default(),
        }
    }
}

/// Field-byte iterator paired with its explicit-NULL flags.
///
/// Yields `(bytes, is_null)` so downstream field deserializers can preserve
/// the distinction between an explicit NULL and a present, empty field
/// without allocating.
struct NullableFields<'de, F> {
    fields: F,
    nulls: FieldNulls<'de>,
}

impl<'de, F> Iterator for NullableFields<'de, F>
where
    F: Iterator<Item = &'de [u8]>,
{
    type Item = (&'de [u8], bool);

    fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.fields.next()?;
        Some((bytes, self.nulls.next()))
    }

    // Pairing each field with its NULL flag does not change how many items
    // remain, so the inner iterator's hint carries over unchanged. Serde reaches
    // this through `ExactSizeIterator::len`, whose default implementation
    // asserts the hint is exact and panics otherwise.
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.fields.size_hint()
    }
}

impl<'de, F> ExactSizeIterator for NullableFields<'de, F> where
    F: ExactSizeIterator<Item = &'de [u8]>
{
}

/// How the record's fields line up with the CSV header columns.
#[derive(Clone, Copy)]
pub(crate) enum HeaderView<'de> {
    /// The record holds every CSV column.
    Full(&'de StructCache),
}

/// Record-level Serde deserializer.
pub(super) struct CsvDeserializer<'de, F> {
    fields: NullableFields<'de, F>,
    headers: Option<HeaderView<'de>>,
}

impl<'de, F> CsvDeserializer<'de, F> {
    /// Create a record-level deserializer with explicit per-field NULL flags.
    pub(super) const fn new_null_aware(
        fields: F,
        nulls: FieldNulls<'de>,
        headers: Option<HeaderView<'de>>,
    ) -> Self {
        Self {
            fields: NullableFields { fields, nulls },
            headers,
        }
    }

    fn first_field(mut self) -> FieldDeserializer<'de>
    where
        F: Iterator<Item = &'de [u8]>,
    {
        match self.fields.next() {
            Some((bytes, null)) => FieldDeserializer::present(bytes, null),
            None => FieldDeserializer::missing(),
        }
    }
}

/// Forward a method that only takes `visitor` to the first field.
macro_rules! forward_scalar_to_first {
    ($($method:ident)*) => {
        $(
            fn $method<V: de::Visitor<'de>>(self, visitor: V)
                -> Result<V::Value, Error>
            {
                self.first_field().$method(visitor)
            }
        )*
    };
}

impl<'de, F> de::Deserializer<'de> for CsvDeserializer<'de, F>
where
    F: ExactSizeIterator<Item = &'de [u8]>,
{
    type Error = Error;

    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_seq(SeqDeserializer::new(self.fields))
    }

    forward_scalar_to_first!(
        deserialize_bool
        deserialize_i8
        deserialize_i16
        deserialize_i32
        deserialize_i64
        deserialize_i128
        deserialize_u8
        deserialize_u16
        deserialize_u32
        deserialize_u64
        deserialize_u128
        deserialize_f32
        deserialize_f64
        deserialize_char
        deserialize_str
        deserialize_string
        deserialize_bytes
        deserialize_byte_buf
        deserialize_identifier
    );

    /// A record with no fields deserializes as `None`.
    ///
    /// A single-field record *is* the value being described, so an explicit
    /// NULL in that field makes the whole record `None`, matching the
    /// field-level rule. Wider records are always `Some`, because the value
    /// spans them and no single field decides its presence.
    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let len = self.fields.len();
        if len == 0 || (len == 1 && self.fields.nulls.peek()) {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_unit<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_seq(SeqDeserializer::new(self.fields))
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_seq(SeqDeserializer::new(self.fields))
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_seq(SeqDeserializer::new(self.fields))
    }

    fn deserialize_map<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.headers {
            // A flattened struct consumes unknown keys itself, so no column is
            // ever reported as ignored and none may be skipped.
            Some(HeaderView::Full(cache)) => {
                visitor.visit_map(MapDeserializer::full(cache, self.fields, false, false))
            }
            None => Err(Error::detailed(
                ErrorKind::Serde,
                "cannot deserialize a map from a CSV record without headers; \
                 provide headers via ParseOptions or IoParser::deserialized",
            )),
        }
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        match self.headers {
            Some(HeaderView::Full(cache)) => {
                let skipping = cache.begin_struct(name, fields);
                visitor.visit_map(MapDeserializer::full(
                    cache,
                    self.fields,
                    skipping,
                    !skipping,
                ))
            }
            None => visitor.visit_seq(SeqDeserializer::new(self.fields)),
        }
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        self.first_field().deserialize_enum(name, variants, visitor)
    }

    fn deserialize_ignored_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }
}

// ── FieldDeserializer ─────────────────────────────────────────────────────────

/// Field-level Serde deserializer for a single raw byte slice.
struct FieldDeserializer<'de> {
    bytes: &'de [u8],
    present: bool,
    /// Whether the source record marked this field as an explicit NULL.
    ///
    /// Always `false` for non-NULL-aware sources, where an empty field is
    /// present data, not `None`.
    null: bool,
    /// Where to report that the visitor discarded this column.
    observer: Option<(&'de StructCache, usize)>,
}

impl<'de> FieldDeserializer<'de> {
    const fn present(bytes: &'de [u8], null: bool) -> Self {
        Self {
            bytes,
            present: true,
            null,
            observer: None,
        }
    }

    const fn missing() -> Self {
        Self {
            bytes: b"",
            present: false,
            null: false,
            observer: None,
        }
    }

    /// Attach the cache slot that records whether this column was discarded.
    const fn observed(mut self, observer: Option<(&'de StructCache, usize)>) -> Self {
        self.observer = observer;
        self
    }

    fn require(self) -> Result<&'de [u8], Error> {
        if self.present {
            Ok(self.bytes)
        } else {
            Err(Error::detailed(
                ErrorKind::Serde,
                "required CSV field is absent",
            ))
        }
    }
}

/// Parse a numeric type from the field's UTF-8 bytes.
///
/// Used for floats, whose parsers require a `&str`, and as the cold fallback
/// for integers so that error messages stay byte-for-byte identical to the
/// UTF-8 path.
#[cold]
#[inline(never)]
fn parse_numeric_slow<T>(bytes: &[u8], ty: &'static str) -> Result<T, Error>
where
    T: str::FromStr,
    T::Err: fmt::Display,
{
    let s = str::from_utf8(bytes).map_err(utf8_error)?;
    s.parse().map_err(|e| parse_error(s, ty, e))
}

/// Parse an integer straight from the field's raw bytes.
///
/// Digits are consumed directly, so no UTF-8 validation is performed on the
/// happy path. Any rejection falls back to [`parse_numeric_slow`], which
/// reproduces the original UTF-8-based error message.
macro_rules! impl_field_integer {
    ($method:ident, $visit:ident, $ty:ty, $parse:ident) => {
        fn $method<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            let bytes = self.require()?;
            let n: $ty = match $parse(bytes) {
                Ok(value) => value,
                Err(_error) => parse_numeric_slow(bytes, stringify!($ty))?,
            };
            visitor.$visit(n)
        }
    };
}

/// Parse a float straight from the field's raw bytes.
///
/// Bytes are consumed directly by the Eisel-Lemire algorithm, so no UTF-8
/// validation happens on the happy path. Any rejection falls back to
/// [`parse_numeric_slow`], which reproduces the original UTF-8-based error
/// message.
macro_rules! impl_field_float {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            let bytes = self.require()?;
            let n: $ty = match fast_float2::parse(bytes) {
                Ok(value) => value,
                Err(_error) => parse_numeric_slow(bytes, stringify!($ty))?,
            };
            visitor.$visit(n)
        }
    };
}

impl<'de> de::Deserializer<'de> for FieldDeserializer<'de> {
    type Error = Error;

    /// Returns raw borrowed `&str` for valid UTF-8, or `&[u8]` otherwise.
    ///
    /// No type inference is performed; the value is always returned as-is.
    fn deserialize_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let bytes = self.require()?;
        match str::from_utf8(bytes) {
            Ok(s) => visitor.visit_borrowed_str(s),
            Err(_) => visitor.visit_borrowed_bytes(bytes),
        }
    }

    fn deserialize_bool<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.require()? {
            b"true" | b"1" => visitor.visit_bool(true),
            b"false" | b"0" => visitor.visit_bool(false),
            other => Err(Error::detailed(
                ErrorKind::Serde,
                format!(
                    "expected \"true\", \"false\", \"1\", or \"0\"; got {:?}",
                    String::from_utf8_lossy(other)
                ),
            )),
        }
    }

    impl_field_integer!(deserialize_i8, visit_i8, i8, parse_i8);
    impl_field_integer!(deserialize_i16, visit_i16, i16, parse_i16);
    impl_field_integer!(deserialize_i32, visit_i32, i32, parse_i32);
    impl_field_integer!(deserialize_i64, visit_i64, i64, parse_i64);
    impl_field_integer!(deserialize_i128, visit_i128, i128, parse_i128);
    impl_field_integer!(deserialize_u8, visit_u8, u8, parse_u8);
    impl_field_integer!(deserialize_u16, visit_u16, u16, parse_u16);
    impl_field_integer!(deserialize_u32, visit_u32, u32, parse_u32);
    impl_field_integer!(deserialize_u64, visit_u64, u64, parse_u64);
    impl_field_integer!(deserialize_u128, visit_u128, u128, parse_u128);
    impl_field_float!(deserialize_f32, visit_f32, f32);
    impl_field_float!(deserialize_f64, visit_f64, f64);

    fn deserialize_char<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let s = str::from_utf8(self.require()?).map_err(utf8_error)?;
        let mut chars = s.chars();
        let c = chars.next().ok_or_else(|| {
            Error::detailed(
                ErrorKind::Serde,
                "expected a single character, got an empty field",
            )
        })?;
        if chars.next().is_some() {
            return Err(Error::detailed(
                ErrorKind::Serde,
                format!("expected a single character, got {s:?}"),
            ));
        }
        visitor.visit_char(c)
    }

    fn deserialize_str<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let s = str::from_utf8(self.require()?).map_err(utf8_error)?;
        visitor.visit_borrowed_str(s)
    }

    fn deserialize_string<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let s = str::from_utf8(self.require()?).map_err(utf8_error)?;
        visitor.visit_str(s)
    }

    fn deserialize_bytes<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_borrowed_bytes(self.require()?)
    }

    fn deserialize_byte_buf<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_bytes(self.require()?)
    }

    fn deserialize_option<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.present && !self.null {
            visitor.visit_some(self)
        } else {
            visitor.visit_none()
        }
    }

    fn deserialize_unit<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: de::Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "sequences are not supported as values within a single CSV field; \
             use a top-level sequence for multiple fields",
        ))
    }

    fn deserialize_tuple<V: de::Visitor<'de>>(
        self,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuples are not supported as values within a single CSV field",
        ))
    }

    fn deserialize_tuple_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        _visitor: V,
    ) -> Result<V::Value, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuple structs are not supported as values within a single CSV field",
        ))
    }

    fn deserialize_map<V: de::Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "maps are not supported as values within a single CSV field",
        ))
    }

    fn deserialize_struct<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Error> {
        Err(Error::detailed(
            ErrorKind::Serde,
            "structs are not supported as values within a single CSV field",
        ))
    }

    fn deserialize_enum<V: de::Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Error> {
        visitor.visit_enum(UnitEnumAccess {
            bytes: self.require()?,
        })
    }

    fn deserialize_identifier<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.deserialize_str(visitor)
    }

    /// Serde calls this for a column the visitor does not want, which is the
    /// signal used to learn which columns may be skipped on later records.
    fn deserialize_ignored_any<V: de::Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if let Some((cache, column)) = self.observer {
            cache.note_ignored(column);
        }
        visitor.visit_unit()
    }
}

// ── SeqDeserializer ───────────────────────────────────────────────────────────

pub(super) struct SeqDeserializer<F> {
    fields: F,
    index: usize,
}

impl<F> SeqDeserializer<F> {
    pub(super) const fn new(fields: F) -> Self {
        Self { fields, index: 0 }
    }
}

impl<'de, F> de::SeqAccess<'de> for SeqDeserializer<F>
where
    F: ExactSizeIterator<Item = (&'de [u8], bool)>,
{
    type Error = Error;

    fn next_element_seed<S>(&mut self, seed: S) -> Result<Option<S::Value>, Error>
    where
        S: de::DeserializeSeed<'de>,
    {
        match self.fields.next() {
            None => Ok(None),
            Some((bytes, null)) => {
                let index = self.index;
                self.index += 1;
                seed.deserialize(FieldDeserializer::present(bytes, null))
                    .map(Some)
                    .map_err(|error| error.with_field_index(index))
            }
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.fields.len())
    }
}

// ── MapDeserializer ───────────────────────────────────────────────────────────

pub(super) struct MapDeserializer<'de, F> {
    cache: &'de StructCache,
    fields: F,
    /// Original CSV column index of the pair currently being yielded.
    column: usize,
    /// Whether columns the visitor previously ignored are skipped outright.
    skipping: bool,
    /// The learned ignored-column set for the first 64 columns, snapshotted once
    /// for this record so the per-column test stays a register bit test.
    ignored: u64,
    /// The learned ignored columns at or beyond 64, borrowed from the cache.
    ///
    /// Empty for an ordinary header, so a narrow record never touches it and
    /// the fast path is exactly what it was before. A wide header reads these
    /// atomic words per skipped column, which is stable within a record because
    /// the learned set is only promoted between records.
    wide_ignored: &'de [AtomicU64],
    /// Whether ignored columns are being recorded for later skipping.
    observing: bool,
    current_header: Option<&'de str>,
}

impl<'de, F> MapDeserializer<'de, F> {
    pub(super) fn full(
        cache: &'de StructCache,
        fields: F,
        skipping: bool,
        observing: bool,
    ) -> Self {
        Self {
            ignored: cache.ignored_mask(),
            wide_ignored: cache.wide_ignored(),
            cache,
            fields,
            column: 0,
            skipping,
            observing,
            current_header: None,
        }
    }
}

impl<'de, F> de::MapAccess<'de> for MapDeserializer<'de, F>
where
    F: ExactSizeIterator<Item = (&'de [u8], bool)>,
{
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Error>
    where
        K: de::DeserializeSeed<'de>,
    {
        // Discard whole columns the visitor is known to ignore, consuming their
        // field so keys and values stay in lockstep. Columns past the first 64
        // are consulted only when a wide header actually allocated their words.
        // gamma::skip(cond.negate, reason = "mutation causes non-termination or unbounded resource use")
        while self.skipping
            && self.column < self.cache.columns
            && (mask_contains(self.ignored, self.column)
                || wide_contains(self.wide_ignored, self.column))
        {
            self.fields.next();
            // gamma::skip(stmt.delete_assign, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            self.column += 1;
        }

        if self.column >= self.cache.columns {
            let trailing = self.fields.len();
            if trailing != 0 {
                return Err(Error::field(
                    ErrorKind::FieldCountMismatch {
                        expected: self.cache.columns,
                        actual: self.cache.columns + trailing,
                    },
                    self.cache.columns,
                    None,
                ));
            }
            return Ok(None);
        }

        // Report a non-UTF-8 header at the column that reaches it, so the
        // failure surfaces per record rather than when the cache was filled.
        if let Some((column, error)) = self.cache.invalid
            && self.column >= column
        {
            return Err(Error::detailed(
                ErrorKind::Serde,
                format!("CSV header is not valid UTF-8: {error}"),
            ));
        }

        let key: &'de str = &self.cache.names[self.column];
        self.current_header = Some(key);
        seed.deserialize(key.into_deserializer()).map(Some)
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let observer = self.observing.then_some((self.cache, self.column));
        let field = match self.fields.next() {
            Some((bytes, null)) => FieldDeserializer::present(bytes, null).observed(observer),
            None => FieldDeserializer::missing().observed(observer),
        };
        let index = self.column;
        let header = self.current_header.take();
        self.column += 1;
        seed.deserialize(field).map_err(|error| {
            let error = error.with_field_index(index);
            match header {
                Some(name) => error.with_field_name(name.to_string()),
                None => error,
            }
        })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.cache.columns - self.column)
    }
}

// ── UnitEnumAccess / UnitVariantAccess ────────────────────────────────────────

/// Enum access that only supports unit variants (matched by name).
struct UnitEnumAccess<'de> {
    bytes: &'de [u8],
}

impl<'de> de::EnumAccess<'de> for UnitEnumAccess<'de> {
    type Error = Error;
    type Variant = UnitVariantAccess;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant), Error>
    where
        V: de::DeserializeSeed<'de>,
    {
        let s = str::from_utf8(self.bytes).map_err(utf8_error)?;
        let val = seed.deserialize(s.into_deserializer())?;
        Ok((val, UnitVariantAccess))
    }
}

/// Variant access that only permits unit variants.
struct UnitVariantAccess;

impl<'de> de::VariantAccess<'de> for UnitVariantAccess {
    type Error = Error;

    fn unit_variant(self) -> Result<(), Error> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, _seed: T) -> Result<T::Value, Error>
    where
        T: de::DeserializeSeed<'de>,
    {
        Err(Error::detailed(
            ErrorKind::Serde,
            "newtype enum variants are not supported in CSV; only unit variants are allowed",
        ))
    }

    fn tuple_variant<V>(self, _len: usize, _visitor: V) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::detailed(
            ErrorKind::Serde,
            "tuple enum variants are not supported in CSV; only unit variants are allowed",
        ))
    }

    fn struct_variant<V>(
        self,
        _fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Error>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::detailed(
            ErrorKind::Serde,
            "struct enum variants are not supported in CSV; only unit variants are allowed",
        ))
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteRecord;
    use serde::Deserialize as _;
    use serde::de::DeserializeSeed;

    struct DummyVisitor;
    impl<'de> de::Visitor<'de> for DummyVisitor {
        type Value = ();
        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("dummy")
        }
        fn visit_unit<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_none<E>(self) -> Result<(), E> {
            Ok(())
        }
        fn visit_some<D: de::Deserializer<'de>>(self, _deserializer: D) -> Result<(), D::Error> {
            Ok(())
        }
        fn visit_char<E>(self, _v: char) -> Result<(), E> {
            Ok(())
        }
        fn visit_bool<E>(self, _v: bool) -> Result<(), E> {
            Ok(())
        }
        fn visit_str<E>(self, _v: &str) -> Result<(), E> {
            Ok(())
        }
        fn visit_borrowed_str<E>(self, _v: &'de str) -> Result<(), E> {
            Ok(())
        }
        fn visit_string<E>(self, _v: String) -> Result<(), E> {
            Ok(())
        }
        fn visit_bytes<E>(self, _v: &[u8]) -> Result<(), E> {
            Ok(())
        }
        fn visit_borrowed_bytes<E>(self, _v: &'de [u8]) -> Result<(), E> {
            Ok(())
        }
        fn visit_byte_buf<E>(self, _v: Vec<u8>) -> Result<(), E> {
            Ok(())
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, _seq: A) -> Result<(), A::Error> {
            Ok(())
        }
        fn visit_newtype_struct<D: de::Deserializer<'de>>(
            self,
            _deserializer: D,
        ) -> Result<(), D::Error> {
            Ok(())
        }
        fn visit_enum<A: de::EnumAccess<'de>>(self, _data: A) -> Result<(), A::Error> {
            Ok(())
        }
    }

    struct DummySeed;
    impl<'de> DeserializeSeed<'de> for DummySeed {
        type Value = ();
        fn deserialize<D: de::Deserializer<'de>>(
            self,
            _deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            Ok(())
        }
    }

    #[test]
    fn test_field_deserializer_unsupported_methods() {
        let fd = FieldDeserializer::present(b"test", false);
        assert!(de::Deserializer::deserialize_seq(fd, DummyVisitor).is_err());

        let fd = FieldDeserializer::present(b"test", false);
        assert!(de::Deserializer::deserialize_tuple(fd, 2, DummyVisitor).is_err());

        let fd = FieldDeserializer::present(b"test", false);
        assert!(de::Deserializer::deserialize_tuple_struct(fd, "T", 2, DummyVisitor).is_err());

        let fd = FieldDeserializer::present(b"test", false);
        assert!(de::Deserializer::deserialize_map(fd, DummyVisitor).is_err());

        let fd = FieldDeserializer::present(b"test", false);
        assert!(de::Deserializer::deserialize_struct(fd, "S", &[], DummyVisitor).is_err());

        let uva = UnitVariantAccess;
        assert!(de::VariantAccess::newtype_variant_seed(uva, DummySeed).is_err());

        let uva = UnitVariantAccess;
        assert!(de::VariantAccess::tuple_variant(uva, 2, DummyVisitor).is_err());

        let uva = UnitVariantAccess;
        assert!(de::VariantAccess::struct_variant(uva, &[], DummyVisitor).is_err());
    }

    #[test]
    fn test_csv_deserializer_map_no_headers_and_enum() {
        use std::collections::HashMap;
        let fields: &[&[u8]] = &[b"a", b"b"];
        let de =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        assert!(<HashMap<String, String> as serde::Deserialize>::deserialize(de).is_err());

        #[derive(serde::Deserialize, PartialEq, Debug)]
        enum Color {
            Red,
            Green,
        }
        let fields: &[&[u8]] = &[b"Red"];
        let de =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        let color = <Color as serde::Deserialize>::deserialize(de).unwrap();
        assert_eq!(color, Color::Red);

        // More edge cases:
        // CsvDeserializer deserialize_unit, unit_struct, newtype_struct, tuple_struct, ignored_any
        let de1 =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        assert!(de::Deserializer::deserialize_unit(de1, DummyVisitor).is_ok());

        let de2 =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        assert!(de::Deserializer::deserialize_unit_struct(de2, "Unit", DummyVisitor).is_ok());

        let fields_str: &[&[u8]] = &[b"hello"];
        let de3 =
            CsvDeserializer::new_null_aware(fields_str.iter().copied(), FieldNulls::Disabled, None);
        assert!(de::Deserializer::deserialize_newtype_struct(de3, "NewType", DummyVisitor).is_ok());

        let fields_ts: &[&[u8]] = &[b"hello", b"42"];
        let de4 =
            CsvDeserializer::new_null_aware(fields_ts.iter().copied(), FieldNulls::Disabled, None);
        assert!(de::Deserializer::deserialize_tuple(de4, 2, DummyVisitor).is_ok());

        let de4_ts =
            CsvDeserializer::new_null_aware(fields_ts.iter().copied(), FieldNulls::Disabled, None);
        assert!(
            de::Deserializer::deserialize_tuple_struct(de4_ts, "TupleStruct", 2, DummyVisitor)
                .is_ok()
        );

        let de5 =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        assert!(de::Deserializer::deserialize_ignored_any(de5, DummyVisitor).is_ok());

        // deserialize_option with 0 fields
        let empty_fields: &[&[u8]] = &[];
        let de_empty = CsvDeserializer::new_null_aware(
            empty_fields.iter().copied(),
            FieldNulls::Disabled,
            None,
        );
        let opt_empty = <Option<String> as serde::Deserialize>::deserialize(de_empty).unwrap();
        assert_eq!(opt_empty, None);

        // deserialize_char errors
        let fd_empty_char = FieldDeserializer::present(b"", false);
        assert!(de::Deserializer::deserialize_char(fd_empty_char, DummyVisitor).is_err());
        let fd_two_char = FieldDeserializer::present(b"ab", false);
        assert!(de::Deserializer::deserialize_char(fd_two_char, DummyVisitor).is_err());

        // deserialize_bool error
        let fd_bad_bool = FieldDeserializer::present(b"maybe", false);
        assert!(de::Deserializer::deserialize_bool(fd_bad_bool, DummyVisitor).is_err());

        // deserialize_byte_buf
        let fd_buf = FieldDeserializer::present(b"abc", false);
        assert!(de::Deserializer::deserialize_byte_buf(fd_buf, DummyVisitor).is_ok());

        // FieldDeserializer unit_struct & newtype_struct
        let fd_unit = FieldDeserializer::present(b"", false);
        assert!(de::Deserializer::deserialize_unit(fd_unit, DummyVisitor).is_ok());
        let fd_unit_struct = FieldDeserializer::present(b"", false);
        assert!(
            de::Deserializer::deserialize_unit_struct(fd_unit_struct, "U", DummyVisitor).is_ok()
        );
        let fd_nt = FieldDeserializer::present(b"nt", false);
        assert!(
            de::Deserializer::deserialize_newtype_struct(fd_nt, "NewType", DummyVisitor).is_ok()
        );
        let fd_bytes = FieldDeserializer::present(b"abc", false);
        assert!(de::Deserializer::deserialize_bytes(fd_bytes, DummyVisitor).is_ok());
        let fd_str = FieldDeserializer::present(b"abc", false);
        assert!(de::Deserializer::deserialize_str(fd_str, DummyVisitor).is_ok());
        let fd_string = FieldDeserializer::present(b"abc", false);
        assert!(de::Deserializer::deserialize_string(fd_string, DummyVisitor).is_ok());
        let fd_any_err = FieldDeserializer::present(b"\xff\xfe", false);
        assert!(de::Deserializer::deserialize_any(fd_any_err, DummyVisitor).is_ok());

        // FieldDeserializer::missing error paths
        assert!(
            de::Deserializer::deserialize_bool(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_i8(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_f32(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_char(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_str(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_string(FieldDeserializer::missing(), DummyVisitor)
                .is_err()
        );
        assert!(
            de::Deserializer::deserialize_bytes(FieldDeserializer::missing(), DummyVisitor)
                .is_err()
        );
        assert!(
            de::Deserializer::deserialize_byte_buf(FieldDeserializer::missing(), DummyVisitor)
                .is_err()
        );
        assert!(
            de::Deserializer::deserialize_enum(
                FieldDeserializer::missing(),
                "E",
                &[],
                DummyVisitor
            )
            .is_err()
        );
        assert!(
            de::Deserializer::deserialize_any(FieldDeserializer::missing(), DummyVisitor).is_err()
        );
        assert!(
            de::Deserializer::deserialize_identifier(FieldDeserializer::missing(), DummyVisitor)
                .is_err()
        );
        assert!(
            de::Deserializer::deserialize_option(FieldDeserializer::missing(), DummyVisitor)
                .is_ok()
        );

        // FieldNulls with Endpoints and Spans
        use crate::ByteRecord;
        let mut br = ByteRecord::new();
        br.push_field(b"a");
        br.push_null();
        let nulls_ep = br.null_flags();
        let mut fn_ep = FieldNulls::Endpoints(nulls_ep);
        assert!(!fn_ep.peek());
        assert!(!fn_ep.next());
        assert!(fn_ep.peek());
        assert!(fn_ep.next());
        assert!(!fn_ep.next());

        let mut parser = crate::SliceParser::with_options(
            b"a,\\N\n",
            crate::config::FormatOptions::CSV.nulls(crate::config::Nulls::Mysql),
            crate::config::ParseOptions::new().headers(crate::config::Headers::None),
        )
        .unwrap();
        let mut line = parser.next_line().unwrap().unwrap();
        let rec = line.record().unwrap();
        let nulls_spans = rec.null_flags();
        let mut fn_sp = FieldNulls::Spans(nulls_spans);
        assert!(!fn_sp.peek());
        assert!(!fn_sp.next());
        assert!(fn_sp.peek());
        assert!(fn_sp.next());
        assert!(!fn_sp.next());

        // Map without headers error
        let de_map_no_hdr =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        assert!(
            <std::collections::HashMap<String, String> as serde::Deserialize>::deserialize(
                de_map_no_hdr
            )
            .is_err()
        );

        // Struct without headers
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct UnheadedStruct {
            a: String,
            b: i32,
        }
        let de_struct_no_hdr =
            CsvDeserializer::new_null_aware(fields_ts.iter().copied(), FieldNulls::Disabled, None);
        let s: UnheadedStruct = serde::Deserialize::deserialize(de_struct_no_hdr).unwrap();
        assert_eq!(s.a, "hello");
        assert_eq!(s.b, 42);

        // Numeric parsing slow path (fallback)
        let fd_int_slow = FieldDeserializer::present(b"not_an_int", false);
        assert!(<i32 as serde::Deserialize>::deserialize(fd_int_slow).is_err());
        let fd_float_slow = FieldDeserializer::present(b"NaN", false);
        assert!(
            <f64 as serde::Deserialize>::deserialize(fd_float_slow)
                .unwrap()
                .is_nan()
        );

        // Enum unit variant
        #[derive(serde::Deserialize, Debug, PartialEq)]
        enum TestEnum {
            Foo,
            Bar,
        }
        let fd_enum = FieldDeserializer::present(b"Foo", false);
        assert_eq!(
            <TestEnum as serde::Deserialize>::deserialize(fd_enum).unwrap(),
            TestEnum::Foo
        );

        // Option Some
        let fd_opt_some = FieldDeserializer::present(b"val", false);
        assert_eq!(
            <Option<String> as serde::Deserialize>::deserialize(fd_opt_some).unwrap(),
            Some("val".to_string())
        );

        // MapDeserializer skipping columns
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct PartialStruct {
            col2: String,
        }
        let mut cache = crate::serde::struct_cache::StructCache::new();
        let mut headers = ByteRecord::new();
        headers.append_field(b"col1");
        headers.append_field(b"col2");
        cache.sync(Some(&headers));
        let field_slice: &[&[u8]] = &[b"val1", b"val2"];
        let de_map = CsvDeserializer::new_null_aware(
            field_slice.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        let ps: PartialStruct = serde::Deserialize::deserialize(de_map).unwrap();
        assert_eq!(ps.col2, "val2");
    }

    #[test]
    fn exhausted_null_flag_iterators_stay_false() {
        let record = ByteRecord::new();
        let mut endpoints = FieldNulls::Endpoints(record.null_flags());
        assert!(!endpoints.next());
        assert!(!endpoints.next());

        let spans = crate::span::SpanSet::new();
        let scratch = Vec::new();
        let record = crate::record::Record::new(spans.resolved(b"", &scratch), 0..0, 0)
            .with_null_aware(true);
        let mut spans = FieldNulls::Spans(record.null_flags());
        assert!(!spans.next());
        assert!(!spans.next());
    }

    #[test]
    fn deserialization_shape_errors_keep_their_diagnostics() {
        let fields: &[&[u8]] = &[b"value"];
        let deserializer =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        let error =
            <std::collections::BTreeMap<String, String> as serde::Deserialize>::deserialize(
                deserializer,
            )
            .expect_err("maps need headers");
        assert!(error.to_string().contains(
            "cannot deserialize a map from a CSV record without headers; provide headers via ParseOptions or IoParser::deserialized"
        ));

        let error = FieldDeserializer::missing()
            .require()
            .expect_err("a missing required field must fail");
        assert!(error.to_string().contains("required CSV field is absent"));

        let error = de::Deserializer::deserialize_char(
            FieldDeserializer::present(b"", false),
            DummyVisitor,
        )
        .expect_err("an empty character must fail");
        assert!(
            error
                .to_string()
                .contains("expected a single character, got an empty field")
        );

        let error = de::Deserializer::deserialize_seq(
            FieldDeserializer::present(b"value", false),
            DummyVisitor,
        )
        .expect_err("a sequence cannot occupy one field");
        assert!(error.to_string().contains(
            "sequences are not supported as values within a single CSV field; use a top-level sequence for multiple fields"
        ));

        let error = de::Deserializer::deserialize_tuple_struct(
            FieldDeserializer::present(b"value", false),
            "Pair",
            2,
            DummyVisitor,
        )
        .expect_err("a tuple struct cannot occupy one field");
        assert!(
            error
                .to_string()
                .contains("tuple structs are not supported as values within a single CSV field")
        );

        let error = de::Deserializer::deserialize_map(
            FieldDeserializer::present(b"value", false),
            DummyVisitor,
        )
        .expect_err("a map cannot occupy one field");
        assert!(
            error
                .to_string()
                .contains("maps are not supported as values within a single CSV field")
        );

        let error = de::Deserializer::deserialize_struct(
            FieldDeserializer::present(b"value", false),
            "Nested",
            &[],
            DummyVisitor,
        )
        .expect_err("a struct cannot occupy one field");
        assert!(
            error
                .to_string()
                .contains("structs are not supported as values within a single CSV field")
        );
    }

    #[test]
    fn sequence_errors_report_the_exact_field_index() {
        let fields: &[&[u8]] = &[b"1", b"not-a-number"];
        let deserializer =
            CsvDeserializer::new_null_aware(fields.iter().copied(), FieldNulls::Disabled, None);
        let error = <(u32, u32) as serde::Deserialize>::deserialize(deserializer)
            .expect_err("the second field is invalid");
        assert_eq!(error.location().field, 1);
    }

    #[test]
    fn struct_observation_records_ignored_columns() {
        #[derive(Debug, serde::Deserialize, PartialEq)]
        struct OnlyKept {
            kept: String,
        }

        let mut headers = ByteRecord::new();
        headers.push_field("ignored");
        headers.push_field("kept");
        let fields: &[&[u8]] = &[b"drop", b"keep"];
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));

        let deserializer = CsvDeserializer::new_null_aware(
            fields.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        let value = OnlyKept::deserialize(deserializer).expect("the kept field deserializes");
        assert_eq!(
            value,
            OnlyKept {
                kept: "keep".to_owned()
            }
        );
        cache.commit();
        assert!(mask_contains(cache.ignored_mask(), 0));
        assert!(!mask_contains(cache.ignored_mask(), 1));
    }

    #[test]
    fn maps_neither_skip_learned_columns_nor_observe_ignored_values() {
        use serde::de::{IgnoredAny, Visitor};

        #[derive(Debug, serde::Deserialize)]
        struct OnlyKept {
            kept: String,
        }

        #[derive(Debug)]
        struct IgnoreValues;

        impl<'de> serde::Deserialize<'de> for IgnoreValues {
            fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct IgnoreVisitor;

                impl<'de> Visitor<'de> for IgnoreVisitor {
                    type Value = IgnoreValues;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a CSV map")
                    }

                    fn visit_map<A: de::MapAccess<'de>>(
                        self,
                        mut map: A,
                    ) -> Result<Self::Value, A::Error> {
                        while map.next_key::<String>()?.is_some() {
                            map.next_value::<IgnoredAny>()?;
                        }
                        Ok(IgnoreValues)
                    }
                }

                deserializer.deserialize_map(IgnoreVisitor)
            }
        }

        let mut headers = ByteRecord::new();
        headers.push_field("ignored");
        headers.push_field("kept");
        let fields: &[&[u8]] = &[b"drop", b"keep"];
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));

        let train = CsvDeserializer::new_null_aware(
            fields.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        let row = OnlyKept::deserialize(train).expect("training struct");
        assert_eq!(row.kept, "keep");
        cache.commit();
        assert!(mask_contains(cache.ignored_mask(), 0));

        let map = CsvDeserializer::new_null_aware(
            fields.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        let decoded =
            <std::collections::BTreeMap<String, String>>::deserialize(map).expect("full map");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded.get("ignored").map(String::as_str), Some("drop"));
        assert_eq!(decoded.get("kept").map(String::as_str), Some("keep"));

        cache.reset();
        cache.sync(Some(&headers));
        assert!(!cache.begin_struct("Pending", &["kept"]));
        let ignored = CsvDeserializer::new_null_aware(
            fields.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        IgnoreValues::deserialize(ignored).expect("ignored values are accepted");
        cache.commit();
        assert_eq!(
            cache.ignored_mask(),
            0,
            "map values must not train the struct skip cache"
        );
    }

    #[test]
    fn map_skipping_consumes_fields_and_stops_at_the_header_boundary() {
        use serde::de::MapAccess as _;

        let mut headers = ByteRecord::new();
        headers.push_field("ignored");
        headers.push_field("kept");
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        assert!(!cache.begin_struct("OnlyKept", &["kept"]));
        cache.note_ignored(0);
        cache.commit();

        let fields: &[&[u8]] = &[b"drop", b"keep"];
        let nullable = fields.iter().copied().map(|field| (field, false));
        let mut map = MapDeserializer::full(&cache, nullable, true, false);
        assert_eq!(map.next_key::<String>().unwrap().as_deref(), Some("kept"));
        assert_eq!(map.next_value::<String>().unwrap(), "keep");
        assert_eq!(map.next_key::<String>().unwrap(), None);
        assert_eq!(map.column, cache.columns);

        let mut one_header = ByteRecord::new();
        one_header.push_field("only");
        let mut boundary_cache = StructCache::new();
        boundary_cache.sync(Some(&one_header));
        assert!(!boundary_cache.begin_struct("NoneKept", &[]));
        boundary_cache.note_ignored(0);
        boundary_cache.note_ignored(1);
        boundary_cache.commit();

        let fields: &[&[u8]] = &[b"drop"];
        let nullable = fields.iter().copied().map(|field| (field, false));
        let mut map = MapDeserializer::full(&boundary_cache, nullable, true, false);
        assert_eq!(map.next_key::<String>().unwrap(), None);
        assert_eq!(
            map.column, boundary_cache.columns,
            "the skip loop must not advance beyond the header count"
        );
    }

    #[test]
    fn invalid_header_errors_begin_at_the_invalid_column() {
        use serde::de::MapAccess as _;

        let mut headers = ByteRecord::new();
        headers.push_field("valid");
        headers.push_field(b"\xff");
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let fields: &[&[u8]] = &[b"first", b"second"];
        let nullable = fields.iter().copied().map(|field| (field, false));
        let mut map = MapDeserializer::full(&cache, nullable, false, false);

        assert_eq!(
            map.next_key::<String>().expect("valid key").as_deref(),
            Some("valid")
        );
        assert_eq!(map.next_value::<String>().expect("valid value"), "first");
        let error = map
            .next_key::<String>()
            .expect_err("the second header is invalid");
        assert!(error.to_string().contains("CSV header is not valid UTF-8"));
    }

    #[test]
    fn map_value_errors_keep_the_header_name_and_column_index() {
        #[derive(Debug, serde::Deserialize)]
        #[expect(dead_code, reason = "deserialization fails on the second field")]
        struct Numbers {
            first: u32,
            second: u32,
        }

        let mut headers = ByteRecord::new();
        headers.push_field("first");
        headers.push_field("second");
        let fields: &[&[u8]] = &[b"1", b"bad"];
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let deserializer = CsvDeserializer::new_null_aware(
            fields.iter().copied(),
            FieldNulls::Disabled,
            Some(HeaderView::Full(&cache)),
        );
        let error = Numbers::deserialize(deserializer).expect_err("second is not a number");
        assert_eq!(error.location().field, 1);
        assert_eq!(error.field_name(), Some("second"));
    }

    #[test]
    fn map_size_hint_tracks_remaining_header_columns() {
        use serde::de::MapAccess as _;

        let mut headers = ByteRecord::new();
        headers.push_field("a");
        headers.push_field("b");
        headers.push_field("c");
        let fields: &[&[u8]] = &[b"1", b"2", b"3"];
        let mut cache = StructCache::new();
        cache.sync(Some(&headers));
        let nullable = fields.iter().copied().map(|field| (field, false));
        let mut map = MapDeserializer::full(&cache, nullable, false, false);

        assert_eq!(map.size_hint(), Some(3));
        assert_eq!(map.next_key::<String>().unwrap().as_deref(), Some("a"));
        assert_eq!(map.next_value::<String>().unwrap(), "1");
        assert_eq!(map.size_hint(), Some(2));
        assert_eq!(map.next_key::<String>().unwrap().as_deref(), Some("b"));
        assert_eq!(map.next_value::<String>().unwrap(), "2");
        assert_eq!(map.size_hint(), Some(1));
        assert_eq!(map.next_key::<String>().unwrap().as_deref(), Some("c"));
        assert_eq!(map.next_value::<String>().unwrap(), "3");
        assert_eq!(map.size_hint(), Some(0));
    }
}

// ── RecordSerializer ─────────────────────────────────────────────────────────────

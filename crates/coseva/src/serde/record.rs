use std::{fmt, str};

use serde::ser;

use super::deserializer::{CsvDeserializer, FieldNulls, HeaderView};
use super::serializer::{DirectSink, HeaderState, Nesting, RecordEmit, RecordSerializer};
use super::struct_cache::StructCache;
use crate::byte_record::ByteRecord;
use crate::config::{Dialect, Nulls, Quoting};
use crate::emit::ByteSink;
use crate::error::{Error, ErrorKind, Location};
use crate::format::CsvFormat;
use crate::record::Record;

struct RecordFormatter<'a, S: RecordEmit + ?Sized>(&'a mut S);

impl<S: RecordEmit + ?Sized> fmt::Write for RecordFormatter<'_, S> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_bytes(s.as_bytes());
        Ok(())
    }
}

pub(super) fn format_into_record<S: RecordEmit + ?Sized, T: fmt::Display + ?Sized>(
    sink: &mut S,
    value: &T,
) -> Result<(), Error> {
    fmt::write(&mut RecordFormatter(sink), format_args!("{value}"))
        .map_err(|_error| Error::detailed(ErrorKind::Serde, "display value could not be formatted"))
}

/// Builds a structured invalid-UTF-8 field error.
pub(super) fn utf8_error(error: str::Utf8Error) -> Error {
    Error::new(ErrorKind::InvalidUtf8(error), Location::UNKNOWN)
}

/// Builds a field parsing error with its input and target type.
pub(super) fn parse_error<E: fmt::Display>(s: &str, ty: &str, error: E) -> Error {
    Error::detailed(
        ErrorKind::Serde,
        format!("could not parse {s:?} as {ty}: {error}"),
    )
}

/// Deserializes an owned value from a byte record.
#[cfg(test)]
pub(super) fn deserialize_byte_record_owned<T: ::serde::de::DeserializeOwned>(
    record: &ByteRecord,
    headers: Option<HeaderView<'_>>,
) -> Result<T, Error> {
    T::deserialize(CsvDeserializer::new_null_aware(
        record.iter(),
        byte_record_field_nulls(record),
        headers,
    ))
}

/// Deserializes a potentially borrowing value from a byte record.
pub(crate) fn deserialize_byte_record<'de, T: ::serde::Deserialize<'de>>(
    record: &'de ByteRecord,
    headers: Option<HeaderView<'de>>,
) -> Result<T, Error> {
    T::deserialize(CsvDeserializer::new_null_aware(
        record.iter(),
        byte_record_field_nulls(record),
        headers,
    ))
}

/// Deserializes a lending record, optionally mapping fields through headers.
pub(crate) fn deserialize_full_record<'de, T: ::serde::Deserialize<'de>>(
    record: &Record<'de>,
    cache: Option<&'de StructCache>,
) -> Result<T, Error> {
    T::deserialize(CsvDeserializer::new_null_aware(
        record.iter(),
        record_field_nulls(record),
        cache.map(HeaderView::Full),
    ))
}

/// Selects explicit-NULL flags for a lending record.
fn record_field_nulls<'de>(record: &Record<'de>) -> FieldNulls<'de> {
    if record.null_aware {
        FieldNulls::Spans(record.null_flags())
    } else {
        FieldNulls::Disabled
    }
}

/// Deserializes a lending record positionally.
pub(crate) fn deserialize_record<'de, T: ::serde::Deserialize<'de>>(
    record: &Record<'de>,
) -> Result<T, Error> {
    T::deserialize(CsvDeserializer::new_null_aware(
        record.iter(),
        record_field_nulls(record),
        None,
    ))
}

/// Selects explicit-NULL flags for a byte record.
fn byte_record_field_nulls(record: &ByteRecord) -> FieldNulls<'_> {
    if record.null_aware() {
        FieldNulls::Endpoints(record.null_flags())
    } else {
        FieldNulls::Disabled
    }
}

#[cfg(test)]
pub(super) fn serialize_to_record<T: ser::Serialize + ?Sized>(
    value: &T,
    record: &mut ByteRecord,
    allow_nested: bool,
) -> Result<(), Error> {
    record.clear();
    let mut ser = RecordSerializer {
        sink: record,
        headers: None,
        nesting: allow_nested.into(),
    };
    value.serialize(&mut ser)?;
    Ok(())
}

/// Serialize `value` as a record, framing each field straight into `output`.
///
/// Fields are never staged in an intermediate record: each is quoted, escaped
/// and written into `output` as it arrives, reusing `scratch` for fields the
/// serializer builds incrementally. The record's terminator is written on
/// success and the field count returned; on error the caller truncates
/// `output` back to where the record began, leaving nothing behind.
pub(crate) fn serialize_direct<T, F, B>(
    value: &T,
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    scratch: &mut Vec<u8>,
    allow_nested: bool,
) -> Result<usize, Error>
where
    T: ser::Serialize + ?Sized,
    F: CsvFormat,
    B: ByteSink,
{
    let mut sink: DirectSink<'_, '_, F, B> =
        DirectSink::new(output, dialect, quoting, nulls, scratch);
    let mut ser = RecordSerializer {
        sink: &mut sink,
        headers: None,
        nesting: allow_nested.into(),
    };
    value.serialize(&mut ser)?;
    Ok(sink.finish())
}

/// Serialize `value` directly, collecting struct field names into `headers`.
///
/// This is the once-per-document header path: the data record is framed into
/// `output` while its field names accumulate in `headers`, so the caller can
/// splice a header row ahead of it. Returns whether the value was a named
/// struct (and so has a header row) alongside the data record's field count.
/// On error the caller truncates `output` back to the record start.
pub(crate) fn serialize_direct_with_headers<T, F, B>(
    value: &T,
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    scratch: &mut Vec<u8>,
    headers: &mut ByteRecord,
) -> Result<(bool, usize), Error>
where
    T: ser::Serialize + ?Sized,
    F: CsvFormat,
    B: ByteSink,
{
    headers.clear();
    let mut sink: DirectSink<'_, '_, F, B> =
        DirectSink::new(output, dialect, quoting, nulls, scratch);
    let named = {
        let mut ser = RecordSerializer {
            sink: &mut sink,
            headers: Some(HeaderState {
                record: headers,
                named: false,
            }),
            nesting: Nesting::Reject,
        };
        value.serialize(&mut ser)?;
        ser.headers.as_ref().is_some_and(|headers| headers.named)
    };
    Ok((named, sink.finish()))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    struct FailingDisplay;

    impl fmt::Display for FailingDisplay {
        fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            Err(fmt::Error)
        }
    }

    #[derive(serde::Deserialize, PartialEq, Debug)]
    struct Pair {
        a: String,
        b: u32,
    }

    #[test]
    fn test_record_serde_functions() {
        let mut br = ByteRecord::new();
        br.push_field(b"foo");
        br.push_field(b"42");

        let pair_owned: Pair = deserialize_byte_record_owned(&br, None).unwrap();
        assert_eq!(
            pair_owned,
            Pair {
                a: "foo".to_string(),
                b: 42
            }
        );

        let pair_borrowed: Pair = deserialize_byte_record(&br, None).unwrap();
        assert_eq!(
            pair_borrowed,
            Pair {
                a: "foo".to_string(),
                b: 42
            }
        );

        let mut parser = crate::SliceParser::<crate::format::Csv>::new(
            b"foo,42\n",
            crate::config::ParseOptions::new().headers(crate::config::Headers::None),
        )
        .unwrap();
        let mut line = parser.next_line().unwrap().unwrap();
        let rec = line.record().unwrap();

        let pair_rec: Pair = deserialize_record(&rec).unwrap();
        assert_eq!(
            pair_rec,
            Pair {
                a: "foo".to_string(),
                b: 42
            }
        );

        let pair_full: Pair = deserialize_full_record(&rec, None).unwrap();
        assert_eq!(
            pair_full,
            Pair {
                a: "foo".to_string(),
                b: 42
            }
        );

        // serialize_to_record error path
        let mut map = std::collections::HashMap::new();
        map.insert("k", "v");
        assert!(serialize_to_record(&map, &mut br, false).is_err());
    }

    #[test]
    fn formatting_failures_keep_their_diagnostic() {
        let mut record = ByteRecord::new();
        let error = format_into_record(&mut record, &FailingDisplay)
            .expect_err("the failing formatter must be reported");
        assert_eq!(error.kind(), ErrorKind::Serde);
        assert!(
            error
                .to_string()
                .contains("display value could not be formatted"),
            "unexpected diagnostic: {error}"
        );
    }

    #[test]
    fn null_flag_sources_follow_record_awareness() {
        let ordinary = ByteRecord::new();
        assert!(matches!(
            byte_record_field_nulls(&ordinary),
            FieldNulls::Disabled
        ));

        let mut aware = ByteRecord::new();
        aware.push_null();
        assert!(matches!(
            byte_record_field_nulls(&aware),
            FieldNulls::Endpoints(_)
        ));

        let spans = crate::span::SpanSet::new();
        let scratch = Vec::new();
        let ordinary = Record::new(spans.resolved(b"", &scratch), 0..0, 0).with_null_aware(false);
        assert!(matches!(
            record_field_nulls(&ordinary),
            FieldNulls::Disabled
        ));

        let aware = ordinary.with_null_aware(true);
        assert!(matches!(record_field_nulls(&aware), FieldNulls::Spans(_)));
    }

    #[test]
    fn header_serialization_rejects_nested_struct_fields() {
        #[derive(serde::Serialize)]
        struct Inner {
            value: u32,
        }

        #[derive(serde::Serialize)]
        struct Outer {
            inner: Inner,
        }

        let mut output = Vec::new();
        let mut scratch = Vec::new();
        let mut headers = ByteRecord::new();
        let result = serialize_direct_with_headers::<_, crate::format::Csv, _>(
            &Outer {
                inner: Inner { value: 7 },
            },
            &mut output,
            Dialect::default(),
            Quoting::Necessary,
            Nulls::None,
            &mut scratch,
            &mut headers,
        );

        let error = result.expect_err("nested fields are not valid with automatic headers");
        assert_eq!(error.kind(), ErrorKind::Serde);
        assert!(error.to_string().contains("structs are not supported"));
    }
}

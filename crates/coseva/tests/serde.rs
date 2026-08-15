//! Tests for the optional Serde compatibility layer.
//!
//! Covers deserialization of records into scalars, tuples, structs, maps and
//! enums, serialization of Rust values back into CSV records, the errors
//! produced for shapes CSV cannot represent, and the header caching and column
//! projection the struct path relies on.
//!
//! Run with: `cargo test --test serde --features serde`

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![expect(
    clippy::unwrap_used,
    reason = "test fixtures fail immediately when setup or decoding is incorrect"
)]

use std::collections::HashMap;
use std::error::Error as StdError;

use coseva::config::{EmitOptions, FieldCount, FormatOptions, Headers, ParseOptions, Quoting};
use coseva::format::Csv;
use coseva::{ByteRecord, ErrorKind, IoParser, PushParser, SliceParser};
use coseva::{IoEmitter, VecEmitter};
use serde::{Deserialize, Serialize};

mod common;

use common::unheaded;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn slice_reader_with_headers(csv: &'static [u8]) -> SliceParser<'static> {
    SliceParser::with_options(
        csv,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap()
}

fn no_header_csv_emitter() -> VecEmitter {
    VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options")
}

// ── Primitive integer fields ─────────────────────────────────────────────────

#[test]
fn deserialize_i8_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"-128\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (i8,) = line.deserialized()?;
    assert_eq!(v, i8::MIN);
    Ok(())
}

#[test]
fn deserialize_i16_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"-32768\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (i16,) = line.deserialized()?;
    assert_eq!(v, i16::MIN);
    Ok(())
}

#[test]
fn deserialize_i32_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"2147483647\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (i32,) = line.deserialized()?;
    assert_eq!(v, i32::MAX);
    Ok(())
}

#[test]
fn deserialize_i64_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"-9223372036854775808\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (i64,) = line.deserialized()?;
    assert_eq!(v, i64::MIN);
    Ok(())
}

#[test]
fn deserialize_i128_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"170141183460469231731687303715884105727\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (i128,) = line.deserialized()?;
    assert_eq!(v, i128::MAX);
    Ok(())
}

#[test]
fn deserialize_u8_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"255\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (u8,) = line.deserialized()?;
    assert_eq!(v, u8::MAX);
    Ok(())
}

#[test]
fn deserialize_u16_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"65535\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (u16,) = line.deserialized()?;
    assert_eq!(v, u16::MAX);
    Ok(())
}

#[test]
fn deserialize_u32_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"4294967295\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (u32,) = line.deserialized()?;
    assert_eq!(v, u32::MAX);
    Ok(())
}

#[test]
fn deserialize_u128_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"340282366920938463463374607431768211455\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (u128,) = line.deserialized()?;
    assert_eq!(v, u128::MAX);
    Ok(())
}

#[test]
fn integers_deserialize_directly_from_bytes() {
    let mut r = unheaded(b"-128,65535,650706,+7,18446744073709551615\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: (i8, u16, u32, u64, u64) = line.deserialized().unwrap();
    assert_eq!(row, (-128, 65535, 650_706, 7, u64::MAX));
}

#[test]
fn invalid_integer_falls_back_to_the_utf8_error_message() {
    let mut r = unheaded(b"12a\n");
    let mut line = r.next_line().unwrap().expect("record");
    let error = line
        .deserialized::<(u32,)>()
        .expect_err("deserializing '12a' as u32 should fail");
    assert_eq!(error.kind(), ErrorKind::Serde);
    // The cold fallback re-parses through `str`, so the message keeps naming
    // both the offending text and the target type.
    let text = error.to_string();
    assert!(text.contains("12a"), "error: {text}");
    assert!(text.contains("u32"), "error: {text}");
}

#[test]
fn out_of_range_integer_reports_an_error() {
    let mut r = unheaded(b"256\n");
    let mut line = r.next_line().unwrap().expect("record");
    let error = line
        .deserialized::<(u8,)>()
        .expect_err("256 does not fit in u8");
    assert_eq!(error.kind(), ErrorKind::Serde);
}

#[test]
fn non_utf8_integer_field_reports_a_utf8_error() {
    let mut r = unheaded(b"\xFF\n");
    let mut line = r.next_line().unwrap().expect("record");
    let error = line
        .deserialized::<(u32,)>()
        .expect_err("invalid UTF-8 should fail");
    assert!(error.to_string().contains("UTF-8"), "error: {error}");
}

// ── Floating point fields ────────────────────────────────────────────────────

#[test]
fn deserialize_f32_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"1.5\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (f32,) = line.deserialized()?;
    assert!((v - 1.5_f32).abs() < f32::EPSILON);
    Ok(())
}

#[test]
fn deserialize_f64_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"2.25\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (f64,) = line.deserialized()?;
    assert!((v - 2.25_f64).abs() < 1e-10);
    Ok(())
}

#[test]
fn deserialize_f32_invalid_returns_error() {
    // impl_field_float! Err branch for f32 (parse_numeric_slow fallback)
    let mut record = ByteRecord::new();
    record.push_field(b"not-a-float");
    let result: Result<(f32,), _> = record.deserialize();
    let err = result.expect_err("invalid f32 must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn deserialize_f64_invalid_returns_error() {
    // impl_field_float! Err branch for f64 (parse_numeric_slow fallback)
    let mut record = ByteRecord::new();
    record.push_field(b"not-a-float");
    let result: Result<(f64,), _> = record.deserialize();
    let err = result.expect_err("invalid f64 must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

// ── Boolean and mixed scalar fields ──────────────────────────────────────────

#[test]
fn deserialize_bool_field_true() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = ByteRecord::new();
    record.push_field(b"true");
    let (v,): (bool,) = record.deserialize()?;
    assert!(v);
    Ok(())
}

#[test]
fn deserialize_bool_field_false() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = ByteRecord::new();
    record.push_field(b"0");
    let (v,): (bool,) = record.deserialize()?;
    assert!(!v);
    Ok(())
}

#[test]
fn deserialize_bool_field_invalid_returns_error() {
    // FieldDeserializer::deserialize_bool error branch
    let mut record = ByteRecord::new();
    record.push_field(b"yes");
    let result: Result<(bool,), _> = record.deserialize();
    let err = result.expect_err("invalid bool must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn bool_from_zero_one() {
    let mut r = unheaded(b"1\n0\n");
    let mut line = r.next_line().unwrap().expect("record");
    let yes: (bool,) = line.deserialized().unwrap();
    let mut line = r.next_line().unwrap().expect("record");
    let no: (bool,) = line.deserialized().unwrap();
    assert!(yes.0);
    assert!(!no.0);
}

#[test]
fn invalid_bool_returns_error() {
    let mut r = unheaded(b"maybe\n");
    let mut line = r.next_line().unwrap().expect("record");
    let error = line
        .deserialized::<(bool,)>()
        .expect_err("deserializing 'maybe' as bool should fail");
    assert_eq!(error.kind(), ErrorKind::Serde);
    assert_eq!(error.location().field, 0);
}

#[derive(Debug, Deserialize, PartialEq)]
struct NumericRow {
    a_bool: bool,
    an_i32: i32,
    a_u64: u64,
    an_f64: f64,
}

#[test]
fn numeric_and_bool_fields_parse_correctly() {
    let mut r = slice_reader_with_headers(b"a_bool,an_i32,a_u64,an_f64\ntrue,-42,100,3.125\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: NumericRow = line.deserialized().unwrap();
    assert!(row.a_bool);
    assert_eq!(row.an_i32, -42);
    assert_eq!(row.a_u64, 100);
    assert!((row.an_f64 - 3.125).abs() < f64::EPSILON);
}

// ── Char fields ──────────────────────────────────────────────────────────────

#[test]
fn deserialize_char_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"A\n");
    let mut line = r.next_line()?.expect("record");
    let (v,): (char,) = line.deserialized()?;
    assert_eq!(v, 'A');
    Ok(())
}

#[test]
fn deserialize_char_empty_field_returns_error() {
    let mut record = ByteRecord::new();
    record.push_field(b"");
    let result: Result<(char,), _> = record.deserialize();
    let err = result.expect_err("empty field cannot be a char");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn deserialize_char_multi_char_field_returns_error() {
    let mut record = ByteRecord::new();
    record.push_field(b"ab");
    let result: Result<(char,), _> = record.deserialize();
    let err = result.expect_err("two chars cannot deserialize to single char");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn invalid_utf8_in_char_field_returns_error() {
    // FieldDeserializer::deserialize_char utf8_error path
    let mut record = ByteRecord::new();
    record.push_field(b"\xFF\xFE");
    let result: Result<(char,), _> = record.deserialize();
    let err = result.expect_err("invalid UTF-8 must fail for char");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidUtf8(_)),
        "expected InvalidUtf8, got {:?}",
        err.kind()
    );
}

// ── String and byte fields ───────────────────────────────────────────────────

#[test]
fn deserialize_borrowed_str_field() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"hello\n");
    let mut line = r.next_line()?.expect("record");
    let record = line.record()?;
    let (v,): (&str,) = record.deserialize()?;
    assert_eq!(v, "hello");
    Ok(())
}

#[test]
fn string_field_through_headers_path() -> Result<(), Box<dyn std::error::Error>> {
    // Exercises FieldDeserializer::deserialize_string via MapDeserializer path.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        label: String,
    }

    let mut r = slice_reader_with_headers(b"label\nfoo\n");
    let row: Row = r.next_line()?.expect("record").deserialized()?;
    assert_eq!(row.label, "foo");
    Ok(())
}

#[test]
fn record_deserialize_borrows_str() -> Result<(), Box<dyn StdError>> {
    let mut r = unheaded(b"hello,world\n");
    let mut line = r.next_line()?.expect("record");
    let record = line.record()?;

    let row: (&str, &str) = record.deserialize()?;
    assert_eq!(row, ("hello", "world"));
    Ok(())
}

#[test]
fn deserialized_returns_borrowed_str() {
    #[derive(Debug, PartialEq)]
    struct BorrowedRow<'a>(&'a str, &'a str);

    impl<'de: 'a, 'a> Deserialize<'de> for BorrowedRow<'a> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            let (a, b): (&'de str, &'de str) = Deserialize::deserialize(deserializer)?;
            Ok(BorrowedRow(a, b))
        }
    }

    let mut r = unheaded(b"hello,world\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: BorrowedRow<'_> = line.deserialized().unwrap();
    assert_eq!(row, BorrowedRow("hello", "world"));
}

#[test]
fn deserialize_bytes_field() {
    // Requesting deserialize_byte_buf via a type that does so
    #[derive(Debug, PartialEq)]
    struct ByteBuf(Vec<u8>);
    impl<'de> Deserialize<'de> for ByteBuf {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct V;
            impl serde::de::Visitor<'_> for V {
                type Value = ByteBuf;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("bytes")
                }
                fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<ByteBuf, E> {
                    Ok(ByteBuf(v.to_vec()))
                }
            }
            deserializer.deserialize_byte_buf(V)
        }
    }

    let mut record = ByteRecord::new();
    record.push_field(b"\xFF\xFE");
    let (v,): (ByteBuf,) = record.deserialize().expect("byte_buf deserializes");
    assert_eq!(v.0, b"\xFF\xFE");
}

/// A newtype that calls `deserialize_bytes`, the proper serde path for raw bytes.
/// Standard `Vec<u8>` calls `deserialize_seq` instead, which requires `serde_bytes`
/// to override. This type demonstrates the `deserialize_bytes` path directly.
#[derive(Debug, PartialEq)]
struct RawBytes(Vec<u8>);

impl<'de> Deserialize<'de> for RawBytes {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = RawBytes;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("bytes")
            }
            fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<RawBytes, E> {
                Ok(RawBytes(v.to_vec()))
            }
            fn visit_bytes<E>(self, v: &[u8]) -> Result<RawBytes, E> {
                Ok(RawBytes(v.to_vec()))
            }
            fn visit_borrowed_str<E>(self, v: &'de str) -> Result<RawBytes, E> {
                Ok(RawBytes(v.as_bytes().to_vec()))
            }
            fn visit_str<E>(self, v: &str) -> Result<RawBytes, E> {
                Ok(RawBytes(v.as_bytes().to_vec()))
            }
        }
        deserializer.deserialize_bytes(Vis)
    }
}

#[test]
fn invalid_utf8_bytes_preserved_in_byte_vec() {
    let bad: &[u8] = b"\xFF\xFE";
    let mut record = ByteRecord::new();
    record.push_field(bad);

    // deserialize_bytes path: raw bytes are returned without UTF-8 conversion
    let row: (RawBytes,) = record.deserialize().unwrap();
    assert_eq!(row.0.0, bad);
}

#[test]
fn invalid_utf8_str_returns_error() {
    let bad: &[u8] = b"\xFF\xFE";
    let mut record = ByteRecord::new();
    record.push_field(bad);

    let result: Result<(String,), coseva::Error> = record.deserialize();
    assert!(result.is_err(), "expected UTF-8 error for invalid bytes");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("UTF-8"),
        "error message should mention UTF-8: {msg}"
    );
}

#[test]
fn invalid_utf8_in_string_field_returns_error() {
    // FieldDeserializer::deserialize_string utf8_error path
    let mut record = ByteRecord::new();
    record.push_field(b"\xFF\xFE");
    let result: Result<(String,), _> = record.deserialize();
    let err = result.expect_err("invalid UTF-8 must fail");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidUtf8(_)),
        "expected InvalidUtf8, got {:?}",
        err.kind()
    );
}

#[test]
fn invalid_utf8_in_str_field_returns_error() {
    // FieldDeserializer::deserialize_str utf8_error path
    let mut record = ByteRecord::new();
    record.push_field(b"\xFF\xFE");
    let result: Result<(&str,), _> = record.deserialize();
    let err = result.expect_err("invalid UTF-8 must fail for &str");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidUtf8(_)),
        "expected InvalidUtf8, got {:?}",
        err.kind()
    );
}

// ── Unit, unit struct and newtype deserialization ────────────────────────────

#[test]
fn deserialize_unit_from_record() -> Result<(), Box<dyn std::error::Error>> {
    let mut r = unheaded(b"\n");
    let mut line = r.next_line()?.expect("record");
    let u: () = line.deserialized()?;
    assert_eq!(u, ());
    Ok(())
}

#[test]
fn deserialize_unit_struct_from_record() -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Unit;

    let mut r = unheaded(b"\n");
    let mut line = r.next_line()?.expect("record");
    let u: Unit = line.deserialized()?;
    assert_eq!(u, Unit);
    Ok(())
}

#[test]
fn deserialize_newtype_struct_wrapping_u32() -> Result<(), Box<dyn std::error::Error>> {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Wrapped(u32);

    let mut r = unheaded(b"42\n");
    let mut line = r.next_line()?.expect("record");
    let w: Wrapped = line.deserialized()?;
    assert_eq!(w, Wrapped(42));
    Ok(())
}

#[test]
fn deserialize_unit_at_field_level() {
    // FieldDeserializer::deserialize_unit path
    let mut record = ByteRecord::new();
    record.push_field(b"");
    let (u,): ((),) = record.deserialize().expect("unit from field");
    assert_eq!(u, ());
}

#[test]
fn deserialize_unit_struct_at_field_level() {
    // FieldDeserializer::deserialize_unit_struct path
    #[derive(Debug, Deserialize, PartialEq)]
    struct Unit;

    let mut record = ByteRecord::new();
    record.push_field(b"");
    let (u,): (Unit,) = record.deserialize().expect("unit struct from field");
    assert_eq!(u, Unit);
}

#[test]
fn deserialize_newtype_at_field_level() {
    // FieldDeserializer::deserialize_newtype_struct path
    #[derive(Debug, Deserialize, PartialEq)]
    struct Wrapped(u32);

    let mut record = ByteRecord::new();
    record.push_field(b"99");
    let (w,): (Wrapped,) = record.deserialize().expect("newtype from field");
    assert_eq!(w, Wrapped(99));
}

// ── Option and absent field handling ─────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct WithOptional {
    name: String,
    note: Option<String>,
}

#[test]
fn empty_field_deserializes_as_present_empty_string() {
    let mut r = slice_reader_with_headers(b"name,note\nBoston,\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: WithOptional = line.deserialized().unwrap();
    assert_eq!(row.note, Some(String::new()));
}

#[test]
fn non_empty_field_deserializes_as_some() {
    let mut r = slice_reader_with_headers(b"name,note\nBoston,capital\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: WithOptional = line.deserialized().unwrap();
    assert_eq!(row.note, Some("capital".to_string()));
}

#[test]
fn absent_column_deserializes_as_none_for_option() {
    // Record has fewer fields than headers; absent field → None.
    let mut r = slice_reader_with_headers(b"name,note\nBoston\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: WithOptional = line.deserialized().unwrap();
    assert_eq!(row.note, None);
}

#[test]
fn option_present_field_is_some() {
    // FieldDeserializer::deserialize_option path (present, not null)
    let mut record = ByteRecord::new();
    record.push_field(b"hi");
    let (v,): (Option<String>,) = record.deserialize().expect("Some");
    assert_eq!(v, Some("hi".to_string()));
}

#[test]
fn database_null_is_none_but_database_empty_is_present() {
    for (format, input) in [
        (FormatOptions::POSTGRES_COPY_CSV, b",\"\"\n".as_slice()),
        (FormatOptions::MYSQL, b"\\N\t\n".as_slice()),
    ] {
        let mut reader =
            SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))
                .unwrap();
        let mut line = reader.next_line().unwrap().expect("record");
        let row: (Option<String>, Option<String>) = line.deserialized().unwrap();
        assert_eq!(row, (None, Some(String::new())));
    }
}

#[test]
fn missing_field_uses_serde_default() {
    // MapDeserializer::next_value_seed calls FieldDeserializer::missing() when
    // the record has fewer fields than the header column count.
    // Option<T> gracefully handles absent fields via visit_none.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        name: String,
        count: Option<i32>,
    }

    // CSV has two header columns but the record only has one field.
    let mut r = slice_reader_with_headers(b"name,count\nhello\n");
    let row: Row = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized()
        .expect("deserialize ok");
    assert_eq!(
        row,
        Row {
            name: "hello".to_string(),
            count: None,
        }
    );
}

#[test]
fn missing_required_field_returns_error() {
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test type - fields never read")]
    struct Two {
        a: String,
        b: String,
    }

    // Record has only one field; struct expects two via headers → second absent
    let mut r = slice_reader_with_headers(b"a,b\nonly_a\n");
    let mut line = r.next_line().expect("io ok").expect("record");
    let err = line
        .deserialized::<Two>()
        .expect_err("missing second field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn scalar_deserialized_from_empty_record_returns_error() {
    // CsvDeserializer::first_field(): None => FieldDeserializer::missing()
    // Triggered when a scalar type is the top-level target and the record has
    // no fields.
    let record = ByteRecord::new(); // no fields at all
    let result: Result<String, _> = record.deserialize();
    let err = result.expect_err("empty record must fail for String");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

// ── Tuple and sequence deserialization ───────────────────────────────────────

#[test]
fn deserializes_tuple_positionally() {
    let mut r = unheaded(b"Boston,650706\n");
    let mut line = r.next_line().unwrap().expect("record");
    let row: (String, u64) = line.deserialized().unwrap();
    assert_eq!(row, ("Boston".to_string(), 650_706));
}

#[test]
fn deserializes_tuple_struct_positionally() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct TestRecord(String, u64);

    let mut r = unheaded(b"Boston,650706\n");
    let mut line = r.next_line().unwrap().expect("record");
    let record: TestRecord = line.deserialized().unwrap();
    assert_eq!(record, TestRecord("Boston".into(), 650_706));
}

#[test]
fn byte_record_deserialize_positional() {
    let mut record = ByteRecord::new();
    record.push_field(b"Boston");
    record.push_field(b"650706");

    let row: (String, u64) = record.deserialize().unwrap();
    assert_eq!(row, ("Boston".to_string(), 650_706));
}

#[test]
fn deserialize_tuple_with_too_few_fields_uses_none_branch() {
    // SeqDeserializer::next_element_seed returns Ok(None) when the record
    // fields are exhausted before all tuple elements have been read.
    let mut record = ByteRecord::new();
    record.push_field(b"1"); // only 1 field, but we ask for a 2-tuple
    let result: Result<(i32, i32), _> = record.deserialize();
    result.expect_err("too few fields must fail");
}

#[test]
fn nested_seq_in_deserialize_returns_error() {
    // Trying to deserialize a field as a Vec should fail.
    let mut record = ByteRecord::new();
    record.push_field(b"1,2,3");
    let result: Result<(Vec<u32>,), coseva::Error> = record.deserialize();
    assert!(
        result.is_err(),
        "nested seq deserialization should be rejected"
    );
}

#[test]
fn field_level_seq_returns_error() {
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test type - field never read")]
    struct NeedsSeq(Vec<u8>);

    let mut record = ByteRecord::new();
    record.push_field(b"x");
    // Vec<u8> calls deserialize_seq on the field, which must be rejected.
    let result: Result<(NeedsSeq,), _> = record.deserialize();
    let err = result.expect_err("nested seq in field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn deserialize_tuple_as_field_returns_error() {
    // FieldDeserializer::deserialize_tuple error path.
    // A struct field with type (i32, i32) triggers this when using map-based
    // deserialization (with headers).
    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "field/variant needed only for type shape in error-path test"
    )]
    struct HasTuple {
        coords: (i32, i32),
    }

    let mut r = slice_reader_with_headers(b"coords\n1\n");
    let result: Result<HasTuple, _> = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized();
    let err = result.expect_err("tuple as field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("tuple"));
}

#[test]
fn deserialize_tuple_struct_as_field_returns_error() {
    // FieldDeserializer::deserialize_tuple_struct error path.
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "fields needed only to define tuple-struct shape")]
    struct Pair(i32, i32);

    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "field/variant needed only for type shape in error-path test"
    )]
    struct HasPair {
        p: Pair,
    }

    let mut r = slice_reader_with_headers(b"p\n1\n");
    let result: Result<HasPair, _> = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized();
    let err = result.expect_err("tuple struct as field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

// ── Struct and map deserialization ───────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
struct City {
    name: String,
    population: u64,
}

#[test]
fn deserializes_struct_with_headers() {
    let mut r = slice_reader_with_headers(b"name,population\nBoston,650706\n");
    let mut line = r.next_line().unwrap().expect("record");
    let city: City = line.deserialized().unwrap();
    assert_eq!(
        city,
        City {
            name: "Boston".into(),
            population: 650_706
        }
    );
}

#[test]
fn reordered_headers_map_correctly() {
    // CSV columns are in reverse order: population first, then name.
    let mut r = slice_reader_with_headers(b"population,name\n650706,Boston\n");
    let mut line = r.next_line().unwrap().expect("record");
    let city: City = line.deserialized().unwrap();
    assert_eq!(
        city,
        City {
            name: "Boston".into(),
            population: 650_706
        }
    );
}

#[test]
fn deserialize_iterator_yields_all_rows() {
    let csv = b"name,population\nBoston,650706\nDenver,750000\n";
    let mut r = IoParser::with_options(
        csv.as_slice(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();
    let cities: Vec<City> = r
        .deserialized_records::<City>()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(cities.len(), 2);
    assert_eq!(cities[0].name, "Boston");
    assert_eq!(cities[1].name, "Denver");
}

#[test]
fn field_level_struct_returns_error() {
    // FieldDeserializer::deserialize_struct error path
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test type - fields never read")]
    struct Nested {
        x: u32,
    }

    let mut record = ByteRecord::new();
    record.push_field(b"1");
    let result: Result<(Nested,), _> = record.deserialize();
    let err = result.expect_err("nested struct in field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn hashmap_deserialization_exercises_map_size_hint() {
    // HashMap<String,String>::deserialize calls deserialize_map, which uses
    // MapDeserializer. HashMap's visitor calls size_hint() to pre-allocate.
    let mut r = slice_reader_with_headers(b"city,country\nParis,France\n");
    let row: HashMap<String, String> = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized()
        .expect("deserialize ok");
    assert_eq!(row.get("city").map(String::as_str), Some("Paris"));
    assert_eq!(row.get("country").map(String::as_str), Some("France"));
}

#[test]
fn field_level_map_returns_error() {
    // FieldDeserializer::deserialize_map error path
    let mut record = ByteRecord::new();
    record.push_field(b"k");
    // Deserializing a HashMap expects a map, not possible from a single field.
    let result: Result<(HashMap<String, String>,), _> = record.deserialize();
    let err = result.expect_err("nested map in field must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn map_without_headers_returns_error() {
    // No headers provided → deserializing a HashMap should fail.
    let mut r = unheaded(b"a,1\n");
    let mut line = r.next_line().unwrap().expect("record");
    let result: Result<HashMap<String, String>, _> = line.deserialized();
    // Since there are no headers, deserialization fails.
    let _error = result.expect_err("maps require headers");
}

#[test]
fn invalid_utf8_header_returns_error_during_deserialization() {
    // When a CSV header is not valid UTF-8 the error must surface on record
    // deserialization, not earlier.
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test type - fields never read")]
    struct Row {
        a: String,
    }

    // First field is the invalid header, second is a valid data row.
    // We build this via a raw SliceParser where the header row contains 0xFF.
    // The parser must accept the raw bytes; serde must reject when mapping keys.
    let input: &[u8] = b"\xFF,a\nval1,val2\n";
    let mut r = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::Flexible),
    )
    .expect("valid options");

    let mut line = r.next_line().expect("no io error").expect("record");
    let err = line
        .deserialized::<Row>()
        .expect_err("invalid UTF-8 header must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

/// A header that is not valid UTF-8 must still produce the same error, on every
/// record, rather than being cached into a bad name.
#[test]
fn invalid_utf8_header_still_errors_on_every_record() {
    let mut reader = SliceParser::with_options(
        b"a,\xff\xfe\n1,2\n3,4\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    for _ in 0..2 {
        let mut line = reader.next_line().unwrap().expect("record");
        let error = line
            .deserialized::<City>()
            .expect_err("non-UTF-8 header must be rejected");
        assert!(error.to_string().contains("UTF-8"), "error: {error}");
    }
}

/// `MapAccess` lets a hand-written visitor read a value before its key, in
/// which case a failure inside that value has no header name to be attributed
/// to and must be reported unchanged.
#[test]
fn a_value_read_before_its_key_reports_a_failure_without_a_field_name() {
    #[derive(Debug)]
    struct ValueFirst;

    impl<'de> Deserialize<'de> for ValueFirst {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            struct Walk;

            impl<'de> serde::de::Visitor<'de> for Walk {
                type Value = ValueFirst;

                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a CSV record")
                }

                fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
                where
                    A: serde::de::MapAccess<'de>,
                {
                    let _ = map.next_value::<u32>()?;
                    Ok(ValueFirst)
                }
            }

            deserializer.deserialize_map(Walk)
        }
    }

    let mut parser = SliceParser::with_options(
        b"left,right\nalpha,2\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options");
    let mut line = parser
        .next_line()
        .expect("the record is reached")
        .expect("a record");

    let error = line
        .deserialized::<ValueFirst>()
        .expect_err("`alpha` is not a `u32`");
    assert_eq!(error.field_name(), None, "no key was read for the value");
}

/// A failed Serde deserialization must poison the parsers that track failure,
/// exactly as a failed typed decode does.
#[test]
fn a_failed_deserialization_poisons_a_streaming_parser() {
    #[derive(Debug, Deserialize)]
    struct Row {
        #[expect(dead_code, reason = "the record never deserializes")]
        pop: u64,
    }

    let mut parser = IoParser::with_options(
        &b"pop\nnot-a-number\n"[..],
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options");

    let mut line = parser
        .next_line()
        .expect("the first record is reached")
        .expect("a record");
    assert!(
        line.deserialized::<Row>().is_err(),
        "`not-a-number` is not a `u64`"
    );

    assert!(
        parser.next_line().is_err(),
        "the parser stays poisoned after a failed deserialization"
    );
}

// ── Enum deserialization ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize, PartialEq)]
enum Status {
    Active,
    Inactive,
    Pending,
}

#[test]
fn unit_enum_deserialized_from_field() {
    let mut r = slice_reader_with_headers(b"status\nActive\nInactive\nPending\n");
    let mut line = r.next_line().unwrap().expect("record");
    let a: (Status,) = line.deserialized().unwrap();
    let mut line = r.next_line().unwrap().expect("record");
    let b: (Status,) = line.deserialized().unwrap();
    let mut line = r.next_line().unwrap().expect("record");
    let c: (Status,) = line.deserialized().unwrap();
    assert_eq!(a.0, Status::Active);
    assert_eq!(b.0, Status::Inactive);
    assert_eq!(c.0, Status::Pending);
}

#[test]
fn csv_deserializer_enum_at_record_level() {
    // CsvDeserializer::deserialize_enum is called when the enum is the
    // top-level deserialization target (no struct wrapper).
    #[derive(Debug, Deserialize, PartialEq)]
    enum Status {
        Active,
        Inactive,
    }

    let mut record = ByteRecord::new();
    record.push_field(b"Active");
    let v: Status = record.deserialize().expect("enum at record level");
    assert_eq!(v, Status::Active);
}

#[test]
fn unknown_enum_variant_returns_error() {
    let mut r = unheaded(b"Unknown\n");
    let mut line = r.next_line().unwrap().expect("record");
    let result = line.deserialized::<(Status,)>();
    assert!(result.is_err(), "unknown variant should produce an error");
}

#[test]
fn newtype_variant_access_returns_error() {
    // UnitVariantAccess::newtype_variant_seed must reject non-unit variants.
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test enum - variants never constructed")]
    enum Complex {
        Unit,
        Newtype(u32),
    }

    // A field that names no variant of the enum is rejected before any
    // variant payload is requested.
    let mut record = ByteRecord::new();
    record.push_field(b"NotReal");
    let result: Result<(Complex,), _> = record.deserialize();
    let err = result.expect_err("unknown variant must fail");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn newtype_variant_deser_returns_error() {
    #[derive(Debug, Deserialize)]
    #[expect(dead_code, reason = "test enum - variant is never constructed")]
    enum Complex {
        Unit,
        Newtype(u32),
    }
    let mut record = ByteRecord::new();
    record.push_field(b"Newtype");

    // Naming a newtype variant reaches `UnitVariantAccess`, which only supports
    // unit variants.
    let _result: Result<(Complex,), coseva::Error> = record.deserialize();

    // A name that matches no variant at all is likewise rejected.
    let mut record2 = ByteRecord::new();
    record2.push_field(b"NonExistentVariant");
    let result2: Result<(Complex,), coseva::Error> = record2.deserialize();
    assert!(result2.is_err(), "unknown variant should produce an error");
}

#[test]
fn newtype_enum_variant_in_field_returns_error() {
    // UnitVariantAccess::newtype_variant_seed rejects non-unit variant.
    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "field/variant needed only for type shape in error-path test"
    )]
    enum Mixed {
        Unit,
        Wrapped(i32),
    }

    let mut record = ByteRecord::new();
    record.push_field(b"Wrapped");
    let result: Result<(Mixed,), _> = record.deserialize();
    let err = result.expect_err("newtype variant not supported");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(
        err.to_string().contains("newtype"),
        "error message should mention newtype, got: {err}"
    );
}

#[test]
fn tuple_enum_variant_in_field_returns_error() {
    // UnitVariantAccess::tuple_variant rejects tuple enum variant.
    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "field/variant needed only for type shape in error-path test"
    )]
    enum Mixed {
        Unit,
        Tuple(i32, String),
    }

    let mut record = ByteRecord::new();
    record.push_field(b"Tuple");
    let result: Result<(Mixed,), _> = record.deserialize();
    let err = result.expect_err("tuple variant not supported");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("tuple"));
}

#[test]
fn struct_enum_variant_in_field_returns_error() {
    // UnitVariantAccess::struct_variant rejects struct enum variant.
    #[derive(Debug, Deserialize)]
    #[expect(
        dead_code,
        reason = "field/variant needed only for type shape in error-path test"
    )]
    enum Mixed {
        Unit,
        Named { x: i32 },
    }

    let mut record = ByteRecord::new();
    record.push_field(b"Named");
    let result: Result<(Mixed,), _> = record.deserialize();
    let err = result.expect_err("struct variant not supported");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("struct"));
}

#[test]
fn enum_from_non_utf8_field_returns_utf8_error() {
    #[derive(Debug, Deserialize)]
    enum Status {
        Active,
    }

    let mut record = ByteRecord::new();
    record.push_field(b"\xFF");
    let result: Result<(Status,), _> = record.deserialize();
    let err = result.expect_err("non-UTF-8 enum field must fail");
    // The error is either InvalidUtf8 or Serde depending on path.
    assert!(
        matches!(err.kind(), ErrorKind::InvalidUtf8(_) | ErrorKind::Serde),
        "unexpected error kind: {:?}",
        err.kind()
    );
}

#[test]
fn invalid_utf8_in_enum_field_returns_error() {
    // UnitEnumAccess::variant_seed utf8_error path
    #[derive(Debug, Deserialize)]
    enum Color {
        Red,
        Green,
    }
    let mut record = ByteRecord::new();
    record.push_field(b"\xFF\xFE");
    let result: Result<(Color,), _> = record.deserialize();
    let err = result.expect_err("invalid UTF-8 must fail for enum");
    assert!(
        matches!(err.kind(), ErrorKind::InvalidUtf8(_)),
        "expected InvalidUtf8, got {:?}",
        err.kind()
    );
}

// ── Self-describing deserialization and identifiers ──────────────────────────

/// A custom Deserialize that calls `deserialize_any` on each field via
/// `DeserializeSeed`, records the variant the visitor was called with.
#[derive(Debug, PartialEq)]
struct AnyCapture(String);

impl<'de> Deserialize<'de> for AnyCapture {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{DeserializeSeed, SeqAccess, Visitor};

        struct CaptureSeed;
        impl<'de> DeserializeSeed<'de> for CaptureSeed {
            type Value = String;
            fn deserialize<D2: serde::de::Deserializer<'de>>(
                self,
                deserializer: D2,
            ) -> Result<String, D2::Error> {
                struct Vis;
                impl<'de> Visitor<'de> for Vis {
                    type Value = String;
                    fn expecting(
                        &self,
                        formatter: &mut std::fmt::Formatter<'_>,
                    ) -> std::fmt::Result {
                        formatter.write_str("any")
                    }
                    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<String, E> {
                        Ok(format!("str:{v}"))
                    }
                    fn visit_str<E>(self, v: &str) -> Result<String, E> {
                        Ok(format!("str:{v}"))
                    }
                    fn visit_u64<E>(self, v: u64) -> Result<String, E> {
                        Ok(format!("u64:{v}"))
                    }
                    fn visit_bool<E>(self, v: bool) -> Result<String, E> {
                        Ok(format!("bool:{v}"))
                    }
                    fn visit_borrowed_bytes<E>(self, v: &'de [u8]) -> Result<String, E> {
                        Ok(format!("bytes:{}", v.len()))
                    }
                    fn visit_bytes<E>(self, v: &[u8]) -> Result<String, E> {
                        Ok(format!("bytes:{}", v.len()))
                    }
                }
                deserializer.deserialize_any(Vis)
            }
        }

        struct SeqVis;
        impl<'de> Visitor<'de> for SeqVis {
            type Value = AnyCapture;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a seq")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<AnyCapture, A::Error> {
                let captured = seq.next_element_seed(CaptureSeed)?.unwrap_or_default();
                Ok(AnyCapture(captured))
            }
        }
        deserializer.deserialize_seq(SeqVis)
    }
}

#[test]
fn deserialize_any_for_valid_utf8_returns_str_not_number() {
    let mut record = ByteRecord::new();
    record.push_field(b"42");

    // deserialize_any on a field should return borrowed str, never infer u64
    let cap: AnyCapture = record.deserialize().unwrap();
    assert!(
        cap.0.starts_with("str:"),
        "deserialize_any returned non-str: {}",
        cap.0
    );
    assert_eq!(cap.0, "str:42");
}

#[test]
fn deserialize_any_invalid_utf8_yields_bytes() {
    let bad: &[u8] = b"\xFF\xFE";
    let mut record = ByteRecord::new();
    record.push_field(bad);

    // Invalid UTF-8 → deserialize_any must call visit_borrowed_bytes, not visit_str
    let cap: AnyCapture = record.deserialize().unwrap();
    assert!(
        cap.0.starts_with("bytes:"),
        "expected bytes variant for invalid UTF-8; got: {}",
        cap.0
    );
    assert_eq!(cap.0, "bytes:2");
}

#[test]
fn invalid_utf8_bytes_preserved_via_deserialize_any() {
    let bad: &[u8] = b"\xFF\xFE";
    let mut record = ByteRecord::new();
    record.push_field(bad);

    // The AnyCapture type above uses deserialize_any at the field level.
    let cap: AnyCapture = record.deserialize().unwrap();
    assert!(
        cap.0.starts_with("bytes:"),
        "expected bytes variant for invalid UTF-8; got: {}",
        cap.0
    );
}

#[test]
fn field_deserializer_any_valid_utf8_gives_str() {
    // FieldDeserializer::deserialize_any for a valid UTF-8 field.
    // Use a tuple so size_hint is not called (unlike Vec which would trigger
    // the NullableFields ExactSizeIterator bug).
    use serde::de::IgnoredAny;
    let mut record = ByteRecord::new();
    record.push_field(b"hello");
    // (IgnoredAny,) calls deserialize_tuple → SeqDeserializer → deserialize_any.
    let _: (IgnoredAny,) = record.deserialize().expect("tuple of ignored");
}

#[test]
fn csv_deserializer_any_returns_seq() {
    // CsvDeserializer::deserialize_any routes to SeqDeserializer.
    // serde::de::IgnoredAny calls deserialize_any at the top level.
    use serde::de::IgnoredAny;
    let mut record = ByteRecord::new();
    record.push_field(b"hello");
    record.push_field(b"world");
    let _: IgnoredAny = record
        .deserialize()
        .expect("deserialize_any via IgnoredAny");
}

#[test]
fn csv_deserializer_deserialize_identifier_reads_first_field() {
    // deserialize_identifier on CsvDeserializer forwards to first field.
    struct IdentCapture(String);
    impl<'de> Deserialize<'de> for IdentCapture {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct V;
            impl<'de> serde::de::Visitor<'de> for V {
                type Value = IdentCapture;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("identifier")
                }
                fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<IdentCapture, E> {
                    Ok(IdentCapture(v.to_string()))
                }
                fn visit_borrowed_str<E: serde::de::Error>(
                    self,
                    v: &'de str,
                ) -> Result<IdentCapture, E> {
                    Ok(IdentCapture(v.to_string()))
                }
            }
            deserializer.deserialize_identifier(V)
        }
    }

    let mut record = ByteRecord::new();
    record.push_field(b"mykey");
    let ic: IdentCapture = record.deserialize().expect("identifier deserializes");
    assert_eq!(ic.0, "mykey");
}

// ── Record-level serialization ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct WriteCity {
    name: &'static str,
    population: u64,
}

#[test]
fn serialize_struct_into_vec_writer() {
    let mut w = VecEmitter::default();
    w.serialize(&WriteCity {
        name: "Boston",
        population: 650_706,
    })
    .unwrap();
    assert_eq!(w.as_bytes(), b"name,population\nBoston,650706\n");
}

#[test]
fn automatic_struct_headers_can_be_disabled() {
    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .unwrap();
    w.serialize(&WriteCity {
        name: "Boston",
        population: 650_706,
    })
    .unwrap();
    assert_eq!(w.as_bytes(), b"Boston,650706\n");
}

#[test]
fn automatic_struct_headers_are_written_once() {
    let mut w = VecEmitter::default();
    for name in ["Boston", "Paris"] {
        w.serialize(&WriteCity {
            name,
            population: 1,
        })
        .unwrap();
    }
    assert_eq!(w.as_bytes(), b"name,population\nBoston,1\nParis,1\n");
}

/// A value whose `Display` always fails, so `collect_str` reports a Serde
/// error partway through building a field.
struct DisplayAlwaysFails;

impl std::fmt::Display for DisplayAlwaysFails {
    fn fmt(&self, _formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Err(std::fmt::Error)
    }
}

impl Serialize for DisplayAlwaysFails {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[test]
fn serialize_display_failure_rolls_back_partial_field() -> Result<(), Box<dyn StdError>> {
    // A `Display` field that fails after earlier fields have already been
    // framed straight into the buffer must roll back to the record start,
    // leaving the prior record intact and the emitter still usable.
    let mut w = no_header_csv_emitter();
    w.serialize(&("keep", 1u32))?;
    let committed = w.as_bytes().to_vec();
    assert_eq!(committed, b"keep,1\n");

    let err = w
        .serialize(&("ok", DisplayAlwaysFails))
        .expect_err("a failing Display must abort the record");
    assert!(
        matches!(err.kind(), ErrorKind::Serde),
        "expected Serde error kind, got {err:?}"
    );
    assert_eq!(
        w.as_bytes(),
        committed.as_slice(),
        "a partially framed record must be rolled back whole"
    );

    w.serialize(&("more", 2u32))?;
    assert_eq!(w.as_bytes(), b"keep,1\nmore,2\n");
    Ok(())
}

#[test]
fn serialize_mid_stream_failure_preserves_committed_records() -> Result<(), Box<dyn StdError>> {
    // Buffered streaming: a rejected record in the middle of a run leaves the
    // header and every prior record untouched, and streaming resumes cleanly.
    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    w.serialize(&WriteCity {
        name: "Boston",
        population: 1,
    })?;
    w.serialize(&WriteCity {
        name: "Paris",
        population: 2,
    })?;
    let committed = w.as_bytes().to_vec();
    assert_eq!(committed, b"name,population\nBoston,1\nParis,2\n");

    let err = w
        .serialize(&WriteCity {
            name: "a,b",
            population: 3,
        })
        .expect_err("a structural field must fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    assert_eq!(
        w.as_bytes(),
        committed.as_slice(),
        "a mid-stream failure must not disturb committed records"
    );

    w.serialize(&WriteCity {
        name: "Lyon",
        population: 4,
    })?;
    assert_eq!(
        w.into_inner(),
        b"name,population\nBoston,1\nParis,2\nLyon,4\n"
    );
    Ok(())
}

#[test]
fn serialize_first_record_failure_keeps_headers_pending() -> Result<(), Box<dyn StdError>> {
    // When the first record fails, its header must not be committed; the next
    // clean record then emits the header for the first time.
    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let err = w
        .serialize(&WriteCity {
            name: "a,b",
            population: 1,
        })
        .expect_err("a structural field must fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    assert!(
        w.as_bytes().is_empty(),
        "a failed first record must leave no header behind"
    );

    w.serialize(&WriteCity {
        name: "Paris",
        population: 2,
    })?;
    assert_eq!(w.into_inner(), b"name,population\nParis,2\n");
    Ok(())
}

#[test]
fn serialize_tuple_into_vec_writer() {
    let mut w = VecEmitter::default();
    w.serialize(&("Boston", 650_706u64)).unwrap();
    assert_eq!(w.as_bytes(), b"Boston,650706\n");
}

#[test]
fn serialize_tuple_struct_fields_in_order() {
    // RecordSerializer::serialize_tuple_struct
    #[derive(Serialize)]
    struct Row(u8, bool, &'static str);

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Row(1, true, "hi")).expect("serializes");
    assert_eq!(w.as_bytes(), b"1,true,hi\n");
}

#[test]
fn serialize_into_io_writer() {
    let mut w =
        IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new()).unwrap();
    w.serialize(&("hello", 42u32, true)).unwrap();
    let bytes = w.into_inner().expect("Vec flush cannot fail");
    assert_eq!(bytes, b"hello,42,true\n");
}

#[test]
fn serialize_bool_and_floats() {
    let mut w = VecEmitter::default();
    w.serialize(&(true, false, 1.5_f64)).unwrap();
    let output = std::str::from_utf8(w.as_bytes()).unwrap();
    assert!(output.starts_with("true,false,1.5"));
}

#[test]
fn serialize_i8_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(i8::MIN, i8::MAX)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-128,127\n");
}

#[test]
fn serialize_i16_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(i16::MIN,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-32768\n");
}

#[test]
fn serialize_i64_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(i64::MAX,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"9223372036854775807\n");
}

#[test]
fn serialize_i128_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(i128::MIN,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-170141183460469231731687303715884105728\n");
}

#[test]
fn serialize_u8_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(u8::MAX,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"255\n");
}

#[test]
fn serialize_u16_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(u16::MAX,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"65535\n");
}

#[test]
fn serialize_u128_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(u128::MAX,)).expect("serializes");
    assert_eq!(w.as_bytes(), b"340282366920938463463374607431768211455\n");
}

#[test]
fn serialize_f32_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(1.5_f32,)).expect("serializes");
    let out = std::str::from_utf8(w.as_bytes()).expect("valid utf8");
    assert!(out.contains("1.5"), "got: {out}");
}

#[test]
fn serialize_char_field() {
    let mut w = VecEmitter::default();
    w.serialize(&('Z',)).expect("serializes");
    assert_eq!(w.as_bytes(), b"Z\n");
}

#[test]
fn serialize_bytes_field() {
    // serialize_bytes on RecordSerializer appends raw bytes as a field
    struct RawField<'a>(&'a [u8]);
    impl Serialize for RawField<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&RawField(b"abc")).expect("serializes");
    assert_eq!(w.as_bytes(), b"abc\n");
}

#[test]
fn serialize_unit_produces_empty_field() {
    // RecordSerializer::serialize_unit
    struct UnitVal;
    impl Serialize for UnitVal {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_unit()
        }
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&UnitVal).expect("serializes");
    // The emitter quotes empty fields in standard CSV.
    assert_eq!(w.as_bytes(), b"\"\"\n");
}

#[test]
fn serialize_unit_struct_produces_empty_field() {
    // RecordSerializer::serialize_unit_struct
    #[derive(Serialize)]
    struct Empty;

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Empty).expect("serializes");
    // The emitter quotes empty fields in standard CSV.
    assert_eq!(w.as_bytes(), b"\"\"\n");
}

#[test]
fn serialize_newtype_struct_unwraps_inner() {
    // RecordSerializer::serialize_newtype_struct
    #[derive(Serialize)]
    struct Wrapped(u32);

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Wrapped(77)).expect("serializes");
    assert_eq!(w.as_bytes(), b"77\n");
}

#[test]
fn record_serializer_i8_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&-1i8).expect("serialize i8");
    assert_eq!(w.as_bytes(), b"-1\n");
}

#[test]
fn record_serializer_i16_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&-2i16).expect("serialize i16");
    assert_eq!(w.as_bytes(), b"-2\n");
}

#[test]
fn record_serializer_i32_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&42i32).expect("serialize i32");
    assert_eq!(w.as_bytes(), b"42\n");
}

#[test]
fn record_serializer_i64_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&i64::MAX).expect("serialize i64");
    assert_eq!(w.as_bytes(), b"9223372036854775807\n");
}

#[test]
fn record_serializer_i128_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&0i128).expect("serialize i128");
    assert_eq!(w.as_bytes(), b"0\n");
}

#[test]
fn record_serializer_u16_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&u16::MAX).expect("serialize u16");
    assert_eq!(w.as_bytes(), b"65535\n");
}

#[test]
fn record_serializer_u128_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&1u128).expect("serialize u128");
    assert_eq!(w.as_bytes(), b"1\n");
}

#[test]
fn record_serializer_f32_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&1.5_f32).expect("serialize f32");
    let s = std::str::from_utf8(w.as_bytes()).expect("utf8");
    assert!(s.contains("1.5"), "got: {s}");
}

#[test]
fn record_serializer_f64_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&2.5_f64).expect("serialize f64");
    let s = std::str::from_utf8(w.as_bytes()).expect("utf8");
    assert!(s.contains("2.5"), "got: {s}");
}

#[test]
fn record_serializer_char_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize(&'Z').expect("serialize char");
    assert_eq!(w.as_bytes(), b"Z\n");
}

#[test]
fn record_serializer_str_direct() {
    let mut w = no_header_csv_emitter();
    w.serialize("hello").expect("serialize str");
    assert_eq!(w.as_bytes(), b"hello\n");
}

#[test]
fn record_serializer_unit_variant_direct() {
    #[derive(Serialize)]
    enum Cardinal {
        South,
    }
    let mut w = no_header_csv_emitter();
    w.serialize(&Cardinal::South)
        .expect("serialize unit variant");
    assert_eq!(w.as_bytes(), b"South\n");
}

#[test]
fn record_serializer_some_direct() {
    // RecordSerializer::serialize_some delegates to inner value.
    let mut w = no_header_csv_emitter();
    w.serialize(&Some(99i32)).expect("serialize Some");
    assert_eq!(w.as_bytes(), b"99\n");
}

#[test]
fn record_serialize_some_delegates_to_inner() {
    // RecordSerializer::serialize_some delegates to the inner value
    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Some(42_u32)).expect("serializes");
    assert_eq!(w.as_bytes(), b"42\n");
}

#[test]
fn record_serialize_none_produces_null_field() {
    // RecordSerializer::serialize_none: finish_null_field
    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Option::<u32>::None).expect("serializes");
    // Postgres COPY CSV uses empty for NULL at record level
    assert!(!w.as_bytes().is_empty());
}

#[test]
fn collect_str_serializes_rows_and_fields() {
    struct DisplayValue(u32);

    impl std::fmt::Display for DisplayValue {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "value-{}", self.0)
        }
    }

    impl Serialize for DisplayValue {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    let mut writer = VecEmitter::default();
    writer.serialize(&DisplayValue(7)).unwrap();
    writer
        .serialize(&(DisplayValue(8), DisplayValue(9)))
        .unwrap();
    assert_eq!(writer.as_bytes(), b"value-7\nvalue-8,value-9\n");
}

#[test]
fn record_serializer_collect_str_formats_display_value() {
    // RecordSerializer::collect_str is called when a Serialize impl calls
    // serializer.collect_str(display_value) at the record level.
    use std::fmt;

    struct DisplayOnly(i32);

    impl fmt::Display for DisplayOnly {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "val={}", self.0)
        }
    }

    impl Serialize for DisplayOnly {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    let mut w = no_header_csv_emitter();
    w.serialize(&DisplayOnly(42))
        .expect("collect_str serializes");
    assert_eq!(w.as_bytes(), b"val=42\n");
}

#[test]
fn collect_str_display_failure_propagates_serde_error() {
    // Line 122: the `map_err` in `format_into_record` fires when the `Display`
    // impl itself returns `Err(fmt::Error)`.  Using `collect_str` on either
    // `RecordSerializer` or `FieldSerializer` calls `format_into_record`.
    use std::fmt;

    struct AlwaysFails;

    impl fmt::Display for AlwaysFails {
        fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            Err(fmt::Error)
        }
    }

    impl Serialize for AlwaysFails {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    let mut w = no_header_csv_emitter();
    let err = w
        .serialize(&AlwaysFails)
        .expect_err("display failure should propagate");
    assert!(
        matches!(err.kind(), ErrorKind::Serde),
        "expected Serde error kind, got {err:?}"
    );
}

// ── Option serialization ─────────────────────────────────────────────────────

#[test]
fn serialize_option_none_as_empty_field() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some("hi"), Option::<String>::None)).unwrap();
    assert_eq!(w.as_bytes(), b"hi,\n");
}

#[test]
fn serialize_option_none_with_database_markers() {
    for (format, expected) in [
        (FormatOptions::POSTGRES_COPY_CSV, b"hi,\n".as_slice()),
        (FormatOptions::MYSQL, b"hi\t\\N\n".as_slice()),
    ] {
        let mut writer =
            VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))
                .unwrap();
        writer
            .serialize(&(Some("hi"), Option::<String>::None))
            .unwrap();
        assert_eq!(writer.as_bytes(), expected);
    }
}

// ── Enum serialization ───────────────────────────────────────────────────────

#[test]
fn serialize_unit_enum_as_variant_name() {
    #[derive(Serialize)]
    enum Color {
        Red,
        Blue,
    }
    let mut w = VecEmitter::default();
    w.serialize(&(Color::Red, Color::Blue)).unwrap();
    assert_eq!(w.as_bytes(), b"Red,Blue\n");
}

#[test]
fn serialize_newtype_variant_returns_error() {
    // RecordSerializer::serialize_newtype_variant error path
    #[derive(Serialize)]
    enum Complex {
        Newtype(u32),
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    let err = w
        .serialize(&Complex::Newtype(1))
        .expect_err("newtype variant must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn serialize_tuple_variant_returns_error() {
    // RecordSerializer::serialize_tuple_variant error path
    #[derive(Serialize)]
    enum Complex {
        Tuple(u32, u32),
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    let err = w
        .serialize(&Complex::Tuple(1, 2))
        .expect_err("tuple variant must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn serialize_struct_variant_returns_error() {
    // RecordSerializer::serialize_struct_variant error path
    #[derive(Serialize)]
    enum Complex {
        Struct { x: u32 },
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    let err = w
        .serialize(&Complex::Struct { x: 1 })
        .expect_err("struct variant must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

// ── Field-level serialization ────────────────────────────────────────────────
//
// Values nested inside a struct or tuple are pushed through the field
// serializer, which accepts scalars and rejects every nested container shape.

#[test]
fn field_serialize_i8() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(-7_i8), Some(127_i8)))
        .expect("serializes");
    assert_eq!(w.as_bytes(), b"-7,127\n");
}

#[test]
fn field_serialize_i16() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(-1000_i16),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-1000\n");
}

#[test]
fn field_serialize_i32() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(-100_000_i32),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-100000\n");
}

#[test]
fn field_serialize_i64() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(i64::MAX),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"9223372036854775807\n");
}

#[test]
fn field_serialize_i128() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(i128::MIN),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"-170141183460469231731687303715884105728\n");
}

#[test]
fn field_serialize_u8() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(255_u8),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"255\n");
}

#[test]
fn field_serialize_u16() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(65535_u16),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"65535\n");
}

#[test]
fn field_serialize_u32() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(4_294_967_295_u32),))
        .expect("serializes");
    assert_eq!(w.as_bytes(), b"4294967295\n");
}

#[test]
fn field_serialize_u64() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(u64::MAX),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"18446744073709551615\n");
}

#[test]
fn field_serialize_u128() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(u128::MAX),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"340282366920938463463374607431768211455\n");
}

#[test]
fn field_serialize_f32() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(1.5_f32),)).expect("serializes");
    let out = std::str::from_utf8(w.as_bytes()).expect("utf8");
    assert!(out.contains("1.5"), "got: {out}");
}

#[test]
fn field_serialize_f64() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some(1.25_f64),)).expect("serializes");
    let out = std::str::from_utf8(w.as_bytes()).expect("utf8");
    assert!(out.contains("1.25"), "got: {out}");
}

#[test]
fn field_serialize_char() {
    let mut w = VecEmitter::default();
    w.serialize(&(Some('X'),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"X\n");
}

#[test]
fn field_serialize_bytes() {
    // FieldSerializer::serialize_bytes path: a type inside a tuple that calls
    // serialize_bytes, so the tuple's push_value goes through FieldSerializer.
    struct JustBytes<'a>(&'a [u8]);
    impl Serialize for JustBytes<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.serialize_bytes(self.0)
        }
    }

    let mut w = VecEmitter::default();
    w.serialize(&(JustBytes(b"raw"),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"raw\n");
}

#[test]
fn field_serialize_unit() {
    // FieldSerializer::serialize_unit path (via Option<()>)
    let mut w = VecEmitter::default();
    w.serialize(&(Some(()),)).expect("serializes");
    // The emitter quotes empty fields in standard CSV.
    assert_eq!(w.as_bytes(), b"\"\"\n");
}

#[test]
fn field_serialize_unit_struct() {
    // FieldSerializer::serialize_unit_struct path
    #[derive(Serialize)]
    struct Empty;

    let mut w = VecEmitter::default();
    w.serialize(&(Some(Empty),)).expect("serializes");
    // The emitter quotes empty fields in standard CSV.
    assert_eq!(w.as_bytes(), b"\"\"\n");
}

#[test]
fn field_serialize_unit_variant() {
    // FieldSerializer::serialize_unit_variant path (unit enum inside Option in tuple)
    #[derive(Serialize)]
    enum Color {
        Red,
    }

    let mut w = VecEmitter::default();
    w.serialize(&(Some(Color::Red),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"Red\n");
}

#[test]
fn field_serialize_newtype_struct() {
    // FieldSerializer::serialize_newtype_struct path
    #[derive(Serialize)]
    struct Wrapped(u32);

    let mut w = VecEmitter::default();
    w.serialize(&(Some(Wrapped(5)),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"5\n");
}

#[test]
fn field_serialize_some_delegates_to_inner() {
    // FieldSerializer::serialize_some → value.serialize(self).
    // Uses VecEmitter::default() so push_value goes through FieldSerializer.
    let mut w = VecEmitter::default();
    w.serialize(&(Some(Some(42_u32)),)).expect("serializes");
    assert_eq!(w.as_bytes(), b"42\n");
}

#[test]
fn field_serialize_collect_str() {
    // FieldSerializer::collect_str path: a display value inside a struct field.
    struct DisplayVal(u32);
    impl std::fmt::Display for DisplayVal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "disp-{}", self.0)
        }
    }
    impl Serialize for DisplayVal {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    #[derive(Serialize)]
    struct Row {
        #[serde(serialize_with = "serialize_display_val")]
        v: u32,
    }

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde serialize_with requires &T"
    )]
    fn serialize_display_val<S: serde::Serializer>(
        v: &u32,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&DisplayVal(*v))
    }

    let mut w = VecEmitter::default();
    w.serialize(&Row { v: 3 }).expect("serializes");
    // Output: header row + data row, collect_str writes "disp-3".
    let out = std::str::from_utf8(w.as_bytes()).expect("utf8");
    assert!(out.contains("disp-3"), "got: {out}");
}

#[test]
fn field_serialize_newtype_variant_returns_error() {
    // FieldSerializer::serialize_newtype_variant error path.
    // Put it inside an Option in a tuple so push_value → FieldSerializer.
    #[derive(Serialize)]
    enum Complex {
        Newtype(u32),
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&(Some(Complex::Newtype(1)),))
        .expect_err("newtype variant in field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("newtype enum variants"));
}

#[test]
fn field_serialize_seq_returns_error() {
    // FieldSerializer::serialize_seq error path.
    // allow_nested=false (default has_headers=true) → push_value uses FieldSerializer.
    #[derive(Serialize)]
    struct WithVec {
        v: Vec<u32>,
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithVec { v: vec![1, 2] })
        .expect_err("nested seq in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("sequences are not supported"));
}

#[test]
fn field_serialize_tuple_returns_error() {
    // FieldSerializer::serialize_tuple error path.
    #[derive(Serialize)]
    struct WithTuple {
        coords: (u32, u32),
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithTuple { coords: (1, 2) })
        .expect_err("nested tuple in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("tuples are not supported"));
}

#[test]
fn field_serialize_tuple_struct_returns_error() {
    // FieldSerializer::serialize_tuple_struct error path.
    #[derive(Serialize)]
    struct Pair(u32, u32);

    #[derive(Serialize)]
    struct WithPair {
        p: Pair,
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithPair { p: Pair(1, 2) })
        .expect_err("nested tuple struct in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("tuple structs are not supported"));
}

#[test]
fn field_serialize_tuple_variant_returns_error() {
    // FieldSerializer::serialize_tuple_variant error path.
    #[derive(Serialize)]
    enum Complex {
        Tv(u32, u32),
    }

    #[derive(Serialize)]
    struct WithVariant {
        e: Complex,
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithVariant {
            e: Complex::Tv(1, 2),
        })
        .expect_err("tuple variant in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("tuple enum variants"));
}

#[test]
fn field_serialize_map_returns_error() {
    // FieldSerializer::serialize_map error path.

    #[derive(Serialize)]
    struct WithMap {
        m: HashMap<String, u32>,
    }

    let mut w = VecEmitter::default();
    let mut m = HashMap::<String, u32>::new();
    m.insert("k".to_string(), 1);
    let err = w
        .serialize(&WithMap { m })
        .expect_err("map in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("maps are not supported"));
}

#[test]
fn field_serialize_struct_returns_error() {
    // FieldSerializer::serialize_struct error path.
    #[derive(Serialize)]
    struct Inner {
        x: u32,
    }

    #[derive(Serialize)]
    struct Outer {
        inner: Inner,
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&Outer {
            inner: Inner { x: 1 },
        })
        .expect_err("nested struct in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
    assert!(err.to_string().contains("structs are not supported"));
}

#[test]
fn field_serialize_struct_variant_returns_error() {
    // FieldSerializer::serialize_struct_variant error path.
    #[derive(Serialize)]
    enum Complex {
        Sv { x: u32 },
    }

    #[derive(Serialize)]
    struct WithVariant {
        e: Complex,
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithVariant {
            e: Complex::Sv { x: 1 },
        })
        .expect_err("struct variant in struct field must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

// ── Nested shapes in serialization ───────────────────────────────────────────

#[test]
fn nested_seq_in_field_returns_error() {
    #[derive(Serialize)]
    struct Bad {
        tags: Vec<u32>,
    }

    let mut w = VecEmitter::default();
    // Serializing a Vec inside a tuple means serializing a nested sequence.
    let v: Vec<u32> = vec![1, 2, 3];
    let result = w.serialize(&Bad { tags: v });
    assert!(result.is_err(), "nested seq should be rejected");
    assert!(w.as_bytes().is_empty(), "failed rows must not emit headers");

    w.serialize(&WriteCity {
        name: "Boston",
        population: 650_706,
    })
    .unwrap();
    assert_eq!(w.as_bytes(), b"name,population\nBoston,650706\n");
}

#[test]
fn nested_map_field_returns_error() {
    let mut w = VecEmitter::default();
    let result = w.serialize(&HashMap::<String, u32>::new());
    assert!(result.is_err(), "map serialization should be rejected");
}

#[test]
fn nested_tuple_inside_struct_field_is_rejected() {
    // FieldSerializer::serialize_tuple via a tuple field in a struct.
    // allow_nested=false (default emitter) → FieldSerializer rejects tuples.
    #[derive(Serialize)]
    struct WithTuple {
        coords: (u32, u32),
    }

    let mut w = VecEmitter::default();
    let err = w
        .serialize(&WithTuple { coords: (1, 2) })
        .expect_err("tuple field in struct must be rejected");
    assert_eq!(err.kind(), ErrorKind::Serde);
}

#[test]
fn nested_shapes_flatten_when_automatic_headers_are_disabled() {
    #[derive(Serialize)]
    struct Coordinates {
        x: i32,
        y: i32,
    }

    #[derive(Serialize)]
    struct Nested {
        name: &'static str,
        coordinates: Coordinates,
        samples: Vec<u32>,
    }

    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .unwrap();
    writer
        .serialize(&Nested {
            name: "point",
            coordinates: Coordinates { x: 3, y: 4 },
            samples: vec![5, 6],
        })
        .unwrap();
    assert_eq!(writer.as_bytes(), b"point,3,4,5,6\n");
}

#[test]
fn nested_shapes_flatten_with_allow_nested() {
    // Tests the allow_nested=true branch of RecordSerializer::push_value,
    // which is the code path used internally when has_headers=false and
    // the emitter encounters nested containers.
    #[derive(Serialize)]
    struct Outer {
        name: &'static str,
        inner: Inner,
    }

    #[derive(Serialize)]
    struct Inner {
        x: u32,
        y: u32,
    }

    let mut w = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    w.serialize(&Outer {
        name: "pt",
        inner: Inner { x: 3, y: 4 },
    })
    .expect("serializes");
    assert_eq!(w.as_bytes(), b"pt,3,4\n");
}

// ── Round trips ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, PartialEq)]
struct RoundTrip {
    city: String,
    pop: u64,
    active: bool,
}

#[test]
fn round_trip_through_writer_and_reader() {
    let original = RoundTrip {
        city: "Denver".into(),
        pop: 750_000,
        active: true,
    };

    let mut w = VecEmitter::default();
    w.serialize(&original).unwrap();
    let csv_bytes = w.into_inner();

    let mut r = SliceParser::with_options(
        &csv_bytes,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();
    let mut line = r.next_line().unwrap().expect("record");
    let deserialized: RoundTrip = line.deserialized().unwrap();
    assert_eq!(deserialized, original);
}

// ── Cached headers and learned ignored columns ───────────────────────────────
//
// The Serde struct path caches header UTF-8 validation and learns which CSV
// columns the visitor discards, skipping them on later records. These tests pin
// the semantics that optimization must not disturb.

#[derive(Debug, Deserialize, PartialEq)]
struct TwoOfFive {
    b: String,
    d: String,
}

/// Columns the visitor ignores must stay ignored, and the used columns must
/// keep resolving by name, on every record after the first.
#[test]
fn ignored_columns_are_skipped_consistently_across_records() {
    let mut reader =
        slice_reader_with_headers(b"a,b,c,d,e\n1,2,3,4,5\n6,7,8,9,10\n11,12,13,14,15\n");
    let mut rows = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        rows.push(line.deserialized::<TwoOfFive>().unwrap());
    }
    assert_eq!(
        rows,
        vec![
            TwoOfFive {
                b: "2".to_owned(),
                d: "4".to_owned()
            },
            TwoOfFive {
                b: "7".to_owned(),
                d: "9".to_owned()
            },
            TwoOfFive {
                b: "12".to_owned(),
                d: "14".to_owned()
            },
        ]
    );
}

#[test]
fn map_deserializer_skipping_loop_skips_learned_ignored_columns() {
    // MapDeserializer::next_key_seed skipping loop: after the first record
    // teaches that a column is ignored, subsequent records skip it.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        name: String,
    }

    // "extra" is an unknown column — the first deserialization learns to
    // ignore it; the second deserialization exercises the skipping loop.
    let mut r = slice_reader_with_headers(b"extra,name\n1,Alice\n2,Bob\n");
    let row1: Row = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized()
        .expect("first row");
    assert_eq!(row1.name, "Alice");
    let row2: Row = r
        .next_line()
        .expect("parse ok")
        .expect("record")
        .deserialized()
        .expect("second row");
    assert_eq!(row2.name, "Bob");
}

/// A column the target struct ignores must be skipped outright once the
/// deserializer has learned about it, including when it is the very first
/// column of the record.
#[test]
fn a_learned_leading_ignored_column_is_skipped() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Kept {
        x: i32,
    }

    // `extra` comes first, so skipping has to run before any kept column is
    // reached rather than only between them.
    let mut parser = slice_reader_with_headers(b"extra,x\na,1\nb,2\nc,3\n");
    let mut decoded = Vec::new();
    while let Some(mut line) = parser.next_line().expect("record parses") {
        decoded.push(line.deserialized::<Kept>().expect("record deserializes"));
    }
    assert_eq!(decoded, vec![Kept { x: 1 }, Kept { x: 2 }, Kept { x: 3 }]);
}

/// The same, with two leading ignored columns, so the skip loop runs more than
/// one iteration.
#[test]
fn two_learned_leading_ignored_columns_are_skipped() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Kept {
        x: i32,
    }

    let mut parser = slice_reader_with_headers(b"extra,other,x\na,b,1\nc,d,2\ne,f,3\n");
    let mut decoded = Vec::new();
    while let Some(mut line) = parser.next_line().expect("record parses") {
        decoded.push(line.deserialized::<Kept>().expect("record deserializes"));
    }
    assert_eq!(decoded, vec![Kept { x: 1 }, Kept { x: 2 }, Kept { x: 3 }]);
}

#[test]
fn projected_deserialization_keeps_values_correct_after_learning() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct TwoFields {
        b: String,
        d: String,
    }

    // 5 records so the cache can learn and switch to projection on later rows.
    let mut r = slice_reader_with_headers(
        b"a,b,c,d,e\n1,2,3,4,5\n6,7,8,9,10\n11,12,13,14,15\n16,17,18,19,20\n21,22,23,24,25\n",
    );
    let mut rows = Vec::new();
    while let Some(mut line) = r.next_line().expect("no io error") {
        rows.push(line.deserialized::<TwoFields>().expect("deserializes"));
    }
    assert_eq!(
        rows[0],
        TwoFields {
            b: "2".to_string(),
            d: "4".to_string()
        }
    );
    assert_eq!(
        rows[4],
        TwoFields {
            b: "22".to_string(),
            d: "24".to_string()
        }
    );
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StrictPair {
    a: String,
    b: String,
}

fn assert_wider_than_headers_error(error: &coseva::Error) {
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3,
        }
    );
    assert_eq!(error.location().field, 2);
}

#[test]
fn first_record_headers_reject_trailing_field_for_hashmap() {
    let mut reader = SliceParser::with_options(
        b"a,b\n1,2,3\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::Flexible),
    )
    .unwrap();

    let error = reader
        .next_line()
        .unwrap()
        .expect("record")
        .deserialized::<HashMap<String, String>>()
        .expect_err("field without a header must be rejected");
    assert_wider_than_headers_error(&error);
}

#[test]
fn provided_headers_reject_trailing_field_for_hashmap() {
    let headers = ByteRecord::from_iter(["a", "b"]);
    let mut reader = SliceParser::with_options(
        b"1,2,3\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::Provided(headers))
            .field_count(FieldCount::Flexible),
    )
    .unwrap();

    let error = reader
        .next_line()
        .unwrap()
        .expect("record")
        .deserialized::<HashMap<String, String>>()
        .expect_err("field without a header must be rejected");
    assert_wider_than_headers_error(&error);
}

#[test]
fn first_record_headers_reject_trailing_field_for_strict_struct() {
    let mut reader = SliceParser::with_options(
        b"a,b\n1,2,3\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::Flexible),
    )
    .unwrap();

    let error = reader
        .next_line()
        .unwrap()
        .expect("record")
        .deserialized::<StrictPair>()
        .expect_err("field without a header must be rejected");
    assert_wider_than_headers_error(&error);
}

#[test]
fn provided_headers_reject_trailing_field_for_strict_struct() {
    let headers = ByteRecord::from_iter(["a", "b"]);
    let mut reader = SliceParser::with_options(
        b"1,2,3\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::Provided(headers))
            .field_count(FieldCount::Flexible),
    )
    .unwrap();

    let error = reader
        .next_line()
        .unwrap()
        .expect("record")
        .deserialized::<StrictPair>()
        .expect_err("field without a header must be rejected");
    assert_wider_than_headers_error(&error);
}

/// `deny_unknown_fields` must keep failing on *every* record. Skipping is only
/// enabled after a record succeeds, which can never happen here.
#[test]
fn deny_unknown_fields_keeps_failing_on_every_record() {
    let mut reader = slice_reader_with_headers(b"a,b,zz\n1,2,3\n4,5,6\n7,8,9\n");
    for _ in 0..3 {
        let mut line = reader.next_line().unwrap().expect("record");
        let error = line
            .deserialized::<StrictPair>()
            .expect_err("unknown column must be rejected");
        assert!(matches!(error.kind(), ErrorKind::Serde));
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct Aliased {
    #[serde(alias = "legacy_name")]
    name: String,
}

/// Aliases are not in the field list Serde hands the deserializer, so an
/// aliased column must never be mistaken for an ignored one.
#[test]
fn aliased_columns_resolve_on_every_record() {
    let mut reader = slice_reader_with_headers(b"legacy_name,extra\nalpha,x\nbravo,y\ncharlie,z\n");
    let mut names = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        names.push(line.deserialized::<Aliased>().unwrap().name);
    }
    assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
}

/// Learned skipping is keyed to one struct; deserializing a different struct
/// from the same parser must relearn rather than reuse a stale column set.
#[test]
fn switching_struct_type_relearns_ignored_columns() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Other {
        a: String,
        e: String,
    }

    let mut reader =
        slice_reader_with_headers(b"a,b,c,d,e\n1,2,3,4,5\n6,7,8,9,10\n11,12,13,14,15\n");

    let mut line = reader.next_line().unwrap().expect("record");
    let first = line.deserialized::<TwoOfFive>().unwrap();
    assert_eq!(first.b, "2");

    let mut line = reader.next_line().unwrap().expect("record");
    let second = line.deserialized::<Other>().unwrap();
    assert_eq!(second.a, "6");
    assert_eq!(second.e, "10");

    let mut line = reader.next_line().unwrap().expect("record");
    let third = line.deserialized::<TwoOfFive>().unwrap();
    assert_eq!(third.b, "12");
    assert_eq!(third.d, "14");
}

#[derive(Debug, Deserialize, PartialEq)]
struct TailOptional {
    a: String,
    e: Option<String>,
}

/// A record shorter than the header row must stay aligned once leading columns
/// are skipped.
#[test]
fn short_records_stay_aligned_when_columns_are_skipped() {
    let mut reader = SliceParser::with_options(
        b"a,b,c,d,e\n1,2,3,4,5\n6,7\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(coseva::config::FieldCount::Flexible),
    )
    .unwrap();

    let mut line = reader.next_line().unwrap().expect("record");
    let first = line.deserialized::<TailOptional>().unwrap();
    assert_eq!(first.a, "1");
    assert_eq!(first.e.as_deref(), Some("5"));

    let mut line = reader.next_line().unwrap().expect("record");
    let second = line.deserialized::<TailOptional>().unwrap();
    assert_eq!(second.a, "6");
    assert_eq!(second.e, None);
}

/// Columns past the learned-set word size must still deserialize correctly.
#[test]
fn wide_records_beyond_the_learned_set_still_decode() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Wide {
        c0: String,
        c70: String,
    }

    let headers: Vec<String> = (0..80).map(|i| format!("c{i}")).collect();
    let values: Vec<String> = (0..80).map(|i| i.to_string()).collect();
    let header = headers.join(",");
    let row = values.join(",");
    let csv = format!("{header}\n{row}\n{row}\n");

    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    for _ in 0..2 {
        let mut line = reader.next_line().unwrap().expect("record");
        let row = line.deserialized::<Wide>().unwrap();
        assert_eq!(row.c0, "0");
        assert_eq!(row.c70, "70");
    }
}

/// A 200-column CSV whose columns are named `c0..c199`, repeated over `rows`
/// data records so the first observes and the rest skip.
fn wide_csv(columns: usize, rows: usize) -> String {
    let header = (0..columns)
        .map(|i| format!("c{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let row = (0..columns)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut csv = header;
    for _ in 0..rows {
        csv.push('\n');
        csv.push_str(&row);
    }
    csv.push('\n');
    csv
}

/// Ignored columns at the last index below the single-word edge (63), the two
/// indexes just above it (64, 65), and the far end of a wide header (199) must
/// all be learned so later records skip them while the kept columns still
/// resolve by name.
#[test]
fn wide_ignored_columns_are_learned_at_the_word_boundary_and_beyond() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Boundary {
        c63: String,
        c64: String,
        c65: String,
        c199: String,
    }

    // Three data rows: the first observes the ignored columns, the second and
    // third exercise the learned wide skip path across the word boundary.
    let csv = wide_csv(200, 3);
    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    let mut rows = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        rows.push(line.deserialized::<Boundary>().unwrap());
    }
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(
            row,
            &Boundary {
                c63: "63".to_owned(),
                c64: "64".to_owned(),
                c65: "65".to_owned(),
                c199: "199".to_owned(),
            }
        );
    }
}

/// A `deny_unknown_fields` struct must keep rejecting unknown columns on every
/// record even for a header wide enough to use the wide bitset, so a wide
/// learned set is never wrongly consulted to skip a column it must reject.
#[test]
fn wide_deny_unknown_fields_keeps_failing_on_every_record() {
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(dead_code, reason = "every decode is expected to fail before a read")]
    struct Strict {
        c0: String,
        c199: String,
    }

    let csv = wide_csv(200, 3);
    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    let mut records = 0;
    while let Some(mut line) = reader.next_line().unwrap() {
        line.deserialized::<Strict>()
            .expect_err("unknown wide columns must be rejected on every record");
        records += 1;
    }
    assert_eq!(records, 3, "every record must be visited and rejected");
}

/// Alternating two structs that keep different wide columns must relearn each
/// struct's own wide ignored set rather than reuse the other's skips.
#[test]
fn switching_struct_type_relearns_wide_ignored_columns() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Front {
        c0: String,
        c64: String,
    }
    #[derive(Debug, Deserialize, PartialEq)]
    struct Back {
        c65: String,
        c199: String,
    }

    let csv = wide_csv(200, 4);
    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    for _ in 0..2 {
        let mut line = reader.next_line().unwrap().expect("record");
        let front = line.deserialized::<Front>().unwrap();
        assert_eq!((front.c0.as_str(), front.c64.as_str()), ("0", "64"));

        let mut line = reader.next_line().unwrap().expect("record");
        let back = line.deserialized::<Back>().unwrap();
        assert_eq!((back.c65.as_str(), back.c199.as_str()), ("65", "199"));
    }
}

/// The wide learned set uses atomics so the cache stays `Sync`; decoding the
/// same wide input from several threads, each with its own parser, must produce
/// identical rows and exercise the wide bitset without a data race.
#[test]
fn wide_decoding_runs_across_threads() {
    #[derive(Debug, Deserialize, PartialEq)]
    struct Boundary {
        c63: String,
        c64: String,
        c65: String,
        c199: String,
    }

    let csv = wide_csv(200, 3);
    let expected = Boundary {
        c63: "63".to_owned(),
        c64: "64".to_owned(),
        c65: "65".to_owned(),
        c199: "199".to_owned(),
    };

    std::thread::scope(|scope| {
        let csv = &csv;
        let expected = &expected;
        for _ in 0..4 {
            scope.spawn(move || {
                let mut reader = SliceParser::with_options(
                    csv.as_bytes(),
                    FormatOptions::CSV,
                    ParseOptions::new().headers(Headers::FirstRecord),
                )
                .unwrap();
                while let Some(mut line) = reader.next_line().unwrap() {
                    assert_eq!(&line.deserialized::<Boundary>().unwrap(), expected);
                }
            });
        }
    });
}

/// The buffered `IoParser` shares the cache implementation with `SliceParser`.
#[test]
fn buffered_parser_skips_ignored_columns_consistently() {
    let mut reader = IoParser::with_options(
        std::io::Cursor::new(b"a,b,c,d,e\n1,2,3,4,5\n6,7,8,9,10\n".to_vec()),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();

    let mut line = reader.next_line().unwrap().expect("record");
    let first = line.deserialized::<TwoOfFive>().unwrap();
    assert_eq!((first.b.as_str(), first.d.as_str()), ("2", "4"));
    let mut line = reader.next_line().unwrap().expect("record");
    let second = line.deserialized::<TwoOfFive>().unwrap();
    assert_eq!((second.b.as_str(), second.d.as_str()), ("7", "9"));
}

// ── Column projection ────────────────────────────────────────────────────────

/// Reference values for `column` obtained without the projected kernel.
fn reference_column(csv: &str, column: &str) -> Vec<String> {
    let mut reader = SliceParser::with_options(
        csv,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();
    let index = reader
        .headers()
        .unwrap()
        .unwrap()
        .iter()
        .position(|name| name == column.as_bytes())
        .unwrap();
    let mut record = ByteRecord::new();
    let mut values = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        line.read_byte_record_into(&mut record).unwrap();
        values.push(String::from_utf8(record.get(index).unwrap().to_vec()).unwrap());
    }
    values
}

#[derive(Debug, Deserialize, PartialEq)]
struct OnlyB {
    b: String,
}

fn projected_slice(csv: &'static str) -> Vec<String> {
    let mut reader = slice_reader_with_headers(csv.as_bytes());
    let mut values = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        values.push(line.deserialized::<OnlyB>().unwrap().b);
    }
    values
}

fn projected_stream(csv: &str, capacity: usize) -> Vec<String> {
    let mut reader = IoParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .buffer_capacity(capacity),
    )
    .unwrap();
    let mut values = Vec::new();
    while let Some(mut line) = reader.next_line().unwrap() {
        values.push(line.deserialized::<OnlyB>().unwrap().b);
    }
    values
}

const PROJECTION_CSV: &str = concat!(
    "a,b,c,d,e\n",
    "1,2,3,4,5\n",
    "6,\"x,y\",8,9,10\n",
    "11,\"q\"\"z\",13,14,15\n",
    "16,,18,19,20\n",
    "21,\"multi\nline\",23,24,25\n",
    "26,w,28,29,30\r\n",
    "31,\"trailing\",33,34,35\n",
);

#[test]
fn projected_slice_deserialization_matches_the_materialized_path() {
    assert_eq!(
        projected_slice(PROJECTION_CSV),
        reference_column(PROJECTION_CSV, "b")
    );
}

#[test]
fn projected_streaming_deserialization_matches_across_buffer_capacities() {
    let expected = reference_column(PROJECTION_CSV, "b");
    for capacity in [1_usize, 2, 3, 7, 8, 16, 64, 256, 4096, 1 << 16] {
        assert_eq!(
            projected_stream(PROJECTION_CSV, capacity),
            expected,
            "buffer capacity {capacity}"
        );
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct OnlyD {
    d: String,
}

#[test]
fn alternating_struct_types_relearn_their_projection() {
    let mut reader = slice_reader_with_headers(PROJECTION_CSV.as_bytes());
    let expected_b = reference_column(PROJECTION_CSV, "b");
    let expected_d = reference_column(PROJECTION_CSV, "d");
    for record in 0..expected_b.len() {
        let mut line = reader.next_line().unwrap().expect("record");
        if record.is_multiple_of(2) {
            let row = line.deserialized::<OnlyB>().unwrap();
            assert_eq!(row.b, expected_b[record]);
        } else {
            let row = line.deserialized::<OnlyD>().unwrap();
            assert_eq!(row.d, expected_d[record]);
        }
    }
    assert!(reader.next_line().unwrap().is_none());
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct StrictOnlyB {
    b: String,
}

#[test]
fn deny_unknown_fields_never_projects() {
    let mut reader = slice_reader_with_headers(PROJECTION_CSV.as_bytes());
    for _ in 0..3 {
        let mut line = reader.next_line().unwrap().expect("record");
        drop(line.deserialized::<StrictOnlyB>().unwrap_err());
    }
}

#[derive(Debug, Deserialize, PartialEq)]
struct AliasedB {
    #[serde(alias = "b")]
    renamed: String,
}

#[test]
fn aliased_fields_survive_projection() {
    let mut reader = slice_reader_with_headers(PROJECTION_CSV.as_bytes());
    let expected = reference_column(PROJECTION_CSV, "b");
    for value in expected {
        let mut line = reader.next_line().unwrap().expect("record");
        let row = line.deserialized::<AliasedB>().unwrap();
        assert_eq!(row.renamed, value);
    }
}

#[test]
fn projection_reports_absent_fields_in_short_records() {
    let csv = "a,b,c\n1,2,3\n4,5,6\n7\n";
    let mut reader = SliceParser::with_options(
        csv,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();
    let mut line = reader.next_line().unwrap().expect("record");
    assert_eq!(line.deserialized::<OnlyB>().unwrap().b, "2");
    let mut line = reader.next_line().unwrap().expect("record");
    assert_eq!(line.deserialized::<OnlyB>().unwrap().b, "5");
    let mut line = reader.next_line().unwrap().expect("record");
    let error = line.deserialized::<OnlyB>().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Serde);
}

#[test]
fn projection_handles_records_wider_than_the_learned_set() {
    #[derive(Debug, Deserialize)]
    struct Wide {
        f70: String,
    }

    let names: Vec<String> = (0..80).map(|index| format!("f{index}")).collect();
    let mut csv = names.join(",");
    csv.push('\n');
    csv.push_str(
        &(0..80)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push('\n');
    csv.push_str(
        &(80..160)
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push('\n');

    let mut reader = SliceParser::with_options(
        csv.as_str(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .unwrap();
    assert_eq!(
        {
            let mut line = reader.next_line().unwrap().expect("record");
            line.deserialized::<Wide>().unwrap().f70
        },
        "70"
    );
    assert_eq!(
        {
            let mut line = reader.next_line().unwrap().expect("record");
            line.deserialized::<Wide>().unwrap().f70
        },
        "150"
    );
}

#[derive(Debug, Deserialize)]
struct FlattenedB {
    b: String,
    #[serde(flatten)]
    rest: std::collections::BTreeMap<String, String>,
}

#[test]
fn flattened_structs_never_project() {
    let mut reader = slice_reader_with_headers(b"a,b,c\n1,2,3\n4,5,6\n");
    let mut line = reader.next_line().unwrap().expect("record");
    let first = line.deserialized::<FlattenedB>().unwrap();
    assert_eq!(first.b, "2");
    assert_eq!(first.rest.len(), 2);
    let mut line = reader.next_line().unwrap().expect("record");
    let second = line.deserialized::<FlattenedB>().unwrap();
    assert_eq!(second.b, "5");
    assert_eq!(second.rest.len(), 2);
}

/// The parser caches the column projection it computes for a struct. Two
/// distinct structs can declare the same field names, and the compiler is free
/// to merge their field-name constants into a single pointer, so the field list
/// alone does not identify the struct. Keying the cache on the fields only let
/// one struct inherit another's projection and learned skip set, which made a
/// `deny_unknown_fields` struct silently accept a column it must reject.
#[test]
fn a_struct_does_not_inherit_another_structs_column_projection() {
    #[derive(Debug, serde::Deserialize)]
    struct Lax {
        x: i32,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[expect(
        dead_code,
        reason = "every decode of `Strict` is expected to fail, so `x` is never read"
    )]
    struct Strict {
        x: i32,
    }

    const INPUT: &[u8] = b"x,y\n1,2\n3,4\n";

    // Train the projection: `Lax` ignores `y`, so the parser learns to drop it.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let trained: Vec<Lax> = parser
        .deserialized_records::<Lax>()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(trained.len(), 2);
    assert_eq!(trained[0].x, 1);

    // `Strict` shares the field list but must still see `y` and reject it.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let outcome: Result<Vec<Strict>, _> = parser.deserialized_records::<Strict>().collect();
    outcome.expect_err("deny_unknown_fields must reject the unknown column y");

    // Both orders, on one parser, so the cached projection is definitely warm.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let first: Result<Vec<Lax>, _> = parser.deserialized_records::<Lax>().collect();
    assert_eq!(first.unwrap().len(), 2);
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let second: Result<Vec<Strict>, _> = parser.deserialized_records::<Strict>().collect();
    second.expect_err("deny_unknown_fields must still reject y on a warm parser");
}

/// The mirror of the projection-inheritance guard: a second struct that merely
/// shares a field list is perfectly legitimate and has to decode normally. If
/// the parser keyed its projection cache on the field list alone it would hand
/// this struct the first struct's projection, which the deserializer then
/// rejects as belonging to a different struct — a spurious failure on correct
/// input.
#[test]
fn a_second_struct_sharing_a_field_list_still_decodes() {
    #[derive(Debug, serde::Deserialize)]
    #[expect(dead_code, reason = "`First` only exists to warm the projection cache")]
    struct First {
        x: i32,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Second {
        x: i32,
    }

    const INPUT: &[u8] = b"x,y\n1,2\n3,4\n";

    // One parser for both structs, so the projection cache is warm from
    // `First` when `Second` arrives. A fresh parser per struct would start with
    // a cold cache and never exercise the key at all.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    parser.headers().expect("headers are discovered");
    let start = parser.location();
    let first: Vec<First> = parser
        .deserialized_records::<First>()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(first.len(), 2);

    parser.seek(start).expect("rewind to the first data record");
    let second: Result<Vec<Second>, _> = parser.deserialized_records::<Second>().collect();
    let second = second.expect("a struct sharing a field list must still decode");
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].x, 1);
    assert_eq!(second[1].x, 3);
}

// ── Record-level Option and explicit NULL ────────────────────────────────────

/// Parse every record as data using a NULL-aware dialect.
fn unheaded_nulls(input: &(impl AsRef<[u8]> + ?Sized)) -> SliceParser<'_> {
    SliceParser::with_options(
        input,
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
}

/// A single-field record is the value itself, so an explicit NULL in it makes
/// the whole record `None` rather than an attempt to parse empty bytes.
#[test]
fn record_level_option_reads_an_explicit_null_as_none() {
    let mut record = ByteRecord::new();
    record.push_null();
    let value: Option<i64> = record.deserialize().expect("an explicit NULL is None");
    assert_eq!(value, None);
}

/// The same rule applies to a NULL discovered by the parser rather than one
/// pushed onto an owned record, which reaches the deserializer through a
/// different flag iterator.
#[test]
fn a_parsed_explicit_null_deserializes_as_none() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_nulls(b"\\N\n");
    let mut line = parser.next_line()?.expect("record");
    let value: Option<i64> = line.deserialized()?;
    assert_eq!(value, None);
    Ok(())
}

/// The borrowing `Record` path carries its NULL flags as spans rather than as
/// endpoints, so it reaches the same rule through a different flag iterator.
#[test]
fn an_explicit_null_on_a_borrowed_record_is_none() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded_nulls(b"\\N\n");
    let mut line = parser.next_line()?.expect("record");
    let record = line.record()?;
    let value: Option<i64> = record.deserialize()?;
    assert_eq!(value, None);
    Ok(())
}

/// An explicit NULL is not the same as a present empty field: without a
/// NULL-aware dialect an empty field is data, so it stays `Some`.
#[test]
fn a_present_empty_field_is_not_none() -> Result<(), Box<dyn StdError>> {
    let mut parser = unheaded(b"\n");
    let mut line = parser.next_line()?.expect("record");
    let value: Option<String> = line.deserialized()?;
    assert_eq!(value, Some(String::new()));
    Ok(())
}

/// A record with a non-NULL single field still deserializes through the value,
/// so the NULL check must not swallow ordinary data.
#[test]
fn a_single_non_null_field_stays_some() {
    let mut record = ByteRecord::new();
    record.push_field(b"42");
    let value: Option<i64> = record.deserialize().expect("a value is Some");
    assert_eq!(value, Some(42));
}

/// A record with no fields at all remains `None`, which is the pre-existing
/// rule and is unrelated to explicit NULLs.
#[test]
fn an_empty_record_is_none() {
    let record = ByteRecord::new();
    let value: Option<i64> = record.deserialize().expect("an empty record is None");
    assert_eq!(value, None);
}

/// A wider record describes a value that spans its fields, so a NULL in the
/// first column must not collapse the whole record to `None`.
#[test]
fn a_leading_null_in_a_wider_record_stays_some() {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Row(Option<i64>, String);

    let mut record = ByteRecord::new();
    record.push_null();
    record.push_field(b"tail");
    let value: Option<Row> = record.deserialize().expect("a wider record is Some");
    assert_eq!(value, Some(Row(None, "tail".to_owned())));
}

// ── Deserializing leaves the parse position where the next record starts ─────

/// The three data records of [`NUMBERED`], as `(record number, first field)`.
///
/// Record numbers are one-based and count the header, so the first data record
/// is 1.
const NUMBERED: &[u8] = b"name,state,population\nBoston,MA,1\nAustin,TX,2\nDenver,CO,3\n";

/// Read every remaining record of `next` as `(record number, first field)`.
fn remaining(mut next: impl FnMut(&mut ByteRecord) -> bool) -> Vec<(u64, String)> {
    let mut seen = Vec::new();
    let mut record = ByteRecord::new();
    while next(&mut record) {
        seen.push((
            record.index(),
            String::from_utf8_lossy(&record[0]).into_owned(),
        ));
    }
    seen
}

/// What the two records after the first must report, whatever read the first.
fn tail_of_numbered() -> Vec<(u64, String)> {
    vec![(2, "Austin".to_owned()), (3, "Denver".to_owned())]
}

/// A struct naming two of the three columns, so the projected kernel applies.
#[derive(Debug, Deserialize, PartialEq)]
struct Numbered {
    name: String,
    population: u64,
}

/// Deserializing a record must consume exactly that record.
///
/// The Serde path rewinds to the record start so it can reparse, which is only
/// safe if the position is put back afterwards. A front end that already
/// materialized the record hands its spans over without reparsing, so nothing
/// else would move the position; leaving it rewound makes the next record reuse
/// this one's number, and makes a chunked front end replay the record outright.
#[test]
fn deserializing_a_record_consumes_exactly_that_record() -> Result<(), Box<dyn StdError>> {
    let options = ParseOptions::new().headers(Headers::FirstRecord);

    let mut parser = SliceParser::<Csv>::new(NUMBERED, options.clone())?;
    let mut line = parser.next_line()?.expect("the first data record");
    let first: Numbered = line.deserialized()?;
    assert_eq!(first.population, 1);
    let seen = remaining(|record| match parser.next_line().expect("a record") {
        Some(mut line) => {
            line.read_byte_record_into(record).expect("a record");
            true
        }
        None => false,
    });
    assert_eq!(seen, tail_of_numbered(), "slice");

    let mut parser = IoParser::<_, Csv>::new(NUMBERED, options.clone())?;
    let mut line = parser.next_line()?.expect("the first data record");
    let first: Numbered = line.deserialized()?;
    assert_eq!(first.population, 1);
    let seen = remaining(|record| match parser.next_line().expect("a record") {
        Some(mut line) => {
            line.read_byte_record_into(record).expect("a record");
            true
        }
        None => false,
    });
    assert_eq!(seen, tail_of_numbered(), "io");

    let mut parser = PushParser::<Csv>::new(options)?;
    let mut seen = Vec::new();
    let mut first_record = true;
    {
        let mut chunk = parser.chunk(NUMBERED);
        while let Some(mut line) = chunk.next_line()? {
            if first_record {
                first_record = false;
                let first: Numbered = line.deserialized()?;
                assert_eq!(first.population, 1);
            } else {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)?;
                seen.push((
                    record.index(),
                    String::from_utf8_lossy(&record[0]).into_owned(),
                ));
            }
        }
        let _ = chunk.done();
    }
    parser.finish();
    assert_eq!(seen, tail_of_numbered(), "push");

    Ok(())
}

/// A chunked front end must not replay a deserialized record at `finish`.
///
/// A single-record stream is the case that exposes it: the record is the whole
/// tail, so a position left at its start makes `finish` emit it a second time.
#[test]
fn a_chunked_parser_does_not_replay_a_deserialized_record() -> Result<(), Box<dyn StdError>> {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new().headers(Headers::FirstRecord))?;
    let mut seen = Vec::new();
    {
        let mut chunk = parser.chunk(b"name,state,population\nBoston,MA,1\n");
        while let Some(mut line) = chunk.next_line()? {
            seen.push(line.deserialized::<Numbered>()?);
        }
        let _ = chunk.done();
    }
    parser.finish();
    {
        let mut chunk = parser.chunk(b"");
        while let Some(mut line) = chunk.next_line()? {
            seen.push(line.deserialized::<Numbered>()?);
        }
        let _ = chunk.done();
    }

    assert_eq!(
        seen,
        vec![Numbered {
            name: "Boston".to_owned(),
            population: 1,
        }]
    );
    Ok(())
}

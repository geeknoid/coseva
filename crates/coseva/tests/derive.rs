//! Integration tests for the `CsvDecode` and `CsvEncode` derive macros.
//!
//! These tests use [`coseva::SliceParser`] for decoding and
//! [`coseva::encoding::CollectVisitor`] for encoding; no changes to `writer.rs`
//! are required.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io::Cursor;

use coseva::config::{
    EmitOptions, FieldCount, FormatOptions, Headers, ParseOptions, RecordEnding, WriteBom,
};
use coseva::encoding::{CollectVisitor, CsvEncode};
use coseva::encoding::{CsvDecode, CsvDecodeOwned, DecodeField};
use coseva::format::Csv;
use coseva::{Error, ErrorKind};
use coseva::{IoParser, PushParser, SliceParser};
use coseva::{
    VecEmitter, encode_append_path, encode_to_path, encode_to_segments, encode_to_vec,
    encode_to_writer, serialize_to_path, serialize_to_vec, serialize_to_writer,
};

mod common;

use common::{FailingSink, temp_file, unheaded};

// ── Test helpers ───────────────────────────────────────────────────────────────

/// Parse the first data record from a static byte slice.
///
/// The `T: CsvDecodeOwned` bound ensures no field borrows from the record,
/// so the decoded value can outlive the local reader.
fn first_record_decode<T: CsvDecodeOwned>(input: &'static [u8]) -> Result<T, Error> {
    let mut reader = unheaded(input);
    let mut line = reader
        .next_line()
        .expect("reader ok")
        .expect("record present");
    let record = line.record().expect("reader ok");
    T::csv_decode(&record)
}

#[derive(Debug, CsvDecode, PartialEq)]
struct SparseTypedRow {
    ordinal: u32,
    active: bool,
}

#[derive(Debug, CsvDecode, PartialEq)]
struct SparseBorrowedRow<'row> {
    name: &'row str,
    active: bool,
}

#[test]
fn typed_projection_decodes_only_mapped_fields() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored_a,active,ignored_b,ordinal\n\
                  \"say \"\"hello\"\"\",true,\"more \"\"ignored\"\"\",42\n";
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    assert_eq!(
        {
            let mut line = reader.next_line()?.expect("record");
            line.decoded::<SparseTypedRow>()?
        },
        SparseTypedRow {
            ordinal: 42,
            active: true,
        }
    );
    Ok(())
}

#[test]
fn typed_projection_preserves_borrowed_selected_escapes() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored,active,name,unused\n\
                  \"say \"\"ignored\"\"\",false,\"hello \"\"world\"\"\",tail\n";
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let _: () = {
        let mut line = reader.next_line()?.expect("record");
        let row = line.decoded::<SparseBorrowedRow<'_>>()?;
        assert_eq!(
            row,
            SparseBorrowedRow {
                name: "hello \"world\"",
                active: false,
            }
        );
    };
    Ok(())
}

#[test]
fn streaming_typed_projection_decodes_mapped_fields() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored_a,active,ignored_b,ordinal\n\
                  \"say \"\"hello\"\"\",true,\"more \"\"ignored\"\"\",42\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    assert_eq!(
        {
            let mut line = reader.next_line()?.expect("record");
            line.decoded::<SparseTypedRow>()?
        },
        SparseTypedRow {
            ordinal: 42,
            active: true,
        }
    );
    Ok(())
}

#[test]
fn unheaded_typed_decode_iterator_maps_fields_positionally() -> Result<(), Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        b"42,true\n7,false\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let rows = reader
        .decoded_records::<SparseTypedRow>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        rows,
        [
            SparseTypedRow {
                ordinal: 42,
                active: true,
            },
            SparseTypedRow {
                ordinal: 7,
                active: false,
            },
        ]
    );
    Ok(())
}

#[test]
fn unheaded_streaming_typed_decode_iterator_maps_fields_positionally()
-> Result<(), Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(b"42,true\n7,false\n"),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let rows = reader
        .decoded_records::<SparseTypedRow>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        rows,
        [
            SparseTypedRow {
                ordinal: 42,
                active: true,
            },
            SparseTypedRow {
                ordinal: 7,
                active: false,
            },
        ]
    );
    Ok(())
}

#[test]
fn projected_decode_iterators_and_reusable_output() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored,active,ordinal\n\"x\"\"x\",true,1\n\"y\"\"y\",false,2";
    let mut slice = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let rows = slice
        .decoded_records::<SparseTypedRow>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        rows,
        [
            SparseTypedRow {
                ordinal: 1,
                active: true,
            },
            SparseTypedRow {
                ordinal: 2,
                active: false,
            },
        ]
    );

    let mut streaming = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .buffer_capacity(7),
    )?;
    let rows = streaming
        .decoded_records::<SparseTypedRow>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        rows,
        [
            SparseTypedRow {
                ordinal: 1,
                active: true,
            },
            SparseTypedRow {
                ordinal: 2,
                active: false,
            },
        ]
    );

    let mut reusable = SparseTypedRow {
        ordinal: 0,
        active: false,
    };
    let mut slice = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = slice.next_line()?.expect("record");
    line.decode_into(&mut reusable)?;
    assert_eq!(
        reusable,
        SparseTypedRow {
            ordinal: 1,
            active: true,
        }
    );
    Ok(())
}

#[test]
fn typed_projection_still_rejects_invalid_ignored_fields() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored,ordinal,active\nbad\"quote,42,true\n";
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .decoded::<SparseTypedRow>()
        .expect_err("ignored fields still require valid CSV syntax");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().field, 0);
    assert_eq!(error.location().line, 2);

    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .decoded::<SparseTypedRow>()
        .expect_err("streaming ignored fields still require valid CSV syntax");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().field, 0);
    assert_eq!(error.location().line, 2);
    Ok(())
}

#[test]
fn streaming_typed_projection_preserves_field_count_errors() -> Result<(), Box<dyn StdError>> {
    let input = b"ignored,active,ordinal\nx,true,1,extra\n";
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::FirstRecord)
            .field_count(FieldCount::Exact(3)),
    )?;
    let mut line = reader.next_line()?.expect("record");
    let error = line
        .decoded::<SparseTypedRow>()
        .expect_err("the projected parser must count ignored fields");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 3,
            actual: 4,
        }
    );
    assert_eq!(error.location().byte, input.len());
    Ok(())
}

// ── Named struct: basic owned types ───────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct City {
    name: String,
    population: u64,
}

#[test]
fn decode_named_owned() -> Result<(), Box<dyn StdError>> {
    let city: City = first_record_decode(b"Boston,650706\n")?;
    assert_eq!(city.name, "Boston");
    assert_eq!(city.population, 650_706);
    Ok(())
}

#[test]
fn decode_integer_boundaries_directly_from_bytes() -> Result<(), Box<dyn StdError>> {
    assert_eq!(i8::decode_field(Some(b"-128"), 0, "i8")?, i8::MIN);
    assert_eq!(i8::decode_field(Some(b"+127"), 0, "i8")?, i8::MAX);
    assert_eq!(i32::decode_field(Some(b"-2147483648"), 0, "i32")?, i32::MIN);
    assert_eq!(
        i128::decode_field(Some(b"-170141183460469231731687303715884105728"), 0, "i128")?,
        i128::MIN
    );
    assert_eq!(u8::decode_field(Some(b"+255"), 0, "u8")?, u8::MAX);
    assert_eq!(
        u128::decode_field(Some(b"340282366920938463463374607431768211455"), 0, "u128")?,
        u128::MAX
    );
    Ok(())
}

#[test]
fn integer_decode_reports_precise_error_kinds() {
    let overflow = i8::decode_field(Some(b"128"), 0, "value").expect_err("i8 overflow");
    assert_eq!(overflow.kind(), ErrorKind::OutOfRange);
    assert_eq!(overflow.location().field, 0);

    let invalid = i32::decode_field(Some(b"12x"), 0, "value").expect_err("invalid digit");
    assert_eq!(invalid.kind(), ErrorKind::InvalidDigit);
    assert_eq!(invalid.location().field, 0);

    // A non-ASCII-digit byte is reported as an invalid digit, matching the
    // byte-oriented `FromBytes` integer parsers.
    let non_digit = i32::decode_field(Some(b"\xFF"), 0, "value").expect_err("invalid digit");
    assert_eq!(non_digit.kind(), ErrorKind::InvalidDigit);
    assert_eq!(non_digit.location().field, 0);
}

#[test]
fn encode_named_owned() -> Result<(), Box<dyn StdError>> {
    let city = City {
        name: "Boston".to_owned(),
        population: 650_706,
    };
    let mut v = CollectVisitor::new();
    city.csv_encode(&mut v)?;
    assert_eq!(v.fields(), [b"Boston".as_slice(), b"650706".as_slice()]);
    Ok(())
}

#[test]
fn field_names_named() {
    assert_eq!(<City as CsvDecode>::field_names(), &["name", "population"]);
    assert_eq!(<City as CsvEncode>::field_names(), &["name", "population"]);
}

// ── Named struct: rename attribute ────────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct RenameRow {
    #[csv(rename = "city_name")]
    name: String,
    #[csv(rename = "pop")]
    population: u64,
}

#[test]
fn decode_renamed_fields() -> Result<(), Box<dyn StdError>> {
    let row: RenameRow = first_record_decode(b"London,8982000\n")?;
    assert_eq!(row.name, "London");
    assert_eq!(row.population, 8_982_000);
    Ok(())
}

#[test]
fn field_names_renamed() {
    assert_eq!(
        <RenameRow as CsvDecode>::field_names(),
        &["city_name", "pop"]
    );
}

// ── Named struct: Option fields ───────────────────────────────────────────────

#[derive(Clone, Debug, CsvDecode, CsvEncode, PartialEq)]
struct WithOptional {
    name: String,
    score: Option<f64>,
}

#[test]
fn decode_option_present() -> Result<(), Box<dyn StdError>> {
    let row: WithOptional = first_record_decode(b"Alice,9.5\n")?;
    assert_eq!(row.name, "Alice");
    assert!((row.score.expect("score") - 9.5_f64).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn decode_option_empty() -> Result<(), Box<dyn StdError>> {
    let row: WithOptional = first_record_decode(b"Bob,\n")?;
    assert_eq!(row.name, "Bob");
    assert_eq!(row.score, None);
    Ok(())
}

#[test]
fn decode_option_absent() -> Result<(), Box<dyn StdError>> {
    let row: WithOptional = first_record_decode(b"Carol\n")?;
    assert_eq!(row.name, "Carol");
    assert_eq!(row.score, None);
    Ok(())
}

#[test]
fn encode_option_some() -> Result<(), Box<dyn StdError>> {
    let row = WithOptional {
        name: "Alice".to_owned(),
        score: Some(9.5),
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[0], b"Alice");
    // f64 Display format is implementation-defined; just check it round-trips.
    let encoded = std::str::from_utf8(&v.fields()[1])?;
    let decoded: f64 = encoded.parse()?;
    assert!((decoded - 9.5_f64).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn encode_option_none() -> Result<(), Box<dyn StdError>> {
    let row = WithOptional {
        name: "Bob".to_owned(),
        score: None,
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[1], b"");
    Ok(())
}

#[test]
fn database_presets_round_trip_native_options() -> Result<(), Box<dyn StdError>> {
    let row = WithOptional {
        name: "Bob".to_owned(),
        score: None,
    };
    for (writer_preset, reader_preset, expected) in [
        (
            FormatOptions::POSTGRES_COPY_CSV,
            FormatOptions::POSTGRES_COPY_CSV,
            b"Bob,\n".as_slice(),
        ),
        (
            FormatOptions::MYSQL,
            FormatOptions::MYSQL,
            b"Bob\t\\N\n".as_slice(),
        ),
    ] {
        let mut writer = VecEmitter::with_options(
            Vec::new(),
            writer_preset,
            EmitOptions::new().has_headers(false),
        )?;
        writer.encode(&row)?;
        let encoded = writer.into_inner();
        assert_eq!(encoded, expected);

        let mut reader = SliceParser::with_options(
            &encoded,
            reader_preset,
            ParseOptions::new().headers(Headers::None),
        )?;
        let mut line = reader.next_line()?.expect("record");
        assert_eq!(line.decoded::<WithOptional>()?, row.clone());
    }
    Ok(())
}

// ── Named struct: bool ────────────────────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct WithBool {
    flag: bool,
}

#[test]
fn decode_bool_true() -> Result<(), Box<dyn StdError>> {
    let row: WithBool = first_record_decode(b"true\n")?;
    assert!(row.flag);
    Ok(())
}

#[test]
fn decode_bool_zero() -> Result<(), Box<dyn StdError>> {
    let row: WithBool = first_record_decode(b"0\n")?;
    assert!(!row.flag);
    Ok(())
}

#[test]
fn decode_bool_error() {
    let err = first_record_decode::<WithBool>(b"maybe\n").expect_err("should fail");
    assert_eq!(err.location().field, 0);
    assert_eq!(err.kind(), ErrorKind::InvalidValue);
}

#[test]
fn encode_bool() -> Result<(), Box<dyn StdError>> {
    let row = WithBool { flag: true };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[0], b"true");
    Ok(())
}

// ── Named struct: default attribute ───────────────────────────────────────────

#[derive(Debug, CsvDecode, PartialEq)]
struct WithDefault {
    name: String,
    #[csv(default)]
    count: u32,
}

#[test]
fn decode_default_when_empty() -> Result<(), Box<dyn StdError>> {
    let row: WithDefault = first_record_decode(b"hello,\n")?;
    assert_eq!(row.name, "hello");
    assert_eq!(row.count, 0);
    Ok(())
}

#[test]
fn decode_default_when_absent() -> Result<(), Box<dyn StdError>> {
    let row: WithDefault = first_record_decode(b"hello\n")?;
    assert_eq!(row.name, "hello");
    assert_eq!(row.count, 0);
    Ok(())
}

#[test]
fn decode_default_when_present() -> Result<(), Box<dyn StdError>> {
    let row: WithDefault = first_record_decode(b"hello,42\n")?;
    assert_eq!(row.name, "hello");
    assert_eq!(row.count, 42);
    Ok(())
}

// ── Named struct: skip attribute ──────────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct WithSkip {
    first: String,
    #[csv(skip)]
    internal: u32,
    second: u64,
}

#[test]
fn skip_does_not_consume_csv_column() -> Result<(), Box<dyn StdError>> {
    // CSV has two columns; `internal` is not a CSV column.
    let row: WithSkip = first_record_decode(b"hello,99\n")?;
    assert_eq!(row.first, "hello");
    assert_eq!(row.internal, 0); // Default::default()
    assert_eq!(row.second, 99);
    Ok(())
}

#[test]
fn skip_field_not_encoded() -> Result<(), Box<dyn StdError>> {
    let row = WithSkip {
        first: "hello".to_owned(),
        internal: 999,
        second: 42,
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    // Only two fields in output; `internal` is skipped.
    assert_eq!(v.len(), 2);
    assert_eq!(v.fields()[0], b"hello");
    assert_eq!(v.fields()[1], b"42");
    Ok(())
}

#[test]
fn skip_field_not_in_field_names() {
    assert_eq!(<WithSkip as CsvDecode>::field_names(), &["first", "second"]);
    assert_eq!(<WithSkip as CsvEncode>::field_names(), &["first", "second"]);
}

// ── Named struct: parse_with ──────────────────────────────────────────────────

#[derive(Debug)]
struct HexError;

impl std::fmt::Display for HexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("field is not a hexadecimal integer")
    }
}

impl StdError for HexError {}

fn parse_hex(bytes: &[u8]) -> Result<u32, HexError> {
    let s = std::str::from_utf8(bytes).map_err(|_error| HexError)?;
    u32::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_error| HexError)
}

#[derive(Debug, CsvDecode, PartialEq)]
struct WithParseWith {
    label: String,
    #[csv(parse_with = "parse_hex")]
    value: u32,
}

#[test]
fn parse_with_custom_fn() -> Result<(), Box<dyn StdError>> {
    let row: WithParseWith = first_record_decode(b"test,0xff\n")?;
    assert_eq!(row.label, "test");
    assert_eq!(row.value, 255);
    Ok(())
}

#[test]
fn parse_with_error_carries_field_info() {
    let err = first_record_decode::<WithParseWith>(b"test,notHex!\n").expect_err("should fail");
    assert_eq!(err.location().field, 1);
    assert_eq!(err.field_name(), Some("value"));
}

// ── Named struct: format_with ─────────────────────────────────────────────────

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "format_with callbacks receive references so they also support non-Copy fields"
)]
fn format_hex(val: &u32) -> Vec<u8> {
    format!("0x{val:02x}").into_bytes()
}

#[derive(Debug, CsvEncode)]
struct WithFormatWith {
    label: String,
    #[csv(format_with = "format_hex")]
    value: u32,
}

#[test]
fn format_with_custom_fn() -> Result<(), Box<dyn StdError>> {
    let row = WithFormatWith {
        label: "test".to_owned(),
        value: 255,
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[0], b"test");
    assert_eq!(v.fields()[1], b"0xff");
    Ok(())
}

// ── Tuple struct ──────────────────────────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct TupleRow(String, u64, Option<f32>);

#[test]
fn decode_tuple_struct() -> Result<(), Box<dyn StdError>> {
    let row: TupleRow = first_record_decode(b"hello,42,3.14\n")?;
    assert_eq!(row.0, "hello");
    assert_eq!(row.1, 42);
    assert!(row.2.is_some());
    Ok(())
}

#[test]
fn decode_tuple_option_none() -> Result<(), Box<dyn StdError>> {
    let row: TupleRow = first_record_decode(b"hello,42,\n")?;
    assert_eq!(row.2, None);
    Ok(())
}

#[test]
fn encode_tuple_struct() -> Result<(), Box<dyn StdError>> {
    let row = TupleRow("world".to_owned(), 7, None);
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[0], b"world");
    assert_eq!(v.fields()[1], b"7");
    assert_eq!(v.fields()[2], b"");
    Ok(())
}

#[test]
fn tuple_field_names_are_positional() {
    assert_eq!(<TupleRow as CsvDecode>::field_names(), &["0", "1", "2"]);
    assert_eq!(<TupleRow as CsvEncode>::field_names(), &["0", "1", "2"]);
}

// ── Borrowed fields ───────────────────────────────────────────────────────────

#[derive(Debug, CsvDecode)]
struct BorrowedRow<'a> {
    name: &'a str,
    data: &'a [u8],
}

#[test]
fn decode_borrowed_fields() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"Boston,abc\n");
    let _: () = {
        let mut line = reader.next_line()?.expect("record");
        let record = line.record()?;
        let row = BorrowedRow::csv_decode(&record)?;
        assert_eq!(row.name, "Boston");
        assert_eq!(row.data, b"abc");
    };
    Ok(())
}

#[test]
fn slice_reader_decodes_borrowed_and_owned_rows() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"Boston,abc\n");
    let _: () = {
        let mut line = reader.next_line()?.expect("missing borrowed row");
        let row = line.decoded::<BorrowedRow<'_>>()?;
        assert_eq!(row.name, "Boston");
        assert_eq!(row.data, b"abc");
    };

    let mut reader = unheaded(b"Boston,650706\nLondon,8982000\n");
    let rows = reader
        .decoded_records::<City>()
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].name, "London");
    Ok(())
}

#[test]
fn streaming_reader_decodes_typed_rows() -> Result<(), Box<dyn StdError>> {
    let input = b"name,population\nBoston,650706\nLondon,8982000\n";
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(input), ParseOptions::new()).expect("parser");
    let mut line = reader.next_line()?.expect("missing first city");
    let first = line.decoded::<City>()?;
    assert_eq!(first.name, "Boston");

    let mut second = City {
        name: String::new(),
        population: 0,
    };
    let mut line = reader.next_line()?.expect("record");
    line.decode_into(&mut second)?;
    assert_eq!(second.population, 8_982_000);
    Ok(())
}

#[derive(Debug, CsvDecode)]
struct BorrowedCity<'a> {
    name: &'a str,
    population: u64,
}

#[test]
fn push_reader_decodes_typed_rows() -> Result<(), Box<dyn StdError>> {
    let input = b"name,population\nBoston,650706\nLondon,8982000\n";
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    parser.finish();
    let mut chunk = parser.chunk(input);

    let _: () = {
        let mut line = chunk.next_line()?.expect("missing first city");
        let first = line.decoded::<BorrowedCity<'_>>()?;
        assert_eq!(first.name, "Boston");
        assert_eq!(first.population, 650_706);
    };

    let mut second = City {
        name: String::new(),
        population: 0,
    };
    let mut line = chunk.next_line()?.expect("record");
    line.decode_into(&mut second)?;
    assert_eq!(second.name, "London");
    assert_eq!(second.population, 8_982_000);

    assert!(chunk.next_line()?.is_none());
    drop(chunk);

    assert_eq!(
        parser.headers().map(|headers| headers.get(0)),
        Some(Some(b"name".as_slice()))
    );
    assert_eq!(parser.header_index("population"), Some(1));
    assert!(parser.is_done());
    Ok(())
}

#[test]
fn typed_reader_resolves_renamed_headers_once() -> Result<(), Box<dyn StdError>> {
    let input = b"pop,city_name\n8982000,London\n";
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(input), ParseOptions::new()).expect("parser");
    let mut line = reader.next_line()?.expect("missing renamed row");
    let row = line.decoded::<RenameRow>()?;
    assert_eq!(row.name, "London");
    assert_eq!(row.population, 8_982_000);
    Ok(())
}

#[test]
fn typed_reader_rejects_duplicate_required_headers() {
    let input = b"name,name\nBoston,650706\n";
    let mut reader =
        IoParser::<_, Csv>::new(Cursor::new(input), ParseOptions::new()).expect("parser");
    let mut line = reader.next_line().expect("reader ok").expect("record");
    let error = line
        .decoded::<City>()
        .expect_err("duplicate header should fail");
    assert_eq!(error.kind(), coseva::ErrorKind::Decode);
}

// ── Wide indexed typed mapping ────────────────────────────────────────────────
//
// A struct whose field count times the header width passes the scan threshold
// resolves its column mapping through the indexed path instead of scanning
// every header once per field. These tests pin that the indexed path lands
// each field on its own column and preserves the duplicate and missing-column
// errors of the scan.

/// Twelve fields whose names occur among a hundred columns, so `12 × 100`
/// crosses the scan threshold and the mapping resolves through the index.
#[derive(Debug, CsvDecode, PartialEq)]
struct WideSelection {
    c3: u32,
    c7: u32,
    c11: u32,
    c19: u32,
    c23: u32,
    c31: u32,
    c47: u32,
    c59: u32,
    c63: u32,
    c64: u32,
    c88: u32,
    c99: u32,
}

/// A `columns`-wide CSV named `c0..cN`, whose data rows carry each column's own
/// index as its value, repeated over `rows` records.
fn wide_csv(columns: usize, rows: usize) -> String {
    let header = (0..columns)
        .map(|column| format!("c{column}"))
        .collect::<Vec<_>>()
        .join(",");
    let row = (0..columns)
        .map(|column| column.to_string())
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

#[test]
fn wide_typed_mapping_resolves_through_the_indexed_path() -> Result<(), Box<dyn StdError>> {
    // Two rows confirm the mapping cached from the first is reused on the
    // second, both resolved through the indexed path.
    let csv = wide_csv(100, 2);
    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let expected = WideSelection {
        c3: 3,
        c7: 7,
        c11: 11,
        c19: 19,
        c23: 23,
        c31: 31,
        c47: 47,
        c59: 59,
        c63: 63,
        c64: 64,
        c88: 88,
        c99: 99,
    };
    let mut rows = 0;
    while let Some(mut line) = reader.next_line()? {
        assert_eq!(line.decoded::<WideSelection>()?, expected);
        rows += 1;
    }
    assert_eq!(rows, 2);
    Ok(())
}

#[test]
fn wide_typed_mapping_rejects_a_duplicate_on_the_indexed_path() {
    // Column 63 is renamed to repeat column 3's name, so the field named `c3`
    // resolves to two columns and the mapping is ambiguous on the indexed path.
    let mut header: Vec<String> = (0..100).map(|column| format!("c{column}")).collect();
    header[63] = "c3".to_owned();
    let row: Vec<String> = (0..100).map(|column| column.to_string()).collect();
    let csv = format!("{}\n{}\n", header.join(","), row.join(","));

    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("parser");
    let mut line = reader.next_line().expect("reader ok").expect("record");
    let error = line
        .decoded::<WideSelection>()
        .expect_err("a duplicated column is ambiguous");
    assert_eq!(error.kind(), ErrorKind::Decode);
}

#[test]
fn wide_typed_mapping_reports_a_missing_column_on_the_indexed_path() {
    // A hundred-column header that lacks `c99`: the struct names it, so the
    // mapping fails as a missing required column on the indexed path.
    let header: Vec<String> = (0..99)
        .map(|column| format!("c{column}"))
        .chain(std::iter::once("filler".to_owned()))
        .collect();
    let row: Vec<String> = (0..100).map(|column| column.to_string()).collect();
    let csv = format!("{}\n{}\n", header.join(","), row.join(","));

    let mut reader = SliceParser::with_options(
        csv.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("parser");
    let mut line = reader.next_line().expect("reader ok").expect("record");
    let error = line
        .decoded::<WideSelection>()
        .expect_err("a missing column fails the mapping");
    assert_eq!(error.kind(), ErrorKind::Decode);
}

#[test]
fn typed_writer_generates_headers_and_rows() -> Result<(), Box<dyn StdError>> {
    let city = City {
        name: "Boston".to_owned(),
        population: 650_706,
    };
    let mut writer = VecEmitter::default();
    writer.encode_header::<City>()?;
    writer.encode(&city)?;
    assert_eq!(writer.into_inner(), b"name,population\nBoston,650706\n",);
    Ok(())
}

// ── Error carries field info ──────────────────────────────────────────────────

#[derive(Debug, CsvDecode)]
struct Strict {
    value: i32,
}

#[test]
fn decode_error_field_index_and_name() {
    let err = first_record_decode::<Strict>(b"notanumber\n").expect_err("should fail");
    assert_eq!(err.location().field, 0);
    assert_eq!(err.field_name(), Some("value"));
    assert_eq!(err.kind(), ErrorKind::InvalidDigit);
    // Display impl should mention the field.
    let display = err.to_string();
    assert!(display.contains("value"), "display: {display}");
}

// ── All primitive integer and float types ─────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode, PartialEq)]
struct AllNumerics {
    a: i8,
    b: i16,
    c: i32,
    d: i64,
    e: u8,
    f: u16,
    g: u32,
    h: u64,
    i: f32,
    j: f64,
}

#[test]
fn decode_all_numeric_types() -> Result<(), Box<dyn StdError>> {
    let row: AllNumerics = first_record_decode(b"-1,2,-3,4,5,6,7,8,1.5,2.5\n")?;
    assert_eq!(row.a, -1_i8);
    assert_eq!(row.b, 2_i16);
    assert_eq!(row.c, -3_i32);
    assert_eq!(row.d, 4_i64);
    assert_eq!(row.e, 5_u8);
    assert_eq!(row.f, 6_u16);
    assert_eq!(row.g, 7_u32);
    assert_eq!(row.h, 8_u64);
    assert!((row.i - 1.5_f32).abs() < f32::EPSILON);
    assert!((row.j - 2.5_f64).abs() < f64::EPSILON);
    Ok(())
}

#[test]
fn encode_all_numeric_types() -> Result<(), Box<dyn StdError>> {
    let row = AllNumerics {
        a: -1,
        b: 2,
        c: -3,
        d: 4,
        e: 5,
        f: 6,
        g: 7,
        h: 8,
        i: 1.5,
        j: 2.5,
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.len(), 10);
    assert_eq!(v.fields()[0], b"-1");
    assert_eq!(v.fields()[4], b"5");
    Ok(())
}

// ── Vec<u8> field ─────────────────────────────────────────────────────────────

#[derive(Debug, CsvDecode, CsvEncode)]
struct WithBytes {
    raw: Vec<u8>,
}

#[test]
fn decode_vec_u8() -> Result<(), Box<dyn StdError>> {
    let row: WithBytes = first_record_decode(b"hello\n")?;
    assert_eq!(row.raw, b"hello");
    Ok(())
}

#[test]
fn encode_vec_u8() -> Result<(), Box<dyn StdError>> {
    let row = WithBytes {
        raw: b"world".to_vec(),
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(v.fields()[0], b"world");
    Ok(())
}

// ── Round-trip via SliceParser ─────────────────────────────────────────────────

#[test]
fn multi_record_decode() -> Result<(), Box<dyn StdError>> {
    let input = b"Alice,100\nBob,200\nCarol,300\n";
    let mut reader = unheaded(input);
    let mut results = Vec::new();
    while let Some(mut line) = reader.next_line()? {
        let record = line.record()?;
        results.push(City::csv_decode(&record)?);
    }
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].name, "Alice");
    assert_eq!(results[1].population, 200);
    assert_eq!(results[2].name, "Carol");
    Ok(())
}

// ── Allocation reuse in decode_into ───────────────────────────────────────────

#[derive(Debug, Default, CsvDecode)]
struct Reusable {
    name: String,
    raw: Vec<u8>,
    note: Option<String>,
    count: u32,
}

/// `decode_into` must overwrite the caller's heap buffers rather than
/// replacing them, so a value whose buffers are already large enough performs
/// no reallocation across records.
#[test]
fn decoding_into_reuses_field_allocations() -> Result<(), Box<dyn StdError>> {
    let input = b"alpha,aaaa,note-a,1\nbravo,bbbb,note-b,2\ncharlie,cccc,note-c,3\n";
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::new(),
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut value = Reusable {
        name: String::with_capacity(64),
        raw: Vec::with_capacity(64),
        note: Some(String::with_capacity(64)),
        count: 0,
    };
    let name_ptr = value.name.as_ptr();
    let raw_ptr = value.raw.as_ptr();
    let note_ptr = value.note.as_ref().expect("seeded").as_ptr();

    let mut seen = 0;
    while let Some(mut line) = parser.next_line()? {
        line.decode_into(&mut value)?;
        seen += 1;
        assert_eq!(value.name.as_ptr(), name_ptr, "name buffer was replaced");
        assert_eq!(value.raw.as_ptr(), raw_ptr, "raw buffer was replaced");
        assert_eq!(
            value.note.as_ref().expect("present").as_ptr(),
            note_ptr,
            "note buffer was replaced"
        );
    }

    assert_eq!(seen, 3);
    assert_eq!(value.name, "charlie");
    assert_eq!(value.raw, b"cccc");
    assert_eq!(value.note.as_deref(), Some("note-c"));
    assert_eq!(value.count, 3);
    Ok(())
}

/// Reuse must not change decoded values: an absent or empty optional field
/// still clears a previously populated `Option`.
#[test]
fn decoding_into_clears_stale_optional_fields() -> Result<(), Box<dyn StdError>> {
    let input = b"alpha,aaaa,note-a,1\nbravo,bbbb,,2\n";
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::new(),
        ParseOptions::new().headers(Headers::None),
    )?;

    let mut value = Reusable::default();
    let mut line = parser.next_line()?.expect("record");
    line.decode_into(&mut value)?;
    assert_eq!(value.note.as_deref(), Some("note-a"));
    let mut line = parser.next_line()?.expect("record");
    line.decode_into(&mut value)?;
    assert_eq!(value.note, None);
    assert_eq!(value.name, "bravo");
    Ok(())
}

// ── Whole-document generation entry points ──────────────────────────────────

#[derive(CsvEncode, Clone)]
struct GeneratedCity {
    name: &'static str,
    pop: u32,
}

const GENERATED_CITIES: [GeneratedCity; 2] = [
    GeneratedCity {
        name: "Boston",
        pop: 650_706,
    },
    GeneratedCity {
        name: "London",
        pop: 8_982_000,
    },
];

const GENERATED_CSV: &[u8] = b"name,pop\nBoston,650706\nLondon,8982000\n";

#[test]
fn to_vec_writes_a_complete_document() -> Result<(), Box<dyn StdError>> {
    assert_eq!(
        encode_to_vec(
            GENERATED_CITIES.clone(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?,
        GENERATED_CSV
    );
    Ok(())
}

#[test]
fn to_writer_matches_to_vec_and_flushes() -> Result<(), Box<dyn StdError>> {
    // The entry point owns finalization, so the caller never has to flush.
    let mut output = Vec::new();
    encode_to_writer(
        &mut output,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    assert_eq!(output, GENERATED_CSV);
    Ok(())
}

#[test]
fn to_path_writes_a_complete_file() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("to_path");
    encode_to_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    assert_eq!(written, GENERATED_CSV);
    Ok(())
}

#[test]
fn to_path_honors_the_configured_format() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("to_path_opts");
    encode_to_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::TSV,
        EmitOptions::new().has_headers(false),
    )?;
    let written = std::fs::read(&path)?;
    assert_eq!(written, b"Boston\t650706\nLondon\t8982000\n");
    Ok(())
}

#[test]
fn generation_headers_can_be_disabled() -> Result<(), Box<dyn StdError>> {
    let encoded = encode_to_vec(
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    assert_eq!(encoded, b"Boston,650706\nLondon,8982000\n");
    Ok(())
}

#[test]
fn generation_writes_a_header_only_document_for_an_empty_iterator() -> Result<(), Box<dyn StdError>>
{
    // The column names are known statically, so an empty run still describes
    // its own schema rather than producing an empty file.
    let encoded = encode_to_vec(
        Vec::<GeneratedCity>::new(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    assert_eq!(encoded, b"name,pop\n");
    Ok(())
}

#[test]
fn generation_honors_format_and_field_count_options() -> Result<(), Box<dyn StdError>> {
    let mut output = Vec::new();
    encode_to_writer(
        &mut output,
        GENERATED_CITIES.clone(),
        FormatOptions::TSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    assert_eq!(output, b"name\tpop\nBoston\t650706\nLondon\t8982000\n");
    Ok(())
}

#[test]
fn generation_reports_a_field_count_violation() {
    let error = encode_to_vec(
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(3)),
    )
    .expect_err("a two-field record must be rejected");
    assert!(matches!(error.kind(), ErrorKind::FieldCountMismatch { .. }));
}

#[test]
fn to_path_reports_an_unopenable_destination() {
    let directory = common::temp_dir("derive-unopenable").expect("temporary directory");
    let error = encode_to_path(
        directory.path().join("missing").join("city.csv"),
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect_err("an unopenable path must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn to_writer_propagates_a_sink_failure() {
    let many = std::iter::repeat_n(GENERATED_CITIES[0].clone(), 10_000);
    let error = encode_to_writer(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        many,
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect_err("a broken sink must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn to_writer_holds_memory_flat_across_a_growing_record_count() -> Result<(), Box<dyn StdError>> {
    // The point of the pull-based entry points is that the iterator is driven
    // lazily into a bounded buffer, so peak buffered bytes must not scale with
    // the number of records.
    let mut peaks = Vec::new();
    for records in [1_000_usize, 100_000] {
        let mut sink = FailingSink::new();
        encode_to_writer(
            &mut sink,
            std::iter::repeat_n(GENERATED_CITIES[0].clone(), records),
            FormatOptions::CSV,
            EmitOptions::new(),
        )?;
        assert!(sink.total() > records, "every record must reach the sink");
        peaks.push(sink.peak());
    }
    // The drain boundary lands mid-record, so the two peaks differ by at most
    // one record rather than exactly matching. What must hold is that a
    // hundredfold increase in records does not raise the bound.
    let bound = 8 * 1024 + 256;
    assert!(
        peaks[1] <= bound,
        "peak buffered bytes {} exceeded the drain threshold plus one record",
        peaks[1]
    );
    assert!(
        peaks[1].abs_diff(peaks[0]) < 256,
        "peak buffered bytes grew with the record count: {peaks:?}"
    );
    Ok(())
}

#[derive(serde::Serialize, Clone)]
struct SerializedCity {
    name: &'static str,
    pop: u32,
}

const SERIALIZED_CITIES: [SerializedCity; 2] = [
    SerializedCity {
        name: "Boston",
        pop: 650_706,
    },
    SerializedCity {
        name: "London",
        pop: 8_982_000,
    },
];

#[test]
fn serialize_to_vec_writes_a_complete_document() -> Result<(), Box<dyn StdError>> {
    assert_eq!(
        serialize_to_vec(
            SERIALIZED_CITIES.clone(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?,
        GENERATED_CSV
    );
    Ok(())
}

#[test]
fn serialize_to_vec_matches_the_native_generation_path() -> Result<(), Box<dyn StdError>> {
    // The two paths describe the same records, so they must agree byte for
    // byte whenever both can emit a header.
    assert_eq!(
        serialize_to_vec(
            SERIALIZED_CITIES.clone(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?,
        encode_to_vec(
            GENERATED_CITIES.clone(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?
    );
    Ok(())
}

#[test]
fn serialize_to_writer_matches_serialize_to_vec_and_flushes() -> Result<(), Box<dyn StdError>> {
    let mut output = Vec::new();
    serialize_to_writer(
        &mut output,
        SERIALIZED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    assert_eq!(output, GENERATED_CSV);
    Ok(())
}

#[test]
fn serialize_to_path_writes_a_complete_file() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("serialize_to_path");
    serialize_to_path(
        &path,
        SERIALIZED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    assert_eq!(written, GENERATED_CSV);
    Ok(())
}

#[test]
fn serialize_to_path_honors_the_configured_format() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("serialize_to_path_opts");
    serialize_to_path(
        &path,
        SERIALIZED_CITIES.clone(),
        FormatOptions::TSV,
        EmitOptions::new().has_headers(false),
    )?;
    let written = std::fs::read(&path)?;
    assert_eq!(written, b"Boston\t650706\nLondon\t8982000\n");
    Ok(())
}

#[test]
fn serialize_generation_headers_can_be_disabled() -> Result<(), Box<dyn StdError>> {
    let encoded = serialize_to_vec(
        SERIALIZED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    assert_eq!(encoded, b"Boston,650706\nLondon,8982000\n");
    Ok(())
}

#[test]
fn serialize_generation_writes_nothing_for_an_empty_iterator() -> Result<(), Box<dyn StdError>> {
    // Serde header names come from the first value serialized rather than from
    // the type, so unlike the native path there is nothing to write. This
    // asymmetry is documented rather than papered over.
    assert!(
        serialize_to_vec(
            Vec::<SerializedCity>::new(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?
        .is_empty()
    );
    assert_eq!(
        encode_to_vec(
            Vec::<GeneratedCity>::new(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?,
        b"name,pop\n"
    );
    Ok(())
}

#[test]
fn serialize_generation_reports_the_first_field_count_error() {
    #[derive(serde::Serialize)]
    struct Wide {
        a: u32,
        b: u32,
        c: u32,
    }

    let mut output = Vec::new();
    let error = serialize_to_writer(
        &mut output,
        [Wide { a: 1, b: 2, c: 3 }],
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )
    .expect_err("a three-field record must be rejected");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3
        }
    );
}

#[test]
fn serialize_to_writer_streams_in_bounded_memory() -> Result<(), Box<dyn StdError>> {
    // Values are pulled one at a time, so the largest single write must not
    // track the record count.
    let mut peaks = Vec::new();
    for records in [1_000_usize, 100_000] {
        let mut sink = FailingSink::new();
        let values = (0..records).map(|i| SerializedCity {
            name: "Boston",
            pop: u32::try_from(i % 1_000).expect("fits"),
        });
        serialize_to_writer(&mut sink, values, FormatOptions::CSV, EmitOptions::new())?;
        peaks.push(sink.peak());
    }
    assert!(
        peaks[1].abs_diff(peaks[0]) < 256,
        "peak write size grew with the record count: {peaks:?}"
    );
    Ok(())
}

fn temp_path(tag: &str) -> common::TempFile {
    temp_file(tag)
}

#[test]
fn append_path_creates_a_file_with_a_header() -> Result<(), Box<dyn StdError>> {
    let path = temp_path("append_new");
    let _ = std::fs::remove_file(&path);
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    // Nothing to resume, so this is an ordinary document.
    assert_eq!(written, GENERATED_CSV);
    Ok(())
}

#[test]
fn append_path_resumes_without_repeating_the_header() -> Result<(), Box<dyn StdError>> {
    let path = temp_path("append_resume");
    let _ = std::fs::remove_file(&path);
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(
        written,
        b"name,pop\nBoston,650706\nLondon,8982000\nBoston,650706\nLondon,8982000\n"
    );
    Ok(())
}

#[test]
fn appending_in_many_runs_matches_one_run() -> Result<(), Box<dyn StdError>> {
    // Resuming is only useful if the result is indistinguishable from having
    // generated the whole document at once.
    let path = temp_path("append_equiv");
    let _ = std::fs::remove_file(&path);
    for city in GENERATED_CITIES.clone() {
        encode_append_path(&path, [city], FormatOptions::CSV, EmitOptions::new())?;
    }
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(
        written,
        encode_to_vec(
            GENERATED_CITIES.clone(),
            FormatOptions::CSV,
            EmitOptions::new()
        )?
    );
    Ok(())
}

#[test]
fn appending_refuses_a_file_with_an_unterminated_final_record() -> Result<(), Box<dyn StdError>> {
    // Appending here would fuse the new first record onto the truncated one,
    // silently corrupting the document, so it is refused.
    let path = temp_path("append_unterminated");
    std::fs::write(&path, b"name,pop\nBoston,650706")?;
    let error = encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect_err("truncated tail must be refused");
    let untouched = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(error.kind(), ErrorKind::UnterminatedRecord);
    assert_eq!(untouched, b"name,pop\nBoston,650706");
    Ok(())
}

#[test]
fn appending_refuses_a_bare_line_feed_tail_under_crlf() -> Result<(), Box<dyn StdError>> {
    // Under `CrLf` a record is terminated by the two-byte `\r\n`, so a lone
    // line feed leaves the last record unterminated. Checking only the final
    // byte would accept it and emit a document that mixes both endings, which
    // a strict `CrLf` parse then rejects.
    let path = temp_path("append_crlf_bare_lf");
    std::fs::write(&path, b"name,pop\r\nBoston,650706\n")?;
    let format = FormatOptions::CSV.record_ending(RecordEnding::CrLf);
    let error = encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())
        .expect_err("a bare line feed does not terminate a CrLf record");
    let untouched = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(error.kind(), ErrorKind::UnterminatedRecord);
    assert_eq!(untouched, b"name,pop\r\nBoston,650706\n");
    Ok(())
}

#[test]
fn appending_resumes_a_crlf_terminated_file() -> Result<(), Box<dyn StdError>> {
    // The counterpart to the test above: a genuine `\r\n` tail is terminated,
    // so the append proceeds and does not repeat the header.
    let path = temp_path("append_crlf_terminated");
    std::fs::write(&path, b"name,pop\r\nBoston,650706\r\n")?;
    let format = FormatOptions::CSV.record_ending(RecordEnding::CrLf);
    encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert!(written.starts_with(b"name,pop\r\nBoston,650706\r\n"));
    assert_eq!(
        written.windows(8).filter(|w| *w == b"name,pop").count(),
        1,
        "the header was repeated"
    );
    Ok(())
}

#[test]
fn appending_to_a_single_byte_file_is_refused_under_crlf() -> Result<(), Box<dyn StdError>> {
    // A one-byte file is shorter than a `CrLf` terminator, so it cannot end
    // with one. Reading two bytes must not seek before the start of the file.
    let path = temp_path("append_crlf_one_byte");
    std::fs::write(&path, b"x")?;
    let format = FormatOptions::CSV.record_ending(RecordEnding::CrLf);
    let error = encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())
        .expect_err("a one-byte file cannot end with a CrLf terminator");
    std::fs::remove_file(&path)?;
    assert_eq!(error.kind(), ErrorKind::UnterminatedRecord);
    Ok(())
}

#[test]
fn appending_to_a_bom_only_file_writes_a_header_without_a_second_mark()
-> Result<(), Box<dyn StdError>> {
    // A file holding nothing but a byte-order mark is a started document with
    // no records in it: the mark must not be repeated, but the header it never
    // got still belongs there.
    let path = temp_path("append_bom_only");
    std::fs::write(&path, b"\xEF\xBB\xBF")?;
    let format = FormatOptions::CSV.write_bom(WriteBom::Emit);
    encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    let mut expected = b"\xEF\xBB\xBF".to_vec();
    expected.extend_from_slice(GENERATED_CSV);
    assert_eq!(written, expected);
    Ok(())
}

#[test]
fn appending_to_an_empty_file_writes_a_header() -> Result<(), Box<dyn StdError>> {
    // An empty file has no unterminated record, so it starts a document.
    let path = temp_path("append_empty");
    std::fs::write(&path, b"")?;
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(written, GENERATED_CSV);
    Ok(())
}

#[test]
fn appending_does_not_repeat_the_byte_order_mark() -> Result<(), Box<dyn StdError>> {
    // A BOM belongs at the start of a document, never in the middle of one.
    let path = temp_path("append_bom");
    let _ = std::fs::remove_file(&path);
    let format = FormatOptions::CSV.write_bom(WriteBom::Emit);
    encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())?;
    encode_append_path(&path, GENERATED_CITIES.clone(), format, EmitOptions::new())?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert!(written.starts_with(b"\xEF\xBB\xBF"));
    assert!(
        !written[3..].windows(3).any(|w| w == b"\xEF\xBB\xBF"),
        "a second byte-order mark was written mid-document"
    );
    Ok(())
}

#[test]
fn appending_honors_a_configured_record_ending() -> Result<(), Box<dyn StdError>> {
    // The terminator check has to use the configured ending, not a hard-coded
    // newline, or CRLF documents would be rejected or corrupted.
    let path = temp_path("append_crlf");
    let _ = std::fs::remove_file(&path);
    let format = FormatOptions::CSV.record_ending(RecordEnding::CrLf);
    let options = EmitOptions::new().has_headers(false);
    encode_append_path(&path, GENERATED_CITIES.clone(), format, options)?;
    encode_append_path(&path, GENERATED_CITIES.clone(), format, options)?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(
        written,
        b"Boston,650706\r\nLondon,8982000\r\nBoston,650706\r\nLondon,8982000\r\n"
    );
    Ok(())
}

#[test]
fn an_appended_document_reparses_to_the_original_records() -> Result<(), Box<dyn StdError>> {
    let path = temp_path("append_roundtrip");
    let _ = std::fs::remove_file(&path);
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    encode_append_path(
        &path,
        GENERATED_CITIES.clone(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let written = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;

    // The header is consumed by the default policy, so only data records
    // remain: the second run must not have injected another header row.
    let mut parser = SliceParser::<Csv>::new(&written, ParseOptions::new()).expect("parser");
    let mut names = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        names.push(record.get_str(0)?.expect("a name field").to_string());
    }
    assert_eq!(names, ["Boston", "London", "Boston", "London"]);
    Ok(())
}

/// Ten cities, enough to span several parts at small size bounds.
fn many_cities() -> Vec<GeneratedCity> {
    (0..10)
        .map(|i| GeneratedCity {
            name: "Boston",
            pop: 1_000 + i,
        })
        .collect()
}

fn segment_namer(tag: &'static str) -> impl FnMut(usize) -> std::path::PathBuf {
    let base = temp_path(tag);
    move |index| base.with_extension(format!("{index}.csv"))
}

fn read_and_remove(paths: &[std::path::PathBuf]) -> Result<Vec<Vec<u8>>, Box<dyn StdError>> {
    let mut contents = Vec::new();
    for path in paths {
        contents.push(std::fs::read(path)?);
        std::fs::remove_file(path)?;
    }
    Ok(contents)
}

#[test]
fn segments_split_on_the_size_bound() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_split"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    assert!(
        paths.len() > 1,
        "ten records must not fit in one 64-byte part"
    );
    for part in &contents {
        assert!(
            part.len() <= 64,
            "part exceeded the size bound: {} bytes",
            part.len()
        );
    }
    Ok(())
}

#[test]
fn every_segment_repeats_the_header() -> Result<(), Box<dyn StdError>> {
    // Each part has to stand alone, which means carrying its own header.
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_header"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    for part in &contents {
        assert!(part.starts_with(b"name,pop\n"), "part lost its header");
    }
    Ok(())
}

#[test]
fn segments_concatenate_to_the_whole_document() -> Result<(), Box<dyn StdError>> {
    // Dropping each part's header must recover exactly the one-file document.
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_concat"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;

    let mut rejoined = b"name,pop\n".to_vec();
    for part in &contents {
        rejoined.extend_from_slice(&part["name,pop\n".len()..]);
    }
    assert_eq!(
        rejoined,
        encode_to_vec(many_cities(), FormatOptions::CSV, EmitOptions::new())?
    );
    Ok(())
}

#[test]
fn each_segment_parses_on_its_own() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_parse"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;

    let mut populations = Vec::new();
    for part in &contents {
        let mut parser = SliceParser::<Csv>::new(part, ParseOptions::new()).expect("parser");
        while let Some(mut line) = parser.next_line()? {
            let record = line.record()?;
            populations.push(record.get_str(1)?.expect("a pop field").to_string());
        }
    }
    let expected: Vec<String> = many_cities().iter().map(|c| c.pop.to_string()).collect();
    assert_eq!(populations, expected);
    Ok(())
}

#[test]
fn a_generous_bound_produces_a_single_segment() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        many_cities(),
        1 << 20,
        segment_namer("seg_single"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    assert_eq!(paths.len(), 1);
    assert_eq!(
        contents[0],
        encode_to_vec(many_cities(), FormatOptions::CSV, EmitOptions::new())?
    );
    Ok(())
}

#[test]
fn a_record_larger_than_the_bound_gets_its_own_oversized_segment() -> Result<(), Box<dyn StdError>>
{
    // Splitting a record would corrupt it, so the bound yields instead. Each
    // part must still hold exactly one record.
    let paths = encode_to_segments(
        many_cities(),
        1,
        segment_namer("seg_oversized"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    assert_eq!(paths.len(), many_cities().len());
    for (part, city) in contents.iter().zip(many_cities()) {
        // Header plus exactly one record, and nothing else.
        assert_eq!(
            part,
            &format!("name,pop\nBoston,{}\n", city.pop).into_bytes()
        );
    }
    Ok(())
}

#[test]
fn an_empty_iterator_produces_one_header_only_segment() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        Vec::<GeneratedCity>::new(),
        64,
        segment_namer("seg_empty"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    assert_eq!(paths.len(), 1);
    assert_eq!(contents[0], b"name,pop\n");
    Ok(())
}

#[test]
fn segments_can_omit_headers() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_noheader"),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    let contents = read_and_remove(&paths)?;
    for part in &contents {
        assert!(!part.starts_with(b"name,pop"));
    }
    Ok(())
}

#[test]
fn every_segment_repeats_the_byte_order_mark() -> Result<(), Box<dyn StdError>> {
    // A part that is a standalone document needs its own mark, so a consumer
    // reading only part three still detects the encoding.
    let paths = encode_to_segments(
        many_cities(),
        80,
        segment_namer("seg_bom"),
        FormatOptions::CSV.write_bom(WriteBom::Emit),
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    assert!(paths.len() > 1);
    for part in &contents {
        assert!(part.starts_with(b"\xEF\xBB\xBFname,pop\n"));
    }
    Ok(())
}

#[test]
fn a_zero_segment_size_is_rejected() {
    let error = encode_to_segments(
        many_cities(),
        0,
        segment_namer("seg_zero"),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect_err("a zero bound cannot be satisfied");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn segments_honor_a_configured_format() -> Result<(), Box<dyn StdError>> {
    let paths = encode_to_segments(
        many_cities(),
        64,
        segment_namer("seg_tsv"),
        FormatOptions::TSV,
        EmitOptions::new(),
    )?;
    let contents = read_and_remove(&paths)?;
    for part in &contents {
        assert!(part.starts_with(b"name\tpop\n"));
    }
    Ok(())
}

// ── Subset and fused decode paths ────────────────────────────────────────────

/// Names a subset of the file's columns, and in a non-contiguous order, so
/// resolving it against the header yields a mapping rather than the identity.
#[derive(Debug, CsvDecode)]
struct Subset {
    left: String,
    count: u32,
}

/// A subset decode reads through the same vectorized parser as a full one, so
/// the record ending must already be trimmed by the time the mapping selects
/// its columns.
#[test]
fn a_subset_decode_trims_crlf_record_endings() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        "left,middle,count\r\nplain,ignored,1\r\nsecond,ignored,2\r\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut seen = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let row: Subset = line.decoded()?;
        seen.push((row.left, row.count));
    }
    assert_eq!(
        seen,
        [("plain".to_owned(), 1), ("second".to_owned(), 2)],
        "a trailing CR must not survive into the selected field",
    );
    Ok(())
}

/// A poisoned parser must refuse a second view of the failed line rather than
/// reparse it and reproduce the original fault. `decode_with_mapping` checks
/// this itself, because it does not go through `read_physical_record`.
#[test]
fn a_second_view_of_a_poisoned_line_reports_the_failure_on_the_decode_path()
-> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,middle,count\nplain,ignored,1\n\"unterminated",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut line = parser.next_line()?.expect("the first, well-formed record");
    let row: Subset = line.decoded()?;
    assert_eq!(row.left, "plain");

    let mut line = parser.next_line()?.expect("the unterminated record");
    let _ = line
        .decoded::<Subset>()
        .expect_err("the quoted field is never closed");
    let again = line
        .decoded::<Subset>()
        .expect_err("a second view of the same poisoned line reports the failure");
    assert_eq!(again.kind(), ErrorKind::ParserFailed);
    Ok(())
}

/// Names the file's columns in declaration order, so it decodes through the
/// fused path.
#[derive(Debug, Default, CsvDecode)]
struct Fused {
    left: String,
    count: u32,
}

/// A fused decode that fails must still report where it failed. The fused
/// in-place path attributes the error itself, separately from the by-value one.
#[test]
fn a_failing_fused_decode_into_is_located() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"left,count\nplain,1\nplain,notanumber\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )?;

    let mut value = Fused::default();
    let mut line = parser.next_line()?.expect("the first record");
    line.decode_into(&mut value)?;
    assert_eq!(value.count, 1);

    let mut line = parser.next_line()?.expect("the malformed record");
    let error = line
        .decode_into(&mut value)
        .expect_err("`notanumber` is not a u32");
    assert_eq!(error.location().record, 2, "{error}");
    Ok(())
}

// ── `rename_all` ───────────────────────────────────────────────────────

#[derive(CsvDecode, CsvEncode, Clone, Debug, PartialEq)]
#[csv(rename_all = "PascalCase")]
struct PascalCity {
    city_name: String,
    total_population: u64,
    #[csv(rename = "ZIP")]
    zip_code: String,
}

/// `rename_all` supplies the column name every field would otherwise take
/// from its own identifier, and an explicit `rename` still wins over it.
#[test]
fn rename_all_names_the_columns_and_rename_overrides_it() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"CityName,TotalPopulation,ZIP\nBoston,650706,02108\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let cities: Vec<PascalCity> = parser.decoded_records().collect::<Result<_, _>>()?;
    assert_eq!(
        cities,
        [PascalCity {
            city_name: "Boston".to_owned(),
            total_population: 650_706,
            zip_code: "02108".to_owned(),
        }]
    );
    Ok(())
}

/// Encoding uses the same names, so a document round-trips through the pair.
#[test]
fn rename_all_round_trips_through_encode() -> Result<(), Box<dyn StdError>> {
    let original = PascalCity {
        city_name: "Denver".to_owned(),
        total_population: 715_522,
        zip_code: "80202".to_owned(),
    };
    let encoded = encode_to_vec([original.clone()], FormatOptions::CSV, EmitOptions::new())?;
    assert_eq!(
        core::str::from_utf8(&encoded)?,
        "CityName,TotalPopulation,ZIP\nDenver,715522,80202\n"
    );

    let mut parser = SliceParser::with_options(&encoded, FormatOptions::CSV, ParseOptions::new())?;
    let decoded: Vec<PascalCity> = parser.decoded_records().collect::<Result<_, _>>()?;
    assert_eq!(decoded, [original]);
    Ok(())
}

/// The field identifier no longer names a column once `rename_all` applies.
#[test]
fn the_unrenamed_header_no_longer_binds() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"city_name,total_population,ZIP\nBoston,650706,02108\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let result: Result<Vec<PascalCity>, _> = parser.decoded_records().collect();
    let error = result.expect_err("`city_name` is not a column of this type");
    assert_eq!(error.kind(), ErrorKind::Decode);
    Ok(())
}

#[derive(CsvDecode, Debug, PartialEq)]
#[csv(rename_all = "kebab-case")]
struct KebabRow {
    first_name: String,
    last_name: String,
}

/// Each rule is checked against Serde's spelling in the macro crate's unit
/// tests; this confirms one of them survives the whole decode path.
#[test]
fn kebab_case_headers_bind() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"first-name,last-name\nAda,Lovelace\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let rows: Vec<KebabRow> = parser.decoded_records().collect::<Result<_, _>>()?;
    assert_eq!(
        rows,
        [KebabRow {
            first_name: "Ada".to_owned(),
            last_name: "Lovelace".to_owned(),
        }]
    );
    Ok(())
}

// ── `alias` ────────────────────────────────────────────────────────────

#[derive(CsvDecode, Debug, PartialEq)]
struct AliasedCity {
    #[csv(alias = "town", alias = "municipality")]
    city: String,
    #[csv(rename = "pop", alias = "population")]
    people: u64,
}

/// Each alias binds the column its field would otherwise miss.
#[test]
fn an_alias_binds_a_column_the_primary_name_misses() -> Result<(), Box<dyn StdError>> {
    for header in ["town,population", "municipality,population"] {
        let input = format!("{header}\nBoston,650706\n");
        let mut parser =
            SliceParser::with_options(input.as_bytes(), FormatOptions::CSV, ParseOptions::new())?;
        let cities: Vec<AliasedCity> = parser.decoded_records().collect::<Result<_, _>>()?;
        assert_eq!(
            cities,
            [AliasedCity {
                city: "Boston".to_owned(),
                people: 650_706,
            }]
        );
    }
    Ok(())
}

/// Aliases add spellings rather than replacing the primary one.
#[test]
fn the_primary_name_still_binds_alongside_aliases() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(
        b"city,pop\nDenver,715522\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let cities: Vec<AliasedCity> = parser.decoded_records().collect::<Result<_, _>>()?;
    assert_eq!(
        cities,
        [AliasedCity {
            city: "Denver".to_owned(),
            people: 715_522,
        }]
    );
    Ok(())
}

/// A document naming one field twice is ambiguous, whichever spellings it used.
#[test]
fn a_primary_name_and_an_alias_in_one_document_is_ambiguous() {
    let mut parser = SliceParser::with_options(
        b"city,town,pop\nBoston,Boston,650706\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )
    .expect("parser");
    let result: Result<Vec<AliasedCity>, _> = parser.decoded_records().collect();
    result.expect_err("the duplicated field to be ambiguous");
}

/// Two types sharing a field-name list keep their own mappings, even though
/// the cached mapping is keyed by that list's address.
#[test]
fn types_sharing_field_names_do_not_share_a_mapping() -> Result<(), Box<dyn StdError>> {
    #[derive(CsvDecode, Debug, PartialEq)]
    struct ByAlias {
        #[csv(alias = "town")]
        city: String,
    }

    #[derive(CsvDecode, Debug, PartialEq)]
    struct ByName {
        city: String,
    }

    let mut parser = SliceParser::with_options(
        b"town\nBoston\nDenver\n",
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;

    let first: ByAlias = parser
        .decoded_records()
        .next()
        .expect("a first record")
        .expect("the alias to bind");
    assert_eq!(
        first,
        ByAlias {
            city: "Boston".to_owned(),
        }
    );

    // Reusing the cached mapping here would wrongly bind `town` to `city`.
    let second: Result<ByName, _> = parser.decoded_records().next().expect("a second record");
    second.expect_err("`city` to be missing from the headers");
    Ok(())
}

// ── Where clauses and multiple lifetimes ──────────────────────────────────────

/// A `where` predicate naming the struct's own lifetime.
///
/// The derive erases the input's lifetimes in favour of its own row lifetime,
/// so a predicate copied verbatim would name a lifetime the generated
/// implementation never declares.
#[derive(Debug, CsvDecode)]
struct WhereClauseRow<'a, T>
where
    T: DecodeField<'a> + 'a,
{
    name: &'a str,
    value: T,
}

#[test]
fn decode_row_whose_where_clause_names_its_own_lifetime() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"Boston,42\n");
    let mut line = reader.next_line()?.expect("record");
    let record = line.record()?;
    let row: WhereClauseRow<'_, u32> = WhereClauseRow::csv_decode(&record)?;
    assert_eq!(row.name, "Boston");
    assert_eq!(row.value, 42);
    Ok(())
}

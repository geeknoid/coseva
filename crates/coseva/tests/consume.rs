//! Integration tests for the whole-document read entry points.
//!
//! Each one is checked against the parser-and-iterator form it replaces, so
//! the convenience cannot drift from the long way round. The property that
//! motivated them — that the iterator outlives the call producing it — is
//! checked by returning one from a function, which is what the borrowing
//! iterators cannot do.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::{
    Error, ErrorKind, SliceParser, decode_from_path, decode_from_reader, decode_from_slice,
    deserialize_from_path, deserialize_from_reader, deserialize_from_slice,
};
use serde::Deserialize;

mod common;

const INPUT: &[u8] = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";

#[derive(Debug, CsvDecode, Deserialize, PartialEq)]
struct Row {
    city: String,
    country: String,
}

/// The records `INPUT` describes, in order.
fn expected() -> Vec<Row> {
    vec![
        Row {
            city: "Boston".into(),
            country: "US".into(),
        },
        Row {
            city: "Paris".into(),
            country: "FR".into(),
        },
        Row {
            city: "Denver".into(),
            country: "US".into(),
        },
    ]
}

#[test]
fn decoding_a_slice_matches_driving_a_parser() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::with_options(INPUT, FormatOptions::CSV, ParseOptions::new())?;
    let driven: Vec<Row> = parser.decoded_records().collect::<Result<_, Error>>()?;

    let whole: Vec<Row> = decode_from_slice(INPUT, FormatOptions::CSV, ParseOptions::new())?
        .collect::<Result<_, Error>>()?;

    assert_eq!(whole, driven);
    assert_eq!(whole, expected());
    Ok(())
}

#[test]
fn decoding_a_reader_matches_decoding_a_slice() -> Result<(), Box<dyn StdError>> {
    let whole: Vec<Row> = decode_from_reader(INPUT, FormatOptions::CSV, ParseOptions::new())?
        .collect::<Result<_, Error>>()?;
    assert_eq!(whole, expected());
    Ok(())
}

#[test]
fn decoding_a_path_matches_decoding_a_slice() -> Result<(), Box<dyn StdError>> {
    let path = common::temp_file("consume-decode");
    std::fs::write(path.path(), INPUT)?;
    let whole: Vec<Row> = decode_from_path(path.path(), FormatOptions::CSV, ParseOptions::new())?
        .collect::<Result<_, Error>>()?;
    assert_eq!(whole, expected());
    Ok(())
}

#[test]
fn deserializing_matches_decoding_on_every_source() -> Result<(), Box<dyn StdError>> {
    let path = common::temp_file("consume-deserialize");
    std::fs::write(path.path(), INPUT)?;

    let from_slice: Vec<Row> =
        deserialize_from_slice(INPUT, FormatOptions::CSV, ParseOptions::new())?
            .collect::<Result<_, Error>>()?;
    let from_reader: Vec<Row> =
        deserialize_from_reader(INPUT, FormatOptions::CSV, ParseOptions::new())?
            .collect::<Result<_, Error>>()?;
    let from_path: Vec<Row> =
        deserialize_from_path(path.path(), FormatOptions::CSV, ParseOptions::new())?
            .collect::<Result<_, Error>>()?;

    assert_eq!(from_slice, expected());
    assert_eq!(from_reader, expected());
    assert_eq!(from_path, expected());

    Ok(())
}

/// The point of the entry points: the iterator owns its parser, so it can be
/// returned from the expression that built it.
fn rows_of(input: &[u8]) -> Result<impl Iterator<Item = Result<Row, Error>> + '_, Error> {
    decode_from_slice(input, FormatOptions::CSV, ParseOptions::new())
}

#[test]
fn the_iterator_outlives_the_call_that_produced_it() -> Result<(), Box<dyn StdError>> {
    let rows: Vec<Row> = rows_of(INPUT)?.collect::<Result<_, Error>>()?;
    assert_eq!(rows, expected());
    Ok(())
}

#[test]
fn a_bad_format_fails_before_any_record_is_read() {
    let error = decode_from_slice::<Row, _>(
        INPUT,
        FormatOptions::CSV.delimiter(b'"'),
        ParseOptions::new(),
    )
    .err()
    .expect("delimiter collides with the quote");
    assert!(matches!(error.kind(), ErrorKind::Configuration));
}

#[test]
fn an_unopenable_file_fails_before_any_record_is_read() {
    let path = common::temp_file("consume-absent");
    let error = decode_from_path::<_, Row>(path.path(), FormatOptions::CSV, ParseOptions::new())
        .err()
        .expect("the file does not exist");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn a_decoding_failure_surfaces_from_the_iterator() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, CsvDecode)]
    struct Numbered {
        city: String,
        country: u32,
    }

    let mut rows =
        decode_from_slice::<Numbered, _>(INPUT, FormatOptions::CSV, ParseOptions::new())?;
    let error = rows
        .next()
        .expect("one item")
        .expect_err("`US` is not a number");
    assert!(matches!(error.kind(), ErrorKind::InvalidDigit));
    Ok(())
}

#[test]
fn headers_default_to_the_first_record() -> Result<(), Box<dyn StdError>> {
    // Field order deliberately differs from the column order, so a positional
    // binding would produce `city: "US"` rather than an error.
    #[derive(Debug, CsvDecode, PartialEq)]
    struct Swapped {
        country: String,
        city: String,
    }

    let rows: Vec<Swapped> = decode_from_slice(INPUT, FormatOptions::CSV, ParseOptions::new())?
        .collect::<Result<_, Error>>()?;

    assert_eq!(rows[0].city, "Boston");
    assert_eq!(rows[0].country, "US");
    Ok(())
}

#[test]
fn an_explicit_header_setting_is_honoured() -> Result<(), Box<dyn StdError>> {
    let rows: Vec<Row> = decode_from_slice(
        b"Boston,US\nParis,FR\nDenver,US\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?
    .collect::<Result<_, Error>>()?;

    assert_eq!(rows, expected());
    Ok(())
}

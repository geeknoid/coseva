//! A statically formatted parser must expose the same API as a dynamic one.
//!
//! The format type parameter defaults to `Dynamic`, so an inherent `impl`
//! written against `IoParser<R>` or `SliceParser<'a>` silently attaches only
//! to the dynamic instantiation. That is invisible until somebody names a
//! format and finds the method missing, so these tests call the whole surface
//! through a static format. They are compile-time assertions first: if a
//! method regresses to dynamic-only, this file stops building.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, reason = "a test asserts by panicking")]

use std::io::Cursor;

use coseva::config::{FormatOptions, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::{IoParser, SliceParser};

mod common;

#[derive(Debug, CsvDecode)]
struct Row {
    name: String,
    size: u32,
}

const DATA: &[u8] = b"name,size\nalpha,1\nbeta,2\n";

fn options() -> ParseOptions {
    ParseOptions::new()
}

/// Every owned-record iterator must exist on a static `SliceParser`.
#[test]
fn slice_parser_iterators_work_for_a_static_format() {
    let mut parser = SliceParser::<Csv>::new(DATA, options()).expect("parser");
    assert_eq!(parser.byte_records().count(), 2);

    let mut parser = SliceParser::<Csv>::new(DATA, options()).expect("parser");
    assert_eq!(parser.text_records().count(), 2);

    let mut parser = SliceParser::<Csv>::new(DATA, options()).expect("parser");
    let decoded: Vec<Row> = parser
        .decoded_records::<Row>()
        .collect::<Result<_, _>>()
        .expect("decode");
    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[1].name, "beta");
    assert_eq!(decoded[1].size, 2);
}

/// Every owned-record iterator must exist on a static `IoParser`.
#[test]
fn io_parser_iterators_work_for_a_static_format() {
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(DATA.to_vec()), options()).expect("parser");
    assert_eq!(parser.byte_records().count(), 2);

    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(DATA.to_vec()), options()).expect("parser");
    assert_eq!(parser.text_records().count(), 2);
}

/// Rewinding is a `Seek` feature, not a dynamic-format feature.
#[test]
fn io_parser_rewind_works_for_a_static_format() {
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(DATA.to_vec()), options()).expect("parser");
    let first = parser.byte_records().count();
    parser.rewind().expect("rewind");
    let second = parser.byte_records().count();
    assert_eq!(
        first, second,
        "a rewound static parser reread a different document"
    );
}

/// Opening a path must not force the dynamic format.
#[test]
fn io_parser_path_works_for_a_static_format() {
    let path = common::temp_file("format-parity");
    std::fs::write(path.path(), DATA).expect("write");

    let mut parser = IoParser::<_, Csv>::new_path(path.path(), options()).expect("parser");
    let statik = parser.byte_records().count();

    let mut parser =
        IoParser::from_path(path.path(), FormatOptions::CSV, options()).expect("parser");
    let dynamic = parser.byte_records().count();

    assert_eq!(statik, dynamic, "static and dynamic path parsers disagreed");
}

/// A static parser must still answer header queries.
#[test]
fn header_access_works_for_a_static_format() {
    let mut parser = SliceParser::<Csv>::new(DATA, options()).expect("parser");
    let headers = parser.headers().expect("headers").expect("present");
    assert_eq!(headers.len(), 2);
    assert_eq!(parser.header_index("size").expect("index"), Some(1));
}

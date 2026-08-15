//! `CompactString` must behave exactly like `String` everywhere a field type
//! can appear, while storing short fields inline.
//!
//! The two impls exist only because the orphan rule puts them out of reach of
//! downstream crates; they carry no behaviour of their own. So every test here
//! is a parity test against `String` on the same bytes, covering each route a
//! field type is reached through: the general decode path, the fused path that
//! `#[derive(CsvDecode)]` opts into, `Record::parse`, the buffer-reusing
//! `decode_field_into`, and encoding.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;

use compact_str::CompactString;
use coseva::ErrorKind;
use coseva::encoding::{CollectVisitor, CsvDecode, CsvEncode, DecodeField};

mod common;

use common::unheaded;

#[derive(Debug, PartialEq, CsvDecode, CsvEncode)]
struct CompactRow {
    city: CompactString,
    state: CompactString,
    population: u64,
}

#[derive(Debug, PartialEq, CsvDecode, CsvEncode)]
struct StringRow {
    city: String,
    state: String,
    population: u64,
}

/// The derived path is the fused one. Decoding the same bytes into the
/// `String` twin must produce the same values.
#[test]
fn derived_decode_matches_string() -> Result<(), Box<dyn StdError>> {
    let input = b"Boston,Massachusetts,650706\n\
                  \"say \"\"hello\"\"\",MA,1\n";

    let mut compact = unheaded(input);
    let mut plain = unheaded(input);

    for _ in 0..2 {
        let c = {
            let mut line = compact.next_line()?.expect("record");
            line.decoded::<CompactRow>()?
        };
        let s = {
            let mut line = plain.next_line()?.expect("record");
            line.decoded::<StringRow>()?
        };
        assert_eq!(c.city.as_str(), s.city.as_str());
        assert_eq!(c.state.as_str(), s.state.as_str());
        assert_eq!(c.population, s.population);
    }
    Ok(())
}

/// A field that arrives unescaped through the scratch buffer must decode the
/// same way as one borrowed straight from the input.
#[test]
fn escaped_field_unescapes() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"\"say \"\"hello\"\"\",MA,1\n");
    let row = {
        let mut line = reader.next_line()?.expect("record");
        line.decoded::<CompactRow>()?
    };
    assert_eq!(row.city.as_str(), "say \"hello\"");
    Ok(())
}

#[test]
fn record_parse_yields_compact_string() -> Result<(), Box<dyn StdError>> {
    let mut reader = unheaded(b"Boston,,Massachusetts\n");
    let mut line = reader.next_line()?.expect("record");
    let record = line.record()?;

    assert_eq!(record.parse::<CompactString>(0)?.as_deref(), Some("Boston"));
    assert_eq!(
        record.parse::<CompactString>(2)?.as_deref(),
        Some("Massachusetts")
    );
    // Past the end of the record, exactly as `String` behaves.
    assert_eq!(record.parse::<CompactString>(9)?, None);
    Ok(())
}

/// `decode_field_into` is what lets the fused path reuse an existing buffer.
/// It must overwrite, never append, and must leave a heap-grown value usable
/// once it shrinks back below the inline threshold.
#[test]
fn decode_field_into_overwrites() -> Result<(), Box<dyn StdError>> {
    let mut buf = CompactString::from("stale contents that will not be kept");

    buf.decode_field_into(Some(b"Boston"), 0, "city")?;
    assert_eq!(buf.as_str(), "Boston");

    let long = "a string comfortably past the inline threshold of 24 bytes";
    buf.decode_field_into(Some(long.as_bytes()), 0, "city")?;
    assert_eq!(buf.as_str(), long);

    buf.decode_field_into(Some(b"MA"), 0, "city")?;
    assert_eq!(buf.as_str(), "MA");

    // A missing field decodes as empty, matching `String`.
    buf.decode_field_into(None, 0, "city")?;
    assert_eq!(buf.as_str(), "");
    Ok(())
}

/// Invalid UTF-8 must fail identically to `String`, with the same kind and the
/// same reported location.
#[test]
fn invalid_utf8_matches_string() {
    let bytes = Some(b"ca\xffe".as_slice());

    let compact = <CompactString as DecodeField>::decode_field(bytes, 3, "city")
        .expect_err("invalid utf-8 must be rejected");
    let plain = <String as DecodeField>::decode_field(bytes, 3, "city").expect_err("invalid utf-8");

    assert!(matches!(compact.kind(), ErrorKind::InvalidUtf8(_)));
    assert_eq!(compact.location().field, plain.location().field);
    assert_eq!(compact.to_string(), plain.to_string());
}

#[test]
fn encodes_like_string() -> Result<(), Box<dyn StdError>> {
    let row = CompactRow {
        city: CompactString::from("Boston"),
        state: CompactString::from("Massachusetts"),
        population: 650_706,
    };
    let mut v = CollectVisitor::new();
    row.csv_encode(&mut v)?;
    assert_eq!(
        v.fields(),
        [
            b"Boston".as_slice(),
            b"Massachusetts".as_slice(),
            b"650706".as_slice()
        ]
    );
    Ok(())
}

/// The whole point of the type: these fields never touch the allocator.
#[test]
fn short_fields_stay_inline() {
    for field in ["", "MA", "Boston", "Massachusetts"] {
        let decoded =
            <CompactString as DecodeField>::decode_field(Some(field.as_bytes()), 0, "city")
                .expect("valid utf-8");
        assert!(!decoded.is_heap_allocated(), "{field:?} should be inline");
    }
}

/// `coseva`'s own `serde` feature turns on `compact_str`'s, but only when the
/// dependency is already present. This is the combination that wiring exists
/// for, so it needs a test that fails if the `compact_str?/serde` link is
/// dropped.
#[cfg(feature = "serde")]
#[test]
fn deserializes_through_serde() -> Result<(), Box<dyn StdError>> {
    let mut r = unheaded(b"Boston,650706\n");
    let mut line = r.next_line()?.expect("record");
    let (city, population): (CompactString, u64) = line.deserialized()?;
    assert_eq!(city.as_str(), "Boston");
    assert_eq!(population, 650_706);
    assert!(!city.is_heap_allocated());
    Ok(())
}

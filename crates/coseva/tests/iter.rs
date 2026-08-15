//! Integration tests for the owned-record iterators.
//!
//! Every iterator is checked against the cursor-free [`coseva::Line`] API
//! reading the same input, so the two ways of walking a parser cannot drift.
//! The `matching_` forms are checked against filtering by hand, which keeps
//! the pushdown scan honest rather than merely self-consistent.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;

use coseva::config::{FormatOptions, Headers, Limits, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::{ErrorKind, IoParser, Predicate, SliceParser};

const INPUT: &[u8] = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";

#[derive(Debug, CsvDecode, PartialEq)]
struct Row {
    city: String,
    country: String,
}

/// Build a slice parser that discovers headers from the first record.
fn slice() -> SliceParser<'static> {
    SliceParser::with_options(
        INPUT,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options")
}

/// Build a streaming parser over the same input.
fn streaming() -> IoParser<&'static [u8]> {
    IoParser::with_options(
        INPUT,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options")
}

// ── byte_records ───────────────────────────────────────────────────────────────

#[test]
fn byte_records_agree_with_the_line_api() -> Result<(), Box<dyn StdError>> {
    let mut parser = slice();
    let mut expected = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        expected.push(line.record()?.get(0).unwrap_or_default().to_vec());
    }

    let from_slice = slice()
        .byte_records()
        .map(|record| Ok(record?.get(0).unwrap_or_default().to_vec()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;
    let from_stream = streaming()
        .byte_records()
        .map(|record| Ok(record?.get(0).unwrap_or_default().to_vec()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;

    assert_eq!(from_slice, expected);
    assert_eq!(from_stream, expected);
    assert_eq!(
        expected,
        [b"Boston".to_vec(), b"Paris".into(), b"Denver".into()]
    );
    Ok(())
}

#[test]
fn text_records_agree_with_byte_records() -> Result<(), Box<dyn StdError>> {
    let bytes = slice()
        .byte_records()
        .map(|record| Ok(record?.get_str(0)?.unwrap_or_default().to_owned()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;
    let text = streaming()
        .text_records()
        .map(|record| Ok(record?.get(0).unwrap_or_default().to_owned()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;

    assert_eq!(bytes, text);
    assert_eq!(text, ["Boston", "Paris", "Denver"]);
    Ok(())
}

// ── filtering ──────────────────────────────────────────────────────────────────

#[test]
fn matching_iterators_agree_with_filtering_by_hand() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("country", "US");

    // Reference: walk every record and filter in the test.
    let mut parser = slice();
    let mut expected = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        if predicate.matches_field(record.get(1)) {
            expected.push(record.get_str(0)?.unwrap_or_default().to_owned());
        }
    }
    assert_eq!(expected, ["Boston", "Denver"]);

    let bytes = slice()
        .matching_byte_records(&predicate)
        .map(|record| Ok(record?.get_str(0)?.unwrap_or_default().to_owned()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;
    let text = streaming()
        .matching_text_records(&predicate)
        .map(|record| Ok(record?.get(0).unwrap_or_default().to_owned()))
        .collect::<Result<Vec<_>, coseva::Error>>()?;

    assert_eq!(bytes, expected);
    assert_eq!(text, expected);
    Ok(())
}

// ── decoded_records ────────────────────────────────────────────────────────────

#[test]
fn decoded_records_agree_across_parsers() -> Result<(), Box<dyn StdError>> {
    let from_slice = slice()
        .decoded_records::<Row>()
        .collect::<Result<Vec<_>, _>>()?;
    let from_stream = streaming()
        .decoded_records::<Row>()
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(from_slice, from_stream);
    assert_eq!(
        from_slice,
        [
            Row {
                city: "Boston".to_owned(),
                country: "US".to_owned()
            },
            Row {
                city: "Paris".to_owned(),
                country: "FR".to_owned()
            },
            Row {
                city: "Denver".to_owned(),
                country: "US".to_owned()
            },
        ]
    );
    Ok(())
}

#[test]
fn matching_decoded_records_skip_non_matches() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("country", "FR");

    let from_slice = slice()
        .matching_decoded_records::<Row>(&predicate)
        .collect::<Result<Vec<_>, _>>()?;
    let from_stream = streaming()
        .matching_decoded_records::<Row>(&predicate)
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(from_slice, from_stream);
    assert_eq!(
        from_slice,
        [Row {
            city: "Paris".to_owned(),
            country: "FR".to_owned()
        }]
    );
    Ok(())
}

// ── deserialized_records ───────────────────────────────────────────────────────

#[cfg(feature = "serde")]
#[test]
fn deserialized_records_agree_with_decoded_records() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SerdeRow {
        city: String,
        country: String,
    }

    let decoded = slice()
        .decoded_records::<Row>()
        .collect::<Result<Vec<_>, _>>()?;
    let deserialized = streaming()
        .deserialized_records::<SerdeRow>()
        .collect::<Result<Vec<_>, _>>()?;

    let paired = decoded
        .iter()
        .zip(&deserialized)
        .all(|(a, b)| a.city == b.city && a.country == b.country);
    assert_eq!(decoded.len(), deserialized.len());
    assert!(paired, "the two typed iterators disagree");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn matching_deserialized_records_skip_non_matches() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SerdeRow {
        city: String,
        country: String,
    }

    let predicate = Predicate::equals("country", "US");
    let rows = slice()
        .matching_deserialized_records::<SerdeRow>(&predicate)
        .collect::<Result<Vec<_>, _>>()?;

    assert_eq!(
        rows,
        [
            SerdeRow {
                city: "Boston".to_owned(),
                country: "US".to_owned()
            },
            SerdeRow {
                city: "Denver".to_owned(),
                country: "US".to_owned()
            },
        ]
    );
    Ok(())
}

// ── failure ────────────────────────────────────────────────────────────────────

#[test]
fn an_iterator_reports_a_failure_once_and_then_ends() {
    let mut parser = SliceParser::with_options(
        b"city\nBos\"ton\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut records = parser.byte_records();
    records
        .next()
        .expect("header record")
        .expect("valid header");
    assert!(
        records.next().expect("a failure").is_err(),
        "quote inside unquoted field"
    );
    assert!(records.next().is_none(), "the run ends after a failure");
}

// ── record extents ─────────────────────────────────────────────────────────────

/// Both views of a record must place it at the same spot in the stream, for a
/// parser that drops the bytes behind it as well as for one that does not.
#[test]
fn borrowed_and_owned_extents_agree_against_the_stream() -> Result<(), Box<dyn StdError>> {
    use coseva::ByteRecord;

    // A capacity far below the input forces the window to drop consumed bytes,
    // so a window-relative range would disagree with a stream-relative one.
    let mut parser = IoParser::with_options(
        INPUT,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(8),
    )?;

    let mut owned = ByteRecord::new();
    let mut expected = 0;
    while let Some(mut line) = parser.next_line()? {
        let borrowed = line.record()?.byte_range();
        line.read_byte_record_into(&mut owned)?;
        assert_eq!(borrowed, owned.byte_range(), "the two views disagree");
        assert_eq!(borrowed.start, expected, "range is not stream-relative");
        expected = borrowed.end;
    }
    assert_eq!(expected, INPUT.len());
    Ok(())
}

/// A failure surfaced by the scan itself, rather than by materializing the
/// record, must also end the run exactly once.
///
/// The `matching_` forms evaluate their predicate during the scan, so a record
/// that cannot be split into fields fails before any view of it is taken.
#[test]
fn a_scan_failure_ends_the_run_once() {
    let predicate = Predicate::equals(0, "ton");
    let mut parser = SliceParser::with_options(
        b"city\nBos\"ton\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut records = parser.matching_byte_records(&predicate);
    assert!(
        records.next().expect("a failure").is_err(),
        "the pushdown scan must reject a quote inside an unquoted field"
    );
    assert!(records.next().is_none(), "the run ends after a failure");
}

/// The same scan failure must end a typed run, whose mapping is resolved
/// before the cursor ever moves.
#[test]
fn a_scan_failure_ends_a_typed_run_once() {
    let predicate = Predicate::equals(0, "ton");
    let mut parser = SliceParser::with_options(
        b"city,country\nBos\"ton,US\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options");

    let mut records = parser.matching_decoded_records::<Row>(&predicate);
    let _ = records
        .next()
        .expect("a failure")
        .expect_err("the scan fails once");
    assert!(records.next().is_none(), "the run ends after a failure");
}

// ── fused behavior ──────────────────────────────────────────────────────────────

/// A `byte_records` iterator keeps returning `None` on every call after the
/// underlying stream is exhausted, rather than resuming or panicking.
#[test]
fn byte_records_iterator_is_fused() {
    let mut parser = SliceParser::with_options(
        b"a,b\nc,d\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut iter = parser.byte_records();
    assert!(iter.next().is_some());
    assert!(iter.next().is_some());
    // First `None` marks the iterator done.
    assert!(iter.next().is_none());
    // A further call must keep returning `None` rather than resuming.
    assert!(iter.next().is_none());
}

/// A record that exceeds the configured size limit surfaces as `Some(Err(_))`
/// from the iterator rather than aborting the run silently.
#[test]
fn byte_records_iterator_propagates_advance_error() {
    let mut parser = SliceParser::with_options(
        b"toolong\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::new(3, 3, 1024)),
    )
    .expect("valid options");
    let mut iter = parser.byte_records();
    // "toolong" is 7 bytes, exceeding the 3-byte limit.
    let item = iter
        .next()
        .expect("Some result")
        .expect_err("record too large");
    assert!(matches!(
        item.kind(),
        ErrorKind::RecordTooLarge { .. } | ErrorKind::FieldTooLarge { .. }
    ));
}

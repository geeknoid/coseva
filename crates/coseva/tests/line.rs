//! Integration tests for the cursor-free [`coseva::Line`] API.
//!
//! The three parsers reach records by different means but expose the same
//! views once positioned, so these tests drive every view through each parser
//! and assert the results agree.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io::Cursor;

use coseva::config::{FormatOptions, Headers, ParseOptions, ReadBom};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::{ByteRecord, ErrorKind, IoParser, Predicate, PushParser, SliceParser, TextRecord};

const INPUT: &[u8] = b"city,pop\nBoston,650706\nLondon,8982000\n\"Sao \"\"Paulo\"\"\",11451999\n";

#[derive(Debug, CsvDecode, PartialEq)]
struct City {
    city: String,
    pop: u64,
}

#[derive(Debug, CsvDecode, PartialEq)]
struct BorrowedCity<'row> {
    city: &'row str,
}

// ── SliceParser ────────────────────────────────────────────────────────────────

#[test]
fn slice_lines_expose_every_view() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    assert_eq!(
        parser.headers()?.expect("headers").get(0),
        Some(&b"city"[..])
    );

    let mut bytes = ByteRecord::new();
    let mut text = TextRecord::new();
    let mut owned = City {
        city: String::new(),
        pop: 0,
    };
    let mut seen = Vec::new();

    while let Some(mut line) = parser.next_line()? {
        // Views of one line are repeatable and mixable.
        assert_eq!(line.record()?.len(), 2);
        line.read_byte_record_into(&mut bytes)?;
        line.read_text_record_into(&mut text)?;
        line.decode_into(&mut owned)?;
        let borrowed: BorrowedCity<'_> = line.decoded()?;

        assert_eq!(bytes.get_str(0)?, Some(text.get(0).expect("field")));
        assert_eq!(borrowed.city, owned.city);
        seen.push((owned.city.clone(), owned.pop));
    }

    assert_eq!(
        seen,
        [
            ("Boston".to_owned(), 650_706),
            ("London".to_owned(), 8_982_000),
            ("Sao \"Paulo\"".to_owned(), 11_451_999),
        ]
    );
    Ok(())
}

#[test]
fn slice_next_line_interleaves_with_parser_queries() -> Result<(), Box<dyn StdError>> {
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let mut cities = Vec::new();

    // Unlike `iter`, `next_line` releases the borrow between records, so the
    // parser stays queryable inside the loop.
    while let Some(mut line) = parser.next_line()? {
        cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
        assert!(parser.header_index("pop")?.is_some());
    }

    assert_eq!(cities.len(), 3);
    assert_eq!(
        parser.location().record,
        4,
        "three data records plus the header"
    );
    Ok(())
}

#[test]
fn slice_matching_skips_non_candidates() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("city", "London");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let mut hits = Vec::new();

    while let Some(mut line) = parser.next_matching_line(&predicate)? {
        hits.push(line.decoded::<City>()?);
    }

    assert_eq!(
        hits,
        [City {
            city: "London".to_owned(),
            pop: 8_982_000
        }]
    );
    Ok(())
}

#[test]
fn slice_lines_report_errors_once_and_then_stop() {
    let mut parser = SliceParser::with_options(
        b"a\nb\"c\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut first = parser.next_line().expect("advance").expect("record");
    first.record().expect("valid first record");

    let mut second = parser.next_line().expect("advance").expect("record");
    assert!(second.record().is_err(), "quote inside unquoted field");
}

// ── IoParser ────────────────────────────────────────────────────────────

#[test]
fn streaming_lines_expose_every_view() -> Result<(), Box<dyn StdError>> {
    let mut parser = IoParser::<_, Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let mut bytes = ByteRecord::new();
    let mut seen = Vec::new();

    while let Some(mut line) = parser.next_line()? {
        let decoded: City = line.decoded()?;
        line.read_byte_record_into(&mut bytes)?;
        assert_eq!(bytes.get_str(0)?, Some(decoded.city.as_str()));
        seen.push(decoded.pop);
    }

    assert_eq!(seen, [650_706, 8_982_000, 11_451_999]);
    Ok(())
}

#[test]
fn streaming_matching_agrees_with_unfiltered_lines() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("city", "Boston");
    let mut parser = IoParser::<_, Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    let pop = {
        let mut line = parser.next_matching_line(&predicate)?.expect("match");
        line.record()?.get(1).map(<[u8]>::to_vec)
    };
    assert_eq!(pop.as_deref(), Some(&b"650706"[..]));

    assert!(
        parser.next_matching_line(&predicate)?.is_none(),
        "exhausted"
    );
    Ok(())
}

// ── PushParser ─────────────────────────────────────────────────────────────────

#[test]
fn push_lines_drain_between_chunks() -> Result<(), Box<dyn StdError>> {
    let chunks: [&[u8]; 3] = [b"city,pop\nBos", b"ton,650706\nLond", b"on,8982000"];
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let mut cities = Vec::new();

    for bytes in chunks {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut chunk = parser.chunk(&bytes[offset..]);
            // Each line's borrow ends with its iteration, so the guard is free
            // to reach the next record on the next pass.
            while let Some(mut line) = chunk.next_line()? {
                cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
            }
            offset += chunk.done();
        }
    }

    // The last record has no terminator, so it stays pending until the stream
    // is declared complete.
    assert!(!parser.is_done());
    parser.finish();
    let mut chunk = parser.chunk(b"");
    while let Some(mut line) = chunk.next_line()? {
        cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
    }
    drop(chunk);

    assert_eq!(cities, [b"Boston".to_vec(), b"London".to_vec()]);
    assert!(parser.is_done());
    Ok(())
}

#[test]
fn push_lines_decode_and_match() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("city", "London");
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    parser.finish();
    let mut chunk = parser.chunk(INPUT);

    let mut hits = Vec::new();
    while let Some(mut line) = chunk.next_matching_line(&predicate)? {
        hits.push(line.decoded::<City>()?);
    }
    drop(chunk);

    assert_eq!(
        hits,
        [City {
            city: "London".to_owned(),
            pop: 8_982_000
        }]
    );
    Ok(())
}

// ── next_matching_line ─────────────────────────────────────────────────────────

#[test]
fn slice_next_matching_line_agrees_with_manual_filtering() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("city", "London");

    // Reference: walk every line and filter in the test.
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let mut expected = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        if predicate.matches_field(record.get(0)) {
            expected.push(record.get_str(0)?.expect("city").to_owned());
        }
    }

    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");
    let mut actual = Vec::new();
    while let Some(mut line) = parser.next_matching_line(&predicate)? {
        actual.push(line.record()?.get_str(0)?.expect("city").to_owned());
    }

    assert_eq!(actual, expected);
    assert_eq!(actual, vec!["London".to_owned()]);
    Ok(())
}

#[test]
fn streaming_next_matching_line_skips_non_matches() -> Result<(), Box<dyn StdError>> {
    let predicate = Predicate::equals("city", "London");
    let mut parser = IoParser::<_, Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    let pop = {
        let mut line = parser.next_matching_line(&predicate)?.expect("a match");
        line.record()?.get(1).map(<[u8]>::to_vec)
    };
    assert_eq!(pop.as_deref(), Some(&b"8982000"[..]));

    assert!(
        parser.next_matching_line(&predicate)?.is_none(),
        "only one line matches"
    );
    Ok(())
}

#[test]
fn push_next_matching_line_matches_the_iterator() -> Result<(), Box<dyn StdError>> {
    let chunks: [&[u8]; 3] = [b"city,pop\nBos", b"ton,650706\nLond", b"on,8982000\n"];
    let predicate = Predicate::equals("city", "London");
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let mut cities = Vec::new();

    for bytes in chunks {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut chunk = parser.chunk(&bytes[offset..]);
            // Drained with the cursor-free form, so the guard is never
            // borrowed between matches.
            while let Some(mut line) = chunk.next_matching_line(&predicate)? {
                cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
            }
            offset += chunk.done();
        }
    }
    parser.finish();
    let mut chunk = parser.chunk(b"");
    while let Some(mut line) = chunk.next_matching_line(&predicate)? {
        cities.push(line.record()?.get(0).unwrap_or_default().to_vec());
    }
    drop(chunk);

    assert_eq!(cities, vec![b"London".to_vec()]);
    Ok(())
}

#[test]
fn next_matching_line_interleaves_with_parser_queries() -> Result<(), Box<dyn StdError>> {
    // Filtering must not lock the parser away: it stays queryable between
    // matched lines.
    let predicate = Predicate::equals("city", "London");
    let mut parser = SliceParser::<Csv>::new(INPUT, ParseOptions::new()).expect("parser");

    let city = {
        let mut line = parser.next_matching_line(&predicate)?.expect("a match");
        line.record()?.get(0).map(<[u8]>::to_vec)
    };
    assert_eq!(city.as_deref(), Some(&b"London"[..]));

    assert_eq!(
        parser.headers()?.expect("headers").get(1),
        Some(&b"pop"[..]),
        "the parser stays queryable between filtered lines"
    );
    Ok(())
}

/// A snapshot of everything a borrowed record reports about itself.
fn snapshot(record: &coseva::Record<'_>) -> (Vec<Vec<u8>>, core::ops::Range<usize>, u64) {
    (
        record.iter().map(<[u8]>::to_vec).collect(),
        record.byte_range(),
        record.index(),
    )
}

/// `Line` documents every view as repeatable and mixable in any order. The
/// windowed parsers assemble records out of a sliding buffer, so a view that
/// consumed engine state instead of reading it would corrupt the next one.
#[test]
fn streaming_line_views_are_repeatable_in_any_order() {
    for input in [
        &b"a,b\n1,2\n3,4\n"[..],
        &b"a,b\n\"x,y\",\"q\"\"r\"\n5,6\n"[..],
    ] {
        for capacity in [1usize, 3, 8, 4096] {
            let mut parser = IoParser::with_options(
                std::io::Cursor::new(input.to_vec()),
                FormatOptions::CSV,
                ParseOptions::new().buffer_capacity(capacity),
            )
            .expect("valid options");
            let mut bytes = ByteRecord::new();
            let mut text = TextRecord::new();
            while let Some(mut line) = parser.next_line().expect("streaming advances") {
                let expected = snapshot(&line.record().expect("borrowed record"));

                line.read_byte_record_into(&mut bytes).expect("owned bytes");
                assert_eq!(
                    expected,
                    snapshot(&line.record().expect("borrowed record again")),
                    "record() drifted after an owned read at capacity {capacity} on {input:?}",
                );
                assert_eq!(expected.1, bytes.byte_range(), "capacity {capacity}");
                assert_eq!(expected.2, bytes.index(), "capacity {capacity}");
                assert_eq!(
                    expected.0,
                    bytes.iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                    "capacity {capacity}",
                );

                let mut repeat = ByteRecord::new();
                line.read_byte_record_into(&mut repeat)
                    .expect("owned bytes again");
                assert_eq!(
                    bytes.byte_range(),
                    repeat.byte_range(),
                    "capacity {capacity}"
                );

                line.read_text_record_into(&mut text).expect("owned text");
                assert_eq!(expected.1, text.byte_range(), "capacity {capacity}");
                let mut repeat_text = TextRecord::new();
                line.read_text_record_into(&mut repeat_text)
                    .expect("owned text again");
                assert_eq!(expected.1, repeat_text.byte_range(), "capacity {capacity}");
            }
        }
    }
}

/// The same repeatability contract, driven through the push parser, whose
/// window is advanced by the caller's chunk boundaries rather than by reads.
#[test]
fn push_line_views_are_repeatable_in_any_order() {
    let input = &b"a,b\n1,2\n\"x,y\",z\n"[..];
    for chunk in [1usize, 3, 64] {
        let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
        let mut bytes = ByteRecord::new();
        let mut text = TextRecord::new();
        let mut ranges = Vec::new();
        let mut fed = 0;
        while fed < input.len() {
            let end = (fed + chunk).min(input.len());
            if end == input.len() {
                parser.finish();
            }
            let mut guard = parser.chunk(&input[fed..end]);
            while let Some(mut line) = guard.next_line().expect("push advances") {
                let expected = snapshot(&line.record().expect("borrowed record"));

                line.read_byte_record_into(&mut bytes).expect("owned bytes");
                assert_eq!(
                    expected,
                    snapshot(&line.record().expect("borrowed record again")),
                    "record() drifted after an owned read at chunk {chunk}",
                );
                assert_eq!(expected.1, bytes.byte_range(), "chunk {chunk}");

                line.read_text_record_into(&mut text).expect("owned text");
                assert_eq!(expected.1, text.byte_range(), "chunk {chunk}");
                let mut repeat_text = TextRecord::new();
                line.read_text_record_into(&mut repeat_text)
                    .expect("owned text again");
                assert_eq!(expected.1, repeat_text.byte_range(), "chunk {chunk}");

                ranges.push(expected.1);
            }
            fed += guard.done();
        }
        assert_eq!(
            ranges.len(),
            2,
            "chunk {chunk} yielded the wrong record count"
        );
        assert_eq!(ranges[0], 4..8, "chunk {chunk} first data record range");
    }
}

// ── failure poisons a tracking parser ──────────────────────────────────────────

/// A failed in-place decode must poison the parsers that track failure, so the
/// run cannot silently continue past a record it never interpreted.
#[test]
fn a_failed_decode_into_poisons_a_streaming_parser() {
    const BAD: &[u8] = b"city,pop\nBoston,not-a-number\nLondon,8982000\n";

    let mut parser = IoParser::with_options(
        BAD,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options");

    let mut owned = City {
        city: String::new(),
        pop: 0,
    };
    let mut line = parser
        .next_line()
        .expect("the first record is reached")
        .expect("a record");
    let error = line
        .decode_into(&mut owned)
        .expect_err("`not-a-number` is not a `u64`");
    assert_eq!(error.location().field, 1);

    assert!(
        parser.next_line().is_err(),
        "the parser stays poisoned after a failed decode"
    );
}

/// The same holds for a push parser, which also tracks failure.
#[test]
fn a_failed_decode_into_poisons_a_push_parser() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid options");
    parser.finish();
    let mut chunk = parser.chunk(b"city,pop\nBoston,not-a-number\n");

    let mut owned = City {
        city: String::new(),
        pop: 0,
    };
    let mut line = chunk
        .next_line()
        .expect("the first record is reached")
        .expect("a record");
    assert!(
        line.decode_into(&mut owned).is_err(),
        "`not-a-number` is not a `u64`"
    );
    assert!(
        chunk.next_line().is_err(),
        "the parser stays poisoned after a failed decode"
    );
}

// ── decoding views borrow parser storage ────────────────────────────────────────

/// `read_text_record_into` decodes a text view directly from the parser's
/// storage.
#[test]
fn line_read_text_record_into() -> Result<(), Box<dyn StdError>> {
    let mut parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,650706\n", ParseOptions::new()).expect("parser");
    let mut text = TextRecord::new();
    let mut line = parser.next_line()?.expect("record");
    line.read_text_record_into(&mut text)?;
    assert_eq!(text.get(0), Some("Boston"));
    assert_eq!(text.get(1), Some("650706"));
    Ok(())
}

/// A typed view via `decoded` borrows from the parser's storage rather than
/// allocating a fresh copy.
#[test]
fn line_decoded_borrows_from_parser_storage() -> Result<(), Box<dyn StdError>> {
    let mut parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,650706\n", ParseOptions::new()).expect("parser");
    let mut line = parser.next_line()?.expect("record");
    let city: City = line.decoded()?;
    assert_eq!(city.city, "Boston");
    assert_eq!(city.pop, 650_706);
    Ok(())
}

/// `decode_into` reuses an existing value's allocations instead of building a
/// fresh one for every record.
#[test]
fn line_decode_into_reuses_allocations() -> Result<(), Box<dyn StdError>> {
    let mut city = City {
        city: String::new(),
        pop: 0,
    };
    let mut parser = SliceParser::<Csv>::new(
        b"city,pop\nBoston,650706\nLondon,8982000\n",
        ParseOptions::new(),
    )
    .expect("parser");
    let mut line = parser.next_line()?.expect("first record");
    line.decode_into(&mut city)?;
    assert_eq!(city.city, "Boston");
    line = parser.next_line()?.expect("second record");
    line.decode_into(&mut city)?;
    assert_eq!(city.city, "London");
    Ok(())
}

/// `deserialized` decodes a serde-derived type from the same record view.
#[cfg(feature = "serde")]
#[test]
fn line_deserialized_via_serde() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SerdeCity {
        city: String,
        pop: u64,
    }

    let mut parser =
        SliceParser::<Csv>::new(b"city,pop\nBoston,650706\n", ParseOptions::new()).expect("parser");
    let mut line = parser.next_line()?.expect("record");
    let city: SerdeCity = line.deserialized()?;
    assert_eq!(city.city, "Boston");
    assert_eq!(city.pop, 650_706);
    Ok(())
}

// ── a rejected BOM fails every view ──────────────────────────────────────────────

/// `read_byte_record_into` fails when the record still carries a rejected
/// BOM.
#[test]
fn line_fail_and_check_bom_via_streaming_bom_reject() -> Result<(), Box<dyn StdError>> {
    let bom_input = b"\xEF\xBB\xBFa,b\nc,d\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let mut record = ByteRecord::new();
    let err = line
        .read_byte_record_into(&mut record)
        .expect_err("rejected BOM should fail the view");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

/// `read_text_record_into` also fails on a rejected BOM.
#[test]
fn line_read_text_record_into_bom_rejected() -> Result<(), Box<dyn StdError>> {
    let bom_input = b"\xEF\xBB\xBFa,b\nc,d\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let mut text = TextRecord::new();
    let err = line
        .read_text_record_into(&mut text)
        .expect_err("rejected BOM should fail text view");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

/// `decoded` fails on a rejected BOM.
#[test]
fn line_decoded_bom_rejected() -> Result<(), Box<dyn StdError>> {
    let bom_input = b"\xEF\xBB\xBFcity,pop\na,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let err = line
        .decoded::<City>()
        .expect_err("rejected BOM should fail decoded");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

/// `decode_into` fails on a rejected BOM.
#[test]
fn line_decode_into_bom_rejected() -> Result<(), Box<dyn StdError>> {
    let bom_input = b"\xEF\xBB\xBFcity,pop\na,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let mut city = City {
        city: String::new(),
        pop: 0,
    };
    let err = line
        .decode_into(&mut city)
        .expect_err("rejected BOM should fail decode_into");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

/// `deserialized` fails on a rejected BOM.
#[cfg(feature = "serde")]
#[test]
fn line_deserialized_bom_rejected() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SerdeCity {
        city: String,
        pop: u64,
    }

    let bom_input = b"\xEF\xBB\xBFcity,pop\na,1\n";
    let mut parser = IoParser::with_options(
        Cursor::new(bom_input.as_slice()),
        FormatOptions::CSV.read_bom(ReadBom::Reject),
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("positioned on BOM record");
    let err = line
        .deserialized::<SerdeCity>()
        .expect_err("rejected BOM should fail deserialized");
    assert_eq!(err.kind(), ErrorKind::RejectedBom);
    Ok(())
}

// ── a bad field fails decode and deserialize views ──────────────────────────────

/// Invalid UTF-8 in a field fails `read_text_record_into` on a `SliceParser`.
#[test]
fn line_read_text_record_into_invalid_utf8_on_slice_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"city\n\xFF\n";
    let mut parser = SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())?;
    let mut line = parser.next_line()?.expect("bad record");
    let mut text = TextRecord::new();
    let err = line
        .read_text_record_into(&mut text)
        .expect_err("invalid UTF-8");
    assert!(matches!(
        err.kind(),
        ErrorKind::InvalidUtf8(_) | ErrorKind::Io(_)
    ));
    Ok(())
}

/// A multi-byte sequence split across two fields fails `read_text_record_into`.
///
/// Delimiters are stripped from the record buffer, so the two invalid halves
/// end up adjacent and the concatenation alone reads as valid UTF-8. Each
/// field is still invalid on its own and must be rejected.
#[test]
fn line_read_text_record_into_rejects_sequence_split_across_fields() -> Result<(), Box<dyn StdError>>
{
    let input = b"\xC3,\xA9\n";
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("bad record");
    let mut text = TextRecord::new();
    let err = line
        .read_text_record_into(&mut text)
        .expect_err("neither half is valid UTF-8");
    assert!(matches!(err.kind(), ErrorKind::InvalidUtf8(_)));
    Ok(())
}

/// A field that fails type conversion fails `decode_into` on a `SliceParser`.
#[test]
fn line_decode_into_field_error_on_slice_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"city,pop\nBoston,notanumber\n";
    let mut parser = SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())?;
    let mut line = parser.next_line()?.expect("bad record");
    let mut city = City {
        city: String::new(),
        pop: 0,
    };
    line.decode_into(&mut city)
        .expect_err("bad field should fail");
    Ok(())
}

/// A field that fails type conversion also fails `deserialized` on a
/// `SliceParser`.
#[cfg(feature = "serde")]
#[test]
fn line_deserialized_field_error_on_slice_parser() -> Result<(), Box<dyn StdError>> {
    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct SerdeCity {
        city: String,
        pop: u64,
    }

    let input = b"city,pop\nBoston,notanumber\n";
    let mut parser = SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())?;
    let mut line = parser.next_line()?.expect("bad record");
    line.deserialized::<SerdeCity>()
        .expect_err("bad field should fail");
    Ok(())
}

// ── a record error poisons a tracking parser ────────────────────────────────────

/// A malformed record fails `record()` and leaves the producing streaming
/// parser marked as failed.
#[test]
fn line_record_error_poisons_streaming_parser() -> Result<(), Box<dyn StdError>> {
    let input = b"city\n\"unclosed\n";
    let mut parser = IoParser::with_options(
        Cursor::new(input.as_slice()),
        FormatOptions::CSV,
        ParseOptions::new(),
    )?;
    let mut line = parser.next_line()?.expect("positioned on bad record");
    let err = line.record().expect_err("malformed record should fail");
    assert!(matches!(err.kind(), ErrorKind::UnterminatedQuotedField));
    Ok(())
}

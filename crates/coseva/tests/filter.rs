//! Filtering behaves exactly like manually filtering every record.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::ErrorKind;
use coseva::Predicate;
use coseva::PushParser;
use coseva::SliceParser;
use coseva::config::{
    BlankRecords, Escape, FormatOptions, Headers, ParseOptions, RecordEnding, Whitespace,
};
use coseva::{Column, MatchKind};

/// Collect matches via the filtering fast path.
fn filtered(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
) -> Vec<Vec<Vec<u8>>> {
    let mut parser = SliceParser::with_options(input, format, options).expect("valid options");
    let mut records = Vec::new();
    while let Some(mut line) = parser
        .next_matching_line(predicate)
        .expect("parse succeeds")
    {
        let record = line.record().expect("parse succeeds");
        records.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    records
}

/// Collect matches by parsing everything and filtering in the application.
fn reference(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
    column: usize,
) -> Vec<Vec<Vec<u8>>> {
    let mut parser = SliceParser::with_options(input, format, options).expect("valid options");
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line().expect("parse succeeds") {
        let record = line.record().expect("parse succeeds");
        if predicate.matches_field(record.get(column)) {
            records.push(record.iter().map(<[u8]>::to_vec).collect());
        }
    }
    records
}

/// Assert the fast path and the reference agree.
fn assert_agrees(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
    column: usize,
) -> Vec<Vec<Vec<u8>>> {
    let expected = reference(input, format, options.clone(), predicate, column);
    let actual = filtered(input, format, options, predicate);
    assert_eq!(actual, expected);
    actual
}

// ── Predicate construction and matching ──────────────────────────────────────────

/// `Predicate::equals/contains/starts_with/ends_with` correctly match (or
/// reject) field bytes, including a missing (`None`) field.
#[test]
fn match_kinds_evaluate_against_field_bytes() {
    let equals = Predicate::equals(1, "US");
    assert!(equals.matches_field(Some(b"US")));
    assert!(!equals.matches_field(Some(b"USA")));
    assert!(!equals.matches_field(None));

    let contains = Predicate::contains(1, "os");
    assert!(contains.matches_field(Some(b"Boston")));
    assert!(!contains.matches_field(Some(b"Paris")));

    let starts = Predicate::starts_with(1, "Bo");
    assert!(starts.matches_field(Some(b"Boston")));
    assert!(!starts.matches_field(Some(b"Aboston")));

    let ends = Predicate::ends_with(1, "on");
    assert!(ends.matches_field(Some(b"Boston")));
    assert!(!ends.matches_field(Some(b"Bostonia")));
}

/// A `Predicate` built from an index or a name reports the matching
/// `Column`, `MatchKind`, and literal bytes it was constructed with.
#[test]
fn column_accepts_index_or_name() {
    assert_eq!(*Predicate::equals(2, "x").column(), Column::Index(2));
    assert_eq!(
        *Predicate::equals("country", "x").column(),
        Column::Name("country".into())
    );
    assert_eq!(Predicate::contains(0, "x").kind(), MatchKind::Contains);
    assert_eq!(Predicate::equals(0, "abc").literal(), b"abc");
}

/// A borrowed column name is indistinguishable from an owned one, so routing
/// a static name through `Column::borrowed` to skip the allocation cannot
/// change what a predicate matches.
#[test]
fn a_borrowed_column_name_behaves_like_an_owned_one() {
    let borrowed = Column::borrowed("country");
    let owned = Column::from("country");
    let from_string = Column::from(String::from("country"));

    assert_eq!(borrowed, owned);
    assert_eq!(borrowed, from_string);
    assert_eq!(borrowed.name(), Some("country"));
    assert_eq!(Column::Index(1).name(), None);

    let input = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";
    let options = ParseOptions::new().headers(Headers::FirstRecord);
    let by_borrowed = filtered(
        input,
        FormatOptions::CSV,
        options.clone(),
        &Predicate::equals(Column::borrowed("country"), "US"),
    );
    let by_owned = filtered(
        input,
        FormatOptions::CSV,
        options,
        &Predicate::equals("country", "US"),
    );
    assert_eq!(by_borrowed, by_owned);
    assert_eq!(by_borrowed.len(), 2);
}

/// A name whose lifetime is shorter than `'static` still converts, since the
/// allocating conversion is the one kept when the two overlapped.
#[test]
fn a_column_name_may_be_shorter_lived_than_the_column() {
    let column = {
        let name = String::from("country");
        Column::from(name.as_str())
    };
    assert_eq!(column.name(), Some("country"));
}

#[test]
fn filters_by_column_index() {
    let input = b"Boston,US\nParis,FR\nDenver,US\nLyon,FR\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0][0], b"Boston");
    assert_eq!(matches[1][0], b"Denver");
}

#[test]
fn filters_by_header_name() {
    let input = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";
    let predicate = Predicate::equals("country", "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn an_unknown_header_matches_nothing() {
    let input = b"city,country\nBoston,US\n";
    let predicate = Predicate::equals("region", "US");
    let matches = filtered(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
    );
    assert!(matches.is_empty());
}

#[test]
fn the_header_record_is_never_returned_as_a_match() {
    // The literal appears in the header line as well as in the data.
    let input = b"city,country\nUS,US\nParis,FR\n";
    let predicate = Predicate::equals("city", "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
        0,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0][0], b"US");
}

#[test]
fn a_literal_hit_in_another_column_does_not_match() {
    let input = b"US,FR\nParis,US\nBerlin,DE\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0][0], b"Paris");
}

#[test]
fn quoted_records_before_a_hit_are_walked_correctly() {
    // The quoted field holds delimiters and newlines, so terminator counting
    // cannot be trusted and the parser must walk these records.
    let input = b"a,\"x,y\nz\"\nb,US\n\"c\nc\",q\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
    assert_eq!(matches[0][0], b"b");
    assert_eq!(matches[1][0], b"d");
}

#[test]
fn a_match_inside_a_quoted_field_is_found() {
    let input = b"a,\"US\"\nb,FR\nc,\"US\"\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn a_literal_split_by_an_escape_is_still_found() {
    // The decoded second field is `a"b`, which no raw-byte scan for `a"b`
    // could locate, so the predicate must disable the literal skip.
    let input = b"one,\"a\"\"b\"\ntwo,plain\n";
    let predicate = Predicate::equals(1, "a\"b");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0][0], b"one");
}

#[test]
fn a_literal_containing_a_delimiter_is_still_found() {
    let input = b"one,\"a,b\"\ntwo,plain\n";
    let predicate = Predicate::equals(1, "a,b");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn a_literal_containing_a_newline_is_still_found() {
    let input = b"one,\"a\nb\"\ntwo,plain\n";
    let predicate = Predicate::equals(1, "a\nb");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn an_empty_literal_matches_every_record() {
    let input = b"a,\nb,x\nc,\n";
    let predicate = Predicate::equals(1, "");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn contains_starts_with_and_ends_with_agree_with_the_reference() {
    let input = b"alpha,prefix-mid-suffix\nbeta,other\ngamma,prefix-only\n";
    for predicate in [
        Predicate::contains(1, "mid"),
        Predicate::starts_with(1, "prefix"),
        Predicate::ends_with(1, "suffix"),
    ] {
        assert_agrees(
            input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
            &predicate,
            1,
        );
    }
}

#[test]
fn the_last_record_without_a_trailing_terminator_is_found() {
    let input = b"a,FR\nb,US";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0][0], b"b");
}

#[test]
fn consecutive_matches_are_all_returned() {
    let input = b"a,US\nb,US\nc,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 3);
}

#[test]
fn trimming_still_matches_the_reference() {
    let input = b"a ,  US  \nb, FR \nc,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV.trim(Whitespace::ALL),
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn skipped_blank_records_still_match_the_reference() {
    let input = b"a,US\n\n\nb,FR\n\nc,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV.blank_records(BlankRecords::Skip),
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn comment_lines_still_match_the_reference() {
    let input = b"#note US\na,US\n#another\nb,FR\nc,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV.comment(Some(b'#')),
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn crlf_endings_still_match_the_reference() {
    let input = b"a,US\r\nb,FR\r\nc,US\r\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn backslash_escapes_still_match_the_reference() {
    let input = b"a,US\nb,FR\nc,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn tab_separated_input_matches_the_reference() {
    let input = b"a\tUS\nb\tFR\nc\tUS\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_agrees(
        input,
        FormatOptions::TSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn filtering_can_resume_after_plain_parsing() {
    let input = b"a,US\nb,FR\nc,US\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"a"[..])
    );

    // Interleave a plain read; it must return the very next physical record.
    let mut line = parser
        .next_line()
        .expect("parse succeeds")
        .expect("a record");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"b"[..])
    );

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"c"[..])
    );
}

#[test]
fn a_large_input_agrees_with_the_reference() {
    // Exercise the SIMD paths with a needle that is rare and far apart.
    let mut input = Vec::new();
    let mut expected = 0;
    for index in 0..5_000 {
        if index % 997 == 0 {
            input.extend_from_slice(b"row,NEEDLE,tail\n");
            expected += 1;
        } else if index % 13 == 0 {
            // A quoted record forces the walk path for the following span.
            input.extend_from_slice(b"row,\"quoted,value\nsecond line\",tail\n");
        } else {
            input.extend_from_slice(b"row,filler,tail\n");
        }
    }

    let predicate = Predicate::equals(1, "NEEDLE");
    let matches = assert_agrees(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), expected);
}

#[test]
fn record_positions_survive_skipping() {
    // Errors are reported against the physical record, so the index and line
    // the parser tracks must stay correct across skipped spans.
    let input = b"a,FR\nb,FR\nc,US\nd,FR\ne,US\n";
    let predicate = Predicate::equals(1, "US");
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(line.record().expect("parse succeeds").index(), 2);

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(line.record().expect("parse succeeds").index(), 4);
}

/// A tiny deterministic xorshift generator, so failures are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).expect("bound fits in usize")
    }
}

#[test]
fn randomized_inputs_never_lose_a_match() {
    // Field bytes deliberately include quotes, delimiters and newlines so
    // that escaping, multi-line records and literal splitting all occur.
    const ALPHABET: &[u8] = b"ab\"c,d\ne";

    for seed in 1..200_u64 {
        let mut rng = Rng(seed);
        let mut input = Vec::new();
        let records = 1 + rng.below(40);

        for _ in 0..records {
            let fields = 1 + rng.below(3);
            for field in 0..fields {
                if field > 0 {
                    input.push(b',');
                }
                let mut value = Vec::new();
                for _ in 0..rng.below(6) {
                    value.push(ALPHABET[rng.below(ALPHABET.len())]);
                }
                // Quote whenever the value is structurally ambiguous.
                if value
                    .iter()
                    .any(|byte| matches!(byte, b'"' | b',' | b'\n' | b'\r'))
                {
                    input.push(b'"');
                    for byte in value {
                        if byte == b'"' {
                            input.push(b'"');
                        }
                        input.push(byte);
                    }
                    input.push(b'"');
                } else {
                    input.extend_from_slice(&value);
                }
            }
            input.push(b'\n');
        }

        let mut literal = Vec::new();
        for _ in 0..=rng.below(3) {
            literal.push(ALPHABET[rng.below(ALPHABET.len())]);
        }

        for predicate in [
            Predicate::equals(1, literal.clone()),
            Predicate::contains(1, literal.clone()),
            Predicate::starts_with(1, literal.clone()),
            Predicate::ends_with(1, literal.clone()),
        ] {
            let expected = reference(
                &input,
                FormatOptions::CSV,
                ParseOptions::new().headers(Headers::None),
                &predicate,
                1,
            );
            let actual = filtered(
                &input,
                FormatOptions::CSV,
                ParseOptions::new().headers(Headers::None),
                &predicate,
            );
            assert_eq!(
                actual,
                expected,
                "seed {seed} predicate {predicate:?} input {:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }
}

#[test]
fn high_byte_structurals_disable_raw_literal_skipping() {
    let input = b"id,value\n1,|a||b|\n2,plain\n";
    let format = FormatOptions::CSV.quote(b'|');
    let predicate = Predicate::equals(1, b"a|b".to_vec());

    let matches = assert_agrees(
        input,
        format,
        ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
        1,
    );

    assert_eq!(matches, [vec![b"1".to_vec(), b"a|b".to_vec()]]);
}

// ---------------------------------------------------------------------------
// Streaming parser
// ---------------------------------------------------------------------------

use coseva::IoParser;
use coseva::format::Csv;
use std::io::Cursor;

/// Collect matches from the streaming parser's filtering fast path.
fn stream_filtered(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
) -> Vec<Vec<Vec<u8>>> {
    let mut parser = IoParser::with_options(input, format, options).expect("valid options");
    let mut records = Vec::new();
    while let Some(mut line) = parser
        .next_matching_line(predicate)
        .expect("parse succeeds")
    {
        let record = line.record().expect("parse succeeds");
        records.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    records
}

/// Collect matches by streaming every record and filtering in the application.
fn stream_reference(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
    column: usize,
) -> Vec<Vec<Vec<u8>>> {
    let mut parser = IoParser::with_options(input, format, options).expect("valid options");
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line().expect("parse succeeds") {
        let record = line.record().expect("parse succeeds");
        if predicate.matches_field(record.get(column)) {
            records.push(record.iter().map(<[u8]>::to_vec).collect());
        }
    }
    records
}

/// Assert streaming filtering agrees with streaming reference, at every buffer
/// size that matters: smaller than a record, around a record, and large.
fn assert_stream_agrees(
    input: &[u8],
    format: FormatOptions,
    options: &ParseOptions,
    predicate: &Predicate,
    column: usize,
) -> Vec<Vec<Vec<u8>>> {
    let expected = reference(input, format, options.clone(), predicate, column);
    for capacity in [1, 2, 3, 7, 16, 64, 113, 1024, 64 * 1024] {
        let sized = options.clone().buffer_capacity(capacity);
        assert_eq!(
            stream_reference(input, format, sized.clone(), predicate, column),
            expected,
            "streaming reference diverged at capacity {capacity}"
        );
        assert_eq!(
            stream_filtered(input, format, sized, predicate),
            expected,
            "streaming filter diverged at capacity {capacity}"
        );
    }
    expected
}

#[test]
fn streaming_filters_by_column_index() {
    let input = b"Boston,US\nParis,FR\nDenver,US\nLyon,FR\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn streaming_filters_by_header_name() {
    let input = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";
    let predicate = Predicate::equals("country", "US");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn streaming_walks_quoted_records() {
    let input = b"a,\"x,y\nz\"\nb,US\n\"c\nc\",q\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn streaming_finds_matches_inside_quoted_fields() {
    let input = b"a,\"US\"\nb,FR\nc,\"US\"\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn streaming_handles_literals_escaping_could_split() {
    let input = b"one,\"a\"\"b\"\ntwo,plain\n";
    let predicate = Predicate::equals(1, "a\"b");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn streaming_handles_the_last_record_without_a_terminator() {
    let input = b"a,FR\nb,US";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_stream_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 1);
}

#[test]
fn streaming_matches_across_dialects_and_policies() {
    let predicate = Predicate::equals(1, "US");
    let cases: [(&[u8], FormatOptions); 5] = [
        (b"a,US\nb,FR\nc,US\n", FormatOptions::CSV),
        (b"a\tUS\nb\tFR\nc\tUS\n", FormatOptions::TSV),
        (
            b"a,US\r\nb,FR\r\nc,US\r\n",
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        ),
        (
            b"#c US\na,US\nb,FR\nc,US\n",
            FormatOptions::CSV.comment(Some(b'#')),
        ),
        (
            b"a,US\n\n\nb,FR\n\nc,US\n",
            FormatOptions::CSV.blank_records(BlankRecords::Skip),
        ),
    ];
    for (input, format) in cases {
        let matches = assert_stream_agrees(
            input,
            format,
            &ParseOptions::new().headers(Headers::None),
            &predicate,
            1,
        );
        assert_eq!(matches.len(), 2);
    }
}

#[test]
fn streaming_filtering_can_resume_after_plain_parsing() {
    let input: &[u8] = b"a,US\nb,FR\nc,US\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let mut parser = IoParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"a"[..])
    );

    let mut line = parser
        .next_line()
        .expect("parse succeeds")
        .expect("a record");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"b"[..])
    );

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(&b"c"[..])
    );
}

#[test]
fn streaming_reports_record_positions_across_skips() {
    let input: &[u8] = b"a,FR\nb,FR\nc,US\nd,FR\ne,US\n";
    let predicate = Predicate::equals(1, "US");
    let mut parser = IoParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");

    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(line.record().expect("parse succeeds").index(), 2);
    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(line.record().expect("parse succeeds").index(), 4);
}

#[test]
fn streaming_agrees_on_a_large_input_spanning_many_chunks() {
    let mut input = Vec::new();
    let mut expected = 0;
    for index in 0..5_000 {
        if index % 997 == 0 {
            input.extend_from_slice(b"row,NEEDLE,tail\n");
            expected += 1;
        } else if index % 13 == 0 {
            input.extend_from_slice(b"row,\"quoted,value\nsecond line\",tail\n");
        } else {
            input.extend_from_slice(b"row,filler,tail\n");
        }
    }

    let predicate = Predicate::equals(1, "NEEDLE");
    let reference_matches = reference(
        &input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(reference_matches.len(), expected);

    for capacity in [8, 97, 4096, 64 * 1024] {
        let options = ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(capacity);
        assert_eq!(
            stream_filtered(&input, FormatOptions::CSV, options, &predicate),
            reference_matches,
            "diverged at capacity {capacity}"
        );
    }
}

#[test]
fn streaming_randomized_inputs_never_lose_a_match() {
    const ALPHABET: &[u8] = b"ab\"c,d\ne";

    for seed in 1..120_u64 {
        let mut rng = Rng(seed);
        let mut input = Vec::new();
        let records = 1 + rng.below(30);

        for _ in 0..records {
            let fields = 1 + rng.below(3);
            for field in 0..fields {
                if field > 0 {
                    input.push(b',');
                }
                let mut value = Vec::new();
                for _ in 0..rng.below(6) {
                    value.push(ALPHABET[rng.below(ALPHABET.len())]);
                }
                if value
                    .iter()
                    .any(|byte| matches!(byte, b'"' | b',' | b'\n' | b'\r'))
                {
                    input.push(b'"');
                    for byte in value {
                        if byte == b'"' {
                            input.push(b'"');
                        }
                        input.push(byte);
                    }
                    input.push(b'"');
                } else {
                    input.extend_from_slice(&value);
                }
            }
            input.push(b'\n');
        }

        let mut literal = Vec::new();
        for _ in 0..=rng.below(3) {
            literal.push(ALPHABET[rng.below(ALPHABET.len())]);
        }

        let predicate = Predicate::contains(1, literal);
        let expected = reference(
            &input,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
            &predicate,
            1,
        );
        for capacity in [1, 5, 17, 64, 4096] {
            let options = ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(capacity);
            assert_eq!(
                stream_filtered(&input, FormatOptions::CSV, options, &predicate),
                expected,
                "seed {seed} capacity {capacity} input {:?}",
                String::from_utf8_lossy(&input)
            );
        }
    }
}

/// Skipping records must keep the streaming line counter in step, otherwise
/// errors reported after a long skip point at the wrong line.
#[test]
fn streaming_skips_keep_error_locations_accurate() {
    let mut input = String::new();
    for index in 0..500 {
        if index == 200 {
            input.push_str("needle,bbb\n");
        } else if index == 400 {
            // A bare quote in an unquoted field forces the parser off its fast
            // path and reports a location, which is what we are checking.
            input.push_str("xx\"yy,zz\n");
        } else {
            input.push_str("aaa,bbb\n");
        }
    }
    let input = input.as_bytes();

    let format = FormatOptions::new();
    let options = ParseOptions::new().headers(Headers::None);
    let predicate = Predicate::contains(0, "needle");

    // The unfiltered walk defines the ground truth for the location.
    let mut plain = IoParser::with_options(input, format, options.clone()).expect("valid options");
    let mut expected = None;
    loop {
        match plain.next_line() {
            Ok(Some(mut line)) => {
                if let Err(error) = line.record() {
                    expected = Some(error.location());
                    break;
                }
            }
            Ok(None) => break,
            Err(error) => {
                expected = Some(error.location());
                break;
            }
        }
    }
    let expected = expected.expect("the stray quote is rejected");
    assert_eq!(expected.line, 401, "sanity check on the fixture");

    for capacity in [16_usize, 64, 113, 1024, 65536] {
        let options = options.clone().buffer_capacity(capacity);
        let mut parser = IoParser::with_options(input, format, options).expect("valid options");
        let mut actual = None;
        loop {
            match parser.next_matching_line(&predicate) {
                Ok(Some(mut line)) => {
                    if let Err(error) = line.record() {
                        actual = Some(error.location());
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    actual = Some(error.location());
                    break;
                }
            }
        }
        let actual = actual.expect("the stray quote is rejected while filtering");
        assert_eq!(actual.line, expected.line, "line at capacity {capacity}");
        assert_eq!(
            actual.record, expected.record,
            "record at capacity {capacity}"
        );
        assert_eq!(actual.byte, expected.byte, "byte at capacity {capacity}");
    }
}

/// Collect matches from the push parser, lending `input` in fixed-size chunks.
fn push_filtered(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    predicate: &Predicate,
    chunk: usize,
) -> Vec<Vec<Vec<u8>>> {
    let mut parser = PushParser::with_options(format, options).expect("valid options");
    let mut records = Vec::new();
    let mut fed = 0;
    while fed < input.len() {
        let end = (fed + chunk).min(input.len());
        while fed < end {
            let mut lent = parser.chunk(&input[fed..end]);
            while let Some(mut line) = lent.next_matching_line(predicate).expect("parse succeeds") {
                let record = line.record().expect("parse succeeds");
                records.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            fed += lent.done();
        }
    }
    parser.finish();
    let mut lent = parser.chunk(b"");
    while let Some(mut line) = lent.next_matching_line(predicate).expect("parse succeeds") {
        let record = line.record().expect("parse succeeds");
        records.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    drop(lent);
    records
}

/// Every chunk size must agree with the slice parser's unfiltered reference.
fn assert_push_agrees(
    input: &[u8],
    format: FormatOptions,
    options: &ParseOptions,
    predicate: &Predicate,
    column: usize,
) -> Vec<Vec<Vec<u8>>> {
    let expected = reference(input, format, options.clone(), predicate, column);
    for chunk in [1_usize, 2, 3, 7, 16, 64, 1024] {
        assert_eq!(
            push_filtered(input, format, options.clone(), predicate, chunk),
            expected,
            "push filter diverged at chunk size {chunk}"
        );
    }
    expected
}

#[test]
fn push_filters_by_column_index() {
    let input = b"Boston,US\nParis,FR\nDenver,US\nLyon,FR\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_push_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn push_filters_by_header_name() {
    let input = b"city,country\nBoston,US\nParis,FR\nDenver,US\n";
    let predicate = Predicate::equals("country", "US");
    let matches = assert_push_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::FirstRecord),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn push_filtering_walks_quoted_records() {
    let input = b"a,\"x,y\nz\"\nb,US\n\"c\nc\",q\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_push_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn push_filtering_handles_the_last_record_without_a_terminator() {
    let input = b"a,US\nb,FR\nc,US";
    let predicate = Predicate::equals(1, "US");
    let matches = assert_push_agrees(
        input,
        FormatOptions::CSV,
        &ParseOptions::new().headers(Headers::None),
        &predicate,
        1,
    );
    assert_eq!(matches.len(), 2);
}

#[test]
fn push_filtering_can_resume_after_plain_parsing() {
    let input = b"a,US\nb,FR\nc,US\nd,FR\ne,US\n";
    let predicate = Predicate::equals(1, "US");
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    parser.finish();
    let mut parser = parser.chunk(input);

    let mut line = parser
        .next_line()
        .expect("parse succeeds")
        .expect("a record");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(b"a".as_slice())
    );
    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(b"c".as_slice())
    );
    let mut line = parser
        .next_line()
        .expect("parse succeeds")
        .expect("a record");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(b"d".as_slice())
    );
    let mut line = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .expect("a match");
    assert_eq!(
        line.record().expect("parse succeeds").get(0),
        Some(b"e".as_slice())
    );
    assert!(
        parser
            .next_matching_line(&predicate)
            .expect("parse succeeds")
            .is_none()
    );
}

#[test]
fn push_filtering_reports_error_locations_like_the_slice_parser() {
    let input = b"a,US\nb,US\nc,\"x\"y\nd,US\n";
    let predicate = Predicate::equals(1, "US");
    let options = ParseOptions::new().headers(Headers::None);

    let mut slice = SliceParser::with_options(input, FormatOptions::CSV, options.clone())
        .expect("valid options");
    let mut expected = None;
    loop {
        match slice.next_matching_line(&predicate) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(error) => {
                expected = Some((error.kind(), error.location()));
                break;
            }
        }
    }
    let expected = expected.expect("the stray quote is rejected while filtering");

    for chunk in [1_usize, 3, 7, 64] {
        let mut parser =
            PushParser::with_options(FormatOptions::CSV, options.clone()).expect("valid options");
        let mut actual = None;
        let mut fed = 0;
        'lending: while fed < input.len() {
            let end = (fed + chunk).min(input.len());
            while fed < end {
                let mut lent = parser.chunk(&input[fed..end]);
                let failure = loop {
                    match lent.next_matching_line(&predicate) {
                        Ok(Some(_)) => {}
                        Ok(None) => break None,
                        Err(error) => break Some((error.kind(), error.location())),
                    }
                };
                fed += lent.done();
                if failure.is_some() {
                    actual = failure;
                    break 'lending;
                }
            }
        }
        if actual.is_none() {
            parser.finish();
            let mut lent = parser.chunk(b"");
            loop {
                match lent.next_matching_line(&predicate) {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(error) => {
                        actual = Some((error.kind(), error.location()));
                        break;
                    }
                }
            }
        }
        let actual = actual.expect("the stray quote is rejected while filtering");
        assert_eq!(actual.0, expected.0, "kind at chunk size {chunk}");
        assert_eq!(actual.1.byte, expected.1.byte, "byte at chunk size {chunk}");
        assert_eq!(actual.1.line, expected.1.line, "line at chunk size {chunk}");
        assert_eq!(
            actual.1.record, expected.1.record,
            "record at chunk size {chunk}"
        );
    }
}

// ── Predicate::contains edge cases ──────────────────────────────────────────────

/// A needle longer than the haystack can never match.
#[test]
fn predicate_contains_needle_longer_than_haystack_does_not_match() {
    let pred = Predicate::contains(0, "longerneedle");
    assert!(!pred.matches_field(Some(b"tiny")));
}

/// An empty needle matches every field, including an empty one.
#[test]
fn predicate_contains_empty_needle_matches_all() {
    let pred = Predicate::contains(0, "");
    assert!(pred.matches_field(Some(b"anything")));
    assert!(pred.matches_field(Some(b"")));
    assert!(pred.matches_field(Some(b"x")));
}

/// Bytes outside the ASCII range are compared exactly like any other byte:
/// `contains` never validates or reinterprets UTF-8.
#[test]
fn predicate_contains_matches_non_ascii_bytes() {
    let field: &[u8] = &[0x00, 0xFF, 0x80, 0xC3, 0xA9, 0x01];
    assert!(Predicate::contains(0, [0x80_u8, 0xC3, 0xA9].as_slice()).matches_field(Some(field)));
    assert!(!Predicate::contains(0, [0xC3_u8, 0x00].as_slice()).matches_field(Some(field)));
}

/// A needle equal to the whole haystack still matches, exercising the
/// two-way engine's boundary where the needle and input end at the same
/// position.
#[test]
fn predicate_contains_needle_equal_to_haystack_matches() {
    let pred = Predicate::contains(0, "exact");
    assert!(pred.matches_field(Some(b"exact")));
}

/// A periodic needle absent from a long, highly repetitive field must still
/// resolve to no match, the shape that made the naive scan this replaced
/// adversarially quadratic (see `docs/TODO.md`, item P8).
#[test]
fn predicate_contains_periodic_needle_over_long_repetition() {
    let haystack = vec![b'a'; 100_000];
    let pred = Predicate::contains(0, "aaaaaaaaab");
    assert!(!pred.matches_field(Some(&haystack)));

    let mut with_match = vec![b'a'; 100_000];
    with_match[50_000..50_010].copy_from_slice(b"aaaaaaaaab");
    assert!(pred.matches_field(Some(&with_match)));
}

// ── Filtering never suppresses a parse error ────────────────────────────────────

/// Drive the slice path to completion, reporting the first error kind.
fn slice_filter_outcome(input: &[u8], predicate: &Predicate) -> Result<usize, ErrorKind> {
    let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new()).expect("parser");
    let mut matches = 0;
    loop {
        match parser.next_matching_line(predicate) {
            Ok(Some(_)) => matches += 1,
            Ok(None) => return Ok(matches),
            Err(error) => return Err(error.kind()),
        }
    }
}

/// Drive the streaming path to completion, reporting the first error kind.
fn stream_filter_outcome(input: &[u8], predicate: &Predicate) -> Result<usize, ErrorKind> {
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(input.to_vec()), ParseOptions::new()).expect("parser");
    let mut matches = 0;
    loop {
        match parser.next_matching_line(predicate) {
            Ok(Some(_)) => matches += 1,
            Ok(None) => return Ok(matches),
            Err(error) => return Err(error.kind()),
        }
    }
}

/// Drive the unfiltered slice path, reporting the first error kind.
fn unfiltered_outcome(input: &[u8]) -> Result<(), ErrorKind> {
    let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new()).expect("parser");
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                if let Err(error) = line.record() {
                    return Err(error.kind());
                }
            }
            Ok(None) => return Ok(()),
            Err(error) => return Err(error.kind()),
        }
    }
}

/// A predicate whose literal never appears must not turn malformed input into
/// a clean end of input. The pushdown scan may skip records, but skipping is
/// an optimization and can never decide that unparsed bytes were valid.
#[test]
fn a_non_matching_filter_still_reports_a_stray_quote() {
    // A valid header, then one data record whose bare field contains a quote.
    // The header must parse cleanly so the error can only come from the
    // filtered walk rather than from header discovery.
    let input = b"col\nx\"y\n";
    let predicate = Predicate::equals(0, "zzz");

    assert_eq!(unfiltered_outcome(input), Err(ErrorKind::UnexpectedQuote));
    assert_eq!(
        slice_filter_outcome(input, &predicate),
        Err(ErrorKind::UnexpectedQuote),
    );
}

/// The slice and streaming paths must reach the same verdict, because the
/// pushdown scan is the only thing that differs between them.
#[test]
fn filtering_agrees_between_slice_and_stream_on_malformed_input() {
    let predicate = Predicate::equals(0, "zzz");
    for input in [
        &b"x\"y\n"[..],
        b"a,b\nx\"y,c\n",
        b"a,b\nc,d\nx\"y\n",
        b"a\n\"unterminated\n",
        b"h\nx\"y",
    ] {
        assert_eq!(
            slice_filter_outcome(input, &predicate),
            stream_filter_outcome(input, &predicate),
            "slice and stream disagree on {input:?}",
        );
    }
}

/// A malformed record after the last match must still be reported, rather
/// than being swallowed once the literal stops appearing.
#[test]
fn a_malformed_record_after_the_final_match_is_reported() {
    let input = b"col\nhit\nx\"y\n";
    let predicate = Predicate::equals(0, "hit");

    assert_eq!(
        slice_filter_outcome(input, &predicate),
        Err(ErrorKind::UnexpectedQuote),
    );
    assert_eq!(
        stream_filter_outcome(input, &predicate),
        Err(ErrorKind::UnexpectedQuote),
    );
}

/// Filtering well-formed input is unaffected: the same records come back, and
/// no error appears where the unfiltered parse succeeds.
#[test]
fn a_non_matching_filter_over_well_formed_input_still_reports_no_matches() {
    let input = b"col\na\nb\nc\n";
    let predicate = Predicate::equals(0, "zzz");

    assert_eq!(unfiltered_outcome(input), Ok(()));
    assert_eq!(slice_filter_outcome(input, &predicate), Ok(0));
    assert_eq!(stream_filter_outcome(input, &predicate), Ok(0));
}

/// The scan repositions over long quote-free spans, so the tail past the last
/// skip must still be validated rather than assumed well formed.
#[test]
fn a_long_skipped_span_still_validates_the_tail() {
    let mut input = b"col\n".to_vec();
    for index in 0..200 {
        input.extend_from_slice(format!("row{index}\n").as_bytes());
    }
    input.extend_from_slice(b"x\"y\n");
    let predicate = Predicate::equals(0, "zzz");

    assert_eq!(
        slice_filter_outcome(&input, &predicate),
        Err(ErrorKind::UnexpectedQuote),
    );
    assert_eq!(
        stream_filter_outcome(&input, &predicate),
        Err(ErrorKind::UnexpectedQuote),
    );
}

/// Two different predicates that happen to occupy the same address must not
/// share a resolved column within one parser.
///
/// Any memoization of predicate resolution keyed on the predicate's address is
/// unsound: a predicate can be dropped and a different one allocated in the
/// same storage, so the second lookup would answer with the first one's column.
#[test]
fn distinct_predicates_reusing_an_address_resolve_independently() {
    let input: &[u8] = b"a,b\nkeep,skip\nskip,keep\n";
    let mut parser =
        SliceParser::with_options(input, FormatOptions::default(), ParseOptions::default())
            .expect("valid options");

    let mut addresses = Vec::new();
    let mut matched = Vec::new();
    for name in ["a", "b"] {
        let predicate = Predicate::equals(Column::Name(name.into()), "keep");
        addresses.push(core::ptr::from_ref(&predicate) as usize);
        let line = parser
            .next_matching_line(&predicate)
            .expect("parse succeeds");
        matched.push(line.map(|mut line| {
            line.record()
                .expect("parse succeeds")
                .iter()
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>()
        }));
    }

    assert_eq!(
        addresses[0], addresses[1],
        "test is only meaningful if the two predicates share an address",
    );
    assert_eq!(matched[0], Some(vec![b"keep".to_vec(), b"skip".to_vec()]));
    assert_eq!(matched[1], Some(vec![b"skip".to_vec(), b"keep".to_vec()]));
}

/// Replacing the headers re-resolves a name that now points elsewhere.
///
/// The resolved column is cached for the run, so the invalidation that clears
/// everything else derived from the header record has to clear it too.
#[test]
fn replacing_the_headers_re_resolves_a_named_column() {
    let input: &[u8] = b"keep,skip\nskip,keep\n";
    let predicate = Predicate::equals(Column::Name("a".into()), "keep");

    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::default(),
        ParseOptions::default().headers(Headers::Provided(["a", "b"].into_iter().collect())),
    )
    .expect("valid options");
    let start = parser.location();
    let first = parser
        .next_matching_line(&predicate)
        .expect("parse succeeds")
        .map(|mut line| line.record().expect("parse succeeds").iter().count());
    assert_eq!(first, Some(2));

    // `a` is now column 1, so the second record is the one that matches.
    parser.set_headers(["b", "a"].into_iter().collect());
    parser.seek(start).expect("seek succeeds");
    let matched: Vec<Vec<Vec<u8>>> = {
        let mut records = Vec::new();
        while let Some(mut line) = parser
            .next_matching_line(&predicate)
            .expect("parse succeeds")
        {
            let record = line.record().expect("parse succeeds");
            records.push(record.iter().map(<[u8]>::to_vec).collect());
        }
        records
    };
    assert_eq!(matched, [[b"skip".to_vec(), b"keep".to_vec()]]);
}

/// The push path caches the same answer, and reaches it once the headers do.
///
/// A name cannot resolve before the chunk carrying the headers arrives, so a
/// miss must stay retryable rather than being remembered.
#[test]
fn the_push_path_resolves_a_named_column_once_the_headers_arrive() {
    let predicate = Predicate::equals(Column::Name("b".into()), "keep");
    let mut matched: Vec<Vec<Vec<u8>>> = Vec::new();
    let mut parser = PushParser::with_options(
        FormatOptions::default(),
        ParseOptions::default().headers(Headers::FirstRecord),
    )
    .expect("valid options");

    for chunk in [&b"a,b\nskip,keep\n"[..], b"keep,skip\nx,keep\n"] {
        let mut feed = parser.chunk(chunk);
        while let Some(mut line) = feed.next_matching_line(&predicate).expect("parse succeeds") {
            let record = line.record().expect("parse succeeds");
            matched.push(record.iter().map(<[u8]>::to_vec).collect());
        }
    }
    parser.finish();

    assert_eq!(
        matched,
        [
            [b"skip".to_vec(), b"keep".to_vec()],
            [b"x".to_vec(), b"keep".to_vec()],
        ]
    );
}

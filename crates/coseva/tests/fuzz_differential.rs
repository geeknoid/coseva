//! Coverage-guided differential properties against `csv` and `csv-core`.
//!
//! Each target runs as an ordinary, bounded test under `cargo test`, replaying
//! its tracked corpus and a capped number of generated inputs so the suite
//! stays fast. The same targets run far deeper under a coverage-guided engine:
//!
//! ```text
//! crates/coseva/scripts/fuzz_campaign.py differential_readers_agree_on_arbitrary_bytes
//! crates/coseva/scripts/fuzz_campaign.py differential_writer_round_trips_through_all_readers
//! ```
//!
//! # The shared semantic domain
//!
//! `coseva`, [`csv`], and [`csv-core`] only agree over the standard,
//! "flexible" CSV that all three implement, so the differential is confined to
//! exactly that intersection rather than to `coseva`'s extensions:
//!
//! * a single-byte delimiter and a single-byte quote (the four dialects in
//!   [`DIALECTS`]), RFC 4180 quote doubling, and the standard family of
//!   CR / LF / CRLF record endings every side accepts;
//! * no comments, no whitespace trimming, no NULL sentinels, no backslash
//!   escaping, and no multi-byte separators — those are `coseva`-only knobs
//!   with no counterpart in `csv-core`;
//! * ragged records (`csv` in `flexible` mode, `coseva` with [`Headers::None`],
//!   and `csv-core` which never validates arity); and
//! * blank physical lines skipped, matching `csv`/`csv-core`, via
//!   [`BlankRecords::Skip`] — `coseva`'s default keeps them as empty records.
//!
//! To reach that intersection `coseva` parses under a lenient [`Syntax`] that
//! only relaxes the two rules `csv`/`csv-core` are permanently lenient about:
//! quoting stays on and a bare quote inside an unquoted field is tolerated
//! ([`Recovery::unquoted_quotes`]). Nothing else is relaxed, so a genuine
//! parser disagreement is not masked by a blanket "accept everything" mode.
//!
//! # Intentional semantic differences (named exclusions)
//!
//! Two behaviours are *documented* differences, encoded here as named, narrowly
//! matched exclusions with fixed regression examples in
//! [`documented_strictness_exclusions_hold`] — never as a broad `return` that
//! would also swallow a real bug:
//!
//! * **Strict quote boundary.** After a closing quote `coseva` requires a
//!   delimiter or record ending; `csv`/`csv-core` instead append the trailing
//!   bytes to the field. So `"ab"cd` is [`ErrorKind::UnexpectedByteAfterQuote`]
//!   for `coseva` but `abcd` for the other two.
//! * **Strict unterminated quote.** A quote opened but never closed before EOF
//!   is [`ErrorKind::UnterminatedQuotedField`] for `coseva`; `csv`/`csv-core`
//!   accept the truncated remainder as the field's contents.
//! * **Lone carriage return.** `coseva`'s CSV record ending is a line feed
//!   optionally preceded by a carriage return, so a `\r` *not* followed by `\n`
//!   is ordinary field data; `csv`/`csv-core` treat a lone `\r` as a record
//!   terminator. Inputs carrying a bare `\r` are therefore outside the shared
//!   domain and skipped by [`has_bare_cr`] in the arbitrary-bytes property, and
//!   the difference is pinned by
//!   [`lone_carriage_return_is_a_documented_record_ending_difference`].
//!
//! A leading UTF-8 BOM is *not* an exclusion: `coseva` (default `Detect`), the
//! `csv` crate, and `csv-core` all strip one, so it stays in the shared domain
//! and is asserted by positive equality in
//! [`leading_bom_is_stripped_identically_across_libraries`]. It is only skipped
//! by the round-trip property (via [`has_leading_bom`]) when it opens the first
//! field, because a document that starts with a BOM loses it on read-back
//! through any of the three readers.
//!
//! When `coseva` accepts an input, the readers must agree exactly; when
//! `coseva` rejects one, the rejection must fall in the documented-strict class
//! above. Anything else is a real finding and fails the property.
//!
//! # Corpus and campaign
//!
//! Coverage-increasing and regression inputs live under
//! `tests/__fuzz__/<target_name>/corpus/`, replayed verbatim on every
//! `cargo test`. The machine-readable campaign definition CI invokes is
//! `tests/__fuzz__/campaign.toml`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::time::Duration;

use coseva::config::{
    BlankRecords, EmitOptions, FormatOptions, Headers, ParseOptions, Recovery, Syntax,
};
use coseva::{Error, ErrorKind, SliceParser, VecEmitter};

/// Records reduced to plain bytes, so results from different libraries compare.
type Rows = Vec<Vec<Vec<u8>>>;

/// Iterations each bounded `cargo test` run performs on top of the corpus.
const BOUNDED_ITERATIONS: usize = 4096;

/// Wall-clock cap for a bounded run, a backstop for the iteration cap.
const BOUNDED_TEST_TIME: Duration = Duration::from_millis(400);

/// Largest arbitrary input a bounded run parses, keeping runs fast and the
/// `csv-core` scratch buffers bounded regardless of what the fuzzer supplies.
const MAX_INPUT: usize = 4096;

macro_rules! bounded {
    () => {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
    };
}

/// A dialect all three libraries implement identically: one delimiter byte and
/// one quote byte, standard quote doubling, standard record endings.
#[derive(Clone, Copy, Debug)]
struct Dialect {
    delimiter: u8,
    quote: u8,
}

/// The dialects the differential sweeps. Byte roles are distinct within each so
/// the configuration is valid for every library.
const DIALECTS: &[Dialect] = &[
    Dialect {
        delimiter: b',',
        quote: b'"',
    },
    Dialect {
        delimiter: b';',
        quote: b'"',
    },
    Dialect {
        delimiter: b'\t',
        quote: b'"',
    },
    Dialect {
        delimiter: b'|',
        quote: b'\'',
    },
];

impl Dialect {
    /// `coseva`'s reader configuration for the shared domain: lenient exactly
    /// where `csv`/`csv-core` are lenient, and blank lines skipped like them.
    fn coseva_read_format(self) -> FormatOptions {
        FormatOptions::CSV
            .delimiter(self.delimiter)
            .quote(self.quote)
            .syntax(Syntax::Compatible(
                Recovery::NONE.quoting(true).unquoted_quotes(true),
            ))
            .blank_records(BlankRecords::Skip)
    }

    /// `coseva`'s writer configuration: the same dialect with default (strict)
    /// emission, so output is canonical CSV every reader can recover.
    fn coseva_write_format(self) -> FormatOptions {
        FormatOptions::CSV
            .delimiter(self.delimiter)
            .quote(self.quote)
    }
}

// ── Library front ends ───────────────────────────────────────────────────────

/// Parse with `coseva`, collecting every record as plain byte fields.
fn coseva_rows(input: &[u8], format: FormatOptions) -> Result<Rows, Error> {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::with_options(input, format, options)?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

/// Parse with the `csv` crate in flexible, header-less mode. Returns `None` if
/// `csv` rejects the input at all — the class of a `csv` rejection has no
/// `coseva` counterpart in the shared domain, so only success and rows matter.
fn csv_rows(input: &[u8], dialect: Dialect) -> Option<Rows> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .from_reader(input);
    let mut rows = Rows::new();
    for record in reader.byte_records() {
        rows.push(record.ok()?.iter().map(<[u8]>::to_vec).collect());
    }
    Some(rows)
}

/// Parse with `csv-core`, driving its byte-at-a-time reader over caller-owned
/// scratch buffers that grow only on demand. Returns `None` if the buffers
/// cannot be satisfied within a bound proportional to the input.
fn csv_core_rows(input: &[u8], dialect: Dialect) -> Option<Rows> {
    let mut reader = csv_core::ReaderBuilder::new()
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .build();
    let mut fields = vec![0u8; 256];
    let mut ends = vec![0usize; 32];
    let mut rows = Rows::new();
    let mut remaining = input;
    let mut field_pos = 0usize;
    let mut end_pos = 0usize;
    // A generous, input-proportional cap so a pathological input can never spin
    // the reader; the standard flush reaches `End` far inside it.
    let mut budget = input.len().saturating_mul(4).saturating_add(16);
    loop {
        if budget == 0 {
            return None;
        }
        budget -= 1;
        let (result, read, wrote, count) =
            reader.read_record(remaining, &mut fields[field_pos..], &mut ends[end_pos..]);
        remaining = &remaining[read..];
        field_pos += wrote;
        end_pos += count;
        match result {
            csv_core::ReadRecordResult::Record => {
                let mut start = 0;
                let mut record = Vec::with_capacity(end_pos);
                for &end in &ends[..end_pos] {
                    record.push(fields[start..end].to_vec());
                    start = end;
                }
                rows.push(record);
                field_pos = 0;
                end_pos = 0;
            }
            csv_core::ReadRecordResult::End => break,
            csv_core::ReadRecordResult::InputEmpty => {}
            csv_core::ReadRecordResult::OutputFull => fields.resize(fields.len() * 2, 0),
            csv_core::ReadRecordResult::OutputEndsFull => ends.resize(ends.len() * 2, 0),
        }
    }
    Some(rows)
}

// ── Named exclusions ─────────────────────────────────────────────────────────

/// The documented cases where `coseva` is intentionally stricter than
/// `csv`/`csv-core`: a byte follows a closing quote, or a quoted field is never
/// closed. See the module docs.
fn is_documented_strictness(error: &Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::UnexpectedByteAfterQuote(_)
            | ErrorKind::UnexpectedQuote
            | ErrorKind::UnterminatedQuotedField
    )
}

/// True if `bytes` contain a carriage return not immediately followed by a line
/// feed. Such a lone `\r` is the one record-ending construct outside the shared
/// domain — data to `coseva`, a terminator to `csv`/`csv-core` — so the
/// arbitrary-bytes property skips inputs that carry one. `\r\n` pairs, which
/// every side treats alike, are unaffected.
fn has_bare_cr(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .enumerate()
        .any(|(index, &byte)| byte == b'\r' && bytes.get(index + 1) != Some(&b'\n'))
}

/// True if `bytes` begin with a UTF-8 BOM. All three libraries strip a leading
/// BOM on read (asserted positively by
/// [`leading_bom_is_stripped_identically_across_libraries`]), so a first field
/// that opens with one cannot survive a write/read round trip and is skipped by
/// the round-trip property.
fn has_leading_bom(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xEF\xBB\xBF")
}

// ── Property: readers agree on arbitrary bytes ───────────────────────────────

#[test]
fn differential_readers_agree_on_arbitrary_bytes() {
    bounded!().for_each(|bytes: &[u8]| {
        if bytes.len() > MAX_INPUT || has_bare_cr(bytes) {
            return;
        }
        for &dialect in DIALECTS {
            match coseva_rows(bytes, dialect.coseva_read_format()) {
                Ok(rows) => {
                    let via_csv = csv_rows(bytes, dialect);
                    let via_core = csv_core_rows(bytes, dialect);
                    assert_eq!(
                        Some(&rows),
                        via_csv.as_ref(),
                        "coseva vs csv on {bytes:?} under {dialect:?}"
                    );
                    assert_eq!(
                        Some(&rows),
                        via_core.as_ref(),
                        "coseva vs csv-core on {bytes:?} under {dialect:?}"
                    );
                }
                Err(error) => assert!(
                    is_documented_strictness(&error),
                    "coseva rejected {bytes:?} under {dialect:?} with an undocumented \
                     kind {:?}; csv/csv-core would accept it",
                    error.kind()
                ),
            }
        }
    });
}

// ── Property: every writer's output round-trips through every reader ─────────

/// Carve arbitrary bytes into a bounded record set. `0x00` ends a field and
/// `0x01` ends a record; every other byte is field content, so quotes,
/// delimiters, and newlines land *inside* fields and force the writers to
/// quote. Degenerate rows that would emit as a blank physical line (empty, or a
/// single empty field) are dropped: a blank line is the one shape the shared
/// blank-line-skipping domain cannot round-trip unambiguously.
fn carve_rows(bytes: &[u8]) -> Rows {
    const MAX_RECORDS: usize = 8;
    const MAX_FIELDS: usize = 6;
    const MAX_FIELD: usize = 64;

    let mut rows = Rows::new();
    let mut row: Vec<Vec<u8>> = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    for &byte in bytes {
        match byte {
            0x00 => {
                row.push(std::mem::take(&mut field));
            }
            0x01 => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            other if field.len() < MAX_FIELD => field.push(other),
            _ => {}
        }
    }
    row.push(field);
    rows.push(row);

    rows.truncate(MAX_RECORDS);
    for row in &mut rows {
        row.truncate(MAX_FIELDS);
    }
    rows.retain(|row| !(row.len() <= 1 && row.first().is_none_or(Vec::is_empty)));
    rows
}

/// Emit `rows` with `coseva`'s writer under `dialect`.
fn emit_with_coseva(rows: &Rows, dialect: Dialect) -> Vec<u8> {
    let mut emitter = VecEmitter::with_options(
        Vec::new(),
        dialect.coseva_write_format(),
        EmitOptions::new().has_headers(false),
    )
    .expect("coseva emitter constructs for a valid dialect");
    for row in rows {
        emitter
            .emit_record(row.iter().map(Vec::as_slice))
            .expect("coseva emits a carved record");
    }
    emitter.into_inner()
}

/// Emit `rows` with the `csv` crate's writer under `dialect`.
fn emit_with_csv(rows: &Rows, dialect: Dialect) -> Vec<u8> {
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .flexible(true)
        .delimiter(dialect.delimiter)
        .quote(dialect.quote)
        .from_writer(Vec::new());
    for row in rows {
        writer.write_record(row).expect("csv emits a carved record");
    }
    writer.into_inner().expect("csv writer flushes")
}

/// Assert `document` parses back to exactly `expected` through all three
/// readers under `dialect`.
fn assert_every_reader_recovers(document: &[u8], expected: &Rows, dialect: Dialect, source: &str) {
    let coseva = coseva_rows(document, dialect.coseva_read_format());
    assert!(
        coseva.is_ok(),
        "coseva failed to read {source} output {document:?}: {:?}",
        coseva.as_ref().err()
    );
    assert_eq!(
        coseva.unwrap_or_default(),
        *expected,
        "coseva reading {source} output {document:?}"
    );

    let via_csv = csv_rows(document, dialect);
    assert!(
        via_csv.is_some(),
        "csv failed to read {source} output {document:?}"
    );
    assert_eq!(
        via_csv.unwrap_or_default(),
        *expected,
        "csv reading {source} output {document:?}"
    );

    let via_core = csv_core_rows(document, dialect);
    assert!(
        via_core.is_some(),
        "csv-core exhausted its budget on {source} output {document:?}"
    );
    assert_eq!(
        via_core.unwrap_or_default(),
        *expected,
        "csv-core reading {source} output {document:?}"
    );
}

#[test]
fn differential_writer_round_trips_through_all_readers() {
    bounded!().for_each(|bytes: &[u8]| {
        if bytes.len() > MAX_INPUT {
            return;
        }
        let rows = carve_rows(bytes);
        if rows.is_empty() {
            return;
        }
        // A first field that opens with a BOM makes the emitted document start
        // with one, which every reader strips on read-back — so it cannot round
        // trip and is out of scope for this equality.
        if rows
            .first()
            .and_then(|record| record.first())
            .is_some_and(|field| has_leading_bom(field))
        {
            return;
        }
        for &dialect in DIALECTS {
            let coseva_doc = emit_with_coseva(&rows, dialect);
            assert_every_reader_recovers(&coseva_doc, &rows, dialect, "coseva");

            let csv_doc = emit_with_csv(&rows, dialect);
            assert_every_reader_recovers(&csv_doc, &rows, dialect, "csv");
        }
    });
}

// ── Deterministic regressions ────────────────────────────────────────────────

#[test]
fn documented_strictness_exclusions_hold() {
    let dialect = DIALECTS[0];

    // Strict quote boundary: a byte follows a closing quote.
    for input in [b"\"ab\"cd\n".as_slice(), b"\"x\"y\n", b"\"z\" ,y\n"] {
        let error = coseva_rows(input, dialect.coseva_read_format())
            .expect_err("coseva must reject bytes after a closing quote");
        assert!(
            is_documented_strictness(&error),
            "unexpected kind {:?} for {input:?}",
            error.kind()
        );
        assert!(
            csv_rows(input, dialect).is_some(),
            "csv is expected to accept {input:?}"
        );
        assert!(
            csv_core_rows(input, dialect).is_some(),
            "csv-core is expected to accept {input:?}"
        );
    }

    // Strict unterminated quote: a quote is opened but never closed.
    let unterminated = b"\"never closed\n".as_slice();
    let error = coseva_rows(unterminated, dialect.coseva_read_format())
        .expect_err("coseva must reject an unterminated quoted field");
    assert_eq!(error.kind(), ErrorKind::UnterminatedQuotedField);
    assert!(csv_rows(unterminated, dialect).is_some());
    assert!(csv_core_rows(unterminated, dialect).is_some());
}

#[test]
fn blank_lines_are_skipped_like_csv() {
    let dialect = DIALECTS[0];
    for input in [
        b"\n".as_slice(),
        b"\n\n",
        b"a,b\n\nc,d\n",
        b"a,b\r\n\r\nc,d",
    ] {
        let coseva = coseva_rows(input, dialect.coseva_read_format()).expect("parses");
        let via_csv = csv_rows(input, dialect).expect("csv parses");
        let via_core = csv_core_rows(input, dialect).expect("csv-core parses");
        assert_eq!(coseva, via_csv, "coseva vs csv on {input:?}");
        assert_eq!(coseva, via_core, "coseva vs csv-core on {input:?}");
    }
}

#[test]
fn leading_bom_is_stripped_identically_across_libraries() {
    // A leading UTF-8 BOM is in the shared domain: `coseva` (default `Detect`),
    // the `csv` crate, and `csv-core` all strip it, so they agree by positive
    // equality. This once could not be asserted because the pre-fix front ends
    // disagreed on a rejected BOM; with that fixed the equality is exact.
    let dialect = DIALECTS[0];
    for input in [
        b"\xEF\xBB\xBFa,b\nc,d\n".as_slice(),
        b"\xEF\xBB\xBFsolo\n",
        b"\xEF\xBB\xBF\"quoted\",b\n",
    ] {
        let coseva = coseva_rows(input, dialect.coseva_read_format()).expect("coseva parses");
        let via_csv = csv_rows(input, dialect).expect("csv parses");
        let via_core = csv_core_rows(input, dialect).expect("csv-core parses");
        assert_eq!(coseva, via_csv, "coseva vs csv on BOM input {input:?}");
        assert_eq!(
            coseva, via_core,
            "coseva vs csv-core on BOM input {input:?}"
        );
        assert!(
            coseva
                .first()
                .and_then(|record| record.first())
                .is_some_and(|field| !field.starts_with(b"\xEF\xBB\xBF")),
            "every library must strip the leading BOM for {input:?}"
        );
    }

    // The stripping applies only at the very start: a BOM sequence mid-field is
    // ordinary data every library keeps.
    let interior = b"a,\xEF\xBB\xBFb\n".as_slice();
    let coseva = coseva_rows(interior, dialect.coseva_read_format()).expect("coseva parses");
    assert_eq!(coseva, csv_rows(interior, dialect).expect("csv parses"));
    assert_eq!(
        coseva,
        csv_core_rows(interior, dialect).expect("csv-core parses")
    );
    assert_eq!(
        coseva[0][1], b"\xEF\xBB\xBFb",
        "an interior BOM is kept as data"
    );
}

#[test]
fn lone_carriage_return_is_a_documented_record_ending_difference() {
    let dialect = DIALECTS[0];
    // A bare CR between two fields: data to coseva, a record ending to the
    // others. `has_bare_cr` must flag it so the arbitrary-bytes property skips
    // exactly this construct.
    let input = b"a\rb\n".as_slice();
    assert!(has_bare_cr(input), "the guard must flag a lone CR");
    assert!(
        !has_bare_cr(b"a\r\nb\n"),
        "a CRLF pair is in the shared domain"
    );

    let coseva = coseva_rows(input, dialect.coseva_read_format()).expect("coseva parses");
    let via_core = csv_core_rows(input, dialect).expect("csv-core parses");
    assert_eq!(
        coseva,
        vec![rows_of(&["a\rb"])],
        "coseva keeps a lone CR as data"
    );
    assert_eq!(
        via_core,
        vec![rows_of(&["a"]), rows_of(&["b"])],
        "csv-core treats a lone CR as a record ending"
    );
    assert_ne!(coseva, via_core, "the record-ending difference is real");
}

#[test]
fn csv_core_driver_matches_reference_documents() {
    let dialect = DIALECTS[0];
    let cases: &[(&[u8], Rows)] = &[
        (
            b"a,b\nc,d\n",
            vec![rows_of(&["a", "b"]), rows_of(&["c", "d"])],
        ),
        (b"a,b", vec![rows_of(&["a", "b"])]),
        (b"\"a,b\",c\n", vec![rows_of(&["a,b", "c"])]),
        (b"\"a\"\"b\"\n", vec![rows_of(&["a\"b"])]),
        (b"", Vec::new()),
    ];
    for (input, expected) in cases {
        assert_eq!(
            csv_core_rows(input, dialect).as_ref(),
            Some(expected),
            "csv-core driver on {input:?}"
        );
    }
}

#[test]
fn deterministic_documents_round_trip_through_every_writer_and_reader() {
    let rows: Rows = vec![
        rows_of(&["name", "note"]),
        rows_of(&["quote \" here", "comma , here"]),
        rows_of(&["line\nbreak", "tab\there"]),
        rows_of(&["carriage\rreturn", "crlf\r\npair"]),
        rows_of(&["", "trailing empty"]),
        rows_of(&["only one field"]),
    ];
    for &dialect in DIALECTS {
        let coseva_doc = emit_with_coseva(&rows, dialect);
        assert_every_reader_recovers(&coseva_doc, &rows, dialect, "coseva");
        let csv_doc = emit_with_csv(&rows, dialect);
        assert_every_reader_recovers(&csv_doc, &rows, dialect, "csv");
    }
}

#[test]
fn carve_rows_stays_within_bounds_and_avoids_blank_records() {
    let bytes: Vec<u8> = (0u16..2048).map(|value| (value % 251) as u8).collect();
    let rows = carve_rows(&bytes);
    assert!(rows.len() <= 8, "record cap");
    for row in &rows {
        assert!(row.len() <= 6, "field cap");
        assert!(row.iter().all(|field| field.len() <= 64), "field-size cap");
        assert!(
            !(row.len() <= 1 && row.first().is_none_or(Vec::is_empty)),
            "no blank-emitting record survives"
        );
    }
}

/// Build one record from string fields.
fn rows_of(fields: &[&str]) -> Vec<Vec<u8>> {
    fields
        .iter()
        .map(|field| field.as_bytes().to_vec())
        .collect()
}

//! Indexed generation tests.
//!
//! Exercises `CsvIndex::generate` and `CsvIndex::generate_path`, which build a
//! persistent index while writing a document rather than by reparsing it
//! afterwards. The governing property is that the two routes must agree
//! exactly, so most tests here are differential.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io::{self, Write};

use coseva::ErrorKind;
use coseva::config::{EmitOptions, FormatOptions, Limits, Quoting, RecordEnding, WriteBom};
use coseva::encode_to_vec;
use coseva::encoding::CsvEncode;
use coseva::index::{CsvIndex, IndexOptions};

#[derive(CsvEncode, Clone)]
struct City {
    name: String,
    pop: u32,
}

#[derive(CsvEncode)]
struct Value {
    value: String,
}

#[derive(CsvEncode)]
struct Triple {
    a: String,
    b: String,
    c: String,
}

fn cities(count: u32) -> Vec<City> {
    (0..count)
        .map(|i| City {
            name: format!("City{i}"),
            pop: 1_000 + i,
        })
        .collect()
}

struct LyingWriter;

impl Write for LyingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len() + 1)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Generate a document and index, then rebuild the index by reparsing.
///
/// Returns the generated document, the index written during generation, and
/// the index recovered by indexing that document afterwards.
fn generated_and_reparsed(
    tag: &str,
    values: Vec<City>,
    options: IndexOptions,
    encode: EmitOptions,
) -> Result<(Vec<u8>, CsvIndex, CsvIndex), Box<dyn StdError>> {
    let dir = tempfile::Builder::new().prefix(tag).tempdir()?;
    let csv = dir.path().join("data.csv");
    let index = dir.path().join("data.idx");

    CsvIndex::generate_path(&csv, &index, values, options, encode)?;
    let document = std::fs::read(&csv)?;
    let generated = CsvIndex::load(&index)?;
    let reparsed = CsvIndex::build(&document, options)?;

    Ok((document, generated, reparsed))
}

#[test]
fn a_generated_index_matches_one_built_by_reparsing() -> Result<(), Box<dyn StdError>> {
    // The whole point of indexing during generation is to avoid the reparse,
    // so the two must be indistinguishable.
    let (_, generated, reparsed) = generated_and_reparsed(
        "match",
        cities(50),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn a_generated_document_matches_the_ordinary_emitter() -> Result<(), Box<dyn StdError>> {
    // Indexing must not perturb the bytes that get written.
    let (document, _, _) = generated_and_reparsed(
        "bytes",
        cities(50),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(
        document,
        encode_to_vec(cities(50), FormatOptions::CSV, EmitOptions::new())?
    );
    Ok(())
}

#[test]
fn a_generated_index_indexes_the_header_as_record_zero() -> Result<(), Box<dyn StdError>> {
    let (_, generated, _) = generated_and_reparsed(
        "header",
        cities(3),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(generated.len(), 4);
    assert_eq!(generated.record_offset(0), Some(0));
    Ok(())
}

#[test]
fn generated_offsets_address_the_right_records() -> Result<(), Box<dyn StdError>> {
    let dir = tempfile::Builder::new().prefix("seek").tempdir()?;
    let csv = dir.path().join("data.csv");
    let index = dir.path().join("data.idx");
    CsvIndex::generate_path(
        &csv,
        &index,
        cities(20),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    let document = std::fs::read(&csv)?;
    let generated = CsvIndex::load(&index)?;

    // Record zero is the header, so data record `i` is at index `i + 1`.
    for i in 0..20 {
        let offset = usize::try_from(
            generated
                .record_offset(i + 1)
                .expect("an indexed data record"),
        )?;
        assert!(
            document[offset..].starts_with(format!("City{i},{}\n", 1_000 + i).as_bytes()),
            "record {i} was indexed at the wrong offset"
        );
    }
    Ok(())
}

#[test]
fn a_generated_index_validates_against_its_own_document() -> Result<(), Box<dyn StdError>> {
    // The index binds itself to the exact source bytes, so generation has to
    // compute the same hash the reparsing path would have.
    let (document, generated, _) = generated_and_reparsed(
        "validate",
        cities(30),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    generated.validate_source(&document)?;
    Ok(())
}

#[test]
fn embedded_newlines_do_not_break_generated_line_numbers() -> Result<(), Box<dyn StdError>> {
    // A quoted field may contain line feeds, so physical line numbers advance
    // by more than one per record and must still agree with the parser.
    let values = vec![
        City {
            name: "two\nlines".to_string(),
            pop: 1,
        },
        City {
            name: "three\nmore\nlines".to_string(),
            pop: 2,
        },
        City {
            name: "plain".to_string(),
            pop: 3,
        },
    ];
    let (_, generated, reparsed) = generated_and_reparsed(
        "newlines",
        values,
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(generated, reparsed);
    // Header on line 1, a two-line record, then a three-line record, so the
    // last record begins on line 7.
    assert_eq!(generated.record_line(3), Some(7));
    Ok(())
}

#[test]
fn a_generated_index_matches_when_a_byte_order_mark_is_written() -> Result<(), Box<dyn StdError>> {
    // The emitter normally splices the mark in front of the first record, which
    // would shift every offset already measured.
    let options = IndexOptions {
        format: FormatOptions::CSV.write_bom(WriteBom::Emit),
        limits: coseva::config::Limits::DEFAULT,
    };
    let (document, generated, reparsed) =
        generated_and_reparsed("bom", cities(20), options, EmitOptions::new())?;
    assert!(document.starts_with(b"\xEF\xBB\xBF"));
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn a_generated_index_matches_without_headers() -> Result<(), Box<dyn StdError>> {
    let (_, generated, reparsed) = generated_and_reparsed(
        "noheader",
        cities(20),
        IndexOptions::default(),
        EmitOptions::new().has_headers(false),
    )?;
    assert_eq!(generated.len(), 20);
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn a_generated_index_matches_for_crlf_documents() -> Result<(), Box<dyn StdError>> {
    let options = IndexOptions {
        format: FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        limits: coseva::config::Limits::DEFAULT,
    };
    let (_, generated, reparsed) =
        generated_and_reparsed("crlf", cities(20), options, EmitOptions::new())?;
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn a_generated_index_matches_across_many_buffer_drains() -> Result<(), Box<dyn StdError>> {
    // Offsets are measured against a buffer that is repeatedly drained, so a
    // tiny threshold is what exercises the bookkeeping.
    let (_, generated, reparsed) = generated_and_reparsed(
        "drain",
        cities(500),
        IndexOptions::default(),
        EmitOptions::new().buffer_capacity(8),
    )?;
    assert_eq!(generated.len(), 501);
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn an_empty_generation_still_indexes_the_header() -> Result<(), Box<dyn StdError>> {
    let (_, generated, reparsed) = generated_and_reparsed(
        "empty",
        Vec::new(),
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(generated.len(), 1);
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn generation_rejects_an_invalid_buffer_capacity() {
    let dir = tempfile::Builder::new()
        .prefix("badcap")
        .tempdir()
        .expect("temp directory");
    let error = CsvIndex::generate_path(
        dir.path().join("data.csv"),
        dir.path().join("data.idx"),
        cities(1),
        IndexOptions::default(),
        EmitOptions::new().buffer_capacity(0),
    )
    .expect_err("a zero buffer capacity is invalid");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn generation_enforces_the_stored_parser_limits() {
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(4, 2, 1),
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [Value {
            value: "oversized".to_owned(),
        }],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("generated records must obey the index parser limits");
    assert!(matches!(
        error.kind(),
        ErrorKind::RecordTooLarge { .. } | ErrorKind::FieldTooLarge { .. }
    ));
}

#[test]
fn generation_rejects_a_record_with_too_many_fields() {
    // The reused validator is pinned to `FieldCount::MatchFirst` rather than
    // the engine's default `Flexible`, since that is what disqualifies the
    // owned-parser fast path `reset` would otherwise re-arm unsafely (see
    // `RecordValidator`). This confirms the substitution still enforces
    // `Limits::max_fields` on every record rather than silently waiving it.
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(
            Limits::DEFAULT.max_record_bytes,
            Limits::DEFAULT.max_field_bytes,
            2,
        ),
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [Triple {
            a: "x".to_owned(),
            b: "y".to_owned(),
            c: "z".to_owned(),
        }],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("a three-field record exceeds a two-field limit");
    assert_eq!(error.kind(), ErrorKind::TooManyFields { limit: 2 });
}

#[test]
fn generation_rejects_an_oversized_field_after_earlier_records_reused_the_validator() {
    // The validator is reset and reused between records, so the field-byte
    // check must still fire on a later record even after several short ones
    // already validated successfully on the same parser instance.
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(1024, 8, 16),
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [
            Value {
                value: "ok".to_owned(),
            },
            Value {
                value: "fine".to_owned(),
            },
            Value {
                value: "way too long for the field limit".to_owned(),
            },
        ],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("a field exceeding the byte limit must still be rejected after reuse");
    assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 8 });
}

#[test]
fn generation_rejects_raw_quoting_ambiguity_after_several_valid_records() {
    // Proves the reused validator's per-record `reset` does not mask
    // ambiguity detection for a later record: several unambiguous records
    // validate first, then one whose encoded form is genuinely ambiguous
    // under `Quoting::Raw` must still be caught on the very same parser.
    let options = IndexOptions {
        format: FormatOptions::CSV.quoting(Quoting::Raw),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [
            Value {
                value: "plain".to_owned(),
            },
            Value {
                value: "another".to_owned(),
            },
            Value {
                value: "a\nb".to_owned(),
            },
        ],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("an ambiguous record must be rejected even after prior successful reuse");
    assert_eq!(error.kind(), ErrorKind::Encode);
}

#[test]
fn generation_validator_reuse_matches_reparsing_across_varied_record_shapes()
-> Result<(), Box<dyn StdError>> {
    // Alternates tiny, huge, quoted, embedded-newline and empty fields in one
    // call so the same `PushParser` is reset between very differently shaped
    // records. If reuse ever leaked state between records, this would drift
    // from the independently reparsed index.
    let values = vec![
        City {
            name: String::new(),
            pop: 0,
        },
        City {
            name: "x".repeat(10_000),
            pop: 1,
        },
        City {
            name: "has,a,comma".to_string(),
            pop: 2,
        },
        City {
            name: "quoted \"word\"".to_string(),
            pop: 3,
        },
        City {
            name: "embedded\nnewline".to_string(),
            pop: 4,
        },
        City {
            name: "tiny".to_string(),
            pop: 5,
        },
    ];
    let (_, generated, reparsed) = generated_and_reparsed(
        "shapes",
        values,
        IndexOptions::default(),
        EmitOptions::new(),
    )?;
    assert_eq!(generated, reparsed);
    Ok(())
}

#[test]
fn generation_rejects_one_value_that_parses_as_multiple_records() {
    let options = IndexOptions {
        format: FormatOptions::CSV.quoting(Quoting::Raw),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [Value {
            value: "a\nb".to_owned(),
        }],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("one encoded value cannot create two parser-visible records");
    assert_eq!(error.kind(), ErrorKind::Encode);
}

#[test]
fn generation_rejects_one_value_that_parses_only_as_a_comment() {
    let options = IndexOptions {
        format: FormatOptions::CSV.comment(Some(b'#')).quoting(Quoting::Raw),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        io::Cursor::new(Vec::<u8>::new()),
        [Value {
            value: "#hidden".to_owned(),
        }],
        options,
        EmitOptions::new().has_headers(false),
    )
    .expect_err("one encoded value cannot disappear from the parser-visible index");
    assert_eq!(error.kind(), ErrorKind::Encode);
}

#[test]
fn generation_rejects_a_writer_that_reports_too_many_bytes() {
    let error = CsvIndex::generate(
        LyingWriter,
        std::io::Cursor::new(Vec::<u8>::new()),
        cities(1),
        IndexOptions::default(),
        EmitOptions::new(),
    )
    .expect_err("a writer cannot accept more bytes than it was given");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::InvalidData));
}

/// An index sink that refuses every write but is otherwise a working file.
#[derive(Debug, Default)]
struct RefusingWriter(io::Cursor<Vec<u8>>);

impl Write for RefusingWriter {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "refused"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for RefusingWriter {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl io::Seek for RefusingWriter {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.0.seek(pos)
    }
}

/// A generation writes one index entry per record as it goes, so a sink that
/// fails partway must surface that failure rather than finish with a
/// truncated index. Headers are disabled so that the failure lands on a data
/// record's entry rather than the header's, and enough records are written
/// that the buffered entry writer drains to the sink mid-run rather than only
/// at the close.
#[test]
fn generation_reports_an_index_sink_that_fails_on_a_data_record() {
    let error = CsvIndex::generate(
        io::Cursor::new(Vec::<u8>::new()),
        RefusingWriter::default(),
        cities(2_000),
        IndexOptions::default(),
        EmitOptions::new().has_headers(false),
    )
    .expect_err("the index sink refuses every write");
    assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::PermissionDenied));
}

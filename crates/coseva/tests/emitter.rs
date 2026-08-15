//! Emitter integration tests.
//!
//! These tests cover the I/O-backed [`IoEmitter`], the in-memory [`VecEmitter`],
//! and the shared encoding logic in `encode/mod.rs` and
//! `encode/into_inner_error.rs`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;

use coseva::ByteRecord;
use coseva::ErrorKind;
use coseva::SliceParser;
use coseva::TextRecord;
use coseva::config::{
    EmitOptions, Escape, FieldCount, FormatOptions, Headers, Nulls, ParseOptions, Quoting,
    RecordEnding, Recovery, Syntax, WriteBom,
};
#[cfg(feature = "derive")]
use coseva::encoding::CsvEncode;
use coseva::format::Csv;
use coseva::{IoEmitter, PushEmitter, VecEmitter};

mod common;

use common::{FailingSink, temp_dir, temp_file};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn temp_path(tag: &str) -> common::TempFile {
    temp_file(tag)
}

/// A typed row encoded via the derived [`CsvEncode`] implementation.
#[cfg(feature = "derive")]
#[derive(coseva::encoding::CsvEncode)]
struct City {
    name: String,
    population: u32,
}

/// A typed row encoded via `serde::Serialize`.
#[cfg(feature = "serde")]
#[derive(serde::Serialize)]
struct SerdeCity {
    name: String,
    population: u32,
}

// ── Structural round-trips through IoEmitter/VecEmitter ─────────────────────────

#[test]
fn writer_round_trips_structural_fields() -> Result<(), Box<dyn StdError>> {
    let fields: [&[u8]; 5] = [b"plain", b"a,b", b"say \"hello\"", b"line\nbreak", b""];
    let mut writer = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_record(fields)?;
    let output = writer.into_inner()?;

    let mut reader = SliceParser::with_options(
        &output,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.iter().collect::<Vec<_>>(), fields);
    Ok(())
}

#[test]
fn custom_writer_round_trips_comments_and_escapes() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b';')
        .quote(b'\'')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::Backslash(b'\\'))
        .comment(Some(b'#'));
    let fields: [&[u8]; 3] = [b"#not a comment", b"a'b", b"c\\d"];
    let mut writer = IoEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
    writer.emit_record(fields)?;
    let output = writer.into_inner()?;

    let mut reader =
        SliceParser::with_options(&output, format, ParseOptions::new().headers(Headers::None))?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.iter().collect::<Vec<_>>(), fields);
    Ok(())
}

#[test]
fn emitter_static_path_constructors_match_their_dynamic_counterparts()
-> Result<(), Box<dyn StdError>> {
    let static_path = temp_path("emitter_static_new_path");
    let dynamic_path = temp_path("emitter_dynamic_to_path");
    let _ = std::fs::remove_file(&static_path);
    let _ = std::fs::remove_file(&dynamic_path);

    // Naming the format as a type parameter and passing it as a value must
    // produce byte-identical files, both when creating and when appending.
    {
        let mut writer = IoEmitter::<_, Csv>::new_path(&static_path, EmitOptions::new())?;
        writer.emit_record(["Boston", "650706"])?;
        writer.emit_record(["with, comma", "\"quoted\""])?;
        writer.into_inner()?;
    };
    {
        let mut writer = IoEmitter::<_, Csv>::new_append_path(&static_path, EmitOptions::new())?;
        writer.emit_record(["Rome", "2800000"])?;
        writer.into_inner()?;
    };
    {
        let mut writer = IoEmitter::to_path(&dynamic_path, FormatOptions::CSV, EmitOptions::new())?;
        writer.emit_record(["Boston", "650706"])?;
        writer.emit_record(["with, comma", "\"quoted\""])?;
        writer.into_inner()?;
    };
    {
        let mut writer =
            IoEmitter::append_path(&dynamic_path, FormatOptions::CSV, EmitOptions::new())?;
        writer.emit_record(["Rome", "2800000"])?;
        writer.into_inner()?;
    };

    let from_static = std::fs::read(&static_path)?;
    let from_dynamic = std::fs::read(&dynamic_path)?;
    std::fs::remove_file(&static_path)?;
    std::fs::remove_file(&dynamic_path)?;
    assert_eq!(
        from_static,
        b"Boston,650706\n\"with, comma\",\"\"\"quoted\"\"\"\nRome,2800000\n"
    );
    assert_eq!(from_static, from_dynamic);
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn emitter_append_path_resumes_without_repeating_the_header() -> Result<(), Box<dyn StdError>> {
    let path = temp_path("emitter_append_path_resume");
    let _ = std::fs::remove_file(&path);

    {
        let mut writer = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())?;
        writer.serialize(&SerdeCity {
            name: "Berlin".to_owned(),
            population: 3_700_000,
        })?;
        writer.into_inner()?
    };
    {
        let mut writer = IoEmitter::append_path(&path, FormatOptions::CSV, EmitOptions::new())?;
        writer.serialize(&SerdeCity {
            name: "Rome".to_owned(),
            population: 2_800_000,
        })?;
        writer.into_inner()?
    };

    let output = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(output, b"name,population\nBerlin,3700000\nRome,2800000\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn emitter_append_path_resumes_configured_documents() -> Result<(), Box<dyn StdError>> {
    let path = temp_path("emitter_append_path_resume");
    let _ = std::fs::remove_file(&path);
    let format = FormatOptions::SEMICOLON.write_bom(WriteBom::Emit);
    let options = EmitOptions::new().buffer_capacity(16);

    {
        let mut writer = IoEmitter::to_path(&path, format, options)?;
        writer.serialize(&SerdeCity {
            name: "Paris".to_owned(),
            population: 2_100_000,
        })?;
        writer.into_inner()?
    };
    {
        let mut writer = IoEmitter::append_path(&path, format, options)?;
        writer.serialize(&SerdeCity {
            name: "Milan".to_owned(),
            population: 1_300_000,
        })?;
        writer.into_inner()?
    };

    let output = std::fs::read(&path)?;
    std::fs::remove_file(&path)?;
    assert_eq!(
        output,
        b"\xEF\xBB\xBFname;population\nParis;2100000\nMilan;1300000\n"
    );
    assert_eq!(
        output
            .windows(3)
            .filter(|bytes| *bytes == b"\xEF\xBB\xBF")
            .count(),
        1
    );
    assert_eq!(
        output
            .windows(15)
            .filter(|bytes| *bytes == b"name;population")
            .count(),
        1
    );
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn emitter_append_path_handles_bom_only_and_three_byte_existing_files()
-> Result<(), Box<dyn StdError>> {
    let bom_path = temp_path("emitter_append_path_bom_only");
    std::fs::write(&bom_path, b"\xEF\xBB\xBF")?;
    let format = FormatOptions::CSV.write_bom(WriteBom::Emit);
    {
        let mut writer = IoEmitter::append_path(&bom_path, format, EmitOptions::new())?;
        writer.serialize(&SerdeCity {
            name: "Oslo".to_owned(),
            population: 700_000,
        })?;
        writer.into_inner()?
    };
    let bom_output = std::fs::read(&bom_path)?;
    std::fs::remove_file(&bom_path)?;
    assert_eq!(
        bom_output, b"\xEF\xBB\xBFname,population\nOslo,700000\n",
        "a BOM-only document keeps its mark and still receives a header"
    );

    let three_byte_path = temp_path("emitter_append_path_three_non_bom");
    std::fs::write(&three_byte_path, b"x\n\n")?;
    {
        let mut writer =
            IoEmitter::append_path(&three_byte_path, FormatOptions::CSV, EmitOptions::new())?;
        writer.serialize(&SerdeCity {
            name: "Riga".to_owned(),
            population: 600_000,
        })?;
        writer.into_inner()?
    };
    let three_byte_output = std::fs::read(&three_byte_path)?;
    std::fs::remove_file(&three_byte_path)?;
    assert_eq!(three_byte_output, b"x\n\nRiga,600000\n");
    Ok(())
}

#[test]
fn named_dialects_round_trip_through_writers() -> Result<(), Box<dyn StdError>> {
    for format in [
        FormatOptions::CSV,
        FormatOptions::TSV,
        FormatOptions::SEMICOLON,
        FormatOptions::PIPE,
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::BACKSLASH_TSV,
        FormatOptions::COMMENTED_CSV,
    ] {
        let fields: [&[u8]; 3] = [b"#first", b"a|b,c;\td", b"say \"hello\" \\"];
        let mut writer = IoEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
        writer.emit_record(fields)?;
        let output = writer.into_inner()?;

        let mut reader =
            SliceParser::with_options(&output, format, ParseOptions::new().headers(Headers::None))?;
        let mut line = reader.next_line()?.expect("missing round-trip record");
        let record = line.record()?;
        assert_eq!(record.iter().collect::<Vec<_>>(), fields, "{format:?}");
    }
    Ok(())
}

#[test]
fn sole_empty_field_is_distinct_from_an_empty_record() -> Result<(), coseva::Error> {
    let mut writer = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_record([b""])?;
    assert_eq!(
        writer.into_inner().expect("Vec flush cannot fail"),
        b"\"\"\n"
    );
    Ok(())
}

#[test]
fn vec_writer_encodes_without_staging() -> Result<(), coseva::Error> {
    let mut writer = VecEmitter::default();
    writer.emit_slices(&[b"a,b", b"c"])?;
    assert_eq!(writer.into_inner(), b"\"a,b\",c\n");
    Ok(())
}

#[test]
fn vec_emitter_default_creates_empty_emitter() {
    let enc = VecEmitter::default();
    assert!(enc.as_bytes().is_empty());
}

#[test]
fn vec_writer_rolls_back_rejected_records() -> Result<(), Box<dyn StdError>> {
    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    writer.emit_slices(&[b"valid"])?;
    writer
        .emit_slices(&[b"partial", b"a,b"])
        .expect_err("ambiguous field should fail");
    assert_eq!(writer.into_inner(), b"valid\n");
    Ok(())
}

#[test]
fn vec_emitter_from_existing_content_skips_bom() -> Result<(), Box<dyn StdError>> {
    // When a non-empty Vec is converted, started = true and no BOM is prepended.
    let existing = b"prefix\n".to_vec();
    let mut enc = VecEmitter::from(existing);
    enc.emit_record([b"row"])?;
    let out = enc.into_inner();
    assert!(
        out.starts_with(b"prefix\n"),
        "existing content should be preserved"
    );
    assert!(!out.starts_with(b"\xEF\xBB\xBF"), "should not insert BOM");
    Ok(())
}

#[test]
fn vec_emitter_as_bytes_and_as_vec_borrow_output() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::default();
    enc.emit_record([b"hi"])?;
    assert_eq!(enc.as_bytes(), b"hi\n");
    assert_eq!(enc.as_vec().as_slice(), b"hi\n");
    Ok(())
}

#[test]
fn emitter_get_ref_and_get_mut_borrow_output() -> Result<(), Box<dyn StdError>> {
    let mut enc =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_record([b"x"])?;
    enc.flush()?;
    assert!(!enc.get_ref().is_empty());
    enc.get_mut().clear();
    assert!(enc.get_ref().is_empty());
    Ok(())
}

// ── Randomized and edge-case round-trips ────────────────────────────────────────

/// Deterministic xorshift generator so the differential tests are
/// reproducible without pulling in a dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: u64) -> usize {
        usize::try_from(self.next_u64() % bound)
            .expect("a value reduced modulo the bound fits usize")
    }
}

fn roundtrip(format: FormatOptions, records: &[Vec<Vec<u8>>]) {
    roundtrip_with(format, records, false);
    roundtrip_with(format, records, true);
}

fn roundtrip_with(format: FormatOptions, records: &[Vec<Vec<u8>>], slices: bool) {
    let mut emitter =
        VecEmitter::with_options(Vec::new(), format, EmitOptions::new()).expect("valid format");
    for record in records {
        let fields: Vec<&[u8]> = record.iter().map(Vec::as_slice).collect();
        let result = if slices {
            emitter.emit_slices(&fields)
        } else {
            emitter.emit_record(fields.iter().copied())
        };
        // Some records are unrepresentable under a policy (for example a
        // structural field under `Quoting::Never`); those are rejected at
        // encode time and are not a round-trip concern.
        if result.is_err() {
            return;
        }
    }
    let bytes = emitter.into_inner();
    let mut parser =
        SliceParser::with_options(&bytes, format, ParseOptions::new().headers(Headers::None))
            .expect("valid parse config");
    let mut parsed: Vec<Vec<Vec<u8>>> = Vec::new();
    while let Some(mut line) = parser.next_line().unwrap_or_else(|error| {
        unreachable!(
            "parse error {error:?} for {format:?} on {:?}",
            String::from_utf8_lossy(&bytes)
        )
    }) {
        let record = line.record().expect("record parses");
        parsed.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    assert_eq!(
        &parsed,
        records,
        "roundtrip mismatch for {format:?}; encoded bytes: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

fn gen_field(rng: &mut Rng, alphabet: &[&[u8]]) -> Vec<u8> {
    let len = rng.below(14);
    let mut field = Vec::new();
    for _ in 0..len {
        let choice = rng.below(alphabet.len() as u64);
        field.extend_from_slice(alphabet[choice]);
    }
    field
}

const FUZZ_ALPHABET: &[&[u8]] = &[
    b"a",
    b"b",
    b"1",
    b",",
    b"\"",
    b"\n",
    b"\r",
    b"\t",
    b";",
    b"|",
    b"#",
    b" ",
    b"\\",
    b"~",
    b"\xEF\xBB\xBF",
    b"N",
    b"0",
    b".",
];

const FUZZ_FORMATS: &[FormatOptions] = &[
    FormatOptions::CSV,
    FormatOptions::TSV,
    FormatOptions::SEMICOLON,
    FormatOptions::PIPE,
    FormatOptions::BACKSLASH_CSV,
    FormatOptions::BACKSLASH_TSV,
    FormatOptions::COMMENTED_CSV,
    FormatOptions::MYSQL,
];

#[test]
fn fuzz_roundtrip_many_formats() {
    let extra = [
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        FormatOptions::CSV.quoting(Quoting::Always),
        FormatOptions::CSV.quoting(Quoting::NonNumeric),
        FormatOptions::CSV.quoting(Quoting::Never),
        FormatOptions::BACKSLASH_CSV.quoting(Quoting::Always),
        FormatOptions::BACKSLASH_CSV.quoting(Quoting::Never),
        FormatOptions::CSV.escape(Escape::Backslash(b'~')),
        FormatOptions::CSV.comment(Some(b'#')),
        FormatOptions::CSV
            .record_ending(RecordEnding::Byte(b'\n'))
            .quoting(Quoting::Always),
        FormatOptions::RFC4180.quoting(Quoting::Always),
    ];
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for format in FUZZ_FORMATS.iter().copied().chain(extra) {
        for _ in 0..1500 {
            let record_count = 1 + rng.below(3);
            let records: Vec<Vec<Vec<u8>>> = (0..record_count)
                .map(|_| {
                    let field_count = 1 + rng.below(4);
                    (0..field_count)
                        .map(|_| gen_field(&mut rng, FUZZ_ALPHABET))
                        .collect()
                })
                .collect();
            roundtrip(format, &records);
        }
    }
}

#[test]
fn edge_case_roundtrips() {
    let bom: &[u8] = b"\xEF\xBB\xBF";
    let cases: &[&[&[u8]]] = &[
        &[b""],
        &[b"", b""],
        &[b"", b"", b""],
        &[b"a", b""],
        &[b"", b"a"],
        &[bom],
        &[bom, b"x"],
        &[b"x", bom],
        &[b"\xEF\xBB\xBFdata", b"y"],
        &[b" leading"],
        &[b"trailing "],
        &[b"  "],
        &[b"\t"],
        &[b"\n"],
        &[b"\r"],
        &[b"\r\n"],
        &[b"a\rb"],
        &[b"a\r"],
        &[b"\"\"\""],
        &[b"\\"],
        &[b"\\x"],
        &[b"a,b", b"c\"d", b"e\nf"],
        &[b"#comment"],
        &[b"#a", b"#b"],
    ];
    let formats = [
        FormatOptions::CSV,
        FormatOptions::CSV.record_ending(RecordEnding::CrLf),
        FormatOptions::TSV,
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::CSV.escape(Escape::Backslash(b'~')),
        FormatOptions::COMMENTED_CSV,
        FormatOptions::CSV.quoting(Quoting::Always),
        FormatOptions::CSV.quoting(Quoting::NonNumeric),
        FormatOptions::MYSQL,
    ];
    for format in formats {
        for case in cases {
            let record: Vec<Vec<u8>> = case.iter().map(|field| field.to_vec()).collect();
            roundtrip(format, &[record]);
        }
    }
}

fn nullable_roundtrip(format: FormatOptions, record: &[Option<Vec<u8>>]) {
    let mut emitter =
        VecEmitter::with_options(Vec::new(), format, EmitOptions::new()).expect("valid format");
    let fields = record.iter().map(|field| field.as_ref().map(Vec::as_slice));
    if emitter.emit_nullable_record(fields).is_err() {
        return;
    }
    let bytes = emitter.into_inner();
    let mut parser =
        SliceParser::with_options(&bytes, format, ParseOptions::new().headers(Headers::None))
            .expect("valid parse config");
    let mut parsed: Vec<Vec<Option<Vec<u8>>>> = Vec::new();
    while let Some(mut line) = parser.next_line().expect("line parses") {
        let decoded = line.record().expect("record parses");
        let mut fields = Vec::new();
        for index in 0..decoded.iter().count() {
            if decoded.is_null(index) == Some(true) {
                fields.push(None);
            } else {
                fields.push(Some(decoded.get(index).unwrap_or(b"").to_vec()));
            }
        }
        parsed.push(fields);
    }
    assert_eq!(
        parsed,
        [record.to_vec()],
        "nullable roundtrip mismatch for {format:?}; encoded: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn nullable_roundtrips() {
    let null: Option<Vec<u8>> = None;
    let cases: &[Vec<Option<Vec<u8>>>] = &[
        vec![null.clone(), Some(b"a".to_vec())],
        vec![Some(b"a".to_vec()), null.clone()],
        vec![null.clone(), null.clone()],
        vec![Some(Vec::new()), Some(b"a".to_vec())],
        vec![Some(Vec::new()), null.clone()],
        vec![null.clone(), Some(Vec::new())],
        vec![Some(b"x,y".to_vec()), null.clone(), Some(b"z".to_vec())],
        vec![null, Some(b"a\tb".to_vec()), Some(b"c\nd".to_vec())],
    ];
    for format in [FormatOptions::POSTGRES_COPY_CSV, FormatOptions::MYSQL] {
        for case in cases {
            nullable_roundtrip(format, case);
        }
    }
}

#[test]
fn mysql_null_marker_roundtrips_as_non_null_data() -> Result<(), Box<dyn StdError>> {
    for format in [FormatOptions::MYSQL, FormatOptions::CSV.nulls(Nulls::Mysql)] {
        let mut emitter =
            VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))?;
        emitter.emit_nullable_record([Some(b"\\N".as_slice())])?;
        let encoded = emitter.into_inner();

        let mut parser = SliceParser::with_options(
            &encoded,
            format,
            ParseOptions::new().headers(Headers::None),
        )?;
        let mut line = parser.next_line()?.expect("one record");
        let record = line.record()?;
        assert_eq!(record.is_null(0), Some(false));
        assert_eq!(record.get(0), Some(b"\\N".as_slice()));
    }
    Ok(())
}

#[test]
fn incompatible_parser_and_emitter_quote_settings_are_rejected() {
    let format = FormatOptions::CSV
        .syntax(Syntax::Compatible(Recovery::NONE))
        .quoting(Quoting::Necessary);
    let error = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())
        .expect_err("the parser cannot read protective quotes");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn comments_cannot_share_the_unquoted_escape_byte() {
    let format = FormatOptions::PYTHON_ESCAPED.comment(Some(b'\\'));
    let error = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())
        .expect_err("the escaped record would still begin with a comment byte");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

/// Regression: the `Escape::Mysql` unquoted path must protect a leading
/// UTF-8 BOM or comment byte in the first field. Without protection the
/// raw output begins with the BOM (stripped on read) or the comment byte
/// (the whole line is skipped), silently corrupting the round-trip.
#[test]
fn mysql_protects_leading_bom_and_comment() -> Result<(), Box<dyn StdError>> {
    let mut emitter =
        VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    emitter.emit_record([b"\xEF\xBB\xBFdata".as_slice(), b"y".as_slice()])?;
    let bytes = emitter.into_inner();
    assert!(
        !bytes.starts_with(b"\xEF\xBB\xBF"),
        "encoded MySQL record leaks an unprotected leading BOM: {bytes:?}"
    );
    let mut parser = SliceParser::with_options(
        &bytes,
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    let fields: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
    assert_eq!(fields, vec![b"\xEF\xBB\xBFdata".to_vec(), b"y".to_vec()]);

    let commented = FormatOptions::MYSQL.comment(Some(b'#'));
    let mut emitter = VecEmitter::with_options(Vec::new(), commented, EmitOptions::new())?;
    emitter.emit_record([b"#not-a-comment".as_slice(), b"z".as_slice()])?;
    let bytes = emitter.into_inner();
    assert_ne!(
        bytes.first(),
        Some(&b'#'),
        "encoded MySQL record leaks an unprotected leading comment byte: {bytes:?}"
    );
    let mut parser = SliceParser::with_options(
        &bytes,
        commented,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = parser.next_line()?.expect("one record");
    let record = line.record()?;
    let fields: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
    assert_eq!(fields, vec![b"#not-a-comment".to_vec(), b"z".to_vec()]);
    Ok(())
}

// ── Quoting policies ───────────────────────────────────────────────────────────

#[test]
fn never_quote_rejects_ambiguous_fields() -> Result<(), Box<dyn StdError>> {
    let mut writer = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let error = writer
        .emit_record([b"a,b"])
        .expect_err("ambiguous field should fail");
    assert_eq!(error.kind(), coseva::ErrorKind::Encode);
    Ok(())
}

#[test]
fn io_emitter_rejects_postgres_nulls_without_protective_quoting() {
    let error = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV.quoting(Quoting::Never),
        EmitOptions::new().has_headers(false),
    )
    .expect_err("an empty non-NULL field cannot be represented");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn vec_emitter_rejects_postgres_nulls_without_protective_quoting() {
    let error = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV.quoting(Quoting::Never),
        EmitOptions::new().has_headers(false),
    )
    .expect_err("an empty non-NULL field cannot be represented");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn emitter_encode_nullable_record_never_quoting_structural_field_fails() {
    // emit_nullable_record(inner) returns Err when Quoting::Never and field has comma.
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new().has_headers(false),
    )
    .expect("valid options");
    let err = enc
        .emit_nullable_record::<_, &[u8]>([Some(&b"a,b"[..])])
        .expect_err("comma in field should fail with Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
}

#[test]
fn encode_nullable_record_quoting_never_rejects_structural_field() -> Result<(), Box<dyn StdError>>
{
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let mut rec = ByteRecord::new();
    rec.push_null();
    rec.push_field(b"a,b");
    let err = enc
        .emit_byte_record(&rec)
        .expect_err("structural field should fail");
    assert_eq!(err.kind(), ErrorKind::Encode);
    Ok(())
}

#[test]
fn encode_nullable_record_non_null_never_quoting_with_empty_field() -> Result<(), Box<dyn StdError>>
{
    // With Quoting::Never, an empty first field should succeed (no quoting needed).
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    enc.emit_nullable_record::<_, &[u8]>([Some(&b""[..]), Some(&b"x"[..])])?;
    assert_eq!(enc.into_inner(), b",x\n");
    Ok(())
}

#[test]
fn encode_nullable_record_quoting_always_via_byte_record() -> Result<(), Box<dyn StdError>> {
    let mut rec = ByteRecord::new();
    rec.push_null();
    rec.push_field(b"plain");
    // PostgresCsv nulls + Quoting::Always triggers write_quoted for empty fields
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV.quoting(Quoting::Always),
        EmitOptions::new(),
    )?;
    enc.emit_byte_record(&rec)?;
    // PostgresCsv null is rendered as empty → quoted empty → ""
    let out = enc.into_inner();
    assert_eq!(out, b",\"plain\"\n");
    Ok(())
}

#[test]
fn encode_nullable_record_quoting_raw_writes_bytes_verbatim() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Raw),
        EmitOptions::new(),
    )?;
    let mut rec = ByteRecord::new();
    rec.push_null();
    rec.push_field(b"raw,value");
    enc.emit_byte_record(&rec)?;
    // Null with Nulls::None is treated as empty; Raw quoting writes verbatim
    let out = enc.into_inner();
    assert_eq!(out, b",raw,value\n");
    Ok(())
}

#[test]
fn encode_nullable_record_non_numeric_quoting_quotes_non_numeric_fields()
-> Result<(), Box<dyn StdError>> {
    // emit_nullable_record routes through write_configured_field which dispatches
    // on Quoting::NonNumeric to write_non_numeric.
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::NonNumeric),
        EmitOptions::new(),
    )?;
    // "hello" is non-numeric → quoted; "42" is numeric → unquoted.
    enc.emit_nullable_record::<_, &[u8]>([Some(&b"hello"[..]), Some(&b"42"[..])])?;
    assert_eq!(enc.into_inner(), b"\"hello\",42\n");
    Ok(())
}

#[test]
fn io_emitter_rejects_quoting_with_mysql_escape() {
    let error = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::MYSQL.quoting(Quoting::Always),
        EmitOptions::new().has_headers(false),
    )
    .expect_err("unquoted escape output cannot honor Quoting::Always");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn vec_emitter_rejects_quoting_with_mysql_escape() {
    for quoting in [
        Quoting::Necessary,
        Quoting::Always,
        Quoting::NonNumeric,
        Quoting::Raw,
    ] {
        let format = FormatOptions::new().escape(Escape::Mysql).quoting(quoting);
        let error = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())
            .expect_err("unquoted escape output cannot honor this quoting policy");
        assert_eq!(error.kind(), ErrorKind::Configuration);
    }
}

// ── Field-count validation ─────────────────────────────────────────────────────

#[test]
fn emitter_exact_field_count_passes_when_correct() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    enc.emit_record([b"a", b"b"])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"a,b\n");
    Ok(())
}

#[test]
fn emitter_exact_field_count_rejects_wrong_count() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    let err = enc
        .emit_record([b"single"])
        .expect_err("1 field vs Exact(2) should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    Ok(())
}

#[test]
fn vec_emitter_exact_field_count_rejects_wrong_count() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    let err = enc
        .emit_record([b"only_one"])
        .expect_err("wrong field count should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    // Output was rolled back
    assert!(enc.into_inner().is_empty());
    Ok(())
}

#[test]
fn vec_emitter_match_first_locks_field_count() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::MatchFirst),
    )?;
    enc.emit_record([b"a", b"b"])?;
    let err = enc
        .emit_record([b"x"])
        .expect_err("wrong field count should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    Ok(())
}

#[test]
fn resumed_emitters_preserve_the_document_match_first_width() -> Result<(), Box<dyn StdError>> {
    let existing = b"city,pop\nBoston,650706\n";
    let options = EmitOptions::new().field_count(FieldCount::MatchFirst);

    let mut vector = VecEmitter::with_options(existing.to_vec(), FormatOptions::CSV, options)?;
    let error = vector
        .emit_record(["London"])
        .expect_err("the existing document established a two-field width");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    assert_eq!(vector.as_bytes(), existing, "the rejected append is atomic");

    let path = temp_path("emitter_match_first_resume");
    std::fs::write(&path, existing)?;
    let mut file = IoEmitter::append_path(&path, FormatOptions::CSV, options)?;
    let error = file
        .emit_record(["London"])
        .expect_err("the existing file established a two-field width");
    assert_eq!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    drop(file.into_inner()?);
    assert_eq!(
        std::fs::read(&path)?,
        existing,
        "the rejected file append is atomic"
    );
    std::fs::remove_file(path)?;
    Ok(())
}

#[test]
fn vec_emitter_encode_slices_field_count_mismatch_rolls_back() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(1)),
    )?;
    enc.emit_slices(&[b"ok"])?;
    let start_len = enc.as_bytes().len();
    let err = enc
        .emit_slices(&[b"a", b"b"])
        .expect_err("two fields vs expected 1 should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 1,
            actual: 2
        }
    );
    assert_eq!(
        enc.as_bytes().len(),
        start_len,
        "rollback should restore length"
    );
    Ok(())
}

#[test]
fn vec_emitter_encode_nullable_record_rolls_back_on_count_mismatch() -> Result<(), Box<dyn StdError>>
{
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    let err = enc
        .emit_nullable_record::<_, &[u8]>([None])
        .expect_err("1 field vs expected 2 should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    assert!(enc.into_inner().is_empty());
    Ok(())
}

// ── ByteRecord / TextRecord encoding ────────────────────────────────────────────

#[test]
fn emitter_encode_byte_record_handles_null_aware_record() -> Result<(), Box<dyn StdError>> {
    let mut rec = ByteRecord::new();
    rec.push_null();
    rec.push_field(b"value");
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_byte_record(&rec)?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"\\N\tvalue\n");
    Ok(())
}

#[test]
fn emitter_encode_text_record_simple() -> Result<(), Box<dyn StdError>> {
    let mut rec = TextRecord::new();
    rec.push_field("hello");
    rec.push_field("world");
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_text_record(&rec)?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"hello,world\n");
    Ok(())
}

#[test]
fn emitter_encode_text_record_null_aware() -> Result<(), Box<dyn StdError>> {
    let mut rec = TextRecord::new();
    rec.push_null();
    rec.push_field("val");
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_text_record(&rec)?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"\\N\tval\n");
    Ok(())
}

#[test]
fn vec_emitter_encode_text_record_non_null_aware() -> Result<(), Box<dyn StdError>> {
    let mut rec = TextRecord::new();
    rec.push_field("a");
    rec.push_field("b");
    let mut enc = VecEmitter::default();
    enc.emit_text_record(&rec)?;
    assert_eq!(enc.into_inner(), b"a,b\n");
    Ok(())
}

#[test]
fn vec_emitter_encode_text_record_null_aware() -> Result<(), Box<dyn StdError>> {
    let mut rec = TextRecord::new();
    rec.push_null();
    rec.push_field("val");
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_text_record(&rec)?;
    assert_eq!(enc.into_inner(), b"\\N\tval\n");
    Ok(())
}

#[test]
fn emitter_encode_record_routes_through_nullable_for_mysql() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_record([b"hello", b"world"])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"hello\tworld\n");
    Ok(())
}

#[test]
fn emitter_encode_nullable_record_mysql_nulls() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_nullable_record(vec![None::<&[u8]>, Some(&b"x"[..])])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"\\N\tx\n");
    Ok(())
}

#[test]
fn emitter_encode_nullable_record_none_field_with_nulls_none_writes_empty()
-> Result<(), Box<dyn StdError>> {
    // encode/mod.rs: the `None if nulls == Nulls::None` arm writes the null
    // field as an empty configured field instead of a sentinel, because the
    // format has no distinguished NULL representation.
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    enc.emit_nullable_record(vec![None::<&[u8]>, Some(&b"x"[..])])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b",x\n");
    Ok(())
}

#[test]
fn emitter_encode_nullable_record_with_non_null_fields() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_nullable_record::<_, &[u8]>([Some(&b"a"[..]), Some(&b"b"[..])])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"a,b\n");
    Ok(())
}

#[test]
fn encode_nullable_record_writes_mysql_null_sentinel() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    // Two explicit None fields → both become \N in MySQL format
    enc.emit_nullable_record::<_, &[u8]>([None, None])?;
    assert_eq!(enc.into_inner(), b"\\N\t\\N\n");
    Ok(())
}

// ── Preset dialects (RFC4180 / Excel / PostgreSQL / MySQL) ─────────────────────

#[test]
fn rfc4180_and_excel_writer_presets_emit_crlf() -> Result<(), Box<dyn StdError>> {
    let mut rfc = VecEmitter::with_options(Vec::new(), FormatOptions::RFC4180, EmitOptions::new())?;
    rfc.emit_record([b"alpha".as_slice(), b"beta".as_slice()])?;
    assert_eq!(rfc.into_inner(), b"alpha,beta\r\n");

    let mut excel = VecEmitter::with_options(Vec::new(), FormatOptions::EXCEL, EmitOptions::new())?;
    excel.emit_record([b"alpha".as_slice(), b"beta".as_slice()])?;
    assert_eq!(excel.into_inner(), b"\xEF\xBB\xBFalpha,beta\r\n");
    Ok(())
}

#[test]
fn emitter_excel_format_emits_bom_before_first_record() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::EXCEL, EmitOptions::new())?;
    enc.emit_record([b"alpha" as &[u8], b"beta"])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert!(out.starts_with(b"\xEF\xBB\xBF"), "should start with BOM");
    assert!(out.contains(&b'\r'), "should use CRLF");
    Ok(())
}

#[test]
fn vec_emitter_encode_slices_emits_bom_on_first_record() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::EXCEL, EmitOptions::new())?;
    enc.emit_slices(&[b"x"])?;
    let out = enc.into_inner();
    assert!(out.starts_with(b"\xEF\xBB\xBF"), "should start with BOM");
    Ok(())
}

#[test]
fn postgres_copy_csv_distinguishes_null_from_empty() -> Result<(), Box<dyn StdError>> {
    let mut source = ByteRecord::new();
    source.push_null();
    source.push_field(b"");
    source.push_field(b"value");

    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV,
        EmitOptions::new(),
    )?;
    writer.emit_byte_record(&source)?;
    let encoded = writer.into_inner();
    assert_eq!(encoded, b",\"\",value\n");

    let mut reader = SliceParser::with_options(
        &encoded,
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing PostgreSQL record");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(&b""[..]));
    assert_eq!(record.get(1), Some(&b""[..]));
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
    Ok(())
}

#[test]
fn encode_nullable_record_postgres_csv_quotes_empty_non_null_field() -> Result<(), Box<dyn StdError>>
{
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::POSTGRES_COPY_CSV,
        EmitOptions::new(),
    )?;
    let mut rec = ByteRecord::new();
    rec.push_field(b""); // non-null empty → should be quoted as ""
    rec.push_field(b"value");
    enc.emit_byte_record(&rec)?;
    assert_eq!(enc.into_inner(), b"\"\",value\n");
    Ok(())
}

#[test]
fn mysql_writer_round_trips_nulls_and_escaped_bytes() -> Result<(), Box<dyn StdError>> {
    let mut source = ByteRecord::new();
    source.push_null();
    source.push_field(b"a\tb");
    source.push_field(b"line\nbreak");
    source.push_field(b"slash\\value");
    source.push_field(b"\\N");
    source.push_field([0, 8, 26]);

    let mut writer =
        VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    writer.emit_byte_record(&source)?;
    let encoded = writer.into_inner();
    assert_eq!(
        encoded,
        b"\\N\ta\\tb\tline\\nbreak\tslash\\\\value\t\\\\N\t\\0\\b\\Z\n"
    );

    let mut reader = SliceParser::with_options(
        &encoded,
        FormatOptions::MYSQL,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing MySQL record");
    let record = line.record()?;
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.get(1), Some(&b"a\tb"[..]));
    assert_eq!(record.get(2), Some(&b"line\nbreak"[..]));
    assert_eq!(record.get(3), Some(&b"slash\\value"[..]));
    assert_eq!(record.get(4), Some(&b"\\N"[..]));
    assert_eq!(record.get(5), Some(&[0, 8, 26][..]));
    Ok(())
}

#[test]
fn mysql_escaping_escapes_the_quote_byte() -> Result<(), Box<dyn StdError>> {
    // MySQL escaping never quotes a field, so a bare quote byte in the payload
    // would be read back as opening a quoted field. It has to be escaped.
    let format = FormatOptions::new()
        .escape(Escape::Mysql)
        .quoting(Quoting::Never);
    let mut writer =
        VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))?;
    writer.emit_slices(&[b"say \"hi\""])?;
    let encoded = writer.into_inner();
    assert_eq!(encoded, b"say \\\"hi\\\"\n");

    let mut reader =
        SliceParser::with_options(&encoded, format, ParseOptions::new().headers(Headers::None))?;
    let mut line = reader.next_line()?.expect("missing record");
    assert_eq!(line.record()?.get(0), Some(&b"say \"hi\""[..]));
    Ok(())
}

#[test]
fn vec_emitter_encode_slices_mysql_path() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.emit_slices(&[b"hello", b"world"])?;
    assert_eq!(enc.into_inner(), b"hello\tworld\n");
    Ok(())
}

// ── begin_record / PendingIoRecord builders ──────────────────────────────────────

#[test]
fn emitter_begin_record_builds_and_commits() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    let mut pending = enc.begin_record();
    pending.write_field(b"alpha")?;
    pending.write_field(b"beta")?;
    pending.finish()?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"alpha,beta\n");
    Ok(())
}

#[test]
fn emitter_pending_record_write_null() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    let mut pending = enc.begin_record();
    pending.write_null()?;
    pending.write_field(b"x")?;
    pending.finish()?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"\\N\tx\n");
    Ok(())
}

#[test]
fn emitter_pending_record_drop_without_finish_commits_nothing() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    {
        let mut pending = enc.begin_record();
        pending.write_field(b"discarded")?;
        // Drop without calling finish
    };
    enc.emit_record([b"kept"])?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"kept\n");
    Ok(())
}

// P1 made the builders reuse one staging record held on the emitter instead of
// allocating a fresh one per record. Nothing of the previous record may survive
// into the next one — not its fields, not its NULL flags, and not the fields of
// a guard that was abandoned rather than finished.
#[test]
fn emitter_pending_record_reuses_staging_without_leaking_state() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;

    let mut pending = enc.begin_record();
    pending.write_null()?;
    pending.write_field(b"first")?;
    pending.write_field(b"extra")?;
    pending.finish()?;

    {
        let mut abandoned = enc.begin_record();
        abandoned.write_field(b"discarded")?;
        abandoned.write_null()?;
        // Dropped without `finish`, committing nothing.
    };

    // Fewer fields than the record before it, and no NULL anywhere.
    let mut pending = enc.begin_record();
    pending.write_field(b"second")?;
    pending.write_field(b"")?;
    pending.finish()?;

    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"\\N\tfirst\textra\nsecond\t\n");
    Ok(())
}

#[test]
fn vec_emitter_pending_record_reuses_staging_without_leaking_state() -> Result<(), Box<dyn StdError>>
{
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;

    let mut pending = enc.begin_record();
    pending.write_null()?;
    pending.write_field(b"first")?;
    pending.finish()?;

    {
        let mut abandoned = enc.begin_record();
        abandoned.write_field(b"discarded")?;
        // Dropped without `finish`, committing nothing.
    };

    let mut pending = enc.begin_record();
    pending.write_field(b"second")?;
    pending.write_field(b"")?;
    pending.finish()?;

    assert_eq!(enc.into_inner(), b"\\N\tfirst\nsecond\t\n");
    Ok(())
}

#[test]
fn push_emitter_pending_record_reuses_staging_without_leaking_state()
-> Result<(), Box<dyn StdError>> {
    let mut enc = PushEmitter::with_options(FormatOptions::MYSQL, EmitOptions::new())?;

    let mut pending = enc.begin_record();
    pending.write_null()?;
    pending.write_field(b"first")?;
    pending.finish()?;

    {
        let mut abandoned = enc.begin_record();
        abandoned.write_field(b"discarded")?;
        // Dropped without `finish`, committing nothing.
    };

    let mut pending = enc.begin_record();
    pending.write_field(b"second")?;
    pending.write_field(b"")?;
    pending.finish()?;

    assert_eq!(enc.buffer(), b"\\N\tfirst\nsecond\t\n");
    Ok(())
}

#[test]
fn emitter_pending_record_debug_format() {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())
        .expect("the default CSV format is valid");
    let mut pending = enc.begin_record();
    pending
        .write_field(b"f1")
        .expect("write_field is infallible");
    let debug = format!("{pending:?}");
    assert!(
        debug.contains("PendingIoRecord"),
        "Debug output was: {debug}"
    );
}

#[test]
fn vec_emitter_begin_record_builds_and_commits() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::default();
    let mut pending = enc.begin_record();
    pending.write_field(b"x")?;
    pending.write_field(b"y")?;
    pending.finish()?;
    assert_eq!(enc.into_inner(), b"x,y\n");
    Ok(())
}

#[test]
fn vec_emitter_pending_record_write_null() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    let mut pending = enc.begin_record();
    pending.write_null()?;
    pending.write_field(b"v")?;
    pending.finish()?;
    assert_eq!(enc.into_inner(), b"\\N\tv\n");
    Ok(())
}

#[test]
fn vec_emitter_pending_record_drop_without_finish_commits_nothing() -> Result<(), Box<dyn StdError>>
{
    let mut enc = VecEmitter::default();
    {
        let mut pending = enc.begin_record();
        pending
            .write_field(b"discarded")
            .expect("write_field is infallible");
    };
    enc.emit_record([b"kept"])?;
    assert_eq!(enc.into_inner(), b"kept\n");
    Ok(())
}

#[test]
fn vec_emitter_pending_record_debug_format() {
    let mut enc = VecEmitter::default();
    let mut pending = enc.begin_record();
    pending
        .write_field(b"f1")
        .expect("write_field is infallible");
    let debug = format!("{pending:?}");
    assert!(
        debug.contains("PendingVecRecord"),
        "Debug output was: {debug}"
    );
}

// ── Typed encoding via the derived CsvEncode impl ──────────────────────────────

#[cfg(feature = "derive")]
#[test]
fn emitter_encode_header_emits_field_names() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.encode_header::<City>()?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"name,population\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn emitter_encode_typed_record() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.encode(&City {
        name: "London".to_owned(),
        population: 9_000_000,
    })?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"London,9000000\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn emitter_encode_all_writes_multiple_typed_records() -> Result<(), Box<dyn StdError>> {
    let cities = [
        City {
            name: "Paris".to_owned(),
            population: 2_200_000,
        },
        City {
            name: "Tokyo".to_owned(),
            population: 14_000_000,
        },
    ];
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.encode_all(cities)?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"Paris,2200000\nTokyo,14000000\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn emitter_encode_method_uses_record_collector() -> Result<(), Box<dyn StdError>> {
    // Exercises encode() -> csv_encode() -> DirectEncodeVisitor::visit_field.
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.encode(&City {
        name: "Vienna".to_owned(),
        population: 1_900_000,
    })?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"Vienna,1900000\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn vec_emitter_encode_header_emits_field_names() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::default();
    enc.encode_header::<City>()?;
    assert_eq!(enc.into_inner(), b"name,population\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn vec_emitter_encode_typed_record() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::default();
    enc.encode(&City {
        name: "Madrid".to_owned(),
        population: 3_200_000,
    })?;
    assert_eq!(enc.into_inner(), b"Madrid,3200000\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn vec_emitter_encode_all_writes_multiple_typed_records() -> Result<(), Box<dyn StdError>> {
    let cities = [
        City {
            name: "Seoul".to_owned(),
            population: 9_700_000,
        },
        City {
            name: "Cairo".to_owned(),
            population: 10_000_000,
        },
    ];
    let mut enc = VecEmitter::default();
    enc.encode_all(cities)?;
    assert_eq!(enc.into_inner(), b"Seoul,9700000\nCairo,10000000\n");
    Ok(())
}

/// A type that produces a null field via `csv_encode`, exercising `visit_null`.
#[cfg(feature = "derive")]
struct WithNull;

#[cfg(feature = "derive")]
impl CsvEncode for WithNull {
    fn csv_encode<V: coseva::encoding::EncodeVisitor>(
        &self,
        visitor: &mut V,
    ) -> Result<(), coseva::Error> {
        visitor.visit_null(0, "nullable")?;
        visitor.visit_field(1, "other", b"val")?;
        Ok(())
    }

    fn field_names() -> &'static [&'static str] {
        &["nullable", "other"]
    }
}

#[cfg(feature = "derive")]
#[test]
fn emitter_encode_with_null_field_via_record_collector() -> Result<(), Box<dyn StdError>> {
    // Exercises encode() -> csv_encode() -> DirectEncodeVisitor::visit_null.
    let mut enc = VecEmitter::with_options(Vec::new(), FormatOptions::MYSQL, EmitOptions::new())?;
    enc.encode(&WithNull)?;
    assert_eq!(enc.into_inner(), b"\\N\tval\n");
    Ok(())
}

/// A native row whose field at `bad` carries a comma, so it cannot be written
/// under [`Quoting::Never`]. Used to place a field error at an early, middle,
/// or late position and check the record is rolled back whole.
#[cfg(feature = "derive")]
struct CommaAt {
    fields: usize,
    bad: usize,
}

#[cfg(feature = "derive")]
impl CsvEncode for CommaAt {
    fn csv_encode<V: coseva::encoding::EncodeVisitor>(
        &self,
        visitor: &mut V,
    ) -> Result<(), coseva::Error> {
        for index in 0..self.fields {
            let bytes: &[u8] = if index == self.bad { b"a,b" } else { b"ok" };
            visitor.visit_field(index, "f", bytes)?;
        }
        Ok(())
    }

    fn field_names() -> &'static [&'static str] {
        &["f", "f", "f"]
    }
}

#[cfg(feature = "derive")]
#[test]
fn encode_field_error_is_atomic_at_every_position() -> Result<(), Box<dyn StdError>> {
    // A field error early, in the middle, or late must leave the buffer at
    // exactly its pre-record state: no partial record, prior records intact,
    // and the emitter still usable afterwards.
    for bad in 0..3 {
        let mut enc = VecEmitter::with_options(
            Vec::new(),
            FormatOptions::CSV.quoting(Quoting::Never),
            EmitOptions::new().has_headers(false),
        )?;
        enc.encode(&CommaAt {
            fields: 3,
            bad: usize::MAX,
        })?;
        let committed = enc.as_bytes().to_vec();
        assert_eq!(committed, b"ok,ok,ok\n");

        let err = enc
            .encode(&CommaAt { fields: 3, bad })
            .expect_err("a structural field must fail under Never quoting");
        assert_eq!(err.kind(), ErrorKind::Encode);
        assert_eq!(
            enc.as_bytes(),
            committed.as_slice(),
            "a failed encode left partial bytes (bad field {bad})"
        );

        enc.encode(&CommaAt {
            fields: 3,
            bad: usize::MAX,
        })?;
        assert_eq!(enc.into_inner(), b"ok,ok,ok\nok,ok,ok\n");
    }
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn encode_field_count_mismatch_is_atomic() -> Result<(), Box<dyn StdError>> {
    // A field-count rejection rolls the whole record back, just like a field
    // error, and leaves the prior record untouched.
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new()
            .has_headers(false)
            .field_count(FieldCount::Exact(3)),
    )?;
    enc.encode(&CommaAt {
        fields: 3,
        bad: usize::MAX,
    })?;
    let committed = enc.as_bytes().to_vec();

    let err = enc
        .encode(&CommaAt {
            fields: 2,
            bad: usize::MAX,
        })
        .expect_err("2 fields vs Exact(3) should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 3,
            actual: 2,
        }
    );
    assert_eq!(enc.as_bytes(), committed.as_slice());
    Ok(())
}

// ── Typed encoding via serde::Serialize ─────────────────────────────────────────

#[cfg(feature = "serde")]
#[test]
fn emitter_serialize_emits_headers_on_first_call() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.serialize(&SerdeCity {
        name: "Berlin".to_owned(),
        population: 3_700_000,
    })?;
    enc.serialize(&SerdeCity {
        name: "Rome".to_owned(),
        population: 2_800_000,
    })?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"name,population\nBerlin,3700000\nRome,2800000\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn emitter_serialize_without_headers() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    enc.serialize(&SerdeCity {
        name: "Oslo".to_owned(),
        population: 700_000,
    })?;
    let out = enc.into_inner().expect("Vec flush cannot fail");
    assert_eq!(out, b"Oslo,700000\n");
    Ok(())
}

#[cfg(feature = "derive")]
#[test]
fn segments_flush_buffer_without_starting_a_new_part() -> Result<(), Box<dyn StdError>> {
    #[derive(coseva::encoding::CsvEncode)]
    struct SegmentRow {
        payload: String,
    }

    let directory = temp_dir("segments-threshold-flush")?;
    let base = directory.path().join("segment.csv");
    let rows: Vec<_> = (0..12)
        .map(|index| SegmentRow {
            payload: format!("{index:02}-{}", "x".repeat(48)),
        })
        .collect();
    let paths = coseva::encode_to_segments(
        rows,
        1 << 20,
        |index| base.with_extension(format!("{index}.csv")),
        FormatOptions::CSV,
        EmitOptions::new().buffer_capacity(64),
    )?;

    assert_eq!(paths.len(), 1, "threshold flush must not roll the segment");
    let output = std::fs::read(&paths[0])?;
    for path in &paths {
        std::fs::remove_file(path)?;
    }
    assert!(output.starts_with(b"payload\n"));
    assert_eq!(std::str::from_utf8(&output)?.lines().count(), 13);
    assert!(
        output.len() > 64,
        "the encoded core must cross the configured flush threshold"
    );
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn emitter_serialize_never_quote_with_structural_field_fails() -> Result<(), Box<dyn StdError>> {
    #[derive(serde::Serialize)]
    struct Row {
        field: String,
    }
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new().has_headers(false),
    )?;
    let err = enc
        .serialize(&Row {
            field: "a,b".to_owned(),
        })
        .expect_err("structural field should fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn vec_emitter_serialize_emits_headers_on_first_call() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::default();
    enc.serialize(&SerdeCity {
        name: "Athens".to_owned(),
        population: 660_000,
    })?;
    enc.serialize(&SerdeCity {
        name: "Nairobi".to_owned(),
        population: 4_400_000,
    })?;
    assert_eq!(
        enc.into_inner(),
        b"name,population\nAthens,660000\nNairobi,4400000\n"
    );
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn vec_emitter_serialize_without_headers() -> Result<(), Box<dyn StdError>> {
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().has_headers(false),
    )?;
    enc.serialize(&SerdeCity {
        name: "Lima".to_owned(),
        population: 10_700_000,
    })?;
    assert_eq!(enc.into_inner(), b"Lima,10700000\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn vec_emitter_serialize_never_quote_with_structural_field_fails() -> Result<(), Box<dyn StdError>>
{
    #[derive(serde::Serialize)]
    struct Row {
        field: String,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new().has_headers(false),
    )?;
    let err = enc
        .serialize(&Row {
            field: "a,b".to_owned(),
        })
        .expect_err("structural field should fail");
    assert_eq!(err.kind(), ErrorKind::Encode);
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn vec_emitter_serialize_never_quote_headers_contain_structural_byte_fails()
-> Result<(), Box<dyn StdError>> {
    // A struct whose field name contains a comma means the header itself would
    // need quoting, exercising validate_unquoted_record for headers.
    #[derive(serde::Serialize)]
    struct WeirdHeaders {
        #[serde(rename = "a,b")]
        field: String,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let err = enc
        .serialize(&WeirdHeaders {
            field: "ok".to_owned(),
        })
        .expect_err("header with comma should fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn vec_emitter_serialize_never_quoting_clean_fields_succeeds() -> Result<(), Box<dyn StdError>> {
    // serialize with Quoting::Never and fields/headers that need no quoting
    // reach the loop body and the final Ok(()) in validate_unquoted_record.
    #[derive(serde::Serialize)]
    struct Clean {
        name: String,
        age: u32,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    enc.serialize(&Clean {
        name: "Alice".to_owned(),
        age: 30,
    })?;
    assert_eq!(enc.into_inner(), b"name,age\nAlice,30\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn serialize_field_error_is_atomic_at_every_position() -> Result<(), Box<dyn StdError>> {
    // A Serde field error early, in the middle, or late rolls the record back
    // whole: the prior record survives and the emitter keeps working.
    for bad in 0..3 {
        let mut enc = VecEmitter::with_options(
            Vec::new(),
            FormatOptions::CSV.quoting(Quoting::Never),
            EmitOptions::new().has_headers(false),
        )?;
        enc.serialize(&("ok", "ok", "ok"))?;
        let committed = enc.as_bytes().to_vec();
        assert_eq!(committed, b"ok,ok,ok\n");

        let row: (&str, &str, &str) = match bad {
            0 => ("a,b", "ok", "ok"),
            1 => ("ok", "a,b", "ok"),
            _ => ("ok", "ok", "a,b"),
        };
        let err = enc
            .serialize(&row)
            .expect_err("a structural field must fail under Never quoting");
        assert_eq!(err.kind(), ErrorKind::Encode);
        assert_eq!(
            enc.as_bytes(),
            committed.as_slice(),
            "a failed serialize left partial bytes (bad field {bad})"
        );

        enc.serialize(&("ok", "ok", "ok"))?;
        assert_eq!(enc.into_inner(), b"ok,ok,ok\nok,ok,ok\n");
    }
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn serialize_first_record_failure_leaves_no_header() -> Result<(), Box<dyn StdError>> {
    // When the very first record fails, the header row it would have carried
    // must not be committed either. The next clean record then emits the
    // header for the first time, proving the header state stayed pending.
    #[derive(serde::Serialize)]
    struct Row {
        name: String,
        city: String,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let err = enc
        .serialize(&Row {
            name: "ok".to_owned(),
            city: "a,b".to_owned(),
        })
        .expect_err("a structural data field must fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    assert!(
        enc.as_bytes().is_empty(),
        "a failed first record must leave neither header nor data behind"
    );

    enc.serialize(&Row {
        name: "ok".to_owned(),
        city: "fine".to_owned(),
    })?;
    assert_eq!(enc.into_inner(), b"name,city\nok,fine\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn serialize_header_framing_failure_rolls_back_the_data() -> Result<(), Box<dyn StdError>> {
    // The data record is framed before its header; when the header itself
    // cannot be represented, the already-written data must be rolled back too,
    // leaving nothing behind.
    #[derive(serde::Serialize)]
    struct WeirdHeaders {
        #[serde(rename = "a,b")]
        field: String,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    let err = enc
        .serialize(&WeirdHeaders {
            field: "ok".to_owned(),
        })
        .expect_err("a header comma must fail under Never quoting");
    assert_eq!(err.kind(), ErrorKind::Encode);
    assert!(
        enc.as_bytes().is_empty(),
        "header framing failure must roll back the data record it preceded"
    );
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn serialize_first_record_field_count_mismatch_leaves_no_header() -> Result<(), Box<dyn StdError>> {
    // A field-count rejection on the first record must roll back the header
    // that would have been spliced ahead of it.
    #[derive(serde::Serialize)]
    struct Three {
        a: u32,
        b: u32,
        c: u32,
    }
    let mut enc = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    let err = enc
        .serialize(&Three { a: 1, b: 2, c: 3 })
        .expect_err("3 fields vs Exact(2) should fail");
    assert_eq!(
        err.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 3,
        }
    );
    assert!(
        enc.as_bytes().is_empty(),
        "a first-record count mismatch must leave no header behind"
    );
    Ok(())
}

// ── IoEmitter failure states (I/O errors, poisoning) ──────────────────────────────

#[test]
fn emitter_rejects_writes_after_io_error() {
    // Records are buffered, so the sink is not touched until the buffer
    // drains; the failure surfaces at the flush that forces the write.
    let mut emitter = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    emitter.emit_record([b"field"]).expect("record is buffered");
    assert!(
        matches!(
            emitter.flush().expect_err("budget exhausted").kind(),
            ErrorKind::Io(_)
        ),
        "expected Io error"
    );
    let second = emitter.emit_record([b"another"]);
    assert_eq!(
        second.expect_err("already failed").kind(),
        ErrorKind::EmitterFailed
    );
}

#[test]
fn emitter_encode_nullable_record_after_failure_is_rejected() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    let _ = enc.emit_record([b"x"]);
    let _ = enc.flush();
    let err = enc
        .emit_nullable_record::<_, &[u8]>([None])
        .expect_err("already failed");
    assert_eq!(err.kind(), ErrorKind::EmitterFailed);
}

#[test]
fn emitter_encode_nullable_record_io_failure_mid_write() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(5, std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_nullable_record::<_, &[u8]>([Some(&b"verylongvalue"[..])])
        .expect("record is buffered");
    let err = enc.flush().expect_err("write should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

#[test]
fn emitter_bom_write_failure_marks_failed() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::EXCEL,
        EmitOptions::new(),
    )
    .expect("valid config");
    enc.emit_record([b"x"]).expect("record is buffered");
    let err = enc.flush().expect_err("BOM write should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

#[test]
fn emitter_encode_nullable_record_bom_write_failure() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::EXCEL,
        EmitOptions::new(),
    )
    .expect("valid config");
    enc.emit_nullable_record::<_, &[u8]>([Some(&b"x"[..])])
        .expect("record is buffered");
    let err = enc.flush().expect_err("BOM write should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

#[test]
fn emitter_encode_nullable_record_body_write_failure() {
    // Budget allows the BOM (3 bytes) but fails on the record write.
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(3, std::io::ErrorKind::Other),
        FormatOptions::EXCEL,
        EmitOptions::new(),
    )
    .expect("valid config");
    enc.emit_nullable_record::<_, &[u8]>([Some(&b"x"[..])])
        .expect("record is buffered");
    let err = enc.flush().expect_err("record write should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

#[test]
fn emitter_flush_after_io_error_returns_failed_error() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    let _ = enc.emit_record([b"x"]);
    // The first flush performs the write and reports the I/O failure; only
    // after it is latched does the emitter reject further work outright.
    let err = enc.flush().expect_err("write should fail");
    assert!(matches!(err.kind(), ErrorKind::Io(_)), "{:?}", err.kind());
    let err = enc.flush().expect_err("already failed");
    assert_eq!(err.kind(), ErrorKind::EmitterFailed);
}

#[test]
fn emitter_flush_io_failure() {
    // Budget covers the BOM write but nothing else; flush will fail.
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_flush(std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_record([b"ok"]).expect("write fits budget");
    let err = enc
        .flush()
        .expect_err("flush should fail when budget empty");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

// ── into_inner / IntoInnerError ─────────────────────────────────────────────────

#[test]
fn emitter_into_inner_returns_io_error_on_flush_failure() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_flush(std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_record([b"data"]).expect("write succeeds");
    let err = enc.into_inner().expect_err("flush should fail");
    let _ = err.error(); // exercises IntoInnerError::error
    let _owned = err.into_error(); // exercises IntoInnerError::into_error
}

#[test]
fn emitter_into_inner_error_into_inner_recovers_writer() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_flush(std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_record([b"data"]).expect("write succeeds");
    let err = enc.into_inner().expect_err("flush should fail");
    let _writer: IoEmitter<FailingSink> = err.into_inner(); // exercises IntoInnerError::into_inner
}

#[test]
fn emitter_into_inner_error_debug_and_display() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_flush(std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_record([b"data"]).expect("write succeeds");
    let err = enc.into_inner().expect_err("flush should fail");
    let debug_str = format!("{err:?}");
    assert!(debug_str.contains("IntoInnerError"), "{debug_str}");
    let display_str = format!("{err}");
    assert!(!display_str.is_empty(), "{display_str}");
}

#[test]
fn emitter_into_inner_error_is_std_error() {
    let mut enc = IoEmitter::with_options(
        FailingSink::new().fail_flush(std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    enc.emit_record([b"data"]).expect("write succeeds");
    let err = enc.into_inner().expect_err("flush should fail");
    // IntoInnerError implements std::error::Error; source() delegates to the inner error.
    let source = (&err as &dyn StdError).source();
    assert!(source.is_some(), "source should be Some");
}

#[test]
fn emitter_into_inner_unflushed_returns_writer() {
    let enc = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(0, std::io::ErrorKind::Other),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    let _writer: FailingSink = enc.into_inner_unflushed();
}

// ── encode_to_path ──────────────────────────────────────────────

#[test]
fn emitter_to_path_creates_and_writes_file() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("emitter_to_path");
    let mut enc = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_record([b"col1", b"col2"])?;
    enc.into_inner().expect("file flush cannot fail");
    let bytes = std::fs::read(&path)?;
    assert_eq!(bytes, b"col1,col2\n");
    Ok(())
}

#[test]
fn emitter_to_path_returns_error_for_nonexistent_directory() {
    let directory = common::temp_dir("emitter-missing").expect("temporary directory");
    let path = directory.path().join("missing").join("file.csv");
    let err = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())
        .expect_err("invalid path should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

#[test]
fn emitter_to_path_creates_file() -> Result<(), Box<dyn StdError>> {
    let path = temp_file("emitter_to_path_opts");
    let mut enc = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_record([b"a", b"b"])?;
    enc.into_inner().expect("file flush cannot fail");
    let bytes = std::fs::read(&path)?;
    assert_eq!(bytes, b"a,b\n");
    Ok(())
}

#[test]
fn emitter_to_path_returns_error_for_invalid_path() {
    let directory = common::temp_dir("emitter-invalid").expect("temporary directory");
    let path = directory.path().join("missing").join("file.csv");
    let err = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())
        .expect_err("invalid path should fail");
    assert!(
        matches!(err.kind(), ErrorKind::Io(_)),
        "expected Io error, got {:?}",
        err.kind()
    );
}

// ── Buffered output ─────────────────────────────────────────────────────────────

/// A sink that counts writes so buffering can be observed directly.
#[derive(Debug, Default)]
struct CountingSink {
    bytes: Vec<u8>,
    writes: usize,
}

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writes += 1;
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn emitter_amortizes_many_records_into_few_writes() -> Result<(), Box<dyn StdError>> {
    let mut enc = IoEmitter::with_options(
        CountingSink::default(),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    for _ in 0..1000 {
        enc.emit_record([b"alpha".as_slice(), b"beta".as_slice()])?;
    }
    enc.flush()?;
    let sink = enc.into_inner_unflushed();
    assert_eq!(sink.bytes.len(), 1000 * b"alpha,beta\n".len());
    // 1000 records of 11 bytes against an 8 KiB threshold: a per-record write
    // would be 1000, and one write per drain is a small handful.
    assert!(
        sink.writes < 10,
        "expected records to be amortized, got {} writes",
        sink.writes
    );
    Ok(())
}

#[test]
fn emitter_holds_records_until_the_threshold_is_reached() -> Result<(), Box<dyn StdError>> {
    let mut enc =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_record([b"x"])?;
    assert!(
        enc.get_ref().is_empty(),
        "a small record should still be buffered"
    );
    enc.flush()?;
    assert_eq!(enc.get_ref().as_slice(), b"x\n");
    Ok(())
}

/// A sink whose contents outlive the emitter that writes to it.
#[derive(Clone, Debug, Default)]
struct SharedSink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn dropping_an_emitter_writes_out_buffered_records() {
    // Records are held back until the buffer fills, so without a Drop that
    // drains, a caller that never flushes would silently truncate its output.
    let sink = SharedSink::default();
    {
        let mut enc = IoEmitter::with_options(sink.clone(), FormatOptions::CSV, EmitOptions::new())
            .expect("the default CSV format is valid");
        enc.emit_record([b"kept"]).expect("record is buffered");
        assert!(
            sink.0.borrow().is_empty(),
            "the record should still be buffered"
        );
    };
    assert_eq!(
        sink.0.borrow().as_slice(),
        b"kept\n",
        "dropping the emitter must write out what it buffered"
    );
}

#[test]
fn into_inner_unflushed_discards_buffered_records() {
    // The unflushed escape hatch is documented to commit nothing, so the
    // buffered record must not reappear when the emitter is torn down.
    let sink = SharedSink::default();
    {
        let mut enc = IoEmitter::with_options(sink.clone(), FormatOptions::CSV, EmitOptions::new())
            .expect("the default CSV format is valid");
        enc.emit_record([b"dropped"]).expect("record is buffered");
        let _recovered = enc.into_inner_unflushed();
    }
    assert!(sink.0.borrow().is_empty());
}

#[test]
fn a_record_larger_than_the_threshold_is_written_and_releases_its_capacity()
-> Result<(), Box<dyn StdError>> {
    let threshold = 64;
    let mut enc = IoEmitter::with_options(
        Vec::<u8>::new(),
        FormatOptions::CSV,
        EmitOptions::new().buffer_capacity(threshold),
    )?;
    let huge = vec![b'z'; 8 * 1024];
    enc.emit_record([huge.as_slice()])?;
    // Oversized records must drain rather than accumulate, so the whole record
    // is already visible without an explicit flush.
    assert_eq!(enc.get_ref().len(), huge.len() + 1);
    // Following records must still be encodable and correct.
    enc.emit_record([b"after"])?;
    enc.flush()?;
    assert!(enc.get_ref().ends_with(b"after\n"));
    Ok(())
}

#[test]
fn buffered_output_round_trips_across_a_drain_boundary() -> Result<(), Box<dyn StdError>> {
    // Records that straddle a drain must not be split or duplicated, so parse
    // the output back and compare it to what was written.
    let mut enc = IoEmitter::with_options(
        Vec::<u8>::new(),
        FormatOptions::CSV,
        EmitOptions::new().buffer_capacity(17),
    )?;
    let expected: Vec<Vec<String>> = (0..200)
        .map(|index| {
            vec![
                format!("row{index}"),
                format!("value,{index}"),
                "plain".to_owned(),
            ]
        })
        .collect();
    for record in &expected {
        enc.emit_record(record.iter().map(String::as_bytes))?;
    }
    enc.flush()?;
    let encoded = enc.into_inner_unflushed();

    let mut parser = SliceParser::with_options(
        &encoded,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut round_tripped = Vec::new();
    let mut record = TextRecord::new();
    while let Some(mut line) = parser.next_line()? {
        line.read_text_record_into(&mut record)?;
        round_tripped.push(record.iter().map(str::to_owned).collect::<Vec<_>>());
    }
    assert_eq!(round_tripped, expected);
    Ok(())
}

#[test]
fn io_emitter_reclaims_capacity_grown_by_one_huge_record() -> Result<(), Box<dyn StdError>> {
    // Draining hands back capacity an outlier record grew, without the caller
    // asking; the records written afterwards must be unaffected.
    let mut enc =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    enc.emit_record([vec![b'q'; 256 * 1024].as_slice()])?;
    enc.flush()?;
    for _ in 0..8 {
        enc.emit_record([b"small"])?;
        enc.flush()?;
    }
    assert!(enc.get_ref().ends_with(b"small\n"));
    Ok(())
}

#[test]
fn vec_emitter_keeps_output_intact_across_an_outlier_record() -> Result<(), Box<dyn StdError>> {
    // The output vector belongs to the caller, so nothing here reclaims it;
    // this pins that an outlier record cannot disturb what surrounds it.
    let mut enc = VecEmitter::default();
    let huge = vec![b'x'; 64 * 1024];
    enc.emit_record([huge.as_slice()])?;
    enc.emit_record([b"after".as_slice()])?;
    let output = enc.into_inner();
    assert!(output.starts_with(&huge));
    assert!(output.ends_with(b"after\n"));
    Ok(())
}

#[test]
fn encode_nullable_record_with_a_bad_field_count_commits_nothing() -> Result<(), Box<dyn StdError>>
{
    // A rejected record must leave no partial bytes behind, otherwise the
    // buffered output would be corrupted by a recoverable user error.
    let mut enc = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    enc.emit_nullable_record([Some(b"a".as_slice()), Some(b"b".as_slice())])?;
    let error = enc
        .emit_nullable_record([Some(b"only-one".as_slice())])
        .expect_err("a short record must be rejected");
    assert!(matches!(
        error.kind(),
        ErrorKind::FieldCountMismatch {
            expected: 2,
            actual: 1
        }
    ));
    enc.flush()?;
    let output = enc.into_inner()?;
    assert_eq!(output, b"a,b\n", "the rejected record must not appear");
    Ok(())
}

// ── PushEmitter: the shared encoding core ───────────────────────────────────

#[test]
fn push_emitter_buffer_is_released_by_clear() -> Result<(), Box<dyn StdError>> {
    // The whole output protocol is borrow-then-release, so a caller that
    // drains every record must still see one continuous document.
    let mut emitter = PushEmitter::default();
    let mut written = Vec::new();
    for row in [["a", "b"], ["c", "d"]] {
        emitter.emit_record(row)?;
        written.extend_from_slice(emitter.buffer());
        emitter.clear();
        assert!(emitter.is_empty(), "clear must release the encoded bytes");
    }
    assert_eq!(written, b"a,b\nc,d\n");
    Ok(())
}

#[test]
fn push_emitter_clear_does_not_reopen_the_document() -> Result<(), Box<dyn StdError>> {
    // The byte-order mark is a once-per-document decision. Releasing bytes the
    // caller has already written must not cause a second mark to be emitted.
    let mut emitter = PushEmitter::with_options(
        FormatOptions::CSV.write_bom(WriteBom::Emit),
        EmitOptions::new(),
    )?;
    emitter.emit_record(["a"])?;
    let mut written = emitter.buffer().to_vec();
    emitter.clear();
    emitter.emit_record(["b"])?;
    written.extend_from_slice(emitter.buffer());
    assert_eq!(written, b"\xEF\xBB\xBFa\nb\n");
    Ok(())
}

#[test]
fn push_emitter_emits_no_bom_when_every_record_is_rejected() -> Result<(), Box<dyn StdError>> {
    // A mark introduces a document. If nothing was ever encoded there is no
    // document, so the output must be empty rather than a lone mark.
    let mut emitter = PushEmitter::with_options(
        FormatOptions::CSV
            .quoting(Quoting::Never)
            .write_bom(WriteBom::Emit),
        EmitOptions::new(),
    )?;
    emitter
        .emit_record(["a,b"])
        .expect_err("an ambiguous field must be rejected");
    assert!(
        emitter.buffer().is_empty(),
        "a rejected record must not open the document"
    );
    Ok(())
}

#[test]
fn push_emitter_rolls_back_a_rejected_nullable_record() -> Result<(), Box<dyn StdError>> {
    // A record rejected part-way through encoding must leave no bytes behind,
    // otherwise the next record is appended onto a partial one.
    let mut emitter = PushEmitter::with_options(
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    emitter.emit_nullable_record([Some("valid")])?;
    emitter
        .emit_nullable_record([Some("partial"), Some("a,b")])
        .expect_err("an ambiguous field must be rejected");
    emitter.emit_nullable_record([Some("after")])?;
    assert_eq!(emitter.buffer(), b"valid\nafter\n");
    Ok(())
}

#[test]
fn vec_emitter_rolls_back_a_rejected_nullable_record() -> Result<(), Box<dyn StdError>> {
    // The same guarantee through the vector-backed wrapper. Rollback happens
    // inside the field loop, so it holds no matter which layer reports it.
    let mut emitter = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Never),
        EmitOptions::new(),
    )?;
    emitter.emit_nullable_record([Some("valid")])?;
    emitter
        .emit_nullable_record([Some("partial"), Some("a,b")])
        .expect_err("an ambiguous field must be rejected");
    assert_eq!(emitter.into_inner(), b"valid\n");
    Ok(())
}

#[test]
fn push_emitter_pending_record_commits_only_on_finish() -> Result<(), Box<dyn StdError>> {
    // Dropping the guard must discard the fields it collected.
    let mut emitter = PushEmitter::default();
    {
        let mut pending = emitter.begin_record();
        pending.write_field("discarded")?;
    };
    assert!(emitter.is_empty());
    let mut pending = emitter.begin_record();
    pending.write_field("kept")?;
    pending.write_null()?;
    pending.finish()?;
    assert_eq!(emitter.buffer(), b"kept,\n");
    Ok(())
}

#[test]
fn push_emitter_from_existing_content_appends() -> Result<(), Box<dyn StdError>> {
    // Existing bytes mean the document is already open, so no mark is added.
    let mut emitter = PushEmitter::from(b"existing\n".to_vec());
    emitter.emit_record(["added"])?;
    assert_eq!(emitter.len(), b"existing\nadded\n".len());
    assert_eq!(emitter.into_inner(), b"existing\nadded\n");
    Ok(())
}

#[cfg(feature = "serde")]
#[test]
fn in_memory_emitters_do_not_repeat_serde_headers_when_resuming() -> Result<(), Box<dyn StdError>> {
    let existing = b"name,population\nBerlin,3700000\n";
    let city = SerdeCity {
        name: "Rome".to_owned(),
        population: 2_800_000,
    };

    let mut push = PushEmitter::from(existing.to_vec());
    push.serialize(&city)?;
    assert_eq!(
        push.buffer(),
        b"name,population\nBerlin,3700000\nRome,2800000\n"
    );

    let mut vector = VecEmitter::from(existing.to_vec());
    vector.serialize(&city)?;
    assert_eq!(
        vector.as_bytes(),
        b"name,population\nBerlin,3700000\nRome,2800000\n"
    );
    Ok(())
}

#[test]
fn push_emitter_clear_reclaims_capacity_grown_by_one_huge_record() -> Result<(), Box<dyn StdError>>
{
    // `clear` is where the caller says the encoded bytes are no longer wanted,
    // so it is where a buffer grown by an outlier record is handed back.
    let mut emitter = PushEmitter::default();
    emitter.emit_record([vec![b'x'; 512 * 1024].as_slice()])?;
    let grown = emitter.as_vec().capacity();
    emitter.clear();
    assert!(
        emitter.as_vec().capacity() < grown,
        "clearing should have released the outlier record's capacity"
    );

    emitter.emit_record([b"after".as_slice()])?;
    assert_eq!(emitter.as_vec(), b"after\n");
    Ok(())
}

#[test]
fn all_three_emitters_produce_identical_bytes() -> Result<(), Box<dyn StdError>> {
    // IoEmitter and VecEmitter are wrappers over PushEmitter, so any divergence
    // between the three is a defect rather than a configuration difference.
    let rows: [[&str; 3]; 4] = [
        ["plain", "with,comma", "with\"quote"],
        ["with\nnewline", "", " leading"],
        ["trailing ", "unicode: é", "tab\there"],
        ["a", "b", "c"],
    ];
    let format = FormatOptions::CSV.write_bom(WriteBom::Emit);

    let mut push = PushEmitter::with_options(format, EmitOptions::new())?;
    let mut vec_emitter = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
    let mut writer = IoEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
    for row in rows {
        push.emit_record(row)?;
        vec_emitter.emit_record(row)?;
        writer.emit_record(row)?;
    }
    writer.flush()?;

    let expected = push.into_inner();
    assert_eq!(vec_emitter.into_inner(), expected);
    assert_eq!(writer.into_inner()?, expected);
    Ok(())
}

#[test]
fn push_emitter_default_matches_new() -> Result<(), Box<dyn StdError>> {
    let mut from_default = PushEmitter::default();
    let mut from_new = PushEmitter::default();
    from_default.emit_record(["a", "b"])?;
    from_new.emit_record(["a", "b"])?;
    assert_eq!(from_default.buffer(), from_new.buffer());
    Ok(())
}

#[test]
fn push_emitter_pending_record_reports_its_field_count() -> Result<(), Box<dyn StdError>> {
    let mut emitter = PushEmitter::default();
    let mut pending = emitter.begin_record();
    pending.write_field("one")?;
    pending.write_field("two")?;
    assert!(format!("{pending:?}").contains("pending_fields: 2"));
    Ok(())
}

/// The statically-formatted constructors take `F::FORMAT` instead of an
/// argument, so they are a distinct entry point from `with_options` and are
/// checked here against the run-time-configured spelling of the same format.
#[test]
fn the_static_format_constructors_agree_with_the_configured_ones() {
    const EXPECTED: &[u8] = b"Boston,650706\n";

    let mut io = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    io.emit_record(["Boston", "650706"]).expect("emits");
    assert_eq!(io.into_inner().expect("drains"), EXPECTED);

    let mut vec = VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    vec.emit_record(["Boston", "650706"]).expect("emits");
    assert_eq!(vec.into_inner(), EXPECTED);

    let mut push = PushEmitter::<Csv>::new(EmitOptions::new()).expect("valid options");
    push.emit_record(["Boston", "650706"]).expect("emits");
    assert_eq!(push.into_inner(), EXPECTED);
}

#[test]
fn non_null_raw_entry_points_emit_identical_bytes() {
    const EXPECTED: &[u8] = b"\"first,field\",\"say \"\"hello\"\"\",\"line\nbreak\"\n";
    let fields: [&[u8]; 3] = [b"first,field", b"say \"hello\"", b"line\nbreak"];

    let mut slices = VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    slices.emit_slices(&fields).expect("slices emit");
    assert_eq!(slices.into_inner(), EXPECTED);

    let mut record = VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    record.emit_record(fields).expect("record emits");
    assert_eq!(record.into_inner(), EXPECTED);

    let byte_record: ByteRecord = fields.into_iter().collect();
    let mut bytes = VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    bytes
        .emit_byte_record(&byte_record)
        .expect("byte record emits");
    assert_eq!(bytes.into_inner(), EXPECTED);

    let text_record: TextRecord = ["first,field", "say \"hello\"", "line\nbreak"]
        .into_iter()
        .collect();
    let mut text = VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new()).expect("valid options");
    text.emit_text_record(&text_record)
        .expect("text record emits");
    assert_eq!(text.into_inner(), EXPECTED);
}

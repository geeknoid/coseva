//! Parity tests for writer additions: `from_writer`, `from_path`,
//! `emit_byte_record`, `emit_text_record`, `get_mut`, `as_vec`, and the
//! field-at-a-time `PendingIoRecord` / `PendingVecRecord` guards.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::io;

use coseva::SliceParser;
use coseva::config::{
    EmitOptions, FieldCount, FormatOptions, Headers, ParseOptions, Quoting, WriteBom,
};
use coseva::{ByteRecord, TextRecord};
use coseva::{IoEmitter, VecEmitter};

mod common;

use common::FailingSink;

// ── IoEmitter<W> new constructors ───────────────────────────────────────────────

#[test]
fn from_writer_is_alias_for_new() -> Result<(), coseva::Error> {
    let fields = [b"city".as_ref(), b"pop".as_ref()];
    let mut w1 = IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    let mut w2 = IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    w1.emit_record(fields)?;
    w2.emit_record(fields)?;
    assert_eq!(
        w1.into_inner().expect("Vec flush cannot fail"),
        w2.into_inner().expect("Vec flush cannot fail")
    );
    Ok(())
}

#[test]
fn impossible_writer_buffer_capacity_is_rejected_without_allocating() {
    let result = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().buffer_capacity(usize::MAX),
    );
    let _error = result.expect_err("capacity above isize::MAX should be rejected");
}

#[test]
fn from_path_errors_on_nonexistent_parent_directory() {
    let directory = common::temp_dir("emitter-parity-missing").expect("temporary directory");
    let result = IoEmitter::<std::fs::File>::to_path(
        directory.path().join("missing").join("output.csv"),
        FormatOptions::CSV,
        EmitOptions::new(),
    );
    let _error = result.expect_err("a missing parent directory should fail");
}

// ── IoEmitter<W> emit_byte_record ──────────────────────────────────────────────

#[test]
fn write_byte_record_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = ByteRecord::new();
    record.push_field(b"Boston");
    record.push_field(b"650706");
    record.push_field(b"has,comma");

    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_byte_record(&record)?;
    let output = writer.into_inner()?;

    let mut reader = SliceParser::with_options(
        &output,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    let got: Vec<&[u8]> = row.iter().collect();
    assert_eq!(
        got,
        [
            b"Boston" as &[u8],
            b"650706" as &[u8],
            b"has,comma" as &[u8]
        ]
    );
    Ok(())
}

#[test]
fn write_byte_record_empty_field_is_quoted() -> Result<(), coseva::Error> {
    let mut record = ByteRecord::new();
    record.push_field(b"");

    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_byte_record(&record)?;
    assert_eq!(
        writer.into_inner().expect("Vec flush cannot fail"),
        b"\"\"\n"
    );
    Ok(())
}

// ── IoEmitter<W> emit_text_record ────────────────────────────────────────────

#[test]
fn write_string_record_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = TextRecord::new();
    record.push_field("Paris");
    record.push_field("2161000");
    record.push_field("say \"hello\"");

    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_text_record(&record)?;
    let output = writer.into_inner()?;

    let mut reader = SliceParser::with_options(
        &output,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.get_str(0)?, Some("Paris"));
    assert_eq!(row.get_str(1)?, Some("2161000"));
    assert_eq!(row.get_str(2)?, Some("say \"hello\""));
    Ok(())
}

// ── IoEmitter<W> get_mut ────────────────────────────────────────────────────────

#[test]
fn get_mut_gives_mutable_access_to_inner_writer() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    writer.emit_record([b"x", b"y"])?;
    writer.flush()?;
    let inner: &mut Vec<u8> = writer.get_mut();
    assert!(!inner.is_empty());
    Ok(())
}

// ── IoEmitter<W> PendingIoRecord guard ─────────────────────────────────────────────

#[test]
fn record_guard_commits_exactly_one_row_on_finish() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    let mut row = writer.begin_record();
    row.write_field(b"Boston")?;
    row.write_field(b"650706")?;
    row.finish()?;
    assert_eq!(
        writer.into_inner().expect("Vec flush cannot fail"),
        b"Boston,650706\n"
    );
    Ok(())
}

#[test]
fn record_guard_dropped_without_finish_commits_nothing() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    let mut row = writer.begin_record();
    row.write_field(b"never")?;
    row.write_field(b"written")?;
    drop(row);
    assert_eq!(writer.into_inner().expect("Vec flush cannot fail"), b"");
    Ok(())
}

#[test]
fn record_guard_interleaves_correctly_with_write_record() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;

    writer.emit_record([b"header1", b"header2"])?;

    let mut row = writer.begin_record();
    row.write_field(b"a")?;
    row.write_field(b"b")?;
    row.finish()?;

    writer.emit_record([b"c", b"d"])?;

    assert_eq!(
        writer.into_inner().expect("Vec flush cannot fail"),
        b"header1,header2\na,b\nc,d\n"
    );
    Ok(())
}

#[test]
fn record_guard_empty_row_writes_terminator() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    let row = writer.begin_record();
    row.finish()?;
    // An empty iterator writes just the record record_ending.
    assert_eq!(writer.into_inner().expect("Vec flush cannot fail"), b"\n");
    Ok(())
}

#[test]
fn record_guard_single_empty_field_is_quoted() -> Result<(), coseva::Error> {
    let mut writer =
        IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())?;
    let mut row = writer.begin_record();
    row.write_field(b"")?;
    row.finish()?;
    assert_eq!(
        writer.into_inner().expect("Vec flush cannot fail"),
        b"\"\"\n"
    );
    Ok(())
}

// ── VecEmitter additions ───────────────────────────────────────────────────────

#[test]
fn vec_as_vec_returns_same_content_as_as_bytes() -> Result<(), coseva::Error> {
    let mut writer = VecEmitter::default();
    writer.emit_record([b"hello", b"world"])?;
    assert_eq!(writer.as_vec().as_slice(), writer.as_bytes());
    Ok(())
}

#[test]
fn vec_write_byte_record_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = ByteRecord::new();
    record.push_field(b"Berlin");
    record.push_field(b"3677000");

    let mut writer = VecEmitter::default();
    writer.emit_byte_record(&record)?;

    let mut reader = SliceParser::with_options(
        writer.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.get(0), Some(b"Berlin".as_ref()));
    assert_eq!(row.get(1), Some(b"3677000".as_ref()));
    Ok(())
}

#[test]
fn vec_write_string_record_round_trips() -> Result<(), Box<dyn std::error::Error>> {
    let mut record = TextRecord::new();
    record.push_field("Tokyo");
    record.push_field("13960000");

    let mut writer = VecEmitter::default();
    writer.emit_text_record(&record)?;

    let mut reader = SliceParser::with_options(
        writer.as_bytes(),
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut line = reader.next_line()?.expect("missing row");
    let row = line.record()?;
    assert_eq!(row.get_str(0)?, Some("Tokyo"));
    assert_eq!(row.get_str(1)?, Some("13960000"));
    Ok(())
}

// ── VecEmitter PendingVecRecord guard ──────────────────────────────────────────

#[test]
fn vec_record_guard_commits_exactly_one_row_on_finish() -> Result<(), coseva::Error> {
    let mut writer = VecEmitter::default();
    let mut row = writer.begin_record();
    row.write_field(b"Chicago")?;
    row.write_field(b"2696000")?;
    row.finish()?;
    assert_eq!(writer.into_inner(), b"Chicago,2696000\n");
    Ok(())
}

#[test]
fn vec_record_guard_dropped_without_finish_commits_nothing() -> Result<(), coseva::Error> {
    let mut writer = VecEmitter::default();
    let mut row = writer.begin_record();
    row.write_field(b"phantom")?;
    drop(row);
    assert_eq!(writer.into_inner(), b"");
    Ok(())
}

#[test]
fn vec_record_guard_partial_drop_preserves_prior_rows() -> Result<(), coseva::Error> {
    let mut writer = VecEmitter::default();
    writer.emit_record([b"row1"])?;
    let mut row = writer.begin_record();
    row.write_field(b"discarded")?;
    drop(row);
    writer.emit_record([b"row2"])?;
    assert_eq!(writer.into_inner(), b"row1\nrow2\n");
    Ok(())
}

#[test]
fn writer_emits_one_bom_before_the_first_record() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = IoEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.write_bom(WriteBom::Emit),
        EmitOptions::new(),
    )?;
    writer.emit_record([b"a", b"b"])?;
    writer.emit_record([b"c", b"d"])?;
    assert_eq!(writer.into_inner()?, b"\xEF\xBB\xBFa,b\nc,d\n");
    Ok(())
}

#[test]
fn writers_quote_bom_leading_first_fields() -> Result<(), Box<dyn std::error::Error>> {
    let expected = [b"\xEF\xBB\xBFdata".as_slice(), b"value".as_slice()];
    let mut generic = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    generic.emit_record(expected)?;
    let generic_output = generic.into_inner()?;
    assert_eq!(generic_output, b"\"\xEF\xBB\xBFdata\",value\n");

    let mut direct = VecEmitter::default();
    direct.emit_record(expected)?;
    assert_eq!(direct.as_bytes(), generic_output);

    for output in [generic_output.as_slice(), direct.as_bytes()] {
        let mut reader = SliceParser::with_options(
            output,
            FormatOptions::CSV,
            ParseOptions::new().headers(Headers::None),
        )?;
        let mut line = reader.next_line()?.expect("missing row");
        let row = line.record()?;
        assert_eq!(row.iter().collect::<Vec<_>>(), expected);
    }
    Ok(())
}

#[test]
fn vec_writer_never_inserts_a_bom_into_existing_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = VecEmitter::with_options(
        b"existing\n".to_vec(),
        FormatOptions::CSV.write_bom(WriteBom::Emit),
        EmitOptions::new(),
    )?;
    writer.emit_record([b"a", b"b"])?;
    assert_eq!(writer.into_inner(), b"existing\na,b\n");
    Ok(())
}

#[test]
fn generic_writer_latches_partial_io_failures() {
    let mut writer = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(2, io::ErrorKind::BrokenPipe),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    // Buffered records reach the sink at the drain, so that is where a
    // partial write is detected and latched.
    writer
        .emit_record([b"alpha".as_slice(), b"beta".as_slice()])
        .expect("record is buffered");
    let first = writer
        .flush()
        .expect_err("sink should fail after a partial write");
    assert_eq!(
        first.kind(),
        coseva::ErrorKind::Io(io::ErrorKind::BrokenPipe)
    );
    let committed = writer.get_ref().bytes().len();

    let second = writer
        .emit_record([
            b"must".as_slice(),
            b"not".as_slice(),
            b"continue".as_slice(),
        ])
        .expect_err("failed writer must reject later rows");
    assert_eq!(second.kind(), coseva::ErrorKind::EmitterFailed);
    assert_eq!(writer.into_inner_unflushed().bytes().len(), committed);
}

#[test]
fn generic_writer_latches_flush_failures() -> Result<(), coseva::Error> {
    let mut writer = IoEmitter::with_options(
        FailingSink::new().fail_flush(io::ErrorKind::BrokenPipe),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    writer.emit_record([b"complete"])?;
    let error = writer.flush().expect_err("flush should fail");
    assert_eq!(
        error.kind(),
        coseva::ErrorKind::Io(io::ErrorKind::BrokenPipe)
    );
    let committed = writer.get_ref().bytes().len();
    writer
        .emit_record([b"must-not-continue"])
        .expect_err("flush failure should permanently fail the writer");
    assert_eq!(writer.into_inner_unflushed().bytes().len(), committed);
    Ok(())
}

#[test]
fn into_inner_returns_recoverable_flush_failure() -> Result<(), coseva::Error> {
    let mut writer = IoEmitter::with_options(
        FailingSink::new().fail_flush(io::ErrorKind::BrokenPipe),
        FormatOptions::CSV,
        EmitOptions::new(),
    )?;
    writer.emit_record([b"complete"])?;

    let error = writer
        .into_inner()
        .expect_err("finalization must report the sink's flush failure");
    assert_eq!(
        error.error().kind(),
        coseva::ErrorKind::Io(io::ErrorKind::BrokenPipe)
    );
    let writer = error.into_inner();
    assert_eq!(writer.into_inner_unflushed().bytes(), b"complete\n");
    Ok(())
}

#[test]
fn writer_validates_field_count_before_committing() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )?;
    writer.emit_record([b"a", b"b"])?;
    writer
        .emit_record([b"invalid"])
        .expect_err("wrong-width row should fail");
    assert_eq!(writer.into_inner(), b"a,b\n");
    Ok(())
}

#[test]
fn raw_quote_style_is_explicitly_ambiguous() -> Result<(), Box<dyn std::error::Error>> {
    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::Raw),
        EmitOptions::new(),
    )?;
    writer.emit_record([b"a,b" as &[u8], b"c" as &[u8]])?;
    assert_eq!(writer.into_inner(), b"a,b,c\n");
    Ok(())
}

#[test]
fn non_numeric_quote_style_only_leaves_f64_values_unquoted()
-> Result<(), Box<dyn std::error::Error>> {
    let mut writer = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(Quoting::NonNumeric),
        EmitOptions::new(),
    )?;
    writer.emit_record([
        b"42".as_slice(),
        b"-3.5e2".as_slice(),
        b"NaN".as_slice(),
        b"text".as_slice(),
        b"".as_slice(),
    ])?;
    assert_eq!(writer.into_inner(), b"42,-3.5e2,NaN,\"text\",\"\"\n");
    Ok(())
}

#[test]
fn partial_write_retains_exactly_the_unconfirmed_suffix() {
    let mut writer = IoEmitter::with_options(
        FailingSink::new().fail_after_bytes(2, io::ErrorKind::BrokenPipe),
        FormatOptions::CSV,
        EmitOptions::new(),
    )
    .expect("the default CSV format is valid");
    writer
        .emit_record([b"alpha".as_slice(), b"beta".as_slice()])
        .expect("record is buffered");
    writer
        .flush()
        .expect_err("sink should fail after a partial write");

    // The sink took two bytes and then refused. Concatenating what it kept
    // with what the emitter still holds must reproduce the whole record, with
    // no byte lost and none written twice.
    let encoded = b"alpha,beta\n";
    assert_eq!(writer.get_ref().bytes(), b"al");
    assert_eq!(writer.pending(), &encoded[2..]);

    let mut recovered = writer.get_ref().bytes().to_vec();
    recovered.extend_from_slice(writer.pending());
    assert_eq!(recovered, encoded);
}

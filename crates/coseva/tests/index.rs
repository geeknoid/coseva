//! Persistent index tests.
//!
//! Exercises `CsvIndex`, `CsvIndexReader`, and `IndexOptions`: index
//! construction and seeking, persistence and format encoding, validation
//! against a source, rejection of corrupted indexes, and the error paths of
//! both the eager and the lazy index readers.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::fs;
use std::io::{self, Cursor, Read, Seek};

use coseva::ErrorKind;
use coseva::config::{
    BlankRecords, Escape, FormatOptions, Limits, Nulls, Quoting, ReadBom, RecordEnding, Recovery,
    Syntax, Whitespace, WriteBom,
};
use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};
use xxhash_rust::xxh3::xxh3_128;

mod common;

use common::{FailingReader, FailingSink};

// ── Test helpers ────────────────────────────────────────────────────────────

fn create_private_test_directory() -> Result<common::TempDir, Box<dyn StdError>> {
    Ok(common::temp_dir("coseva-index")?)
}

/// Bytes of the fixed header shared by every persisted index. Index files
/// currently written by this crate (format version 9) trail the entry table
/// with two independent 16-byte checksums (entries, then header) — see
/// `TRAILER_BYTES`. This mirrors the private `FIXED_HEADER_BYTES` constant in
/// `crates/coseva/src/index/format.rs` and is a stable part of the on-disk
/// format.
const FIXED_HEADER_BYTES: usize = 93;

/// Byte offset of encoded format options within the fixed header.
const FORMAT_OFFSET: usize = 36;

/// Byte offset of the first entry within a saved index.
const ENTRY_OFFSET: usize = FIXED_HEADER_BYTES;

/// Byte offset of the declared record count within the fixed header.
const COUNT_OFFSET: usize = FIXED_HEADER_BYTES - 8;

/// Size in bytes of a single `xxh3_128` checksum.
const CHECKSUM_BYTES: usize = 16;

/// Size in bytes of the trailer written after the entry table by the
/// current (version 9) format: an entries checksum followed by an
/// independent header checksum.
const TRAILER_BYTES: usize = 2 * CHECKSUM_BYTES;

const GOLDEN_SOURCE: &[u8] = include_bytes!("fixtures/index/golden.csv");
const GOLDEN_V9_INDEX: &[u8] = include_bytes!("fixtures/index/golden-v9.idx");
const GOLDEN_V8_INDEX: &[u8] = include_bytes!("fixtures/index/golden-v8.idx");
#[cfg(target_pointer_width = "32")]
const OFFSET_OVER_U32_INDEX: &[u8] = include_bytes!("fixtures/index/offset-over-u32-v9.idx");

/// Build an index for `source` with `format`, save it, then load it back.
/// Returns (original, round-tripped) pair.
fn save_and_load(
    source: &[u8],
    format: FormatOptions,
    dir: &std::path::Path,
    stem: &str,
) -> Result<(CsvIndex, CsvIndex), Box<dyn StdError>> {
    let options = IndexOptions {
        format,
        limits: Limits::DEFAULT,
    };
    let original = CsvIndex::build(source, options)?;
    let path = dir.join(format!("{stem}.idx"));
    original.save(&path)?;
    let loaded = CsvIndex::load(&path)?;
    fs::remove_file(&path)?;
    Ok((original, loaded))
}

/// Build a [`CsvIndex`], save it to disk, and open it as a <code>[CsvIndexReader]<File></code>.
fn build_and_open_reader(
    source: &[u8],
    format: FormatOptions,
    dir: &std::path::Path,
    stem: &str,
) -> Result<(std::path::PathBuf, CsvIndexReader<std::fs::File>), Box<dyn StdError>> {
    let options = IndexOptions {
        format,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(source, options)?;
    let path = dir.join(format!("{stem}.idx"));
    index.save(&path)?;
    let reader = CsvIndexReader::open(&path)?;
    Ok((path, reader))
}

/// Build a valid index over `source`, save it, and return the raw bytes.
fn valid_index_bytes(source: &[u8]) -> Result<Vec<u8>, Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("bytes.idx");
    CsvIndex::build(source, IndexOptions::default())?.save(&path)?;
    let bytes = fs::read(&path)?;
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(bytes)
}

/// Build a valid index for `source` entirely in memory (no file I/O).
fn index_bytes_in_memory(source: &[u8]) -> Result<Vec<u8>, Box<dyn StdError>> {
    let reader = CsvIndex::create(
        Cursor::new(source.to_vec()),
        Cursor::new(Vec::<u8>::new()),
        IndexOptions::default(),
    )?;
    Ok(reader.into_inner().into_inner())
}

// ── CsvIndex: building and seeking ──────────────────────────────────────────

#[test]
fn index_seeks_to_exact_records() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n\"c\nc\",d\ne,f\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    assert_eq!(index.len(), 3);
    assert_eq!(index.record_offset(0), Some(0));
    assert_eq!(index.record_offset(1), Some(4));
    assert_eq!(index.record_line(0), Some(1));
    assert_eq!(index.record_line(1), Some(2));
    assert_eq!(index.record_line(2), Some(4));

    let mut reader = index.parser_at(source, 1)?;
    assert_eq!(reader.location().line, 2);
    let (first, second) = {
        let mut line = reader.next_line()?.expect("missing indexed row");
        let row = line.record()?;
        (
            row.get(0).map(<[u8]>::to_vec),
            row.get(1).map(<[u8]>::to_vec),
        )
    };
    assert_eq!(first.as_deref(), Some(b"c\nc".as_slice()));
    assert_eq!(second.as_deref(), Some(b"d".as_slice()));
    assert_eq!(reader.location().line, 4);
    Ok(())
}

#[test]
fn indexed_parsers_report_positions_against_the_whole_source() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\nc,3\nd,4\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;

    let mut reader = index.parser_at(source, 2)?;
    // The seek restores all three counters, not just the physical line.
    assert_eq!(reader.location().byte, 8);
    assert_eq!(reader.location().line, 3);
    assert_eq!(reader.location().record, 2);

    let (field, range, position) = {
        let mut line = reader.next_line()?.expect("missing indexed row");
        let row = line.record()?;
        (
            row.get(0).map(<[u8]>::to_vec),
            row.byte_range(),
            row.index(),
        )
    };
    assert_eq!(field.as_deref(), Some(b"c".as_slice()));
    // Extents are absolute against the file rather than a seeked suffix.
    assert_eq!(range, 8..12);
    assert_eq!(position, 2);

    // Reading on keeps counting from the seeked record.
    let next = {
        let mut line = reader.next_line()?.expect("missing following row");
        let row = line.record()?;
        (
            row.get(0).map(<[u8]>::to_vec),
            row.byte_range(),
            row.index(),
        )
    };
    assert_eq!(next.0.as_deref(), Some(b"d".as_slice()));
    assert_eq!(next.1, 12..16);
    assert_eq!(next.2, 3);
    assert_eq!(reader.location().line, 5);
    Ok(())
}

#[test]
fn index_preserves_bom_bytes_at_noninitial_record_offsets() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n\"\xEF\xBB\xBFdata\",value\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let mut reader = index.parser_at(source, 1)?;
    let mut line = reader.next_line()?.expect("missing indexed row");
    let row = line.record()?;
    assert_eq!(row.get(0), Some(b"\xEF\xBB\xBFdata".as_slice()));
    assert_eq!(row.get(1), Some(b"value".as_slice()));
    Ok(())
}

#[test]
fn index_entry_points_accept_any_byte_source() -> Result<(), Box<dyn StdError>> {
    let source = String::from("a,b\nc,d\n");
    let index = CsvIndex::build(&source, IndexOptions::default())?;
    assert_eq!(index.len(), 2);
    index.validate_source(&source)?;
    let mut reader = index.parser_at(&source, 1)?;
    let mut line = reader.next_line()?.expect("missing record");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"c".as_slice()));
    Ok(())
}

#[test]
fn commented_preset_indexes_only_data_records() -> Result<(), Box<dyn StdError>> {
    let source = b"# ignored\n\nvalue,row\n";
    let index = CsvIndex::build(
        source,
        IndexOptions {
            format: FormatOptions::COMMENTED_CSV,
            limits: Limits::DEFAULT,
        },
    )?;
    assert_eq!(index.len(), 1);
    assert_eq!(index.record_offset(0), Some(11));
    let mut reader = index.parser_at(source, 0)?;
    let mut line = reader.next_line()?.expect("missing indexed record");
    let record = line.record()?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"value".as_slice(), b"row".as_slice()],
    );
    Ok(())
}

#[test]
fn csv_index_is_empty_returns_true_for_empty_source() -> Result<(), Box<dyn StdError>> {
    let index = CsvIndex::build(b"", IndexOptions::default())?;
    assert!(index.is_empty());
    assert_eq!(index.len(), 0);
    Ok(())
}

#[test]
fn csv_index_is_empty_returns_false_for_nonempty_source() -> Result<(), Box<dyn StdError>> {
    let index = CsvIndex::build(b"a,b\n", IndexOptions::default())?;
    assert!(!index.is_empty());
    Ok(())
}

#[test]
fn parser_at_reports_absolute_record_index() -> Result<(), Box<dyn StdError>> {
    // `parser_at` seeks over the whole source, so the counters it restores
    // must be absolute. The first record read back through record 2 has to
    // report index 2, not a suffix-local zero.
    let source = b"a,1\nb,2\nc,3\nd,4\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let mut reader = index.parser_at(source, 2)?;
    assert_eq!(reader.location().record, 2);
    assert_eq!(reader.location().byte, 8);
    assert_eq!(reader.location().line, 3);
    let mut line = reader.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.index(), 2);
    assert_eq!(record.get(0), Some(b"c".as_slice()));
    Ok(())
}

#[test]
fn indexes_a_single_record_without_a_terminator() -> Result<(), Box<dyn StdError>> {
    // A source whose only record has no trailing newline still yields one
    // entry whose offset is the record start and whose line is 1.
    let source = b"solo,row";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    assert_eq!(index.len(), 1);
    assert_eq!(index.record_offset(0), Some(0));
    assert_eq!(index.record_line(0), Some(1));
    assert_eq!(
        index
            .parser_at(source, 1)
            .expect_err("record 1 does not exist")
            .kind(),
        ErrorKind::RecordOutOfRange { record: 1 },
    );
    let mut reader = index.parser_at(source, 0)?;
    let mut line = reader.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"solo".as_slice()));
    assert_eq!(record.get(1), Some(b"row".as_slice()));
    Ok(())
}

// ── CsvIndex: parser entry points ───────────────────────────────────────────

#[test]
fn csv_index_parser_at_reader_seeks_via_cursor() -> Result<(), Box<dyn StdError>> {
    let source = b"aa,1\nbb,2\ncc,3\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let cursor = Cursor::new(source.to_vec());
    let mut parser = index.parser_at_reader(cursor, 1)?;
    let mut line = parser.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"bb".as_slice()));
    assert_eq!(record.index(), 1);
    Ok(())
}

#[test]
fn csv_index_parser_at_reader_rejects_length_mismatch() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    // Source with different length
    let wrong = Cursor::new(b"a,1\nb,2\nc,3\n".to_vec());
    let error = index
        .parser_at_reader(wrong, 0)
        .expect_err("length mismatch must be rejected");
    assert_eq!(error.kind(), ErrorKind::SourceMismatch);
    Ok(())
}

#[test]
fn csv_index_parser_at_path_seeks_to_indexed_record() -> Result<(), Box<dyn StdError>> {
    let source = b"p,1\nq,2\nr,3\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let dir = create_private_test_directory()?;
    let source_path = dir.join("source.csv");
    fs::write(&source_path, source)?;
    let mut parser = index.parser_at_path(&source_path, 2)?;
    let mut line = parser.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"r".as_slice()));
    assert_eq!(record.index(), 2);
    fs::remove_file(&source_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_parser_at_path_rejects_out_of_range_record() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let dir = create_private_test_directory()?;
    let source_path = dir.join("source.csv");
    fs::write(&source_path, source)?;
    let error = index
        .parser_at_path(&source_path, 99)
        .expect_err("out-of-range must fail");
    assert_eq!(error.kind(), ErrorKind::RecordOutOfRange { record: 99 });
    fs::remove_file(&source_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn parser_at_rejects_wrong_source() -> Result<(), Box<dyn StdError>> {
    // validate_source fails when a different buffer is supplied.
    let source = b"a,b\nc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let wrong_source = b"totally different bytes";
    let err = index
        .parser_at(wrong_source, 0)
        .expect_err("wrong source must be rejected");
    assert_eq!(err.kind(), ErrorKind::SourceMismatch);
    Ok(())
}

#[test]
fn parser_at_rejects_out_of_range_record() -> Result<(), Box<dyn StdError>> {
    // location_at fails for a record beyond the index.
    let source = b"a,b\nc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let err = index
        .parser_at(source, 99)
        .expect_err("out-of-range record must be rejected");
    assert!(matches!(err.kind(), ErrorKind::RecordOutOfRange { .. }));
    Ok(())
}

// ── CsvIndex: bound sources ──────────────────────────────────────────────────

/// A byte source that counts how many times it is asked for its bytes.
///
/// `bind` and `parser_at` can only ever reach a source's bytes through
/// [`AsRef::as_ref`], so counting those calls is a direct proof of how many
/// times a source was touched, not merely a proxy for it.
struct CountingSource<'a> {
    bytes: &'a [u8],
    calls: std::cell::Cell<usize>,
}

impl<'a> CountingSource<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            calls: std::cell::Cell::new(0),
        }
    }
}

impl AsRef<[u8]> for CountingSource<'_> {
    fn as_ref(&self) -> &[u8] {
        self.calls.set(self.calls.get() + 1);
        self.bytes
    }
}

#[test]
fn bind_rejects_a_mismatched_source() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\nc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let wrong_source = b"totally different bytes";
    let err = index
        .bind(wrong_source)
        .expect_err("a mismatched source must be rejected before any seek");
    assert_eq!(err.kind(), ErrorKind::SourceMismatch);
    Ok(())
}

#[test]
fn bound_source_parser_at_rejects_out_of_range_record() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\nc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let bound = index.bind(source)?;
    let err = bound
        .parser_at(99)
        .expect_err("out-of-range record must be rejected");
    assert!(matches!(err.kind(), ErrorKind::RecordOutOfRange { .. }));
    Ok(())
}

#[test]
fn bound_source_seeks_match_one_shot_parser_at() -> Result<(), Box<dyn StdError>> {
    // A `BoundSource` must find the exact same records `parser_at` would,
    // since the only thing `bind` changes is when the source is validated.
    let source = b"a,1\nb,2\nc,3\nd,4\n";
    let index = CsvIndex::build(source.as_slice(), IndexOptions::default())?;
    let bound = index.bind(source.as_slice())?;

    for record in 0..index.len() {
        let mut one_shot = index.parser_at(source.as_slice(), record)?;
        let mut bound_parser = bound.parser_at(record)?;
        let mut one_shot_line = one_shot.next_line()?.expect("one-shot record");
        let expected = one_shot_line.record()?;
        let mut bound_line = bound_parser.next_line()?.expect("bound record");
        let actual = bound_line.record()?;
        assert_eq!(expected.get(0), actual.get(0));
        assert_eq!(expected.get(1), actual.get(1));
        assert_eq!(expected.index(), actual.index());
        assert_eq!(expected.byte_range(), actual.byte_range());
    }
    Ok(())
}

#[test]
fn bound_source_seeks_do_not_rehash_the_source() -> Result<(), Box<dyn StdError>> {
    // `parser_at` revalidates, and therefore rehashes, `source` on every call.
    // `bind` hashes it once and hands back a `BoundSource` whose seeks must
    // never touch the source again — proven directly, since hashing or
    // comparing bytes has no way to reach them other than `AsRef::as_ref`.
    let source = b"a,b\nc,d\ne,f\ng,h\n";
    let index = CsvIndex::build(source.as_slice(), IndexOptions::default())?;
    let counting = CountingSource::new(source.as_slice());

    let bound = index.bind(&counting)?;
    assert_eq!(
        counting.calls.get(),
        1,
        "bind must touch the source exactly once, to hash it"
    );

    for record in 0..index.len() {
        let mut parser = bound.parser_at(record)?;
        let mut line = parser.next_line()?.expect("indexed record");
        assert_eq!(line.record()?.index(), record as u64);
    }
    assert_eq!(
        counting.calls.get(),
        1,
        "repeated seeks through a BoundSource must not rehash the source"
    );
    Ok(())
}

#[test]
fn streaming_parsers_reject_a_source_of_the_wrong_length() -> Result<(), Box<dyn StdError>> {
    let index = CsvIndex::build(b"a,b\nc,d\n", IndexOptions::default())?;
    assert_eq!(
        index
            .parser_at_reader(io::Cursor::new(b"a,b\nc,d\ne,f\n".to_vec()), 0)
            .expect_err("a longer source must be rejected")
            .kind(),
        ErrorKind::SourceMismatch,
    );
    Ok(())
}

// ── CsvIndex: source validation ─────────────────────────────────────────────

#[test]
fn index_is_bound_to_source_content() -> Result<(), Box<dyn StdError>> {
    let index = CsvIndex::build(b"a,b\n", IndexOptions::default())?;
    assert_eq!(
        index
            .validate_source(b"a,c\n")
            .expect_err("a different source must be rejected")
            .kind(),
        ErrorKind::SourceMismatch,
    );
    Ok(())
}

#[test]
fn csv_index_validate_reader_accepts_matching_source() -> Result<(), Box<dyn StdError>> {
    let source = b"x,y\n1,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    index.validate_reader(source.as_slice())?;
    Ok(())
}

#[test]
fn csv_index_validate_reader_rejects_different_content() -> Result<(), Box<dyn StdError>> {
    let source = b"x,y\n1,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let error = index
        .validate_reader(b"x,y\n1,3\n".as_slice())
        .expect_err("different content should be rejected");
    assert_eq!(error.kind(), ErrorKind::SourceMismatch);
    Ok(())
}

// ── Persistence: saving, loading, and round trips ───────────────────────────

#[test]
fn save_and_load_round_trip_reproduces_every_field() -> Result<(), Box<dyn StdError>> {
    // The success path: saving a real index and loading it back must
    // reproduce every field exactly.
    let source = b"a,1\nb,2\nc,3\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let directory = create_private_test_directory()?;
    let path = directory.join("roundtrip.idx");
    index.save(&path)?;
    let loaded = CsvIndex::load(&path)?;
    assert_eq!(loaded, index);
    fs::remove_file(path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn persisted_index_retains_parsing_limits() -> Result<(), Box<dyn StdError>> {
    let source = b"abc,d\n";
    let options = IndexOptions {
        limits: Limits::new(6, 3, 2),
        ..IndexOptions::default()
    };
    let index = CsvIndex::build(source, options)?;
    let directory = create_private_test_directory()?;
    let path = directory.join("limits.idx");
    index.save(&path)?;
    let loaded = CsvIndex::load(&path)?;
    assert_eq!(loaded.limits(), options.limits);
    let mut reader = loaded.parser_at(source, 0)?;
    let mut line = reader.next_line()?.expect("missing indexed row");
    let row = line.record()?;
    assert_eq!(row.get(0), Some(b"abc".as_slice()));
    fs::remove_file(path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn indexed_reader_enforces_persisted_limits() -> Result<(), Box<dyn StdError>> {
    let source = b"abc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let directory = create_private_test_directory()?;
    let path = directory.join("limits.idx");
    index.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // Patch max_field_bytes in place: 8 magic + 4 version + 8 source_len
    // + 16 hash + 25 format + 8 max_record_bytes = 69.
    bytes[69..77].copy_from_slice(&2_u64.to_le_bytes());
    // Version 9 trails the entry table with two independent checksums: the
    // entries checksum (over the entry table alone) and the header checksum
    // (over the fixed header alone). Both must be recomputed after patching
    // the header above.
    let entries_checksum_at = bytes.len() - TRAILER_BYTES;
    let header_checksum_at = bytes.len() - CHECKSUM_BYTES;
    let entries_checksum = xxh3_128(&bytes[ENTRY_OFFSET..entries_checksum_at]);
    bytes[entries_checksum_at..header_checksum_at].copy_from_slice(&entries_checksum.to_le_bytes());
    let header_checksum = xxh3_128(&bytes[..FIXED_HEADER_BYTES]);
    bytes[header_checksum_at..].copy_from_slice(&header_checksum.to_le_bytes());
    fs::write(&path, bytes)?;
    let loaded = CsvIndex::load(&path)?;
    let mut parser = loaded.parser_at(source, 0)?;
    let mut line = parser.next_line()?.expect("missing indexed row");
    let error = line
        .record()
        .expect_err("persisted field limit should be enforced");
    assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 2 });
    fs::remove_file(path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn persisted_index_reconstructs_named_reader_preset() -> Result<(), Box<dyn StdError>> {
    let source = b"first,   \"second\"\nnext, value\n";
    let index = CsvIndex::build(
        source,
        IndexOptions {
            format: FormatOptions::PYTHON_CSV,
            limits: Limits::DEFAULT,
        },
    )?;
    let directory = create_private_test_directory()?;
    let path = directory.join("format.idx");
    index.save(&path)?;

    let loaded = CsvIndex::load(&path)?;
    assert_eq!(loaded.format(), FormatOptions::PYTHON_CSV);
    let mut reader = loaded.parser_at(source, 0)?;
    let mut line = reader.next_line()?.expect("missing indexed record");
    let record = line.record()?;
    assert_eq!(
        record.iter().collect::<Vec<_>>(),
        [b"first".as_slice(), b"second".as_slice()],
    );

    fs::remove_file(path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn persisted_indexes_retain_new_dialect_semantics() -> Result<(), Box<dyn StdError>> {
    for (name, source, format, expected_records) in [
        (
            "rfc4180",
            b"alpha,beta\r\ngamma,delta\r\n".as_slice(),
            FormatOptions::RFC4180,
            2,
        ),
        (
            "postgres",
            b",\"\"\nvalue,\n".as_slice(),
            FormatOptions::POSTGRES_COPY_CSV,
            2,
        ),
        (
            "mysql",
            b"\\N\tline\\nbreak\nvalue\t\\\\N\n".as_slice(),
            FormatOptions::MYSQL,
            2,
        ),
    ] {
        let index = CsvIndex::build(
            source,
            IndexOptions {
                format,
                limits: Limits::DEFAULT,
            },
        )?;
        assert_eq!(index.len(), expected_records);
        let directory = create_private_test_directory()?;
        let path = directory.join(format!("{name}.idx"));
        index.save(&path)?;
        let loaded = CsvIndex::load(&path)?;
        assert_eq!(loaded.format(), format);

        let mut reader = loaded.parser_at(source, 0)?;
        let mut line = reader.next_line()?.expect("missing indexed record");
        let record = line.record()?;
        if format == FormatOptions::POSTGRES_COPY_CSV || format == FormatOptions::MYSQL {
            assert_eq!(record.is_null(0), Some(true));
        }

        fs::remove_file(path)?;
        fs::remove_dir(directory)?;
    }
    Ok(())
}

#[test]
fn created_indexes_match_materialized_ones() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\n\"b\nb\",2\nc,3\n";
    let expected = CsvIndex::build(source, IndexOptions::default())?;

    let mut written = io::Cursor::new(Vec::new());
    let mut reader = CsvIndex::create(source.as_slice(), &mut written, IndexOptions::default())?;
    assert_eq!(reader.len(), 3);
    assert!(!reader.is_empty());
    assert_eq!(reader.source_len(), source.len() as u64);
    assert_eq!(reader.format(), expected.format());
    assert_eq!(reader.limits(), expected.limits());
    for record in 0..3 {
        assert_eq!(
            reader.record_offset(record)?,
            expected.record_offset(record)
        );
        assert_eq!(reader.record_line(record)?, expected.record_line(record));
    }
    assert_eq!(reader.record_offset(3)?, None);
    assert_eq!(
        reader
            .location(3)
            .expect_err("record 3 does not exist")
            .kind(),
        ErrorKind::RecordOutOfRange { record: 3 },
    );
    reader.verify()?;
    reader.validate_reader(source.as_slice())?;

    // A constant-memory build produces exactly the format a full build saves.
    let directory = create_private_test_directory()?;
    let saved = directory.join("saved.idx");
    expected.save(&saved)?;
    assert_eq!(fs::read(&saved)?, written.into_inner());

    fs::remove_file(saved)?;
    fs::remove_dir(directory)?;
    Ok(())
}

// ── Format encoding round trips ─────────────────────────────────────────────

/// Build index with format, save, load, verify format is preserved,
/// then check the record count matches [`CsvIndex::build`].
macro_rules! format_round_trip_test {
    ($name:ident, $source:expr, $format:expr) => {
        #[test]
        fn $name() -> Result<(), Box<dyn StdError>> {
            let source: &[u8] = $source;
            let format: FormatOptions = $format;
            let dir = create_private_test_directory()?;
            let (original, loaded) = save_and_load(source, format, &dir, stringify!($name))?;
            assert_eq!(
                original.len(),
                loaded.len(),
                "record count must survive round-trip"
            );
            assert_eq!(
                original.format(),
                loaded.format(),
                "format must survive round-trip"
            );
            assert_eq!(
                original.limits(),
                loaded.limits(),
                "limits must survive round-trip"
            );
            fs::remove_dir(&dir)?;
            Ok(())
        }
    };
}

format_round_trip_test!(
    format_backslash_escape_round_trips,
    b"hello\\,world\nnext,row\n",
    FormatOptions::BACKSLASH_CSV
);

format_round_trip_test!(
    format_custom_byte_record_ending_round_trips,
    b"a,1|b,2|c,3|",
    FormatOptions::new()
        .delimiter(b',')
        .record_ending(RecordEnding::Byte(b'|'))
);

format_round_trip_test!(
    format_skip_blank_records_round_trips,
    b"a,1\n\nb,2\n",
    FormatOptions::new().blank_records(BlankRecords::Skip)
);

format_round_trip_test!(
    format_read_bom_preserve_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().read_bom(ReadBom::Preserve)
);

format_round_trip_test!(
    format_read_bom_reject_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().read_bom(ReadBom::Reject)
);

format_round_trip_test!(
    format_write_bom_emit_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().write_bom(WriteBom::Emit)
);

format_round_trip_test!(
    format_syntax_compatible_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().syntax(Syntax::Compatible(Recovery::PERMISSIVE))
);

format_round_trip_test!(
    format_nulls_postgres_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().nulls(Nulls::PostgresCsv)
);

format_round_trip_test!(
    format_nulls_mysql_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().nulls(Nulls::Mysql)
);

format_round_trip_test!(
    format_quoting_always_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().quoting(Quoting::Always)
);

format_round_trip_test!(
    format_quoting_never_round_trips,
    b"a\\,b,c\n",
    FormatOptions::new()
        .escape(Escape::Unquoted(b'\\'))
        .quoting(Quoting::Never)
);

format_round_trip_test!(
    format_quoting_non_numeric_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().quoting(Quoting::NonNumeric)
);

format_round_trip_test!(
    format_quoting_raw_round_trips,
    b"a,b\n1,2\n",
    FormatOptions::new().quoting(Quoting::Raw)
);

format_round_trip_test!(
    format_comment_byte_round_trips,
    b"# ignored\na,b\n1,2\n",
    FormatOptions::COMMENTED_CSV
);

format_round_trip_test!(
    format_mysql_escape_round_trips,
    b"a\\,b,c\n",
    FormatOptions::new()
        .escape(Escape::Mysql)
        .quoting(Quoting::Never)
);

format_round_trip_test!(
    format_unquoted_escape_round_trips,
    b"a\\,b,c\n",
    FormatOptions::new()
        .escape(Escape::Unquoted(b'\\'))
        .quoting(Quoting::Never)
);

format_round_trip_test!(
    format_crlf_record_ending_round_trips,
    b"a,b\r\n1,2\r\n",
    FormatOptions::new().record_ending(RecordEnding::CrLf)
);

format_round_trip_test!(
    format_trim_whitespace_round_trips,
    b"  a  ,  b  \n1,2\n",
    FormatOptions::new().trim(Whitespace::ALL)
);

#[test]
fn golden_v9_index_loads_expected_records() -> Result<(), Box<dyn StdError>> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index/golden-v9.idx");
    let loaded = CsvIndex::load(path)?;
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded.record_offset(0), Some(0));
    assert_eq!(loaded.record_offset(1), Some(8));
    assert_eq!(loaded.record_offset(2), Some(15));
    assert_eq!(loaded.record_line(0), Some(1));
    assert_eq!(loaded.record_line(1), Some(2));
    assert_eq!(loaded.record_line(2), Some(3));
    loaded.validate_source(GOLDEN_SOURCE)?;
    let mut parser = loaded.parser_at(GOLDEN_SOURCE, 1)?;
    let mut line = parser.next_line()?.expect("missing indexed row");
    assert_eq!(line.record()?.get(0), Some(b"beta".as_slice()));
    Ok(())
}

#[test]
fn golden_v8_index_loads_expected_records() -> Result<(), Box<dyn StdError>> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/index/golden-v8.idx");
    let loaded = CsvIndex::load(path)?;
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded.record_offset(0), Some(0));
    assert_eq!(loaded.record_offset(1), Some(8));
    assert_eq!(loaded.record_offset(2), Some(15));
    loaded.validate_source(GOLDEN_SOURCE)?;

    let mut reader = CsvIndexReader::new(Cursor::new(GOLDEN_V8_INDEX))?;
    reader.verify()?;
    assert_eq!(reader.record_line(2)?, Some(3));
    Ok(())
}

#[test]
fn golden_v9_index_bytes_match_current_encoder() -> Result<(), Box<dyn StdError>> {
    let generated = index_bytes_in_memory(GOLDEN_SOURCE)?;
    assert_eq!(generated, GOLDEN_V9_INDEX);
    assert_eq!(
        &generated[..FIXED_HEADER_BYTES],
        &GOLDEN_V9_INDEX[..FIXED_HEADER_BYTES]
    );
    Ok(())
}

#[cfg(target_pointer_width = "32")]
#[test]
fn index_offset_too_wide_for_usize_is_invalid_index() -> Result<(), Box<dyn StdError>> {
    let mut reader = CsvIndexReader::new(Cursor::new(OFFSET_OVER_U32_INDEX))?;
    reader.verify()?;
    let error = reader
        .location(0)
        .expect_err("a record offset above usize::MAX must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error
            .to_string()
            .contains("record offset does not fit this platform's address width"),
        "{error}"
    );
    Ok(())
}

// ── Corrupted indexes rejected on load and open ─────────────────────────────

#[test]
fn persisted_index_detects_corruption() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\nc,d\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let directory = create_private_test_directory()?;
    let path = directory.join("index.bin");
    index.save(&path)?;
    let loaded = CsvIndex::load(&path)?;
    loaded.validate_source(source)?;
    assert_eq!(loaded.record_offset(1), Some(4));

    let mut bytes = fs::read(&path)?;
    bytes[10] ^= 0x55;
    fs::write(&path, bytes)?;
    assert_eq!(
        CsvIndex::load(&path)
            .expect_err("a truncated index must be rejected")
            .kind(),
        ErrorKind::InvalidIndex,
    );
    fs::remove_file(path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn load_rejects_file_shorter_than_header() -> Result<(), Box<dyn StdError>> {
    // Fewer than FIXED_HEADER_BYTES (93) bytes → read_index_exact in decode_reader fails.
    let dir = create_private_test_directory()?;
    let path = dir.join("short.idx");
    fs::write(&path, b"BCSVIDX2")?; // 8 bytes — far too short
    let error = CsvIndex::load(&path).expect_err("truncated file must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_file_with_wrong_declared_size() -> Result<(), Box<dyn StdError>> {
    // Build a valid 2-record index (133 bytes), then truncate it to 100 bytes.
    // The header still says count=2 → expected_len=133 ≠ file_len=100.
    let source = b"a,b\nc,d\n";
    let dir = create_private_test_directory()?;
    let path = dir.join("trunc.idx");
    CsvIndex::build(source, IndexOptions::default())?.save(&path)?;
    let mut bytes = fs::read(&path)?;
    bytes.truncate(100);
    fs::write(&path, &bytes)?;
    let error = CsvIndex::load(&path).expect_err("wrong length must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_file_with_corrupt_checksum() -> Result<(), Box<dyn StdError>> {
    // Valid header + valid entries + corrupted last byte of checksum.
    let source = b"a,b\nc,d\n";
    let dir = create_private_test_directory()?;
    let path = dir.join("badck.idx");
    CsvIndex::build(source, IndexOptions::default())?.save(&path)?;
    let mut bytes = fs::read(&path)?;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&path, &bytes)?;
    let error = CsvIndex::load(&path).expect_err("bad checksum must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_non_increasing_entry_offsets() -> Result<(), Box<dyn StdError>> {
    // Two records: offsets [0, 4] in an 8-byte source.
    let source = b"a,b\nc,d\n";
    let dir = create_private_test_directory()?;
    let path = dir.join("swap.idx");
    CsvIndex::build(source, IndexOptions::default())?.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // Swap the two 16-byte entries so offsets are [4, 0] (non-increasing).
    // decode_reader validates check_entry with the actual previous offset,
    // so the second entry triggers "not strictly increasing".
    let first = bytes[ENTRY_OFFSET..ENTRY_OFFSET + 16].to_vec();
    let second = bytes[ENTRY_OFFSET + 16..ENTRY_OFFSET + 32].to_vec();
    bytes[ENTRY_OFFSET..ENTRY_OFFSET + 16].copy_from_slice(&second);
    bytes[ENTRY_OFFSET + 16..ENTRY_OFFSET + 32].copy_from_slice(&first);
    fs::write(&path, &bytes)?;
    // load() calls decode_reader which fires check_entry before the checksum check.
    let error = CsvIndex::load(&path).expect_err("non-increasing offsets must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

/// Overwrite the declared record count of a persisted index and refresh its
/// trailing checksum accordingly, without touching the location table bytes.
/// This produces headers whose declared count disagrees with the real payload,
/// which is how the count-driven overflow and length checks in `load` are
/// reached without ever writing a location table that large.
fn tamper_declared_count(mut bytes: Vec<u8>, new_count: u64) -> Vec<u8> {
    bytes[COUNT_OFFSET..FIXED_HEADER_BYTES].copy_from_slice(&new_count.to_le_bytes());
    // Version 9 trails the entry table with an entries checksum followed by
    // an independent header checksum; only the header changed above, but
    // both must be rewritten since they occupy fixed trailing positions.
    let entries_checksum_at = bytes.len() - TRAILER_BYTES;
    let header_checksum_at = bytes.len() - CHECKSUM_BYTES;
    let entries_checksum = xxh3_128(&bytes[ENTRY_OFFSET..entries_checksum_at]);
    bytes[entries_checksum_at..header_checksum_at].copy_from_slice(&entries_checksum.to_le_bytes());
    let header_checksum = xxh3_128(&bytes[..FIXED_HEADER_BYTES]);
    bytes[header_checksum_at..].copy_from_slice(&header_checksum.to_le_bytes());
    bytes
}

#[test]
fn load_rejects_a_file_shorter_than_the_header_and_checksum() -> Result<(), Box<dyn StdError>> {
    // Any file too small to hold even the fixed header plus checksum must be
    // rejected before decoding attempts to read past its end.
    let dir = create_private_test_directory()?;
    let path = dir.join("tiny.idx");
    fs::write(&path, [0; 4])?;
    let error = CsvIndex::load(&path).expect_err("a truncated file must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_a_corrupted_payload_byte() -> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("payload.idx");
    CsvIndex::build(b"a,b\nc,d\n", IndexOptions::default())?.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // Flip one payload byte inside the recorded source hash, which spans
    // bytes 20..36 (8 magic + 4 version + 8 source_len). Every field the
    // header decoder validates stays intact and the entry table is untouched,
    // so nothing but the trailing checksum can notice the damage.
    bytes[24] ^= 0xFF;
    fs::write(&path, &bytes)?;
    let error = CsvIndex::load(&path).expect_err("a corrupted payload must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_a_declared_count_that_overflows_the_location_table_size()
-> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("count_overflow.idx");
    CsvIndex::build(b"a,b\n", IndexOptions::default())?.save(&path)?;
    let bytes = fs::read(&path)?;
    // A declared count this large cannot possibly have a matching location
    // table: multiplying it by the 16-byte entry width already overflows
    // `u64`, so `load` must reject it before ever trying to size or fill a
    // location table.
    fs::write(&path, tamper_declared_count(bytes, 1u64 << 62))?;
    let error = CsvIndex::load(&path).expect_err("an overflowing declared count must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn load_rejects_a_location_table_whose_length_does_not_match_the_declared_count()
-> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("count_mismatch.idx");
    CsvIndex::build(b"a,b\n", IndexOptions::default())?.save(&path)?;
    let bytes = fs::read(&path)?;
    // The declared count disagrees with the number of entries actually
    // present, without overflowing the size computation itself.
    fs::write(&path, tamper_declared_count(bytes, 2))?;
    let error =
        CsvIndex::load(&path).expect_err("a mismatched location table length must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

/// A sparse index file is rejected on its contents, not by exhausting memory.
///
/// The declared entry count is untrusted and is checked only against the file's
/// *logical* length, which `set_len` here inflates to roughly 4 TiB over a file
/// that stores almost nothing. Pre-sizing the tables to that count would commit
/// memory proportional to a length the file cannot back, so the eager
/// reservation is capped and the tables grow as entries are actually read. The
/// sparse region reads back as zeroes, and the first entry fails validation
/// against the four-byte source -- which is what the message asserted below
/// proves: the loader reached entry validation rather than dying in the
/// allocator.
#[test]
fn load_rejects_a_sparse_index_without_a_count_proportional_allocation()
-> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("offset_allocation.idx");
    CsvIndex::build(b"a,b\n", IndexOptions::default())?.save(&path)?;
    let bytes = fs::read(&path)?;
    let huge_count = 1u64 << 38;
    fs::write(&path, tamper_declared_count(bytes, huge_count))?;
    let expected_len = FIXED_HEADER_BYTES as u64 + 16 * huge_count + TRAILER_BYTES as u64;
    fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_len(expected_len)?;
    let error = CsvIndex::load(&path).expect_err("a sparse index must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error.to_string().contains("record offset exceeds source"),
        "unexpected error: {error}"
    );
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

/// Overwrite the declared record count in a persisted index's header with a
/// value chosen so that `16 * count + FIXED_HEADER_BYTES` computes cleanly as
/// a `u64`, but adding the checksum length on top overflows `u64::MAX`. This
/// reaches the "index length overflows u64" checks in both `CsvIndex::load`
/// and `CsvIndexReader::open` without ever needing a genuinely huge file: the
/// overflow is caught from the header alone, before any comparison against
/// the file's actual length.
fn write_index_with_overflowing_declared_count(
    source: &[u8],
    path: &std::path::Path,
) -> Result<(), Box<dyn StdError>> {
    let index = CsvIndex::build(source, IndexOptions::default())?;
    index.save(path)?;
    let mut bytes = fs::read(path)?;
    // `16 * count + FIXED_HEADER_BYTES` sits 10 bytes below `u64::MAX`, so
    // adding the 16-byte checksum length on top wraps past it.
    let huge_count = (1u64 << 60) - 6;
    bytes[COUNT_OFFSET..FIXED_HEADER_BYTES].copy_from_slice(&huge_count.to_le_bytes());
    fs::write(path, &bytes)?;
    Ok(())
}

#[test]
fn load_rejects_a_header_whose_declared_count_overflows_the_expected_length()
-> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("overflow.idx");
    write_index_with_overflowing_declared_count(b"a,b\n", &path)?;
    let error = CsvIndex::load(&path).expect_err("an overflowing declared count must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_dir_all(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_open_rejects_a_header_whose_declared_count_overflows_the_expected_length()
-> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let path = dir.join("overflow.idx");
    write_index_with_overflowing_declared_count(b"a,b\n", &path)?;
    let error =
        CsvIndexReader::open(&path).expect_err("an overflowing declared count must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_dir_all(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_new_rejects_truncated_header() {
    let too_short = vec![0u8; 10];
    let error =
        CsvIndexReader::new(Cursor::new(too_short)).expect_err("truncated header must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
}

#[test]
fn csv_index_reader_new_rejects_bad_magic() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n";
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "bad_magic")?;
    let mut bytes = fs::read(&path)?;
    // Overwrite magic bytes
    bytes[..8].copy_from_slice(b"WRONGMAG");
    let error = CsvIndexReader::new(Cursor::new(bytes)).expect_err("bad magic must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_new_rejects_wrong_version() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n";
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "bad_ver")?;
    let mut bytes = fs::read(&path)?;
    // Overwrite version (bytes 8..12)
    bytes[8..12].copy_from_slice(&99u32.to_le_bytes());
    let error = CsvIndexReader::new(Cursor::new(bytes)).expect_err("wrong version must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_new_rejects_wrong_table_length() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n1,2\n";
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "bad_len")?;
    let mut bytes = fs::read(&path)?;
    // Append an extra byte so the total length no longer matches the header
    bytes.push(0xFF);
    let error = CsvIndexReader::new(Cursor::new(bytes)).expect_err("wrong length must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn index_reader_rejects_entry_with_offset_exceeding_source() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n"; // 4 bytes
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "oor_offset")?;
    let mut bytes = fs::read(&path)?;
    // Set the first entry's offset (bytes ENTRY_OFFSET..ENTRY_OFFSET+8) to 100,
    // which exceeds source_len=4. CsvIndexReader::new passes without validating
    // entries; the check fires lazily when entry(0) is called.
    bytes[ENTRY_OFFSET..ENTRY_OFFSET + 8].copy_from_slice(&100u64.to_le_bytes());
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("header is still valid");
    let error = reader
        .record_offset(0)
        .expect_err("offset exceeding source must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn index_reader_rejects_entry_with_zero_line_number() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n"; // 4 bytes, 1 record
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "zero_line")?;
    let mut bytes = fs::read(&path)?;
    // Set the first entry's line (bytes ENTRY_OFFSET+8..ENTRY_OFFSET+16) to 0.
    // Lines are 1-based; 0 is invalid.
    bytes[ENTRY_OFFSET + 8..ENTRY_OFFSET + 16].copy_from_slice(&0u64.to_le_bytes());
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("header is still valid");
    let error = reader
        .record_offset(0)
        .expect_err("zero line number must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_verify_rejects_corrupted_checksum() -> Result<(), Box<dyn StdError>> {
    let source = b"alpha,1\nbeta,2\n";
    let dir = create_private_test_directory()?;
    let (path, _) = build_and_open_reader(source, FormatOptions::CSV, &dir, "verify_corrupt")?;
    // Flip the last byte of the file (part of the checksum)
    let mut bytes = fs::read(&path)?;
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    fs::write(&path, &bytes)?;
    let mut reader = CsvIndexReader::open(&path)?;
    let error = reader.verify().expect_err("corrupt checksum must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn lazy_index_readers_reject_corrupted_indexes() -> Result<(), Box<dyn StdError>> {
    let directory = create_private_test_directory()?;
    let source_path = directory.join("corrupt.csv");
    let index_path = directory.join("corrupt.idx");
    fs::write(&source_path, b"a,b\nc,d\n")?;
    drop(CsvIndex::create_path(
        &source_path,
        &index_path,
        IndexOptions::default(),
    )?);

    // Truncation is caught when the header is read.
    let mut bytes = fs::read(&index_path)?;
    let good = bytes.clone();
    bytes.truncate(bytes.len() - 1);
    fs::write(&index_path, &bytes)?;
    assert_eq!(
        CsvIndexReader::open(&index_path)
            .expect_err("a truncated index must be rejected")
            .kind(),
        ErrorKind::InvalidIndex,
    );

    // A flipped position bit survives opening but not verification.
    let mut bytes = good;
    let entry = bytes.len() - TRAILER_BYTES - 16;
    bytes[entry] ^= 0x55;
    fs::write(&index_path, &bytes)?;
    let mut reader = CsvIndexReader::open(&index_path)?;
    assert_eq!(
        reader
            .verify()
            .expect_err("a corrupted index must be rejected")
            .kind(),
        ErrorKind::InvalidIndex,
    );

    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

// ── Version 9 trailer: independent checksums and version 8 compatibility ───

/// Rewrite valid version-9 index bytes as an equivalent version-8 file: the
/// same header (with its version field patched to 8) and entries, but with
/// the two independent trailer checksums version 9 writes replaced by the
/// single combined checksum version 8 expects (one `xxh3_128` hash over the
/// header immediately followed by the entries, matching what version 8's
/// `hash_payload` computed by rereading the whole payload).
fn downgrade_to_v8(mut bytes: Vec<u8>) -> Vec<u8> {
    // 8 magic bytes precede the 4-byte version field.
    bytes[8..12].copy_from_slice(&8u32.to_le_bytes());
    let payload_end = bytes.len() - TRAILER_BYTES;
    let checksum = xxh3_128(&bytes[..payload_end]);
    bytes.truncate(payload_end);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

#[test]
fn v8_format_indexes_still_load_and_verify() -> Result<(), Box<dyn StdError>> {
    let source = b"alpha,1\nbeta,2\ngamma,3\n";
    let bytes = downgrade_to_v8(valid_index_bytes(source)?);
    let dir = create_private_test_directory()?;
    let path = dir.join("v8.idx");
    fs::write(&path, &bytes)?;

    // CsvIndex::load must still accept the legacy single-checksum format and
    // parse records correctly from it.
    let loaded = CsvIndex::load(&path)?;
    let mut parser = loaded.parser_at(source, 1)?;
    let mut line = parser.next_line()?.expect("missing indexed row");
    assert_eq!(line.record()?.get(0), Some(b"beta".as_slice()));

    // CsvIndexReader::open followed by verify() must also still accept it.
    let mut reader = CsvIndexReader::open(&path)?;
    reader.verify()?;

    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn v8_format_indexes_still_detect_corruption() -> Result<(), Box<dyn StdError>> {
    let source = b"alpha,1\nbeta,2\ngamma,3\n";
    let mut bytes = downgrade_to_v8(valid_index_bytes(source)?);
    // Flip a bit inside the first entry; the single legacy checksum must
    // still catch it exactly as it did before version 9 existed.
    bytes[FIXED_HEADER_BYTES] ^= 0x55;
    let dir = create_private_test_directory()?;
    let path = dir.join("v8_corrupt.idx");
    fs::write(&path, &bytes)?;
    let error = CsvIndex::load(&path).expect_err("a corrupted legacy index must be rejected");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn v9_entries_and_header_checksums_are_independently_authenticated() -> Result<(), Box<dyn StdError>>
{
    let source = b"alpha,1\nbeta,2\ngamma,3\n";
    let dir = create_private_test_directory()?;

    // Corrupting a byte inside the entry table is reported through the
    // entries checksum, without disturbing the header checksum.
    let mut entry_bytes = valid_index_bytes(source)?;
    entry_bytes[FIXED_HEADER_BYTES] ^= 0x55;
    let entry_path = dir.join("entry_corrupt.idx");
    fs::write(&entry_path, &entry_bytes)?;
    let mut reader = CsvIndexReader::open(&entry_path)?;
    let error = reader
        .verify()
        .expect_err("a corrupted entry must fail verification");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error.to_string().contains("entries checksum"),
        "unexpected error: {error}"
    );

    // Corrupting a byte inside the header (here, the recorded source
    // length, which neither `new()` nor `verify()` otherwise inspects) is
    // instead reported through the header checksum, proving the two checks
    // are independent of one another.
    let mut header_bytes = valid_index_bytes(source)?;
    header_bytes[12] ^= 0x55;
    let header_path = dir.join("header_corrupt.idx");
    fs::write(&header_path, &header_bytes)?;
    let mut reader = CsvIndexReader::open(&header_path)?;
    let error = reader
        .verify()
        .expect_err("a corrupted header must fail verification");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error.to_string().contains("header checksum"),
        "unexpected error: {error}"
    );

    fs::remove_file(&entry_path)?;
    fs::remove_file(&header_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn v9_trailer_holds_two_independent_checksums() -> Result<(), Box<dyn StdError>> {
    let source = b"alpha,1\nbeta,2\n";
    let bytes = valid_index_bytes(source)?;
    let entries_checksum_at = bytes.len() - TRAILER_BYTES;
    let header_checksum_at = bytes.len() - CHECKSUM_BYTES;

    let expected_entries_checksum = xxh3_128(&bytes[ENTRY_OFFSET..entries_checksum_at]);
    let expected_header_checksum = xxh3_128(&bytes[..FIXED_HEADER_BYTES]);

    assert_eq!(
        &bytes[entries_checksum_at..header_checksum_at],
        expected_entries_checksum.to_le_bytes(),
    );
    assert_eq!(
        &bytes[header_checksum_at..],
        expected_header_checksum.to_le_bytes(),
    );
    Ok(())
}

// Each test corrupts one byte in the 93-byte header of a valid in-memory
// index and feeds it to `CsvIndexReader::new`, which decodes the header, the
// format, and the dialect. The format section occupies header bytes 36..53:
//   byte 36 : delimiter
//   byte 37 : quote
//   bytes 38-39 : record_ending type + payload
//   bytes 40-41 : escape type + payload
//   bytes 42-43 : comment type + payload
//   byte 44 : Whitespace trim bits (valid range 0..=7)
//   byte 45 : blank_records (0 or 1)
//   byte 46 : read_bom (0, 1, or 2)
//   byte 47 : write_bom (0 or 1)
//   bytes 48-49 : syntax type + flags
//   byte 50 : nulls (0, 1, or 2)
//   byte 51 : quoting (0..=4)
//   byte 52 : skip_initial_space (0 or 1)

/// Save an index, corrupt one byte in the format section of the header, and
/// check that opening it produces `ErrorKind::InvalidIndex`.
fn corrupt_format_byte(
    source: &[u8],
    format_offset: usize,
    bad_value: u8,
) -> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(source, options)?;
    let path = dir.join("corrupt.idx");
    index.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // Fixed header: 8 magic + 4 version + 8 source_len + 16 hash = 36 bytes before format
    bytes[36 + format_offset] = bad_value;
    // CsvIndexReader::new checks header + length but not full checksum,
    // so the corruption is caught during header decode.
    let error = CsvIndexReader::new(Cursor::new(bytes)).expect_err("corrupt format byte must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn corrupt_record_ending_type_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // record_ending type is at format offset 2
    corrupt_format_byte(b"a,b\n", 2, 0xFF)
}

#[test]
fn corrupt_escape_type_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // escape type is at format offset 4
    corrupt_format_byte(b"a,b\n", 4, 0xFF)
}

#[test]
fn corrupt_comment_type_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // comment type is at format offset 6
    corrupt_format_byte(b"a,b\n", 6, 0xFF)
}

#[test]
fn corrupt_trim_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // trim bits at format offset 8 — use an invalid bitflag combination
    corrupt_format_byte(b"a,b\n", 8, 0xFF)
}

#[test]
fn corrupt_blank_records_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // blank_records is at format offset 9
    corrupt_format_byte(b"a,b\n", 9, 0xFF)
}

#[test]
fn corrupt_read_bom_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // read_bom is at format offset 10
    corrupt_format_byte(b"a,b\n", 10, 0xFF)
}

#[test]
fn corrupt_write_bom_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // write_bom is at format offset 11
    corrupt_format_byte(b"a,b\n", 11, 0xFF)
}

#[test]
fn corrupt_syntax_type_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // syntax type is at format offset 12
    corrupt_format_byte(b"a,b\n", 12, 0xFF)
}

#[test]
fn corrupt_syntax_flags_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // syntax flags for Compatible mode: set type=1, flags=0xFF (invalid Recovery bits)
    let dir = create_private_test_directory()?;
    // Build with Syntax::Compatible(Recovery::PERMISSIVE) so type byte is 1
    let format = FormatOptions::new().syntax(Syntax::Compatible(Recovery::PERMISSIVE));
    let options = IndexOptions {
        format,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(b"a,b\n", options)?;
    let path = dir.join("corrupt_flags.idx");
    index.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // syntax type is at format offset 12
    // syntax flags are at format offset 13 — set to invalid bits
    bytes[36 + 13] = 0xFF;
    let error =
        CsvIndexReader::new(Cursor::new(bytes)).expect_err("invalid syntax flags must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn corrupt_nulls_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // nulls is at format offset 14
    corrupt_format_byte(b"a,b\n", 14, 0xFF)
}

#[test]
fn corrupt_quoting_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // quoting is at format offset 15
    corrupt_format_byte(b"a,b\n", 15, 0xFF)
}

#[test]
fn corrupt_skip_initial_space_byte_is_rejected() -> Result<(), Box<dyn StdError>> {
    // skip_initial_space is at format offset 16
    corrupt_format_byte(b"a,b\n", 16, 0xFF)
}

#[test]
fn corrupt_dialect_delimiter_equals_quote_is_rejected() -> Result<(), Box<dyn StdError>> {
    // Set the delimiter byte (format offset 0) to the same value as the quote
    // byte (format offset 1 = b'"' for CSV), making the dialect invalid.
    // decode_dialect decodes the bytes successfully but Dialect::new rejects them.
    let dir = create_private_test_directory()?;
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(b"a,b\n", options)?;
    let path = dir.join("bad_dialect.idx");
    index.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // format offset 0 = delimiter; set it to b'"' (== quote)
    bytes[36] = b'"';
    let error = CsvIndexReader::new(Cursor::new(bytes)).expect_err("delimiter==quote must fail");
    assert_eq!(error.kind(), ErrorKind::Configuration);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn corrupt_dialect_comment_conflicts_with_delimiter_is_rejected() -> Result<(), Box<dyn StdError>> {
    // Build an index with a plain CSV dialect (no comment byte).
    // Corrupt the format bytes to indicate a comment byte equal to the delimiter.
    // decode_dialect calls with_comment which detects the conflict.
    let dir = create_private_test_directory()?;
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(b"a,b\n", options)?;
    let path = dir.join("bad_comment.idx");
    index.save(&path)?;
    let mut bytes = fs::read(&path)?;
    // Format byte layout: [0]=delimiter(b','), [1]=quote, [2..4]=record_ending,
    // [4..6]=escape, [6]=comment_type(0=None), [7]=comment_byte.
    // Set comment_type=1 (Some) and comment_byte=b',' (conflicts with delimiter).
    bytes[36 + 6] = 1;
    bytes[36 + 7] = b',';
    let error = CsvIndexReader::new(Cursor::new(bytes))
        .expect_err("comment conflicting with delimiter must fail");
    assert_eq!(error.kind(), ErrorKind::Configuration);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

/// Corrupt one byte of `bytes[pos]` to `val`, then try `CsvIndexReader::new`.
/// Panics if `new()` unexpectedly succeeds.
fn assert_new_rejects_corrupt_byte(bytes: &[u8], pos: usize, val: u8, label: &str, detail: &str) {
    let mut corrupt = bytes.to_vec();
    corrupt[pos] = val;
    let error = CsvIndexReader::new(Cursor::new(corrupt)).expect_err(&format!(
        "corrupt {label} (byte {pos}=0x{val:02X}) must be rejected"
    ));
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error.to_string().contains(detail),
        "{label} rejection must contain {detail:?}, got {error}"
    );
}

#[test]
fn decode_header_rejects_unknown_record_ending() -> Result<(), Box<dyn StdError>> {
    // RecordEnding type byte = 3 → "unknown record_ending encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 2,
        3,
        "record_ending",
        "unknown record_ending encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_escape() -> Result<(), Box<dyn StdError>> {
    // Escape type byte = 4 → "unknown escape encoding". 3 is `Unquoted`.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 4,
        4,
        "escape",
        "unknown escape encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_comment() -> Result<(), Box<dyn StdError>> {
    // Comment type byte = 2 → "unknown comment encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 6,
        2,
        "comment",
        "unknown comment encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_trim_bits() -> Result<(), Box<dyn StdError>> {
    // Whitespace uses bits 0–2 only; 0x08 sets bit 3, which from_bits rejects
    // as "unknown trim encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 8,
        0x08,
        "trim",
        "unknown trim encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_blank_records() -> Result<(), Box<dyn StdError>> {
    // blank_records has only two valid values (0 and 1); 2 → "unknown
    // blank-record encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 9,
        2,
        "blank_records",
        "unknown blank-record encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_read_bom() -> Result<(), Box<dyn StdError>> {
    // read_bom has three valid values (0–2); 3 → "unknown read BOM encoding"
    //.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 10,
        3,
        "read_bom",
        "unknown read BOM encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_write_bom() -> Result<(), Box<dyn StdError>> {
    // write_bom has two valid values (0 and 1); 2 → "unknown write BOM encoding"
    //.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 11,
        2,
        "write_bom",
        "unknown write BOM encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_syntax_type() -> Result<(), Box<dyn StdError>> {
    // syntax type has two valid values (0=Strict, 1=Compatible); 2 →
    // "unknown syntax mode encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 12,
        2,
        "syntax type",
        "unknown syntax mode encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_recovery_rules() -> Result<(), Box<dyn StdError>> {
    // Set syntax type = 1 (Compatible) and flags byte = 0x10 (bit 4 set, which
    // is beyond Recovery::ALL_FLAGS = 0x0F) → "unknown recovery rule encoding"
    //.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    let mut corrupt = bytes;
    corrupt[FORMAT_OFFSET + 12] = 1; // syntax = Compatible
    corrupt[FORMAT_OFFSET + 13] = 0x10; // flags with an unknown bit
    let error =
        CsvIndexReader::new(Cursor::new(corrupt)).expect_err("unknown recovery flags must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    assert!(
        error.to_string().contains("unknown recovery rule encoding"),
        "{error}"
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_nulls() -> Result<(), Box<dyn StdError>> {
    // nulls has three valid values (0–2); 3 → "unknown NULL policy encoding"
    //.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 14,
        3,
        "nulls",
        "unknown NULL policy encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_quoting() -> Result<(), Box<dyn StdError>> {
    // quoting has five valid values (0–4); 5 → "unknown quote policy encoding"
    //.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 15,
        5,
        "quoting",
        "unknown quote policy encoding",
    );
    Ok(())
}

#[test]
fn decode_header_rejects_unknown_skip_initial_space() -> Result<(), Box<dyn StdError>> {
    // skip_initial_space has two valid values (0 and 1); 2 → "unknown
    // initial-space encoding".
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    assert_new_rejects_corrupt_byte(
        &bytes,
        FORMAT_OFFSET + 16,
        2,
        "skip_initial_space",
        "unknown initial-space encoding",
    );
    Ok(())
}

// ── CsvIndexReader: metadata and lazy access ────────────────────────────────

#[test]
fn csv_index_reader_open_and_metadata_accessors() -> Result<(), Box<dyn StdError>> {
    let source = b"col1,col2\nval1,val2\n";
    let dir = create_private_test_directory()?;
    let (path, reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "meta")?;
    assert_eq!(reader.len(), 2);
    assert!(!reader.is_empty());
    assert_eq!(reader.format(), FormatOptions::CSV);
    assert_eq!(reader.limits(), Limits::DEFAULT);
    assert_eq!(reader.source_len(), source.len() as u64);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_is_empty_for_empty_source() -> Result<(), Box<dyn StdError>> {
    let source = b"";
    let dir = create_private_test_directory()?;
    let (path, reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "empty")?;
    assert!(reader.is_empty());
    assert_eq!(reader.len(), 0);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_record_offset_and_line_return_none_out_of_range()
-> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\n";
    let dir = create_private_test_directory()?;
    let (path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "oor")?;
    // Within range
    assert!(reader.record_offset(0)?.is_some());
    assert!(reader.record_line(0)?.is_some());
    // Out of range
    assert_eq!(reader.record_offset(99)?, None);
    assert_eq!(reader.record_line(99)?, None);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_record_line_returns_correct_value() -> Result<(), Box<dyn StdError>> {
    // record_line() reports the 1-based physical line each record starts on.
    let bytes = index_bytes_in_memory(b"first\nsecond\nthird\n")?;
    let mut reader = CsvIndexReader::new(Cursor::new(bytes))?;
    // Record 0 starts on physical line 1.
    let line = reader.record_line(0)?.expect("record 0 must exist");
    assert_eq!(line, 1, "first record starts on line 1");
    // Record 1 starts on physical line 2.
    let line = reader.record_line(1)?.expect("record 1 must exist");
    assert_eq!(line, 2, "second record starts on line 2");
    // Out-of-range returns None.
    assert!(reader.record_line(99)?.is_none());
    Ok(())
}

#[test]
fn csv_index_reader_location_rejects_out_of_range_record() -> Result<(), Box<dyn StdError>> {
    let source = b"x,1\ny,2\n";
    let dir = create_private_test_directory()?;
    let (path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "loc")?;
    let error = reader
        .location(999)
        .expect_err("out-of-range record must fail");
    assert_eq!(error.kind(), ErrorKind::RecordOutOfRange { record: 999 });
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_validate_reader_accepts_matching_source() -> Result<(), Box<dyn StdError>> {
    let source = b"first,second\nalpha,beta\n";
    let dir = create_private_test_directory()?;
    let (path, reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "validate")?;
    reader.validate_reader(source.as_slice())?;
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_validate_reader_rejects_wrong_source() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n1,2\n";
    let dir = create_private_test_directory()?;
    let (path, reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "validate_wrong")?;
    let error = reader
        .validate_reader(b"a,b\n1,9\n".as_slice())
        .expect_err("wrong source must fail");
    assert_eq!(error.kind(), ErrorKind::SourceMismatch);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_verify_passes_for_valid_index() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\nc,3\n";
    let dir = create_private_test_directory()?;
    let (path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "verify")?;
    reader.verify()?;
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_parser_at_reader_seeks_via_cursor() -> Result<(), Box<dyn StdError>> {
    let source = b"one,1\ntwo,2\nthree,3\n";
    let dir = create_private_test_directory()?;
    let (path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "par")?;
    let cursor = Cursor::new(source.to_vec());
    let mut parser = reader.parser_at_reader(cursor, 2)?;
    let mut line = parser.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"three".as_slice()));
    assert_eq!(record.index(), 2);
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_parser_at_path_seeks_via_file() -> Result<(), Box<dyn StdError>> {
    let source = b"row0,0\nrow1,1\nrow2,2\n";
    let dir = create_private_test_directory()?;
    let source_path = dir.join("src.csv");
    fs::write(&source_path, source)?;
    let (index_path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "pap")?;
    let mut parser = reader.parser_at_path(&source_path, 1)?;
    let mut line = parser.next_line()?.expect("record present");
    let record = line.record()?;
    assert_eq!(record.get(0), Some(b"row1".as_slice()));
    assert_eq!(record.index(), 1);
    fs::remove_file(&source_path)?;
    fs::remove_file(&index_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_reader_parser_at_reader_location_error() -> Result<(), Box<dyn StdError>> {
    // parser_at_reader calls self.location(record) first; an out-of-range
    // record makes location() return Err, propagating to the caller
    //.
    let bytes = index_bytes_in_memory(b"a,b\nc,d\n")?;
    let mut reader = CsvIndexReader::new(Cursor::new(bytes))?;
    let source = b"a,b\nc,d\n";
    let err = reader
        .parser_at_reader(Cursor::new(source.to_vec()), 99)
        .expect_err("out-of-range record must fail");
    assert!(matches!(err.kind(), ErrorKind::RecordOutOfRange { .. }));
    Ok(())
}

#[test]
fn csv_index_reader_into_inner_recovers_underlying_reader() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n";
    let dir = create_private_test_directory()?;
    let (path, reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "inner")?;
    // Consuming into_inner gives back the File; verify we can still read it
    let mut file = reader.into_inner();
    file.seek(io::SeekFrom::Start(0))
        .expect("seek on recovered file");
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).expect("read recovered file");
    assert!(!buf.is_empty());
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn lazy_index_readers_seek_streaming_sources() -> Result<(), Box<dyn StdError>> {
    let directory = create_private_test_directory()?;
    let source_path = directory.join("lazy.csv");
    let index_path = directory.join("lazy.idx");
    let source = b"a,1\n\"b\nb\",2\nc,3\nd,4\n";
    fs::write(&source_path, source)?;

    // Build without ever materialising the location table, then reopen it.
    let created = CsvIndex::create_path(&source_path, &index_path, IndexOptions::default())?;
    assert_eq!(created.len(), 4);
    drop(created);

    let mut reader = CsvIndexReader::open(&index_path)?;
    reader.verify()?;
    reader.validate_reader(fs::File::open(&source_path)?)?;
    assert_eq!(reader.location(1)?.byte, 4);
    assert_eq!(reader.location(1)?.line, 2);

    let mut parser = reader.parser_at_path(&source_path, 1)?;
    let (first, second) = {
        let mut line = parser.next_line()?.expect("missing indexed record");
        let row = line.record()?;
        (
            row.get(0).map(<[u8]>::to_vec),
            row.get(1).map(<[u8]>::to_vec),
        )
    };
    assert_eq!(first.as_deref(), Some(b"b\nb".as_slice()));
    assert_eq!(second.as_deref(), Some(b"2".as_slice()));
    assert_eq!(parser.location().line, 4);

    // The eagerly loaded index agrees with the lazily read one.
    let loaded = CsvIndex::load(&index_path)?;
    assert_eq!(loaded.len(), 4);
    for record in 0..4 {
        assert_eq!(reader.record_offset(record)?, loaded.record_offset(record));
        assert_eq!(reader.record_line(record)?, loaded.record_line(record));
    }

    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

// ── Path-based sources and streaming ────────────────────────────────────────

#[test]
fn build_path_streams_and_binds_the_complete_source() -> Result<(), Box<dyn StdError>> {
    let directory = create_private_test_directory()?;
    let source_path = directory.join("source.csv");
    let index_path = directory.join("source.idx");
    let source = b"a,b\n\"c\nc\",d\n";
    fs::write(&source_path, source)?;
    let index = CsvIndex::build_path(&source_path, &index_path, IndexOptions::default())?;
    index.validate_source(source)?;
    assert_eq!(CsvIndex::load(&index_path)?, index);
    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn indexes_seek_streaming_sources_without_holding_them() -> Result<(), Box<dyn StdError>> {
    let directory = create_private_test_directory()?;
    let source_path = directory.join("streamed.csv");
    let index_path = directory.join("streamed.idx");
    let source = b"a,1\n\"b\nb\",2\nc,3\nd,4\n";
    fs::write(&source_path, source)?;
    let index = CsvIndex::build_path(&source_path, &index_path, IndexOptions::default())?;

    // A seekable source is validated by length and then seeked in place.
    let mut reader = index.parser_at_path(&source_path, 2)?;
    assert_eq!(reader.location().byte, 12);
    assert_eq!(reader.location().line, 4);
    assert_eq!(reader.location().record, 2);
    let (field, range, position) = {
        let mut line = reader.next_line()?.expect("missing indexed record");
        let row = line.record()?;
        (
            row.get(0).map(<[u8]>::to_vec),
            row.byte_range(),
            row.index(),
        )
    };
    assert_eq!(field.as_deref(), Some(b"c".as_slice()));
    assert_eq!(range, 12..16);
    assert_eq!(position, 2);

    // Reading on keeps counting against the whole file.
    let next = {
        let mut line = reader.next_line()?.expect("missing following record");
        let row = line.record()?;
        (row.get(0).map(<[u8]>::to_vec), row.index())
    };
    assert_eq!(next.0.as_deref(), Some(b"d".as_slice()));
    assert_eq!(next.1, 3);

    // The full identity check streams the source rather than buffering it.
    index.validate_reader(fs::File::open(&source_path)?)?;
    assert_eq!(
        index
            .validate_reader(b"a,1\n\"b\nb\",2\nc,3\nd,5\n".as_slice())
            .expect_err("a different source must be rejected")
            .kind(),
        ErrorKind::SourceMismatch,
    );

    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn csv_index_reader_open_rejects_nonexistent_path() {
    let path = common::temp_file("index-reader-absent");
    let error = CsvIndexReader::open(path.path()).expect_err("nonexistent path must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_build_path_rejects_nonexistent_source() {
    let directory = common::temp_dir("index-build-absent").expect("temporary directory");
    let error = CsvIndex::build_path(
        directory.path().join("source.csv"),
        directory.path().join("out.idx"),
        IndexOptions::default(),
    )
    .expect_err("nonexistent source must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_path_rejects_nonexistent_source() {
    let directory = common::temp_dir("index-create-absent").expect("temporary directory");
    let error = CsvIndex::create_path(
        directory.path().join("source.csv"),
        directory.path().join("out.idx"),
        IndexOptions::default(),
    )
    .expect_err("nonexistent source must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_path_rejects_bad_index_path() -> Result<(), Box<dyn StdError>> {
    let dir = create_private_test_directory()?;
    let source_path = dir.join("s.csv");
    fs::write(&source_path, b"a,b\n")?;
    let index_path = dir.join("missing").join("out.idx");
    let error = CsvIndex::create_path(&source_path, &index_path, IndexOptions::default())
        .expect_err("bad index path must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    fs::remove_file(&source_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_parser_at_path_rejects_nonexistent_source_file() -> Result<(), Box<dyn StdError>> {
    let source = b"x,1\ny,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let path = common::temp_file("index-parser-absent");
    let error = index
        .parser_at_path(path.path(), 0)
        .expect_err("nonexistent source must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_parser_at_path_rejects_nonexistent_source_file() -> Result<(), Box<dyn StdError>>
{
    let source = b"p,1\nq,2\n";
    let dir = create_private_test_directory()?;
    let (path, mut reader) = build_and_open_reader(source, FormatOptions::CSV, &dir, "pap_ne")?;
    let missing = dir.join("missing.csv");
    let error = reader
        .parser_at_path(&missing, 0)
        .expect_err("nonexistent source must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    fs::remove_file(&path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn csv_index_save_rejects_bad_directory() -> Result<(), Box<dyn StdError>> {
    let source = b"a,b\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let directory = common::temp_dir("index-save-missing")?;
    let error = index
        .save(directory.path().join("missing").join("out.idx"))
        .expect_err("bad directory must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_load_rejects_nonexistent_file() {
    let path = common::temp_file("index-load-absent");
    let error = CsvIndex::load(path.path()).expect_err("nonexistent file must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn build_path_fails_when_index_path_is_unwritable() -> Result<(), Box<dyn StdError>> {
    // build_path() calls save(index_path); if the path is bad, save
    // propagates an I/O error back through build_path.
    let dir = create_private_test_directory()?;
    let source_path = dir.join("src.csv");
    fs::write(&source_path, b"a,b\nc,d\n")?;
    let index_path = dir.join("missing").join("index.idx");
    let err = CsvIndex::build_path(&source_path, &index_path, IndexOptions::default())
        .expect_err("bad index path must fail");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    fs::remove_file(&source_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn save_to_dev_full_fails_on_header_write() -> Result<(), Box<dyn StdError>> {
    // save() opens the path and writes the header first; /dev/full always returns
    // ENOSPC, so the first write_all fails.
    let index = CsvIndex::build(b"a,b\nc,d\n", IndexOptions::default())?;
    let err = index
        .save("/dev/full")
        .expect_err("write to /dev/full must fail");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn build_path_fails_when_index_save_write_fails() -> Result<(), Box<dyn StdError>> {
    // build_path() calls index.save(index_path); /dev/full triggers
    // the write-error path inside save(), which propagates back.
    let dir = create_private_test_directory()?;
    let source_path = dir.join("src.csv");
    fs::write(&source_path, b"a,b\n")?;
    let err = CsvIndex::build_path(&source_path, "/dev/full", IndexOptions::default())
        .expect_err("write to /dev/full must fail");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    fs::remove_file(&source_path)?;
    fs::remove_dir(&dir)?;
    Ok(())
}

// ── I/O failures while reading and writing indexes ──────────────────────────

#[test]
fn csv_index_reader_new_fails_on_seek_error() {
    // FailingSeekRW: seek #1 (SeekFrom::Start(0)) fails immediately.
    let error = CsvIndexReader::new(FailingSink::new().fail_all_seeks(io::ErrorKind::Other))
        .expect_err("seek failure must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_reader_new_fails_on_second_seek_error() -> Result<(), Box<dyn StdError>> {
    // NthSeekRW(fail_at=2): seek #1 succeeds (header read follows), seek #2
    // (SeekFrom::End(0) to measure file length) fails.
    let bytes = valid_index_bytes(b"a,b\n")?;
    let rw = FailingSink::with_bytes(bytes).fail_on_seek(2, io::ErrorKind::Other);
    let error = CsvIndexReader::new(rw).expect_err("second seek must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_new_fails_on_non_eof_read_error() -> Result<(), Box<dyn StdError>> {
    // FailingReadRS: seek succeeds, then read_index_exact gets Other (not
    // UnexpectedEof) → the non-EOF branch of read_index_exact fires.
    let bytes = valid_index_bytes(b"a,b\n")?;
    let rs = FailingReader::new(bytes).fail_all_reads(io::ErrorKind::Other);
    let error = CsvIndexReader::new(rs).expect_err("read failure must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_entry_seek_error() -> Result<(), Box<dyn StdError>> {
    // entry() calls index_seek before reading the entry bytes.  new() uses
    // exactly 2 seeks (Start(0) and End(0)), so fail_at=3 makes new() succeed
    // and the very first entry seek fail.
    let bytes = index_bytes_in_memory(b"a,b\nc,d\n")?;
    let rw = FailingSink::with_bytes(bytes).fail_on_seek(3, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rw).expect("new must succeed");
    let err = reader
        .record_offset(0)
        .expect_err("entry seek failure must propagate");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_entry_read_error() -> Result<(), Box<dyn StdError>> {
    // entry() calls read_index_exact to load the 16-byte encoded entry.
    // new() makes exactly 1 read call (the 93-byte header), so fail_at=2
    // lets new() succeed and makes entry()'s read fail.
    let bytes = index_bytes_in_memory(b"a,b\nc,d\n")?;
    let rs = FailingReader::new(bytes).fail_on_read(2, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rs).expect("new must succeed");
    let err = reader
        .record_offset(0)
        .expect_err("entry read failure must propagate");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_location_via_entry_seek_error() -> Result<(), Box<dyn StdError>> {
    // location() calls entry(), which seeks first, so a failing seek surfaces
    // as an error from location() itself.
    let bytes = index_bytes_in_memory(b"a,b\nc,d\n")?;
    let rw = FailingSink::with_bytes(bytes).fail_on_seek(3, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rw).expect("new must succeed");
    let err = reader
        .location(0)
        .expect_err("seek failure in entry must propagate");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_record_line_propagates_entry_error() -> Result<(), Box<dyn StdError>> {
    // record_line() propagates the error from entry(). new() uses 2 seeks;
    // fail_at=3 makes the entry seek fail.
    let bytes = index_bytes_in_memory(b"a,b\nc,d\n")?;
    let rw = FailingSink::with_bytes(bytes).fail_on_seek(3, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rw).expect("new must succeed");
    let err = reader
        .record_line(0)
        .expect_err("entry seek failure must propagate through record_line");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_verify_seek_error() -> Result<(), Box<dyn StdError>> {
    // verify() calls index_seek(Start(0)) as its first I/O operation.
    // new() uses exactly 2 seeks, so fail_at=3 lets new() succeed and makes
    // verify()'s seek fail.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    let rw = FailingSink::with_bytes(bytes).fail_on_seek(3, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rw).expect("new must succeed");
    let err = reader
        .verify()
        .expect_err("verify seek failure must propagate");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn csv_index_reader_verify_checksum_read_error() -> Result<(), Box<dyn StdError>> {
    // For a 1-record source, version 9's verify() reads:
    //   read call 1 = new()'s header read
    //   read call 2 = verify()'s header reread
    //   read call 3 = hash_entries's single-shot read of the 16-byte entry
    //                 table
    //   read call 4 = read_index_exact for the stored entries checksum
    //   read call 5 = read_index_exact for the stored header checksum
    // fail_at=4 lets the first three reads succeed and makes the stored
    // entries-checksum read fail.
    let bytes = index_bytes_in_memory(b"a,b\n")?;
    let rs = FailingReader::new(bytes).fail_on_read(4, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rs).expect("new must succeed");
    let err = reader
        .verify()
        .expect_err("checksum read failure must propagate");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn verify_retries_on_interrupted_read() -> Result<(), Box<dyn StdError>> {
    // InterruptOnNthReadRS(interrupt_at=2): new() read call #1 succeeds;
    // verify()'s header-reread call #2 returns Interrupted → `read_exact`
    // retries internally → subsequent reads succeed → verify() returns Ok.
    let bytes = valid_index_bytes(b"a,b\n")?;
    let rs = FailingReader::new(bytes).interrupt_on_read(2);
    let mut reader = CsvIndexReader::new(rs).expect("new must succeed");
    reader
        .verify()
        .expect("verify must succeed after Interrupted retry");
    Ok(())
}

#[test]
fn verify_fails_on_non_interrupted_read_error() -> Result<(), Box<dyn StdError>> {
    // FailOnNthReadRS(fail_at=2): new() read call #1 succeeds; verify()'s
    // header-reread call #2 returns Other → propagates as an I/O error.
    let bytes = valid_index_bytes(b"a,b\n")?;
    let rs = FailingReader::new(bytes).fail_on_read(2, io::ErrorKind::Other);
    let mut reader = CsvIndexReader::new(rs).expect("new must succeed");
    let error = reader.verify().expect_err("read failure must propagate");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn verify_fails_on_early_eof_in_hash_payload() -> Result<(), Box<dyn StdError>> {
    // EarlyEofOnNthReadRS(eof_at=2): new() read call #1 succeeds; verify()'s
    // header-reread call #2 returns Ok(0) (early EOF) → `read_exact` maps the
    // short read to "index is truncated".
    let bytes = valid_index_bytes(b"a,b\n")?;
    let rs = FailingReader::new(bytes).early_eof_on_read(2);
    let mut reader = CsvIndexReader::new(rs).expect("new must succeed");
    let error = reader.verify().expect_err("early EOF must fail");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
    Ok(())
}

#[test]
fn csv_index_create_fails_on_initial_seek() {
    // FailingSeekRW: the very first index_seek in create() fails.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_all_seeks(io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("initial seek failure must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_on_second_seek() {
    // NthSeekRW(fail_at=2): seek #1 (before BufWriter) succeeds; all writes
    // succeed; seek #2 (rewrite header position) fails.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_on_seek(2, io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("second seek must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_on_third_seek() {
    // NthSeekRW(fail_at=3): seeks #1-2 succeed; seek #3 (to the end of the
    // file, positioning to write the two trailer checksums) fails.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_on_seek(3, io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("third seek must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_when_into_inner_flush_fails() {
    // BudgetWriteRW(budget=0): BufWriter accumulates 117 bytes for a 2-record
    // source then into_inner() tries to flush them; the very first write to the
    // underlying writer fails → IntoInnerError is propagated.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_after_bytes(0, io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("budget=0 must fail at into_inner");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_on_header_overwrite() {
    // budget=125: exactly covers the BufWriter flush (93+32=125 bytes for a
    // 2-record source), leaving budget=0 when the header is rewritten at pos 0.
    let source = b"a,b\nc,d\n"; // 2 records → BufWriter flush = 125 bytes
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_after_bytes(125, io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("budget=125 must fail on header overwrite");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_on_checksum_write() {
    // budget=218 (125 flush + 93 header): all preceding writes succeed; the
    // 16-byte checksum write then exhausts the budget.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_after_bytes(218, io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("budget=218 must fail on checksum write");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn csv_index_create_fails_on_flush() {
    // fail_flush=true: every write succeeds but the final flush() call fails.
    let source = b"a,b\nc,d\n";
    let error = CsvIndex::create(
        source.as_slice(),
        FailingSink::new().fail_flush(io::ErrorKind::Other),
        IndexOptions::default(),
    )
    .expect_err("fail_flush must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
}

#[test]
fn create_fails_midloop_when_bufwriter_flushes_to_failed_writer() {
    // BufWriter(capacity 8192) holds all small writes in its buffer.
    // With 600 records the buffer fills at approximately record 507's physical
    // write, at which point BufWriter flushes to the underlying writer.
    // budget=0 → flush immediately returns Err, which write_all propagates
    //.
    let source: Vec<u8> = b"x\n".repeat(600);
    let writer = FailingSink::new().fail_after_bytes(0, io::ErrorKind::Other);
    let err = CsvIndex::create(Cursor::new(source), writer, IndexOptions::default())
        .expect_err("exhausted budget must fail");
    assert!(matches!(err.kind(), ErrorKind::Io(_)));
}

#[test]
fn parser_at_reader_fails_when_first_source_seek_fails() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    // NthSeekRW(fail_at=1) as source: the SeekFrom::End(0) call in
    // streaming_parser_at fails immediately.
    let bad_source = FailingSink::with_bytes(source.to_vec()).fail_on_seek(1, io::ErrorKind::Other);
    let error = index
        .parser_at_reader(bad_source, 0)
        .expect_err("first source seek failure must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn parser_at_reader_fails_when_second_source_seek_fails() -> Result<(), Box<dyn StdError>> {
    let source = b"a,1\nb,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    // NthSeekRW(fail_at=2, data=source): seek #1 (End) succeeds and returns
    // the correct length so the SourceMismatch check passes; seek #2 (Start)
    // then fails.
    let bad_source = FailingSink::with_bytes(source.to_vec()).fail_on_seek(2, io::ErrorKind::Other);
    let error = index
        .parser_at_reader(bad_source, 0)
        .expect_err("second source seek failure must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn validate_reader_propagates_io_error() -> Result<(), Box<dyn StdError>> {
    let source = b"col,row\n1,2\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    // FailingReader::new(Vec::new()).fail_all_reads(io::ErrorKind::Other): io::copy inside validate_identity fails → map_err fires.
    // HashingReader::read propagates the error.
    let error = index
        .validate_reader(FailingReader::new(Vec::new()).fail_all_reads(io::ErrorKind::Other))
        .expect_err("io error in source must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

#[test]
fn validate_reader_with_liar_read_fails() -> Result<(), Box<dyn StdError>> {
    // FailingReader::new(Vec::new()).overrun()::read returns Ok(buf.len()+1), triggering the sanity check in
    // HashingReader that detects a Read impl violating the contract.
    let source = b"a,b\n";
    let index = CsvIndex::build(source, IndexOptions::default())?;
    let error = index
        .validate_reader(FailingReader::new(Vec::new()).overrun())
        .expect_err("liar reader must fail");
    assert!(matches!(error.kind(), ErrorKind::Io(_)));
    Ok(())
}

// ── Limits and invalid dialects ─────────────────────────────────────────────

#[test]
fn build_fails_when_record_exceeds_limit() {
    // next_line()? or record()? propagates a limit error back through build().
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(4, 8, 8),
    };
    let err = CsvIndex::build(b"ab,cd\n", options).expect_err("limit violation must fail");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
}

#[test]
fn build_path_fails_when_record_exceeds_limit() -> Result<(), Box<dyn StdError>> {
    // A limit violation in the streaming parsing loop aborts build_path().
    let dir = create_private_test_directory()?;
    let source_path = dir.join("src.csv");
    let index_path = dir.join("idx.idx");
    fs::write(&source_path, b"ab,cd\n")?;
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(4, 8, 8),
    };
    let err = CsvIndex::build_path(&source_path, &index_path, options)
        .expect_err("limit violation must fail");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
    let _ = fs::remove_file(&source_path);
    let _ = fs::remove_file(&index_path);
    fs::remove_dir(&dir)?;
    Ok(())
}

#[test]
fn create_fails_when_record_exceeds_limit() {
    // record()? propagates a limit error out of create().
    let options = IndexOptions {
        format: FormatOptions::CSV,
        limits: Limits::new(4, 8, 8),
    };
    let err = CsvIndex::create(
        Cursor::new(b"ab,cd\n".to_vec()),
        Cursor::new(Vec::<u8>::new()),
        options,
    )
    .expect_err("limit violation must fail");
    assert_eq!(err.kind(), ErrorKind::RecordTooLarge { limit: 4 });
}

#[test]
fn build_rejects_a_dialect_with_equal_delimiter_and_quote() {
    // `CsvIndex::build` hands the format straight to `SliceParser::with_options`,
    // which validates the dialect before parsing a single byte.
    let options = IndexOptions {
        format: FormatOptions::CSV.delimiter(b'"'),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::build(b"a,b\n", options)
        .expect_err("an equal delimiter and quote must be rejected");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

#[test]
fn build_path_rejects_a_dialect_with_equal_delimiter_and_quote() -> Result<(), Box<dyn StdError>> {
    // Same validation, reached through the streaming file-backed constructor.
    let dir = create_private_test_directory()?;
    let source_path = dir.join("source.csv");
    fs::write(&source_path, b"a,b\n")?;
    let options = IndexOptions {
        format: FormatOptions::CSV.delimiter(b'"'),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::build_path(&source_path, dir.join("index.idx"), options)
        .expect_err("an equal delimiter and quote must be rejected");
    assert_eq!(error.kind(), ErrorKind::Configuration);
    fs::remove_dir_all(&dir)?;
    Ok(())
}

#[test]
fn create_rejects_a_dialect_with_equal_delimiter_and_quote() {
    // Same validation again, reached through the constant-memory constructor.
    let options = IndexOptions {
        format: FormatOptions::CSV.delimiter(b'"'),
        limits: Limits::DEFAULT,
    };
    let error = CsvIndex::create(
        Cursor::new(b"a,b\n".to_vec()),
        Cursor::new(Vec::<u8>::new()),
        options,
    )
    .expect_err("an equal delimiter and quote must be rejected");
    assert_eq!(error.kind(), ErrorKind::Configuration);
}

// ── Indexed reads compared against a linear scan ────────────────────────────

/// For every record in the indexed source, compare the field values returned
/// by a seeked parser against those returned by a plain linear scan.
fn differential_index_vs_linear(
    source: &[u8],
    format: FormatOptions,
) -> Result<(), Box<dyn StdError>> {
    use coseva::SliceParser;
    use coseva::config::{Headers, ParseOptions};

    let options = IndexOptions {
        format,
        limits: Limits::DEFAULT,
    };
    let index = CsvIndex::build(source, options)?;

    // Collect all records via linear scan
    let mut linear =
        SliceParser::with_options(source, format, ParseOptions::new().headers(Headers::None))?;
    let mut expected: Vec<Vec<Vec<u8>>> = Vec::new();
    while let Some(mut line) = linear.next_line()? {
        let record = line.record()?;
        expected.push(record.iter().map(<[u8]>::to_vec).collect());
    }

    // Verify each record individually via index
    for (n, expected_fields) in expected.iter().enumerate() {
        let mut parser = index.parser_at(source, n)?;
        let mut line = parser.next_line()?.expect("indexed record must exist");
        let record = line.record()?;
        let got: Vec<Vec<u8>> = record.iter().map(<[u8]>::to_vec).collect();
        assert_eq!(
            got, *expected_fields,
            "record {n} via index differs from linear scan"
        );
        assert_eq!(
            record.index(),
            n as u64,
            "record {n} has wrong index counter"
        );
    }
    Ok(())
}

#[test]
fn differential_plain_csv() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(b"a,b,c\n1,2,3\n4,5,6\n7,8,9\n", FormatOptions::CSV)
}

#[test]
fn differential_quoted_fields_with_embedded_newlines() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(
        b"\"line\none\",b\n\"line\ntwo\",d\nplain,row\n",
        FormatOptions::CSV,
    )
}

#[test]
fn differential_rfc4180_crlf() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(
        b"col1,col2\r\nval1,val2\r\nval3,val4\r\n",
        FormatOptions::RFC4180,
    )
}

#[test]
fn differential_backslash_escape() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(b"a\\,b,c\nnext,row\n", FormatOptions::BACKSLASH_CSV)
}

#[test]
fn differential_mysql_dialect() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(b"hello\tworld\nfoo\tbar\n", FormatOptions::MYSQL)
}

#[test]
fn differential_byte_record_ending() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::new()
        .delimiter(b',')
        .record_ending(RecordEnding::Byte(b'|'));
    differential_index_vs_linear(b"a,1|b,2|c,3|", format)
}

#[test]
fn differential_single_record_no_terminator() -> Result<(), Box<dyn StdError>> {
    differential_index_vs_linear(b"solo,field", FormatOptions::CSV)
}

#[test]
fn differential_many_records() -> Result<(), Box<dyn StdError>> {
    let mut source = Vec::new();
    for i in 0..200u32 {
        source.extend_from_slice(format!("row{i},{i}\n").as_bytes());
    }
    differential_index_vs_linear(&source, FormatOptions::CSV)
}

// ── Large sources ───────────────────────────────────────────────────────────

#[test]
fn lazy_index_readers_seek_deep_into_a_large_source() -> Result<(), Box<dyn StdError>> {
    // Small sources fit one read, so buffer refills after a seek only show up
    // on an input that is many windows long.
    let mut source = Vec::new();
    let mut expected = Vec::new();
    for record in 0..50_000u32 {
        expected.push(source.len() as u64);
        if record % 7 == 0 {
            source.extend_from_slice(format!("\"row\n{record}\",{record}\n").as_bytes());
        } else {
            source.extend_from_slice(format!("row{record},{record}\n").as_bytes());
        }
    }

    let directory = create_private_test_directory()?;
    let source_path = directory.join("large.csv");
    let index_path = directory.join("large.idx");
    fs::write(&source_path, &source)?;
    drop(CsvIndex::create_path(
        &source_path,
        &index_path,
        IndexOptions::default(),
    )?);

    let mut reader = CsvIndexReader::open(&index_path)?;
    assert_eq!(reader.len(), 50_000);
    let loaded = CsvIndex::load(&index_path)?;
    for record in [0usize, 1, 6, 7, 12_345, 33_333, 49_998, 49_999] {
        assert_eq!(reader.record_offset(record)?, Some(expected[record]));
        assert_eq!(loaded.record_offset(record), Some(expected[record]));

        let mut parser = reader.parser_at_path(&source_path, record)?;
        assert_eq!(parser.location().byte as u64, expected[record]);
        let (first, second, index) = {
            let mut line = parser.next_line()?.expect("missing indexed record");
            let row = line.record()?;
            (
                row.get(0).map(<[u8]>::to_vec),
                row.get(1).map(<[u8]>::to_vec),
                row.index(),
            )
        };
        let name = if record % 7 == 0 {
            format!("row\n{record}")
        } else {
            format!("row{record}")
        };
        assert_eq!(first.as_deref(), Some(name.as_bytes()));
        assert_eq!(second.as_deref(), Some(record.to_string().as_bytes()));
        assert_eq!(index, record as u64);
    }

    // The lazily built index is byte-for-byte what an in-memory build saves.
    let materialized = directory.join("materialized.idx");
    CsvIndex::build(&source, IndexOptions::default())?.save(&materialized)?;
    assert_eq!(fs::read(&materialized)?, fs::read(&index_path)?);

    fs::remove_file(materialized)?;
    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

#[test]
fn indexed_lines_match_a_naive_count_for_every_builder() -> Result<(), Box<dyn StdError>> {
    // Line numbers are counted incrementally from the previously indexed
    // record, so irregular newlines have to agree with a naive prefix count.
    let mut source = Vec::new();
    for record in 0..5_000u32 {
        match record % 5 {
            0 => source.extend_from_slice(format!("\"a\nb\nc{record}\",{record}\r\n").as_bytes()),
            1 => source.extend_from_slice(format!("plain{record},{record}\n").as_bytes()),
            2 => source.extend_from_slice(format!("\"quoted,{record}\",{record}\r\n").as_bytes()),
            3 => source.extend_from_slice(format!("\"\nlead{record}\",{record}\n").as_bytes()),
            _ => source.extend_from_slice(format!("tail{record},{record}\n").as_bytes()),
        }
    }

    let directory = create_private_test_directory()?;
    let source_path = directory.join("lines.csv");
    let index_path = directory.join("lines.idx");
    let streamed_path = directory.join("streamed.idx");
    fs::write(&source_path, &source)?;

    let built = CsvIndex::build(&source, IndexOptions::default())?;
    let streamed = CsvIndex::build_path(&source_path, &streamed_path, IndexOptions::default())?;
    let mut created = CsvIndexReader::open({
        drop(CsvIndex::create_path(
            &source_path,
            &index_path,
            IndexOptions::default(),
        )?);
        &index_path
    })?;

    assert_eq!(built.len(), 5_000);
    assert_eq!(built, streamed);
    assert_eq!(created.len(), 5_000);

    for record in 0..built.len() {
        let offset = usize::try_from(built.record_offset(record).expect("indexed offset"))?;
        #[expect(
            clippy::naive_bytecount,
            reason = "counting naively is the independent reference the incremental line origin must match"
        )]
        let newlines = source[..offset]
            .iter()
            .filter(|&&byte| byte == b'\n')
            .count();
        let expected = 1 + newlines as u64;
        let offset = offset as u64;
        assert_eq!(built.record_line(record), Some(expected), "record {record}");
        assert_eq!(created.record_offset(record)?, Some(offset));
        assert_eq!(created.record_line(record)?, Some(expected));
    }

    fs::remove_file(source_path)?;
    fs::remove_file(index_path)?;
    fs::remove_file(streamed_path)?;
    fs::remove_dir(directory)?;
    Ok(())
}

// ── Malformed serialized index fuzz ─────────────────────────────────────────

/// Drive every reader entry point over `bytes`, reporting whether any panicked.
///
/// The reader parses a binary format that may come from anywhere, so a
/// malformed input must produce an error, never a panic and never a read past
/// the end of the buffer.
fn exercise_reader(bytes: &[u8]) -> Result<(), Box<dyn StdError>> {
    let outcome = std::panic::catch_unwind(|| {
        if let Ok(mut reader) = CsvIndexReader::new(Cursor::new(bytes.to_vec())) {
            for record in [0_usize, 1, 2, 7, usize::MAX] {
                let _ = reader.record_offset(record);
                let _ = reader.location(record);
            }
            let _ = reader.verify();
        }
    });
    if outcome.is_err() {
        return Err("reader panicked on malformed index bytes".into());
    }
    Ok(())
}

#[test]
fn a_truncated_index_is_rejected_at_every_length() -> Result<(), Box<dyn StdError>> {
    // Every multi-byte field read must check the remaining length first. A
    // missing check shows up as a panic at exactly the length that cuts that
    // field in half, so every prefix is tried.
    let bytes = valid_index_bytes(b"a,b\n1,2\n3,4\n5,6\n")?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut failure = None;
    for length in 0..bytes.len() {
        if let Err(error) = exercise_reader(&bytes[..length]) {
            failure = Some(format!("length {length}: {error}"));
            break;
        }
    }
    std::panic::set_hook(previous);
    assert!(failure.is_none(), "{}", failure.unwrap_or_default());
    Ok(())
}

#[test]
fn a_corrupted_index_byte_never_panics() -> Result<(), Box<dyn StdError>> {
    // Flipping bits anywhere -- header, payload, or checksum -- must be caught
    // and reported, not trusted into an out-of-bounds read.
    let bytes = valid_index_bytes(b"a,b\n1,2\n3,4\n5,6\n")?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut failure = None;
    'outer: for position in 0..bytes.len() {
        for mask in [0x01_u8, 0x55, 0x80, 0xFF] {
            let mut corrupted = bytes.clone();
            corrupted[position] ^= mask;
            if let Err(error) = exercise_reader(&corrupted) {
                failure = Some(format!("position {position} mask {mask:#x}: {error}"));
                break 'outer;
            }
        }
    }
    std::panic::set_hook(previous);
    assert!(failure.is_none(), "{}", failure.unwrap_or_default());
    Ok(())
}

#[test]
fn an_index_declaring_a_huge_record_count_allocates_nothing() -> Result<(), Box<dyn StdError>> {
    // The record count is attacker-controlled, so it must be reconciled against
    // the real byte length before it is used to reserve anything.
    let bytes = valid_index_bytes(b"a,b\n1,2\n")?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let mut results = Vec::new();
    for offset in 0..bytes.len().saturating_sub(8) {
        let mut corrupted = bytes.clone();
        corrupted[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        results.push(exercise_reader(&corrupted).is_ok());
    }
    std::panic::set_hook(previous);
    assert!(
        results.iter().all(|ok| *ok),
        "a u64::MAX field caused a panic or an oversized allocation"
    );
    Ok(())
}

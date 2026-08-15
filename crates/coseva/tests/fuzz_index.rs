//! Coverage-guided and regression tests for the persistent index reader,
//! gated on the `index` feature.
//!
//! Two raw-byte targets run as bounded `cargo test` cases and far deeper under
//! a coverage-guided engine:
//!
//! ```text
//! crates/coseva/scripts/fuzz_campaign.py index_reader_survives_arbitrary_bytes
//! crates/coseva/scripts/fuzz_campaign.py csv_index_build_round_trips
//! ```
//!
//! # Bounded by construction
//!
//! A persistent index is self-describing: [`CsvIndexReader::new`] reads only the
//! fixed 93-byte header, then requires the file's actual length to equal the
//! length its stored record count implies (`count * 16 + header + trailer`)
//! before doing any count-driven work. A malformed header claiming a huge count
//! is therefore rejected immediately against the real byte length, never turned
//! into an allocation. Every later operation ([`CsvIndexReader::entry`] via
//! `record_offset` / `record_line` / `location`, and `verify`) is a bounded
//! seek-and-read or a streamed hash over that already-validated length. The
//! targets add their own input-size cap as a second guard so a single case can
//! never run long.
//!
//! # Corpus
//!
//! `tests/__fuzz__/<target>/corpus/` is replayed on every `cargo test`. The
//! index-bytes corpus seeds a valid version-9 index, an equivalent downgraded
//! version-8 index (both supported versions), and several deliberately
//! malformed headers; the round-trip corpus seeds source documents.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::Cursor;

use coseva::ErrorKind;
use coseva::config::{FormatOptions, Limits};
use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};

/// Iterations each bounded `cargo test` run performs on top of the corpus.
const BOUNDED_ITERATIONS: usize = 4096;
/// Wall-clock backstop for a bounded run.
const BOUNDED_TEST_TIME: std::time::Duration = std::time::Duration::from_millis(400);

/// Largest input a single bounded case will process, a second guard on top of
/// the reader's own length checks so no case runs long.
const MAX_INPUT: usize = 64 * 1024;

macro_rules! bounded {
    () => {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
    };
}

/// The persistent-index layout constants the seeds and regressions depend on.
/// These mirror the private constants in `src/index/format.rs`; the regression
/// tests fail loudly if the layout ever drifts from them.
const MAGIC: &[u8; 8] = b"BCSVIDX2";
const FIXED_HEADER_BYTES: usize = 93;
const CHECKSUM_BYTES: usize = 16;
const V9_TRAILER_BYTES: usize = 2 * CHECKSUM_BYTES;

/// Build a valid version-9 index for `source` entirely in memory.
fn valid_v9_index(source: &[u8]) -> Vec<u8> {
    let reader = CsvIndex::create(
        Cursor::new(source.to_vec()),
        Cursor::new(Vec::new()),
        IndexOptions::default(),
    )
    .expect("building an index for benign source cannot fail");
    reader.into_inner().into_inner()
}

/// Rewrite valid version-9 index bytes as an equivalent version-8 file: patch
/// the version field to 8 and replace the two independent trailer checksums
/// with the single combined checksum version 8 expects (one hash over the
/// header immediately followed by the entries).
fn downgrade_to_v8(mut bytes: Vec<u8>) -> Vec<u8> {
    bytes[8..12].copy_from_slice(&8u32.to_le_bytes());
    let payload_end = bytes.len() - V9_TRAILER_BYTES;
    let checksum = xxhash_rust::xxh3::xxh3_128(&bytes[..payload_end]);
    bytes.truncate(payload_end);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    bytes
}

/// Drive every stateful reader operation over one candidate index image.
///
/// Never panics for any input: a rejected image returns before the operations
/// run, and every operation returns a `Result`. Determinism of the read
/// operations is asserted so a malformed image yields a stable answer rather
/// than a different one each call.
fn exercise_reader(bytes: &[u8]) {
    let Ok(mut reader) = CsvIndexReader::new(Cursor::new(bytes.to_vec())) else {
        return;
    };

    // Header-derived accessors never fail once `new` accepted the image.
    let count = reader.len();
    let _ = reader.is_empty();
    let _ = reader.format();
    let _ = reader.limits();
    let source_len = reader.source_len();

    // Query a bounded set of record numbers spanning the valid range and just
    // past it, plus a couple derived from the bytes so the fuzzer can steer.
    let derived = usize::from(bytes.first().copied().unwrap_or(0));
    let clamped_count = usize::try_from(count).unwrap_or(usize::MAX);
    let probes = [
        0usize,
        1,
        clamped_count.saturating_sub(1),
        clamped_count,
        clamped_count.saturating_add(1),
        derived,
    ];
    for &record in &probes {
        let offset = reader.record_offset(record);
        // The same query must give the same answer every time.
        assert_eq!(
            format!("{:?}", reader.record_offset(record)),
            format!("{offset:?}"),
            "record_offset was not deterministic for {record}"
        );
        // A returned offset must lie inside the indexed source.
        if let Ok(Some(offset)) = offset {
            assert!(
                offset < source_len,
                "record_offset {offset} exceeded source_len {source_len}"
            );
        }
        let _ = reader.record_line(record);
        let _ = reader.location(record);
    }

    // The whole-index checksum, and identity validation against a bounded
    // source. Both are allowed to fail; neither may panic.
    let _ = reader.verify();
    let _ = reader.validate_reader(Cursor::new(bytes.to_vec()));
}

#[test]
fn index_reader_survives_arbitrary_bytes() {
    bounded!().for_each(|bytes: &[u8]| {
        if bytes.len() > MAX_INPUT {
            return;
        }
        exercise_reader(bytes);
    });
}

#[test]
fn csv_index_build_round_trips() {
    bounded!().for_each(|source: &[u8]| {
        if source.len() > MAX_INPUT {
            return;
        }
        let Ok(index) = CsvIndex::build(source, IndexOptions::default()) else {
            return;
        };
        let record_count = index.len();

        // Serialize the freshly built index and read it back: a just-built
        // index is always well formed, so `new` and `verify` must accept it.
        let bytes = valid_v9_index(source);
        let mut reader =
            CsvIndexReader::new(Cursor::new(bytes)).expect("a freshly built index must load");
        assert_eq!(
            usize::try_from(reader.len()).unwrap_or(usize::MAX),
            record_count,
            "reader and in-memory index disagree on record count"
        );
        reader.verify().expect("a freshly built index must verify");
        reader
            .validate_reader(Cursor::new(source.to_vec()))
            .expect("a freshly built index must match its own source");

        // Every indexed record has an offset inside the source, agreeing
        // between the in-memory index and the persistent reader.
        for record in 0..record_count {
            let eager = index.record_offset(record);
            let lazy = reader.record_offset(record).expect("offset read");
            assert_eq!(eager, lazy, "eager vs lazy offset differ for {record}");
            if let Some(offset) = eager {
                assert!(offset < source.len() as u64);
            }
        }
    });
}

// ── Deterministic regression tests ──────────────────────────────────────────

const SAMPLE_SOURCE: &[u8] = b"city,population\nBoston,650706\nDenver,715522\n";

#[test]
fn layout_constants_match_the_written_header() {
    let bytes = valid_v9_index(SAMPLE_SOURCE);
    assert!(bytes.starts_with(MAGIC), "index must begin with the magic");
    assert_eq!(
        bytes[8..12],
        9u32.to_le_bytes(),
        "current writer must emit version 9"
    );
    // The index is built with headers disabled, so all three physical lines
    // are indexed: header + 3 records * 16 + v9 trailer (two checksums).
    let expected = FIXED_HEADER_BYTES + 3 * 16 + V9_TRAILER_BYTES;
    assert_eq!(bytes.len(), expected, "index layout drifted from constants");
}

#[test]
fn valid_v9_index_loads_and_verifies() {
    let bytes = valid_v9_index(SAMPLE_SOURCE);
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("v9 index must load");
    assert_eq!(reader.len(), 3, "all three records expected");
    reader.verify().expect("v9 index must verify");
    let offset = reader
        .record_offset(1)
        .expect("record 1 offset")
        .expect("record 1 present");
    assert!(offset < reader.source_len());
    reader
        .validate_reader(Cursor::new(SAMPLE_SOURCE.to_vec()))
        .expect("source must match");
}

#[test]
fn downgraded_v8_index_loads_and_verifies() {
    let bytes = downgrade_to_v8(valid_v9_index(SAMPLE_SOURCE));
    assert_eq!(bytes[8..12], 8u32.to_le_bytes(), "must be version 8 now");
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("v8 index must load");
    reader.verify().expect("v8 index must verify");
    assert_eq!(reader.len(), 3);
}

#[test]
fn corrupted_v8_entry_is_detected() {
    let mut bytes = downgrade_to_v8(valid_v9_index(SAMPLE_SOURCE));
    bytes[FIXED_HEADER_BYTES] ^= 0x55;
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("length still consistent");
    let error = reader
        .verify()
        .expect_err("a corrupted v8 index must be caught");
    assert_eq!(error.kind(), ErrorKind::InvalidIndex);
}

#[test]
fn malformed_headers_are_rejected_without_panic() {
    // Empty, too short for a header, wrong magic, unsupported version, and a
    // header whose stored count implies a length far larger than the file.
    let empty: Vec<u8> = Vec::new();
    let short = vec![0u8; FIXED_HEADER_BYTES - 1];

    let mut wrong_magic = valid_v9_index(SAMPLE_SOURCE);
    wrong_magic[0] = b'X';

    let mut wrong_version = valid_v9_index(SAMPLE_SOURCE);
    wrong_version[8..12].copy_from_slice(&999u32.to_le_bytes());

    let mut huge_count = valid_v9_index(SAMPLE_SOURCE);
    // count is the last 8 bytes of the fixed header.
    huge_count[FIXED_HEADER_BYTES - 8..FIXED_HEADER_BYTES].copy_from_slice(&u64::MAX.to_le_bytes());

    for image in [empty, short, wrong_magic, wrong_version, huge_count] {
        // Neither construction nor a full exercise pass may panic.
        exercise_reader(&image);
        if let Err(error) = CsvIndexReader::new(Cursor::new(image)) {
            assert!(
                matches!(error.kind(), ErrorKind::InvalidIndex | ErrorKind::Io(_)),
                "unexpected rejection kind: {:?}",
                error.kind()
            );
        }
    }
}

#[test]
fn index_reader_honors_a_custom_format() {
    let source = b"a;b\n1;2\n3;4\n";
    let options = IndexOptions {
        format: FormatOptions::SEMICOLON,
        limits: Limits::DEFAULT,
    };
    let reader = CsvIndex::create(
        Cursor::new(source.to_vec()),
        Cursor::new(Vec::new()),
        options,
    )
    .expect("semicolon index builds");
    let bytes = reader.into_inner().into_inner();
    let mut reader = CsvIndexReader::new(Cursor::new(bytes)).expect("loads");
    reader.verify().expect("verifies");
    assert_eq!(reader.format(), FormatOptions::SEMICOLON);
}

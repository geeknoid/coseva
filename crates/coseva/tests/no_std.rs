//! Behavioral coverage for the alloc-only crate surface.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(not(feature = "std"))]

use coseva::ErrorKind;
use coseva::Predicate;
use coseva::PushEmitter;
use coseva::PushParser;
use coseva::SliceParser;
use coseva::VecEmitter;
use coseva::config::{EmitOptions, FormatOptions, Headers, ParseOptions};
use coseva::{ByteRecord, FieldProjection, TextRecord};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const OUTLIER_BYTES: usize = 512 * 1024;
static CAPTURE_LARGE_ALLOCATION: AtomicBool = AtomicBool::new(false);
static SCRATCH_ALLOCATION: AtomicUsize = AtomicUsize::new(0);
static SCRATCH_RELEASED: AtomicBool = AtomicBool::new(false);

struct TrackingAllocator;

// SAFETY: Every allocation operation is forwarded unchanged to `System`.
unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller upholds `GlobalAlloc::alloc`'s layout contract.
        let ptr = unsafe { System.alloc(layout) };
        if CAPTURE_LARGE_ALLOCATION.load(Ordering::Relaxed)
            && layout.size() >= OUTLIER_BYTES
            && !ptr.is_null()
        {
            let _ = SCRATCH_ALLOCATION.compare_exchange(
                0,
                ptr as usize,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr as usize == SCRATCH_ALLOCATION.load(Ordering::Relaxed) {
            SCRATCH_RELEASED.store(true, Ordering::Relaxed);
        }
        // SAFETY: The caller passes the pointer and layout returned by this allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let tracked = ptr as usize == SCRATCH_ALLOCATION.load(Ordering::Relaxed);
        // SAFETY: The caller upholds `GlobalAlloc::realloc`'s pointer and layout contract.
        let replacement = unsafe { System.realloc(ptr, layout, new_size) };
        if tracked && new_size < layout.size() {
            SCRATCH_RELEASED.store(true, Ordering::Relaxed);
        }
        if CAPTURE_LARGE_ALLOCATION.load(Ordering::Relaxed)
            && new_size >= OUTLIER_BYTES
            && !replacement.is_null()
        {
            let _ = SCRATCH_ALLOCATION.compare_exchange(
                0,
                replacement as usize,
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

#[test]
fn alloc_only_slice_reader_preserves_dialect_metadata() {
    let mut reader = SliceParser::with_options(
        b",\"\",value\n",
        FormatOptions::POSTGRES_COPY_CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid alloc-only reader");
    let mut line = reader.next_line().expect("valid CSV").expect("one record");
    let record = line.record().expect("valid CSV");

    assert_eq!(record.get(0), Some(&b""[..]));
    assert_eq!(record.get(1), Some(&b""[..]));
    assert_eq!(record.get(2), Some(&b"value"[..]));
    assert_eq!(record.is_null(0), Some(true));
    assert_eq!(record.is_null(1), Some(false));
}

#[test]
fn alloc_only_vec_writer_reports_policy_errors() {
    let mut writer = VecEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())
        .expect("valid alloc-only writer");
    writer
        .emit_record([b"city".as_slice(), b"population".as_slice()])
        .expect("record is encodable");
    assert_eq!(writer.as_bytes(), b"city,population\n");

    let mut unquoted = VecEmitter::with_options(
        Vec::new(),
        FormatOptions::CSV.quoting(coseva::config::Quoting::Never),
        EmitOptions::new(),
    )
    .expect("valid alloc-only writer");
    let error = unquoted
        .emit_record([b"contains,delimiter".as_slice()])
        .expect_err("unquoted structural data must fail");
    assert_eq!(error.kind(), coseva::ErrorKind::Encode);
}

#[test]
fn alloc_only_push_emitter_clear_reclaims_builder_scratch_and_remains_reusable() {
    let mut emitter = PushEmitter::default();
    let outlier = vec![b'x'; OUTLIER_BYTES];
    {
        let mut pending = emitter.begin_record();
        CAPTURE_LARGE_ALLOCATION.store(true, Ordering::Relaxed);
        pending
            .write_field(&outlier)
            .expect("outlier field is accepted");
        CAPTURE_LARGE_ALLOCATION.store(false, Ordering::Relaxed);
    }
    assert_ne!(
        SCRATCH_ALLOCATION.load(Ordering::Relaxed),
        0,
        "the outlier must grow builder scratch"
    );
    {
        // Taking the returned builder clears its live bytes while retaining
        // the outlier allocation, making that spare capacity reclaimable.
        let _pending = emitter.begin_record();
    }
    drop(outlier);

    emitter.clear();
    assert!(
        SCRATCH_RELEASED.load(Ordering::Relaxed),
        "clear must release the outlier builder allocation"
    );

    let mut pending = emitter.begin_record();
    pending
        .write_field("after")
        .expect("the reclaimed builder remains usable");
    pending.finish().expect("the reused emitter encodes");
    assert_eq!(emitter.buffer(), b"after\n");
}

#[test]
fn alloc_only_slice_errors_keep_exact_positions() {
    let mut reader = SliceParser::with_options(
        b"ok\nbad\"quote\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut line = reader
        .next_line()
        .expect("first record is valid")
        .expect("first record exists");
    line.record().expect("first record is valid");
    let mut line = reader
        .next_line()
        .expect("second record exists")
        .expect("second record exists");
    let error = line
        .record()
        .expect_err("second record contains an unexpected quote");
    assert_eq!(error.kind(), ErrorKind::UnexpectedQuote);
    assert_eq!(error.location().byte, 6);
    assert_eq!(error.location().line, 2);
}

#[test]
fn alloc_only_incremental_primitives_work() {
    let mut parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options");
    let mut chunk = parser.chunk(b"a,b\nc");
    let mut line = chunk
        .next_line()
        .expect("first record is complete")
        .expect("first record is complete");
    assert_eq!(
        line.record().expect("first record is valid").get(1),
        Some(&b"b"[..])
    );
    drop(line);
    assert!(
        chunk
            .next_line()
            .expect("the tail record is unterminated")
            .is_none(),
        "the final record waits for the end of the stream"
    );
    assert_eq!(chunk.done(), 5, "the whole chunk is taken");

    parser.finish();
    let mut chunk = parser.chunk(b"");
    let mut line = chunk
        .next_line()
        .expect("final record is valid")
        .expect("final record is valid");
    assert_eq!(
        line.record().expect("final record is valid").get(0),
        Some(&b"c"[..])
    );
    drop(line);
    assert!(
        chunk
            .next_line()
            .expect("the stream is exhausted")
            .is_none()
    );
    drop(chunk);
    assert!(parser.is_done());
}

#[test]
fn alloc_only_direct_owned_chunk_reads_work() {
    let options = || ParseOptions::new().headers(Headers::None);

    let mut bytes_parser =
        PushParser::with_options(FormatOptions::CSV, options()).expect("valid options");
    let mut bytes = ByteRecord::new();
    let mut chunk = bytes_parser.chunk(b"a,b\nc,d");
    assert!(
        chunk
            .read_byte_record_into(&mut bytes)
            .expect("first byte record")
    );
    assert_eq!(bytes.get(1), Some(&b"b"[..]));
    assert!(
        !chunk
            .read_byte_record_into(&mut bytes)
            .expect("unterminated byte record waits")
    );
    assert_eq!(chunk.done(), 7);
    bytes_parser.finish();
    let mut chunk = bytes_parser.chunk(b"");
    assert!(
        chunk
            .read_byte_record_into(&mut bytes)
            .expect("final byte record")
    );
    assert_eq!(bytes.get(1), Some(&b"d"[..]));
    drop(chunk);

    let mut text_parser =
        PushParser::with_options(FormatOptions::CSV, options()).expect("valid options");
    let mut text = TextRecord::new();
    let mut chunk = text_parser.chunk(b"alpha,\xC3");
    assert!(
        !chunk
            .read_text_record_into(&mut text)
            .expect("split UTF-8 waits")
    );
    assert_eq!(chunk.done(), 7);
    let mut chunk = text_parser.chunk(b"\xA9\n");
    assert!(
        chunk
            .read_text_record_into(&mut text)
            .expect("completed text record")
    );
    assert_eq!(text.get(1), Some("é"));
    drop(chunk);

    let mut invalid_parser =
        PushParser::with_options(FormatOptions::CSV, options()).expect("valid options");
    let mut chunk = invalid_parser.chunk(b"alpha,\xC3");
    assert!(
        !chunk
            .read_text_record_into(&mut text)
            .expect("incomplete UTF-8 is provisional before EOF")
    );
    let _ = chunk.done();
    invalid_parser.finish();
    let mut chunk = invalid_parser.chunk(b"");
    let error = chunk
        .read_text_record_into(&mut text)
        .expect_err("incomplete UTF-8 is invalid at EOF");
    assert!(matches!(error.kind(), ErrorKind::InvalidUtf8(_)));
}

#[test]
fn alloc_only_projection_selects_fields() {
    let headers = ByteRecord::from(vec![b"city".to_vec(), b"population".to_vec()]);
    let projection =
        FieldProjection::from_headers(&headers, ["population", "city"]).expect("resolvable");
    let record = ByteRecord::from(vec![b"Boston".to_vec(), b"650706".to_vec()]);
    let fields: Vec<_> = record.project(&projection).collect();
    assert_eq!(fields, [Some(&b"650706"[..]), Some(&b"Boston"[..])]);
}

#[test]
fn alloc_only_predicate_filters_records() {
    let predicate = Predicate::equals("city", "Boston");
    let mut reader = SliceParser::with_options(
        b"city,population\nBoston,650706\nDenver,715522\nBoston,1\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::FirstRecord),
    )
    .expect("valid alloc-only reader");

    let matched: Vec<ByteRecord> = reader
        .matching_byte_records(&predicate)
        .collect::<Result<_, _>>()
        .expect("valid CSV");

    // By content rather than by count: a predicate that matched everything, or
    // that resolved the name to the wrong column, would still return records.
    assert_eq!(matched.len(), 2);
    assert_eq!(matched[0].get(0), Some(&b"Boston"[..]));
    assert_eq!(matched[0].get(1), Some(&b"650706"[..]));
    assert_eq!(matched[1].get(0), Some(&b"Boston"[..]));
    assert_eq!(matched[1].get(1), Some(&b"1"[..]));
}

#[cfg(feature = "derive")]
#[derive(Debug, Eq, PartialEq, coseva::encoding::CsvDecode, coseva::encoding::CsvEncode)]
struct DerivedRow {
    city: String,
    population: u64,
}

#[cfg(feature = "derive")]
#[test]
fn alloc_only_derive_uses_core_paths() {
    let mut reader = SliceParser::with_options(
        b"Boston,650706\n",
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid alloc-only reader");
    let mut line = reader
        .next_line()
        .expect("valid typed row")
        .expect("one typed row");
    let row = line.decoded::<DerivedRow>().expect("valid typed row");
    assert_eq!(
        row,
        DerivedRow {
            city: "Boston".into(),
            population: 650_706,
        }
    );

    let mut writer = VecEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())
        .expect("valid alloc-only writer");
    writer.encode(&row).expect("derived row is encodable");
    assert_eq!(writer.as_bytes(), b"Boston,650706\n");
}

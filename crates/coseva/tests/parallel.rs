//! Parallel slice parsing, checked against the serial parser it must match.
//!
//! [`SliceParser`] is the reference throughout: the parallel parser is only
//! ever an optimization of it, so every test here asks whether the two agree
//! on records, on their order, on the positions they carry, and on which error
//! a malformed document produces.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![cfg(feature = "parallel")]

use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use coseva::config::{
    BlankRecords, Escape, FieldCount, FormatOptions, Headers, ParseOptions, RecordEnding, Recovery,
    Syntax,
};
use coseva::format::Csv;
use coseva::parallel::ParallelParser;
use coseva::{ByteRecord, Error, SliceParser};

fn threads(count: usize) -> NonZeroUsize {
    NonZeroUsize::new(count).expect("a positive thread count")
}

/// Every record a serial parse produces, with the positions it assigns.
fn serial(input: &[u8], format: FormatOptions, options: ParseOptions) -> Vec<ByteRecord> {
    let mut parser =
        SliceParser::with_options(input, format, options).expect("valid configuration");
    let mut records = Vec::new();
    while let Some(mut line) = parser.next_line().expect("a well-formed document") {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)
            .expect("a well-formed record");
        records.push(record);
    }
    records
}

fn same(left: &[ByteRecord], right: &[ByteRecord]) {
    assert_eq!(left.len(), right.len(), "record counts differ");
    for (index, (want, got)) in left.iter().zip(right).enumerate() {
        assert_eq!(
            want.iter().collect::<Vec<_>>(),
            got.iter().collect::<Vec<_>>(),
            "fields of record {index} differ"
        );
        assert_eq!(
            want.byte_range(),
            got.byte_range(),
            "byte range of record {index} differs"
        );
        assert_eq!(want.index(), got.index(), "index of record {index} differs");
    }
}

/// A document with quoted fields holding delimiters, record endings and
/// doubled quotes, so a split at the wrong byte cannot go unnoticed.
fn hostile_document(records: usize) -> Vec<u8> {
    let mut document = Vec::new();
    document.extend_from_slice(b"id,name,note,value\n");
    for index in 0..records {
        let row = match index % 4 {
            0 => format!("{index},plain,nothing to see,{index}\n"),
            1 => format!("{index},\"quoted, with a comma\",fine,{index}\n"),
            2 => format!("{index},\"spans\na line\",\"and a \"\"quote\"\"\",{index}\n"),
            _ => format!("{index},,\"\",{index}\n"),
        };
        document.extend_from_slice(row.as_bytes());
    }

    document
}

fn hostile_crlf_document(records: usize) -> Vec<u8> {
    hostile_document(records)
        .into_iter()
        .flat_map(|byte| {
            if byte == b'\n' {
                vec![b'\r', b'\n']
            } else {
                vec![byte]
            }
        })
        .collect()
}

/// A parser that splits whatever it is given, so small documents in tests
/// still take the threaded path.
fn eager(count: usize) -> ParallelParser<Csv> {
    ParallelParser::<Csv>::new(ParseOptions::new())
        .threads(threads(count))
        .parallel_threshold(0)
}

#[test]
fn records_and_their_order_match_a_serial_parse() {
    for (format, document) in [
        (FormatOptions::CSV, hostile_document(5_000)),
        (
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            hostile_crlf_document(5_000),
        ),
    ] {
        let want = serial(&document, format, ParseOptions::new());

        for count in 1..=17 {
            let got = ParallelParser::with_options(format, ParseOptions::new())
                .threads(threads(count))
                .parallel_threshold(0)
                .byte_records(&document)
                .expect("a well-formed document");
            same(&want, &got);
        }
    }
}

#[test]
fn the_batch_size_changes_nothing_but_the_batches() {
    let document = hostile_document(2_000);
    let want = serial(&document, FormatOptions::CSV, ParseOptions::new());

    for size in [1, 2, 7, 64, 100_000] {
        let got = eager(4)
            .batch_records(threads(size))
            .byte_records(&document)
            .expect("a well-formed document");
        same(&want, &got);
    }
}

#[test]
fn a_document_below_the_threshold_parses_on_the_calling_thread() {
    let document = hostile_document(100);
    let want = serial(&document, FormatOptions::CSV, ParseOptions::new());
    let got = ParallelParser::<Csv>::new(ParseOptions::new())
        .threads(threads(8))
        .byte_records(&document)
        .expect("a well-formed document");
    same(&want, &got);
}

#[test]
fn batches_are_bounded_by_the_configured_work_unit() {
    let document = hostile_document(5_000);
    let largest = AtomicUsize::new(0);
    eager(4)
        .batch_records(threads(32))
        .for_each_batch::<_, Error>(&document, |batch| {
            largest.fetch_max(batch.len(), Ordering::Relaxed);
            batch.clear();
            Ok(())
        })
        .expect("a well-formed document");
    assert!(
        largest.load(Ordering::Relaxed) <= 32,
        "a batch exceeded the configured work unit"
    );
}

#[test]
fn headers_are_read_once_rather_than_once_per_chunk() {
    let document = hostile_document(3_000);
    let parser = eager(4);

    let headers = parser
        .headers(&document)
        .expect("a well-formed header record")
        .expect("headers");
    assert_eq!(headers.get(0), Some(&b"id"[..]));

    // Every chunk after the first would otherwise eat a data record.
    let records = parser
        .byte_records(&document)
        .expect("a well-formed document");
    assert_eq!(records.len(), 3_000);
    assert_eq!(records[0].get(1), Some(&b"plain"[..]));
}

#[test]
fn a_headerless_document_keeps_every_record() {
    let document = hostile_document(3_000);
    let options = ParseOptions::new().headers(Headers::None);
    let want = serial(&document, FormatOptions::CSV, options.clone());
    let got = ParallelParser::<Csv>::new(options)
        .threads(threads(4))
        .parallel_threshold(0)
        .byte_records(&document)
        .expect("a well-formed document");
    same(&want, &got);
    assert_eq!(got.len(), 3_001);
}

#[test]
fn provided_headers_consume_no_record() {
    let document = hostile_document(2_000);
    let names = ByteRecord::from_iter(["id", "name", "note", "value"]);
    let options = ParseOptions::new().headers(Headers::Provided(names));
    let want = serial(&document, FormatOptions::CSV, options.clone());
    let got = ParallelParser::<Csv>::new(options)
        .threads(threads(4))
        .parallel_threshold(0)
        .byte_records(&document)
        .expect("a well-formed document");
    same(&want, &got);
}

#[test]
fn a_matched_field_count_is_taken_from_the_first_record_of_the_document() {
    let mut document = hostile_document(2_000);
    document.extend_from_slice(b"one,two\n");
    let options = ParseOptions::new().field_count(FieldCount::MatchFirst);

    // Every chunk must be measured against the document's first record, not
    // against its own, so the ragged record at the end is rejected wherever a
    // chunk boundary happens to fall.
    let failure = ParallelParser::<Csv>::new(options.clone())
        .threads(threads(4))
        .parallel_threshold(0)
        .byte_records(&document)
        .expect_err("a ragged record");
    let want = serial_failure(&document, options);
    assert_eq!(failure.kind(), want.kind());
    // The record's position is the same; only the field index differs, because
    // the parallel parser resolves the rule to `FieldCount::Exact` up front and
    // so reports the record rather than the field it ran short at.
    assert_eq!(failure.location().byte, want.location().byte);
    assert_eq!(failure.location().record, want.location().record);
}

#[test]
fn the_reported_failure_is_the_first_in_the_document() {
    let mut document = hostile_document(4_000);
    let early = document.len();
    document.extend_from_slice(b"9,\"unterminated,x,1\n");
    document.extend_from_slice(&hostile_document(4_000)[19..]);
    document.extend_from_slice(b"9,\"another one,x,1\n");

    // Whichever thread reaches a failure first, the one that is reported is
    // the one at the lowest byte offset.
    for count in [1, 2, 3, 8, 16] {
        let failure = eager(count)
            .byte_records(&document)
            .expect_err("a malformed document");
        assert_eq!(
            failure.location(),
            serial_failure(&document, ParseOptions::new()).location(),
            "with {count} threads"
        );
        assert!(failure.location().byte >= early);
    }
}

/// The failure a serial parse gives up with.
fn serial_failure(input: &[u8], options: ParseOptions) -> Error {
    let mut parser =
        SliceParser::with_options(input, FormatOptions::CSV, options).expect("valid configuration");
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                if let Err(error) = line.read_byte_record_into(&mut ByteRecord::new()) {
                    return error;
                }
            }
            Ok(None) => unreachable!("the document is malformed"),
            Err(error) => return error,
        }
    }
}

/// A consumer error type that is not a parse error, which is the case the
/// [`From`] bound on `for_each_batch` exists to serve.
#[derive(Debug, Eq, PartialEq)]
enum Enough {
    Consumer,
    Parse(Error),
}

impl From<Error> for Enough {
    fn from(error: Error) -> Self {
        Self::Parse(error)
    }
}

#[test]
fn an_error_from_the_consumer_stops_parsing_and_is_returned_unchanged() {
    let document = hostile_document(20_000);
    let seen = AtomicUsize::new(0);
    let failure = eager(4)
        .batch_records(threads(8))
        .for_each_batch::<_, Enough>(&document, |batch| {
            if seen.fetch_add(batch.len(), Ordering::Relaxed) > 100 {
                return Err(Enough::Consumer);
            }
            batch.clear();
            Ok(())
        })
        .expect_err("the consumer's error");
    assert_eq!(failure, Enough::Consumer);
    assert!(seen.load(Ordering::Relaxed) < 20_000);
}

fn consumer_then_parse_error_document() -> Vec<u8> {
    let mut document = Vec::from(&b"id,value\n"[..]);
    for index in 0..256 {
        document.extend_from_slice(format!("{index},ok\n").as_bytes());
    }
    for index in 0..16 {
        document.extend_from_slice(format!("reject-{index},prefix\n").as_bytes());
    }
    document.extend_from_slice(b"malformed,\"unterminated");
    document
}

fn rejects_the_partial_batch(parser: ParallelParser<Csv>) {
    let failure = parser
        .for_each_batch::<_, Enough>(&consumer_then_parse_error_document(), |batch| {
            if batch.iter().any(|record| {
                record
                    .get(0)
                    .is_some_and(|field| field.starts_with(b"reject-"))
            }) {
                return Err(Enough::Consumer);
            }
            batch.clear();
            Ok(())
        })
        .expect_err("the consumer rejects the prefix before the malformed record");
    assert_eq!(failure, Enough::Consumer);
}

#[test]
fn a_serial_consumer_error_wins_over_a_later_parse_error_in_the_same_batch() {
    rejects_the_partial_batch(ParallelParser::<Csv>::new(ParseOptions::new()));
}

#[test]
fn a_threaded_consumer_error_wins_over_a_later_parse_error_in_the_same_batch() {
    rejects_the_partial_batch(eager(2));
}

#[test]
fn a_format_that_cannot_be_split_exactly_is_rejected() {
    let document = hostile_document(10);
    let unsplittable = [
        FormatOptions::CSV.comment(Some(b'#')),
        FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
        FormatOptions::CSV.escape(Escape::Mysql),
        FormatOptions::CSV.blank_records(BlankRecords::Skip),
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE.unquoted_quotes(true))),
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE.quoting(false))),
    ];

    for format in unsplittable {
        let failure = ParallelParser::with_options(format, ParseOptions::new())
            .byte_records(&document)
            .expect_err("an unsplittable format");
        assert!(
            failure
                .to_string()
                .contains("parallel parsing cannot split"),
            "unexpected error: {failure}"
        );
    }
}

#[test]
fn a_dynamic_format_splits_as_a_static_one_does() {
    let document = hostile_document(3_000)
        .into_iter()
        .map(|byte| if byte == b',' { b'\t' } else { byte })
        .collect::<Vec<_>>();
    let want = serial(&document, FormatOptions::TSV, ParseOptions::new());
    let got = ParallelParser::with_options(FormatOptions::TSV, ParseOptions::new())
        .threads(threads(4))
        .parallel_threshold(0)
        .byte_records(&document)
        .expect("a well-formed document");
    same(&want, &got);
}

#[test]
fn a_document_with_carriage_returns_splits_the_same_way() {
    let document = hostile_document(3_000)
        .into_iter()
        .flat_map(|byte| {
            if byte == b'\n' {
                vec![b'\r', b'\n']
            } else {
                vec![byte]
            }
        })
        .collect::<Vec<_>>();
    let want = serial(&document, FormatOptions::CSV, ParseOptions::new());
    let got = eager(4)
        .byte_records(&document)
        .expect("a well-formed document");
    same(&want, &got);
}

#[test]
fn a_short_or_empty_document_is_handled_without_threads_going_wrong() {
    for document in [
        &b""[..],
        b"a,b\n",
        b"a,b\nc,d\n",
        b"a,b\nc,d",
        b"\"a\nb\",c\n1,2\n",
    ] {
        let want = serial(document, FormatOptions::CSV, ParseOptions::new());
        let got = eager(8).byte_records(document).expect("a valid document");
        same(&want, &got);
    }
}

#[test]
fn a_document_whose_records_dwarf_a_chunk_still_agrees() {
    // One enormous quoted field per record, so most chunks hold a single
    // record and many workers find their chunk already consumed.
    let mut document = Vec::from(&b"id,blob\n"[..]);
    for index in 0..200 {
        document.extend_from_slice(format!("{index},\"").as_bytes());
        document.extend(std::iter::repeat_n(b'x', 4_096));
        document.extend_from_slice(b",\ny\"\n");
    }
    let want = serial(&document, FormatOptions::CSV, ParseOptions::new());
    let got = eager(8).byte_records(&document).expect("a valid document");
    same(&want, &got);
}

/// A small deterministic generator, so a failure is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).expect("a bound that fits")
    }

    /// A lowercase letter, the filler these documents are made of.
    fn letter(&mut self) -> u8 {
        b'a' + u8::try_from(self.below(26)).expect("a letter within the alphabet")
    }
}

/// A random well-formed document, weighted towards the bytes that matter.
fn generated_document(rng: &mut Rng, records: usize) -> Vec<u8> {
    let mut document = Vec::new();
    for _ in 0..records {
        let fields = 1 + rng.below(4);
        for field in 0..fields {
            if field > 0 {
                document.push(b',');
            }
            if rng.below(3) == 0 {
                document.push(b'"');
                for _ in 0..rng.below(12) {
                    match rng.below(6) {
                        0 => document.extend_from_slice(b"\"\""),
                        1 => document.push(b','),
                        2 => document.push(b'\n'),
                        3 => document.extend_from_slice(b"\r\n"),
                        _ => document.push(rng.letter()),
                    }
                }
                document.push(b'"');
            } else {
                for _ in 0..rng.below(8) {
                    document.push(rng.letter());
                }
            }
        }
        document.push(b'\n');
    }
    document
}

#[test]
#[expect(
    clippy::panic,
    reason = "the panic includes the generated case and thread count"
)]
fn generated_documents_match_a_serial_parse() {
    let mut rng = Rng(0x5eed_1234_9abc_def1);
    for case in 0..200 {
        let records = 1 + rng.below(300);
        let document = generated_document(&mut rng, records);
        let want = serial(&document, FormatOptions::CSV, ParseOptions::new());
        for count in [2, 3, 5] {
            let got = eager(count)
                .byte_records(&document)
                .unwrap_or_else(|error| panic!("case {case} with {count} threads: {error}"));
            same(&want, &got);
        }
    }
}

/// One record's identity, made comparable across the two parsers: the fields,
/// the absolute byte range, and the record index it carries.
type Snapshot = (Vec<Vec<u8>>, std::ops::Range<usize>, u64);

/// A serial parse's records reduced to comparable snapshots.
fn snapshots(records: &[ByteRecord]) -> Vec<Snapshot> {
    records
        .iter()
        .map(|record| {
            (
                record.iter().map(<[u8]>::to_vec).collect(),
                record.byte_range(),
                record.index(),
            )
        })
        .collect()
}

/// Every record `for_each_record` produces, restored to document order.
///
/// The borrowed path delivers records on the worker threads in no guaranteed
/// order, so each is snapshotted under a lock and the results are sorted by the
/// byte offset each carries. Records are disjoint, so that offset is a total
/// order and reproduces the serial sequence exactly when the parse agrees.
fn borrowed_in_order(parser: &ParallelParser<Csv>, input: &[u8]) -> Vec<Snapshot> {
    let collected = Mutex::new(Vec::new());
    parser
        .for_each_record::<_, Error>(input, |record| {
            let snapshot = (
                record.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                record.byte_range(),
                record.index(),
            );
            collected
                .lock()
                .expect("an uncontended lock")
                .push(snapshot);
            Ok(())
        })
        .expect("a well-formed document");
    let mut records = collected.into_inner().expect("no panic poisoned the lock");
    records.sort_by_key(|(_, range, _)| range.start);
    records
}

fn borrowed_in_order_with_format(
    format: FormatOptions,
    input: &[u8],
    count: usize,
) -> Vec<Snapshot> {
    let collected = Mutex::new(Vec::new());
    ParallelParser::with_options(format, ParseOptions::new())
        .threads(threads(count))
        .parallel_threshold(0)
        .for_each_record::<_, Error>(input, |record| {
            let snapshot = (
                record.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                record.byte_range(),
                record.index(),
            );
            collected
                .lock()
                .expect("an uncontended lock")
                .push(snapshot);
            Ok(())
        })
        .expect("a well-formed document");
    let mut records = collected.into_inner().expect("no panic poisoned the lock");
    records.sort_by_key(|(_, range, _)| range.start);
    records
}

fn serial_snapshots_until_failure(input: &[u8]) -> (Vec<Snapshot>, Error) {
    let mut parser = SliceParser::with_options(input, FormatOptions::CSV, ParseOptions::new())
        .expect("valid configuration");
    let mut records = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                let mut record = ByteRecord::new();
                match line.read_byte_record_into(&mut record) {
                    Ok(()) => records.push(record),
                    Err(error) => return (snapshots(&records), error),
                }
            }
            Ok(None) => unreachable!("the document is malformed"),
            Err(error) => return (snapshots(&records), error),
        }
    }
}

#[test]
fn for_each_record_agrees_with_a_serial_parse() {
    for (format, document) in [
        (FormatOptions::CSV, hostile_document(5_000)),
        (
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            hostile_crlf_document(5_000),
        ),
    ] {
        let want = snapshots(&serial(&document, format, ParseOptions::new()));

        // The threaded borrowed path and its serial fallback must both
        // reproduce the serial parse: the fields, the absolute ranges, and the
        // indices.
        for count in 1..=17 {
            let got = borrowed_in_order_with_format(format, &document, count);
            assert_eq!(got, want, "with {count} threads");
        }
    }
}

#[test]
fn for_each_batch_delivers_the_serial_prefix_before_a_parse_error() {
    let document = b"id,name\n1,ok\n2,\"unterminated".to_vec();
    let (want, want_error) = serial_snapshots_until_failure(&document);
    assert!(!want.is_empty(), "the malformed input needs a valid prefix");

    for count in [1, 2, 4, 8] {
        let mut got = Vec::new();
        let failure = eager(count)
            .batch_records(threads(1024))
            .for_each_batch::<_, Error>(&document, |batch| {
                got.extend(snapshots(batch));
                batch.clear();
                Ok(())
            })
            .expect_err("a malformed document");
        assert_eq!(got, want, "with {count} threads");
        assert_eq!(failure.kind(), want_error.kind(), "with {count} threads");
        assert_eq!(
            failure.location(),
            want_error.location(),
            "with {count} threads"
        );
    }
}

#[test]
fn for_each_record_below_the_threshold_runs_on_the_calling_thread_in_order() {
    let document = hostile_document(200);
    let want = snapshots(&serial(&document, FormatOptions::CSV, ParseOptions::new()));

    // The default threshold leaves a small document on the calling thread, so
    // the records arrive in document order without a sort.
    let order = Mutex::new(Vec::new());
    ParallelParser::<Csv>::new(ParseOptions::new())
        .threads(threads(8))
        .for_each_record::<_, Error>(&document, |record| {
            order.lock().expect("an uncontended lock").push((
                record.into_iter().map(<[u8]>::to_vec).collect::<Vec<_>>(),
                record.byte_range(),
                record.index(),
            ));
            Ok(())
        })
        .expect("a well-formed document");
    assert_eq!(
        order.into_inner().expect("no panic poisoned the lock"),
        want
    );
}

#[test]
fn for_each_record_sums_a_column_across_threads() {
    let mut document = Vec::from(&b"n\n"[..]);
    let mut expected = 0_u64;
    for value in 0..20_000_u64 {
        document.extend_from_slice(format!("{value}\n").as_bytes());
        expected += value;
    }

    for count in [1, 2, 4, 8] {
        let total = AtomicU64::new(0);
        eager(count)
            .for_each_record::<_, Error>(&document, |record| {
                total.fetch_add(record.parse::<u64>(0)?.unwrap_or(0), Ordering::Relaxed);
                Ok(())
            })
            .expect("a well-formed document");
        assert_eq!(total.into_inner(), expected, "with {count} threads");
    }
}

#[test]
fn for_each_record_reports_the_first_failure_in_the_document() {
    let mut document = hostile_document(4_000);
    let early = document.len();
    document.extend_from_slice(b"9,\"unterminated,x,1\n");
    document.extend_from_slice(&hostile_document(4_000)[19..]);
    document.extend_from_slice(b"9,\"another one,x,1\n");

    // However the chunks are dealt out, the failure returned is the one at the
    // lowest byte offset, exactly as the serial parser and the ordered path
    // report it.
    let want = serial_failure(&document, ParseOptions::new());
    for count in [1, 2, 3, 8, 16] {
        let failure = eager(count)
            .for_each_record::<_, Error>(&document, |_| Ok(()))
            .expect_err("a malformed document");
        assert_eq!(failure.location(), want.location(), "with {count} threads");
        assert!(failure.location().byte >= early);
    }
}

#[test]
fn for_each_record_stops_and_returns_a_consumer_error_unchanged() {
    let document = hostile_document(20_000);
    let seen = AtomicUsize::new(0);
    let failure = eager(4)
        .for_each_record::<_, Enough>(&document, |_| {
            if seen.fetch_add(1, Ordering::Relaxed) > 100 {
                return Err(Enough::Consumer);
            }
            Ok(())
        })
        .expect_err("the consumer's error");
    assert_eq!(failure, Enough::Consumer);
    // A refusing consumer fences off the later chunks, so the whole document is
    // never parsed.
    assert!(seen.load(Ordering::Relaxed) < 20_000);
}

#[test]
fn for_each_record_rejects_a_format_it_cannot_split() {
    let document = hostile_document(10);
    let failure =
        ParallelParser::with_options(FormatOptions::CSV.comment(Some(b'#')), ParseOptions::new())
            .for_each_record::<_, Error>(&document, |_| Ok(()))
            .expect_err("an unsplittable format");
    assert!(
        failure
            .to_string()
            .contains("parallel parsing cannot split"),
        "unexpected error: {failure}"
    );
}

#[test]
fn for_each_record_handles_short_and_empty_documents() {
    for document in [
        &b""[..],
        b"a,b\n",
        b"a,b\nc,d\n",
        b"a,b\nc,d",
        b"\"a\nb\",c\n1,2\n",
    ] {
        let want = snapshots(&serial(document, FormatOptions::CSV, ParseOptions::new()));
        let got = borrowed_in_order(&eager(8), document);
        assert_eq!(got, want);
    }
}

/// The total number of fields a serial parse of `document` sees.
///
/// This is the reduction the `fold` tests reproduce in parallel: summing a
/// per-record quantity is the workload `fold` exists to make scale, and the
/// serial total is the answer every thread count has to agree on.
fn serial_field_count(document: &[u8]) -> usize {
    serial(document, FormatOptions::CSV, ParseOptions::new())
        .iter()
        .map(ByteRecord::len)
        .sum()
}

#[test]
fn fold_sums_a_column_across_threads() {
    let mut document = Vec::from(&b"n\n"[..]);
    let mut expected = 0_u64;
    for value in 0..20_000_u64 {
        document.extend_from_slice(format!("{value}\n").as_bytes());
        expected += value;
    }

    // Each worker sums into its own u64; combining the per-worker sums must
    // reproduce the serial total on every thread count, with no shared atomic
    // in the hot loop.
    for count in [1, 2, 4, 8] {
        let subtotals = eager(count)
            .fold::<u64, _, _, Error>(
                &document,
                || 0,
                |sum, record| {
                    *sum += record.parse::<u64>(0)?.unwrap_or(0);
                    Ok(())
                },
            )
            .expect("a well-formed document");
        assert_eq!(
            subtotals.into_iter().sum::<u64>(),
            expected,
            "with {count} threads"
        );
    }
}

#[test]
fn fold_agrees_with_a_serial_parse() {
    for (format, document) in [
        (FormatOptions::CSV, hostile_document(5_000)),
        (
            FormatOptions::CSV.record_ending(RecordEnding::CrLf),
            hostile_crlf_document(5_000),
        ),
    ] {
        let want: usize = serial(&document, format, ParseOptions::new())
            .iter()
            .map(ByteRecord::len)
            .sum();

        // The threaded fold and its serial fallback must both reproduce the
        // serial reduction over a document whose quoted fields resist a naive
        // split.
        for count in 1..=17 {
            let got: usize = ParallelParser::with_options(format, ParseOptions::new())
                .threads(threads(count))
                .parallel_threshold(0)
                .fold::<usize, _, _, Error>(
                    &document,
                    || 0,
                    |fields, record| {
                        *fields += record.len();
                        Ok(())
                    },
                )
                .expect("a well-formed document")
                .into_iter()
                .sum();
            assert_eq!(got, want, "with {count} threads");
        }
    }
}

#[test]
fn fold_below_the_threshold_folds_on_the_calling_thread() {
    let document = hostile_document(200);
    let want = serial_field_count(&document);

    // Under the default threshold the reduction runs on the calling thread, so
    // it comes back as a single accumulator rather than one per worker.
    let subtotals = ParallelParser::<Csv>::new(ParseOptions::new())
        .threads(threads(8))
        .fold::<usize, _, _, Error>(
            &document,
            || 0,
            |fields, record| {
                *fields += record.len();
                Ok(())
            },
        )
        .expect("a well-formed document");
    assert_eq!(
        subtotals.len(),
        1,
        "the serial fallback folds into one accumulator"
    );
    assert_eq!(subtotals.into_iter().sum::<usize>(), want);
}

#[test]
fn fold_returns_one_accumulator_per_worker() {
    let document = hostile_document(5_000);
    let want = serial_field_count(&document);

    // A document this size splits into more chunks than threads, so every
    // requested worker gets one and hands back its own accumulator.
    for count in [2, 4, 8] {
        let subtotals = eager(count)
            .fold::<usize, _, _, Error>(
                &document,
                || 0,
                |fields, record| {
                    *fields += record.len();
                    Ok(())
                },
            )
            .expect("a well-formed document");
        assert_eq!(subtotals.len(), count, "with {count} threads");
        assert_eq!(
            subtotals.into_iter().sum::<usize>(),
            want,
            "with {count} threads"
        );
    }
}

#[test]
fn fold_reports_the_first_failure_in_the_document() {
    let mut document = hostile_document(4_000);
    let early = document.len();
    document.extend_from_slice(b"9,\"unterminated,x,1\n");
    document.extend_from_slice(&hostile_document(4_000)[19..]);
    document.extend_from_slice(b"9,\"another one,x,1\n");

    // The reduction reports the same lowest-offset failure the serial parser
    // and the ordered path do, whichever worker reached it.
    let want = serial_failure(&document, ParseOptions::new());
    for count in [1, 2, 3, 8, 16] {
        let failure = eager(count)
            .fold::<usize, _, _, Error>(&document, || 0, |_, _| Ok(()))
            .expect_err("a malformed document");
        assert_eq!(failure.location(), want.location(), "with {count} threads");
        assert!(failure.location().byte >= early);
    }
}

#[test]
fn fold_stops_and_returns_a_consumer_error_unchanged() {
    let document = hostile_document(20_000);
    let seen = AtomicUsize::new(0);
    let failure = eager(4)
        .fold::<usize, _, _, Enough>(
            &document,
            || 0,
            |_, _| {
                if seen.fetch_add(1, Ordering::Relaxed) > 100 {
                    return Err(Enough::Consumer);
                }
                Ok(())
            },
        )
        .expect_err("the consumer's error");
    assert_eq!(failure, Enough::Consumer);
    // A refusing fold fences off the later chunks, so the whole document is
    // never parsed.
    assert!(seen.load(Ordering::Relaxed) < 20_000);
}

#[test]
fn fold_rejects_a_format_it_cannot_split() {
    let document = hostile_document(10);
    let failure =
        ParallelParser::with_options(FormatOptions::CSV.comment(Some(b'#')), ParseOptions::new())
            .fold::<usize, _, _, Error>(&document, || 0, |_, _| Ok(()))
            .expect_err("an unsplittable format");
    assert!(
        failure
            .to_string()
            .contains("parallel parsing cannot split"),
        "unexpected error: {failure}"
    );
}

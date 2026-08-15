//! Where threads start paying for themselves, and by how much.
//!
//! This is a wall-clock suite, and deliberately so. Every other benchmark in
//! this crate counts instructions under Callgrind, because instruction counts
//! are deterministic and resolve a 2% regression. They cannot answer this
//! question at all: parsing on four threads executes *more* instructions than
//! parsing on one, and the whole point is that it finishes sooner. Elapsed time
//! is the only thing that measures a speedup, so this file measures elapsed
//! time and stays out of the optimization suite.
//!
//! It reports the one number a caller needs -- the document size above which a
//! [`ParallelParser`] beats a [`SliceParser`] on the same bytes -- for each of
//! the two paths, against the serial parse it has to beat:
//!
//! * `fold` reduces borrowed records on the workers, each into its own
//!   accumulator, so its honest baseline is a serial *borrowed* reduction
//!   (`serial/borrowed`) doing the same sum on one thread. This is the selected
//!   path, and the default [threshold](ParallelParser::parallel_threshold) is
//!   sited from it.
//! * `for_each_batch` hands back owned records in order, so its baseline is a
//!   serial *owned* parse (`serial/owned`).
//!
//! The reduction is deliberately a real one -- summing a field's length -- and
//! `fold` gives each worker its own accumulator, so what is measured is the
//! parser scaling and not a row of workers contending on one atomic. A
//! reduction written on [`ParallelParser::for_each_record`] with a single
//! shared counter measures the cache line, not the parser, and would report no
//! crossover where there is a large one; `fold` is what this suite uses to
//! avoid exactly that trap.
//!
//! Each path is measured on two, four, eight, and the default (`auto`, one per
//! core) threads, so the shape of the scaling is visible rather than a single
//! point, over sizes that bracket the crossover. The ordered owned path also
//! sweeps 32, 256, 4096, and 16384 records per batch on `auto`; this attributes
//! its coordination cost without multiplying the already-expensive full
//! thread sweep. The threshold is forced to zero throughout, so the fallback
//! does not silently answer the question this suite is asking.
//!
//! Read the crossover off the output by finding, for a path, the smallest size
//! at which its `auto` row beats the matching `serial` baseline by the margin
//! wanted *and holds it across runs*; that size is where the default threshold
//! belongs. On the shared reference host that size is thirty-two mebibytes --
//! sixteen crosses on an idle machine but its margin is too thin to trust under
//! load -- which is what the default
//! [threshold](ParallelParser::parallel_threshold) is set from.
//!
//! Run it with `cargo bench -p coseva --features parallel --bench parallel`,//! and on an otherwise idle machine: unlike the Callgrind suites, this one
//! measures a shared resource and a loaded machine will not answer honestly. A
//! focused run is enough to site the threshold, e.g. append
//! `-- --warm-up-time 1 --measurement-time 3 --sample-size 20`. The pinned-host
//! baseline and 64 MiB gate are managed by
//! `benchmarks/parallel/run.py`; results from a different environment are
//! explicitly non-comparable.
//!
//! The `skew` group asks a different question with the same machinery: what a
//! static chunk-to-worker rotation costs when the chunks are not equally
//! expensive. It runs both paths over a document whose parse cost is
//! concentrated unevenly, at two, four and eight threads, each against the
//! serial parse of the same bytes. Read it as a comparison between the two
//! paths on one input, not as a throughput figure.
//!
//! Two things this suite cannot say are measured elsewhere. The gate is not
//! only that absolute throughput: `run.py` also derives a *speedup* floor from
//! the parallel and serial cases of the same run, which is a ratio and so
//! holds on any host, and it is the borrowed floor that blocks. And a median
//! throughput cannot show head-of-line blocking in the ordered drain at all —
//! `scripts/perf_parallel_tail.rs` measures the gap between consecutive batch
//! deliveries and reports its percentiles.

use std::hint::black_box;
use std::num::NonZeroUsize;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::parallel::ParallelParser;
use coseva::{ByteRecord, SliceParser};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// Narrow numeric rows, the shape single-threaded parsing is fastest on and so
/// the hardest case for threads to beat.
fn document(bytes: usize) -> Vec<u8> {
    let mut document = Vec::with_capacity(bytes + 64);
    let mut index = 0_u64;
    while document.len() < bytes {
        document.extend_from_slice(
            format!("{index},{},{},{}\n", index * 3, index % 97, index % 1013).as_bytes(),
        );
        index += 1;
    }
    document
}

/// The same document, but with its parse cost concentrated unevenly.
///
/// Thirty-two regions of roughly equal byte length, each either cheap (the
/// narrow numeric rows [`document`] uses) or expensive (long quoted fields
/// carrying doubled quotes, which force the escape-aware kernel and cost far
/// more per byte). The pattern is a fixed pseudo-random one rather than a
/// stripe, so no thread count is accidentally balanced by it: the point is that
/// a static chunk-to-worker rotation can hand one worker several expensive
/// regions while another gets none, where claiming from a shared cursor cannot.
///
/// Thirty-two is chosen against `CHUNKS_PER_THREAD`, which is 16: two threads
/// split this document into 32 chunks and so see one region each, and eight
/// threads into 128, four chunks to a region. Either way a region is coarse
/// enough that a chunk inherits its cost rather than averaging over many.
fn skewed_document(bytes: usize) -> Vec<u8> {
    // A fixed 32-bit pattern, roughly half set. Region `r` is expensive when
    // bit `r` is set.
    const PATTERN: u32 = 0b1001_0110_0011_1000_1101_0010_0100_1110;
    const REGIONS: usize = 32;

    let region = bytes / REGIONS;
    let mut out = Vec::with_capacity(bytes + 64);
    let mut index = 0_u64;
    for slot in 0..REGIONS {
        let target = out.len() + region;
        if PATTERN & (1 << slot) == 0 {
            while out.len() < target {
                out.extend_from_slice(
                    format!("{index},{},{},{}\n", index * 3, index % 97, index % 1013).as_bytes(),
                );
                index += 1;
            }
        } else {
            while out.len() < target {
                out.extend_from_slice(
                    format!(
                        "\"say \"\"hello\"\", {index}\",\"and, again\",\"{}\"\n",
                        index % 1013
                    )
                    .as_bytes(),
                );
                index += 1;
            }
        }
    }
    out
}

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

/// Read every record serially into a reused owned record, the baseline the
/// owned parallel path has to beat.
fn serial_owned(input: &[u8]) -> usize {
    let mut parser = SliceParser::<Csv>::new(input, options()).expect("valid configuration");
    let mut record = ByteRecord::new();
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("a well-formed document") {
        line.read_byte_record_into(&mut record)
            .expect("a well-formed record");
        fields += record.len();
    }
    fields
}

/// Reduce every record serially as a borrowed view, the baseline the borrowed
/// `fold` path has to beat: the same sum, on one thread, into one accumulator.
fn serial_borrowed(input: &[u8]) -> usize {
    let mut parser = SliceParser::<Csv>::new(input, options()).expect("valid configuration");
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("a well-formed document") {
        fields += line.record().expect("a well-formed record").len();
    }
    fields
}

/// Owned parallel path: batches of owned records, drained in order.
fn parallel_owned(parser: &ParallelParser<Csv>, input: &[u8]) -> usize {
    let mut fields = 0;
    parser
        .for_each_batch::<_, coseva::Error>(input, |batch| {
            // Reading the records in place rather than taking them out leaves
            // them to be recycled, which is what keeps this allocation-free in
            // the steady state.
            for record in batch.iter() {
                fields += record.len();
            }
            Ok(())
        })
        .expect("a well-formed document");
    fields
}

/// Borrowed parallel path: each worker sums into its own accumulator, no copy,
/// no queue, no shared counter; the per-worker sums are combined at the end.
fn parallel_fold(parser: &ParallelParser<Csv>, input: &[u8]) -> usize {
    parser
        .fold::<usize, _, _, coseva::Error>(
            input,
            || 0,
            |fields, record| {
                *fields += record.len();
                Ok(())
            },
        )
        .expect("a well-formed document")
        .into_iter()
        .sum()
}

/// Parse unconditionally on `count` threads, ignoring the size threshold.
///
/// `None` leaves the default of one worker per core, the `auto` row.
fn parser(count: Option<usize>, batch_records: Option<usize>) -> ParallelParser<Csv> {
    let parser = ParallelParser::<Csv>::new(options()).parallel_threshold(0);
    let parser = match count {
        Some(count) => parser.threads(NonZeroUsize::new(count).expect("a positive thread count")),
        None => parser,
    };
    match batch_records {
        Some(records) => parser.batch_records(
            NonZeroUsize::new(records).expect("a positive number of records per batch"),
        ),
        None => parser,
    }
}

/// The thread counts every size is measured across: two, four, eight, and one
/// per core.
const THREAD_COUNTS: [Option<usize>; 4] = [Some(2), Some(4), Some(8), None];

/// The batch-size sweep for the ordered owned path. The default 4096-record
/// point is already measured by the full thread sweep.
const BATCH_RECORD_COUNTS: [usize; 4] = [32, 256, 4096, 16_384];
const DEFAULT_BATCH_RECORDS: usize = 4096;

/// Fail loudly before timing anything if a path disagrees with the serial parse.
///
/// A wall-clock suite cannot assert on a duration, but it can assert that every
/// path it is about to time still produces the records a serial parse would, so
/// a benchmark can never quietly measure a broken parser.
fn verify(input: &[u8]) {
    let owned = serial_owned(input);
    assert_eq!(serial_borrowed(input), owned, "serial paths disagree");
    for count in THREAD_COUNTS {
        let parser = parser(count, None);
        assert_eq!(parallel_owned(&parser, input), owned, "owned {count:?}");
        assert_eq!(parallel_fold(&parser, input), owned, "fold {count:?}");
    }
    for batch_records in BATCH_RECORD_COUNTS {
        let parser = parser(None, Some(batch_records));
        assert_eq!(
            parallel_owned(&parser, input),
            owned,
            "owned batch {batch_records}"
        );
    }
}

/// What a static chunk-to-worker rotation costs on an uneven document.
///
/// The owned path deals chunk `i` to worker `i % threads`, decided before any
/// work starts, while the borrowed path lets workers claim from a shared
/// cursor. On [`document`] every chunk costs the same and the two rules are
/// indistinguishable. This group runs both paths over [`skewed_document`],
/// where they are not, and reports each against the serial parse of the same
/// bytes — so the rotation's cost is the gap between the owned path's scaling
/// here and the borrowed path's, both measured on the same input.
fn skew(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("skew");

    // One size, well above the threshold, because this group is asking about
    // balance rather than about where threads start paying.
    for mib in [32_usize] {
        let size = format!("{mib}MiB");
        let input = skewed_document(mib << 20);
        verify(&input);
        group.throughput(Throughput::Bytes(input.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("serial/owned", &size),
            &input,
            |bencher, input| bencher.iter(|| black_box(serial_owned(input))),
        );
        group.bench_with_input(
            BenchmarkId::new("serial/borrowed", &size),
            &input,
            |bencher, input| bencher.iter(|| black_box(serial_borrowed(input))),
        );

        for count in [Some(2), Some(4), Some(8)] {
            let parser = parser(count, None);
            let label = count.map_or_else(|| "auto".to_owned(), |count| count.to_string());
            group.bench_with_input(
                BenchmarkId::new(format!("fold/threads-{label}"), &size),
                &input,
                |bencher, input| bencher.iter(|| black_box(parallel_fold(&parser, input))),
            );
            group.bench_with_input(
                BenchmarkId::new(format!("owned/threads-{label}"), &size),
                &input,
                |bencher, input| bencher.iter(|| black_box(parallel_owned(&parser, input))),
            );
        }
    }

    group.finish();
}

fn throughput_floor(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("parallel");

    // Crossover region: eight mebibytes is below the threshold and sixty-four
    // well above it, so these four sizes bracket the thirty-two-mebibyte answer
    // without a long exhaustive sweep.
    for mib in [8_usize, 16, 32, 64] {
        let size = format!("{mib}MiB");
        let input = document(mib << 20);
        verify(&input);
        group.throughput(Throughput::Bytes(input.len() as u64));

        group.bench_with_input(
            BenchmarkId::new("serial/owned", &size),
            &input,
            |bencher, input| bencher.iter(|| black_box(serial_owned(input))),
        );
        group.bench_with_input(
            BenchmarkId::new("serial/borrowed", &size),
            &input,
            |bencher, input| bencher.iter(|| black_box(serial_borrowed(input))),
        );

        // Two, four, eight, and one-per-core, for each path, so the scaling and
        // not just a single thread count is on display.
        for count in THREAD_COUNTS {
            let parser = parser(count, None);
            let label = count.map_or_else(|| "auto".to_owned(), |count| count.to_string());

            group.bench_with_input(
                BenchmarkId::new(format!("fold/threads-{label}"), &size),
                &input,
                |bencher, input| bencher.iter(|| black_box(parallel_fold(&parser, input))),
            );
            group.bench_with_input(
                BenchmarkId::new(format!("owned/threads-{label}"), &size),
                &input,
                |bencher, input| bencher.iter(|| black_box(parallel_owned(&parser, input))),
            );
        }

        // Batch-size attribution uses automatic thread selection. The default
        // 4096-record point above is reused rather than timed twice.
        for batch_records in BATCH_RECORD_COUNTS {
            if batch_records == DEFAULT_BATCH_RECORDS {
                continue;
            }
            let parser = parser(None, Some(batch_records));
            group.bench_with_input(
                BenchmarkId::new(format!("owned/batch-{batch_records}/threads-auto"), &size),
                &input,
                |bencher, input| bencher.iter(|| black_box(parallel_owned(&parser, input))),
            );
        }
    }

    group.finish();
}

criterion_group!(benches, throughput_floor, skew);
criterion_main!(benches);

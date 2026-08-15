//! Wall-clock scaling for in-memory index construction.
//!
//! The index Callgrind suite reports deterministic instruction counts for
//! [`CsvIndex::build`], but that cannot answer whether a `parallel`+`index`
//! build finishes sooner than the serial `index` build: threads may execute
//! more instructions and still win elapsed time. This suite therefore measures
//! elapsed time and compares only ratios from matched documents on the same
//! otherwise-idle host.
//!
//! `CsvIndex::build` picks a builder by document size, so timing it alone
//! measures whichever side of `PARALLEL_INDEX_THRESHOLD_BYTES` the input fell
//! on and never both. The two cases here therefore go through
//! [`coseva::benchmark::build_index_serial`] and
//! [`coseva::benchmark::build_index_parallel`], which force one builder each,
//! and every figure the harness gates is a ratio between two cases measured in
//! the same run on the same document. Absolute throughput here means nothing
//! off the recorded host; the ratio means the same thing everywhere.
//!
//! The sizes are the ones
//! `PARALLEL_INDEX_THRESHOLD_BYTES`'s documented sweep names: 2 and 4 MiB
//! below the threshold, where the serial builder is expected to win, and 8, 32
//! and 64 MiB above it, where the parallel one is. `benchmarks/index/run.py`
//! turns those into a speedup floor.

use std::hint::black_box;
use std::thread::available_parallelism;

use coseva::benchmark::{build_index_parallel, build_index_serial};
use coseva::index::{CsvIndex, IndexOptions};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

/// Sizes either side of the documented parallel-index threshold.
const SIZES_MIB: [usize; 5] = [2, 4, 8, 32, 64];

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

fn threads() -> usize {
    available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// Confirm both builders agree before either is timed.
///
/// A speedup over a builder that produced a different index would be
/// meaningless, and the two paths reach their tables by entirely different
/// routes.
fn verify(input: &[u8]) {
    let serial =
        build_index_serial(input, IndexOptions::default()).expect("a well-formed document");
    let parallel = build_index_parallel(input, IndexOptions::default(), threads())
        .expect("a well-formed document");

    assert!(!serial.is_empty(), "the generated document is non-empty");
    assert_eq!(
        serial.len(),
        parallel.len(),
        "both builders find the same number of records"
    );
    for record in [0, 1, serial.len() / 2, serial.len() - 1] {
        assert_eq!(
            serial.record_offset(record),
            parallel.record_offset(record),
            "both builders put record {record} at the same offset"
        );
        assert_eq!(
            serial.record_line(record),
            parallel.record_line(record),
            "both builders put record {record} on the same line"
        );
    }
    assert_eq!(
        serial.record_offset(0),
        Some(0),
        "the first record starts at byte zero"
    );
    assert_eq!(
        serial.record_line(0),
        Some(1),
        "the first record starts on line one"
    );
}

fn throughput_floor(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("index_build_wallclock");
    let threads = threads();

    for mib in SIZES_MIB {
        let size = format!("{mib}MiB");
        let input = document(mib << 20);
        verify(&input);
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("serial", &size),
            &input,
            |bencher, input| {
                bencher.iter(|| {
                    black_box(
                        build_index_serial(input, IndexOptions::default())
                            .expect("a well-formed document")
                            .len(),
                    )
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("parallel/threads-auto", &size),
            &input,
            |bencher, input| {
                bencher.iter(|| {
                    black_box(
                        build_index_parallel(input, IndexOptions::default(), threads)
                            .expect("a well-formed document")
                            .len(),
                    )
                });
            },
        );
        group.bench_with_input(
            BenchmarkId::new("dispatched", &size),
            &input,
            |bencher, input| {
                bencher.iter(|| {
                    black_box(
                        CsvIndex::build(input, IndexOptions::default())
                            .expect("a well-formed document")
                            .len(),
                    )
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, throughput_floor);
criterion_main!(benches);

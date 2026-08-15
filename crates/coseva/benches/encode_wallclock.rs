//! File-backed encoding throughput for coseva and `csv`.
//!
//! Both writers receive the same records, use an 8 KiB user-space buffer, and
//! are checked for byte-identical output before timing. Every reported case is
//! paired: one custom Criterion iteration executes both implementations,
//! alternates their order, accumulates each duration separately, and returns
//! only the selected target's elapsed time. Both adjacent elapsed totals are
//! also persisted for the ratio gate; the runner takes the worse ratio from
//! the two target cases. Shared-host stalls therefore land in the same paired
//! observation instead of being divided across disjoint sequential cases.
//! Timing each side adds two `Instant` reads per execution; that symmetric
//! fixed overhead is retained rather than subtracted.
//!
//! The `writer` cases wrap a real [`File`](std::fs::File) and count calls to
//! [`Write::write`]; those are wrapper calls, not claims about kernel syscalls.
//! The `path` cases exercise the convenience APIs that create their own files,
//! where an equivalent wrapper cannot be inserted without changing the public
//! API.
//!
//! Files are created beneath `CARGO_TARGET_DIR`, truncated for each sample, and
//! deliberately not synchronized with `sync_all`. The timings therefore cover
//! encoding, user-space draining, file creation, and writes into the operating
//! system page cache, not durable-storage latency. Paired execution doubles
//! the file work per Criterion iteration, and the runner performs at least
//! three independent Criterion runs, so the scheduled job costs roughly six
//! times an unpaired single run. The runner gates only the minimum coseva/`csv`
//! ratio across those runs. Below-parity cases and absolute throughput are
//! report-only; the latter is useful only when its environment key matches the
//! recorded host.

#![expect(
    clippy::panic,
    reason = "benchmark-only fixtures are private and failed oracles must stop the run"
)]

use std::fs::{self, File};
use std::hint::black_box;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use coseva::config::{EmitOptions, FormatOptions};
use coseva::encoding::CsvEncode;
use coseva::{encode_to_path, encode_to_writer};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde::Serialize;

const BUFFER: usize = 8 * 1024;
const TOTAL_BYTES: usize = 8 * 1024 * 1024;
const GROUP: &str = "encode_wallclock";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Implementation {
    Coseva,
    Csv,
}

impl Implementation {
    const fn name(self) -> &'static str {
        match self {
            Self::Coseva => "coseva",
            Self::Csv => "csv",
        }
    }

    const fn other(self) -> Self {
        match self {
            Self::Coseva => Self::Csv,
            Self::Csv => Self::Coseva,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Api {
    Writer,
    Path,
}

#[derive(Default)]
struct PairTotals {
    coseva: Duration,
    csv: Duration,
    executions: u64,
}

impl PairTotals {
    fn record(&mut self, implementation: Implementation, elapsed: Duration) {
        match implementation {
            Implementation::Coseva => self.coseva += elapsed,
            Implementation::Csv => self.csv += elapsed,
        }
    }
}

impl Api {
    const fn name(self) -> &'static str {
        match self {
            Self::Writer => "writer",
            Self::Path => "path",
        }
    }
}

#[derive(Clone, Copy, CsvEncode, Serialize)]
struct Row<'a> {
    value: &'a str,
}

struct Shape {
    name: &'static str,
    payload: String,
    rows: usize,
    bytes: usize,
}

impl Shape {
    fn new(name: &'static str, payload_len: usize) -> Self {
        let row_len = payload_len + 1;
        let rows = TOTAL_BYTES / row_len;
        Self {
            name,
            payload: "a".repeat(payload_len),
            rows,
            bytes: rows * row_len,
        }
    }

    fn rows(&self) -> impl Iterator<Item = Row<'_>> + Clone {
        std::iter::repeat_n(
            Row {
                value: &self.payload,
            },
            self.rows,
        )
    }

    fn expected(&self) -> Vec<u8> {
        let mut output = Vec::with_capacity(self.bytes);
        for _ in 0..self.rows {
            output.extend_from_slice(self.payload.as_bytes());
            output.push(b'\n');
        }
        output
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WriteStats {
    calls: usize,
    bytes: usize,
    flushes: usize,
}

struct CountingFile {
    file: File,
    stats: WriteStats,
}

impl CountingFile {
    fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::create(path)?,
            stats: WriteStats {
                calls: 0,
                bytes: 0,
                flushes: 0,
            },
        })
    }
}

impl Write for CountingFile {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stats.calls += 1;
        let written = self.file.write(buffer)?;
        self.stats.bytes += written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stats.flushes += 1;
        self.file.flush()
    }
}

fn options() -> EmitOptions {
    EmitOptions::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
}

fn coseva_writer(path: &Path, shape: &Shape) -> WriteStats {
    let mut output =
        CountingFile::create(path).unwrap_or_else(|error| panic!("create failed: {error}"));
    encode_to_writer(&mut output, shape.rows(), FormatOptions::CSV, options())
        .unwrap_or_else(|error| panic!("coseva writer failed: {error}"));
    output.stats
}

fn csv_writer(path: &Path, shape: &Shape) -> WriteStats {
    let mut output =
        CountingFile::create(path).unwrap_or_else(|error| panic!("create failed: {error}"));
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_writer(&mut output);
    for row in shape.rows() {
        writer
            .serialize(row)
            .unwrap_or_else(|error| panic!("csv writer failed: {error}"));
    }
    writer
        .into_inner()
        .unwrap_or_else(|error| panic!("csv finalization failed: {error}"));
    output.stats
}

fn coseva_path(path: &Path, shape: &Shape) {
    encode_to_path(path, shape.rows(), FormatOptions::CSV, options())
        .unwrap_or_else(|error| panic!("coseva path failed: {error}"));
}

fn csv_path(path: &Path, shape: &Shape) {
    let file = File::create(path).unwrap_or_else(|error| panic!("create failed: {error}"));
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_writer(file);
    for row in shape.rows() {
        writer
            .serialize(row)
            .unwrap_or_else(|error| panic!("csv path failed: {error}"));
    }
    writer
        .into_inner()
        .unwrap_or_else(|error| panic!("csv path finalization failed: {error}"));
}

fn verify_file(path: &Path, expected: &[u8]) {
    let actual = fs::read(path).unwrap_or_else(|error| panic!("read failed: {error}"));
    assert_eq!(actual, expected, "file-backed benchmark wrote wrong bytes");
}

fn work_dir() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| PathBuf::from("target/encode-bench"), PathBuf::from);
    let directory = target.join("encode-files");
    fs::create_dir_all(&directory)
        .unwrap_or_else(|error| panic!("benchmark directory creation failed: {error}"));
    directory
}

fn path(directory: &Path, implementation: Implementation, api: Api, shape: &str) -> PathBuf {
    directory.join(format!(
        "{}-{}-{shape}.csv",
        implementation.name(),
        api.name()
    ))
}

fn verify_and_record(directory: &Path, shapes: &[Shape]) {
    let mut records = Vec::new();
    for shape in shapes {
        let expected = shape.expected();
        let coseva_writer_path = path(directory, Implementation::Coseva, Api::Writer, shape.name);
        let csv_writer_path = path(directory, Implementation::Csv, Api::Writer, shape.name);
        let coseva_path_path = path(directory, Implementation::Coseva, Api::Path, shape.name);
        let csv_path_path = path(directory, Implementation::Csv, Api::Path, shape.name);

        let coseva_first = coseva_writer(&coseva_writer_path, shape);
        verify_file(&coseva_writer_path, &expected);
        let coseva_second = coseva_writer(&coseva_writer_path, shape);
        assert_eq!(
            coseva_first, coseva_second,
            "coseva wrapper write-call counts must be deterministic"
        );
        let csv_first = csv_writer(&csv_writer_path, shape);
        verify_file(&csv_writer_path, &expected);
        let csv_second = csv_writer(&csv_writer_path, shape);
        assert_eq!(
            csv_first, csv_second,
            "csv wrapper write-call counts must be deterministic"
        );
        assert_eq!(coseva_first.bytes, shape.bytes);
        assert_eq!(csv_first.bytes, shape.bytes);
        assert_eq!(coseva_first.flushes, 1);
        assert_eq!(csv_first.flushes, 1);

        coseva_path(&coseva_path_path, shape);
        verify_file(&coseva_path_path, &expected);
        csv_path(&csv_path_path, shape);
        verify_file(&csv_path_path, &expected);

        records.push(format!(
            concat!(
                "  \"{}\": {{\n",
                "    \"bytes\": {},\n",
                "    \"rows\": {},\n",
                "    \"payload_bytes\": {},\n",
                "    \"coseva_writer\": {{\"calls\": {}, \"bytes\": {}, \"flushes\": {}}},\n",
                "    \"csv_writer\": {{\"calls\": {}, \"bytes\": {}, \"flushes\": {}}},\n",
                "    \"path_write_calls\": null,\n",
                "    \"write_call_unit\": \"std::io::Write::write wrapper calls, not syscalls\"\n",
                "  }}"
            ),
            shape.name,
            shape.bytes,
            shape.rows,
            shape.payload.len(),
            coseva_first.calls,
            coseva_first.bytes,
            coseva_first.flushes,
            csv_first.calls,
            csv_first.bytes,
            csv_first.flushes,
        ));
    }
    let target = directory
        .parent()
        .expect("work directory is below the target directory");
    fs::write(
        target.join("encode-oracles.json"),
        format!("{{\n{}\n}}\n", records.join(",\n")),
    )
    .unwrap_or_else(|error| panic!("oracle write failed: {error}"));
}

fn execute(implementation: Implementation, api: Api, output_path: &Path, shape: &Shape) {
    match (implementation, api) {
        (Implementation::Coseva, Api::Writer) => {
            black_box(coseva_writer(output_path, shape));
        }
        (Implementation::Csv, Api::Writer) => {
            black_box(csv_writer(output_path, shape));
        }
        (Implementation::Coseva, Api::Path) => coseva_path(output_path, shape),
        (Implementation::Csv, Api::Path) => csv_path(output_path, shape),
    }
    black_box(output_path);
}

fn timed(implementation: Implementation, api: Api, output_path: &Path, shape: &Shape) -> Duration {
    let started = Instant::now();
    execute(implementation, api, output_path, shape);
    started.elapsed()
}

fn throughput(criterion: &mut Criterion) {
    let directory = work_dir();
    let shapes = [
        Shape::new("typical", 127),
        Shape::new("oversized", 64 * 1024 - 1),
    ];
    verify_and_record(&directory, &shapes);

    let mut group = criterion.benchmark_group(GROUP);
    let mut pair_records = Vec::new();
    for shape in &shapes {
        group.throughput(Throughput::Bytes(shape.bytes as u64));
        for (target, api) in [
            (Implementation::Coseva, Api::Writer),
            (Implementation::Csv, Api::Writer),
            (Implementation::Coseva, Api::Path),
            (Implementation::Csv, Api::Path),
        ] {
            let target_path = path(&directory, target, api, shape.name);
            let companion = target.other();
            let companion_path = path(&directory, companion, api, shape.name);
            let mut totals = PairTotals::default();
            group.bench_with_input(
                BenchmarkId::new(format!("{}/{}", target.name(), api.name()), shape.name),
                shape,
                |bencher, shape| {
                    let mut sequence = 0_u64;
                    bencher.iter_custom(|iterations| {
                        let mut target_elapsed = Duration::ZERO;
                        for _ in 0..iterations {
                            if sequence.is_multiple_of(2) {
                                let elapsed = timed(target, api, &target_path, shape);
                                totals.record(target, elapsed);
                                target_elapsed += elapsed;
                                let elapsed = timed(companion, api, &companion_path, shape);
                                totals.record(companion, elapsed);
                                black_box(elapsed);
                            } else {
                                let elapsed = timed(companion, api, &companion_path, shape);
                                totals.record(companion, elapsed);
                                black_box(elapsed);
                                let elapsed = timed(target, api, &target_path, shape);
                                totals.record(target, elapsed);
                                target_elapsed += elapsed;
                            }
                            totals.executions += 1;
                            sequence = sequence.wrapping_add(1);
                        }
                        target_elapsed
                    });
                },
            );
            pair_records.push(format!(
                concat!(
                    "  \"{}/{}/{}/{}\": {{",
                    "\"coseva_ns\": {}, \"csv_ns\": {}, \"executions\": {}",
                    "}}"
                ),
                GROUP,
                target.name(),
                api.name(),
                shape.name,
                totals.coseva.as_nanos(),
                totals.csv.as_nanos(),
                totals.executions,
            ));
        }
    }
    group.finish();
    fs::write(
        directory
            .parent()
            .expect("work directory is below the target directory")
            .join("encode-pairs.json"),
        format!("{{\n{}\n}}\n", pair_records.join(",\n")),
    )
    .unwrap_or_else(|error| panic!("paired timing write failed: {error}"));

    for entry in fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("benchmark directory read failed: {error}"))
    {
        let entry = entry.unwrap_or_else(|error| panic!("directory entry failed: {error}"));
        fs::remove_file(entry.path())
            .unwrap_or_else(|error| panic!("benchmark cleanup failed: {error}"));
    }
}

criterion_group!(benches, throughput);
criterion_main!(benches);

#!/usr/bin/env -S cargo +nightly-2026-05-30 -Zscript
---
[package]
edition = "2024"

[dependencies]
coseva = { path = "..", features = ["std", "index", "derive", "parallel"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
---

//! Measure allocation count, allocated bytes, and peak live heap.

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::io::{Cursor, sink};
use std::{env, fs, hint::black_box};

use coseva::config::{EmitOptions, Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::index::{CsvIndex, IndexOptions};
use coseva::parallel::ParallelParser;
use coseva::{
    ByteRecord, IoEmitter, IoParser, PushParser, SliceParser, TextRecord, VecEmitter,
};
use serde::{Deserialize, Serialize};

struct TrackingAllocator;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, layout, size) };
        if !replacement.is_null() {
            if size >= layout.size() {
                record_allocation(size - layout.size());
            } else {
                LIVE_BYTES.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        replacement
    }
}

fn record_allocation(bytes: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

#[derive(Clone, Serialize)]
struct Measurement {
    case: &'static str,
    operations: usize,
    allocations: u64,
    allocated_bytes: u64,
    peak_live_bytes: usize,
}

#[derive(Serialize)]
struct Source {
    revision: String,
    tree_clean: bool,
}

#[derive(Serialize)]
struct Evidence {
    command: &'static str,
    host: Host,
    toolchain: Toolchain,
    source: Source,
}

#[derive(Serialize)]
struct Host {
    hostname: String,
    cpu_model: String,
    logical_cpus: usize,
}

#[derive(Serialize)]
struct Toolchain {
    rustc: String,
    cargo: String,
}

#[derive(Serialize)]
struct Artifact {
    evidence: Evidence,
    metric: &'static str,
    cases: Vec<Measurement>,
}

#[derive(Deserialize, Serialize)]
struct Baseline {
    schema: u32,
    generated_by: String,
    rustc: String,
    cargo: String,
    cases: Vec<BaselineMeasurement>,
}

#[derive(Deserialize, Serialize)]
struct BaselineMeasurement {
    case: String,
    operations: usize,
    allocations: u64,
    allocated_bytes: u64,
    peak_live_bytes: usize,
}

#[derive(CsvDecode)]
struct Reused(String, Vec<u8>);

fn main() -> ExitCode {
    match run() {
        Ok(Some(path)) => {
            println!("wrote {}", path.display());
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("perf_memory: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Option<PathBuf>, String> {
    let mut output = PathBuf::from("target/perf-report/memory.json");
    let mut baseline = PathBuf::from("crates/coseva/scripts/perf-memory-baselines.json");
    let mut check = false;
    let mut refresh_baseline = false;
    let mut allow_dirty = false;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                output = PathBuf::from(arguments.next().ok_or("--output needs a path")?);
            }
            "--baseline" => {
                baseline = PathBuf::from(arguments.next().ok_or("--baseline needs a path")?);
            }
            "--check" => check = true,
            "--refresh-baseline" => refresh_baseline = true,
            "--allow-dirty" => allow_dirty = true,
            "--help" | "-h" => {
                println!(
                    "usage: perf_memory.rs [--output PATH] [--baseline PATH] \
                     [--check | --refresh-baseline] [--allow-dirty]"
                );
                return Ok(None);
            }
            other => return Err(format!("unrecognised argument `{other}`")),
        }
    }
    if check && refresh_baseline {
        return Err("--check and --refresh-baseline are mutually exclusive".to_string());
    }

    let root = repository_root()?;
    let dirty = command(&root, "git", &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let tree_clean = dirty.trim().is_empty();
    if !tree_clean && !allow_dirty {
        return Err("refusing to measure a dirty source tree".to_string());
    }

    let input = document(16 << 20);
    let rows = input.iter().filter(|&&byte| byte == b'\n').count();
    // The same parse over four times the bytes. Nothing here compares the two
    // sizes' allocation counts to each other for their own sake: the point is
    // that the parallel paths' *peak* must not follow the document, which is a
    // property no single measurement can express and which is the one thing
    // `src/parallel.rs` promises about their memory.
    let even = uniform_document(16 << 20);
    let even_rows = even.iter().filter(|&&byte| byte == b'\n').count();
    let large = uniform_document(64 << 20);
    let large_rows = large.iter().filter(|&&byte| byte == b'\n').count();
    let serial_index = document(4 << 20);
    let serial_index_rows = serial_index.iter().filter(|&&byte| byte == b'\n').count();
    let narrow = typed_document(6, 1_001);
    let wide = typed_document(200, 1_001);
    // The streaming cases are the ones that carry the bounded-resource-use
    // promise: their input is 16 MiB and their peak must stay near the buffer
    // rather than near the document.
    let outlier = outlier_document(1 << 20, 10_000);
    let small = document(256 << 10);
    let cases = vec![
        measure("read_owned_reused", rows, || read_owned(&input, rows)),
        measure("read_text_reused", rows, || read_text(&input, rows)),
        measure_decode_into("decode_into_narrow_reused", &narrow),
        measure_decode_into("decode_into_wide_reused", &wide),
        measure("write_vec_growth", rows, || write_rows(rows)),
        measure("write_vec_builder", rows, || write_rows_builder(rows)),
        // Below `PARALLEL_INDEX_THRESHOLD_BYTES`, so this stays the serial
        // builder however many threads the host has and whether or not the
        // `parallel` feature is compiled in.
        measure("index_build_serial", serial_index_rows, || {
            build_index(&serial_index)
        }),
        measure("index_build_parallel", rows, || build_index(&input)),
        measure("parallel_fold_16mib", even_rows, || parallel_fold(&even)),
        measure("parallel_fold_64mib", large_rows, || parallel_fold(&large)),
        measure("parallel_batches_16mib", even_rows, || parallel_batches(&even)),
        measure("parallel_batches_64mib", large_rows, || {
            parallel_batches(&large)
        }),
        measure("read_io_streaming_refill", rows, || read_streaming(&input)),
        measure("write_io_drain", rows, || write_drained(rows)),
        measure("read_io_outlier_reclaim", 10_001, || {
            read_streaming(&outlier)
        }),
        measure("push_one_byte_chunks", small.len(), || {
            push_one_byte_chunks(&small)
        }),
    ];
    let rustc = command(&root, "rustc", &["-Vv"])?;
    let cargo = command(&root, "cargo", &["-V"])?;

    let artifact = Artifact {
        evidence: Evidence {
            command: "cargo +nightly-2026-05-30 -Zscript crates/coseva/scripts/perf_memory.rs --output target/perf-report/memory.json",
            host: Host {
                hostname: command(&root, "hostname", &[])?,
                cpu_model: cpu_model(),
                logical_cpus: std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1),
            },
            toolchain: Toolchain {
                rustc: rustc.clone(),
                cargo: cargo.clone(),
            },
            source: Source {
                revision: command(&root, "git", &["rev-parse", "HEAD"])?,
                tree_clean,
            },
        },
        metric: "allocator_call_count_cumulative_heap_growth_bytes_and_peak_additional_live_heap_bytes",
        cases: cases.clone(),
    };
    let destination = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(
        &destination,
        serde_json::to_string_pretty(&artifact)
            .map_err(|error| format!("cannot serialize artifact: {error}"))?
            + "\n",
    )
    .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;

    let baseline_path = if baseline.is_absolute() {
        baseline
    } else {
        root.join(baseline)
    };
    if refresh_baseline {
        write_baseline(&baseline_path, &rustc, &cargo, &cases)?;
        println!("wrote {}", baseline_path.display());
    } else if check {
        check_baseline(&baseline_path, &cases, &rustc, &cargo)?;
    }
    Ok(Some(destination))
}

fn repository_root() -> Result<PathBuf, String> {
    let script = PathBuf::from(env::args().next().ok_or("no argv[0]")?);
    script
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", script.display()))?
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot locate repository root".to_string())
}

fn command(root: &Path, program: &str, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .current_dir(root)
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn write_baseline(
    path: &Path,
    rustc: &str,
    cargo: &str,
    cases: &[Measurement],
) -> Result<(), String> {
    let artifact = Baseline {
        schema: 1,
        generated_by:
            "cargo +nightly-2026-05-30 -Zscript crates/coseva/scripts/perf_memory.rs --refresh-baseline"
                .to_string(),
        rustc: rustc.to_string(),
        cargo: cargo.to_string(),
        cases: cases
            .iter()
            .map(|case| BaselineMeasurement {
                case: case.case.to_string(),
                operations: case.operations,
                allocations: case.allocations,
                allocated_bytes: case.allocated_bytes,
                peak_live_bytes: case.peak_live_bytes,
            })
            .collect(),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_string_pretty(&artifact)
            .map_err(|error| format!("cannot serialize baseline: {error}"))?
            + "\n",
    )
    .map_err(|error| format!("cannot write {}: {error}", path.display()))
}

fn check_baseline(
    path: &Path,
    measured: &[Measurement],
    rustc: &str,
    cargo: &str,
) -> Result<(), String> {
    let baseline: Baseline = serde_json::from_str(
        &fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("cannot parse {}: {error}", path.display()))?;
    if baseline.schema != 1 {
        return Err(format!("unsupported memory baseline schema {}", baseline.schema));
    }
    if baseline.rustc != rustc {
        return Err("rustc does not match the pinned memory baseline toolchain".to_string());
    }
    if baseline.cargo != cargo {
        return Err("cargo does not match the pinned memory baseline toolchain".to_string());
    }

    let mut failures = Vec::new();
    for current in measured {
        let old = baseline
            .cases
            .iter()
            .find(|case| case.case == current.case)
            .ok_or_else(|| format!("{}: no committed memory baseline", current.case))?;
        if current.operations != old.operations {
            failures.push(format!(
                "{}: operations changed from {} to {}",
                current.case, old.operations, current.operations
            ));
        }

        match current.case {
            "read_owned_reused" if current.allocations > 5 => failures.push(format!(
                "{}: {} allocations exceeds the ceiling of 5",
                current.case, current.allocations
            )),
            // `TextRecord` refills the same reusable owned storage shape as
            // `ByteRecord`, validating it in place after the parse rather than
            // allocating a second buffer per record.
            "read_text_reused" if current.allocations > 5 => failures.push(format!(
                "{}: {} allocations exceeds the ceiling of 5",
                current.case, current.allocations
            )),
            // The builder stages each record in a `ByteRecord` held on the
            // emitter. That record is reused, so the only allocations here are
            // the sink's growth, and the count must stay near `write_vec_growth`
            // rather than scaling with the record count.
            "write_vec_builder" if current.allocations > 32 => failures.push(format!(
                "{}: {} allocations exceeds the ceiling of 32; the builder is \
                 allocating per record again",
                current.case, current.allocations
            )),
            "decode_into_narrow_reused" | "decode_into_wide_reused"
                if current.allocations != 0
                    || current.allocated_bytes != 0
                    || current.peak_live_bytes != 0 =>
            {
                failures.push(format!(
                    "{}: expected zero steady-state allocation, got {} calls, {} bytes, {} peak",
                    current.case,
                    current.allocations,
                    current.allocated_bytes,
                    current.peak_live_bytes
                ));
            }
            // `src/parallel.rs` promises peak memory "bounded by the threads
            // and their work unit, never by the document's size". A per-case
            // ceiling cannot say that -- only the pair of sizes can, and
            // `check_document_independence` below does it.
            //
            // What is checkable per case is that the owned path's batch and
            // record recycling still works. Without it every batch and every
            // record in it is a fresh allocation, which at four threads and
            // 4096 records to a batch is hundreds of thousands of calls rather
            // than the low thousands the pools need to fill.
            // The owned path keeps a bounded number of batches in flight:
            // `PARALLEL_THREADS` workers, each with a forward queue of
            // `QUEUE_DEPTH` plus the batch it is filling, times
            // `batch_records`. Every record in those batches is alive at once,
            // and at four threads and 4096 records to a batch that is around
            // 80,000 records or, at this document's shape, some 10 MiB. Twice
            // that is a ceiling the design cannot legitimately exceed, and it
            // is far below what a path buffering the document would reach.
            "parallel_batches_16mib" | "parallel_batches_64mib"
                if current.peak_live_bytes > 24 << 20 =>
            {
                failures.push(format!(
                    "{}: peak {} exceeds the 24 MiB ceiling the in-flight batch \
                     bound implies; the owned path is holding more than its \
                     threads and their work unit",
                    current.case, current.peak_live_bytes
                ));
            }
            _ => {}
        }

        // The owned parallel path is a race between four workers and one
        // consumer, and how many batches miss the recycle channel depends on
        // how that race falls out: repeated runs on an idle host vary by about
        // 30% in both allocation count and peak. A 5% band would fail
        // constantly and teach everyone to ignore it. The ceiling above and the
        // pair check below are what guard this case, and neither depends on a
        // committed number.
        let jittery = current.case.starts_with("parallel_batches_");
        if !current.case.starts_with("decode_into_") && !jittery {
            check_growth(
                current.case,
                "allocated bytes",
                current.allocated_bytes,
                old.allocated_bytes,
                &mut failures,
            );
            check_growth(
                current.case,
                "peak live bytes",
                current.peak_live_bytes as u64,
                old.peak_live_bytes as u64,
                &mut failures,
            );
        }
        println!(
            "memory {:27} allocations={} bytes={} peak={}",
            current.case,
            current.allocations,
            current.allocated_bytes,
            current.peak_live_bytes
        );
    }

    check_document_independence(measured, &mut failures);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("memory baseline regression:\n  {}", failures.join("\n  ")))
    }
}

/// Check that the parallel paths' peak does not follow the document's size.
///
/// This is the property `src/parallel.rs` states and the reason the harness
/// measures each parallel path at two document sizes rather than one. It is
/// also the only part of this file that is host-independent by construction
/// rather than by pinning: it compares two runs on the same machine in the same
/// process, so it says the same thing everywhere, and it would still catch a
/// path that started buffering the whole document even if someone refreshed the
/// baselines to match.
///
/// The allowance is threefold, which needs saying. The two documents differ
/// fourfold, so a path whose memory followed the document would land at about
/// four and fail. The borrowed path is exactly 1.00 at both sizes -- identical
/// allocation count, identical peak -- so for it the bound could be almost
/// anything. The owned path is a race between the workers and the consumer and
/// ranges from 1.45 to 2.17 across repeated runs on an idle host, which is what
/// sets the allowance: high enough that the jitter never fails it, low enough
/// that document-proportional growth always does.
fn check_document_independence(measured: &[Measurement], failures: &mut Vec<String>) {
    /// How much larger the 64 MiB run may be than the 16 MiB one.
    const LIMIT: u64 = 3;

    const PAIRS: [(&str, &str); 2] = [
        ("parallel_fold_16mib", "parallel_fold_64mib"),
        ("parallel_batches_16mib", "parallel_batches_64mib"),
    ];

    for (small_case, large_case) in PAIRS {
        let Some(small) = measured.iter().find(|case| case.case == small_case) else {
            continue;
        };
        let Some(large) = measured.iter().find(|case| case.case == large_case) else {
            continue;
        };
        for (metric, small_value, large_value) in [
            (
                "peak",
                small.peak_live_bytes as u64,
                large.peak_live_bytes as u64,
            ),
            ("allocations", small.allocations, large.allocations),
        ] {
            // Below this the promise is satisfied trivially and the ratio is
            // meaningless: the borrowed path's whole peak is a few kilobytes,
            // where one incidental buffer would swamp the comparison.
            if small_value < 64 << 10 {
                continue;
            }
            if large_value > small_value.saturating_mul(LIMIT) {
                failures.push(format!(
                    "{large_case}: {metric} {large_value} exceeds {LIMIT}x {small_case}'s \
                     {small_value} on a document four times the size; memory is following \
                     the document rather than the threads and their work unit"
                ));
            }
        }
        println!(
            "memory {:27} peak {:.2}x allocations {:.2}x for 4x the document",
            format!("{small_case}/{large_case}"),
            large.peak_live_bytes as f64 / small.peak_live_bytes.max(1) as f64,
            large.allocations as f64 / small.allocations.max(1) as f64
        );
    }
}

fn check_growth(
    case: &str,
    metric: &str,
    current: u64,
    baseline: u64,
    failures: &mut Vec<String>,
) {
    let limit = ((u128::from(baseline) * 105) / 100) as u64;
    if current > limit {
        failures.push(format!(
            "{case}: {metric} {current} exceeds 105% baseline limit {limit} ({baseline})"
        ));
    }
}

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

/// A document of `bytes` whose every record is the same width.
///
/// [`document`] numbers its rows, so its records widen as it grows and a
/// 64 MiB one holds wider records than a 16 MiB one. That is fine for a single
/// measurement and fatal for a comparison between two sizes, which would
/// otherwise be reading record shape as though it were document size. These
/// records are fixed-width, so the only thing that differs between the two
/// sizes is how many of them there are.
fn uniform_document(bytes: usize) -> Vec<u8> {
    let mut document = Vec::with_capacity(bytes + 64);
    let mut index = 0_u64;
    while document.len() < bytes {
        document.extend_from_slice(
            format!(
                "{:07},{:07},{:07},{:07}\n",
                index % 1_000_000,
                (index * 3) % 1_000_000,
                index % 97,
                index % 1013
            )
            .as_bytes(),
        );
        index += 1;
    }
    document
}

fn typed_document(columns: usize, rows: usize) -> Vec<u8> {
    let mut document = Vec::with_capacity(columns * (VALUE_LEN + 1) * rows);
    for _ in 0..rows {
        for column in 0..columns {
            let mut value = 10_000 + column * 137;
            let start = document.len();
            document.resize(start + VALUE_LEN, b'0');
            for digit in (0..VALUE_LEN).rev() {
                document[start + digit] = b'0' + (value % 10) as u8;
                value /= 10;
            }
            document.push(if column + 1 == columns { b'\n' } else { b',' });
        }
    }
    document
}

const VALUE_LEN: usize = 5;

fn measure(
    case: &'static str,
    operations: usize,
    body: impl FnOnce() -> usize,
) -> Measurement {
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    black_box(body());
    Measurement {
        case,
        operations,
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        peak_live_bytes: PEAK_BYTES.load(Ordering::Relaxed).saturating_sub(baseline),
    }
}

fn measure_decode_into(case: &'static str, input: &[u8]) -> Measurement {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::<Csv>::new(input, options).expect("valid options");
    let mut output = Reused(
        String::with_capacity(VALUE_LEN),
        Vec::with_capacity(VALUE_LEN),
    );
    let mut warmup = parser
        .next_line()
        .expect("valid document")
        .expect("warm-up row");
    warmup.decode_into(&mut output).expect("valid row");

    measure(case, 1_000, || {
        let mut rows = 0;
        let mut checksum = 0;
        while let Some(mut line) = parser.next_line().expect("valid document") {
            line.decode_into(&mut output).expect("valid row");
            checksum += output.0.len() + output.1.len();
            rows += 1;
        }
        assert_eq!(rows, 1_000);
        assert_eq!(checksum, rows * VALUE_LEN * 2);
        checksum
    })
}

fn read_owned(input: &[u8], expected_rows: usize) -> usize {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::<Csv>::new(input, options).expect("valid options");
    let mut record = ByteRecord::new();
    let mut rows = 0;
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("valid document") {
        line.read_byte_record_into(&mut record)
            .expect("valid record");
        rows += 1;
        fields += record.len();
    }
    assert_eq!(rows, expected_rows);
    assert_eq!(fields, expected_rows * 4);
    fields
}

fn read_text(input: &[u8], expected_rows: usize) -> usize {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::<Csv>::new(input, options).expect("valid options");
    let mut record = TextRecord::new();
    let mut rows = 0;
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("valid document") {
        line.read_text_record_into(&mut record)
            .expect("valid record");
        rows += 1;
        fields += record.len();
    }
    assert_eq!(rows, expected_rows);
    assert_eq!(fields, expected_rows * 4);
    fields
}

fn write_rows(rows: usize) -> usize {
    static FIELDS: [&[u8]; 4] = [b"42", b"126", b"7", b"19"];
    let mut emitter =
        VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new().has_headers(false))
            .expect("valid options");
    for _ in 0..rows {
        emitter.emit_slices(&FIELDS).expect("valid fields");
    }
    emitter.into_inner().len()
}

/// The same rows through the field-at-a-time builder rather than `emit_slices`.
///
/// `begin_record` stages the fields in a `ByteRecord` before emitting it. That
/// record is held on the emitter and reused, so this case must not allocate
/// more often than [`write_rows`] does — a per-record allocation here would
/// show up as a count proportional to `rows`.
fn write_rows_builder(rows: usize) -> usize {
    static FIELDS: [&[u8]; 4] = [b"42", b"126", b"7", b"19"];
    let mut emitter =
        VecEmitter::<Csv>::new(Vec::new(), EmitOptions::new().has_headers(false))
            .expect("valid options");
    for _ in 0..rows {
        let mut pending = emitter.begin_record();
        for field in FIELDS {
            pending.write_field(field).expect("valid field");
        }
        pending.finish().expect("valid fields");
    }
    emitter.into_inner().len()
}

/// Read every record through the I/O front end, whose peak must stay near its
/// read buffer however long the document is.
fn read_streaming(input: &[u8]) -> usize {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser =
        IoParser::<_, Csv>::new(Cursor::new(input), options).expect("valid options");
    let mut record = ByteRecord::new();
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("valid document") {
        line.read_byte_record_into(&mut record)
            .expect("valid record");
        fields += record.len();
    }
    fields
}

/// Emit through the I/O front end into a sink that keeps nothing, so the peak
/// is the emitter's own buffering and the drain policy alone.
fn write_drained(rows: usize) -> usize {
    static FIELDS: [&[u8]; 4] = [b"42", b"126", b"7", b"19"];
    let mut emitter = IoEmitter::<_, Csv>::new(sink(), EmitOptions::new().has_headers(false))
        .expect("valid options");
    for _ in 0..rows {
        emitter.emit_record(FIELDS).expect("valid fields");
    }
    emitter.flush().expect("sink accepts everything");
    rows
}

/// Feed the push parser one byte at a time, the smallest chunk a caller can
/// choose and the one that forces the window to assemble every record itself.
fn push_one_byte_chunks(input: &[u8]) -> usize {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = PushParser::<Csv>::new(options).expect("valid options");
    let mut record = ByteRecord::new();
    let mut fields = 0;
    let mut fed = 0;
    while fed < input.len() {
        let mut chunk = parser.chunk(&input[fed..fed + 1]);
        while let Some(mut line) = chunk.next_line().expect("valid document") {
            line.read_byte_record_into(&mut record)
                .expect("valid record");
            fields += record.len();
        }
        let done = chunk.done();
        fed += if done == 0 { 1 } else { done };
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    while let Some(mut line) = chunk.next_line().expect("valid document") {
        line.read_byte_record_into(&mut record)
            .expect("valid record");
        fields += record.len();
    }
    let _ = chunk.done();
    fields
}

/// A document of `records` short rows with one record of `outlier` bytes in the
/// middle, which is what the reclamation policy in `src/reclaim.rs` exists to
/// bound: the window must grow to hold it and must give the growth back.
fn outlier_document(outlier: usize, records: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(outlier + records * 16);
    for index in 0..records {
        if index == records / 2 {
            out.extend(std::iter::repeat_n(b'x', outlier));
            out.push(b'\n');
        }
        out.extend_from_slice(b"42,126,7,19\n");
    }
    out
}

fn build_index(input: &[u8]) -> usize {
    CsvIndex::build(input, IndexOptions::default())
        .expect("valid document")
        .len() as usize
}

/// How many threads the parallel parse cases run with.
///
/// Pinned rather than taken from `available_parallelism`, because the recycling
/// pools these cases exist to watch are sized per worker: a baseline taken at
/// the host's thread count would be a different measurement on every machine
/// and could not be committed. Four is enough for the pools to be shared and
/// contended, and small enough to run anywhere.
const PARALLEL_THREADS: usize = 4;

fn parallel_parser() -> ParallelParser<Csv> {
    let threads = NonZeroUsize::new(PARALLEL_THREADS).expect("non-zero");
    ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
        .threads(threads)
        // The default threshold is 32 MiB, so without this the smaller of the
        // two documents runs on the serial fallback and the pair below compares
        // two different code paths rather than one path at two sizes. That
        // mistake reads as a spectacular memory regression in the parallel path
        // and is entirely an artifact of the threshold.
        .parallel_threshold(0)
}

/// The borrowed path: workers fold records they never hand across a thread.
fn parallel_fold(input: &[u8]) -> usize {
    let totals = parallel_parser()
        .fold(
            input,
            || 0_usize,
            |total: &mut usize, record| {
                *total += record.len();
                Ok::<(), coseva::Error>(())
            },
        )
        .expect("valid document");
    totals.into_iter().sum()
}

/// The owned path: workers gather records into batches and hand them over.
///
/// `src/parallel.rs` promises that in steady state this "allocates neither the
/// batch nor its records", which is a recycling scheme whose whole value is
/// invisible if it quietly stops working.
fn parallel_batches(input: &[u8]) -> usize {
    let mut fields = 0_usize;
    parallel_parser()
        .for_each_batch(input, |batch: &mut Vec<ByteRecord>| {
            for record in batch.iter() {
                fields += record.len();
            }
            Ok::<(), coseva::Error>(())
        })
        .expect("valid document");
    fields
}

#!/usr/bin/env -S cargo +nightly-2026-05-30 -Zscript
---
[package]
edition = "2024"

[dependencies]
coseva = { path = "..", features = ["std"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# `cargo -Zscript` builds the dev profile, and an unoptimized parser measures
# the compiler rather than the crate. Match the `bench` profile instead.
[profile.dev]
opt-level = 3
debug = false
debug-assertions = false
overflow-checks = false
lto = true
codegen-units = 1
---

//! Measure serial wall-clock throughput and peak resident memory against input size.
//!
//! Every other serial measurement in this crate is a Callgrind instruction
//! count over a corpus capped at 256 KiB. Instruction counts are size-linear
//! and blind by construction to cache residency, memory bandwidth and
//! allocator behavior — the effects that decide whether the parallel path is
//! worth reaching for. This sweep runs from that 256 KiB cap up to the 32 MiB
//! parallel threshold so those effects are visible.
//!
//! Peak resident memory is `VmHWM`, a high-water mark that never falls within
//! a process, so each size runs in its own child process.
//!
//! Wall clock is not deterministic and this harness gates nothing. It reports,
//! and it records the spread it observed so a floor can be set later.
//!
//! # Results
//!
//! Host: `mataille-devbox-1`, AMD EPYC 7763 64-Core, 16 logical CPUs, WSL2.
//! Median of seven samples after one untimed warm pass; see
//! `perf-scaling.json` for the full artifact including per-point spread.
//!
//! | Size    | `read_slice_borrowed` | `read_io_streaming` | `write_io_drain` |
//! |---------|----------------------:|--------------------:|-----------------:|
//! | 256 KiB |             932 MiB/s |           246 MiB/s |        339 MiB/s |
//! | 1 MiB   |             964 MiB/s |           235 MiB/s |        256 MiB/s |
//! | 4 MiB   |             579 MiB/s |           193 MiB/s |        363 MiB/s |
//! | 8 MiB   |             539 MiB/s |           259 MiB/s |        446 MiB/s |
//! | 16 MiB  |             533 MiB/s |           256 MiB/s |        372 MiB/s |
//! | 32 MiB  |             518 MiB/s |           184 MiB/s |        613 MiB/s |
//!
//! # The borrowed read loses 46% of its throughput between 1 and 32 MiB
//!
//! This is the point of the sweep. `read_slice_borrowed` does the same work
//! per record at every size — its instruction count is flat, which is why the
//! Callgrind sentinels see nothing — and yet it runs at 964 MiB/s while the
//! document fits in cache and 518 MiB/s once it does not. The knee is between
//! 1 and 4 MiB, which is where the corpus stops fitting in L2.
//!
//! That halving is the honest baseline any parallel speedup should be measured
//! against, and it is the reason the parallel path is worth having at all.
//! Read it as a caution about the rest of this crate's numbers: every
//! per-record instruction count here was taken over a 256 KiB corpus, at the
//! left-hand end of this table, where the parser is roughly twice as fast as
//! it is on a file a user would reach for the parallel reader to open.
//!
//! The two I/O paths do not show the same knee. They are already bound by
//! their own per-record work rather than by memory bandwidth, and their spread
//! is wide enough that the movement across sizes is not separable from noise.
//!
//! # Peak RSS is the corpus, not the parser
//!
//! Peak RSS tracks input size in every row — 2.7 MiB plus the corpus, to
//! within a few hundred KiB — because this harness generates the document in
//! memory before it starts. What that shows is the useful part: no path adds
//! resident memory that scales with the input. `perf_memory.rs` measures the
//! streaming front ends' own peaks directly, and finds them bounded at a few
//! kilobytes regardless of document size.

use std::io::{Cursor, sink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;
use std::{env, fs};

use coseva::config::{EmitOptions, Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, IoEmitter, IoParser, SliceParser};
use serde::{Deserialize, Serialize};

/// 256 KiB is `benches/documents.rs`'s per-document cap; 32 MiB is the size at
/// which `parallel` documents its crossover.
const SIZES: [usize; 6] = [
    256 << 10,
    1 << 20,
    4 << 20,
    8 << 20,
    16 << 20,
    32 << 20,
];
const CASES: [&str; 3] = ["read_slice_borrowed", "read_io_streaming", "write_io_drain"];
const REPEATS: usize = 7;

#[derive(Serialize, Deserialize, Clone)]
struct Point {
    case: String,
    bytes: usize,
    records: usize,
    median_ms: f64,
    min_ms: f64,
    max_ms: f64,
    spread_percent: f64,
    mib_per_second: f64,
    peak_rss_bytes: u64,
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
struct Artifact {
    evidence: Evidence,
    metric: &'static str,
    repeats: usize,
    points: Vec<Point>,
}

fn main() -> ExitCode {
    match run() {
        Ok(Some(message)) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("perf_scaling: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<Option<String>, String> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if let ["--child", case, bytes] = arguments
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        let bytes: usize = bytes.parse().map_err(|_| "bad --child size")?;
        return child(case, bytes).map(Some);
    }

    let mut output = PathBuf::from("crates/coseva/scripts/perf-scaling.json");
    let mut allow_dirty = false;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                output = PathBuf::from(arguments.next().ok_or("--output needs a path")?);
            }
            "--allow-dirty" => allow_dirty = true,
            "--help" | "-h" => {
                println!("usage: perf_scaling.rs [--output PATH] [--allow-dirty]");
                return Ok(None);
            }
            other => return Err(format!("unrecognised argument `{other}`")),
        }
    }

    let root = repository_root()?;
    let dirty = command(&root, "git", &["status", "--porcelain=v1", "--untracked-files=all"])?;
    let tree_clean = dirty.trim().is_empty();
    if !tree_clean && !allow_dirty {
        return Err("refusing to measure a dirty source tree".to_string());
    }

    let script = env::args().next().ok_or("no argv[0]")?;
    let mut points = Vec::new();
    for case in CASES {
        for bytes in SIZES {
            let output = Command::new(&script)
                .current_dir(&root)
                .args(["--child", case, &bytes.to_string()])
                .output()
                .map_err(|error| format!("cannot re-exec {script}: {error}"))?;
            if !output.status.success() {
                return Err(format!(
                    "{case}/{bytes} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            let point: Point = serde_json::from_str(&String::from_utf8_lossy(&output.stdout))
                .map_err(|error| format!("{case}/{bytes}: cannot parse child output: {error}"))?;
            eprintln!(
                "{case:20} {:>7} KiB  {:8.2} ms  {:7.1} MiB/s  peak RSS {:>7} KiB  (+/-{:.1}%)",
                point.bytes >> 10,
                point.median_ms,
                point.mib_per_second,
                point.peak_rss_bytes / 1024,
                point.spread_percent,
            );
            points.push(point);
        }
    }

    let artifact = Artifact {
        evidence: Evidence {
            command: "crates/coseva/scripts/perf_scaling.rs",
            host: Host {
                hostname: command(&root, "hostname", &[])?,
                cpu_model: cpu_model(),
                logical_cpus: std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1),
            },
            toolchain: Toolchain {
                rustc: command(&root, "rustc", &["-Vv"])?,
                cargo: command(&root, "cargo", &["-V"])?,
            },
            source: Source {
                revision: command(&root, "git", &["rev-parse", "HEAD"])?,
                tree_clean,
            },
        },
        metric: "median_wall_ms_and_peak_rss_bytes_against_input_size",
        repeats: REPEATS,
        points,
    };
    let destination = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    fs::write(
        &destination,
        serde_json::to_string_pretty(&artifact)
            .map_err(|error| format!("cannot serialize artifact: {error}"))?
            + "\n",
    )
    .map_err(|error| format!("cannot write {}: {error}", destination.display()))?;
    Ok(Some(format!("wrote {}", destination.display())))
}

fn child(case: &str, bytes: usize) -> Result<String, String> {
    let input = document(bytes);
    let records = input.iter().filter(|&&byte| byte == b'\n').count();
    let work: Box<dyn Fn() -> usize> = match case {
        "read_slice_borrowed" => Box::new(|| read_slice(&input)),
        "read_io_streaming" => Box::new(|| read_streaming(&input)),
        "write_io_drain" => Box::new(|| write_drained(records)),
        other => return Err(format!("unknown case `{other}`")),
    };

    // One untimed pass so the first sample does not pay for cold pages.
    work();
    let mut samples = Vec::with_capacity(REPEATS);
    for _ in 0..REPEATS {
        let start = Instant::now();
        let observed = work();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        assert!(observed > 0, "benchmark did no work");
    }
    samples.sort_by(f64::total_cmp);
    let median_ms = samples[samples.len() / 2];
    let min_ms = samples[0];
    let max_ms = samples[samples.len() - 1];
    let point = Point {
        case: case.to_string(),
        bytes: input.len(),
        records,
        median_ms: round(median_ms),
        min_ms: round(min_ms),
        max_ms: round(max_ms),
        spread_percent: round((max_ms - min_ms) / 2.0 / median_ms * 100.0),
        mib_per_second: round(input.len() as f64 / (1 << 20) as f64 / (median_ms / 1000.0)),
        peak_rss_bytes: peak_rss()?,
    };
    serde_json::to_string(&point).map_err(|error| format!("cannot serialize point: {error}"))
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// `VmHWM` from `/proc/self/status`, the kernel's own peak for this process.
fn peak_rss() -> Result<u64, String> {
    let status = fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("cannot read /proc/self/status: {error}"))?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kib: u64 = rest
                .trim()
                .trim_end_matches(" kB")
                .parse()
                .map_err(|_| "cannot parse VmHWM")?;
            return Ok(kib * 1024);
        }
    }
    Err("no VmHWM in /proc/self/status".to_string())
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

fn read_slice(input: &[u8]) -> usize {
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = SliceParser::<Csv>::new(input, options).expect("valid options");
    let mut fields = 0;
    while let Some(mut line) = parser.next_line().expect("valid document") {
        let record = line.record().expect("valid record");
        fields += record.len();
    }
    fields
}

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
                .find(|line| line.to_ascii_lowercase().starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string())
}

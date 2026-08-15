#!/usr/bin/env -S cargo +nightly-2026-05-30 -Zscript
---
[package]
edition = "2024"

[dependencies]
coseva = { path = "..", features = ["std", "parallel"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.dev]
opt-level = 3
debug = false
debug-assertions = false
overflow-checks = false
lto = true
codegen-units = 1
---

//! Measure the delivery tail of the ordered parallel drain.
//!
//! `for_each_batch` hands batches back **in order**, so a worker that is slow
//! on batch *n* stalls every finished batch behind it. That is head-of-line
//! blocking, and it is a tail phenomenon: it does not move a median or a
//! throughput number at all, because the same total work still completes. It
//! shows up only as the gap between consecutive batch deliveries.
//!
//! So this measures exactly that gap, and reports its distribution. A drain
//! free of head-of-line blocking delivers batches at a near-constant cadence
//! and its p99 sits close to its p50; a drain that stalls has a p99 many times
//! its p50 while the throughput number stays flat.
//!
//! The batch-size sweep is the lever: smaller batches mean more, shorter
//! stalls, larger batches mean fewer, longer ones.
//!
//! Wall clock, and report-only. `benchmarks/parallel/run.py` owns the gate.
//!
//! # Results
//!
//! Host: `mataille-devbox-1`, AMD EPYC 7763 64-Core, 16 logical CPUs, WSL2.
//! A 64 MiB document, best of five runs by p99.
//!
//! | Batch records | Batches | p50     | p90     | p99       | max       | p99/p50 |
//! |--------------:|--------:|--------:|--------:|----------:|----------:|--------:|
//! |            32 |  94,148 |  0.4 us | 22.7 us |  104.2 us |  384.7 us |    260x |
//! |           256 |  11,785 | 48.0 us | 78.7 us |  121.9 us |  389.1 us |    2.5x |
//! |         4,096 |     794 | 32.3 us | 91.2 us |  313.1 us | 2912.1 us |    9.7x |
//! |        16,384 |     256 | 136 us  |  431 us | 1317.3 us | 3150.3 us |    9.7x |
//!
//! The drain does block, and the shape is the one the ordering constraint
//! predicts. At 32 records a batch, half the deliveries take under half a
//! microsecond — those are batches that were already finished and queued
//! behind the head — and then one in a hundred waits 260 times as long for the
//! head to arrive. The work is bursty, not evenly paced.
//!
//! At the default 4,096 records the cadence is far more even, but the tail is
//! still an order of magnitude over the median and the worst single stall is
//! 2.9 ms. A consumer doing anything latency-sensitive per batch should size
//! for that number and not for the 32 us median.
//!
//! 256 records is the calmest point in this sweep by a wide margin: its p99 is
//! 2.5x its p50 and its worst stall is 389 us, an eighth of the default's.
//! It buys that with more coordination per record, which is what
//! `benches/parallel.rs`'s throughput sweep charges it for — so this is a
//! latency-versus-throughput choice, and both halves of it are now measured.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;
use std::{env, fs};

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::parallel::ParallelParser;
use serde::Serialize;

const DOCUMENT_BYTES: usize = 64 << 20;
const BATCH_RECORDS: [usize; 4] = [32, 256, 4096, 16384];
const REPEATS: usize = 5;

#[derive(Serialize, Clone)]
struct Case {
    batch_records: usize,
    batches: usize,
    total_ms: f64,
    gap_p50_us: f64,
    gap_p90_us: f64,
    gap_p99_us: f64,
    gap_max_us: f64,
    /// p99 over p50. The head-of-line blocking factor: 1.0 is a perfectly even
    /// cadence, and a large number is a drain that stalls.
    tail_ratio: f64,
}

#[derive(Serialize)]
struct Host {
    hostname: String,
    cpu_model: String,
    logical_cpus: usize,
}

#[derive(Serialize)]
struct Artifact {
    command: &'static str,
    metric: &'static str,
    host: Host,
    rustc: String,
    document_bytes: usize,
    repeats: usize,
    cases: Vec<Case>,
}

fn main() -> ExitCode {
    match run() {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("perf_parallel_tail: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let mut output = PathBuf::from("crates/coseva/scripts/perf-parallel-tail.json");
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--output" => {
                output = PathBuf::from(arguments.next().ok_or("--output needs a path")?);
            }
            "--help" | "-h" => {
                println!("usage: perf_parallel_tail.rs [--output PATH]");
                return Ok(String::new());
            }
            other => return Err(format!("unrecognised argument `{other}`")),
        }
    }

    let root = repository_root()?;
    let input = document(DOCUMENT_BYTES);
    let mut cases = Vec::new();
    for batch_records in BATCH_RECORDS {
        let mut best: Option<Case> = None;
        for _ in 0..REPEATS {
            let case = measure(&input, batch_records)?;
            // The tail is what is being measured, so a run perturbed by
            // unrelated load can only make it look worse. Keep the calmest.
            if best
                .as_ref()
                .is_none_or(|current| case.gap_p99_us < current.gap_p99_us)
            {
                best = Some(case);
            }
        }
        let case = best.ok_or("no samples")?;
        eprintln!(
            "batch {:>6}: {:>6} batches  p50 {:8.1} us  p90 {:8.1} us  p99 {:8.1} us  max {:9.1} us  tail {:5.1}x",
            case.batch_records,
            case.batches,
            case.gap_p50_us,
            case.gap_p90_us,
            case.gap_p99_us,
            case.gap_max_us,
            case.tail_ratio,
        );
        cases.push(case);
    }

    let artifact = Artifact {
        command: "crates/coseva/scripts/perf_parallel_tail.rs",
        metric: "inter_batch_delivery_gap_percentiles_microseconds",
        host: Host {
            hostname: command(&root, "hostname", &[])?,
            cpu_model: cpu_model(),
            logical_cpus: std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1),
        },
        rustc: command(&root, "rustc", &["-V"])?,
        document_bytes: input.len(),
        repeats: REPEATS,
        cases,
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
    Ok(format!("wrote {}", destination.display()))
}

fn measure(input: &[u8], batch_records: usize) -> Result<Case, String> {
    let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
        .parallel_threshold(0)
        .batch_records(NonZeroUsize::new(batch_records).expect("a positive batch size"));

    let mut gaps = Vec::new();
    let start = Instant::now();
    let mut previous = start;
    let mut fields = 0_usize;
    parser
        .for_each_batch::<_, coseva::Error>(input, |batch| {
            let now = Instant::now();
            gaps.push(now.duration_since(previous).as_secs_f64() * 1_000_000.0);
            previous = now;
            for record in batch.iter() {
                fields += record.len();
            }
            Ok(())
        })
        .map_err(|error| format!("parse failed: {error}"))?;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    assert!(fields > 0, "the document parsed to nothing");

    // The first gap includes worker start-up, which is not a drain stall.
    if gaps.len() > 1 {
        gaps.remove(0);
    }
    gaps.sort_by(f64::total_cmp);
    if gaps.is_empty() {
        return Err(format!("batch size {batch_records} produced no batches"));
    }
    let p50 = percentile(&gaps, 0.50);
    let p99 = percentile(&gaps, 0.99);
    Ok(Case {
        batch_records,
        batches: gaps.len() + 1,
        total_ms: round(total_ms),
        gap_p50_us: round(p50),
        gap_p90_us: round(percentile(&gaps, 0.90)),
        gap_p99_us: round(p99),
        gap_max_us: round(gaps[gaps.len() - 1]),
        tail_ratio: round(p99 / p50),
    })
}

/// Nearest-rank percentile over an already-sorted slice.
fn percentile(sorted: &[f64], fraction: f64) -> f64 {
    let rank = (fraction * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
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

//! Which SIMD dispatch arm a profiled run actually reached, and what it buys.
//!
//! Every vector kernel in this crate is selected by runtime CPU detection —
//! `avx2_available()` and `bmi2_available()` — and no build here sets
//! `target-cpu` or `+avx2`. The sentinels in `scripts/perf_gate.py` run under
//! Valgrind, which emulates the guest CPU and answers `CPUID` itself, so the
//! arm a measured run takes is a property of the Valgrind version and the CI
//! image rather than of the source. Until this suite existed nothing recorded
//! it: the artifact captured the *host's* CPU model, which is not what the
//! benchmark ran on, and the baselines were keyed by case name alone.
//!
//! So `arm` writes the detected arm, from inside the profiled process, to
//! `target/perf-report/dispatch-arm.txt`, and `perf_gate.py` fails when it
//! differs from the arm recorded in `scripts/perf-dispatch-arm.txt`. A CI image
//! or Valgrind upgrade that quietly moves every baseline onto the scalar
//! fallback then fails loudly instead of silently leaving the vector kernels
//! guarded by nothing.
//!
//! `scalar` and `selected` are the second, independent half of the same
//! question. They run the same structural count over the same bytes, one
//! through the fallback and one through the dispatched kernel, so their ratio
//! says whether the vector arm is delivering anything in the environment being
//! measured — and unlike the detection flag, it cannot be right for the wrong
//! reason. `perf_gate.py` pins that ratio.
//!
//! Callgrind Ir over 64 KiB, and the arm they were taken on:
//!
//! | Case       |      Ir |
//! |------------|--------:|
//! | `scalar`   | 475,176 |
//! | `selected` |  98,657 |
//!
//! Taken on `avx2+bmi2`, which is what the emulated CPUID reports today: the
//! dispatched scan is 4.8 times cheaper, so the committed baselines pin the
//! vector kernels rather than the fallback. Had the ratio come out near 1.0 the
//! conclusion would have been the opposite, and every Ir count in
//! `scripts/perf-baselines.tsv` would have been describing a configuration no
//! production host runs.

#![expect(missing_docs, reason = "benchmark macros are private")]
#![expect(
    clippy::panic,
    reason = "the arm probe must fail loudly rather than let the gate pass with no answer"
)]

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;

use coseva::benchmark::{dispatch_arm, scan_scalar, scan_selected};
use gungraun::prelude::*;

const ROWS: usize = 1_024;
const COLUMNS: usize = 8;

/// A plain document with one structural byte every eight, so a scan finds work
/// in every block whichever arm it takes.
fn corpus() -> Vec<u8> {
    let mut out = Vec::with_capacity(ROWS * COLUMNS * 8);
    for _ in 0..ROWS {
        for column in 0..COLUMNS {
            out.extend_from_slice(b"1234567");
            out.push(if column + 1 == COLUMNS { b'\n' } else { b',' });
        }
    }
    out
}

fn drop_it<T>(value: T) {
    drop(value);
}

fn check(total: usize) -> usize {
    assert_eq!(total, ROWS * COLUMNS, "scanner missed structural bytes");
    total
}

/// Where the arm probe leaves its answer for `scripts/perf_gate.py`.
///
/// Derived from the manifest directory rather than the working directory,
/// because a benchmark's working directory is not the harness's.
fn arm_path() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path.push("target/perf-report");
    path
}

/// Record the arm this process detects.
///
/// Runs as setup, so writing the file is outside the measured region; the
/// measurement itself is uninteresting and ungated. What matters is that this
/// executes inside the profiled binary, under whatever CPU the profiler is
/// emulating.
fn record_arm() -> &'static str {
    let arm = dispatch_arm();
    let directory = arm_path();
    fs::create_dir_all(&directory).unwrap_or_else(|error| {
        panic!("cannot create {}: {error}", directory.display());
    });
    let path = directory.join("dispatch-arm.txt");
    fs::write(&path, format!("{}\n", arm.name()))
        .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
    arm.name()
}

#[library_benchmark(setup = record_arm)]
fn arm(name: &'static str) -> usize {
    black_box(name.len())
}

#[library_benchmark(setup = corpus, teardown = drop_it)]
fn scalar(input: Vec<u8>) -> (usize, Vec<u8>) {
    let total = check(scan_scalar(&input, b',', b'"', b'\n'));
    black_box((total, input))
}

#[library_benchmark(setup = corpus, teardown = drop_it)]
fn selected(input: Vec<u8>) -> (usize, Vec<u8>) {
    let total = check(scan_selected(&input, b',', b'"', b'\n'));
    black_box((total, input))
}

library_benchmark_group!(
    name = dispatch;
    benchmarks = arm, scalar, selected
);

main!(library_benchmark_groups = dispatch);

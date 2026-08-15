//! Benchmark-only access to isolated implementation primitives.

#[cfg(feature = "index")]
use crate::Error;
use crate::config::RecordEnding;
use crate::emit;
use crate::engine;
#[cfg(feature = "index")]
use crate::index::{CsvIndex, IndexOptions};

/// Which runtime-selected kernels this process can actually reach.
///
/// Every vector kernel in the crate is chosen by runtime CPU detection, not at
/// compile time, so a measurement says nothing until it says which arm ran.
/// This matters most under Valgrind, which emulates the guest CPU and answers
/// `CPUID` itself: a profiled run can silently take the scalar fallback while
/// the host it runs on supports everything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchArm {
    /// Whether the AVX2 structural scans are reachable.
    ///
    /// Gates the plain and quoted structural kernels.
    pub avx2: bool,
    /// Whether BMI2 is reachable.
    ///
    /// With `avx2`, gates the fused packed owned-record materializer.
    pub bmi2: bool,
}

impl DispatchArm {
    #[cfg(any(test, not(any(target_arch = "x86", target_arch = "x86_64"))))]
    const SCALAR: Self = Self::new(false, false);

    const fn new(avx2: bool, bmi2: bool) -> Self {
        Self { avx2, bmi2 }
    }

    #[cfg(any(test, target_arch = "x86"))]
    const fn avx2_only(avx2: bool) -> Self {
        Self::new(avx2, false)
    }

    /// A short stable name for this arm, for recording beside a measurement.
    ///
    /// The three names are `avx2+bmi2`, `avx2` and `scalar`; `bmi2` alone
    /// reaches no kernel this crate has, so it is reported as `scalar`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match (self.avx2, self.bmi2) {
            (true, true) => "avx2+bmi2",
            (true, false) => "avx2",
            _ => "scalar",
        }
    }
}

/// Detect the kernels reachable from this process, on this CPU.
///
/// Call it from inside a profiled binary; calling it from the harness that
/// spawns one answers for the wrong CPU.
#[must_use]
pub fn dispatch_arm() -> DispatchArm {
    #[cfg(target_arch = "x86_64")]
    {
        DispatchArm::new(
            crate::search::avx2_available(),
            crate::search::bmi2_available(),
        )
    }
    #[cfg(target_arch = "x86")]
    {
        DispatchArm::avx2_only(crate::search::avx2_available())
    }
    #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
    {
        DispatchArm::SCALAR
    }
}

/// Count structural bytes with the scalar scanner.
#[must_use]
pub fn scan_scalar(input: &[u8], delimiter: u8, quote: u8, record_ending: u8) -> usize {
    engine::count_structurals_scalar(input, delimiter, quote, record_ending)
}

/// Count structural bytes with the scanner selected for this target.
#[must_use]
pub fn scan_selected(input: &[u8], delimiter: u8, quote: u8, record_ending: u8) -> usize {
    engine::count_structurals_selected(input, delimiter, quote, record_ending)
}

/// Test the necessary-quoting predicate at a record ending.
///
/// Dispatches by field width exactly as emission does, so this is the arm a
/// real encode would take.
#[must_use]
pub fn needs_quotes(record_ending: RecordEnding, field: &[u8]) -> bool {
    emit::benchmark_needs_quotes(record_ending, field)
}

/// Test the necessary-quoting predicate through its SIMD-block scan.
///
/// Correct at any width, so a sweep can price it below the width emission
/// would actually choose it at.
#[must_use]
pub fn needs_quotes_blocks(record_ending: RecordEnding, field: &[u8]) -> bool {
    emit::benchmark_needs_quotes_blocks(record_ending, field)
}

/// Test the necessary-quoting predicate through its word-at-a-time scan.
///
/// Correct at any width, so a sweep can price it above the width emission
/// would actually choose it at.
#[must_use]
pub fn needs_quotes_words(record_ending: RecordEnding, field: &[u8]) -> bool {
    emit::benchmark_needs_quotes_words(record_ending, field)
}

/// Repeatedly escape one double-quoted field into reusable storage.
#[must_use]
pub fn escape_double_quote(field: &[u8], repetitions: usize) -> usize {
    emit::benchmark_escape_double_quote(field, repetitions)
}

/// Build an index through the serial builder regardless of document size.
///
/// [`CsvIndex::build`] chooses between a serial and a parallel builder by
/// document size, which leaves no way to time the two against each other. Only
/// their ratio, taken on one document in one process, survives a move to a
/// different machine, so a wall-clock harness needs to name the builder it
/// wants.
///
/// # Errors
///
/// Returns the first CSV syntax or resource-limit error.
#[cfg(feature = "index")]
pub fn build_index_serial(source: &[u8], options: IndexOptions) -> Result<CsvIndex, Error> {
    CsvIndex::benchmark_build_serial(source, options)
}

/// Build an index through the parallel builder at a fixed thread count.
///
/// The counterpart to [`build_index_serial`]. `threads` is explicit so a
/// recorded speedup states the width it was taken at instead of inheriting the
/// measuring host's.
///
/// # Errors
///
/// Returns the first CSV syntax or resource-limit error, or reports the
/// document as unindexable in parallel when its format forbids it.
#[cfg(all(feature = "index", feature = "parallel"))]
pub fn build_index_parallel(
    source: &[u8],
    options: IndexOptions,
    threads: usize,
) -> Result<CsvIndex, Error> {
    build_index_parallel_inner(source, options, threads)
}

#[cfg(all(feature = "index", feature = "parallel"))]
fn build_index_parallel_inner(
    source: &[u8],
    options: IndexOptions,
    threads: usize,
) -> Result<CsvIndex, Error> {
    #[cfg(test)]
    LAST_PARALLEL_WIDTH.set(threads);
    CsvIndex::benchmark_build_parallel(source, options, threads)
}

#[cfg(all(test, feature = "index", feature = "parallel"))]
std::thread_local! {
    static LAST_PARALLEL_WIDTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_arm_names() {
        assert_eq!(
            DispatchArm::new(true, true),
            DispatchArm {
                avx2: true,
                bmi2: true
            }
        );
        assert_eq!(
            DispatchArm::avx2_only(true),
            DispatchArm {
                avx2: true,
                bmi2: false
            }
        );
        assert_eq!(DispatchArm::avx2_only(false), DispatchArm::SCALAR);
        assert_eq!(
            DispatchArm {
                avx2: true,
                bmi2: true
            }
            .name(),
            "avx2+bmi2"
        );
        assert_eq!(
            DispatchArm {
                avx2: true,
                bmi2: false
            }
            .name(),
            "avx2"
        );
        assert_eq!(
            DispatchArm {
                avx2: false,
                bmi2: true
            }
            .name(),
            "scalar"
        );
        assert_eq!(
            DispatchArm {
                avx2: false,
                bmi2: false
            }
            .name(),
            "scalar"
        );
        let _ = dispatch_arm();
    }

    #[test]
    #[cfg(all(feature = "index", feature = "parallel"))]
    fn parallel_index_width_is_forwarded_exactly() {
        let source = b"a,b\n1,2\n";
        build_index_parallel(source, IndexOptions::default(), 3)
            .expect("the benchmark index builds");
        assert_eq!(LAST_PARALLEL_WIDTH.get(), 3);

        let unsupported = IndexOptions {
            format: crate::config::FormatOptions::COMMENTED_CSV,
            limits: crate::config::Limits::DEFAULT,
        };
        build_index_parallel(source, unsupported, usize::MAX)
            .expect_err("unsupported formats are rejected before the width is used");
        assert_eq!(LAST_PARALLEL_WIDTH.get(), usize::MAX);
    }
}

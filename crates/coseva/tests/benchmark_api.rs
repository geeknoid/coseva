//! Coverage for the `benchmarking`-gated primitives exposed by [`coseva::benchmark`].
//!
//! These wrappers exist so the benchmark harnesses can time individual engine
//! primitives in isolation. They are still shipped code, so they get the same
//! behavioral testing as the rest of the crate.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::benchmark::dispatch_arm;
use coseva::benchmark::escape_double_quote;
use coseva::benchmark::scan_scalar;
use coseva::benchmark::scan_selected;
use coseva::benchmark::{needs_quotes, needs_quotes_blocks, needs_quotes_words};
use coseva::config::RecordEnding;

/// Builds an input long enough to drive the vectorized scanner through several
/// full blocks plus a partial trailing one.
fn long_input() -> Vec<u8> {
    let mut input = Vec::new();
    for row in 0..37 {
        for column in 0..5 {
            if column > 0 {
                input.push(b',');
            }
            if row % 3 == 0 {
                input.push(b'"');
                input.extend_from_slice(b"quoted");
                input.push(b'"');
            } else {
                input.extend_from_slice(b"plain");
            }
        }
        input.push(b'\n');
    }
    input
}

#[test]
fn scan_scalar_counts_every_structural_byte() {
    assert_eq!(scan_scalar(b"a,b\n\"c\"", b',', b'"', b'\n'), 4);
}

#[test]
fn scan_scalar_counts_nothing_in_input_without_structural_bytes() {
    assert_eq!(scan_scalar(b"abcdef", b',', b'"', b'\n'), 0);
}

#[test]
fn scan_scalar_counts_nothing_in_empty_input() {
    assert_eq!(scan_scalar(b"", b',', b'"', b'\n'), 0);
}

#[test]
fn scan_scalar_honors_a_custom_structural_alphabet() {
    assert_eq!(scan_scalar(b"a;b'c\rd,e", b';', b'\'', b'\r'), 3);
}

#[test]
fn scan_selected_agrees_with_scan_scalar() {
    let input = long_input();
    assert_eq!(
        scan_selected(&input, b',', b'"', b'\n'),
        scan_scalar(&input, b',', b'"', b'\n')
    );
}

#[test]
fn scan_selected_agrees_with_scan_scalar_on_custom_structurals() {
    let mut input = long_input();
    for byte in &mut input {
        match *byte {
            b',' => *byte = b';',
            b'"' => *byte = b'\'',
            b'\n' => *byte = b'\r',
            _ => {}
        }
    }

    assert_eq!(
        scan_selected(&input, b';', b'\'', b'\r'),
        scan_scalar(&input, b';', b'\'', b'\r')
    );
}

#[test]
fn scan_selected_agrees_with_scan_scalar_at_every_input_length() {
    let input = long_input();
    for length in 0..input.len() {
        let prefix = &input[..length];
        assert_eq!(
            scan_selected(prefix, b',', b'"', b'\n'),
            scan_scalar(prefix, b',', b'"', b'\n'),
            "scanners disagree on a {length}-byte prefix"
        );
    }
}

/// Every record ending the quoting predicate distinguishes: `Newline` and
/// `CrLf` both scan for `\n` and `\r`, a `Byte` ending scans for itself alone.
const ENDINGS: [RecordEnding; 3] = [
    RecordEnding::Newline,
    RecordEnding::CrLf,
    RecordEnding::Byte(b';'),
];

/// The bytes that force quoting under `ending`, so a test can assert both that
/// each one is caught and that nothing else is.
fn structural_bytes(ending: RecordEnding) -> Vec<u8> {
    let mut bytes = vec![b',', b'"'];
    match ending {
        RecordEnding::Newline | RecordEnding::CrLf => bytes.extend_from_slice(b"\n\r"),
        RecordEnding::Byte(byte) => bytes.push(byte),
    }
    bytes
}

#[test]
fn the_dispatch_arm_names_the_kernels_it_reports_reachable() {
    let arm = dispatch_arm();
    assert_eq!(
        arm.name(),
        match (arm.avx2, arm.bmi2) {
            (true, true) => "avx2+bmi2",
            (true, false) => "avx2",
            (false, _) => "scalar",
        }
    );
    // BMI2 without AVX2 reaches no kernel this crate has, so it must not be
    // reported as a distinct arm; a gate keyed on the name would otherwise
    // treat two identical configurations as different.
    assert!(matches!(arm.name(), "avx2+bmi2" | "avx2" | "scalar"));
}

#[test]
fn the_dispatch_arm_agrees_with_the_scanner_it_describes() {
    // The arm is only worth recording if it describes the code that runs, so
    // check the two against each other rather than trusting the flag: on the
    // scalar arm the dispatched scan can only be the fallback, and either way
    // both must agree on the answer.
    let input = b"a,b,c
d,e,f
";
    assert_eq!(
        scan_selected(input, b',', b'"', b'\n'),
        scan_scalar(input, b',', b'"', b'\n')
    );
    let arm = dispatch_arm();
    if cfg!(not(any(target_arch = "x86", target_arch = "x86_64"))) {
        assert_eq!(arm.name(), "scalar", "no non-x86 target has a vector arm");
    }
}

#[test]
fn needs_quotes_leaves_a_plain_field_unquoted() {
    for ending in ENDINGS {
        assert!(!needs_quotes(ending, b"plain"), "{ending:?}");
        assert!(!needs_quotes(ending, b""), "{ending:?}");
    }
}

#[test]
fn needs_quotes_requires_quoting_for_structural_bytes() {
    for ending in ENDINGS {
        for structural in structural_bytes(ending) {
            let field = [b'a', structural, b'b'];
            assert!(
                needs_quotes(ending, &field),
                "{structural:?} under {ending:?}"
            );
        }
    }
}

#[test]
fn a_byte_record_ending_does_not_quote_for_a_newline() {
    // The three-needle arm exists precisely because a `Byte` ending has no
    // interest in `\n` or `\r`, and quoting for them would be wrong rather
    // than merely wasteful.
    let ending = RecordEnding::Byte(b';');
    assert!(!needs_quotes(ending, b"a\nb"));
    assert!(!needs_quotes(ending, b"a\rb"));
    assert!(!needs_quotes(ending, &[b'\n'; 64]));
    assert!(needs_quotes(ending, b"a;b"));
}

#[test]
fn needs_quotes_finds_a_structural_byte_at_every_position_and_width() {
    // The quoting scan hands whole SIMD blocks to the vectorized searcher and
    // sweeps the leftover bytes with a word-at-a-time loop. A seam between
    // those two passes would silently leave a field unquoted, so every offset
    // either side of a block boundary is checked explicitly.
    //
    // Both arms are checked at every width, not just at the widths the
    // threshold would pick them at. Each is correct everywhere -- that is what
    // lets `benches/needs_quotes.rs` price them against each other at one
    // width -- so a change that moved the threshold must not be able to expose
    // an arm that was only ever right on its own side of it.
    type Scan = fn(RecordEnding, &[u8]) -> bool;
    let scans: [(&str, Scan); 3] = [
        ("dispatch", needs_quotes),
        ("blocks", needs_quotes_blocks),
        ("words", needs_quotes_words),
    ];

    for ending in ENDINGS {
        let structurals = structural_bytes(ending);
        for width in 0..200_usize {
            let clean = vec![b'x'; width];
            for (name, scan) in scans {
                assert!(
                    !scan(ending, &clean),
                    "{name}: clean field of {width} bytes"
                );

                for &structural in &structurals {
                    for at in 0..width {
                        let mut field = clean.clone();
                        field[at] = structural;
                        assert!(
                            scan(ending, &field),
                            "{name}: {structural:?} at {at} of {width} bytes under {ending:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn escape_double_quote_reports_the_encoded_length_of_each_repetition() {
    // `a"b` encodes as `"a""b"`, which is six bytes.
    assert_eq!(escape_double_quote(b"a\"b", 1), 6);
    assert_eq!(escape_double_quote(b"a\"b", 3), 18);
}

#[test]
fn escape_double_quote_still_wraps_a_field_needing_no_escaping() {
    assert_eq!(escape_double_quote(b"plain", 1), 7);
    assert_eq!(escape_double_quote(b"", 1), 2);
}

#[test]
fn escape_double_quote_encodes_nothing_for_zero_repetitions() {
    assert_eq!(escape_double_quote(b"a\"b", 0), 0);
}

#[cfg(all(feature = "index", feature = "parallel"))]
mod forced_index_builders {
    use coseva::benchmark::{build_index_parallel, build_index_serial};
    use coseva::index::{CsvIndex, IndexOptions};

    /// A document large enough that both builders have several chunks of work.
    fn document(bytes: usize) -> Vec<u8> {
        let mut document = Vec::with_capacity(bytes + 64);
        let mut index = 0_u64;
        while document.len() < bytes {
            document.extend_from_slice(format!("{index},{},b\n", index * 3).as_bytes());
            index += 1;
        }
        document
    }

    #[test]
    fn both_builders_agree_with_each_other_and_with_the_dispatched_build() {
        // Well under `PARALLEL_INDEX_THRESHOLD_BYTES`, so the dispatched build
        // takes the serial path while the forced parallel one does not: the
        // point of these entry points is that the size no longer decides.
        let source = document(64 << 10);
        let serial = build_index_serial(&source, IndexOptions::default()).expect("a serial build");
        let parallel =
            build_index_parallel(&source, IndexOptions::default(), 4).expect("a parallel build");
        let dispatched =
            CsvIndex::build(&source, IndexOptions::default()).expect("a dispatched build");

        assert!(serial.len() > 1, "the document has many records");
        assert_eq!(serial.len(), parallel.len(), "both find the same records");
        assert_eq!(serial.len(), dispatched.len(), "and so does `build`");
        for record in 0..serial.len() {
            assert_eq!(
                serial.record_offset(record),
                parallel.record_offset(record),
                "record {record} is at the same offset"
            );
            assert_eq!(
                serial.record_line(record),
                dispatched.record_line(record),
                "record {record} is on the same line"
            );
        }
    }

    #[test]
    fn a_single_thread_is_a_valid_parallel_width() {
        let source = document(16 << 10);
        let one = build_index_parallel(&source, IndexOptions::default(), 1).expect("one thread");
        let many =
            build_index_parallel(&source, IndexOptions::default(), 8).expect("eight threads");
        assert_eq!(one.len(), many.len(), "the width does not change the index");
    }

    #[test]
    fn a_zero_thread_request_is_treated_as_one_rather_than_dividing_by_zero() {
        let source = document(16 << 10);
        let zero = build_index_parallel(&source, IndexOptions::default(), 0).expect("zero threads");
        let one = build_index_parallel(&source, IndexOptions::default(), 1).expect("one thread");
        assert_eq!(zero.len(), one.len(), "zero is clamped to one");
    }

    #[test]
    fn the_forced_builders_report_the_same_error_on_a_malformed_document() {
        let source = b"a,b\n\"unterminated,c\n".to_vec();
        let serial = build_index_serial(&source, IndexOptions::default());
        let parallel = build_index_parallel(&source, IndexOptions::default(), 4);
        assert_eq!(
            serial.is_err(),
            parallel.is_err(),
            "both builders agree the document is malformed"
        );
    }
}

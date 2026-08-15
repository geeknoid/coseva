//! Adversarial corpus driven through every parse path, for Miri.
//!
//! The parser reaches structural-scan fast paths implemented by
//! `coseva_unsafe`. Their bounds are argued from the invariant that a
//! structural mask only ever contains positions inside the scanned window.
//! Under Miri every low-level read is validated, turning that argument into an
//! observation.
//!
//! Every one of those fast paths is selected at *run time*, by
//! `is_x86_feature_detected!`. Miri does not execute a real `cpuid`, so under
//! the interpreter that macro reports the compile-time target features — and a
//! default `x86_64-unknown-linux-gnu` build sets neither `avx2` nor `bmi2`.
//! Run plainly, therefore, this file interprets the scalar fallback and says
//! nothing whatever about the vectorized kernels it exists to validate. That
//! is not a deduction: a deliberate one-byte over-read introduced into
//! `load_avx2` passes this target run plainly and fails it under
//! `-C target-feature=+avx2,+bmi2`.
//!
//! So it is run twice, once per dispatch arm, and
//! [`the_dispatch_arm_is_the_one_the_job_asked_for`] makes each run state which
//! arm it got rather than assume it. Without that assertion a flag that stopped
//! taking effect would return the vector run to the scalar path silently, which
//! is exactly the failure being fixed.
//!
//! Run with:
//! `cargo +nightly miri test -p coseva --test miri_unsafe --features std`
//! `COSEVA_MIRI_EXPECT_AVX2=1 RUSTFLAGS="-C target-feature=+avx2,+bmi2" \
//!  cargo +nightly miri test -p coseva --test miri_unsafe --features std`

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{FormatOptions, Headers, Limits, ParseOptions, Recovery, Syntax};
use coseva::{ByteRecord, PushParser, SliceParser};

/// Inputs chosen to land structural bytes at block edges and to drive the
/// quoted-field, escape, and empty-field branches that drive low-level reads.
fn corpus() -> Vec<Vec<u8>> {
    let mut inputs: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"\n".to_vec(),
        b",".to_vec(),
        b",,,,,,,,".to_vec(),
        b"a,b\nc,d\n".to_vec(),
        b"\"a\"\"b\",c\n".to_vec(),
        b"\"multi\nline\",x\r\n".to_vec(),
        b"\"unterminated".to_vec(),
        b"\"closed\"trailing\n".to_vec(),
        b"a\"b,c\n".to_vec(),
        b"\xEF\xBB\xBFa,b\n".to_vec(),
        b"\xff\xfe,\x00\n".to_vec(),
    ];

    // Straddle the 32-byte structural block grid: place a quote, a delimiter,
    // and a terminator at every offset around a block boundary.
    for pad in 28..40 {
        for &marker in b"\",\n" {
            let mut input = vec![b'x'; pad];
            input.push(marker);
            input.extend_from_slice(b"tail,end\n");
            inputs.push(input);
        }
    }

    // A quoted field whose closing quote sits just past a block boundary.
    for pad in 28..40 {
        let mut input = vec![b'"'];
        input.extend(core::iter::repeat_n(b'y', pad));
        input.extend_from_slice(b"\",z\n");
        inputs.push(input);
    }

    // Escaped quotes straddling a boundary.
    for pad in 28..40 {
        let mut input = vec![b'"'];
        input.extend(core::iter::repeat_n(b'z', pad));
        input.extend_from_slice(b"\"\"more\",tail\n");
        inputs.push(input);
    }

    inputs
}

fn formats() -> Vec<FormatOptions> {
    vec![
        FormatOptions::CSV,
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
        FormatOptions::TSV,
    ]
}

#[test]
fn slice_parsing_the_corpus_is_free_of_undefined_behavior() {
    for input in corpus() {
        for format in formats() {
            for headers in [Headers::None, Headers::FirstRecord] {
                let options = ParseOptions::new().headers(headers);
                let Ok(mut parser) = SliceParser::with_options(&input, format, options) else {
                    continue;
                };
                let mut owned = ByteRecord::new();
                while let Ok(Some(mut line)) = parser.next_line() {
                    if let Ok(record) = line.record() {
                        for index in 0..record.len() {
                            let _ = record.get(index);
                        }
                    }
                    let _ = line.read_byte_record_into(&mut owned);
                }
            }
        }
    }
}

#[test]
fn push_parsing_the_corpus_in_small_chunks_is_free_of_undefined_behavior() {
    for input in corpus() {
        for format in formats() {
            for size in [1_usize, 3, 32] {
                let options = ParseOptions::new().headers(Headers::None);
                let Ok(mut parser) = PushParser::with_options(format, options) else {
                    continue;
                };
                let mut record = ByteRecord::new();
                let mut failed = false;
                for bytes in input.chunks(size) {
                    let mut offset = 0;
                    while offset < bytes.len() {
                        let mut lent = parser.chunk(&bytes[offset..]);
                        loop {
                            match lent.next_line() {
                                Ok(Some(mut line)) => {
                                    if line.read_byte_record_into(&mut record).is_err() {
                                        failed = true;
                                        break;
                                    }
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    failed = true;
                                    break;
                                }
                            }
                        }
                        offset += lent.done();
                        if failed {
                            break;
                        }
                    }
                    if failed {
                        break;
                    }
                }
                if !failed {
                    parser.finish();
                    let mut lent = parser.chunk(b"");
                    while let Ok(Some(mut line)) = lent.next_line() {
                        if line.read_byte_record_into(&mut record).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn constrained_limits_do_not_read_out_of_bounds() {
    // Tight limits cut the scan window short, which is exactly where an
    // unsafe read could outrun the bytes it was proven against.
    for input in corpus() {
        for limits in [
            Limits::new(8, 8, 8),
            Limits::new(4, 4, 4),
            Limits::new(1, 1, 1),
        ] {
            let options = ParseOptions::new().headers(Headers::None).limits(limits);
            let Ok(mut parser) = SliceParser::with_options(&input, FormatOptions::CSV, options)
            else {
                continue;
            };
            while let Ok(Some(mut line)) = parser.next_line() {
                if line.record().is_err() {
                    break;
                }
            }
        }
    }
}

/// Which dispatch arm this run is interpreting, asserted rather than assumed.
///
/// `COSEVA_MIRI_EXPECT_AVX2` is set by the CI step that also sets
/// `-C target-feature=+avx2,+bmi2`, so the two must agree. Both directions
/// matter: the plain run failing here would mean it is no longer the scalar
/// baseline it is documented to be, and the vectorized run failing here means
/// the target features stopped reaching the compiler and the job has quietly
/// gone back to interpreting code it already covers.
///
/// `option_env!` rather than `std::env::var` because the thing being checked is
/// a compile-time property; reading it at run time would let a stale binary
/// report the wrong answer.
///
/// `cfg(miri)` because the property is Miri's alone. Run natively this file is
/// an ordinary test target, `is_x86_feature_detected!` executes a real `cpuid`,
/// and the answer describes the host rather than anything the job chose.
#[cfg(all(miri, target_arch = "x86_64"))]
#[test]
fn the_dispatch_arm_is_the_one_the_job_asked_for() {
    let expects_vector = option_env!("COSEVA_MIRI_EXPECT_AVX2").is_some();
    assert_eq!(
        coseva_unsafe::record::default_plain_packed_available(),
        expects_vector,
        "this run asked for the {} arm and got the other one; under Miri the \
         dispatch predicate follows `-C target-feature`, so the flags and \
         `COSEVA_MIRI_EXPECT_AVX2` have to be set together",
        if expects_vector {
            "avx2+bmi2"
        } else {
            "scalar"
        },
    );
}

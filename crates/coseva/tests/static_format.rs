//! `Csv` and `Dynamic` must agree, byte for byte, on every input.
//!
//! The static and run-time-configured parsers share one kernel body and
//! differ only in whether `F::OPTIONS` folds, so this is not a comparison of
//! two implementations: it is the same implementation under two
//! constant-folding regimes. Any divergence means an accessor reads a
//! different setting than the field it stands in for.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::csv_format;
use coseva::format::Csv;
use coseva::{ByteRecord, IoParser, PushParser, SliceParser};

/// Collect every record, so a divergence in field splitting is visible.
fn read_dynamic(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut parser = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .map_err(|error| error.to_string())?;
    collect(&mut parser)
}

fn read_static(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut parser = SliceParser::<Csv>::new(input, ParseOptions::new().headers(Headers::None))
        .map_err(|error| error.to_string())?;
    collect(&mut parser)
}

fn collect<F: coseva::format::CsvFormat>(
    parser: &mut SliceParser<'_, F>,
) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut out = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)
                    .map_err(|error| error.to_string())?;
                out.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            Ok(None) => return Ok(out),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The streaming parser refills its window mid-record, so it exercises the
/// specialized kernels against buffer seams the slice parser never sees. A
/// deliberately small capacity makes those seams frequent.
fn stream_dynamic(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut parser = IoParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(64),
    )
    .map_err(|error| error.to_string())?;
    collect_stream(&mut parser)
}

fn stream_static(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut parser = IoParser::<_, Csv>::new(
        input,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(64),
    )
    .map_err(|error| error.to_string())?;
    collect_stream(&mut parser)
}

fn collect_stream<R: std::io::Read, F: coseva::format::CsvFormat>(
    parser: &mut IoParser<R, F>,
) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut out = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)
                    .map_err(|error| error.to_string())?;
                out.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            Ok(None) => return Ok(out),
            Err(error) => return Err(error.to_string()),
        }
    }
}

/// The push parser is fed in small chunks, so a record is routinely split
/// across feeds. That puts the specialized kernels under a third windowing
/// discipline, distinct from both the slice and streaming parsers.
fn push_dynamic(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let parser = PushParser::with_options(
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .map_err(|error| error.to_string())?;
    collect_push(parser, input)
}

fn push_static(input: &[u8]) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let parser = PushParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
        .map_err(|error| error.to_string())?;
    collect_push(parser, input)
}

fn collect_push<F: coseva::format::CsvFormat>(
    mut parser: PushParser<F>,
    input: &[u8],
) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut out = Vec::new();
    for bytes in input.chunks(7) {
        let mut fed = 0;
        while fed < bytes.len() {
            fed += drain_push(&mut parser, &bytes[fed..], &mut out)?;
        }
    }
    parser.finish();
    let _ = drain_push(&mut parser, b"", &mut out)?;
    Ok(out)
}

/// Lend one chunk, collect the records it completes, and report the take.
fn drain_push<F: coseva::format::CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    out: &mut Vec<Vec<Vec<u8>>>,
) -> Result<usize, String> {
    let mut chunk = parser.chunk(input);
    loop {
        match chunk.next_line() {
            Ok(Some(mut line)) => {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)
                    .map_err(|error| error.to_string())?;
                out.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            Ok(None) => break,
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(chunk.done())
}

/// Inputs chosen to exercise the branches the accessors gate: delimiters,
/// quotes, CR handling, terminators at and across block boundaries, and the
/// malformed cases that must fail identically.
fn corpus() -> Vec<Vec<u8>> {
    let mut inputs: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"a".to_vec(),
        b"a,b,c".to_vec(),
        b"a,b,c\n".to_vec(),
        b"a,b,c\nd,e,f\n".to_vec(),
        b"a,,c\n,,\n".to_vec(),
        b"\"a\",\"b\"\n".to_vec(),
        b"\"a,b\",c\n".to_vec(),
        b"\"a\"\"b\",c\n".to_vec(),
        b"a\r\nb\r\n".to_vec(),
        b"a,b\r\nc,d\r\n".to_vec(),
        b"\"a\r\nb\",c\n".to_vec(),
        b"a,\"b\nc\",d\n".to_vec(),
        b"\"unterminated,c\n".to_vec(),
        b"a,\"b\"x,c\n".to_vec(),
        b"\n\n\n".to_vec(),
        b"\r\n".to_vec(),
        b"a\rb\n".to_vec(),
    ];

    // Walk a quote and a terminator across a SIMD block boundary, where the
    // scanner's block seam is the interesting case.
    for pad in 0..80_usize {
        let mut wide = b"x".repeat(pad);
        wide.extend_from_slice(b",\"quoted,field\"\nnext,record\n");
        inputs.push(wide);

        let mut late = b"y".repeat(pad);
        late.extend_from_slice(b"\na,b\n");
        inputs.push(late);
    }
    inputs
}

#[test]
fn static_and_dynamic_agree_over_corpus() {
    for input in corpus() {
        let dynamic = read_dynamic(&input);
        let statik = read_static(&input);
        assert_eq!(
            dynamic,
            statik,
            "static and dynamic diverged on {:?}",
            String::from_utf8_lossy(&input)
        );
    }
}

#[test]
fn static_and_dynamic_agree_over_generated_inputs() {
    // A cheap deterministic generator beats a fixed list for seam coverage:
    // it mixes field widths so record and block boundaries land everywhere.
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for _ in 0..2_000 {
        let mut input = Vec::new();
        let records = (next() % 6) + 1;
        for _ in 0..records {
            let fields = (next() % 5) + 1;
            for field in 0..fields {
                if field > 0 {
                    input.push(b',');
                }
                match next() % 4 {
                    0 => input.extend_from_slice(&b"z".repeat((next() % 40) as usize)),
                    1 => input.extend_from_slice(b"\"quoted, value\""),
                    2 => input.extend_from_slice(b"\"esc\"\"aped\""),
                    _ => {}
                }
            }
            input.push(b'\n');
        }
        assert_eq!(
            read_dynamic(&input),
            read_static(&input),
            "diverged on {:?}",
            String::from_utf8_lossy(&input)
        );
    }
}

/// The streaming parser must agree with its dynamic twin too, and with the
/// slice parser, so window seams cannot hide a specialization fault.
#[test]
fn static_and_dynamic_streaming_agree_over_corpus() {
    for input in corpus() {
        assert_eq!(
            stream_static(&input),
            stream_dynamic(&input),
            "static and dynamic streaming diverged on {input:?}"
        );
        assert_eq!(
            stream_static(&input),
            read_static(&input),
            "streaming and slice diverged on {input:?}"
        );
    }
}

/// The push parser must agree with its dynamic twin across chunk seams.
#[test]
fn static_and_dynamic_push_agree_over_corpus() {
    for input in corpus() {
        assert_eq!(
            push_static(&input),
            push_dynamic(&input),
            "static and dynamic push diverged on {input:?}"
        );
    }
}

csv_format! {
    /// A format no built-in covers, declared the way a user would.
    pub Upstream = FormatOptions::CSV.delimiter(b'|').quote(b'\'');
}

/// A user-declared format must specialize as soundly as a built-in one.
///
/// This is the case the public macro exists to serve, and it exercises a
/// delimiter and a quote the built-ins never use, so a kernel that had
/// hard-coded either would be caught here.
#[test]
fn a_custom_format_agrees_with_its_dynamic_twin() {
    let upstream_options = FormatOptions::CSV.delimiter(b'|').quote(b'\'');
    for input in corpus() {
        // The corpus is comma-and-quote shaped, so also feed it translated
        // into the custom format; otherwise the delimiter never appears.
        let translated: Vec<u8> = input
            .iter()
            .map(|&byte| match byte {
                b',' => b'|',
                b'"' => b'\'',
                other => other,
            })
            .collect();
        for candidate in [&input, &translated] {
            let mut dynamic = SliceParser::with_options(
                candidate,
                upstream_options,
                ParseOptions::new().headers(Headers::None),
            )
            .map_err(|error| error.to_string());
            let dynamic = match dynamic.as_mut() {
                Ok(parser) => collect(parser),
                Err(error) => Err(error.clone()),
            };

            let mut statik =
                SliceParser::<Upstream>::new(candidate, ParseOptions::new().headers(Headers::None))
                    .map_err(|error| error.to_string());
            let statik = match statik.as_mut() {
                Ok(parser) => collect(parser),
                Err(error) => Err(error.clone()),
            };

            assert_eq!(statik, dynamic, "custom format diverged on {candidate:?}");
        }
    }
}

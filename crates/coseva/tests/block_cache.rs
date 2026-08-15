//! Streaming parser checks for structural cache invalidation across windows.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{Headers, ParseOptions};
use coseva::format::{Csv, CsvFormat};
use coseva::{ByteRecord, Error, PushParser, SliceParser};

type Shape = (Vec<Vec<u8>>, core::ops::Range<usize>, u64);

fn shape(record: &ByteRecord) -> Shape {
    (
        record.iter().map(<[u8]>::to_vec).collect(),
        record.byte_range(),
        record.index(),
    )
}

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn slice_shapes(input: &[u8], options: ParseOptions) -> Result<Vec<Shape>, Error> {
    let mut parser = SliceParser::<Csv>::new(input, options)?;
    let mut shapes = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        shapes.push(shape(&record));
    }
    Ok(shapes)
}

fn push_shapes<F: CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    size: usize,
) -> Result<Vec<Shape>, Error> {
    let mut shapes = Vec::new();
    let mut start = 0_usize;
    loop {
        let end = start.saturating_add(size).min(input.len());
        let bytes = &input[start..end];
        if end == input.len() {
            parser.finish();
        }
        let mut offset = 0;
        loop {
            let mut chunk = parser.chunk(&bytes[offset..]);
            while let Some(mut line) = chunk.next_line()? {
                let mut record = ByteRecord::new();
                line.read_byte_record_into(&mut record)?;
                shapes.push(shape(&record));
            }
            let taken = chunk.done();
            assert!(
                taken > 0 || offset >= bytes.len(),
                "a chunk that takes nothing cannot make progress"
            );
            offset += taken;
            if offset >= bytes.len() {
                break;
            }
        }
        start = end;
        if start >= input.len() {
            break;
        }
    }
    Ok(shapes)
}

fn push_shapes_one_record_per_chunk<F: CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
    size: usize,
) -> Result<Vec<Shape>, Error> {
    let mut shapes = Vec::new();
    let mut start = 0_usize;
    loop {
        let end = start.saturating_add(size).min(input.len());
        let bytes = &input[start..end];
        if end == input.len() {
            parser.finish();
        }
        let mut chunk = parser.chunk(bytes);
        if let Some(mut line) = chunk.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            shapes.push(shape(&record));
        }
        let taken = chunk.done();
        assert!(
            taken > 0 || start >= input.len(),
            "a chunk that takes nothing cannot make progress"
        );
        start += taken;
        if start >= input.len() {
            break;
        }
    }
    Ok(shapes)
}

fn push_shapes_abandoning_first_chunk<F: CsvFormat>(
    parser: &mut PushParser<F>,
    input: &[u8],
) -> Result<Vec<Shape>, Error> {
    let mut shapes = Vec::new();
    let mut chunk = parser.chunk(input);
    if let Some(mut line) = chunk.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        shapes.push(shape(&record));
    }
    let mut offset = chunk.done();
    assert!(offset > 0, "the abandoned chunk makes progress");

    while offset < input.len() {
        let mut chunk = parser.chunk(&input[offset..]);
        while let Some(mut line) = chunk.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            shapes.push(shape(&record));
        }
        let taken = chunk.done();
        assert!(taken > 0, "each abandoned chunk makes progress");
        offset += taken;
    }
    parser.finish();
    loop {
        let mut chunk = parser.chunk(&[]);
        let before = shapes.len();
        while let Some(mut line) = chunk.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            shapes.push(shape(&record));
        }
        let _ = chunk.done();
        if shapes.len() == before {
            break;
        }
    }
    Ok(shapes)
}

fn hostile_window_document(padding: usize, tail: usize) -> Vec<u8> {
    let mut input = Vec::new();
    input.extend_from_slice(b"r0,c0\n");
    input.extend_from_slice(b"r1,");
    input.extend(std::iter::repeat_n(b'a', padding));
    input.extend_from_slice(b",");
    input.extend(std::iter::repeat_n(b'b', 96));
    input.extend_from_slice(b"\n");
    input.extend_from_slice(b"r2,\"");
    input.extend(std::iter::repeat_n(b'x', 64 + tail));
    input.extend_from_slice(b",");
    input.extend(std::iter::repeat_n(b'y', 64));
    input.extend_from_slice(b"\"\n");
    input.extend_from_slice(b"r3,");
    input.extend(std::iter::repeat_n(b'z', 128 + padding));
    input.extend_from_slice(b"\n");
    input
}

fn push_parser_reassembles_records_after_structural_cache_window_shifts() -> Result<(), Error> {
    for padding in 0..64 {
        for tail in [0, 1, 7, 31, 32, 33] {
            let input = hostile_window_document(padding, tail);
            let want = slice_shapes(&input, options())?;
            for size in [1, 3, 32] {
                let mut parser = PushParser::<Csv>::new(options())?;
                let got = push_shapes(&mut parser, &input, size)?;
                assert_eq!(
                    got, want,
                    "padding {padding}, tail {tail}, chunk size {size}"
                );

                let mut parser = PushParser::<Csv>::new(options())?;
                let got = push_shapes_one_record_per_chunk(&mut parser, &input, size)?;
                assert_eq!(
                    got, want,
                    "padding {padding}, tail {tail}, one-record chunk size {size}"
                );
            }

            #[cfg(feature = "test-util")]
            {
                let mut parser = PushParser::<Csv>::new(options().force_general_parser(true))?;
                let got = push_shapes(&mut parser, &input, 32)?;
                assert_eq!(got, want, "padding {padding}, tail {tail}, general");

                let mut parser = PushParser::<Csv>::new(options().force_general_parser(true))?;
                let got = push_shapes_one_record_per_chunk(&mut parser, &input, 32)?;
                assert_eq!(
                    got, want,
                    "padding {padding}, tail {tail}, one-record general"
                );
            }
        }
    }
    Ok(())
}

#[test]
fn push_parser_reassembles_records_after_structural_cache_window_shifts_csv() -> Result<(), Error> {
    push_parser_reassembles_records_after_structural_cache_window_shifts()
}

#[test]
fn abandoned_large_chunks_do_not_reuse_stale_structural_masks() -> Result<(), Error> {
    let input = b"aaaaa,bbbbb\n12,345678901234567890\nxx,yy\n";
    let want = slice_shapes(input, options())?;
    let mut parser = PushParser::<Csv>::new(options())?;
    let got = push_shapes_abandoning_first_chunk(&mut parser, input)?;
    assert_eq!(got, want);
    Ok(())
}

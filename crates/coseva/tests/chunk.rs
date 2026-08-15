//! Tests for the borrowing chunk interface of the push parser.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{
    FieldCount, FormatOptions, Headers, Limits, ParseOptions, ReadBom, Recovery, Syntax,
};
use coseva::format::{Csv, CsvFormat};
use coseva::{ByteRecord, Chunk, Error, ErrorKind, IoParser, Predicate, PushParser, SliceParser};
use std::io::{self, Read};

/// One record as the tests compare them: fields, extent, and index.
type Shape = (Vec<Vec<u8>>, core::ops::Range<usize>, u64);

fn shape(record: &ByteRecord) -> Shape {
    (
        record.iter().map(<[u8]>::to_vec).collect(),
        record.byte_range(),
        record.index(),
    )
}

/// Read `input` through the chunk interface, cut into chunks of `size` bytes.
///
/// The stream is declared complete before the last chunk, so an unterminated
/// final record is reported like every other one.
fn chunked<F: CsvFormat>(
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

/// The same records read from the whole input at once.
fn whole(input: &[u8], options: ParseOptions) -> Result<Vec<Shape>, Error> {
    let mut parser = SliceParser::<Csv>::new(input, options)?;
    let mut shapes = Vec::new();
    while let Some(mut line) = parser.next_line()? {
        let mut record = ByteRecord::new();
        line.read_byte_record_into(&mut record)?;
        shapes.push(shape(&record));
    }
    Ok(shapes)
}

/// The first field of the next record, if the chunk holds one.
fn first_field<F: CsvFormat>(chunk: &mut Chunk<'_, '_, F>) -> Option<Vec<u8>> {
    let mut line = chunk.next_line().expect("records parse")?;
    let record = line.record().expect("record parses");
    Some(record.get(0).unwrap_or_default().to_vec())
}

fn unheaded() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn fields(shapes: &[Shape]) -> Vec<Vec<Vec<u8>>> {
    shapes.iter().map(|(fields, ..)| fields.clone()).collect()
}

#[test]
fn records_aligned_with_chunk_boundaries_are_all_reported() {
    let input = b"a,1\nb,2\nc,3\n";
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    let shapes = chunked(&mut parser, input, 4).expect("records parse");
    assert_eq!(
        fields(&shapes),
        vec![
            vec![b"a".to_vec(), b"1".to_vec()],
            vec![b"b".to_vec(), b"2".to_vec()],
            vec![b"c".to_vec(), b"3".to_vec()],
        ]
    );
    assert!(parser.is_done());
}

#[test]
fn a_record_split_across_two_chunks_is_reassembled() {
    let input = b"alpha,beta\ngamma,delta\n";
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");

    let mut shapes = Vec::new();
    let mut chunk = parser.chunk(&input[..14]);
    while let Some(mut line) = chunk.next_line().expect("records parse") {
        shapes.push(
            line.record()
                .expect("record parses")
                .get(0)
                .map(<[u8]>::to_vec),
        );
    }
    assert_eq!(chunk.done(), 14);
    assert_eq!(shapes, vec![Some(b"alpha".to_vec())]);

    parser.finish();
    let mut chunk = parser.chunk(&input[14..]);
    while let Some(mut line) = chunk.next_line().expect("records parse") {
        shapes.push(
            line.record()
                .expect("record parses")
                .get(0)
                .map(<[u8]>::to_vec),
        );
    }
    assert_eq!(chunk.done(), input.len() - 14);
    assert_eq!(
        shapes,
        vec![Some(b"alpha".to_vec()), Some(b"gamma".to_vec())]
    );
}

#[test]
fn a_record_split_across_three_chunks_keeps_its_extent() {
    let input = b"one,two,three\nfour,five,six\n";
    let expected = whole(input, unheaded()).expect("records parse");

    // Cut the second record twice, so its middle chunk completes nothing.
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    let mut shapes = Vec::new();
    for (index, bounds) in [(0, 0..17), (1, 17..21), (2, 21..input.len())]
        .into_iter()
        .enumerate()
        .map(|(index, (_, bounds))| (index, bounds))
    {
        if index == 2 {
            parser.finish();
        }
        let mut chunk = parser.chunk(&input[bounds.clone()]);
        while let Some(mut line) = chunk.next_line().expect("records parse") {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record).expect("record");
            shapes.push(shape(&record));
        }
        assert_eq!(chunk.done(), bounds.len());
    }
    assert_eq!(shapes, expected);
}

#[test]
fn a_quoted_record_ending_split_across_chunks_stays_one_field() {
    let input = b"\"a\nb\",second\nthird,fourth\n";
    let expected = whole(input, unheaded()).expect("records parse");
    for size in 1..=input.len() {
        let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
        let shapes = chunked(&mut parser, input, size).expect("records parse");
        assert_eq!(shapes, expected, "chunk size {size}");
    }
}

#[test]
fn a_chunk_holding_no_whole_record_reports_nothing() {
    let input = b"city,pop\n";
    let mut parser =
        PushParser::with_options(FormatOptions::CSV, unheaded()).expect("valid options");
    let mut chunk = parser.chunk(&input[..5]);
    assert!(chunk.next_line().expect("no record").is_none());
    assert_eq!(chunk.done(), 5);
    assert!(!parser.is_done());

    parser.finish();
    let mut chunk = parser.chunk(&input[5..]);
    assert_eq!(first_field(&mut chunk).as_deref(), Some(&b"city"[..]));
    assert!(chunk.next_line().expect("no record").is_none());
    assert_eq!(chunk.done(), 4);
}

#[test]
fn empty_chunks_are_accepted_and_change_nothing() {
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    for _ in 0..3 {
        let mut chunk = parser.chunk(b"");
        assert!(chunk.next_line().expect("no record").is_none());
        assert_eq!(chunk.done(), 0);
    }

    let mut chunk = parser.chunk(b"a,b\nc,");
    let mut seen = Vec::new();
    while let Some(mut line) = chunk.next_line().expect("records parse") {
        seen.push(line.record().expect("record").get(0).map(<[u8]>::to_vec));
    }
    assert_eq!(chunk.done(), 6);
    assert_eq!(seen, vec![Some(b"a".to_vec())]);

    // An empty chunk before the stream ends still reports nothing, and one
    // after `finish` releases the unterminated tail.
    let mut chunk = parser.chunk(b"");
    assert!(chunk.next_line().expect("no record").is_none());
    assert_eq!(chunk.done(), 0);

    parser.finish();
    let mut chunk = parser.chunk(b"");
    assert_eq!(first_field(&mut chunk).as_deref(), Some(&b"c"[..]));
    assert_eq!(chunk.done(), 0);
    assert!(parser.is_done());
}

#[test]
fn a_byte_order_mark_split_across_chunks_is_stripped() {
    let input = b"\xEF\xBB\xBFa,b\nc,d\n";
    let expected = whole(input, unheaded()).expect("records parse");
    assert_eq!(
        fields(&expected),
        vec![
            vec![b"a".to_vec(), b"b".to_vec()],
            vec![b"c".to_vec(), b"d".to_vec()],
        ]
    );
    for size in 1..=input.len() {
        let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
        let shapes = chunked(&mut parser, input, size).expect("records parse");
        assert_eq!(shapes, expected, "chunk size {size}");
    }
}

#[test]
fn a_rejected_byte_order_mark_split_across_chunks_still_fails() {
    let input = b"\xEF\xBB\xBFa,b\n";
    for size in 1..=input.len() {
        let format = FormatOptions::CSV.read_bom(ReadBom::Reject);
        let mut parser = PushParser::with_options(format, unheaded()).expect("valid options");
        let error = chunked(&mut parser, input, size).expect_err("the mark is rejected");
        assert_eq!(error.kind(), ErrorKind::RejectedBom, "chunk size {size}");
    }
}

#[test]
fn headers_arriving_in_the_first_chunk_are_available_afterwards() {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let mut chunk = parser.chunk(b"city,pop\nBoston,650706\nLondon,1\n");
    assert_eq!(first_field(&mut chunk).as_deref(), Some(&b"Boston"[..]));
    assert_eq!(chunk.done(), 32);

    assert!(parser.has_headers());
    assert_eq!(parser.header_index("pop"), Some(1));
    assert_eq!(
        parser.headers().map(ByteRecord::len),
        Some(2),
        "the header record survives the chunk it arrived in"
    );
}

#[test]
fn headers_split_across_chunks_are_still_discovered() {
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
    let shapes = chunked(&mut parser, b"city,pop\nBoston,650706\n", 3).expect("records parse");
    assert_eq!(
        fields(&shapes),
        vec![vec![b"Boston".to_vec(), b"650706".to_vec()]]
    );
    assert_eq!(parser.header_index("city"), Some(0));
}

#[test]
fn every_chunk_size_agrees_with_the_slice_parser() {
    let input: &[u8] = b"city,pop,note\r\n\
Boston,650706,ok\r\n\
London,8982000,\"quoted, with \"\"escapes\"\"\nand a newline\"\r\n\
Paris,2148000,\r\n\
,,\r\n\
Tokyo,13960000,last";
    let expected = whole(input, ParseOptions::new()).expect("records parse");
    assert_eq!(expected.len(), 5);
    for size in 1..=input.len().saturating_add(4) {
        let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
        let shapes = chunked(&mut parser, input, size).expect("records parse");
        assert_eq!(shapes, expected, "chunk size {size}");
    }
}

#[test]
fn a_dropped_chunk_settles_the_parser_the_same_way() {
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    {
        let mut chunk = parser.chunk(b"a,1\nb,");
        let mut line = chunk.next_line().expect("record").expect("one record");
        assert_eq!(line.record().expect("record").get(0), Some(&b"a"[..]));
        // No `done`: the guard has to hand the tail back on its own.
    };
    parser.finish();
    let mut chunk = parser.chunk(b"2\n");
    let mut line = chunk.next_line().expect("record").expect("one record");
    let record = line.record().expect("record");
    assert_eq!(record.get(0), Some(&b"b"[..]));
    assert_eq!(record.get(1), Some(&b"2"[..]));
}

#[test]
fn a_record_beyond_the_limit_is_reported_once_it_cannot_grow() {
    let limits = Limits::new(8, 8, 8);
    let mut parser = PushParser::<Csv>::new(unheaded().limits(limits)).expect("parser");
    let input = vec![b'x'; 1024];
    let mut offset = 0;
    let error = loop {
        let mut chunk = parser.chunk(&input[offset..]);
        match chunk.next_line() {
            Ok(line) => assert!(line.is_none(), "no record can be completed"),
            Err(error) => break error,
        }
        offset += chunk.done();
        assert!(offset < input.len(), "the limit has to be reported");
    };
    assert_eq!(error.kind(), ErrorKind::RecordTooLarge { limit: 8 });
}

#[test]
fn abandoning_a_large_chunk_keeps_the_parser_within_the_record_limit() {
    // The tail left by a caller that stops reading is not bounded by a record,
    // so the parser must take it a record limit at a time rather than letting
    // the caller's chunk size decide how much it holds.
    let limits = Limits::new(64, 64, 64);
    let mut parser = PushParser::<Csv>::new(unheaded().limits(limits)).expect("parser");
    let mut input = Vec::new();
    for index in 0..4096 {
        input.extend_from_slice(format!("{index},value\n").as_bytes());
    }

    let mut chunk = parser.chunk(&input);
    chunk.next_line().expect("record").expect("one record");
    let consumed = chunk.done();

    assert!(
        consumed < input.len(),
        "an abandoned chunk must be handed back for a further round, took {consumed} of {}",
        input.len()
    );

    // Whatever was retained has to sit inside what the limit allows, or the
    // limit stops describing the parser's footprint at all.
    let mut offset = consumed;
    let mut rounds = 0;
    while offset < input.len() {
        let mut chunk = parser.chunk(&input[offset..]);
        while chunk.next_line().expect("record").is_some() {}
        let taken = chunk.done();
        assert!(taken > 0, "each round has to make progress");
        offset += taken;
        rounds += 1;
        assert!(rounds < input.len(), "the rounds have to terminate");
    }
    parser.finish();
}

#[test]
fn filtering_over_chunks_matches_filtering_over_the_whole_input() {
    let input = b"city,pop\nBoston,650706\nLondon,8982000\nBoston,1\n";
    let predicate = Predicate::equals("city", "Boston");
    for size in 1..=input.len() {
        let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
        let mut matched = Vec::new();
        let mut start = 0_usize;
        while start < input.len() {
            let end = start.saturating_add(size).min(input.len());
            if end == input.len() {
                parser.finish();
            }
            let mut chunk = parser.chunk(&input[start..end]);
            while let Some(mut line) = chunk.next_matching_line(&predicate).expect("records parse")
            {
                matched.push(line.record().expect("record").get(1).map(<[u8]>::to_vec));
            }
            let taken = chunk.done();
            assert_eq!(taken, end - start);
            start = end;
        }
        assert_eq!(
            matched,
            vec![Some(b"650706".to_vec()), Some(b"1".to_vec())],
            "chunk size {size}"
        );
    }
}

/// A first chunk that is exactly the header record, with nothing of the
/// first data record following it, exercises the borrowed literal skip
/// before headers are known: the skip must not mistake the still-unconsumed
/// header for a rejected data record and search past it, or header discovery
/// never runs and every predicate lookup on a named column fails.
#[test]
fn a_first_chunk_that_is_exactly_the_header_does_not_let_the_literal_skip_run_early() {
    let header = b"city,pop\n";
    let rest = b"Boston,650706\nLondon,8982000\n";
    let predicate = Predicate::equals("city", "Boston");
    let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");

    let mut chunk = parser.chunk(header);
    assert!(
        chunk
            .next_matching_line(&predicate)
            .expect("records parse")
            .is_none(),
        "the header alone completes no data record"
    );
    let taken = chunk.done();
    assert_eq!(taken, header.len());

    parser.finish();
    let mut chunk = parser.chunk(rest);
    let mut line = chunk
        .next_matching_line(&predicate)
        .expect("records parse")
        .expect("Boston still matches");
    assert_eq!(line.record().expect("record").get(1), Some(&b"650706"[..]));
    assert!(
        chunk
            .next_matching_line(&predicate)
            .expect("records parse")
            .is_none(),
        "London does not match"
    );
    let _ = chunk.done();
}

#[test]
fn a_parser_reused_after_reset_reads_chunks_from_the_start() {
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    let first = chunked(&mut parser, b"a,1\nb,2\n", 3).expect("records parse");
    parser.reset();
    let second = chunked(&mut parser, b"a,1\nb,2\n", 5).expect("records parse");
    assert_eq!(first, second);
}

#[test]
fn the_reported_location_tracks_the_stream_across_chunks() {
    let input = b"a,1\nb,2\nc,3\n";
    let mut parser = PushParser::<Csv>::new(unheaded()).expect("parser");
    assert_eq!(parser.location().byte, 0);

    // Two whole records and the start of a third.
    let mut chunk = parser.chunk(&input[..10]);
    while chunk.next_line().expect("records parse").is_some() {}
    assert_eq!(chunk.done(), 10);
    let at = parser.location();
    assert_eq!(at.byte, 8, "the parser stands at the start of the tail");
    assert_eq!(at.line, 3);
    assert_eq!(at.record, 2);
    assert!(!parser.is_done());

    parser.finish();
    let mut chunk = parser.chunk(&input[10..]);
    assert_eq!(first_field(&mut chunk).as_deref(), Some(&b"c"[..]));
    assert_eq!(chunk.done(), 2);
    assert_eq!(parser.location().byte, input.len());
    assert!(parser.is_done());
}

// ─── Resumable incomplete-record parsing ───────────────────────────────────
//
// A record delivered in tiny chunks must reassemble byte-for-byte the same way
// the slice parser reads it whole, at every possible chunk boundary, and the
// resume checkpoint must never change what is reported — only how much work it
// costs. These tests pin the "what" against the slice-parser oracle across an
// adversarial corpus and every dialect class, including the ones that decline
// the fast resume path and fall back to a full re-parse.

/// A parse result reduced to what a caller can observe: the records read, or
/// the kind and byte of the error that stopped the stream.
type Outcome = Result<Vec<Shape>, (ErrorKind, usize)>;

fn observe(result: Result<Vec<Shape>, Error>) -> Outcome {
    result.map_err(|error| (error.kind(), error.location().byte))
}

/// The whole input read at once through the dynamic slice parser, the oracle.
fn whole_dyn(input: &[u8], format: FormatOptions, options: ParseOptions) -> Outcome {
    observe((|| {
        let mut parser = SliceParser::with_options(input, format, options)?;
        let mut shapes = Vec::new();
        while let Some(mut line) = parser.next_line()? {
            let mut record = ByteRecord::new();
            line.read_byte_record_into(&mut record)?;
            shapes.push(shape(&record));
        }
        Ok(shapes)
    })())
}

/// The same input pushed through the dynamic chunk interface in `size`-byte
/// pieces.
fn chunked_dyn(input: &[u8], size: usize, format: FormatOptions, options: ParseOptions) -> Outcome {
    observe((|| {
        let mut parser = PushParser::with_options(format, options)?;
        chunked(&mut parser, input, size)
    })())
}

/// A reader that hands out at most `size` bytes per `read` call, so the
/// `IoParser`'s refills land on the same boundaries the push arm cuts on.
struct ChunkReader<'a> {
    input: &'a [u8],
    size: usize,
}

impl Read for ChunkReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let taken = self.size.min(buf.len()).min(self.input.len());
        buf[..taken].copy_from_slice(&self.input[..taken]);
        self.input = &self.input[taken..];
        Ok(taken)
    }
}

/// The same input pulled through the streaming front end, refilled `size`
/// bytes at a time.
fn streamed_dyn(
    input: &[u8],
    size: usize,
    format: FormatOptions,
    options: ParseOptions,
) -> Outcome {
    observe((|| {
        let reader = ChunkReader { input, size };
        let mut parser = IoParser::with_options(reader, format, options)?;
        let mut shapes = Vec::new();
        let mut record = ByteRecord::new();
        while let Some(mut line) = parser.next_line()? {
            line.read_byte_record_into(&mut record)?;
            shapes.push(shape(&record));
        }
        Ok(shapes)
    })())
}

/// Assert that every chunk boundary reproduces the oracle for one input,
/// through both incremental front ends.
///
/// The push arm and the `io` arm cut the input identically, so a divergence
/// that only one of them has is a divergence in that front end rather than in
/// the boundary pre-scan they share.
fn assert_every_boundary_matches(input: &[u8], format: FormatOptions, options: &ParseOptions) {
    let expected = whole_dyn(input, format, options.clone());
    for size in 1..=input.len().max(1) {
        let actual = chunked_dyn(input, size, format, options.clone());
        assert_eq!(
            actual,
            expected,
            "chunk size {size} diverged for input {:?} ({format:?})",
            String::from_utf8_lossy(input),
        );

        let streamed = streamed_dyn(input, size, format, options.clone());
        assert_eq!(
            streamed,
            expected,
            "io read size {size} diverged for input {:?} ({format:?})",
            String::from_utf8_lossy(input),
        );
    }
}

/// Run the whole corpus through one format at every boundary.
fn sweep(format: FormatOptions, options: &ParseOptions, corpus: &[&[u8]]) {
    for input in corpus {
        assert_every_boundary_matches(input, format, options);
    }
}

/// Adversarial CSV inputs that exercise the resume state machine and the paths
/// that decline it: embedded terminators and delimiters inside quotes, doubled
/// quotes, empty quoted fields, stray bytes after a close quote, quotes away
/// from a field start, CRLF, multibyte data, and unterminated tails.
const CSV_CORPUS: &[&[u8]] = &[
    b"a,b,c\n",
    b"alpha,beta,gamma\ndelta,epsilon,zeta\n",
    b"\"a\nb\",second\nthird,fourth\n",
    b"\"a,b\",c\n",
    b"\"a\"\"b\",c\n",
    b"\"\",\"\"\n",
    b"\"multi\r\nline\",x\ny,z\n",
    b"one,\"two\",three\nfour,\"five\",six\n",
    b"\"lead\",plain\nplain,\"trail\"\n",
    b"a,b\r\nc,d\r\n",
    b"trailing,no,newline",
    b"\"unterminated,quote\n",
    b"a,\"b\"x,c\n",
    b"x\"y,z\n",
    b"a,b,c\n\nd,e,f\n",
    b"\xE2\x9C\x93,check\ndata,\xE2\x82\xAC\n",
    b",,,\n",
    b"\"\"\"\",x\n",
    b"\"long quoted field that spans more than one structural block of input\",tail\n",
    b"first\n\"second\nwith\nmany\nlines\"\nthird\n",
];

/// Inputs for a dialect whose delimiter and record ending are two bytes each.
///
/// The interesting cases are the ones with a *lone* lead byte — a `|` that no
/// second `|` follows, an `@` that no second `@` follows — and sequences that a
/// chunk boundary can fall inside. Those are what the pre-scan's three
/// tail-confirmation guards exist for: a window that ends mid-sequence has to
/// decline to decide rather than guess.
#[cfg(feature = "multibyte")]
const MULTIBYTE_CORPUS: &[&[u8]] = &[
    b"a||b||c@@",
    b"alpha||beta@@gamma||delta@@",
    b"\"a@@b\"||second@@third||fourth@@",
    b"\"a||b\"||c@@",
    b"a|b||c@@",
    b"a@b||c@@",
    b"\"quoted|\"||x@@",
    b"\"quoted@\"||x@@",
    b"trailing||no||ending",
    b"\"unterminated||quote@@",
    b"a||\"b\"x||c@@",
    b"x\"y||z@@",
    b"||||@@",
    b"\"\"||\"\"@@",
    b"a||b@@@@c||d@@",
    b"ends with a delimiter lead|@@",
    b"ends with an ending lead@",
    b"a||b@",
    b"a||b|",
    b"\"long quoted field spanning more than one structural block\"||tail@@",
];

#[cfg(feature = "multibyte")]
#[test]
fn resume_reproduces_the_oracle_for_multibyte_delimiters_and_endings() {
    // The pre-scan's tail guards (`src/engine/cursor.rs`) are the only code a
    // multi-byte dialect reaches that a single-byte one does not, and no
    // boundary sweep covered them: a chunk cut between the two bytes of a
    // delimiter or a record ending is exactly the window they have to decline.
    let multibyte = FormatOptions::CSV
        .delimiter_sequence(b"||")
        .record_ending_sequence(b"@@");
    sweep(multibyte, &unheaded(), MULTIBYTE_CORPUS);

    // One axis at a time, so a guard that only the delimiter path or only the
    // ending path reaches is still exercised on its own.
    let multibyte_delimiter = FormatOptions::CSV.delimiter_sequence(b"||");
    let single_ending: Vec<Vec<u8>> = MULTIBYTE_CORPUS
        .iter()
        .map(|input| {
            String::from_utf8_lossy(input)
                .replace("@@", "\n")
                .into_bytes()
        })
        .collect();
    for input in &single_ending {
        assert_every_boundary_matches(input, multibyte_delimiter, &unheaded());
    }

    let multibyte_ending = FormatOptions::CSV.record_ending_sequence(b"@@");
    let single_delimiter: Vec<Vec<u8>> = MULTIBYTE_CORPUS
        .iter()
        .map(|input| {
            String::from_utf8_lossy(input)
                .replace("||", ",")
                .into_bytes()
        })
        .collect();
    for input in &single_delimiter {
        assert_every_boundary_matches(input, multibyte_ending, &unheaded());
    }

    // A two-byte separator has a one-byte tail, so every tail comparison is a
    // single-byte one and a rule that only ever inspects the tail's first byte
    // is indistinguishable from one that compares all of it. Three-byte
    // separators tell the two apart, and they are also the only widths where a
    // chunk can land *inside* the tail rather than merely before it.
    let wide = FormatOptions::CSV
        .delimiter_sequence(b"<->")
        .record_ending_sequence(b"[==]");
    let wide_corpus: Vec<Vec<u8>> = MULTIBYTE_CORPUS
        .iter()
        .map(|input| {
            String::from_utf8_lossy(input)
                .replace("||", "<->")
                .replace("@@", "[==]")
                .into_bytes()
        })
        .collect();
    for input in &wide_corpus {
        assert_every_boundary_matches(input, wide, &unheaded());
    }
}

#[test]
fn resume_reproduces_the_oracle_for_default_csv_at_every_boundary() {
    sweep(FormatOptions::CSV, &unheaded(), CSV_CORPUS);
}

#[test]
fn resume_reproduces_the_oracle_with_discovered_headers() {
    sweep(FormatOptions::CSV, &ParseOptions::new(), CSV_CORPUS);
}

#[test]
fn resume_reproduces_the_oracle_for_tab_and_semicolon_and_pipe() {
    // Eligible non-comma dialects: the delimiter changes but the resume rules
    // are the same. Re-spell the comma corpus into each.
    for (format, delimiter) in [
        (FormatOptions::TSV, b'\t'),
        (FormatOptions::SEMICOLON, b';'),
        (FormatOptions::PIPE, b'|'),
    ] {
        let corpus: Vec<Vec<u8>> = CSV_CORPUS
            .iter()
            .map(|input| {
                input
                    .iter()
                    .map(|&byte| if byte == b',' { delimiter } else { byte })
                    .collect()
            })
            .collect();
        for input in &corpus {
            assert_every_boundary_matches(input, format, &unheaded());
        }
    }
}

#[test]
fn resume_reproduces_the_oracle_across_dialects() {
    // Almost every dialect now resumes: backslash and MySQL and unquoted
    // escapes, CRLF-mandatory endings, skip-initial-space, and Postgres NULLs.
    // Only comments and blank-skip still decline and fall back to the unchanged
    // full re-parse. All must match the oracle at every boundary.
    for format in [
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::MYSQL,
        FormatOptions::RFC4180,
        FormatOptions::COMMENTED_CSV,
        FormatOptions::PYTHON_CSV,
        FormatOptions::PYTHON_ESCAPED,
        FormatOptions::POSTGRES_COPY_CSV,
    ] {
        sweep(format, &unheaded(), CSV_CORPUS);
    }
}

#[test]
fn resume_reproduces_the_oracle_under_tight_limits() {
    // A record or field that overruns its limit must report the same error at
    // the same byte no matter how the window grew to reveal it.
    let options = unheaded().limits(Limits::new(12, 6, 4));
    sweep(FormatOptions::CSV, &options, CSV_CORPUS);
}

#[test]
fn resume_reproduces_the_oracle_under_a_fixed_field_count() {
    let options = unheaded().field_count(FieldCount::Exact(3));
    sweep(FormatOptions::CSV, &options, CSV_CORPUS);
}

#[test]
fn resume_reproduces_the_oracle_with_compatible_recovery() {
    // Compatible recovery relaxes quote handling, which the resume scan
    // declines; the full parse still owns the answer.
    let format = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE));
    sweep(format, &unheaded(), CSV_CORPUS);
}

#[test]
fn resume_keeps_a_long_record_delivered_one_byte_at_a_time_correct() {
    // A single record whose quoted field is far larger than any window forces
    // the resume path to carry its checkpoint across thousands of one-byte
    // growths. Quadratic re-parsing would make this unbearably slow; a linear
    // resume returns immediately. Correctness is asserted against the oracle.
    let mut input = Vec::new();
    input.extend_from_slice(b"\"");
    for index in 0..20_000_u32 {
        // Embed terminators, delimiters, and doubled quotes inside the quotes
        // so the scan must track quoting across the whole field.
        match index % 7 {
            0 => input.extend_from_slice(b"\n"),
            1 => input.extend_from_slice(b","),
            2 => input.extend_from_slice(b"\"\""),
            _ => input.push(b'a' + u8::try_from(index % 26).expect("fits")),
        }
    }
    input.extend_from_slice(b"\",tail\nnext,row\n");

    let expected = whole_dyn(&input, FormatOptions::CSV, unheaded());
    for size in [1_usize, 2, 3, 7, 32, 64, 4096] {
        let actual = chunked_dyn(&input, size, FormatOptions::CSV, unheaded());
        assert_eq!(actual, expected, "chunk size {size}");
    }
    assert!(matches!(expected, Ok(ref shapes) if shapes.len() == 2));
}

/// Push one long adversarial record for `format` through the chunk interface at
/// several chunk sizes and assert every one reproduces the oracle. One field
/// far larger than any window carries that dialect's structural bytes, so the
/// resume checkpoint must survive across the growths. Quadratic re-parsing
/// would make the one-byte case crawl; the `shapes.len() >= 2` check keeps a
/// corpus that stops before the long field ends from passing by accident.
fn assert_long_record_chunks(format: FormatOptions, input: &[u8]) {
    let expected = whole_dyn(input, format, unheaded());
    for size in [1_usize, 32, 4096] {
        let actual = chunked_dyn(input, size, format, unheaded());
        assert_eq!(actual, expected, "format={format:?} chunk size {size}");
    }
    assert!(
        matches!(expected, Ok(ref shapes) if shapes.len() >= 2),
        "format={format:?} corpus must parse as a long record plus a tail",
    );
}

#[test]
fn resume_keeps_long_records_of_every_resuming_dialect_correct() {
    // The push short-circuit and the generalized engine resume together must
    // keep a kilobytes-long record linear and correct for every dialect family
    // that now resumes, not just strict CSV. Each corpus buries the dialect's
    // structural bytes inside one field so the scan tracks quoting and escape
    // state across the whole thing.
    let reps = 8_000_u32;
    let letter = |index: u32| b'a' + u8::try_from(index % 26).expect("fits");

    // CRLF endings, doubled-quote escapes.
    let mut crlf = vec![b'"'];
    for index in 0..reps {
        match index % 7 {
            0 => crlf.extend_from_slice(b"\r\n"),
            1 => crlf.push(b','),
            2 => crlf.extend_from_slice(b"\"\""),
            _ => crlf.push(letter(index)),
        }
    }
    crlf.extend_from_slice(b"\",tail\r\nnext,row\r\n");
    assert_long_record_chunks(FormatOptions::RFC4180, &crlf);

    // Backslash escapes inside a quoted field.
    let mut backslash = vec![b'"'];
    for index in 0..reps {
        match index % 7 {
            0 => backslash.extend_from_slice(b"\\\""),
            1 => backslash.extend_from_slice(b"\\\\"),
            2 => backslash.push(b','),
            3 => backslash.push(b'\n'),
            _ => backslash.push(letter(index)),
        }
    }
    backslash.extend_from_slice(b"\",tail\nnext,row\n");
    assert_long_record_chunks(FormatOptions::BACKSLASH_CSV, &backslash);

    // MySQL: unquoted, tab-delimited, backslash-escaped structural bytes.
    let mut mysql = Vec::new();
    for index in 0..reps {
        match index % 7 {
            0 => mysql.extend_from_slice(b"\\\t"),
            1 => mysql.extend_from_slice(b"\\\n"),
            2 => mysql.extend_from_slice(b"\\\\"),
            _ => mysql.push(letter(index)),
        }
    }
    mysql.extend_from_slice(b"\ttail\nnext\trow\n");
    assert_long_record_chunks(FormatOptions::MYSQL, &mysql);

    // Python QUOTE_NONE, comma-delimited, backslash escapechar.
    let mut unquoted = Vec::new();
    for index in 0..reps {
        match index % 7 {
            0 => unquoted.extend_from_slice(b"\\,"),
            1 => unquoted.extend_from_slice(b"\\\n"),
            2 => unquoted.extend_from_slice(b"\\\\"),
            _ => unquoted.push(letter(index)),
        }
    }
    unquoted.extend_from_slice(b",tail\nnext,row\n");
    assert_long_record_chunks(FormatOptions::PYTHON_ESCAPED, &unquoted);

    // skip-initial-space: the long quoted field opens after a delimiter and
    // spaces.
    let mut trim = vec![b'a', b',', b' ', b' ', b'"'];
    for index in 0..reps {
        match index % 5 {
            0 => trim.push(b'\n'),
            1 => trim.push(b','),
            2 => trim.extend_from_slice(b"\"\""),
            _ => trim.push(letter(index)),
        }
    }
    trim.extend_from_slice(b"\"\nnext\n");
    assert_long_record_chunks(FormatOptions::PYTHON_CSV, &trim);

    // Compatible recovery: quoting with permits for unquoted quotes and
    // trailing whitespace after a close quote.
    let recovery_format = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE));
    let mut recovery = vec![b'"'];
    for index in 0..reps {
        match index % 5 {
            0 => recovery.push(b'\n'),
            1 => recovery.push(b','),
            2 => recovery.extend_from_slice(b"\"\""),
            _ => recovery.push(letter(index)),
        }
    }
    recovery.extend_from_slice(b"\"   ,pla\"in,tail\nnext,row\n");
    assert_long_record_chunks(recovery_format, &recovery);
}

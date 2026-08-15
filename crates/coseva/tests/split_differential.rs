//! Split-at-every-boundary differential tests for the incremental parsers.
//!
//! [`SliceParser`] sees a whole document at once and is the reference. The
//! streaming and push parsers see it in pieces, and a record, a field, a
//! quoted field, or a `\r\n` pair can be cut in half by a chunk boundary. The
//! tests below feed a corpus of straddle-hostile documents to both incremental
//! parsers split at *every* byte position and assert the observed records and
//! errors match the reference exactly.
//!
//! `differential.rs` covers random inputs at a handful of chunk sizes. This
//! file is the complement: hand-picked documents where the interesting split
//! points are, exercised exhaustively.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::io::{self, Read};

use coseva::config::{FormatOptions, Headers, ParseOptions, Recovery, Syntax};
use coseva::{ByteRecord, Chunk, IoParser, PushParser, SliceParser};

/// One observable parser event, comparable across the three parsers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Record(Vec<Vec<u8>>),
    Failure(String, usize, u64, u64),
    End,
}

fn options() -> ParseOptions {
    ParseOptions::new().headers(Headers::None)
}

fn failure(error: &coseva::Error) -> Event {
    let at = error.location();
    Event::Failure(format!("{:?}", error.kind()), at.byte, at.line, at.record)
}

/// The reference: parse the whole document from a slice.
fn slice_events(input: &[u8], format: FormatOptions) -> Vec<Event> {
    let mut parser =
        SliceParser::with_options(input, format, options()).expect("valid configuration");
    let mut events = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => match line.record() {
                Ok(record) => {
                    events.push(Event::Record(record.iter().map(<[u8]>::to_vec).collect()));
                }
                Err(error) => {
                    events.push(failure(&error));
                    return events;
                }
            },
            Ok(None) => {
                events.push(Event::End);
                return events;
            }
            Err(error) => {
                events.push(failure(&error));
                return events;
            }
        }
    }
}

/// A reader that hands out one queued chunk per `read` call.
///
/// Empty chunks are dropped rather than returned: `Ok(0)` from a `Read` means
/// end of input, so returning one mid-stream would be a contract violation
/// rather than a parser test.
struct ChunkReader {
    chunks: Vec<Vec<u8>>,
    index: usize,
}

impl ChunkReader {
    fn new(chunks: Vec<Vec<u8>>) -> Self {
        Self {
            chunks: chunks
                .into_iter()
                .filter(|chunk| !chunk.is_empty())
                .collect(),
            index: 0,
        }
    }
}

impl Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.index >= self.chunks.len() {
            return Ok(0);
        }
        let chunk = &self.chunks[self.index];
        let taken = chunk.len().min(buf.len());
        buf[..taken].copy_from_slice(&chunk[..taken]);
        if taken < chunk.len() {
            self.chunks[self.index] = chunk[taken..].to_vec();
        } else {
            self.index += 1;
        }
        Ok(taken)
    }
}

fn streaming_events(chunks: Vec<Vec<u8>>, format: FormatOptions, capacity: usize) -> Vec<Event> {
    let mut parser = IoParser::with_options(
        ChunkReader::new(chunks),
        format,
        options().buffer_capacity(capacity),
    )
    .expect("valid configuration");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    loop {
        match parser.next_line() {
            Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                Ok(()) => events.push(Event::Record(record.iter().map(<[u8]>::to_vec).collect())),
                Err(error) => {
                    events.push(failure(&error));
                    return events;
                }
            },
            Ok(None) => {
                events.push(Event::End);
                return events;
            }
            Err(error) => {
                events.push(failure(&error));
                return events;
            }
        }
    }
}

/// Read at most one record out of a lent chunk.
///
/// Returns `Some(true)` for a record, `Some(false)` for a failure, and `None`
/// when the chunk completes no further record.
fn next_event(
    chunk: &mut Chunk<'_, '_>,
    events: &mut Vec<Event>,
    record: &mut ByteRecord,
) -> Option<bool> {
    match chunk.next_line() {
        Ok(Some(mut line)) => match line.read_byte_record_into(record) {
            Ok(()) => {
                events.push(Event::Record(record.iter().map(<[u8]>::to_vec).collect()));
                Some(true)
            }
            Err(error) => {
                events.push(failure(&error));
                Some(false)
            }
        },
        Ok(None) => None,
        Err(error) => {
            events.push(failure(&error));
            Some(false)
        }
    }
}

/// Replay `chunks` through a push parser, lending each one afresh for every
/// record it completes.
///
/// [`chunk_events`] drains a whole loan at a time. Ending the loan after every
/// single record instead makes the parser settle and re-borrow between records
/// rather than only at a chunk boundary, which is the path a caller that reads
/// one record per wake-up takes.
fn push_events(chunks: &[&[u8]], format: FormatOptions) -> Vec<Event> {
    let mut parser = PushParser::with_options(format, options()).expect("valid configuration");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    let mut failed = false;

    'outer: for bytes in chunks {
        let mut offset = 0;
        loop {
            let mut lent = parser.chunk(&bytes[offset..]);
            let outcome = next_event(&mut lent, &mut events, &mut record);
            offset += lent.done();
            if outcome == Some(false) {
                failed = true;
                break 'outer;
            }
            if outcome.is_none() || offset >= bytes.len() {
                break;
            }
        }
    }

    if !failed {
        parser.finish();
        let mut lent = parser.chunk(b"");
        if drain_chunk(&mut lent, &mut events, &mut record) {
            events.push(Event::End);
        }
    }
    events
}

/// Drain every record a lent chunk can produce.
///
/// Returns `false` once a failure has been recorded, so the caller stops
/// offering chunks.
fn drain_chunk(
    chunk: &mut Chunk<'_, '_>,
    events: &mut Vec<Event>,
    record: &mut ByteRecord,
) -> bool {
    loop {
        match chunk.next_line() {
            Ok(Some(mut line)) => match line.read_byte_record_into(record) {
                Ok(()) => {
                    events.push(Event::Record(record.iter().map(<[u8]>::to_vec).collect()));
                }
                Err(error) => {
                    events.push(failure(&error));
                    return false;
                }
            },
            Ok(None) => return true,
            Err(error) => {
                events.push(failure(&error));
                return false;
            }
        }
    }
}

fn chunk_events(chunks: &[&[u8]], format: FormatOptions) -> Vec<Event> {
    let mut parser = PushParser::with_options(format, options()).expect("valid configuration");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    let mut failed = false;

    'outer: for bytes in chunks {
        let mut offset = 0;
        loop {
            let mut lent = parser.chunk(&bytes[offset..]);
            if !drain_chunk(&mut lent, &mut events, &mut record) {
                failed = true;
                break 'outer;
            }
            let taken = lent.done();
            assert!(
                taken > 0 || offset >= bytes.len(),
                "a chunk that takes nothing cannot make progress"
            );
            offset += taken;
            if offset >= bytes.len() {
                break;
            }
        }
    }

    if !failed {
        parser.finish();
        let mut lent = parser.chunk(b"");
        if drain_chunk(&mut lent, &mut events, &mut record) {
            events.push(Event::End);
        }
    }
    events
}

/// Documents chosen so that some split point falls in a hazardous place.
fn corpus() -> Vec<&'static [u8]> {
    vec![
        b"",
        b"\n",
        b"\r\n",
        b"\r",
        b"\n\n",
        b"a\n",
        b"a\r\n",
        b"a,b\r\nc,d",
        b"\"a\"",
        b"\"a,b\"\n",
        b"\"a\nb\"\n",
        b"\"a\r\nb\"\n",
        b"\"a\"\"b\",c\n",
        b"\"\"\"\"\n",
        b"\xEF\xBB\xBFa,b\n",
        b"\xEF\xBB\xBF",
        b"a,b\nc,d,e\n",
        b"\"unterminated",
        b"\"closed\"x\n",
        b"a\"b\n",
        b"a,\"b\nc\",d\ne,f\n",
        b"\"quoted, field\",\"with \"\"quotes\"\"\",plain\n",
    ]
}

fn formats() -> [FormatOptions; 2] {
    [
        FormatOptions::CSV,
        FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE)),
    ]
}

#[test]
fn streaming_split_at_every_boundary_matches_slice() {
    for &input in &corpus() {
        for format in formats() {
            let expected = slice_events(input, format);
            for split in 0..=input.len() {
                let chunks = vec![input[..split].to_vec(), input[split..].to_vec()];
                // Tiny capacities force compaction and window growth on almost
                // every byte, so a mis-rebased offset would surface here.
                for capacity in [1_usize, 2, 3, 8, 64] {
                    assert_eq!(
                        streaming_events(chunks.clone(), format, capacity),
                        expected,
                        "streaming split={split} capacity={capacity} input={:?}",
                        String::from_utf8_lossy(input)
                    );
                }
            }
        }
    }
}

#[test]
fn push_split_at_every_boundary_matches_slice() {
    for &input in &corpus() {
        for format in formats() {
            let expected = slice_events(input, format);
            for split in 0..=input.len() {
                let chunks: [&[u8]; 2] = [&input[..split], &input[split..]];
                assert_eq!(
                    push_events(&chunks, format),
                    expected,
                    "push split={split} input={:?}",
                    String::from_utf8_lossy(input)
                );
            }
        }
    }
}

#[test]
fn push_three_way_split_matches_slice() {
    for &input in &corpus() {
        if input.len() > 24 {
            continue;
        }
        for format in formats() {
            let expected = slice_events(input, format);
            for first in 0..=input.len() {
                for second in first..=input.len() {
                    let chunks: [&[u8]; 3] =
                        [&input[..first], &input[first..second], &input[second..]];
                    assert_eq!(
                        push_events(&chunks, format),
                        expected,
                        "push splits={first},{second} input={:?}",
                        String::from_utf8_lossy(input)
                    );
                }
            }
        }
    }
}

#[test]
fn one_byte_at_a_time_matches_slice() {
    for &input in &corpus() {
        for format in formats() {
            let expected = slice_events(input, format);
            let dripped: Vec<Vec<u8>> = input.iter().map(|&byte| vec![byte]).collect();
            assert_eq!(
                streaming_events(dripped, format, 1),
                expected,
                "streaming drip input={:?}",
                String::from_utf8_lossy(input)
            );
            let borrowed: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(
                push_events(&borrowed, format),
                expected,
                "push drip input={:?}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

#[test]
fn chunk_split_at_every_boundary_matches_slice() {
    for &input in &corpus() {
        for format in formats() {
            let expected = slice_events(input, format);
            for split in 0..=input.len() {
                let chunks: [&[u8]; 2] = [&input[..split], &input[split..]];
                assert_eq!(
                    chunk_events(&chunks, format),
                    expected,
                    "chunk split={split} input={:?}",
                    String::from_utf8_lossy(input)
                );
            }
        }
    }
}

#[test]
fn chunk_three_way_split_matches_slice() {
    for &input in &corpus() {
        if input.len() > 24 {
            continue;
        }
        for format in formats() {
            let expected = slice_events(input, format);
            for first in 0..=input.len() {
                for second in first..=input.len() {
                    let chunks: [&[u8]; 3] =
                        [&input[..first], &input[first..second], &input[second..]];
                    assert_eq!(
                        chunk_events(&chunks, format),
                        expected,
                        "chunk splits={first},{second} input={:?}",
                        String::from_utf8_lossy(input)
                    );
                }
            }
        }
    }
}

#[test]
fn chunk_one_byte_at_a_time_matches_slice() {
    for &input in &corpus() {
        for format in formats() {
            let expected = slice_events(input, format);
            let borrowed: Vec<&[u8]> = input.chunks(1).collect();
            assert_eq!(
                chunk_events(&borrowed, format),
                expected,
                "chunk drip input={:?}",
                String::from_utf8_lossy(input)
            );
        }
    }
}

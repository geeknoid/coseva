//! Promptness of the streaming boundary pre-scan.
//!
//! `Engine`'s chunked front ends do not re-parse a settled prefix on every
//! resumed window. They first ask a fast pre-scan, `window_lacks_record`,
//! whether the window provably holds no whole record yet, and skip the parse
//! when it says so. That pre-scan is a second, independently maintained copy
//! of the parser's delimiter, quote, escape, terminator and multi-byte-tail
//! framing rules, and its own doc claims it "mirrors the parser's framing
//! rules exactly".
//!
//! Its bias is deliberate: it answers "not proven" on every ambiguity, and the
//! EOF path re-parses authoritatively, so a divergence cannot produce a wrong
//! record. That is also why the existing differential tests in
//! `tests/differential.rs` cannot see a divergence at all — they compare the
//! event sequence *after* EOF, by which point any withheld record has been
//! recovered.
//!
//! The failure this file catches is the other one: the pre-scan claiming a
//! window holds no record when it does, so the record is withheld until the
//! stream closes. On a socket or a pipe that never closes, that is a record
//! delivered never.
//!
//! The oracle does not restate the framing rules a third time. A `SliceParser`
//! over the whole document reports where each record starts; once the *first
//! byte of record `i + 1`* has been fed, record `i` is complete under any
//! framing rule whatsoever, because a later record could not have started
//! otherwise. So the parser must already have been able to hand it over,
//! without EOF and without another byte.
//!
//! Input is fed one byte per chunk, which is what makes the pre-scan visible:
//! it is consulted only on a *resumed* window, so handing the parser a whole
//! prefix in a single chunk bypasses it entirely.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva::config::{Escape, FormatOptions, Headers, ParseOptions, Quoting, RecordEnding};
use coseva::{ByteRecord, PushParser, SliceParser};

type Fields = Vec<Vec<u8>>;

/// A dialect, plus the delimiter and terminator bytes its corpus must be built
/// from and whether it honours quotes.
struct Dialect {
    name: &'static str,
    format: FormatOptions,
    delimiter: &'static [u8],
    ending: &'static [u8],
    quoting: bool,
}

/// The authoritative parse: every record's fields, and the byte offset it
/// starts at.
fn reference(input: &[u8], format: FormatOptions) -> Vec<(usize, Fields)> {
    let mut parser =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))
            .expect("valid options");

    let mut records = Vec::new();
    let mut record = ByteRecord::new();
    while let Some(mut line) = parser.next_line().expect("the corpus parses cleanly") {
        line.read_byte_record_into(&mut record)
            .expect("the corpus parses cleanly");
        records.push((
            record.byte_range().start,
            record.iter().map(<[u8]>::to_vec).collect(),
        ));
    }
    records
}

/// Stream `input` one byte per chunk, never signalling EOF, and return what
/// the parser had handed over after each byte.
fn delivered_per_byte(input: &[u8], format: FormatOptions) -> (Vec<usize>, Vec<Fields>) {
    let mut parser = PushParser::with_options(format, ParseOptions::new().headers(Headers::None))
        .expect("valid options");

    let mut record = ByteRecord::new();
    let mut delivered: Vec<Fields> = Vec::new();
    let mut counts = Vec::with_capacity(input.len());

    for index in 0..input.len() {
        let byte = &input[index..=index];
        let mut offset = 0;
        while offset < byte.len() {
            let mut chunk = parser.chunk(&byte[offset..]);
            while let Some(mut line) = chunk.next_line().expect("the corpus parses cleanly") {
                line.read_byte_record_into(&mut record)
                    .expect("the corpus parses cleanly");
                delivered.push(record.iter().map(<[u8]>::to_vec).collect());
            }
            let taken = chunk.done();
            assert!(taken > 0, "a chunk that takes nothing cannot make progress");
            offset += taken;
        }
        counts.push(delivered.len());
    }

    (counts, delivered)
}

/// A multi-byte record ending dialect.
///
/// Ensures straddling multi-byte record endings across chunk boundaries are
/// recognized promptly without withholding complete records.
fn multibyte_terminator() -> Dialect {
    Dialect {
        name: "multi-byte terminator",
        format: FormatOptions::new().record_ending_sequence(b"<>"),
        delimiter: b",",
        ending: b"<>",
        quoting: true,
    }
}

/// The dialects the pre-scan has the most independent logic for, each paired
/// with the delimiter and terminator bytes the corpus must be built from and
/// whether quotes are honoured: a bare-LF baseline, CRLF (a terminator whose
/// first byte is ambiguous until the second arrives), an unquoted-escape
/// mode, a multi-byte delimiter, a multi-byte terminator, and leading-space
/// skipping.
fn dialects() -> Vec<Dialect> {
    vec![
        Dialect {
            name: "default",
            format: FormatOptions::new(),
            delimiter: b",",
            ending: b"\n",
            quoting: true,
        },
        Dialect {
            name: "crlf",
            format: FormatOptions::new().record_ending(RecordEnding::CrLf),
            delimiter: b",",
            ending: b"\r\n",
            quoting: true,
        },
        Dialect {
            name: "unquoted escape",
            format: FormatOptions::new()
                .escape(Escape::Unquoted(b'\\'))
                .quoting(Quoting::Never),
            delimiter: b",",
            ending: b"\n",
            quoting: false,
        },
        Dialect {
            name: "multi-byte delimiter",
            format: FormatOptions::new().delimiter_sequence(b"||"),
            delimiter: b"||",
            ending: b"\n",
            quoting: true,
        },
        Dialect {
            name: "skip initial space",
            format: FormatOptions::new().skip_initial_space(true),
            delimiter: b",",
            ending: b"\n",
            quoting: true,
        },
    ]
}

/// Documents built to sit on the pre-scan's ambiguities: empty fields, and a
/// quoted field carrying both the delimiter and the terminator inside it.
///
/// A dialect that does not honour quotes would read the quoted document as
/// malformed rather than as the boundary puzzle it is meant to be, so it only
/// gets the plain one.
fn corpus(delimiter: &[u8], ending: &[u8], quoting: bool) -> Vec<Vec<u8>> {
    let quoted_row: &[&[u8]] = if quoting {
        &[b"\"q\"", b"c"]
    } else {
        &[b"q", b"c"]
    };
    let rows: &[&[&[u8]]] = &[&[b"a", b"b"], &[b"", b""], quoted_row, &[b"x", b"y"]];

    let mut plain = Vec::new();
    for row in rows {
        for (index, field) in row.iter().enumerate() {
            if index > 0 {
                plain.extend_from_slice(delimiter);
            }
            plain.extend_from_slice(field);
        }
        plain.extend_from_slice(ending);
    }

    if !quoting {
        return vec![plain];
    }

    let mut quoted = Vec::new();
    quoted.extend_from_slice(b"\"in");
    quoted.extend_from_slice(delimiter);
    quoted.extend_from_slice(b"side");
    quoted.extend_from_slice(ending);
    quoted.extend_from_slice(b"still\"\"quoted\"");
    quoted.extend_from_slice(delimiter);
    quoted.extend_from_slice(b"tail");
    quoted.extend_from_slice(ending);
    quoted.extend_from_slice(b"after");
    quoted.extend_from_slice(delimiter);
    quoted.extend_from_slice(b"end");
    quoted.extend_from_slice(ending);

    vec![plain, quoted]
}

/// Assert that every record of every document in `dialect`'s corpus is handed
/// over as soon as its successor begins.
fn assert_prompt(dialect: &Dialect) {
    let Dialect {
        name,
        format,
        delimiter,
        ending,
        quoting,
    } = *dialect;
    for input in corpus(delimiter, ending, quoting) {
        let expected = reference(&input, format);
        let (counts, delivered) = delivered_per_byte(&input, format);

        for (index, record) in delivered.iter().enumerate() {
            assert_eq!(
                record, &expected[index].1,
                "{name}: streamed record {index} differs from the whole-document \
                 parse. input: {input:?}",
            );
        }

        for (fed, count) in counts.iter().copied().enumerate() {
            let fed = fed + 1;
            // Records whose successor has started within the fed bytes.
            let due = expected
                .iter()
                .skip(1)
                .take_while(|(start, _)| *start < fed)
                .count();

            assert!(
                count >= due,
                "{name}: after {fed} of {} bytes the parser had delivered {count} \
                 records but {due} were already complete; the rest are withheld \
                 until EOF, which a stream that never closes never reaches. \
                 input: {input:?}",
                input.len(),
            );
        }
    }
}

/// A record whose successor has already begun is complete beyond any doubt,
/// so the parser must hand it over without waiting for the stream to close.
/// Withholding it is the divergence this file exists to detect, and on a
/// stream that never reaches EOF it is unrecoverable.
#[test]
fn every_record_is_delivered_as_soon_as_the_next_one_begins() {
    for dialect in dialects() {
        assert_prompt(&dialect);
    }
}

/// Multi-byte record endings must deliver complete records immediately upon
/// the arrival of the full terminator sequence rather than waiting for subsequent bytes.
#[test]
fn a_multi_byte_record_ending_does_not_delay_delivery() {
    assert_prompt(&multibyte_terminator());
}

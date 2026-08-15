//! Differential and generated-input tests.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::error::Error as StdError;
use std::io::Cursor;

use coseva::ByteRecord;
#[cfg(feature = "test-util")]
use coseva::Error;
use coseva::ErrorKind;
use coseva::Location;
use coseva::config::{
    BlankRecords, EmitOptions, Escape, FormatOptions, Headers, Limits, ParseOptions, RecordEnding,
    Recovery, Syntax,
};
use coseva::format::Csv;
use coseva::{Chunk, PushParser};
use coseva::{IoEmitter, VecEmitter};
use coseva::{IoParser, SliceParser};

type Rows = Vec<Vec<Vec<u8>>>;

#[derive(Clone, Copy)]
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn below(&mut self, limit: usize) -> usize {
        let bytes = self.next().to_le_bytes();
        usize::from(u16::from_le_bytes([bytes[0], bytes[1]])) % limit
    }
}

fn generated_rows() -> Rows {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789,\";'|\r\n\t\\\0";
    let mut generator = Generator(0xC5_61_9B_42_D3_8F_27_A1);
    let mut rows = Vec::with_capacity(512);
    for _ in 0..512 {
        let field_count = generator.below(7) + 1;
        let mut row = Vec::with_capacity(field_count);
        for _ in 0..field_count {
            let field_len = generator.below(49);
            let mut field = Vec::with_capacity(field_len);
            for _ in 0..field_len {
                field.push(ALPHABET[generator.below(ALPHABET.len())]);
            }
            row.push(field);
        }
        rows.push(row);
    }
    rows
}

fn parse_better(input: &[u8]) -> Result<Rows, Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut rows = Vec::new();
    while let Some(mut line) = reader.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

fn parse_csv(input: &[u8]) -> Result<Rows, Box<dyn StdError>> {
    csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(input)
        .byte_records()
        .map(|record| {
            record
                .map(|record| record.iter().map(<[u8]>::to_vec).collect())
                .map_err(Into::into)
        })
        .collect()
}

fn parse_better_owned(input: &[u8]) -> Result<Rows, Box<dyn StdError>> {
    let mut reader = SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )?;
    let mut record = ByteRecord::with_capacity(8, 128);
    let mut rows = Vec::new();
    while let Some(mut line) = reader.next_line()? {
        line.read_byte_record_into(&mut record)?;
        let mut row = Vec::with_capacity(record.len());
        for index in 0..record.len() {
            row.push(
                record
                    .get(index)
                    .ok_or("owned record endpoint was missing")?
                    .to_vec(),
            );
        }
        rows.push(row);
    }
    Ok(rows)
}

fn parse_io_owned(input: &[u8]) -> Result<Rows, Box<dyn StdError>> {
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(127),
    )?;
    let mut record = ByteRecord::with_capacity(8, 128);
    let mut rows = Vec::new();
    while reader.read_byte_record_into(&mut record)? {
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

#[test]
fn generated_records_match_csv() -> Result<(), Box<dyn StdError>> {
    let expected = generated_rows();

    let mut better = IoEmitter::with_options(Vec::new(), FormatOptions::CSV, EmitOptions::new())?;
    for row in &expected {
        better.emit_record(row.iter().map(Vec::as_slice))?;
    }
    let better_output = better.into_inner()?;
    assert_eq!(parse_better(&better_output)?, expected);
    assert_eq!(parse_better_owned(&better_output)?, expected);
    assert_eq!(parse_io_owned(&better_output)?, expected);
    assert_eq!(parse_csv(&better_output)?, expected);

    let mut direct = VecEmitter::default();
    for row in &expected {
        let fields: Vec<&[u8]> = row.iter().map(Vec::as_slice).collect();
        direct.emit_slices(&fields)?;
    }
    let direct_output = direct.into_inner();
    assert_eq!(parse_better(&direct_output)?, expected);
    assert_eq!(parse_csv(&direct_output)?, expected);

    let mut csv = csv::WriterBuilder::new()
        .flexible(true)
        .from_writer(Vec::new());
    for row in &expected {
        csv.write_record(row)?;
    }
    let csv_output = csv.into_inner()?;
    assert_eq!(parse_better(&csv_output)?, expected);
    assert_eq!(parse_better_owned(&csv_output)?, expected);
    assert_eq!(parse_io_owned(&csv_output)?, expected);
    assert_eq!(parse_csv(&csv_output)?, expected);
    Ok(())
}

#[test]
fn generated_custom_dialect_round_trips() -> Result<(), Box<dyn StdError>> {
    let format = FormatOptions::CSV
        .delimiter(b';')
        .quote(b'\'')
        .record_ending(RecordEnding::Byte(b'|'))
        .escape(Escape::Backslash(b'\\'));
    let expected = generated_rows();
    let mut writer = IoEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
    for row in &expected {
        writer.emit_record(row.iter().map(Vec::as_slice))?;
    }
    let output = writer.into_inner()?;

    let mut reader =
        SliceParser::with_options(&output, format, ParseOptions::new().headers(Headers::None))?;
    for expected_row in expected {
        let mut line = reader.next_line()?.expect("missing generated record");
        let row = line.record()?;
        assert_eq!(row.iter().collect::<Vec<_>>(), expected_row);
    }
    assert!(reader.next_line()?.is_none());
    Ok(())
}

#[test]
fn arbitrary_inputs_respect_limits_without_panicking() -> Result<(), Box<dyn StdError>> {
    const ALPHABET: &[u8] = b"ab,\"\r\n\\\0";
    let mut generator = Generator(0x74_0C_2E_91_A5_6B_38_DF);
    for _ in 0..20_000 {
        let input_len = generator.below(129);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let mut reader = SliceParser::with_options(
            &input,
            FormatOptions::CSV,
            ParseOptions::new().limits(Limits::new(96, 32, 16)),
        )?;
        let mut records = 0;
        while let Ok(outcome) = reader.next_line() {
            let Some(mut line) = outcome else {
                break;
            };
            if line.record().is_err() {
                break;
            }
            records += 1;
            assert!(records <= input.len() + 1);
        }

        let mut reader = SliceParser::with_options(
            &input,
            FormatOptions::CSV,
            ParseOptions::new().limits(Limits::new(96, 32, 16)),
        )?;
        let mut record = ByteRecord::with_capacity(8, 64);
        let mut records = 0;
        while let Ok(outcome) = reader.next_line() {
            let Some(mut line) = outcome else {
                break;
            };
            if line.read_byte_record_into(&mut record).is_err() {
                break;
            }
            records += 1;
            assert!(records <= input.len() + 1);
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum ParseEvent {
    Row(Vec<Vec<u8>>, Vec<bool>),
    Error(ErrorKind, usize, u64, u64, usize),
    End,
}

fn slice_events(
    input: &[u8],
    format: FormatOptions,
    syntax: Syntax,
    limits: Limits,
    blank_records: BlankRecords,
) -> Vec<ParseEvent> {
    let mut reader = SliceParser::with_options(
        input,
        format.syntax(syntax).blank_records(blank_records),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect("generated format is valid");
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => match line.record() {
                Ok(record) => events.push(ParseEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    (0..record.len())
                        .map(|index| record.is_null(index) == Some(true))
                        .collect(),
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(ParseEvent::Error(
                        error.kind(),
                        location.byte,
                        location.line,
                        location.record,
                        location.field,
                    ));
                    return events;
                }
            },
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

fn streaming_events(
    input: &[u8],
    format: FormatOptions,
    syntax: Syntax,
    limits: Limits,
    blank_records: BlankRecords,
    buffer_capacity: usize,
) -> Vec<ParseEvent> {
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        format.syntax(syntax).blank_records(blank_records),
        ParseOptions::new()
            .headers(Headers::None)
            .limits(limits)
            .buffer_capacity(buffer_capacity),
    )
    .expect("generated format is valid");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                Ok(()) => events.push(ParseEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    (0..record.len())
                        .map(|index| record.is_null(index) == Some(true))
                        .collect(),
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(ParseEvent::Error(
                        error.kind(),
                        location.byte,
                        location.line,
                        location.record,
                        location.field,
                    ));
                    return events;
                }
            },
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

fn push_events(
    input: &[u8],
    format: FormatOptions,
    syntax: Syntax,
    limits: Limits,
    chunk_size: usize,
) -> Vec<ParseEvent> {
    fn failure(error: &coseva::Error) -> ParseEvent {
        let location = error.location();
        ParseEvent::Error(
            error.kind(),
            location.byte,
            location.line,
            location.record,
            location.field,
        )
    }

    let mut parser = PushParser::with_options(
        format.syntax(syntax),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect("valid options");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    // Each chunk is lent and drained in turn, so the parser only ever retains
    // the record the lent bytes have not finished.
    let drain = |parser: &mut PushParser,
                 input: &[u8],
                 record: &mut ByteRecord,
                 events: &mut Vec<ParseEvent>| {
        let mut guard = parser.chunk(input);
        let ok = loop {
            match guard.next_line() {
                Ok(Some(mut line)) => match line.read_byte_record_into(record) {
                    Ok(()) => events.push(ParseEvent::Row(
                        record.iter().map(<[u8]>::to_vec).collect(),
                        (0..record.len())
                            .map(|index| record.is_null(index) == Some(true))
                            .collect(),
                    )),
                    Err(error) => {
                        events.push(failure(&error));
                        break false;
                    }
                },
                Ok(None) => break true,
                Err(error) => {
                    events.push(failure(&error));
                    break false;
                }
            }
        };
        (guard.done(), ok)
    };

    for chunk in input.chunks(chunk_size) {
        let mut offset = 0;
        loop {
            let (taken, ok) = drain(&mut parser, &chunk[offset..], &mut record, &mut events);
            offset += taken;
            if !ok {
                return events;
            }
            if offset >= chunk.len() {
                break;
            }
        }
    }

    parser.finish();
    if drain(&mut parser, b"", &mut record, &mut events).1 {
        events.push(ParseEvent::End);
    }
    events
}

fn chunk_events(
    input: &[u8],
    format: FormatOptions,
    syntax: Syntax,
    limits: Limits,
    chunk_size: usize,
) -> Vec<ParseEvent> {
    fn failure(error: &coseva::Error) -> ParseEvent {
        let location = error.location();
        ParseEvent::Error(
            error.kind(),
            location.byte,
            location.line,
            location.record,
            location.field,
        )
    }

    let mut parser = PushParser::with_options(
        format.syntax(syntax),
        ParseOptions::new().headers(Headers::None).limits(limits),
    )
    .expect("valid options");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    let drain =
        |chunk: &mut Chunk<'_, '_>, record: &mut ByteRecord, events: &mut Vec<ParseEvent>| loop {
            match chunk.next_line() {
                Ok(Some(mut line)) => match line.read_byte_record_into(record) {
                    Ok(()) => events.push(ParseEvent::Row(
                        record.iter().map(<[u8]>::to_vec).collect(),
                        (0..record.len())
                            .map(|index| record.is_null(index) == Some(true))
                            .collect(),
                    )),
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
        };

    for bytes in input.chunks(chunk_size) {
        let mut offset = 0;
        loop {
            let mut lent = parser.chunk(&bytes[offset..]);
            if !drain(&mut lent, &mut record, &mut events) {
                return events;
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

    parser.finish();
    let mut lent = parser.chunk(b"");
    if drain(&mut lent, &mut record, &mut events) {
        events.push(ParseEvent::End);
    }
    events
}

fn slice_preset_events(input: &[u8], format: FormatOptions) -> Vec<ParseEvent> {
    let mut reader =
        SliceParser::with_options(input, format, ParseOptions::new().headers(Headers::None))
            .expect("format is valid");
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => match line.record() {
                Ok(record) => events.push(ParseEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    (0..record.len())
                        .map(|index| record.is_null(index) == Some(true))
                        .collect(),
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(ParseEvent::Error(
                        error.kind(),
                        location.byte,
                        location.line,
                        location.record,
                        location.field,
                    ));
                    return events;
                }
            },
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

fn streaming_preset_events(
    input: &[u8],
    format: FormatOptions,
    buffer_capacity: usize,
) -> Vec<ParseEvent> {
    let mut reader = IoParser::with_options(
        Cursor::new(input),
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(buffer_capacity),
    )
    .expect("format is valid");
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                Ok(()) => events.push(ParseEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    (0..record.len())
                        .map(|index| record.is_null(index) == Some(true))
                        .collect(),
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(ParseEvent::Error(
                        error.kind(),
                        location.byte,
                        location.line,
                        location.record,
                        location.field,
                    ));
                    return events;
                }
            },
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

// Replaying the fixed generator to each shard's start preserves the original
// 20,000-case corpus exactly while keeping each mutation-test process short.
fn generated_inputs_match_across_all_reader_modes_range(start: usize, end: usize) {
    const ALPHABET: &[u8] = b"ab,;\"'\r\n|\\#\0";
    let formats = [
        FormatOptions::CSV,
        FormatOptions::CSV.comment(Some(b'#')),
        FormatOptions::CSV
            .delimiter(b';')
            .quote(b'\'')
            .record_ending(RecordEnding::Byte(b'|'))
            .escape(Escape::Backslash(b'\\'))
            .comment(Some(b'#')),
    ];
    let parse_modes = [Syntax::Strict, Syntax::Compatible(Recovery::PERMISSIVE)];
    let mut generator = Generator(0xA7_13_D8_4C_92_6E_05_B1);
    for case in 0..end {
        let input_len = generator.below(129);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let format = formats[generator.below(formats.len())];
        let syntax = parse_modes[generator.below(parse_modes.len())];
        if case < start {
            continue;
        }
        for limits in [Limits::new(96, 32, 16), Limits::DEFAULT] {
            let expected = slice_events(&input, format, syntax, limits, BlankRecords::Preserve);
            for buffer_capacity in [1, 2, 3, 7, 8, 64] {
                assert_eq!(
                    streaming_events(
                        &input,
                        format,
                        syntax,
                        limits,
                        BlankRecords::Preserve,
                        buffer_capacity,
                    ),
                    expected,
                    "streaming divergence for {format:?} in {syntax:?} mode with {buffer_capacity}-byte buffers and {limits:?} in generated case {case}: {input:?}",
                );
            }
            for chunk_size in [7, 8, 64] {
                assert_eq!(
                    push_events(&input, format, syntax, limits, chunk_size),
                    expected,
                    "push divergence for {format:?} in {syntax:?} mode with {chunk_size}-byte chunks and {limits:?} in generated case {case}: {input:?}",
                );
                assert_eq!(
                    chunk_events(&input, format, syntax, limits, chunk_size),
                    expected,
                    "chunk divergence for {format:?} in {syntax:?} mode with {chunk_size}-byte chunks and {limits:?} in generated case {case}: {input:?}",
                );
            }
        }
    }
}

#[test]
fn generated_inputs_match_across_all_reader_modes() {
    generated_inputs_match_across_all_reader_modes_range(0, 5_000);
}

#[test]
fn generated_inputs_match_across_all_reader_modes_cases_5000_to_9999() {
    generated_inputs_match_across_all_reader_modes_range(5_000, 10_000);
}

#[test]
fn generated_inputs_match_across_all_reader_modes_cases_10000_to_14999() {
    generated_inputs_match_across_all_reader_modes_range(10_000, 15_000);
}

#[test]
fn generated_inputs_match_across_all_reader_modes_cases_15000_to_19999() {
    generated_inputs_match_across_all_reader_modes_range(15_000, 20_000);
}

#[test]
fn generated_inputs_match_named_streaming_kernels() {
    const ALPHABET: &[u8] = b"ab,;\"\r\n\t|\\\0";
    const DIALECTS: [FormatOptions; 6] = [
        FormatOptions::TSV,
        FormatOptions::SEMICOLON,
        FormatOptions::PIPE,
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::BACKSLASH_TSV,
        FormatOptions::RFC4180,
    ];
    let mut generator = Generator(0x5E_92_A7_31_C4_68_DB_0F);
    for case in 0..20_000 {
        let input_len = generator.below(257);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let format = DIALECTS[generator.below(DIALECTS.len())];
        let expected = slice_events(
            &input,
            format,
            Syntax::Strict,
            Limits::DEFAULT,
            BlankRecords::Preserve,
        );
        for buffer_capacity in [2, 3, 7, 8, 64, 256] {
            assert_eq!(
                streaming_events(
                    &input,
                    format,
                    Syntax::Strict,
                    Limits::DEFAULT,
                    BlankRecords::Preserve,
                    buffer_capacity,
                ),
                expected,
                "named streaming divergence for {format:?} with {buffer_capacity}-byte buffers in generated case {case}: {input:?}",
            );
        }
    }
}

#[test]
fn generated_inputs_match_commented_streaming_kernel() {
    const ALPHABET: &[u8] = b"ab,#\"\r\n\0";
    let mut generator = Generator(0x73_A1_C9_5E_20_D4_8B_F6);
    for case in 0..20_000 {
        let input_len = generator.below(257);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let expected = slice_events(
            &input,
            FormatOptions::COMMENTED_CSV,
            Syntax::Strict,
            Limits::DEFAULT,
            BlankRecords::Skip,
        );
        for buffer_capacity in [2, 3, 7, 8, 64, 256] {
            assert_eq!(
                streaming_events(
                    &input,
                    FormatOptions::COMMENTED_CSV,
                    Syntax::Strict,
                    Limits::DEFAULT,
                    BlankRecords::Skip,
                    buffer_capacity,
                ),
                expected,
                "commented streaming divergence with {buffer_capacity}-byte buffers in generated case {case}: {input:?}",
            );
        }
    }
}

#[test]
fn generated_inputs_match_database_streaming_kernels() {
    const ALPHABET: &[u8] = b"ab,\"\r\n\t\\Nnrt0Z\0|@";
    // A separator only the general parser can find, so the two front ends are
    // compared over inputs where a partial separator straddles a refill.
    #[cfg(feature = "multibyte")]
    const PRESETS: [FormatOptions; 4] = [
        FormatOptions::POSTGRES_COPY_CSV,
        FormatOptions::MYSQL,
        FormatOptions::PYTHON_ESCAPED,
        FormatOptions::CSV
            .delimiter_sequence(b"||")
            .record_ending_sequence(b"@@"),
    ];
    #[cfg(not(feature = "multibyte"))]
    const PRESETS: [FormatOptions; 3] = [
        FormatOptions::POSTGRES_COPY_CSV,
        FormatOptions::MYSQL,
        FormatOptions::PYTHON_ESCAPED,
    ];
    let mut generator = Generator(0x49_B7_D2_6C_A8_31_E5_0F);
    for case in 0..20_000 {
        let input_len = generator.below(257);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let format = PRESETS[generator.below(PRESETS.len())];
        let expected = slice_preset_events(&input, format);
        for buffer_capacity in [2, 3, 7, 8, 64, 256] {
            assert_eq!(
                streaming_preset_events(&input, format, buffer_capacity),
                expected,
                "database streaming divergence for {format:?} with {buffer_capacity}-byte buffers in generated case {case}: {input:?}",
            );
        }
    }
}

/// Parse with the general parser forced, so it can serve as the oracle its
/// own optimization is checked against.
#[cfg(feature = "test-util")]
fn general_preset_events(input: &[u8], format: FormatOptions, owned: bool) -> Vec<ParseEvent> {
    preset_events(
        input,
        format,
        ParseOptions::new()
            .headers(Headers::None)
            .force_general_parser(true),
        owned,
    )
}

/// Parse the same bytes the way a caller would, through whichever fast path
/// the format and the record shape select.
#[cfg(feature = "test-util")]
fn kernel_preset_events(input: &[u8], format: FormatOptions, owned: bool) -> Vec<ParseEvent> {
    preset_events(
        input,
        format,
        ParseOptions::new().headers(Headers::None),
        owned,
    )
}

#[cfg(feature = "test-util")]
fn preset_events(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    owned: bool,
) -> Vec<ParseEvent> {
    let mut reader = SliceParser::with_options(input, format, options).expect("format is valid");
    let mut record = ByteRecord::with_capacity(8, 128);
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => {
                let read = if owned {
                    line.read_byte_record_into(&mut record).map(|()| {
                        (
                            (0..record.len())
                                .map(|index| {
                                    record.get(index).expect("field index is in range").to_vec()
                                })
                                .collect(),
                            (0..record.len())
                                .map(|index| record.is_null(index) == Some(true))
                                .collect(),
                        )
                    })
                } else {
                    line.record().map(|record| {
                        (
                            record.iter().map(<[u8]>::to_vec).collect(),
                            (0..record.len())
                                .map(|index| record.is_null(index) == Some(true))
                                .collect(),
                        )
                    })
                };
                match read {
                    Ok((fields, nulls)) => events.push(ParseEvent::Row(fields, nulls)),
                    Err(error) => {
                        events.push(error_event(&error));
                        return events;
                    }
                }
            }
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                events.push(error_event(&error));
                return events;
            }
        }
    }
}

#[cfg(feature = "test-util")]
fn error_event(error: &Error) -> ParseEvent {
    let location = error.location();
    ParseEvent::Error(
        error.kind(),
        location.byte,
        location.line,
        location.record,
        location.field,
    )
}

/// The vectorized kernel is only ever an optimization of the general parser,
/// so the general parser is the one oracle that can check it.
///
/// The generative tests around this one cross-check the *front ends* against
/// each other. They share the parser core, so a bug in the core is invisible
/// to them -- they agree, identically wrongly. `csv` is an outside reference
/// but cannot express strict CRLF, `MySQL` escapes or either NULL policy, which
/// is precisely the set of dialects the kernel was specialized for. Two real
/// bugs reached the tree through that gap: a wrong error `field` index on the
/// `CrLf` pass, and a missed commit point in the `MySQL` bail.
#[cfg(feature = "test-util")]
#[test]
fn generated_inputs_match_the_general_parser() {
    const ALPHABET: &[u8] = b"ab,\"\r\n\t\\N \0;#";
    const PRESETS: [FormatOptions; 6] = [
        FormatOptions::CSV,
        FormatOptions::TSV,
        FormatOptions::MYSQL,
        FormatOptions::POSTGRES_COPY_CSV,
        FormatOptions::RFC4180,
        FormatOptions::PYTHON_ESCAPED,
    ];
    let mut generator = Generator(0x6D_3E_92_A1_04_C7_5B_88);
    for case in 0..40_000 {
        let input_len = generator.below(257);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let format = PRESETS[generator.below(PRESETS.len())];
        for owned in [false, true] {
            assert_eq!(
                kernel_preset_events(&input, format, owned),
                general_preset_events(&input, format, owned),
                "kernel diverged from the general parser for {format:?} (owned: {owned}) in generated case {case}: {input:?}",
            );
        }
    }
}

/// Every record boundary a stream reports must be seekable back to, and
/// reading on from there must reproduce exactly the tail that followed it the
/// first time. Rewinding must likewise reproduce the whole stream.
#[test]
fn generated_inputs_replay_from_every_seeked_boundary() {
    const ALPHABET: &[u8] = b"ab,,\r\n\"ab\r\n";
    let mut generator = Generator(0x3C_71_E4_08_5A_9D_26_B3);
    for case in 0..2_000 {
        let input_len = generator.below(129);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        for buffer_capacity in [1, 3, 8, 64] {
            let full = streaming_events(
                &input,
                FormatOptions::CSV,
                Syntax::Strict,
                Limits::DEFAULT,
                BlankRecords::Preserve,
                buffer_capacity,
            );
            let boundaries = record_boundaries(&input, buffer_capacity);

            for (skipped, boundary) in boundaries.iter().enumerate() {
                let replayed = seeked_events(&input, buffer_capacity, Some(*boundary));
                assert_eq!(
                    replayed,
                    full[skipped..],
                    "seek to {boundary:?} with {buffer_capacity}-byte buffers diverged in generated case {case}: {input:?}",
                );
            }

            let rewound = seeked_events(&input, buffer_capacity, None);
            assert_eq!(
                rewound, full,
                "rewind with {buffer_capacity}-byte buffers diverged in generated case {case}: {input:?}",
            );
        }
    }
}

/// The location reported immediately before each successfully read record.
fn record_boundaries(input: &[u8], buffer_capacity: usize) -> Vec<Location> {
    let mut reader = seekable_reader(input, buffer_capacity);
    let mut record = ByteRecord::new();
    let mut boundaries = Vec::new();
    loop {
        let location = reader.location();
        let Ok(Some(mut line)) = reader.next_line() else {
            return boundaries;
        };
        if line.read_byte_record_into(&mut record).is_err() {
            return boundaries;
        }
        boundaries.push(location);
    }
}

/// Read a stream to its end after either seeking to `target` or rewinding.
fn seeked_events(
    input: &[u8],
    buffer_capacity: usize,
    target: Option<Location>,
) -> Vec<ParseEvent> {
    let mut reader = seekable_reader(input, buffer_capacity);
    // Consume a record first, so the seek has to undo real progress.
    let mut record = ByteRecord::new();
    if let Ok(Some(mut line)) = reader.next_line() {
        let _ = line.read_byte_record_into(&mut record);
    }
    match target {
        Some(location) => reader.seek(location).expect("boundary is seekable"),
        None => reader.rewind().expect("cursor input rewinds"),
    }
    drain(&mut reader)
}

fn seekable_reader(input: &[u8], buffer_capacity: usize) -> IoParser<Cursor<Vec<u8>>> {
    IoParser::with_options(
        Cursor::new(input.to_vec()),
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT)
            .buffer_capacity(buffer_capacity),
    )
    .expect("generated format is valid")
}

fn drain(reader: &mut IoParser<Cursor<Vec<u8>>>) -> Vec<ParseEvent> {
    let mut record = ByteRecord::new();
    let mut events = Vec::new();
    loop {
        let outcome = match reader.next_line() {
            Ok(Some(mut line)) => line.read_byte_record_into(&mut record).map(|()| true),
            Ok(None) => Ok(false),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(true) => events.push(ParseEvent::Row(
                record.iter().map(<[u8]>::to_vec).collect(),
                (0..record.len())
                    .map(|index| record.is_null(index) == Some(true))
                    .collect(),
            )),
            Ok(false) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

/// Every record boundary a slice reports must be seekable back to, and reading
/// on from there must reproduce exactly the tail that followed it the first
/// time, including the absolute byte, line, and record numbers.
#[test]
fn generated_slice_seeks_replay_from_every_boundary() {
    const ALPHABET: &[u8] = b"ab,,\r\n\"ab\r\n";
    let mut generator = Generator(0x5E_1B_A7_43_9C_02_D8_6F);
    for case in 0..2_000 {
        let input_len = generator.below(129);
        let mut input = Vec::with_capacity(input_len);
        for _ in 0..input_len {
            input.push(ALPHABET[generator.below(ALPHABET.len())]);
        }
        let full = slice_events(
            &input,
            FormatOptions::CSV,
            Syntax::Strict,
            Limits::DEFAULT,
            BlankRecords::Preserve,
        );

        for (skipped, boundary) in slice_boundaries(&input).iter().enumerate() {
            let replayed = slice_seeked_events(&input, *boundary);
            assert_eq!(
                replayed,
                full[skipped..],
                "seek to {boundary:?} diverged in generated case {case}: {input:?}",
            );
        }
    }
}

/// The location reported immediately before each successfully read record.
fn slice_boundaries(input: &[u8]) -> Vec<Location> {
    let mut reader = seekable_slice(input);
    let mut record = ByteRecord::new();
    let mut boundaries = Vec::new();
    loop {
        let location = reader.location();
        let Ok(Some(mut line)) = reader.next_line() else {
            return boundaries;
        };
        if line.read_byte_record_into(&mut record).is_err() {
            return boundaries;
        }
        boundaries.push(location);
    }
}

/// Read a slice to its end after seeking back to `target`.
fn slice_seeked_events(input: &[u8], target: Location) -> Vec<ParseEvent> {
    let mut reader = seekable_slice(input);
    // Consume a record first, so the seek has to undo real progress.
    let mut record = ByteRecord::new();
    if let Ok(Some(mut line)) = reader.next_line() {
        let _ = line.read_byte_record_into(&mut record);
    }
    reader.seek(target).expect("boundary is seekable");
    drain_slice(&mut reader)
}

fn seekable_slice(input: &[u8]) -> SliceParser<'_> {
    SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(Limits::DEFAULT),
    )
    .expect("generated format is valid")
}

fn drain_slice(reader: &mut SliceParser<'_>) -> Vec<ParseEvent> {
    let mut events = Vec::new();
    loop {
        match reader.next_line() {
            Ok(Some(mut line)) => match line.record() {
                Ok(record) => events.push(ParseEvent::Row(
                    record.iter().map(<[u8]>::to_vec).collect(),
                    (0..record.len())
                        .map(|index| record.is_null(index) == Some(true))
                        .collect(),
                )),
                Err(error) => {
                    let location = error.location();
                    events.push(ParseEvent::Error(
                        error.kind(),
                        location.byte,
                        location.line,
                        location.record,
                        location.field,
                    ));
                    return events;
                }
            },
            Ok(None) => {
                events.push(ParseEvent::End);
                return events;
            }
            Err(error) => {
                let location = error.location();
                events.push(ParseEvent::Error(
                    error.kind(),
                    location.byte,
                    location.line,
                    location.record,
                    location.field,
                ));
                return events;
            }
        }
    }
}

/// `Record::byte_range` is documented as an offset into the whole source, so
/// every parser has to report the same range for the same record no matter how
/// the bytes were delivered. The windowed parsers rebase their spans as the
/// window slides, which is exactly where the three could drift apart.
#[test]
fn all_parsers_agree_on_record_byte_ranges() {
    for input in [
        &b"a,b\n1,2\n3,4\n"[..],
        &b"a,b\n1,2\n3,4"[..],
        &b"a,b\r\n1,2\r\n3,4\r\n"[..],
        &b"a,b\n\"x,y\",z\n5,6\n"[..],
        &b"a,b\n\"multi\nline\",z\n5,6\n"[..],
    ] {
        let mut slice = SliceParser::<Csv>::new(input, ParseOptions::new()).expect("parser");
        let mut expected = Vec::new();
        while let Some(mut line) = slice.next_line().expect("slice advances") {
            expected.push(line.record().expect("slice record").byte_range());
        }

        for capacity in [1usize, 3, 8, 64, 4096] {
            let mut parser = IoParser::with_options(
                Cursor::new(input.to_vec()),
                FormatOptions::CSV,
                ParseOptions::new().buffer_capacity(capacity),
            )
            .expect("valid options");
            let mut actual = Vec::new();
            while let Some(mut line) = parser.next_line().expect("streaming advances") {
                actual.push(line.record().expect("streaming record").byte_range());
            }
            assert_eq!(
                expected, actual,
                "streaming with capacity {capacity} disagrees with the slice parser on {input:?}",
            );
        }

        for chunk in [1usize, 3, 64] {
            let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
            let mut actual = Vec::new();
            let mut fed = 0;
            while fed < input.len() {
                let end = (fed + chunk).min(input.len());
                if end == input.len() {
                    parser.finish();
                }
                let mut guard = parser.chunk(&input[fed..end]);
                while let Some(mut line) = guard.next_line().expect("push advances") {
                    actual.push(line.record().expect("push record").byte_range());
                }
                fed += guard.done();
            }
            assert_eq!(
                expected, actual,
                "push with chunk {chunk} disagrees with the slice parser on {input:?}",
            );
        }
    }
}

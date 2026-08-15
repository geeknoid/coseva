//! Deterministic pairwise covering array over the format, parse, and emit
//! option space (item T5).
//!
//! A full Cartesian product of every option would be unmaintainable and slow,
//! so this suite instead builds a *pairwise* (2-way) covering array: a small
//! set of configurations in which every pair of option values from any two
//! option groups appears together in at least one configuration. That is the
//! coverage strength interaction bugs are found at, without the explosion.
//!
//! The array is generated deterministically ([`pairwise`], a fixed greedy
//! construction over an ordered pair set), so a failure always reproduces and a
//! regression can be pinned to a specific row. Every generated row is a *valid*
//! configuration (byte roles are chosen so [`FormatOptions::invalidity`] is
//! `None`); [`invalid_configurations_are_rejected`] enumerates the invalid side
//! separately and asserts the exact documented configuration error for each.
//!
//! For every valid row the suite asserts that:
//!
//! * the three emitters ([`VecEmitter`], [`IoEmitter`], [`PushEmitter`]) encode
//!   the fixed record set to byte-identical output;
//! * the three parser front ends ([`SliceParser`], [`IoParser`], [`PushParser`],
//!   the last over several chunk seams) agree on the parse [`Outcome`]; and
//! * the format round-trips (emit then parse) without data loss wherever no BOM
//!   survives into the parsed data.
//!
//! The BOM read policies agree exactly across every front end, path, and buffer
//! size: [`bom_read_policies_are_consistent`] asserts that a rejected mark
//! yields the same `RejectedBom` kind everywhere (never a downstream syntax
//! error) and that a preserved mark is kept identically, at buffer sizes down
//! to one byte and on the general (eager) scanning path.
//!
//! [`static_and_dynamic_parsers_agree`] and (under the `parallel` feature)
//! [`parallel_parser_agrees_with_slice`] cover the specialized subsets.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeSet;
use std::io::Cursor;

use coseva::config::{
    BlankRecords, EmitOptions, Escape, FieldCount, FormatOptions, Headers, Limits, Nulls,
    ParseOptions, Quoting, ReadBom, RecordEnding, Recovery, Syntax, Whitespace, WriteBom,
};
use coseva::{
    ByteRecord, Error, ErrorKind, IoEmitter, IoParser, PushEmitter, PushParser, SliceParser,
    VecEmitter,
};

/// Records reduced to plain bytes, so results from different parsers compare.
type Rows = Vec<Vec<Vec<u8>>>;

/// The fixed, benign record set every row emits and round-trips.
///
/// Deliberately free of delimiters, quotes, newlines, leading/trailing spaces,
/// comment leaders, and NULL sentinels, so it encodes and decodes losslessly
/// under every valid policy. Any round-trip difference is then attributable to
/// the configuration under test rather than to the payload.
const BENIGN: [[&[u8]; 2]; 3] = [
    [b"alpha", b"bravo"],
    [b"charlie", b"delta"],
    [b"echo", b"foxtrot"],
];

// ── Parse outcome, normalized across front ends ─────────────────────────────

/// A parse outcome reduced to what every front end must agree on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Parsed(Rows),
    Failed(ErrorKind),
}

impl Outcome {
    fn from_result(result: Result<Rows, Error>) -> Self {
        match result {
            Ok(rows) => Self::Parsed(rows),
            Err(error) => Self::Failed(error.kind()),
        }
    }
}

// ── Front-end drivers ───────────────────────────────────────────────────────

fn parse_slice(input: &[u8], format: FormatOptions, options: ParseOptions) -> Result<Rows, Error> {
    let mut parser = SliceParser::with_options(input, format, options)?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

fn parse_streaming(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    buffer: usize,
) -> Result<Rows, Error> {
    let mut parser =
        IoParser::with_options(Cursor::new(input), format, options.buffer_capacity(buffer))?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

fn parse_push(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    chunk: usize,
) -> Result<Rows, Error> {
    let mut parser = PushParser::with_options(format, options)?;
    let mut rows = Rows::new();
    let mut record = ByteRecord::new();

    let mut offset = 0;
    while offset < input.len() {
        let end = input.len().min(offset + chunk.max(1));
        let mut fed = 0;
        while fed < end - offset {
            let mut lent = parser.chunk(&input[offset + fed..end]);
            let outcome = loop {
                match lent.next_line() {
                    Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                        Ok(()) => rows.push(record.iter().map(<[u8]>::to_vec).collect()),
                        Err(error) => break Err(error),
                    },
                    Ok(None) => break Ok(()),
                    Err(error) => break Err(error),
                }
            };
            let accepted = lent.done();
            outcome?;
            fed += accepted;
            if accepted == 0 {
                break;
            }
        }
        offset = end;
    }

    parser.finish();
    let mut lent = parser.chunk(b"");
    let outcome = loop {
        match lent.next_line() {
            Ok(Some(mut line)) => match line.read_byte_record_into(&mut record) {
                Ok(()) => rows.push(record.iter().map(<[u8]>::to_vec).collect()),
                Err(error) => break Err(error),
            },
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    drop(lent);
    outcome?;
    Ok(rows)
}

// ── Emitters ────────────────────────────────────────────────────────────────

fn emit_vec(records: &[[&[u8]; 2]], format: FormatOptions, emit: EmitOptions) -> Vec<u8> {
    let mut emitter = VecEmitter::with_options(Vec::new(), format, emit).expect("valid emitter");
    for record in records {
        emitter.emit_record(*record).expect("vec emit");
    }
    emitter.into_inner()
}

fn emit_io(
    records: &[[&[u8]; 2]],
    format: FormatOptions,
    emit: EmitOptions,
    buffer: usize,
) -> Vec<u8> {
    let mut emitter = IoEmitter::with_options(Vec::new(), format, emit.buffer_capacity(buffer))
        .expect("valid emitter");
    for record in records {
        emitter.emit_record(*record).expect("io emit");
    }
    emitter.into_inner().expect("flush")
}

fn emit_push(records: &[[&[u8]; 2]], format: FormatOptions, emit: EmitOptions) -> Vec<u8> {
    let mut emitter = PushEmitter::with_options(format, emit).expect("valid emitter");
    for record in records {
        emitter.emit_record(*record).expect("push emit");
    }
    emitter.into_inner()
}

// ── The option groups (pairwise factors) ────────────────────────────────────
//
// Each group lists individually valid values. Byte roles are chosen from
// mutually distinct bytes so that every combination materializes into a valid
// configuration; `valid_matrix_rows_are_all_accepted` proves this.

const SEP: usize = 0;
const QUOTE: usize = 1;
const POLICY: usize = 2;
const COMMENT: usize = 3;
const TRIM: usize = 4;
const BLANK: usize = 5;
const SYNTAX: usize = 6;
const READBOM: usize = 7;
const WRITEBOM: usize = 8;
const SKIP: usize = 9;
const HEADERS: usize = 10;
const FIELDCOUNT: usize = 11;
const LIMITS: usize = 12;
const BUFCAP: usize = 13;

#[cfg(feature = "multibyte")]
const SEP_COUNT: usize = 6;
#[cfg(not(feature = "multibyte"))]
const SEP_COUNT: usize = 5;

/// Sizes of each factor, in factor-index order.
fn factor_sizes() -> Vec<usize> {
    let mut sizes = vec![0usize; 14];
    sizes[SEP] = SEP_COUNT;
    sizes[QUOTE] = 2;
    sizes[POLICY] = 9;
    sizes[COMMENT] = 3;
    sizes[TRIM] = 5;
    sizes[BLANK] = 2;
    sizes[SYNTAX] = 5;
    sizes[READBOM] = 3;
    sizes[WRITEBOM] = 2;
    sizes[SKIP] = 2;
    sizes[HEADERS] = 3;
    sizes[FIELDCOUNT] = 3;
    sizes[LIMITS] = 2;
    sizes[BUFCAP] = 4;
    sizes
}

#[derive(Clone, Copy)]
enum HeadersKind {
    None,
    First,
    Provided,
}

/// A fully materialized configuration derived from one covering-array row.
struct Case {
    format: FormatOptions,
    headers: HeadersKind,
    field_count: FieldCount,
    limits: Limits,
    parse_buffer: usize,
    emit_buffer: usize,
    chunk: usize,
    write_bom_emit: bool,
}

impl Case {
    fn parse_options(&self) -> ParseOptions {
        let headers = match self.headers {
            HeadersKind::None => Headers::None,
            HeadersKind::First => Headers::FirstRecord,
            HeadersKind::Provided => {
                Headers::Provided(ByteRecord::from_iter([b"h0".as_slice(), b"h1".as_slice()]))
            }
        };
        ParseOptions::new()
            .headers(headers)
            .field_count(self.field_count)
            .limits(self.limits)
    }

    fn emit_options(&self) -> EmitOptions {
        EmitOptions::new()
            .has_headers(false)
            .field_count(self.field_count)
    }

    /// The records a lossless round-trip is expected to reproduce.
    fn expected_rows(&self) -> Rows {
        let start = match self.headers {
            HeadersKind::First => 1,
            HeadersKind::None | HeadersKind::Provided => 0,
        };
        BENIGN[start..]
            .iter()
            .map(|record| record.iter().map(|f| f.to_vec()).collect())
            .collect()
    }
}

fn apply_sep(format: FormatOptions, value: usize) -> FormatOptions {
    match value {
        0 => format.delimiter(b',').record_ending(RecordEnding::Newline),
        1 => format.delimiter(b';').record_ending(RecordEnding::Newline),
        2 => format.delimiter(b'\t').record_ending(RecordEnding::Newline),
        3 => format.delimiter(b'|').record_ending(RecordEnding::CrLf),
        4 => format
            .delimiter(b',')
            .record_ending(RecordEnding::Byte(0x1e)),
        #[cfg(feature = "multibyte")]
        5 => format
            .delimiter_sequence(b"||")
            .record_ending_sequence(b"\r\n"),
        other => unreachable!("sep value {other}"),
    }
}

fn apply_quote(format: FormatOptions, value: usize) -> FormatOptions {
    match value {
        0 => format.quote(b'"'),
        1 => format.quote(b'\''),
        other => unreachable!("quote value {other}"),
    }
}

/// The escape / quoting / NULL triples, together covering every escape, every
/// quoting policy, and every NULL policy in a valid combination.
fn apply_policy(format: FormatOptions, value: usize) -> FormatOptions {
    let (escape, quoting, nulls) = match value {
        0 => (Escape::DoubleQuote, Quoting::Necessary, Nulls::None),
        1 => (Escape::DoubleQuote, Quoting::Always, Nulls::PostgresCsv),
        2 => (Escape::DoubleQuote, Quoting::Never, Nulls::None),
        3 => (Escape::DoubleQuote, Quoting::NonNumeric, Nulls::PostgresCsv),
        4 => (Escape::DoubleQuote, Quoting::Raw, Nulls::None),
        5 => (Escape::Backslash(b'\\'), Quoting::Necessary, Nulls::None),
        6 => (Escape::Backslash(b'\\'), Quoting::Never, Nulls::None),
        7 => (Escape::Mysql, Quoting::Never, Nulls::Mysql),
        8 => (Escape::Unquoted(b'\\'), Quoting::Never, Nulls::None),
        other => unreachable!("policy value {other}"),
    };
    format.escape(escape).quoting(quoting).nulls(nulls)
}

fn apply_comment(format: FormatOptions, value: usize) -> FormatOptions {
    match value {
        0 => format.comment(None),
        1 => format.comment(Some(b'#')),
        2 => format.comment(Some(b'~')),
        other => unreachable!("comment value {other}"),
    }
}

fn apply_trim(format: FormatOptions, value: usize) -> FormatOptions {
    let trim = match value {
        0 => Whitespace::NONE,
        1 => Whitespace::FIELDS,
        2 => Whitespace::HEADERS,
        3 => Whitespace::ALL,
        4 => Whitespace::ALL.unquoted_only(),
        other => unreachable!("trim value {other}"),
    };
    format.trim(trim)
}

fn apply_blank(format: FormatOptions, value: usize) -> FormatOptions {
    let blank = match value {
        0 => BlankRecords::Preserve,
        1 => BlankRecords::Skip,
        other => unreachable!("blank value {other}"),
    };
    format.blank_records(blank)
}

/// Strict, plus each recovery flag. Every variant keeps quoting enabled so a
/// quote-producing policy stays valid.
fn apply_syntax(format: FormatOptions, value: usize) -> FormatOptions {
    let syntax = match value {
        0 => Syntax::Strict,
        1 => Syntax::Compatible(Recovery::NONE.quoting(true)),
        2 => Syntax::Compatible(Recovery::NONE.quoting(true).unquoted_quotes(true)),
        3 => Syntax::Compatible(Recovery::NONE.quoting(true).any_backslash_escape(true)),
        4 => Syntax::Compatible(
            Recovery::NONE
                .quoting(true)
                .trailing_whitespace_after_quote(true),
        ),
        other => unreachable!("syntax value {other}"),
    };
    format.syntax(syntax)
}

fn read_bom(value: usize) -> ReadBom {
    match value {
        0 => ReadBom::Detect,
        1 => ReadBom::Preserve,
        2 => ReadBom::Reject,
        other => unreachable!("readbom value {other}"),
    }
}

fn field_count(value: usize) -> FieldCount {
    match value {
        0 => FieldCount::Flexible,
        1 => FieldCount::MatchFirst,
        2 => FieldCount::Exact(2),
        other => unreachable!("fieldcount value {other}"),
    }
}

fn limits(value: usize) -> Limits {
    match value {
        0 => Limits::DEFAULT,
        1 => Limits::new(4096, 1024, 64),
        other => unreachable!("limits value {other}"),
    }
}

/// Parse buffer, emit buffer, and push chunk size for a bufcap value, chosen to
/// exercise tiny refill and chunk seams as well as a comfortable capacity.
fn bufcap(value: usize) -> (usize, usize, usize) {
    match value {
        0 => (1, 1, 1),
        1 => (2, 3, 2),
        2 => (7, 5, 3),
        3 => (64, 64, 64),
        other => unreachable!("bufcap value {other}"),
    }
}

fn materialize(row: &[usize]) -> Case {
    let mut format = FormatOptions::CSV;
    format = apply_sep(format, row[SEP]);
    format = apply_quote(format, row[QUOTE]);
    format = apply_policy(format, row[POLICY]);
    format = apply_comment(format, row[COMMENT]);
    format = apply_trim(format, row[TRIM]);
    format = apply_blank(format, row[BLANK]);
    format = apply_syntax(format, row[SYNTAX]);
    format = format.read_bom(read_bom(row[READBOM]));
    let write_bom_emit = row[WRITEBOM] == 1;
    format = format.write_bom(if write_bom_emit {
        WriteBom::Emit
    } else {
        WriteBom::Omit
    });
    format = format.skip_initial_space(row[SKIP] == 1);

    let headers = match row[HEADERS] {
        0 => HeadersKind::None,
        1 => HeadersKind::First,
        2 => HeadersKind::Provided,
        other => unreachable!("headers value {other}"),
    };
    let (parse_buffer, emit_buffer, chunk) = bufcap(row[BUFCAP]);

    Case {
        format,
        headers,
        field_count: field_count(row[FIELDCOUNT]),
        limits: limits(row[LIMITS]),
        parse_buffer,
        emit_buffer,
        chunk,
        write_bom_emit,
    }
}

// ── Deterministic pairwise covering-array construction ──────────────────────

/// Build a pairwise covering array for factors of the given sizes.
///
/// Deterministic greedy construction: repeatedly take the lowest still-uncovered
/// value pair, seed a new row with it, then fill the remaining factors by
/// greedily choosing, for each, the value that covers the most currently
/// uncovered pairs (ties broken toward the lowest value). Each row removes at
/// least its seed pair, so the loop terminates; the result covers every pair.
fn pairwise(sizes: &[usize]) -> Vec<Vec<usize>> {
    let n = sizes.len();
    let mut uncovered: BTreeSet<(usize, usize, usize, usize)> = BTreeSet::new();
    for fa in 0..n {
        for fb in (fa + 1)..n {
            for va in 0..sizes[fa] {
                for vb in 0..sizes[fb] {
                    uncovered.insert((fa, va, fb, vb));
                }
            }
        }
    }

    let mut rows: Vec<Vec<usize>> = Vec::new();
    while let Some(&(seed_fa, seed_va, seed_fb, seed_vb)) = uncovered.iter().next() {
        let mut row = vec![usize::MAX; n];
        row[seed_fa] = seed_va;
        row[seed_fb] = seed_vb;

        for f in 0..n {
            if row[f] != usize::MAX {
                continue;
            }
            let mut best_value = 0;
            let mut best_gain = -1i64;
            for value in 0..sizes[f] {
                let mut gain = 0i64;
                for (g, &fixed) in row.iter().enumerate() {
                    if g == f || fixed == usize::MAX {
                        continue;
                    }
                    let pair = if g < f {
                        (g, fixed, f, value)
                    } else {
                        (f, value, g, fixed)
                    };
                    if uncovered.contains(&pair) {
                        gain += 1;
                    }
                }
                if gain > best_gain {
                    best_gain = gain;
                    best_value = value;
                }
            }
            row[f] = best_value;
        }

        for fa in 0..n {
            for fb in (fa + 1)..n {
                uncovered.remove(&(fa, row[fa], fb, row[fb]));
            }
        }
        rows.push(row);
    }
    rows
}

/// The valid covering array, generated once per call from the factor sizes.
fn valid_matrix() -> Vec<Vec<usize>> {
    pairwise(&factor_sizes())
}

// ── Tests over the valid domain ─────────────────────────────────────────────

#[test]
fn pairwise_array_covers_every_pair_and_stays_small() {
    let sizes = factor_sizes();
    let rows = pairwise(&sizes);

    let mut required: BTreeSet<(usize, usize, usize, usize)> = BTreeSet::new();
    for fa in 0..sizes.len() {
        for fb in (fa + 1)..sizes.len() {
            for va in 0..sizes[fa] {
                for vb in 0..sizes[fb] {
                    required.insert((fa, va, fb, vb));
                }
            }
        }
    }
    for row in &rows {
        for fa in 0..sizes.len() {
            for fb in (fa + 1)..sizes.len() {
                required.remove(&(fa, row[fa], fb, row[fb]));
            }
        }
    }
    assert!(required.is_empty(), "uncovered pairs remain: {required:?}");

    let cartesian: usize = sizes.iter().product();
    assert!(
        rows.len() < cartesian / 1000,
        "covering array is not far smaller than the {cartesian}-row product: {} rows",
        rows.len()
    );
}

#[test]
fn valid_matrix_rows_are_all_accepted() {
    for row in valid_matrix() {
        let case = materialize(&row);
        assert!(
            case.format.invalidity().is_none(),
            "row {row:?} materialized to an invalid format: {:?}",
            case.format.invalidity()
        );
    }
}

#[test]
fn emitters_agree_on_every_valid_row() {
    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    for row in valid_matrix() {
        let case = materialize(&row);
        let from_vec = emit_vec(&BENIGN, case.format, case.emit_options());
        let from_io = emit_io(&BENIGN, case.format, case.emit_options(), case.emit_buffer);
        let from_push = emit_push(&BENIGN, case.format, case.emit_options());
        assert_eq!(
            from_vec, from_io,
            "vec vs io emitter differ for row {row:?}"
        );
        assert_eq!(
            from_vec, from_push,
            "vec vs push emitter differ for row {row:?}"
        );
        assert_eq!(
            from_vec.starts_with(&BOM),
            case.write_bom_emit,
            "BOM presence did not follow WriteBom for row {row:?}"
        );
    }
}

#[test]
fn front_ends_agree_and_round_trip_on_every_valid_row() {
    for row in valid_matrix() {
        let case = materialize(&row);
        // Emit a clean document (no BOM) so the round-trip exercises the parse
        // side cleanly; WriteBom is covered on the emit side by
        // `emitters_agree_on_every_valid_row`, and the BOM read policies by
        // `bom_read_policies_are_consistent`.
        let format = case.format.write_bom(WriteBom::Omit);
        let document = emit_vec(&BENIGN, format, case.emit_options());

        let slice = Outcome::from_result(parse_slice(&document, format, case.parse_options()));
        let streaming = Outcome::from_result(parse_streaming(
            &document,
            format,
            case.parse_options(),
            case.parse_buffer,
        ));
        let push = Outcome::from_result(parse_push(
            &document,
            format,
            case.parse_options(),
            case.chunk,
        ));

        assert_eq!(slice, streaming, "slice vs io disagree for row {row:?}");
        assert_eq!(slice, push, "slice vs push disagree for row {row:?}");
        assert_eq!(
            slice,
            Outcome::Parsed(case.expected_rows()),
            "round-trip lost data for row {row:?}"
        );
    }
}

/// The BOM read policies behave consistently: every front end, path, and
/// buffer size agrees exactly, including refusing a rejected mark with the
/// same `RejectedBom` kind rather than a downstream syntax error.
#[test]
fn bom_read_policies_are_consistent() {
    const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
    let base = FormatOptions::CSV.write_bom(WriteBom::Omit);
    let clean = emit_vec(&BENIGN, base, EmitOptions::new().has_headers(false));
    let mut with_bom = BOM.to_vec();
    with_bom.extend_from_slice(&clean);

    let expected: Rows = BENIGN
        .iter()
        .map(|record| record.iter().map(|f| f.to_vec()).collect())
        .collect();
    let mut preserved = expected.clone();
    let mut prefixed = BOM.to_vec();
    prefixed.extend_from_slice(&preserved[0][0]);
    preserved[0][0] = prefixed;

    let options = || ParseOptions::new().headers(Headers::None);

    for &cap in &[1usize, 3, 64] {
        // Detect strips the leading BOM: identical to the clean document.
        let detect = base.read_bom(ReadBom::Detect);
        for outcome in [
            Outcome::from_result(parse_slice(&with_bom, detect, options())),
            Outcome::from_result(parse_streaming(&with_bom, detect, options(), cap)),
            Outcome::from_result(parse_push(&with_bom, detect, options(), cap)),
        ] {
            assert_eq!(
                outcome,
                Outcome::Parsed(expected.clone()),
                "Detect did not strip the BOM at cap {cap}"
            );
        }

        // Preserve keeps the BOM as a prefix of the first field.
        let preserve = base.read_bom(ReadBom::Preserve);
        for outcome in [
            Outcome::from_result(parse_slice(&with_bom, preserve, options())),
            Outcome::from_result(parse_streaming(&with_bom, preserve, options(), cap)),
            Outcome::from_result(parse_push(&with_bom, preserve, options(), cap)),
        ] {
            assert_eq!(
                outcome,
                Outcome::Parsed(preserved.clone()),
                "Preserve did not keep the BOM at cap {cap}"
            );
        }

        // Reject refuses the BOM identically across every front end, path, and
        // buffer size: the same `RejectedBom` kind, never a downstream syntax
        // error. This once diverged (SliceParser reported `Configuration` at
        // construction; the IoParser general path leaked `UnexpectedQuote` when
        // a quote followed the mark), so the equality below is the regression
        // guard for that fix.
        let reject = base.read_bom(ReadBom::Reject);
        for outcome in [
            Outcome::from_result(parse_slice(&with_bom, reject, options())),
            Outcome::from_result(parse_streaming(&with_bom, reject, options(), cap)),
            Outcome::from_result(parse_push(&with_bom, reject, options(), cap)),
        ] {
            assert_eq!(
                outcome,
                Outcome::Failed(ErrorKind::RejectedBom),
                "Reject did not report RejectedBom at cap {cap}"
            );
        }
        // The general (eager) path and a quote straddling the mark are the exact
        // shapes that can hide a syntax error; every front end must still
        // report the mark itself.
        let reject_general = reject.comment(Some(b'#'));
        let mut with_bom_quote = BOM.to_vec();
        with_bom_quote.extend_from_slice(b"\"a\",b\nc,d\n");
        for format in [reject, reject_general] {
            for outcome in [
                Outcome::from_result(parse_slice(&with_bom_quote, format, options())),
                Outcome::from_result(parse_streaming(&with_bom_quote, format, options(), cap)),
                Outcome::from_result(parse_push(&with_bom_quote, format, options(), cap)),
            ] {
                assert_eq!(
                    outcome,
                    Outcome::Failed(ErrorKind::RejectedBom),
                    "Reject leaked a syntax error at cap {cap} for {format:?}"
                );
            }
        }
        // A document with no BOM is accepted under Reject.
        assert_eq!(
            Outcome::from_result(parse_slice(&clean, reject, options())),
            Outcome::Parsed(expected.clone()),
            "Reject rejected a BOM-free document"
        );
    }
}

// ── Tests over the invalid domain ───────────────────────────────────────────

/// Each invalid configuration and the documented reason its construction must
/// report. Every one asserts a rejected constructor, never a skipped case.
#[test]
fn invalid_configurations_are_rejected() {
    let base_parse = || ParseOptions::new().headers(Headers::None);

    // (format, reason substring). Byte-role collisions.
    let byte_role_cases: Vec<(FormatOptions, &str)> = vec![
        (
            FormatOptions::CSV.delimiter(b'"'),
            "delimiter and quote must be distinct",
        ),
        (
            FormatOptions::CSV.delimiter(b'\n'),
            "record_ending must differ from delimiter and quote",
        ),
        (
            FormatOptions::CSV.escape(Escape::Backslash(b',')),
            "escape must differ from structural bytes",
        ),
        (
            FormatOptions::CSV
                .escape(Escape::Unquoted(b','))
                .quoting(Quoting::Never),
            "escape must differ from structural bytes",
        ),
        (
            FormatOptions::CSV
                .delimiter(b'\\')
                .escape(Escape::Mysql)
                .quoting(Quoting::Never),
            "MySQL escape byte must differ from structural bytes",
        ),
        (
            FormatOptions::CSV.comment(Some(b',')),
            "comment must differ from structural bytes",
        ),
    ];

    // Incompatible policy pairs.
    let policy_cases: Vec<(FormatOptions, &str)> = vec![
        (
            FormatOptions::CSV
                .escape(Escape::Unquoted(b'\\'))
                .quoting(Quoting::Always),
            "unquoted escaping requires Quoting::Never",
        ),
        (
            FormatOptions::CSV
                .syntax(Syntax::Compatible(Recovery::NONE))
                .quoting(Quoting::Always),
            "quote-producing output requires parser quote syntax",
        ),
        (
            FormatOptions::CSV
                .nulls(Nulls::PostgresCsv)
                .quoting(Quoting::Never),
            "PostgreSQL CSV NULLs require protective quoting",
        ),
        (
            FormatOptions::CSV
                .nulls(Nulls::Mysql)
                .quoting(Quoting::Never),
            "MySQL NULLs require quoting or unquoted escaping",
        ),
    ];

    for (format, reason) in byte_role_cases.into_iter().chain(policy_cases) {
        assert_eq!(
            format.invalidity(),
            Some(reason),
            "invalidity oracle disagreed for {format:?}"
        );
        let error = SliceParser::with_options(b"a,b\n", format, base_parse())
            .expect_err("invalid configuration must be rejected");
        assert_eq!(error.kind(), ErrorKind::Configuration, "for {format:?}");
        assert!(
            error.to_string().contains(reason),
            "error {error} did not mention {reason:?}"
        );
        // The emitter must reject the same configuration.
        assert!(
            VecEmitter::with_options(Vec::new(), format, EmitOptions::new()).is_err(),
            "emitter accepted invalid {format:?}"
        );
    }

    // A zero buffer capacity is rejected at parser build, not by `invalidity`.
    assert!(FormatOptions::CSV.invalidity().is_none());
    let zero_buffer = SliceParser::with_options(
        b"a,b\n",
        FormatOptions::CSV,
        ParseOptions::new()
            .headers(Headers::None)
            .buffer_capacity(0),
    )
    .expect_err("zero buffer capacity must be rejected");
    assert_eq!(zero_buffer.kind(), ErrorKind::Configuration);
    assert!(
        zero_buffer
            .to_string()
            .contains("buffer capacity must be greater than zero")
    );
}

#[cfg(feature = "multibyte")]
#[test]
fn invalid_multibyte_configurations_are_rejected() {
    let base_parse = || ParseOptions::new().headers(Headers::None);
    let cases: Vec<(FormatOptions, &str)> = vec![
        (
            FormatOptions::CSV.delimiter_sequence(b"abcde"),
            "a delimiter or record ending sequence must be 1 to 4 bytes",
        ),
        (
            FormatOptions::CSV.delimiter_sequence(b"a\"b"),
            "a delimiter or record ending sequence must not contain the quote or escape byte",
        ),
    ];
    for (format, reason) in cases {
        assert_eq!(format.invalidity(), Some(reason), "for {format:?}");
        let error = SliceParser::with_options(b"a,b\n", format, base_parse())
            .expect_err("invalid multibyte configuration must be rejected");
        assert_eq!(error.kind(), ErrorKind::Configuration);
        assert!(error.to_string().contains(reason), "error {error}");
    }
}

/// A record that exceeds a configured limit is rejected, not silently accepted.
#[test]
fn configured_limits_reject_oversized_input() {
    let format = FormatOptions::CSV;
    let options = ParseOptions::new()
        .headers(Headers::None)
        .limits(Limits::new(1024, 4, 16));
    let error = parse_slice(b"toolongfield,b\n", format, options)
        .expect_err("field over the limit must be rejected");
    assert_eq!(error.kind(), ErrorKind::FieldTooLarge { limit: 4 });
}

// ── Static / dynamic and parallel subsets ───────────────────────────────────

fn static_dynamic_agree<F: coseva::format::StaticFormat>() {
    let format = <F as coseva::format::StaticFormat>::FORMAT;
    let document = emit_vec(&BENIGN, format, EmitOptions::new().has_headers(false));
    let options = || ParseOptions::new().headers(Headers::None);

    let mut static_parser =
        SliceParser::<F>::new(&document, options()).expect("static parser constructs");
    let mut static_rows = Rows::new();
    while let Some(mut line) = static_parser.next_line().expect("static next_line") {
        let record = line.record().expect("static record");
        static_rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }

    let dynamic_rows = parse_slice(&document, format, options()).expect("dynamic parse");
    assert_eq!(
        static_rows, dynamic_rows,
        "static and dynamic parsers disagree for a built-in format"
    );
}

#[test]
fn static_and_dynamic_parsers_agree() {
    use coseva::format::{
        BackslashCsv, BackslashTsv, CommentedCsv, Csv, Excel, Mysql, Pipe, PostgresCopyCsv,
        PythonCsv, PythonEscaped, Rfc4180, Semicolon, TrimmedCsv, Tsv,
    };
    static_dynamic_agree::<Csv>();
    static_dynamic_agree::<Tsv>();
    static_dynamic_agree::<Semicolon>();
    static_dynamic_agree::<Pipe>();
    static_dynamic_agree::<BackslashCsv>();
    static_dynamic_agree::<BackslashTsv>();
    static_dynamic_agree::<CommentedCsv>();
    static_dynamic_agree::<TrimmedCsv>();
    static_dynamic_agree::<PythonCsv>();
    static_dynamic_agree::<PythonEscaped>();
    static_dynamic_agree::<Rfc4180>();
    static_dynamic_agree::<Excel>();
    static_dynamic_agree::<PostgresCopyCsv>();
    static_dynamic_agree::<Mysql>();
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_parser_agrees_with_slice() {
    use coseva::parallel::ParallelParser;

    // A representative valid subset that the parallel splitter supports: it
    // requires double-quote escaping (policies 0..5), no comments, single-byte
    // separators, and enabled quoting, all of which these formats satisfy.
    for sep in 0..5usize {
        for policy in 0..5usize {
            let mut format = FormatOptions::CSV;
            format = apply_sep(format, sep);
            format = apply_policy(format, policy);
            let document = emit_vec(&BENIGN, format, EmitOptions::new().has_headers(false));

            let slice = parse_slice(
                &document,
                format,
                ParseOptions::new().headers(Headers::None),
            )
            .expect("slice parse");
            let parallel =
                ParallelParser::with_options(format, ParseOptions::new().headers(Headers::None))
                    .byte_records(&document)
                    .expect("parallel parse");
            let parallel_rows: Rows = parallel
                .iter()
                .map(|record| record.iter().map(<[u8]>::to_vec).collect())
                .collect();
            assert_eq!(
                slice, parallel_rows,
                "parallel and slice disagree for sep {sep}, policy {policy}"
            );
        }
    }
}

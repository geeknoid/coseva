//! Property-based and coverage-guided tests driven by [`bolero`].
//!
//! Each target runs as an ordinary, bounded test under `cargo test`, replaying
//! its tracked corpus and a capped number of generated inputs so the suite
//! stays fast. The same targets run far deeper under a coverage-guided engine:
//!
//! ```text
//! crates/coseva/scripts/fuzz_campaign.py <target_name>
//! ```
//!
//! # Design: two configuration domains
//!
//! Generated configurations are split into a *valid* domain and an *invalid*
//! domain, because a single generator that mixes them cannot tell a parser bug
//! from a configuration rule it happened to trip over (item T4).
//!
//! * The valid domain ([`ValidDialect`]) assigns byte roles in a fixed order so
//!   the result is a configuration the crate always accepts. Its properties
//!   *fail* if construction is rejected, rather than silently dropping the
//!   case, so an agreement or round-trip property can never "pass" by having
//!   every front end reject the same format.
//! * The invalid domain ([`InvalidDialect`]) builds a configuration that
//!   violates exactly one documented rule and asserts that specific rule fires.
//!   A rejected constructor is the asserted outcome, never a skipped case.
//!
//! [`valid_domain_reaches_every_public_variant`] is a deterministic companion
//! that enumerates every public option variant — including the ones random
//! generation is least likely to reach, such as [`Escape::Unquoted`],
//! [`WriteBom::Emit`], [`Headers::Provided`], emitter field-count policies,
//! non-default I/O buffer capacities, and (under the `multibyte` feature)
//! multi-byte separators — and proves each parser and emitter body runs on it.
//!
//! # Corpus and campaign
//!
//! Coverage-increasing and regression inputs live under
//! `tests/__fuzz__/<target_name>/corpus/`, which bolero replays on every
//! `cargo test`. The raw-byte targets ([`raw_parsing_across_front_ends`],
//! [`parse_emit_round_trip`], and [`filter_matches_manual_scan`]) take their
//! corpus files as literal input bytes, so a discovered failure is committed
//! there verbatim as a permanent regression. The machine-readable campaign
//! definition CI invokes is `tests/__fuzz__/campaign.toml`.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::collections::BTreeSet;
use std::io::Cursor;
use std::time::Duration;

use bolero::TypeGenerator;

use coseva::config::{
    BlankRecords, EmitOptions, Escape, FieldCount, FormatOptions, Headers, Limits, Nulls,
    ParseOptions, Quoting, ReadBom, RecordEnding, Recovery, Syntax, Whitespace, WriteBom,
};
use coseva::{
    ByteRecord, Error, ErrorKind, IoEmitter, IoParser, Predicate, PushEmitter, PushParser,
    SliceParser, VecEmitter,
};

/// Records reduced to plain bytes, so results from different parsers compare.
type Rows = Vec<Vec<Vec<u8>>>;

/// Iterations each bounded `cargo test` run performs on top of the corpus.
///
/// Small on purpose: ordinary test runs stay fast while the corpus still
/// replays in full. The campaign builds with `--cfg fuzzing`, where these
/// calls are no-ops, so a real campaign is never capped by them.
const BOUNDED_ITERATIONS: usize = 4096;

/// Wall-clock cap for a bounded run, a backstop for the iteration cap.
const BOUNDED_TEST_TIME: Duration = Duration::from_millis(400);

/// Bytes the generators draw structural characters from.
///
/// Restricting the alphabet keeps generated inputs dense in the characters that
/// drive parser state transitions rather than in inert payload bytes.
const STRUCTURAL: &[u8] = b",;\t|\"'\\\r\n#\0 ";

/// A parse outcome reduced to what every parser must agree on.
///
/// Errors compare by raw [`ErrorKind`]: every front end now refuses a rejected
/// BOM with the same [`ErrorKind::RejectedBom`], so no class normalization is
/// needed and a genuine cross-front-end disagreement is never masked.
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

/// Collect every record a slice parser yields.
fn parse_slice(input: &[u8], format: FormatOptions, options: ParseOptions) -> Result<Rows, Error> {
    let mut parser = SliceParser::with_options(input, format, options)?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

/// Collect every record a streaming parser yields, over a chosen buffer size.
fn parse_streaming(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
) -> Result<Rows, Error> {
    let mut parser = IoParser::with_options(Cursor::new(input), format, options)?;
    let mut rows = Rows::new();
    while let Some(mut line) = parser.next_line()? {
        let record = line.record()?;
        rows.push(record.iter().map(<[u8]>::to_vec).collect());
    }
    Ok(rows)
}

/// Collect every record a push parser yields when lent fixed-size chunks.
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
        // A loan is only taken in part when the pending record would outgrow
        // its limit, so the chunk is offered again from where it stopped.
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

// ── Valid configuration domain ──────────────────────────────────────────────

/// A generated CSV dialect drawn from the *valid* domain.
///
/// The fields are raw bytes rather than the crate's enums so that the fuzzer
/// mutates a flat byte string. Each is folded into range and byte roles are
/// assigned in a fixed order that skips collisions, so [`Self::format`] always
/// yields a configuration the crate accepts. The properties over this type
/// assert exactly that, rather than tolerating a rejection.
#[derive(Debug, Clone, Copy, TypeGenerator)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each field is an independent generator knob, not a state flag"
)]
struct ValidDialect {
    delimiter: u8,
    quote: u8,
    record_ending: u8,
    escape: u8,
    escape_byte: u8,
    comment: u8,
    trim: u8,
    blank_records: u8,
    syntax: u8,
    recovery: u8,
    nulls: u8,
    quoting: u8,
    read_bom: u8,
    write_bom: bool,
    skip_initial_space: bool,
    headers: u8,
    provided_headers: bool,
    field_count: u8,
    emit_field_count: u8,
    parse_buffer: u8,
    emit_buffer: u8,
    multibyte_delimiter: bool,
    max_record_bytes: u16,
    max_field_bytes: u16,
    max_fields: u8,
}

impl ValidDialect {
    /// The delimiter this dialect resolves to.
    ///
    /// CR and LF are reserved first, matching [`Self::byte_roles`], so that the
    /// delimiter the document is rendered with is the same byte the format
    /// parses on.
    fn delimiter(self) -> u8 {
        Self::pick(self.delimiter, b"\r\n")
    }

    /// Assign every structural byte role so no two collide.
    ///
    /// CR and LF are reserved up front, so a `Newline` or `CrLf` record ending
    /// never lands on the delimiter, quote, escape or comment. Roles are then
    /// chosen in a fixed order, each avoiding the ones already taken, so the
    /// result always satisfies [`FormatOptions::invalidity`].
    fn byte_roles(self) -> (u8, u8, RecordEnding, Escape, Option<u8>) {
        let mut taken = vec![b'\r', b'\n'];
        let delimiter = pick_into(&mut taken, self.delimiter);
        let quote = pick_into(&mut taken, self.quote);
        let ending = match self.record_ending % 3 {
            0 => RecordEnding::Newline,
            1 => RecordEnding::CrLf,
            _ => RecordEnding::Byte(pick_into(&mut taken, self.record_ending)),
        };
        let escape = match self.escape % 4 {
            1 => Escape::Backslash(pick_into(&mut taken, self.escape_byte)),
            2 if !taken.contains(&b'\\') => {
                taken.push(b'\\');
                Escape::Mysql
            }
            3 => Escape::Unquoted(pick_into(&mut taken, self.escape_byte)),
            _ => Escape::DoubleQuote,
        };
        let comment = if self.comment.is_multiple_of(4) {
            None
        } else {
            Some(pick_into(&mut taken, self.comment))
        };
        (delimiter, quote, ending, escape, comment)
    }

    /// The quoting policy this dialect resolves to.
    ///
    /// An unquoted escape forces [`Quoting::Never`], the only policy compatible
    /// with it; every other case is free to vary. Quote syntax is always on
    /// (see [`Self::syntax_policy`]), so quote-producing policies stay valid.
    fn quoting_policy(self, escape: Escape) -> Quoting {
        if escape_escapes_unquoted(escape) {
            return Quoting::Never;
        }
        match self.quoting % 5 {
            0 => Quoting::Necessary,
            1 => Quoting::Always,
            2 => Quoting::Never,
            3 => Quoting::NonNumeric,
            _ => Quoting::Raw,
        }
    }

    /// The null-encoding policy, held to values compatible with the rest.
    fn nulls_policy(self, escape: Escape, quoting: Quoting) -> Nulls {
        let escapes_unquoted = escape_escapes_unquoted(escape);
        let protective = matches!(
            quoting,
            Quoting::Necessary | Quoting::Always | Quoting::NonNumeric
        );
        match self.nulls % 3 {
            1 if protective && !escapes_unquoted => Nulls::PostgresCsv,
            2 if protective || escapes_unquoted => Nulls::Mysql,
            _ => Nulls::None,
        }
    }

    /// Pick a structural byte, avoiding collisions with already-chosen roles.
    fn pick(seed: u8, taken: &[u8]) -> u8 {
        let start = usize::from(seed) % STRUCTURAL.len();
        for offset in 0..STRUCTURAL.len() {
            let candidate = STRUCTURAL[(start + offset) % STRUCTURAL.len()];
            if !taken.contains(&candidate) {
                return candidate;
            }
        }
        b','
    }

    /// Realize the format half of the dialect.
    fn format(self) -> FormatOptions {
        let (delimiter, quote, ending, escape, comment) = self.byte_roles();
        let trim = match self.trim % 5 {
            0 => Whitespace::NONE,
            1 => Whitespace::FIELDS,
            2 => Whitespace::HEADERS,
            3 => Whitespace::ALL,
            _ => Whitespace::ALL.unquoted_only(),
        };
        let syntax = self.syntax_policy();
        let quoting = self.quoting_policy(escape);
        let nulls = self.nulls_policy(escape, quoting);

        let format = FormatOptions::new()
            .delimiter(delimiter)
            .quote(quote)
            .record_ending(ending)
            .escape(escape)
            .comment(comment)
            .trim(trim)
            .blank_records(if self.blank_records.is_multiple_of(2) {
                BlankRecords::Preserve
            } else {
                BlankRecords::Skip
            })
            .syntax(syntax)
            .nulls(nulls)
            .quoting(quoting)
            .read_bom(match self.read_bom % 3 {
                0 => ReadBom::Detect,
                1 => ReadBom::Preserve,
                _ => ReadBom::Reject,
            })
            .write_bom(if self.write_bom {
                WriteBom::Emit
            } else {
                WriteBom::Omit
            })
            .skip_initial_space(self.skip_initial_space);

        self.apply_multibyte(format, delimiter, quote, escape)
    }

    /// Layer a multi-byte delimiter on, keeping its tail off the quote/escape.
    #[cfg(feature = "multibyte")]
    fn apply_multibyte(
        self,
        format: FormatOptions,
        delimiter: u8,
        quote: u8,
        escape: Escape,
    ) -> FormatOptions {
        if !self.multibyte_delimiter {
            return format;
        }
        // Only the tail byte is constrained, and only against the quote and the
        // escape's own byte; it may otherwise repeat the lead delimiter.
        let escape_byte = match escape {
            Escape::DoubleQuote => quote,
            Escape::Backslash(byte) | Escape::Unquoted(byte) => byte,
            Escape::Mysql => b'\\',
        };
        let filler = Self::pick(self.escape_byte.wrapping_add(1), &[quote, escape_byte]);
        format.delimiter_sequence(&[delimiter, filler])
    }

    /// Without the feature there is nothing to layer.
    #[cfg(not(feature = "multibyte"))]
    #[expect(
        clippy::unused_self,
        reason = "mirrors the multibyte-enabled signature"
    )]
    fn apply_multibyte(
        self,
        format: FormatOptions,
        _delimiter: u8,
        _quote: u8,
        _escape: Escape,
    ) -> FormatOptions {
        format
    }

    /// Realize the strictness policy, exercising each recovery flag.
    ///
    /// Quoting stays enabled so any quoting policy remains compatible; the
    /// three other flags vary freely.
    fn syntax_policy(self) -> Syntax {
        if self.syntax.is_multiple_of(2) {
            return Syntax::Strict;
        }
        Syntax::Compatible(
            Recovery::NONE
                .quoting(true)
                .unquoted_quotes(self.recovery & 2 != 0)
                .any_backslash_escape(self.recovery & 4 != 0)
                .trailing_whitespace_after_quote(self.recovery & 8 != 0),
        )
    }

    /// Headers policy, including the caller-provided variant.
    fn headers(self) -> Headers {
        match self.headers % 3 {
            0 => Headers::None,
            1 => Headers::FirstRecord,
            _ if self.provided_headers => {
                Headers::Provided(ByteRecord::from_iter([b"h0".as_slice(), b"h1"]))
            }
            _ => Headers::FirstRecord,
        }
    }

    /// Realize the parse half of the dialect.
    fn parse(self) -> ParseOptions {
        ParseOptions::new()
            .headers(self.headers())
            .field_count(field_count(self.field_count))
            .buffer_capacity(buffer_capacity(self.parse_buffer))
            .limits(self.limits())
    }

    /// Realize emitter options, covering every field-count policy and capacity.
    fn emit(self) -> EmitOptions {
        EmitOptions::new()
            .field_count(field_count(self.emit_field_count))
            .buffer_capacity(buffer_capacity(self.emit_buffer))
            .has_headers(self.write_bom)
    }

    /// A format that preserves field bytes exactly, for round-tripping.
    ///
    /// Uses the same collision-free byte roles as [`Self::format`] but pins the
    /// escape to `DoubleQuote` and forces `Quoting::Always`, so every field —
    /// including ones holding the delimiter, quote or a record ending — is
    /// written quoted and read back byte-for-byte.
    fn round_trip_format(self) -> FormatOptions {
        let (delimiter, quote, ending, _escape, _comment) = self.byte_roles();
        FormatOptions::new()
            .delimiter(delimiter)
            .quote(quote)
            .record_ending(ending)
            .escape(Escape::DoubleQuote)
            .comment(None)
            .trim(Whitespace::NONE)
            .blank_records(BlankRecords::Preserve)
            .syntax(Syntax::Strict)
            .nulls(Nulls::None)
            .quoting(Quoting::Always)
            .read_bom(ReadBom::Preserve)
            .write_bom(WriteBom::Omit)
            .skip_initial_space(false)
    }

    /// Resource limits kept small enough to actually bind.
    fn limits(self) -> Limits {
        Limits::new(
            usize::from(self.max_record_bytes % 512) + 1,
            usize::from(self.max_field_bytes % 256) + 1,
            usize::from(self.max_fields % 16) + 1,
        )
    }

    /// Parse options whose limits never bind, for cross-front-end agreement.
    fn parse_unbounded(self) -> ParseOptions {
        self.parse().limits(Limits::new(1 << 20, 1 << 20, 1 << 10))
    }

    /// Render generated content as a document in this dialect's delimiter.
    fn render(self, input: &Input) -> Vec<u8> {
        input.render(self.delimiter())
    }
}

/// Map a byte to a field-count policy, covering all three variants.
fn field_count(seed: u8) -> FieldCount {
    match seed % 3 {
        0 => FieldCount::Flexible,
        1 => FieldCount::MatchFirst,
        _ => FieldCount::Exact(usize::from(seed % 8) + 1),
    }
}

/// Map a byte to a buffer capacity, half the time a non-default value.
fn buffer_capacity(seed: u8) -> usize {
    if seed.is_multiple_of(2) {
        8 * 1024
    } else {
        usize::from(seed) + 1
    }
}

/// Pick a structural byte with [`ValidDialect::pick`], then reserve it.
fn pick_into(taken: &mut Vec<u8>, seed: u8) -> u8 {
    let byte = ValidDialect::pick(seed, taken);
    taken.push(byte);
    byte
}

/// Whether an escape applies outside quoted fields.
fn escape_escapes_unquoted(escape: Escape) -> bool {
    matches!(escape, Escape::Mysql | Escape::Unquoted(_))
}

/// Generated CSV input.
///
/// A raw byte string alone rarely forms a well-shaped record, so inputs are
/// generated as a structure and rendered. The rendered text still contains
/// quotes, delimiters and newlines drawn from the structural alphabet, so it
/// reaches the same states raw bytes would, but far more often.
#[derive(Debug, Clone, TypeGenerator)]
struct Input {
    /// Rendered directly, letting the fuzzer reach states the shaped form cannot.
    raw: Vec<u8>,
    /// Rendered as delimiter-joined, newline-separated fields.
    shaped: Vec<Vec<Vec<u8>>>,
    use_raw: bool,
}

impl Input {
    fn render(&self, delimiter: u8) -> Vec<u8> {
        if self.use_raw {
            return self.raw.clone();
        }
        let mut out = Vec::new();
        for row in &self.shaped {
            for (index, field) in row.iter().enumerate() {
                if index > 0 {
                    out.push(delimiter);
                }
                out.extend_from_slice(field);
            }
            out.push(b'\n');
        }
        out
    }
}

// ── Bounded-run helper ──────────────────────────────────────────────────────

/// Cap an ordinary `cargo test` run so the whole suite stays fast; a no-op
/// under the coverage-guided campaign, which sets its own budget.
///
/// A macro rather than a function so it applies to both the [`TypeGenerator`]
/// targets and the raw `&[u8]` targets, whose generator and engine types
/// differ and which no single generic signature covers cleanly.
macro_rules! bounded {
    () => {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
    };
}

// ── Property: the valid domain is genuinely valid ───────────────────────────

#[test]
fn valid_configurations_always_construct() {
    bounded!().with_type::<ValidDialect>().for_each(|dialect| {
        let format = dialect.format();
        assert!(
            format.invalidity().is_none(),
            "valid domain produced a rejected format: {:?} ({:?})",
            format.invalidity(),
            format,
        );
        // Every front end must accept it, since the domain is valid.
        SliceParser::with_options(b"", format, dialect.parse())
            .expect("slice parser rejected a valid configuration");
        IoParser::with_options(Cursor::new(b""), format, dialect.parse())
            .expect("io parser rejected a valid configuration");
        PushParser::with_options(format, dialect.parse())
            .expect("push parser rejected a valid configuration");
        VecEmitter::with_options(Vec::new(), format, dialect.emit())
            .expect("vec emitter rejected a valid configuration");
    });
}

// ── Property: parsers never panic and agree ─────────────────────────────────

#[test]
fn slice_parser_never_panics() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let bytes = dialect.render(input);
            drop(parse_slice(&bytes, format, dialect.parse()));
        });
}

#[test]
fn streaming_parser_never_panics() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let bytes = dialect.render(input);
            drop(parse_streaming(&bytes, format, dialect.parse()));
        });
}

#[test]
fn push_parser_never_panics() {
    bounded!()
        .with_type::<(ValidDialect, Input, u8)>()
        .for_each(|(dialect, input, chunk)| {
            let format = dialect.format();
            let bytes = dialect.render(input);
            drop(parse_push(
                &bytes,
                format,
                dialect.parse(),
                usize::from(*chunk),
            ));
        });
}

#[test]
fn all_parsers_agree() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let options = dialect.parse_unbounded();
            let bytes = dialect.render(input);

            let slice = Outcome::from_result(parse_slice(&bytes, format, options.clone()));
            let streaming = Outcome::from_result(parse_streaming(&bytes, format, options));

            assert_eq!(
                slice, streaming,
                "slice and streaming parsers disagreed on {bytes:?}"
            );
        });
}

#[test]
fn push_parser_agrees_regardless_of_chunking() {
    bounded!()
        .with_type::<(ValidDialect, Input, u8)>()
        .for_each(|(dialect, input, chunk)| {
            let format = dialect.format();
            let options = dialect.parse_unbounded();
            let bytes = dialect.render(input);

            let whole = Outcome::from_result(parse_push(
                &bytes,
                format,
                options.clone(),
                bytes.len().max(1),
            ));
            let split = Outcome::from_result(parse_push(
                &bytes,
                format,
                options,
                usize::from(*chunk).max(1),
            ));

            assert_eq!(
                whole, split,
                "push parser depended on chunk boundaries for {bytes:?}"
            );
        });
}

#[test]
fn push_parser_agrees_with_slice_parser() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let options = dialect.parse_unbounded();
            let bytes = dialect.render(input);

            let slice = Outcome::from_result(parse_slice(&bytes, format, options.clone()));
            let push =
                Outcome::from_result(parse_push(&bytes, format, options, bytes.len().max(1)));

            assert_eq!(slice, push, "slice and push parsers disagreed on {bytes:?}");
        });
}

// ── Property: encoding round-trips through parsing ──────────────────────────

#[test]
fn encoding_round_trips_through_parsing() {
    bounded!()
        .with_type::<(ValidDialect, Vec<Vec<Vec<u8>>>)>()
        .for_each(|(dialect, rows)| {
            let format = dialect.round_trip_format();
            // Round-tripping is only defined when every record carries the same
            // non-zero number of fields, since a parse cannot recover a record
            // boundary the emitter never had a reason to write.
            let Some(width) = rows.first().map(Vec::len).filter(|width| *width > 0) else {
                return;
            };
            if rows.iter().any(|row| row.len() != width) {
                return;
            }

            // Construction cannot fail here: `round_trip_format` is valid by
            // design, so a failure is a bug, not a case to skip.
            let mut emitter =
                VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))
                    .expect("round-trip format must construct an emitter");
            for row in rows {
                emitter.emit_record(row.iter()).expect("encode a record");
            }
            let reencoded = emitter.into_inner();

            let parsed = parse_slice(
                &reencoded,
                format,
                ParseOptions::new()
                    .headers(Headers::None)
                    .limits(Limits::new(1 << 20, 1 << 20, 1 << 10)),
            );

            assert_eq!(
                parsed.as_deref().ok(),
                Some(rows.as_slice()),
                "round trip lost data for {rows:?} encoded as {reencoded:?}"
            );
        });
}

// ── Property: limits and accessors ──────────────────────────────────────────

#[test]
fn limits_are_never_exceeded() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let limits = dialect.limits();
            let bytes = dialect.render(input);

            let Ok(rows) = parse_slice(&bytes, format, dialect.parse()) else {
                return;
            };
            for row in &rows {
                assert!(
                    row.len() <= limits.max_fields,
                    "record exceeded the field limit"
                );
                for field in row {
                    assert!(
                        field.len() <= limits.max_field_bytes,
                        "field exceeded the byte limit"
                    );
                }
            }
        });
}

#[test]
fn record_accessors_are_self_consistent() {
    bounded!()
        .with_type::<(ValidDialect, Input)>()
        .for_each(|(dialect, input)| {
            let format = dialect.format();
            let bytes = dialect.render(input);
            let Ok(mut parser) = SliceParser::with_options(&bytes, format, dialect.parse()) else {
                return;
            };

            while let Ok(Some(mut line)) = parser.next_line() {
                let Ok(record) = line.record() else { return };
                let collected: Vec<_> = record.iter().collect();

                assert_eq!(record.len(), collected.len(), "len disagreed with iter");
                assert_eq!(record.is_empty(), collected.is_empty());
                assert_eq!(record.get(record.len()), None, "read past the last field");

                for (index, field) in collected.iter().enumerate() {
                    assert_eq!(record.get(index), Some(*field), "get disagreed with iter");
                    match record.get_str(index) {
                        Ok(Some(text)) => assert_eq!(text.as_bytes(), *field),
                        Ok(None) => assert!(record.is_null(index).unwrap_or(false)),
                        Err(_) => drop(std::str::from_utf8(field).expect_err("non-UTF-8 field")),
                    }
                }
            }
        });
}

// ── Invalid configuration domain ────────────────────────────────────────────

/// A generated configuration that violates exactly one documented rule.
#[derive(Debug, Clone, Copy, TypeGenerator)]
struct InvalidDialect {
    rule: u8,
    byte: u8,
}

/// One built invalid configuration and the exact reason it must be rejected.
struct InvalidCase {
    format: FormatOptions,
    reason: &'static str,
    parse: ParseOptions,
}

/// The reason string of the one non-format (buffer-capacity) invalid rule.
const BUFFER_CAPACITY_REASON: &str = "buffer capacity must be greater than zero";

impl InvalidDialect {
    /// The number of rules the invalid domain enumerates.
    #[cfg(not(feature = "multibyte"))]
    const RULES: u8 = 10;
    #[cfg(feature = "multibyte")]
    const RULES: u8 = 12;

    fn case(self) -> InvalidCase {
        invalid_case(self.rule % Self::RULES, self.byte)
    }
}

/// Build the invalid configuration for a rule index and a mutable byte.
///
/// Each arm violates a single rule from [`FormatOptions::invalidity`] (or the
/// buffer-capacity check), so the assertion can name the exact incompatibility
/// rather than accepting any error.
fn invalid_case(rule: u8, byte: u8) -> InvalidCase {
    let _ = byte;
    let base = FormatOptions::CSV;
    let headerless = || ParseOptions::new().headers(Headers::None);
    match rule {
        0 => InvalidCase {
            format: base.delimiter(b'"'),
            reason: "delimiter and quote must be distinct",
            parse: headerless(),
        },
        1 => InvalidCase {
            format: base.record_ending(RecordEnding::Byte(b',')),
            reason: "record_ending must differ from delimiter and quote",
            parse: headerless(),
        },
        2 => InvalidCase {
            format: base.escape(Escape::Backslash(b',')),
            reason: "escape must differ from structural bytes",
            parse: headerless(),
        },
        3 => InvalidCase {
            format: base.delimiter(b'\\').escape(Escape::Mysql),
            reason: "MySQL escape byte must differ from structural bytes",
            parse: headerless(),
        },
        4 => InvalidCase {
            format: base.escape(Escape::Unquoted(b',')).quoting(Quoting::Never),
            reason: "escape must differ from structural bytes",
            parse: headerless(),
        },
        5 => InvalidCase {
            format: base.comment(Some(b',')),
            reason: "comment must differ from structural bytes",
            parse: headerless(),
        },
        6 => InvalidCase {
            format: base
                .escape(Escape::Unquoted(b'\\'))
                .quoting(Quoting::Necessary),
            reason: "unquoted escaping requires Quoting::Never",
            parse: headerless(),
        },
        7 => InvalidCase {
            format: base
                .syntax(Syntax::Compatible(Recovery::NONE))
                .quoting(Quoting::Always),
            reason: "quote-producing output requires parser quote syntax",
            parse: headerless(),
        },
        8 => InvalidCase {
            format: base.nulls(Nulls::PostgresCsv).quoting(Quoting::Never),
            reason: "PostgreSQL CSV NULLs require protective quoting",
            parse: headerless(),
        },
        9 => InvalidCase {
            // A valid format, made invalid by a zero-length input buffer.
            format: base,
            reason: BUFFER_CAPACITY_REASON,
            parse: ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(0),
        },
        #[cfg(feature = "multibyte")]
        10 => InvalidCase {
            format: base.delimiter_sequence(b",abcd"),
            reason: "a delimiter or record ending sequence must be 1 to 4 bytes",
            parse: headerless(),
        },
        #[cfg(feature = "multibyte")]
        11 => InvalidCase {
            format: base.delimiter_sequence(b",\""),
            reason: "a delimiter or record ending sequence must not contain the quote or escape byte",
            parse: headerless(),
        },
        _ => InvalidCase {
            format: base.delimiter(b'"'),
            reason: "delimiter and quote must be distinct",
            parse: headerless(),
        },
    }
}

#[test]
fn invalid_configurations_assert_their_specific_rule() {
    for rule in 0..InvalidDialect::RULES {
        let case = invalid_case(rule, 0);

        // The format-level rules are reported by `invalidity` verbatim; the
        // buffer-capacity rule surfaces only when a parser is built.
        if case.reason != BUFFER_CAPACITY_REASON {
            assert_eq!(
                case.format.invalidity(),
                Some(case.reason),
                "rule {rule} did not report its documented reason",
            );
        }

        // Constructing any front end must fail with `Configuration`, and never
        // yield a usable parser that a property could count as a parse.
        let error = SliceParser::with_options(b"a,b\n", case.format, case.parse.clone())
            .expect_err(&format!("rule {rule} was accepted"));
        assert_eq!(
            error.kind(),
            ErrorKind::Configuration,
            "rule {rule} was not a configuration error",
        );
        assert!(
            error.to_string().contains(case.reason),
            "rule {rule} error {error:?} did not mention {:?}",
            case.reason,
        );

        // The emitter path rejects the same formats (except the read-only
        // buffer rule, which the emitter's own capacity governs instead).
        if case.reason != BUFFER_CAPACITY_REASON {
            assert!(
                VecEmitter::with_options(Vec::new(), case.format, EmitOptions::new()).is_err(),
                "rule {rule} was accepted by the emitter",
            );
        }
    }
}

#[test]
fn invalid_domain_never_reaches_a_parser() {
    bounded!()
        .with_type::<InvalidDialect>()
        .for_each(|dialect| {
            let case = dialect.case();
            let error = SliceParser::with_options(b"a,b\n", case.format, case.parse.clone())
                .expect_err("an invalid configuration must never build a parser");
            assert_eq!(error.kind(), ErrorKind::Configuration);
            assert!(
                error.to_string().contains(case.reason),
                "invalid case error {error:?} did not mention {:?}",
                case.reason,
            );
        });
}

// ── Deterministic coverage: every public variant reaches every body ─────────

/// A named valid configuration and the variant tags it is responsible for.
struct VariantCase {
    tags: &'static [&'static str],
    format: FormatOptions,
    parse: ParseOptions,
}

/// Simple benign records every dialect parses identically: no structural
/// bytes, no empty fields, so trimming, nulls, comments and blank-record
/// handling cannot change the result.
const BENIGN: [[&[u8]; 2]; 2] = [[b"alpha", b"bravo"], [b"charlie", b"delta"]];

/// Render [`BENIGN`] with explicit separators.
fn benign_document(delimiter: &[u8], ending: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for row in BENIGN {
        for (index, field) in row.iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(delimiter);
            }
            out.extend_from_slice(field);
        }
        out.extend_from_slice(ending);
    }
    out
}

/// Every valid parser-side variant, each on an otherwise-default CSV base.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per public variant; a table is clearer than splitting it"
)]
fn variant_cases() -> Vec<VariantCase> {
    let base = FormatOptions::CSV;
    let headerless = || ParseOptions::new().headers(Headers::None);
    let cases = vec![
        VariantCase {
            tags: &["escape.doublequote"],
            format: base,
            parse: headerless(),
        },
        VariantCase {
            tags: &["escape.backslash"],
            format: base.escape(Escape::Backslash(b'\\')),
            parse: headerless(),
        },
        VariantCase {
            tags: &["escape.mysql"],
            format: base.escape(Escape::Mysql).quoting(Quoting::Never),
            parse: headerless(),
        },
        VariantCase {
            tags: &["escape.unquoted"],
            format: base.escape(Escape::Unquoted(b'\\')).quoting(Quoting::Never),
            parse: headerless(),
        },
        VariantCase {
            tags: &["ending.newline"],
            format: base.record_ending(RecordEnding::Newline),
            parse: headerless(),
        },
        VariantCase {
            tags: &["ending.crlf"],
            format: base.record_ending(RecordEnding::CrLf),
            parse: headerless(),
        },
        VariantCase {
            tags: &["ending.byte"],
            format: base.record_ending(RecordEnding::Byte(b'|')),
            parse: headerless(),
        },
        VariantCase {
            tags: &["quoting.necessary"],
            format: base.quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["quoting.always"],
            format: base.quoting(Quoting::Always),
            parse: headerless(),
        },
        VariantCase {
            tags: &["quoting.never"],
            format: base.quoting(Quoting::Never),
            parse: headerless(),
        },
        VariantCase {
            tags: &["quoting.nonnumeric"],
            format: base.quoting(Quoting::NonNumeric),
            parse: headerless(),
        },
        VariantCase {
            tags: &["quoting.raw"],
            format: base.quoting(Quoting::Raw),
            parse: headerless(),
        },
        VariantCase {
            tags: &["nulls.none"],
            format: base.nulls(Nulls::None),
            parse: headerless(),
        },
        VariantCase {
            tags: &["nulls.postgres"],
            format: base.nulls(Nulls::PostgresCsv).quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["nulls.mysql"],
            format: base
                .escape(Escape::Mysql)
                .quoting(Quoting::Never)
                .nulls(Nulls::Mysql),
            parse: headerless(),
        },
        VariantCase {
            tags: &["readbom.detect"],
            format: base.read_bom(ReadBom::Detect),
            parse: headerless(),
        },
        VariantCase {
            tags: &["readbom.preserve"],
            format: base.read_bom(ReadBom::Preserve),
            parse: headerless(),
        },
        VariantCase {
            tags: &["readbom.reject"],
            format: base.read_bom(ReadBom::Reject),
            parse: headerless(),
        },
        VariantCase {
            tags: &["blank.preserve"],
            format: base.blank_records(BlankRecords::Preserve),
            parse: headerless(),
        },
        VariantCase {
            tags: &["blank.skip"],
            format: base.blank_records(BlankRecords::Skip),
            parse: headerless(),
        },
        VariantCase {
            tags: &["syntax.strict"],
            format: base.syntax(Syntax::Strict),
            parse: headerless(),
        },
        VariantCase {
            tags: &["recovery.quoting"],
            format: base
                .syntax(Syntax::Compatible(Recovery::NONE.quoting(true)))
                .quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["recovery.unquoted_quotes"],
            format: base
                .syntax(Syntax::Compatible(
                    Recovery::NONE.quoting(true).unquoted_quotes(true),
                ))
                .quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["recovery.any_backslash"],
            format: base
                .syntax(Syntax::Compatible(
                    Recovery::NONE.quoting(true).any_backslash_escape(true),
                ))
                .quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["recovery.trailing_ws"],
            format: base
                .syntax(Syntax::Compatible(
                    Recovery::NONE
                        .quoting(true)
                        .trailing_whitespace_after_quote(true),
                ))
                .quoting(Quoting::Necessary),
            parse: headerless(),
        },
        VariantCase {
            tags: &["trim.none"],
            format: base.trim(Whitespace::NONE),
            parse: headerless(),
        },
        VariantCase {
            tags: &["trim.fields"],
            format: base.trim(Whitespace::FIELDS),
            parse: headerless(),
        },
        VariantCase {
            tags: &["trim.headers"],
            format: base.trim(Whitespace::HEADERS),
            parse: headerless(),
        },
        VariantCase {
            tags: &["trim.all"],
            format: base.trim(Whitespace::ALL),
            parse: headerless(),
        },
        VariantCase {
            tags: &["trim.unquoted_only"],
            format: base.trim(Whitespace::ALL.unquoted_only()),
            parse: headerless(),
        },
        VariantCase {
            tags: &["skip_initial_space"],
            format: base.skip_initial_space(true),
            parse: headerless(),
        },
        VariantCase {
            tags: &["comment.none"],
            format: base.comment(None),
            parse: headerless(),
        },
        VariantCase {
            tags: &["comment.some"],
            format: base.comment(Some(b'#')),
            parse: headerless(),
        },
        VariantCase {
            tags: &["headers.none"],
            format: base,
            parse: ParseOptions::new().headers(Headers::None),
        },
        VariantCase {
            tags: &["headers.first"],
            format: base,
            parse: ParseOptions::new().headers(Headers::FirstRecord),
        },
        VariantCase {
            tags: &["headers.provided"],
            format: base,
            parse: ParseOptions::new().headers(Headers::Provided(ByteRecord::from_iter([
                b"h0".as_slice(),
                b"h1",
            ]))),
        },
        VariantCase {
            tags: &["fieldcount.flexible"],
            format: base,
            parse: ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::Flexible),
        },
        VariantCase {
            tags: &["fieldcount.matchfirst"],
            format: base,
            parse: ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::MatchFirst),
        },
        VariantCase {
            tags: &["fieldcount.exact"],
            format: base,
            parse: ParseOptions::new()
                .headers(Headers::None)
                .field_count(FieldCount::Exact(2)),
        },
        VariantCase {
            tags: &["bufcap.parse.nondefault"],
            format: base,
            parse: ParseOptions::new()
                .headers(Headers::None)
                .buffer_capacity(3),
        },
    ];

    #[cfg(feature = "multibyte")]
    let cases = {
        let mut cases = cases;
        cases.push(VariantCase {
            tags: &["delimiter.multibyte"],
            format: base.delimiter_sequence(b"||"),
            parse: headerless(),
        });
        cases.push(VariantCase {
            tags: &["ending.multibyte"],
            format: base.record_ending_sequence(b"@@"),
            parse: headerless(),
        });
        cases
    };

    cases
}

/// The separators to render a benign document for a variant case.
fn separators_for(tags: &[&str]) -> (Vec<u8>, Vec<u8>) {
    let mut delimiter = vec![b','];
    let mut ending = vec![b'\n'];
    for tag in tags {
        match *tag {
            "ending.crlf" => ending = vec![b'\r', b'\n'],
            "ending.byte" => ending = vec![b'|'],
            "delimiter.multibyte" => delimiter = vec![b'|', b'|'],
            "ending.multibyte" => ending = vec![b'@', b'@'],
            _ => {}
        }
    }
    (delimiter, ending)
}

/// Expected data-row count once headers are accounted for.
///
/// [`BENIGN`] is two records; only `Headers::FirstRecord` consumes one as a
/// header row, so every other configuration yields both.
fn expected_rows(tags: &[&str]) -> usize {
    if tags.contains(&"headers.first") {
        1
    } else {
        2
    }
}

#[test]
fn valid_domain_reaches_every_public_variant() {
    let mut reached: BTreeSet<&'static str> = BTreeSet::new();

    for case in variant_cases() {
        assert_eq!(
            case.format.invalidity(),
            None,
            "variant case {:?} built an invalid format",
            case.tags,
        );

        let (delimiter, ending) = separators_for(case.tags);
        let document = benign_document(&delimiter, &ending);
        let expected = expected_rows(case.tags);

        let slice = parse_slice(&document, case.format, case.parse.clone())
            .expect("slice parser accepted a valid config");
        let streaming = parse_streaming(&document, case.format, case.parse.clone())
            .expect("io parser accepted a valid config");
        let push = parse_push(
            &document,
            case.format,
            case.parse.clone(),
            document.len().max(1),
        )
        .expect("push parser accepted a valid config");

        assert_eq!(slice.len(), expected, "slice row count for {:?}", case.tags);
        assert_eq!(slice, streaming, "slice/io disagreed for {:?}", case.tags);
        assert_eq!(slice, push, "slice/push disagreed for {:?}", case.tags);

        reached.insert("parser.slice");
        reached.insert("parser.io");
        reached.insert("parser.push");
        for tag in case.tags {
            reached.insert(tag);
        }
    }

    let mut required: BTreeSet<&'static str> = BTreeSet::new();
    for case in variant_cases() {
        for tag in case.tags {
            required.insert(tag);
        }
    }
    required.insert("parser.slice");
    required.insert("parser.io");
    required.insert("parser.push");

    let missing: Vec<_> = required.difference(&reached).collect();
    assert!(
        missing.is_empty(),
        "variants never reached a parser: {missing:?}"
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one block per emitter body and policy; splitting hides the coverage"
)]
fn valid_domain_reaches_every_emitter_body_and_policy() {
    let mut reached: BTreeSet<&'static str> = BTreeSet::new();
    let base = FormatOptions::CSV;

    // Every emitter body, over the default flexible policy.
    let mut vec_emitter = VecEmitter::with_options(Vec::new(), base, EmitOptions::new())
        .expect("vec emitter constructs");
    vec_emitter.emit_record(BENIGN[0]).expect("vec emit");
    assert!(!vec_emitter.into_inner().is_empty());
    reached.insert("emitter.vec");

    let mut io_emitter = IoEmitter::with_options(Vec::new(), base, EmitOptions::new())
        .expect("io emitter constructs");
    io_emitter.emit_record(BENIGN[0]).expect("io emit");
    assert!(!io_emitter.into_inner().expect("flush").is_empty());
    reached.insert("emitter.io");

    let mut push_emitter =
        PushEmitter::with_options(base, EmitOptions::new()).expect("push emitter constructs");
    push_emitter.emit_record(BENIGN[0]).expect("push emit");
    assert!(!push_emitter.into_inner().is_empty());
    reached.insert("emitter.push");

    // WriteBom::Emit prepends a BOM; WriteBom::Omit does not.
    let bom = b"\xEF\xBB\xBF";
    let omit = {
        let mut emitter = VecEmitter::with_options(
            Vec::new(),
            base.write_bom(WriteBom::Omit),
            EmitOptions::new(),
        )
        .expect("omit emitter");
        emitter.emit_record(BENIGN[0]).expect("omit emit");
        emitter.into_inner()
    };
    assert!(
        !omit.starts_with(bom),
        "WriteBom::Omit must not write a BOM"
    );
    reached.insert("writebom.omit");

    let emit = {
        let mut emitter = VecEmitter::with_options(
            Vec::new(),
            base.write_bom(WriteBom::Emit),
            EmitOptions::new(),
        )
        .expect("emit emitter");
        emitter.emit_record(BENIGN[0]).expect("emit emit");
        emitter.into_inner()
    };
    assert!(emit.starts_with(bom), "WriteBom::Emit must write a BOM");
    reached.insert("writebom.emit");

    // Emitter field-count policies: Flexible accepts any width; MatchFirst and
    // Exact bind and reject a wrong width.
    let mut flexible = VecEmitter::with_options(
        Vec::new(),
        base,
        EmitOptions::new().field_count(FieldCount::Flexible),
    )
    .expect("flexible emitter");
    flexible.emit_record(BENIGN[0]).expect("flexible two");
    flexible
        .emit_record([b"solo".as_slice()])
        .expect("flexible one");
    reached.insert("emit.fieldcount.flexible");

    let mut match_first = VecEmitter::with_options(
        Vec::new(),
        base,
        EmitOptions::new().field_count(FieldCount::MatchFirst),
    )
    .expect("matchfirst emitter");
    match_first.emit_record(BENIGN[0]).expect("matchfirst two");
    assert!(
        match_first.emit_record([b"solo".as_slice()]).is_err(),
        "MatchFirst must reject a differing width",
    );
    reached.insert("emit.fieldcount.matchfirst");

    let mut exact = VecEmitter::with_options(
        Vec::new(),
        base,
        EmitOptions::new().field_count(FieldCount::Exact(2)),
    )
    .expect("exact emitter");
    exact.emit_record(BENIGN[0]).expect("exact two");
    assert!(
        exact.emit_record([b"solo".as_slice()]).is_err(),
        "Exact(2) must reject a one-field record",
    );
    reached.insert("emit.fieldcount.exact");

    // Non-default emitter buffer capacity, exercised through the buffered sink.
    let mut buffered =
        IoEmitter::with_options(Vec::new(), base, EmitOptions::new().buffer_capacity(1))
            .expect("buffered emitter");
    buffered.emit_record(BENIGN[0]).expect("buffered emit");
    buffered.emit_record(BENIGN[1]).expect("buffered emit");
    assert!(!buffered.into_inner().expect("flush").is_empty());
    reached.insert("bufcap.emit.nondefault");

    // Null encoding through the nullable record path.
    let mut nullable = VecEmitter::with_options(
        Vec::new(),
        base.nulls(Nulls::PostgresCsv).quoting(Quoting::Necessary),
        EmitOptions::new(),
    )
    .expect("nullable emitter");
    nullable
        .emit_nullable_record([Some(b"present".as_slice()), None])
        .expect("nullable emit");
    reached.insert("emit.nulls");

    let required = [
        "emitter.vec",
        "emitter.io",
        "emitter.push",
        "writebom.omit",
        "writebom.emit",
        "emit.fieldcount.flexible",
        "emit.fieldcount.matchfirst",
        "emit.fieldcount.exact",
        "bufcap.emit.nondefault",
        "emit.nulls",
    ];
    for tag in required {
        assert!(reached.contains(tag), "emitter coverage missing {tag}");
    }
}

// ── Raw-byte, corpus-seeded coverage-guided targets ─────────────────────────

/// The representative formats a raw-byte document is read under, chosen to
/// span the distinct kernels: default CSV, tab, backslash-escaped, comment-and-
/// blank handling, and strict CRLF.
fn representative_formats() -> Vec<FormatOptions> {
    vec![
        FormatOptions::CSV,
        FormatOptions::TSV,
        FormatOptions::BACKSLASH_CSV,
        FormatOptions::COMMENTED_CSV,
        FormatOptions::RFC4180,
    ]
}

#[test]
fn raw_parsing_across_front_ends() {
    bounded!().for_each(|bytes: &[u8]| {
        for format in representative_formats() {
            let options = || ParseOptions::new().headers(Headers::None);

            let slice = Outcome::from_result(parse_slice(bytes, format, options()));
            let streaming = Outcome::from_result(parse_streaming(bytes, format, options()));
            let push = Outcome::from_result(parse_push(bytes, format, options(), 1));
            let push_whole =
                Outcome::from_result(parse_push(bytes, format, options(), bytes.len().max(1)));

            assert_eq!(
                slice, streaming,
                "slice vs io on {bytes:?} under {format:?}"
            );
            assert_eq!(
                slice, push,
                "slice vs push(1) on {bytes:?} under {format:?}"
            );
            assert_eq!(
                slice, push_whole,
                "push chunk seam on {bytes:?} under {format:?}"
            );
        }
    });
}

#[test]
fn parse_emit_round_trip() {
    bounded!().for_each(|bytes: &[u8]| {
        let format = FormatOptions::CSV;
        let Ok(rows) = parse_slice(bytes, format, ParseOptions::new().headers(Headers::None))
        else {
            return;
        };
        // Re-emit the parsed records verbatim and read them back; a record set
        // that parsed cleanly must survive the writer/reader pair unchanged.
        let mut emitter =
            VecEmitter::with_options(Vec::new(), format, EmitOptions::new().has_headers(false))
                .expect("csv emitter constructs");
        for row in &rows {
            emitter
                .emit_record(row.iter().map(Vec::as_slice))
                .expect("re-emit a parsed record");
        }
        let reemitted = emitter.into_inner();
        let reparsed = parse_slice(
            &reemitted,
            format,
            ParseOptions::new().headers(Headers::None),
        )
        .expect("re-parse emitter output");
        assert_eq!(rows, reparsed, "round trip changed {bytes:?}");
    });
}

#[test]
fn filter_matches_manual_scan() {
    bounded!().for_each(|bytes: &[u8]| {
        let format = FormatOptions::CSV;
        let predicate = Predicate::equals(0usize, b"US".to_vec());

        // The filtering reader must select exactly the records a manual scan
        // with the same predicate keeps.
        let Ok(all) = parse_slice(bytes, format, ParseOptions::new().headers(Headers::None)) else {
            return;
        };
        let expected: Rows = all
            .into_iter()
            .filter(|row| predicate.matches_field(row.first().map(Vec::as_slice)))
            .collect();

        let mut parser =
            SliceParser::with_options(bytes, format, ParseOptions::new().headers(Headers::None))
                .expect("filter parser constructs");
        let mut filtered = Rows::new();
        for record in parser.matching_byte_records(&predicate) {
            let record = record.expect("filtered record");
            filtered.push(
                (0..record.len())
                    .map(|i| record.get(i).unwrap_or_default().to_vec())
                    .collect(),
            );
        }
        assert_eq!(
            expected, filtered,
            "filter disagreed with manual scan on {bytes:?}"
        );
    });
}

// ── Typed conversion, native and Serde ──────────────────────────────────────

/// Native and Serde typed round-trips over the same arbitrary bytes.
///
/// Feature-gated because it needs both the derive macros (native path) and the
/// Serde integration. Both paths decode positionally (`Headers::None`) so an
/// arbitrary document has a real chance of yielding typed rows rather than
/// failing header validation on almost every input.
#[cfg(all(feature = "serde", feature = "derive"))]
mod typed {
    use super::BOUNDED_ITERATIONS;
    use super::BOUNDED_TEST_TIME;

    use coseva::config::{EmitOptions, FormatOptions, Headers, ParseOptions};
    use coseva::encoding::{CsvDecode, CsvEncode};
    use coseva::{decode_from_slice, deserialize_from_slice, encode_to_vec, serialize_to_vec};

    /// The native typed shape: three fields the derive maps by position.
    #[derive(Debug, Clone, PartialEq, Eq, CsvDecode, CsvEncode)]
    struct NativeRow {
        name: String,
        count: i64,
        flag: bool,
    }

    /// The Serde typed shape: a tuple, which maps positionally without headers.
    type SerdeRow = (String, i64, bool);

    fn parse_options() -> ParseOptions {
        ParseOptions::new().headers(Headers::None)
    }

    fn emit_options() -> EmitOptions {
        EmitOptions::new().has_headers(false)
    }

    /// Decode every record, returning `None` if any record fails so that a
    /// round-trip is asserted only on a document that fully typed.
    fn decode_all_native(bytes: &[u8]) -> Option<Vec<NativeRow>> {
        let iter =
            decode_from_slice::<NativeRow, _>(bytes, FormatOptions::CSV, parse_options()).ok()?;
        iter.collect::<Result<Vec<_>, _>>().ok()
    }

    fn decode_all_serde(bytes: &[u8]) -> Option<Vec<SerdeRow>> {
        let iter =
            deserialize_from_slice::<SerdeRow, _>(bytes, FormatOptions::CSV, parse_options())
                .ok()?;
        iter.collect::<Result<Vec<_>, _>>().ok()
    }

    #[test]
    fn native_typed_round_trip() {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
            .for_each(|bytes: &[u8]| {
                let Some(rows) = decode_all_native(bytes) else {
                    return;
                };
                let encoded = encode_to_vec(rows.clone(), FormatOptions::CSV, emit_options())
                    .expect("re-encode typed rows");
                let decoded = decode_all_native(&encoded).expect("re-decode native typed output");
                assert_eq!(rows, decoded, "native typed round trip changed {bytes:?}");
            });
    }

    #[test]
    fn serde_typed_round_trip() {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
            .for_each(|bytes: &[u8]| {
                let Some(rows) = decode_all_serde(bytes) else {
                    return;
                };
                let encoded = serialize_to_vec(rows.clone(), FormatOptions::CSV, emit_options())
                    .expect("re-serialize typed rows");
                let decoded =
                    decode_all_serde(&encoded).expect("re-deserialize Serde typed output");
                assert_eq!(rows, decoded, "Serde typed round trip changed {bytes:?}");
            });
    }

    #[test]
    fn native_and_serde_agree_on_typed_rows() {
        bolero::check!()
            .with_iterations(BOUNDED_ITERATIONS)
            .with_test_time(BOUNDED_TEST_TIME)
            .for_each(|bytes: &[u8]| {
                // Where both frontends fully type the same document, they must
                // recover the same field values; the shapes differ only in how
                // the three columns are named.
                let (Some(native), Some(serde)) =
                    (decode_all_native(bytes), decode_all_serde(bytes))
                else {
                    return;
                };
                let projected: Vec<SerdeRow> = native
                    .into_iter()
                    .map(|row| (row.name, row.count, row.flag))
                    .collect();
                assert_eq!(projected, serde, "native and Serde disagreed on {bytes:?}");
            });
    }
}

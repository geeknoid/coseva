//! The window-scanning engine shared by every parser.

mod access;
mod borrowed_parser;
mod construct;
mod cursor;
mod format_access;
mod framing;
mod header_lookup;
mod headers;
mod limits;
mod owned_parser;
mod record_parser;
mod scanner;

use record_parser::try_parse_default_borrowed_record;
#[cfg(feature = "benchmarking")]
pub(crate) use record_parser::{count_structurals_scalar, count_structurals_selected};
#[cfg(target_arch = "x86_64")]
use record_parser::{
    try_parse_default_borrowed_plain, try_parse_default_plain_packed,
    try_parse_default_plain_packed_ascii,
};
use record_parser::{
    try_parse_default_interior_prefix, try_parse_default_interior_record_structural_ascii,
    try_parse_default_quoted_prefix, try_parse_default_quoted_record_structural_appending,
    try_parse_default_record, try_parse_named_dialect_record,
};
use record_parser::{
    try_parse_default_interior_prefix_windowed,
    try_parse_default_quoted_record_structural_windowed, try_parse_default_record_windowed,
};
pub(crate) use scanner::TypedMapping;
use scanner::{StructuralScanner, typed_mapping_from};

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::boxed::Box;
use alloc::sync::Arc;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::cmp;
use core::iter;
use core::mem;
use core::ops::Range;
use core::ptr;

pub(crate) use self::header_lookup::hash_name;
use self::header_lookup::{HeaderLookup, HeaderSlots};
use crate::byte_record::ByteRecord;
use crate::config::FormatTag;
use crate::config::{
    BlankRecords, Dialect, Escape, FieldCount, Headers, Limits, Nulls, ParserSettings, ReadBom,
    RecordEnding, Syntax, Tail, Whitespace,
};
use crate::encoding::{
    CsvDecode, CsvDecodeOwned, DecodeNew, DecodeSink, FusedFields, MappedRecord,
};
use crate::error::{Error, ErrorKind, Location};
use crate::filter::{Column, Predicate};
use crate::format::{Csv, CsvFormat, Dynamic, FormatKind, Tsv};
use crate::record::Record;
use crate::search::{
    BlockCache, count1, find_literal, find1, find1_near, find2, find2_near, find3, find3_near,
    find4_near, rfind1,
};
#[cfg(feature = "serde")]
use crate::serde::{StructCache, deserialize_full_record};
use crate::span::{Source, Span, SpanStorage};
use crate::text_record::TextRecord;
use coseva_unsafe::storage::{RecordStorage, Utf8RecordError};

/// The literal to skip-scan for, if pushdown is sound for this configuration.
///
/// Record-terminator counting is only sound when comments, skipped blanks,
/// backslash escapes, strict CRLF handling, and multi-byte separators cannot
/// hide or synthesize one.
pub(crate) fn skip_literal(
    dialect: Dialect,
    blank_records: BlankRecords,
    predicate: &Predicate,
) -> Option<&[u8]> {
    if dialect.comment.is_some()
        || blank_records == BlankRecords::Skip
        || dialect.escape != Escape::DoubleQuote
        || dialect.record_ending == RecordEnding::CrLf
        || dialect.multibyte()
    {
        return None;
    }

    let structural = [
        dialect.delimiter,
        dialect.quote,
        dialect.record_ending.byte(),
    ];
    predicate
        .is_skippable(&structural)
        .then(|| predicate.literal())
}

/// Records walked before a filter scan that skipped nothing is retried.
pub(crate) const FILTER_BACKOFF: u32 = 16;

/// The mark stripped from the start of a stream by [`ReadBom::Detect`].
#[cfg(feature = "std")]
pub(crate) const BOM: &[u8] = b"\xEF\xBB\xBF";

#[inline]
fn physical_line(input: &[u8], line_base: u64, line_origin: usize, byte: usize) -> u64 {
    let end = byte.min(input.len());
    let prefix = &input[line_origin.min(end)..end];
    line_base.saturating_add(count1(b'\n', prefix) as u64)
}

#[inline]
pub(crate) fn append_owned_segment(output: &mut RecordStorage, segment: &[u8]) {
    output.extend_bytes(segment);
}

/// Combine two candidate offsets, keeping the earlier one.
///
/// Used when [`RecordEnding::CrLf`] or an unquoted-field escape adds candidate
/// bytes beyond what [`find3_near`] can search at once.
const fn earliest(a: Option<usize>, b: Option<usize>) -> Option<usize> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if a < b { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Whether a dialect/NULL-style combination requires the general *parser*.
///
/// This is no longer the same question as whether it rules out the vectorized
/// *kernel*, which nothing now does. The kernel either agrees with these
/// dialects or declines the record outright, so each of them reaches the
/// general parser only for the records it actually applies to:
///
/// - A trim policy that exempts quoted fields needs the general field parser
///   because that is the one that knows which fields were quoted while it
///   trims. The kernel bails at a quote opening a field, so every field it
///   produces is unquoted and the exemption cannot apply; `Engine::trim_spans`
///   then trims each span by the quoted bit it carries.
/// - [`RecordEnding::CrLf`] agrees with the kernel on every boundary, since
///   both endings search for `\n`. It differs only in requiring the `\r` the
///   other tolerates, which `Engine::validate_crlf` judges after the fact.
/// - An escape style that applies outside quotes -- [`Escape::Mysql`] and
///   [`Escape::Unquoted`] -- does change what the scanner must find, but only
///   in a record containing the escape byte. The kernel tests for one before it
///   commits and declines the record if it finds one, so escaped input reaches
///   the general parser and unescaped input never has to.
///
/// - A separator longer than one byte cannot use the kernel at all, since the
///   whole scan is built on single-byte matching. This is the one case that
///   never returns to the fast path for any record.
///
/// A non-default [`Nulls`] policy needs neither: it changes only how a field is
/// finalized, which `Engine::mark_null_spans` handles as a pass over the
/// finished record.
const fn needs_general_parsing(dialect: Dialect, trim: Whitespace) -> bool {
    matches!(dialect.record_ending, RecordEnding::CrLf)
        || dialect.escape.escapes_unquoted()
        || dialect.multibyte()
        || trim.exempts_quoted()
}

/// Whether the fast paths may run at all for these settings.
///
/// Both the borrowed kernel and the owned parser gate on this, so it is the
/// single point at which the test suite's oracle knob takes every record away
/// from them and onto the general parser.
///
/// A separator of more than one byte is here rather than in
/// [`needs_general_parsing`] alone because the kernel runs before that gate is
/// consulted: it commits to a record and only declines for the reasons it can
/// detect in the bytes. A multi-byte separator is not one of those reasons,
/// since the kernel would have already split on the lead byte.
fn plain_kernel(options: &ParserSettings) -> bool {
    #[cfg(feature = "test-util")]
    if options.force_general_parser {
        return false;
    }
    !options.skip_initial_space && !options.dialect.multibyte()
}

/// Whether a dialect needs any pass over a record the plain kernel finished.
///
/// Both such passes -- `Engine::mark_null_spans` and `Engine::validate_crlf` --
/// are off for the default dialect, so they share one guard rather than each
/// testing its own setting. That matters only for a runtime-configured format,
/// where each test is a load and a compare the kernel pays per record; a static
/// format folds the whole thing away either way.
const fn needs_record_pass(dialect: Dialect, nulls: Nulls) -> bool {
    !matches!(nulls, Nulls::None) || matches!(dialect.record_ending, RecordEnding::CrLf)
}

/// Whether an unquoted field's raw bytes are this dialect's explicit NULL.
///
/// The fast paths reach this only for *unquoted* fields, which is the whole
/// rule: a quoted field is never NULL, so `"\N"` and `""` stay ordinary
/// values. Callers pass `Nulls::None` for a header record, matching
/// `push_general_unquoted_span`, so a header spelled `\N` stays a name.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "this has to fold to a constant false, or to one comparison, inside the parse kernels"
)]
const fn raw_field_is_null(nulls: Nulls, raw: &[u8]) -> bool {
    match nulls {
        Nulls::None => false,
        Nulls::PostgresCsv => raw.is_empty(),
        Nulls::Mysql => matches!(raw, b"\\N"),
    }
}

/// Pre-size a brand new record from the running high-water hint.
///
/// Outlined and marked cold so the reuse fast path stays a single not-taken
/// test in the record reader's hot path.
#[cold]
#[inline(never)]
fn presize_owned(output: &mut RecordStorage, hint: (usize, usize)) {
    output.reserve_storage(hint.0, hint.1);
}

/// Decode one `MySQL` text-export backslash escape target byte.
///
/// Matches `write_unquoted_escaped_field`: `0`, `b`, `n`, `r`, `t`, `Z`, and `\\` decode
/// specially; other bytes pass through unchanged.
const fn mysql_unescape_byte(byte: u8) -> u8 {
    match byte {
        b'0' => 0x00,
        b'b' => 0x08,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        b'Z' => 0x1A,
        other => other,
    }
}

/// Alternate header spellings per field, parallel to the field names.
///
/// Empty means no field has alternates. See
/// [`CsvDecode::field_aliases`](crate::encoding::CsvDecode::field_aliases).
pub(crate) type FieldAliases = &'static [&'static [&'static str]];

/// The `names × headers` product past which resolving a typed mapping by
/// building a header lookup beats scanning every header once per name.
///
/// A scan costs `names × headers` comparisons; an indexed resolution costs one
/// pass to hash the headers plus one lookup per name, so it wins once a wide
/// type names enough of a wide header. Below the threshold the scan is cheaper
/// than building and probing a map, and it never allocates — which is why a
/// two-of-a-hundred projection keeps scanning. The constant is where the two
/// costs cross in practice, not a hard rule: correctness is identical either
/// way.
const MAPPING_INDEX_THRESHOLD: usize = 1024;

/// Whether resolving `names` against `headers` should build a lookup rather
/// than scan the headers once per name.
///
/// Shared by typed decode and [`FieldProjection`](crate::FieldProjection) so
/// both cross over at the same width.
pub(crate) const fn wide_mapping(names: usize, headers: usize) -> bool {
    names.saturating_mul(headers) > MAPPING_INDEX_THRESHOLD
}

/// Resolve each named field to its source column by scanning the header once
/// per name.
///
/// Linear in `names × headers`, allocation-free, and the right choice for the
/// common case of a narrow type or a sparse projection. Wide types resolve
/// through [`resolve_decode_mapping_indexed`] instead, which shares this
/// function's duplicate, alias, missing-column, and ambiguity semantics
/// exactly.
pub(in crate::engine) fn resolve_decode_mapping(
    headers: &ByteRecord,
    names: &'static [&'static str],
    aliases: FieldAliases,
) -> Result<Vec<usize>, Error> {
    let mut mapping = Vec::with_capacity(names.len());
    for (target, name) in names.iter().enumerate() {
        let alternates = aliases.get(target).copied().unwrap_or(&[]);
        let mut matches = headers.iter().enumerate().filter_map(|(index, header)| {
            let matched = header == name.as_bytes()
                || alternates.iter().any(|alias| header == alias.as_bytes());
            matched.then_some(index)
        });
        let Some(source) = matches.next() else {
            return Err(Error::field(ErrorKind::Decode, target, Some(name)));
        };
        if matches.next().is_some() {
            return Err(Error::field(ErrorKind::Decode, target, Some(name)));
        }
        mapping.push(source);
    }
    Ok(mapping)
}

/// Resolve each named field to its source column through a prebuilt header
/// lookup, for wide types where scanning every header per name is quadratic.
///
/// The result is identical to [`resolve_decode_mapping`] on the same inputs:
///
/// * a name matched by no column is a missing-column error,
/// * a name (or the union of a name and its aliases) matched by two or more
///   distinct columns is an ambiguity error, and
/// * the single matching column is the lowest-numbered one, which is what the
///   scan's first-match rule also yields.
///
/// The lookup already reports every column carrying a name, so a duplicate
/// header surfaces as two distinct columns and is rejected exactly as the scan
/// rejects it. A column can only appear under one header value, so the only way
/// the same column is seen twice is a name that equals one of its own aliases;
/// comparing each column against the first one found folds that repeat away
/// without treating it as a second match.
pub(in crate::engine) fn resolve_decode_mapping_indexed(
    headers: &ByteRecord,
    lookup: &HeaderLookup,
    names: &'static [&'static str],
    aliases: FieldAliases,
) -> Result<Vec<usize>, Error> {
    let mut mapping = Vec::with_capacity(names.len());
    for (target, name) in names.iter().enumerate() {
        let alternates = aliases.get(target).copied().unwrap_or(&[]);
        let mut source: Option<usize> = None;
        let mut ambiguous = false;
        for query in
            iter::once(name.as_bytes()).chain(alternates.iter().map(|alias| alias.as_bytes()))
        {
            let Some(slots) = lookup.get(headers, query) else {
                continue;
            };
            for &column in slots.as_slice() {
                match source {
                    None => source = Some(column),
                    Some(first) if first == column => {}
                    Some(_) => ambiguous = true,
                }
            }
        }
        match source {
            Some(column) if !ambiguous => mapping.push(column),
            _ => return Err(Error::field(ErrorKind::Decode, target, Some(name))),
        }
    }
    Ok(mapping)
}

type SliceOwnedParser = fn(&[u8], &mut RecordStorage) -> Option<usize>;

/// Parses a record's leading quoted fields, reporting the offset it reached
/// and whether the record ended there.
type SliceQuotedPrefixParser = fn(&[u8], &mut RecordStorage) -> Option<(usize, bool)>;

/// Parses a record's unquoted head and the quoted field that follows it,
/// reporting the offset it reached and whether the record ended there.
///
/// The counterpart to [`SliceQuotedPrefixParser`] for a record whose first
/// byte is not a quote: it reads the plain prefix and the interior quoted
/// field the kernel bailed on, then hands the plain tail back.
type SliceInteriorPrefixParser = fn(&[u8], &mut RecordStorage) -> Option<(usize, bool)>;

/// Sentinel for an unset cursor offset.
///
/// No record can start here, so it also stands in for "not positioned" and
/// "not fully parsed", avoiding nested `Option`s on the hot `advance` path.
const NO_OFFSET: usize = usize::MAX;

/// Field count a fresh parser reserves span storage for.
///
/// Wide enough that a typical record never reallocates while it is being
/// parsed, small enough that reserving it for a document that turns out to be
/// narrow costs a few hundred bytes and nothing else.
const TYPICAL_FIELDS: usize = 16;

/// What positioning a cursor on a window found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Advance {
    /// A record is positioned and can be borrowed.
    Record,
    /// The window holds no whole record; it must grow or end.
    NeedMore,
    /// The input is exhausted.
    Done,
}

/// Cursor fields restored when a record needs a wider window.
///
/// Replacing these latches with "was unset" bits measured 0.5-0.7% worse on
/// every streaming benchmark; the plain copy stays in registers.
#[derive(Clone, Copy, Debug)]
struct CursorState {
    location: usize,
    folded_upto: usize,
    folded_lines: u64,
    record_index: u64,
    expected_fields: Option<usize>,
    headers_initialized: bool,
    cursor_start: usize,
    cursor_end: usize,
    cursor_index: u64,
    failed: bool,
}

/// The far smaller subset of [`CursorState`] a chunked rewind must restore.
///
/// A chunked rewind always happens with the cursor already anchored on the
/// record being retried, so the cursor fields need no snapshot at all:
/// clearing them says "no positioned record", which is exactly the truth
/// after a rewind. `folded_upto` follows from the restored `location`, and
/// `failed` is provably clear on every path that rewinds. The io parsers
/// rewind from places that can still hold a live cursor, so they keep the
/// full state.
#[derive(Clone, Copy)]
struct ChunkCursorState {
    location: usize,
    record_index: u64,
    folded_lines: u64,
    expected_fields: Option<usize>,
    headers_initialized: bool,
}

/// A resumable checkpoint for the boundary pre-scan that skips re-parsing an
/// incomplete record's prefix each time a chunk or short read widens the
/// window.
///
/// A record delivered in tiny pieces is otherwise re-parsed from its start on
/// every growth, which is quadratic in the record length. Instead, the engine
/// scans just far enough to prove the window still holds no whole record and
/// remembers where it stopped, so the next, wider window resumes from there.
/// The scan is a strict shortcut: it only ever decides "provably no record
/// yet", and every record that can complete still goes through the unchanged
/// full parse.
///
/// The checkpoint is keyed on the record it describes: [`Self::record_start`]
/// is the window offset the record begins at, and a mismatch means the
/// checkpoint is stale and a fresh scan must start. [`NO_OFFSET`] marks it
/// unset. It is cold state, untouched on the hot path where a record completes
/// inside the window it first appears in.
#[derive(Clone, Copy, Debug)]
struct ResumeState {
    /// Window offset the checkpointed record starts at, or [`NO_OFFSET`].
    record_start: usize,
    /// Offset the boundary scan has already examined up to.
    scanned_to: usize,
    /// Offset the field currently being scanned starts at.
    field_start: usize,
    /// Whether the scan stopped inside a quoted field.
    in_quotes: bool,
    /// Whether this checkpoint scans an ignored comment rather than data.
    ignored: bool,
}

impl ResumeState {
    /// An unset checkpoint that keys to no record.
    const fn new() -> Self {
        Self {
            record_start: NO_OFFSET,
            scanned_to: NO_OFFSET,
            field_start: NO_OFFSET,
            in_quotes: false,
            ignored: false,
        }
    }

    /// A checkpoint positioned at the start of `record_start`, before any of
    /// the record has been scanned.
    const fn fresh(record_start: usize) -> Self {
        Self {
            record_start,
            scanned_to: record_start,
            field_start: record_start,
            in_quotes: false,
            ignored: false,
        }
    }

    /// A checkpoint inside an ignored comment line.
    const fn ignored(record_start: usize, scanned_to: usize) -> Self {
        Self {
            record_start,
            scanned_to,
            field_start: record_start,
            in_quotes: false,
            ignored: true,
        }
    }
}

/// Whether an error only reflects the window ending.
///
/// Either an unterminated quoted field, which a wider window can terminate, or
/// any error located close enough to the window edge that a wider window could
/// still change the verdict — which covers the one-byte-short `\r\n` case and
/// also a definite syntax error that happens to land there. Deferring a
/// definite error is still correct: it is not discarded, and re-parsing the
/// wider window raises the identical error at the identical location, because
/// the bytes before the edge are unchanged.
///
/// `lookahead` is how many bytes from the offending byte onwards the verdict
/// can depend on. One byte for a single-byte dialect; a multi-byte delimiter or
/// record ending needs its whole separator, because a byte that opens one is
/// only an error once the bytes that would complete it have arrived and turned
/// out not to.
fn truncated_by_window(error: &Error, window_len: usize, lookahead: usize) -> bool {
    error.kind() == ErrorKind::UnterminatedQuotedField
        || matches!(
            error.kind(),
            ErrorKind::InvalidUtf8(error) if error.error_len().is_none()
        )
        || error.location().byte.saturating_add(lookahead) >= window_len
}

/// Reports that a view was called without a current record.
///
/// Public callers cannot reach this, so it is excluded from coverage.
#[cfg_attr(coverage_nightly, coverage(off))]
#[cold]
#[expect(
    clippy::panic,
    reason = "using a view off a record is a caller bug with no sensible value to return"
)]
fn not_positioned() -> ! {
    panic!(
        "no current record: `advance` must return `true` before a record can be read, \
         and must be called again before the next one"
    );
}

/// The window-scanning engine shared by every parser.
///
/// Parsers keep the input window outside the engine so owned bytes and parser
/// state can live in separate fields.
///
/// The struct is 592 bytes with `serde` and 480 without, with roughly 256 bytes
/// of cold setup and cache state. Field reordering changes estimated cycles by
/// less than 0.02%, and 256 bytes of dead padding costs only 0.03%, both within
/// control-benchmark drift. One engine remains resident per parser, so moving
/// cold state behind an indirection adds header and Serde costs without helping
/// the record hot path.
#[expect(
    clippy::struct_excessive_bools,
    reason = "cached parser-policy flags avoid repeated work in record hot paths"
)]
#[derive(Debug)]
pub(crate) struct Engine {
    location: usize,
    line_base: u64,
    line_origin: usize,
    folded_upto: usize,
    folded_lines: u64,
    record_index: u64,
    dialect: Dialect,
    format_kind: FormatKind,
    limits: Limits,
    field_count: FieldCount,
    expected_fields: Option<usize>,
    /// Whether the first input record is still to be consumed as headers.
    ///
    /// This is the whole of the header policy the engine needs. `Headers` is
    /// the caller's vocabulary and carries a `ByteRecord` for the provided
    /// case, but construction resolves that variant away — the record moves to
    /// `header_record` and the policy becomes `None` — so the engine only ever
    /// distinguishes "consume the first record" from "do not". Storing the
    /// enum here cost 80 bytes to hold one bit.
    consume_first_record: bool,
    header_record: Option<ByteRecord>,
    header_lookup: HeaderLookup,
    /// Whether `header_lookup` reflects the current `header_record`.
    ///
    /// The map is built on demand rather than when the headers change, because
    /// nothing on the typed-decode or Serde paths reads it — both resolve their
    /// columns against the header record directly. Parsing a file into a struct
    /// therefore never builds it at all.
    header_lookup_ready: bool,
    typed_mapping: Option<(&'static [&'static str], FieldAliases, TypedMapping)>,
    /// The column a `Column::Name` predicate last resolved to, beside the name
    /// it was resolved for.
    ///
    /// Keyed by the name's bytes rather than its address, because a predicate
    /// is borrowed for the call and a later one can reuse a dropped one's
    /// allocation. Only a hit is remembered: a name that resolves to nothing
    /// ends the run anyway, and on the push path a miss can also mean the
    /// headers have not arrived yet, which must stay retryable.
    filter_column: Option<(Vec<u8>, usize)>,
    /// Cached header names and learned ignored-column set for the Serde path.
    #[cfg(feature = "serde")]
    serde_cache: StructCache,
    headers_initialized: bool,
    /// Whether the headers are established *and* `serde_cache` reflects them.
    ///
    /// Syncing the cache validates every header name as UTF-8 and copies it to
    /// the heap, which at 200 columns was 55% of the cost of reading the first
    /// record — paid by every parser that sets a header, including the great
    /// majority that never deserialize anything. Deferring it to the first
    /// Serde call is only free if the check itself is free, so this flag
    /// subsumes `headers_initialized` on that path rather than adding a second
    /// test to it.
    #[cfg(feature = "serde")]
    serde_ready: bool,
    trim: Whitespace,
    blank_records: BlankRecords,
    syntax: Syntax,
    skip_initial_space: bool,
    nulls: Nulls,
    general_parsing: bool,
    plain_kernel: bool,
    record_pass: bool,
    /// Whether some records are passed over rather than reported, which is the
    /// only reason to walk ahead of a record before parsing it.
    skips_records: bool,
    spans: SpanStorage,
    owned_scratch: ByteRecord,
    owned_parser: Option<SliceOwnedParser>,

    /// Parses a leading-quoted record's quoted head, so the vectorized kernel
    /// can take the unquoted tail. Only the default dialect has one.
    quoted_prefix_parser: Option<SliceQuotedPrefixParser>,

    /// Active parser for a predicted interior-quoted record. It starts as the
    /// prefix handoff and switches to the whole-record multi-quote parser when
    /// a row proves it has several separated quote runs.
    interior_prefix_parser: Option<SliceInteriorPrefixParser>,
    /// Prefix handoff restored whenever the structural route re-tests a run.
    interior_handoff_parser: Option<SliceInteriorPrefixParser>,
    /// Whole-record parser selected for repeated multi-quote rows.
    multi_quote_parser: Option<SliceInteriorPrefixParser>,
    /// How many more records may skip the structural attempt on the owned path.
    ///
    /// A record whose *first* byte is not a quote but which contains one later
    /// stands up a structural scanner, consumes the short prefix ahead of the
    /// quote, and then bails to the scalar parser -- paying for a scan sized to
    /// amortize over a whole record and getting a few bytes of it. Since files
    /// are overwhelmingly uniform in shape, the previous record predicts this.
    ///
    /// This counts down so the prediction re-tests itself: at zero the next
    /// record takes the structural route regardless, which reports the truth
    /// for free -- it bails if a quote is there and succeeds if it is not.
    interior_quotes: u8,
    ascii_structural_backoff: u8,
    ascii_structural_succeeded: bool,
    block_cache: BlockCache,
    /// Records left to walk before the filter scan is probed again.
    filter_backoff: u32,
    /// Largest owned record produced so far, as (fields, bytes).
    ///
    /// Fresh `ByteRecord`s size themselves from this high-water hint to avoid
    /// repeated buffer growth.
    owned_hint: (usize, usize),
    /// The record `advance` positioned on, and how far it has been parsed.
    cursor_start: usize,
    cursor_index: u64,
    cursor_end: usize,
    /// Resumable checkpoint for the incomplete-record boundary pre-scan.
    ///
    /// Cold state: it is only consulted and written on the streaming paths
    /// when a record does not fit the current window, never on the hot path
    /// where a record completes inside the window it first appears in.
    resume: ResumeState,
    pub(crate) failed: bool,
    terminated: bool,
    /// Record parsed straight into owned form by a window advance.
    ///
    /// Staging lets owned readers swap the record out instead of copying spans.
    /// It stays boxed and lazy so slice parsers allocate nothing and `Engine`
    /// stays one pointer wider rather than a whole [`ByteRecord`] wider. That
    /// is a size bound, not a layout one: `Engine` is `repr(Rust)`, so field
    /// order and any resulting cache-line grouping are the compiler's to
    /// choose and nothing here constrains them.
    staged_record: Option<Box<ByteRecord>>,
    /// Whether [`Self::staged_record`] holds the record the cursor is on.
    staged_valid: bool,
    /// Whether the last streamed reader wanted owned form.
    ///
    /// Window parsers speculate from the previous record; a wrong guess only
    /// re-parses one record.
    staged_form_owned: bool,
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod engine_tests {
    use super::{
        Engine, ErrorKind, HeaderLookup, RecordStorage, append_owned_segment, physical_line,
        plain_kernel, presize_owned, resolve_decode_mapping_indexed, skip_literal,
    };
    use crate::ByteRecord;
    #[cfg(feature = "std")]
    use crate::config::Headers;
    #[cfg(feature = "multibyte")]
    use crate::config::Tail;
    use crate::config::{BlankRecords, Dialect, Escape, Limits, ParserSettings, RecordEnding};
    use crate::filter::Predicate;
    #[cfg(feature = "std")]
    use crate::format::Dynamic;

    /// `line_origin` is window-relative, so dropping a prefix must shift it
    /// with the other window-relative offsets.
    #[test]
    fn dropping_a_window_prefix_keeps_a_seeked_line_count_exact() {
        let input = b"a\nb\nc\nd\ne\n";
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);

        // Seek onto the fourth line, then drop the first six bytes.
        let mut core = Engine::from_config(input, settings.clone());
        core.seek_to(6, 4, 3);
        assert_eq!(core.location(input).line, 4);

        let mut shifted = Engine::from_config(input, settings);
        shifted.seek_to(6, 4, 3);
        shifted.shift_window(input, 6);
        assert_eq!(
            shifted.location(&input[6..]).line,
            4,
            "dropping the prefix ahead of the seek point changed the line",
        );
    }

    /// `PushParser`/`IoParser` keep their `failed` flag in lock-step
    /// with the engine, so this defense-in-depth guard is exercised directly.
    #[cfg(feature = "std")]
    #[test]
    fn try_advance_window_reports_a_poisoned_engine() {
        let input = b"a,b\n";
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut core = Engine::from_config_windowed(input, settings);
        core.failed = true;

        let error = core
            .try_advance_window::<Dynamic>(input)
            .expect_err("a poisoned engine must not advance");
        assert_eq!(error.kind(), ErrorKind::ParserFailed);
    }

    /// `advance_window_eagerly` shares the same poisoned-engine guard, but
    /// public wrappers keep their `failed` flag in lock-step with the engine.
    #[cfg(feature = "std")]
    #[test]
    fn advance_window_eagerly_reports_a_poisoned_engine_at_eof() {
        let input = b"a,b\n";
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut core = Engine::from_config_windowed(input, settings);
        core.failed = true;

        let error = core
            .advance_window_eagerly::<Dynamic>(input, true)
            .expect_err("a poisoned engine must not advance");
        assert_eq!(error.kind(), ErrorKind::ParserFailed);
    }

    /// `advance_window_eagerly` resolves headers itself at EOF, a path public
    /// streaming wrappers do not reach before their own lazy header setup.
    #[cfg(feature = "std")]
    #[test]
    fn advance_window_eagerly_resolves_headers_at_eof() {
        let input = b"left,right\n1,2\n";
        let settings = ParserSettings::headed(Dialect::default(), Limits::DEFAULT);
        assert_eq!(settings.headers, Headers::FirstRecord);
        let mut core = Engine::from_config_windowed(input, settings);

        let advance = core
            .advance_window_eagerly::<Dynamic>(input, true)
            .expect("headers resolve and the first data record is positioned");
        assert!(
            matches!(advance, super::Advance::Record),
            "the first data record should be reported once headers resolve"
        );
    }

    /// Production callers never pass an empty dropped span, so this guard is
    /// exercised directly.
    #[test]
    fn shift_window_is_a_no_op_for_an_empty_dropped_span() {
        let input = b"a,b\nc,d\n";
        let settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        let mut core = Engine::from_config(input, settings);
        let before = core.location(input);

        core.shift_window(input, 0);

        assert_eq!(
            core.location(input),
            before,
            "shifting by nothing must not move the cursor"
        );
    }

    #[test]
    fn filter_skip_literal_requires_every_soundness_precondition() {
        let predicate = Predicate::equals(0, "needle");
        let csv = Dialect::default();
        assert_eq!(
            skip_literal(csv, BlankRecords::Preserve, &predicate),
            Some(&b"needle"[..])
        );

        let mut commented = csv;
        commented.comment = Some(b'#');
        assert!(skip_literal(commented, BlankRecords::Preserve, &predicate).is_none());
        assert!(skip_literal(csv, BlankRecords::Skip, &predicate).is_none());

        let mut escaped = csv;
        escaped.escape = Escape::Backslash(b'\\');
        assert!(skip_literal(escaped, BlankRecords::Preserve, &predicate).is_none());

        let mut crlf = csv;
        crlf.record_ending = RecordEnding::CrLf;
        assert!(skip_literal(crlf, BlankRecords::Preserve, &predicate).is_none());

        #[cfg(feature = "multibyte")]
        {
            let mut multibyte = csv;
            multibyte.delimiter_tail = Tail::of(b",:");
            assert!(skip_literal(multibyte, BlankRecords::Preserve, &predicate).is_none());
        }
    }

    #[test]
    fn physical_lines_exclude_the_byte_at_the_reported_offset() {
        let input = b"a\nb\n";
        assert_eq!(physical_line(input, 1, 0, 1), 1);
        assert_eq!(physical_line(input, 1, 0, 2), 2);
        assert_eq!(physical_line(input, 7, 2, 1), 7);
    }

    #[test]
    fn owned_segments_preserve_empty_single_and_multi_byte_appends() {
        let mut storage = RecordStorage::new();
        storage.extend_bytes(b"seed");

        append_owned_segment(&mut storage, b"");
        assert_eq!(storage.bytes(), b"seed");
        append_owned_segment(&mut storage, b"x");
        assert_eq!(storage.bytes(), b"seedx");
        append_owned_segment(&mut storage, b"yz");
        assert_eq!(storage.bytes(), b"seedxyz");
    }

    #[test]
    fn owned_record_presizing_reserves_both_storage_dimensions() {
        let mut storage = RecordStorage::new();
        presize_owned(&mut storage, (7, 19));
        assert!(storage.field_capacity() >= 7);
        assert!(storage.byte_capacity() >= 19);
    }

    #[cfg(feature = "test-util")]
    #[test]
    fn forcing_the_general_parser_disables_the_plain_kernel() {
        let mut settings = ParserSettings::unheaded(Dialect::default(), Limits::DEFAULT);
        assert!(plain_kernel(&settings));
        settings.force_general_parser = true;
        assert!(!plain_kernel(&settings));
    }

    #[test]
    fn indexed_mapping_errors_retain_the_target_field_name() {
        let headers: ByteRecord = [b"present".as_slice()].into_iter().collect();
        let mut lookup = HeaderLookup::default();
        lookup.rebuild(&headers);

        let error = resolve_decode_mapping_indexed(&headers, &lookup, &["missing"], &[])
            .expect_err("a missing indexed header must error");
        assert_eq!(error.kind(), ErrorKind::Decode);
        assert_eq!(error.location().field, 0);
        assert_eq!(error.field_name(), Some("missing"));
    }
}

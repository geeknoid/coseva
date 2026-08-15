//! Quoting, escaping and record framing shared by the three emitters.
//!
//! The emitters themselves live beside the parsers at the crate root; this
//! module holds only the byte-level machinery they have in common.

#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::{marker::PhantomData, str};

use crate::config::{Dialect, Escape, FieldCount, Nulls, Quoting, RecordEnding};
use crate::encoding::EncodeVisitor;
use crate::error::{Error, ErrorKind, Location};
use crate::format::CsvFormat;
use crate::search::{find1, find2, find3, find4};

pub(crate) const BYTE_ONES: u64 = u64::MAX / u8::MAX as u64;
pub(crate) const BYTE_HIGHS: u64 = BYTE_ONES << 7;

/// Field width at which the quoting scan switches from words to SIMD blocks.
///
/// One SIMD block is 32 bytes, and `find` falls back to a byte-at-a-time scan
/// below that width, so handing it a shorter field would lose to the
/// word-at-a-time loop that covers eight bytes per iteration.
///
/// This is not a tuning constant with a crossover to be searched for. The sweep
/// in `benches/needs_quotes.rs` prices both arms at every width and finds a
/// discontinuity rather than a crossing: the block scan is 1.3-2.1 times dearer
/// than the word loop at every width below 32, and 2.3-2.8 times cheaper the
/// moment the width reaches 32. The value is therefore fixed by the block
/// width, and `scripts/perf_gate.py` pins both sides of it.
pub(crate) const SIMD_QUOTING_SCAN_BYTES: usize = 32;

#[cfg(feature = "serde")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SerdeHeaderState {
    Pending,
    Written,
    Disabled,
}

pub(crate) const fn repeated_byte(byte: u8) -> u64 {
    BYTE_ONES * byte as u64
}

pub(crate) const fn zero_byte_mask(word: u64) -> u64 {
    word.wrapping_sub(BYTE_ONES) & !word & BYTE_HIGHS
}

/// A growable byte buffer that encoded fields are appended to.
///
/// Has a single implementor, `Vec<u8>`. Taking `&mut Vec<u8>` directly instead
/// of this trait measures 4.6% worse on the `MySQL` encoding benchmarks (and
/// 1% better on Postgres) due to per-instantiation codegen differences.
pub(crate) trait ByteSink {
    fn len(&self) -> usize;
    fn push(&mut self, byte: u8);
    fn extend(&mut self, bytes: &[u8]);
    fn truncate(&mut self, len: usize);
}

impl ByteSink for Vec<u8> {
    fn len(&self) -> usize {
        self.len()
    }

    fn push(&mut self, byte: u8) {
        self.push(byte);
    }

    fn extend(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }

    fn truncate(&mut self, len: usize) {
        self.truncate(len);
    }
}

#[cold]
#[inline(never)]
#[cfg_attr(coverage_nightly, coverage(off))]
pub(crate) fn record_too_large() -> Error {
    Error::detailed(ErrorKind::Encode, "encoded record size overflows usize")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NecessaryPath {
    Csv,
    General,
}

#[inline]
fn necessary_path(dialect: Dialect) -> NecessaryPath {
    if dialect == Dialect::CSV {
        NecessaryPath::Csv
    } else {
        NecessaryPath::General
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfiguredPath {
    Plain,
    Configured,
}

#[inline]
fn configured_path(dialect: Dialect, nulls: Nulls) -> ConfiguredPath {
    if nulls == Nulls::None && !dialect.escape.escapes_unquoted() {
        ConfiguredPath::Plain
    } else {
        ConfiguredPath::Configured
    }
}

fn write_non_null_empty<B: ByteSink>(output: &mut B, dialect: Dialect, quoting: Quoting) {
    match quoting {
        Quoting::NonNumeric | Quoting::Always => write_quoted(output, dialect, b""),
        Quoting::Necessary | Quoting::Never | Quoting::Raw => {}
    }
}

/// Write one already-assembled field the way [`emit_nullable_record_runtime`] does, quoting it
/// only as the policy requires and never treating it as NULL.
///
/// This is the per-field body of [`emit_nullable_record_runtime`]'s fast path, lifted out so the
/// push-driven [`DirectEncodeVisitor`] can reach exactly the same writers —
/// including the CSV-specialised [`write_csv_necessary`] — one field at a time.
/// The `quoting` match folds to the selected arm whenever the caller's format
/// is static, so a compile-time format pays nothing for the extra indirection.
#[inline]
pub(crate) fn write_record_field<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    field: &[u8],
    is_first: bool,
) -> Result<(), Error> {
    match quoting {
        Quoting::Necessary => match necessary_path(dialect) {
            NecessaryPath::Csv => write_csv_necessary(output, field, is_first),
            NecessaryPath::General => write_necessary(output, dialect, field, is_first),
        },
        Quoting::NonNumeric => write_non_numeric(output, dialect, field, is_first),
        Quoting::Always => write_quoted(output, dialect, field),
        Quoting::Never => write_unquoted(output, dialect, field, is_first)?,
        Quoting::Raw => output.extend(field),
    }
    Ok(())
}

/// A push-driven emitter that frames configured fields straight into
/// rollback-capable output, one field at a time.
///
/// This is the shared core behind native
/// [`CsvEncode`](crate::encoding::CsvEncode) and Serde record emission: rather
/// than staging every field in a [`ByteRecord`](crate::ByteRecord) and copying
/// the record out, both drive this visitor field by field. Quoting, escaping
/// and NULL handling are applied as each field arrives, so nothing is copied
/// twice. If a later field, formatter, field-count check or Serde operation
/// fails, the caller truncates the output back to the record start, leaving no
/// partial record behind.
///
/// The visitor writes the record's fields; [`finish`] then appends the
/// terminator and reports the field count.
///
/// [`finish`]: DirectEncodeVisitor::finish
pub(crate) struct DirectEncodeVisitor<'output, F: CsvFormat, B: ByteSink> {
    output: &'output mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    record_start: usize,
    field_count: usize,
    only_field_was_non_null_empty: bool,
    marker: PhantomData<fn() -> F>,
}

impl<'output, F: CsvFormat, B: ByteSink> DirectEncodeVisitor<'output, F, B> {
    pub(crate) fn new(
        output: &'output mut B,
        dialect: Dialect,
        quoting: Quoting,
        nulls: Nulls,
    ) -> Self {
        let record_start = output.len();
        Self {
            output,
            dialect: fmt_dialect::<F>(dialect),
            quoting: fmt_quoting::<F>(quoting),
            nulls: fmt_nulls::<F>(nulls),
            record_start,
            field_count: 0,
            only_field_was_non_null_empty: false,
            marker: PhantomData,
        }
    }

    /// Frame one non-NULL field, choosing the same writer [`emit_nullable_record_runtime`] and
    /// [`emit_nullable_record`] would for the configured NULL policy.
    #[inline]
    pub(crate) fn write_field(&mut self, field: &[u8]) -> Result<(), Error> {
        if let Some(options) = F::OPTIONS
            && options.dialect == Dialect::CSV
            && options.quoting == Quoting::Necessary
            && options.nulls == Nulls::None
        {
            if self.field_count != 0 {
                self.output.push(b',');
            }
            self.field_count += 1;
            let is_first = self.field_count == 1;
            if is_first {
                self.only_field_was_non_null_empty = field.is_empty();
            }
            write_csv_necessary(self.output, field, is_first);
            return Ok(());
        }
        let dialect = self.dialect;
        let quoting = self.quoting;
        let nulls = self.nulls;
        if self.field_count != 0 {
            write_delimiter(self.output, dialect);
        }
        self.field_count += 1;
        let is_first = self.field_count == 1;
        if is_first {
            self.only_field_was_non_null_empty = field.is_empty();
        }
        match configured_path(dialect, nulls) {
            ConfiguredPath::Plain => {
                write_record_field(self.output, dialect, quoting, field, is_first)
            }
            ConfiguredPath::Configured => {
                write_configured_field(self.output, dialect, quoting, nulls, field, is_first)
            }
        }
    }

    /// Frame one explicit NULL field, exactly as [`emit_nullable_record`] would.
    #[inline]
    pub(crate) fn write_null_field(&mut self) -> Result<(), Error> {
        let dialect = self.dialect;
        let quoting = self.quoting;
        let nulls = self.nulls;
        if self.field_count != 0 {
            write_delimiter(self.output, dialect);
        }
        self.field_count += 1;
        if nulls == Nulls::None {
            // Under `Nulls::None` an absent value is an empty field, which is
            // writable under every policy, so this cannot fail.
            self.only_field_was_non_null_empty = true;
            write_non_null_empty(self.output, dialect, quoting);
            Ok(())
        } else {
            write_null(self.output, nulls);
            Ok(())
        }
    }

    /// Splice in the `""` that a lone empty field needs under
    /// [`Quoting::Necessary`], matching [`emit_nullable_record_runtime`] and
    /// [`emit_nullable_record`].
    fn apply_single_empty_fixup(&mut self) {
        let dialect = self.dialect;
        let quoting = self.quoting;
        if self.field_count == 1
            && self.only_field_was_non_null_empty
            && quoting == Quoting::Necessary
            && self.output.len() == self.record_start
        {
            self.output.extend(&[dialect.quote, dialect.quote]);
        }
    }

    /// Finish the record, writing its terminator, and return the field count.
    pub(crate) fn finish(mut self) -> usize {
        self.apply_single_empty_fixup();
        finish_record(self.output, self.dialect);
        self.field_count
    }
}

impl<F: CsvFormat, B: ByteSink> EncodeVisitor for DirectEncodeVisitor<'_, F, B> {
    fn visit_field(
        &mut self,
        _index: usize,
        _name: &'static str,
        bytes: &[u8],
    ) -> Result<(), Error> {
        self.write_field(bytes)
    }

    fn visit_null(&mut self, _index: usize, _name: &'static str) -> Result<(), Error> {
        self.write_null_field()
    }
}

// ── Compile-time format accessors ───────────────────────────────────────────
//
// Static formats fold `F::OPTIONS` to immediates; `Dynamic` keeps the value the
// emitter passed in. These must stay `#[inline(always)]` so the fold happens in
// the calling kernel.

#[inline(always)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[expect(
    clippy::inline_always,
    reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
)]
pub(crate) fn fmt_dialect<F: CsvFormat>(dialect: Dialect) -> Dialect {
    match F::OPTIONS {
        Some(options) => options.dialect,
        None => dialect,
    }
}

#[inline(always)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[expect(
    clippy::inline_always,
    reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
)]
pub(crate) fn fmt_quoting<F: CsvFormat>(quoting: Quoting) -> Quoting {
    match F::OPTIONS {
        Some(options) => options.quoting,
        None => quoting,
    }
}

#[inline(always)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[expect(
    clippy::inline_always,
    reason = "the fold from `F::OPTIONS` to an immediate only happens once this body is inlined into the kernel, which is the whole point of the accessor"
)]
pub(crate) fn fmt_nulls<F: CsvFormat>(nulls: Nulls) -> Nulls {
    match F::OPTIONS {
        Some(options) => options.nulls,
        None => nulls,
    }
}

#[inline]
pub(crate) fn emit_record<F, B, I, T>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    fields: I,
) -> Result<usize, Error>
where
    F: CsvFormat,
    B: ByteSink,
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    emit_record_runtime(
        output,
        fmt_dialect::<F>(dialect),
        fmt_quoting::<F>(quoting),
        fields,
    )
}

pub(crate) fn emit_record_runtime<B, I, T>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    fields: I,
) -> Result<usize, Error>
where
    B: ByteSink,
    I: IntoIterator<Item = T>,
    T: AsRef<[u8]>,
{
    let record_start = output.len();
    let mut fields = fields.into_iter();
    let Some(first) = fields.next() else {
        finish_record(output, dialect);
        return Ok(0);
    };
    match quoting {
        Quoting::Necessary => {
            let mut field_count = 1;
            match necessary_path(dialect) {
                NecessaryPath::Csv => {
                    write_csv_necessary(output, first.as_ref(), true);
                    for field in fields {
                        field_count += 1;
                        output.push(b',');
                        write_csv_necessary(output, field.as_ref(), false);
                    }
                }
                NecessaryPath::General => {
                    write_necessary(output, dialect, first.as_ref(), true);
                    for field in fields {
                        field_count += 1;
                        write_delimiter(output, dialect);
                        write_necessary(output, dialect, field.as_ref(), false);
                    }
                }
            }
            if output.len() == record_start {
                output.extend(&[dialect.quote, dialect.quote]);
            }
            finish_record(output, dialect);
            Ok(field_count)
        }
        Quoting::NonNumeric => {
            let mut field_count = 1;
            write_non_numeric(output, dialect, first.as_ref(), true);
            for field in fields {
                field_count += 1;
                write_delimiter(output, dialect);
                write_non_numeric(output, dialect, field.as_ref(), false);
            }
            finish_record(output, dialect);
            Ok(field_count)
        }
        Quoting::Always => {
            let mut field_count = 1;
            write_quoted(output, dialect, first.as_ref());
            for field in fields {
                field_count += 1;
                write_delimiter(output, dialect);
                write_quoted(output, dialect, field.as_ref());
            }
            finish_record(output, dialect);
            Ok(field_count)
        }
        Quoting::Never => (|| {
            let mut field_count = 1;
            let result = (|| {
                write_unquoted(output, dialect, first.as_ref(), true)?;
                for field in fields {
                    field_count += 1;
                    write_delimiter(output, dialect);
                    write_unquoted(output, dialect, field.as_ref(), false)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                output.truncate(record_start);
                return Err(error);
            }
            finish_record(output, dialect);
            Ok(field_count)
        })(),
        Quoting::Raw => {
            let mut field_count = 1;
            output.extend(first.as_ref());
            for field in fields {
                field_count += 1;
                write_delimiter(output, dialect);
                output.extend(field.as_ref());
            }
            finish_record(output, dialect);
            Ok(field_count)
        }
    }
}

#[inline]
pub(crate) fn emit_nullable_record<F, B, I, T>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    fields: I,
) -> Result<usize, Error>
where
    F: CsvFormat,
    B: ByteSink,
    I: IntoIterator<Item = Option<T>>,
    T: AsRef<[u8]>,
{
    emit_nullable_record_runtime(
        output,
        fmt_dialect::<F>(dialect),
        fmt_quoting::<F>(quoting),
        fmt_nulls::<F>(nulls),
        fields,
    )
}

pub(crate) fn emit_nullable_record_runtime<B, I, T>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    fields: I,
) -> Result<usize, Error>
where
    B: ByteSink,
    I: IntoIterator<Item = Option<T>>,
    T: AsRef<[u8]>,
{
    let record_start = output.len();
    let mut field_count = 0;
    let mut only_field_was_non_null_empty = false;
    for field in fields {
        if field_count != 0 {
            write_delimiter(output, dialect);
        }
        field_count += 1;
        match field {
            None if nulls == Nulls::None => {
                only_field_was_non_null_empty = true;
                // Every reason to reject a field is a property of its bytes, so
                // an empty one is writable under every policy and this cannot
                // fail. The rollback the other arms need is therefore absent.
                write_non_null_empty(output, dialect, quoting);
            }
            None => write_null(output, nulls),
            Some(field) => {
                let field = field.as_ref();
                only_field_was_non_null_empty = field.is_empty();
                if let Err(error) =
                    write_configured_field(output, dialect, quoting, nulls, field, field_count == 1)
                {
                    output.truncate(record_start);
                    return Err(error);
                }
            }
        }
    }
    if field_count == 1
        && only_field_was_non_null_empty
        && output.len() == record_start
        && quoting == Quoting::Necessary
    {
        output.extend(&[dialect.quote, dialect.quote]);
    }
    finish_record(output, dialect);
    Ok(field_count)
}

pub(crate) fn write_configured_field<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    field: &[u8],
    is_first: bool,
) -> Result<(), Error> {
    if let Some(escape) = dialect.escape.unquoted_byte() {
        if nulls == Nulls::Mysql && field == b"\\N" {
            output.extend(&[escape, b'\\', b'N']);
            return Ok(());
        }
        write_unquoted_escaped_field(output, dialect, escape, field, is_first);
        return Ok(());
    }
    if nulls == Nulls::PostgresCsv && field.is_empty() {
        write_quoted(output, dialect, field);
        return Ok(());
    }
    if nulls == Nulls::Mysql && field == b"\\N" {
        match quoting {
            Quoting::Necessary | Quoting::NonNumeric | Quoting::Always => {
                write_quoted(output, dialect, field);
                return Ok(());
            }
            Quoting::Never | Quoting::Raw => {
                return Err(Error::detailed(
                    ErrorKind::Encode,
                    "a non-NULL MySQL \\N field requires quoting or unquoted escaping",
                ));
            }
        }
    }
    match quoting {
        Quoting::Necessary => write_necessary(output, dialect, field, is_first),
        Quoting::NonNumeric => write_non_numeric(output, dialect, field, is_first),
        Quoting::Always => write_quoted(output, dialect, field),
        Quoting::Never => write_unquoted(output, dialect, field, is_first)?,
        Quoting::Raw => output.extend(field),
    }
    Ok(())
}

pub(crate) fn write_null<B: ByteSink>(output: &mut B, nulls: Nulls) {
    if nulls == Nulls::Mysql {
        output.extend(b"\\N");
    }
}

/// Write a field for a dialect that escapes rather than quotes.
///
/// `MySQL` and Python's `QUOTE_NONE`-plus-`escapechar` share the shape --
/// nothing is ever quoted, so every byte that would otherwise read as
/// structure is prefixed with `escape` instead. They differ in one place: the
/// control bytes `MySQL` spells with a letter, which Python writes literally
/// after the escape.
pub(crate) fn write_unquoted_escaped_field<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    escape: u8,
    field: &[u8],
    is_first: bool,
) {
    let mysql = dialect.escape == Escape::Mysql;
    // A leading BOM or comment byte would be read as structure, and this
    // syntax cannot quote a field, so escape that byte instead. The quote byte
    // is escaped for the same reason: a parser with quoting enabled would read
    // a bare leading quote as opening one.
    let mut start = if is_first
        && (field.starts_with(b"\xEF\xBB\xBF")
            || field
                .first()
                .is_some_and(|&byte| dialect.comment == Some(byte)))
    {
        output.extend(&[escape, field[0]]);
        1
    } else {
        0
    };
    for (index, &byte) in field.iter().enumerate().skip(start) {
        let escaped = match byte {
            0 if mysql => Some(b'0'),
            b'\x08' if mysql => Some(b'b'),
            b'\n' if mysql => Some(b'n'),
            b'\r' if mysql => Some(b'r'),
            b'\t' if mysql => Some(b't'),
            b'\x1A' if mysql => Some(b'Z'),
            _ if byte == escape => Some(escape),
            _ if byte == dialect.delimiter
                || byte == dialect.record_ending.byte()
                || byte == dialect.quote =>
            {
                Some(byte)
            }
            _ => None,
        };
        if let Some(escaped) = escaped {
            output.extend(&field[start..index]);
            output.extend(&[escape, escaped]);
            start = index + 1;
        }
    }
    output.extend(&field[start..]);
}

pub(crate) fn validate_field_count(
    policy: FieldCount,
    expected: &mut Option<usize>,
    actual: usize,
) -> Result<(), Error> {
    let required = match policy {
        FieldCount::Flexible => return Ok(()),
        FieldCount::Exact(required) => required,
        FieldCount::MatchFirst => {
            if let Some(required) = *expected {
                required
            } else {
                *expected = Some(actual);
                return Ok(());
            }
        }
    };
    if actual == required {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::FieldCountMismatch {
                expected: required,
                actual,
            },
            Location::START,
        ))
    }
}

/// Write the delimiter between two fields.
///
/// A single-byte dialect writes one byte, as it always did; the test that says
/// so is constant for every built-in.
#[inline(always)]
#[expect(
    clippy::inline_always,
    reason = "the tail test has to fold to a constant at each of the six call sites, which only happens once this body is inlined into them"
)]
fn write_delimiter<B: ByteSink>(output: &mut B, dialect: Dialect) {
    output.push(dialect.delimiter);
    if !dialect.delimiter_tail().is_empty() {
        output.extend(dialect.delimiter_tail().as_slice());
    }
}

pub(crate) fn finish_record<B: ByteSink>(output: &mut B, dialect: Dialect) {
    if dialect.record_ending == RecordEnding::CrLf {
        output.push(b'\r');
    }
    output.push(dialect.record_ending.byte());
    if !dialect.ending_tail().is_empty() {
        output.extend(dialect.ending_tail().as_slice());
    }
}

pub(crate) fn write_necessary<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    field: &[u8],
    is_first: bool,
) {
    if needs_quotes(dialect, field, is_first) {
        write_quoted(output, dialect, field);
    } else {
        output.extend(field);
    }
}

#[inline]
pub(crate) fn write_csv_necessary<B: ByteSink>(output: &mut B, field: &[u8], is_first: bool) {
    if needs_csv_quotes(field, is_first) {
        output.push(b'"');
        let mut remaining = field;
        while let Some(at) = find1(b'"', remaining) {
            output.extend(&remaining[..at]);
            output.extend(b"\"\"");
            // gamma::skip(stmt.delete_assign, arith.add_to_mul, arith.add_to_sub, expr.decrement, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
            remaining = &remaining[at + 1..];
        }
        output.extend(remaining);
        output.push(b'"');
    } else {
        output.extend(field);
    }
}

pub(crate) fn needs_csv_quotes(field: &[u8], is_first: bool) -> bool {
    if is_first && field.starts_with(b"\xEF\xBB\xBF") {
        return true;
    }
    if field.len() >= SIMD_QUOTING_SCAN_BYTES {
        let comma = repeated_byte(b',');
        let quote = repeated_byte(b'"');
        let newline = repeated_byte(b'\n');
        let carriage_return = repeated_byte(b'\r');
        let (chunks, remainder) = field.as_chunks::<8>();
        chunks.iter().any(|chunk| {
            let word = u64::from_ne_bytes(*chunk);
            zero_byte_mask(word ^ comma)
                | zero_byte_mask(word ^ quote)
                | zero_byte_mask(word ^ newline)
                | zero_byte_mask(word ^ carriage_return)
                != 0
        }) || remainder
            .iter()
            .any(|&byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    } else {
        field
            .iter()
            .any(|&byte| matches!(byte, b',' | b'"' | b'\n' | b'\r'))
    }
}

pub(crate) fn write_non_numeric<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    field: &[u8],
    is_first: bool,
) {
    if needs_quotes(dialect, field, is_first) || !is_numeric(field) {
        write_quoted(output, dialect, field);
    } else {
        output.extend(field);
    }
}

pub(crate) fn is_numeric(field: &[u8]) -> bool {
    match scan_numeric(field) {
        NumericScan::Numeric => true,
        NumericScan::NotNumeric => false,
        NumericScan::Fallback => {
            str::from_utf8(field).is_ok_and(|field| field.parse::<f64>().is_ok())
        }
    }
}

/// The outcome of a byte-level scan for the common integer/decimal grammar.
enum NumericScan {
    /// Only ASCII sign, digits and at most one `.`, with at least one digit:
    /// always accepted by `f64::from_str`, so the caller can skip the parse.
    Numeric,
    /// Contains a byte that can never appear in any `f64::from_str`-accepted
    /// text (any non-ASCII byte, whitespace, most punctuation, and so on):
    /// always rejected by `f64::from_str`, so the caller can skip the parse.
    NotNumeric,
    /// Contains only bytes that could belong to a valid float, but not in the
    /// plain digit/dot shape above — an exponent, `inf`/`infinity`/`nan` in
    /// either case, a second `.`, or a malformed mix of those. Full grammar
    /// recognition is more work than the fallback parse it exists to
    /// replace, so the caller runs that parse instead of duplicating it.
    Fallback,
}

/// Scan `field` for the plain `[sign] digits [. digits]` shape that covers
/// ordinary numeric CSV fields, without validating UTF-8 or invoking the
/// float parser. Bytes that can only appear in `inf`/`infinity`/`nan`
/// (case-insensitive) or an exponent defer to the exact fallback rather than
/// reimplementing that grammar, per the scope this item settled on.
fn scan_numeric(field: &[u8]) -> NumericScan {
    let body = match field {
        [b'+' | b'-', rest @ ..] => rest,
        _ => field,
    };
    let mut seen_digit = false;
    let mut seen_dot = false;
    for &byte in body {
        match byte {
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            b'.' | b'e' | b'E' | b'i' | b'I' | b'n' | b'N' | b'f' | b'F' | b'a' | b'A' | b'y'
            | b'Y' => return NumericScan::Fallback,
            _ => return NumericScan::NotNumeric,
        }
    }
    if seen_digit {
        NumericScan::Numeric
    } else {
        // Only a `.`, e.g. "." or "-.": never accepted by `f64::from_str`.
        NumericScan::NotNumeric
    }
}

pub(crate) fn write_unquoted<B: ByteSink>(
    output: &mut B,
    dialect: Dialect,
    field: &[u8],
    is_first: bool,
) -> Result<(), Error> {
    if needs_quotes(dialect, field, is_first) {
        return Err(Error::detailed(ErrorKind::Encode, "field requires quoting"));
    }
    output.extend(field);
    Ok(())
}

#[cold]
#[inline(never)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn unreachable_unquoted_escape() -> ! {
    unreachable!("unquoted-escape dialects are written by write_unquoted_escaped_field")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EscapeMatch {
    Leading,
    Later(usize),
}

impl EscapeMatch {
    const fn index(self) -> usize {
        match self {
            Self::Leading => 0,
            Self::Later(index) => index,
        }
    }
}

fn next_doubled_quote(dialect: Dialect, remaining: &[u8]) -> Option<EscapeMatch> {
    if remaining.first() == Some(&dialect.quote) {
        Some(EscapeMatch::Leading)
    } else {
        find1(dialect.quote, remaining).map(EscapeMatch::Later)
    }
}

fn next_backslash_escape(dialect: Dialect, escape: u8, remaining: &[u8]) -> Option<EscapeMatch> {
    if remaining
        .first()
        .is_some_and(|&byte| byte == dialect.quote || byte == escape)
    {
        Some(EscapeMatch::Leading)
    } else {
        find2(dialect.quote, escape, remaining).map(EscapeMatch::Later)
    }
}

pub(crate) fn write_quoted<B: ByteSink>(output: &mut B, dialect: Dialect, field: &[u8]) {
    output.push(dialect.quote);
    match dialect.escape {
        Escape::DoubleQuote => {
            let mut remaining = field;
            while let Some(found) = next_doubled_quote(dialect, remaining) {
                let at = found.index();
                output.extend(&remaining[..at]);
                output.extend(&[dialect.quote, dialect.quote]);
                // gamma::skip(stmt.delete_assign, arith.add_to_mul, arith.add_to_sub, expr.decrement, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                remaining = &remaining[at + 1..];
            }
            output.extend(remaining);
        }
        Escape::Backslash(escape) => {
            let mut remaining = field;
            while let Some(found) = next_backslash_escape(dialect, escape, remaining) {
                let at = found.index();
                output.extend(&remaining[..at]);
                output.extend(&[escape, remaining[at]]);
                // gamma::skip(stmt.delete_assign, arith.add_to_mul, arith.add_to_sub, expr.decrement, literal.int_decrement, reason = "mutation causes non-termination or unbounded resource use")
                remaining = &remaining[at + 1..];
            }
            output.extend(remaining);
        }
        Escape::Mysql | Escape::Unquoted(_) => unreachable_unquoted_escape(),
    }
    output.push(dialect.quote);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuotingScan {
    Blocks,
    Words,
}

#[inline]
const fn quoting_scan(field: &[u8]) -> QuotingScan {
    if field.len() >= SIMD_QUOTING_SCAN_BYTES {
        QuotingScan::Blocks
    } else {
        QuotingScan::Words
    }
}

pub(crate) fn needs_quotes(dialect: Dialect, field: &[u8], is_first: bool) -> bool {
    if is_first
        && (field.starts_with(b"\xEF\xBB\xBF")
            || dialect
                .comment
                .is_some_and(|comment| field.first() == Some(&comment)))
    {
        return true;
    }
    let delimiter = dialect.delimiter;
    let quote = dialect.quote;
    let record_ending = dialect.record_ending.byte();
    let newline = matches!(
        dialect.record_ending,
        RecordEnding::Newline | RecordEnding::CrLf
    );

    // Wide fields amortise the SIMD block setup, while the word-at-a-time scan
    // stays ahead on the short fields that dominate real documents (`find`
    // degrades to byte-at-a-time under one block). Handing it the whole field
    // rather than only whole blocks and routing the remainder back through the
    // word loop measured worse, since it forces that loop's setup even when the
    // remainder is empty. `benches/needs_quotes.rs` prices both arms at widths
    // either side of the threshold, and `scripts/perf_gate.py` pins the
    // crossover.
    match quoting_scan(field) {
        QuotingScan::Blocks => needs_quotes_blocks(delimiter, quote, record_ending, newline, field),
        QuotingScan::Words => needs_quotes_words(delimiter, quote, record_ending, newline, field),
    }
}

/// Scan a field for bytes that force quoting, one SIMD block at a time.
///
/// Chosen at and above [`SIMD_QUOTING_SCAN_BYTES`]; below it `find` itself
/// falls back to a byte-at-a-time scan and loses to [`needs_quotes_words`].
fn needs_quotes_blocks(
    delimiter: u8,
    quote: u8,
    record_ending: u8,
    newline: bool,
    field: &[u8],
) -> bool {
    if newline {
        find4(delimiter, quote, record_ending, b'\r', field)
    } else {
        find3(delimiter, quote, record_ending, field)
    }
    .is_some()
}

/// Scan a field for bytes that force quoting, eight bytes at a time.
///
/// Chosen below [`SIMD_QUOTING_SCAN_BYTES`]. Correct at any width, so the
/// benchmark can run it against [`needs_quotes_blocks`] on the same input.
fn needs_quotes_words(
    delimiter: u8,
    quote: u8,
    record_ending: u8,
    newline: bool,
    field: &[u8],
) -> bool {
    let delimiter_word = repeated_byte(delimiter);
    let quote_word = repeated_byte(quote);
    let terminator_word = repeated_byte(record_ending);
    let carriage_return_word = repeated_byte(b'\r');
    let (chunks, remainder) = field.as_chunks::<8>();
    if chunks.iter().any(|chunk| {
        let word = u64::from_ne_bytes(*chunk);
        let mut found = zero_byte_mask(word ^ delimiter_word)
            | zero_byte_mask(word ^ quote_word)
            | zero_byte_mask(word ^ terminator_word);
        if newline {
            found |= zero_byte_mask(word ^ carriage_return_word);
        }
        found != 0
    }) {
        return true;
    }
    remainder.iter().any(|&byte| {
        byte == delimiter || byte == quote || byte == record_ending || (newline && byte == b'\r')
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(feature = "benchmarking")]
fn benchmark_dialect(record_ending: RecordEnding) -> Dialect {
    Dialect {
        record_ending,
        ..Dialect::default()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(feature = "benchmarking")]
pub(crate) fn benchmark_needs_quotes(record_ending: RecordEnding, field: &[u8]) -> bool {
    needs_quotes(benchmark_dialect(record_ending), field, false)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(feature = "benchmarking")]
pub(crate) fn benchmark_needs_quotes_blocks(record_ending: RecordEnding, field: &[u8]) -> bool {
    let dialect = benchmark_dialect(record_ending);
    needs_quotes_blocks(
        dialect.delimiter,
        dialect.quote,
        dialect.record_ending.byte(),
        matches!(record_ending, RecordEnding::Newline | RecordEnding::CrLf),
        field,
    )
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(feature = "benchmarking")]
pub(crate) fn benchmark_needs_quotes_words(record_ending: RecordEnding, field: &[u8]) -> bool {
    let dialect = benchmark_dialect(record_ending);
    needs_quotes_words(
        dialect.delimiter,
        dialect.quote,
        dialect.record_ending.byte(),
        matches!(record_ending, RecordEnding::Newline | RecordEnding::CrLf),
        field,
    )
}

#[cfg(feature = "benchmarking")]
const fn benchmark_quoted_capacity(field: &[u8]) -> usize {
    field.len().saturating_mul(2).saturating_add(2)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(feature = "benchmarking")]
pub(crate) fn benchmark_escape_double_quote(field: &[u8], repetitions: usize) -> usize {
    let dialect = Dialect::default();
    let mut output = Vec::with_capacity(benchmark_quoted_capacity(field));
    let mut bytes = 0;
    for _ in 0..repetitions {
        output.clear();
        write_quoted(&mut output, dialect, field);
        bytes += output.len();
    }
    bytes
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{
        ByteSink, ConfiguredPath, DirectEncodeVisitor, EscapeMatch, NecessaryPath, NumericScan,
        QuotingScan, configured_path, emit_nullable_record_runtime, emit_record_runtime,
        is_numeric, necessary_path, needs_csv_quotes, needs_quotes, needs_quotes_blocks,
        needs_quotes_words, next_backslash_escape, next_doubled_quote, quoting_scan,
        record_too_large, scan_numeric, validate_field_count, write_configured_field, write_null,
        write_record_field, write_unquoted, write_unquoted_escaped_field,
    };
    #[cfg(feature = "multibyte")]
    use crate::config::Tail;
    use crate::config::{Dialect, Escape, FieldCount, FormatOptions, Nulls, Quoting, RecordEnding};
    use crate::error::ErrorKind;
    use crate::format::{Csv, CsvFormat, Dynamic};

    fn emitted_record(dialect: Dialect, quoting: Quoting, fields: &[&[u8]]) -> (Vec<u8>, usize) {
        let mut output = Vec::new();
        let count =
            emit_record_runtime(&mut output, dialect, quoting, fields.iter().copied()).unwrap();
        (output, count)
    }

    fn emitted_nullable_record(
        dialect: Dialect,
        quoting: Quoting,
        nulls: Nulls,
        fields: &[Option<&[u8]>],
    ) -> (Vec<u8>, usize) {
        let mut output = Vec::new();
        let count = emit_nullable_record_runtime(
            &mut output,
            dialect,
            quoting,
            nulls,
            fields.iter().copied(),
        )
        .unwrap();
        (output, count)
    }

    #[derive(Default)]
    struct TracingSink {
        bytes: Vec<u8>,
        empty_extends: usize,
        truncations: Vec<usize>,
    }

    impl ByteSink for TracingSink {
        fn len(&self) -> usize {
            self.bytes.len()
        }

        fn push(&mut self, byte: u8) {
            self.bytes.push(byte);
        }

        fn extend(&mut self, bytes: &[u8]) {
            if bytes.is_empty() {
                self.empty_extends += 1;
            }
            self.bytes.extend_from_slice(bytes);
        }

        fn truncate(&mut self, len: usize) {
            self.truncations.push(len);
            self.bytes.truncate(len);
        }
    }

    #[test]
    fn record_emission_matrix_has_exact_bytes_and_counts() {
        let commented = Dialect {
            comment: Some(b'#'),
            ..Dialect::CSV
        };
        let crlf = Dialect {
            record_ending: RecordEnding::CrLf,
            ..Dialect::CSV
        };
        let semicolon_ending = Dialect {
            record_ending: RecordEnding::Byte(b';'),
            ..Dialect::TSV
        };
        let numeric_commented = Dialect {
            comment: Some(b'1'),
            ..Dialect::CSV
        };
        let cases: &[(Dialect, Quoting, &[&[u8]], &[u8], usize)] = &[
            (Dialect::CSV, Quoting::Necessary, &[], b"\n", 0),
            (
                Dialect::CSV,
                Quoting::Necessary,
                &[b"".as_slice()],
                b"\"\"\n",
                1,
            ),
            (
                Dialect::CSV,
                Quoting::Necessary,
                &[b"".as_slice(), b""],
                b",\n",
                2,
            ),
            (
                Dialect::CSV,
                Quoting::Necessary,
                &[b"a".as_slice(), b"b,c", b"d\"e"],
                b"a,\"b,c\",\"d\"\"e\"\n",
                3,
            ),
            (
                Dialect::TSV,
                Quoting::Necessary,
                &[b"a,b".as_slice(), b"x\ty"],
                b"a,b\t\"x\ty\"\n",
                2,
            ),
            (
                commented,
                Quoting::Necessary,
                &[b"#first".as_slice(), b"#second"],
                b"\"#first\",#second\n",
                2,
            ),
            (
                Dialect::CSV,
                Quoting::Necessary,
                &[b"first".as_slice(), b"\xEF\xBB\xBFsecond"],
                b"first,\xEF\xBB\xBFsecond\n",
                2,
            ),
            (
                Dialect::CSV,
                Quoting::NonNumeric,
                &[b"12".as_slice(), b"-1.5", b"text", b"1e3"],
                b"12,-1.5,\"text\",1e3\n",
                4,
            ),
            (
                numeric_commented,
                Quoting::NonNumeric,
                &[b"123".as_slice()],
                b"\"123\"\n",
                1,
            ),
            (
                numeric_commented,
                Quoting::NonNumeric,
                &[b"0".as_slice(), b"123"],
                b"0,123\n",
                2,
            ),
            (
                Dialect::CSV,
                Quoting::Always,
                &[b"a".as_slice(), b"b\"c"],
                b"\"a\",\"b\"\"c\"\n",
                2,
            ),
            (
                Dialect::CSV,
                Quoting::Raw,
                &[b"a,b".as_slice(), b"c"],
                b"a,b,c\n",
                2,
            ),
            (
                commented,
                Quoting::Never,
                &[b"ok".as_slice(), b"#later"],
                b"ok,#later\n",
                2,
            ),
            (
                crlf,
                Quoting::Necessary,
                &[b"a".as_slice(), b"b"],
                b"a,b\r\n",
                2,
            ),
            (
                semicolon_ending,
                Quoting::Raw,
                &[b"a".as_slice(), b"b"],
                b"a\tb;",
                2,
            ),
        ];

        for &(dialect, quoting, fields, expected, expected_count) in cases {
            let (actual, count) = emitted_record(dialect, quoting, fields);
            assert_eq!(actual, expected, "{dialect:?} {quoting:?} {fields:?}");
            assert_eq!(count, expected_count, "{dialect:?} {quoting:?}");
        }
    }

    #[test]
    fn nullable_emission_distinguishes_null_from_empty() {
        let cases: &[(Nulls, Quoting, &[Option<&[u8]>], &[u8], usize)] = &[
            (Nulls::None, Quoting::Necessary, &[None], b"\"\"\n", 1),
            (Nulls::None, Quoting::NonNumeric, &[None], b"\"\"\n", 1),
            (Nulls::None, Quoting::Always, &[None], b"\"\"\n", 1),
            (Nulls::None, Quoting::Never, &[None], b"\n", 1),
            (Nulls::None, Quoting::Raw, &[None], b"\n", 1),
            (Nulls::None, Quoting::Necessary, &[Some(b"")], b"\"\"\n", 1),
            (Nulls::None, Quoting::Necessary, &[None, None], b",\n", 2),
            (Nulls::PostgresCsv, Quoting::Necessary, &[None], b"\n", 1),
            (
                Nulls::PostgresCsv,
                Quoting::Necessary,
                &[Some(b"")],
                b"\"\"\n",
                1,
            ),
            (
                Nulls::PostgresCsv,
                Quoting::Necessary,
                &[None, Some(b"")],
                b",\"\"\n",
                2,
            ),
            (
                Nulls::Mysql,
                Quoting::Necessary,
                &[None, Some(b"\\N"), Some(b"")],
                b"\\N,\"\\N\",\n",
                3,
            ),
        ];

        for &(nulls, quoting, fields, expected, expected_count) in cases {
            let (actual, count) = emitted_nullable_record(Dialect::CSV, quoting, nulls, fields);
            assert_eq!(actual, expected, "{nulls:?} {quoting:?} {fields:?}");
            assert_eq!(count, expected_count, "{nulls:?} {quoting:?}");
        }
    }

    #[test]
    fn direct_visitor_matches_record_framing_for_empty_and_multiple_fields() {
        let mut untouched = b"prefix".to_vec();
        let visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut untouched,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        assert_eq!(visitor.record_start, 6);
        assert_eq!(visitor.field_count, 0);
        assert!(!visitor.only_field_was_non_null_empty);
        assert_eq!(visitor.finish(), 0);
        assert_eq!(untouched, b"prefix\n");

        let mut empty = Vec::new();
        let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut empty,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        visitor.write_field(b"").unwrap();
        assert_eq!(visitor.finish(), 1);
        assert_eq!(empty, b"\"\"\n");

        let mut multiple = Vec::new();
        let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut multiple,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        visitor.write_field(b"a").unwrap();
        assert!(!visitor.only_field_was_non_null_empty);
        visitor.write_field(b"b,c").unwrap();
        assert!(!visitor.only_field_was_non_null_empty);
        assert_eq!(visitor.finish(), 2);
        assert_eq!(multiple, b"a,\"b,c\"\n");

        let mut later_empty = Vec::new();
        let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut later_empty,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        visitor.write_field(b"a").unwrap();
        assert!(!visitor.only_field_was_non_null_empty);
        visitor.write_field(b"").unwrap();
        assert!(!visitor.only_field_was_non_null_empty);
        assert_eq!(visitor.finish(), 2);
        assert_eq!(later_empty, b"a,\n");

        let mut null_none = Vec::new();
        let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut null_none,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        visitor.write_null_field().unwrap();
        assert_eq!(visitor.finish(), 1);
        assert_eq!(null_none, b"\"\"\n");

        for (quoting, expected) in [
            (Quoting::NonNumeric, b"\"\"\n".as_slice()),
            (Quoting::Always, b"\"\"\n"),
            (Quoting::Never, b"\n"),
            (Quoting::Raw, b"\n"),
        ] {
            let mut output = Vec::new();
            let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
                &mut output,
                Dialect::CSV,
                quoting,
                Nulls::None,
            );
            visitor.write_null_field().unwrap();
            assert_eq!(visitor.finish(), 1);
            assert_eq!(output, expected, "{quoting:?}");
        }

        let mut postgres_null = Vec::new();
        let mut visitor = DirectEncodeVisitor::<Dynamic, _>::new(
            &mut postgres_null,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::PostgresCsv,
        );
        visitor.write_null_field().unwrap();
        assert_eq!(visitor.finish(), 1);
        assert_eq!(postgres_null, b"\n");
    }

    #[test]
    fn static_csv_routing_does_not_capture_custom_formats() {
        struct Pipes;

        impl CsvFormat for Pipes {
            const OPTIONS: Option<FormatOptions> =
                Some(FormatOptions::CSV.delimiter(b'|').quote(b'\''));
        }

        let fields = [b"a,b".as_slice(), b"x|y", b"say 'hi'", b"\xC3\xA9"];

        let mut csv = Vec::new();
        let mut csv_visitor = DirectEncodeVisitor::<Csv, _>::new(
            &mut csv,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::None,
        );
        for field in fields {
            csv_visitor.write_field(field).unwrap();
        }
        csv_visitor.finish();
        assert_eq!(csv, b"\"a,b\",x|y,say 'hi',\xC3\xA9\n");

        let custom = FormatOptions::CSV.delimiter(b'|').quote(b'\'');
        let mut pipes = Vec::new();
        let mut pipes_visitor = DirectEncodeVisitor::<Pipes, _>::new(
            &mut pipes,
            custom.dialect,
            custom.quoting,
            custom.nulls,
        );
        for field in fields {
            pipes_visitor.write_field(field).unwrap();
        }
        pipes_visitor.finish();
        assert_eq!(pipes, b"a,b|'x|y'|'say ''hi'''|\xC3\xA9\n");
    }

    #[test]
    fn csv_swar_scan_covers_every_word_position_and_tail() {
        for width in 0..=200 {
            let clean = vec![b'a'; width];
            assert!(!needs_csv_quotes(&clean, false), "plain width {width}");

            let non_ascii = vec![0xC3; width];
            assert!(
                !needs_csv_quotes(&non_ascii, false),
                "non-ASCII width {width}"
            );

            for special in [b',', b'"', b'\n', b'\r'] {
                for at in 0..width {
                    let mut field = clean.clone();
                    field[at] = special;
                    assert!(
                        needs_csv_quotes(&field, false),
                        "{special:?} at {at} of {width}"
                    );
                }
            }
        }

        assert!(needs_csv_quotes(b"\xEF\xBB\xBFvalue", true));
        assert!(!needs_csv_quotes(b"\xEF\xBB\xBFvalue", false));
    }

    #[test]
    fn failed_record_emission_rolls_back_and_reports_exact_errors() {
        let mut output = b"prefix".to_vec();
        let error = emit_record_runtime(
            &mut output,
            Dialect::CSV,
            Quoting::Never,
            [b"ok".as_slice(), b"needs,quotes"],
        )
        .expect_err("the second field requires quoting");
        assert_eq!(output, b"prefix");
        assert_eq!(error.kind(), ErrorKind::Encode);
        assert_eq!(error.to_string(), "field requires quoting");

        let mut mysql = b"prefix".to_vec();
        let error = emit_nullable_record_runtime(
            &mut mysql,
            Dialect::CSV,
            Quoting::Raw,
            Nulls::Mysql,
            [Some(b"ok".as_slice()), Some(b"\\N")],
        )
        .expect_err("a non-NULL MySQL marker cannot be emitted raw");
        assert_eq!(mysql, b"prefix");
        assert_eq!(
            error.to_string(),
            "a non-NULL MySQL \\N field requires quoting or unquoted escaping"
        );

        assert_eq!(
            record_too_large().to_string(),
            "encoded record size overflows usize"
        );
    }

    #[test]
    fn configured_fields_and_unquoted_escaping_are_exact() {
        let mut configured = Vec::new();
        write_configured_field(
            &mut configured,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::Mysql,
            b"\\N",
            true,
        )
        .unwrap();
        assert_eq!(configured, b"\"\\N\"");

        let mut postgres_empty = Vec::new();
        write_configured_field(
            &mut postgres_empty,
            Dialect::CSV,
            Quoting::Necessary,
            Nulls::PostgresCsv,
            b"",
            true,
        )
        .unwrap();
        assert_eq!(postgres_empty, b"\"\"");

        let mysql_dialect = Dialect {
            escape: Escape::Mysql,
            comment: Some(b'#'),
            ..Dialect::CSV
        };
        let mut marker = Vec::new();
        write_configured_field(
            &mut marker,
            mysql_dialect,
            Quoting::Necessary,
            Nulls::Mysql,
            b"\\N",
            true,
        )
        .unwrap();
        assert_eq!(marker, b"\\\\N");

        let hybrid_dialect = Dialect {
            escape: Escape::Unquoted(b'!'),
            ..Dialect::CSV
        };
        let mut hybrid_mysql = Vec::new();
        write_configured_field(
            &mut hybrid_mysql,
            hybrid_dialect,
            Quoting::Necessary,
            Nulls::Mysql,
            b"\\N",
            true,
        )
        .unwrap();
        assert_eq!(hybrid_mysql, b"!\\N");

        let mut hybrid_plain = Vec::new();
        write_configured_field(
            &mut hybrid_plain,
            hybrid_dialect,
            Quoting::Necessary,
            Nulls::None,
            b"\\N",
            true,
        )
        .unwrap();
        assert_eq!(hybrid_plain, b"\\N");

        let field = b"#\x00\x08\n\r\t\x1A\\,\"";
        let mut mysql = Vec::new();
        write_unquoted_escaped_field(&mut mysql, mysql_dialect, b'\\', field, true);
        let mysql_expected = [
            b'\\', b'#', b'\\', b'0', b'\\', b'b', b'\\', b'n', b'\\', b'r', b'\\', b't', b'\\',
            b'Z', b'\\', b'\\', b'\\', b',', b'\\', b'"',
        ];
        assert_eq!(mysql, mysql_expected);

        let python_dialect = Dialect {
            escape: Escape::Unquoted(b'\\'),
            comment: Some(b'#'),
            ..Dialect::CSV
        };
        let mut python = Vec::new();
        write_unquoted_escaped_field(&mut python, python_dialect, b'\\', field, true);
        let python_expected = [
            b'\\', b'#', 0, b'\x08', b'\\', b'\n', b'\r', b'\t', b'\x1A', b'\\', b'\\', b'\\',
            b',', b'\\', b'"',
        ];
        assert_eq!(python, python_expected);

        let mut bom = Vec::new();
        write_unquoted_escaped_field(&mut bom, python_dialect, b'\\', b"\xEF\xBB\xBFvalue", true);
        assert_eq!(bom, b"\\\xEF\xBB\xBFvalue");

        let mut null = Vec::new();
        write_null(&mut null, Nulls::PostgresCsv);
        assert!(null.is_empty());
        write_null(&mut null, Nulls::Mysql);
        assert_eq!(null, b"\\N");
    }

    #[test]
    fn quoted_writers_escape_leading_consecutive_and_embedded_bytes() {
        assert_eq!(EscapeMatch::Leading.index(), 0);
        assert_eq!(EscapeMatch::Later(3).index(), 3);
        assert_eq!(
            next_doubled_quote(Dialect::CSV, b"\"a"),
            Some(EscapeMatch::Leading)
        );
        assert_eq!(
            next_doubled_quote(Dialect::CSV, b"a\""),
            Some(EscapeMatch::Later(1))
        );
        assert_eq!(next_doubled_quote(Dialect::CSV, b"ab"), None);

        let mut csv = Vec::new();
        super::write_csv_necessary(&mut csv, b"\"a\"\"b", true);
        assert_eq!(csv, b"\"\"\"a\"\"\"\"b\"");

        let mut doubled = Vec::new();
        super::write_quoted(&mut doubled, Dialect::CSV, b"\"a\"\"b");
        assert_eq!(doubled, b"\"\"\"a\"\"\"\"b\"");

        let backslash = Dialect {
            escape: Escape::Backslash(b'\\'),
            ..Dialect::CSV
        };
        assert_eq!(
            next_backslash_escape(backslash, b'\\', b"\"a"),
            Some(EscapeMatch::Leading)
        );
        assert_eq!(
            next_backslash_escape(backslash, b'\\', b"\\a"),
            Some(EscapeMatch::Leading)
        );
        assert_eq!(
            next_backslash_escape(backslash, b'\\', b"a\\"),
            Some(EscapeMatch::Later(1))
        );
        let mut escaped = Vec::new();
        super::write_quoted(&mut escaped, backslash, b"\"a\\b\"c");
        assert_eq!(escaped, b"\"\\\"a\\\\b\\\"c\"");

        let mut unquoted = Vec::new();
        write_unquoted(&mut unquoted, Dialect::CSV, b"plain", true).unwrap();
        assert_eq!(unquoted, b"plain");
        assert_eq!(
            write_unquoted(&mut unquoted, Dialect::CSV, b"not,plain", false)
                .expect_err("comma requires quoting")
                .to_string(),
            "field requires quoting"
        );
    }

    #[test]
    fn separator_writers_avoid_empty_extensions_and_emit_multibyte_tails() {
        let mut traced = TracingSink::default();
        let count = emit_record_runtime(
            &mut traced,
            Dialect::TSV,
            Quoting::Raw,
            [b"a".as_slice(), b"b"],
        )
        .unwrap();
        assert_eq!(count, 2);
        assert_eq!(traced.bytes, b"a\tb\n");
        assert_eq!(traced.empty_extends, 0);

        #[cfg(feature = "multibyte")]
        {
            let dialect = Dialect {
                delimiter: b'|',
                record_ending: RecordEnding::Byte(b'<'),
                delimiter_tail: Tail::of(b"||"),
                ending_tail: Tail::of(b"<E>"),
                ..Dialect::CSV
            };
            let (output, count) = emitted_record(dialect, Quoting::Raw, &[b"a".as_slice(), b"b"]);
            assert_eq!(count, 2);
            assert_eq!(output, b"a||b<E>");
        }
    }

    #[test]
    fn record_field_writer_and_field_count_validation_cover_every_policy() {
        assert_eq!(necessary_path(Dialect::CSV), NecessaryPath::Csv);
        assert_eq!(necessary_path(Dialect::TSV), NecessaryPath::General);
        assert_eq!(
            configured_path(Dialect::CSV, Nulls::None),
            ConfiguredPath::Plain
        );
        assert_eq!(
            configured_path(Dialect::CSV, Nulls::Mysql),
            ConfiguredPath::Configured
        );
        assert_eq!(
            configured_path(Dialect::MYSQL, Nulls::None),
            ConfiguredPath::Configured
        );

        let cases: &[(Quoting, &[u8], bool, &[u8])] = &[
            (Quoting::Necessary, b"a,b", true, b"\"a,b\""),
            (Quoting::NonNumeric, b"text", false, b"\"text\""),
            (Quoting::Always, b"a\"b", false, b"\"a\"\"b\""),
            (Quoting::Never, b"plain", false, b"plain"),
            (Quoting::Raw, b"a,b", false, b"a,b"),
        ];
        for &(quoting, field, is_first, expected) in cases {
            let mut output = Vec::new();
            write_record_field(&mut output, Dialect::CSV, quoting, field, is_first).unwrap();
            assert_eq!(output, expected, "{quoting:?}");
        }

        let mut expected = None;
        validate_field_count(FieldCount::MatchFirst, &mut expected, 2).unwrap();
        assert_eq!(expected, Some(2));
        validate_field_count(FieldCount::MatchFirst, &mut expected, 2).unwrap();
        let error = validate_field_count(FieldCount::MatchFirst, &mut expected, 3)
            .expect_err("later records must match the first");
        assert_eq!(
            error.kind(),
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 3,
            }
        );
        validate_field_count(FieldCount::Flexible, &mut expected, 99).unwrap();
        validate_field_count(FieldCount::Exact(4), &mut expected, 4).unwrap();
    }

    #[test]
    fn numeric_scan_classification_and_quoting_boundaries_are_exact() {
        for field in [b"0".as_slice(), b"+.5", b"1.", b"-12.50"] {
            assert!(
                matches!(scan_numeric(field), NumericScan::Numeric),
                "{field:?}"
            );
        }
        for field in [b"1e3".as_slice(), b"1..2", b"nan", b"infinity"] {
            assert!(
                matches!(scan_numeric(field), NumericScan::Fallback),
                "{field:?}"
            );
        }
        for field in [b"".as_slice(), b"+", b"-", b".", b"12 3"] {
            assert!(
                matches!(scan_numeric(field), NumericScan::NotNumeric),
                "{field:?}"
            );
        }

        let safe_short = [b'a'; super::SIMD_QUOTING_SCAN_BYTES - 1];
        let safe_long = [b'a'; super::SIMD_QUOTING_SCAN_BYTES];
        assert_eq!(quoting_scan(&safe_short), QuotingScan::Words);
        assert_eq!(quoting_scan(&safe_long), QuotingScan::Blocks);
        assert!(!needs_quotes(Dialect::CSV, &safe_short, false));
        assert!(!needs_quotes(Dialect::CSV, &safe_long, false));
        assert!(!super::needs_csv_quotes(&safe_short, false));
        assert!(!super::needs_csv_quotes(&safe_long, false));
        assert!(!needs_quotes_words(b',', b'"', b'\n', true, &safe_short));
        assert!(!needs_quotes_blocks(b',', b'"', b'\n', true, &safe_long));

        for special in [b',', b'"', b'\n', b'\r'] {
            let mut short = safe_short;
            short[short.len() / 2] = special;
            assert!(needs_quotes(Dialect::CSV, &short, false), "{special:?}");
            assert!(super::needs_csv_quotes(&short, false), "{special:?}");

            let mut long = safe_long;
            long[long.len() - 1] = special;
            assert!(needs_quotes(Dialect::CSV, &long, false), "{special:?}");
            assert!(super::needs_csv_quotes(&long, false), "{special:?}");
        }

        for harmless_near_quote in [b'!', b'#'] {
            let mut field = safe_long;
            field[0] = harmless_near_quote;
            assert!(!needs_quotes(Dialect::CSV, &field, false));
        }

        let custom_ending = Dialect {
            record_ending: RecordEnding::Byte(b';'),
            ..Dialect::CSV
        };
        let mut short_cr = safe_short;
        short_cr[3] = b'\r';
        assert!(!needs_quotes(custom_ending, &short_cr, false));
        let mut long_cr = safe_long;
        long_cr[17] = b'\r';
        assert!(!needs_quotes(custom_ending, &long_cr, false));
        long_cr[17] = b';';
        assert!(needs_quotes(custom_ending, &long_cr, false));

        assert!(super::needs_csv_quotes(b"\xEF\xBB\xBFvalue", true));
        assert!(!super::needs_csv_quotes(b"\xEF\xBB\xBFvalue", false));

        assert_eq!(super::repeated_byte(0x55), 0x5555555555555555);
        assert_ne!(super::zero_byte_mask(0), 0);
    }

    #[cfg(feature = "benchmarking")]
    #[test]
    fn benchmarking_helpers_preserve_first_field_and_capacity_boundaries() {
        assert!(!super::benchmark_needs_quotes(
            RecordEnding::Newline,
            b"\xEF\xBB\xBFvalue"
        ));
        assert_eq!(super::benchmark_quoted_capacity(b""), 2);
        assert_eq!(super::benchmark_quoted_capacity(b"abc"), 8);
        assert_eq!(super::benchmark_escape_double_quote(b"a\"b", 2), 12);
    }

    #[test]
    fn the_oversized_record_error_is_an_encode_failure() {
        // The guard that raises this needs a record whose encoded size
        // overflows `usize`, which no test can allocate, so the constructor
        // is checked directly instead.
        assert_eq!(record_too_large().kind(), ErrorKind::Encode);
    }

    /// The exact predicate `is_numeric` replaced, kept here only as the
    /// oracle the byte scan is checked against. Any drift between this and
    /// `is_numeric` changes which fields `Quoting::NonNumeric` quotes.
    fn reference_is_numeric(field: &[u8]) -> bool {
        !field.is_empty()
            && core::str::from_utf8(field).is_ok_and(|field| field.parse::<f64>().is_ok())
    }

    /// Every awkward form `f64::from_str` is documented to accept or reject —
    /// leading `+`, `inf`/`infinity`/`nan` case-insensitively, exponents, a
    /// leading or trailing decimal point, empty, whitespace-padded, non-ASCII,
    /// and a digit string too large to represent finitely — must classify
    /// identically under the byte scan and the reference parse. This is the
    /// scan's fidelity requirement: it changes which fields get quoted if it
    /// ever disagrees.
    #[test]
    fn byte_scan_agrees_with_the_reference_parse_on_awkward_forms() {
        let cases: &[&[u8]] = &[
            b"",
            b"0",
            b"123",
            b"-123",
            b"+123",
            b"123.456",
            b"-123.456",
            b"+123.456",
            b"1.",
            b".5",
            b"-.5",
            b"+.5",
            b".",
            b"-.",
            b"+",
            b"-",
            b"1..2",
            b"1.2.3",
            b"1e10",
            b"1E10",
            b"1e-10",
            b"1e+10",
            b"-1e10",
            b"1e999",
            b"1e",
            b"e10",
            b"inf",
            b"Inf",
            b"INF",
            b"infinity",
            b"Infinity",
            b"INFINITY",
            b"-inf",
            b"+infinity",
            b"nan",
            b"NaN",
            b"NAN",
            b"-nan",
            b"infinityx",
            b"nanx",
            b"na",
            b" 123",
            b"123 ",
            b" 123 ",
            b"\t123",
            b"123\n",
            b"12_3",
            b"0x1p3",
            b"12,345",
            b"1,000.5",
            b"Boston",
            b"true",
            b"false",
            b"null",
            b"NULL",
            b"a",
            b"\xE2\x82\xAC", // "€", multi-byte UTF-8
            b"1\xE2\x82\xAC2",
            b"\xFF\xFE", // invalid UTF-8
            b"123\x00456",
            b"00000",
            b"00000.00000",
            &[b'9'; 400],
            &[b'0'; 400],
        ];
        for field in cases {
            assert_eq!(
                is_numeric(field),
                reference_is_numeric(field),
                "byte scan disagreed with the reference parse on {field:?}"
            );
        }
    }

    /// A differential property over generated byte strings, biased toward the
    /// alphabet a float literal is built from so the generator does not spend
    /// its whole budget on inputs the scan trivially rejects. `is_numeric`
    /// must agree with `reference_is_numeric` on every input bolero produces.
    #[test]
    fn byte_scan_agrees_with_the_reference_parse_under_fuzzing() {
        const ALPHABET: &[u8] = b"0123456789+-.eEiInNfFaAyY xX_,\0";

        bolero::check!()
            .with_generator(bolero::generator::produce::<Vec<u8>>())
            .for_each(|indices: &Vec<u8>| {
                let field: Vec<u8> = indices
                    .iter()
                    .map(|&index| ALPHABET[index as usize % ALPHABET.len()])
                    .collect();
                assert_eq!(
                    is_numeric(&field),
                    reference_is_numeric(&field),
                    "byte scan disagreed with the reference parse on {field:?}"
                );
            });
    }
}

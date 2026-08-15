use std::io::{self, Read, Seek, SeekFrom};

use crate::IoParser;
use crate::config::{
    BlankRecords, Dialect, Escape, FormatOptions, Headers, Limits, Nulls, ParseOptions, Quoting,
    ReadBom, RecordEnding, Recovery, Syntax, Tail, Whitespace, WriteBom,
};
use crate::error::{Error, ErrorKind, Location};
use xxhash_rust::xxh3::{Xxh3, xxh3_128};

pub(super) const MAGIC: &[u8; 8] = b"BCSVIDX2";
/// Version 8 authenticates the payload with one checksum computed by hashing
/// the header followed by every entry, in file order, which can only be
/// computed once the whole payload exists to be reread. Version 9 keeps the
/// same header and entry layout but authenticates the entries and the header
/// as two independent checksums: the entries checksum is accumulated while
/// entries are written (or streamed back in for verification), and the
/// header checksum is a plain one-shot hash of the finalized header, which is
/// already held in memory. Neither needs the payload reread. Both versions
/// remain readable; only version 9 is ever written.
pub(super) const VERSION: u32 = 9;
pub(super) const MIN_VERSION: u32 = 8;
pub(super) const HASH_BYTES: usize = 16;
/// The encoded format: 17 bytes of single-byte dialect and options, then a
/// length and up to `Tail::MAX` bytes for each of the two separator tails.
const FORMAT_BYTES: usize = 17 + 2 * (1 + Tail::MAX);
pub(super) const FIXED_HEADER_BYTES: usize = 8 + 4 + 8 + HASH_BYTES + FORMAT_BYTES + 24 + 8;
/// Width of one stored checksum. Version 8 trails one of these; version 9
/// trails two (the entries checksum, then the header checksum).
pub(super) const CHECKSUM_BYTES: usize = HASH_BYTES;
pub(super) const INDEX_LENGTH_OVERFLOW: &str = "index length overflows u64";
pub(super) const RECORD_OFFSET_EXCEEDS_SOURCE: &str = "record offset exceeds source";
pub(super) const RECORD_OFFSETS_NOT_INCREASING: &str = "record offsets are not strictly increasing";
pub(super) const RECORD_LINES_INVALID: &str = "record lines are not valid and nondecreasing";
pub(super) const INDEX_TRUNCATED: &str = "index is truncated";
pub(super) const INDEX_MAGIC_MISMATCH: &str = "index magic does not match";
pub(super) const UNSUPPORTED_INDEX_VERSION: &str = "unsupported index version";
pub(super) const READ_OVERRAN_BUFFER: &str =
    "Read implementation returned more bytes than the buffer holds";
pub(super) const SOURCE_LENGTH_OVERFLOW: &str = "source length exceeds u64";
pub(super) const RECORD_LIMIT_NOT_USIZE: &str = "record limit does not fit usize";
pub(super) const FIELD_LIMIT_NOT_USIZE: &str = "field limit does not fit usize";
pub(super) const FIELD_COUNT_LIMIT_NOT_USIZE: &str = "field-count limit does not fit usize";
pub(super) const INDEX_CURSOR_OVERFLOW: &str = "index cursor overflow";
pub(super) const LOCATION_TABLE_LENGTH_MISMATCH: &str = "location table length does not match";
pub(super) const INDEX_CHECKSUM_MISMATCH: &str = "index checksum does not match";
pub(super) const INDEX_ENTRIES_CHECKSUM_MISMATCH: &str = "index entries checksum does not match";
pub(super) const INDEX_HEADER_CHECKSUM_MISMATCH: &str = "index header checksum does not match";
pub(super) const RECORD_COUNT_NOT_USIZE: &str = "record count does not fit usize";
pub(super) const TOO_MANY_RECORDS: &str = "too many records";
pub(super) const PARALLEL_FORMAT_UNSUPPORTED: &str = "this format cannot be indexed in parallel";
pub(super) const ENCODED_VALUE_NO_RECORD: &str =
    "encoded value does not produce a parser-visible record";
pub(super) const ENCODED_VALUE_MULTIPLE_RECORDS: &str =
    "encoded value produces more than one parser-visible record";
pub(super) const WRITE_OVERRAN_BUFFER: &str =
    "Write implementation reported more bytes than the buffer holds";
pub(super) const OUTPUT_LENGTH_OVERFLOW: &str = "output length exceeds u64";

/// Byte length of the trailer following the payload for a given format
/// version: one checksum for version 8, two independent checksums for
/// version 9.
pub(super) const fn trailer_bytes(version: u32) -> usize {
    if version < 9 {
        CHECKSUM_BYTES
    } else {
        2 * CHECKSUM_BYTES
    }
}

pub(super) fn record_out_of_range(record: usize) -> Error {
    Error::new(ErrorKind::RecordOutOfRange { record }, Location::START)
}

/// Widen a `usize` to the `u64` the index format stores it in.
///
/// `usize` is never wider than `u64` on any target this crate builds for, so
#[inline]
pub(super) const fn widen(value: usize) -> u64 {
    value as u64
}

/// Narrow a `u64` read out of an index file to a `usize`.
///
/// The value is untrusted, so on a 32-bit host this genuinely rejects a file
/// written on a 64-bit one. On a 64-bit host `usize` is as wide as `u64` and
/// the failure arm cannot be reached, so it is excluded from coverage.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn narrow(value: u64, what: &'static str) -> Result<usize, Error> {
    usize::try_from(value).map_err(|_error| Error::detailed(ErrorKind::InvalidIndex, what))
}

/// Decoded fixed header shared by eager and lazy index readers.
pub(super) struct IndexHeader {
    pub(super) version: u32,
    pub(super) source_len: u64,
    pub(super) source_hash: [u8; HASH_BYTES],
    pub(super) format: FormatOptions,
    pub(super) limits: Limits,
    pub(super) count: u64,
}

pub(super) fn io_parser_at<R: Read + Seek>(
    source: R,
    source_len: u64,
    format: FormatOptions,
    limits: Limits,
    location: Location,
) -> Result<IoParser<R>, Error> {
    let mut source = source;
    let len = source.seek(SeekFrom::End(0)).map_err(Error::io_at_start)?;
    if len != source_len {
        return Err(Error::new(ErrorKind::SourceMismatch, Location::START));
    }
    // Header and field-width discovery run against the start of the stream,
    // exactly as they do for a slice parser that is seeked afterwards.
    source.rewind().map_err(Error::io_at_start)?;
    let mut reader = IoParser::with_options(
        source,
        format.read_bom(ReadBom::Preserve),
        ParseOptions::new().limits(limits).headers(Headers::None),
    )?;
    reader.seek(location)?;
    Ok(reader)
}

/// Turn one stored position into an absolute parser location.
pub(super) fn entry_location(
    offset: u64,
    line: u64,
    source_len: u64,
    record: usize,
) -> Result<Location, Error> {
    if offset >= source_len {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            RECORD_OFFSET_EXCEEDS_SOURCE,
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "offset is validated < source_len"
    )]
    let byte = offset as usize;
    Ok(Location {
        byte,
        line,
        record: record as u64,
        field: 0,
    })
}

/// Reject stored positions that cannot describe a record of the source.
pub(super) fn check_entry(
    offset: u64,
    line: u64,
    source_len: u64,
    previous_offset: Option<u64>,
    previous_line: Option<u64>,
) -> Result<(), Error> {
    check_entry_bounds(offset, line, source_len)?;
    if previous_offset.is_some_and(|previous| offset <= previous) {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            RECORD_OFFSETS_NOT_INCREASING,
        ));
    }
    if previous_line.is_some_and(|previous| line < previous) {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            RECORD_LINES_INVALID,
        ));
    }
    Ok(())
}

/// Reject a single lazily read entry without inventing a predecessor.
pub(super) fn check_first_entry(offset: u64, line: u64, source_len: u64) -> Result<(), Error> {
    check_entry_bounds(offset, line, source_len)
}

fn check_entry_bounds(offset: u64, line: u64, source_len: u64) -> Result<(), Error> {
    if offset >= source_len {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            RECORD_OFFSET_EXCEEDS_SOURCE,
        ));
    }
    if line == 0 {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            RECORD_LINES_INVALID,
        ));
    }
    Ok(())
}

/// Byte length of the checksummed part of an index holding `count` records.
pub(super) fn payload_len(count: u64) -> Result<u64, Error> {
    count
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(FIXED_HEADER_BYTES as u64))
        .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_LENGTH_OVERFLOW))
}

/// Feed `len` bytes read from `index` into `hasher`, without holding them in
/// memory. Used both for a one-shot digest (`hash_payload`) and to fold a
/// streamed region into a hasher that a caller finishes with more updates.
fn update_hash(index: impl Read, len: u64, hasher: &mut Xxh3) -> Result<(), Error> {
    let mut reader = index.take(len);
    let mut buffer = [u8::default(); 8192];
    let mut seen = 0u64;
    loop {
        let read = match reader.read(&mut buffer) {
            // gamma::skip(loop.break_to_continue, reason = "a reader returning EOF would spin forever")
            Ok(0) => break,
            Ok(read) => read,
            // gamma::skip(match_guard.always_true, match_guard.negate, relational.eq_to_ne, reason = "retrying every persistent I/O error would spin forever")
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::io(error, Location::START)),
        };
        hasher.update(&buffer[..read]);
        seen += read as u64;
    }
    if seen != len {
        return Err(Error::detailed(ErrorKind::InvalidIndex, INDEX_TRUNCATED));
    }
    Ok(())
}

/// Hash `len` bytes of an index without holding them in memory.
pub(super) fn hash_payload(index: impl Read, len: u64) -> Result<[u8; CHECKSUM_BYTES], Error> {
    let mut hasher = Xxh3::new();
    update_hash(index, len, &mut hasher)?;
    Ok(hasher.digest128().to_le_bytes())
}

/// Hash the `count`-record entry table, reading it from the current position
/// without holding it in memory.
pub(super) fn hash_entries(index: impl Read, count: u64) -> Result<[u8; CHECKSUM_BYTES], Error> {
    let entries_len = count
        .checked_mul(16)
        .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_LENGTH_OVERFLOW))?;
    hash_payload(index, entries_len)
}

/// Confirm that a streamed source matches a recorded length and hash.
pub(super) fn validate_identity(
    source: impl Read,
    source_len: u64,
    source_hash: [u8; HASH_BYTES],
) -> Result<(), Error> {
    let mut reader = HashingReader::new(source);
    io::copy(&mut reader, &mut io::sink()).map_err(Error::io_at_start)?;
    if reader.len != source_len || reader.hasher.digest128().to_le_bytes() != source_hash {
        return Err(Error::new(ErrorKind::SourceMismatch, Location::START));
    }
    Ok(())
}

/// Seek within an index, reporting failures as index I/O errors.
pub(super) fn index_seek(index: &mut impl Seek, position: SeekFrom) -> Result<u64, Error> {
    index.seek(position).map_err(Error::io_at_start)
}

/// Encode the fixed index header.
pub(super) fn encode_header(
    source_len: u64,
    source_hash: [u8; HASH_BYTES],
    format: FormatOptions,
    limits: Limits,
    count: u64,
) -> Vec<u8> {
    let mut output = Vec::with_capacity(FIXED_HEADER_BYTES);
    output.extend_from_slice(MAGIC);
    output.extend_from_slice(&VERSION.to_le_bytes());
    output.extend_from_slice(&source_len.to_le_bytes());
    output.extend_from_slice(&source_hash);
    encode_format(&mut output, format);
    output.extend_from_slice(&(limits.max_record_bytes as u64).to_le_bytes());
    output.extend_from_slice(&(limits.max_field_bytes as u64).to_le_bytes());
    output.extend_from_slice(&(limits.max_fields as u64).to_le_bytes());
    output.extend_from_slice(&count.to_le_bytes());
    debug_assert_eq!(output.len(), FIXED_HEADER_BYTES);
    output
}

/// Decode the fixed index header.
#[cfg_attr(coverage_nightly, coverage(off))]
pub(super) fn decode_header(header: &[u8; FIXED_HEADER_BYTES]) -> Result<IndexHeader, Error> {
    let mut cursor = Cursor::new(header);
    if cursor.take(8)? != MAGIC {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            INDEX_MAGIC_MISMATCH,
        ));
    }
    let version = cursor.u32()?;
    if !(MIN_VERSION..=VERSION).contains(&version) {
        return Err(Error::detailed(
            ErrorKind::InvalidIndex,
            UNSUPPORTED_INDEX_VERSION,
        ));
    }
    let source_len = cursor.u64()?;
    let source_hash = cursor.array_16()?;
    let format = decode_format(&mut cursor)?;
    let limits = decode_limits(&mut cursor)?;
    let count = cursor.u64()?;
    Ok(IndexHeader {
        version,
        source_len,
        source_hash,
        format,
        limits,
        count,
    })
}

/// Record one record position in both position tables.
pub(super) fn push_position(
    offsets: &mut Vec<u64>,
    lines: &mut Vec<u64>,
    offset: usize,
    line: u64,
) {
    offsets.push(offset as u64);
    lines.push(line);
}

pub(super) fn hash_bytes(data: &[u8]) -> [u8; HASH_BYTES] {
    xxh3_128(data).to_le_bytes()
}

pub(super) fn read_index_exact(reader: &mut impl Read, output: &mut [u8]) -> Result<(), Error> {
    reader.read_exact(output).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            Error::detailed(ErrorKind::InvalidIndex, INDEX_TRUNCATED)
        } else {
            Error::io(error, Location::START)
        }
    })
}

pub(super) struct HashingReader<R> {
    inner: R,
    pub(super) hasher: Xxh3,
    pub(super) len: u64,
}

impl<R> HashingReader<R> {
    pub(super) fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Xxh3::new(),
            len: 0,
        }
    }
}

impl<R: Read> Read for HashingReader<R> {
    // gamma::skip(fn_value.ok, reason = "returning a byte without consuming input makes read_exact and io::copy loop forever")
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                READ_OVERRAN_BUFFER,
            ));
        }
        self.len = self
            .len
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, SOURCE_LENGTH_OVERFLOW))?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

fn encode_format(output: &mut Vec<u8>, format: FormatOptions) {
    let dialect = format.dialect;
    output.push(dialect.delimiter());
    output.push(dialect.quote());
    match dialect.record_ending() {
        RecordEnding::Newline => output.extend_from_slice(&[0, 0]),
        RecordEnding::Byte(byte) => output.extend_from_slice(&[1, byte]),
        RecordEnding::CrLf => output.extend_from_slice(&[2, 0]),
    }
    match dialect.escape() {
        Escape::DoubleQuote => output.extend_from_slice(&[0, 0]),
        Escape::Backslash(byte) => output.extend_from_slice(&[1, byte]),
        Escape::Mysql => output.extend_from_slice(&[2, 0]),
        Escape::Unquoted(byte) => output.extend_from_slice(&[3, byte]),
    }
    match dialect.comment() {
        None => output.extend_from_slice(&[0, 0]),
        Some(byte) => output.extend_from_slice(&[1, byte]),
    }
    output.push(format.trim.bits());
    output.push(match format.blank_records {
        BlankRecords::Preserve => 0,
        BlankRecords::Skip => 1,
    });
    output.push(match format.read_bom {
        ReadBom::Detect => 0,
        ReadBom::Preserve => 1,
        ReadBom::Reject => 2,
    });
    output.push(match format.write_bom {
        WriteBom::Omit => 0,
        WriteBom::Emit => 1,
    });
    match format.syntax {
        Syntax::Strict => output.extend_from_slice(&[0, 0]),
        Syntax::Compatible(rules) => output.extend_from_slice(&[1, rules.bits()]),
    }
    output.push(match format.nulls {
        Nulls::None => 0,
        Nulls::PostgresCsv => 1,
        Nulls::Mysql => 2,
    });
    output.push(match format.quoting {
        Quoting::Necessary => 0,
        Quoting::Always => 1,
        Quoting::Never => 2,
        Quoting::NonNumeric => 3,
        Quoting::Raw => 4,
    });
    output.push(u8::from(format.skip_initial_space));
    // Appending tails preserves every existing field offset in the format.
    for tail in [dialect.delimiter_tail(), dialect.ending_tail()] {
        let (len, bytes) = tail.parts();
        output.push(len);
        output.extend_from_slice(&bytes);
    }
}

fn decode_format(cursor: &mut Cursor<'_>) -> Result<FormatOptions, Error> {
    let dialect = decode_dialect(cursor)?;
    let trim = Whitespace::from_bits(cursor.byte()?)
        .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, "unknown trim encoding"))?;
    let blank_records = match cursor.byte()? {
        0 => BlankRecords::Preserve,
        1 => BlankRecords::Skip,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown blank-record encoding",
            ));
        }
    };
    let read_bom = match cursor.byte()? {
        0 => ReadBom::Detect,
        1 => ReadBom::Preserve,
        2 => ReadBom::Reject,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown read BOM encoding",
            ));
        }
    };
    let write_bom = match cursor.byte()? {
        0 => WriteBom::Omit,
        1 => WriteBom::Emit,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown write BOM encoding",
            ));
        }
    };
    let syntax = match (cursor.byte()?, cursor.byte()?) {
        (0, _) => Syntax::Strict,
        (1, flags) => Syntax::Compatible(Recovery::from_bits(flags).ok_or_else(|| {
            Error::detailed(ErrorKind::InvalidIndex, "unknown recovery rule encoding")
        })?),
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown syntax mode encoding",
            ));
        }
    };
    let nulls = match cursor.byte()? {
        0 => Nulls::None,
        1 => Nulls::PostgresCsv,
        2 => Nulls::Mysql,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown NULL policy encoding",
            ));
        }
    };
    let quoting = match cursor.byte()? {
        0 => Quoting::Necessary,
        1 => Quoting::Always,
        2 => Quoting::Never,
        3 => Quoting::NonNumeric,
        4 => Quoting::Raw,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown quote policy encoding",
            ));
        }
    };
    let skip_initial_space = match cursor.byte()? {
        0 => false,
        1 => true,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown initial-space encoding",
            ));
        }
    };
    let mut tails = [Tail::EMPTY; 2];
    for tail in &mut tails {
        let len = cursor.byte()?;
        let mut bytes = [u8::default(); Tail::MAX];
        for byte in &mut bytes {
            *byte = cursor.byte()?;
        }
        *tail = Tail::from_parts(len, bytes);
    }
    let dialect = dialect.with_tails(tails[0], tails[1])?;
    Ok(FormatOptions::from_dialect(dialect)
        .trim(trim)
        .blank_records(blank_records)
        .read_bom(read_bom)
        .write_bom(write_bom)
        .syntax(syntax)
        .nulls(nulls)
        .quoting(quoting)
        .skip_initial_space(skip_initial_space))
}

fn decode_dialect(cursor: &mut Cursor<'_>) -> Result<Dialect, Error> {
    let delimiter = cursor.byte()?;
    let quote = cursor.byte()?;
    let record_ending = match (cursor.byte()?, cursor.byte()?) {
        (0, _) => RecordEnding::Newline,
        (1, byte) => RecordEnding::Byte(byte),
        (2, _) => RecordEnding::CrLf,
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown record_ending encoding",
            ));
        }
    };
    let escape = match (cursor.byte()?, cursor.byte()?) {
        (0, _) => Escape::DoubleQuote,
        (1, byte) => Escape::Backslash(byte),
        (2, _) => Escape::Mysql,
        (3, byte) => Escape::Unquoted(byte),
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown escape encoding",
            ));
        }
    };
    let comment = match (cursor.byte()?, cursor.byte()?) {
        (0, _) => None,
        (1, byte) => Some(byte),
        _ => {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                "unknown comment encoding",
            ));
        }
    };
    let dialect = Dialect::new(delimiter, quote, record_ending, escape)?;
    comment.map_or(Ok(dialect), |comment| dialect.with_comment(comment))
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn decode_limits(cursor: &mut Cursor<'_>) -> Result<Limits, Error> {
    let max_record_bytes = narrow(cursor.u64()?, RECORD_LIMIT_NOT_USIZE)?;
    let max_field_bytes = narrow(cursor.u64()?, FIELD_LIMIT_NOT_USIZE)?;
    let max_fields = narrow(cursor.u64()?, FIELD_COUNT_LIMIT_NOT_USIZE)?;
    Ok(Limits::new(max_record_bytes, max_field_bytes, max_fields))
}

struct Cursor<'bytes> {
    bytes: &'bytes [u8],
    location: usize,
}

#[cfg_attr(coverage_nightly, coverage(off))]
impl<'bytes> Cursor<'bytes> {
    const fn new(bytes: &'bytes [u8]) -> Self {
        Self { bytes, location: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'bytes [u8], Error> {
        let end = self
            .location
            .checked_add(len)
            .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_CURSOR_OVERFLOW))?;
        let bytes = self
            .bytes
            .get(self.location..end)
            .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_TRUNCATED))?;
        self.location = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let mut bytes = [u8::default(); 4];
        bytes.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let mut bytes = [u8::default(); 8];
        bytes.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(bytes))
    }

    fn array_16(&mut self) -> Result<[u8; HASH_BYTES], Error> {
        let mut bytes = [u8::default(); HASH_BYTES];
        bytes.copy_from_slice(self.take(HASH_BYTES)?);
        Ok(bytes)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind as IoErrorKind;

    /// Yields `Interrupted` before each of its chunks, the way a reader
    /// interrupted by a signal does.
    struct InterruptingReader {
        chunks: Vec<&'static [u8]>,
        interrupt_next: bool,
    }

    impl Read for InterruptingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(std::io::Error::from(IoErrorKind::Interrupted));
            }
            let Some(chunk) = self.chunks.first() else {
                return Ok(0);
            };
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            if n == chunk.len() {
                self.chunks.remove(0);
                self.interrupt_next = true;
            } else {
                self.chunks[0] = &chunk[n..];
            }
            Ok(n)
        }
    }

    /// `Interrupted` is not a failure — the read simply has to be reissued.
    /// Treating it as one would make a signal during index verification look
    /// like a corrupt index, so the digest must come out identical to the one
    /// an uninterrupted reader produces over the same bytes.
    #[test]
    fn an_interrupted_read_is_retried_rather_than_reported() {
        let payload: &[u8] = b"the quick brown fox jumps over the lazy dog";
        let len = payload.len() as u64;

        let mut expected = Xxh3::new();
        update_hash(payload, len, &mut expected).expect("uninterrupted hash");

        let reader = InterruptingReader {
            chunks: vec![&payload[..10], &payload[10..25], &payload[25..]],
            interrupt_next: true,
        };
        let mut actual = Xxh3::new();
        update_hash(reader, len, &mut actual).expect("interrupted reads must be retried");

        assert_eq!(actual.digest128(), expected.digest128());
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(IoErrorKind::PermissionDenied))
        }
    }

    /// The counterpart: an error that is *not* `Interrupted` is a genuine
    /// failure and must surface. Retrying it would spin forever on a reader
    /// that keeps failing, turning an unreadable index into a hang.
    #[test]
    fn a_real_io_error_is_reported_rather_than_retried() {
        let mut hasher = Xxh3::new();
        let error =
            update_hash(FailingReader, 64, &mut hasher).expect_err("a real io error must surface");
        assert_eq!(
            error.kind(),
            crate::ErrorKind::Io(IoErrorKind::PermissionDenied)
        );
    }

    #[test]
    fn hashing_reader_rejects_a_running_length_that_overflows_u64() {
        // Reaching this overflow through genuine reads would require
        // streaming past `u64::MAX` bytes, which is not practical to
        // exercise. Constructing the accumulator right at the boundary lets
        // one small read trigger the same `checked_add` failure directly.
        let mut reader = HashingReader::new(&b"12345678"[..]);
        reader.len = u64::MAX - 3;
        let mut buffer = [0; 8];
        let error = reader
            .read(&mut buffer)
            .expect_err("a length overflowing u64 must be rejected");
        assert_eq!(error.kind(), IoErrorKind::InvalidData);
        assert_eq!(error.to_string(), "source length exceeds u64");
    }

    #[test]
    fn index_error_details_are_stable() {
        assert_eq!(INDEX_LENGTH_OVERFLOW, "index length overflows u64");
        assert_eq!(RECORD_OFFSET_EXCEEDS_SOURCE, "record offset exceeds source");
        assert_eq!(
            RECORD_OFFSETS_NOT_INCREASING,
            "record offsets are not strictly increasing"
        );
        assert_eq!(
            RECORD_LINES_INVALID,
            "record lines are not valid and nondecreasing"
        );
        assert_eq!(INDEX_TRUNCATED, "index is truncated");
        assert_eq!(INDEX_MAGIC_MISMATCH, "index magic does not match");
        assert_eq!(UNSUPPORTED_INDEX_VERSION, "unsupported index version");
        assert_eq!(
            READ_OVERRAN_BUFFER,
            "Read implementation returned more bytes than the buffer holds"
        );
        assert_eq!(SOURCE_LENGTH_OVERFLOW, "source length exceeds u64");
        assert_eq!(RECORD_LIMIT_NOT_USIZE, "record limit does not fit usize");
        assert_eq!(FIELD_LIMIT_NOT_USIZE, "field limit does not fit usize");
        assert_eq!(
            FIELD_COUNT_LIMIT_NOT_USIZE,
            "field-count limit does not fit usize"
        );
        assert_eq!(INDEX_CURSOR_OVERFLOW, "index cursor overflow");
        assert_eq!(
            LOCATION_TABLE_LENGTH_MISMATCH,
            "location table length does not match"
        );
        assert_eq!(INDEX_CHECKSUM_MISMATCH, "index checksum does not match");
        assert_eq!(
            INDEX_ENTRIES_CHECKSUM_MISMATCH,
            "index entries checksum does not match"
        );
        assert_eq!(
            INDEX_HEADER_CHECKSUM_MISMATCH,
            "index header checksum does not match"
        );
        assert_eq!(RECORD_COUNT_NOT_USIZE, "record count does not fit usize");
        assert_eq!(TOO_MANY_RECORDS, "too many records");
        assert_eq!(
            PARALLEL_FORMAT_UNSUPPORTED,
            "this format cannot be indexed in parallel"
        );
        assert_eq!(
            ENCODED_VALUE_NO_RECORD,
            "encoded value does not produce a parser-visible record"
        );
        assert_eq!(
            ENCODED_VALUE_MULTIPLE_RECORDS,
            "encoded value produces more than one parser-visible record"
        );
        assert_eq!(
            WRITE_OVERRAN_BUFFER,
            "Write implementation reported more bytes than the buffer holds"
        );
        assert_eq!(OUTPUT_LENGTH_OVERFLOW, "output length exceeds u64");
    }

    #[test]
    fn first_entry_and_location_boundaries_are_exact() {
        check_first_entry(0, 1, 1).expect("the only byte starts a valid record");
        assert!(check_first_entry(1, 1, 1).is_err());
        assert!(check_first_entry(0, 0, 1).is_err());

        let location = entry_location(0, 7, 1, 11).expect("last source byte is addressable");
        assert_eq!(
            location,
            Location {
                byte: 0,
                line: 7,
                record: 11,
                field: 0,
            }
        );
    }

    #[test]
    fn hashing_one_missing_byte_is_truncation() {
        let mut hasher = Xxh3::new();
        let error = update_hash(&b""[..], 1, &mut hasher)
            .expect_err("zero bytes cannot satisfy a one-byte payload");
        assert_eq!(error.kind(), crate::ErrorKind::InvalidIndex);
        assert!(error.to_string().contains("index is truncated"), "{error}");
    }

    #[test]
    fn versions_immediately_outside_the_supported_range_are_rejected() {
        let header = encode_header(1, [3; HASH_BYTES], FormatOptions::CSV, Limits::DEFAULT, 0);
        let mut header: [u8; FIXED_HEADER_BYTES] = header.try_into().expect("fixed header length");

        for version in [MIN_VERSION - 1, VERSION + 1] {
            header[8..12].copy_from_slice(&version.to_le_bytes());
            let error = match decode_header(&header) {
                Ok(_) => panic!("unsupported adjacent version"),
                Err(error) => error,
            };
            assert!(
                error.to_string().contains("unsupported index version"),
                "{error}"
            );
        }
    }

    #[cfg(feature = "multibyte")]
    #[test]
    fn format_round_trip_preserves_every_separator_tail_byte() {
        let format = FormatOptions::CSV
            .delimiter_sequence(b"|~!?")
            .record_ending_sequence(b"@#$%");
        let header = encode_header(17, [9; HASH_BYTES], format, Limits::new(31, 17, 5), 3);
        let header: [u8; FIXED_HEADER_BYTES] = header.try_into().expect("fixed header length");
        let decoded = decode_header(&header).expect("multi-byte header decodes");
        assert_eq!(decoded.format, format);
        assert_eq!(decoded.limits, Limits::new(31, 17, 5));
        assert_eq!(decoded.source_len, 17);
        assert_eq!(decoded.source_hash, [9; HASH_BYTES]);
        assert_eq!(decoded.count, 3);
    }

    #[cfg(target_pointer_width = "32")]
    #[test]
    fn narrow_rejects_offsets_that_do_not_fit_usize_as_invalid_index() {
        let error = super::narrow(
            u64::from(u32::MAX) + 1,
            "record offset does not fit this platform's address width",
        )
        .expect_err("an offset above usize::MAX must be rejected");
        assert_eq!(error.kind(), crate::error::ErrorKind::InvalidIndex);
        assert!(
            error
                .to_string()
                .contains("record offset does not fit this platform's address width"),
            "{error}"
        );
    }

    /// The line guard is the one `check_entry` arm no stored-index test can
    /// reach: swapping two whole entries moves their offsets out of order too,
    /// and the offset check runs first, so it is what rejects them. Calling
    /// `check_entry` directly with a *valid* increasing offset is the only way
    /// to make the line arm the one that fires.
    ///
    /// The entry has no predecessor, which isolates `line == 0` from the
    /// decreasing-line half of the same condition: with a predecessor, a zero
    /// line is also a decreasing one, and either half would reject it.
    #[test]
    fn a_zero_record_line_is_rejected() {
        let error = check_entry(16, 0, 4096, None, None)
            .expect_err("physical lines are numbered from one, so zero is not a valid line");
        assert_eq!(error.kind(), crate::error::ErrorKind::InvalidIndex);
        assert!(
            error
                .to_string()
                .contains("record lines are not valid and nondecreasing"),
            "the line arm must be what rejects this, not the offset arm: {error}"
        );
    }

    /// Nondecreasing rather than increasing: several records share a physical
    /// line when a field contains no record ending, so equal lines are valid
    /// and only a decrease is corruption.
    #[test]
    fn a_decreasing_record_line_is_rejected_but_an_equal_one_is_not() {
        check_entry(16, 7, 4096, Some(0), Some(7))
            .expect("records sharing a physical line are ordinary, not corrupt");

        let error = check_entry(16, 6, 4096, Some(0), Some(7))
            .expect_err("a line before its predecessor's cannot describe a later record");
        assert_eq!(error.kind(), crate::error::ErrorKind::InvalidIndex);
        assert!(
            error
                .to_string()
                .contains("record lines are not valid and nondecreasing"),
            "{error}"
        );

        // offset <= previous
        let error = check_entry(16, 8, 4096, Some(16), Some(7)).expect_err("offset <= previous");
        assert_eq!(error.kind(), crate::error::ErrorKind::InvalidIndex);

        // hash_entries
        let hash = super::hash_entries(&b"1234567890123456"[..], 1).expect("hash_entries");
        assert_eq!(hash.len(), 16);

        // Truncated update_hash
        let mut hasher = Xxh3::new();
        let err = update_hash(&b"short"[..], 100, &mut hasher).expect_err("truncated");
        assert_eq!(err.kind(), crate::error::ErrorKind::InvalidIndex);

        // entry_location
        assert!(super::entry_location(10, 1, 100, 0).is_ok());
        assert!(super::entry_location(100, 1, 10, 0).is_err());

        // io_parser_at with seek error and mismatched source len
        struct FailingSeek {
            seek_fail_count: u32,
        }
        impl Read for FailingSeek {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl std::io::Seek for FailingSeek {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                if self.seek_fail_count == 0 {
                    Err(std::io::Error::from(IoErrorKind::BrokenPipe))
                } else {
                    self.seek_fail_count -= 1;
                    Ok(10)
                }
            }
        }
        let fail_seek = FailingSeek { seek_fail_count: 0 };
        assert!(
            super::io_parser_at(
                fail_seek,
                10,
                FormatOptions::CSV,
                Limits::DEFAULT,
                Location::START
            )
            .is_err()
        );

        let fail_second_seek = FailingSeek { seek_fail_count: 1 };
        assert!(
            super::io_parser_at(
                fail_second_seek,
                10,
                FormatOptions::CSV,
                Limits::DEFAULT,
                Location::START
            )
            .is_err()
        );

        let mismatch_seek = FailingSeek { seek_fail_count: 5 };
        assert!(
            super::io_parser_at(
                mismatch_seek,
                20,
                FormatOptions::CSV,
                Limits::DEFAULT,
                Location::START
            )
            .is_err()
        );

        // Header decode error coverage for magic, version, format options, limits, etc.
        let valid_hdr = super::encode_header(100, [1; 16], FormatOptions::CSV, Limits::DEFAULT, 5);
        let mut hdr_bytes: [u8; super::FIXED_HEADER_BYTES] = [0; super::FIXED_HEADER_BYTES];
        hdr_bytes.copy_from_slice(&valid_hdr);
        assert!(super::decode_header(&hdr_bytes).is_ok());

        // Corrupted magic
        let mut bad_magic = hdr_bytes;
        bad_magic[0] = b'X';
        assert!(super::decode_header(&bad_magic).is_err());

        // Corrupted version
        let mut bad_ver = hdr_bytes;
        bad_ver[8] = 0; // version 0 (unsupported)
        assert!(super::decode_header(&bad_ver).is_err());

        // Test individual decode_format error branches
        let test_cursor = |bytes: &[u8]| {
            let mut cursor = super::Cursor::new(bytes);
            let _ = super::decode_format(&mut cursor);
        };

        // Test decode_dialect variants and errors:
        // Dialect bytes: [delimiter, quote, record_ending tag, record_ending byte, escape tag, escape byte, comment tag, comment byte]
        // Valid dialect: [b',', b'"', 0, 0, 0, 0, 0, 0]
        let test_dialect = |bytes: &[u8]| {
            let mut cursor = super::Cursor::new(bytes);
            super::decode_dialect(&mut cursor)
        };
        // Record ending invalid tag
        assert!(test_dialect(&[b',', b'"', 99, 0, 0, 0, 0, 0]).is_err());
        // Escape invalid tag
        assert!(test_dialect(&[b',', b'"', 0, 0, 99, 0, 0, 0]).is_err());
        // Comment invalid tag
        assert!(test_dialect(&[b',', b'"', 0, 0, 0, 0, 99, 0]).is_err());
        // Invalid delimiter/quote (same byte)
        assert!(test_dialect(&[b',', b',', 0, 0, 0, 0, 0, 0]).is_err());
        // Invalid comment equals delimiter
        assert!(test_dialect(&[b',', b'"', 0, 0, 0, 0, 1, b',']).is_err());

        // Valid base dialect prefix for format test:
        // [b',', b'"', 0, 0, 0, 0, 0, 0]
        // Then:
        // trim: byte (valid: 0)
        // blank_records: byte (0 or 1, invalid: 99)
        // read_bom: byte (0..=2, invalid: 99)
        // write_bom: byte (0..=1, invalid: 99)
        // syntax: (0, 0) or (1, flags), invalid: (99, 0) or (1, 99)
        // nulls: byte (0..=2, invalid: 99)
        // quoting: byte (0..=4, invalid: 99)
        // skip_initial_space: byte (0 or 1, invalid: 99)
        // tails: 2 * (len + 7 bytes)

        let base_fmt = vec![
            b',', b'"', 0, 0, 0, 0, 0, 0, // dialect (8 bytes)
            0, // trim
            0, // blank_records
            0, // read_bom
            0, // write_bom
            0, 0, // syntax
            0, // nulls
            0, // quoting
            0, // skip_initial_space
            0, 0, 0, 0, 0, 0, 0, 0, // tail 1
            0, 0, 0, 0, 0, 0, 0, 0, // tail 2
        ];

        // Invalid trim
        let mut b = base_fmt.clone();
        b[8] = 0xFF;
        test_cursor(&b);
        // Invalid blank_records
        let mut b = base_fmt.clone();
        b[9] = 99;
        test_cursor(&b);
        // Invalid read_bom
        let mut b = base_fmt.clone();
        b[10] = 99;
        test_cursor(&b);
        // Invalid write_bom
        let mut b = base_fmt.clone();
        b[11] = 99;
        test_cursor(&b);
        // Invalid syntax tag
        let mut b = base_fmt.clone();
        b[12] = 99;
        test_cursor(&b);
        // Invalid syntax flags
        let mut b = base_fmt.clone();
        b[12] = 1;
        b[13] = 0xFF;
        test_cursor(&b);
        // Invalid nulls
        let mut b = base_fmt.clone();
        b[14] = 99;
        test_cursor(&b);
        // Invalid quoting
        let mut b = base_fmt.clone();
        b[15] = 99;
        test_cursor(&b);
        // Invalid skip_initial_space
        let mut b = base_fmt.clone();
        b[16] = 99;
        test_cursor(&b);
        // Invalid tail (too long)
        let mut b = base_fmt.clone();
        b[17] = 255;
        test_cursor(&b);

        // Test Cursor methods and error branches
        let mut short_cursor = super::Cursor::new(&[1, 2]);
        assert_eq!(short_cursor.byte().unwrap(), 1);
        assert!(short_cursor.u32().is_err());
        assert!(short_cursor.u64().is_err());
        assert!(short_cursor.array_16().is_err());
        let mut overflow_cursor = super::Cursor::new(&[1, 2]);
        assert!(overflow_cursor.take(usize::MAX).is_err());

        // Test truncated prefixes on decode_format
        for len in 0..base_fmt.len() {
            let mut c = super::Cursor::new(&base_fmt[..len]);
            let _ = super::decode_format(&mut c);
        }

        // Test io_parser_at with invalid location (field != 0)
        let source_cursor = std::io::Cursor::new(b"a,b\n1,2\n");
        assert!(
            super::io_parser_at(
                source_cursor,
                8,
                FormatOptions::CSV,
                Limits::DEFAULT,
                Location {
                    byte: 0,
                    line: 1,
                    record: 0,
                    field: 1
                },
            )
            .is_err()
        );

        // Test hash_payload with failing reader
        assert!(super::hash_payload(FailingReader, 64).is_err());

        // payload_len overflow and record_out_of_range
        assert!(super::payload_len(u64::MAX).is_err());
        assert!(super::hash_entries(&b""[..], u64::MAX).is_err());
        assert_eq!(
            super::record_out_of_range(10).kind(),
            crate::error::ErrorKind::RecordOutOfRange { record: 10 }
        );
        assert_eq!(super::trailer_bytes(8), 16);
        assert_eq!(super::trailer_bytes(9), 32);
    }
}

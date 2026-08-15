use super::*;
#[cfg(feature = "parallel")]
use crate::config::{BlankRecords, Escape};
#[cfg(feature = "parallel")]
use crate::parallel::split::{Boundary, boundaries};
#[cfg(feature = "parallel")]
use crate::parallel::unordered::{NO_FAILURE, fence};
use crate::search::count1;
#[cfg(all(test, feature = "parallel"))]
use core::cell::Cell;
#[cfg(feature = "parallel")]
use core::num::NonZeroUsize;
#[cfg(feature = "parallel")]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(all(test, feature = "parallel"))]
use std::sync::Mutex;
#[cfg(feature = "parallel")]
use std::thread;

/// Largest number of entries an eager index load will pre-size its tables for.
///
/// The entry count in an index header is untrusted, so pre-sizing to it lets a
/// file that claims far more entries than it stores commit that much memory
/// before validation rejects it. Beyond this cap the tables grow as entries
/// are read, which bounds the commitment to the bytes actually delivered. At
/// 8 bytes per offset and 8 per line number the cap is 16 MiB of tables, well
/// above any index a legitimate reader pre-sizes and far below a length a
/// sparse file can invent.
const MAX_EAGER_ENTRY_RESERVE: usize = 1 << 20;
/// Document size at which splitting the index build across threads pays.
///
/// Indexing only records where each record begins, so it does far less work
/// per byte than a full parse and the fixed cost of finding boundaries and
/// spawning workers is repaid later. A wall-clock sweep on an otherwise idle
/// host put the serial-to-parallel ratio at 0.79x for 2 MiB, 1.34x for 4 MiB,
/// then 1.77x for 8 MiB and 1.8-2.5x from there to 64 MiB. Eight mebibytes is
/// the smallest size that won clearly, so smaller documents stay serial rather
/// than pay for threads that would not earn their keep.
///
/// `benchmarks/index/run.py` now measures that sweep on every scheduled run
/// and gates it, so the figures above are checked rather than remembered. On
/// the 16-core reference host it records 1.18x, 1.77x, 2.29x, 1.97x and 2.33x
/// across the same sizes -- more parallelism than the original sweep saw, and
/// enough that 2 MiB is nominally a win there too. The threshold stays at 8
/// MiB regardless: at 2 MiB the whole build takes about five milliseconds, and
/// waking every core to save under a millisecond of it is a poor trade for a
/// caller who may be running this crate on many documents at once. The
/// harness's routing check fails if this constant and the builder actually
/// reached ever disagree.
#[cfg(feature = "parallel")]
const PARALLEL_INDEX_THRESHOLD_BYTES: usize = 8 << 20;
#[cfg(feature = "parallel")]
const INDEX_CHUNKS_PER_THREAD: usize = 16;

fn visit_io_positions<R: Read>(
    reader: &mut IoParser<HashingReader<R>>,
    mut visit: impl FnMut(usize, u64) -> Result<(), Error>,
) -> Result<(), Error> {
    while let Some(mut line) = reader.next_line()? {
        let start = line.record()?.byte_range().start;
        let physical = reader.line_for_offset(start);
        reader.advance_line_origin(start, physical);
        debug_assert_eq!(
            reader.line_for_offset(start.saturating_sub(1)),
            physical,
            "advancing the line origin must make the preceding byte relative to the new record"
        );
        visit(start, physical)?;
    }
    Ok(())
}

/// A record-offset index, letting you seek straight to any record.
///
/// CSV has no record index, so reaching record four million normally means
/// scanning to it — and you cannot seek to a newline and trust it, because
/// that newline may be inside a quoted field. Build one of these with a single
/// scan, optionally save it next to the file, and every later lookup is a
/// seek. This pays off for a file that is read many times: a report tool, a
/// paging UI, or anything sampling rows out of order.
///
/// The index is bound to the exact bytes it was built from. Use
/// [`Self::validate_source`] or [`Self::validate_reader`] to detect a file
/// that changed underneath it.
///
/// Records are numbered as the raw document orders them, so record 0 is the
/// header row when the document has one.
///
/// ```
/// use coseva::index::{CsvIndex, IndexOptions};
///
/// let source = b"city,population\nBoston,650706\nDenver,715522\n";
/// let index = CsvIndex::build(source, IndexOptions::default())?;
///
/// assert_eq!(index.len(), 3);
///
/// // Jump straight to the second data record.
/// let mut parser = index.parser_at(source, 2)?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
/// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
///
/// // The index still describes these bytes.
/// assert!(index.validate_source(source).is_ok());
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// For a source larger than memory, use [`Self::create_path`] to build without
/// holding the document, and [`CsvIndexReader`] to look positions up without
/// holding the index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CsvIndex {
    source_len: u64,
    source_hash: [u8; HASH_BYTES],
    format: FormatOptions,
    limits: Limits,
    offsets: Vec<u64>,
    lines: Vec<u64>,
}

impl CsvIndex {
    /// Build an index for an in-memory source.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = b"city,population\nBoston,650706\nDenver,715522\n";
    /// let index = CsvIndex::build(source, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = index.parser_at(source, 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first CSV syntax or resource-limit error.
    pub fn build(source: impl AsRef<[u8]>, options: IndexOptions) -> Result<Self, Error> {
        let source = source.as_ref();
        Self::from_positions(source, options, build_positions(source, options)?)
    }

    /// Assemble an index around position tables already built for `source`.
    fn from_positions(
        source: &[u8],
        options: IndexOptions,
        (offsets, lines): (Vec<u64>, Vec<u64>),
    ) -> Result<Self, Error> {
        Ok(Self {
            source_len: widen(source.len()),
            source_hash: hash_bytes(source),
            format: options.format,
            limits: options.limits,
            offsets,
            lines,
        })
    }

    /// Build an index through the serial builder, whatever the size threshold
    /// and the host's thread count would have chosen.
    ///
    /// [`Self::build`] picks between two builders by document size, so nothing
    /// outside this crate can measure one against the other -- and a ratio
    /// between two runs on the same document in the same process is the only
    /// wall-clock figure that means anything on a shared machine. That is what
    /// `benches/index_build_wallclock.rs` needs, and the only reason this
    /// exists.
    ///
    /// # Errors
    ///
    /// Returns the first CSV syntax or resource-limit error.
    #[cfg(feature = "benchmarking")]
    pub(crate) fn benchmark_build_serial(
        source: &[u8],
        options: IndexOptions,
    ) -> Result<Self, Error> {
        Self::from_positions(source, options, build_positions_serial(source, options)?)
    }

    /// Build an index through the parallel builder at a fixed thread count.
    ///
    /// The counterpart to [`Self::benchmark_build_serial`]. Takes `threads`
    /// explicitly so a measurement states the width it was taken at rather than
    /// inheriting the host's.
    ///
    /// # Errors
    ///
    /// Returns the first CSV syntax or resource-limit error, or reports the
    /// document as unindexable in parallel when its format forbids it.
    #[cfg(all(feature = "benchmarking", feature = "parallel"))]
    pub(crate) fn benchmark_build_parallel(
        source: &[u8],
        options: IndexOptions,
        threads: usize,
    ) -> Result<Self, Error> {
        if !parallel_index_supported(options.format) {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                PARALLEL_FORMAT_UNSUPPORTED,
            ));
        }
        Self::from_positions(
            source,
            options,
            build_positions_parallel(source, options, normalized_thread_count(threads))?,
        )
    }

    /// Build an index and persist it to a path.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// let index_path = directory.path().join("cities.idx");
    /// std::fs::write(&source_path, b"city,population\nBoston,650706\nDenver,715522\n")?;
    ///
    /// let index = CsvIndex::build_path(&source_path, &index_path, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = index.parser_at_path(&source_path, 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    ///
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a source-reading, parse, or index-writing error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn build_path(
        source_path: impl AsRef<Path>,
        index_path: impl AsRef<Path>,
        options: IndexOptions,
    ) -> Result<Self, Error> {
        let source = File::open(source_path).map_err(Error::io_at_start)?;
        let source = HashingReader::new(source);
        let mut reader = IoParser::with_options(
            source,
            options.format,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(options.limits),
        )?;
        let (mut offsets, mut lines) = position_tables_default();
        visit_io_positions(&mut reader, |start, physical| {
            push_position(&mut offsets, &mut lines, start, physical);
            Ok(())
        })?;
        // The index outlives the build, so the slack the final doubling left
        // would be held for as long as it exists.
        offsets.shrink_to_fit();
        lines.shrink_to_fit();
        let source = reader.into_inner();
        let index = Self {
            source_len: source.len,
            source_hash: source.hasher.digest128().to_le_bytes(),
            format: options.format,
            limits: options.limits,
            offsets,
            lines,
        };
        index.save(index_path)?;
        Ok(index)
    }

    /// Number of indexed records.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.offsets.len()
    }

    /// Whether no records were indexed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// CSV format used while indexing.
    #[must_use]
    pub const fn format(&self) -> FormatOptions {
        self.format
    }

    /// Parsing limits used while constructing this index.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }

    /// Byte offset of one zero-based record.
    #[must_use]
    pub fn record_offset(&self, record: usize) -> Option<u64> {
        self.offsets.get(record).copied()
    }

    /// One-based physical line of one zero-based record.
    #[must_use]
    pub fn record_line(&self, record: usize) -> Option<u64> {
        self.lines.get(record).copied()
    }

    /// Check that this index still describes `source`.
    ///
    /// An index is bound to the exact bytes it was built from. Validate before
    /// relying on one that was loaded from disk, or a file that changed since
    /// the index was built will silently yield the wrong records.
    ///
    /// # Errors
    ///
    /// Returns an error of kind [`ErrorKind::SourceMismatch`] when the
    /// source's length or content differs from the indexed bytes.
    pub fn validate_source(&self, source: impl AsRef<[u8]>) -> Result<(), Error> {
        let source = source.as_ref();
        if u64::try_from(source.len()).ok() != Some(self.source_len)
            || hash_bytes(source) != self.source_hash
        {
            return Err(Error::new(ErrorKind::SourceMismatch, Location::START));
        }
        Ok(())
    }

    /// Create a slice parser beginning at one indexed record.
    ///
    /// The returned parser uses the indexed format and treats all records as
    /// data. Because it spans the whole source and is then seeked, the byte,
    /// line, and record counters it reports stay absolute against the file: the
    /// first record it yields carries index `record`, not zero.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = b"city,population\nBoston,650706\nDenver,715522\nAustin,961855\n";
    /// let index = CsvIndex::build(source, IndexOptions::default())?;
    ///
    /// // Skip directly to record 3, without scanning records 0 through 2.
    /// let mut parser = index.parser_at(source, 3)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 3"))?;
    /// let record = line.record()?;
    /// assert_eq!(record.index(), 3);
    /// assert_eq!(record.get_str(0)?, Some("Austin"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for a source mismatch, out-of-range record, or invalid
    /// reconstructed parser configuration.
    pub fn parser_at<'source, S: AsRef<[u8]> + ?Sized>(
        &self,
        source: &'source S,
        record: usize,
    ) -> Result<SliceParser<'source>, Error> {
        let source = source.as_ref();
        self.validate_source(source)?;
        self.parser_at_validated(source, record)
    }

    /// Bind this index to `source`, checking its identity once.
    ///
    /// [`Self::parser_at`] is a one-shot convenience: it validates `source`
    /// and hands back a parser in a single call, which is the right cost for
    /// a single seek but means `source` is rehashed on every one of `n` seeks.
    /// Reading many records out of the same source instead calls for checking
    /// identity once and reusing that check, which is what the returned
    /// [`BoundSource`] does: every [`BoundSource::parser_at`] afterwards costs
    /// only a location lookup and a parser construction, flat regardless of
    /// how large `source` is or how many seeks are made against it.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = b"city,population\nBoston,650706\nDenver,715522\nAustin,961855\n";
    /// let index = CsvIndex::build(source, IndexOptions::default())?;
    /// let bound = index.bind(source)?;
    ///
    /// // Every seek below reuses the identity check `bind` already paid for.
    /// for record in 1..index.len() {
    ///     let mut parser = bound.parser_at(record)?;
    ///     let mut line = parser
    ///         .next_line()?
    ///         .ok_or_else(|| std::io::Error::other("expected indexed record"))?;
    ///     assert_eq!(line.record()?.index(), record as u64);
    /// }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::SourceMismatch`] when `source`'s length or content
    /// differs from the indexed bytes.
    pub fn bind<'index, 'source, S: AsRef<[u8]> + ?Sized>(
        &'index self,
        source: &'source S,
    ) -> Result<BoundSource<'index, 'source>, Error> {
        let source = source.as_ref();
        self.validate_source(source)?;
        Ok(BoundSource {
            index: self,
            source,
        })
    }

    /// Create a slice parser beginning at one indexed record, without
    /// validating `source` against this index.
    ///
    /// Shared by [`Self::parser_at`], which validates first, and
    /// [`BoundSource::parser_at`], whose caller already validated through
    /// [`Self::bind`].
    #[cfg_attr(coverage_nightly, coverage(off))]
    fn parser_at_validated<'source>(
        &self,
        source: &'source [u8],
        record: usize,
    ) -> Result<SliceParser<'source>, Error> {
        let location = self.location_at(record)?;
        // The parser spans the whole source and is then seeked, so the byte,
        // line, and record counters it reports stay absolute against the file.
        let mut reader = SliceParser::with_options(
            source,
            self.format.read_bom(ReadBom::Preserve),
            ParseOptions::new()
                .limits(self.limits)
                .headers(Headers::None),
        )?;
        reader.seek(location)?;
        Ok(reader)
    }

    /// Build an index with constant memory, streaming it straight to `index`.
    ///
    /// Unlike [`Self::build_path`], no location table is ever held in memory:
    /// record positions are written to `index` as they are discovered, so the
    /// working set stays constant no matter how many records the source holds.
    /// The returned [`CsvIndexReader`] reads positions back from `index` on
    /// demand, which keeps the whole pipeline suitable for enormous sources.
    ///
    /// `index` is written from its current position onwards, is rewritten in
    /// place to record the final header, and is left positioned at its start.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = &b"city,population\nBoston,650706\nDenver,715522\n"[..];
    /// let index = std::io::Cursor::new(Vec::new());
    /// let mut reader = CsvIndex::create(source, index, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_reader(std::io::Cursor::new(source), 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the reader and writer
    ///
    /// `source` and `index` are taken by value, and the returned reader hands
    /// the index back through [`CsvIndexReader::into_inner`]. A caller that
    /// must keep either can pass `&mut reader`, since `&mut R` implements
    /// [`Read`], [`Write`] and [`Seek`] wherever `R` does.
    ///
    /// # Errors
    ///
    /// Returns a source-reading, parse, or index-writing error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn create<R: Read, W: Read + Write + Seek>(
        source: R,
        index: W,
        options: IndexOptions,
    ) -> Result<CsvIndexReader<W>, Error> {
        let mut index = index;
        index_seek(&mut index, SeekFrom::Start(0))?;
        let mut writer = BufWriter::new(index);
        writer
            .write_all(&[0; FIXED_HEADER_BYTES])
            .map_err(Error::io_at_start)?;

        let source = HashingReader::new(source);
        let mut reader = IoParser::with_options(
            source,
            options.format,
            ParseOptions::new()
                .headers(Headers::None)
                .limits(options.limits),
        )?;
        let mut count: u64 = 0;
        let mut entries_hasher = Xxh3::new();
        visit_io_positions(&mut reader, |start, physical| {
            let offset = widen(start);
            let encoded = offset.to_le_bytes();
            entries_hasher.update(&encoded);
            writer.write_all(&encoded).map_err(Error::io_at_start)?;
            let encoded = physical.to_le_bytes();
            entries_hasher.update(&encoded);
            writer.write_all(&encoded).map_err(Error::io_at_start)?;
            count = count
                .checked_add(1)
                .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, TOO_MANY_RECORDS))?;
            Ok(())
        })?;
        let source = reader.into_inner();
        let mut index = writer
            .into_inner()
            .map_err(|error| Error::io(error.into_error(), Location::START))?;

        let header = encode_header(
            source.len,
            source.hasher.digest128().to_le_bytes(),
            options.format,
            options.limits,
            count,
        );
        index_seek(&mut index, SeekFrom::Start(0))?;
        index.write_all(&header).map_err(Error::io_at_start)?;

        // Each checksum is authenticated independently: the entries checksum
        // was accumulated above as every entry was written, and the header
        // checksum is a one-shot hash of the header now sitting in memory.
        // Neither needs the payload read back.
        let entries_checksum = entries_hasher.digest128().to_le_bytes();
        let header_checksum = hash_bytes(&header);
        index_seek(&mut index, SeekFrom::End(0))?;
        index
            .write_all(&entries_checksum)
            .map_err(Error::io_at_start)?;
        index
            .write_all(&header_checksum)
            .map_err(Error::io_at_start)?;
        index.flush().map_err(Error::io_at_start)?;
        CsvIndexReader::new(index)
    }

    /// Build an index for a file with constant memory, persisting it to a path.
    ///
    /// This is the constant-memory counterpart to [`Self::build_path`], which
    /// materializes the whole location table before saving it. It hands back a
    /// [`CsvIndexReader`] over the file just written, so lookups read the
    /// location table from disk rather than from memory.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// let index_path = directory.path().join("cities.idx");
    /// std::fs::write(&source_path, b"city,population\nBoston,650706\nDenver,715522\n")?;
    ///
    /// let mut index = CsvIndex::create_path(&source_path, &index_path, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = index.parser_at_path(&source_path, 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    ///
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a source-reading, parse, or index-writing error.
    pub fn create_path(
        source_path: impl AsRef<Path>,
        index_path: impl AsRef<Path>,
        options: IndexOptions,
    ) -> Result<CsvIndexReader<File>, Error> {
        let source = File::open(source_path).map_err(Error::io_at_start)?;
        let index = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(index_path)
            .map_err(Error::io_at_start)?;
        Self::create(source, index, options)
    }

    /// Validate that this index belongs to the bytes produced by `source`.
    ///
    /// The source is read once and hashed, so an enormous file can be checked
    /// without being held in memory.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::SourceMismatch`] when length or content differs,
    /// or an I/O error when the source cannot be read.
    pub fn validate_reader(&self, source: impl Read) -> Result<(), Error> {
        validate_identity(source, self.source_len, self.source_hash)
    }

    /// Create an [`IoParser`] beginning at one indexed record.
    ///
    /// This is the I/O counterpart to [`Self::parser_at`]: the source is
    /// seeked rather than held in memory, so records can be visited at random
    /// in a file far larger than RAM. As with [`Self::parser_at`], the byte,
    /// line, and record counters stay absolute against the file.
    ///
    /// Only the source length is checked, because hashing the whole source on
    /// every seek would defeat the purpose. Call [`Self::validate_reader`] once
    /// beforehand to confirm the full source identity.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = b"city,population\nBoston,650706\nDenver,715522\n";
    /// let index = CsvIndex::build(source, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = index.parser_at_reader(std::io::Cursor::new(source), 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the reader
    ///
    /// `source` is taken by value, and [`IoParser::into_inner`] hands it back.
    /// A caller that must keep the reader can pass `&mut reader`, since
    /// `&mut R` implements [`Read`] and [`Seek`] wherever `R` does.
    ///
    /// # Errors
    ///
    /// Returns an error for a source-length mismatch, an out-of-range record,
    /// an invalid reconstructed parser configuration, or a failed seek.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn parser_at_reader<R: Read + Seek>(
        &self,
        source: R,
        record: usize,
    ) -> Result<IoParser<R>, Error> {
        let location = self.location_at(record)?;
        io_parser_at(source, self.source_len, self.format, self.limits, location)
    }

    /// Open a file and create an [`IoParser`] at one indexed record.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// std::fs::write(&source_path, b"city,population\nBoston,650706\nDenver,715522\n")?;
    ///
    /// let source = std::fs::read(&source_path)?;
    /// let index = CsvIndex::build(&source, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = index.parser_at_path(&source_path, 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    ///
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be opened, for a source-length
    /// mismatch, or for an out-of-range record.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn parser_at_path(
        &self,
        source_path: impl AsRef<Path>,
        record: usize,
    ) -> Result<IoParser<File>, Error> {
        let source = File::open(source_path).map_err(Error::io_at_start)?;
        self.parser_at_reader(source, record)
    }

    /// Resolve one record to a validated absolute parser location.
    fn location_at(&self, record: usize) -> Result<Location, Error> {
        let (&offset, &line) = self
            .offsets
            .get(record)
            .zip(self.lines.get(record))
            .ok_or_else(|| record_out_of_range(record))?;
        entry_location(offset, line, self.source_len, record)
    }

    /// Persist this index.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the complete index cannot be written.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), Error> {
        let count = widen(self.offsets.len());
        let header = self.encode_header(count);
        let mut entries_hasher = Xxh3::new();
        let mut file = File::create(path).map_err(Error::io_at_start)?;
        file.write_all(&header).map_err(Error::io_at_start)?;
        for (&offset, &line) in self.offsets.iter().zip(&self.lines) {
            let encoded = offset.to_le_bytes();
            entries_hasher.update(&encoded);
            file.write_all(&encoded).map_err(Error::io_at_start)?;
            let encoded = line.to_le_bytes();
            entries_hasher.update(&encoded);
            file.write_all(&encoded).map_err(Error::io_at_start)?;
        }
        // Entries and header are authenticated independently, so this writer
        // never needs to reread anything it already wrote.
        file.write_all(&entries_hasher.digest128().to_le_bytes())
            .map_err(Error::io_at_start)?;
        file.write_all(&hash_bytes(&header))
            .map_err(Error::io_at_start)
    }

    /// Load and validate a persisted index.
    ///
    /// # Errors
    ///
    /// Returns an I/O, version, truncation, overflow, or checksum error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let file = File::open(path).map_err(Error::io_at_start)?;
        let file_len = file.metadata().map_err(Error::io_at_start)?.len();
        Self::decode_reader(file, file_len)
    }

    fn encode_header(&self, count: u64) -> Vec<u8> {
        encode_header(
            self.source_len,
            self.source_hash,
            self.format,
            self.limits,
            count,
        )
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn decode_reader(reader: File, file_len: u64) -> Result<Self, Error> {
        let mut reader = BufReader::new(reader);
        let mut header = [u8::default(); FIXED_HEADER_BYTES];
        read_index_exact(&mut reader, &mut header)?;

        let IndexHeader {
            version,
            source_len,
            source_hash,
            format,
            limits,
            count: count_u64,
        } = decode_header(&header)?;
        let expected_len = payload_len(count_u64)?
            .checked_add(trailer_bytes(version) as u64)
            .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_LENGTH_OVERFLOW))?;
        if expected_len != file_len {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                LOCATION_TABLE_LENGTH_MISMATCH,
            ));
        }
        // Version 8 authenticates header and entries with one checksum
        // computed over both in file order; version 9 authenticates them
        // independently. Priming the same hasher with the header only for
        // version 8 lets both cases share one accumulation loop below.
        let legacy = version < 9;
        let mut entries_hasher = Xxh3::new();
        if legacy {
            entries_hasher.update(&header);
        }
        let count = narrow(count_u64, RECORD_COUNT_NOT_USIZE)?;
        // The count is untrusted and has been checked only against the index
        // file's *logical* length, which a sparse file can inflate far beyond
        // the bytes it actually stores. Pre-sizing to the full count would let
        // such a file commit memory proportional to a length it cannot back,
        // so the eager reservation is capped and the vectors grow from there
        // as entries are genuinely read. Legitimate indexes below the cap keep
        // their exact single allocation.
        let (mut offsets, mut lines) = position_tables(eager_reserve(count));
        let mut previous: Option<(u64, u64)> = Option::default();
        for _ in 0..count {
            let mut encoded = [u8::default(); 8];
            read_index_exact(&mut reader, &mut encoded)?;
            entries_hasher.update(&encoded);
            let offset = u64::from_le_bytes(encoded);
            read_index_exact(&mut reader, &mut encoded)?;
            entries_hasher.update(&encoded);
            let line = u64::from_le_bytes(encoded);
            let (previous_offset, previous_line) = previous.unzip();
            check_entry(offset, line, source_len, previous_offset, previous_line)?;
            previous = Some((offset, line));
            offsets.push(offset);
            lines.push(line);
        }
        if legacy {
            let mut checksum = [u8::default(); CHECKSUM_BYTES];
            read_index_exact(&mut reader, &mut checksum)?;
            if entries_hasher.digest128().to_le_bytes() != checksum {
                return Err(Error::detailed(
                    ErrorKind::InvalidIndex,
                    INDEX_CHECKSUM_MISMATCH,
                ));
            }
        } else {
            let mut entries_checksum = [u8::default(); CHECKSUM_BYTES];
            read_index_exact(&mut reader, &mut entries_checksum)?;
            if entries_hasher.digest128().to_le_bytes() != entries_checksum {
                return Err(Error::detailed(
                    ErrorKind::InvalidIndex,
                    INDEX_ENTRIES_CHECKSUM_MISMATCH,
                ));
            }
            let mut header_checksum = [u8::default(); CHECKSUM_BYTES];
            read_index_exact(&mut reader, &mut header_checksum)?;
            if hash_bytes(&header) != header_checksum {
                return Err(Error::detailed(
                    ErrorKind::InvalidIndex,
                    INDEX_HEADER_CHECKSUM_MISMATCH,
                ));
            }
        }
        Ok(Self {
            source_len,
            source_hash,
            format,
            limits,
            offsets,
            lines,
        })
    }
}

// Test-only override of `PARALLEL_INDEX_THRESHOLD_BYTES`, so a test can drive
// the dispatch below through the public `CsvIndex::build` without an 8 MiB
// fixture. Production code always reads the constant; this seam only exists
// under `#[cfg(test)]`, mirroring `TEST_MAX_OFFSET` in `engine::framing`.
#[cfg(all(test, feature = "parallel"))]
std::thread_local! {
    static TEST_PARALLEL_INDEX_THRESHOLD: Cell<Option<usize>> =
        const { Cell::new(None) };
}

#[cfg(feature = "parallel")]
fn parallel_index_threshold() -> usize {
    #[cfg(test)]
    {
        TEST_PARALLEL_INDEX_THRESHOLD
            .with(Cell::get)
            .unwrap_or(PARALLEL_INDEX_THRESHOLD_BYTES)
    }
    #[cfg(not(test))]
    {
        PARALLEL_INDEX_THRESHOLD_BYTES
    }
}

#[cfg(feature = "parallel")]
fn normalized_thread_count(threads: usize) -> usize {
    threads.max(1)
}

#[cfg(feature = "parallel")]
fn should_build_parallel(source_len: usize, format: FormatOptions) -> bool {
    source_len >= parallel_index_threshold() && parallel_index_supported(format)
}

fn build_positions(source: &[u8], options: IndexOptions) -> Result<(Vec<u64>, Vec<u64>), Error> {
    #[cfg(feature = "parallel")]
    if should_build_parallel(source.len(), options.format) {
        let threads = thread::available_parallelism()
            .unwrap_or(NonZeroUsize::MIN)
            .get();
        return build_positions_parallel(source, options, threads);
    }

    build_positions_serial(source, options)
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn build_positions_serial(
    source: &[u8],
    options: IndexOptions,
) -> Result<(Vec<u64>, Vec<u64>), Error> {
    let mut reader = SliceParser::with_options(
        source,
        options.format,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(options.limits),
    )?;
    let (mut offsets, mut lines) = position_tables(build_entry_estimate(source, options.format));
    while let Some(mut line) = reader.next_line()? {
        let start = line.record()?.byte_range().start;
        let physical = reader.line_for_offset(start);
        // Record starts are visited in order, so the line origin moves with
        // them and each lookup scans only the bytes since the last record.
        // gamma::skip(stmt.delete_call, reason = "without advancing the origin, line lookup repeatedly rescans the full parsed prefix")
        reader.advance_line_origin(start, physical);
        push_position(&mut offsets, &mut lines, start, physical);
    }
    // The index outlives the build, so the slack the final doubling left would
    // be held for as long as it exists.
    offsets.shrink_to_fit();
    lines.shrink_to_fit();
    Ok((offsets, lines))
}

/// Chunks actually handed to [`parse_index_chunk`], for the test that shows a
/// failed build stops rather than indexing the rest of the document.
#[cfg(all(test, feature = "parallel"))]
static CHUNKS_PARSED: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, feature = "parallel"))]
static LAST_PARALLEL_REQUESTED_THREADS: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, feature = "parallel"))]
static LAST_PARALLEL_CHUNKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, feature = "parallel"))]
static LAST_PARALLEL_WORKERS: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, feature = "parallel"))]
static LAST_PARALLEL_SPAWNED: AtomicUsize = AtomicUsize::new(0);
#[cfg(all(test, feature = "parallel"))]
static LAST_PARALLEL_CHUNK_RESERVE: AtomicUsize = AtomicUsize::new(0);

/// Held for the duration of any test that builds positions in parallel.
///
/// `CHUNKS_PARSED` is process-global while the test harness runs tests
/// concurrently, so a second parallel build overlapping the counting test adds
/// its own chunks to the total and makes the assertion fail for a reason that
/// has nothing to do with the fence it is checking.
#[cfg(all(test, feature = "parallel"))]
static PARALLEL_BUILD_LOCK: Mutex<()> = Mutex::new(());

#[cfg(feature = "parallel")]
#[derive(Debug)]
struct ChunkPositions {
    index: usize,
    offsets: Vec<u64>,
    lines: Vec<u64>,
}

#[cfg(feature = "parallel")]
fn parallel_worker_count(threads: usize, chunks: usize) -> usize {
    threads.min(chunks).max(1)
}

#[cfg(feature = "parallel")]
const fn chunk_is_blocked(start: usize, failure: usize) -> bool {
    start >= failure
}

#[cfg(feature = "parallel")]
fn failure_barrier() -> AtomicUsize {
    AtomicUsize::new(NO_FAILURE)
}

#[cfg(feature = "parallel")]
fn chunk_entry_reserve(estimate: usize, chunks: usize) -> usize {
    // gamma::skip(arith.div_to_mul, iter.max_to_min, reason = "multiplying the full estimate or dividing by zero makes each worker reserve runaway memory")
    estimate / chunks.max(NonZeroUsize::MIN.get()) + NonZeroUsize::MIN.get()
}

#[cfg(feature = "parallel")]
fn keep_earliest_failure(earliest: &mut Option<(usize, Error)>, error: Error) {
    if earliest
        .as_ref()
        .is_none_or(|(offset, _)| error.location().byte < *offset)
    {
        *earliest = Some((error.location().byte, error));
    }
}

#[cfg(feature = "parallel")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn build_positions_parallel(
    source: &[u8],
    options: IndexOptions,
    threads: usize,
) -> Result<(Vec<u64>, Vec<u64>), Error> {
    #[cfg(test)]
    LAST_PARALLEL_REQUESTED_THREADS.store(threads, Ordering::Relaxed);
    let mut probe = SliceParser::with_options(
        source,
        options.format,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(options.limits),
    )?;
    let start = Boundary::from(probe.location());
    probe.seek(start.into())?;

    let chunks = boundaries(
        source,
        options.format.dialect,
        start,
        threads * INDEX_CHUNKS_PER_THREAD,
    );
    #[cfg(test)]
    LAST_PARALLEL_CHUNKS.store(chunks.len(), Ordering::Relaxed);
    let threads = parallel_worker_count(threads, chunks.len());
    #[cfg(test)]
    LAST_PARALLEL_WORKERS.store(threads, Ordering::Relaxed);
    let estimate = build_entry_estimate(source, options.format);
    let chunk_reserve = chunk_entry_reserve(estimate, chunks.len());
    #[cfg(test)]
    LAST_PARALLEL_CHUNK_RESERVE.store(chunk_reserve, Ordering::Relaxed);
    let cursor = AtomicUsize::new(0);
    // The earliest failure any worker has seen. A chunk beginning at or after
    // it holds only records later than the error that will be reported, so
    // indexing it is work whose result is thrown away — and on a malformed
    // document that is the whole rest of the file, on every thread.
    let barrier = failure_barrier();

    let results = thread::scope(|scope| {
        let handles = std::iter::repeat_with(|| {
            let cursor = &cursor;
            let barrier = &barrier;
            let chunks = &chunks;
            scope.spawn(move || {
                let mut worker_chunks = Vec::new();
                loop {
                    // gamma::skip(literal.int_decrement, reason = "a zero fetch increment leaves every worker on one chunk forever")
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(&start) = chunks.get(index) else {
                        return worker_chunks;
                    };
                    if chunk_is_blocked(start.byte, barrier.load(Ordering::Relaxed)) {
                        continue;
                    }
                    #[cfg(test)]
                    CHUNKS_PARSED.fetch_add(1, Ordering::Relaxed);
                    let parsed =
                        parse_index_chunk(source, options, chunks, index, start, chunk_reserve);
                    if let Err(error) = &parsed {
                        fence(barrier, error.location().byte);
                    }
                    worker_chunks.push(parsed);
                }
            })
        })
        .take(threads)
        .collect::<Vec<_>>();
        #[cfg(test)]
        LAST_PARALLEL_SPAWNED.store(handles.len(), Ordering::Relaxed);

        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("an index worker panicked"))
            .collect::<Vec<_>>()
    });

    let mut chunks = Vec::with_capacity(results.len());
    let mut earliest: Option<(usize, Error)> = Option::default();
    for result in results {
        match result {
            Ok(chunk) => chunks.push(chunk),
            Err(error) => keep_earliest_failure(&mut earliest, error),
        }
    }
    if let Some((_, error)) = earliest {
        return Err(error);
    }

    chunks.sort_by_key(|chunk| chunk.index);
    // `entries` is the exact final length, so reserve that exact capacity
    // to avoid over-allocating or requiring a subsequent `shrink_to_fit`.
    let entries: usize = chunks.iter().map(|chunk| chunk.offsets.len()).sum();
    let (mut offsets, mut lines) = position_tables(entries);
    for mut chunk in chunks {
        offsets.append(&mut chunk.offsets);
        lines.append(&mut chunk.lines);
    }
    Ok((offsets, lines))
}

#[cfg(feature = "parallel")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn parse_index_chunk(
    source: &[u8],
    options: IndexOptions,
    chunks: &[Boundary],
    index: usize,
    start: Boundary,
    reserve: usize,
) -> Result<ChunkPositions, Error> {
    let end = chunks
        .get(index + 1)
        .map_or(source.len(), |boundary| boundary.byte);
    let mut reader = SliceParser::with_options(
        &source[..end],
        options.format,
        ParseOptions::new()
            .headers(Headers::None)
            .limits(options.limits),
    )?;
    reader.seek(start.into())?;
    let (mut offsets, mut lines) = position_tables(reserve);

    while let Some(mut line) = reader.next_line()? {
        let range = line.record()?.byte_range();
        let physical = reader.line_for_offset(range.start);
        // gamma::skip(stmt.delete_call, reason = "without advancing the origin, line lookup repeatedly rescans the full chunk prefix")
        reader.advance_line_origin(range.start, physical);
        push_position(&mut offsets, &mut lines, range.start, physical);
    }

    Ok(ChunkPositions {
        index,
        offsets,
        lines,
    })
}

#[cfg(feature = "parallel")]
fn parallel_index_supported(format: FormatOptions) -> bool {
    format.dialect.comment.is_none()
        && format.dialect.escape == Escape::DoubleQuote
        && !format.dialect.multibyte()
        && format.syntax.quoting_enabled()
        && !format.syntax.permits_unquoted_quotes()
        && format.blank_records != BlankRecords::Skip
}

/// Document size at which `build` begins estimating its table capacity.
///
/// The slack that doubling leaves is a fraction of the index, so it only
/// matters once the index itself does. Keeping small documents off the estimate
/// keeps the cheapest index path free of the counting scan entirely.
const BUILD_ESTIMATE_THRESHOLD: usize = 1024 * 1024;

/// Smallest mean record size that makes an estimate worth acting on.
///
/// Record endings inside quoted fields are counted as though they ended a
/// record, so a document dense with them looks like one of implausibly short
/// records. Past this density the count is treated as unrepresentative and the
/// tables are left to grow, rather than committing memory a skewed document
/// does not need.
const MIN_ESTIMATED_RECORD_BYTES: usize = 8;

/// Estimates how many entries an in-memory document will index, for `build`'s
/// up-front reservation.
///
/// Growing both tables by doubling from empty leaves up to half the final
/// capacity as slack at the moment of peak, and the peak is what bounds a build
/// of a large document.
///
/// Counting record endings is a second pass over the document, so it is done
/// only above [`BUILD_ESTIMATE_THRESHOLD`], where the slack it saves is worth
/// more than the scan. Sampling a bounded prefix instead was measured and
/// rejected: on a document whose early records are shorter than its mean, the
/// projection overshoots to exactly the capacity doubling would have reached,
/// saving nothing.
///
/// The count can only ever overshoot the true record count, never undershoot
/// it, so the reservation is not left to grow. Neither direction is a
/// correctness concern: the result is capped, and a short estimate simply
/// resumes doubling.
fn build_entry_estimate(source: &[u8], format: FormatOptions) -> usize {
    if source.len() < BUILD_ESTIMATE_THRESHOLD {
        return 0;
    }
    let terminator = format.dialect.record_ending.byte();
    let mut endings = count1(terminator, source);
    if !source.ends_with(&[terminator]) {
        endings = endings.saturating_add(1);
    }
    if endings == 0 || source.len() / endings < MIN_ESTIMATED_RECORD_BYTES {
        return 0;
    }
    endings.min(MAX_EAGER_ENTRY_RESERVE)
}

fn eager_reserve(count: usize) -> usize {
    count.min(MAX_EAGER_ENTRY_RESERVE)
}

/// Create the two position tables with the same exact reservation.
fn position_tables(entries: usize) -> (Vec<u64>, Vec<u64>) {
    (Vec::with_capacity(entries), Vec::with_capacity(entries))
}

fn position_tables_default() -> (Vec<u64>, Vec<u64>) {
    (Vec::new(), Vec::new())
}

/// A source whose identity has already been checked against a [`CsvIndex`].
///
/// Returned by [`CsvIndex::bind`]. Every [`Self::parser_at`] call skips the
/// source hash [`CsvIndex::parser_at`] would otherwise repeat, since `bind`
/// already confirmed `source` is the exact bytes the index describes.
#[derive(Clone, Copy, Debug)]
pub struct BoundSource<'index, 'source> {
    index: &'index CsvIndex,
    source: &'source [u8],
}

impl<'source> BoundSource<'_, 'source> {
    /// Create a slice parser beginning at one indexed record.
    ///
    /// Identical to [`CsvIndex::parser_at`], except the source was already
    /// validated by [`CsvIndex::bind`], so this call costs a location lookup
    /// and a parser construction — never another pass over `source`.
    ///
    /// # Errors
    ///
    /// Returns an error for an out-of-range record or invalid reconstructed
    /// parser configuration.
    pub fn parser_at(&self, record: usize) -> Result<SliceParser<'source>, Error> {
        self.index.parser_at_validated(self.source, record)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    #[cfg(feature = "parallel")]
    use super::{
        CHUNKS_PARSED, INDEX_CHUNKS_PER_THREAD, Ordering, PARALLEL_BUILD_LOCK,
        TEST_PARALLEL_INDEX_THRESHOLD, build_positions_parallel, build_positions_serial,
    };
    use super::{CsvIndex, ErrorKind, hash_bytes};
    #[cfg(feature = "parallel")]
    use crate::config::{BlankRecords, Escape, Recovery, Syntax};
    use crate::config::{FormatOptions, Limits};
    #[cfg(feature = "parallel")]
    use crate::index::IndexOptions;
    #[cfg(feature = "parallel")]
    use crate::{Error, Location};
    use std::fs;
    use std::path::PathBuf;
    #[cfg(feature = "parallel")]
    use std::sync::PoisonError;

    struct ScratchPath {
        path: PathBuf,
        _directory: tempfile::TempDir,
    }

    impl AsRef<std::path::Path> for ScratchPath {
        fn as_ref(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl std::ops::Deref for ScratchPath {
        type Target = std::path::Path;

        fn deref(&self) -> &Self::Target {
            &self.path
        }
    }

    /// A unique scratch path whose parent directory is removed on drop.
    fn scratch_path(name: &str) -> ScratchPath {
        let directory = tempfile::Builder::new()
            .prefix("coseva-csv-index")
            .tempdir()
            .expect("temporary directory");
        ScratchPath {
            path: directory.path().join(name),
            _directory: directory,
        }
    }

    /// A `CsvIndex` claiming a record in a source with no bytes at all. Only
    /// hand construction, available to this in-crate test module, can produce
    /// such a value.
    fn invalid_empty_source_index() -> CsvIndex {
        CsvIndex {
            source_len: 0,
            source_hash: hash_bytes(b""),
            format: FormatOptions::CSV,
            limits: Limits::DEFAULT,
            offsets: vec![0],
            lines: vec![1],
        }
    }

    /// A `CsvIndex` whose format could never come from a successful `build`:
    /// the delimiter and quote bytes are equal, which the dialect validator
    /// rejects. Only hand construction (available to this in-crate test
    /// module) can produce such a value, letting the reconstruction paths in
    /// `parser_at` and `parser_at_reader` be exercised directly.
    fn invalid_format_index() -> CsvIndex {
        CsvIndex {
            source_len: 1,
            source_hash: hash_bytes(b"x"),
            format: FormatOptions::CSV.delimiter(b'"'),
            limits: Limits::DEFAULT,
            offsets: vec![0],
            lines: vec![1],
        }
    }

    fn write_test_index(path: &std::path::Path, source_len: u64, entries: &[(u64, u64)]) {
        let header = super::encode_header(
            source_len,
            [7; super::HASH_BYTES],
            FormatOptions::CSV,
            Limits::DEFAULT,
            entries.len() as u64,
        );
        let mut bytes = header.clone();
        let mut encoded_entries = Vec::new();
        for &(offset, line) in entries {
            encoded_entries.extend_from_slice(&offset.to_le_bytes());
            encoded_entries.extend_from_slice(&line.to_le_bytes());
        }
        bytes.extend_from_slice(&encoded_entries);
        bytes.extend_from_slice(&super::hash_bytes(&encoded_entries));
        bytes.extend_from_slice(&super::hash_bytes(&header));
        fs::write(path, bytes).expect("write test index");
    }

    #[test]
    fn streamed_position_collection_advances_its_line_origin() {
        let source = super::HashingReader::new(&b"a\nb\nc\n"[..]);
        let mut reader = crate::IoParser::with_options(
            source,
            FormatOptions::CSV,
            crate::config::ParseOptions::new().headers(crate::config::Headers::None),
        )
        .expect("parser");
        let mut positions = Vec::new();
        super::visit_io_positions(&mut reader, |offset, line| {
            positions.push((offset, line));
            Ok(())
        })
        .expect("positions");
        assert_eq!(positions, [(0, 1), (2, 2), (4, 3)]);
        assert_eq!(
            reader.line_for_offset(0),
            3,
            "the final record must become the parser's line-counting origin"
        );
    }

    #[test]
    fn eager_table_planning_and_estimation_are_exact() {
        assert_eq!(super::eager_reserve(0), 0);
        assert_eq!(
            super::eager_reserve(super::MAX_EAGER_ENTRY_RESERVE),
            super::MAX_EAGER_ENTRY_RESERVE
        );
        assert_eq!(
            super::eager_reserve(super::MAX_EAGER_ENTRY_RESERVE + 1),
            super::MAX_EAGER_ENTRY_RESERVE
        );

        let (offsets, lines) = super::position_tables(17);
        assert_eq!(offsets.capacity(), 17);
        assert_eq!(lines.capacity(), 17);
        let (offsets, lines) = super::position_tables_default();
        assert_eq!(offsets.capacity(), 0);
        assert_eq!(lines.capacity(), 0);

        let small = vec![b'a'; super::BUILD_ESTIMATE_THRESHOLD - 1];
        assert_eq!(super::build_entry_estimate(&small, FormatOptions::CSV), 0);

        let no_ending = vec![b'a'; super::BUILD_ESTIMATE_THRESHOLD];
        assert_eq!(
            super::build_entry_estimate(&no_ending, FormatOptions::CSV),
            1
        );

        let mut ending = no_ending.clone();
        *ending.last_mut().expect("nonempty") = b'\n';
        assert_eq!(super::build_entry_estimate(&ending, FormatOptions::CSV), 1);

        let mut starts_only = no_ending.clone();
        starts_only[0] = b'\n';
        assert_eq!(
            super::build_entry_estimate(&starts_only, FormatOptions::CSV),
            2
        );

        let mut boundary_density = vec![b'a'; super::BUILD_ESTIMATE_THRESHOLD];
        for at in (7..boundary_density.len()).step_by(super::MIN_ESTIMATED_RECORD_BYTES) {
            boundary_density[at] = b'\n';
        }
        assert_eq!(
            super::build_entry_estimate(&boundary_density, FormatOptions::CSV),
            boundary_density.len() / super::MIN_ESTIMATED_RECORD_BYTES
        );

        let mut capped =
            vec![b'a'; (super::MAX_EAGER_ENTRY_RESERVE + 1) * super::MIN_ESTIMATED_RECORD_BYTES];
        for at in (7..capped.len()).step_by(super::MIN_ESTIMATED_RECORD_BYTES) {
            capped[at] = b'\n';
        }
        assert_eq!(
            super::build_entry_estimate(&capped, FormatOptions::CSV),
            super::MAX_EAGER_ENTRY_RESERVE
        );
    }

    #[test]
    fn eager_decoder_preserves_entries_and_rejects_exact_malformed_shapes() {
        let valid = scratch_path("exact-valid");
        write_test_index(&valid, 4, &[(0, 1), (1, 2), (3, 3)]);
        let loaded = CsvIndex::load(&valid).expect("valid test index");
        assert_eq!(loaded.offsets, [0, 1, 3]);
        assert_eq!(loaded.lines, [1, 2, 3]);
        assert_eq!(loaded.offsets.capacity(), 3);
        assert_eq!(loaded.lines.capacity(), 3);
        fs::remove_file(&valid).expect("remove valid index");

        let duplicate = scratch_path("duplicate-offset");
        write_test_index(&duplicate, 4, &[(0, 1), (0, 2)]);
        let error = CsvIndex::load(&duplicate).expect_err("duplicate offsets are invalid");
        assert!(
            error
                .to_string()
                .contains("record offsets are not strictly increasing"),
            "{error}"
        );
        fs::remove_file(&duplicate).expect("remove duplicate index");

        let decreasing = scratch_path("decreasing-line");
        write_test_index(&decreasing, 4, &[(0, 2), (2, 1)]);
        let error = CsvIndex::load(&decreasing).expect_err("decreasing lines are invalid");
        assert!(
            error
                .to_string()
                .contains("record lines are not valid and nondecreasing"),
            "{error}"
        );
        fs::remove_file(&decreasing).expect("remove decreasing index");

        let wrong_len = scratch_path("wrong-length");
        write_test_index(&wrong_len, 4, &[(0, 1)]);
        let mut bytes = fs::read(&wrong_len).expect("read index");
        bytes.push(0xAA);
        fs::write(&wrong_len, bytes).expect("append trailing byte");
        let error = CsvIndex::load(&wrong_len).expect_err("trailing bytes are invalid");
        assert!(
            error
                .to_string()
                .contains("location table length does not match"),
            "{error}"
        );
        fs::remove_file(&wrong_len).expect("remove wrong-length index");
    }

    #[test]
    fn build_path_reclaims_final_position_table_slack() {
        let source_path = scratch_path("capacity-source");
        let index_path = scratch_path("capacity-index");
        let mut source = String::new();
        for record in 0..65 {
            use std::fmt::Write as _;
            writeln!(source, "{record},value").expect("write source");
        }
        fs::write(&source_path, source).expect("write source file");
        let index = CsvIndex::build_path(
            &source_path,
            &index_path,
            crate::index::IndexOptions::default(),
        )
        .expect("streamed index");
        assert_eq!(index.offsets.capacity(), index.offsets.len());
        assert_eq!(index.lines.capacity(), index.lines.len());
        fs::remove_file(source_path).expect("remove source");
        fs::remove_file(index_path).expect("remove index");
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_routing_helpers_cover_every_boundary_and_disqualifier() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        assert_eq!(super::PARALLEL_INDEX_THRESHOLD_BYTES, 8 << 20);
        assert_eq!(super::normalized_thread_count(0), 1);
        assert_eq!(super::normalized_thread_count(1), 1);
        assert_eq!(super::normalized_thread_count(4), 4);
        assert_eq!(super::parallel_worker_count(0, 0), 1);
        assert_eq!(super::parallel_worker_count(8, 3), 3);
        assert_eq!(super::parallel_worker_count(2, 8), 2);
        assert!(super::chunk_is_blocked(7, 7));
        assert!(super::chunk_is_blocked(8, 7));
        assert!(!super::chunk_is_blocked(6, 7));
        assert_eq!(
            super::failure_barrier().load(Ordering::Relaxed),
            super::NO_FAILURE,
            "no failure is published initially"
        );
        assert_eq!(super::chunk_entry_reserve(0, 0), 1);
        assert_eq!(super::chunk_entry_reserve(10, 2), 6);
        assert_eq!(super::chunk_entry_reserve(10, 3), 4);

        let source = b"a,b\nc,d\n";
        let options = IndexOptions::default();
        let mut probe = crate::SliceParser::with_options(
            source,
            options.format,
            crate::config::ParseOptions::new().headers(crate::config::Headers::None),
        )
        .expect("probe");
        let start = super::Boundary::from(probe.location());
        probe.seek(start.into()).expect("seek probe");
        let chunks = super::boundaries(source, options.format.dialect, start, 2);
        let chunk =
            super::parse_index_chunk(source, options, &chunks, 0, chunks[0], 7).expect("chunk");
        assert_eq!(chunk.offsets.capacity(), 7);
        assert_eq!(chunk.lines.capacity(), 7);

        assert!(super::parallel_index_supported(FormatOptions::CSV));
        assert!(!super::parallel_index_supported(
            FormatOptions::CSV.comment(Some(b'#'))
        ));
        assert!(!super::parallel_index_supported(
            FormatOptions::CSV.escape(Escape::Backslash(b'\\'))
        ));
        assert!(!super::parallel_index_supported(
            FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE))
        ));
        assert!(!super::parallel_index_supported(FormatOptions::CSV.syntax(
            Syntax::Compatible(Recovery::NONE.quoting(true).unquoted_quotes(true))
        )));
        assert!(!super::parallel_index_supported(
            FormatOptions::CSV.blank_records(BlankRecords::Skip)
        ));
        #[cfg(feature = "multibyte")]
        assert!(!super::parallel_index_supported(
            FormatOptions::CSV.delimiter_sequence(b"||")
        ));

        super::TEST_PARALLEL_INDEX_THRESHOLD.with(|threshold| threshold.set(None));
        assert_eq!(
            super::parallel_index_threshold(),
            super::PARALLEL_INDEX_THRESHOLD_BYTES
        );
        super::TEST_PARALLEL_INDEX_THRESHOLD.with(|threshold| threshold.set(Some(17)));
        assert!(!super::should_build_parallel(16, FormatOptions::CSV));
        assert!(super::should_build_parallel(17, FormatOptions::CSV));
        assert!(!super::should_build_parallel(
            18,
            FormatOptions::CSV.comment(Some(b'#'))
        ));
        super::TEST_PARALLEL_INDEX_THRESHOLD.with(|threshold| threshold.set(None));
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_failure_selection_keeps_the_first_error_at_the_earliest_byte() {
        fn error_at(byte: usize, kind: ErrorKind) -> Error {
            Error::new(
                kind,
                Location {
                    byte,
                    line: 1,
                    record: 0,
                    field: 0,
                },
            )
        }

        let mut earliest = None;
        super::keep_earliest_failure(&mut earliest, error_at(5, ErrorKind::InvalidIndex));
        super::keep_earliest_failure(&mut earliest, error_at(6, ErrorKind::SourceMismatch));
        assert_eq!(
            earliest.as_ref().expect("first error retained").1.kind(),
            ErrorKind::InvalidIndex
        );

        super::keep_earliest_failure(
            &mut earliest,
            error_at(5, ErrorKind::TooManyFields { limit: 2 }),
        );
        assert_eq!(
            earliest
                .as_ref()
                .expect("equal-position error retained")
                .1
                .kind(),
            ErrorKind::InvalidIndex
        );

        super::keep_earliest_failure(
            &mut earliest,
            error_at(4, ErrorKind::FieldTooLarge { limit: 3 }),
        );
        assert_eq!(
            earliest.as_ref().expect("earlier error replaces").1.kind(),
            ErrorKind::FieldTooLarge { limit: 3 }
        );
    }

    #[cfg(all(feature = "parallel", feature = "benchmarking"))]
    #[test]
    fn benchmark_parallel_builder_normalizes_zero_threads_and_reports_format_detail() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let source = b"a,b\nc,d\n";
        let built =
            CsvIndex::benchmark_build_parallel(source, crate::index::IndexOptions::default(), 0)
                .expect("zero requested threads still means one worker");
        assert_eq!(built.offsets, [0, 4]);

        CsvIndex::benchmark_build_parallel(source, crate::index::IndexOptions::default(), 8)
            .expect("eight requested threads build");
        let chunks = super::LAST_PARALLEL_CHUNKS.load(Ordering::Relaxed);
        assert!(
            chunks < 8,
            "the tiny source must expose chunk-count clamping"
        );
        assert_eq!(
            super::LAST_PARALLEL_REQUESTED_THREADS.load(Ordering::Relaxed),
            8
        );
        assert_eq!(super::LAST_PARALLEL_WORKERS.load(Ordering::Relaxed), chunks);
        assert_eq!(super::LAST_PARALLEL_SPAWNED.load(Ordering::Relaxed), chunks);

        let mut reserve_probe = Vec::with_capacity(271 * 4_096);
        for _ in 0..271 {
            reserve_probe.resize(reserve_probe.len() + 4_095, b'a');
            reserve_probe.push(b'\n');
        }
        CsvIndex::benchmark_build_parallel(
            &reserve_probe,
            crate::index::IndexOptions::default(),
            1,
        )
        .expect("reserve probe builds");
        assert_eq!(super::LAST_PARALLEL_CHUNKS.load(Ordering::Relaxed), 16);
        assert_eq!(
            super::LAST_PARALLEL_CHUNK_RESERVE.load(Ordering::Relaxed),
            17
        );

        let error = CsvIndex::benchmark_build_parallel(
            source,
            crate::index::IndexOptions {
                format: FormatOptions::CSV.comment(Some(b'#')),
                limits: Limits::DEFAULT,
            },
            2,
        )
        .expect_err("commented formats cannot use the parallel splitter");
        assert!(
            error
                .to_string()
                .contains("this format cannot be indexed in parallel"),
            "{error}"
        );
    }

    #[test]
    fn load_rejects_offsets_for_an_empty_source() {
        let path = scratch_path("empty-source");
        invalid_empty_source_index()
            .save(&path)
            .expect("test index should save");
        let error = CsvIndex::load(&path).expect_err("a malformed index must be rejected");
        assert_eq!(error.kind(), ErrorKind::InvalidIndex);
        fs::remove_file(&path).expect("scratch index should be removed");
    }

    #[test]
    fn reader_at_defensively_checks_offsets_before_slicing() {
        assert_eq!(
            invalid_empty_source_index()
                .parser_at(b"", 0)
                .expect_err("a malformed index must be rejected")
                .kind(),
            ErrorKind::InvalidIndex,
        );
    }

    #[test]
    fn parser_at_propagates_an_invalid_reconstructed_dialect() {
        // `parser_at` only validates length and hash before rebuilding a
        // parser from the stored format; a dialect that could never survive
        // `build` must still surface as a configuration error rather than
        // panic or silently succeed.
        let index = invalid_format_index();
        let error = index
            .parser_at(b"x", 0)
            .expect_err("an invalid dialect must be rejected");
        assert_eq!(error.kind(), ErrorKind::Configuration);
    }

    /// An index is built once and then kept, so any slack the final doubling
    /// left is held for as long as the index lives. A record count just past a
    /// power of two is the worst case: `Vec` will have doubled to roughly
    /// twice what is needed.
    #[test]
    fn building_an_index_leaves_no_spare_capacity() {
        const RECORDS: usize = 65;

        let mut source = String::new();
        for record in 0..RECORDS {
            use std::fmt::Write as _;
            writeln!(source, "field{record},other{record}").expect("writing to a String");
        }

        let index = CsvIndex::build(source.as_bytes(), crate::index::IndexOptions::default())
            .expect("the generated document is well formed");

        assert_eq!(index.offsets.len(), RECORDS);
        assert_eq!(
            index.offsets.capacity(),
            index.offsets.len(),
            "record offsets kept spare capacity for the life of the index"
        );
        assert_eq!(
            index.lines.capacity(),
            index.lines.len(),
            "line numbers kept spare capacity for the life of the index"
        );
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_build_positions_match_serial_build() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let cases: &[(&[u8], IndexOptions)] = &[
            (b"", IndexOptions::default()),
            (b"a,b\nc,d\n", IndexOptions::default()),
            (
                b"a,b\n\"x\nnext\",\"y\nz\"\nlast,row",
                IndexOptions::default(),
            ),
            (
                b"a,b\r\n\"x\r\nnext\",\"y\nz\"\r\nlast,row\r\n",
                IndexOptions {
                    format: FormatOptions::RFC4180,
                    limits: Limits::DEFAULT,
                },
            ),
            (b"a,b\nc,d", IndexOptions::default()),
        ];

        for &(source, options) in cases {
            let serial = CsvIndex::build(source, options).expect("serial index builds");
            let (offsets, lines) =
                build_positions_parallel(source, options, 4).expect("parallel positions build");
            assert_eq!(offsets, serial.offsets, "offsets for {source:?}");
            assert_eq!(lines, serial.lines, "lines for {source:?}");
            assert_eq!(offsets.len(), serial.len(), "length for {source:?}");
        }
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_build_positions_match_serial_across_many_chunks() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Small inputs split into only two or three chunks, so they never place
        // a boundary in the interior of a long quoted run. This document is
        // large enough that the split lands repeatedly inside multi-line quoted
        // fields, which is where a boundary scan is most likely to desynchronize.
        let mut source = Vec::new();
        for record in 0..4096 {
            match record % 4 {
                0 => source.extend_from_slice(b"plain,field,value\n"),
                1 => source.extend_from_slice(b"\"quoted\nspanning\nlines\",b,c\n"),
                2 => source.extend_from_slice(b"\"has \"\"escaped\"\" quotes\",b,c\r\n"),
                _ => source.extend_from_slice(b"x,\"y\r\nz\",\"w,v\"\n"),
            }
        }
        // Exercise the unterminated-final-record path at scale too.
        source.extend_from_slice(b"tail,without,terminator");

        let options = IndexOptions::default();
        let serial = CsvIndex::build(&source, options).expect("serial index builds");
        for threads in [2, 3, 8] {
            let (offsets, lines) = build_positions_parallel(&source, options, threads)
                .expect("parallel positions build");
            assert_eq!(offsets, serial.offsets, "offsets with {threads} threads");
            assert_eq!(lines, serial.lines, "lines with {threads} threads");
        }
    }

    /// Drives the *public* `CsvIndex::build` across the parallel dispatch.
    ///
    /// Every other parallel-index test calls `build_positions_parallel`
    /// directly, so the dispatch itself — the size comparison, the
    /// format-support predicate and the core-count guard — is not exercised by
    /// them at all: a build that silently reverted to serial on every input
    /// would leave them all green. The threshold is overridden rather than met
    /// with a real 8 MiB fixture, and `CHUNKS_PARSED` is the evidence of which
    /// builder actually ran.
    #[cfg(feature = "parallel")]
    #[test]
    fn csv_index_build_reaches_the_parallel_builder_through_the_public_api() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        let mut source = Vec::new();
        for record in 0..8_192_u32 {
            source.extend_from_slice(format!("{record},{record},{record}\n").as_bytes());
        }
        let options = IndexOptions::default();
        let expected = build_positions_serial(&source, options).expect("serial positions build");

        // One byte below the threshold must stay serial, while exactly at it
        // must engage the parallel builder.
        TEST_PARALLEL_INDEX_THRESHOLD.with(|threshold| threshold.set(Some(source.len())));
        CHUNKS_PARSED.store(0, Ordering::Relaxed);
        let below =
            CsvIndex::build(&source[..source.len() - 1], options).expect("shorter index builds");
        let below_parsed = CHUNKS_PARSED.load(Ordering::Relaxed);

        CHUNKS_PARSED.store(0, Ordering::Relaxed);
        let index = CsvIndex::build(&source, options).expect("index builds");
        let parsed = CHUNKS_PARSED.load(Ordering::Relaxed);
        let requested = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let chunks = super::LAST_PARALLEL_CHUNKS.load(Ordering::Relaxed);
        let workers = requested.min(chunks).max(1);

        // A dialect the parallel splitter cannot handle must stay serial even
        // above the threshold, which is what the support predicate is for.
        let commented = IndexOptions {
            format: FormatOptions::COMMENTED_CSV,
            limits: Limits::DEFAULT,
        };
        CHUNKS_PARSED.store(0, Ordering::Relaxed);
        let commented_index = CsvIndex::build(&source, commented).expect("index builds");
        let commented_parsed = CHUNKS_PARSED.load(Ordering::Relaxed);
        TEST_PARALLEL_INDEX_THRESHOLD.with(|threshold| threshold.set(None));

        assert!(
            parsed > 0,
            "a document at the threshold must reach the parallel builder, but no chunk was parsed"
        );
        assert_eq!(
            below_parsed, 0,
            "a document one byte below the threshold must stay serial"
        );
        assert_eq!(
            super::LAST_PARALLEL_REQUESTED_THREADS.load(Ordering::Relaxed),
            requested
        );
        assert_eq!(
            super::LAST_PARALLEL_WORKERS.load(Ordering::Relaxed),
            workers
        );
        assert_eq!(
            super::LAST_PARALLEL_SPAWNED.load(Ordering::Relaxed),
            workers
        );
        assert_eq!(
            super::LAST_PARALLEL_CHUNK_RESERVE.load(Ordering::Relaxed),
            super::chunk_entry_reserve(
                super::build_entry_estimate(&source, options.format),
                chunks
            )
        );
        assert_eq!(
            commented_parsed, 0,
            "a commented dialect must stay serial: the parallel splitter cannot handle it"
        );

        // Reaching the parallel builder is worth nothing if it disagrees.
        assert_eq!(below.offsets, expected.0);
        assert_eq!(below.lines, expected.1);
        assert_eq!(index.offsets, expected.0);
        assert_eq!(index.lines, expected.1);
        assert_eq!(index.offsets.capacity(), index.offsets.len());
        assert_eq!(index.lines.capacity(), index.lines.len());
        assert_eq!(commented_index.offsets, expected.0);
        assert_eq!(commented_index.lines, expected.1);
    }

    /// Two malformed records, in the first two chunks. With two workers those
    /// chunks are both claimed before either has finished parsing, so the
    /// failure fence cannot skip the second one and the driver genuinely has
    /// two errors to choose between — which is what makes "earliest" testable.
    /// A single malformation cannot distinguish the earliest from the latest.
    #[cfg(feature = "parallel")]
    #[test]
    fn a_parallel_build_reports_the_earliest_of_several_failures() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);

        // 32 chunks for two threads, so chunk 0 covers the first 1/32 of the
        // document and chunk 1 the next.
        let records = 32_000_u32;
        let malformed_at = [records / 64, records * 3 / 64];
        let mut source = Vec::new();
        for record in 0..records {
            if malformed_at.contains(&record) {
                source.extend_from_slice(b"\"x\"y,b,c\n");
            } else {
                source.extend_from_slice(format!("{record},{record},{record}\n").as_bytes());
            }
        }

        let options = IndexOptions::default();
        let expected = build_positions_serial(&source, options)
            .expect_err("the document contains malformed records");
        let actual = build_positions_parallel(&source, options, 2)
            .expect_err("the document contains malformed records");

        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(
            actual.location(),
            expected.location(),
            "the earliest failure is the one a serial parse reports"
        );
    }

    // P4: a worker that fails publishes the failing chunk's start byte, and
    // every worker skips chunks beginning at or after it. Before that fence a
    // malformed record in the first chunk still cost a full scan of the
    // document on every thread. The count of chunks actually parsed is the
    // direct evidence, because wall clock is not deterministic.
    #[cfg(feature = "parallel")]
    /// The malformation sits deep in the document rather than in the first
    /// chunk, so the worker that meets it is not the one that starts at byte
    /// zero. That is what makes the comparison meaningful: a worker reporting
    /// its chunk-local offset instead of the document offset, or a driver
    /// keeping the last worker's error rather than the earliest, is invisible
    /// when the failure is in chunk zero and both offsets coincide.
    ///
    /// Every thread count re-splits the document, so the failing record lands
    /// at a different position within its chunk each time.
    #[cfg(feature = "parallel")]
    #[test]
    fn a_failed_parallel_build_reports_the_same_error_as_a_serial_one() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut source = Vec::new();
        for record in 0..20_000_u32 {
            source.extend_from_slice(format!("{record},{record},{record}\n").as_bytes());
        }
        // An unexpected quote after a closing quote: malformed on every
        // dialect the parallel splitter accepts.
        source.extend_from_slice(b"\"x\"y,b,c\n");
        for record in 0..20_000_u32 {
            source.extend_from_slice(format!("{record},{record},{record}\n").as_bytes());
        }

        let options = IndexOptions::default();
        let expected = build_positions_serial(&source, options)
            .expect_err("the document contains a malformed record");

        for threads in [2, 3, 4, 8] {
            let actual = build_positions_parallel(&source, options, threads)
                .expect_err("the document contains a malformed record");
            assert_eq!(
                actual.kind(),
                expected.kind(),
                "error kind with {threads} threads"
            );
            assert_eq!(
                actual.location(),
                expected.location(),
                "error location with {threads} threads"
            );
        }
    }

    #[test]
    fn a_failed_parallel_build_stops_instead_of_indexing_the_whole_document() {
        let _serialized = PARALLEL_BUILD_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Enough records that the split produces every one of the
        // `threads * INDEX_CHUNKS_PER_THREAD` chunks it asks for.
        let mut source = Vec::new();
        source.extend_from_slice(b"\"x\"y,b,c\n");
        for record in 0..200_000_u32 {
            source.extend_from_slice(format!("{record},{record},{record}\n").as_bytes());
        }

        let threads = 4;
        let options = IndexOptions::default();

        CHUNKS_PARSED.store(0, Ordering::Relaxed);
        let error = build_positions_parallel(&source, options, threads)
            .expect_err("the first record is malformed");
        let parsed = CHUNKS_PARSED.load(Ordering::Relaxed);

        assert_eq!(error.location().byte, 3, "the earliest failure is reported");

        // Every worker in flight when the failure was published may already
        // have claimed a chunk, so up to `threads` chunks beyond the failing
        // one can still be parsed; the remaining
        // `threads * INDEX_CHUNKS_PER_THREAD` must not be.
        let total = threads * INDEX_CHUNKS_PER_THREAD;
        assert!(
            parsed <= threads * 2,
            "a failed build parsed {parsed} of {total} chunks; the failure fence is not working"
        );

        // The same document without the malformed record still indexes in
        // full, so the fence costs nothing on a valid document.
        CHUNKS_PARSED.store(0, Ordering::Relaxed);
        let (offsets, _) = build_positions_parallel(&source[9..], options, threads)
            .expect("the rest of the document is valid");
        assert_eq!(offsets.len(), 200_000);
        assert_eq!(
            CHUNKS_PARSED.load(Ordering::Relaxed),
            total,
            "a valid document must still be parsed chunk for chunk"
        );
    }

    #[test]
    fn parser_at_reader_propagates_an_invalid_reconstructed_dialect() {
        // The streaming counterpart of the test above: `parser_at_reader`
        // reconstructs a `IoParser` from the same stored format, so it
        // must reject the invalid dialect the same way.
        let index = invalid_format_index();
        let error = index
            .parser_at_reader(std::io::Cursor::new(b"x".to_vec()), 0)
            .expect_err("an invalid dialect must be rejected");
        assert_eq!(error.kind(), ErrorKind::Configuration);
    }

    #[test]
    fn csv_index_additional_coverage() {
        let empty_idx =
            CsvIndex::build(b"", crate::index::IndexOptions::default()).expect("empty build");
        assert!(empty_idx.is_empty());
        assert_eq!(empty_idx.record_offset(0), None);
        assert_eq!(empty_idx.record_line(0), None);
        assert_eq!(empty_idx.format(), FormatOptions::CSV);
        assert_eq!(empty_idx.limits(), Limits::DEFAULT);

        let data = b"col1,col2\nval1,val2\nval3,val4\n";
        let idx =
            CsvIndex::build(data, crate::index::IndexOptions::default()).expect("valid build");
        assert_eq!(idx.record_offset(0), Some(0));
        assert_eq!(idx.record_line(0), Some(1));
        assert!(idx.validate_reader(std::io::Cursor::new(data)).is_ok());

        // Test estimate calculation with short dense endings and non-terminated endings
        let dense = vec![b'\n'; 2 * 1024 * 1024];
        let est = super::build_entry_estimate(&dense, FormatOptions::CSV);
        assert_eq!(est, 0);

        let mut large_no_nl = vec![b'a'; 1024 * 1024 + 100];
        large_no_nl[500] = b'\n';
        large_no_nl[1000] = b'\n';
        let est2 = super::build_entry_estimate(&large_no_nl, FormatOptions::CSV);
        assert!(est2 > 0);

        // Test CsvIndex::create and reader
        let mut index_buf = std::io::Cursor::new(Vec::new());
        let reader = CsvIndex::create(
            std::io::Cursor::new(data),
            &mut index_buf,
            crate::index::IndexOptions::default(),
        )
        .expect("create index");
        assert_eq!(reader.len(), 3);

        // Test corrupted header checksum in CsvIndex::load
        let path_hdr = scratch_path("corrupted-header-checksum");
        idx.save(&path_hdr).expect("save index");
        let mut file_bytes_hdr = fs::read(&path_hdr).expect("read index file");
        // Corrupt the entries checksum (second to last 16-byte block)
        let entries_pos = file_bytes_hdr.len() - 25;
        file_bytes_hdr[entries_pos] ^= 0xFF;
        fs::write(&path_hdr, &file_bytes_hdr).expect("write corrupted entries checksum");
        assert!(CsvIndex::load(&path_hdr).is_err());

        // Corrupt the very last byte (part of header checksum)
        let last = file_bytes_hdr.len() - 1;
        file_bytes_hdr[last] ^= 0xFF;
        fs::write(&path_hdr, &file_bytes_hdr).expect("write corrupted header checksum");
        assert!(CsvIndex::load(&path_hdr).is_err());
        fs::remove_file(&path_hdr).expect("remove scratch");

        // Test CsvIndex::create failure with failing writer
        struct FailingIndexWriter;
        impl std::io::Read for FailingIndexWriter {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
        }
        impl std::io::Write for FailingIndexWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
            }
        }
        impl std::io::Seek for FailingIndexWriter {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                Ok(0)
            }
        }
        assert!(
            CsvIndex::create(
                std::io::Cursor::new(data),
                FailingIndexWriter,
                IndexOptions::default()
            )
            .is_err()
        );

        // Test legacy v8 format in CsvIndex::load
        let path_v8 = scratch_path("v8-index");
        let mut v8_header = idx.encode_header(idx.len() as u64);
        v8_header[8..12].copy_from_slice(&8_u32.to_le_bytes()); // set version to 8
        let mut v8_hasher = xxhash_rust::xxh3::Xxh3::new();
        v8_hasher.update(&v8_header);
        let mut v8_bytes = v8_header;
        for (&offset, &line) in idx.offsets.iter().zip(&idx.lines) {
            let enc_off = offset.to_le_bytes();
            let enc_line = line.to_le_bytes();
            v8_hasher.update(&enc_off);
            v8_hasher.update(&enc_line);
            v8_bytes.extend_from_slice(&enc_off);
            v8_bytes.extend_from_slice(&enc_line);
        }
        let v8_checksum = v8_hasher.digest128().to_le_bytes();
        v8_bytes.extend_from_slice(&v8_checksum);
        fs::write(&path_v8, &v8_bytes).expect("write v8 index");
        let loaded_v8 = CsvIndex::load(&path_v8).expect("load v8 index");
        assert_eq!(loaded_v8.len(), idx.len());

        // Corrupt v8 checksum
        let last_idx = v8_bytes.len() - 1;
        v8_bytes[last_idx] ^= 0xFF;
        fs::write(&path_v8, &v8_bytes).expect("write corrupted v8 index");
        assert!(CsvIndex::load(&path_v8).is_err());
        fs::remove_file(&path_v8).expect("remove scratch");

        // Test build_path and create_path
        let src_path = scratch_path("src-csv.csv");
        let idx_path = scratch_path("dst-idx.idx");
        fs::write(&src_path, data).expect("write src");
        let built_idx = CsvIndex::build_path(&src_path, &idx_path, IndexOptions::default())
            .expect("build_path");
        assert_eq!(built_idx.len(), 3);
        let p = built_idx
            .parser_at_path(&src_path, 1)
            .expect("parser_at_path");
        assert_eq!(p.location().record, 1);

        // Load bad magic
        let path_bad_magic = scratch_path("bad-magic");
        fs::write(&path_bad_magic, [0u8; 100]).expect("write bad magic");
        assert!(CsvIndex::load(&path_bad_magic).is_err());
        fs::remove_file(&path_bad_magic).expect("remove scratch");

        #[cfg(feature = "parallel")]
        {
            use crate::parallel::split::Boundary;
            let invalid_options = IndexOptions {
                format: FormatOptions::CSV.delimiter(b'"'),
                limits: Limits::DEFAULT,
            };
            assert!(build_positions_parallel(data, invalid_options, 2).is_err());
            assert!(
                super::parse_index_chunk(
                    data,
                    invalid_options,
                    &[],
                    0,
                    Boundary {
                        byte: 0,
                        line: 1,
                        record: 0
                    },
                    0
                )
                .is_err()
            );

            // parse_index_chunk with start >= end to hit immediate break
            let chunk_b = [
                Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
            ];
            let ch_res = super::parse_index_chunk(
                b"a,b\n",
                IndexOptions::default(),
                &chunk_b,
                0,
                Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                1,
            )
            .unwrap();
            assert_eq!(ch_res.offsets.len(), 0);
        }

        // Test create_path with nonexistent source path and nonexistent destination
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing_source = directory.path().join("source.csv");
        let missing_index = directory.path().join("missing").join("index.idx");
        assert!(
            CsvIndex::create_path(&missing_source, &idx_path, IndexOptions::default()).is_err()
        );
        assert!(CsvIndex::create_path(&src_path, &missing_index, IndexOptions::default()).is_err());
        assert!(CsvIndex::build_path(&missing_source, &idx_path, IndexOptions::default()).is_err());

        // Test load on 0-byte file (UnexpectedEof) and truncated payload
        let path_0byte = scratch_path("0byte.idx");
        fs::write(&path_0byte, []).expect("write 0 byte");
        assert!(CsvIndex::load(&path_0byte).is_err());
        fs::remove_file(&path_0byte).expect("remove 0 byte");

        // Truncated payload length mismatch
        let path_trunc = scratch_path("trunc.idx");
        let header_claiming_entries = idx.encode_header(100);
        fs::write(&path_trunc, header_claiming_entries).expect("write header only");
        assert!(CsvIndex::load(&path_trunc).is_err());
        fs::remove_file(&path_trunc).expect("remove trunc");

        let mut created_reader =
            CsvIndex::create_path(&src_path, &idx_path, IndexOptions::default())
                .expect("create_path");
        assert_eq!(created_reader.len(), 3);
        let mut p2 = created_reader
            .parser_at_path(&src_path, 2)
            .expect("parser_at_path");
        assert_eq!(
            p2.next_line()
                .unwrap()
                .unwrap()
                .record()
                .unwrap()
                .get_str(0)
                .unwrap(),
            Some("val3")
        );

        fs::remove_file(&src_path).expect("remove src");
        fs::remove_file(&idx_path).expect("remove idx");

        #[cfg(feature = "benchmarking")]
        {
            let _ = CsvIndex::benchmark_build_serial(data, crate::index::IndexOptions::default());
            let _ = CsvIndex::benchmark_build_serial(
                b"\"unterminated",
                crate::index::IndexOptions::default(),
            );
        }

        #[cfg(all(feature = "benchmarking", feature = "parallel"))]
        {
            let _ = CsvIndex::benchmark_build_parallel(b"a,b\n1,2\n", IndexOptions::default(), 2);
            let _ =
                CsvIndex::benchmark_build_parallel(b"\"unterminated", IndexOptions::default(), 2);
            let commented = IndexOptions {
                format: FormatOptions::COMMENTED_CSV,
                limits: Limits::DEFAULT,
            };
            let _ = CsvIndex::benchmark_build_parallel(b"# comment\na,b\n", commented, 2);
        }

        // Test build with invalid dialect
        let invalid_opts = IndexOptions {
            format: FormatOptions::CSV.delimiter(b'"'),
            limits: Limits::DEFAULT,
        };
        assert!(CsvIndex::build(data, invalid_opts).is_err());

        // Test parser_at with out-of-range record
        assert!(idx.parser_at(data, 100).is_err());
        assert!(
            idx.parser_at_reader(std::io::Cursor::new(data), 100)
                .is_err()
        );

        // Test bound source parser_at with out-of-range record
        let bound = idx.bind(data).expect("bind source");
        assert!(bound.parser_at(100).is_err());

        // Test create with invalid options and malformed CSV
        let mut idx_cursor = std::io::Cursor::new(Vec::new());
        assert!(
            CsvIndex::create(std::io::Cursor::new(data), &mut idx_cursor, invalid_opts).is_err()
        );
        assert!(
            CsvIndex::create(
                std::io::Cursor::new(b"\"unterminated quote"),
                &mut idx_cursor,
                IndexOptions::default()
            )
            .is_err()
        );

        // Test build_path with malformed CSV
        let mal_src = scratch_path("mal-src.csv");
        let mal_dst = scratch_path("mal-dst.idx");
        fs::write(&mal_src, b"\"unterminated quote").expect("write mal");
        assert!(CsvIndex::build_path(&mal_src, &mal_dst, IndexOptions::default()).is_err());
        fs::remove_file(&mal_src).expect("remove mal src");

        // Test build_positions_serial and parse_index_chunk with malformed CSV
        assert!(super::build_positions_serial(b"\"unterminated", IndexOptions::default()).is_err());
        let directory = tempfile::tempdir().expect("temporary directory");
        assert!(CsvIndex::load(directory.path()).is_err());
        assert!(
            idx.save(directory.path().join("missing").join("test.idx"))
                .is_err()
        );

        #[cfg(feature = "parallel")]
        {
            assert!(
                super::parse_index_chunk(
                    b"\"unterminated",
                    IndexOptions::default(),
                    &[crate::parallel::split::Boundary {
                        byte: 0,
                        line: 1,
                        record: 0
                    }],
                    0,
                    crate::parallel::split::Boundary {
                        byte: 0,
                        line: 1,
                        record: 0
                    },
                    1
                )
                .is_err()
            );
        }

        // Test check_entry failure during load (decreasing offset / line)
        let path_corrupt_entry = scratch_path("corrupted-entry");
        let mut corrupt_idx_bytes = idx.encode_header(idx.len() as u64);
        // Put two entries where the second has decreasing offset
        let off1 = 100u64.to_le_bytes();
        let l1 = 1u64.to_le_bytes();
        let off2 = 50u64.to_le_bytes(); // decreasing!
        let l2 = 2u64.to_le_bytes();
        corrupt_idx_bytes.extend_from_slice(&off1);
        corrupt_idx_bytes.extend_from_slice(&l1);
        corrupt_idx_bytes.extend_from_slice(&off2);
        corrupt_idx_bytes.extend_from_slice(&l2);
        let mut entries_hash = xxhash_rust::xxh3::Xxh3::new();
        entries_hash.update(&off1);
        entries_hash.update(&l1);
        entries_hash.update(&off2);
        entries_hash.update(&l2);
        corrupt_idx_bytes.extend_from_slice(&entries_hash.digest128().to_le_bytes());
        corrupt_idx_bytes
            .extend_from_slice(&hash_bytes(&corrupt_idx_bytes[..super::FIXED_HEADER_BYTES]));
        // Fix up count in header to 2
        corrupt_idx_bytes[super::FIXED_HEADER_BYTES - 8..super::FIXED_HEADER_BYTES]
            .copy_from_slice(&2u64.to_le_bytes());
        fs::write(&path_corrupt_entry, &corrupt_idx_bytes).expect("write corrupt entries");
        assert!(CsvIndex::load(&path_corrupt_entry).is_err());
        fs::remove_file(&path_corrupt_entry).expect("remove scratch");

        // Test save and load roundtrip
        let test_idx_file = scratch_path("test_save_load.idx");
        idx.save(&test_idx_file).unwrap();
        let loaded = CsvIndex::load(&test_idx_file).unwrap();
        assert_eq!(loaded.len(), idx.len());
        fs::remove_file(&test_idx_file).unwrap();

        // Test CsvIndex::create returning CsvIndexReader
        let mut idx_out = std::io::Cursor::new(Vec::new());
        let reader = CsvIndex::create(
            std::io::Cursor::new(b"a,b\n1,2\n3,4\n"),
            &mut idx_out,
            IndexOptions::default(),
        )
        .unwrap();
        assert_eq!(reader.len(), 3);

        // Test parser_at_reader and parser_at
        let cur_src = std::io::Cursor::new(data);
        let mut p_at = idx.parser_at_reader(cur_src, 0).unwrap();
        assert!(p_at.advance().unwrap());

        let mut p_mem = idx.parser_at(data, 0).unwrap();
        assert!(p_mem.advance().unwrap());

        #[cfg(feature = "parallel")]
        {
            TEST_PARALLEL_INDEX_THRESHOLD.with(|c| c.set(Some(16)));
            let par_idx = CsvIndex::build(data, IndexOptions::default()).unwrap();
            assert_eq!(par_idx.len(), idx.len());
            TEST_PARALLEL_INDEX_THRESHOLD.with(|c| c.set(None));
        }
    }
}

use super::*;

/// Lazily read record positions held in a persisted index.
///
/// [`CsvIndex`] materializes the whole location table, which costs 16 bytes per
/// record. A `CsvIndexReader` keeps the index on disk and reads one position
/// per lookup instead, so random access over a source with billions of records
/// needs only constant memory. Opening validates the header and overall length;
/// call [`Self::verify`] to check the whole-index checksum, which necessarily
/// reads every position.
///
/// ```no_run
/// use coseva::index::CsvIndexReader;
///
/// let mut reader = CsvIndexReader::open("huge.idx")?;
/// assert!(reader.record_offset(4_000_000)?.is_some());
///
/// let mut parser = reader.parser_at_path("huge.csv", 4_000_000)?;
/// let mut line = parser
///     .next_line()?
///     .ok_or_else(|| std::io::Error::other("expected indexed record"))?;
/// assert_eq!(line.record()?.index(), 4_000_000);
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsvIndexReader<R> {
    index: R,
    version: u32,
    source_len: u64,
    source_hash: [u8; HASH_BYTES],
    format: FormatOptions,
    limits: Limits,
    count: u64,
}

impl CsvIndexReader<File> {
    /// Open a persisted index for lazy lookups.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// let index_path = directory.path().join("cities.idx");
    /// std::fs::write(&source_path, b"city,population\nBoston,650706\nDenver,715522\n")?;
    /// CsvIndex::build_path(&source_path, &index_path, IndexOptions::default())?;
    ///
    /// let mut reader = CsvIndexReader::open(&index_path)?;
    /// assert_eq!(reader.len(), 3);
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_path(&source_path, 2)?;
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
    /// Returns an I/O, version, truncation, or overflow error.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let index = File::open(path).map_err(Error::io_at_start)?;
        Self::new(index)
    }
}

impl<R: Read + Seek> CsvIndexReader<R> {
    /// Read the header of an index and prepare it for lazy lookups.
    ///
    /// ```
    /// use std::io::Cursor;
    /// use coseva::index::{CsvIndex, CsvIndexReader, IndexOptions};
    ///
    /// let source = &b"city,population\nBoston,650706\nDenver,715522\n"[..];
    /// let mut reader = CsvIndex::create(source, Cursor::new(Vec::new()), IndexOptions::default())?;
    /// let bytes = reader.into_inner().into_inner();
    ///
    /// let mut reader = CsvIndexReader::new(Cursor::new(bytes))?;
    /// assert_eq!(reader.len(), 3);
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_reader(Cursor::new(source), 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the reader
    ///
    /// `index` is taken by value, and [`into_inner`](Self::into_inner) hands it back. A
    /// caller that must keep the reader can pass `&mut reader` instead, since
    /// `&mut R` implements [`Read`] wherever `R: Read` does.
    ///
    /// # Errors
    ///
    /// Returns an I/O, version, truncation, or overflow error.
    pub fn new(index: R) -> Result<Self, Error> {
        let mut index = index;
        index_seek(&mut index, SeekFrom::Start(0))?;
        let mut header = [u8::default(); FIXED_HEADER_BYTES];
        read_index_exact(&mut index, &mut header)?;
        let header = decode_header(&header)?;
        let expected_len = payload_len(header.count)?
            .checked_add(trailer_bytes(header.version) as u64)
            .ok_or_else(|| Error::detailed(ErrorKind::InvalidIndex, INDEX_LENGTH_OVERFLOW))?;
        let actual_len = index_seek(&mut index, SeekFrom::End(0))?;
        if expected_len != actual_len {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                LOCATION_TABLE_LENGTH_MISMATCH,
            ));
        }
        Ok(Self {
            index,
            version: header.version,
            source_len: header.source_len,
            source_hash: header.source_hash,
            format: header.format,
            limits: header.limits,
            count: header.count,
        })
    }

    /// Number of indexed records.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.count
    }

    /// Whether no records were indexed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
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

    /// Length in bytes of the source this index was built from.
    #[must_use]
    pub const fn source_len(&self) -> u64 {
        self.source_len
    }

    /// Byte offset of one zero-based record, read from the index on demand.
    ///
    /// # Errors
    ///
    /// Returns an I/O error, or [`ErrorKind::InvalidIndex`] when the stored
    /// position is not consistent with the indexed source.
    pub fn record_offset(&mut self, record: usize) -> Result<Option<u64>, Error> {
        Ok(self.entry(record)?.map(|(offset, _line)| offset))
    }

    /// One-based physical line of one zero-based record.
    ///
    /// # Errors
    ///
    /// Returns an I/O error, or [`ErrorKind::InvalidIndex`] when the stored
    /// position is not consistent with the indexed source.
    pub fn record_line(&mut self, record: usize) -> Result<Option<u64>, Error> {
        Ok(self.entry(record)?.map(|(_offset, line)| line))
    }

    /// Absolute parser location of one zero-based record.
    ///
    /// # Errors
    ///
    /// Returns an I/O error, [`ErrorKind::RecordOutOfRange`], or
    /// [`ErrorKind::InvalidIndex`] for an inconsistent stored position.
    pub fn location(&mut self, record: usize) -> Result<Location, Error> {
        let (offset, line) = self
            .entry(record)?
            .ok_or_else(|| record_out_of_range(record))?;
        entry_location(offset, line, self.source_len, record)
    }

    /// Validate that this index belongs to the bytes produced by `source`.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::SourceMismatch`] when length or content differs,
    /// or an I/O error when the source cannot be read.
    pub fn validate_reader(&self, source: impl Read) -> Result<(), Error> {
        validate_identity(source, self.source_len, self.source_hash)
    }

    /// Verify the whole-index checksum.
    ///
    /// Every stored position is read, so this costs one pass over the index.
    ///
    /// # Errors
    ///
    /// Returns an I/O error or [`ErrorKind::InvalidIndex`] on a mismatch.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn verify(&mut self) -> Result<(), Error> {
        index_seek(&mut self.index, SeekFrom::Start(0))?;
        if self.version < 9 {
            let payload_len = payload_len(self.count)?;
            let checksum = hash_payload(&mut self.index, payload_len)?;
            let mut stored = [u8::default(); CHECKSUM_BYTES];
            read_index_exact(&mut self.index, &mut stored)?;
            if stored != checksum {
                return Err(Error::detailed(
                    ErrorKind::InvalidIndex,
                    INDEX_CHECKSUM_MISMATCH,
                ));
            }
            return Ok(());
        }
        // Version 9 authenticates entries and header independently: the
        // entries region is streamed straight into a hash, and the header is
        // reread and hashed on its own, so neither check needs the other or
        // the whole payload as one unit.
        let mut header = [u8::default(); FIXED_HEADER_BYTES];
        read_index_exact(&mut self.index, &mut header)?;
        let entries_checksum = hash_entries(&mut self.index, self.count)?;
        let mut stored_entries = [u8::default(); CHECKSUM_BYTES];
        read_index_exact(&mut self.index, &mut stored_entries)?;
        if stored_entries != entries_checksum {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                INDEX_ENTRIES_CHECKSUM_MISMATCH,
            ));
        }
        let mut stored_header = [u8::default(); CHECKSUM_BYTES];
        read_index_exact(&mut self.index, &mut stored_header)?;
        if stored_header != hash_bytes(&header) {
            return Err(Error::detailed(
                ErrorKind::InvalidIndex,
                INDEX_HEADER_CHECKSUM_MISMATCH,
            ));
        }
        Ok(())
    }

    /// Create an [`IoParser`] beginning at one indexed record.
    ///
    /// Only the source length is checked; call [`Self::validate_reader`] once
    /// beforehand to confirm the full source identity.
    ///
    /// ```
    /// use std::io::Cursor;
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let source = &b"city,population\nBoston,650706\nDenver,715522\n"[..];
    /// let mut reader = CsvIndex::create(source, Cursor::new(Vec::new()), IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_reader(Cursor::new(source), 2)?;
    /// let mut line = parser
    ///     .next_line()?
    ///     .ok_or_else(|| std::io::Error::other("expected record 2"))?;
    /// assert_eq!(line.record()?.get_str(0)?, Some("Denver"));
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error for a source-length mismatch, an out-of-range record,
    /// an invalid reconstructed parser configuration, or a failed seek.
    pub fn parser_at_reader<S: Read + Seek>(
        &mut self,
        source: S,
        record: usize,
    ) -> Result<IoParser<S>, Error> {
        let location = self.location(record)?;
        io_parser_at(source, self.source_len, self.format, self.limits, location)
    }

    /// Open a file and create an [`IoParser`] at one indexed record.
    ///
    /// ```
    /// use coseva::index::{CsvIndex, IndexOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let source_path = directory.path().join("cities.csv");
    /// let index_path = directory.path().join("cities.idx");
    /// std::fs::write(&source_path, b"city,population\nBoston,650706\nDenver,715522\n")?;
    ///
    /// let mut reader = CsvIndex::create_path(&source_path, &index_path, IndexOptions::default())?;
    ///
    /// // Jump straight to record 2 (the second data row) and confirm it's the right one.
    /// let mut parser = reader.parser_at_path(&source_path, 2)?;
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
    pub fn parser_at_path(
        &mut self,
        source_path: impl AsRef<Path>,
        record: usize,
    ) -> Result<IoParser<File>, Error> {
        let source = File::open(source_path).map_err(Error::io_at_start)?;
        self.parser_at_reader(source, record)
    }

    /// Consume this reader and hand back the underlying index.
    pub fn into_inner(self) -> R {
        self.index
    }

    /// Read one stored position, validating it against the indexed source.
    fn entry(&mut self, record: usize) -> Result<Option<(u64, u64)>, Error> {
        // `usize` is never wider than `u64` on any supported target.
        let record = widen(record);
        if record >= self.count {
            return Ok(None);
        }
        // `new` proved the whole location table fits the index, so neither the
        // scaled record nor the header offset can overflow here.
        let position = FIXED_HEADER_BYTES as u64 + record * 16;
        index_seek(&mut self.index, SeekFrom::Start(position))?;
        let mut encoded = [u8::default(); 16];
        read_index_exact(&mut self.index, &mut encoded)?;
        let mut field = [u8::default(); 8];
        field.copy_from_slice(&encoded[..8]);
        let offset = u64::from_le_bytes(field);
        field.copy_from_slice(&encoded[8..]);
        let line = u64::from_le_bytes(field);
        check_first_entry(offset, line, self.source_len)?;
        Ok(Some((offset, line)))
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn index_bytes(source: &[u8]) -> Vec<u8> {
        CsvIndex::create(
            std::io::Cursor::new(source),
            std::io::Cursor::new(Vec::new()),
            crate::index::IndexOptions::default(),
        )
        .expect("index creation")
        .into_inner()
        .into_inner()
    }

    #[test]
    fn reader_reports_exact_length_and_checksum_failures() {
        let bytes = index_bytes(b"a,b\nc,d\n");

        let mut wrong_len = bytes.clone();
        wrong_len.push(0xAA);
        let error = CsvIndexReader::new(std::io::Cursor::new(wrong_len))
            .expect_err("trailing bytes must be rejected");
        assert!(
            error
                .to_string()
                .contains("location table length does not match"),
            "{error}"
        );

        let mut bad_entries = bytes.clone();
        let entries_checksum = bad_entries.len() - 2 * CHECKSUM_BYTES;
        bad_entries[entries_checksum] ^= 0x80;
        let mut reader =
            CsvIndexReader::new(std::io::Cursor::new(bad_entries)).expect("length is valid");
        let error = reader
            .verify()
            .expect_err("entries checksum must be checked");
        assert!(
            error
                .to_string()
                .contains("index entries checksum does not match"),
            "{error}"
        );

        let mut bad_header = bytes.clone();
        let last = bad_header.len() - 1;
        bad_header[last] ^= 0x80;
        let mut reader =
            CsvIndexReader::new(std::io::Cursor::new(bad_header)).expect("length is valid");
        let error = reader
            .verify()
            .expect_err("header checksum must be checked");
        assert!(
            error
                .to_string()
                .contains("index header checksum does not match"),
            "{error}"
        );

        let count = 2_u64;
        let payload = FIXED_HEADER_BYTES + count as usize * 16;
        let mut legacy = bytes[..payload].to_vec();
        legacy[8..12].copy_from_slice(&MIN_VERSION.to_le_bytes());
        let checksum = hash_payload(&legacy[..], payload as u64).expect("legacy checksum");
        legacy.extend_from_slice(&checksum);
        let last = legacy.len() - 1;
        legacy[last] ^= 0x80;
        let mut reader =
            CsvIndexReader::new(std::io::Cursor::new(legacy)).expect("legacy length is valid");
        let error = reader
            .verify()
            .expect_err("legacy checksum must be checked");
        assert!(
            error.to_string().contains("index checksum does not match"),
            "{error}"
        );
    }

    #[test]
    fn reader_location_accepts_the_last_source_byte() {
        let mut reader =
            CsvIndexReader::new(std::io::Cursor::new(index_bytes(b"\nx"))).expect("valid index");
        assert_eq!(
            reader
                .location(1)
                .expect("second record starts at final byte"),
            Location {
                byte: 1,
                line: 2,
                record: 1,
                field: 0,
            }
        );
        assert_eq!(
            reader.record_offset(usize::MAX).expect("out of range"),
            None
        );
    }

    #[test]
    fn test_csv_index_reader_coverage() {
        let data = b"col1,col2\nval1,val2\n";
        let index_buf = std::io::Cursor::new(Vec::new());
        let mut reader = CsvIndex::create(
            std::io::Cursor::new(data),
            index_buf,
            crate::index::IndexOptions::default(),
        )
        .unwrap();
        assert_eq!(reader.len(), 2);
        assert!(!reader.is_empty());
        assert_eq!(reader.format(), FormatOptions::CSV);
        assert_eq!(reader.limits(), Limits::DEFAULT);
        assert_eq!(reader.source_len(), data.len() as u64);

        assert_eq!(reader.record_offset(0).unwrap(), Some(0));
        assert_eq!(reader.record_line(0).unwrap(), Some(1));
        assert_eq!(reader.record_offset(10).unwrap(), None);
        assert_eq!(reader.record_line(10).unwrap(), None);
        assert!(reader.location(10).is_err());
        assert!(reader.validate_reader(std::io::Cursor::new(data)).is_ok());
        assert!(reader.verify().is_ok());

        // Test parser_at_reader
        let mut p = reader
            .parser_at_reader(std::io::Cursor::new(data), 0)
            .unwrap();
        assert_eq!(
            p.next_line()
                .unwrap()
                .unwrap()
                .record()
                .unwrap()
                .get_str(0)
                .unwrap(),
            Some("col1")
        );

        // Test v8 verify
        let bytes = reader.into_inner().into_inner();
        let mut v8_bytes = bytes.clone();
        v8_bytes[8..12].copy_from_slice(&8_u32.to_le_bytes()); // set v8
        // Payload length = FIXED_HEADER_BYTES + 2 * 16 = 80 + 32 = 112 (or whatever FIXED_HEADER_BYTES is)
        let payload_len = FIXED_HEADER_BYTES as u64 + 2 * 16;
        let v8_hash = hash_payload(&v8_bytes[..payload_len as usize], payload_len).unwrap();
        let mut v8_file = v8_bytes[..payload_len as usize].to_vec();
        v8_file.extend_from_slice(&v8_hash);
        let mut v8_reader = CsvIndexReader::new(std::io::Cursor::new(v8_file.clone())).unwrap();
        assert!(v8_reader.verify().is_ok());

        // Corrupt v8 verify
        let mut bad_v8_file = v8_file;
        let last = bad_v8_file.len() - 1;
        bad_v8_file[last] ^= 0xFF;
        let mut bad_v8_reader = CsvIndexReader::new(std::io::Cursor::new(bad_v8_file)).unwrap();
        assert!(bad_v8_reader.verify().is_err());

        // Corrupt v9 entries checksum
        let mut bad_v9_entries = bytes.clone();
        let ent_chk_pos = bad_v9_entries.len() - 32;
        bad_v9_entries[ent_chk_pos] ^= 0xFF;
        let mut bad_v9_reader = CsvIndexReader::new(std::io::Cursor::new(bad_v9_entries)).unwrap();
        assert!(bad_v9_reader.verify().is_err());

        // Corrupt v9 header checksum
        let mut bad_v9_hdr = bytes.clone();
        let hdr_chk_pos = bad_v9_hdr.len() - 1;
        bad_v9_hdr[hdr_chk_pos] ^= 0xFF;
        let mut bad_v9_hdr_reader = CsvIndexReader::new(std::io::Cursor::new(bad_v9_hdr)).unwrap();
        assert!(bad_v9_hdr_reader.verify().is_err());
    }
}

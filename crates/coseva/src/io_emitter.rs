#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;
use core::mem;
#[cfg(feature = "std")]
use std::fs::{File, OpenOptions};
#[cfg(feature = "std")]
use std::io;
#[cfg(feature = "std")]
use std::io::{Read, Seek, SeekFrom};
#[cfg(feature = "std")]
use std::path::Path;

use crate::byte_record::ByteRecord;
use crate::config::{
    Dialect, EmitOptions, FieldCount, FormatOptions, Headers, Nulls, ParseOptions, Quoting,
    RecordEnding, WriteBom,
};
use crate::encoding::CsvEncode;
use crate::error::{Error, ErrorKind, Location};
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::into_inner_error::IntoInnerError;
use crate::io_parser::IoParser;
use crate::push_emitter::PushEmitter;
use crate::text_record::TextRecord;

/// Write all of `buffer` to `output`, reporting how much the sink confirmed.
///
/// [`io::Write::write_all`] discards that count, which leaves a caller unable
/// to tell an error that wrote nothing from one that wrote most of the buffer.
/// The retry semantics are otherwise identical: [`io::ErrorKind::Interrupted`]
/// is retried, and a zero-length write is treated as a failure to make
/// progress.
fn write_confirmed<W: io::Write>(output: &mut W, buffer: &[u8]) -> Result<(), (usize, io::Error)> {
    let mut written = 0;
    while written < buffer.len() {
        match output.write(&buffer[written..]) {
            Ok(0) => {
                return Err((
                    written,
                    io::Error::new(
                        io::ErrorKind::WriteZero,
                        "CSV sink accepted no bytes of the buffered output",
                    ),
                ));
            }
            Ok(count) => written += count,
            // gamma::skip(match_guard.negate, match_guard.always_true, relational.eq_to_ne, reason = "mutation retries permanent write errors and causes non-termination")
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err((written, error)),
        }
    }
    Ok(())
}

/// CSV emitter for an arbitrary [`io::Write`] destination.
///
/// Use this to write a CSV document to a file, a socket, or anything else
/// that implements [`io::Write`]. Output is buffered internally, so a sink
/// sees one large write per batch of records rather than one write per record;
/// wrapping the sink in a `BufWriter` first is redundant.
///
/// Buffered output must reach the sink before the emitter is discarded. Call
/// [`Self::flush`] or [`Self::into_inner`] to find out whether the final write
/// succeeded; dropping the emitter flushes on a best-effort basis and cannot
/// report a failure.
///
/// ```
/// use coseva::IoEmitter;
/// use coseva::config::EmitOptions;
/// use coseva::format::Csv;
///
/// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
/// emitter.emit_record(["city", "population"])?;
/// emitter.emit_record(["Washington, D.C.", "689545"])?;
///
/// assert_eq!(
///     emitter.into_inner()?,
///     b"city,population\n\"Washington, D.C.\",689545\n",
/// );
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
#[derive(Debug)]
pub struct IoEmitter<W: io::Write, F: CsvFormat = Dynamic> {
    /// The encoding core, whose buffer holds records awaiting a write.
    core: PushEmitter<F>,
    /// The sink, taken only by [`IoEmitter::into_inner`] and friends.
    ///
    /// Held in an `Option` so that this type can implement [`Drop`], which a
    /// buffered emitter needs: dropping one that still holds buffered records
    /// would otherwise truncate the output silently.
    output: Option<W>,
    /// Buffered byte count at which [`IoEmitter::drain`] writes to the sink.
    ///
    /// A record is appended whole, so a record larger than the threshold grows
    /// the buffer for as long as it takes to drain it and the capacity is
    /// handed back immediately afterwards; the resident cost is therefore the
    /// threshold plus the largest record in flight, never the whole document.
    threshold: usize,
    /// Bytes drained by the previous drain, used as a capacity floor.
    ///
    /// Shrinking straight back to `threshold` after every drain reallocates on
    /// every record when the caller's records are larger than their configured
    /// threshold. Keeping the previous drain's size means sustained large
    /// records reuse their capacity, while a single oversized record is
    /// released one drain later.
    previous_drain: usize,
    /// Latched once a write fails, since a partial record makes the output
    /// unusable and every later call must refuse rather than compound it.
    failed: bool,
}

#[cfg(feature = "std")]
impl<W: io::Write, F: StaticFormat> IoEmitter<W, F> {
    /// Create an emitter wrapping `output`, encoding the format `F`.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the writer
    ///
    /// `output` is taken by value, and [`into_inner`](Self::into_inner) hands
    /// it back. A caller that cannot give the writer up can pass `&mut writer`
    /// instead, since `&mut W` implements [`std::io::Write`] wherever
    /// `W: Write` does.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut out = Vec::new();
    ///
    /// // The emitter borrows the writer rather than consuming it.
    /// {
    ///     let mut emitter = IoEmitter::<_, Csv>::new(&mut out, EmitOptions::new())?;
    ///     emitter.emit_record(["Boston", "650706"])?;
    ///     emitter.flush()?;
    /// }
    ///
    /// // `out` was never surrendered, so it can be appended to here.
    /// out.extend_from_slice(b"Denver,715522\n");
    /// assert_eq!(out, b"Boston,650706\nDenver,715522\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the encode options are invalid.
    pub fn new(output: W, options: EmitOptions) -> Result<Self, Error> {
        Self::build(output, F::FORMAT, options)
    }
}

impl<W: io::Write> IoEmitter<W, Dynamic> {
    /// Create an emitter for an explicit format and encode options.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions};
    ///
    /// let mut emitter = IoEmitter::with_options(
    ///     Vec::new(),
    ///     FormatOptions::CSV.delimiter(b';'),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.into_inner()?, b"Boston;650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Borrowing the writer
    ///
    /// `output` is taken by value, and [`into_inner`](Self::into_inner) hands it back. A
    /// caller that must keep the writer can pass `&mut writer` instead, since
    /// `&mut W` implements [`std::io::Write`] wherever `W: Write` does.
    ///
    /// # Errors
    ///
    /// Returns an error when the format is ambiguous or the buffer capacity is invalid.
    pub fn with_options(
        output: W,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        Self::build(output, format, options)
    }
}

impl<W: io::Write, F: CsvFormat> IoEmitter<W, F> {
    /// The shared fallible constructor behind `new` and `with_options`.
    fn build(output: W, format: FormatOptions, options: EmitOptions) -> Result<Self, Error> {
        options.validate_buffered(format)?;
        Ok(Self::from_config(
            output,
            format.dialect,
            format.quoting,
            format.write_bom,
            format.nulls,
            options.field_count_policy(),
            options.writes_headers(),
            options.capacity(),
        ))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the internal constructor receives the validated writer configuration"
    )]
    pub(crate) fn from_config(
        output: W,
        dialect: Dialect,
        quoting: Quoting,
        bom: WriteBom,
        nulls: Nulls,
        field_count: FieldCount,
        has_headers: bool,
        buffer_capacity: usize,
    ) -> Self {
        Self {
            core: PushEmitter::from_config(
                Vec::with_capacity(buffer_capacity),
                dialect,
                quoting,
                bom,
                nulls,
                field_count,
                has_headers,
            ),
            output: Some(output),
            threshold: buffer_capacity,
            previous_drain: 0,
            failed: false,
        }
    }

    /// The sink, which is absent only after it has been taken.
    fn sink(&mut self) -> &mut W {
        self.output
            .as_mut()
            .expect("the sink is taken only by value")
    }

    /// Reject further work once an I/O failure has been latched.
    fn check_failed(&self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::detailed(
                ErrorKind::EmitterFailed,
                "CSV writer stopped after an earlier I/O error",
            ));
        }
        Ok(())
    }

    /// Write every buffered byte to the sink.
    ///
    /// The buffer keeps its capacity for reuse, except after a record larger
    /// than both the threshold and the previous drain forced it to grow: that
    /// capacity is returned so one pathological record cannot raise the
    /// emitter's resident memory for the rest of the run. Retaining the
    /// previous drain's size keeps a caller whose records exceed their
    /// configured threshold from reallocating on every single record.
    ///
    /// A write error retains the bytes the sink did not confirm, so a caller
    /// recovering through [`IntoInnerError`] still holds every accepted record
    /// that has not reached the sink, and holds no byte the sink already took.
    fn drain(&mut self) -> Result<(), Error> {
        let Some(output) = self.output.as_mut() else {
            return Ok(());
        };
        let buffer = self.core.buffer_mut();
        if buffer.is_empty() {
            return Ok(());
        }
        let drained = buffer.len();
        let floor = self.threshold.max(self.previous_drain);
        self.previous_drain = drained;
        let result = write_confirmed(output, buffer);
        match result {
            Ok(()) => buffer.clear(),
            // Keep the suffix the sink never confirmed. `drain` on a `Vec` is
            // a memmove of the remainder, which only runs on the error path.
            Err((written, error)) => {
                buffer.drain(..written);
                self.failed = true;
                return Err(Error::io(error, Location::START));
            }
        }
        buffer.shrink_to(floor);
        self.core.reclaim_scratch();
        Ok(())
    }

    /// Drain if the record just appended brought the buffer to the threshold.
    fn commit(&mut self) -> Result<(), Error> {
        if self.core.len() >= self.threshold {
            return self.drain();
        }
        Ok(())
    }

    /// Write one record.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the output fails or a field requires quoting
    /// while [`Quoting::Never`] is configured.
    pub fn emit_record<I, T>(&mut self, fields: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.check_failed()?;
        self.core.emit_record(fields)?;
        self.commit()
    }

    /// Write one record from a [`ByteRecord`].
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::ByteRecord;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// let record: ByteRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_byte_record(&record)?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the output fails or a field requires quoting
    /// while [`Quoting::Never`] is configured.
    pub fn emit_byte_record(&mut self, record: &ByteRecord) -> Result<(), Error> {
        self.check_failed()?;
        self.core.emit_byte_record(record)?;
        self.commit()
    }

    /// Write one record from a [`TextRecord`].
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::TextRecord;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// let record: TextRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_text_record(&record)?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the output fails or a field requires quoting
    /// while [`Quoting::Never`] is configured.
    pub fn emit_text_record(&mut self, record: &TextRecord) -> Result<(), Error> {
        self.check_failed()?;
        self.core.emit_text_record(record)?;
        self.commit()
    }

    /// Write one record whose fields may contain explicit NULL values.
    ///
    /// A `None` item is encoded using the configured [`Nulls`].
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions, Nulls};
    /// use coseva::format::Dynamic;
    ///
    /// let mut emitter = IoEmitter::<_, Dynamic>::with_options(
    ///     Vec::new(),
    ///     FormatOptions::CSV.nulls(Nulls::PostgresCsv),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_nullable_record([Some(&b"Boston"[..]), None])?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when output fails or a field cannot be represented by
    /// the configured format or field-count policy.
    pub fn emit_nullable_record<I, T>(&mut self, fields: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = Option<T>>,
        T: AsRef<[u8]>,
    {
        self.check_failed()?;
        self.core.emit_nullable_record(fields)?;
        self.commit()
    }

    /// Write the static headers declared by a native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::encoding::CsvEncode;
    /// use coseva::format::Csv;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.encode_header::<City>()?;
    /// assert_eq!(emitter.into_inner()?, b"name,pop\n");
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an output or field-count error.
    pub fn encode_header<T: CsvEncode>(&mut self) -> Result<(), Error> {
        self.check_failed()?;
        self.core.encode_header::<T>()?;
        self.commit()
    }

    /// Encode one native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::encoding::CsvEncode;
    /// use coseva::format::Csv;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.encode(&City { name: "Boston", pop: 650_706 })?;
    /// assert_eq!(emitter.into_inner()?, b"Boston,650706\n");
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a typed encoding or output error.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub fn encode<T: CsvEncode>(&mut self, value: &T) -> Result<(), Error> {
        self.check_failed()?;
        self.core.encode(value)?;
        self.commit()
    }

    /// Encode every native typed record from an iterator.
    ///
    /// Unlike [`Self::encode_header`], this never writes headers on its own;
    /// call `encode_header` first if a header row is wanted.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::encoding::CsvEncode;
    /// use coseva::format::Csv;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let cities = [
    ///     City { name: "Boston", pop: 650_706 },
    ///     City { name: "London", pop: 8_982_000 },
    /// ];
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.encode_header::<City>()?;
    /// emitter.encode_all(cities)?;
    /// assert_eq!(emitter.into_inner()?, b"name,pop\nBoston,650706\nLondon,8982000\n");
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first typed encoding or output error.
    pub fn encode_all<T, I>(&mut self, values: I) -> Result<(), Error>
    where
        T: CsvEncode,
        I: IntoIterator<Item = T>,
    {
        for value in values {
            self.encode(&value)?;
        }
        Ok(())
    }

    /// Start a field-at-a-time record builder.
    ///
    /// Fields are buffered until [`PendingIoRecord::finish`] is called.
    /// Dropping the returned guard without calling `finish` commits nothing.
    pub fn begin_record(&mut self) -> PendingIoRecord<'_, W, F> {
        PendingIoRecord::new(self)
    }

    /// Flush the underlying output sink.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying output sink cannot be flushed.
    pub fn flush(&mut self) -> Result<(), Error> {
        self.check_failed()?;
        self.drain()?;
        if let Err(error) = self.sink().flush() {
            self.failed = true;
            return Err(Error::io(error, Location::START));
        }
        Ok(())
    }

    /// Borrow the encoded bytes still waiting to reach the sink.
    ///
    /// This is empty whenever the last drain succeeded. After a write error it
    /// is exactly the suffix the sink did not confirm: every accepted record
    /// that has not been written, and no byte the sink already took. A caller
    /// recovering from a failed [`Self::flush`] or [`Self::into_inner`] can
    /// write these bytes to a replacement sink without losing or duplicating
    /// output.
    #[must_use]
    pub fn pending(&self) -> &[u8] {
        self.core.buffer()
    }

    /// Borrow the underlying output sink.
    #[must_use]
    pub fn get_ref(&self) -> &W {
        self.output
            .as_ref()
            .expect("the sink is taken only by value")
    }

    /// Mutably borrow the underlying output sink.
    pub fn get_mut(&mut self) -> &mut W {
        self.sink()
    }

    /// Flush and consume this emitter.
    ///
    /// # Errors
    ///
    /// Returns a recoverable error containing this emitter when the underlying
    /// sink cannot be flushed.
    pub fn into_inner(mut self) -> Result<W, IntoInnerError<Self>> {
        match self.flush() {
            Ok(()) => Ok(self.take_sink()),
            Err(error) => Err(IntoInnerError::new(self, error)),
        }
    }

    /// Consume this emitter without flushing the underlying sink.
    ///
    /// This is primarily useful for recovering a sink from
    /// [`IntoInnerError`]. Prefer [`Self::into_inner`] for normal finalization.
    #[must_use]
    pub fn into_inner_unflushed(mut self) -> W {
        self.take_sink()
    }

    /// Take the sink, leaving this emitter inert.
    ///
    /// Buffered bytes are dropped with it, which is why the only callers
    /// either drained first or were explicitly asked not to.
    fn take_sink(&mut self) -> W {
        self.core.clear();
        self.output.take().expect("the sink is taken only by value")
    }

    /// Serialize `value` as a CSV record using Serde.
    ///
    /// The complete record is collected before output begins, so serialization
    /// and field-validation errors commit nothing. An underlying I/O failure
    /// may leave a partial record in the destination and permanently fails this
    /// emitter.
    ///
    /// ```
    /// # #[cfg(feature = "serde")] {
    /// use coseva::IoEmitter;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct City {
    ///     name: String,
    ///     population: u32,
    /// }
    ///
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.serialize(&City { name: "Boston".to_owned(), population: 650_706 })?;
    /// assert_eq!(emitter.into_inner()?, b"name,population\nBoston,650706\n");
    /// # }
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when `value` contains unsupported Serde shapes
    /// (nested sequences, maps, or non-unit enum variants), when the emitter's
    /// field-count policy rejects the record, or when the underlying output fails.
    #[cfg(feature = "serde")]
    pub fn serialize<T: ::serde::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        self.check_failed()?;
        self.core.serialize(value)?;
        self.commit()
    }
}

#[cfg(feature = "std")]
impl<F: CsvFormat> IoEmitter<File, F> {
    /// The shared fallible constructor behind `to_path` and `new_path`.
    fn build_path(
        path: impl AsRef<Path>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        options.validate_buffered(format)?;
        let output = File::create(path).map_err(|error| {
            Error::io(
                error,
                Location {
                    byte: 0,
                    record: 0,
                    field: 0,
                    line: 1,
                },
            )
        })?;
        Ok(Self::from_config(
            output,
            format.dialect,
            format.quoting,
            format.write_bom,
            format.nulls,
            options.field_count_policy(),
            options.writes_headers(),
            options.capacity(),
        ))
    }

    /// Open for appending, also reporting whether the file already held records.
    ///
    /// The whole-document entry points need that flag to decide whether to emit
    /// a native header, which the emitter itself never writes unprompted.
    #[cfg_attr(coverage_nightly, coverage(off))]
    pub(crate) fn append_path_resuming(
        path: impl AsRef<Path>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<(Self, bool), Error> {
        options.validate_buffered(format)?;
        let mut output = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(Error::io_at_start)?;
        let state = document_state(&mut output, format.dialect)?;
        // A byte-order mark belongs at the start of a document, so it is
        // suppressed as soon as the file has one, whether or not any record
        // follows it. A header is only suppressed once a record exists to
        // have been described by it.
        let write_bom = if state == DocumentState::Empty {
            format.write_bom
        } else {
            WriteBom::Omit
        };
        let resuming = state == DocumentState::HasRecords;
        let expected_fields = match (resuming, options.field_count_policy()) {
            (true, FieldCount::MatchFirst) => existing_file_field_count(&mut output, format)?,
            _ => None,
        };
        let mut emitter = Self::from_config(
            output,
            format.dialect,
            format.quoting,
            write_bom,
            format.nulls,
            options.field_count_policy(),
            options.writes_headers() && !resuming,
            options.capacity(),
        );
        emitter.core.set_expected_fields(expected_fields);
        Ok((emitter, resuming))
    }
}

#[cfg(feature = "std")]
impl IoEmitter<File, Dynamic> {
    /// Create a file and build an emitter for an explicit format and encode
    /// options.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("cities.csv");
    ///
    /// let mut emitter = IoEmitter::to_path(&path, FormatOptions::CSV, EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// emitter.into_inner()?;
    ///
    /// assert_eq!(std::fs::read(&path)?, b"Boston,650706\n");
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, or an error when the file cannot be
    /// created.
    pub fn to_path(
        path: impl AsRef<Path>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        Self::build_path(path, format, options)
    }

    /// Open an existing file and build an emitter that appends to it.
    ///
    /// The file is created when it does not exist, so a run can be resumed
    /// without first checking whether it ever started.
    ///
    /// Appending suppresses the byte-order mark and header record, since
    /// those only belong at the start of a document; existing content is left
    /// untouched. A file holding only a byte-order mark is treated as a
    /// started document with no records, so the mark is not repeated but the
    /// header is still written.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions};
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("cities.csv");
    /// std::fs::write(&path, b"city,pop\nBoston,650706\n")?;
    ///
    /// let mut emitter = IoEmitter::append_path(&path, FormatOptions::CSV, EmitOptions::new())?;
    /// emitter.emit_record(["London", "8982000"])?;
    /// emitter.into_inner()?;
    ///
    /// assert_eq!(
    ///     std::fs::read(&path)?,
    ///     b"city,pop\nBoston,650706\nLondon,8982000\n",
    /// );
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, an error when the file cannot be opened,
    /// or [`ErrorKind::UnterminatedRecord`] when the existing file does not end
    /// with a record terminator — appending would otherwise fuse the new
    /// first record onto the truncated last one. Under [`RecordEnding::CrLf`]
    /// the terminator is the full two-byte `\r\n`, so a file ending in a bare
    /// line feed is refused.
    pub fn append_path(
        path: impl AsRef<Path>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        Self::append_path_resuming(path, format, options).map(|(emitter, _resuming)| emitter)
    }
}

#[cfg(feature = "std")]
impl<F: StaticFormat> IoEmitter<File, F> {
    /// Create a file and build an emitter encoding the format `F`.
    ///
    /// This is [`IoEmitter::to_path`] with the format named as a type
    /// parameter instead of passed as a value, so the encoder folds the
    /// delimiter, quote and escaping to constants.
    ///
    /// ```no_run
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new_path("cities.csv", EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// emitter.into_inner()?;
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, or an error when the file cannot be
    /// created.
    pub fn new_path(path: impl AsRef<Path>, options: EmitOptions) -> Result<Self, Error> {
        Self::build_path(path, F::FORMAT, options)
    }

    /// Open an existing file and build an emitter encoding the format `F` that
    /// appends to it.
    ///
    /// This is [`IoEmitter::append_path`] with the format named as a type
    /// parameter instead of passed as a value; it resumes an existing document
    /// on exactly the same terms.
    ///
    /// ```
    /// use coseva::IoEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let directory = tempfile::tempdir()?;
    /// let path = directory.path().join("cities.csv");
    /// std::fs::write(&path, b"city,pop\nBoston,650706\n")?;
    ///
    /// let mut emitter = IoEmitter::<_, Csv>::new_append_path(&path, EmitOptions::new())?;
    /// emitter.emit_record(["London", "8982000"])?;
    /// emitter.into_inner()?;
    ///
    /// assert_eq!(
    ///     std::fs::read(&path)?,
    ///     b"city,pop\nBoston,650706\nLondon,8982000\n",
    /// );
    /// # Ok::<(), Box<dyn core::error::Error>>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error, an error when the file cannot be opened,
    /// or [`ErrorKind::UnterminatedRecord`] when the existing file does not end
    /// with a record terminator.
    pub fn new_append_path(path: impl AsRef<Path>, options: EmitOptions) -> Result<Self, Error> {
        Self::append_path_resuming(path, F::FORMAT, options).map(|(emitter, _resuming)| emitter)
    }
}

/// What an existing file already holds, as far as appending is concerned.
#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentState {
    /// No bytes at all: appending starts a fresh document.
    Empty,
    /// Only a byte-order mark: a started document with no records yet.
    BomOnly,
    /// At least one complete record.
    HasRecords,
}

/// Classify `file` for appending, verifying it does not end mid-record.
///
/// A file ending mid-record cannot be safely appended to, since the missing
/// terminator would fuse the existing last record with the next one written.
/// An empty file, or one holding only a byte-order mark, starts a fresh
/// document.
///
/// [`RecordEnding::CrLf`] checks the full two-byte terminator, since a lone
/// line feed would leave the last record unterminated under that dialect.
/// [`RecordEnding::Newline`] treats a bare line feed as a complete terminator,
/// so only the single byte is checked. A multi-byte terminator is checked
/// whole, for the same reason `CrLf` is.
#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn document_state(file: &mut File, dialect: Dialect) -> Result<DocumentState, Error> {
    let len = file.seek(SeekFrom::End(0)).map_err(Error::io_at_start)?;
    if len == 0 {
        return Ok(DocumentState::Empty);
    }

    let unterminated = || Error::new(ErrorKind::UnterminatedRecord, Location::START);
    // Spell the terminator out once rather than testing each dialect's shape
    // separately: a leading `\r` under `CrLf`, the ending byte, then whatever
    // a multi-byte ending requires after it.
    let mut expected = Vec::new();
    if dialect.record_ending == RecordEnding::CrLf {
        expected.push(b'\r');
    }
    expected.push(dialect.record_ending.byte());
    expected.extend_from_slice(dialect.ending_tail().as_slice());
    let needed = expected.len();
    #[expect(
        clippy::cast_possible_wrap,
        reason = "`needed` is at most `TERMINATOR_LIMIT`, so the negation cannot wrap"
    )]
    let back = -(needed as i64);

    match len.cmp(&(crate::engine::BOM.len() as u64)) {
        Ordering::Equal => {
            file.seek(SeekFrom::Start(0)).map_err(Error::io_at_start)?;
            let head = file
                .by_ref()
                .bytes()
                .collect::<Result<Vec<_>, _>>()
                .map_err(Error::io_at_start)?;
            if head == crate::engine::BOM {
                return Ok(DocumentState::BomOnly);
            }
        }
        Ordering::Less | Ordering::Greater => {}
    }

    // A file shorter than the terminator cannot end with one, and seeking back
    // by `needed` would land before the start of the file.
    if len < needed as u64 {
        return Err(unterminated());
    }

    file.seek(SeekFrom::End(back)).map_err(Error::io_at_start)?;
    let tail = file
        .by_ref()
        .bytes()
        .collect::<Result<Vec<_>, _>>()
        .map_err(Error::io_at_start)?;

    if tail == expected {
        Ok(DocumentState::HasRecords)
    } else {
        Err(unterminated())
    }
}

#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn existing_file_field_count(
    file: &mut File,
    format: FormatOptions,
) -> Result<Option<usize>, Error> {
    file.seek(SeekFrom::Start(0)).map_err(Error::io_at_start)?;
    let options = ParseOptions::new().headers(Headers::None);
    let mut parser = IoParser::with_options(file, format, options)?;
    match parser.next_line()? {
        Some(mut line) => Ok(Some(line.record()?.len())),
        None => Ok(None),
    }
}

#[cfg(feature = "std")]
/// A field-at-a-time record builder that guards one pending record in a [`IoEmitter`].
///
/// Fields accumulate in an internal buffer until [`PendingIoRecord::finish`] is
/// called. Dropping this guard without calling `finish` discards all buffered
/// fields and commits nothing to the underlying emitter.
/// For a worked example, see [`IoEmitter`].
pub struct PendingIoRecord<'writer, W: io::Write, F: CsvFormat = Dynamic> {
    writer: &'writer mut IoEmitter<W, F>,
    record: ByteRecord,
}

#[cfg(feature = "std")]
impl<'writer, W: io::Write, F: CsvFormat> PendingIoRecord<'writer, W, F> {
    fn new(writer: &'writer mut IoEmitter<W, F>) -> Self {
        let record = writer.core.take_builder_record();
        Self { writer, record }
    }

    /// Append one field to the pending record.
    ///
    /// # Errors
    ///
    /// Currently infallible; returns a `Result` for symmetry with
    /// [`IoEmitter::emit_record`] and to support future validation.
    pub fn write_field(&mut self, field: impl AsRef<[u8]>) -> Result<(), Error> {
        self.record.push_field(field);
        Ok(())
    }

    /// Append an explicit NULL field to the pending record.
    ///
    /// # Errors
    ///
    /// Currently infallible; returns a `Result` for API symmetry.
    pub fn write_null(&mut self) -> Result<(), Error> {
        self.record.push_null();
        Ok(())
    }

    /// Encode and write the complete record, consuming the guard.
    ///
    /// # Errors
    ///
    /// Returns an error when the output fails or a field requires quoting
    /// while [`Quoting::Never`] is configured.
    pub fn finish(self) -> Result<(), Error> {
        // `self` is dropped at the end of this expression, and that `Drop` is
        // what returns the staging record to the emitter.
        self.writer.emit_byte_record(&self.record)
    }
}

#[cfg(feature = "std")]
impl<W: io::Write, F: CsvFormat> Drop for PendingIoRecord<'_, W, F> {
    fn drop(&mut self) {
        self.writer
            .core
            .return_builder_record(mem::take(&mut self.record));
    }
}

#[cfg(feature = "std")]
impl<W: io::Write, F: CsvFormat> fmt::Debug for PendingIoRecord<'_, W, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingIoRecord")
            .field("pending_fields", &self.record.len())
            .finish()
    }
}

#[cfg(feature = "std")]
impl<W: io::Write, F: CsvFormat> Drop for IoEmitter<W, F> {
    /// Write out anything still buffered.
    ///
    /// Records are held back until the buffer fills, so without this a dropped
    /// emitter would truncate its own output. Errors cannot be reported from a
    /// drop and are discarded, so callers that need to observe them must call
    /// [`IoEmitter::flush`] or [`IoEmitter::into_inner`], which also flush the sink
    /// itself.
    fn drop(&mut self) {
        drop(self.drain());
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    fn small_threshold_emitter() -> IoEmitter<Vec<u8>> {
        IoEmitter::with_options(
            Vec::new(),
            FormatOptions::default(),
            EmitOptions::default().buffer_capacity(64),
        )
        .expect("valid options")
    }

    #[test]
    fn sustained_oversized_records_keep_their_buffer_capacity() {
        let mut emitter = small_threshold_emitter();
        let field = "x".repeat(1024);
        let record = [field.as_str()];

        emitter.emit_record(record).expect("first record");
        emitter.emit_record(record).expect("second record");
        let grown = emitter.core.buffer_mut().capacity();
        emitter.emit_record(record).expect("third record");

        assert!(
            emitter.core.buffer_mut().capacity() >= grown,
            "a steady stream of oversized records must not reallocate every time"
        );
        assert!(
            grown >= 1024,
            "capacity should have grown to hold the record"
        );
    }

    #[test]
    fn test_io_emitter_coverage_paths() {
        let directory = tempfile::tempdir().unwrap();

        // ZeroWriter for write_confirmed WriteZero
        struct ZeroWriter;
        impl io::Write for ZeroWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut zero_emitter = IoEmitter::with_options(
            ZeroWriter,
            FormatOptions::CSV,
            EmitOptions::default().buffer_capacity(1),
        )
        .unwrap();
        assert!(zero_emitter.emit_record(["foo"]).is_err());
        assert!(zero_emitter.check_failed().is_err());
        assert!(!zero_emitter.pending().is_empty());
        io::Write::flush(&mut ZeroWriter).unwrap();

        // Multi-byte ending in document_state
        #[cfg(feature = "multibyte")]
        {
            let path = directory.path().join("tail.csv");
            let dialect = FormatOptions::CSV.record_ending_sequence(b";--").dialect;
            std::fs::write(&path, b"foo,bar;--").unwrap();
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap();
            assert_eq!(
                document_state(&mut file, dialect).unwrap(),
                DocumentState::HasRecords
            );
        }

        // existing_file_field_count on empty file
        let empty_path = directory.path().join("empty.csv");
        std::fs::write(&empty_path, b"").unwrap();
        let mut empty_file = File::open(&empty_path).unwrap();
        assert_eq!(
            existing_file_field_count(&mut empty_file, FormatOptions::CSV).unwrap(),
            None
        );

        // Additional API methods
        let mut mem_emitter = IoEmitter::<_, Dynamic>::with_options(
            Vec::new(),
            FormatOptions::CSV,
            EmitOptions::default(),
        )
        .unwrap();
        mem_emitter.emit_nullable_record([Some("a"), None]).unwrap();
        let mut text_rec = TextRecord::new();
        text_rec.push_field("txt");
        mem_emitter.emit_text_record(&text_rec).unwrap();

        let mut pending = mem_emitter.begin_record();
        pending.write_field("f1").unwrap();
        pending.write_null().unwrap();
        pending.finish().unwrap();

        assert!(mem_emitter.get_ref().is_empty() || !mem_emitter.get_ref().is_empty());
        assert!(mem_emitter.get_mut().is_empty() || !mem_emitter.get_mut().is_empty());

        let unflushed = mem_emitter.into_inner_unflushed();
        assert!(unflushed.is_empty());

        // InterruptedWriter for write_confirmed ErrorKind::Interrupted
        struct InterruptedWriter {
            interrupted_once: bool,
            inner: Vec<u8>,
        }
        impl io::Write for InterruptedWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                if !self.interrupted_once {
                    self.interrupted_once = true;
                    return Err(io::Error::from(io::ErrorKind::Interrupted));
                }
                self.inner.write(buf)
            }
            fn flush(&mut self) -> io::Result<()> {
                self.inner.flush()
            }
        }
        let mut int_emitter = IoEmitter::with_options(
            InterruptedWriter {
                interrupted_once: false,
                inner: Vec::new(),
            },
            FormatOptions::CSV,
            EmitOptions::default().buffer_capacity(1),
        )
        .unwrap();
        int_emitter.emit_record(["interrupted_ok"]).unwrap();
        int_emitter.flush().unwrap();

        // to_path, append_path, new_path, new_append_path
        let p1 = directory.path().join("dynamic.csv");
        let mut em1 = IoEmitter::to_path(&p1, FormatOptions::CSV, EmitOptions::default()).unwrap();
        em1.emit_record(["p1_val"]).unwrap();
        em1.into_inner().unwrap();

        let mut em2 =
            IoEmitter::append_path(&p1, FormatOptions::CSV, EmitOptions::default()).unwrap();
        em2.emit_record(["p2_val"]).unwrap();
        em2.into_inner().unwrap();

        let p2 = directory.path().join("static.csv");
        let mut em3 =
            IoEmitter::<_, crate::format::Csv>::new_path(&p2, EmitOptions::default()).unwrap();
        em3.emit_record(["p3_val"]).unwrap();
        em3.into_inner().unwrap();

        let mut em4 =
            IoEmitter::<_, crate::format::Csv>::new_append_path(&p2, EmitOptions::default())
                .unwrap();
        em4.emit_record(["p4_val"]).unwrap();
        em4.into_inner().unwrap();

        // Test invalid options on to_path and append_path
        assert!(
            IoEmitter::to_path(
                directory.path().join("invalid.csv"),
                FormatOptions::CSV,
                EmitOptions::default().buffer_capacity(0)
            )
            .is_err()
        );
        assert!(
            IoEmitter::append_path(
                directory.path().join("invalid.csv"),
                FormatOptions::CSV,
                EmitOptions::default().buffer_capacity(0)
            )
            .is_err()
        );

        // Test append_path with MatchFirst on existing file
        let temp_mf = directory.path().join("match-first.csv");
        std::fs::write(&temp_mf, b"c1,c2\nv1,v2\n").unwrap();
        let mut mf_emitter = IoEmitter::append_path(
            &temp_mf,
            FormatOptions::CSV,
            EmitOptions::default().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        let _ = mf_emitter.sink();
        mf_emitter.emit_record(["v3", "v4"]).unwrap();
        mf_emitter.into_inner().unwrap();

        // Test append_path on BOM only file
        let temp_bom = directory.path().join("bom.csv");
        std::fs::write(&temp_bom, crate::engine::BOM).unwrap();
        let mut bom_emitter =
            IoEmitter::append_path(&temp_bom, FormatOptions::CSV, EmitOptions::default()).unwrap();
        bom_emitter.emit_record(["a"]).unwrap();
        bom_emitter.into_inner().unwrap();

        // Serialize error path
        #[cfg(feature = "serde")]
        {
            let mut ser_emitter = IoEmitter::with_options(
                Vec::new(),
                FormatOptions::CSV,
                EmitOptions::default().field_count(FieldCount::Exact(1)),
            )
            .unwrap();
            #[derive(::serde::Serialize)]
            struct TwoFields {
                a: u32,
                b: u32,
            }
            assert!(ser_emitter.serialize(&TwoFields { a: 1, b: 2 }).is_err());
        }

        // Error path for into_inner when sink flush fails
        struct FailingFlushWriter;
        impl io::Write for FailingFlushWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::from(io::ErrorKind::Other))
            }
        }
        let fail_flush_emitter = IoEmitter::with_options(
            FailingFlushWriter,
            FormatOptions::CSV,
            EmitOptions::default(),
        )
        .unwrap();
        assert!(fail_flush_emitter.into_inner().is_err());

        // Test encode_header, encode, encode_all
        #[derive(Clone, Copy)]
        struct SimpleRow {
            a: &'static str,
            b: u32,
        }
        impl CsvEncode for SimpleRow {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                visitor: &mut V,
            ) -> Result<(), Error> {
                visitor.visit_field(0, "a", self.a.as_bytes())?;
                visitor.visit_field(1, "b", self.b.to_string().as_bytes())?;
                Ok(())
            }
            fn field_names() -> &'static [&'static str] {
                &["a", "b"]
            }
        }
        let mut enc_emitter = IoEmitter::<_, Dynamic>::with_options(
            Vec::new(),
            FormatOptions::CSV,
            EmitOptions::default(),
        )
        .unwrap();
        enc_emitter.encode_header::<SimpleRow>().unwrap();
        enc_emitter.encode(&SimpleRow { a: "x", b: 1 }).unwrap();
        enc_emitter
            .encode_all([SimpleRow { a: "y", b: 2 }])
            .unwrap();
        assert_eq!(enc_emitter.into_inner().unwrap(), b"a,b\nx,1\ny,2\n");

        // Test check_failed on all methods when emitter failed
        let mut fail_emitter = IoEmitter::with_options(
            ZeroWriter,
            FormatOptions::CSV,
            EmitOptions::default().buffer_capacity(1),
        )
        .unwrap();
        let _ = fail_emitter.emit_record(["foo"]); // triggers failure
        assert!(fail_emitter.emit_record(["bar"]).is_err());
        let br = ByteRecord::new();
        assert!(fail_emitter.emit_byte_record(&br).is_err());
        let tr = TextRecord::new();
        assert!(fail_emitter.emit_text_record(&tr).is_err());
        assert!(fail_emitter.emit_nullable_record([Some("a")]).is_err());
        assert!(fail_emitter.encode_header::<SimpleRow>().is_err());
        assert!(fail_emitter.encode(&SimpleRow { a: "x", b: 1 }).is_err());
        #[cfg(feature = "serde")]
        {
            assert!(fail_emitter.serialize(&("a", 1)).is_err());
        }

        // Test emit_byte_record, emit_text_record, and encode validation error paths
        let mut strict_emit = IoEmitter::with_options(
            Vec::new(),
            FormatOptions::CSV,
            EmitOptions::new().field_count(crate::config::FieldCount::Exact(2)),
        )
        .unwrap();
        let mut br_bad = ByteRecord::new();
        br_bad.push_field(b"one");
        assert!(strict_emit.emit_byte_record(&br_bad).is_err());
        let mut tr_bad = TextRecord::new();
        tr_bad.push_field("one");
        assert!(strict_emit.emit_text_record(&tr_bad).is_err());
        assert!(strict_emit.encode(&SimpleRow { a: "x", b: 1 }).is_ok());

        // Test append_path on existing non-empty file with matching headers
        let exist_path = directory.path().join("existing.csv");
        std::fs::write(&exist_path, b"a,b\n1,2\n").unwrap();
        let mut app_emitter = IoEmitter::append_path(
            &exist_path,
            FormatOptions::CSV,
            EmitOptions::new()
                .has_headers(true)
                .field_count(crate::config::FieldCount::MatchFirst),
        )
        .unwrap();
        app_emitter.emit_record(["3", "4"]).unwrap();
        let app_file = app_emitter.into_inner().unwrap();
        drop(app_file);
        let content = std::fs::read_to_string(&exist_path).unwrap();
        assert_eq!(content, "a,b\n1,2\n3,4\n");
    }

    #[test]
    fn write_confirmed_reports_exact_partial_progress_and_errors() {
        struct Zero;

        impl io::Write for Zero {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Ok(0)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let (written, error) =
            write_confirmed(&mut Zero, b"abc").expect_err("zero writes make no progress");
        assert_eq!(written, 0);
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(
            error.to_string(),
            "CSV sink accepted no bytes of the buffered output",
        );

        #[derive(Default)]
        struct Partial {
            bytes: Vec<u8>,
            calls: usize,
        }

        impl io::Write for Partial {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.calls += 1;
                if self.calls == 1 {
                    self.bytes.extend_from_slice(&buffer[..2]);
                    Ok(2)
                } else {
                    Err(io::Error::new(io::ErrorKind::BrokenPipe, "stopped"))
                }
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut partial = Partial::default();
        let (written, error) =
            write_confirmed(&mut partial, b"abcdef").expect_err("second write fails");
        assert_eq!(written, 2);
        assert_eq!(partial.bytes, b"ab");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn constructor_and_threshold_boundary_keep_exact_buffer_state() {
        #[derive(Debug, Default)]
        struct Counting {
            bytes: Vec<u8>,
            writes: usize,
        }

        impl io::Write for Counting {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(buffer);
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut emitter = IoEmitter::with_options(
            Counting::default(),
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(2),
        )
        .expect("emitter");
        assert_eq!(emitter.threshold, 2);
        assert_eq!(emitter.previous_drain, 0);
        assert_eq!(emitter.core.as_vec().capacity(), 2);

        emitter.emit_record(["x"]).expect("two-byte record");
        assert_eq!(
            emitter.get_ref().writes,
            1,
            "equality reaches the threshold"
        );
        assert_eq!(emitter.get_ref().bytes, b"x\n");
        assert!(emitter.core.is_empty());
    }

    #[test]
    fn drain_skips_empty_buffers_and_reclaims_output_and_builder_scratch() {
        struct RejectEmpty;

        impl io::Write for RejectEmpty {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                assert!(!buffer.is_empty(), "empty drains must not call the sink");
                Ok(buffer.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut emitter = IoEmitter::with_options(
            RejectEmpty,
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(8),
        )
        .expect("emitter");
        let mut builder = ByteRecord::with_capacity(4, 64 * 1024);
        builder.push_field(b"x");
        emitter.core.return_builder_record(builder);
        emitter.core.buffer_mut().reserve_exact(1024);
        emitter.previous_drain = 31;
        let empty_capacity = emitter.core.buffer_mut().capacity();
        let returned = emitter.core.take_builder_record();
        let empty_scratch = returned.byte_capacity();
        emitter.core.return_builder_record(returned);
        emitter.drain().expect("empty drain");
        assert_eq!(emitter.core.buffer_mut().capacity(), empty_capacity);
        let returned = emitter.core.take_builder_record();
        assert_eq!(returned.byte_capacity(), empty_scratch);
        emitter.core.return_builder_record(returned);
        assert_eq!(emitter.previous_drain, 31);

        let recycled = emitter.core.take_builder_record();
        let scratch = recycled.byte_capacity();
        assert!(scratch >= 64 * 1024);
        emitter.core.return_builder_record(recycled);

        emitter.core.buffer_mut().extend_from_slice(b"abc");
        let grown = emitter.core.buffer_mut().capacity();
        assert!(grown > emitter.threshold);
        emitter.drain().expect("drain");
        assert!(emitter.core.buffer_mut().capacity() <= 31);
        assert!(emitter.core.buffer_mut().capacity() < grown);
        let reclaimed = emitter.core.take_builder_record();
        assert!(reclaimed.byte_capacity() < scratch);
        emitter.core.return_builder_record(reclaimed);
        assert_eq!(emitter.previous_drain, 3);
    }

    #[test]
    fn failed_emitter_message_and_retained_suffix_are_exact() {
        #[derive(Debug, Default)]
        struct OneThenFail {
            bytes: Vec<u8>,
            wrote: bool,
        }

        impl io::Write for OneThenFail {
            fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
                if self.wrote {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stopped"));
                }
                self.wrote = true;
                self.bytes.push(buffer[0]);
                Ok(1)
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut emitter = IoEmitter::with_options(
            OneThenFail::default(),
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(1),
        )
        .expect("emitter");
        let error = emitter
            .emit_record(["abc"])
            .expect_err("partial write fails the emitter");
        assert_eq!(error.kind(), ErrorKind::Io(io::ErrorKind::BrokenPipe));
        assert_eq!(emitter.pending(), b"bc\n");
        assert_eq!(emitter.get_ref().bytes, b"a");
        assert_eq!(
            emitter
                .emit_record(["later"])
                .expect_err("failure is latched")
                .to_string(),
            "CSV writer stopped after an earlier I/O error",
        );
    }

    #[test]
    fn take_sink_discards_pending_core_bytes_and_scratch() {
        let mut emitter = IoEmitter::with_options(
            Vec::<u8>::new(),
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(64),
        )
        .expect("emitter");
        emitter.emit_record(["pending"]).expect("buffered record");
        assert!(!emitter.core.is_empty());
        let sink = emitter.take_sink();
        assert!(sink.is_empty());
        assert!(emitter.core.is_empty());
        assert!(emitter.output.is_none());
    }

    #[test]
    fn pending_io_record_debug_and_drop_return_builder_storage() {
        let mut emitter =
            IoEmitter::with_options(Vec::<u8>::new(), FormatOptions::CSV, EmitOptions::new())
                .expect("emitter");
        {
            let mut pending = emitter.begin_record();
            pending.write_field(vec![b'x'; 1024]).expect("field");
            pending.write_field("two").expect("field");
            assert_eq!(
                format!("{pending:?}"),
                "PendingIoRecord { pending_fields: 2 }",
            );
        }
        assert!(emitter.pending().is_empty());
        let recycled = emitter.core.take_builder_record();
        assert_eq!(recycled.len(), 0);
        assert!(recycled.byte_capacity() >= 1024);
        emitter.core.return_builder_record(recycled);
    }

    #[test]
    fn document_state_accepts_exact_terminators() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let cases = [
            ("newline.csv", b"\n".as_slice(), FormatOptions::CSV.dialect),
            (
                "crlf.csv",
                b"\r\n".as_slice(),
                FormatOptions::CSV.record_ending(RecordEnding::CrLf).dialect,
            ),
        ];
        for (name, bytes, dialect) in cases {
            let path = directory.path().join(name);
            std::fs::write(&path, bytes).expect("test file");
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .expect("open");
            assert_eq!(
                document_state(&mut file, dialect).expect("complete record"),
                DocumentState::HasRecords,
            );
            std::fs::remove_file(path).expect("remove");
        }
    }

    #[test]
    fn append_match_first_starts_empty_and_bom_only_documents() {
        let directory = tempfile::tempdir().expect("temporary directory");
        for (name, initial) in [
            ("empty.csv", b"".as_slice()),
            ("bom.csv", crate::engine::BOM),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, initial).expect("initial file");
            let mut emitter = IoEmitter::append_path(
                &path,
                FormatOptions::CSV.write_bom(WriteBom::Emit),
                EmitOptions::new()
                    .field_count(FieldCount::MatchFirst)
                    .buffer_capacity(17),
            )
            .expect("append emitter");
            assert_eq!(emitter.threshold, 17);
            emitter.emit_record(["a", "b"]).expect("first width");
            assert_eq!(
                emitter
                    .emit_record(["one"])
                    .expect_err("first record established width")
                    .kind(),
                ErrorKind::FieldCountMismatch {
                    expected: 2,
                    actual: 1,
                },
            );
            emitter.into_inner().expect("finish");
            std::fs::remove_file(path).expect("remove");
        }

        let path = directory.path().join("leading-delimiter.csv");
        std::fs::write(&path, b",value\n").expect("existing record");
        let mut emitter = IoEmitter::append_path(
            &path,
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .expect("append emitter");
        assert_eq!(
            emitter
                .emit_record(["one"])
                .expect_err("the leading empty field counts")
                .kind(),
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 1,
            },
        );
        drop(emitter.into_inner_unflushed());
        std::fs::remove_file(path).expect("remove");
    }

    #[test]
    fn path_construction_preserves_capacity_and_start_error_location() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing_parent = directory.path().join("missing-parent");
        let error = IoEmitter::to_path(
            missing_parent.join("output.csv"),
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(17),
        )
        .expect_err("missing parent");
        assert_eq!(error.location(), Location::START);

        let path = directory.path().join("capacity.csv");
        let emitter = IoEmitter::to_path(
            &path,
            FormatOptions::CSV,
            EmitOptions::new().buffer_capacity(17),
        )
        .expect("path emitter");
        assert_eq!(emitter.threshold, 17);
        assert_eq!(emitter.core.as_vec().capacity(), 17);
        drop(emitter.into_inner_unflushed());
        std::fs::remove_file(path).expect("remove");
    }
}

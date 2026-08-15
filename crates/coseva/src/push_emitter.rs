#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;
use core::mem;
use core::str;

use crate::byte_record::ByteRecord;
use crate::config::{
    Dialect, EmitOptions, FieldCount, FormatOptions, Headers, Nulls, ParseOptions, Quoting,
    WriteBom,
};
#[cfg(feature = "serde")]
use crate::emit::SerdeHeaderState;
use crate::emit::{
    DirectEncodeVisitor, emit_nullable_record, emit_nullable_record_runtime, emit_record,
    emit_record_runtime, fmt_dialect, fmt_nulls, record_too_large, validate_field_count,
};
use crate::encoding::CsvEncode;
use crate::error::{Error, ErrorKind};
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::slice_parser::SliceParser;
use crate::text_record::TextRecord;

use crate::reclaim::reclaim;
#[cfg(feature = "serde")]
use crate::serde::{serialize_direct, serialize_direct_with_headers};

/// Push-based CSV encoding for chunked, asynchronous, and non-`std` sinks.
///
/// This is the dual of [`crate::PushParser`]: it encodes records into a
/// caller-visible buffer and never performs I/O. Where the push parser has
/// bytes fed to it and yields records, this has records fed to it and yields
/// bytes, which the caller writes out however it likes — an async socket, a
/// WASM or FFI callback, or a compressor consuming blocks.
///
/// The buffer grows until the caller takes the bytes, so a caller producing
/// more output than fits in memory must drain periodically. [`Self::buffer`]
/// borrows the encoded bytes and [`Self::clear`] releases them once written;
/// together they are the whole output protocol.
///
/// This is also the shared encoding core: [`crate::IoEmitter`] adds an owned
/// sink and a drain threshold on top of it, and [`crate::VecEmitter`] simply
/// retains its buffer. Neither reimplements any encoding logic.
///
/// ```
/// use coseva::PushEmitter;
///
/// let mut emitter = PushEmitter::default();
/// let mut written = Vec::new();
///
/// for row in [["city", "pop"], ["Boston", "650706"]] {
///     emitter.emit_record(row)?;
///     // Hand the bytes to whatever owns the write loop, then release them.
///     written.extend_from_slice(emitter.buffer());
///     emitter.clear();
/// }
///
/// assert_eq!(written, b"city,pop\nBoston,650706\n");
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Debug)]
pub struct PushEmitter<F: CsvFormat = Dynamic> {
    /// Encoded bytes the caller has not taken yet.
    buffer: Vec<u8>,
    dialect: Dialect,
    quoting: Quoting,
    bom: WriteBom,
    nulls: Nulls,
    field_count: FieldCount,
    expected_fields: Option<usize>,
    /// Whether the document has been opened, which is what makes the
    /// byte-order mark a once-per-document decision rather than a per-record
    /// one. It stays latched across [`Self::clear`], since bytes already
    /// handed to the caller are still part of the document.
    started: bool,
    #[cfg(feature = "serde")]
    serde_headers: SerdeHeaderState,
    #[cfg(feature = "serde")]
    serde_header: ByteRecord,
    /// Reused scratch for Serde fields the serializer builds incrementally.
    ///
    /// A field is quoted and escaped only once complete, so incrementally
    /// serialized fields (struct and tuple members, `Display` values) land
    /// here first and are cleared after each field. It grows at most to the
    /// widest field and is never allocated per field.
    #[cfg(feature = "serde")]
    serde_scratch: Vec<u8>,
    /// Reused staging for the field-at-a-time record builders.
    ///
    /// `begin_record` takes this record, the guard fills it, and `finish` or
    /// the guard's `Drop` gives it back, so a caller assembling records field
    /// by field allocates the pair of buffers once rather than once per
    /// record. At most one guard can be live, because it borrows the emitter
    /// mutably, so a single slot is enough.
    builder_scratch: ByteRecord,
    /// The compile-time format, when there is one.
    ///
    /// The fields above still hold the same configuration, so a `Dynamic`
    /// emitter reads them as before; a static format lets the encoder fold
    /// them to immediates instead.
    format: PhantomData<F>,
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn reserve_output_capacity(buffer: &mut Vec<u8>, capacity: usize) -> Result<(), Error> {
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_error| Error::detailed(ErrorKind::Encode, "emitter output allocation failed"))
}

fn reclaim_live<T>(buffer: &mut Vec<T>) {
    let live = buffer.len();
    reclaim(buffer, live);
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn emit_slices_runtime(
    buffer: &mut Vec<u8>,
    dialect: Dialect,
    quoting: Quoting,
    nulls: Nulls,
    fields: &[&[u8]],
) -> Result<usize, Error> {
    let field_overhead = fields.len().checked_mul(3).ok_or_else(record_too_large)?;
    let field_bytes = fields.iter().try_fold(0usize, |bytes, field| {
        bytes.checked_add(field.len()).ok_or_else(record_too_large)
    })?;
    let capacity = field_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(field_overhead))
        .and_then(|bytes| bytes.checked_add(2))
        .ok_or_else(record_too_large)?;
    reserve_output_capacity(buffer, capacity)?;
    if nulls == Nulls::None && !dialect.escape.escapes_unquoted() {
        emit_record_runtime(buffer, dialect, quoting, fields.iter().copied())
    } else {
        emit_nullable_record_runtime(
            buffer,
            dialect,
            quoting,
            nulls,
            fields.iter().copied().map(Some),
        )
    }
}

impl<F: StaticFormat> PushEmitter<F> {
    /// Create an emitter over an empty buffer, encoding the format `F`.
    ///
    /// The format is named as a type parameter, so the encoder folds its
    /// delimiter, quote and escaping to constants:
    ///
    /// ```
    /// use coseva::PushEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Csv;
    ///
    /// let mut emitter = PushEmitter::<Csv>::new(EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.into_inner(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the encode options are invalid.
    pub fn new(options: EmitOptions) -> Result<Self, Error> {
        Self::build(Vec::new(), F::FORMAT, options)
    }
}

impl PushEmitter<Dynamic> {
    /// Create an emitter for an explicit format and encode options.
    ///
    /// ```
    /// use coseva::PushEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions};
    ///
    /// let mut emitter = PushEmitter::with_options(
    ///     FormatOptions::CSV.delimiter(b';'),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.buffer(), b"Boston;650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the configured format is ambiguous.
    pub fn with_options(format: FormatOptions, options: EmitOptions) -> Result<Self, Error> {
        Self::build(Vec::new(), format, options)
    }
}

impl<F: CsvFormat> PushEmitter<F> {
    /// The shared fallible constructor behind `new` and `with_options`.
    #[inline]
    pub(crate) fn build(
        buffer: Vec<u8>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        format.validate()?;
        let expected_fields = Self::existing_field_count(&buffer, format, options)?;
        let mut emitter = Self::from_config(
            buffer,
            format.dialect,
            format.quoting,
            format.write_bom,
            format.nulls,
            options.field_count_policy(),
            options.writes_headers(),
        );
        emitter.expected_fields = expected_fields;
        Ok(emitter)
    }

    pub(crate) fn from_config(
        buffer: Vec<u8>,
        dialect: Dialect,
        quoting: Quoting,
        bom: WriteBom,
        nulls: Nulls,
        field_count: FieldCount,
        #[cfg_attr(
            not(feature = "serde"),
            expect(unused_variables, reason = "header state is tracked only for Serde")
        )]
        has_headers: bool,
    ) -> Self {
        let started = !buffer.is_empty();
        Self {
            buffer,
            dialect,
            quoting,
            bom,
            nulls,
            field_count,
            expected_fields: None,
            started,
            format: PhantomData,
            #[cfg(feature = "serde")]
            serde_headers: if !has_headers {
                SerdeHeaderState::Disabled
            } else if started {
                SerdeHeaderState::Written
            } else {
                SerdeHeaderState::Pending
            },
            #[cfg(feature = "serde")]
            serde_header: ByteRecord::new(),
            #[cfg(feature = "serde")]
            serde_scratch: Vec::new(),
            builder_scratch: ByteRecord::new(),
        }
    }

    #[cfg(feature = "std")]
    pub(crate) const fn set_expected_fields(&mut self, expected_fields: Option<usize>) {
        self.expected_fields = expected_fields;
    }

    /// Borrow the encoded bytes not yet released.
    #[must_use]
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn existing_field_count(
        buffer: &[u8],
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Option<usize>, Error> {
        match (buffer.is_empty(), options.field_count_policy()) {
            (_, FieldCount::Flexible | FieldCount::Exact(_)) | (true, FieldCount::MatchFirst) => {
                return Ok(None);
            }
            (false, FieldCount::MatchFirst) => {}
        }
        let parse_options = ParseOptions::new().headers(Headers::None);
        let mut parser = SliceParser::<Dynamic>::build(buffer, format, parse_options)?;
        match parser.next_line()? {
            Some(mut line) => Ok(Some(line.record()?.len())),
            None => Ok(None),
        }
    }

    /// Borrow the underlying buffer.
    #[must_use]
    pub const fn as_vec(&self) -> &Vec<u8> {
        &self.buffer
    }

    /// Mutably borrow the underlying buffer.
    ///
    /// This exists so an owner can drain the bytes without copying them.
    #[cfg(feature = "std")]
    pub(crate) const fn buffer_mut(&mut self) -> &mut Vec<u8> {
        &mut self.buffer
    }

    /// The number of encoded bytes held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether no encoded bytes are held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Release the encoded bytes, keeping the capacity for reuse.
    ///
    /// The document is not reopened: a byte-order mark is not emitted again,
    /// and the field-count policy keeps whatever it learned.
    pub fn clear(&mut self) {
        self.buffer.clear();
        // Releasing the output is the one point at which the caller has told
        // us the buffer is no longer wanted at its current size, so it is
        // where capacity grown by an outlier record is handed back.
        reclaim_live(&mut self.buffer);
        self.reclaim_scratch();
    }

    /// Consume this emitter and return the encoded bytes.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.buffer
    }

    /// Take the builder's staging record, emptied but keeping its capacity.
    pub(crate) fn take_builder_record(&mut self) -> ByteRecord {
        let mut record = mem::take(&mut self.builder_scratch);
        record.clear();
        record
    }

    /// Return the staging record taken by [`Self::take_builder_record`].
    pub(crate) fn return_builder_record(&mut self, record: ByteRecord) {
        self.builder_scratch = record;
    }

    /// Hand back scratch capacity grown by an unusually large record.
    ///
    /// Called from the points at which the caller releases encoded output,
    /// never from the per-record path.
    #[cfg(feature = "serde")]
    pub(crate) fn reclaim_scratch(&mut self) {
        reclaim_live(&mut self.serde_scratch);
        self.serde_header.reclaim();
        self.builder_scratch.reclaim();
    }

    #[cfg(not(feature = "serde"))]
    pub(crate) fn reclaim_scratch(&mut self) {
        self.builder_scratch.reclaim();
    }

    /// Open the document, reserving room for a byte-order mark.
    ///
    /// The mark is written by [`Self::commit`] rather than here so that a
    /// record rejected before it is committed leaves nothing behind at all.
    fn record_start(&self) -> usize {
        self.buffer.len()
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the static format must fold before selecting the non-null encoding path"
    )]
    fn fmt_dialect(&self) -> Dialect {
        fmt_dialect::<F>(self.dialect)
    }

    #[inline(always)]
    #[expect(
        clippy::inline_always,
        reason = "the static format must fold before selecting the non-null encoding path"
    )]
    fn fmt_nulls(&self) -> Nulls {
        fmt_nulls::<F>(self.nulls)
    }

    /// Accept the record occupying the buffer from `start` onwards.
    ///
    /// The byte-order mark is spliced in only once a record has actually been
    /// accepted, so an emitter whose every record was rejected emits nothing.
    fn commit(&mut self, start: usize) {
        if !self.started {
            self.started = true;
            if self.bom == WriteBom::Emit {
                debug_assert_eq!(start, 0, "the document opens on the first committed record");
                self.buffer.splice(0..0, b"\xEF\xBB\xBF".iter().copied());
            }
        }
    }

    /// Discard the partial record occupying the buffer from `start` onwards.
    fn rollback(&mut self, start: usize) {
        self.buffer.truncate(start);
    }

    /// Append one encoded record.
    ///
    /// ```
    /// use coseva::PushEmitter;
    ///
    /// let mut emitter = PushEmitter::default();
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.buffer(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`] is configured, or when the field-count policy
    /// rejects the record.
    pub fn emit_record<I, T>(&mut self, fields: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        let start = self.record_start();
        let encoded =
            if self.fmt_nulls() == Nulls::None && !self.fmt_dialect().escape.escapes_unquoted() {
                emit_record::<F, _, _, _>(&mut self.buffer, self.dialect, self.quoting, fields)
            } else {
                emit_nullable_record::<F, _, _, _>(
                    &mut self.buffer,
                    self.dialect,
                    self.quoting,
                    self.nulls,
                    fields.into_iter().map(Some),
                )
            };
        self.finish_record(start, encoded)
    }

    /// Append one record from a slice without intermediate capacity checks.
    ///
    /// Unlike [`Self::emit_record`], which accepts any iterator of field
    /// values, this takes an already-materialized slice of byte slices, which
    /// lets it size its output allocation up front.
    ///
    /// ```
    /// use coseva::PushEmitter;
    ///
    /// let mut emitter = PushEmitter::default();
    /// emitter.emit_slices(&[b"Boston", b"650706"])?;
    /// assert_eq!(emitter.buffer(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`] is configured, when the record's encoded size
    /// overflows, or when the field-count policy rejects the record.
    pub fn emit_slices(&mut self, fields: &[&[u8]]) -> Result<(), Error> {
        let start = self.record_start();
        let encoded = emit_slices_runtime(
            &mut self.buffer,
            self.dialect,
            self.quoting,
            self.nulls,
            fields,
        );
        self.finish_record(start, encoded)
    }

    /// Append one record whose fields may contain explicit NULL values.
    ///
    /// A `None` item is encoded using the configured [`Nulls`].
    ///
    /// ```
    /// use coseva::PushEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions, Nulls};
    /// use coseva::format::Dynamic;
    ///
    /// let mut emitter = PushEmitter::<Dynamic>::with_options(
    ///     FormatOptions::CSV.nulls(Nulls::PostgresCsv),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_nullable_record([Some(&b"Boston"[..]), None])?;
    /// assert_eq!(emitter.buffer(), b"Boston,\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field cannot be represented by the configured
    /// format or field-count policy.
    pub fn emit_nullable_record<I, T>(&mut self, fields: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = Option<T>>,
        T: AsRef<[u8]>,
    {
        let start = self.record_start();
        let encoded = emit_nullable_record::<F, _, _, _>(
            &mut self.buffer,
            self.dialect,
            self.quoting,
            self.nulls,
            fields,
        );
        self.finish_record(start, encoded)
    }

    /// Validate the record just encoded, rolling it back if it is rejected.
    ///
    /// Encoding writes straight into the buffer and is undone on rejection, so
    /// a rejected record commits nothing while still costing only one copy.
    ///
    /// `#[inline]` is measured: omitting it costs 1.5-3.4% on the emitter
    /// benchmarks; inlining the neighbouring helpers too is worse (+4.7%).
    #[inline]
    fn finish_record(&mut self, start: usize, encoded: Result<usize, Error>) -> Result<(), Error> {
        let field_count = match encoded {
            Ok(field_count) => field_count,
            Err(error) => {
                self.rollback(start);
                return Err(error);
            }
        };
        if let Err(error) =
            validate_field_count(self.field_count, &mut self.expected_fields, field_count)
        {
            self.rollback(start);
            return Err(error);
        }
        self.commit(start);
        Ok(())
    }

    /// Append one record from a [`ByteRecord`].
    ///
    /// ```
    /// use coseva::PushEmitter;
    /// use coseva::ByteRecord;
    ///
    /// let mut emitter = PushEmitter::default();
    /// let record: ByteRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_byte_record(&record)?;
    /// assert_eq!(emitter.buffer(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`] is configured.
    pub fn emit_byte_record(&mut self, record: &ByteRecord) -> Result<(), Error> {
        match record.null_aware() {
            false => self.emit_record(record.iter()),
            true => self.emit_nullable_record((0..record.len()).map(|index| {
                if record.is_null(index) == Some(true) {
                    None
                } else {
                    record.get(index)
                }
            })),
        }
    }

    /// Append one record from a [`TextRecord`].
    ///
    /// ```
    /// use coseva::PushEmitter;
    /// use coseva::TextRecord;
    ///
    /// let mut emitter = PushEmitter::default();
    /// let record: TextRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_text_record(&record)?;
    /// assert_eq!(emitter.buffer(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`] is configured.
    pub fn emit_text_record(&mut self, record: &TextRecord) -> Result<(), Error> {
        match record.null_aware() {
            false => self.emit_record(record.iter()),
            true => self.emit_nullable_record((0..record.len()).map(|index| {
                if record.is_null(index) == Some(true) {
                    None
                } else {
                    record.get(index).map(str::as_bytes)
                }
            })),
        }
    }

    /// Append the static headers declared by a native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::PushEmitter;
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = PushEmitter::default();
    /// emitter.encode_header::<City>()?;
    /// assert_eq!(emitter.buffer(), b"name,pop\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a field-count error.
    pub fn encode_header<T: CsvEncode>(&mut self) -> Result<(), Error> {
        self.emit_record(T::field_names())
    }

    /// Encode one native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::PushEmitter;
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = PushEmitter::default();
    /// emitter.encode(&City { name: "Boston", pop: 650_706 })?;
    /// assert_eq!(emitter.buffer(), b"Boston,650706\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a typed encoding error.
    pub fn encode<T: CsvEncode>(&mut self, value: &T) -> Result<(), Error> {
        let start = self.record_start();
        let mut visitor = DirectEncodeVisitor::<F, _>::new(
            &mut self.buffer,
            self.dialect,
            self.quoting,
            self.nulls,
        );
        let result = value.csv_encode(&mut visitor).map(|()| visitor.finish());
        self.finish_record(start, result)
    }

    /// Encode every native typed record from an iterator.
    ///
    /// Unlike [`Self::encode_header`], this never writes headers on its own;
    /// call `encode_header` first if a header row is wanted.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::PushEmitter;
    /// use coseva::encoding::CsvEncode;
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
    /// let mut emitter = PushEmitter::default();
    /// emitter.encode_header::<City>()?;
    /// emitter.encode_all(cities)?;
    /// assert_eq!(emitter.buffer(), b"name,pop\nBoston,650706\nLondon,8982000\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first typed encoding error.
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
    /// Fields are buffered until [`PendingPushRecord::finish`] is called.
    /// Dropping the returned guard without calling `finish` commits nothing.
    pub fn begin_record(&mut self) -> PendingPushRecord<'_, F> {
        PendingPushRecord::new(self)
    }

    /// Serialize `value` as a CSV record using Serde.
    ///
    /// Each field is framed straight into the output as it is serialized,
    /// without staging the record in between. If serialization, framing, or
    /// the field-count policy fails partway, the output is truncated back to
    /// the record start, so no partial record — or header — is committed.
    ///
    /// ```
    /// # #[cfg(feature = "serde")] {
    /// use coseva::PushEmitter;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct City {
    ///     name: String,
    ///     population: u32,
    /// }
    ///
    /// let mut emitter = PushEmitter::default();
    /// emitter.serialize(&City { name: "Boston".to_owned(), population: 650_706 })?;
    /// assert_eq!(emitter.buffer(), b"name,population\nBoston,650706\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when `value` contains unsupported Serde shapes
    /// (nested sequences, maps, or non-unit enum variants), or when the
    /// emitter's field-count policy rejects the record.
    #[cfg(feature = "serde")]
    pub fn serialize<T: ::serde::Serialize + ?Sized>(&mut self, value: &T) -> Result<(), Error> {
        if self.serde_headers == SerdeHeaderState::Pending {
            self.serialize_first_with_headers(value)
        } else {
            let allow_nested = self.serde_headers == SerdeHeaderState::Disabled;
            self.serialize_direct_record(value, allow_nested)
        }
    }

    /// Serialize one record straight into the buffer, no header row involved.
    ///
    /// Used once headers are settled (already written, or disabled): each
    /// field is framed as it is serialized, and [`Self::finish_record`] rolls
    /// the buffer back on any error so no partial record survives.
    #[cfg(feature = "serde")]
    fn serialize_direct_record<T: ::serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
        allow_nested: bool,
    ) -> Result<(), Error> {
        let start = self.record_start();
        let mut scratch = mem::take(&mut self.serde_scratch);
        let encoded = serialize_direct::<T, F, _>(
            value,
            &mut self.buffer,
            self.dialect,
            self.quoting,
            self.nulls,
            &mut scratch,
            allow_nested,
        );
        self.serde_scratch = scratch;
        self.finish_record(start, encoded)
    }

    /// Serialize the first record, discovering and writing its header row.
    ///
    /// The data record is framed into the buffer while its field names are
    /// collected, then a header row is framed and spliced ahead of it. Any
    /// failure (serialization, header framing, or field count) truncates the
    /// buffer back to the record start, so neither the header nor the record
    /// is left behind.
    #[cfg(feature = "serde")]
    fn serialize_first_with_headers<T: ::serde::Serialize + ?Sized>(
        &mut self,
        value: &T,
    ) -> Result<(), Error> {
        let start = self.record_start();
        let mut scratch = mem::take(&mut self.serde_scratch);
        let mut headers = mem::take(&mut self.serde_header);
        let outcome = serialize_direct_with_headers::<T, F, _>(
            value,
            &mut self.buffer,
            self.dialect,
            self.quoting,
            self.nulls,
            &mut scratch,
            &mut headers,
        );
        self.serde_scratch = scratch;
        let result = self.commit_first_with_headers(start, &headers, outcome);
        self.serde_header = headers;
        if result.is_ok() {
            self.serde_headers = SerdeHeaderState::Written;
        }
        result
    }

    /// Frame and splice the header row ahead of the first data record.
    ///
    /// The data record has already been written at `start`; this validates the
    /// field counts of both rows against the policy and, on success, splices
    /// the framed header row in front of the data record before committing.
    #[cfg(feature = "serde")]
    fn commit_first_with_headers(
        &mut self,
        start: usize,
        headers: &ByteRecord,
        outcome: Result<(bool, usize), Error>,
    ) -> Result<(), Error> {
        let (named, data_count) = match outcome {
            Ok(pair) => pair,
            Err(error) => {
                self.rollback(start);
                return Err(error);
            }
        };
        let mut expected = self.expected_fields;
        if named {
            let mut header_bytes = Vec::new();
            let header_count = match emit_nullable_record_runtime(
                &mut header_bytes,
                crate::emit::fmt_dialect::<F>(self.dialect),
                crate::emit::fmt_quoting::<F>(self.quoting),
                crate::emit::fmt_nulls::<F>(self.nulls),
                headers.iter().map(Some),
            ) {
                Ok(count) => count,
                Err(error) => {
                    self.rollback(start);
                    return Err(error);
                }
            };
            if let Err(error) = validate_field_count(self.field_count, &mut expected, header_count)
            {
                self.rollback(start);
                return Err(error);
            }
            self.buffer.splice(start..start, header_bytes);
        } else if let Err(error) = validate_field_count(self.field_count, &mut expected, data_count)
        {
            self.rollback(start);
            return Err(error);
        }
        self.expected_fields = expected;
        self.commit(start);
        Ok(())
    }
}

/// Encode into an existing vector, using the default CSV format.
///
/// A non-empty vector is treated as an already-opened document, so no
/// byte-order mark is inserted ahead of what is already there.
impl From<Vec<u8>> for PushEmitter<Dynamic> {
    fn from(buffer: Vec<u8>) -> Self {
        Self::from_config(
            buffer,
            Dialect::default(),
            Quoting::Necessary,
            WriteBom::Omit,
            Nulls::None,
            FieldCount::Flexible,
            true,
        )
    }
}

impl Default for PushEmitter<Dynamic> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

/// A field-at-a-time record builder guarding one pending [`PushEmitter`] record.
///
/// Fields accumulate in an internal record until [`PendingPushRecord::finish`]
/// is called. Dropping this guard without calling `finish` discards all
/// buffered fields and encodes nothing.
/// For a worked example, see [`PushEmitter`].
pub struct PendingPushRecord<'emitter, F: CsvFormat = Dynamic> {
    emitter: &'emitter mut PushEmitter<F>,
    record: ByteRecord,
}

impl<F: CsvFormat> fmt::Debug for PendingPushRecord<'_, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingPushRecord")
            .field("pending_fields", &self.record.len())
            .finish()
    }
}

impl<'emitter, F: CsvFormat> PendingPushRecord<'emitter, F> {
    fn new(emitter: &'emitter mut PushEmitter<F>) -> Self {
        let record = emitter.take_builder_record();
        Self { emitter, record }
    }

    /// Append one field to the pending record.
    ///
    /// # Errors
    ///
    /// Currently infallible; returns a `Result` for symmetry with
    /// [`PushEmitter::emit_record`] and to support future validation.
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

    /// Encode the complete record, consuming the guard.
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`] is configured.
    pub fn finish(self) -> Result<(), Error> {
        // `self` is dropped at the end of this expression, and that `Drop` is
        // what returns the staging record to the emitter.
        self.emitter.emit_byte_record(&self.record)
    }
}

impl<F: CsvFormat> Drop for PendingPushRecord<'_, F> {
    fn drop(&mut self) {
        self.emitter
            .return_builder_record(mem::take(&mut self.record));
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn push_emitter_edge_cases() {
        let mut emitter = PushEmitter::with_options(
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::Exact(2)),
        )
        .unwrap();

        // is_empty
        assert!(emitter.is_empty());

        // build validation error
        assert!(
            PushEmitter::with_options(FormatOptions::CSV.quote(b','), EmitOptions::new(),).is_err()
        );

        // existing_field_count with erroring buffer
        assert!(
            PushEmitter::<Dynamic>::build(
                b"\"unclosed".to_vec(),
                FormatOptions::CSV,
                EmitOptions::new().field_count(FieldCount::MatchFirst),
            )
            .is_err()
        );

        // Rollback on field count mismatch
        assert!(emitter.emit_record(["a"]).is_err());
        assert_eq!(emitter.len(), 0);

        // PendingPushRecord debug and methods
        let mut pending = emitter.begin_record();
        let _ = format!("{pending:?}");
        pending.write_field("a").unwrap();
        pending.write_null().unwrap();
        pending.finish().unwrap();
        assert_eq!(emitter.buffer(), b"a,\n");

        // emit_slices oversized capacity calculation
        let huge_slice = &[&b"foo"[..]; 10];
        emitter.emit_slices(huge_slice).unwrap_err(); // Mismatch field count exact(2)

        // existing_field_count with non-empty buffer and MatchFirst
        let existing = PushEmitter::<Dynamic>::build(
            b"a,b\n".to_vec(),
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert_eq!(existing.expected_fields, Some(2));

        // existing_field_count with blank buffer returning None
        let existing_blank = PushEmitter::<Dynamic>::build(
            b"".to_vec(),
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert_eq!(existing_blank.expected_fields, None);

        // existing_field_count with comments-only buffer returning None
        let existing_comment = PushEmitter::<Dynamic>::build(
            b"# only comment\n".to_vec(),
            FormatOptions::COMMENTED_CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert_eq!(existing_comment.expected_fields, None);

        #[cfg(feature = "serde")]
        {
            #[derive(serde::Serialize)]
            struct Row {
                a: u32,
                b: u32,
                c: u32,
            }
            let mut serde_emitter = PushEmitter::with_options(
                FormatOptions::CSV,
                EmitOptions::new()
                    .field_count(FieldCount::Exact(2))
                    .has_headers(true),
            )
            .unwrap();
            // Should fail due to field count mismatch on first record with headers
            assert!(serde_emitter.serialize(&Row { a: 1, b: 2, c: 3 }).is_err());

            // Unnamed first record with headers and field count mismatch (tuple)
            let mut serde_unnamed = PushEmitter::with_options(
                FormatOptions::CSV,
                EmitOptions::new()
                    .field_count(FieldCount::Exact(5))
                    .has_headers(true),
            )
            .unwrap();
            assert!(serde_unnamed.serialize(&(1, 2)).is_err());

            // Header emit error with Quoting::Never and header requiring quote
            #[derive(serde::Serialize)]
            struct QuotedHeaderRow {
                #[serde(rename = "a,b")]
                a: u32,
            }
            let mut serde_never = PushEmitter::with_options(
                FormatOptions::CSV.quoting(Quoting::Never),
                EmitOptions::new().has_headers(true),
            )
            .unwrap();
            assert!(serde_never.serialize(&QuotedHeaderRow { a: 1 }).is_err());
        }

        // Null-aware text record
        let mut tr = TextRecord::new();
        tr.push_field("foo");
        tr.push_null();
        let mut null_emitter = PushEmitter::default();
        null_emitter.emit_text_record(&tr).unwrap();

        // encode_header, encode, encode_all, as_vec, into_inner
        #[derive(Clone, Copy)]
        struct Simple {
            a: &'static str,
            b: u32,
        }
        impl CsvEncode for Simple {
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

        let mut enc_emitter = PushEmitter::<crate::format::Csv>::new(EmitOptions::new()).unwrap();
        enc_emitter.encode_header::<Simple>().unwrap();
        enc_emitter.encode(&Simple { a: "boston", b: 1 }).unwrap();
        enc_emitter
            .encode_all([Simple { a: "austin", b: 2 }])
            .unwrap();
        assert!(!enc_emitter.as_vec().is_empty());
        let out = enc_emitter.into_inner();
        assert_eq!(out, b"a,b\nboston,1\naustin,2\n");

        // build on non-empty buffer with MatchFirst
        let mut buf_mf = PushEmitter::<crate::format::Csv>::build(
            b"h1,h2\nv1,v2\n".to_vec(),
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert_eq!(buf_mf.expected_fields, Some(2));
        buf_mf.emit_record(["v3", "v4"]).unwrap();

        // build on non-empty buffer with MatchFirst but no records
        let buf_no_records = PushEmitter::<crate::format::Csv>::build(
            b"# comment only\n".to_vec(),
            FormatOptions::CSV.comment(Some(b'#')),
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .unwrap();
        assert_eq!(buf_no_records.expected_fields, None);

        let mut dyn_enc =
            PushEmitter::with_options(FormatOptions::CSV, EmitOptions::new()).unwrap();
        dyn_enc.encode_all([Simple { a: "seattle", b: 3 }]).unwrap();

        // encode_all with error
        let mut fail_enc = PushEmitter::with_options(
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::Exact(1)),
        )
        .unwrap();
        assert!(fail_enc.encode_all([Simple { a: "x", b: 1 }]).is_err());

        // emit_slices
        let mut slices_enc = PushEmitter::<crate::format::Csv>::new(EmitOptions::new()).unwrap();
        slices_enc.emit_slices(&[b"foo", b"bar"]).unwrap();
        assert_eq!(slices_enc.into_inner(), b"foo,bar\n");
    }

    #[test]
    fn slice_reservation_and_allocation_errors_are_exact() {
        let mut impossible = Vec::new();
        let error = reserve_output_capacity(&mut impossible, usize::MAX)
            .expect_err("impossible reservation");
        assert_eq!(error.kind(), ErrorKind::Encode);
        assert_eq!(error.to_string(), "emitter output allocation failed");

        let mut buffer = Vec::new();
        let fields = [b"abcd".as_slice(), b"ef".as_slice()];
        assert_eq!(
            emit_slices_runtime(
                &mut buffer,
                Dialect::default(),
                Quoting::Necessary,
                Nulls::None,
                &fields,
            )
            .expect("record"),
            2,
        );
        assert_eq!(buffer, b"abcd,ef\n");
        assert_eq!(buffer.capacity(), 20);

        let options = EmitOptions::new();
        assert_eq!(
            PushEmitter::<Dynamic>::existing_field_count(b"a,b\n", FormatOptions::CSV, options,)
                .expect("flexible policy"),
            None,
        );
        assert_eq!(
            PushEmitter::<Dynamic>::existing_field_count(
                b"a,b\n",
                FormatOptions::CSV,
                options.field_count(FieldCount::Exact(2)),
            )
            .expect("exact policy"),
            None,
        );

        let mut mysql =
            PushEmitter::with_options(FormatOptions::MYSQL, EmitOptions::new()).expect("emitter");
        mysql.emit_slices(&[b"say \"hi\""]).expect("MySQL record");
        assert_eq!(mysql.buffer(), b"say \\\"hi\\\"\n");

        let mut mysql =
            PushEmitter::with_options(FormatOptions::MYSQL, EmitOptions::new()).expect("emitter");
        mysql
            .emit_record([b"say \"hi\""])
            .expect("MySQL iterator record");
        assert_eq!(mysql.buffer(), b"say \\\"hi\\\"\n");

        let live = 9 * 1024;
        let mut reclaimed = Vec::with_capacity(live * 5);
        reclaimed.resize(live, b'x');
        reclaim_live(&mut reclaimed);
        assert_eq!(reclaimed.capacity(), live);
    }

    #[test]
    fn builder_scratch_is_returned_empty_and_reclaimed_on_clear() {
        let mut emitter = PushEmitter::default();
        let mut record = ByteRecord::with_capacity(4, 64 * 1024);
        record.push_field(b"x");
        let grown = record.byte_capacity();
        assert!(grown >= 64 * 1024);
        emitter.return_builder_record(record);

        let recycled = emitter.take_builder_record();
        assert!(recycled.is_empty());
        assert!(recycled.byte_capacity() >= grown);
        emitter.return_builder_record(recycled);

        let before_clear = emitter.builder_scratch.byte_capacity();
        emitter.clear();
        assert!(
            emitter.builder_scratch.byte_capacity() < before_clear,
            "clear must reclaim the returned builder scratch",
        );
    }

    #[test]
    fn pending_record_debug_and_drop_restore_the_staging_record() {
        let mut emitter = PushEmitter::default();
        {
            let mut pending = emitter.begin_record();
            pending.write_field(vec![b'x'; 1024]).expect("field");
            pending.write_field("two").expect("field");
            assert_eq!(
                format!("{pending:?}"),
                "PendingPushRecord { pending_fields: 2 }",
            );
        }
        assert!(emitter.buffer().is_empty());
        assert_eq!(emitter.builder_scratch.len(), 2);
        assert!(emitter.builder_scratch.byte_capacity() >= 1024);

        {
            let mut pending = emitter.begin_record();
            assert_eq!(
                format!("{pending:?}"),
                "PendingPushRecord { pending_fields: 0 }"
            );
            pending.write_field("committed").expect("field");
            pending.finish().expect("record");
        }
        assert_eq!(emitter.buffer(), b"committed\n");
    }

    #[test]
    fn null_aware_records_take_the_nullable_encoding_path() {
        let format = FormatOptions::CSV.nulls(Nulls::Mysql);
        let mut bytes = ByteRecord::new();
        bytes.push_field(b"value");
        bytes.push_null();
        let mut byte_emitter =
            PushEmitter::with_options(format, EmitOptions::new()).expect("emitter");
        byte_emitter.emit_byte_record(&bytes).expect("byte record");
        assert_eq!(byte_emitter.buffer(), b"value,\\N\n");

        let mut text = TextRecord::new();
        text.push_field("value");
        text.push_null();
        let mut text_emitter =
            PushEmitter::with_options(format, EmitOptions::new()).expect("emitter");
        text_emitter.emit_text_record(&text).expect("text record");
        assert_eq!(text_emitter.buffer(), b"value,\\N\n");
    }

    #[test]
    fn rejected_records_rollback_output_and_preserve_match_first_width() {
        let mut emitter = PushEmitter::with_options(
            FormatOptions::CSV.write_bom(WriteBom::Emit),
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .expect("emitter");
        emitter.emit_record(["a", "b"]).expect("first record");
        let committed = emitter.buffer().to_vec();
        assert_eq!(emitter.expected_fields, Some(2));

        let error = emitter.emit_record(["one"]).expect_err("width mismatch");
        assert_eq!(
            error.kind(),
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 1,
            },
        );
        assert_eq!(emitter.buffer(), committed);
        assert_eq!(emitter.expected_fields, Some(2));

        emitter.clear();
        emitter.emit_record(["c", "d"]).expect("same width");
        assert_eq!(emitter.buffer(), b"c,d\n", "BOM is not repeated");
    }

    #[cfg(feature = "serde")]
    #[test]
    fn first_serde_commit_without_named_headers_uses_data_width_and_start() {
        let mut headers = ByteRecord::new();
        headers.push_field(b"left");
        headers.push_field(b"right");

        let mut rejected = PushEmitter::with_options(
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::Exact(2)),
        )
        .expect("emitter");
        rejected.buffer.extend_from_slice(b"kept\n");
        let start = rejected.buffer.len();
        rejected.buffer.extend_from_slice(b"one\n");
        let error = rejected
            .commit_first_with_headers(start, &headers, Ok((false, 1)))
            .expect_err("data width is validated");
        assert_eq!(
            error.kind(),
            ErrorKind::FieldCountMismatch {
                expected: 2,
                actual: 1,
            },
        );
        assert_eq!(rejected.buffer(), b"kept\n");

        let mut accepted = PushEmitter::with_options(
            FormatOptions::CSV,
            EmitOptions::new().field_count(FieldCount::MatchFirst),
        )
        .expect("emitter");
        accepted.buffer.extend_from_slice(b"a,b\n");
        accepted
            .commit_first_with_headers(0, &headers, Ok((false, 2)))
            .expect("first data width");
        assert_eq!(accepted.buffer(), b"a,b\n", "headers were not named");
        assert_eq!(accepted.expected_fields, Some(2));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_scratch_and_headers_are_returned_after_each_serialization() {
        #[derive(serde::Serialize)]
        struct Row {
            name: String,
            value: u32,
        }

        let mut emitter =
            PushEmitter::with_options(FormatOptions::CSV, EmitOptions::new().has_headers(true))
                .expect("emitter");
        emitter
            .serialize(&Row {
                name: "x".repeat(64 * 1024),
                value: 1,
            })
            .expect("first row");
        assert!(emitter.serde_scratch.is_empty());
        assert_eq!(
            emitter.serde_header.iter().collect::<Vec<_>>(),
            [b"name".as_slice(), b"value"],
        );
        let scratch = emitter.serde_scratch.capacity();
        emitter
            .serialize(&Row {
                name: "short".to_owned(),
                value: 2,
            })
            .expect("second row");
        assert_eq!(emitter.serde_scratch.capacity(), scratch);

        emitter.serde_scratch.reserve(64 * 1024);
        let scratch = emitter.serde_scratch.capacity();
        let mut oversized_header = ByteRecord::with_capacity(2, 64 * 1024);
        oversized_header.push_field(b"name");
        oversized_header.push_field(b"value");
        emitter.serde_header = oversized_header;
        let header_capacity = emitter.serde_header.byte_capacity();
        emitter.clear();
        assert!(emitter.serde_scratch.capacity() < scratch);
        assert!(emitter.serde_header.byte_capacity() < header_capacity);
    }
}

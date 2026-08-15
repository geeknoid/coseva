#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
use core::fmt;
use core::mem;

use crate::byte_record::ByteRecord;
use crate::config::{EmitOptions, FormatOptions};
use crate::encoding::CsvEncode;
use crate::error::Error;
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::push_emitter::PushEmitter;
use crate::text_record::TextRecord;

/// CSV emitter that builds the whole document in a byte vector.
///
/// Use this when the finished document is what you want — to return it from a
/// function, put it in an HTTP response, or compare it in a test. The whole
/// document accumulates in memory; use [`crate::IoEmitter`] for output larger
/// than memory, or [`crate::PushEmitter`] when something else owns the write
/// loop.
///
/// ```
/// use coseva::VecEmitter;
///
/// let mut emitter = VecEmitter::default();
/// emitter.emit_record(["city", "population"])?;
/// emitter.emit_record(["Boston", "650706"])?;
///
/// assert_eq!(emitter.as_bytes(), b"city,population\nBoston,650706\n");
/// # Ok::<(), coseva::Error>(())
/// ```
#[derive(Debug)]
pub struct VecEmitter<F: CsvFormat = Dynamic> {
    core: PushEmitter<F>,
}

impl<F: StaticFormat> VecEmitter<F> {
    /// Create an emitter over `output`, encoding the format `F`.
    ///
    /// ```
    /// use coseva::VecEmitter;
    /// use coseva::config::EmitOptions;
    /// use coseva::format::Tsv;
    ///
    /// let mut emitter = VecEmitter::<Tsv>::new(Vec::new(), EmitOptions::new())?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston\t650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the encode options are invalid, or when existing
    /// output cannot establish the width required by `FieldCount::MatchFirst`.
    pub fn new(output: Vec<u8>, options: EmitOptions) -> Result<Self, Error> {
        Self::build(output, F::FORMAT, options)
    }
}

impl VecEmitter<Dynamic> {
    /// Create a vector-backed emitter for an explicit format and encode options.
    ///
    /// ```
    /// use coseva::VecEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions};
    ///
    /// let mut emitter = VecEmitter::with_options(
    ///     Vec::new(),
    ///     FormatOptions::CSV.delimiter(b';'),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston;650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when the configured format is ambiguous, or when
    /// existing output cannot establish the width required by
    /// `FieldCount::MatchFirst`.
    pub fn with_options(
        output: Vec<u8>,
        format: FormatOptions,
        options: EmitOptions,
    ) -> Result<Self, Error> {
        Self::build(output, format, options)
    }
}

impl<F: CsvFormat> VecEmitter<F> {
    /// The shared fallible constructor behind `new` and `with_options`.
    fn build(output: Vec<u8>, format: FormatOptions, options: EmitOptions) -> Result<Self, Error> {
        Ok(Self {
            core: PushEmitter::build(output, format, options)?,
        })
    }

    /// Append one encoded record.
    ///
    /// ```
    /// use coseva::VecEmitter;
    ///
    /// let mut emitter = VecEmitter::default();
    /// emitter.emit_record(["Boston", "650706"])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`](crate::config::Quoting::Never) is configured.
    pub fn emit_record<I, T>(&mut self, fields: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        self.core.emit_record(fields)
    }

    /// Append one record from a slice without intermediate capacity checks.
    ///
    /// Unlike [`Self::emit_record`], which accepts any iterator of field
    /// values, this takes an already-materialized slice of byte slices, which
    /// lets it size its output allocation up front.
    ///
    /// ```
    /// use coseva::VecEmitter;
    ///
    /// let mut emitter = VecEmitter::default();
    /// emitter.emit_slices(&[b"Boston", b"650706"])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`](crate::config::Quoting::Never) is configured.
    pub fn emit_slices(&mut self, fields: &[&[u8]]) -> Result<(), Error> {
        self.core.emit_slices(fields)
    }

    /// Borrow the encoded bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.core.buffer()
    }

    /// Borrow the underlying byte vector.
    #[must_use]
    pub const fn as_vec(&self) -> &Vec<u8> {
        self.core.as_vec()
    }

    /// Append one encoded record from a [`ByteRecord`].
    ///
    /// ```
    /// use coseva::VecEmitter;
    /// use coseva::ByteRecord;
    ///
    /// let mut emitter = VecEmitter::default();
    /// let record: ByteRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_byte_record(&record)?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`](crate::config::Quoting::Never) is configured.
    pub fn emit_byte_record(&mut self, record: &ByteRecord) -> Result<(), Error> {
        self.core.emit_byte_record(record)
    }

    /// Append one encoded record from a [`TextRecord`].
    ///
    /// ```
    /// use coseva::VecEmitter;
    /// use coseva::TextRecord;
    ///
    /// let mut emitter = VecEmitter::default();
    /// let record: TextRecord = ["Boston", "650706"].into_iter().collect();
    /// emitter.emit_text_record(&record)?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`](crate::config::Quoting::Never) is configured.
    pub fn emit_text_record(&mut self, record: &TextRecord) -> Result<(), Error> {
        self.core.emit_text_record(record)
    }

    /// Append one record whose fields may contain explicit NULL values.
    ///
    /// A `None` item is encoded using the configured
    /// [`Nulls`](crate::config::Nulls).
    ///
    /// ```
    /// use coseva::VecEmitter;
    /// use coseva::config::{EmitOptions, FormatOptions, Nulls};
    /// use coseva::format::Dynamic;
    ///
    /// let mut emitter = VecEmitter::<Dynamic>::with_options(
    ///     Vec::new(),
    ///     FormatOptions::CSV.nulls(Nulls::PostgresCsv),
    ///     EmitOptions::new(),
    /// )?;
    /// emitter.emit_nullable_record([Some(&b"Boston"[..]), None])?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,\n");
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
        self.core.emit_nullable_record(fields)
    }

    /// Write the static headers declared by a native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::VecEmitter;
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = VecEmitter::default();
    /// emitter.encode_header::<City>()?;
    /// assert_eq!(emitter.as_bytes(), b"name,pop\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an encoding or field-count error.
    pub fn encode_header<T: CsvEncode>(&mut self) -> Result<(), Error> {
        self.core.encode_header::<T>()
    }

    /// Encode one native typed record.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::VecEmitter;
    /// use coseva::encoding::CsvEncode;
    ///
    /// #[derive(CsvEncode)]
    /// struct City {
    ///     name: &'static str,
    ///     pop: u32,
    /// }
    ///
    /// let mut emitter = VecEmitter::default();
    /// emitter.encode(&City { name: "Boston", pop: 650_706 })?;
    /// assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a typed encoding or CSV output error.
    pub fn encode<T: CsvEncode>(&mut self, value: &T) -> Result<(), Error> {
        self.core.encode(value)
    }

    /// Encode every native typed record from an iterator.
    ///
    /// Unlike [`Self::encode_header`], this never writes headers on its own;
    /// call `encode_header` first if a header row is wanted.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use coseva::VecEmitter;
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
    /// let mut emitter = VecEmitter::default();
    /// emitter.encode_header::<City>()?;
    /// emitter.encode_all(cities)?;
    /// assert_eq!(emitter.as_bytes(), b"name,pop\nBoston,650706\nLondon,8982000\n");
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the first typed encoding or CSV output error.
    pub fn encode_all<T, I>(&mut self, values: I) -> Result<(), Error>
    where
        T: CsvEncode,
        I: IntoIterator<Item = T>,
    {
        self.core.encode_all(values)
    }

    /// Start a field-at-a-time record builder.
    ///
    /// Fields are buffered until [`PendingVecRecord::finish`] is called.
    /// Dropping the returned guard without calling `finish` commits nothing.
    pub fn begin_record(&mut self) -> PendingVecRecord<'_, F> {
        PendingVecRecord::new(self)
    }

    /// Consume this emitter.
    #[must_use]
    pub fn into_inner(self) -> Vec<u8> {
        self.core.into_inner()
    }

    /// Serialize `value` as a CSV record using Serde.
    ///
    /// The complete record is collected into an internal buffer before being
    /// written, so no partial records are committed on error.
    ///
    /// ```
    /// # #[cfg(feature = "serde")] {
    /// use coseva::VecEmitter;
    /// use serde::Serialize;
    ///
    /// #[derive(Serialize)]
    /// struct City {
    ///     name: String,
    ///     population: u32,
    /// }
    ///
    /// let mut emitter = VecEmitter::default();
    /// emitter.serialize(&City { name: "Boston".to_owned(), population: 650_706 })?;
    /// assert_eq!(emitter.as_bytes(), b"name,population\nBoston,650706\n");
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
        self.core.serialize(value)
    }
}

/// Write into an existing vector, using the default CSV format.
impl From<Vec<u8>> for VecEmitter<Dynamic> {
    fn from(output: Vec<u8>) -> Self {
        Self {
            core: PushEmitter::from(output),
        }
    }
}

impl Default for VecEmitter<Dynamic> {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

/// A field-at-a-time record builder that guards one pending record in a [`VecEmitter`].
///
/// Fields accumulate in an internal buffer until [`PendingVecRecord::finish`]
/// is called. Dropping this guard without calling `finish` discards all
/// buffered fields and commits nothing to the underlying emitter.
/// For a worked example, see [`VecEmitter`].
pub struct PendingVecRecord<'writer, F: CsvFormat = Dynamic> {
    writer: &'writer mut VecEmitter<F>,
    record: ByteRecord,
}

impl<'writer, F: CsvFormat> PendingVecRecord<'writer, F> {
    fn new(writer: &'writer mut VecEmitter<F>) -> Self {
        let record = writer.core.take_builder_record();
        Self { writer, record }
    }

    /// Append one field to the pending record.
    ///
    /// # Errors
    ///
    /// Currently infallible; returns a result for symmetry with
    /// [`VecEmitter::emit_record`] and to support future validation.
    pub fn write_field(&mut self, field: impl AsRef<[u8]>) -> Result<(), Error> {
        self.record.push_field(field);
        Ok(())
    }

    /// Append an explicit NULL field to the pending record.
    ///
    /// # Errors
    ///
    /// Currently infallible; returns a result for API symmetry.
    pub fn write_null(&mut self) -> Result<(), Error> {
        self.record.push_null();
        Ok(())
    }

    /// Encode and append the complete record, consuming the guard.
    ///
    /// # Errors
    ///
    /// Returns an error when a field requires quoting while
    /// [`Quoting::Never`](crate::config::Quoting::Never) is configured.
    pub fn finish(self) -> Result<(), Error> {
        // `self` is dropped at the end of this expression, and that `Drop` is
        // what returns the staging record to the emitter.
        self.writer.emit_byte_record(&self.record)
    }
}

impl<F: CsvFormat> Drop for PendingVecRecord<'_, F> {
    fn drop(&mut self) {
        self.writer
            .core
            .return_builder_record(mem::take(&mut self.record));
    }
}

impl<F: CsvFormat> fmt::Debug for PendingVecRecord<'_, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingVecRecord")
            .field("pending_fields", &self.record.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_vec_record_debug_and_drop_return_builder_storage() {
        let mut emitter = VecEmitter::default();
        {
            let mut pending = emitter.begin_record();
            pending.write_field(vec![b'x'; 1024]).expect("field");
            pending.write_field("two").expect("field");
            assert_eq!(
                format!("{pending:?}"),
                "PendingVecRecord { pending_fields: 2 }",
            );
        }
        assert!(emitter.as_bytes().is_empty());
        let recycled = emitter.core.take_builder_record();
        assert!(recycled.is_empty());
        assert!(recycled.byte_capacity() >= 1024);
        emitter.core.return_builder_record(recycled);

        {
            let mut pending = emitter.begin_record();
            assert_eq!(
                format!("{pending:?}"),
                "PendingVecRecord { pending_fields: 0 }"
            );
            pending.write_field("committed").expect("field");
            pending.finish().expect("record");
        }
        assert_eq!(emitter.as_bytes(), b"committed\n");
    }
}

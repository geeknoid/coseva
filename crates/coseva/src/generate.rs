#[cfg(all(not(feature = "std"), not(test), feature = "serde"))]
use alloc::format;
#[cfg(all(not(feature = "std"), not(test)))]
use alloc::vec::Vec;
#[cfg(feature = "std")]
use std::fs::File;
#[cfg(feature = "std")]
use std::io;
#[cfg(feature = "std")]
use std::io::Write;
#[cfg(feature = "std")]
use std::path::{Path, PathBuf};

#[cfg(feature = "std")]
use crate::config::WriteBom;
use crate::config::{EmitOptions, FormatOptions};
use crate::encoding::CsvEncode;
use crate::error::Error;
#[cfg(feature = "std")]
use crate::error::ErrorKind;
#[cfg(feature = "std")]
use crate::format::{Csv, CsvFormat};
#[cfg(feature = "std")]
use crate::io_emitter::IoEmitter;
#[cfg(feature = "std")]
use crate::push_emitter::PushEmitter;
use crate::vec_emitter::VecEmitter;

/// Encode every value from an iterator into a new byte vector.
///
/// The whole document is retained, since it is what gets returned. Use
/// [`encode_to_writer`] or [`encode_to_path`] for output that should not be held in memory.
///
/// A header record is written first from [`CsvEncode::field_names`]. Disable
/// it with [`EmitOptions::has_headers`].
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::encoding::CsvEncode;
/// use coseva::encode_to_vec;
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
/// assert_eq!(
///     encode_to_vec(cities, FormatOptions::CSV, EmitOptions::new())?,
///     b"name,pop\nBoston,650706\nLondon,8982000\n",
/// );
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or the first typed encoding or field-count
/// error.
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn encode_to_vec<T, I>(
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<Vec<u8>, Error>
where
    T: CsvEncode,
    I: IntoIterator<Item = T>,
{
    let write_headers = options.writes_headers();
    let mut emitter = VecEmitter::with_options(Vec::new(), format, options)?;
    if write_headers {
        emitter.encode_header::<T>()?;
    }
    emitter.encode_all(values)?;
    Ok(emitter.into_inner())
}

/// Encode every value from an iterator into `output`, then flush it.
///
/// Values are pulled one at a time and the emitter drains on its buffer
/// threshold, so resident memory stays flat however many records the iterator
/// yields — unlike [`encode_to_vec`], and unlike the one-shot deserializers, which
/// are proportional to their input.
///
/// A header record is written first from [`CsvEncode::field_names`]. Disable
/// it with [`EmitOptions::has_headers`].
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::encoding::CsvEncode;
/// use coseva::encode_to_writer;
///
/// #[derive(CsvEncode)]
/// struct City {
///     name: &'static str,
///     pop: u32,
/// }
///
/// let cities = [City { name: "Boston", pop: 650_706 }];
/// let mut output = Vec::new();
/// encode_to_writer(&mut output, cities, FormatOptions::CSV, EmitOptions::new())?;
///
/// assert_eq!(output, b"name,pop\nBoston,650706\n");
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Borrowing the writer
///
/// `output` is taken by value. A caller that must keep the writer can pass
/// `&mut writer` instead, since `&mut W` implements [`std::io::Write`]
/// wherever `W: Write` does.
///
/// # Errors
///
/// Returns a configuration error, or the first typed encoding, field-count, or
/// I/O error. Because output is written incrementally, an error may leave a
/// partial document in `output`.
#[cfg(feature = "std")]
pub fn encode_to_writer<W, T, I>(
    output: W,
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<(), Error>
where
    W: io::Write,
    T: CsvEncode,
    I: IntoIterator<Item = T>,
{
    let write_headers = options.writes_headers();
    if format == FormatOptions::CSV {
        let mut emitter = IoEmitter::<_, Csv>::new(output, options)?;
        drive(&mut emitter, values, write_headers)
    } else {
        let mut emitter = IoEmitter::with_options(output, format, options)?;
        drive(&mut emitter, values, write_headers)
    }
}

/// Create a file and encode every value from an iterator into it.
///
/// Resident memory stays flat however many records the iterator yields, so
/// this is the entry point for generating a file larger than memory in one
/// call.
///
/// A header record is written first from [`CsvEncode::field_names`]. Disable
/// it with [`EmitOptions::has_headers`].
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::encoding::CsvEncode;
/// use coseva::encode_to_path;
///
/// #[derive(CsvEncode)]
/// struct City {
///     name: &'static str,
///     pop: u32,
/// }
///
/// let directory = tempfile::tempdir()?;
/// let path = directory.path().join("cities.csv");
/// let cities = [City { name: "Boston", pop: 650_706 }];
/// encode_to_path(&path, cities, FormatOptions::CSV, EmitOptions::new())?;
///
/// assert_eq!(std::fs::read(&path)?, b"name,pop\nBoston,650706\n");
/// # }
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, an error when the file cannot be created, or
/// the first typed encoding, field-count, or I/O error. Because output is
/// written incrementally, an error may leave a partial file behind.
#[cfg(feature = "std")]
pub fn encode_to_path<P, T, I>(
    path: P,
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<(), Error>
where
    P: AsRef<Path>,
    T: CsvEncode,
    I: IntoIterator<Item = T>,
{
    let write_headers = options.writes_headers();
    if format == FormatOptions::CSV {
        let mut emitter = IoEmitter::<File, Csv>::new_path(path, options)?;
        drive(&mut emitter, values, write_headers)
    } else {
        let mut emitter = IoEmitter::to_path(path, format, options)?;
        drive(&mut emitter, values, write_headers)
    }
}

/// Drive `values` to completion and finalize the sink.
///
/// Finalizing is the whole reason these entry points exist: a caller that
/// forgets to flush gets a truncated document, and here it cannot be
/// forgotten.
#[cfg(feature = "std")]
fn drive<W, F, T, I>(
    emitter: &mut IoEmitter<W, F>,
    values: I,
    write_headers: bool,
) -> Result<(), Error>
where
    W: io::Write,
    F: CsvFormat,
    T: CsvEncode,
    I: IntoIterator<Item = T>,
{
    if write_headers {
        emitter.encode_header::<T>()?;
    }
    emitter.encode_all(values)?;
    emitter.flush()
}

/// Serialize every value from an iterator into a new byte vector using Serde.
///
/// The whole document is retained, since it is what gets returned. Use
/// [`serialize_to_writer`] or [`serialize_to_path`] for output that should not
/// be held in memory.
///
/// # Headers
///
/// Serde header names come from the first value actually serialized, not from
/// the type, so an empty iterator produces an empty document rather than a
/// lone header record. This differs from [`encode_to_vec`], where the header comes
/// from [`CsvEncode::field_names`] and is written even for an empty iterator.
/// Disable it with [`EmitOptions::has_headers`].
///
/// ```
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::serialize_to_vec;
/// use serde::Serialize;
///
/// #[derive(Serialize)]
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
/// assert_eq!(
///     serialize_to_vec(&cities, FormatOptions::CSV, EmitOptions::new())?,
///     b"name,pop\nBoston,650706\nLondon,8982000\n",
/// );
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, or the first serialization or field-count
/// error.
#[cfg(feature = "serde")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn serialize_to_vec<T, I>(
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<Vec<u8>, Error>
where
    T: ::serde::Serialize,
    I: IntoIterator<Item = T>,
{
    let mut emitter = VecEmitter::with_options(Vec::new(), format, options)?;
    for value in values {
        emitter.serialize(&value)?;
    }
    Ok(emitter.into_inner())
}

/// Serialize every value from an iterator into `output` using Serde, then
/// flush it.
///
/// Values are pulled one at a time and the emitter drains on its buffer
/// threshold, so resident memory stays flat however many records the iterator
/// yields.
///
/// See [`serialize_to_vec`] for how Serde headers differ from the native path.
///
/// ```
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::serialize_to_writer;
/// use serde::Serialize;
///
/// #[derive(Serialize)]
/// struct City {
///     name: &'static str,
///     pop: u32,
/// }
///
/// let cities = [City { name: "Boston", pop: 650_706 }];
/// let mut output = Vec::new();
/// serialize_to_writer(&mut output, &cities, FormatOptions::CSV, EmitOptions::new())?;
///
/// assert_eq!(output, b"name,pop\nBoston,650706\n");
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Borrowing the writer
///
/// `output` is taken by value. A caller that must keep the writer can pass
/// `&mut writer` instead, since `&mut W` implements [`std::io::Write`]
/// wherever `W: Write` does.
///
/// # Errors
///
/// Returns a configuration error, or the first serialization, field-count, or
/// I/O error. Because output is written incrementally, an error may leave a
/// partial document in `output`.
#[cfg(all(feature = "std", feature = "serde"))]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn serialize_to_writer<W, T, I>(
    output: W,
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<(), Error>
where
    W: io::Write,
    T: ::serde::Serialize,
    I: IntoIterator<Item = T>,
{
    let mut emitter = IoEmitter::with_options(output, format, options)?;
    drive_serialized(&mut emitter, values)
}

/// Create a file and serialize every value from an iterator into it.
///
/// Resident memory stays flat however many records the iterator yields, so
/// this is the entry point for generating a file larger than memory in one
/// call.
///
/// See [`serialize_to_vec`] for how Serde headers differ from the native path.
///
/// ```
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::serialize_to_path;
/// use serde::Serialize;
///
/// #[derive(Serialize)]
/// struct City {
///     name: &'static str,
///     pop: u32,
/// }
///
/// let directory = tempfile::tempdir()?;
/// let path = directory.path().join("cities.csv");
/// let cities = [City { name: "Boston", pop: 650_706 }];
/// serialize_to_path(&path, &cities, FormatOptions::CSV, EmitOptions::new())?;
///
/// assert_eq!(std::fs::read(&path)?, b"name,pop\nBoston,650706\n");
/// # Ok::<(), Box<dyn core::error::Error>>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, an error when the file cannot be created, or
/// the first serialization, field-count, or I/O error. Because output is
/// written incrementally, an error may leave a partial file behind.
#[cfg(all(feature = "std", feature = "serde"))]
pub fn serialize_to_path<P, T, I>(
    path: P,
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<(), Error>
where
    P: AsRef<Path>,
    T: ::serde::Serialize,
    I: IntoIterator<Item = T>,
{
    let mut emitter = IoEmitter::to_path(path, format, options)?;
    drive_serialized(&mut emitter, values)
}

/// Drive `values` through Serde to completion and finalize the sink.
///
/// The header, when enabled, is emitted by the first `serialize` call rather
/// than up front, so unlike [`drive`] there is nothing to write first.
#[cfg(all(feature = "std", feature = "serde"))]
#[cfg_attr(coverage_nightly, coverage(off))]
fn drive_serialized<W, T, I>(emitter: &mut IoEmitter<W>, values: I) -> Result<(), Error>
where
    W: io::Write,
    T: ::serde::Serialize,
    I: IntoIterator<Item = T>,
{
    for value in values {
        emitter.serialize(&value)?;
    }
    emitter.flush()
}

/// Append every value from an iterator to an existing CSV file.
///
/// The file is created when it does not exist. When it already holds records,
/// no header is written and no byte-order mark is emitted, so a document can be
/// produced across many runs. A file holding nothing but a byte-order mark has
/// no records yet, so the mark is not repeated but the header is still written.
/// Resident memory stays flat however many records the iterator yields.
///
/// ```no_run
/// # #[cfg(all(feature = "std", feature = "derive"))] {
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::encoding::CsvEncode;
/// use coseva::{encode_append_path, encode_to_path};
///
/// #[derive(CsvEncode)]
/// struct Event { id: u32, kind: &'static str }
///
/// let format = FormatOptions::CSV;
///
/// // The first run creates the file and writes its header row.
/// let first = [Event { id: 0, kind: "click" }];
/// encode_to_path("events.csv", first, format, EmitOptions::new())?;
///
/// // Later runs continue it; no second header row is emitted.
/// let later = [Event { id: 1, kind: "view" }];
/// encode_append_path("events.csv", later, format, EmitOptions::new())?;
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error, [`ErrorKind::UnterminatedRecord`] when the
/// existing file does not end with a record terminator, an error when the file
/// cannot be opened, or the first typed encoding, field-count, or I/O error.
#[cfg(feature = "std")]
pub fn encode_append_path<P, T, I>(
    path: P,
    values: I,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<(), Error>
where
    P: AsRef<Path>,
    T: CsvEncode,
    I: IntoIterator<Item = T>,
{
    let write_headers = options.writes_headers();
    let (mut emitter, resuming) = IoEmitter::<File>::append_path_resuming(path, format, options)?;
    // A header belongs only at the start of a document, so resuming one that
    // already has records must not emit a second.
    drive(&mut emitter, values, write_headers && !resuming)
}

/// Encode every value from an iterator into a numbered sequence of files.
///
/// `namer` is called with `0`, `1`, `2`, ... to name each part, so any layout
/// the caller wants is expressible. A new part is started whenever adding the
/// next record would push the current one past `max_bytes`. Every part is a
/// complete CSV document in its own right: it repeats the header record and the
/// byte-order mark when those are configured, so a part can be parsed on its
/// own without reference to its siblings.
///
/// Returns the paths written, in order.
///
/// # Size bound
///
/// `max_bytes` bounds each part, but a part always holds at least one record.
/// A single record larger than `max_bytes` — together with the preamble it must
/// repeat — therefore produces an oversized part rather than a truncated or
/// split one, since splitting a record would corrupt it. An empty iterator
/// produces one part containing just the header, matching [`encode_to_path`].
///
/// ```no_run
/// # #[cfg(all(feature = "std", feature = "derive"))] {
/// use coseva::config::{EmitOptions, FormatOptions};
/// use coseva::encoding::CsvEncode;
/// use coseva::encode_to_segments;
///
/// #[derive(CsvEncode)]
/// struct Event { id: u32, kind: &'static str }
///
/// let events = (0..100_000).map(|id| Event { id, kind: "click" });
///
/// // As many 4 MiB parts as it takes; you choose the names.
/// let parts = encode_to_segments(
///     events,
///     4 << 20,
///     |part| format!("events-{part:03}.csv"),
///     FormatOptions::CSV,
///     EmitOptions::new(),
/// )?;
/// println!("wrote {} parts", parts.len());
/// # }
/// # Ok::<(), coseva::Error>(())
/// ```
///
/// # Errors
///
/// Returns a configuration error when `max_bytes` is zero or the options are
/// invalid, an error when a part cannot be created, or the first typed
/// encoding, field-count, or I/O error. Because parts are written
/// incrementally, an error may leave earlier parts complete and the current
/// one partial.
#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
pub fn encode_to_segments<T, I, F, P>(
    values: I,
    max_bytes: u64,
    mut namer: F,
    format: FormatOptions,
    options: EmitOptions,
) -> Result<Vec<PathBuf>, Error>
where
    T: CsvEncode,
    I: IntoIterator<Item = T>,
    F: FnMut(usize) -> P,
    P: AsRef<Path>,
{
    options.validate_buffered(format)?;
    let max_bytes = SegmentLimit::new(max_bytes)?;

    let preamble = segment_preamble::<T>(format, options)?;
    let threshold = BufferThreshold::from_options(&options);
    // The preamble is written per part, so the core emitter must not splice a
    // byte-order mark of its own; it would land only in the first part.
    let mut core = PushEmitter::with_options(format.write_bom(WriteBom::Omit), options)?;

    let mut parts = Vec::new();
    let mut part = create_segment(&mut namer, 0, &preamble, &mut parts)?;
    let mut progress = SegmentProgress::from_preamble(&preamble);

    for value in values {
        let start = core.len();
        core.encode(&value)?;
        let buffered = BufferedSize::from_emitter(&core);

        if segment_would_overflow(progress, buffered, max_bytes) {
            // Everything before this record still belongs to the part being
            // closed; the record itself opens the next one.
            write_segment(&mut part, &core.buffer()[..start])?;
            part = create_segment(&mut namer, parts.len(), &preamble, &mut parts)?;
            progress = SegmentProgress::from_preamble(&preamble);
            core.buffer_mut().drain(..start);
        }

        progress.has_records = true;
        let buffered = BufferedSize::from_emitter(&core);
        if buffer_ready(buffered, threshold) {
            progress.written = progress.written.saturating_add(buffered.0);
            write_segment(&mut part, core.buffer())?;
            core.clear();
        }
    }

    write_segment(&mut part, core.buffer())?;
    part.flush().map_err(Error::io_at_start)?;
    Ok(parts)
}

#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BufferedSize(u64);

#[cfg(feature = "std")]
impl BufferedSize {
    fn from_emitter(emitter: &PushEmitter) -> Self {
        Self(emitter.len() as u64)
    }
}

#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BufferThreshold(u64);

#[cfg(feature = "std")]
impl BufferThreshold {
    fn from_options(options: &EmitOptions) -> Self {
        Self(options.capacity() as u64)
    }
}

#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SegmentLimit(u64);

#[cfg(feature = "std")]
impl SegmentLimit {
    fn new(max_bytes: u64) -> Result<Self, Error> {
        if max_bytes == 0 {
            Err(Error::detailed(
                ErrorKind::Configuration,
                "segment size must be greater than zero",
            ))
        } else {
            Ok(Self(max_bytes))
        }
    }
}

#[cfg(feature = "std")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SegmentProgress {
    written: u64,
    has_records: bool,
}

#[cfg(feature = "std")]
impl SegmentProgress {
    fn from_preamble(preamble: &[u8]) -> Self {
        Self {
            written: preamble.len() as u64,
            has_records: false,
        }
    }
}

#[cfg(feature = "std")]
fn segment_would_overflow(
    progress: SegmentProgress,
    buffered: BufferedSize,
    max_bytes: SegmentLimit,
) -> bool {
    progress.has_records && progress.written.saturating_add(buffered.0) > max_bytes.0
}

#[cfg(feature = "std")]
fn buffer_ready(buffered: BufferedSize, threshold: BufferThreshold) -> bool {
    buffered.0 >= threshold.0
}

/// The bytes every part repeats: the byte-order mark, then the header record.
#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn segment_preamble<T: CsvEncode>(
    format: FormatOptions,
    options: EmitOptions,
) -> Result<Vec<u8>, Error> {
    let mut preamble = Vec::new();
    if format.emits_bom() {
        preamble.extend_from_slice(b"\xEF\xBB\xBF");
    }
    if options.writes_headers() {
        let mut header = PushEmitter::with_options(format.write_bom(WriteBom::Omit), options)?;
        header.encode_header::<T>()?;
        preamble.extend_from_slice(header.buffer());
    }
    Ok(preamble)
}

/// Create the next part, record its path, and write the shared preamble.
#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn create_segment<F, P>(
    namer: &mut F,
    index: usize,
    preamble: &[u8],
    parts: &mut Vec<PathBuf>,
) -> Result<File, Error>
where
    F: FnMut(usize) -> P,
    P: AsRef<Path>,
{
    let path = namer(index).as_ref().to_path_buf();
    let mut file = File::create(&path).map_err(Error::io_at_start)?;
    parts.push(path);
    file.write_all(preamble).map_err(Error::io_at_start)?;
    Ok(file)
}

/// Write buffered bytes to the part currently being filled.
#[cfg(feature = "std")]
#[cfg_attr(coverage_nightly, coverage(off))]
fn write_segment(part: &mut File, bytes: &[u8]) -> Result<(), Error> {
    part.write_all(bytes).map_err(Error::io_at_start)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "std")]
    struct TestDirectory(tempfile::TempDir);

    #[cfg(feature = "std")]
    impl AsRef<Path> for TestDirectory {
        fn as_ref(&self) -> &Path {
            self.0.path()
        }
    }

    #[cfg(feature = "std")]
    impl core::ops::Deref for TestDirectory {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            self.0.path()
        }
    }

    #[cfg(feature = "std")]
    fn test_directory(name: &str) -> TestDirectory {
        TestDirectory(
            tempfile::Builder::new()
                .prefix(name)
                .tempdir()
                .expect("create temporary test directory"),
        )
    }

    #[test]
    #[cfg(feature = "std")]
    fn segment_boundaries_and_generated_output_are_exact() {
        #[derive(Clone, Copy)]
        struct Row {
            a: u32,
            b: &'static str,
        }

        impl CsvEncode for Row {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                visitor: &mut V,
            ) -> Result<(), Error> {
                visitor.visit_field(0, "a", self.a.to_string().as_bytes())?;
                visitor.visit_field(1, "b", self.b.as_bytes())
            }

            fn field_names() -> &'static [&'static str] {
                &["a", "b"]
            }
        }

        assert!(SegmentLimit::new(0).is_err());
        assert_eq!(SegmentLimit::new(12), Ok(SegmentLimit(12)));
        assert!(!segment_would_overflow(
            SegmentProgress {
                written: 100,
                has_records: false,
            },
            BufferedSize(100),
            SegmentLimit(1),
        ));
        let progress = SegmentProgress {
            written: 4,
            has_records: true,
        };
        assert!(!segment_would_overflow(
            progress,
            BufferedSize(8),
            SegmentLimit(12),
        ));
        assert!(segment_would_overflow(
            progress,
            BufferedSize(9),
            SegmentLimit(12),
        ));
        assert!(!buffer_ready(BufferedSize(31), BufferThreshold(32)));
        assert!(buffer_ready(BufferedSize(32), BufferThreshold(32)));

        let options = EmitOptions::new().has_headers(false);
        let mut core = PushEmitter::with_options(FormatOptions::CSV, options)
            .expect("the test emitter is valid");
        core.encode(&Row { a: 1, b: "x" })
            .expect("the test row encodes");
        assert_eq!(BufferedSize::from_emitter(&core), BufferedSize(4));
        assert_eq!(
            BufferThreshold::from_options(&options),
            BufferThreshold(options.capacity() as u64),
        );
        assert_eq!(
            SegmentProgress::from_preamble(b"header\n"),
            SegmentProgress {
                written: 7,
                has_records: false,
            },
        );

        let directory = test_directory("segments");
        let rows = [
            Row { a: 1, b: "x" },
            Row { a: 2, b: "x" },
            Row { a: 3, b: "x" },
            Row { a: 4, b: "x" },
        ];
        let paths = encode_to_segments(
            rows,
            12,
            |index| directory.join(format!("part-{index}.csv")),
            FormatOptions::CSV,
            EmitOptions::new().has_headers(true).buffer_capacity(4),
        )
        .expect("segments are generated");

        assert_eq!(
            paths,
            [directory.join("part-0.csv"), directory.join("part-1.csv")]
        );
        assert_eq!(
            std::fs::read(&paths[0]).expect("read first segment"),
            b"a,b\n1,x\n2,x\n"
        );
        assert_eq!(
            std::fs::read(&paths[1]).expect("read second segment"),
            b"a,b\n3,x\n4,x\n"
        );
        std::fs::remove_dir_all(directory).expect("remove local test directory");
    }

    #[test]
    #[cfg(feature = "std")]
    fn segment_buffer_threshold_controls_when_bytes_reach_the_file() {
        #[derive(Clone, Copy)]
        struct Row(u32);

        impl CsvEncode for Row {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                visitor: &mut V,
            ) -> Result<(), Error> {
                visitor.visit_field(0, "value", self.0.to_string().as_bytes())
            }

            fn field_names() -> &'static [&'static str] {
                &["value"]
            }
        }

        fn length_before_second_record(capacity: usize, name: &str) -> u64 {
            let directory = test_directory(name);
            let path = directory.join("part.csv");
            let mut next = 0_u32;
            let mut observed = None;
            let rows = core::iter::from_fn(|| {
                if next == 1 {
                    observed = Some(std::fs::metadata(&path).expect("segment exists").len());
                }
                if next == 2 {
                    return None;
                }
                next += 1;
                Some(Row(next))
            });

            encode_to_segments(
                rows,
                100,
                |_| path.clone(),
                FormatOptions::CSV,
                EmitOptions::new()
                    .has_headers(false)
                    .buffer_capacity(capacity),
            )
            .expect("segments are generated");
            let observed = observed.expect("the iterator inspected the second record boundary");
            std::fs::remove_dir_all(directory).expect("remove local test directory");
            observed
        }

        assert_eq!(length_before_second_record(8, "buffer-not-ready"), 0);
        assert_eq!(length_before_second_record(2, "buffer-ready"), 2);
    }

    #[test]
    #[cfg(feature = "std")]
    fn zero_segment_size_reports_the_exact_configuration_error() {
        struct Row;
        impl CsvEncode for Row {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                _visitor: &mut V,
            ) -> Result<(), Error> {
                Ok(())
            }

            fn field_names() -> &'static [&'static str] {
                &[]
            }
        }

        let error = encode_to_segments::<Row, _, _, _>(
            Vec::new(),
            0,
            |_| PathBuf::from("unused"),
            FormatOptions::CSV,
            EmitOptions::new(),
        )
        .expect_err("zero is not a valid segment size");
        assert_eq!(error.kind(), ErrorKind::Configuration);
        assert_eq!(error.to_string(), "segment size must be greater than zero");
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_serialize_to_vec() {
        let items = [("a", 1), ("b", 2)];
        let res = serialize_to_vec(&items, FormatOptions::CSV, EmitOptions::new());
        assert!(res.is_ok());

        #[cfg(feature = "std")]
        {
            let mut writer = Vec::new();
            assert!(
                serialize_to_writer(&mut writer, &items, FormatOptions::CSV, EmitOptions::new())
                    .is_ok()
            );
            let directory = test_directory("serialize");
            let path = directory.join("coseva_test_ser_path.csv");
            assert!(
                serialize_to_path(&path, &items, FormatOptions::CSV, EmitOptions::new()).is_ok()
            );
            std::fs::remove_dir_all(directory).expect("remove local test directory");
        }
    }

    #[test]
    #[cfg(all(feature = "std", feature = "derive"))]
    fn test_generate_error_paths() {
        #[derive(Clone, coseva_macros::CsvEncode)]
        struct Row {
            a: u32,
            b: &'static str,
        }
        let items = [Row { a: 1, b: "x" }];
        let invalid_fmt = FormatOptions::CSV.delimiter(b'"').quote(b'"');
        let options = EmitOptions::new();
        let directory = test_directory("errors");
        let missing = directory.join("missing").join("test.csv");

        assert!(encode_to_vec(items.clone(), invalid_fmt, options).is_err());
        assert!(encode_to_writer(Vec::new(), items.clone(), invalid_fmt, options).is_err());
        assert!(encode_to_path(&missing, items.clone(), FormatOptions::CSV, options).is_err());
        assert!(encode_append_path(&missing, items.clone(), FormatOptions::CSV, options).is_err());
        assert!(
            encode_to_segments(
                items.clone(),
                100,
                |_| &missing,
                FormatOptions::CSV,
                options
            )
            .is_err()
        );

        // Error during encode_all in encode_to_vec and encode_to_writer
        let fc_options = EmitOptions::new().field_count(crate::config::FieldCount::Exact(5));
        assert!(encode_to_vec(items.clone(), FormatOptions::CSV, fc_options).is_err());
        assert!(
            encode_to_writer(Vec::new(), items.clone(), FormatOptions::CSV, fc_options).is_err()
        );
        assert!(
            encode_to_path(
                directory.join("test_fc.csv"),
                items.clone(),
                FormatOptions::CSV,
                fc_options
            )
            .is_err()
        );

        // Error during encode_header in encode_to_vec and encode_to_writer
        let fc_hdr_opts = EmitOptions::new()
            .field_count(crate::config::FieldCount::Exact(5))
            .has_headers(true);
        assert!(encode_to_vec(items.clone(), FormatOptions::CSV, fc_hdr_opts).is_err());
        assert!(
            encode_to_writer(Vec::new(), items.clone(), FormatOptions::CSV, fc_hdr_opts).is_err()
        );
        assert!(
            encode_to_path(
                directory.join("test_fc_hdr.csv"),
                items.clone(),
                FormatOptions::CSV,
                fc_hdr_opts
            )
            .is_err()
        );

        // Error during core.encode in encode_to_segments
        assert!(
            encode_to_segments(
                items.clone(),
                128,
                |i| directory.join(format!("seg-{i}.csv")),
                FormatOptions::CSV,
                fc_options
            )
            .is_err()
        );

        #[cfg(feature = "serde")]
        {
            let ser_items = [("a,with,comma", 1)];
            let nq_fmt = FormatOptions::CSV.quoting(crate::config::Quoting::Never);
            assert!(serialize_to_vec(&ser_items, nq_fmt, EmitOptions::new()).is_err());
            assert!(
                serialize_to_writer(Vec::new(), &ser_items, nq_fmt, EmitOptions::new()).is_err()
            );
            assert!(
                serialize_to_path(&missing, &ser_items, FormatOptions::CSV, EmitOptions::new())
                    .is_err()
            );
            assert!(
                serialize_to_path(
                    directory.join("test_ser_err.csv"),
                    &ser_items,
                    nq_fmt,
                    EmitOptions::new()
                )
                .is_err()
            );
        }

        // encode_to_segments options.validate_buffered error
        assert!(
            encode_to_segments(
                items.clone(),
                100,
                |_| directory.join("seg.csv"),
                invalid_fmt,
                options
            )
            .is_err()
        );
        std::fs::remove_dir_all(directory).expect("remove local test directory");
    }

    #[test]
    #[cfg(feature = "std")]
    fn test_encode_functions() {
        #[derive(Clone, Copy)]
        struct Row {
            a: u32,
            b: &'static str,
        }
        impl CsvEncode for Row {
            fn csv_encode<V: crate::encoding::EncodeVisitor>(
                &self,
                visitor: &mut V,
            ) -> Result<(), Error> {
                visitor.visit_field(0, "a", self.a.to_string().as_bytes())?;
                visitor.visit_field(1, "b", self.b.as_bytes())?;
                Ok(())
            }
            fn field_names() -> &'static [&'static str] {
                &["a", "b"]
            }
        }
        let items = [
            Row { a: 1, b: "x" },
            Row { a: 2, b: "y" },
            Row { a: 3, b: "z" },
        ];
        let directory = test_directory("encode-functions");

        // encode_to_vec with headers
        let vec_out = encode_to_vec(
            items.clone(),
            FormatOptions::CSV,
            EmitOptions::new().has_headers(true),
        );
        assert!(vec_out.is_ok());

        // encode_to_writer with headers
        let mut writer = Vec::new();
        assert!(
            encode_to_writer(
                &mut writer,
                items.clone(),
                FormatOptions::CSV,
                EmitOptions::new().has_headers(true)
            )
            .is_ok()
        );

        // serialize_to_vec
        #[cfg(feature = "serde")]
        {
            let ser_items = [("alice", 30), ("bob", 25)];
            assert!(serialize_to_vec(&ser_items, FormatOptions::CSV, EmitOptions::new()).is_ok());

            let mut ser_writer = Vec::new();
            assert!(
                serialize_to_writer(
                    &mut ser_writer,
                    &ser_items,
                    FormatOptions::CSV,
                    EmitOptions::new()
                )
                .is_ok()
            );

            let ser_path = directory.join("coseva_test_ser_path.csv");
            assert!(
                serialize_to_path(
                    &ser_path,
                    &ser_items,
                    FormatOptions::CSV,
                    EmitOptions::new()
                )
                .is_ok()
            );
        }

        // encode_to_path
        let path = directory.join("coseva_test_enc_path.csv");
        assert!(
            encode_to_path(&path, items.clone(), FormatOptions::CSV, EmitOptions::new()).is_ok()
        );

        // encode_append_path
        assert!(
            encode_append_path(&path, items.clone(), FormatOptions::CSV, EmitOptions::new())
                .is_ok()
        );

        // encode_to_segments zero bytes
        let items_empty: Vec<Row> = Vec::new();
        let res = encode_to_segments(
            items_empty,
            0,
            |i| directory.join(format!("zero-seg-{i}.csv")),
            FormatOptions::CSV,
            EmitOptions::new(),
        );
        assert!(res.is_err());

        // encode_to_segments with small max_bytes and threshold to trigger segment roll & buffer flush
        let many_items: Vec<Row> = (0..50)
            .map(|i| Row {
                a: i,
                b: "some relatively long text value to fill buffer",
            })
            .collect();
        let emit_opts = EmitOptions::new().buffer_capacity(32).has_headers(true);
        let seg_paths = encode_to_segments(
            many_items,
            128,
            |i| directory.join(format!("seg-{i}.csv")),
            FormatOptions::CSV.write_bom(crate::config::WriteBom::Emit),
            emit_opts,
        )
        .unwrap();
        assert!(seg_paths.len() > 1);
        std::fs::remove_dir_all(directory).expect("remove local test directory");
    }
}

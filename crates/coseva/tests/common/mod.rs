#![allow(
    dead_code,
    reason = "each integration test binary uses a subset of helpers"
)]

use std::io::{self, Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions};

static NEXT_PATH_ID: AtomicU64 = AtomicU64::new(0);

/// Parses every record as data.
pub(crate) fn unheaded(input: &(impl AsRef<[u8]> + ?Sized)) -> SliceParser<'_> {
    SliceParser::with_options(
        input,
        FormatOptions::CSV,
        ParseOptions::new().headers(Headers::None),
    )
    .expect("valid options")
}

/// Returns a unique test file path and removes it when the guard drops.
pub(crate) fn temp_file(tag: &str) -> TempFile {
    TempFile {
        path: unique_path(tag, "csv"),
    }
}

/// Creates a unique test directory and removes it when the guard drops.
pub(crate) fn temp_dir(tag: &str) -> io::Result<TempDir> {
    let path = unique_path(tag, "dir");
    std::fs::create_dir_all(&path)?;
    Ok(TempDir { path })
}

fn unique_path(tag: &str, extension: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/test-artifacts");
    std::fs::create_dir_all(&dir).expect("test artifact directory can be created");
    let id = NEXT_PATH_ID.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        "{tag}_{}_{:?}_{id}.{extension}",
        std::process::id(),
        std::thread::current().id()
    ))
}

pub(crate) struct TempFile {
    path: PathBuf,
}

impl TempFile {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn into_path(self) -> PathBuf {
        self.path.clone()
    }
}

impl AsRef<Path> for TempFile {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl std::ops::Deref for TempFile {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Parameterized read/seek failure injection for streaming parser and index tests.
#[derive(Debug)]
pub(crate) struct FailingReader {
    inner: Cursor<Vec<u8>>,
    read_calls: usize,
    seek_calls: usize,
    fail_after_bytes: Option<(usize, io::ErrorKind)>,
    fail_on_read: Option<(usize, io::ErrorKind)>,
    interrupt_on_read: Option<usize>,
    early_eof_on_read: Option<usize>,
    max_chunk: Option<usize>,
    overrun: bool,
    fail_all_reads: Option<io::ErrorKind>,
    fail_all_seeks: Option<io::ErrorKind>,
    fail_on_seek: Option<(usize, io::ErrorKind)>,
    seek_lie: Option<u64>,
}

impl FailingReader {
    pub(crate) fn new(data: impl Into<Vec<u8>>) -> Self {
        Self {
            inner: Cursor::new(data.into()),
            read_calls: 0,
            seek_calls: 0,
            fail_after_bytes: None,
            fail_on_read: None,
            interrupt_on_read: None,
            early_eof_on_read: None,
            max_chunk: None,
            overrun: false,
            fail_all_reads: None,
            fail_all_seeks: None,
            fail_on_seek: None,
            seek_lie: None,
        }
    }

    pub(crate) fn fail_after_bytes(mut self, bytes: usize, kind: io::ErrorKind) -> Self {
        self.fail_after_bytes = Some((bytes, kind));
        self
    }

    pub(crate) fn fail_on_read(mut self, call: usize, kind: io::ErrorKind) -> Self {
        self.fail_on_read = Some((call, kind));
        self
    }

    pub(crate) fn interrupt_on_read(mut self, call: usize) -> Self {
        self.interrupt_on_read = Some(call);
        self
    }

    pub(crate) fn early_eof_on_read(mut self, call: usize) -> Self {
        self.early_eof_on_read = Some(call);
        self
    }

    pub(crate) fn max_chunk(mut self, bytes: usize) -> Self {
        self.max_chunk = Some(bytes);
        self
    }

    pub(crate) fn overrun(mut self) -> Self {
        self.overrun = true;
        self
    }

    pub(crate) fn fail_all_reads(mut self, kind: io::ErrorKind) -> Self {
        self.fail_all_reads = Some(kind);
        self
    }

    pub(crate) fn fail_all_seeks(mut self, kind: io::ErrorKind) -> Self {
        self.fail_all_seeks = Some(kind);
        self
    }

    pub(crate) fn fail_on_seek(mut self, call: usize, kind: io::ErrorKind) -> Self {
        self.fail_on_seek = Some((call, kind));
        self
    }

    pub(crate) fn lie_on_seek(mut self, position: u64) -> Self {
        self.seek_lie = Some(position);
        self
    }
}

impl Read for FailingReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.overrun {
            return Ok(buf.len() + 1);
        }
        self.read_calls += 1;
        if let Some(kind) = self.fail_all_reads {
            return Err(io::Error::new(kind, "injected read failure"));
        }
        if self
            .interrupt_on_read
            .is_some_and(|call| call == self.read_calls)
        {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "interrupted"));
        }
        if let Some((call, kind)) = self.fail_on_read
            && call == self.read_calls
        {
            return Err(io::Error::new(kind, "injected read failure"));
        }
        if self
            .early_eof_on_read
            .is_some_and(|call| call == self.read_calls)
        {
            return Ok(0);
        }
        let mut limit = buf.len();
        if let Some(max_chunk) = self.max_chunk {
            limit = limit.min(max_chunk);
        }
        if let Some((budget, kind)) = &mut self.fail_after_bytes {
            if *budget == 0 {
                return Err(io::Error::new(*kind, "injected read failure"));
            }
            limit = limit.min(*budget);
            let read = self.inner.read(&mut buf[..limit])?;
            *budget -= read;
            return Ok(read);
        }
        self.inner.read(&mut buf[..limit])
    }
}

impl Seek for FailingReader {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.seek_calls += 1;
        if let Some(kind) = self.fail_all_seeks {
            return Err(io::Error::new(kind, "injected seek failure"));
        }
        if let Some((call, kind)) = self.fail_on_seek
            && call == self.seek_calls
        {
            return Err(io::Error::new(kind, "injected seek failure"));
        }
        let actual = self.inner.seek(pos)?;
        Ok(self.seek_lie.unwrap_or(actual))
    }
}

/// Parameterized write/flush failure injection with committed-byte and peak-size recording.
#[derive(Debug)]
pub(crate) struct FailingSink {
    inner: Cursor<Vec<u8>>,
    budget: Option<usize>,
    fail_kind: io::ErrorKind,
    flush_error: Option<io::ErrorKind>,
    seek_calls: usize,
    fail_all_seeks: Option<io::ErrorKind>,
    fail_on_seek: Option<(usize, io::ErrorKind)>,
    peak: usize,
    total: usize,
}

impl Default for FailingSink {
    fn default() -> Self {
        Self {
            inner: Cursor::new(Vec::new()),
            budget: None,
            fail_kind: io::ErrorKind::Other,
            flush_error: None,
            seek_calls: 0,
            fail_all_seeks: None,
            fail_on_seek: None,
            peak: 0,
            total: 0,
        }
    }
}

impl FailingSink {
    pub(crate) fn new() -> Self {
        Self {
            fail_kind: io::ErrorKind::Other,
            ..Self::default()
        }
    }

    pub(crate) fn with_bytes(bytes: Vec<u8>) -> Self {
        Self {
            inner: Cursor::new(bytes),
            fail_kind: io::ErrorKind::Other,
            ..Self::default()
        }
    }

    pub(crate) fn fail_after_bytes(mut self, bytes: usize, kind: io::ErrorKind) -> Self {
        self.budget = Some(bytes);
        self.fail_kind = kind;
        self
    }

    pub(crate) fn fail_flush(mut self, kind: io::ErrorKind) -> Self {
        self.flush_error = Some(kind);
        self
    }

    pub(crate) fn fail_all_seeks(mut self, kind: io::ErrorKind) -> Self {
        self.fail_all_seeks = Some(kind);
        self
    }

    pub(crate) fn fail_on_seek(mut self, call: usize, kind: io::ErrorKind) -> Self {
        self.fail_on_seek = Some((call, kind));
        self
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.inner.get_ref()
    }

    pub(crate) fn peak(&self) -> usize {
        self.peak
    }

    pub(crate) fn total(&self) -> usize {
        self.total
    }
}

impl Write for FailingSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.peak = self.peak.max(buf.len());
        self.total += buf.len();
        let allowed = self
            .budget
            .map_or(buf.len(), |budget| budget.min(buf.len()));
        if allowed == 0 && !buf.is_empty() {
            return Err(io::Error::new(self.fail_kind, "injected write failure"));
        }
        self.inner.write_all(&buf[..allowed])?;
        if let Some(budget) = &mut self.budget {
            *budget -= allowed;
        }
        Ok(allowed)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(kind) = self.flush_error {
            Err(io::Error::new(kind, "injected flush failure"))
        } else {
            Ok(())
        }
    }
}

impl Read for FailingSink {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for FailingSink {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.seek_calls += 1;
        if let Some(kind) = self.fail_all_seeks {
            return Err(io::Error::new(kind, "injected seek failure"));
        }
        if let Some((call, kind)) = self.fail_on_seek
            && call == self.seek_calls
        {
            return Err(io::Error::new(kind, "injected seek failure"));
        }
        self.inner.seek(pos)
    }
}

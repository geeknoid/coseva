//! Parse a whole in-memory document across several threads.
//!
//! This is for one case and no other: the document is already in memory, the
//! caller wants all of it, and a single thread is the bottleneck. Measured
//! single-threaded throughput is high enough that a network or gzipped source
//! is bounded elsewhere and gains nothing here. Threads pay when storage is
//! already faster than the parser: a memory-mapped file on `NVMe`, an object in
//! page cache, bulk reprocessing. For many files at once the answer is a parser
//! per file, which needs nothing from this module -- [`SliceParser`],
//! [`ByteRecord`] and [`Error`] are already [`Send`].
//!
//! # Two paths, and which to reach for
//!
//! There are two ways through this module, and they cost very differently.
//!
//! The **borrowed** path hands each worker its records *borrowed* and runs the
//! caller's code on the worker that parsed them. Nothing is copied into an
//! owned record and nothing crosses a channel, so it keeps the whole saving of
//! a borrowed parse -- about twice the throughput of an owned one on narrow
//! rows, because owning a record copies every field -- while spreading the
//! parse across threads. The price is that the caller's code runs on several
//! threads at once, so it must be [`Sync`], and records arrive in no guaranteed
//! order. Reach for [`ParallelParser::fold`] to reduce a document -- a sum, a
//! count, a histogram -- because it gives each worker its own accumulator and
//! so touches no shared memory in the hot loop; reach for
//! [`ParallelParser::for_each_record`] for a side effect per record, such as a
//! filter that writes its matches somewhere. A reduction written on
//! `for_each_record` with one shared counter is the one way to make this path
//! *slower* than serial, which is exactly what `fold` exists to avoid.
//!
//! [`ParallelParser::for_each_batch`] hands the calling thread *owned* records
//! in document order. It is the path for building an ordered `Vec`, or for any
//! consumer that cannot be made [`Sync`], and [`ParallelParser::byte_records`]
//! is the collect-everything convenience over it. Ownership and ordering are
//! what it is for and also what they cost: every field is copied, and the
//! records travel a bounded queue per worker to be drained in order.
//!
//! # What it is worth, measured
//!
//! Threads are not free, and the [threshold](ParallelParser::parallel_threshold)
//! exists so a document only pays for them once they pay off. The default is
//! **thirty-two mebibytes**, the benchmarked crossover on the reference host:
//! at and above it, on the default one-worker-per-core thread count, the
//! borrowed path finished a same-workload parse more than twice as fast as
//! serial -- about 2.2 times at the threshold and up to 2.6 on larger
//! documents -- and the owned path about a quarter faster, both past the
//! twenty percent the backlog asks for. Below it the margin thins, turns
//! noisy, and then inverts as thread start-up outweighs the parse, so smaller
//! documents stay serial. The measured crossover, not a guess, is what sets the
//! line.
//!
//! Two things matter for reading those numbers. The comparison is against the
//! *same-workload* serial parse -- a borrowed parallel parse against a serial
//! borrowed one, an owned against an owned -- because that is the choice a
//! caller actually has. And the reference host is a shared, oversubscribed
//! sixteen-core machine, so its cores are contended and its `auto` row is
//! noisy; thirty-two mebibytes is the size whose win survived that noise across
//! repeated runs, which is why the threshold sits there rather than at the
//! sixteen where an idle host already crosses. The crossover moves with the
//! core count, which is why it is a benchmark rather than a constant:
//! `benches/parallel.rs` re-derives it, and it should be re-run on an idle
//! machine wherever the answer matters.
//!
//! ```
//! # #[cfg(feature = "parallel")] {
//! use coseva::config::ParseOptions;
//! use coseva::format::Csv;
//! use coseva::parallel::ParallelParser;
//!
//! let document = "city,pop\nBoston,650706\nDenver,715522\n";
//! // Thread unconditionally, ignoring the size threshold, for the example.
//! let parser = ParallelParser::<Csv>::new(ParseOptions::new()).parallel_threshold(0);
//!
//! // Each worker sums into its own u64; combine the per-worker sums at the end.
//! let subtotals = parser.fold::<u64, _, _, coseva::Error>(
//!     document.as_bytes(),
//!     || 0,
//!     |sum, record| {
//!         *sum += record.parse::<u64>(1)?.unwrap_or(0);
//!         Ok(())
//!     },
//! )?;
//!
//! assert_eq!(subtotals.into_iter().sum::<u64>(), 1_366_228);
//! # }
//! # Ok::<(), coseva::Error>(())
//! ```
//!
//! # How it works
//!
//! A worker is a plain [`SliceParser`] over the whole input, seeked to a record
//! boundary and stopped at the next one, so nothing in the scanning kernels
//! knows this module exists and records carry absolute byte offsets, physical
//! lines and record indices without fixing up.
//!
//! The one new algorithm is the boundary pass: a serial scan that tracks
//! whether it is inside a quoted field and emits split points at true record
//! starts. It is exact rather than heuristic, so a value containing a record
//! ending needs no opt-out. Because it is serial it bounds the speedup, and it
//! is therefore kept to a SIMD scan over three bytes with a flag and two
//! counters per hit. On the borrowed path workers claim those chunks from a
//! shared cursor, so an uneven document -- a run of long quoted fields on one
//! thread -- balances itself rather than stranding a chunk. The owned path
//! deals them round-robin instead, because its consumer reads the workers'
//! queues in a fixed rotation to recover document order, so there a run of
//! expensive chunks does stall the worker holding them.
//!
//! The borrowed path stops there: a worker parses its chunk and runs the
//! closure in place, so its only shared state is the closure, the chunk cursor,
//! and one atomic naming the earliest failure. The owned path adds a bounded
//! queue per worker, because an owned record is the only sound shape to carry
//! across threads -- an escaped field lives in its worker's scratch -- and the
//! consumer drains those queues in order. Either way peak memory is bounded by
//! the threads and their work unit, never by the document's size.
//!
//! # Ordering and errors
//!
//! [`ParallelParser::for_each_batch`] delivers batches in document order:
//! chunks are drained in the rotation they were dealt, so the consumer sees
//! exactly the records a [`SliceParser`] would, in the same order, and needs no
//! reorder buffer. [`ParallelParser::for_each_record`] makes no such promise --
//! that freedom is what lets a worker run the closure the instant it has a
//! record.
//!
//! Both report a deterministic failure regardless. Chunks beginning before the
//! earliest failure remain eligible so an earlier error cannot be hidden, while
//! a chunk at or after it is abandoned. The unordered path also checks the
//! failure fence before each callback, preventing already-running later chunks
//! from adding side effects beyond the authoritative document prefix.
//!
//! # Formats this can split
//!
//! Splitting rests on quote counting, so it is exact only where a quote byte
//! always means a quote. The parser rejects a format where it would not: one
//! with comments, with an escape style other than [`Escape::DoubleQuote`], with
//! quoting disabled or bare quotes permitted in unquoted fields, with blank
//! records skipped, or with a multi-byte separator. Such a format is a
//! configuration error rather than a silent fallback, so a caller who asked for
//! threads is told they cannot have them.
//!
//! [`Escape::DoubleQuote`]: crate::config::Escape::DoubleQuote

use core::marker::PhantomData;
use core::num::NonZeroUsize;
#[cfg(test)]
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;

use crate::byte_record::ByteRecord;
use crate::config::{BlankRecords, Escape, FieldCount, FormatOptions, Headers, ParseOptions};
use crate::error::{Error, ErrorKind};
use crate::format::{CsvFormat, Dynamic, StaticFormat};
use crate::record::Record;
use crate::slice_parser::SliceParser;

pub(crate) mod split;
pub(crate) mod unordered;

use split::Boundary;

/// Chunks to cut per thread.
///
/// More chunks than threads is what absorbs an uneven document: a chunk that
/// happens to hold long records does not hold up a whole thread's share. The
/// cost of a chunk is one seek and one batch flush, so this can be generous.
const CHUNKS_PER_THREAD: usize = 16;

/// Batches a worker may run ahead of the consumer.
const QUEUE_DEPTH: usize = 4;

/// Records in one batch, unless the caller says otherwise.
const DEFAULT_BATCH_RECORDS: usize = 4096;

/// Documents smaller than this are parsed on the calling thread.
///
/// It is thirty-two mebibytes, the benchmarked crossover on the reference
/// host: at and above it, on the default one-worker-per-core thread count, the
/// borrowed path ([`ParallelParser::for_each_record`], [`ParallelParser::fold`])
/// finished a same-workload parse more than twice as fast as serial — about
/// 2.2 times at the threshold and up to 2.6 on larger documents — while the
/// owned path ([`ParallelParser::for_each_batch`]), which copies and orders,
/// won more modestly, roughly a quarter faster. Both clear the 20% margin the
/// backlog asks for at this size. Below it the gain thins, turns noisy, and
/// then inverts as thread start-up outweighs the parse, so smaller documents
/// stay serial rather than pay for threads that lose.
///
/// Sixteen mebibytes still crosses on an idle host, but its margin is thin
/// enough that a loaded machine can wash it out; thirty-two is where the win
/// held up across repeated runs on a shared, oversubscribed reference host, so
/// it is the deliberately conservative line. `benches/parallel.rs` re-derives
/// it on any host — the crossover moves with the core count — and a caller who
/// knows its workload can move the line with
/// [`ParallelParser::parallel_threshold`].
const DEFAULT_PARALLEL_THRESHOLD_BYTES: usize = 32 << 20;

// Test-only injection of a worker parser-build failure, so a unit test can
// reach `run_worker`'s error path without a format that passes validation in
// `prepare` and then fails on a worker thread. Workers run on other threads,
// so this cannot be a thread-local; the test module serializes access.
#[cfg(test)]
static INJECT_WORKER_BUILD_FAILURE: AtomicBool = AtomicBool::new(false);

#[cfg(test)]
fn injected_build_failure() -> Option<Error> {
    INJECT_WORKER_BUILD_FAILURE
        .load(Ordering::Relaxed)
        .then(|| Error::detailed(ErrorKind::Configuration, "injected worker build failure"))
}

/// What a worker sends its consumer.
enum Message {
    /// A batch of records, in document order within its chunk.
    Batch(Batch),
    /// The chunk parsed cleanly and is finished.
    ChunkDone,
    /// The chunk failed; no further message for it follows.
    Failed(Error),
}

enum BatchDisposition {
    Reuse(Batch),
    Stop,
}

struct Batch(Vec<ByteRecord>);

impl Batch {
    #[cfg(test)]
    fn new() -> Self {
        Self(Vec::new())
    }

    fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn push(&mut self, record: ByteRecord) {
        self.0.push(record);
    }
}

#[derive(Clone, Copy)]
enum ChunkEnd<'input> {
    Boundary(usize),
    Document(&'input [u8]),
}

impl ChunkEnd<'_> {
    fn reached(self, record_end: usize) -> bool {
        let end = match self {
            Self::Boundary(end) => end,
            Self::Document(input) => input.len(),
        };
        record_end >= end
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WorkerCount(NonZeroUsize);

impl WorkerCount {
    fn requested(threads: NonZeroUsize) -> Self {
        Self(threads)
    }

    pub(super) fn get(self) -> usize {
        self.0.get()
    }

    fn is_single(self) -> bool {
        self.0 == NonZeroUsize::MIN
    }
}

struct WorkerChannels {
    senders: Vec<SyncSender<Message>>,
    receivers: Vec<Receiver<Message>>,
    returns: Vec<SyncSender<Batch>>,
    recycled: Vec<Receiver<Batch>>,
}

fn requested_chunks(threads: WorkerCount) -> usize {
    threads.get().saturating_mul(CHUNKS_PER_THREAD)
}

fn active_workers(requested: WorkerCount, chunks: &[Boundary]) -> WorkerCount {
    let active = chunks.iter().take(requested.get()).count();
    WorkerCount(NonZeroUsize::new(active).expect("the splitter always returns one active chunk"))
}

fn worker_channels(threads: WorkerCount) -> WorkerChannels {
    let threads = threads.get();
    let mut senders = Vec::with_capacity(threads);
    let mut receivers = Vec::with_capacity(threads);
    let mut returns = Vec::with_capacity(threads);
    let mut recycled = Vec::with_capacity(threads);
    for _ in 0..threads {
        let (sender, receiver) = sync_channel(QUEUE_DEPTH);
        senders.push(sender);
        receivers.push(receiver);
        // One deeper than the forward queue, so returning a batch never
        // blocks the consumer even with every forward slot in flight.
        let (giving_back, taking_back) = sync_channel(QUEUE_DEPTH.saturating_add(1));
        returns.push(giving_back);
        recycled.push(taking_back);
    }
    WorkerChannels {
        senders,
        receivers,
        returns,
        recycled,
    }
}

/// Multi-threaded parser for an input already held in memory.
///
/// See the [module documentation](self) for when threads pay, how splitting
/// works, which formats can be split, and a worked example.
#[derive(Clone, Debug)]
pub struct ParallelParser<F: CsvFormat = Dynamic> {
    format: FormatOptions,
    options: ParseOptions,
    threads: NonZeroUsize,
    batch_records: NonZeroUsize,
    threshold: usize,
    marker: PhantomData<fn() -> F>,
}

impl ParallelParser<Dynamic> {
    /// Create a parser for an explicit format and parse options.
    ///
    /// ```
    /// # #[cfg(feature = "parallel")] {
    /// use coseva::config::{FormatOptions, ParseOptions};
    /// use coseva::parallel::ParallelParser;
    ///
    /// let parser = ParallelParser::with_options(FormatOptions::TSV, ParseOptions::new());
    /// let records = parser.byte_records(b"city\tpop\nBoston\t650706\n")?;
    /// assert_eq!(records.len(), 1);
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    #[must_use]
    pub fn with_options(format: FormatOptions, options: ParseOptions) -> Self {
        Self::build(format, options)
    }
}

impl<F: StaticFormat> ParallelParser<F> {
    /// Parse with the format named by `F`.
    ///
    /// Each worker's kernel is specialized for `F` exactly as a
    /// [`SliceParser<F>`] is, so threads and static formats compose.
    #[must_use]
    pub fn new(options: ParseOptions) -> Self {
        Self::build(F::FORMAT, options)
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
impl<F: CsvFormat> ParallelParser<F> {
    fn build(format: FormatOptions, options: ParseOptions) -> Self {
        Self {
            format,
            options,
            threads: thread::available_parallelism().unwrap_or(NonZeroUsize::MIN),
            batch_records: NonZeroUsize::new(DEFAULT_BATCH_RECORDS).unwrap_or(NonZeroUsize::MIN),
            threshold: DEFAULT_PARALLEL_THRESHOLD_BYTES,
            marker: PhantomData,
        }
    }

    fn uses_serial_fallback(&self, input: &[u8], start: Boundary, threads: WorkerCount) -> bool {
        threads.is_single() || input.len().saturating_sub(start.byte).lt(&self.threshold)
    }

    /// Set how many threads to parse with.
    ///
    /// Defaults to [`thread::available_parallelism`]. More threads than there
    /// are chunks is harmless; the surplus are never spawned.
    #[must_use]
    pub const fn threads(mut self, threads: NonZeroUsize) -> Self {
        self.threads = threads;
        self
    }

    /// Set how many records a worker gathers before handing a batch over.
    ///
    /// This is the work unit: peak memory is bounded by threads times this
    /// times a small queue depth, so a large batch trades memory for fewer
    /// handoffs. Defaults to 4096, and the trade is steep in one direction --
    /// a 64 MiB document that takes 0.29s in batches of 4096 takes 1.16s in
    /// batches of 32, because at that size the handoffs cost more than the
    /// parsing. Going the other way buys much less: 16384 was within noise of
    /// 4096 on the same document.
    #[must_use]
    pub const fn batch_records(mut self, records: NonZeroUsize) -> Self {
        self.batch_records = records;
        self
    }

    /// Set the document size below which parsing stays on the calling thread.
    ///
    /// Defaults to thirty-two mebibytes, the benchmarked crossover on the
    /// reference host, above which the parallel paths beat a same-workload
    /// serial parse by a wide margin and below which thread start-up costs more
    /// than it saves. Set it to a crossover `benches/parallel.rs` finds on the
    /// machine that matters — it moves with the core count — or to zero to
    /// thread unconditionally. The result is identical either way, so the
    /// fallback is silent.
    #[must_use]
    pub const fn parallel_threshold(mut self, bytes: usize) -> Self {
        self.threshold = bytes;
        self
    }

    /// Return the headers this parser would use for `input`.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid format or a parse error in the header
    /// record.
    pub fn headers(&self, input: &[u8]) -> Result<Option<ByteRecord>, Error> {
        let mut probe = SliceParser::<F>::build(input, self.format, self.options.clone())?;
        Ok(probe.headers()?.cloned())
    }

    /// Parse `input`, handing each batch of records to `consume`.
    ///
    /// `consume` runs on the calling thread and sees batches in document
    /// order. It is given the batch by mutable reference so it can drain or
    /// take the records without copying them; whatever it leaves behind is
    /// dropped.
    ///
    /// It reports failure in its own error type, which stops parsing and is
    /// returned unchanged, so it is also the way to stop early. That type has
    /// to be able to carry a parse failure too, which is what the [`From`]
    /// bound asks for; use [`Error`] itself when the consumer cannot fail.
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a format that cannot be split
    /// exactly, the parse error at the lowest byte offset in the document, or
    /// whatever `consume` returned.
    pub fn for_each_batch<C, E>(&self, input: &[u8], mut consume: C) -> Result<(), E>
    where
        C: FnMut(&mut Vec<ByteRecord>) -> Result<(), E>,
        E: From<Error>,
    {
        self.check_splittable()?;
        let (options, start) = self.prepare(input)?;

        let threads = WorkerCount::requested(self.threads);
        if self.uses_serial_fallback(input, start, threads) {
            let mut parser = SliceParser::<F>::build(input, self.format, options)?;
            let mut pool = Vec::new();
            // The consumer's error cannot travel through a chunk's own result
            // type, so it is set aside here and raised once the chunk stops.
            let mut refused = None;
            self.parse_chunk(
                &mut parser,
                start,
                ChunkEnd::Document(input),
                &mut pool,
                |mut batch| match consume(&mut batch.0) {
                    Ok(()) => Ok(BatchDisposition::Reuse(batch)),
                    Err(error) => {
                        refused = Some(error);
                        Ok(BatchDisposition::Stop)
                    }
                },
            )?;
            return refused.map_or(Ok(()), Err);
        }

        let chunks =
            split::boundaries(input, self.format.dialect, start, requested_chunks(threads));
        let threads = active_workers(threads, &chunks);
        let WorkerChannels {
            senders,
            receivers,
            returns,
            recycled,
        } = worker_channels(threads);

        thread::scope(|scope| {
            for ((worker, sender), recycled) in senders.into_iter().enumerate().zip(recycled) {
                let chunks = &chunks;
                let options = options.clone();
                let this = &self;
                drop(scope.spawn(move || {
                    this.run_worker(input, options, chunks, threads, worker, &sender, &recycled);
                }));
            }

            drain(receivers, &returns, threads, &chunks, &mut consume)
        })
    }

    /// Parse `input`, handing each borrowed record to `consume` on a worker.
    ///
    /// This is the fast path for a caller that neither needs records in
    /// document order nor needs to keep them. Unlike [`Self::for_each_batch`],
    /// `consume` runs on the worker threads and is handed a borrowed
    /// [`Record`], so nothing is copied into an owned record and nothing is
    /// carried across a channel — a serial borrowed parse is about twice as
    /// fast as an owned one on narrow rows, and this keeps that saving while
    /// spreading the work across threads.
    ///
    /// Because `consume` runs on several threads at once it must be [`Sync`],
    /// and any state it touches is its own to synchronize. It sees records in
    /// no guaranteed order. It reports failure in its own error type, which
    /// stops parsing and is returned unchanged, so it is also the way to stop
    /// early; that type has to be able to carry a parse failure too, which is
    /// what the [`From`] bound asks for, and it has to be [`Send`] so a
    /// worker's failure can be returned. Use [`Error`] itself when the consumer
    /// cannot fail.
    ///
    /// Below the [threshold](Self::parallel_threshold), or with one thread, the
    /// records are parsed on the calling thread instead, which needs no `Sync`
    /// and delivers them in document order; the result is otherwise identical.
    ///
    /// # Ordering and errors
    ///
    /// The reported failure is deterministic despite the disorder: chunks
    /// beginning before the earliest failure remain eligible so an earlier
    /// error cannot be hidden. A chunk at or after that offset is abandoned,
    /// and an already-running later chunk checks the fence before each
    /// callback, preventing side effects beyond the authoritative prefix.
    ///
    /// ```
    /// # #[cfg(feature = "parallel")] {
    /// use std::sync::atomic::{AtomicU64, Ordering};
    /// use coseva::config::{Headers, ParseOptions};
    /// use coseva::format::Csv;
    /// use coseva::parallel::ParallelParser;
    ///
    /// let document = "1,10\n2,20\n3,30\n";
    /// let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None));
    ///
    /// let total = AtomicU64::new(0);
    /// parser.for_each_record::<_, coseva::Error>(document.as_bytes(), |record| {
    ///     total.fetch_add(record.parse::<u64>(1)?.unwrap_or(0), Ordering::Relaxed);
    ///     Ok(())
    /// })?;
    /// assert_eq!(total.into_inner(), 60);
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a format that cannot be split
    /// exactly, the parse error at the lowest byte offset in the document, or
    /// whatever `consume` returned.
    pub fn for_each_record<C, E>(&self, input: &[u8], consume: C) -> Result<(), E>
    where
        C: Fn(&Record<'_>) -> Result<(), E> + Sync,
        E: From<Error> + Send,
    {
        self.check_splittable()?;
        let (options, start) = self.prepare(input)?;

        let threads = WorkerCount::requested(self.threads);
        if self.uses_serial_fallback(input, start, threads) {
            let mut parser = SliceParser::<F>::build(input, self.format, options)?;
            parser.seek(start.into())?;
            while let Some(mut line) = parser.next_line()? {
                consume(&line.record()?)?;
            }
            return Ok(());
        }

        let chunks =
            split::boundaries(input, self.format.dialect, start, requested_chunks(threads));
        let threads = active_workers(threads, &chunks);
        unordered::for_each::<F, _, _>(input, self.format, &options, &chunks, threads, &consume)
    }

    /// Fold every record into a per-worker accumulator, returning one per
    /// worker for the caller to combine.
    ///
    /// This is the way to reduce a document in parallel — a sum, a count, a
    /// histogram — without the trap [`Self::for_each_record`] leaves open: a
    /// closure that reduces into shared state has every worker contend on one
    /// cache line, and a parallel sum written that way is slower than a serial
    /// one. Here each worker seeds its own accumulator with `init`, folds every
    /// [`Record`] it parses into it with `fold`, and hands it back, so the hot
    /// loop touches no shared memory and the reduction scales with the cores.
    ///
    /// The returned accumulators are in no particular order, one per worker,
    /// and a worker that happened to claim no chunk still returns its seed;
    /// combine them however the reduction demands — sum the sums, merge the
    /// maps. Both `init` and `fold` run on the worker threads, so both must be
    /// [`Sync`], and the accumulator must be [`Send`] to come back.
    ///
    /// Below the [threshold](Self::parallel_threshold), or with one thread, the
    /// records are folded on the calling thread into a single accumulator, so
    /// the result is a one-element vector; the reduction is otherwise
    /// identical.
    ///
    /// # Ordering and errors
    ///
    /// As [`Self::for_each_record`]: records reach `fold` in no guaranteed
    /// order, but a failure is still reported at the lowest byte offset in the
    /// document, whichever worker reached it. Once that failure is fenced, no
    /// later-record fold begins. `fold`'s own error is returned unchanged, so
    /// it doubles as an early stop.
    ///
    /// ```
    /// # #[cfg(feature = "parallel")] {
    /// use coseva::config::{Headers, ParseOptions};
    /// use coseva::format::Csv;
    /// use coseva::parallel::ParallelParser;
    ///
    /// let document = "1,10\n2,20\n3,30\n";
    /// let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None));
    ///
    /// let subtotals = parser.fold::<u64, _, _, coseva::Error>(
    ///     document.as_bytes(),
    ///     || 0,
    ///     |sum, record| {
    ///         *sum += record.parse::<u64>(1)?.unwrap_or(0);
    ///         Ok(())
    ///     },
    /// )?;
    /// assert_eq!(subtotals.into_iter().sum::<u64>(), 60);
    /// # }
    /// # Ok::<(), coseva::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns a configuration error for a format that cannot be split
    /// exactly, the parse error at the lowest byte offset in the document, or
    /// whatever `fold` returned.
    pub fn fold<T, Init, Fold, E>(&self, input: &[u8], init: Init, fold: Fold) -> Result<Vec<T>, E>
    where
        Init: Fn() -> T + Sync,
        Fold: Fn(&mut T, &Record<'_>) -> Result<(), E> + Sync,
        T: Send,
        E: From<Error> + Send,
    {
        self.check_splittable()?;
        let (options, start) = self.prepare(input)?;

        let threads = WorkerCount::requested(self.threads);
        if self.uses_serial_fallback(input, start, threads) {
            let mut parser = SliceParser::<F>::build(input, self.format, options)?;
            parser.seek(start.into())?;
            let mut accumulator = init();
            while let Some(mut line) = parser.next_line()? {
                fold(&mut accumulator, &line.record()?)?;
            }
            return Ok(vec![accumulator]);
        }

        let chunks =
            split::boundaries(input, self.format.dialect, start, requested_chunks(threads));
        let threads = active_workers(threads, &chunks);
        unordered::drive::<F, T, _, _, _>(
            input,
            self.format,
            &options,
            &chunks,
            threads,
            &init,
            &fold,
        )
    }

    /// Parse `input` and collect every record.
    ///
    /// This holds the whole document's records in memory at once, which is the
    /// one thing [`Self::for_each_batch`] exists to avoid, so reach for it only
    /// when that is what you wanted anyway.
    ///
    /// # Errors
    ///
    /// As [`Self::for_each_batch`].
    pub fn byte_records(&self, input: &[u8]) -> Result<Vec<ByteRecord>, Error> {
        let mut records = Vec::new();
        self.for_each_batch(input, |batch| {
            records.append(batch);
            Ok(())
        })?;
        Ok(records)
    }

    /// Reject a format whose records cannot be located without parsing it.
    fn check_splittable(&self) -> Result<(), Error> {
        let format = &self.format;
        let unsupported = if format.dialect.comment.is_some() {
            "parallel parsing cannot split a format with comments"
        } else if format.dialect.escape != Escape::DoubleQuote {
            "parallel parsing cannot split a format escaping with anything but doubled quotes"
        } else if format.dialect.multibyte() {
            "parallel parsing cannot split a format with multi-byte separators"
        } else if !format.syntax.quoting_enabled() {
            "parallel parsing cannot split a format with quoting disabled"
        } else if format.syntax.permits_unquoted_quotes() {
            "parallel parsing cannot split a format permitting bare quotes in unquoted fields"
        } else if format.blank_records == BlankRecords::Skip {
            "parallel parsing cannot split a format that skips blank records"
        } else {
            return Ok(());
        };

        Err(Error::detailed(ErrorKind::Configuration, unsupported))
    }

    /// Settle headers and field width serially, and locate the first record.
    ///
    /// Both must happen once for the whole document rather than once per
    /// chunk: a chunk after the first would otherwise eat a data record as its
    /// headers, and [`FieldCount::MatchFirst`] would match each chunk against
    /// its own first record instead of the document's.
    fn prepare(&self, input: &[u8]) -> Result<(ParseOptions, Boundary), Error> {
        let mut probe = SliceParser::<F>::build(input, self.format, self.options.clone())?;
        let headers = probe.headers()?.cloned();
        let start = Boundary::from(probe.location());

        let field_count = if self.options.requested_field_count() == FieldCount::MatchFirst {
            match probe.next_line()? {
                Some(mut line) => FieldCount::Exact(line.record()?.len()),
                None => FieldCount::Flexible,
            }
        } else {
            self.options.requested_field_count()
        };

        let options = self
            .options
            .clone()
            .headers(headers.map_or(Headers::None, Headers::Provided))
            .field_count(field_count);
        Ok((options, start))
    }

    /// Parse every chunk this worker owns, sending batches as they fill.
    #[expect(
        clippy::too_many_arguments,
        reason = "a worker's whole context, and bundling it into a struct would buy nothing but a name"
    )]
    fn run_worker(
        &self,
        input: &[u8],
        options: ParseOptions,
        chunks: &[Boundary],
        threads: WorkerCount,
        worker: usize,
        sender: &SyncSender<Message>,
        recycled: &Receiver<Batch>,
    ) {
        let parser = SliceParser::<F>::build(input, self.format, options);

        #[cfg(test)]
        let parser = injected_build_failure().map_or(parser, Err);

        let mut parser = match parser {
            Ok(parser) => parser,
            // Disconnecting instead would reach the consumer as an ordinary
            // end of stream, so the whole parse would return `Ok(())` having
            // produced neither records nor an error. Reporting it keeps the
            // ordered driver's behaviour identical to the unordered one's and
            // to the single-threaded path's.
            Err(error) => {
                drop(sender.send(Message::Failed(error)));
                return;
            }
        };

        let capacity = self.batch_records.get();
        let mut pool = Vec::new();
        let mut index = worker;
        while index < chunks.len() {
            let end = chunks
                .get(index + 1)
                .map_or(ChunkEnd::Document(input), |boundary| {
                    ChunkEnd::Boundary(boundary.byte)
                });
            let outcome = self.parse_chunk(&mut parser, chunks[index], end, &mut pool, |batch| {
                if sender.send(Message::Batch(batch)).is_err() {
                    return Ok(BatchDisposition::Stop);
                }
                // Whatever the consumer has finished with comes back here, so a
                // steady state allocates neither the batch nor its records.
                Ok(BatchDisposition::Reuse(
                    recycled
                        .try_recv()
                        .unwrap_or_else(|_| Batch::with_capacity(capacity)),
                ))
            });

            match outcome {
                Ok(()) => {
                    if sender.send(Message::ChunkDone).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    drop(sender.send(Message::Failed(error)));
                    return;
                }
            }
            index += threads.get();
        }
    }

    /// Parse the records from `start` up to `end`, in batches.
    ///
    /// `emit` takes each finished batch and hands back the container for the
    /// next one, or `None` to stop, which is how a worker gives up quietly once
    /// its consumer has gone away. Handing the container back rather than
    /// keeping one is what makes recycling possible: a batch the consumer has
    /// finished with returns here with its records intact, and `pool` holds
    /// them so the buffers they have already grown are filled again instead of
    /// being freed and reallocated per record.
    fn parse_chunk<E>(
        &self,
        parser: &mut SliceParser<'_, F>,
        start: Boundary,
        end: ChunkEnd<'_>,
        pool: &mut Vec<ByteRecord>,
        mut emit: E,
    ) -> Result<(), Error>
    where
        E: FnMut(Batch) -> Result<BatchDisposition, Error>,
    {
        parser.seek(start.into())?;
        let capacity = self.batch_records.get();
        let mut batch = Batch::with_capacity(capacity);

        while let Ok(Some(mut line)) = parser.next_line() {
            let mut record = pool.pop().unwrap_or_default();
            if let Err(error) = line.read_byte_record_into(&mut record) {
                return emit_before_error(batch, &mut emit, error);
            }
            // A record's extent includes its terminator, so the record that
            // ends at the next chunk's first byte is this chunk's last.
            let last = end.reached(record.byte_range().end);
            batch.push(record);

            if last || batch.len() >= capacity {
                let mut returned = match emit(batch)? {
                    BatchDisposition::Reuse(returned) => returned,
                    BatchDisposition::Stop => return Ok(()),
                };
                pool.append(&mut returned.0);
                batch = returned;
            }
            if last {
                return Ok(());
            }
        }

        if !batch.is_empty() {
            let _returned = emit(batch)?;
        }
        Ok(())
    }
}

/// Deliver the records parsed before a failure, then report the failure.
///
/// A serial `SliceParser` yields every record it already produced before
/// returning the error at the record that failed. Emitting the partial batch
/// here gives the parallel path the same records-before-error sequence, which
/// is what its serial-equivalence contract promises.
fn emit_before_error<E>(batch: Batch, emit: &mut E, error: Error) -> Result<(), Error>
where
    E: FnMut(Batch) -> Result<BatchDisposition, Error>,
{
    if batch.is_empty() {
        return Err(error);
    }
    match emit(batch)? {
        BatchDisposition::Reuse(_) => Err(error),
        BatchDisposition::Stop => Ok(()),
    }
}

/// Drain the workers in chunk order, handing every batch to `consume`.
///
/// Consuming in order is what makes the reported failure the one at the lowest
/// byte offset: every chunk before it has already been delivered, so the first
/// failure seen is the first in the document.
fn drain<C, E>(
    receivers: Vec<Receiver<Message>>,
    returns: &[SyncSender<Batch>],
    threads: WorkerCount,
    chunks: &[Boundary],
    consume: &mut C,
) -> Result<(), E>
where
    C: FnMut(&mut Vec<ByteRecord>) -> Result<(), E>,
    E: From<Error>,
{
    for (chunk, _) in chunks.iter().enumerate() {
        let worker = chunk % threads.get();
        let receiver = &receivers[worker];
        loop {
            match receiver.recv() {
                Ok(Message::Batch(mut batch)) => {
                    consume(&mut batch.0)?;
                    // A batch the consumer left records in goes back for its
                    // buffers to be refilled; a full return queue just means
                    // the worker is not short of them.
                    drop(returns[worker].try_send(batch));
                }
                Ok(Message::ChunkDone) => break,
                Ok(Message::Failed(error)) => return Err(E::from(error)),
                // A worker's sender is dropped without a `ChunkDone` or a
                // `Failed` only when the worker stopped early, which it does
                // solely in response to cancellation or to this consumer
                // having already gone away. A build or parse failure sends
                // `Failed` rather than disconnecting, so a disconnect here
                // never hides one.
                Err(_) => return Ok(()),
            }
        }
    }
    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use core::sync::atomic::Ordering;
    use std::sync::mpsc::TrySendError;
    use std::sync::{Mutex, MutexGuard, PoisonError};

    use super::{
        Batch, BatchDisposition, ChunkEnd, Error, ErrorKind, INJECT_WORKER_BUILD_FAILURE,
        ParallelParser, SliceParser, WorkerCount, active_workers, requested_chunks,
        worker_channels,
    };
    use crate::ByteRecord;
    use crate::config::{Headers, ParseOptions};
    use crate::format::Csv;

    /// `INJECT_WORKER_BUILD_FAILURE` is process-global because workers run on
    /// their own threads, so tests that touch it must not run concurrently.
    static INJECTION: Mutex<()> = Mutex::new(());

    struct InjectionGuard(
        #[expect(dead_code, reason = "held for its lifetime")] MutexGuard<'static, ()>,
    );

    impl InjectionGuard {
        fn arm() -> Self {
            let lock = INJECTION.lock().unwrap_or_else(PoisonError::into_inner);
            INJECT_WORKER_BUILD_FAILURE.store(true, Ordering::Relaxed);
            Self(lock)
        }
    }

    impl Drop for InjectionGuard {
        fn drop(&mut self) {
            INJECT_WORKER_BUILD_FAILURE.store(false, Ordering::Relaxed);
        }
    }

    /// A worker whose parser cannot be built must report the failure, not
    /// abandon its chunk. Abandoning drops the worker's sender, which the
    /// consumer reads as an ordinary end of stream, so `for_each_batch`
    /// returned `Ok(())` having produced no records and no error — silent
    /// data loss on the one API where the caller has no way to notice. The
    /// unordered driver and the single-threaded path both surface this as an
    /// `Err`, and the ordered one now agrees.
    #[test]
    fn a_worker_that_cannot_build_its_parser_reports_an_error_rather_than_no_records() {
        let _guard = InjectionGuard::arm();

        let input = b"city,pop\nBoston,650706\nAustin,961855\nDenver,715522\n";
        let parser = ParallelParser::<Csv>::new(ParseOptions::new())
            .threads(NonZeroUsize::new(2).expect("2 is non-zero"))
            .parallel_threshold(0);

        let mut seen = 0_usize;
        let error: Error = parser
            .for_each_batch(input, |batch| {
                seen += batch.len();
                Ok::<(), Error>(())
            })
            .expect_err("a worker build failure must not be reported as a clean parse");

        assert_eq!(error.kind(), ErrorKind::Configuration);
        assert_eq!(seen, 0);
    }

    #[test]
    fn scheduling_limits_are_exact() {
        let parser = ParallelParser::<Csv>::new(ParseOptions::new());
        assert_eq!(parser.batch_records.get(), 4_096);
        let one = WorkerCount::requested(NonZeroUsize::MIN);
        let three = WorkerCount::requested(NonZeroUsize::new(3).expect("three is non-zero"));
        let eight = WorkerCount::requested(NonZeroUsize::new(8).expect("eight is non-zero"));
        assert_eq!(requested_chunks(one), 16);
        assert_eq!(requested_chunks(three), 48);
        let boundary = super::split::Boundary {
            byte: 0,
            line: 1,
            record: 0,
        };
        assert_eq!(active_workers(eight, &[boundary; 3]), three);
        assert_eq!(active_workers(three, &[boundary; 8]), three);
        assert_eq!(active_workers(three, &[boundary; 3]), three);
        assert!(!ChunkEnd::Document(b"abc").reached(2));
        assert!(ChunkEnd::Document(b"abc").reached(3));
        assert!(!ChunkEnd::Boundary(4).reached(3));
        assert!(ChunkEnd::Boundary(4).reached(4));

        let two = WorkerCount::requested(NonZeroUsize::new(2).expect("two is non-zero"));
        let channels = worker_channels(two);
        assert_eq!(channels.senders.len(), 2);
        assert_eq!(channels.receivers.len(), 2);
        assert_eq!(channels.returns.len(), 2);
        assert_eq!(channels.recycled.len(), 2);

        for sender in &channels.senders {
            for _ in 0..4 {
                sender
                    .try_send(super::Message::ChunkDone)
                    .expect("the forward queue has four slots");
            }
            assert!(matches!(
                sender.try_send(super::Message::ChunkDone),
                Err(TrySendError::Full(_))
            ));
        }
        for sender in &channels.returns {
            for _ in 0..5 {
                sender
                    .try_send(Batch::new())
                    .expect("the return queue has five slots");
            }
            assert!(matches!(
                sender.try_send(Batch::new()),
                Err(TrySendError::Full(_))
            ));
        }
    }

    #[test]
    fn serial_fallback_uses_one_thread_or_strictly_below_threshold() {
        let parser = ParallelParser::<Csv>::new(ParseOptions::new()).parallel_threshold(10);
        let start = super::split::Boundary {
            byte: 3,
            line: 1,
            record: 0,
        };
        let one = WorkerCount::requested(NonZeroUsize::MIN);
        let two = WorkerCount::requested(NonZeroUsize::new(2).expect("two is non-zero"));
        assert!(parser.uses_serial_fallback(&[0; 13], start, one));
        assert!(parser.uses_serial_fallback(&[0; 12], start, two));
        assert!(!parser.uses_serial_fallback(&[0; 13], start, two));
    }

    #[test]
    fn record_serial_fallback_runs_callbacks_on_the_calling_thread() {
        let caller = std::thread::current().id();
        let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
            .threads(NonZeroUsize::new(2).expect("two is non-zero"))
            .parallel_threshold(usize::MAX);

        parser
            .for_each_record::<_, Error>(b"a\nb\n", |_| {
                assert_eq!(std::thread::current().id(), caller);
                Ok(())
            })
            .expect("the serial fallback parses");
    }

    #[test]
    fn serial_batch_consumers_succeed_or_return_their_error_exactly() {
        let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
            .threads(NonZeroUsize::MIN)
            .parallel_threshold(0);
        let mut batches = 0;
        parser
            .for_each_batch(b"a\nb\n", |_| {
                batches += 1;
                Ok::<(), Error>(())
            })
            .expect("the consumer accepts the records");
        assert_eq!(batches, 1);

        let error = parser
            .for_each_batch(b"a\nb\n", |_| {
                Err::<(), _>(Error::detailed(ErrorKind::Configuration, "refused"))
            })
            .expect_err("the consumer refusal is returned");
        assert_eq!(error.to_string(), "refused");
    }

    #[test]
    fn parse_chunk_obeys_batch_capacity_reuses_returns_and_skips_empty_emits() {
        let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
            .batch_records(NonZeroUsize::new(2).expect("two is non-zero"));
        let mut slice = SliceParser::<Csv>::new(
            b"a\nb\nc\nd\ne\n",
            ParseOptions::new().headers(Headers::None),
        )
        .expect("valid input");
        let mut pool = Vec::new();
        let mut sizes = Vec::new();
        let mut returned = ByteRecord::new();
        returned.push_field(b"recycled");
        parser
            .parse_chunk(
                &mut slice,
                super::split::Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                ChunkEnd::Document(b"a\nb\nc\nd\ne\n"),
                &mut pool,
                |batch| {
                    sizes.push(batch.len());
                    Ok(BatchDisposition::Reuse(Batch(if sizes.len() == 1 {
                        vec![returned.clone(); 4]
                    } else {
                        Vec::new()
                    })))
                },
            )
            .expect("the chunk parses");
        assert_eq!(sizes, [2, 2, 1]);
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].get(0), Some(b"recycled".as_slice()));

        let mut empty = SliceParser::<Csv>::new(b"", ParseOptions::new().headers(Headers::None))
            .expect("valid empty input");
        let mut emits = 0;
        parser
            .parse_chunk(
                &mut empty,
                super::split::Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                ChunkEnd::Document(b""),
                &mut Vec::new(),
                |_| {
                    emits += 1;
                    Ok(BatchDisposition::Reuse(Batch::new()))
                },
            )
            .expect("an empty chunk is valid");
        assert_eq!(emits, 0);

        let mut emits = 0;
        let error = super::emit_before_error(
            Batch::new(),
            &mut |_| {
                emits += 1;
                Ok(BatchDisposition::Reuse(Batch::new()))
            },
            Error::detailed(ErrorKind::Configuration, "failed"),
        )
        .expect_err("the original error is returned");
        assert_eq!(error.to_string(), "failed");
        assert_eq!(emits, 0);
    }

    #[test]
    fn ordered_drain_preserves_order_and_returns_consumed_batches() {
        let (sender, receiver) = super::sync_channel(2);
        let mut first = ByteRecord::new();
        first.push_field(b"first");
        sender
            .send(super::Message::Batch(Batch(vec![first])))
            .expect("send batch");
        sender
            .send(super::Message::ChunkDone)
            .expect("finish chunk");
        drop(sender);

        let (give_back, recycled) = super::sync_channel(1);
        let chunks = [super::split::Boundary {
            byte: 0,
            line: 1,
            record: 0,
        }];
        let mut seen = Vec::new();
        super::drain(
            vec![receiver],
            &[give_back],
            WorkerCount::requested(NonZeroUsize::MIN),
            &chunks,
            &mut |batch| {
                seen.push(batch[0].get(0).expect("one field").to_vec());
                Ok::<(), Error>(())
            },
        )
        .expect("the drain completes");
        assert_eq!(seen, [b"first".to_vec()]);
        assert_eq!(
            recycled
                .try_recv()
                .expect("the consumed batch is returned")
                .len(),
            1
        );
    }

    #[test]
    fn borrowed_parallel_path_forwards_the_exact_active_worker_width() {
        let input = b"a\nb\nc\nd\n";
        let parser = ParallelParser::<Csv>::new(ParseOptions::new().headers(Headers::None))
            .threads(NonZeroUsize::new(3).expect("three is non-zero"))
            .parallel_threshold(0);
        parser
            .for_each_record::<_, Error>(input, |_| Ok(()))
            .expect("the records parse");
        assert_eq!(super::unordered::LAST_DRIVE_THREADS.get(), 3);
    }

    #[test]
    fn splittable_errors_name_the_exact_unsupported_feature() {
        use crate::config::{FormatOptions, Recovery, Syntax};

        let multi = ParallelParser::with_options(
            FormatOptions::CSV.delimiter_sequence(b"--"),
            ParseOptions::new(),
        )
        .byte_records(b"a\n")
        .expect_err("multi-byte delimiters cannot be split");
        assert_eq!(
            multi.to_string(),
            "parallel parsing cannot split a format with multi-byte separators"
        );

        let bare = ParallelParser::with_options(
            FormatOptions::CSV.syntax(Syntax::Compatible(
                Recovery::NONE.quoting(true).unquoted_quotes(true),
            )),
            ParseOptions::new(),
        )
        .byte_records(b"a\n")
        .expect_err("bare quotes cannot be split");
        assert_eq!(
            bare.to_string(),
            "parallel parsing cannot split a format permitting bare quotes in unquoted fields"
        );
    }

    #[test]
    fn parallel_parser_edge_cases() {
        let _lock = INJECTION.lock().unwrap_or_else(PoisonError::into_inner);
        use crate::config::{BlankRecords, Escape, FieldCount, FormatOptions, Recovery, Syntax};

        // Check each check_splittable rejection branch:
        // 1. Comments
        let p_comm = ParallelParser::with_options(
            FormatOptions::CSV.comment(Some(b'#')),
            ParseOptions::new(),
        );
        assert!(p_comm.byte_records(b"a\n").is_err());

        // 2. Escape != DoubleQuote
        let p_esc = ParallelParser::with_options(
            FormatOptions::CSV.escape(Escape::Backslash(b'\\')),
            ParseOptions::new(),
        );
        assert!(p_esc.byte_records(b"a\n").is_err());

        // 3. Multi-byte
        let p_mb = ParallelParser::with_options(
            FormatOptions::CSV.delimiter_sequence(b"--"),
            ParseOptions::new(),
        );
        assert!(p_mb.byte_records(b"a\n").is_err());

        // 4. Quoting disabled
        let p_noq = ParallelParser::with_options(
            FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::NONE.quoting(false))),
            ParseOptions::new(),
        );
        assert!(p_noq.byte_records(b"a\n").is_err());

        // 5. Permits unquoted quotes
        let p_uq = ParallelParser::with_options(
            FormatOptions::CSV.syntax(Syntax::Compatible(
                Recovery::NONE.quoting(true).unquoted_quotes(true),
            )),
            ParseOptions::new(),
        );
        assert!(p_uq.byte_records(b"a\n").is_err());

        // 6. Blank records skip
        let p_skip = ParallelParser::with_options(
            FormatOptions::CSV.blank_records(BlankRecords::Skip),
            ParseOptions::new(),
        );
        assert!(p_skip.byte_records(b"a\n").is_err());

        let parser =
            ParallelParser::<Csv>::new(ParseOptions::new().field_count(FieldCount::MatchFirst))
                .threads(NonZeroUsize::new(2).expect("2"))
                .parallel_threshold(0);

        // MatchFirst on empty input
        let (opts, _) = parser.prepare(b"").expect("prepare on empty");
        assert_eq!(opts.requested_field_count(), FieldCount::Flexible);

        // headers() method
        let hdrs = parser.headers(b"a,b\n1,2\n").expect("headers");
        assert_eq!(hdrs.unwrap().len(), 2);

        // malformed data in parallel for_each_batch
        let malformed = b"a,b\n1,2\n\"bad\"quote,4\n5,6\n";
        let err = parser.byte_records(malformed).expect_err("should fail");
        assert!(matches!(
            err.kind(),
            ErrorKind::UnexpectedByteAfterQuote(_) | ErrorKind::UnexpectedQuote
        ));

        // consumer returning early refusal
        let count = std::sync::atomic::AtomicUsize::new(0);
        let parser_single =
            ParallelParser::<Csv>::new(ParseOptions::new()).parallel_threshold(usize::MAX);
        let _ = parser_single.for_each_batch(b"a,b\n1,2\n3,4\n", |_| {
            count.fetch_add(1, Ordering::Relaxed);
            Err(Error::detailed(ErrorKind::Configuration, "stop early"))
        });

        let doc = b"city,pop\nBoston,650706\nAustin,961855\nDenver,715522\n";

        // Parallel consumer returning early refusal (triggers worker channel send error / cancellation)
        let _ = parser.for_each_batch(doc, |_| {
            Err(Error::detailed(
                ErrorKind::Configuration,
                "stop parallel early",
            ))
        });

        // Parallel for_each_record and fold over threshold = 0
        let count_rec = std::sync::atomic::AtomicUsize::new(0);
        parser
            .for_each_record(doc, |_rec| {
                count_rec.fetch_add(1, Ordering::Relaxed);
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(count_rec.load(Ordering::Relaxed), 3);

        // Single-threaded fallback for for_each_record and fold (threads = 1 or threshold = MAX)
        let parser_st =
            ParallelParser::<Csv>::new(ParseOptions::new()).threads(NonZeroUsize::new(1).unwrap());
        let st_count = std::sync::atomic::AtomicUsize::new(0);
        parser_st
            .for_each_record(doc, |_rec| {
                st_count.fetch_add(1, Ordering::Relaxed);
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(st_count.load(Ordering::Relaxed), 3);

        let folded_st: Vec<usize> = parser_st
            .fold::<usize, _, _, Error>(
                doc,
                || 0,
                |acc, _rec| {
                    *acc += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(folded_st.into_iter().sum::<usize>(), 3);

        let folded: Vec<usize> = parser
            .fold::<usize, _, _, Error>(
                doc,
                || 0,
                |acc, _rec| {
                    *acc += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(folded.into_iter().sum::<usize>(), 3);

        // Multiple chunks with small batch_records to exercise batch recycling in run_worker
        let mut recycling_doc = Vec::new();
        for i in 0..500 {
            recycling_doc.extend_from_slice(format!("city{i},pop{i}\n").as_bytes());
        }
        let recycling_parser =
            ParallelParser::<Csv>::new(ParseOptions::new().headers(crate::config::Headers::None))
                .batch_records(NonZeroUsize::new(1).unwrap())
                .threads(NonZeroUsize::new(2).unwrap())
                .parallel_threshold(0);
        let mut rec_count = 0;
        recycling_parser
            .for_each_batch(&recycling_doc, |batch| {
                rec_count += batch.len();
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(rec_count, 500);

        // emit_before_error with non-empty batch before failure
        let partial_fail_doc = b"good1,val1\ngood2,val2\n\"bad\"quote,3\n";
        let pf_parser = ParallelParser::<Csv>::new(ParseOptions::new())
            .batch_records(NonZeroUsize::new(10).unwrap())
            .parallel_threshold(usize::MAX);
        let mut pf_seen = 0;
        let _ = pf_parser.for_each_batch(partial_fail_doc, |b| {
            pf_seen += b.len();
            Ok::<(), Error>(())
        });
        assert!(pf_seen > 0);

        // drain with disconnected receiver
        let (tx, rx) = super::sync_channel::<super::Message>(1);
        let (rtx, _rrx) = super::sync_channel::<Batch>(1);
        drop(tx); // disconnect immediately
        assert!(
            super::drain(
                vec![rx],
                &[rtx],
                WorkerCount::requested(NonZeroUsize::MIN),
                &[super::split::Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                }],
                &mut |_| Ok::<(), Error>(())
            )
            .is_ok()
        );

        // parse_chunk with read_byte_record_into error and un-newline-terminated EOF flush
        let no_nl_doc = b"a,b";
        let no_nl_parser =
            ParallelParser::<Csv>::new(ParseOptions::new().headers(crate::config::Headers::None))
                .threads(NonZeroUsize::new(1).unwrap())
                .parallel_threshold(usize::MAX);
        let mut no_nl_count = 0;
        no_nl_parser
            .for_each_batch(no_nl_doc, |batch| {
                no_nl_count += batch.len();
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(no_nl_count, 1);

        // read_byte_record_into error in parse_chunk
        let field_limit_parser = ParallelParser::<Csv>::new(
            ParseOptions::new()
                .headers(crate::config::Headers::None)
                .limits(crate::config::Limits::new(100, 2, 10)),
        )
        .threads(NonZeroUsize::new(1).unwrap())
        .parallel_threshold(usize::MAX);
        assert!(
            field_limit_parser
                .for_each_batch(b"toolong,1\n", |_| Ok::<(), Error>(()))
                .is_err()
        );

        // parse_chunk with end past document length to hit the trailing batch emission
        let mut slice_p = SliceParser::<Csv>::new(
            b"a,b\n",
            ParseOptions::new().headers(crate::config::Headers::None),
        )
        .unwrap();
        let mut p_pool = Vec::new();
        let mut emitted_batches = 0;
        let parser_obj =
            ParallelParser::<Csv>::new(ParseOptions::new().headers(crate::config::Headers::None));
        parser_obj
            .parse_chunk(
                &mut slice_p,
                super::split::Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                ChunkEnd::Boundary(100), // end past EOF
                &mut p_pool,
                |b| {
                    emitted_batches += 1;
                    Ok(BatchDisposition::Reuse(b))
                },
            )
            .unwrap();
        assert_eq!(emitted_batches, 1);

        // headers, try_fold, for_each_record, fold_records on multi-threaded parallel parser
        let mut big_csv = b"h1,h2\n".to_vec();
        for i in 0..100 {
            big_csv.extend_from_slice(format!("row_{i}_col1,row_{i}_col2\n").as_bytes());
        }
        let par_p = ParallelParser::<Csv>::new(
            ParseOptions::new().headers(crate::config::Headers::FirstRecord),
        )
        .threads(NonZeroUsize::new(2).unwrap())
        .parallel_threshold(10);
        let hdrs = par_p.headers(&big_csv).unwrap().unwrap();
        assert_eq!(hdrs.get(0), Some(b"h1".as_slice()));

        let count_fold = par_p
            .fold(
                &big_csv,
                || 0usize,
                |acc, _rec| {
                    *acc += 1;
                    Ok::<(), Error>(())
                },
            )
            .unwrap();
        assert_eq!(count_fold.into_iter().sum::<usize>(), 100);

        let for_each_count = std::sync::atomic::AtomicUsize::new(0);
        par_p
            .for_each_record(&big_csv, |_| {
                for_each_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(
            for_each_count.load(std::sync::atomic::Ordering::Relaxed),
            100
        );

        // parse_chunk emit returning None (early exit)
        let mut slice_p2 = SliceParser::<Csv>::new(
            b"a,b\nc,d\n",
            ParseOptions::new().headers(crate::config::Headers::None),
        )
        .unwrap();
        let mut p_pool2 = Vec::new();
        parser_obj
            .parse_chunk(
                &mut slice_p2,
                super::split::Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                ChunkEnd::Boundary(3),
                &mut p_pool2,
                |_| Ok(BatchDisposition::Stop), // early exit
            )
            .unwrap();

        // Single-threaded branches for fold, for_each_record, and prepare_options with FirstRecord width
        let st_parser = ParallelParser::with_options(
            FormatOptions::CSV,
            ParseOptions::new()
                .headers(crate::config::Headers::FirstRecord)
                .field_count(crate::config::FieldCount::MatchFirst),
        )
        .threads(NonZeroUsize::new(1).unwrap())
        .parallel_threshold(usize::MAX);

        let st_hdrs = st_parser.headers(b"h1,h2\nv1,v2\n").unwrap();
        assert!(st_hdrs.is_some());

        let st_count = std::sync::atomic::AtomicUsize::new(0);
        st_parser
            .for_each_record(b"h1,h2\nv1,v2\n", |_| {
                st_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok::<(), Error>(())
            })
            .unwrap();
        assert_eq!(st_count.load(std::sync::atomic::Ordering::Relaxed), 1);

        let st_fold = st_parser
            .fold(
                b"h1,h2\nv1,v2\n",
                || 0usize,
                |acc, _| {
                    *acc += 1;
                    Ok::<(), Error>(())
                },
            )
            .unwrap();
        assert_eq!(st_fold, vec![1]);
    }
}

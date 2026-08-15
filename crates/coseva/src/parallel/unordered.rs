//! Process a document's records on the worker threads, borrowed and unordered.
//!
//! This is the path for a caller that does not need records in document order
//! and does not need to keep them: a sum, a count, a filter that writes matches
//! somewhere. Each worker runs the caller's closure over [`Record`]s that
//! borrow its own parser, so nothing is copied into an owned record and nothing
//! is handed across a channel. The only shared state is the closure itself, a
//! chunk cursor, and one atomic that names the earliest failure, so
//! coordination is a few atomics rather than a queue per worker.
//!
//! Dropping the owned record and the queue is most of why this is worth having:
//! a serial borrowed parse is about twice as fast as a serial owned one on
//! narrow rows, because owning a record copies every field, and this keeps that
//! saving while spreading the parse across threads.
//!
//! # Per-worker state
//!
//! [`drive`] threads a worker-local accumulator of any type through the parse:
//! each worker builds its own with `init`, folds every record it parses into it
//! with `step`, and returns it, so a reduction touches no shared memory in the
//! hot loop. It is the difference between a parallel sum that scales and one
//! that serializes on a single contended cache line. [`for_each`] is that same
//! machine with the unit accumulator, for a closure that keeps no state of its
//! own.
//!
//! # Ordering and errors
//!
//! Records are delivered in no particular order, which is the freedom that lets
//! a worker run the closure the instant it has a record. The reported failure
//! is still deterministic: every chunk that begins before the earliest failure
//! is parsed to completion, so the error returned is the one a serial parse
//! would have stopped at — the lowest byte offset in the document — regardless
//! of which worker reached it first. A chunk that begins at or after that
//! offset is abandoned, and an already-running later chunk checks the same
//! fence before each callback, so a failure or early stop prevents further
//! side effects beyond the document's authoritative prefix.

use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(test)]
use std::cell::Cell;
use std::thread;

use crate::config::{FormatOptions, ParseOptions};
use crate::error::Error;
use crate::format::CsvFormat;
use crate::record::Record;
use crate::slice_parser::SliceParser;

use super::{ChunkEnd, WorkerCount, split::Boundary};

/// The barrier's value when no worker has failed yet.
///
/// Every real chunk begins below this, so until a failure lowers it no chunk is
/// ever skipped.
pub(crate) const NO_FAILURE: usize = usize::MAX;

fn failure_barrier() -> AtomicUsize {
    AtomicUsize::new(NO_FAILURE)
}

fn is_fenced(start: usize, barrier: &AtomicUsize) -> bool {
    start >= barrier.load(Ordering::Relaxed)
}

#[cfg(test)]
std::thread_local! {
    pub(super) static LAST_DRIVE_THREADS: Cell<usize> = const { Cell::new(0) };
}

/// Fold every record from `start` into a per-worker accumulator, across
/// `threads` workers, returning one accumulator per worker.
///
/// `chunks` are the record boundaries [`super::split::boundaries`] found, whose
/// first entry is `start`; consecutive entries delimit one chunk each. Workers
/// claim chunks from a shared cursor, so an uneven document balances itself
/// rather than stranding a long chunk on one thread.
///
/// Each worker seeds its accumulator with `init` and folds its records into it
/// with `step`. The returned vector holds those accumulators in no particular
/// order — a worker that claimed no chunk still contributes its seed — so the
/// caller combines them however its reduction demands. On failure the
/// accumulators are discarded and the earliest error is returned instead.
pub(super) fn drive<F, T, Init, Step, E>(
    input: &[u8],
    format: FormatOptions,
    options: &ParseOptions,
    chunks: &[Boundary],
    threads: WorkerCount,
    init: &Init,
    step: &Step,
) -> Result<Vec<T>, E>
where
    F: CsvFormat,
    Init: Fn() -> T + Sync,
    Step: Fn(&mut T, &Record<'_>) -> Result<(), E> + Sync,
    T: Send,
    E: From<Error> + Send,
{
    #[cfg(test)]
    LAST_DRIVE_THREADS.set(threads.get());

    let cursor = AtomicUsize::new(0);
    let barrier = failure_barrier();

    let results = thread::scope(|scope| {
        let handles = (0..threads.get())
            .map(|_| {
                let cursor = &cursor;
                let barrier = &barrier;
                let options = options.clone();
                scope.spawn(move || {
                    worker::<F, T, Init, Step, E>(
                        input, format, options, chunks, cursor, barrier, init, step,
                    )
                })
            })
            .collect::<Vec<_>>();

        handles
            .into_iter()
            .map(|handle| handle.join().expect("a worker thread panicked"))
            .collect::<Vec<_>>()
    });

    // The earliest failure by byte offset is the one a serial parse would have
    // reported, so it is the one returned no matter which worker found it.
    let mut accumulators = Vec::with_capacity(results.len());
    let mut earliest: Option<(usize, E)> = None;
    for (accumulator, failure) in results {
        accumulators.push(accumulator);
        if let Some(failure) = failure {
            let keep = failure_is_earlier(failure.0, earliest.as_ref().map(|(offset, _)| *offset));
            if keep {
                earliest = Some(failure);
            }
        }
    }
    match earliest {
        Some((_, error)) => Err(error),
        None => Ok(accumulators),
    }
}

fn failure_is_earlier(candidate: usize, current: Option<usize>) -> bool {
    current.is_none_or(|current| candidate < current)
}

/// Run `consume` over every record from `start`, across `threads` workers.
///
/// This is [`drive`] with the unit accumulator: a closure that keeps no state
/// of its own and reports only whether it accepted each record.
pub(super) fn for_each<F, C, E>(
    input: &[u8],
    format: FormatOptions,
    options: &ParseOptions,
    chunks: &[Boundary],
    threads: WorkerCount,
    consume: &C,
) -> Result<(), E>
where
    F: CsvFormat,
    C: Fn(&Record<'_>) -> Result<(), E> + Sync,
    E: From<Error> + Send,
{
    let init = || ();
    let step = |(): &mut (), record: &Record<'_>| consume(record);
    drive::<F, (), _, _, E>(input, format, options, chunks, threads, &init, &step)?;
    Ok(())
}

/// Claim and parse chunks until they run out or an earlier failure fences them.
///
/// A worker returns its own accumulator and its first failure, if any; the
/// caller reconciles the several workers' failures into the earliest.
#[expect(
    clippy::too_many_arguments,
    reason = "a worker is handed the whole parse context by reference; bundling it would only move the arguments into a struct built at the call site"
)]
fn worker<F, T, Init, Step, E>(
    input: &[u8],
    format: FormatOptions,
    options: ParseOptions,
    chunks: &[Boundary],
    cursor: &AtomicUsize,
    barrier: &AtomicUsize,
    init: &Init,
    step: &Step,
) -> (T, Option<(usize, E)>)
where
    F: CsvFormat,
    Init: Fn() -> T,
    Step: Fn(&mut T, &Record<'_>) -> Result<(), E>,
    E: From<Error>,
{
    let mut parser = match SliceParser::<F>::build(input, format, options) {
        Ok(parser) => parser,
        // The configuration was already validated once on the calling thread,
        // so a failure here is a boundary this worker cannot seek to; report it
        // at the document's start so it never masks a real parse error.
        Err(error) => return (init(), Some((0, E::from(error)))),
    };

    let mut accumulator = init();
    loop {
        // gamma::skip(literal.int_decrement, reason = "a zero increment repeatedly claims the same chunk and exceeded the two-minute timeout")
        let index = cursor.fetch_add(1, Ordering::Relaxed);
        let Some(&start) = chunks.get(index) else {
            return (accumulator, None);
        };
        let end = chunks
            .get(index + 1)
            .map_or(ChunkEnd::Document(input), |boundary| {
                ChunkEnd::Boundary(boundary.byte)
            });

        if let Some(failure) =
            parse_chunk::<F, T, Step, E>(&mut parser, start, end, barrier, &mut accumulator, step)
        {
            fence(barrier, failure.0);
            return (accumulator, Some(failure));
        }
    }
}

/// Parse the records in `[start, end)`, folding each borrowed one into `acc`.
///
/// Returns the byte offset and error of the first failure — a seek that could
/// not land, a malformed record, or the closure's own refusal — or `None` once
/// the chunk is exhausted.
fn parse_chunk<F, T, Step, E>(
    parser: &mut SliceParser<'_, F>,
    start: Boundary,
    end: ChunkEnd<'_>,
    barrier: &AtomicUsize,
    acc: &mut T,
    step: &Step,
) -> Option<(usize, E)>
where
    F: CsvFormat,
    Step: Fn(&mut T, &Record<'_>) -> Result<(), E>,
    E: From<Error>,
{
    if let Err(error) = parser.seek(start.into()) {
        return Some((start.byte, E::from(error)));
    }

    while let Ok(Some(mut line)) = parser.next_line() {
        match line.record() {
            Ok(record) => {
                // A record's extent includes its terminator, so the record
                // ending at the next chunk's first byte is this chunk's
                // last; stopping there is what keeps the chunks disjoint.
                let range = record.byte_range();
                if is_fenced(range.start, barrier) {
                    return None;
                }
                if let Err(error) = step(acc, &record) {
                    return Some((range.start, error));
                }
                if end.reached(range.end) {
                    return None;
                }
            }
            Err(error) => return Some((error.location().byte, E::from(error))),
        }
    }
    None
}

/// Lower the barrier to `offset` if it names an earlier failure than is known.
pub(crate) fn fence(barrier: &AtomicUsize, offset: usize) {
    // gamma::skip(expr.decrement, reason = "lowering the fence by one admitted an unbounded worker loop and exceeded the two-minute timeout")
    barrier.fetch_min(offset, Ordering::Relaxed);
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use core::num::NonZeroUsize;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::thread;

    use super::{
        Boundary, ChunkEnd, LAST_DRIVE_THREADS, NO_FAILURE, WorkerCount, drive, failure_barrier,
        failure_is_earlier, fence, is_fenced, parse_chunk, worker,
    };
    use crate::config::{FormatOptions, Headers, ParseOptions};
    use crate::error::Error;
    use crate::format::Csv;
    use crate::slice_parser::SliceParser;

    #[test]
    fn an_already_running_later_chunk_stops_before_its_next_callback() {
        let input = b"early\nlater-1\nlater-2\n";
        let options = ParseOptions::new().headers(Headers::None);
        let mut parser = SliceParser::<Csv>::new(input, options).expect("a valid test document");
        let barrier = AtomicUsize::new(NO_FAILURE);
        let callbacks = AtomicUsize::new(0);
        let gate = (Mutex::new((false, false)), Condvar::new());
        let step = |(): &mut (), _record: &crate::Record<'_>| {
            callbacks.fetch_add(1, Ordering::Relaxed);
            let (state, changed) = &gate;
            let mut state = state.lock().expect("the gate is not poisoned");
            state.0 = true;
            changed.notify_one();
            while !state.1 {
                state = changed.wait(state).expect("the gate is not poisoned");
            }
            Ok::<(), Error>(())
        };

        thread::scope(|scope| {
            let worker = scope.spawn(|| {
                parse_chunk::<Csv, (), _, Error>(
                    &mut parser,
                    Boundary {
                        byte: 6,
                        line: 2,
                        record: 1,
                    },
                    ChunkEnd::Document(input),
                    &barrier,
                    &mut (),
                    &step,
                )
            });

            let (state, changed) = &gate;
            let mut state = state.lock().expect("the gate is not poisoned");
            while !state.0 {
                state = changed.wait(state).expect("the gate is not poisoned");
            }
            fence(&barrier, 0);
            state.1 = true;
            changed.notify_one();
            drop(state);

            assert!(worker.join().expect("the worker did not panic").is_none());
        });

        assert_eq!(
            callbacks.load(Ordering::Relaxed),
            1,
            "the callback that was already running may finish, but no later callback may begin"
        );
    }

    #[test]
    fn failure_order_and_worker_width_are_exact() {
        assert_eq!(NO_FAILURE, usize::MAX);
        let barrier = failure_barrier();
        assert_eq!(barrier.load(Ordering::Relaxed), usize::MAX);
        assert!(!is_fenced(usize::MAX - 1, &barrier));
        assert!(is_fenced(usize::MAX, &barrier));
        fence(&barrier, 4);
        assert!(!is_fenced(3, &barrier));
        assert!(is_fenced(4, &barrier));
        assert!(failure_is_earlier(4, None));
        assert!(failure_is_earlier(4, Some(5)));
        assert!(!failure_is_earlier(4, Some(4)));
        assert!(!failure_is_earlier(5, Some(4)));

        let options = ParseOptions::new().headers(Headers::None);
        let chunks = [
            Boundary {
                byte: 0,
                line: 1,
                record: 0,
            },
            Boundary {
                byte: 2,
                line: 2,
                record: 1,
            },
        ];
        super::for_each::<Csv, _, Error>(
            b"a\nb\n",
            FormatOptions::CSV,
            &options,
            &chunks,
            WorkerCount::requested(NonZeroUsize::new(2).expect("two is non-zero")),
            &|_| Ok(()),
        )
        .expect("the records parse");
        assert_eq!(LAST_DRIVE_THREADS.get(), 2);
    }

    #[test]
    fn barriers_stop_equal_offsets_and_worker_failures_lower_the_fence() {
        let input = b"a\nb\n";
        let options = ParseOptions::new().headers(Headers::None);
        let chunks = [Boundary {
            byte: 0,
            line: 1,
            record: 0,
        }];

        let cursor = AtomicUsize::new(0);
        let barrier = AtomicUsize::new(0);
        let callbacks = AtomicUsize::new(0);
        let (_, failure) = worker::<Csv, (), _, _, Error>(
            input,
            FormatOptions::CSV,
            options.clone(),
            &chunks,
            &cursor,
            &barrier,
            &|| (),
            &|_, _| {
                callbacks.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(failure.is_none());
        assert_eq!(callbacks.load(Ordering::Relaxed), 0);

        let mut parser =
            SliceParser::<Csv>::new(input, options.clone()).expect("the parser builds");
        let callbacks = AtomicUsize::new(0);
        let failure = parse_chunk::<Csv, (), _, Error>(
            &mut parser,
            chunks[0],
            ChunkEnd::Document(input),
            &barrier,
            &mut (),
            &|_, _| {
                callbacks.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(failure.is_none());
        assert_eq!(callbacks.load(Ordering::Relaxed), 0);

        let cursor = AtomicUsize::new(0);
        let barrier = AtomicUsize::new(NO_FAILURE);
        let (_, failure) = worker::<Csv, (), _, _, Error>(
            input,
            FormatOptions::CSV,
            options,
            &chunks,
            &cursor,
            &barrier,
            &|| (),
            &|_, _| Err(Error::detailed(crate::ErrorKind::Configuration, "stop")),
        );
        assert_eq!(failure.as_ref().map(|failure| failure.0), Some(0));
        assert_eq!(barrier.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn parse_chunk_seek_and_parse_errors() {
        let input = b"valid\n\"bad\"quote\n";
        let options = ParseOptions::new().headers(Headers::None);
        let mut parser = SliceParser::<Csv>::new(input, options).expect("valid");
        let barrier = AtomicUsize::new(NO_FAILURE);

        // Seek past end of input
        let res = parse_chunk::<Csv, (), _, Error>(
            &mut parser,
            Boundary {
                byte: 1000,
                line: 10,
                record: 5,
            },
            ChunkEnd::Document(input),
            &barrier,
            &mut (),
            &|_acc, _rec| Ok(()),
        );
        assert!(res.is_some());

        // Parse error in chunk (line.record error)
        let res = parse_chunk::<Csv, (), _, Error>(
            &mut parser,
            Boundary {
                byte: 6,
                line: 2,
                record: 1,
            },
            ChunkEnd::Document(input),
            &barrier,
            &mut (),
            &|_acc, _rec| Ok(()),
        );
        assert!(res.is_some());

        // next_line error in parse_chunk
        let opts_limit = ParseOptions::new()
            .headers(Headers::None)
            .limits(crate::config::Limits::new(4, 100, 10));
        let mut p_limit =
            SliceParser::<Csv>::new(b"toolong_record\n", opts_limit).expect("valid parser");
        let res_nl = parse_chunk::<Csv, (), _, Error>(
            &mut p_limit,
            Boundary {
                byte: 0,
                line: 1,
                record: 0,
            },
            ChunkEnd::Document(b"toolong_record\n"),
            &barrier,
            &mut (),
            &|_acc, _rec| Ok(()),
        );
        assert!(res_nl.is_some());

        // fence updates
        let b = AtomicUsize::new(NO_FAILURE);
        fence(&b, 100);
        assert_eq!(b.load(Ordering::Relaxed), 100);
        fence(&b, 50);
        assert_eq!(b.load(Ordering::Relaxed), 50);
        fence(&b, 200); // larger than current, no change
        assert_eq!(b.load(Ordering::Relaxed), 50);

        // step returning error
        let res_step_err = parse_chunk::<Csv, (), _, Error>(
            &mut parser,
            Boundary {
                byte: 0,
                line: 1,
                record: 0,
            },
            ChunkEnd::Document(input),
            &barrier,
            &mut (),
            &|_acc, _rec| {
                Err(Error::detailed(
                    crate::error::ErrorKind::Configuration,
                    "step fail",
                ))
            },
        );
        assert!(res_step_err.is_some());
    }

    #[test]
    fn test_worker_skips_chunks_after_barrier() {
        let input = b"valid\n\"bad\"quote\nvalid3\nvalid4\n";
        let options = ParseOptions::new().headers(Headers::None);
        let chunks = vec![
            Boundary {
                byte: 0,
                line: 1,
                record: 0,
            },
            Boundary {
                byte: 6,
                line: 2,
                record: 1,
            },
            Boundary {
                byte: 17,
                line: 3,
                record: 2,
            },
            Boundary {
                byte: 24,
                line: 4,
                record: 3,
            },
        ];
        let res = drive::<Csv, usize, _, _, Error>(
            input,
            FormatOptions::CSV,
            &options,
            &chunks,
            WorkerCount::requested(NonZeroUsize::new(2).expect("two is non-zero")),
            &|| 0,
            &|_acc, _rec| Ok(()),
        );
        assert!(res.is_err());

        // for_each test
        let count = AtomicUsize::new(0);
        let res_fe = super::for_each::<Csv, _, Error>(
            b"a\nb\n",
            FormatOptions::CSV,
            &options,
            &[
                Boundary {
                    byte: 0,
                    line: 1,
                    record: 0,
                },
                Boundary {
                    byte: 2,
                    line: 2,
                    record: 1,
                },
            ],
            WorkerCount::requested(NonZeroUsize::new(2).expect("two is non-zero")),
            &|_rec| {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(res_fe.is_ok());

        // Test worker with invalid format options to hit SliceParser::build error in worker
        let invalid_opts = FormatOptions::CSV.delimiter(b'"');
        let cursor = AtomicUsize::new(0);
        let barrier = AtomicUsize::new(NO_FAILURE);
        let (_, err) = worker::<Csv, (), _, _, Error>(
            input,
            invalid_opts,
            options,
            &chunks,
            &cursor,
            &barrier,
            &|| (),
            &|_acc, _rec| Ok(()),
        );
        assert_eq!(err.as_ref().map(|error| error.0), Some(0));
    }
}

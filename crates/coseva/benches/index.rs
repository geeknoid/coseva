//! Building a record index, and seeking with one.
//!
//! An index turns "read record 900,000" from a scan of everything before it
//! into an offset lookup and a parser positioned at that offset. Whether that
//! is worth doing depends on two numbers this suite exists to supply: what
//! building the index costs per record, and what a seek costs once
//! it is built.
//!
//! # Three ways to build
//!
//! [`CsvIndex::build`] parses the source and keeps two `Vec<u64>` — a byte
//! offset and a physical line per record — in memory. [`CsvIndex::create`]
//! parses the same source but streams each position straight to a writer
//! and keeps nothing, so its working set is constant no matter how long the
//! file is. [`CsvIndex::generate`] has no source to parse: it encodes typed
//! values into a document with [`CsvEncode`] and indexes the record it just
//! wrote, in the same pass, so a caller building a document from values
//! already in memory is not made to write it and read it back just to index
//! it. Every record `generate` writes is still fully validated against the
//! index's own limits before being counted — by scanning the record's own
//! bytes for field and record boundaries wherever that scan alone can settle
//! the question, falling back to a parser only for the formats where it
//! cannot. All three are measured, because each exists for a caller who does
//! not have what the other two assume, and deserves to know the price of the
//! guarantee it buys.
//!
//! # Validating what `generate` just wrote
//!
//! `FastValidator` is what makes that validation cheap: built once per call
//! to `generate`, not once per record, it walks a record's bytes looking for
//! the same delimiter, quote and terminator a parser would, using the same
//! SIMD-accelerated search the crate's own borrowed parser already uses to do
//! it. For a safe, unambiguous dialect this settles a record's field count,
//! field lengths and total length — everything [`Limits`] governs — without
//! ever constructing a parser. [`Quoting::Raw`] and other ambiguity-capable
//! configurations are excluded from this scan up front, since encoded output
//! in those dialects can be structurally ambiguous in ways only a real parser
//! resolves correctly; those records, and the rare record the scan cannot
//! confidently settle on its own, still fall back to
//! `RecordValidator`, a single [`PushParser`] reused across the whole call
//! rather than rebuilt per record. `generate_raw` measures that fallback path
//! directly, by forcing every record through it.
//!
//! # The two ways to seek
//!
//! `at_first`, `at_middle` and `at_last` all do the same work: resolve one
//! record to a location, stand up a parser there, and read that record. The
//! record number is swept only to demonstrate that it does not matter, which
//! is the whole claim an index makes. `reader_at_*` asks the same of
//! [`CsvIndexReader`], which reads positions back from the index rather than
//! holding them.
//!
//! Both seek groups include [`CsvIndex::validate_source`], because
//! [`CsvIndex::parser_at`] calls it on every seek: an index is bound to the
//! exact bytes it was built from, and the API refuses to hand back a parser
//! without checking. That check is part of what a seek costs, so it is
//! measured rather than hidden.
//!
//! # Binding a source
//!
//! [`CsvIndex::parser_at`] validates its source on every call, because
//! nothing tells it two calls were ever given the same bytes to compare.
//! [`CsvIndex::bind`] validates once and returns a [`BoundSource`], whose own
//! [`BoundSource::parser_at`] skips that check: it already holds the exact
//! slice `bind` validated, so there is nothing left to recheck. This exists
//! for a caller who seeks the same source repeatedly, where the choice is
//! between paying [`CsvIndex::validate_source`]'s cost once, up front, or
//! once per seek. `seek_by_size` and `bound_seek` measure exactly that
//! choice, swept over corpus size rather than position, since size is what
//! binding changes the cost of.
//!
//! # Results
//!
//! Callgrind instruction counts. Per record for the build groups is
//! `(rows_1000 - rows_100) / 900`, which cancels the fixed setup.
//!
//! Building:
//!
//! | Case       | 1      | 10     | 100     | 1000      | Per record |
//! |------------|--------|--------|---------|-----------|------------|
//! | `build`    |  2,219 |  9,817 |  68,040 |   627,052 |        621 |
//! | `create`   | 15,081 | 22,924 |  93,805 |   828,287 |        816 |
//! | `generate` | 10,086 | 41,560 | 348,755 | 3,469,199 |      3,467 |
//!
//! Five more `generate` calls, each varying one thing about the record's
//! shape rather than its size, to separate what the byte count buys from
//! what the field count, quoting or a single long field costs:
//!
//! | Case               | 1      | 10     | 100       | 1000       | Per record |
//! |---------------------|--------|--------|-----------|------------|------------|
//! | `generate_narrow`   |  5,851 | 11,126 |    57,617 |    529,231 |        524 |
//! | `generate_wide`     | 29,859 | 238,146 | 2,335,766 | 23,337,558 |     23,335 |
//! | `generate_quoted`   | 10,002 | 41,694 |   344,820 |  3,476,019 |      3,479 |
//! | `generate_raw`      |  9,499 | 38,810 |   324,319 |  3,227,863 |      3,226 |
//! | `generate_long`     | 11,931 | 61,111 |   574,599 |  5,695,175 |      5,690 |
//!
//! Seeking one record of a 1000-record index. This is a fixed cost, and the
//! record number is swept only to show that it is:
//!
//! | Case          | first  | middle | last   |
//! |---------------|--------|--------|--------|
//! | `seek`        | 31,996 | 32,041 | 32,058 |
//! | `reader_seek` | 11,797 | 11,797 | 11,109 |
//!
//! Seeking the last record of an index, corpus size swept instead of
//! position:
//!
//! | Case           | 1     | 10    | 100   | 1000   |
//! |----------------|-------|-------|-------|--------|
//! | `seek_by_size` | 1,826 | 2,273 | 4,984 | 32,058 |
//! | `bound_seek`   | 1,708 | 1,791 | 1,753 |  1,674 |
//!
//! # What the numbers say
//!
//! Seeking does not depend on how far into the file the record is. `seek`
//! spans 62 instructions across the whole corpus, 0.2%, and `reader_seek` is
//! flat between its first two positions. That is the property an index is
//! bought for, and it is worth having as a measurement rather than as a
//! description of the data structure. `reader_seek`'s last position is
//! cheaper than its first two rather than dearer, which rules out any
//! remaining suspicion of a scan.
//!
//! Streaming the index costs 31% more per record than building it in memory —
//! 816 against 621 — and pays a much larger fixed cost, 15,081 instructions
//! before the first record against 2,219. So `create` is not something to
//! reach for by default on small inputs, but its marginal price for a bounded
//! working set is modest, and on any file large enough to need it the fixed
//! cost has long since stopped mattering.
//!
//! `generate` costs 3,467 instructions per record on `City`'s six fields,
//! over four times `create` and nearly six times `build`, because it does
//! more than either: it encodes a typed value, writes it, indexes the
//! record, and validates it against the same limits a stored source would
//! face — all in one pass. `generate_narrow`, at 524 instructions per record
//! for a single `u32` field, is the floor that per-record bookkeeping alone
//! costs once field count and quoting stop being variables, and it already
//! sits below `create`. `generate_wide` grows that same bookkeeping to a
//! hundred fields and lands at 23,335 per record — about 233 per field,
//! close to what `generate_narrow`'s one field costs, which says the scan's
//! per-field work does not get relatively cheaper or dearer as a record grows
//! wider; it is linear in field count, as a boundary scan should be.
//! `generate_quoted` forces every field through the quoted-field scan instead
//! of the unquoted one and costs about the same as plain `generate`, which
//! means quoting is not what makes validation expensive here. `generate_long`
//! adds one roughly 300-byte unquoted field and costs 5,690 per record, most
//! of which is the cost of encoding and copying that many extra bytes rather
//! than validating them, since the boundary scan itself is one pass over
//! whatever length a field turns out to have.
//!
//! `generate_raw` is the one case that cannot use the boundary scan at all:
//! [`Quoting::Raw`] writes fields unescaped, so a record's own bytes cannot
//! be told apart from a delimiter or terminator that happens to occur inside
//! a field's content without a real parser resolving the ambiguity, and
//! `generate` falls back to reusing one parser across the whole call rather
//! than rebuilding one per record. That reuse is not a shortcut through the
//! checks: `tests/index_generate.rs` reparses every record in full on every
//! fallback call and still catches oversized fields, excess fields and
//! quoting ambiguity at every point in the reused parser's lifetime,
//! including after many earlier records have already passed. Measured here it
//! costs about the same per record as plain `generate` at this scale — the
//! scan `generate` otherwise uses is cheap enough, and the reused parser
//! disciplined enough, that neither path stands out as the expensive one; the
//! difference is what each can safely claim about a record's bytes, not raw
//! speed.
//!
//! `seek` costs 2.7× `reader_seek`, at 31,996 against 11,797, which inverts
//! what the shapes suggest — one reads positions from a `Vec` already in
//! memory, the other from a `Cursor`. The difference is
//! [`CsvIndex::validate_source`], which `parser_at` runs over the entire
//! source on every seek and `parser_at_reader` cannot, having no slice to run
//! it over. These two rows therefore do not measure the same work: the gap is
//! the price of a safety check, not a reason to prefer the reader. It also
//! means `seek`'s cost is proportional to the size of the file rather than to
//! anything about the index, which is worth knowing before seeking in a loop.
//!
//! That proportionality is exactly what `seek_by_size` and `bound_seek`
//! measure, by sweeping corpus size instead of position. `seek_by_size` — a
//! plain `parser_at` call — grows from 1,826 instructions at one row to
//! 32,058 at a thousand, because `validate_source` hashes the whole source
//! every call. `bound_seek` — the same seek through a [`BoundSource`]
//! returned by [`CsvIndex::bind`] — stays within a hundred-odd instructions of
//! 1,700 across the same range, because that hash happened once, in `bind`,
//! and [`BoundSource::parser_at`] never touches the source again. This is not
//! inferred from the flat shape: `tests/index.rs` proves it directly by
//! counting calls into the source's own `AsRef` implementation and showing
//! `bind` makes exactly one while repeated seeks make none. A caller seeking
//! the same source more than a handful of times pays the hash once under
//! `bind` instead of once per seek, and the saving grows with the source, not
//! with the number of seeks.
//!
//! Building costs 621 instructions per record where `dialects` measures a
//! plain `ByteRecord` parse of the same rows at 672. Indexing is therefore
//! about one parse — slightly less, since it records positions rather than
//! materializing fields — so an index is roughly a one-pass investment that
//! makes every later access independent of position. That comparison crosses
//! binaries and is offered as a sense of scale, not a measurement.
//!
//! # Index size
//!
//! Measured on the same corpus, whose records are a fixed 51 bytes:
//!
//! | Records | Source bytes | Index bytes | Per record |
//! |---------|--------------|-------------|------------|
//! | 1       |           51 |         141 |     141.00 |
//! | 10      |          510 |         285 |      28.50 |
//! | 100     |        5,100 |       1,725 |      17.25 |
//! | 1000    |       51,000 |      16,125 |      16.13 |
//!
//! Sixteen bytes per record asymptotically — a byte offset and a physical
//! line, both `u64` — over a 125-byte header. The in-memory index holds the
//! same two values in two `Vec<u64>`, so it costs the same 16 bytes per record
//! without the header.
//!
//! An index is therefore 32% of the size of this source, which is a corpus of
//! unusually short records; on realistic rows the ratio falls, but the 16
//! bytes per record does not.
//!
//! The line is not redundant with the record number in general, since a quoted
//! field may contain a newline and push a record past its ordinal line. It is
//! what lets an error raised after a seek report the physical line rather than
//! only the record, which is the reason both are kept.
//!
//! # What this does not measure
//!
//! `build_path`, `create_path`, `save` and `load` all touch the filesystem, so
//! they would measure the host rather than the crate. Nor is the `csv`
//! ecosystem's `csv-index` compared, since it is not a dependency here.
//!
//! Numbers in this file are comparable only to each other; `fixture.rs`
//! records the measurement showing why.
//!
//! [`CsvIndex::build`]: coseva::index::CsvIndex::build
//! [`CsvIndex::create`]: coseva::index::CsvIndex::create
//! [`CsvIndex::generate`]: coseva::index::CsvIndex::generate
//! [`CsvIndex::bind`]: coseva::index::CsvIndex::bind
//! [`CsvIndex::parser_at`]: coseva::index::CsvIndex::parser_at
//! [`CsvIndex::validate_source`]: coseva::index::CsvIndex::validate_source
//! [`CsvIndexReader`]: coseva::index::CsvIndexReader
//! [`BoundSource`]: coseva::index::BoundSource
//! [`BoundSource::parser_at`]: coseva::index::BoundSource::parser_at
//! [`CsvEncode`]: coseva::encoding::CsvEncode
//! [`Limits`]: coseva::config::Limits
//! [`Quoting::Raw`]: coseva::config::Quoting::Raw
//! [`PushParser`]: coseva::PushParser

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{EmitOptions, FormatOptions, Quoting};
use coseva::encoding::CsvEncode;
use coseva::index::{BoundSource, CsvIndex, CsvIndexReader, IndexOptions};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(
    dead_code,
    reason = "this suite indexes the shared corpus but uses none of its checksum helpers"
)]
mod fixture;

use fixture::{ROWS_1, ROWS_10, ROWS_100, ROWS_1000, drop_it};

/// The record every seek case asks for in the `at_middle` position.
const MIDDLE: usize = 500;

/// The last record of the 1000-record corpus.
const LAST: usize = 999;

/// Assert the index found one entry per record, so a case cannot quietly index
/// a truncated source and still look cheap.
fn check(index: &CsvIndex, input: &[u8]) -> u64 {
    let expected = (input.len() / fixture::ROW_LEN) as u64;
    let last = usize::try_from(expected - 1).expect("corpus fits in usize");
    assert!(
        index.record_offset(last).is_some() && index.record_offset(last + 1).is_none(),
        "benchmark indexed the wrong number of records"
    );
    expected
}

// ── building ─────────────────────────────────────────────────────────────────

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), teardown = drop_it)]
fn build(input: &'static [u8]) -> CsvIndex {
    let index = CsvIndex::build(input, IndexOptions::default())
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    black_box(check(&index, input));
    index
}

// The `Vec` behind the cursor is pre-sized in setup, so the measured region
// pays for writing the index rather than for growing the buffer it lands in.
fn create_state(input: &'static [u8]) -> (&'static [u8], Cursor<Vec<u8>>) {
    let records = input.len() / fixture::ROW_LEN;
    (input, Cursor::new(Vec::with_capacity(64 + records * 16)))
}

#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = create_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = create_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = create_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = create_state, teardown = drop_it)]
fn create(state: (&'static [u8], Cursor<Vec<u8>>)) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (input, sink) = state;
    CsvIndex::create(input, sink, IndexOptions::default())
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
}

// The same row [`fixture::ROW`] parses, as a struct, so `generate` writes
// exactly the corpus `build` and `create` read. Kept local rather than moved
// into `fixture.rs`, since that module is shared by suites that only ever
// read and none of them need a `CsvEncode` type.
#[derive(Clone, CsvEncode)]
#[expect(
    clippy::struct_field_names,
    reason = "the field names are the corpus's header names"
)]
struct City {
    city: &'static str,
    state: &'static str,
    population: u32,
    latitude: f64,
    longitude: f64,
    coastal: bool,
}

static CITY: City = City {
    city: "Boston",
    state: "Massachusetts",
    population: 4_500_000,
    latitude: 42.3601,
    longitude: -71.0589,
    coastal: true,
};

/// Values, a pre-sized document sink and a pre-sized index sink, so the
/// measured region pays for encoding and indexing rather than for growing
/// either buffer it lands in.
type GenerateState = (Vec<City>, Cursor<Vec<u8>>, Cursor<Vec<u8>>);

fn generate_state(rows: usize) -> GenerateState {
    let values = vec![CITY.clone(); rows];
    let out = Cursor::new(Vec::with_capacity(rows * fixture::ROW_LEN + 64));
    let idx = Cursor::new(Vec::with_capacity(64 + rows * 16));
    (values, out, idx)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = generate_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = generate_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = generate_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = generate_state, teardown = drop_it)]
fn generate(state: GenerateState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions::default(),
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

// `generate` above already exercises `FastValidator`'s common case: a handful
// of short, unquoted fields. The five cases below each change exactly one
// axis of a record's shape away from that baseline, isolating what the shape
// of a record — rather than its count — costs. Every case shares `generate`'s
// own structure (encode into a pre-sized document sink, index into a
// pre-sized index sink, assert the record count) so a row is comparable
// across cases the same way it already is across corpus sizes.

/// A record of one short field, narrower than `City`, isolating the
/// fixed per-record cost (encode, entry write, checksum, top-level
/// [`Limits`] checks) from any per-field cost the other cases below add on
/// top of it.
#[derive(Clone, CsvEncode)]
struct Narrow {
    value: u32,
}

type NarrowState = (Vec<Narrow>, Cursor<Vec<u8>>, Cursor<Vec<u8>>);

fn narrow_state(rows: usize) -> NarrowState {
    let values = vec![Narrow { value: 42 }; rows];
    let out = Cursor::new(Vec::with_capacity(rows * 8 + 64));
    let idx = Cursor::new(Vec::with_capacity(64 + rows * 16));
    (values, out, idx)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = narrow_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = narrow_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = narrow_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = narrow_state, teardown = drop_it)]
fn generate_narrow(state: NarrowState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions::default(),
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

/// The number of columns [`Wide`] encodes, matching `decode_wide.rs`'s own
/// hundred-column convention for what this crate calls a "wide" record.
const WIDE_COLUMNS: usize = 100;

/// A record of a hundred short fields, to see whether `FastValidator`'s
/// per-field cost scales linearly with field count or carries a hidden cost
/// per record that only a many-field record would expose.
///
/// Implemented by hand rather than derived: a hundred named fields would be
/// pure boilerplate for a shape whose only interesting property is its field
/// count. `tests/emitter.rs`'s `CommaAt` establishes the same
/// hand-written-`CsvEncode` pattern for a field count picked at runtime
/// rather than spelled out in a struct definition.
#[derive(Clone)]
struct Wide;

impl CsvEncode for Wide {
    fn csv_encode<V: coseva::encoding::EncodeVisitor>(
        &self,
        visitor: &mut V,
    ) -> Result<(), coseva::Error> {
        for index in 0..WIDE_COLUMNS {
            visitor.visit_field(index, "c", b"42")?;
        }
        Ok(())
    }

    fn field_names() -> &'static [&'static str] {
        // Not read by `generate` (no header is written; see
        // `EmitOptions::has_headers(false)` below), so a hundred identical
        // names cost nothing here.
        &["c"; WIDE_COLUMNS]
    }
}

type WideState = (Vec<Wide>, Cursor<Vec<u8>>, Cursor<Vec<u8>>);

fn wide_state(rows: usize) -> WideState {
    let values = vec![Wide; rows];
    let row_len = WIDE_COLUMNS * 3; // "42," per column, one trailing "\n".
    let out = Cursor::new(Vec::with_capacity(rows * row_len + 64));
    let idx = Cursor::new(Vec::with_capacity(64 + rows * 16));
    (values, out, idx)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = wide_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = wide_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = wide_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = wide_state, teardown = drop_it)]
fn generate_wide(state: WideState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions::default(),
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

/// [`City`] indexed under [`Quoting::Always`], so every field takes
/// `FastValidator::scan_quoted_field` rather than
/// `FastValidator::scan_unquoted_field`. None of `City`'s fields are long
/// enough to escalate past the manual scalar prefix (see
/// `FAST_SCALAR_PREFIX_BYTES` in `src/index/generate.rs`), so this isolates
/// the quoted path's own per-field cost from the escalation `generate_long`
/// below measures instead.
fn quoted_state(rows: usize) -> GenerateState {
    generate_state(rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = quoted_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = quoted_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = quoted_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = quoted_state, teardown = drop_it)]
fn generate_quoted(state: GenerateState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions {
            format: FormatOptions::CSV.quoting(Quoting::Always),
            ..IndexOptions::default()
        },
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

/// [`City`] indexed under [`Quoting::Raw`], the one quoting policy
/// `FastValidator::for_format` declines outright: `Raw` never escapes
/// anything, so a scan can never soundly tell a field's true end from a
/// delimiter that only happens to appear inside one. Every record here
/// therefore falls all the way back to `RecordValidator`, which reparses
/// it — the same cost `generate` paid everywhere before `FastValidator`
/// existed. This case exists to keep that fallback cost visible rather
/// than only provable by reading the source: a caller who chooses `Raw`
/// for its speed of encoding should be able to see what it costs on the
/// indexing side too.
fn raw_state(rows: usize) -> GenerateState {
    generate_state(rows)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = raw_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = raw_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = raw_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = raw_state, teardown = drop_it)]
fn generate_raw(state: GenerateState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions {
            format: FormatOptions::CSV.quoting(Quoting::Raw),
            ..IndexOptions::default()
        },
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

/// A free-text field running a few hundred bytes, beside two short ones —
/// `documents.rs`'s own convention for a "long" field (see its `prose`
/// corpus), reused here for the same reason: a field this size is common
/// enough (a description, a note, a comment column) to deserve its own case
/// rather than being extrapolated from `City`'s much shorter fields.
///
/// `SUMMARY` holds no delimiter, quote or record-ending byte, so under the
/// default [`Quoting::Necessary`] it is written unquoted and this measures
/// `FastValidator::scan_unquoted_field`'s manual-prefix-then-escalate scan
/// (`near3`/`near4` in `src/index/generate.rs`) with the escalation path
/// actually taken, rather than only the short-field case every other row in
/// this suite exercises.
const SUMMARY: &str = "A quick survey of regional infrastructure spending across \
the last four fiscal years shows steady growth in transit and roadway \
maintenance budgets offset in part by declining allocations toward new \
construction projects and a modest increase in administrative overhead \
across most reporting districts and neighboring counties as well";

#[derive(Clone, CsvEncode)]
struct Long {
    name: &'static str,
    id: u32,
    summary: &'static str,
}

type LongState = (Vec<Long>, Cursor<Vec<u8>>, Cursor<Vec<u8>>);

fn long_state(rows: usize) -> LongState {
    let value = Long {
        name: "Boston",
        id: 1,
        summary: SUMMARY,
    };
    let values = vec![value; rows];
    let row_len = SUMMARY.len() + 32;
    let out = Cursor::new(Vec::with_capacity(rows * row_len + 64));
    let idx = Cursor::new(Vec::with_capacity(64 + rows * 16));
    (values, out, idx)
}

#[library_benchmark]
#[bench::rows_1(args = (1), setup = long_state, teardown = drop_it)]
#[bench::rows_10(args = (10), setup = long_state, teardown = drop_it)]
#[bench::rows_100(args = (100), setup = long_state, teardown = drop_it)]
#[bench::rows_1000(args = (1000), setup = long_state, teardown = drop_it)]
fn generate_long(state: LongState) -> CsvIndexReader<Cursor<Vec<u8>>> {
    let (values, out, idx) = state;
    let rows = values.len();
    let reader = CsvIndex::generate(
        out,
        idx,
        values,
        IndexOptions::default(),
        EmitOptions::new()
            .has_headers(false)
            .buffer_capacity(fixture::BUFFER),
    )
    .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        reader.len(),
        rows as u64,
        "benchmark generated the wrong number of records"
    );
    black_box(reader)
}

// ── seeking ──────────────────────────────────────────────────────────────────

type SeekState = (CsvIndex, &'static [u8], usize);

fn seek_state(record: usize) -> SeekState {
    let index = CsvIndex::build(ROWS_1000, IndexOptions::default())
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    (index, ROWS_1000, record)
}

// The record is read, not merely positioned at, because an index that returned
// a parser pointing anywhere would look identically fast otherwise.
#[library_benchmark]
#[bench::at_first(args = (0), setup = seek_state, teardown = drop_it)]
#[bench::at_middle(args = (MIDDLE), setup = seek_state, teardown = drop_it)]
#[bench::at_last(args = (LAST), setup = seek_state, teardown = drop_it)]
fn seek(state: SeekState) -> (u64, CsvIndex) {
    let (index, source, record) = state;
    let mut parser = index
        .parser_at(source, record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
        .unwrap_or_else(|| panic!("benchmark input failed: record {record} missing"));
    let fields = line
        .record()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        fields.index(),
        record as u64,
        "benchmark seeked to the wrong record"
    );
    (black_box(fields.len() as u64), index)
}

type ReaderState = (CsvIndexReader<Cursor<Vec<u8>>>, usize);

fn reader_state(record: usize) -> ReaderState {
    let reader = CsvIndex::create(ROWS_1000, Cursor::new(Vec::new()), IndexOptions::default())
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    (reader, record)
}

#[library_benchmark]
#[bench::at_first(args = (0), setup = reader_state, teardown = drop_it)]
#[bench::at_middle(args = (MIDDLE), setup = reader_state, teardown = drop_it)]
#[bench::at_last(args = (LAST), setup = reader_state, teardown = drop_it)]
fn reader_seek(state: ReaderState) -> (u64, CsvIndexReader<Cursor<Vec<u8>>>) {
    let (mut reader, record) = state;
    let mut parser = reader
        .parser_at_reader(Cursor::new(ROWS_1000), record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
        .unwrap_or_else(|| panic!("benchmark input failed: record {record} missing"));
    let fields = line
        .record()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        fields.index(),
        record as u64,
        "benchmark seeked to the wrong record"
    );
    (black_box(fields.len() as u64), reader)
}

// `seek` sweeps position at one fixed corpus size to show a seek does not
// depend on where the record is. `bound_seek` and `seek_by_size` instead
// sweep corpus size at one fixed position (the last record), because the
// property they are measuring — whether a seek's cost depends on how large
// the source is — is invisible to a sweep across positions in a single,
// already-built corpus.
type OneShotSeekBySizeState = (CsvIndex, &'static [u8], usize);

fn one_shot_seek_by_size_state(input: &'static [u8]) -> OneShotSeekBySizeState {
    let index = CsvIndex::build(input, IndexOptions::default())
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    let last = input.len() / fixture::ROW_LEN - 1;
    (index, input, last)
}

// The one-shot baseline for `bound_seek`, over the same corpus sizes.
// `parser_at` calls `validate_source`, which hashes the entire source on
// every call, so this is expected to grow with corpus size where a seek
// through an already-bound source does not.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = one_shot_seek_by_size_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = one_shot_seek_by_size_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = one_shot_seek_by_size_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = one_shot_seek_by_size_state, teardown = drop_it)]
fn seek_by_size(state: OneShotSeekBySizeState) -> (u64, CsvIndex) {
    let (index, source, record) = state;
    let mut parser = index
        .parser_at(source, record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
        .unwrap_or_else(|| panic!("benchmark input failed: record {record} missing"));
    let fields = line
        .record()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        fields.index(),
        record as u64,
        "benchmark seeked to the wrong record"
    );
    (black_box(fields.len() as u64), index)
}

type BoundSeekState = (BoundSource<'static, 'static>, usize);

/// Bind a leaked, `'static` index to `input`, so the measured region in
/// [`bound_seek`] pays only for [`BoundSource::parser_at`] — never for
/// [`CsvIndex::build`] or the one hash [`CsvIndex::bind`] performs.
///
/// The index is leaked deliberately: a `BoundSource` borrows the index it is
/// bound to, and this benchmark's only use for that index is being seeked
/// through, never freed, so there is nothing wrong with never freeing it.
fn bound_seek_state(input: &'static [u8]) -> BoundSeekState {
    let index: &'static CsvIndex = Box::leak(Box::new(
        CsvIndex::build(input, IndexOptions::default())
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}")),
    ));
    let last = input.len() / fixture::ROW_LEN - 1;
    let bound = index
        .bind(input)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    (bound, last)
}

// `CsvIndex::bind` is called once in setup, outside the measured region, so
// what is measured here is exactly what repeating `BoundSource::parser_at`
// costs: a location lookup and a parser construction, never another pass over
// the source. Swept across the same corpus sizes as `seek_by_size` for a
// direct, apples-to-apples contrast between the two.
#[library_benchmark]
#[bench::rows_1(args = (ROWS_1), setup = bound_seek_state, teardown = drop_it)]
#[bench::rows_10(args = (ROWS_10), setup = bound_seek_state, teardown = drop_it)]
#[bench::rows_100(args = (ROWS_100), setup = bound_seek_state, teardown = drop_it)]
#[bench::rows_1000(args = (ROWS_1000), setup = bound_seek_state, teardown = drop_it)]
fn bound_seek(state: BoundSeekState) -> (u64, BoundSource<'static, 'static>) {
    let (bound, record) = state;
    let mut parser = bound
        .parser_at(record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    let mut line = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
        .unwrap_or_else(|| panic!("benchmark input failed: record {record} missing"));
    let fields = line
        .record()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
    assert_eq!(
        fields.index(),
        record as u64,
        "benchmark seeked to the wrong record"
    );
    (black_box(fields.len() as u64), bound)
}

library_benchmark_group!(
    name = building;
    benchmarks = build, create, generate
);

library_benchmark_group!(
    name = generate_shapes;
    benchmarks = generate_narrow, generate_wide, generate_quoted, generate_raw, generate_long
);

library_benchmark_group!(
    name = seeking;
    benchmarks = seek, reader_seek
);

library_benchmark_group!(
    name = bound_seeking;
    benchmarks = seek_by_size, bound_seek
);

main!(
    library_benchmark_groups = building,
    generate_shapes,
    seeking,
    bound_seeking
);

//! What quoting and escaping cost, which every other table in this directory
//! quietly assumes away.
//!
//! Every other suite here reads unquoted ASCII. That makes their numbers a best
//! case. This suite is what measures the unescape
//! path, the per-field assembly routines, and a field that has to be rewritten
//! into scratch before it can be handed out.
//!
//! # The four corpora
//!
//! This suite mirrors [`byte_record`](../byte_record/index.html) exactly — same
//! front ends, same record shape, same row counts — and varies only how the two
//! text columns are written:
//!
//! | Corpus     | The row                                            | Bytes |
//! |------------|----------------------------------------------------|-------|
//! | `plain`    | `Boston,Massachusetts,…`                           |    51 |
//! | `quoted`   | `"Boston","Massachusetts",…`                       |    55 |
//! | `escaped`  | `"Bo""ton","Ma""sachusetts",…`                     |    57 |
//! | `interior` | `Boston,"Massachusetts",…`                         |    53 |
//! | `dense`    | `"Bo""ton","Ma""sachusetts","45""0000",…`           |    69 |
//!
//! The four numeric columns are identical in the first four, so any difference
//! between those rows is the text columns and nothing else. `dense` quotes and
//! escapes all six.
//!
//! `dense` exists because the vectorized whole-record parser has a distinct
//! branch for decoding masked quoted fields carrying escapes, and no other
//! corpus puts more than two such fields in a record. Six escaped fields in 69
//! bytes is what makes several masked runs land inside one block, which is the
//! shape that branch exists for. It is measured on `slice` alone: the branch is
//! in the vectorized parser, and driving it once is what guards it.
//!
//! `interior` exists because the other three all put their quote in the
//! record's *first* byte, and the dispatch under measurement keys on exactly
//! that byte. A corpus that only ever quotes from the front cannot see what
//! happens when a producer quotes the one column that might contain a
//! delimiter and leaves the rest alone — which is what most real files look
//! like. It turns out to matter more than the number of quoted fields does.
//!
//! The escaped corpus is built so that unescaping it yields fields of exactly
//! the same lengths as the other two: `Bo"ton` is six bytes just as `Boston`
//! is, and `Ma"sachusetts` is thirteen just as `Massachusetts` is. All three
//! corpora therefore share one checksum, which every case asserts — as does
//! `interior`, whose fields are the plain ones with quotes around one of them.
//! A case that silently stopped unescaping, or stopped stripping quotes, would
//! fail that assertion rather than quietly report a better number.
//!
//! # Results
//!
//! Callgrind instruction counts. "Per record" is the marginal cost, taken from
//! the difference between 100 and 1000 rows. Every case here is linear to
//! within 1.4%, so those figures carry.
//!
//! | Case    | Corpus     | 1     | 10     | 100     | 1000      | Per record | vs `plain` | vs `csv` |
//! |---------|------------|-------|--------|---------|-----------|------------|------------|----------|
//! | `slice` | `plain`    | 1,136 |  8,283 |  79,050 |   787,294 |        786 |            | -27%     |
//! | `slice` | `quoted`   | 1,671 | 10,089 |  94,652 |   940,038 |        939 | +19%       | -18%     |
//! | `slice` | `interior` | 1,666 | 11,125 | 106,138 | 1,056,545 |      1,056 | +34%       | -5%      |
//! | `slice` | `escaped`  | 1,739 | 11,432 | 107,937 | 1,073,442 |      1,072 | +36%       | -15%     |
//! | `slice` | `dense`    | 1,696 | 11,821 | 113,071 | 1,125,571 |      1,125 | +43%       |          |
//! | `push`  | `plain`    | 1,839 | 12,439 |  97,080 |   944,334 |        941 |            | -12%     |
//! | `push`  | `quoted`   | 2,657 | 14,216 | 112,942 | 1,099,685 |      1,096 | +16%       | -5%      |
//! | `push`  | `interior` | 2,740 | 15,232 | 124,318 | 1,214,853 |      1,211 | +29%       | +9%      |
//! | `push`  | `escaped`  | 3,599 | 16,396 | 127,133 | 1,235,198 |      1,231 | +31%       | -2%      |
//! | `io`    | `plain`    | 1,644 | 12,513 |  99,417 |   974,498 |        972 |            | -9%      |
//! | `io`    | `quoted`   | 2,310 | 14,542 | 117,265 | 1,152,257 |      1,149 | +18%       | +0%      |
//! | `io`    | `interior` | 2,407 | 15,612 | 129,262 | 1,274,889 |      1,272 | +31%       | +15%     |
//! | `io`    | `escaped`  | 3,281 | 16,829 | 133,505 | 1,309,094 |      1,306 | +34%       | +4%      |
//! | `csv`   | `plain`    | 2,681 | 12,340 | 108,712 | 1,073,457 |      1,071 |            |          |
//! | `csv`   | `quoted`   | 2,757 | 13,112 | 116,348 | 1,149,824 |      1,148 | +7%        |          |
//! | `csv`   | `interior` | 2,719 | 12,732 | 112,536 | 1,111,616 |      1,110 | +4%        |          |
//! | `csv`   | `escaped`  | 2,865 | 14,192 | 127,172 | 1,258,000 |      1,256 | +17%       |          |
//!
//! # Quoting costs this crate about twice what it costs `csv`
//!
//! Adding quotes that need no escaping costs coseva 153 instructions per record
//! on `slice`, 155 on `push` and 177 on `io`. It costs `csv` 77. Unescaping a
//! doubled quote in each of the two text columns then costs a further 133, 135
//! and 157 against `csv`'s 108 — much closer, and the part this crate handles
//! competitively.
//!
//! So the expensive half is not the unescaping. It is that a quote costs the
//! record around it more of its fast path than it should.
//!
//! The two parsers are each better at one thing, so each does that thing. The
//! scalar parser is 46 instructions per *unquoted* field worse
//! than the kernel, because it pays a search per field where the kernel pays
//! one scan per record — routing a whole record to it because the record opens
//! with a quote costs `plain` 1,066 instructions per record against the
//! kernel's 791. So the scalar parser
//! reads the quoted head and stops at the first field that is not quoted; the
//! kernel resumes there and takes the plain tail. That is worth 9.3% on
//! `slice`, 8.1% on `push` and 8.8% on `io`, and it applies to `escaped`
//! equally since the tail after a doubled quote is just as plain.
//!
//! `interior` does not benefit, and is the worst of the three relative to
//! `plain` at +34%. A record that opens *unquoted* has no quoted head to split
//! off, so it is the one shape parsed scalar the whole way once the
//! prediction below has fired. The interior handoff described below
//! closes most of that gap without perturbing the ordinary kernel.
//!
//! # Why one interior quote is not cheaper than two leading ones
//!
//! `interior` has *half* as many quoted fields as `quoted`, so arithmetic says
//! it should be cheaper, and on `csv` it is: 1,110 against 1,148.
//!
//! The pressure is visible in `parse_owned_record`. When the first byte is a
//! quote the record goes straight to the scalar parser and the structural scan
//! is never started. When the first byte is *not* a quote, the record instead
//! enters `try_parse_owned_plain`, which stands up a `StructuralScanner`,
//! loads and masks a block, consumes the one unquoted field ahead of the quote,
//! and then bails — after which `try_parse_owned_tail` runs the scalar parser
//! over everything that is left. The fields already parsed are kept, so nothing
//! is recomputed; what is lost is that a scanner setup sized to amortise over
//! a whole record amortises over six bytes instead.
//!
//! Forcing every record onto the scalar parser prices that exactly: it puts
//! `interior` at 1,165 and `quoted` at 1,164, so 116 instructions per record
//! are recoverable and no more.
//!
//! Nothing about a record reveals in advance that it will bail, but the file
//! does, because a producer quotes the same column in every row. So the engine
//! counts down from a bail and sends the records that follow it straight to
//! the scalar parser. `interior` is 1,051 on `slice`, 1,207 on `push` and 1,267
//! on `io` — within 2%, 1% and 1% of `quoted`. Against `csv`, `slice` is 5%
//! faster and `io` 14% slower.
//!
//! The count decays rather than latching, which is what makes it correct
//! itself: at zero the next record takes the structural route regardless, and
//! that route reports the truth for free — it bails if a quote is there and
//! runs to the end if it is not. A file that quotes one record and never again
//! pays 5,778 instructions in total for the misprediction, 0.006% over 100,000
//! records.
//!
//! It costs the unquoted corpora 0.5% to 0.8%, which is the one load and one
//! branch the prediction adds per record.
//!
//! # Why the final handoff stays out of line
//!
//! The interior-prefix and resume helpers deliberately remain `inline(never)`.
//! With that shape, 1,000 `slice` rows measure 1,014,204 instructions against
//! 932,652 for the quoted-prefix corpus, an 8.75% gap, while plain stays flat.
//! Inlining closes the gap to 3.4% but adds 0.51% to plain. The gain is confined
//! to an uncommon shape while the loss lands on every ordinary record, so the
//! protected common kernel is the better global trade.
//!
//! The larger quoted-versus-plain gap is about the scalar parser itself, not
//! dispatch. A structural kernel that consumes quoted fields is slower than
//! the scalar parser across six arrangements of the idea, because the scalar
//! parser finds the next quote
//! with one `find1_near` and appends one segment, whereas a structural scan pays
//! to enumerate every delimiter and quote position in the record.
//!
//! Rows with several separated quote runs make the opposite trade. Repeatedly
//! alternating between the prefix parser and kernel costs more than reading a
//! short row scalar once, so the first such row switches the prediction's
//! function pointer to the whole-record parser. That leaves the
//! one-interior-quote corpus above untouched and is worth about 15% on the
//! spreadsheet matrix shape in the isolated buffered comparison.
//!
//! # Why the fixed costs look worse than the plain suite's
//!
//! The `1` column is larger here than in `byte_record` for the buffered front
//! ends. That is header-free startup plus one record over a corpus whose rows
//! are wider, and at one record the fixed cost dominates entirely; it is not a
//! per-record result and should not be read as one. The `startup` suite is
//! where fixed cost is measured on purpose.
//!
//! # What this does not measure
//!
//! Fields containing a delimiter or a newline inside quotes. Those change which
//! branch the scanner takes rather than how much a field costs to assemble.
//! `dialects` covers activated read policies and `encode` covers activated
//! write quoting and escaping.

#![expect(
    missing_docs,
    clippy::panic,
    reason = "benchmark macros are private and a bad fixture must fail loudly"
)]

use std::hint::black_box;
use std::io::Cursor;

use coseva::config::{Headers, ParseOptions};
use coseva::format::Csv;
use coseva::{ByteRecord, Chunk, IoParser, PushParser, SliceParser};
use gungraun::prelude::*;

#[path = "fixture.rs"]
#[expect(dead_code, reason = "this file builds its own quoted corpora")]
mod fixture;

use fixture::{BUFFER, FIELD_BYTES, FIELDS, drop_it};

// ── the three rows ───────────────────────────────────────────────────────────

/// Unquoted, and byte-for-byte the row every other suite reads.
static PLAIN_ROW: &[u8] = b"Boston,Massachusetts,4500000,42.3601,-71.0589,true\n";

/// Quoted, but with nothing inside the quotes that needs escaping.
static QUOTED_ROW: &[u8] = b"\"Boston\",\"Massachusetts\",4500000,42.3601,-71.0589,true\n";

/// Quoted, with a doubled quote in each text column that must be unescaped.
///
/// `Bo""ton` unescapes to `Bo"ton`, six bytes, exactly as long as `Boston`;
/// `Ma""sachusetts` unescapes to thirteen, exactly as long as `Massachusetts`.
/// That is what lets all three corpora share [`fixture::FIELD_BYTES`].
static ESCAPED_ROW: &[u8] = b"\"Bo\"\"ton\",\"Ma\"\"sachusetts\",4500000,42.3601,-71.0589,true\n";

/// Every column quoted and every column carrying a doubled quote, so one
/// record's quote mask holds six separate quoted runs rather than one or two.
///
/// The vectorized whole-record parser has a distinct branch for decoding masked
/// quoted fields with escapes, and `escaped` drives it with two quoted columns
/// out of six. This corpus is what makes several masked runs land inside a
/// single block, which is the shape that branch exists for.
///
/// Each field unescapes to exactly the length of the corresponding column in
/// [`PLAIN_ROW`], so this corpus checks against the same [`fixture::FIELD_BYTES`]
/// as the others and a case that skipped unescaping cannot pass.
static DENSE_ROW: &[u8] =
    b"\"Bo\"\"ton\",\"Ma\"\"sachusetts\",\"45\"\"0000\",\"42\"\"3601\",\"-71\"\"0589\",\"tr\"\"e\"\n";

/// The same record with only its *second* column quoted, so the first field is
/// unquoted and the record's first byte is not a quote.
///
/// This is the shape real files most often have — a producer quotes the columns
/// that might contain a delimiter and leaves the rest alone — and it is the
/// shape the leading-quote corpora cannot reach, because the dispatch in
/// `parse_owned_record` keys on the first byte of the record.
static INTERIOR_ROW: &[u8] = b"Boston,\"Massachusetts\",4500000,42.3601,-71.0589,true\n";

/// Build a corpus of whole copies of `row` at compile time.
///
/// `N` must be an exact multiple of the row's length, which the statics below
/// guarantee by construction.
const fn corpus<const N: usize>(row: &[u8]) -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    while index < N {
        out[index] = row[index % row.len()];
        index += 1;
    }
    out
}

/// The row shapes a `mixed` corpus cycles through, one of each in this order.
///
/// A full cycle is four records — plain, quoted, interior, escaped — and so
/// `MIXED_CYCLE_BYTES` bytes; a mixed corpus is a whole number of these.
static MIXED_ROWS: [&[u8]; 4] = [PLAIN_ROW, QUOTED_ROW, INTERIOR_ROW, ESCAPED_ROW];

/// Bytes in one full cycle of [`MIXED_ROWS`], i.e. one of each row shape.
const MIXED_CYCLE_BYTES: usize = PLAIN_LEN + QUOTED_LEN + INTERIOR_LEN + ESCAPED_LEN;

/// Build a corpus that interleaves the four row shapes in [`MIXED_ROWS`] order.
///
/// Every real file quotes some rows and not others, so `interior` and `quoted`
/// on their own never exercise the prediction counter arming and decaying
/// against a moving mix of shapes. This one does, and because all four corpora
/// yield the same field lengths it still checks against a single [`FIELD_BYTES`]
/// expectation. `N` must be an exact multiple of [`MIXED_CYCLE_BYTES`] so the
/// corpus ends on a record boundary, which the statics below guarantee.
const fn mixed_corpus<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut index = 0;
    let mut row = 0;
    let mut col = 0;
    while index < N {
        let current = MIXED_ROWS[row];
        out[index] = current[col];
        index += 1;
        col += 1;
        if col == current.len() {
            col = 0;
            row += 1;
            if row == MIXED_ROWS.len() {
                row = 0;
            }
        }
    }
    out
}

const PLAIN_LEN: usize = 51;
const QUOTED_LEN: usize = 55;
const ESCAPED_LEN: usize = 57;
const INTERIOR_LEN: usize = 53;
const DENSE_LEN: usize = 69;

static PLAIN_1: [u8; PLAIN_LEN] = corpus(PLAIN_ROW);
static PLAIN_10: [u8; PLAIN_LEN * 10] = corpus(PLAIN_ROW);
static PLAIN_100: [u8; PLAIN_LEN * 100] = corpus(PLAIN_ROW);
static PLAIN_1000: [u8; PLAIN_LEN * 1000] = corpus(PLAIN_ROW);

static QUOTED_1: [u8; QUOTED_LEN] = corpus(QUOTED_ROW);
static QUOTED_10: [u8; QUOTED_LEN * 10] = corpus(QUOTED_ROW);
static QUOTED_100: [u8; QUOTED_LEN * 100] = corpus(QUOTED_ROW);
static QUOTED_1000: [u8; QUOTED_LEN * 1000] = corpus(QUOTED_ROW);

static ESCAPED_1: [u8; ESCAPED_LEN] = corpus(ESCAPED_ROW);
static ESCAPED_10: [u8; ESCAPED_LEN * 10] = corpus(ESCAPED_ROW);
static ESCAPED_100: [u8; ESCAPED_LEN * 100] = corpus(ESCAPED_ROW);
static ESCAPED_1000: [u8; ESCAPED_LEN * 1000] = corpus(ESCAPED_ROW);

static DENSE_1: [u8; DENSE_LEN] = corpus(DENSE_ROW);
static DENSE_10: [u8; DENSE_LEN * 10] = corpus(DENSE_ROW);
static DENSE_100: [u8; DENSE_LEN * 100] = corpus(DENSE_ROW);
static DENSE_1000: [u8; DENSE_LEN * 1000] = corpus(DENSE_ROW);

static INTERIOR_1: [u8; INTERIOR_LEN] = corpus(INTERIOR_ROW);
static INTERIOR_10: [u8; INTERIOR_LEN * 10] = corpus(INTERIOR_ROW);
static INTERIOR_100: [u8; INTERIOR_LEN * 100] = corpus(INTERIOR_ROW);
static INTERIOR_1000: [u8; INTERIOR_LEN * 1000] = corpus(INTERIOR_ROW);

// Mixed corpora hold whole cycles of `MIXED_ROWS`, four records to a cycle, so
// every size below is a clean multiple of `MIXED_CYCLE_BYTES` and its record
// count is four times the cycle count.
static MIXED_4: [u8; MIXED_CYCLE_BYTES] = mixed_corpus();
static MIXED_40: [u8; MIXED_CYCLE_BYTES * 10] = mixed_corpus();
static MIXED_100: [u8; MIXED_CYCLE_BYTES * 25] = mixed_corpus();
static MIXED_1000: [u8; MIXED_CYCLE_BYTES * 250] = mixed_corpus();

/// A corpus and the number of records in it, since the row width now varies.
type Input = (&'static [u8], usize);

static P1: Input = (&PLAIN_1, 1);
static P10: Input = (&PLAIN_10, 10);
static P100: Input = (&PLAIN_100, 100);
static P1000: Input = (&PLAIN_1000, 1000);

static Q1: Input = (&QUOTED_1, 1);
static Q10: Input = (&QUOTED_10, 10);
static Q100: Input = (&QUOTED_100, 100);
static Q1000: Input = (&QUOTED_1000, 1000);

static E1: Input = (&ESCAPED_1, 1);
static E10: Input = (&ESCAPED_10, 10);
static E100: Input = (&ESCAPED_100, 100);
static E1000: Input = (&ESCAPED_1000, 1000);

static D1: Input = (&DENSE_1, 1);
static D10: Input = (&DENSE_10, 10);
static D100: Input = (&DENSE_100, 100);
static D1000: Input = (&DENSE_1000, 1000);

static I1: Input = (&INTERIOR_1, 1);
static I10: Input = (&INTERIOR_10, 10);
static I100: Input = (&INTERIOR_100, 100);
static I1000: Input = (&INTERIOR_1000, 1000);

static M4: Input = (&MIXED_4, 4);
static M40: Input = (&MIXED_40, 40);
static M100: Input = (&MIXED_100, 100);
static M1000: Input = (&MIXED_1000, 1000);

/// Assert the case saw every field of every record, fully unescaped.
///
/// All three corpora yield the same field lengths, so one expectation covers
/// them all and a case that skipped unescaping cannot pass.
fn check(total: u64, rows: usize) -> u64 {
    let expected = rows as u64 * FIELD_BYTES;
    assert_eq!(total, expected, "benchmark parsed the wrong fields");
    total
}

// ── setup: everything that is not per-record work ────────────────────────────

fn options() -> ParseOptions {
    ParseOptions::new()
        .headers(Headers::None)
        .buffer_capacity(BUFFER)
}

/// A record buffer wide enough for the corpus, so no case grows one while
/// being measured.
fn record() -> ByteRecord {
    ByteRecord::with_capacity(FIELDS, DENSE_LEN)
}

type SliceState = (SliceParser<'static, Csv>, ByteRecord, usize);
type IoState = (IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord, usize);
type PushState = (PushParser<Csv>, ByteRecord, &'static [u8], usize);
type CsvState = (
    ::csv::Reader<Cursor<&'static [u8]>>,
    ::csv::ByteRecord,
    usize,
);

fn slice_state(input: Input) -> SliceState {
    let (bytes, rows) = input;
    let parser = SliceParser::<Csv>::new(bytes, options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), rows)
}

fn io_state(input: Input) -> IoState {
    let (bytes, rows) = input;
    let parser = IoParser::<_, Csv>::new(Cursor::new(bytes), options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), rows)
}

fn push_state(input: Input) -> PushState {
    let (bytes, rows) = input;
    let parser = PushParser::<Csv>::new(options())
        .unwrap_or_else(|error| panic!("invalid benchmark config: {error}"));
    (parser, record(), bytes, rows)
}

// The generated benchmark module below takes the `csv` name, so the crate
// itself is reached through an absolute path everywhere in this file.
fn csv_state(input: Input) -> CsvState {
    let (bytes, rows) = input;
    let reader = ::csv::ReaderBuilder::new()
        .has_headers(false)
        .buffer_capacity(BUFFER)
        .from_reader(Cursor::new(bytes));
    (
        reader,
        ::csv::ByteRecord::with_capacity(ESCAPED_LEN, FIELDS),
        rows,
    )
}

// ── the measured bodies ──────────────────────────────────────────────────────

fn sum(record: &ByteRecord) -> u64 {
    let mut total = 0_u64;
    for index in 0..record.len() {
        total = total.wrapping_add(record.get(index).map_or(0, <[u8]>::len) as u64);
    }
    total
}

#[library_benchmark]
#[bench::plain_1(args = (P1), setup = slice_state, teardown = drop_it)]
#[bench::plain_10(args = (P10), setup = slice_state, teardown = drop_it)]
#[bench::plain_100(args = (P100), setup = slice_state, teardown = drop_it)]
#[bench::plain_1000(args = (P1000), setup = slice_state, teardown = drop_it)]
#[bench::quoted_1(args = (Q1), setup = slice_state, teardown = drop_it)]
#[bench::quoted_10(args = (Q10), setup = slice_state, teardown = drop_it)]
#[bench::quoted_100(args = (Q100), setup = slice_state, teardown = drop_it)]
#[bench::quoted_1000(args = (Q1000), setup = slice_state, teardown = drop_it)]
#[bench::escaped_1(args = (E1), setup = slice_state, teardown = drop_it)]
#[bench::escaped_10(args = (E10), setup = slice_state, teardown = drop_it)]
#[bench::escaped_100(args = (E100), setup = slice_state, teardown = drop_it)]
#[bench::escaped_1000(args = (E1000), setup = slice_state, teardown = drop_it)]
#[bench::interior_1(args = (I1), setup = slice_state, teardown = drop_it)]
#[bench::interior_10(args = (I10), setup = slice_state, teardown = drop_it)]
#[bench::interior_100(args = (I100), setup = slice_state, teardown = drop_it)]
#[bench::interior_1000(args = (I1000), setup = slice_state, teardown = drop_it)]
#[bench::dense_1(args = (D1), setup = slice_state, teardown = drop_it)]
#[bench::dense_10(args = (D10), setup = slice_state, teardown = drop_it)]
#[bench::dense_100(args = (D100), setup = slice_state, teardown = drop_it)]
#[bench::dense_1000(args = (D1000), setup = slice_state, teardown = drop_it)]
#[bench::mixed_4(args = (M4), setup = slice_state, teardown = drop_it)]
#[bench::mixed_40(args = (M40), setup = slice_state, teardown = drop_it)]
#[bench::mixed_100(args = (M100), setup = slice_state, teardown = drop_it)]
#[bench::mixed_1000(args = (M1000), setup = slice_state, teardown = drop_it)]
fn slice(state: SliceState) -> (u64, SliceParser<'static, Csv>, ByteRecord) {
    let (mut parser, mut record, rows) = state;
    let mut total = 0_u64;
    while let Some(mut line) = parser
        .next_line()
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        line.read_byte_record_into(&mut record)
            .unwrap_or_else(|error| panic!("benchmark input failed: {error}"));
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, rows)), parser, record)
}

#[library_benchmark]
#[bench::plain_1(args = (P1), setup = io_state, teardown = drop_it)]
#[bench::plain_10(args = (P10), setup = io_state, teardown = drop_it)]
#[bench::plain_100(args = (P100), setup = io_state, teardown = drop_it)]
#[bench::plain_1000(args = (P1000), setup = io_state, teardown = drop_it)]
#[bench::quoted_1(args = (Q1), setup = io_state, teardown = drop_it)]
#[bench::quoted_10(args = (Q10), setup = io_state, teardown = drop_it)]
#[bench::quoted_100(args = (Q100), setup = io_state, teardown = drop_it)]
#[bench::quoted_1000(args = (Q1000), setup = io_state, teardown = drop_it)]
#[bench::escaped_1(args = (E1), setup = io_state, teardown = drop_it)]
#[bench::escaped_10(args = (E10), setup = io_state, teardown = drop_it)]
#[bench::escaped_100(args = (E100), setup = io_state, teardown = drop_it)]
#[bench::escaped_1000(args = (E1000), setup = io_state, teardown = drop_it)]
#[bench::interior_1(args = (I1), setup = io_state, teardown = drop_it)]
#[bench::interior_10(args = (I10), setup = io_state, teardown = drop_it)]
#[bench::interior_100(args = (I100), setup = io_state, teardown = drop_it)]
#[bench::interior_1000(args = (I1000), setup = io_state, teardown = drop_it)]
fn io(state: IoState) -> (u64, IoParser<Cursor<&'static [u8]>, Csv>, ByteRecord) {
    let (mut parser, mut record, rows) = state;
    let mut total = 0_u64;
    while parser
        .read_byte_record_into(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        total = total.wrapping_add(sum(&record));
    }
    (black_box(check(total, rows)), parser, record)
}

// `finish` is inside the measured region because it is what tells the parser
// the unterminated tail is complete; without it the final record of a stream
// never arrives, so it is per-record work rather than teardown.
#[library_benchmark]
#[bench::plain_1(args = (P1), setup = push_state, teardown = drop_it)]
#[bench::plain_10(args = (P10), setup = push_state, teardown = drop_it)]
#[bench::plain_100(args = (P100), setup = push_state, teardown = drop_it)]
#[bench::plain_1000(args = (P1000), setup = push_state, teardown = drop_it)]
#[bench::quoted_1(args = (Q1), setup = push_state, teardown = drop_it)]
#[bench::quoted_10(args = (Q10), setup = push_state, teardown = drop_it)]
#[bench::quoted_100(args = (Q100), setup = push_state, teardown = drop_it)]
#[bench::quoted_1000(args = (Q1000), setup = push_state, teardown = drop_it)]
#[bench::escaped_1(args = (E1), setup = push_state, teardown = drop_it)]
#[bench::escaped_10(args = (E10), setup = push_state, teardown = drop_it)]
#[bench::escaped_100(args = (E100), setup = push_state, teardown = drop_it)]
#[bench::escaped_1000(args = (E1000), setup = push_state, teardown = drop_it)]
#[bench::interior_1(args = (I1), setup = push_state, teardown = drop_it)]
#[bench::interior_10(args = (I10), setup = push_state, teardown = drop_it)]
#[bench::interior_100(args = (I100), setup = push_state, teardown = drop_it)]
#[bench::interior_1000(args = (I1000), setup = push_state, teardown = drop_it)]
fn push(state: PushState) -> (u64, PushParser<Csv>, ByteRecord) {
    let (mut parser, mut record, input, rows) = state;
    let mut total = 0_u64;
    let mut fed = 0;
    while fed < input.len() {
        let end = fed.saturating_add(BUFFER).min(input.len());
        let mut chunk = parser.chunk(&input[fed..end]);
        total = total.wrapping_add(drain(&mut chunk, &mut record));
        fed += chunk.done();
    }
    parser.finish();
    let mut chunk = parser.chunk(&[]);
    total = total.wrapping_add(drain(&mut chunk, &mut record));
    let _ = chunk.done();
    (black_box(check(total, rows)), parser, record)
}

fn drain(chunk: &mut Chunk<'_, '_, Csv>, record: &mut ByteRecord) -> u64 {
    let mut total = 0_u64;
    while chunk
        .read_byte_record_into(record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        total = total.wrapping_add(sum(record));
    }
    total
}

#[library_benchmark]
#[bench::plain_1(args = (P1), setup = csv_state, teardown = drop_it)]
#[bench::plain_10(args = (P10), setup = csv_state, teardown = drop_it)]
#[bench::plain_100(args = (P100), setup = csv_state, teardown = drop_it)]
#[bench::plain_1000(args = (P1000), setup = csv_state, teardown = drop_it)]
#[bench::quoted_1(args = (Q1), setup = csv_state, teardown = drop_it)]
#[bench::quoted_10(args = (Q10), setup = csv_state, teardown = drop_it)]
#[bench::quoted_100(args = (Q100), setup = csv_state, teardown = drop_it)]
#[bench::quoted_1000(args = (Q1000), setup = csv_state, teardown = drop_it)]
#[bench::escaped_1(args = (E1), setup = csv_state, teardown = drop_it)]
#[bench::escaped_10(args = (E10), setup = csv_state, teardown = drop_it)]
#[bench::escaped_100(args = (E100), setup = csv_state, teardown = drop_it)]
#[bench::escaped_1000(args = (E1000), setup = csv_state, teardown = drop_it)]
#[bench::interior_1(args = (I1), setup = csv_state, teardown = drop_it)]
#[bench::interior_10(args = (I10), setup = csv_state, teardown = drop_it)]
#[bench::interior_100(args = (I100), setup = csv_state, teardown = drop_it)]
#[bench::interior_1000(args = (I1000), setup = csv_state, teardown = drop_it)]
fn csv(state: CsvState) -> (u64, ::csv::Reader<Cursor<&'static [u8]>>, ::csv::ByteRecord) {
    let (mut reader, mut record, rows) = state;
    let mut total = 0_u64;
    while reader
        .read_byte_record(&mut record)
        .unwrap_or_else(|error| panic!("benchmark input failed: {error}"))
    {
        for field in &record {
            total = total.wrapping_add(field.len() as u64);
        }
    }
    (black_box(check(total, rows)), reader, record)
}

library_benchmark_group!(
    name = quoted;
    benchmarks = slice, io, push, csv
);

main!(library_benchmark_groups = quoted);

//! Documents that look like files people actually have, for the customer matrix.
//!
//! The other corpora in this directory are deliberately degenerate: one row
//! repeated, uniform width, ASCII, unquoted. That is the right shape for a
//! regression measurement, because holding everything constant is what makes a
//! 2% delta mean something. It is the wrong shape for telling somebody what
//! this crate will cost them, because no file looks like that.
//!
//! # These are generated, not real
//!
//! Nothing here was scraped from anywhere, and none of it is claimed to be a
//! real document. Each is built by the deterministic generator below from a
//! fixed seed, so the bytes are identical on every machine and in every run,
//! and the thing that produced them is checked in and reviewable. That is worth
//! more for a benchmark than genuine data would be: real files cannot be
//! redistributed without a license, cannot be regenerated if lost, and cannot
//! be adjusted when a suite needs a shape they do not have.
//!
//! What is real is the *shapes* — the column counts, the quoting habits, the
//! line endings and the field-width distributions are drawn from what CSV
//! producers actually emit. Read the numbers as "a file like this costs about
//! this much", not as a measurement of any particular document.
//!
//! # The five documents
//!
//! Each stresses something the others do not:
//!
//! | Document      | Shape                                          | What it stresses          |
//! |---------------|------------------------------------------------|---------------------------|
//! | `metrics`     | 5 narrow numeric columns                       | per-record overhead       |
//! | `wide`        | 128 columns, mostly short                      | per-column cost           |
//! | `quoted`      | text with embedded delimiters and newlines     | the quoted path           |
//! | `prose`       | a long free-text column beside short ones      | large-field copying       |
//! | `spreadsheet` | CRLF endings, a UTF-8 BOM, quoted-everything   | what a spreadsheet emits  |
//!
//! Every document begins with the same two columns, `name` and `value`, so the
//! typed rows of the matrix decode the same schema from all five. Their
//! differences are then the cost of the document rather than the cost of a
//! different struct.
//!
//! # The size budget
//!
//! [`BUDGET_BYTES`] is 256 KiB per document, asserted at construction. A corpus
//! is easy to let grow into tens of megabytes that every clone pays for
//! forever, so the budget is stated here for a later addition to argue against
//! rather than left implicit.
//!
//! Nothing is checked in as data — the documents are built in `setup`, outside
//! the measured region, so they cost the repository nothing at all and the
//! budget bounds memory rather than disk. 256 KiB is comfortably enough to
//! amortize startup: the smallest document here is still thousands of records.

// This module has two consumers -- `benches/matrix.rs` and
// `examples/document_dimensions.rs` -- which `#[path]`-include it, and each
// compiles it separately while using a different part of it. Every item below
// is reachable from one of them, but neither sees all of it, so the unused and
// unreachable-pub warnings here are artefacts of the include and not dead code.
#![allow(
    dead_code,
    unreachable_pub,
    reason = "each #[path] consumer of this module uses a different subset"
)]
use std::sync::LazyLock;

/// The most any one document may occupy, in bytes.
///
/// Asserted rather than documented, so a generator change that blows the budget
/// fails the benchmark instead of quietly making every clone slower.
pub const BUDGET_BYTES: usize = 256 * 1024;

/// A deterministic generator, so every run sees byte-identical documents.
///
/// xorshift64*, chosen because it is four lines and its quality is irrelevant
/// here — the requirement is reproducibility, not randomness.
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..bound`, biased but deterministic, which is all that is
    /// needed to give fields a spread of widths.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the modulus is below `bound`, which is already a usize"
    )]
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// Word stock for the text columns, so fields vary in width the way real ones do.
const WORDS: &[&str] = &[
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliett",
    "kilo", "lima", "mike", "november", "oscar", "papa", "quebec", "romeo", "sierra", "tango",
];

const CITIES: &[&str] = &[
    "Boston",
    "San Francisco",
    "Rio de Janeiro",
    "Ulaanbaatar",
    "Lyon",
    "Cape Town",
    "Wellington",
    "Reykjavik",
];

/// Build `count` words joined by spaces.
fn phrase(rng: &mut Rng, count: usize) -> String {
    let mut out = String::new();
    for index in 0..count {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(rng.pick(WORDS));
    }
    out
}

/// A document and the facts a benchmark needs to assert it read the whole thing.
pub struct Document {
    /// What the suite calls this shape.
    pub name: &'static str,
    /// The bytes, which are identical on every machine.
    pub bytes: Vec<u8>,
    /// How many data records follow the header.
    pub records: usize,
    /// The total length of every field of every record, after unquoting.
    ///
    /// This is the checksum every case in the matrix asserts. It is accumulated
    /// by the generator as it writes, rather than recomputed by parsing, so a
    /// parser bug cannot agree with it by construction.
    pub field_bytes: u64,
    /// The sum of the `value` column, which the typed rows assert instead.
    pub value_sum: u64,
}

/// Accumulates fields into a record, tracking the decoded length as it goes.
struct Builder {
    out: Vec<u8>,
    field_bytes: u64,
    fields_in_record: usize,
    crlf: bool,
}

impl Builder {
    fn new(crlf: bool) -> Self {
        Self {
            out: Vec::with_capacity(BUDGET_BYTES),
            field_bytes: 0,
            fields_in_record: 0,
            crlf,
        }
    }

    /// Append a field exactly as written, counting its decoded length.
    ///
    /// `raw` is what goes on the wire and `decoded` is what a parser should
    /// hand back; they differ only where the field is quoted or escaped.
    fn field(&mut self, raw: &str, decoded_len: usize) {
        if self.fields_in_record > 0 {
            self.out.push(b',');
        }
        self.out.extend_from_slice(raw.as_bytes());
        self.field_bytes += decoded_len as u64;
        self.fields_in_record += 1;
    }

    /// A field needing no quoting, where the wire form is the decoded form.
    fn plain(&mut self, text: &str) {
        self.field(text, text.len());
    }

    /// A field wrapped in quotes, with any interior quote doubled.
    fn quoted(&mut self, text: &str) {
        let escaped = text.replace('"', "\"\"");
        let raw = format!("\"{escaped}\"");
        self.field(&raw, text.len());
    }

    fn end_record(&mut self) {
        if self.crlf {
            self.out.push(b'\r');
        }
        self.out.push(b'\n');
        self.fields_in_record = 0;
    }

    /// The header row is written but not counted, since no case reads it as data.
    fn end_header(&mut self) {
        self.end_record();
        self.field_bytes = 0;
    }

    fn room_for_another(&self) -> bool {
        // The margin is generous enough that the widest record this file can
        // produce still fits, so no document ends mid-record.
        self.out.len() + 8 * 1024 < BUDGET_BYTES
    }
}

fn finish(name: &'static str, builder: Builder, records: usize, value_sum: u64) -> Document {
    assert!(
        builder.out.len() <= BUDGET_BYTES,
        "document `{name}` is {} bytes, over the {BUDGET_BYTES}-byte budget",
        builder.out.len()
    );
    Document {
        name,
        bytes: builder.out,
        records,
        field_bytes: builder.field_bytes,
        value_sum,
    }
}

/// Narrow and numeric, the shape of a metrics export.
///
/// Five short columns, no quoting, no text worth speaking of. Per-record
/// overhead dominates because there is almost nothing else to pay for, which
/// makes this the document that punishes a slow record boundary.
fn metrics() -> Document {
    let mut rng = Rng::new(0x5EED_0001);
    let mut builder = Builder::new(false);
    for column in ["name", "value", "count", "ratio", "flag"] {
        builder.plain(column);
    }
    builder.end_header();

    let mut records = 0;
    let mut value_sum = 0;
    while builder.room_for_another() {
        let value = rng.below(1_000_000) as u64;
        builder.plain(WORDS[records % WORDS.len()]);
        builder.plain(&value.to_string());
        builder.plain(&rng.below(10_000).to_string());
        builder.plain(&format!("{}.{:04}", rng.below(100), rng.below(10_000)));
        builder.plain(if rng.below(2) == 0 { "true" } else { "false" });
        builder.end_record();
        value_sum += value;
        records += 1;
    }
    finish("metrics", builder, records, value_sum)
}

/// A hundred and twenty-eight columns, the shape of an analytics extract.
///
/// Every column is short, so per-column cost dominates: the work is in field
/// boundaries and record assembly rather than in copying bytes. This is the
/// document where a per-field constant shows up magnified.
fn wide() -> Document {
    let mut rng = Rng::new(0x5EED_0002);
    let mut builder = Builder::new(false);
    builder.plain("name");
    builder.plain("value");
    for column in 2..128 {
        builder.plain(&format!("c{column}"));
    }
    builder.end_header();

    let mut records = 0;
    let mut value_sum = 0;
    while builder.room_for_another() {
        let value = rng.below(100_000) as u64;
        builder.plain(WORDS[records % WORDS.len()]);
        builder.plain(&value.to_string());
        for _ in 2..128 {
            builder.plain(&rng.below(1000).to_string());
        }
        builder.end_record();
        value_sum += value;
        records += 1;
    }
    finish("wide", builder, records, value_sum)
}

/// Text with embedded delimiters and newlines, which is why quoting exists.
///
/// Roughly half the text fields carry a comma, a quote or a line break, so they
/// must be quoted and some must be unescaped. A record with an embedded newline
/// also spans more than one physical line, which is the case that forces the
/// general path.
fn quoted() -> Document {
    let mut rng = Rng::new(0x5EED_0003);
    let mut builder = Builder::new(false);
    for column in ["name", "value", "city", "note"] {
        builder.plain(column);
    }
    builder.end_header();

    let mut records = 0;
    let mut value_sum = 0;
    while builder.room_for_another() {
        let value = rng.below(1_000_000) as u64;
        builder.plain(WORDS[records % WORDS.len()]);
        builder.plain(&value.to_string());

        // A city, comma-qualified often enough that quoting is the common case.
        let city = rng.pick(CITIES);
        match rng.below(4) {
            0 => builder.plain(city),
            1 => builder.quoted(&format!("{city}, region {}", rng.below(50))),
            2 => builder.quoted(&format!("{city} \"central\"")),
            _ => builder.quoted(city),
        }

        // A note, sometimes spanning two physical lines.
        let words = 2 + rng.below(6);
        let note = phrase(&mut rng, words);
        if rng.below(5) == 0 {
            builder.quoted(&format!("{note}\ncontinued"));
        } else {
            builder.quoted(&note);
        }

        builder.end_record();
        value_sum += value;
        records += 1;
    }
    finish("quoted", builder, records, value_sum)
}

/// One long free-text column beside short ones, the shape of an export with
/// descriptions in it.
///
/// The free-text field runs to a few hundred bytes, so copying dominates and
/// the per-field constants that `wide` magnifies are invisible here. Between
/// them the two documents bracket where a parser's time actually goes.
fn prose() -> Document {
    let mut rng = Rng::new(0x5EED_0004);
    let mut builder = Builder::new(false);
    for column in ["name", "value", "summary"] {
        builder.plain(column);
    }
    builder.end_header();

    let mut records = 0;
    let mut value_sum = 0;
    while builder.room_for_another() {
        let value = rng.below(1_000_000) as u64;
        builder.plain(WORDS[records % WORDS.len()]);
        builder.plain(&value.to_string());
        let words = 40 + rng.below(40);
        builder.quoted(&phrase(&mut rng, words));
        builder.end_record();
        value_sum += value;
        records += 1;
    }
    finish("prose", builder, records, value_sum)
}

/// CRLF endings, a UTF-8 BOM and quotes around every text field.
///
/// This is what a spreadsheet writes, and it is the document most likely to be
/// handed to a parser in practice. It is here because all three of those
/// properties are handled off the fast path, and a suite that never sees them
/// reports a best case that the most common producer never hits.
fn spreadsheet() -> Document {
    let mut rng = Rng::new(0x5EED_0005);
    let mut builder = Builder::new(true);
    builder.out.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    // The header is written unquoted so the BOM sits at the start of an
    // ordinary field. A quoted first header would put three stray bytes ahead
    // of the opening quote, which is a malformed field rather than a BOM, and
    // would be measuring an error path instead of a spreadsheet.
    // `id` leads so the BOM lands on a column the typed rows do not name. A BOM
    // is not stripped by this crate, so whichever column comes first has three
    // extra bytes in its header; putting `name` there would stop it resolving
    // and would make this document measure a lookup failure.
    for column in ["id", "name", "value", "city", "active"] {
        builder.plain(column);
    }
    builder.end_header();

    let mut records = 0;
    let mut value_sum = 0;
    while builder.room_for_another() {
        let value = rng.below(1_000_000) as u64;
        builder.plain(&records.to_string());
        builder.quoted(WORDS[records % WORDS.len()]);
        builder.plain(&value.to_string());
        builder.quoted(rng.pick(CITIES));
        builder.quoted(if rng.below(2) == 0 { "TRUE" } else { "FALSE" });
        builder.end_record();
        value_sum += value;
        records += 1;
    }
    finish("spreadsheet", builder, records, value_sum)
}

/// Every document, built once per process and shared by every case.
///
/// Construction is deliberately outside any measured region: benchmarks take a
/// `&'static Document` from `setup`, so nothing here is counted.
pub static DOCUMENTS: LazyLock<[Document; 5]> =
    LazyLock::new(|| [metrics(), wide(), quoted(), prose(), spreadsheet()]);

/// Look a document up by the name the suite publishes it under.
///
/// # Panics
///
/// If no document goes by that name. Every caller passes a literal, so this is
/// a typo in a benchmark rather than anything a run can encounter.
#[expect(
    clippy::panic,
    reason = "a benchmark naming a document that does not exist should not measure something else instead"
)]
pub fn document(name: &str) -> &'static Document {
    DOCUMENTS
        .iter()
        .find(|document| document.name == name)
        .unwrap_or_else(|| panic!("no document named `{name}`"))
}

/// Assert a case read every field of every record.
///
/// Taking the document back means a case cannot pass by checking against a
/// number it computed itself, and the expected value comes from the generator
/// rather than from a parse, so both sides of a comparison are held to a figure
/// neither of them produced.
pub fn check(total: u64, document: &Document) -> u64 {
    assert_eq!(
        total, document.field_bytes,
        "`{}` case read the wrong fields",
        document.name
    );
    total
}

/// Assert a typed case decoded the `value` column of every record.
pub fn check_values(total: u64, document: &Document) -> u64 {
    assert_eq!(
        total, document.value_sum,
        "`{}` typed case decoded the wrong values",
        document.name
    );
    total
}

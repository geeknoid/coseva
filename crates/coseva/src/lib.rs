#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(not(any(feature = "std", test)), no_std)]
#![forbid(unsafe_code)]

//! A fast, strict, low-allocation CSV reader and writer.
//!
//! `coseva` reads and writes CSV and the many formats shaped like it — TSV,
//! semicolon-separated European exports, Postgres `COPY`, MySQL-style text dumps,
//! Excel's CRLF-and-BOM flavor. It is built for the case where the file is
//! large and the loop around it is hot: fields are handed to you as slices of
//! the input wherever the format allows, and steady-state reading does not
//! allocate.
//!
//! ```
//! use coseva::format::Csv;
//! use coseva::config::ParseOptions;
//! use coseva::SliceParser;
//!
//! let mut parser = SliceParser::<Csv>::new(b"city,population\nBoston,650706\n", ParseOptions::new())?;
//!
//! // The first record is taken as headers by default.
//! assert_eq!(parser.header_index("population")?, Some(1));
//!
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected one record"))?;
//! let record = line.record()?;
//! assert_eq!(record.get_str(0)?, Some("Boston"));
//! assert_eq!(record.parse::<u64>(1)?, Some(650_706));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Choosing a reader
//!
//! There are three readers. They differ only in **who owns the read loop** —
//! the API you use once you have a record is the same for all three, so
//! switching between them is a change of constructor and nothing else.
//!
//! | Reader | Who drives the loop | Use it when |
//! |---|---|---|
//! | [`SliceParser`] | You do | The whole document is already one `&[u8]` — mapped, embedded, or read in full |
//! | [`IoParser`] | You do, but it fetches | You have a file, socket, or any [`Read`] and want it to handle refills |
//! | [`PushParser`] | Something else does | An async runtime, an FFI callback, or a decompressor hands you blocks |
//!
//! [`SliceParser`] is the fastest of the three because there is no
//! intermediate buffer: every field is a slice of the document you passed in,
//! and nothing is copied or refilled. Prefer it whenever the whole input
//! genuinely fits in memory.
//!
//! [`IoParser`] reads through a fixed-size window that it refills as
//! you advance, so memory stays bounded no matter how large the file is. A
//! record that straddles a refill is stitched into parser-owned storage, which
//! is why fields borrow from *the parser* rather than from your input. Peak
//! memory is the window plus the largest single record.
//!
//! [`PushParser`] inverts control. You cannot call it in a loop; instead you
//! lend it whatever bytes you have with `chunk` and it hands back the records
//! those bytes completed, holding only a partial trailing record until the
//! next chunk.
//! Records lying wholly inside a chunk are borrowed from it directly, so the
//! copying is limited to the straddling record. This is the one to reach for
//! when the source is `async`, or lives behind a callback you do not control,
//! since neither can be expressed as a blocking [`Read`].
//!
//! [`SliceParser`] and [`PushParser`] work without `std`; [`IoParser`]
//! needs it, because [`Read`] does.
//!
//! [`next_line`]: SliceParser::next_line
//! [`Read`]: std::io::Read
//!
//! # Getting data out of a record
//!
//! [`next_line`] does not parse anything — it only finds where the record
//! ends. What that record *becomes* is decided per record, on the [`Line`],
//! which means one loop can take the cheap path for most records and the
//! expensive one only where it matters.
//!
//! | From a `Line` | You get | Lifetime | Allocates |
//! |---|---|---|---|
//! | [`record`](Line::record) | A borrowed [`Record`] | Tied to the line | No, unless a field is escaped |
//! | [`read_byte_record_into`](Line::read_byte_record_into) | A reusable owned [`ByteRecord`] | Yours to keep | Only to grow the buffer |
//! | [`read_text_record_into`](Line::read_text_record_into) | A reusable owned [`TextRecord`] | Yours to keep | Only to grow the buffer |
//! | [`decoded`](Line::decoded) | Your own struct, via [`encoding::CsvDecode`] | Your choice | Only what your fields need |
//! | [`deserialized`](Line::deserialized) | Your own type, via Serde | Your choice | Only what your fields need |
//!
//! ## Borrowed or owned
//!
//! [`Record`] is the zero-copy form and the fastest one. Its fields point
//! straight into the parser's bytes, so producing it costs nothing beyond
//! recording where each field starts and stops. The catch is the lifetime: a
//! `Record` borrows the line, so the parser cannot advance while you hold one.
//! Read what you need from it — [`get`](Record::get),
//! [`get_str`](Record::get_str), [`parse`](Record::parse) — and move on. This
//! is the right default for a filter, an aggregation, or anything that
//! consumes a record and does not keep it.
//!
//! [`ByteRecord`] and [`TextRecord`] are the owned forms, for when a record
//! must outlive the line — collecting into a `Vec`, handing one to another
//! thread, keeping a running best-so-far. They copy, but they are built to be
//! *reused*: declare one outside the loop, fill it with
//! [`read_byte_record_into`](Line::read_byte_record_into) on each iteration,
//! and after the first few records the buffer has grown to fit and steady-state
//! reading stops allocating entirely. Constructing a fresh one per record
//! instead gives back most of what the crate is for.
//!
//! An [`IoParser`] loop that always wants an owned record can call
//! [`IoParser::read_byte_record_into`] or [`IoParser::read_text_record_into`]
//! directly. That fuses advancement and parsing into the caller's reusable
//! record instead of staging an owned record while waiting to learn which
//! [`Line`] view will be requested.
//! [`Chunk::read_byte_record_into`] and [`Chunk::read_text_record_into`] provide
//! the same fused operation for an incremental [`PushParser`] stream; a
//! `false` result pauses until another chunk arrives or `finish` closes the
//! stream.
//!
//! The two differ only in whether text is validated. [`ByteRecord`] holds raw
//! bytes and imposes no encoding, which is correct for data that is not
//! guaranteed UTF-8 and is the cheaper of the two. [`TextRecord`] validates on
//! the way in and hands you `&str` thereafter, so the cost is paid once at
//! parse time rather than at every access. Reach for [`ByteRecord`] unless you
//! actually want `&str`.
//!
//! Each of these has an iterator form — [`byte_records`], [`text_records`],
//! [`decoded_records`], [`deserialized_records`] — for when you want the whole
//! document rather than a cursor. Note that the iterator forms must yield
//! owning values, since an iterator cannot hand out items that borrow from
//! itself; a cursor loop over [`next_line`] is what lets you stay borrowed.
//!
//! ## Typed rows
//!
//! Typed decoding is the usual destination. Derive [`CsvDecode`] and columns
//! are matched by name when the document has headers, positionally when it
//! does not:
//!
//! ```
//! use coseva::format::Csv;
//! use coseva::config::ParseOptions;
//! # #[cfg(feature = "derive")] {
//! use coseva::SliceParser;
//! use coseva::encoding::CsvDecode;
//!
//! #[derive(CsvDecode)]
//! struct City<'row> {
//!     name: &'row str,
//!     population: u64,
//! }
//!
//! let mut parser = SliceParser::<Csv>::new(b"name,population\nBoston,650706\n", ParseOptions::new())?;
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected one record"))?;
//! let city: City<'_> = line.decoded()?;
//!
//! assert_eq!(city.name, "Boston");
//! assert_eq!(city.population, 650_706);
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Decoding versus Serde
//!
//! Both [`decoded`](Line::decoded) and [`deserialized`](Line::deserialized)
//! turn a record into your own type, and both can borrow from the parser. They
//! differ in what stands between the bytes and your struct.
//!
//! [`CsvDecode`] is generated for CSV specifically. The derive knows at compile
//! time which field reads which column and what type it converts to, so the
//! expansion is a straight-line sequence of "take field *n*, convert it" — no
//! trait objects, no visitor, no intermediate representation of a row. Serde
//! must route the same work through its data model: the record is presented as
//! a map or sequence, and every field arrives through a `Deserializer` call
//! that has to be resolved at run time. In the `decode` and `deserialize`
//! benchmarks, which decode the same two columns of the same corpus through
//! each route, that indirection is the single largest cost in the typed
//! reading path.
//!
//! So: **use [`CsvDecode`] by default.** Reach for Serde when you need what
//! Serde gives you and decoding does not — an existing type you do not own but
//! that already derives `Deserialize`, `#[serde(...)]` attributes such as
//! aliases, flattening, or defaults, or a type that must round-trip through
//! several formats and can only afford one set of derives.
//!
//! Either way, prefer a **borrowing** struct — one with `&'row str` fields —
//! over an owning one with `String`s. A borrowing struct points at bytes the
//! parser already holds, so an unquoted text column costs nothing to read; an
//! owning struct copies every one of them and allocates. Use owning types when
//! the row must outlive the line, which is also what the `_records` iterators
//! require, and borrowing types everywhere else.
//!
//! When the whole document is wanted and the parser is not, [`decode_from_slice`],
//! [`decode_from_reader`], and [`decode_from_path`] do it in one call, and
//! [`deserialize_from_slice`], [`deserialize_from_reader`], and
//! [`deserialize_from_path`] are their Serde counterparts. Each yields an
//! iterator of owned records that owns its parser, so unlike
//! [`decoded_records`](SliceParser::decoded_records) it can be returned from
//! the expression that built it. They stream rather than collect, so only
//! [`decode_from_slice`] holds the document in memory, and only because the
//! caller already did.
//!
//! # Writing
//!
//! Writing mirrors reading. Pick the front end that matches where the bytes
//! need to go:
//!
//! | Writer | Use it when |
//! |---|---|
//! | [`IoEmitter`] | Writing to a file or any [`Write`], with buffered output |
//! | [`VecEmitter`] | You want the finished document as a `Vec<u8>` |
//! | [`PushEmitter`] | You want to route the encoded bytes yourself |
//!
//! When the values are already in hand, [`encode_to_vec`], [`encode_to_writer`], and
//! [`encode_to_path`] do the whole document in one call, writing the header row from
//! the type. [`encode_append_path`] resumes an existing document without repeating
//! its header, and [`encode_to_segments`] splits one across size-bounded files.
//!
//! ```
//! # #[cfg(feature = "derive")] {
//! use coseva::encoding::CsvEncode;
//! use coseva::VecEmitter;
//!
//! #[derive(CsvEncode)]
//! struct City {
//!     name: &'static str,
//!     population: u64,
//! }
//!
//! let mut emitter = VecEmitter::default();
//! emitter.encode_header::<City>()?;
//! emitter.encode(&City { name: "Washington, D.C.", population: 689_545 })?;
//!
//! // Quoting happens only where the data requires it.
//! assert_eq!(
//!     emitter.as_bytes(),
//!     b"name,population\n\"Washington, D.C.\",689545\n",
//! );
//! # }
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! [`Write`]: std::io::Write
//!
//! # Formats
//!
//! [`config::FormatOptions`] describes one CSV-shaped format, and the same
//! value configures a reader and a writer, so a round trip names its format
//! once. Presets cover the formats you are likely to meet —
//! [`CSV`](config::FormatOptions::CSV), [`TSV`](config::FormatOptions::TSV),
//! [`SEMICOLON`](config::FormatOptions::SEMICOLON),
//! [`EXCEL`](config::FormatOptions::EXCEL),
//! [`POSTGRES_COPY_CSV`](config::FormatOptions::POSTGRES_COPY_CSV),
//! [`MYSQL`](config::FormatOptions::MYSQL), and more — and every setter is
//! `const`, so a house format can be declared once as a constant.
//!
//! Behavior that belongs to the invocation rather than the format lives in
//! [`config::ParseOptions`] and [`config::EmitOptions`]: header handling,
//! field-count validation, resource limits, and buffer sizing.
//!
//! ```
//! use coseva::SliceParser;
//! use coseva::config::{FormatOptions, Headers, ParseOptions};
//!
//! const HOUSE_FORMAT: FormatOptions = FormatOptions::TSV.comment(Some(b'#'));
//!
//! let mut parser = SliceParser::with_options(
//!     b"# a comment\nBoston\t650706\n",
//!     HOUSE_FORMAT,
//!     ParseOptions::new().headers(Headers::None),
//! )?;
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected one record"))?;
//! assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Naming the format at compile time
//!
//! A parser normally reads the delimiter, quote, escape kind and record ending
//! out of its options, consulting them once per field. Knowing the format at
//! the type level instead makes those immediates, and the branches that cannot
//! be taken disappear.
//!
//! **You usually get this for free.** A parser built from run-time options
//! recognizes the common formats when it is built and runs the specialized
//! kernel for them, so the win needs no API and no opt-in — it applies even
//! when the format comes from a command-line flag or a config file. That
//! recognition is worth 12% to 23% on quoted CSV, so it is not a rounding
//! error. What follows is only for a format coseva cannot recognize, meaning
//! a custom one:
//!
//! ```
//! use coseva::SliceParser;
//! use coseva::config::{FormatOptions, Headers, ParseOptions};
//! use coseva::csv_format;
//!
//! csv_format! {
//!     /// Our upstream system's pipe-delimited export.
//!     pub Upstream = FormatOptions::CSV.delimiter(b'|');
//! }
//!
//! let mut parser = SliceParser::<Upstream>::new(
//!     b"Boston|650706\n",
//!     ParseOptions::new().headers(Headers::None),
//! )?;
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected one record"))?;
//! assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! Be selective about this. The win is concentrated on **quoted** data, where
//! the per-field branching is a real cost — around a fifth of parsing
//! instructions. Unquoted data is dominated by a SIMD scan that already holds
//! the format bytes in registers, and gains close to nothing. Each format you
//! instantiate is also another copy of the parsing code.
//!
//! ### Choosing between `new` and `with_options`
//!
//! The two constructors differ in *when* the format is known, not in what they
//! parse. [`with_options`](SliceParser::with_options) takes it as a value, so
//! it can arrive from a flag or a config file; [`new`](SliceParser::new) takes
//! it as a type parameter, so it must be known as you write the code. Use
//! whichever matches what you know; `new` is never slower.
//!
//! For a format coseva recognizes, such as plain CSV, the two are close:
//! `new` measured 1.0% to 3.1% fewer instructions across the benchmark pairs.
//! What is left is mostly in *unquoted* records, because automatic recognition
//! applies to the field splitter and unquoted records are answered before
//! reaching it.
//!
//! For a custom format nothing folds automatically, so the whole difference
//! remains and lands where per-field branching is expensive: a pipe-delimited,
//! single-quoted export measured about 15% faster declared than configured at
//! run time.
//!
//! Declaring a format this way also checks it: an unusable combination, such as
//! a delimiter that is also the quote byte, fails to compile rather than failing
//! when the first parser is built.
//!
//! Specialization composes with the run-time path rather than replacing it. All
//! three parsers take a format type parameter defaulting to
//! [`format::Dynamic`], so code that never mentions a format is unaffected —
//! and, because `Dynamic` specializes internally, is not slower for it. See the
//! `static_formats` example.
//!
//! # Reading less
//!
//! The fastest way to parse a field is to never parse it. Most CSV work reads
//! a document far larger than the answer it wants, and coseva gives you three
//! ways to say so up front.
//!
//! ## Skip records you do not want
//!
//! A [`Predicate`] is pushed *into* the reader rather than applied after it.
//! [`next_matching_line`](SliceParser::next_matching_line) tests one column's
//! raw bytes while scanning; a record that fails is never split into fields,
//! never unescaped, and never converted. Only survivors reach your loop.
//!
//! The win scales with how much you reject. Filtering in the caller costs the
//! same no matter what — you paid to parse the record before you could test it
//! — whereas pushdown costs almost nothing on the records it discards. On the
//! benchmark document, a predicate matching 1 record in 1000 does about a tenth
//! the work of the same test written in the loop. The trade is at the other
//! end: when nearly everything matches, pushdown is slightly *dearer*, because
//! the scan pays for a test that never saves anything. Push down when you
//! expect to reject most of the document.
//!
//! ## Read only the columns you want
//!
//! Name only the columns you actually need on your decoded or deserialized
//! struct. Columns you do not name are located but never unescaped and never
//! converted, so a wide document with long quoted columns you ignore costs
//! little more than a narrow one. Decoding 2 of 6 columns runs about a third
//! cheaper than decoding the whole record and using two fields of it — and the
//! gap widens the more the unwanted columns hold.
//!
//! For the cases where the columns are chosen at run time rather than by a
//! struct definition, [`FieldProjection`] names them once and
//! [`Record::project`] yields just those, in the order you asked for.
//!
//! ## Skip straight to a record
//!
//! With the `index` feature, [`index::CsvIndex`] builds a record-offset index
//! once and then seeks directly to any record, instead of rescanning the
//! document to reach it. Worth it when you will read the same file many times,
//! or need random access rather than a pass.
//!
//! The `benches/` suites carry measured figures in their module
//! documentation.
//!
//! # Errors and strictness
//!
//! Parsing is strict by default. Quotes inside unquoted fields, bytes after a
//! closing quote, invalid escapes, and unterminated quoted fields are all
//! errors, and every [`Error`] carries a [`Location`] — byte offset, line,
//! record, and field. A syntax error permanently fails that reader, because
//! once the byte stream stops making sense every later offset is a guess.
//!
//! Real-world input that a strict reader rejects can be accepted deliberately
//! and selectively through [`config::Syntax::Compatible`], and resource limits
//! ([`config::Limits`]) are enforced while scanning, before a hostile document
//! can force a large allocation.
//!
//! # Feature flags
//!
//! | Feature | Default | Adds |
//! |---|---|---|
//! | `std` | yes | [`IoParser`], [`IoEmitter`], filesystem helpers, seeking |
//! | `derive` | no | `#[derive(CsvDecode)]` and `#[derive(CsvEncode)]` |
//! | `serde` | no | Serde deserialization and serialization (implies `std`) |
//! | `index` | no | [`index::CsvIndex`] random access (implies `std`) |
//!
//! With default features disabled the crate is `no_std` with `alloc`.
//! [`SliceParser`], [`PushParser`], records, typed decoding and encoding,
//! [`VecEmitter`], [`PushEmitter`], and field projection all remain available;
//! only buffered I/O and filesystem access require `std`.
//!
//! ```toml
//! coseva = { version = "0.1", default-features = false }
//! ```
//!
//! # Examples
//!
//! The `examples/` directory holds a runnable program per topic:
//! `quickstart`, `io`, `typed_decode`, `writing`, `dialects`,
//! `static_formats`, `filtering`, `projection`, `indexed`, `errors`,
//! `serde_roundtrip`, `split_and_append`, and `push`.
//!
//! [`CsvDecode`]: encoding::CsvDecode
//! [`byte_records`]: SliceParser::byte_records
//! [`text_records`]: SliceParser::text_records
//! [`decoded_records`]: SliceParser::decoded_records
//! [`deserialized_records`]: SliceParser::deserialized_records

extern crate alloc;

#[cfg(test)]
extern crate self as coseva;

pub mod config;
pub mod encoding;
pub mod format;

mod byte_record;
mod consume;
mod emit;
mod engine;
mod error;
mod field_ends;
mod field_value;
mod filter;
mod from_bytes;
mod generate;
#[cfg(feature = "std")]
mod into_inner_error;
#[cfg(feature = "std")]
mod io_emitter;
#[cfg(feature = "std")]
mod io_parser;
mod iter;
mod line;
mod projection;
mod push;
mod push_emitter;
mod reclaim;
mod record;
mod search;
mod slice_parser;
mod span;
mod text_record;
mod vec_emitter;

#[doc(inline)]
pub use byte_record::{ByteRecord, ByteRecordIntoIter, ByteRecordIter};
#[doc(inline)]
pub use consume::decode_from_slice;
#[doc(inline)]
pub use error::{Error, ErrorKind, Location, Result};
#[doc(inline)]
pub use filter::{Column, MatchKind, Predicate};
#[doc(inline)]
pub use from_bytes::FromBytes;
#[doc(inline)]
pub use generate::encode_to_vec;
#[doc(inline)]
pub use iter::{ByteRecords, DecodedRecords, TextRecords};
#[doc(inline)]
pub use line::Line;
#[doc(inline)]
pub use projection::{FieldProjection, ProjectedFields, ProjectedTextFields};
#[doc(inline)]
pub use push::{Chunk, PushParser};
#[doc(inline)]
pub use push_emitter::{PendingPushRecord, PushEmitter};
#[doc(inline)]
pub use record::{Record, RecordIter};
#[doc(inline)]
pub use slice_parser::SliceParser;
#[doc(inline)]
pub use text_record::{TextRecord, TextRecordIntoIter, TextRecordIter};
#[doc(inline)]
pub use vec_emitter::{PendingVecRecord, VecEmitter};

#[cfg(feature = "std")]
#[doc(inline)]
pub use consume::{decode_from_path, decode_from_reader};
#[cfg(feature = "std")]
#[doc(inline)]
pub use generate::{encode_append_path, encode_to_path, encode_to_segments, encode_to_writer};
#[cfg(feature = "std")]
#[doc(inline)]
pub use into_inner_error::IntoInnerError;
#[cfg(feature = "std")]
#[doc(inline)]
pub use io_emitter::{IoEmitter, PendingIoRecord};
#[cfg(feature = "std")]
#[doc(inline)]
pub use io_parser::IoParser;

#[cfg(feature = "serde")]
#[doc(inline)]
pub use consume::deserialize_from_slice;
#[cfg(all(feature = "std", feature = "serde"))]
#[doc(inline)]
pub use consume::{deserialize_from_path, deserialize_from_reader};
#[cfg(feature = "serde")]
#[doc(inline)]
pub use generate::serialize_to_vec;
#[cfg(all(feature = "std", feature = "serde"))]
#[doc(inline)]
pub use generate::{serialize_to_path, serialize_to_writer};

#[cfg(feature = "serde")]
#[doc(inline)]
pub use iter::DeserializedRecords;

#[cfg(feature = "index")]
pub mod index;

#[cfg(feature = "parallel")]
pub mod parallel;

#[cfg(feature = "serde")]
pub mod serde;

#[cfg(feature = "benchmarking")]
#[doc(hidden)]
pub mod benchmark;

#[cfg(all(doctest, feature = "std", feature = "derive"))]
#[doc = include_str!("../../../README.md")]
/// Compiles and runs the README's Rust examples as doc tests, so the crate's
/// front page cannot drift out of sync with the API. The README documents the
/// `std` API surface and two of its examples derive `CsvDecode`, so the harness
/// needs both features. A doc test is compiled as its own crate and cannot see
/// this crate's features, so the derive cannot be gated per example; the whole
/// harness is gated instead. Run the front page with `--all-features`.
mod readme_doctests {}

#[cfg(all(doctest, feature = "std", feature = "derive"))]
#[doc = include_str!("../docs/TUTORIAL.md")]
/// Compiles and runs the tutorial's examples, for the same reason the README
/// is compiled: a tutorial that no longer matches the API is worse than none,
/// because a reader cannot tell which of the two is wrong. Gated exactly like
/// the README harness, and for the same reason.
mod tutorial_doctests {}

#[cfg(all(
    doctest,
    feature = "std",
    feature = "derive",
    feature = "serde",
    feature = "index"
))]
#[doc = include_str!("../docs/COOKBOOK.md")]
/// Compiles and runs every cookbook recipe.
///
/// Gated on all four optional features rather than marking the `serde` and
/// `index` recipes `ignore`. An ignored recipe is an unverified one, and a
/// cookbook exists to be copied from, so the recipes most likely to be pasted
/// verbatim are the ones least worth leaving untested. Run with
/// `--all-features`.
mod cookbook_doctests {}

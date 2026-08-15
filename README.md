# coseva

A fast, strict, low-allocation CSV reader and writer.

`coseva` reads and writes CSV and the many formats shaped like it — TSV,
semicolon-separated European exports, PostgreSQL `COPY`, MySQL text dumps,
Excel's CRLF-and-BOM flavor. It is built for the case where the file is large
and the loop around it is hot: fields are handed to you as slices of the input
wherever the format allows, and steady-state reading does not allocate.

The core types are re-exported at the crate root; policy and configuration live
in `config`, and the typed conversion traits and their derive macros live in
`encoding`.

For ordinary files and streams, `IoParser<R>` treats the first record as headers
by default:

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use std::io::Cursor;

use coseva::IoParser;
use coseva::ByteRecord;

let mut parser = IoParser::<_, Csv>::new(Cursor::new(
    b"city,population\nBoston,650706\n",
), ParseOptions::new()).expect("parser");
assert_eq!(parser.header_index("population")?, Some(1));

let mut record = ByteRecord::new();
while let Some(mut line) = parser.next_line()? {
    line.read_byte_record_into(&mut record)?;
    println!("{:?}", record.get(0));
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`read_byte_record_into` retains the record's byte and field-index capacities. It may
allocate on first use or when a larger record arrives, but steady-state reads
with sufficient capacity do not allocate.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;

let mut parser = SliceParser::<Csv>::new(b"city,population\nBoston,650706\n", ParseOptions::new()).expect("parser");

// Every parser consumes the first record as headers by default.
let headers = parser.headers()?.expect("headers");
assert_eq!(headers.get(0), Some(&b"city"[..]));

let mut line = parser.next_line()?.expect("first data record");
let record = line.record()?;
assert_eq!(record.get_str(0)?, Some("Boston"));
assert_eq!(record.parse::<u64>(1)?, Some(650_706));
# Ok::<(), Box<dyn std::error::Error>>(())
```

`FormatOptions` describes one CSV format for both reading and writing:
delimiters, quoting, escaping, comments, trimming, blank lines, strict
compatibility recovery, BOM handling, and NULL conventions. Every constructor
and setter is `const`, so custom formats can be declared as constants.
`ParseOptions` and `EmitOptions` are separate, format-independent values
carrying the per-invocation concerns: headers, field-count validation,
resource limits, and buffering. A format and a matching options value are
passed together when a parser or emitter is constructed, through
`SliceParser::with_options`, `IoParser::with_options`,
`IoParser::from_path`, `VecEmitter::with_options`,
`IoEmitter::with_options`, and `IoEmitter::to_path`.
`ByteRecord` and
`TextRecord` support mutation, conversion, capacity reservation, clearing,
and reuse, plus contiguous-storage and per-field range access. Seekable inputs
support validated `seek`, `seek_raw`, and header-aware `rewind`; owned byte
records can be converted to UTF-8 strictly or lossily. Parser locations report
exact one-based physical lines across quoted newlines, CRLF buffer splits,
comments, blank lines, custom record endings, seeking, and persistent
indexes.

Common formats are named and can still be overridden:

```rust
use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::SliceParser;

let mut parser = SliceParser::with_options(
    b"Boston\t650706\n",
    FormatOptions::TSV,
    ParseOptions::new().headers(Headers::None),
)?;
let mut line = parser.next_line()?.expect("record");
let record = line.record()?;
assert_eq!(record.get(0), Some(&b"Boston"[..]));
# Ok::<(), Box<dyn std::error::Error>>(())
```

| Format | Behavior |
| --- | --- |
| `CSV` | Standard comma-separated values |
| `TSV` | Tab-separated values |
| `SEMICOLON` | Semicolon-separated values |
| `PIPE` | Pipe-delimited values |
| `BACKSLASH_CSV` / `BACKSLASH_TSV` | Backslash escaping inside quoted fields |
| `COMMENTED_CSV` | `#` comments and skipped blank lines |
| `TRIMMED_CSV` | ASCII-whitespace trimming for headers and fields |
| `PYTHON_CSV` | Python-style `skipinitialspace=True` |
| `RFC4180` | Strict RFC 4180 records with mandatory CRLF terminators |
| `EXCEL` | CRLF records, detecting a UTF-8 BOM on read and writing one |
| `POSTGRES_COPY_CSV` | PostgreSQL `COPY ... CSV`, including explicit NULL fields |
| `MYSQL` | MySQL text export with unquoted backslash escapes and `\N` NULL |

One constant describes both directions, so a round trip names its format once:
passing `FormatOptions::EXCEL` to both a parser and an emitter makes them agree
on CRLF records and the UTF-8 BOM. Formats can be refined with `const` setters
such as `delimiter`, `quote`, `escape`, `comment`, `trim`, `syntax`,
`nulls`, and `quoting`. Options that describe only one direction are
ignored by the other: an emitter has nothing to do about `trim`, and a parser
has nothing to do about `quoting`. Python-style initial-space handling
ignores only spaces after delimiters; it does not trim the first field or
trailing spaces. Push-based adapters take the same formats and parse
options through `PushParser::with_options`.

Structural bytes are validated when a parser or emitter is built, not when a
format is declared, because `const` setters cannot report errors.

Database formats retain NULL separately from an empty field without allocating

parallel metadata. `Record`, `ByteRecord`, and `TextRecord` expose
`is_null`; owned records also expose `push_null` and `set_null`. Existing
`get` and iteration APIs continue to return an empty value for a NULL field,
while native and Serde `Option<T>` decoding observes the distinction.
`emit_byte_record`, `emit_text_record`, typed encoding, and
`emit_nullable_record` emit the configured database marker.

Strict parsing is always the default. Non-standard recovery—such as disabling
quote syntax, accepting arbitrary backslash escapes, or allowing whitespace
after a closing quote—requires an explicit `Syntax::Compatible` policy.

`IoEmitter` and `VecEmitter` accept byte records, text records, iterators, or an
atomic field-at-a-time guard:

```rust
use coseva::VecEmitter;

let mut emitter = VecEmitter::default();
let mut record = emitter.begin_record();
record.write_field("Boston")?;
record.write_field("650706")?;
record.finish()?;
assert_eq!(emitter.as_bytes(), b"Boston,650706\n");
# Ok::<(), coseva::Error>(())
```

Emitter policies include BOM output, field-count validation, safe necessary,
always, and never quoting, plus an explicitly ambiguous raw mode. Generic
emitter finalization flushes through `into_inner()` and returns a recoverable
`IntoInnerError` on failure.

With the `derive` feature, native typed records resolve headers once and decode
directly without Serde:

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use std::io::Cursor;

use coseva::encoding::CsvDecode;
use coseva::IoParser;

#[derive(CsvDecode)]
struct City {
    name: String,
    population: u64,
}

let mut parser = IoParser::<_, Csv>::new(Cursor::new(
    b"population,name\n650706,Boston\n",
), ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("record");
let city = line.decoded::<City>()?;
assert_eq!(city.name, "Boston");
# Ok::<(), coseva::Error>(())
```

Where a loop over owned records reads better than a cursor, `SliceParser` and
`IoParser` also hand out iterators: `byte_records`, `text_records`,
`decoded_records`, and `deserialized_records`. Each has a `matching_` form that
pushes a predicate down into the scan, so filtering is a field on the same
iterator rather than a type of its own.

```rust
use coseva::encoding::CsvDecode;
use coseva::{Predicate, SliceParser};
use coseva::config::{FormatOptions, Headers, ParseOptions};

#[derive(CsvDecode)]
struct City {
    city: String,
    country: String,
}

let mut parser = SliceParser::with_options(
    b"city,country\nBoston,US\nParis,FR\nDenver,US\n",
    FormatOptions::CSV,
    ParseOptions::new().headers(Headers::FirstRecord),
)?;

let predicate = Predicate::equals("country", "US");
let cities = parser
    .matching_decoded_records::<City>(&predicate)
    .map(|city| Ok(city?.city))
    .collect::<Result<Vec<_>, coseva::Error>>()?;
assert_eq!(cities, ["Boston", "Denver"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Every item is independently owned, because an iterator cannot lend an item
that borrows the parser it came from. Reach for `next_line` and
`Line::read_byte_record_into` instead when one record buffer should be reused
across the loop.

Header mappings that select only part of a record automatically use a
projected parser: every CSV field is still validated and counted, but ignored
fields are not copied or entered into generic record storage. Integer and
boolean fields decode directly from their byte slices; strings alone require
UTF-8 handling. No alternate typed API or projection call is required.

Typed encoding uses stack-based integer and floating-point formatting.
`encode_header::<T>()`, `encode`, and `encode_all` are available on both emitter
types.

The optional `serde` feature provides familiar `deserialize`,
`deserialized`, and `serialize` methods. Deserialization walks record and
header iterators directly instead of allocating per-record reference vectors.
Absent fields deserialize as `None`; a present empty field remains data (for
example, `Some(String::new())`) rather than being treated as null globally.
Serde serialization emits named-struct headers automatically by default; use
`EmitOptions::has_headers(false)` to suppress them and enable depth-first
flattening of nested sequences, tuples, and structs.
The native derives remain the preferred specialization and code-generation
path.

A `FieldProjection` resolves the columns a workload cares about once, either by
zero-based position or by name against a header sequence. Every record type then
applies it with `project`, which yields the selected fields in projection order.
A selected position past the end of a short record yields `None`, so a narrow
record never silently shifts the remaining fields:

```rust
use coseva::{ByteRecord, FieldProjection};

let headers = ByteRecord::from(vec![b"city".to_vec(), b"state".to_vec(), b"pop".to_vec()]);
let projection = FieldProjection::from_headers(&headers, ["pop", "city"])?;

let record = ByteRecord::from(vec![b"Boston".to_vec(), b"MA".to_vec(), b"650706".to_vec()]);
assert_eq!(
    record.project(&projection).collect::<Vec<_>>(),
    [Some(&b"650706"[..]), Some(&b"Boston"[..])],
);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Projecting a lending `Record` yields fields that borrow from the parsed input
rather than from the record view, so they stay usable after the view is dropped.
`TextRecord::project` yields `Option<&str>` instead of `Option<&[u8]>`.

`PushParser` inverts the control flow for sources that own the read loop, such
as async sockets, FFI callbacks, or decompressors. A slice is lent to `chunk`,
and the records those bytes completed are then walked with the same cursor the
other parsers expose, borrowed straight out of the caller's memory. `done`
ends the loan and reports how much of the slice the parser took, leaving only
a record the chunk cut in half behind:

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::PushParser;

let chunks: [&[u8]; 3] = [b"city,pop\nBos", b"ton,650706\nLond", b"on,8982000"];
let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
let mut cities = Vec::new();

for bytes in chunks {
    let mut offset = 0;
    while offset < bytes.len() {
        let mut chunk = parser.chunk(&bytes[offset..]);
        while let Some(mut line) = chunk.next_line()? {
            cities.push(line.record()?.get_str(0)?.unwrap_or_default().to_owned());
        }
        offset += chunk.done();
    }
}

// The final record has no terminator, so it is released by `finish`.
parser.finish();
let mut chunk = parser.chunk(b"");
while let Some(mut line) = chunk.next_line()? {
    cities.push(line.record()?.get_str(0)?.unwrap_or_default().to_owned());
}
drop(chunk);
assert_eq!(cities, ["Boston", "London"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

`Chunk::next_line` returning `None` means "no further record from the bytes
lent so far", which is a suspension rather than an end of input; `is_done`
tells the two apart. Only a record straddling the end of a chunk is copied and
retained, so memory stays bounded by the configured record limit and a
`RecordTooLarge` is reported for a source that never terminates a record.

A `coseva::Predicate` pushes a single-column match down into the parser.
Because a `Predicate` is an inspectable value rather than a closure, the
literal is searched for directly in the raw input with the SIMD byte scanner,
so records that cannot match are never split into fields:

```rust
use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::Predicate;
use coseva::SliceParser;

let mut parser = SliceParser::with_options(
    b"city,country\nBoston,US\nParis,FR\nDenver,US\n",
    FormatOptions::CSV,
    ParseOptions::new().headers(Headers::FirstRecord),
)?;

let predicate = Predicate::equals("country", "US");
let mut cities = Vec::new();
while let Some(mut line) = parser.next_matching_line(&predicate)? {
    let record = line.record()?;
    cities.push(record.get_str(0)?.unwrap_or_default().to_owned());
}
assert_eq!(cities, ["Boston", "Denver"]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Matching stays exact: a record located by the scan is fully parsed and
evaluated before it is returned, and literals that escaping could split
transparently fall back to inspecting every record. At one match in a
thousand this takes roughly 9x fewer instructions than filtering parsed
records in the application, and 20x fewer than the same filter on the `csv`
crate.

`IoParser::next_matching_line()` pushes the same predicate down into a streaming read,
skipping whole records inside the read buffer, so enormous inputs can be
filtered without ever materialising the records that do not match. At one
match in a thousand that is roughly 8x fewer instructions than filtering
streamed records in the application.

With the `index` feature, `coseva::index::CsvIndex` builds, saves, loads,
and validates versioned record-offset indexes. Indexes store the complete
format and parsing limits, the source length and XXH3 identity, and a
whole-index checksum before they can construct a parser at an indexed record.

Indexing never requires the source or the index to fit in memory.
`CsvIndex::create_path` streams a file and writes each record position straight
to the index, so building uses constant memory, and `CsvIndexReader` reads one
position per lookup off disk instead of materialising the whole location table.
Random access is served by seeking the source itself, so a file far larger than
RAM can be visited at random.

Seeking checks the source length, which is cheap; `validate_reader` streams the
source once to confirm the full XXH3 identity, and `CsvIndexReader::verify`
checks the whole-index checksum. `CsvIndex::parser_at_reader` and
`parser_at_path` offer the same seeking access from an eagerly loaded index.

## Feature flags and `no_std`

The default `std` feature provides buffered `IoParser`/`IoEmitter` types, filesystem
helpers, seeking, Serde, persistent indexes, and benchmark support. Disable
default features for an alloc-only build:

```toml
coseva = { version = "0.1", default-features = false }
```

The alloc-only surface includes formats and options, `SliceParser`, borrowed
and owned records, native typed decoding and encoding, `VecEmitter`, field
projections, and `PushParser`.
The `derive` feature is also alloc-only compatible. Every fallible operation
reports the same `Error` type in every configuration, so `VecEmitter` returns
`Error` here exactly as it does under `std`.

The `serde`, `index`, and `benchmarking` features enable `std`. Applications
provide the global allocator required by their target.

The optional `compact_str` feature implements the field traits for
[`compact_str::CompactString`], letting it be used anywhere `String` can — as a
derived struct field, through `Record::parse`, and through Serde. A value of 24
bytes or fewer is stored inline, so short fields decode without allocating
while the type stays the same size as a `String`. It is alloc-only compatible
and does not enable `std`.

```toml
coseva = { version = "0.1", features = ["compact_str"] }
```

[`compact_str::CompactString`]: https://docs.rs/compact_str/latest/compact_str/struct.CompactString.html

The optional `multibyte` feature allows a delimiter or record terminator of up
to four bytes, through `FormatOptions::delimiter_sequence` and
`FormatOptions::record_ending_sequence`. Files delimited by `||` or `\t|\t`
exist, and pandas' `read_csv` accepts a multi-character separator.

It is a feature rather than a plain option because the bytes have to live in
every `FormatOptions` value, which grows from 20 to 28 bytes and costs about 80
instructions per parser built — measurable at construction even for a dialect
that never uses one. With the feature off, every benchmark is instruction-for-
instruction identical to a build that does not know the option exists. A
multi-byte separator also parses on the general path rather than the vectorized
one, since every scan in the crate matches single bytes.

```toml
coseva = { version = "0.1", features = ["multibyte"] }
```

The optional `parallel` feature parses a whole in-memory document across
threads, through `parallel::ParallelParser`. A worker is a plain `SliceParser`
seeked to a record boundary, so nothing in the scanning kernels knows it exists;
the one new algorithm is a serial quote-counting scan that splits the document
at true record starts, which is exact rather than a heuristic, so a value
containing a record ending needs no opt-out. Batches of owned records arrive on
the calling thread in document order, and the reported failure is the one at the
lowest byte offset rather than whichever thread failed first.

The crossover is high and worth knowing before reaching for it. On a 16-core
machine reading narrow numeric rows, threads drew level with a single thread at
about 16 MiB and were about 2.2x faster at 64 MiB, for roughly three times the
CPU time, because every record is owned rather than read into one reused buffer.
Below 16 MiB `ParallelParser` silently parses on the calling thread. Formats
that cannot be split by counting quotes — comments, backslash or MySQL
escaping, quoting relaxations, multi-byte separators — are rejected rather than
silently run serially. `benches/parallel.rs` re-measures the crossover.

```toml
coseva = { version = "0.1", features = ["parallel"] }
```

## Grammar and failure behavior

- An unquoted field cannot contain the quote byte.
- A quoted field must begin at the start of a field. Its closing quote must be
  followed by a delimiter, record ending, or end of input.
- Double-quote escaping accepts `""`. Backslash escaping accepts only the
  quote byte or the configured escape byte after a backslash.
- The newline record ending accepts LF and CRLF. Newlines inside quoted fields
  are data. A final record does not need a record ending.
- A leading UTF-8 BOM is stripped by default. Comments are recognized only at
  the start of a record and continue through the configured record ending.
- Empty input contains no records; an empty terminated line contains one empty
  field.

Malformed input returns a location-aware error and permanently fails that
parser. Record, field, and field-count limits are checked while scanning,
before an unbounded allocation can occur. Field and record byte limits apply
to raw input bytes. A `PushParser` delivers the records completed before a
malformed one and reports the error from the call that reaches it, exactly
where the slice and streaming parsers report it. Generic emitters permanently
reject further output after an underlying write or flush failure.

## Benchmarks

If you just want the answer, [`crates/coseva/docs/PERF.md`](crates/coseva/docs/PERF.md)
is the summary: five record shapes across three front ends over five documents,
against `csv` wherever `csv` can express the same shape. It is generated by
`crates/coseva/scripts/perf_report.rs` straight from Callgrind output, so no
number in it is transcribed by hand, and `--check` fails if it has gone stale.

Seventeen Callgrind suites measure deterministic instruction counts. Comparison
rows use identical bytes and asserted matching checksums, so a case cannot
drift into doing different work and still look comparable:

```text
cargo bench --features "std derive serde"  --bench matrix
cargo bench --features std                 --bench read_record
cargo bench --features std                 --bench byte_record
cargo bench --features std                 --bench text_record
cargo bench --features "std serde"         --bench deserialize
cargo bench --features "std derive serde"  --bench decode
cargo bench --features "std derive"        --bench decode_wide
cargo bench --features "std derive serde"  --bench width_sweep
cargo bench --features std                 --bench startup
cargo bench --features "std derive serde"  --bench encode
cargo bench --features std                 --bench quoted
cargo bench --features "std multibyte"     --bench dialects
cargo bench --features "std index derive"  --bench index
cargo bench --features std                 --bench filter
cargo bench --features "std multibyte"     --bench window
cargo bench --features "std serde"         --bench mapping
cargo bench --features std                 --bench literal_search
```

`matrix` is the customer-facing one, and the only suite whose corpus is five
documents shaped like files people actually have — narrow numeric, 128 columns,
heavily quoted, long free text, and a spreadsheet export with CRLF and a BOM —
rather than one row repeated. The others hold everything constant on purpose,
which is what makes a 2% change meaningful, but makes their tables a best case.

`read_record` measures the borrowed path against `csv-core`; `byte_record` and
`text_record` measure the owned records against `csv`, and differencing them
gives the cost of UTF-8 validation. `deserialize` and `decode` both take two of
six columns into a struct — one through Serde, one through
`#[derive(CsvDecode)]` — over an identical corpus. `decode` also runs coseva's
Serde path itself, so what native decoding saves is read within its table
rather than across the two. `decode_wide` asks the
same question at a hundred columns of which five are wanted, and `width_sweep`
sweeps 6, 20, 60 and 200 columns of identical field content to measure how cost
actually grows with width rather than estimating it from two points.

`quoted` reads the same records three ways — unquoted, quoted, and quoted with
a doubled quote to unescape — through the same front ends. Every other suite
here reads unquoted ASCII, so their tables are a best case; this is the one
that says what the common case of a quoted text column actually costs, and its
answer is currently unflattering.

`dialects` measures the four options that route away from the specialized
parser — CRLF endings, `MySQL` escapes, a NULL policy, and a trim that spares
quoted fields — against both the specialized parser and `csv` over identical
bytes.

`window` drives the read window down through the record size, so that the
refill paths every other suite avoids are all that is left. It is the one
place that says what `buffer_capacity` is actually worth, and it finds that
`push` — whose chunk size its caller does not choose — degrades much further
than `io` does.

`filter` sweeps selectivity rather than record count, since skipping work is
the entire point of `next_matching_line`. It compares filtering against the
hand-written loop a caller would otherwise write, over identical bytes, for
both an equality and a substring predicate. Its answer has a threshold in it
worth knowing before reaching for the filter.

`index` is the only suite whose axis is not throughput. It measures what
building a record index costs per record, in memory and streamed, and what a
seek costs once one exists — swept across the file to show that the answer does
not depend on position — with the resulting index size reported alongside.

`encode` is the only suite that writes. It puts the same six fields through
all three emitters and through `#[derive(CsvEncode)]` and Serde, against the
`csv` writer, so that half the crate is not left without numbers.

`startup` is the one suite whose axis is not records at all. It measures what a
parser costs before it has produced anything — construction, the first record,
and resolving one column by name — across the same widths, because that fixed
cost is otherwise visible only as a component of a per-record column built to
measure something else.

Every suite except `width_sweep` and `startup` runs at 1, 10, 100 and 1000
records; `width_sweep` runs at 100 and 1000 because only their difference feeds
it, and `startup` sweeps header width instead. A single size conflates the
fixed cost a front end pays once with the marginal cost it pays per record;
differencing the sizes separates them, and only the second is a parsing speed.

The measured tables live in each benchmark's module documentation, next to the
code that produced them. Each file is a separate binary, and the optimizer's
inlining decisions inside a measured loop depend on the rest of that binary:
adding an unrelated function to a file has been shown to move an unchanged case
by 23%, which is why two files measuring identical work disagree by 17%. So
compare rows within one table, never across files, and treat a table as
belonging to the commit that produced it.

## Documentation

- [`crates/coseva/docs/TUTORIAL.md`](crates/coseva/docs/TUTORIAL.md) — a
  start-to-finish narrative: choosing a parser, how records borrow, typed
  rows, errors, and writing. Read this first.
- [`crates/coseva/docs/COOKBOOK.md`](crates/coseva/docs/COOKBOOK.md) —
  task-oriented recipes: other dialects, headerless input, NULL conventions,
  projections, random access, Serde, and error reporting.
- [`crates/coseva/docs/PERF.md`](crates/coseva/docs/PERF.md) — what this crate
  costs, and what `csv` costs on the same bytes, across five record shapes and
  five document shapes. Generated from Callgrind output, never edited by hand.
- [`crates/coseva/docs/DESIGN.md`](crates/coseva/docs/DESIGN.md) —
  architecture: the techniques, algorithms, and data flow behind the parser,
  emitter, and index.
- [`crates/coseva/examples/`](crates/coseva/examples) — a runnable program per
  topic: `quickstart`, `streaming`, `typed_decode`, `writing`, `dialects`,
  `filtering`, `projection`, `indexed`, `errors`, `serde_roundtrip`,
  `split_and_append`, and `push`. Run one with
  `cargo run -p coseva --example quickstart`.

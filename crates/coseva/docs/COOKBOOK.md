# coseva cookbook

Recipes, each self-contained: a problem, then the shortest correct code for
it. Every block is compiled and run as a doc test, so anything here can be
pasted and will work.

If you want the story instead of the answers, read `TUTORIAL.md` first. If you
want a runnable program, `examples/` has one per topic.

## Reading

### Read a TSV, semicolon or pipe-separated file

The common dialects are constants. Nothing else about your code changes.

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, ParseOptions};

let mut parser = SliceParser::with_options(
    "city\tpopulation\nBoston\t650706\n",
    FormatOptions::TSV,
    ParseOptions::new(),
)?;

let mut line = parser.next_line()?.expect("a record");
assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
# Ok::<(), coseva::Error>(())
```

`FormatOptions::SEMICOLON` and `FormatOptions::PIPE` are there too. European
exports usually want `SEMICOLON`.

### Read a file with no header row

Otherwise you silently lose your first record.

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions};

let mut parser = SliceParser::with_options(
    "Boston,650706\nSydney,5312163\n",
    FormatOptions::CSV,
    ParseOptions::new().headers(Headers::None),
)?;

assert_eq!(parser.byte_records().count(), 2);
# Ok::<(), coseva::Error>(())
```

With `Headers::None`, a struct's fields bind **by position**, not by name.

### Skip comment lines and blank lines

```rust
use coseva::SliceParser;
use coseva::config::{BlankRecords, FormatOptions, ParseOptions};

const FORMAT: FormatOptions = FormatOptions::CSV
    .comment(Some(b'#'))
    .blank_records(BlankRecords::Skip);

let mut parser = SliceParser::with_options(
    "# generated, do not edit\ncity,population\n\nBoston,650706\n\nSydney,5312163\n",
    FORMAT,
    ParseOptions::new(),
)?;

assert_eq!(parser.byte_records().count(), 2);
# Ok::<(), coseva::Error>(())
```

### Treat a sentinel as NULL

An empty field and a missing value are different things in a database export.
`Nulls` says which spelling your source uses, and `is_null` distinguishes it
from an empty string.

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, Nulls, ParseOptions};

let mut parser = SliceParser::with_options(
    "city,region\nBoston,\\N\nSydney,NSW\n",
    FormatOptions::CSV.nulls(Nulls::Mysql),
    ParseOptions::new(),
)?;

let mut line = parser.next_line()?.expect("a record");
let record = line.record()?;
assert_eq!(record.is_null(1), Some(true));
# Ok::<(), coseva::Error>(())
```

`Nulls::Postgres` covers `COPY` output. The default is `Nulls::None`, where an
empty field is simply empty.

### Trim surrounding whitespace

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions, Whitespace};

let mut parser = SliceParser::with_options(
    "  Boston  ,  650706  \n",
    FormatOptions::CSV.trim(Whitespace::ALL),
    ParseOptions::new().headers(Headers::None),
)?;

let mut line = parser.next_line()?.expect("a record");
assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
# Ok::<(), coseva::Error>(())
```

### Accept input a strict parser rejects

Real exports contain stray quotes. `Syntax::Compatible` reads them instead of
failing.

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions, Recovery, Syntax};

let format = FormatOptions::CSV.syntax(Syntax::Compatible(Recovery::PERMISSIVE));
let mut parser = SliceParser::with_options(
    "Bris\"tol,467099\n",
    format,
    ParseOptions::new().headers(Headers::None),
)?;

let mut line = parser.next_line()?.expect("a record");
assert_eq!(line.record()?.get_str(0)?, Some("Bris\"tol"));
# Ok::<(), coseva::Error>(())
```

Use this deliberately. It accepts input that is genuinely ambiguous, so it
trades a loud failure for a quiet guess.

### Bound how much memory a parse may use

Untrusted input should not be able to ask for an unbounded allocation.
`Limits` caps the record size, the field size and the field count.

```rust
use std::io::Cursor;

use coseva::IoParser;
use coseva::config::{FormatOptions, Limits, ParseOptions};

// At most a 1 MiB record, a 64 KiB field, and 1,024 fields per record.
let options = ParseOptions::new().limits(Limits::new(1 << 20, 1 << 16, 1024));
let mut parser = IoParser::with_options(
    Cursor::new(b"city,population\nBoston,650706\n".to_vec()),
    FormatOptions::CSV,
    options,
)?;

assert!(parser.next_line()?.is_some());
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Parse a file larger than memory, without reading it in

`SliceParser::new` takes anything that dereferences to bytes, and a `memmap2`
mapping does, so a mapped file needs no support from this crate. The pairing is
worth reaching for rather than merely possible: borrowed fields point straight
into the mapped pages, so a document larger than RAM is parsed with the
operating system paging underneath it and no copy at all. Reading the file into
a `Vec` cannot do that — it has to hold the whole document at once.

Mapping is `unsafe` because the mapped file must not be truncated or written by
another process while the map is alive; doing so is undefined behaviour, and no
wrapper can check it. Map only files whose lifecycle you control.

```rust
use std::fs;

use coseva::SliceParser;
use coseva::config::{FormatOptions, ParseOptions};

let path = std::env::temp_dir().join(format!("coseva-mmap-{}.csv", std::process::id()));
fs::write(&path, b"city,population
Boston,650706
Sydney,5231150
")?;

let file = fs::File::open(&path)?;
// SAFETY: this process wrote the file and nothing else touches it while the
// mapping is alive.
let mapped = unsafe { memmap2::Mmap::map(&file)? };

let mut parser = SliceParser::with_options(
    &*mapped,
    FormatOptions::CSV,
    ParseOptions::new(),
)?;

let mut total = 0_u64;
while let Some(mut line) = parser.next_line()? {
    // Borrowed straight out of the mapped pages: nothing is copied.
    total += line.record()?.parse::<u64>(1)?.unwrap_or_default();
}

assert_eq!(total, 5_881_856);
fs::remove_file(&path)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Typed rows

### Collect rows into a `Vec`

The type must own its data, so use `String` rather than `&str`.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct City {
    city: String,
    population: u64,
}

let mut parser = SliceParser::<Csv>::new("city,population\nBoston,650706\nSydney,5312163\n", ParseOptions::new()).expect("parser");
let rows: Vec<City> = parser
    .decoded_records::<City>()
    .collect::<Result<_, coseva::Error>>()?;

assert_eq!(rows[1].population, 5_312_163);
# Ok::<(), coseva::Error>(())
```

### Decode without allocating

Give the struct a lifetime and borrow. Nothing is copied, and the rows cannot
outlive the loop.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct CityRef<'row> {
    city: &'row str,
    population: u64,
}

let mut parser = SliceParser::<Csv>::new("city,population\nBoston,650706\nSydney,5312163\n", ParseOptions::new()).expect("parser");
let mut total = 0_u64;
while let Some(mut line) = parser.next_line()? {
    let row: CityRef<'_> = line.decoded()?;
    total += row.population;
}

assert_eq!(total, 5_962_869);
# Ok::<(), coseva::Error>(())
```

### Handle a column whose name does not match the field

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(rename = "pop")]
    population: u64,
}

let mut parser = SliceParser::<Csv>::new("pop\n650706\n", ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("a record");
let row: Row = line.decoded()?;
assert_eq!(row.population, 650_706);
# Ok::<(), coseva::Error>(())
```

### Tolerate missing or empty values

`#[csv(default)]` substitutes `Default::default()` instead of failing.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    city: String,
    #[csv(default)]
    population: u64,
}

let mut parser = SliceParser::<Csv>::new("city,population\nBoston,\n", ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("a record");
let row: Row = line.decoded()?;
assert_eq!(row.population, 0);
# Ok::<(), coseva::Error>(())
```

Use `Option<u64>` instead when "absent" and "zero" must stay distinguishable.

### Decode a type the crate has never heard of

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

fn parse_hex(bytes: &[u8]) -> Result<u32, std::num::ParseIntError> {
    let text = std::str::from_utf8(bytes).unwrap_or("");
    u32::from_str_radix(text.trim_start_matches("0x"), 16)
}

#[derive(CsvDecode)]
struct Swatch {
    #[csv(parse_with = "parse_hex")]
    rgb: u32,
}

let mut parser = SliceParser::<Csv>::new("rgb\n0xE34234\n", ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("a record");
let swatch: Swatch = line.decoded()?;
assert_eq!(swatch.rgb, 0x00E3_4234);
# Ok::<(), coseva::Error>(())
```

## Reading less

### Read only some rows

The predicate is tested during the scan, so rejected records are never split
into fields at all.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::{Predicate, SliceParser};

let data = "city,country\nBoston,US\nSydney,AU\nDallas,US\n";
let predicate = Predicate::equals("country", "US");
let mut parser = SliceParser::<Csv>::new(data, ParseOptions::new()).expect("parser");

let mut hits = 0;
while let Some(mut line) = parser.next_matching_line(&predicate)? {
    let _ = line.record()?;
    hits += 1;
}

assert_eq!(hits, 2);
# Ok::<(), coseva::Error>(())
```

`Predicate::contains`, `starts_with` and `ends_with` are also available.

### Read only some columns

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::{FieldProjection, SliceParser};

let mut parser = SliceParser::<Csv>::new("city,country,population\nBoston,US,650706\n", ParseOptions::new()).expect("parser");
let headers = parser.headers()?.expect("headers").clone();
let projection = FieldProjection::from_headers(&headers, ["population", "city"])?;

let mut line = parser.next_line()?.expect("a record");
let record = line.record()?;
let picked: Vec<&[u8]> = record.project(&projection).flatten().collect();

// Yielded in the order you asked for, not the order in the file.
assert_eq!(picked, [&b"650706"[..], &b"Boston"[..]]);
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Jump straight to record 500

You cannot seek to a record by counting newlines, because a newline may sit
inside a quoted field. An index records where each one truly starts.

```rust
use coseva::index::{CsvIndex, IndexOptions};

let source = "city,population\nBoston,650706\nSydney,5312163\nDallas,1304379\n";
let index = CsvIndex::build(source, IndexOptions::default())?;

// Record 0 is the header row, so record 2 is the second data row.
let mut parser = index.parser_at(source, 2)?;
let mut line = parser.next_line()?.expect("that record exists");
assert_eq!(line.record()?.get_str(0)?, Some("Sydney"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

Build once with `CsvIndex::build_path`, `save` it, and `load` it later.
`CsvIndexReader` keeps the table on disk when even the index is too big to
hold.

## Writing

### Write to a `Vec<u8>`

```rust
use coseva::VecEmitter;

let mut emitter = VecEmitter::default();
emitter.emit_record(["city", "population"])?;
emitter.emit_record(["Washington, D.C.", "689545"])?;

// The comma forced a quote; nothing else was quoted.
assert_eq!(
    emitter.as_bytes(),
    b"city,population\n\"Washington, D.C.\",689545\n",
);
# Ok::<(), coseva::Error>(())
```

### Write to a file or any `Write`

```rust
use coseva::IoEmitter;
use coseva::config::EmitOptions;
use coseva::format::Csv;

let mut emitter = IoEmitter::<_, Csv>::new(Vec::new(), EmitOptions::new())?;
emitter.emit_record(["city", "population"])?;

// `into_inner` flushes and returns the sink, so a failed flush is reported
// rather than swallowed by a drop.
let bytes = emitter.into_inner()?;
assert_eq!(bytes, b"city,population\n");
# Ok::<(), Box<dyn std::error::Error>>(())
```

Use `IoEmitter::to_path` to write a file directly.

### Write structs, header included

```rust
use coseva::encoding::CsvEncode;
use coseva::VecEmitter;

#[derive(CsvEncode)]
struct City {
    city: &'static str,
    population: u64,
}

let mut emitter = VecEmitter::default();
emitter.encode_header::<City>()?;
emitter.encode_all([City { city: "Boston", population: 650_706 }])?;

assert_eq!(emitter.as_bytes(), b"city,population\nBoston,650706\n");
# Ok::<(), coseva::Error>(())
```

### Force quoting for a fussy consumer

```rust
use coseva::config::{EmitOptions, FormatOptions, Quoting};
use coseva::VecEmitter;

let format = FormatOptions::CSV.quoting(Quoting::Always);
let mut emitter = VecEmitter::with_options(Vec::new(), format, EmitOptions::new())?;
emitter.emit_record(["Boston", "650706"])?;

assert_eq!(emitter.as_bytes(), b"\"Boston\",\"650706\"\n");
# Ok::<(), coseva::Error>(())
```

`Quoting::NonNumeric` quotes text but leaves numbers bare.

## Interop

### Use a type that already derives Serde

Reach for this when the type exists for JSON or a database and you would
rather not maintain a second set of attributes. `CsvDecode` is faster, because
it skips Serde's intermediate model.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use serde::Deserialize;

#[derive(Deserialize)]
struct Measurement {
    station: String,
    #[serde(rename = "temp_c")]
    celsius: f64,
}

let mut parser = SliceParser::<Csv>::new("station,temp_c\nKBOS,21.5\nKSFO,17.25\n", ParseOptions::new()).expect("parser");
let rows: Vec<Measurement> = parser
    .deserialized_records::<Measurement>()
    .collect::<Result<_, coseva::Error>>()?;

assert_eq!(rows[1].celsius, 17.25);
# Ok::<(), coseva::Error>(())
```

### Parse CSV arriving from an async socket

`PushParser` performs no I/O, so it composes with any runtime. Lend it what
arrives; drain what completed.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::PushParser;

let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
let mut cities = Vec::new();

for bytes in [&b"city\nBos"[..], &b"ton\nSydney\n"[..]] {
    let mut offset = 0;
    while offset < bytes.len() {
        // The parser may take less than it was offered, so the loan belongs
        // in a loop.
        let mut chunk = parser.chunk(&bytes[offset..]);
        while let Some(mut line) = chunk.next_line()? {
            cities.push(line.record()?.get_str(0)?.unwrap_or_default().to_owned());
        }
        offset += chunk.done();
    }
}

// Without `finish`, a last record with no trailing newline stays buffered.
parser.finish();
let mut chunk = parser.chunk(b"");
while let Some(mut line) = chunk.next_line()? {
    cities.push(line.record()?.get_str(0)?.unwrap_or_default().to_owned());
}
drop(chunk);

assert_eq!(cities, ["Boston", "Sydney"]);
# Ok::<(), coseva::Error>(())
```

## Formats

### Declare a house format as a constant

Every `FormatOptions` constructor and setter is `const`.

```rust
use coseva::config::FormatOptions;

const UPSTREAM: FormatOptions = FormatOptions::CSV
    .delimiter(b'|')
    .quote(b'\'')
    .comment(Some(b'#'));
```

### Specialize the parser for a custom format

You do not need this for CSV or TSV — a parser recognises those from its
options and specializes itself. It pays only for a dialect the crate cannot
recognise, and mostly on quote-heavy data.

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::csv_format;

csv_format! {
    /// Our upstream system's pipe-delimited export.
    pub Upstream = FormatOptions::CSV.delimiter(b'|');
}

let mut parser = SliceParser::<Upstream>::new(
    "Boston|650706\n",
    ParseOptions::new().headers(Headers::None),
)?;

let mut line = parser.next_line()?.expect("a record");
assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
# Ok::<(), coseva::Error>(())
```

Declaring a format also validates it: a delimiter that is also the quote byte
fails to **compile** rather than failing when the first parser is built.

## Errors

### Report exactly where a file went wrong

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;

let mut parser = SliceParser::<Csv>::new("city,population\nBoston,650706\nBris\"tol,467099\n", ParseOptions::new()).expect("parser");
let mut failure = None;
while failure.is_none() {
    match parser.next_line() {
        Ok(Some(mut line)) => failure = line.record().err(),
        Ok(None) => break,
        Err(error) => failure = Some(error),
    }
}

let error = failure.expect("the third record is malformed");
let at = error.location();
println!("line {}, byte {}, record {}: {error}", at.line, at.byte, at.record);
assert_eq!(at.record, 2);
# Ok::<(), coseva::Error>(())
```

### Get a full location on a conversion failure

Convert *through* the parser. `Record::parse` knows only the field index it
was handed, because a bare record does not know where it came from.

```rust
use coseva::format::Csv;
use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(Debug, CsvDecode)]
struct Row {
    population: u64,
}

let mut parser = SliceParser::<Csv>::new("population\nnot-a-number\n", ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("a record");
let error = line.decoded::<Row>().expect_err("not a number");

assert_eq!(error.location().record, 1);
assert_eq!(error.field_name(), Some("population"));
# Ok::<(), coseva::Error>(())
```

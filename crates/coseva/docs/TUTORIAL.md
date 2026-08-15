# coseva tutorial

This is the connecting narrative for the crate. It starts from nothing and
builds to the point where the rest of the documentation is useful on its own.
Every code block here is compiled and run as a doc test, so nothing on this
page can drift away from the API.

If you want a runnable program per topic instead of prose, `examples/` has
one for each, and `COOKBOOK.md` answers "how do I do X" without the story.

One dataset runs through the whole tutorial:

```text
city,country,population,coastal
Boston,US,650706,true
"Washington, D.C.",US,689545,false
Sydney,AU,5312163,true
```

Note the second record. It contains a comma inside a quoted field, which is
the entire reason CSV cannot be read with `split(',')`.

## The shape of the crate

Three ideas carry most of the design, and knowing them makes the API
predictable rather than something to memorise.

**One format type, two directions.** `FormatOptions` describes a CSV
*dialect* — delimiter, quoting, escaping, comments, NULL conventions — and the
same value configures both reading and writing. `ParseOptions` and
`EmitOptions` describe what you want *done* with that dialect, such as
whether the first record is a header row.

**Three parsers, one engine.** They differ only in where the bytes come from:

| Parser | Source | Use when |
|---|---|---|
| `SliceParser` | bytes you already hold | the document is in memory |
| `IoParser` | any `Read` | the file is large, or is a stream |
| `PushParser` | you lend it chunks | you own the read loop — async, a socket, a decompressor |

They accept identical input and produce identical records and identical
errors. Choosing between them is a question about your I/O, not about CSV.

**Records lend.** Reading is a two-step: get a *line*, then ask it for a
*record*. That looks like a formality and is actually the whole performance
story, so it gets its own section below.

## Reading your first document

`SliceParser` borrows the bytes you give it:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::SliceParser;

const DATA: &str = "\
city,country,population,coastal
Boston,US,650706,true
\"Washington, D.C.\",US,689545,false
Sydney,AU,5312163,true
";

let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");

while let Some(mut line) = parser.next_line()? {
    let record = line.record()?;
    println!("{:?}", record.get_str(0)?);
}
# Ok::<(), coseva::Error>(())
```

That prints three cities, not four: **the first record is consumed as headers
by default**. And it prints `Washington, D.C.` as one field with the quotes
removed — the parser has already decoded it.

`get_str` validates UTF-8 and hands back a `&str`. `get` gives you the raw
`&[u8]` if you would rather not pay for validation, and `parse` converts
straight out of the bytes:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
# use coseva::SliceParser;
# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\n";
# let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let mut line = parser.next_line()?.expect("one data record");
let record = line.record()?;

assert_eq!(record.get(0), Some(&b"Boston"[..]));
assert_eq!(record.get_str(1)?, Some("US"));
assert_eq!(record.parse::<u64>(2)?, Some(650_706));
assert_eq!(record.parse::<bool>(3)?, Some(true));
# Ok::<(), coseva::Error>(())
```

Each accessor returns `Option`, because a record may be shorter than you
expect. `None` means "this record had no such field", which is a different
thing from an empty field, and different again from a NULL.

## Why records lend

The two-step — `next_line()`, then `line.record()` — exists so that a field
can be a slice *of your input* rather than a copy of it. The record borrows
from the parser, which borrows from the input, so nothing is allocated and
nothing is copied for an ordinary field.

The consequence is that a record cannot outlive the step that produced it.
This does not compile, and should not:

```rust,compile_fail
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
# use coseva::SliceParser;
# let mut parser = SliceParser::<Csv>::new("a\n1\n2\n", ParseOptions::new()).expect("parser");
let mut kept = Vec::new();
while let Some(mut line) = parser.next_line()? {
    kept.push(line.record()?); // error: borrowed value does not live long enough
}
# Ok::<(), coseva::Error>(())
```

When you need records to outlive the loop, ask for owned ones. `ByteRecord`
owns its bytes:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::{ByteRecord, SliceParser};

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");

let cities: Vec<ByteRecord> = parser
    .byte_records()
    .collect::<Result<_, coseva::Error>>()?;

assert_eq!(cities.len(), 2);
# Ok::<(), coseva::Error>(())
```

There is a middle path worth knowing, because it is the one to reach for in a
hot loop. `read_byte_record_into` fills a record you own, reusing its buffers
instead of allocating a new one per row:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::{ByteRecord, SliceParser};

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let mut record = ByteRecord::new();
let mut total = 0_u64;

while let Some(mut line) = parser.next_line()? {
    line.read_byte_record_into(&mut record)?;
    total += record.get_str(2)?.unwrap_or("0").parse::<u64>().unwrap_or(0);
}

assert_eq!(total, 5_962_869);
# Ok::<(), coseva::Error>(())
```

After the first few records that loop stops allocating entirely: the record's
buffers are already big enough, so each row overwrites them.

So the ladder, cheapest first: **borrow** with `line.record()`, **reuse** with
`read_byte_record_into`, **own** with `byte_records()`. Start at the top and
move down only when the borrow checker or your design asks you to.

## Naming columns instead of counting them

Hard-coded column numbers rot the first time someone adds a column. Resolve
the name once, before the scan:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::SliceParser;

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\n\"Washington, D.C.\",US,689545,false\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let population = parser.header_index("population")?.expect("a population column");

let mut largest = 0_u64;
while let Some(mut line) = parser.next_line()? {
    largest = largest.max(line.record()?.parse::<u64>(population)?.unwrap_or(0));
}

assert_eq!(largest, 5_312_163);
# Ok::<(), coseva::Error>(())
```

`header_index` is resolved once and costs nothing per record, which is why it
is preferred over searching the header row inside the loop.

## Decoding into your own types

Reaching fields by index is fine for a quick scan and tiresome for a program.
With the `derive` feature, a struct can describe the row and the parser will
fill it in:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct City {
    city: String,
    country: String,
    population: u64,
    coastal: bool,
}

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\n\"Washington, D.C.\",US,689545,false\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let cities: Vec<City> = parser
    .decoded_records::<City>()
    .collect::<Result<_, coseva::Error>>()?;

assert_eq!(cities.len(), 3);
assert_eq!(cities[1].city, "Washington, D.C.");
assert_eq!(cities[2].population, 5_312_163);
# Ok::<(), coseva::Error>(())
```

Fields bind **by header name, not by position**, so a column moving in the
file changes nothing, and columns your struct does not mention are ignored.

Now the part worth slowing down for. That struct has `String` fields, so each
row allocates. Give the struct a lifetime and `&str` fields instead, and
decoding a row copies nothing at all — the strings point into the parser's
buffer:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::SliceParser;
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct CityRef<'row> {
    city: &'row str,
    population: u64,
}

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let mut total = 0_u64;

while let Some(mut line) = parser.next_line()? {
    let row: CityRef<'_> = line.decoded()?;
    total += row.population;
}

assert_eq!(total, 5_962_869);
# Ok::<(), coseva::Error>(())
```

This is the same trade as the previous section, in typed form: `decoded()`
inside the loop borrows and is free; `decoded_records()` owns and can be
collected. A borrowing struct cannot be collected into a `Vec`, for exactly
the reason a `Record` cannot.

Four attributes cover the awkward cases:

```rust
use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    /// Bind to a column spelled differently from the field.
    #[csv(rename = "swatch_name")]
    name: String,
    /// Missing or empty input becomes `Default::default()` rather than an error.
    #[csv(default)]
    opacity: f32,
    /// Never read from the document at all.
    #[csv(skip)]
    seen: bool,
}
```

The fourth, `#[csv(parse_with = "...")]`, names a function that turns raw
bytes into your field type — the escape hatch for a type the crate has never
heard of. `examples/typed_decode.rs` runs all four.

## When something goes wrong

"Invalid CSV" is not an actionable bug report for a 4 GB file, so every error
carries a `Location`: byte offset, line, record and field.

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::SliceParser;

// The third record opens a quote in the middle of a field.
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
let location = error.location();
assert_eq!(location.record, 2);
println!("line {}, byte {}: {}", location.line, location.byte, error);
# Ok::<(), coseva::Error>(())
```

Errors arrive from two places, and the difference matters. An error raised
*through the parser* — `next_line`, `line.record()`, `line.decoded()` — knows
where it is. An error from `Record::parse` knows only the field index you
handed it, because a bare record has no idea where it came from. If you want
full locations on conversion failures, decode through the parser rather than
converting fields yourself.

Strictness is a policy, not a fact. If your input is known-imperfect and you
would rather read it than reject it, `Syntax::Compatible` accepts what a
strict reader refuses; `examples/errors.rs` walks the recovery options.

## When the file does not fit in memory

Everything so far used `SliceParser`, which needs the whole document. Swap in
`IoParser` and the loop is unchanged:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use std::io::Cursor;

use coseva::IoParser;

# let file = Cursor::new(b"city,country,population,coastal\nBoston,US,650706,true\nSydney,AU,5312163,true\n".to_vec());
// Any `Read`: a `File`, a socket, a decompressor. `Cursor` stands in here.
let mut parser = IoParser::<_, Csv>::new(file, ParseOptions::new()).expect("parser");

let mut count = 0;
while let Some(mut line) = parser.next_line()? {
    let _record = line.record()?;
    count += 1;
}

assert_eq!(count, 2);
# Ok::<(), Box<dyn std::error::Error>>(())
```

Memory stays bounded by the buffer and the largest single record, not by the
file. Records still borrow — from the parser's buffer now rather than from
your input — so the same lending rules apply, and the same ladder of borrow,
reuse, own.

The one thing streaming cannot do is hand you a field that outlives the
buffer refill that overwrote it, which is the rule the borrow checker was
already enforcing.

## When you do not control the read loop

Async runtimes, callback-driven transports and decompressors all want to hand
*you* bytes rather than be read from. `PushParser` inverts the loop: you lend
it whatever arrives, and drain whatever records that completed.

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::PushParser;

let mut parser = PushParser::<Csv>::new(ParseOptions::new()).expect("parser");
let chunks: [&[u8]; 3] = [b"city,pop\nBos", b"ton,650706\nSyd", b"ney,5312163\n"];

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

parser.finish();
let mut chunk = parser.chunk(b"");
while let Some(mut line) = chunk.next_line()? {
    cities.push(line.record()?.get_str(0)?.unwrap_or_default().to_owned());
}
drop(chunk);

assert_eq!(cities, ["Boston", "Sydney"]);
# Ok::<(), coseva::Error>(())
```

Two details make this correct rather than nearly correct. `done` returns how
many bytes the parser *took*, which may be fewer than you offered, so the loan
belongs in a loop. And `finish` matters: without it a final record not
terminated by a newline is still buffered, waiting for more input that will
never come. Note also that records are read straight out of the slice you lent,
so the only thing copied is a record a chunk boundary cut in half.

Note that no I/O appears anywhere above. That is the point — `PushParser` is a
state machine, so it works under any runtime without the crate knowing which.

## Reading less

The fastest way to parse a field is to not parse it. Two tools push work down
into the scan instead of doing it in your loop.

A **predicate** tests one column's raw bytes while scanning. Records that fail
are never split into fields, never unescaped and never converted:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::{Predicate, SliceParser};

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\n\"Washington, D.C.\",US,689545,false\nSydney,AU,5312163,true\n";
let predicate = Predicate::equals("country", "US");
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");

let mut matched = 0;
while let Some(mut line) = parser.next_matching_line(&predicate)? {
    let _record = line.record()?;
    matched += 1;
}

assert_eq!(matched, 2);
# Ok::<(), coseva::Error>(())
```

A **projection** names the columns you want once, and yields just those:

```rust
# use coseva::format::Csv;
# use coseva::config::ParseOptions;
use coseva::{FieldProjection, SliceParser};

# const DATA: &str = "city,country,population,coastal\nBoston,US,650706,true\nSydney,AU,5312163,true\n";
let mut parser = SliceParser::<Csv>::new(DATA, ParseOptions::new()).expect("parser");
let headers = parser.headers()?.expect("headers").clone();
let projection = FieldProjection::from_headers(&headers, ["city", "population"])?;

while let Some(mut line) = parser.next_line()? {
    let record = line.record()?;
    let picked: Vec<&[u8]> = record.project(&projection).flatten().collect();
    assert_eq!(picked.len(), 2);
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

Both scale with how much you discard. Filtering in your own loop costs the
same no matter what, because you paid to parse the record before you could
test it; pushing the test down means you never paid.

## Writing

Encoding mirrors reading: one core does quoting and escaping, and three front
ends decide where the bytes land — `IoEmitter` to any `Write`, `VecEmitter` to a
`Vec<u8>`, `PushEmitter` to wherever you route it yourself.

```rust
use coseva::VecEmitter;

let mut emitter = VecEmitter::default();
emitter.emit_record(["city", "country", "population"])?;
emitter.emit_record(["Boston", "US", "650706"])?;
emitter.emit_record(["Washington, D.C.", "US", "689545"])?;

let out = String::from_utf8(emitter.as_bytes().to_vec()).expect("utf-8");
assert!(out.contains("\"Washington, D.C.\""));
# Ok::<(), coseva::Error>(())
```

Quoting is applied only where it is needed, which is both the cheapest and the
most readable choice. Set `Quoting::Always` or `Quoting::NonNumeric` when
something downstream insists.

Structs work here too, and the header row is generated from the field names,
so it cannot fall out of step with the values written under it:

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
emitter.encode_all([
    City { city: "Boston", population: 650_706 },
    City { city: "Sydney", population: 5_312_163 },
])?;

let out = String::from_utf8(emitter.as_bytes().to_vec()).expect("utf-8");
assert_eq!(out, "city,population\nBoston,650706\nSydney,5312163\n");
# Ok::<(), coseva::Error>(())
```

## Reading a format that is not CSV

`FormatOptions` carries the dialect, and the common ones are constants:

```rust
use coseva::SliceParser;
use coseva::config::{FormatOptions, ParseOptions};

let mut parser = SliceParser::with_options(
    "city\tpopulation\nBoston\t650706\n",
    FormatOptions::TSV,
    ParseOptions::new(),
)?;

let mut line = parser.next_line()?.expect("one record");
assert_eq!(line.record()?.get_str(0)?, Some("Boston"));
# Ok::<(), coseva::Error>(())
```

`FormatOptions::CSV`, `TSV`, `SEMICOLON` and `PIPE` cover most of what you
will meet. Every constructor and setter is `const`, so a house format is a
constant rather than something rebuilt per file:

```rust
use coseva::config::FormatOptions;

const UPSTREAM: FormatOptions = FormatOptions::CSV
    .delimiter(b'|')
    .comment(Some(b'#'));
```

You do not need to do anything to make the common formats fast. A parser
recognises the format it was built with and specialises itself, so CSV and TSV
run a constant-folded kernel whether the format came from a constant or from a
command-line flag. Naming a format at the *type* level, with `csv_format!`,
is a further step that only pays for a custom dialect; the crate docs cover it
and `COOKBOOK.md` shows the shape.

## Where to go next

- **`COOKBOOK.md`** — the same material as recipes, plus NULL conventions,
  headerless input, random access by record number, and Serde.
- **`examples/`** — a runnable program per topic. Start with `quickstart`,
  then `typed_decode`, then whichever matches your problem.
- **`docs/DESIGN.md`** — how the parser actually works, if you want to change
  it or trust it.
- **`benches/`** — measured numbers, including where the wins are *not*. Each
  suite's module documentation carries the table its code produced.

A closing note on performance, since it is the reason this crate exists.
Nothing in this tutorial asked you to opt into a fast path. Borrowing rather
than owning, resolving headers once, pushing filters into the scan and
letting the parser specialise itself are all just the ordinary way to use the
API. The tuning knobs are there when you need them, but the default path is
the one that was optimised.

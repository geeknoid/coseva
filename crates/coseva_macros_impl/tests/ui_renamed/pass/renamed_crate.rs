//! The `coseva` package is depended on under the name `renamed_coseva` (see
//! this crate's `Cargo.toml`). Binding it back to the crate-root name `coseva`
//! is all a downstream consumer needs for the derive's absolute `::coseva::…`
//! paths and the exported `csv_format!` macro to resolve.

extern crate renamed_coseva as coseva;

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::csv_format;
use coseva::encoding::{CsvDecode, CsvEncode};
use coseva::format::StaticFormat;
use coseva::SliceParser;

#[derive(CsvDecode, CsvEncode)]
struct City {
    name: String,
    population: u64,
}

csv_format! {
    pub Pipes = FormatOptions::CSV.delimiter(b'|');
}

fn main() {
    assert_eq!(<City as CsvDecode>::field_names(), &["name", "population"]);
    assert_eq!(<City as CsvEncode>::field_names(), &["name", "population"]);
    assert_eq!(Pipes::FORMAT, FormatOptions::CSV.delimiter(b'|'));

    let mut parser = SliceParser::<Pipes>::new(
        b"left|right\n",
        ParseOptions::new().headers(Headers::None),
    )
    .expect("parser");
    let mut line = parser.next_line().expect("read").expect("record");
    let record = line.record().expect("record");
    assert_eq!(record.get(0), Some(&b"left"[..]));
    assert_eq!(record.get(1), Some(&b"right"[..]));
}

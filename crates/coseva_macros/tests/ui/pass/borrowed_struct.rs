//! A struct with one lifetime borrows its string and byte fields straight from
//! the record, and the derive rewrites that lifetime to the row lifetime.

use coseva::config::{Headers, ParseOptions};
use coseva::encoding::CsvDecode;
use coseva::format::Csv;
use coseva::SliceParser;

#[derive(CsvDecode)]
struct Borrowed<'a> {
    name: &'a str,
    raw: &'a [u8],
}

fn main() {
    assert_eq!(<Borrowed as CsvDecode>::field_names(), &["name", "raw"]);

    let mut parser = SliceParser::<Csv>::new(
        b"Boston,MA\n",
        ParseOptions::new().headers(Headers::None),
    )
    .expect("parser");
    let mut line = parser.next_line().expect("read").expect("record");
    let record = line.record().expect("record");
    let row = Borrowed::csv_decode(&record).expect("decode");
    assert_eq!(row.name, "Boston");
    assert_eq!(row.raw, b"MA");
}

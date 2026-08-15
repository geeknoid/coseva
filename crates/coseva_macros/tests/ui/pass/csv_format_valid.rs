//! Valid downstream `csv_format!` declarations expand, implement the sealed
//! `StaticFormat`, and drive a parser.

use coseva::config::{FormatOptions, Headers, ParseOptions};
use coseva::csv_format;
use coseva::format::StaticFormat;
use coseva::SliceParser;

csv_format! {
    /// A downstream pipe-delimited format.
    pub Pipes = FormatOptions::CSV.delimiter(b'|');
    /// A second declaration in the same invocation, with a private visibility.
    Tabs = FormatOptions::CSV.delimiter(b'\t');
}

fn main() {
    // The declared format carries its validated options at the type level.
    assert_eq!(Pipes::FORMAT, FormatOptions::CSV.delimiter(b'|'));
    assert_eq!(Tabs::FORMAT, FormatOptions::CSV.delimiter(b'\t'));

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

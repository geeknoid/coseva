//! A `csv_format!` declaration whose options are unusable fails to compile at
//! the declaration, not when a parser is later built. Here the delimiter is
//! also the quote byte.

use coseva::config::FormatOptions;
use coseva::csv_format;

csv_format! {
    pub Broken = FormatOptions::CSV.delimiter(b'"');
}

fn main() {}

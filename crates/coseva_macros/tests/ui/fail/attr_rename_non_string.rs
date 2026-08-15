//! `rename` requires a *string* value, not an integer literal.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(rename = 7)]
    value: u32,
}

fn main() {}

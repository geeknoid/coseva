//! `rename` requires a string value.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(rename)]
    value: u32,
}

fn main() {}

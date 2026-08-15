//! `parse_with` requires a string value naming a function path.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(parse_with)]
    value: u32,
}

fn main() {}

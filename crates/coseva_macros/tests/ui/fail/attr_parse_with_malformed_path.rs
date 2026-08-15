//! `parse_with` must name a path; an arbitrary expression is rejected.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(parse_with = "1 + 1")]
    value: u32,
}

fn main() {}

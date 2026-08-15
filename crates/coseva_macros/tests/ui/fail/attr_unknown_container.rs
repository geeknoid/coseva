//! An unrecognized container attribute is rejected.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
#[csv(bogus)]
struct Row {
    value: u32,
}

fn main() {}

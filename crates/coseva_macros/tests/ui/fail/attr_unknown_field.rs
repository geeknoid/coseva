//! An unrecognized field attribute is rejected at its own span.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(bogus)]
    value: u32,
}

fn main() {}

//! `format_with` must name a path; an arbitrary expression is rejected.

use coseva::encoding::CsvEncode;

#[derive(CsvEncode)]
struct Row {
    #[csv(format_with = "1 + 1")]
    value: u32,
}

fn main() {}

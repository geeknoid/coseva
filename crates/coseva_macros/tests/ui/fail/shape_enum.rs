//! Enums are not a supported derive shape.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
enum Choice {
    A,
    B,
}

fn main() {}

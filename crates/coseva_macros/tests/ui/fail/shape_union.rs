//! Unions are not a supported derive shape.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
union Overlap {
    a: u32,
    b: f32,
}

fn main() {}

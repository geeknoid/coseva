//! A unit struct has no fields to map to columns.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Unit;

fn main() {}

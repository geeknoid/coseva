//! A repeated value-less flag such as `default` is rejected, not treated as
//! idempotent.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(default, default)]
    a: u32,
}

fn main() {}

//! A repeated single-valued `#[csv(...)]` key is rejected instead of silently
//! keeping the last value.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(rename = "first", rename = "second")]
    a: String,
}

fn main() {}

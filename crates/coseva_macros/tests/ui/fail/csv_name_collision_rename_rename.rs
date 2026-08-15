//! Two fields renamed to the same CSV name are rejected: the generated header
//! would repeat a column, and nothing could read the file back.

use coseva::encoding::CsvEncode;

#[derive(CsvEncode)]
struct Row {
    #[csv(rename = "shared")]
    a: String,
    #[csv(rename = "shared")]
    b: String,
}

fn main() {}

//! `alias` affects decoding only; `CsvEncode` rejects it because a column is
//! written under a single name.

use coseva::encoding::CsvEncode;

#[derive(CsvEncode)]
struct Row {
    #[csv(alias = "town")]
    city: String,
}

fn main() {}

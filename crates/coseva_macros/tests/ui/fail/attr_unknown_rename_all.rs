//! An unknown `rename_all` rule is rejected and the accepted spellings are
//! listed in the diagnostic.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
#[csv(rename_all = "SpongeCase")]
struct Row {
    first_name: String,
}

fn main() {}

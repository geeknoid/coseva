extern crate renamed_coseva as coseva;

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(skip, alias = "first", alias = "second")]
    value: u32,
}

fn main() {}

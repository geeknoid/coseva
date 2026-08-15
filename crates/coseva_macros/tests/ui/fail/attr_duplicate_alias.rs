//! `alias` is repeatable, but the same spelling twice is a mistake and is
//! rejected; distinct aliases (see `pass/decode_alias.rs`) still work.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(alias = "town", alias = "town")]
    city: String,
}

fn main() {}

//! `parse_with` already produces the decoded value, so a `default` beside it
//! would never run; the combination is rejected as a conflict.

use coseva::encoding::CsvDecode;

fn parse_it(_raw: &[u8]) -> Result<u32, std::convert::Infallible> {
    Ok(0)
}

#[derive(CsvDecode)]
struct Row {
    #[csv(default, parse_with = "parse_it")]
    a: u32,
}

fn main() {}

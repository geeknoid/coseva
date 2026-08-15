//! `parse_with` must name `fn(&[u8]) -> Result<T, E>`. A function taking the
//! wrong argument type is a compile error inside the generated decode body.

use coseva::encoding::CsvDecode;

fn parses_an_int(_value: u32) -> Result<u32, std::convert::Infallible> {
    Ok(0)
}

#[derive(CsvDecode)]
struct Row {
    #[csv(parse_with = "parses_an_int")]
    value: u32,
}

fn main() {
    let _ = <Row as CsvDecode>::field_names();
}

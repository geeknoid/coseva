//! Every field-shaping attribute the derive documents, exercised on a single
//! struct so a regression in any one of them fails to compile.

use coseva::encoding::{CsvDecode, CsvEncode};

fn parse_flag(raw: &[u8]) -> Result<bool, std::str::Utf8Error> {
    Ok(std::str::from_utf8(raw)?.trim() == "yes")
}

fn format_flag(flag: &bool) -> &'static str {
    if *flag {
        "yes"
    } else {
        "no"
    }
}

#[derive(CsvDecode, CsvEncode)]
#[csv(rename_all = "PascalCase")]
struct Row {
    // `rename` overrides the container rule.
    #[csv(rename = "City")]
    city_name: String,
    // The container rule supplies this header.
    population: u64,
    // `default` fills the column when it is absent or empty.
    #[csv(default)]
    elevation: i32,
    // Custom decode and encode hooks on one field.
    #[csv(parse_with = "parse_flag", format_with = "format_flag")]
    capital: bool,
    // Excluded from CSV in both directions.
    #[csv(skip)]
    internal: usize,
}

fn main() {
    assert_eq!(
        <Row as CsvDecode>::field_names(),
        &["City", "Population", "Elevation", "Capital"]
    );
    assert_eq!(
        <Row as CsvEncode>::field_names(),
        &["City", "Population", "Elevation", "Capital"]
    );
}

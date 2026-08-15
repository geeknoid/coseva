//! Decode-only attributes: repeated `alias` entries and the interaction with
//! `rename`. `alias` is rejected by `CsvEncode`, so this fixture derives only
//! `CsvDecode`.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    // A renamed column that also accepts two legacy spellings on decode.
    #[csv(rename = "pop", alias = "population", alias = "inhabitants")]
    count: u64,
    // A field with a single alias and no rename keeps its own name too.
    #[csv(alias = "town")]
    city: String,
}

fn main() {
    // Aliases do not change the canonical header names.
    assert_eq!(<Row as CsvDecode>::field_names(), &["pop", "city"]);
    // The alias table is emitted parallel to the field names.
    assert_eq!(
        <Row as CsvDecode>::field_aliases(),
        &[
            &["population", "inhabitants"] as &[&str],
            &["town"] as &[&str],
        ]
    );
}

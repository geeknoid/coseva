//! A plain named struct derives both directions and both impls are usable.

use coseva::encoding::{CollectVisitor, CsvDecode, CsvEncode};

#[derive(CsvDecode, CsvEncode)]
struct City {
    name: String,
    population: u64,
}

fn main() {
    // Both derives publish the same column order.
    assert_eq!(<City as CsvDecode>::field_names(), &["name", "population"]);
    assert_eq!(<City as CsvEncode>::field_names(), &["name", "population"]);

    let city = City {
        name: "Boston".to_owned(),
        population: 650_706,
    };
    let mut visitor = CollectVisitor::new();
    city.csv_encode(&mut visitor).expect("encode");
}

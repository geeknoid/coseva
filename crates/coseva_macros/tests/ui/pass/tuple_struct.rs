//! A tuple struct derives with positional column names.

use coseva::encoding::{CsvDecode, CsvEncode};

#[derive(CsvDecode, CsvEncode)]
struct Pair(String, u32);

fn main() {
    assert_eq!(<Pair as CsvDecode>::field_names(), &["0", "1"]);
    assert_eq!(<Pair as CsvEncode>::field_names(), &["0", "1"]);
}

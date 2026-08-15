//! A struct generic over a type and a const parameter, with a where-clause,
//! derives both directions. The type parameter is reached only through a
//! skipped field, so decoding needs just the `Default` bound the struct
//! already declares and no unnameable row-lifetime bound arises.

use coseva::encoding::{CsvDecode, CsvEncode};

#[derive(CsvDecode, CsvEncode)]
struct Row<T, const COLS: usize>
where
    T: Default,
{
    id: u64,
    label: String,
    #[csv(skip)]
    _extra: T,
}

fn main() {
    assert_eq!(<Row<u8, 3> as CsvDecode>::field_names(), &["id", "label"]);
    assert_eq!(<Row<u8, 3> as CsvEncode>::field_names(), &["id", "label"]);
}

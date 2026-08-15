//! A generic field decoded directly needs a `DecodeField` bound the caller
//! cannot spell, because the derive introduces its own row lifetime. Without a
//! satisfying bound the impl fails to type-check.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row<T> {
    value: T,
}

fn main() {
    let _ = <Row<u8> as CsvDecode>::field_names();
}

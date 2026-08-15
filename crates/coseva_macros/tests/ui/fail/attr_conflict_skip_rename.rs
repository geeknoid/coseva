//! A `skip`ped field carries no CSV column, so pairing it with a
//! column-shaping attribute is a conflict rather than a silent no-op.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    #[csv(skip, rename = "kept")]
    hidden: u32,
    shown: u32,
}

fn main() {}

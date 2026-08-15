//! A field-level attribute placed on the container is not a container
//! attribute, so it is rejected as unsupported there.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
#[csv(skip)]
struct Row {
    value: u32,
}

fn main() {}

//! `format_with` must return something `AsRef<[u8]>`. A function returning a
//! bare integer is a compile error inside the generated encode body.

use coseva::encoding::CsvEncode;

fn formats_to_int(_value: &u32) -> u32 {
    0
}

#[derive(CsvEncode)]
struct Row {
    #[csv(format_with = "formats_to_int")]
    value: u32,
}

fn main() {
    let _ = <Row as CsvEncode>::field_names();
}

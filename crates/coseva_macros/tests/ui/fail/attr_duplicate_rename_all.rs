//! The container `rename_all` may appear only once; a second occurrence is
//! rejected rather than silently overriding the first.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
#[csv(rename_all = "snake_case", rename_all = "kebab-case")]
struct Row {
    first_name: String,
}

fn main() {}

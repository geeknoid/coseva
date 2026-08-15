//! A `rename` colliding with another field's default name is rejected for the
//! same reason as two explicit renames: the collision is what matters, not how
//! either name was arrived at.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Row {
    a: String,
    #[csv(rename = "a")]
    b: String,
}

fn main() {}

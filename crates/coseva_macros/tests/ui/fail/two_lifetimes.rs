//! The derive borrows through a single row lifetime, so a second lifetime
//! parameter has no meaning it could give.

use coseva::encoding::CsvDecode;

#[derive(CsvDecode)]
struct Two<'a, 'b> {
    a: &'a str,
    b: &'b str,
}

fn main() {}

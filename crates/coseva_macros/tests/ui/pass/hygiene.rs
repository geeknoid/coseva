//! The derive must expand hygienically: a downstream module that shadows the
//! standard names it relies on, and even declares local modules that collide
//! with the crate roots it references, must not change what it generates. The
//! derive names everything through absolute `::core` and `::coseva` paths, so
//! none of the hostile items below can reach its expansion.

use ::coseva::encoding::{CsvDecode, CsvEncode};

// Shadow the prelude names the generated code would otherwise pick up.
#[allow(dead_code)]
type Result = ();
#[allow(dead_code)]
type Option = ();
#[allow(dead_code)]
struct Ok;
#[allow(dead_code)]
struct Some;
#[allow(dead_code)]
struct None;
#[allow(dead_code)]
struct Err;
#[allow(dead_code)]
trait Default {}
#[allow(dead_code)]
trait AsRef {}

// Local modules whose names collide with the crate roots the derive names.
// Absolute paths ignore them, so expansion is unaffected.
mod core {}
mod std {}
mod coseva {}

#[derive(CsvDecode, CsvEncode)]
struct Row {
    a: u32,
    b: String,
}

fn main() {
    assert_eq!(<Row as CsvDecode>::field_names(), &["a", "b"]);
    assert_eq!(<Row as CsvEncode>::field_names(), &["a", "b"]);
}

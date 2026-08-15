//! A field whose type does not implement `DecodeField` cannot be decoded; the
//! derive's generated bound surfaces as an unsatisfied trait bound at the use
//! site.

use coseva::encoding::CsvDecode;

struct NotDecodable;

#[derive(CsvDecode)]
struct Row {
    value: NotDecodable,
}

fn main() {
    let _ = <Row as CsvDecode>::field_names();
}

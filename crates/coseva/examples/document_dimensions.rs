//! Print the dimensions of the benchmark documents, for `scripts/perf_report.rs`.
//!
//! The Callgrind output the matrix suite leaves behind carries instruction
//! counts and nothing else, so a report built from it alone could only publish
//! totals. Totals are useless across documents that differ in size by a factor
//! of ten — `prose` has a few thousand large records where `metrics` has tens
//! of thousands of small ones — so the report needs each document's record
//! count and byte length to normalise them.
//!
//! Those numbers exist in exactly one place, `benches/documents.rs`, and this
//! prints them from that place rather than restating them. If the generator
//! changes, the report follows automatically; there is no table to keep in
//! step, which is the failure that made the previous report generator
//! unreliable enough to delete.
//!
//! Run it directly to see the corpus dimensions:
//!
//! ```text
//! cargo run --example document_dimensions --all-features
//! ```

#[path = "../benches/documents.rs"]
mod documents;

use documents::{BUDGET_BYTES, DOCUMENTS};

fn main() {
    println!("# name\tbytes\trecords\tfield_bytes\tvalue_sum\tbudget={BUDGET_BYTES}");
    for document in DOCUMENTS.iter() {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            document.name,
            document.bytes.len(),
            document.records,
            document.field_bytes,
            document.value_sum
        );
    }
}

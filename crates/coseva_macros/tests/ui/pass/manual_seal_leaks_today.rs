//! CON9 characterization: `StaticFormat` is only *pseudo*-sealed.
//!
//! A manual `StaticFormat` / `sealed::Sealed` implementation cannot currently
//! be rejected honestly on stable Rust: the exported `csv_format!` macro must expand in the
//! downstream crate and emit `impl $crate::format::sealed::Sealed for $name`,
//! so `sealed::Sealed` has to be a `pub` item reachable from any downstream
//! crate. Anything a downstream-expanded macro can name, hand-written
//! downstream code can name too — there is no privilege escalation across macro
//! hygiene. A coseva-owned generic carrier (the only design that keeps every
//! `Sealed` impl inside coseva) would require carrying a whole `FormatOptions`
//! value as a const-generic parameter, which needs the unstable
//! `adt_const_params` feature, so it is not available on stable either.
//!
//! This fixture therefore *passes*: it demonstrates the leak by hand-rolling
//! the exact impls `csv_format!` would generate. It is a deliberate tripwire —
//! if `StaticFormat` is ever genuinely sealed (closing CON9), this file will
//! stop compiling and must be moved to `fail/` with a snapshot.

use coseva::config::FormatOptions;
use coseva::format::{CsvFormat, StaticFormat};

struct Rogue;

impl CsvFormat for Rogue {
    const OPTIONS: Option<FormatOptions> = Some(FormatOptions::CSV);
}

// The "sealing" marker is public and hand-implementable — this is CON9.
impl coseva::format::sealed::Sealed for Rogue {}

impl StaticFormat for Rogue {
    const FORMAT: FormatOptions = FormatOptions::CSV;
}

fn main() {
    assert_eq!(<Rogue as StaticFormat>::FORMAT, FormatOptions::CSV);
}

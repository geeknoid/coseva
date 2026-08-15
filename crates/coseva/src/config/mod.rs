//! Formats, dialects, and parser and emitter options.
//!
//! [`FormatOptions`] describes byte-level CSV syntax for readers and writers;
//! presets cover common formats and `const` setters support house formats.
//! [`ParseOptions`] and [`EmitOptions`] describe an individual invocation:
//! headers, field counts, resource limits, and buffer sizing.
//!
//! ```
//! use coseva::config::{FormatOptions, Headers, Limits, ParseOptions};
//!
//! const HOUSE_FORMAT: FormatOptions = FormatOptions::TSV.comment(Some(b'#'));
//! let options = ParseOptions::new()
//!     .headers(Headers::None)
//!     .limits(Limits::new(1 << 20, 1 << 16, 512));
//! # let _ = (HOUSE_FORMAT, options);
//! ```

mod blank_records;
mod buffer_capacity;
mod dialect;
mod emit_options;
mod escape;
mod field_count;
mod format_options;
mod headers;
mod limits;
mod nulls;
mod parse_options;
mod quoting;
mod read_bom;
mod record_ending;
mod recovery;
mod syntax;
mod tail;
mod whitespace;
mod write_bom;

#[doc(inline)]
pub use blank_records::BlankRecords;
use buffer_capacity::{
    DEFAULT_READ_BUFFER_BYTES, DEFAULT_WRITE_BUFFER_BYTES, validate_buffer_capacity,
};
pub(crate) use dialect::Dialect;
#[doc(inline)]
pub use emit_options::EmitOptions;
#[doc(inline)]
pub use escape::Escape;
#[doc(inline)]
pub use field_count::FieldCount;
#[doc(inline)]
pub use format_options::FormatOptions;
pub(crate) use format_options::FormatTag;
#[doc(inline)]
pub use headers::Headers;
#[doc(inline)]
pub use limits::Limits;
#[doc(inline)]
pub use nulls::Nulls;
#[doc(inline)]
pub use parse_options::ParseOptions;
pub(crate) use parse_options::ParserSettings;
#[doc(inline)]
pub use quoting::Quoting;
#[doc(inline)]
pub use read_bom::ReadBom;
#[doc(inline)]
pub use record_ending::RecordEnding;
#[doc(inline)]
pub use recovery::Recovery;
#[doc(inline)]
pub use syntax::Syntax;
pub(crate) use tail::Tail;
#[doc(inline)]
pub use whitespace::Whitespace;
#[doc(inline)]
pub use write_bom::WriteBom;

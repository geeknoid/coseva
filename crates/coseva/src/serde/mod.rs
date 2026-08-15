//! Optional Serde compatibility layer.
//!
//! Enable the `serde` Cargo feature to deserialize records and serialize
//! values through Serde. Errors use the crate's unified [`crate::Error`].
//!
//! # Deserialization
//!
//! Scalars consume one field, tuples map positionally, and named structs map
//! by header when headers are present. `Option<T>` is `None` only for an absent
//! field; present empty bytes deserialize as `Some(T)`.
//! Unit enum variants match field bytes. `deserialize_any` yields borrowed
//! `&str` for valid UTF-8 and `&[u8]` otherwise, without type inference.
//! Nested maps and sequences inside one field are rejected.
//!
//! # Serialization
//!
//! Scalars produce one field, tuples and structs produce one field per
//! element, `None` produces an empty field, and unit enums produce the variant
//! name. With automatic headers disabled, nested sequences, tuples, and
//! structs are flattened depth-first. Automatic headers require scalar struct
//! fields. Maps and non-unit enum variants are rejected.
//!
//! Serialization validates a complete record before committing any bytes.
//!
//! ```
//! # use coseva::config::{FormatOptions, Headers, ParseOptions};
//! # use coseva::{SliceParser, VecEmitter};
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Debug, Deserialize, Serialize, PartialEq)]
//! struct City {
//!     name: String,
//!     population: u64,
//! }
//!
//! let csv = "name,population\nBoston,650706\n";
//! let mut parser = SliceParser::with_options(
//!     csv.as_bytes(),
//!     FormatOptions::CSV,
//!     ParseOptions::new().headers(Headers::FirstRecord),
//! )?;
//! let city: City = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected record"))?
//!     .deserialized()?;
//!
//! let mut out = VecEmitter::default();
//! out.serialize(&city)?;
//! assert_eq!(out.as_bytes(), b"name,population\nBoston,650706\n");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod deserializer;
mod record;
mod serializer;
mod struct_cache;
#[cfg(test)]
mod tests;

pub(crate) use record::{
    deserialize_byte_record, deserialize_full_record, deserialize_record, serialize_direct,
    serialize_direct_with_headers,
};
#[cfg(test)]
use record::{deserialize_byte_record_owned, serialize_to_record};
use record::{format_into_record, parse_error, utf8_error};
pub(crate) use struct_cache::StructCache;

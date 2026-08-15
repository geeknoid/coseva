//! Native typed CSV conversion.
//!
//! [`CsvDecode`] turns a record into a `T` and [`CsvEncode`] turns a `T` back
//! into a record. They are the layer above the parsers and emitters, which
//! deal only in bytes. Both can be derived, and both are driven by the same
//! `#[csv(...)]` attributes.
//!
//! # Decoding
//!
//! With the `derive` feature, [`CsvDecode`] can be derived. Fields map to CSV
//! columns positionally, and may be annotated with `#[csv(...)]` to control
//! that mapping:
//!
//! | Attribute | Effect |
//! |---|---|
//! | `rename = "name"` | Use `name` as the column header instead of the field name |
//! | `default` | Use [`Default::default()`] when the column is absent or empty |
//! | `skip` | Consume no column; decode as [`Default::default()`] |
//! | `parse_with = "fn"` | Call `fn(&[u8]) -> Result<T, E>` (where `E` implements `core::error::Error`) instead of [`DecodeField`] |
//!
//! ```
//! # #[cfg(all(feature = "std", feature = "derive"))] {
//! use coseva::config::{FormatOptions, Headers, ParseOptions};
//! use coseva::encoding::CsvDecode;
//! use coseva::SliceParser;
//!
//! #[derive(Debug)]
//! struct HexError;
//! impl core::fmt::Display for HexError {
//!     fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
//!         f.write_str("field is not a hexadecimal integer")
//!     }
//! }
//! impl core::error::Error for HexError {}
//!
//! fn parse_hex(bytes: &[u8]) -> Result<u32, HexError> {
//!     let text = core::str::from_utf8(bytes).map_err(|_error| HexError)?;
//!     u32::from_str_radix(text.trim_start_matches("0x"), 16).map_err(|_error| HexError)
//! }
//!
//! #[derive(CsvDecode)]
//! struct City {
//!     #[csv(rename = "city_name")]
//!     name: String,
//!     #[csv(default)]
//!     population: u64,
//!     #[csv(parse_with = "parse_hex")]
//!     color: u32,
//!     #[csv(skip)]
//!     visited: bool,
//! }
//!
//! assert_eq!(
//!     <City as CsvDecode>::field_names(),
//!     &["city_name", "population", "color"],
//! );
//!
//! let mut parser = SliceParser::with_options(
//!     b"London,,0x1f\n",
//!     FormatOptions::CSV,
//!     ParseOptions::new().headers(Headers::None),
//! )?;
//! let mut line = parser
//!     .next_line()?
//!     .ok_or_else(|| std::io::Error::other("expected one record"))?;
//! let record = line.record()?;
//! let city = City::csv_decode(&record)?;
//!
//! assert_eq!(city.name, "London");
//! assert_eq!(city.population, 0); // `default` fills the empty column
//! assert_eq!(city.color, 0x1f); // `parse_with` ran instead of `DecodeField`
//! assert!(!city.visited); // `skip` consumed no column
//! # }
//! # Ok::<(), Box<dyn core::error::Error>>(())
//! ```
//!
//! # Encoding
//!
//! With the `derive` feature, [`CsvEncode`] can be derived. Fields map to CSV
//! columns positionally, and may be annotated with `#[csv(...)]` to control
//! that mapping:
//!
//! | Attribute | Effect |
//! |---|---|
//! | `rename = "name"` | Use `name` as the column header instead of the field name |
//! | `skip` | Do not emit a column for this field |
//! | `format_with = "fn"` | Call `fn(&T) -> impl AsRef<[u8]>` instead of [`EncodeField`] |
//!
//! ```
//! # #[cfg(all(feature = "std", feature = "derive"))] {
//! use coseva::encoding::{CollectVisitor, CsvEncode};
//!
//! fn format_hex(value: &u32) -> Vec<u8> {
//!     format!("0x{value:02x}").into_bytes()
//! }
//!
//! #[derive(CsvEncode)]
//! struct City {
//!     #[csv(rename = "city_name")]
//!     name: String,
//!     #[csv(format_with = "format_hex")]
//!     color: u32,
//!     #[csv(skip)]
//!     visited: bool,
//! }
//!
//! assert_eq!(<City as CsvEncode>::field_names(), &["city_name", "color"]);
//!
//! let city = City {
//!     name: "London".to_owned(),
//!     color: 0x1f,
//!     visited: true,
//! };
//!
//! let mut visitor = CollectVisitor::new();
//! city.csv_encode(&mut visitor)?;
//!
//! // `skip` emitted no column, and `format_with` replaced `EncodeField`.
//! assert_eq!(visitor.fields(), &[b"London".to_vec(), b"0x1f".to_vec()]);
//! # }
//! # Ok::<(), Box<dyn core::error::Error>>(())
//! ```

mod csv_decode;
mod csv_encode;
mod decode_field;
mod decode_record;
mod encode_field;
mod encode_visitor;

#[doc(inline)]
pub use csv_decode::{CsvDecode, CsvDecodeOwned};
#[doc(inline)]
pub use csv_encode::CsvEncode;
#[doc(inline)]
pub use decode_field::{DecodeField, decode_field_or_default};
#[doc(inline)]
pub use decode_record::{ByteRecordRef, DecodeRecord, FusedFields, MappedRecord};
#[doc(inline)]
pub use encode_field::EncodeField;
#[doc(inline)]
pub use encode_visitor::{CollectVisitor, EncodeVisitor};

pub(crate) use csv_decode::{DecodeNew, DecodeSink};

#[cfg(feature = "derive")]
#[doc(inline)]
pub use coseva_macros::{CsvDecode, CsvEncode};

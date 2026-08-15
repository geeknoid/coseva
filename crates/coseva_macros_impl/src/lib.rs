#![forbid(unsafe_code)]
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, allow(unused_features))]
//! Implementation of the `coseva` typed record decoding and encoding derives.
//!
//! This crate holds the code-generation logic behind the `CsvDecode` and
//! `CsvEncode` derives exposed by the `coseva_macros` proc-macro crate. It is
//! an ordinary library so the expansion logic can be unit-tested directly;
//! it is not intended to be used on its own.
//! For runnable entry-point examples, see [`derive_csv_decode`] and
//! [`derive_csv_encode`].
//!
//! ## Supported shapes
//!
//! - Named structs: `struct Foo { field: Type }`
//! - Tuple structs: `struct Foo(Type, Type)`
//! - At most one lifetime parameter (used for borrowing field data)
//!
//! ## Container attributes (`#[csv(...)]`)
//!
//! | Attribute | Effect |
//! |---|---|
//! | `rename_all = "rule"` | Derive every column name from the field name by `rule` |
//!
//! The rules and their spellings are Serde's: `lowercase`, `UPPERCASE`,
//! `PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`,
//! `kebab-case` and `SCREAMING-KEBAB-CASE`. A field's own `rename` wins.
//!
//! ## Field attributes (`#[csv(...)]`)
//!
//! | Attribute | Effect |
//! |---|---|
//! | `rename = "name"` | Use `name` as the CSV column header |
//! | `alias = "name"` | Also accept `name` as this field's column, on decode; repeatable |
//! | `default` | Use [`Default::default()`] when the field is absent or empty |
//! | `skip` | Exclude from CSV; use [`Default::default()`] on decode |
//! | `parse_with = "fn"` | Call `fn(&[u8]) -> Result<T, E>` (where `E` implements `core::error::Error`) instead of `DecodeField` |
//! | `format_with = "fn"` | Call `fn(&T) -> impl AsRef<[u8]>` instead of `EncodeField` |

mod attrs;
mod decode;
mod derive;
mod encode;
mod shared;

#[cfg(test)]
use derive::default_root;
pub use derive::{derive_csv_decode, derive_csv_encode};

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests;

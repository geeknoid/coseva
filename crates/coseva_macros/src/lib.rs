//! Derive macros for `coseva` typed record decoding and encoding.
//!
//! Enabled by the `derive` feature of `coseva`.
//!
//! This crate is a thin proc-macro entry point; the code-generation logic
//! lives in the `coseva_macros_impl` crate.
//! For worked derive examples without introducing a dependency cycle, see the
//! main crate's
//! [`CsvDecode` docs](https://docs.rs/coseva/latest/coseva/encoding/trait.CsvDecode.html)
//! and
//! [`CsvEncode` docs](https://docs.rs/coseva/latest/coseva/encoding/trait.CsvEncode.html).
//!
//! ## Supported shapes
//!
//! - Named structs: `struct Foo { field: Type }`
//! - Tuple structs: `struct Foo(Type, Type)`
//! - At most one lifetime parameter (used for borrowing field data)
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

#![forbid(unsafe_code)]

extern crate proc_macro;

use proc_macro::TokenStream;
use syn::{Path, parse_quote};

/// Derive `coseva::encoding::CsvDecode` for a struct.
///
/// Fields are decoded positionally from CSV columns in declaration order.
/// Skipped fields (`#[csv(skip)]`) do not consume a CSV column.
///
/// # Container attributes
///
/// The struct may be annotated with `#[csv(...)]`:
///
/// | Attribute | Effect |
/// |---|---|
/// | `rename_all = "rule"` | Derive every column name from the field name by `rule` |
///
/// The rules and their spellings are Serde's: `lowercase`, `UPPERCASE`,
/// `PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`,
/// `kebab-case` and `SCREAMING-KEBAB-CASE`. A field's own
/// `#[csv(rename = "...")]` takes precedence over the rule.
///
/// # Field attributes
///
/// Fields may be annotated with `#[csv(...)]`:
///
/// | Attribute | Effect |
/// |---|---|
/// | `rename = "name"` | Use `name` as the column header instead of the field name |
/// | `alias = "name"` | Also accept `name` as this column, on decode; repeatable |
/// | `default` | Use [`Default::default()`] when the column is absent or empty |
/// | `skip` | Consume no column; decode as [`Default::default()`] |
/// | `parse_with = "fn"` | Call `fn(&[u8]) -> Result<T, E>` (where `E` implements `core::error::Error`) instead of `DecodeField` |
///
/// `#[csv(format_with = "...")]` is accepted but only affects `CsvEncode`.
///
/// For a worked example, see the main crate's
/// [`CsvDecode` docs](https://docs.rs/coseva/latest/coseva/encoding/trait.CsvDecode.html).
#[proc_macro_derive(CsvDecode, attributes(csv))]
pub fn derive_csv_decode(input: TokenStream) -> TokenStream {
    let root_path: Path = parse_quote!(::coseva);
    coseva_macros_impl::derive_csv_decode(input.into(), &root_path).into()
}

/// Derive `coseva::encoding::CsvEncode` for a struct.
///
/// Fields are encoded positionally to CSV columns in declaration order.
/// Skipped fields (`#[csv(skip)]`) are not emitted.
///
/// # Container attributes
///
/// The struct may be annotated with `#[csv(...)]`:
///
/// | Attribute | Effect |
/// |---|---|
/// | `rename_all = "rule"` | Derive every column name from the field name by `rule` |
///
/// The rules and their spellings are Serde's: `lowercase`, `UPPERCASE`,
/// `PascalCase`, `camelCase`, `snake_case`, `SCREAMING_SNAKE_CASE`,
/// `kebab-case` and `SCREAMING-KEBAB-CASE`. A field's own
/// `#[csv(rename = "...")]` takes precedence over the rule.
///
/// # Field attributes
///
/// Fields may be annotated with `#[csv(...)]`:
///
/// | Attribute | Effect |
/// |---|---|
/// | `rename = "name"` | Use `name` as the column header instead of the field name |
/// | `skip` | Do not emit a column for this field |
/// | `format_with = "fn"` | Call `fn(&T) -> impl AsRef<[u8]>` instead of `EncodeField` |
///
/// `#[csv(default)]` and `#[csv(parse_with = "...")]` are accepted but only
/// affect `CsvDecode`. `#[csv(alias = "...")]` is rejected, because a column
/// is written under a single name.
///
/// For a worked example, see the main crate's
/// [`CsvEncode` docs](https://docs.rs/coseva/latest/coseva/encoding/trait.CsvEncode.html).
#[proc_macro_derive(CsvEncode, attributes(csv))]
pub fn derive_csv_encode(input: TokenStream) -> TokenStream {
    let root_path: Path = parse_quote!(::coseva);
    coseva_macros_impl::derive_csv_encode(input.into(), &root_path).into()
}

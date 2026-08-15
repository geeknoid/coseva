use proc_macro2::TokenStream as TokenStream2;
#[cfg(test)]
use syn::parse_quote;
use syn::{DeriveInput, Error, Path, parse2};

use crate::decode::expand_decode;
use crate::encode::expand_encode;

/// Generates an implementation of `CsvDecode` using `root_path` as the
/// `coseva` crate root.
///
/// Errors are returned as a `compile_error!` invocation rather than as an
/// `Err`, so the result can be emitted directly by a derive macro.
///
/// ```
/// use coseva_macros_impl::derive_csv_decode;
/// use quote::quote;
/// use syn::{Path, parse_quote};
///
/// let input = quote! {
///     struct Row<'a> {
///         #[csv(rename = "city_name")]
///         city: &'a str,
///         population: u64,
///     }
/// };
/// let root_path: Path = parse_quote!(::coseva);
/// let generated = derive_csv_decode(input, &root_path)
///     .to_string()
///     .replace([' ', '\n'], "");
///
/// assert!(generated.contains("impl<'__row>::coseva::encoding::CsvDecode<'__row>forRow<'__row>"));
/// assert!(generated.contains("city_name"));
/// ```
#[must_use]
pub fn derive_csv_decode(input: TokenStream2, root_path: &Path) -> TokenStream2 {
    parse2::<DeriveInput>(input)
        .and_then(|input| expand_decode(&input, root_path))
        .unwrap_or_else(Error::into_compile_error)
}

/// Generates an implementation of `CsvEncode` using `root_path` as the
/// `coseva` crate root.
///
/// Errors are returned as a `compile_error!` invocation rather than as an
/// `Err`, so the result can be emitted directly by a derive macro.
///
/// ```
/// use coseva_macros_impl::derive_csv_encode;
/// use quote::quote;
/// use syn::{Path, parse_quote};
///
/// let input = quote! {
///     struct Row {
///         #[csv(rename = "city_name")]
///         city: &'static str,
///         population: u64,
///     }
/// };
/// let root_path: Path = parse_quote!(::coseva);
/// let generated = derive_csv_encode(input, &root_path)
///     .to_string()
///     .replace([' ', '\n'], "");
///
/// assert!(generated.contains("impl::coseva::encoding::CsvEncodeforRow"));
/// assert!(generated.contains("city_name"));
/// ```
#[must_use]
pub fn derive_csv_encode(input: TokenStream2, root_path: &Path) -> TokenStream2 {
    parse2::<DeriveInput>(input)
        .and_then(|input| expand_encode(&input, root_path))
        .unwrap_or_else(Error::into_compile_error)
}

/// The default path to the `coseva` crate root used by tests.
#[cfg(test)]
pub(crate) fn default_root() -> Path {
    parse_quote!(::coseva)
}

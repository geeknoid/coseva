//! Public derive entry-point tests.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use coseva_macros_impl::{derive_csv_decode, derive_csv_encode};
use proc_macro2::TokenStream;
use quote::quote;
use syn::{Path, parse_quote};

fn packed(tokens: &TokenStream) -> String {
    tokens.to_string().replace([' ', '\n'], "")
}

fn default_root() -> Path {
    parse_quote!(::coseva)
}

#[test]
fn the_entry_points_honor_the_supplied_crate_root() {
    let input = quote! { struct Row { a: u32 } };
    let root: Path = parse_quote!(::renamed);

    let decoded = packed(&derive_csv_decode(input.clone(), &root));
    assert!(decoded.contains("::renamed::encoding::CsvDecode<'__row>"));

    let encoded = packed(&derive_csv_encode(input, &root));
    assert!(encoded.contains("::renamed::encoding::CsvEncode"));
}

#[test]
fn the_entry_points_report_a_parse_failure_as_a_compile_error() {
    let input = quote! { this is not a struct };
    assert!(packed(&derive_csv_decode(input.clone(), &default_root())).contains("compile_error!"));
    assert!(packed(&derive_csv_encode(input, &default_root())).contains("compile_error!"));
}

#[test]
fn the_entry_points_report_an_expansion_failure_as_a_compile_error() {
    let input = quote! { enum Choice { A, B } };
    assert!(packed(&derive_csv_decode(input.clone(), &default_root())).contains("compile_error!"));
    assert!(packed(&derive_csv_encode(input, &default_root())).contains("compile_error!"));
}

//! Code generation for the `CsvEncode` derive.

use std::collections::BTreeMap;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Error, Index, Path, Result};

use crate::attrs::{parse_container_attrs, parse_field_attrs};
use crate::shared::{claim_csv_name, extract_fields, field_ident_str};

pub(super) fn expand_encode(input: &DeriveInput, root: &Path) -> Result<TokenStream2> {
    let struct_name = &input.ident;

    let (field_list, is_tuple) = extract_fields(input)?;

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let container = parse_container_attrs(&input.attrs)?;

    let mut csv_index: usize = 0;
    let mut field_names: Vec<String> = Vec::new();
    let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();
    let mut encode_stmts: Vec<TokenStream2> = Vec::new();

    for (pos, field) in field_list.iter().enumerate() {
        let attrs = parse_field_attrs(&field.attrs)?;
        let field_ty = &field.ty;
        let ident_str = field_ident_str(field, pos, is_tuple);
        let csv_name = attrs
            .rename
            .clone()
            .unwrap_or_else(|| container.default_name(&ident_str));

        if let Some(alias) = attrs.aliases.first() {
            return Err(Error::new_spanned(
                field,
                format!(
                    "`alias = \"{alias}\"` applies to decoding only, because a column is                      encoded under a single name; use `rename` to choose it"
                ),
            ));
        }

        if attrs.skip {
            continue;
        }

        let idx = csv_index;
        let name = &csv_name;
        csv_index += 1;
        claim_csv_name(&mut claimed_names, &csv_name, &ident_str, field)?;
        field_names.push(csv_name.clone());

        let field_access = if is_tuple {
            let tuple_idx = Index::from(pos);
            quote! { self.#tuple_idx }
        } else {
            let field_ident = field
                .ident
                .as_ref()
                .expect("named field always has an ident");
            quote! { self.#field_ident }
        };

        let stmt = if let Some(format_fn) = &attrs.format_with {
            quote! {
                {
                    let __encoded = #format_fn(&#field_access);
                    __visitor.visit_field(
                        #idx,
                        #name,
                        ::core::convert::AsRef::<[u8]>::as_ref(&__encoded),
                    )?;
                }
            }
        } else {
            quote! {
                <#field_ty as #root::encoding::EncodeField>::encode_to(
                    &#field_access,
                    #idx,
                    #name,
                    __visitor,
                )?;
            }
        };

        encode_stmts.push(stmt);
    }

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #root::encoding::CsvEncode
            for #struct_name #ty_generics
        #where_clause
        {
            fn csv_encode<__V: #root::encoding::EncodeVisitor>(
                &self,
                __visitor: &mut __V,
            ) -> ::core::result::Result<(), #root::Error> {
                #(#encode_stmts)*
                ::core::result::Result::Ok(())
            }

            fn field_names() -> &'static [&'static str] {
                &[#(#field_names),*]
            }
        }
    })
}

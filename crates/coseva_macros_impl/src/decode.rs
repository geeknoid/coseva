//! Code generation for the `CsvDecode` derive.

use std::collections::BTreeMap;

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{DeriveInput, Error, GenericParam, Index, Lifetime, Path, Result, parse_quote};

use crate::attrs::{parse_container_attrs, parse_field_attrs};
use crate::shared::{
    claim_csv_name, extract_fields, field_ident_str, subst_lts_in_type, subst_lts_in_where_clause,
};

#[expect(
    clippy::too_many_lines,
    reason = "code-generation function; the length is a direct consequence of the number of structural cases it must handle"
)]
pub(super) fn expand_decode(input: &DeriveInput, root: &Path) -> Result<TokenStream2> {
    let struct_name = &input.ident;

    let (field_list, is_tuple) = extract_fields(input)?;

    let lifetime_count = input.generics.lifetimes().count();
    if lifetime_count > 1 {
        return Err(Error::new_spanned(
            &input.generics,
            "CsvDecode derive supports at most one lifetime parameter",
        ));
    }

    let row_lt: Lifetime = parse_quote!('__row);

    // The implementation declares only `'__row`, so every lifetime the input
    // declares is rewritten to it -- in field types and in the where clause
    // alike. Copying a predicate such as `T: 'a` verbatim would name a
    // lifetime the implementation never declares and reject a valid derive.
    let struct_lts: Vec<Lifetime> = input
        .generics
        .lifetimes()
        .map(|lp| lp.lifetime.clone())
        .collect();

    // Keep type and const parameters while replacing the source lifetime.
    let impl_params: Vec<TokenStream2> = {
        let mut params = vec![quote! { '__row }];
        for param in &input.generics.params {
            match param {
                GenericParam::Lifetime(_) => {}
                GenericParam::Type(tp) => params.push(quote! { #tp }),
                GenericParam::Const(cp) => params.push(quote! { #cp }),
            }
        }
        params
    };

    let ty_args: Vec<TokenStream2> = input
        .generics
        .params
        .iter()
        .map(|param| match param {
            GenericParam::Lifetime(_) => quote! { #row_lt },
            GenericParam::Type(tp) => {
                let ident = &tp.ident;
                quote! { #ident }
            }
            GenericParam::Const(cp) => {
                let ident = &cp.ident;
                quote! { #ident }
            }
        })
        .collect();

    let where_clause = input
        .generics
        .where_clause
        .as_ref()
        .map(|clause| subst_lts_in_where_clause(clause, &struct_lts, &row_lt));

    let container = parse_container_attrs(&input.attrs)?;

    let mut csv_index: usize = 0;
    let mut field_names: Vec<String> = Vec::new();
    let mut claimed_names: BTreeMap<String, String> = BTreeMap::new();
    let mut field_aliases: Vec<Vec<String>> = Vec::new();
    let mut decode_exprs: Vec<TokenStream2> = Vec::new();
    let mut into_stmts: Vec<TokenStream2> = Vec::new();

    for (pos, field) in field_list.iter().enumerate() {
        let attrs = parse_field_attrs(&field.attrs)?;
        let ident_str = field_ident_str(field, pos, is_tuple);
        let csv_name = attrs
            .rename
            .clone()
            .unwrap_or_else(|| container.default_name(&ident_str));

        let decode_ty = subst_lts_in_type(&field.ty, &struct_lts, &row_lt);

        let mut plain_field: Option<usize> = None;
        let val = if attrs.skip {
            quote! { ::core::default::Default::default() }
        } else {
            let idx = csv_index;
            let name = &csv_name;
            csv_index += 1;
            claim_csv_name(&mut claimed_names, &csv_name, &ident_str, field)?;
            field_names.push(csv_name.clone());
            field_aliases.push(attrs.aliases.clone());

            if let Some(parse_fn) = &attrs.parse_with {
                quote! {
                    {
                        let __raw: &[u8] = match #root::encoding::DecodeRecord::get_field(record, #idx) {
                            ::core::option::Option::Some(__b) => __b,
                            ::core::option::Option::None => b"",
                        };
                        #parse_fn(__raw).map_err(|__e| #root::Error::from_field_conversion(
                            __e,
                            #idx,
                            #name,
                        ))?
                    }
                }
            } else if attrs.default_value {
                quote! {
                    #root::encoding::decode_field_or_default::<'__row, #decode_ty>(
                        #root::encoding::DecodeRecord::get_field(record, #idx),
                        #idx,
                        #name,
                    )?
                }
            } else {
                plain_field = Some(idx);
                quote! {
                    <#decode_ty as #root::encoding::DecodeField<'__row>>::decode_field_from_record(
                        record,
                        #idx,
                        #name,
                    )?
                }
            }
        };

        let place = if is_tuple {
            let index = Index::from(pos);
            quote! { self.#index }
        } else {
            let field_ident = field
                .ident
                .as_ref()
                .expect("named field always has an ident");
            quote! { self.#field_ident }
        };

        // Reuse the existing field's allocation where the decode is a plain
        // `DecodeField` call; `skip`, `default`, and `parse_with` produce a
        // whole value and can only be assigned.
        if let Some(idx) = plain_field {
            let name = &csv_name;
            into_stmts.push(quote! {
                <#decode_ty as #root::encoding::DecodeField<'__row>>::decode_field_into_from_record(
                    &mut #place,
                    record,
                    #idx,
                    #name,
                )?;
            });
        } else {
            into_stmts.push(quote! { #place = #val; });
        }

        if is_tuple {
            decode_exprs.push(val);
        } else {
            let field_ident = field
                .ident
                .as_ref()
                .expect("named field always has an ident");
            decode_exprs.push(quote! { #field_ident: #val });
        }
    }

    let construct = if is_tuple {
        quote! { ::core::result::Result::Ok(Self(#(#decode_exprs),*)) }
    } else {
        quote! { ::core::result::Result::Ok(Self { #(#decode_exprs),* }) }
    };

    let arity = csv_index;

    let ty_generic_tokens = if ty_args.is_empty() {
        quote! {}
    } else {
        quote! { <#(#ty_args),*> }
    };

    // Only override the trait default when at least one field is aliased, so
    // header resolution keeps taking its cheap no-alias path everywhere else.
    let field_aliases_fn = if field_aliases.iter().any(|aliases| !aliases.is_empty()) {
        let rows = field_aliases
            .iter()
            .map(|aliases| quote! { &[#(#aliases),*] as &'static [&'static str] });
        quote! {
            fn field_aliases() -> &'static [&'static [&'static str]] {
                &[#(#rows),*]
            }
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #[automatically_derived]
        impl<#(#impl_params),*> #root::encoding::CsvDecode<'__row>
            for #struct_name #ty_generic_tokens
        #where_clause
        {
            fn csv_decode<__R>(
                record: &__R,
            ) -> ::core::result::Result<Self, #root::Error>
            where
                __R: #root::encoding::DecodeRecord<'__row> + ?Sized,
            {
                #construct
            }

            fn csv_decode_into<__R>(
                &mut self,
                record: &__R,
            ) -> ::core::result::Result<(), #root::Error>
            where
                __R: #root::encoding::DecodeRecord<'__row> + ?Sized,
            {
                #(#into_stmts)*
                ::core::result::Result::Ok(())
            }

            fn field_names() -> &'static [&'static str] {
                &[#(#field_names),*]
            }

            #field_aliases_fn

            const FUSED_ARITY: ::core::option::Option<usize> =
                ::core::option::Option::Some(#arity);

            // These forward to the generic pair rather than repeating the
            // per-field body, so a struct's field conversions are emitted once
            // instead of twice and wide structs cost the compiler
            // proportionally less. `FusedFields<'__row>` implements
            // `DecodeRecord<'__row>`, so the forwarding call monomorphizes cleanly.
            //
            // Measured across struct arities of 5, 16, 24, 32, 48 and 64
            // `u32` fields: forced inlining is never worse than a plain
            // `#[inline]`, and is slightly cheaper at 64. This holds because
            // the engine calls these from exactly two sites, each
            // monomorphized per target type, so there is no call-site
            // fan-out for a large body to multiply.
            #[expect(
                clippy::inline_always,
                reason = "measured: left to its own judgement LLVM keeps this out of line and the caller pays a call per record, which costs more than the mapping indirection it replaces"
            )]
            #[inline(always)]
            fn fused_decode(
                record: &#root::encoding::FusedFields<'__row>,
            ) -> ::core::result::Result<Self, #root::Error> {
                <Self as #root::encoding::CsvDecode<'__row>>::csv_decode(record)
            }

            #[expect(
                clippy::inline_always,
                reason = "measured: left to its own judgement LLVM keeps this out of line and the caller pays a call per record, which costs more than the mapping indirection it replaces"
            )]
            #[inline(always)]
            fn fused_decode_into(
                &mut self,
                record: &#root::encoding::FusedFields<'__row>,
            ) -> ::core::result::Result<(), #root::Error> {
                <Self as #root::encoding::CsvDecode<'__row>>::csv_decode_into(self, record)
            }
        }
    })
}

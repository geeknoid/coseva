use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{DeriveInput, Field, FieldsNamed, FieldsUnnamed, Ident, Lifetime, Type, parse_quote};

use crate::attrs::{RenameRule, parse_container_attrs, parse_field_attrs};
use crate::decode::expand_decode;
use crate::default_root;
use crate::encode::expand_encode;
use crate::shared::{field_ident_str, subst_lts_in_type, subst_lts_in_where_clause};

/// Renders generated tokens with all whitespace removed so assertions do
/// not depend on `TokenStream` spacing.
fn packed(tokens: &TokenStream) -> String {
    tokens.to_string().replace([' ', '\n'], "")
}

fn decode(input: &DeriveInput) -> String {
    packed(&expand_decode(input, &default_root()).expect("derive succeeds"))
}

fn encode(input: &DeriveInput) -> String {
    packed(&expand_encode(input, &default_root()).expect("derive succeeds"))
}

fn decode_error(input: &DeriveInput) -> String {
    expand_decode(input, &default_root())
        .expect_err("derive fails")
        .to_string()
}

fn encode_error(input: &DeriveInput) -> String {
    expand_encode(input, &default_root())
        .expect_err("derive fails")
        .to_string()
}

// ── Field name derivation ──────────────────────────────────────────────

#[test]
fn raw_identifier_field_name_strips_prefix() {
    let fields: FieldsNamed = parse_quote! { { r#type: String } };
    let field = fields.named.first().expect("one field");
    assert_eq!(field_ident_str(field, 0, false), "type");
}

#[test]
fn tuple_struct_field_names_are_positional() {
    let fields: FieldsUnnamed = parse_quote! { (String, u32) };
    let second = fields.unnamed.iter().nth(1).expect("two fields");
    assert_eq!(field_ident_str(second, 1, true), "1");
}

#[test]
fn tuple_mode_uses_the_position_even_for_a_field_with_an_identifier() {
    let field: Field = parse_quote! { incidental_name: String };
    assert_eq!(field_ident_str(&field, 7, true), "7");
}

// ── Attribute parsing ──────────────────────────────────────────────────

#[test]
fn no_attributes_yields_all_defaults() {
    let attrs = parse_field_attrs(&[]).expect("parses");
    assert!(attrs.rename.is_none());
    assert!(!attrs.default_value);
    assert!(!attrs.skip);
    assert!(attrs.parse_with.is_none());
    assert!(attrs.format_with.is_none());
}

#[test]
fn non_csv_attributes_are_ignored() {
    let fields: FieldsNamed = parse_quote! { { #[doc = "hi"] #[serde(skip)] a: u32 } };
    let field = fields.named.first().expect("one field");
    let attrs = parse_field_attrs(&field.attrs).expect("parses");
    assert!(!attrs.skip);
}

#[test]
fn a_csv_attribute_after_an_unrelated_attribute_is_still_parsed() {
    let fields: FieldsNamed = parse_quote! { { #[doc = "hi"] #[csv(skip)] a: u32 } };
    let field = fields.named.first().expect("one field");
    let attrs = parse_field_attrs(&field.attrs).expect("parses");
    assert!(attrs.skip);
}

#[test]
fn every_supported_attribute_is_recognized() {
    // Use separate fields to cover mutually exclusive attributes.
    let fields: FieldsNamed = parse_quote! {
        {
            #[csv(rename = "Column", parse_with = "my::parse", format_with = "my::format")]
            a: u32,
            #[csv(default)]
            b: u32,
            #[csv(skip)]
            c: u32,
        }
    };
    let mut it = fields.named.iter();

    let a = parse_field_attrs(&it.next().expect("field a").attrs).expect("parses");
    assert_eq!(a.rename.as_deref(), Some("Column"));
    assert!(a.parse_with.is_some());
    assert!(a.format_with.is_some());

    let b = parse_field_attrs(&it.next().expect("field b").attrs).expect("parses");
    assert!(b.default_value);

    let c = parse_field_attrs(&it.next().expect("field c").attrs).expect("parses");
    assert!(c.skip);
}

#[test]
fn a_repeated_single_valued_field_attribute_is_rejected() {
    for attribute in ["rename", "default", "skip", "parse_with", "format_with"] {
        let once: TokenStream = match attribute {
            "rename" => quote! { rename = "a" },
            "parse_with" => quote! { parse_with = "my::p" },
            "format_with" => quote! { format_with = "my::f" },
            other => {
                let ident = Ident::new(other, Span::call_site());
                quote! { #ident }
            }
        };
        let fields: FieldsNamed = parse_quote! { { #[csv(#once, #once)] a: u32 } };
        let field = fields.named.first().expect("one field");
        let message = parse_field_attrs(&field.attrs)
            .err()
            .expect("a duplicate attribute must be rejected")
            .to_string();
        assert!(
            message.contains(&format!("duplicate `{attribute}`")),
            "`{attribute}`: {message}"
        );
    }
}

#[test]
fn a_repeated_alias_spelling_is_rejected_but_distinct_ones_are_kept() {
    let fields: FieldsNamed = parse_quote! { { #[csv(alias = "town", alias = "town")] a: u32 } };
    let field = fields.named.first().expect("one field");
    let message = parse_field_attrs(&field.attrs)
        .err()
        .expect("duplicate alias fails")
        .to_string();
    assert!(message.contains("duplicate csv alias"), "{message}");

    let fields: FieldsNamed = parse_quote! { { #[csv(alias = "town", alias = "city")] a: u32 } };
    let field = fields.named.first().expect("one field");
    let attrs = parse_field_attrs(&field.attrs).expect("distinct aliases parse");
    assert_eq!(attrs.aliases, ["town", "city"]);
}

#[test]
fn skip_conflicts_with_every_column_shaping_attribute() {
    for (other, name) in [
        (quote! { rename = "a" }, "rename"),
        (quote! { alias = "a" }, "alias"),
        (quote! { default }, "default"),
        (quote! { parse_with = "my::p" }, "parse_with"),
        (quote! { format_with = "my::f" }, "format_with"),
    ] {
        let fields: FieldsNamed = parse_quote! { { #[csv(skip, #other)] a: u32 } };
        let field = fields.named.first().expect("one field");
        let message = parse_field_attrs(&field.attrs)
            .err()
            .expect("a `skip` conflict must be rejected")
            .to_string();
        assert_eq!(
            message,
            format!("`{name}` conflicts with `skip`; a skipped field has no CSV column"),
            "{other}"
        );
    }
}

#[test]
fn default_conflicts_with_parse_with() {
    let fields: FieldsNamed = parse_quote! { { #[csv(default, parse_with = "my::p")] a: u32 } };
    let field = fields.named.first().expect("one field");
    let message = parse_field_attrs(&field.attrs)
        .err()
        .expect("default + parse_with fails")
        .to_string();
    assert!(
        message.contains("`default` conflicts with `parse_with`"),
        "{message}"
    );
}

#[test]
fn a_repeated_rename_all_is_rejected() {
    let input: DeriveInput = parse_quote! {
        #[csv(rename_all = "snake_case", rename_all = "kebab-case")]
        struct Row { a: u32 }
    };
    let message = parse_container_attrs(&input.attrs)
        .err()
        .expect("duplicate rename_all fails")
        .to_string();
    assert!(message.contains("duplicate `rename_all`"), "{message}");
}

#[test]
fn unsupported_attribute_is_rejected() {
    let fields: FieldsNamed = parse_quote! { { #[csv(bogus)] a: u32 } };
    let field = fields.named.first().expect("one field");
    let message = parse_field_attrs(&field.attrs)
        .err()
        .expect("parse fails")
        .to_string();
    assert_eq!(message, "unsupported csv field attribute");
}

#[test]
fn container_attributes_default_to_no_renaming() {
    let container = parse_container_attrs(&[]).expect("parses");
    assert_eq!(container.default_name("first_name"), "first_name");
}

#[test]
fn every_rename_all_rule_is_recognized() {
    let cases = [
        ("lowercase", "first_name"),
        ("UPPERCASE", "FIRST_NAME"),
        ("PascalCase", "FirstName"),
        ("camelCase", "firstName"),
        ("snake_case", "first_name"),
        ("SCREAMING_SNAKE_CASE", "FIRST_NAME"),
        ("kebab-case", "first-name"),
        ("SCREAMING-KEBAB-CASE", "FIRST-NAME"),
    ];
    for (rule, expected) in cases {
        let input: DeriveInput = parse_quote! {
            #[csv(rename_all = #rule)]
            struct Row { first_name: String }
        };
        let container = parse_container_attrs(&input.attrs).expect("parses");
        assert_eq!(
            container.default_name("first_name"),
            expected,
            "rule `{rule}`"
        );
    }
    assert_eq!(RenameRule::Pascal.apply("__first__name_"), "FirstName");
}

#[test]
fn an_unknown_rename_all_rule_is_rejected_and_lists_the_spellings() {
    let input: DeriveInput = parse_quote! {
        #[csv(rename_all = "SpongeCase")]
        struct Row { a: u32 }
    };
    let message = parse_container_attrs(&input.attrs)
        .err()
        .expect("parse fails")
        .to_string();
    assert_eq!(
        message,
        "unknown rename_all rule, expected one of: lowercase, UPPERCASE, PascalCase, camelCase, \
         snake_case, SCREAMING_SNAKE_CASE, kebab-case, SCREAMING-KEBAB-CASE"
    );
}

#[test]
fn an_unsupported_container_attribute_is_rejected() {
    let input: DeriveInput = parse_quote! {
        #[csv(bogus)]
        struct Row { a: u32 }
    };
    let message = parse_container_attrs(&input.attrs)
        .err()
        .expect("parse fails")
        .to_string();
    assert_eq!(message, "unsupported csv container attribute");
}

#[test]
fn a_field_attribute_is_not_accepted_on_the_container() {
    let input: DeriveInput = parse_quote! {
        #[csv(skip)]
        struct Row { a: u32 }
    };
    assert!(parse_container_attrs(&input.attrs).is_err());
}

#[test]
fn value_bearing_attributes_reject_a_missing_value() {
    for attribute in ["rename", "parse_with", "format_with"] {
        let ident = Ident::new(attribute, Span::call_site());
        let fields: FieldsNamed = parse_quote! { { #[csv(#ident)] a: u32 } };
        let field = fields.named.first().expect("one field");
        assert!(
            parse_field_attrs(&field.attrs).is_err(),
            "`{attribute}` without a value must be rejected"
        );
    }
}

#[test]
fn value_bearing_attributes_reject_a_non_string_value() {
    for attribute in ["rename", "parse_with", "format_with"] {
        let ident = Ident::new(attribute, Span::call_site());
        let fields: FieldsNamed = parse_quote! { { #[csv(#ident = 7)] a: u32 } };
        let field = fields.named.first().expect("one field");
        assert!(
            parse_field_attrs(&field.attrs).is_err(),
            "`{attribute}` with a non-string value must be rejected"
        );
    }
}

#[test]
fn an_unnamed_field_falls_back_to_its_position() {
    // Defensive fallback: a field with no identifier is named positionally
    // even when it is not being treated as a tuple-struct field.
    let fields: FieldsUnnamed = parse_quote! { (String) };
    let field = fields.unnamed.first().expect("one field");
    assert_eq!(field_ident_str(field, 3, false), "3");
}

#[test]
fn parse_with_rejects_a_malformed_path() {
    let fields: FieldsNamed = parse_quote! { { #[csv(parse_with = "1 + 1")] a: u32 } };
    let field = fields.named.first().expect("one field");
    assert!(parse_field_attrs(&field.attrs).is_err());
}

#[test]
fn format_with_rejects_a_malformed_path() {
    let fields: FieldsNamed = parse_quote! { { #[csv(format_with = "1 + 1")] a: u32 } };
    let field = fields.named.first().expect("one field");
    assert!(parse_field_attrs(&field.attrs).is_err());
}

// ── Lifetime substitution ──────────────────────────────────────────────

#[test]
fn substitution_rewrites_only_the_named_lifetimes() {
    let ty: Type = parse_quote! { Cow<'a, [&'b str]> };
    let from: [Lifetime; 1] = [parse_quote! { 'a }];
    let to: Lifetime = parse_quote! { '__row };
    let rewritten = subst_lts_in_type(&ty, &from, &to);
    assert_eq!(packed(&quote! { #rewritten }), "Cow<'__row,[&'bstr]>");
}

#[test]
fn substitution_rewrites_every_struct_lifetime() {
    let ty: Type = parse_quote! { Cow<'a, [&'b str]> };
    let from: [Lifetime; 2] = [parse_quote! { 'a }, parse_quote! { 'b }];
    let to: Lifetime = parse_quote! { '__row };
    let rewritten = subst_lts_in_type(&ty, &from, &to);
    assert_eq!(packed(&quote! { #rewritten }), "Cow<'__row,[&'__rowstr]>");
}

#[test]
fn substitution_rewrites_where_clause_predicates() {
    let clause: syn::WhereClause = parse_quote! { where T: 'a, &'b T: Copy };
    let from: [Lifetime; 2] = [parse_quote! { 'a }, parse_quote! { 'b }];
    let to: Lifetime = parse_quote! { '__row };
    let rewritten = subst_lts_in_where_clause(&clause, &from, &to);
    assert_eq!(
        packed(&quote! { #rewritten }),
        "whereT:'__row,&'__rowT:Copy"
    );
}

#[test]
fn substitution_leaves_higher_ranked_lifetimes_alone() {
    let clause: syn::WhereClause = parse_quote! { where for<'x> T: Fn(&'x u8) -> &'a u8 };
    let from: [Lifetime; 1] = [parse_quote! { 'a }];
    let to: Lifetime = parse_quote! { '__row };
    let rewritten = subst_lts_in_where_clause(&clause, &from, &to);
    assert_eq!(
        packed(&quote! { #rewritten }),
        "wherefor<'x>T:Fn(&'xu8)->&'__rowu8"
    );
}

// ── Shape rejection ────────────────────────────────────────────────────

#[test]
fn unit_structs_are_rejected() {
    let input: DeriveInput = parse_quote! { struct Unit; };
    assert_eq!(
        decode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for unit structs"
    );
    assert_eq!(
        encode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for unit structs"
    );
}

#[test]
fn enums_are_rejected() {
    let input: DeriveInput = parse_quote! { enum Choice { A, B } };
    assert_eq!(
        decode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for enums"
    );
    assert_eq!(
        encode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for enums"
    );
}

#[test]
fn unions_are_rejected() {
    let input: DeriveInput = parse_quote! { union Overlap { a: u32, b: f32 } };
    assert_eq!(
        decode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for unions"
    );
    assert_eq!(
        encode_error(&input),
        "CsvDecode/CsvEncode cannot be derived for unions"
    );
}

#[test]
fn more_than_one_lifetime_is_rejected() {
    let input: DeriveInput = parse_quote! {
        struct Two<'a, 'b> { a: &'a str, b: &'b str }
    };
    assert_eq!(
        decode_error(&input),
        "CsvDecode derive supports at most one lifetime parameter"
    );
}

#[test]
fn a_field_attribute_error_propagates_out_of_both_derives() {
    let input: DeriveInput = parse_quote! {
        struct Bad { #[csv(bogus)] a: u32 }
    };
    assert_eq!(decode_error(&input), "unsupported csv field attribute");
    assert_eq!(encode_error(&input), "unsupported csv field attribute");
}

// ── Decode code generation ─────────────────────────────────────────────

#[test]
fn plain_fields_decode_positionally_in_declaration_order() {
    let input: DeriveInput = parse_quote! {
        struct Row { first: String, second: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"fnfield_names()->&'static[&'staticstr]{&["first","second"]}"#));
    assert!(generated.contains(r#"decode_field_from_record(record,0usize,"first",)"#));
    assert!(generated.contains(r#"decode_field_from_record(record,1usize,"second",)"#));
    // The in-place path reuses each field's existing allocation.
    assert!(
        generated
            .contains(r#"decode_field_into_from_record(&mutself.first,record,0usize,"first",)"#)
    );
}

#[test]
fn rename_changes_the_column_name_but_not_the_field() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(rename = "Given Name")] first: String }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["GivenName"]"#));
    assert!(generated.contains("self.first"));
}

#[test]
fn rename_all_renames_every_field() {
    let input: DeriveInput = parse_quote! {
        #[csv(rename_all = "PascalCase")]
        struct Row { first_name: String, total_count: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["FirstName","TotalCount"]"#));
}

#[test]
fn an_explicit_rename_wins_over_rename_all() {
    let input: DeriveInput = parse_quote! {
        #[csv(rename_all = "PascalCase")]
        struct Row { first_name: String, #[csv(rename = "ZIP")] zip_code: String }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["FirstName","ZIP"]"#));
}

#[test]
fn encode_applies_rename_all_identically() {
    let input: DeriveInput = parse_quote! {
        #[csv(rename_all = "kebab-case")]
        struct Row { first_name: String, #[csv(rename = "ZIP")] zip_code: String }
    };
    let generated = encode(&input);
    assert!(generated.contains(r#"&["first-name","ZIP"]"#));
}

#[test]
fn aliases_are_collected_in_declaration_order() {
    let field: Field = parse_quote! {
        #[csv(rename = "pop", alias = "population", alias = "people")]
        count: u64
    };
    let attrs = parse_field_attrs(&field.attrs).expect("attributes parse");
    assert_eq!(attrs.rename.as_deref(), Some("pop"));
    assert_eq!(attrs.aliases, ["population", "people"]);
}

#[test]
fn aliases_are_emitted_parallel_to_the_field_names() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(alias = "town")] city: String, pop: u64 }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["city","pop"]"#));
    assert!(
        generated.contains(r#"&["town"]as&'static[&'staticstr]"#),
        "{generated}"
    );
}

/// The trait default stands in for the common no-alias case, so header
/// resolution can branch out of alias matching on an empty slice.
#[test]
fn a_struct_without_aliases_emits_no_override() {
    let input: DeriveInput = parse_quote! {
        struct Row { city: String, pop: u64 }
    };
    let generated = decode(&input);
    assert!(!generated.contains("fnfield_aliases"), "{generated}");
}

#[test]
fn encode_rejects_an_alias() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(alias = "town")] city: String }
    };
    let message = encode_error(&input);
    assert!(message.contains("applies to decoding only"), "{message}");
}

#[test]
fn encode_reports_the_first_alias_when_multiple_are_present() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(alias = "town", alias = "village")] city: String }
    };
    let message = encode_error(&input);
    assert!(message.contains(r#"alias = "town""#), "{message}");
    assert!(!message.contains("village"), "{message}");
}

#[test]
fn decode_and_encode_reject_duplicate_csv_name() {
    let input: DeriveInput = parse_quote! {
        struct Row {
            #[csv(rename = "dup")]
            a: u32,
            #[csv(rename = "dup")]
            b: u32,
        }
    };
    let dec_msg = decode_error(&input);
    assert!(
        dec_msg.contains("is already used by field `a`"),
        "{dec_msg}"
    );
    let enc_msg = encode_error(&input);
    assert!(
        enc_msg.contains("is already used by field `a`"),
        "{enc_msg}"
    );
}

#[test]
fn decode_and_encode_propagate_container_attr_error() {
    let input: DeriveInput = parse_quote! {
        #[csv(unknown_container_attr)]
        struct Row {
            a: u32,
        }
    };
    let dec_msg = decode_error(&input);
    assert!(
        dec_msg.contains("unsupported csv container attribute"),
        "{dec_msg}"
    );
    let enc_msg = encode_error(&input);
    assert!(
        enc_msg.contains("unsupported csv container attribute"),
        "{enc_msg}"
    );
}

#[test]
fn skipped_fields_consume_no_column() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(skip)] hidden: u32, shown: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["shown"]"#));
    // `shown` is the first *CSV* column even though it is the second field.
    assert!(generated.contains(r#"decode_field_from_record(record,0usize,"shown",)"#));
    assert!(generated.contains("self.hidden=::core::default::Default::default();"));
}

#[test]
fn default_fields_route_through_the_default_helper() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(default)] count: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains("decode_field_or_default::<'__row,u32>"));
    // Qualified rather than method-call syntax so the same body compiles
    // against the concrete record type the fused path hands it.
    assert!(generated.contains(r#"DecodeRecord::get_field(record,0usize),0usize,"count""#));
}

#[test]
fn parse_with_fields_call_the_custom_parser_and_map_its_error() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(parse_with = "my::parser")] value: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains("my::parser(__raw)"));
    assert!(generated.contains(r#"Error::from_field_conversion(__e,0usize,"value",)"#));
    // An absent column is presented to the parser as an empty field.
    assert!(generated.contains(r#"None=>b"","#));
}

#[test]
fn tuple_structs_construct_positionally() {
    let input: DeriveInput = parse_quote! {
        struct Row(String, u32);
    };
    let generated = decode(&input);
    assert!(generated.contains(r#"&["0","1"]"#));
    assert!(generated.contains("Ok(Self("));
    assert!(generated.contains("&mutself.0"));
    assert!(generated.contains("&mutself.1"));
    assert_eq!(generated.matches("decode_field_from_record").count(), 2);
}

#[test]
fn named_structs_construct_with_field_initializers() {
    let input: DeriveInput = parse_quote! {
        struct Row { first: String, second: u32 }
    };
    let generated = decode(&input);
    assert!(generated.contains("Ok(Self{first:"), "{generated}");
    assert!(generated.contains(",second:"), "{generated}");
}

#[test]
fn the_struct_lifetime_is_replaced_with_the_row_lifetime() {
    let input: DeriveInput = parse_quote! {
        struct Row<'a> { name: &'a str }
    };
    let generated = decode(&input);
    assert!(generated.contains("impl<'__row>"));
    assert!(generated.contains("forRow<'__row>"));
    assert!(generated.contains("<&'__rowstras::coseva::encoding::DecodeField<'__row>>"));
}

#[test]
fn type_and_const_parameters_and_where_clauses_are_preserved() {
    let input: DeriveInput = parse_quote! {
        struct Row<'a, T, const N: usize> where T: Clone { name: &'a str, value: T }
    };
    let generated = decode(&input);
    assert!(generated.contains("impl<'__row,T,constN:usize>"));
    assert!(generated.contains("forRow<'__row,T,N>"));
    assert!(generated.contains("whereT:Clone"));
}

#[test]
fn a_struct_without_generics_gets_a_bare_impl_head() {
    let input: DeriveInput = parse_quote! { struct Row { a: u32 } };
    let generated = decode(&input);
    assert!(generated.contains("impl<'__row>::coseva::encoding::CsvDecode<'__row>forRow"));
    assert!(!generated.contains("forRow<"));
}

// ── Encode code generation ─────────────────────────────────────────────

#[test]
fn encode_visits_each_field_positionally() {
    let input: DeriveInput = parse_quote! {
        struct Row { first: String, second: u32 }
    };
    let generated = encode(&input);
    assert!(generated.contains(r#"&["first","second"]"#));
    assert!(generated.contains(r#"encode_to(&self.first,0usize,"first",__visitor,)"#));
    assert!(generated.contains(r#"encode_to(&self.second,1usize,"second",__visitor,)"#));
}

#[test]
fn encode_omits_skipped_fields_and_renumbers_the_rest() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(skip)] hidden: u32, shown: u32 }
    };
    let generated = encode(&input);
    assert!(generated.contains(r#"&["shown"]"#));
    assert!(!generated.contains("self.hidden"));
    assert!(generated.contains(r#"encode_to(&self.shown,0usize,"shown",__visitor,)"#));
}

#[test]
fn encode_honors_rename() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(rename = "Total")] total: u32 }
    };
    let generated = encode(&input);
    assert!(generated.contains(r#"&["Total"]"#));
    assert!(generated.contains(r#"0usize,"Total""#));
}

#[test]
fn format_with_fields_are_rendered_by_the_custom_formatter() {
    let input: DeriveInput = parse_quote! {
        struct Row { #[csv(format_with = "my::render")] value: u32 }
    };
    let generated = encode(&input);
    assert!(generated.contains("my::render(&self.value)"));
    assert!(generated.contains(r#"__visitor.visit_field(0usize,"value","#));
    assert!(generated.contains("AsRef::<[u8]>::as_ref(&__encoded)"));
}

#[test]
fn encode_supports_tuple_structs() {
    let input: DeriveInput = parse_quote! { struct Row(String, u32); };
    let generated = encode(&input);
    assert!(generated.contains(r#"&["0","1"]"#));
    assert!(generated.contains("&self.0"));
    assert!(generated.contains("&self.1"));
}

#[test]
fn encode_preserves_generics_and_where_clauses() {
    let input: DeriveInput = parse_quote! {
        struct Row<'a, T, const N: usize> where T: Clone { name: &'a str, value: T }
    };
    let generated = encode(&input);
    assert!(generated.contains("impl<'a,T,constN:usize>"));
    assert!(generated.contains("forRow<'a,T,N>"));
    assert!(generated.contains("whereT:Clone"));
}

#[test]
fn decode_emits_a_fused_path_with_the_decoded_field_count() {
    let input: DeriveInput = parse_quote! {
        struct Row {
            city: String,
            #[csv(skip)] ignored: u8,
            count: u32,
        }
    };
    let generated = decode(&input);
    // `skip` consumes no column, so the arity counts decoded fields only.
    assert!(generated.contains("constFUSED_ARITY"));
    assert!(generated.contains("::core::option::Option::Some(2usize)"));
    assert!(generated.contains("fnfused_decode(record:&::coseva::encoding::FusedFields<'__row>"));
    assert!(
        generated.contains("fnfused_decode_into(&mutself,record:&::coseva::encoding::FusedFields")
    );
}

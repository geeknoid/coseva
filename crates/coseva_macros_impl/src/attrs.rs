//! Parsing of the container- and field-level `#[csv(...)]` attributes.

use proc_macro2::Span;
use syn::meta::ParseNestedMeta;
use syn::spanned::Spanned;
use syn::{Attribute, Error, LitStr, Path, Result, parse_str};

/// How a field name is converted into a CSV column name by `rename_all`.
///
/// The spellings and the conversions match Serde's, because a caller who
/// knows one should not have to learn the other. Rust field names are
/// `snake_case` by convention, so every rule reads the name as underscore
/// separated words.
#[derive(Clone, Copy)]
pub(crate) enum RenameRule {
    Lower,
    Upper,
    Pascal,
    Camel,
    Snake,
    ScreamingSnake,
    Kebab,
    ScreamingKebab,
}

/// Every spelling accepted by `rename_all`, paired with its rule.
const RENAME_RULES: &[(&str, RenameRule)] = &[
    ("lowercase", RenameRule::Lower),
    ("UPPERCASE", RenameRule::Upper),
    ("PascalCase", RenameRule::Pascal),
    ("camelCase", RenameRule::Camel),
    ("snake_case", RenameRule::Snake),
    ("SCREAMING_SNAKE_CASE", RenameRule::ScreamingSnake),
    ("kebab-case", RenameRule::Kebab),
    ("SCREAMING-KEBAB-CASE", RenameRule::ScreamingKebab),
];

impl RenameRule {
    fn from_str(name: &str) -> Option<Self> {
        RENAME_RULES
            .iter()
            .find_map(|&(spelling, rule)| (spelling == name).then_some(rule))
    }

    /// Convert a `snake_case` field name into the CSV column name.
    pub(super) fn apply(self, field: &str) -> String {
        match self {
            Self::Lower | Self::Snake => field.to_owned(),
            Self::Upper | Self::ScreamingSnake => field.to_uppercase(),
            Self::Kebab => field.replace('_', "-"),
            Self::ScreamingKebab => field.to_uppercase().replace('_', "-"),
            Self::Pascal => upper_camel(field),
            Self::Camel => {
                let pascal = upper_camel(field);
                let mut chars = pascal.chars();
                match chars.next() {
                    Some(first) => first.to_lowercase().chain(chars).collect(),
                    None => pascal,
                }
            }
        }
    }
}

fn upper_camel(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for word in field.split('_') {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.extend(first.to_uppercase());
            out.extend(chars);
        }
    }
    out
}

/// Parsed container-level `#[csv(...)]` attributes.
#[derive(Default)]
pub(super) struct ContainerAttrs {
    pub(super) rename_all: Option<RenameRule>,
}

impl ContainerAttrs {
    /// The CSV column name for a field, before any explicit `rename`.
    pub(super) fn default_name(&self, field: &str) -> String {
        self.rename_all
            .map_or_else(|| field.to_owned(), |rule| rule.apply(field))
    }
}

fn string_value(meta: &ParseNestedMeta<'_>) -> Result<LitStr> {
    meta.value()?.parse()
}

fn path_value(meta: &ParseNestedMeta<'_>) -> Result<Path> {
    let value = string_value(meta)?;
    parse_str(&value.value()).map_err(|error| meta.error(error.to_string()))
}

/// Records that a single-valued attribute has been seen, rejecting a second
/// occurrence at its own span. `slot` holds the span of the first occurrence.
fn set_once(slot: &mut Option<Span>, span: Span, name: &str) -> Result<()> {
    if slot.is_some() {
        return Err(Error::new(
            span,
            format!("duplicate `{name}` csv attribute; it may appear at most once"),
        ));
    }
    *slot = Some(span);
    Ok(())
}

pub(super) fn parse_container_attrs(attrs: &[Attribute]) -> Result<ContainerAttrs> {
    let mut result = ContainerAttrs::default();
    let mut rename_all_span: Option<Span> = None;
    for attr in attrs {
        if !attr.path().is_ident("csv") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                set_once(&mut rename_all_span, meta.path.span(), "rename_all")?;
                let value = string_value(&meta)?;
                let Some(rule) = RenameRule::from_str(&value.value()) else {
                    let spellings = RENAME_RULES
                        .iter()
                        .map(|&(spelling, _)| spelling)
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(Error::new(
                        value.span(),
                        format!("unknown rename_all rule, expected one of: {spellings}"),
                    ));
                };
                result.rename_all = Some(rule);
            } else {
                return Err(meta.error("unsupported csv container attribute"));
            }
            Ok(())
        })?;
    }
    Ok(result)
}

/// Parsed field-level `#[csv(...)]` attributes.
#[derive(Default)]
pub(super) struct FieldAttrs {
    pub(super) rename: Option<String>,
    /// Additional header spellings this field also binds to, on decode.
    pub(super) aliases: Vec<String>,
    pub(super) default_value: bool,
    pub(super) skip: bool,
    pub(super) parse_with: Option<Path>,
    pub(super) format_with: Option<Path>,
}

pub(super) fn parse_field_attrs(attrs: &[Attribute]) -> Result<FieldAttrs> {
    let mut result = FieldAttrs::default();
    // Spans of the single-valued attributes seen so far, for duplicate
    // rejection and, once every attribute is known, conflict reporting.
    let mut rename_span: Option<Span> = None;
    let mut default_span: Option<Span> = None;
    let mut skip_span: Option<Span> = None;
    let mut parse_with_span: Option<Span> = None;
    let mut format_with_span: Option<Span> = None;
    // `alias` is intentionally repeatable, but the same spelling twice is a
    // mistake, so each value is tracked with its span for a precise duplicate.
    let mut alias_spans: Vec<(String, Span)> = Vec::new();

    for attr in attrs {
        if !attr.path().is_ident("csv") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                set_once(&mut rename_span, meta.path.span(), "rename")?;
                let value = string_value(&meta)?;
                result.rename = Some(value.value());
            } else if meta.path.is_ident("alias") {
                let value = string_value(&meta)?;
                let alias = value.value();
                if alias_spans.iter().any(|(seen, _)| *seen == alias) {
                    return Err(Error::new(
                        value.span(),
                        format!("duplicate csv alias {alias:?}"),
                    ));
                }
                alias_spans.push((alias.clone(), value.span()));
                result.aliases.push(alias);
            } else if meta.path.is_ident("default") {
                set_once(&mut default_span, meta.path.span(), "default")?;
                result.default_value = true;
            } else if meta.path.is_ident("skip") {
                set_once(&mut skip_span, meta.path.span(), "skip")?;
                result.skip = true;
            } else if meta.path.is_ident("parse_with") {
                set_once(&mut parse_with_span, meta.path.span(), "parse_with")?;
                result.parse_with = Some(path_value(&meta)?);
            } else if meta.path.is_ident("format_with") {
                set_once(&mut format_with_span, meta.path.span(), "format_with")?;
                result.format_with = Some(path_value(&meta)?);
            } else {
                return Err(meta.error("unsupported csv field attribute"));
            }
            Ok(())
        })?;
    }

    check_field_conflicts(
        skip_span,
        rename_span,
        default_span,
        parse_with_span,
        format_with_span,
        &alias_spans,
    )?;

    Ok(result)
}

/// Rejects attribute combinations the expansion would silently drop, pointing
/// at the attribute that should be removed.
fn check_field_conflicts(
    skip_span: Option<Span>,
    rename_span: Option<Span>,
    default_span: Option<Span>,
    parse_with_span: Option<Span>,
    format_with_span: Option<Span>,
    alias_spans: &[(String, Span)],
) -> Result<()> {
    // A skipped field carries no CSV column, so every attribute that shapes a
    // column would be ignored; reject it rather than pretend it took effect.
    if skip_span.is_some() {
        let alias_span = alias_spans.iter().next().map(|(_, span)| *span);
        for (span, name) in [
            (rename_span, "rename"),
            (alias_span, "alias"),
            (default_span, "default"),
            (parse_with_span, "parse_with"),
            (format_with_span, "format_with"),
        ] {
            if let Some(span) = span {
                return Err(Error::new(
                    span,
                    format!("`{name}` conflicts with `skip`; a skipped field has no CSV column"),
                ));
            }
        }
    }

    // On decode, `parse_with` produces the value outright, so a `default` next
    // to it never runs; the two are mutually exclusive strategies.
    if let (Some(_), Some(span)) = (parse_with_span, default_span) {
        return Err(Error::new(
            span,
            "`default` conflicts with `parse_with`; `parse_with` already produces the decoded value",
        ));
    }

    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_quote;

    #[test]
    fn rename_rule_case_conversions() {
        assert!(RenameRule::from_str("lowercase").is_some());
        assert!(RenameRule::from_str("invalid_rule").is_none());
        for (rule, expected) in [
            (RenameRule::Lower, "foo_bar"),
            (RenameRule::Upper, "FOO_BAR"),
            (RenameRule::Pascal, "FooBar"),
            (RenameRule::Camel, "fooBar"),
            (RenameRule::Snake, "foo_bar"),
            (RenameRule::ScreamingSnake, "FOO_BAR"),
            (RenameRule::Kebab, "foo-bar"),
            (RenameRule::ScreamingKebab, "FOO-BAR"),
        ] {
            assert_eq!(rule.apply("foo_bar"), expected);
        }
        assert_eq!(RenameRule::Camel.apply(""), "");
    }

    #[test]
    fn parse_container_and_field_attrs_tests() {
        let attrs: Vec<Attribute> = vec![
            parse_quote!(#[doc = "hello"]),
            parse_quote!(#[csv(rename_all = "camelCase")]),
        ];
        let c_res = parse_container_attrs(&attrs).unwrap();
        assert_eq!(c_res.default_name("foo_bar"), "fooBar");

        let bad_c_attrs: Vec<Attribute> = vec![parse_quote!(#[csv(rename_all = 123)])];
        assert!(parse_container_attrs(&bad_c_attrs).is_err());

        let dup_c_attrs: Vec<Attribute> = vec![
            parse_quote!(#[csv(rename_all = "lowercase")]),
            parse_quote!(#[csv(rename_all = "UPPERCASE")]),
        ];
        assert!(parse_container_attrs(&dup_c_attrs).is_err());

        let bad_c_rule: Vec<Attribute> = vec![parse_quote!(#[csv(rename_all = "unknown_rule")])];
        assert!(parse_container_attrs(&bad_c_rule).is_err());

        let bad_c_unsupp: Vec<Attribute> = vec![parse_quote!(#[csv(unknown_attr)])];
        assert!(parse_container_attrs(&bad_c_unsupp).is_err());

        let f_attrs: Vec<Attribute> = vec![
            parse_quote!(#[csv(rename = "custom_name", alias = "alias1", alias = "alias2", default)]),
        ];
        let field_res = parse_field_attrs(&f_attrs).unwrap();
        assert_eq!(field_res.rename, Some("custom_name".to_string()));
        assert_eq!(field_res.aliases, vec!["alias1", "alias2"]);
        assert!(field_res.default_value);

        let bad_f_alias: Vec<Attribute> = vec![parse_quote!(#[csv(alias = 123)])];
        assert!(parse_field_attrs(&bad_f_alias).is_err());

        let bad_f_unsupp: Vec<Attribute> = vec![parse_quote!(#[csv(unknown_attr)])];
        assert!(parse_field_attrs(&bad_f_unsupp).is_err());
    }
}

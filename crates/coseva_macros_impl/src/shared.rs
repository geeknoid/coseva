//! Helpers shared by the decode and encode expansions.

use std::collections::BTreeMap;

use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::visit_mut::VisitMut;
use syn::{Data, DeriveInput, Error, Field, Fields, Lifetime, Result, Type, WhereClause};

/// Records one field's resolved CSV name, rejecting a collision.
///
/// Two fields resolving to the same name produce a header row with a repeated
/// column. Nothing can read that file back — `coseva`'s own name resolution
/// rejects a repeated header with `ErrorKind::DuplicateHeader` — so the round
/// trip cannot close. Both names are known at expansion time, which makes this
/// a compile error rather than a run-time failure in a different process.
pub(super) fn claim_csv_name(
    claimed: &mut BTreeMap<String, String>,
    csv_name: &str,
    ident_str: &str,
    field: &Field,
) -> Result<()> {
    if let Some(first) = claimed.get(csv_name) {
        return Err(Error::new_spanned(
            field,
            format!(
                "the CSV name `{csv_name}` is already used by field `{first}`, so the \
                 generated header would repeat a column and could not be read back; \
                 give one of them a distinct `#[csv(rename = \"...\")]`"
            ),
        ));
    }
    claimed.insert(csv_name.to_owned(), ident_str.to_owned());
    Ok(())
}

/// Replaces every lifetime in `from` with `to`.
///
/// A derive that erases the input's lifetime parameters has to rewrite every
/// one of them, not just the first: the generated implementation declares only
/// its own row lifetime, so any struct lifetime left behind names nothing.
/// Lifetimes bound by a higher-ranked predicate are not in `from`, so they are
/// left alone.
struct SubstLifetimes {
    from: Vec<Lifetime>,
    to: Lifetime,
}

impl VisitMut for SubstLifetimes {
    fn visit_lifetime_mut(&mut self, i: &mut Lifetime) {
        if self.from.iter().any(|lifetime| lifetime.ident == i.ident) {
            *i = self.to.clone();
        }
    }
}

pub(super) fn subst_lts_in_type(ty: &Type, from: &[Lifetime], to: &Lifetime) -> Type {
    let mut ty = ty.clone();
    SubstLifetimes {
        from: from.to_vec(),
        to: to.clone(),
    }
    .visit_type_mut(&mut ty);
    ty
}

pub(super) fn subst_lts_in_where_clause(
    clause: &WhereClause,
    from: &[Lifetime],
    to: &Lifetime,
) -> WhereClause {
    let mut clause = clause.clone();
    SubstLifetimes {
        from: from.to_vec(),
        to: to.clone(),
    }
    .visit_where_clause_mut(&mut clause);
    clause
}

pub(super) fn field_ident_str(field: &Field, positional_index: usize, is_tuple: bool) -> String {
    use syn::ext::IdentExt as _;
    if is_tuple {
        positional_index.to_string()
    } else {
        field.ident.as_ref().map_or_else(
            || positional_index.to_string(),
            |ident| ident.unraw().to_string(),
        )
    }
}

/// Extract the field list from a named or tuple struct, rejecting enums and
/// unit structs. The boolean reports whether the struct is a tuple struct.
pub(super) fn extract_fields(input: &DeriveInput) -> Result<(&Punctuated<Field, Comma>, bool)> {
    match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => Ok((&n.named, false)),
            Fields::Unnamed(u) => Ok((&u.unnamed, true)),
            Fields::Unit => Err(Error::new_spanned(
                input,
                "CsvDecode/CsvEncode cannot be derived for unit structs",
            )),
        },
        Data::Enum(_) => Err(Error::new_spanned(
            input,
            "CsvDecode/CsvEncode cannot be derived for enums",
        )),
        Data::Union(_) => Err(Error::new_spanned(
            input,
            "CsvDecode/CsvEncode cannot be derived for unions",
        )),
    }
}

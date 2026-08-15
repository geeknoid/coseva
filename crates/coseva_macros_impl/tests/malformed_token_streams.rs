//! Property harness for the derive entry points' untrusted token input.
//!
//! Run this as a bounded regression test with `cargo test`, or as an open-ended
//! reducer-backed campaign with:
//!
//! ```text
//! cargo bolero test -p coseva_macros_impl malformed_derive_entry_points
//! ```
//!
//! Bolero replays committed inputs from
//! `tests/__fuzz__/malformed_derive_entry_points/corpus/` and reduces failures
//! back to byte strings accepted by the deterministic token builder below.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::iter;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use coseva_macros_impl::{derive_csv_decode, derive_csv_encode};
use proc_macro2::{Delimiter, Group, Ident, Literal, Punct, Spacing, Span, TokenStream, TokenTree};
use quote::quote;
use syn::{Path, parse_quote};

const BOUNDED_ITERATIONS: usize = 4096;
const BOUNDED_TEST_TIME: Duration = Duration::from_millis(500);
const MAX_INPUT: usize = 512;
const MAX_DEPTH: usize = 3;

const IDENTIFIERS: &[&str] = &[
    "struct",
    "enum",
    "union",
    "fn",
    "pub",
    "impl",
    "where",
    "Row",
    "field",
    "csv",
    "rename",
    "rename_all",
    "alias",
    "default",
    "skip",
    "parse_with",
    "format_with",
    "type",
    "const",
    "async",
];
const PUNCTUATION: &[char] = &[
    '#', '!', ':', ';', ',', '=', '<', '>', '+', '-', '*', '&', '|', '.', '?', '\'',
];

#[derive(Clone, Copy)]
enum EntryPoint {
    Decode,
    Encode,
}

impl EntryPoint {
    fn name(self) -> &'static str {
        match self {
            Self::Decode => "derive_csv_decode",
            Self::Encode => "derive_csv_encode",
        }
    }

    fn expand(self, input: TokenStream, root: &Path) -> TokenStream {
        match self {
            Self::Decode => derive_csv_decode(input, root),
            Self::Encode => derive_csv_encode(input, root),
        }
    }
}

struct Bytes<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Bytes<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.bytes.get(self.offset).copied();
        self.offset += usize::from(byte.is_some());
        byte
    }

    fn tokens(&mut self, depth: usize, budget: usize) -> TokenStream {
        let mut output = TokenStream::new();
        for _ in 0..budget {
            let Some(tag) = self.next() else {
                break;
            };
            let tree = match tag % 5 {
                0 => {
                    let name =
                        IDENTIFIERS[usize::from(self.next().unwrap_or(tag)) % IDENTIFIERS.len()];
                    TokenTree::Ident(Ident::new(name, Span::call_site()))
                }
                1 => {
                    let punct =
                        PUNCTUATION[usize::from(self.next().unwrap_or(tag)) % PUNCTUATION.len()];
                    let spacing = if self.next().unwrap_or_default() & 1 == 0 {
                        Spacing::Alone
                    } else {
                        Spacing::Joint
                    };
                    TokenTree::Punct(Punct::new(punct, spacing))
                }
                2 => TokenTree::Literal(Literal::u64_unsuffixed(u64::from(
                    self.next().unwrap_or(tag),
                ))),
                3 => {
                    let length = usize::from(self.next().unwrap_or_default() % 8);
                    let value: Vec<u8> = iter::from_fn(|| self.next()).take(length).collect();
                    TokenTree::Literal(Literal::byte_string(&value))
                }
                _ if depth < MAX_DEPTH => {
                    let delimiter = match self.next().unwrap_or_default() % 3 {
                        0 => Delimiter::Parenthesis,
                        1 => Delimiter::Brace,
                        _ => Delimiter::Bracket,
                    };
                    let nested_budget = usize::from(self.next().unwrap_or_default() % 12);
                    TokenTree::Group(Group::new(delimiter, self.tokens(depth + 1, nested_budget)))
                }
                _ => TokenTree::Group(Group::new(Delimiter::None, TokenStream::new())),
            };
            output.extend(iter::once(tree));
        }
        output
    }
}

fn malformed_inputs(bytes: &[u8]) -> [TokenStream; 4] {
    let bytes = &bytes[..bytes.len().min(MAX_INPUT)];
    let arbitrary = Bytes::new(bytes).tokens(0, 64);
    let container = arbitrary.clone();
    let field = arbitrary.clone();

    [
        arbitrary,
        quote! { #[csv(#container)] struct Row { field: u8 } },
        quote! { struct Row { #[csv(#field)] field: u8 } },
        quote! { struct Row #field },
    ]
}

fn is_impl_or_compile_error(output: &TokenStream) -> bool {
    let trees: Vec<_> = output.clone().into_iter().collect();
    // Generated impls carry `#[automatically_derived]`, so `impl` is not the
    // first token. Expansion bodies are groups, making this top-level scan exact.
    let is_impl = trees
        .iter()
        .any(|tree| matches!(tree, TokenTree::Ident(ident) if ident == "impl"));
    let is_compile_error = trees.windows(2).any(|pair| {
        matches!(&pair[0], TokenTree::Ident(ident) if ident == "compile_error")
            && matches!(&pair[1], TokenTree::Punct(punct) if punct.as_char() == '!')
    });
    is_impl || is_compile_error
}

fn check_entry_point(entry_point: EntryPoint, input: TokenStream, root: &Path, bytes: &[u8]) {
    let input_text = input.to_string();
    let result = catch_unwind(AssertUnwindSafe(|| entry_point.expand(input, root)));
    let output = result.unwrap_or_else(|payload| {
        std::panic::resume_unwind(Box::new(format!(
            "{} panicked for reduced bytes {bytes:?} producing `{input_text}`: {payload:?}",
            entry_point.name()
        )))
    });

    assert!(
        is_impl_or_compile_error(&output),
        "{} returned neither an impl nor compile_error! for reduced bytes {bytes:?}: \
         input=`{input_text}`, output=`{output}`",
        entry_point.name()
    );
}

#[test]
fn malformed_derive_entry_points() {
    let root: Path = parse_quote!(::coseva);

    bolero::check!()
        .with_iterations(BOUNDED_ITERATIONS)
        .with_test_time(BOUNDED_TEST_TIME)
        .for_each(|bytes: &[u8]| {
            for input in malformed_inputs(bytes) {
                check_entry_point(EntryPoint::Decode, input.clone(), &root, bytes);
                check_entry_point(EntryPoint::Encode, input, &root, bytes);
            }
        });
}

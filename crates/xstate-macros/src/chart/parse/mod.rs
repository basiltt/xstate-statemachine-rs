//! Parser of the `statechart!` DSL. One module per construct; this one holds the shared
//! token helpers and the [`Chart`] entry point.

use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitStr, Token};

use super::ast::{Chart, Name};

mod action;
mod body;
mod invoke;
mod state;
mod transition;

impl Parse for Chart {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let krate = parse_crate_override(input)?;
        let mut root = body::parse_body(input, body::Level::Root)?;
        let Some(id) = root.id.take() else {
            return Err(syn::Error::new(
                Span::call_site(),
                "statechart! needs a machine `id: \"...\"`",
            ));
        };
        if root.states.is_none() {
            return Err(id.error("statechart! needs a `states: { ... }` block"));
        }
        Ok(Self { krate, id, root })
    }
}

/// The optional leading `crate: path,` item.
fn parse_crate_override(input: ParseStream<'_>) -> syn::Result<Option<syn::Path>> {
    if !input.peek(Token![crate]) {
        return Ok(None);
    }
    input.parse::<Token![crate]>()?;
    input.parse::<Token![:]>()?;
    let path: syn::Path = input.parse()?;
    separator(input, false)?;
    Ok(Some(path))
}

/// An identifier (keywords allowed) or a non-empty string literal.
pub(super) fn parse_name(input: ParseStream<'_>) -> syn::Result<Name> {
    if input.peek(LitStr) {
        let lit: LitStr = input.parse()?;
        if lit.value().is_empty() {
            return Err(syn::Error::new(lit.span(), "names must not be empty"));
        }
        return Ok(Name::new(lit.value(), lit));
    }
    if !input.peek(Ident::peek_any) {
        return Err(input.error("expected a name (an identifier or a string literal)"));
    }
    let ident = Ident::parse_any(input)?;
    Ok(Name::new(crate::unraw(&ident), ident))
}

/// `a.b.c` (and, when `wildcard` is set, a trailing `.*`), appending the tokens to `tokens`.
pub(super) fn parse_path(
    input: ParseStream<'_>,
    tokens: &mut TokenStream,
    wildcard: bool,
) -> syn::Result<String> {
    let first = Ident::parse_any(input)?;
    let mut path = crate::unraw(&first);
    first.to_tokens(tokens);
    while input.peek(Token![.]) {
        if wildcard && input.peek2(Token![*]) {
            input.parse::<Token![.]>()?.to_tokens(tokens);
            input.parse::<Token![*]>()?.to_tokens(tokens);
            path.push_str(".*");
            break;
        }
        if !input.peek2(Ident::peek_any) {
            break;
        }
        input.parse::<Token![.]>()?.to_tokens(tokens);
        let segment = Ident::parse_any(input)?;
        path.push('.');
        path.push_str(&crate::unraw(&segment));
        segment.to_tokens(tokens);
    }
    Ok(path)
}

/// Raw `serde_json::json!` input: every token up to the next top-level `,`.
///
/// A top-level `ident :` or `ident =>` after the first token can only be the next item of the
/// enclosing block, so it is reported as a missing comma instead of being swallowed into the
/// JSON.
pub(super) fn parse_json(input: ParseStream<'_>) -> syn::Result<TokenStream> {
    let mut tokens = TokenStream::new();
    while !input.is_empty() && !input.peek(Token![,]) {
        if !tokens.is_empty() && starts_next_item(input) {
            return Err(input.error("expected `,` after JSON value"));
        }
        tokens.extend([input.parse::<TokenTree>()?]);
    }
    if tokens.is_empty() {
        return Err(input.error("expected a JSON value (`serde_json::json!` syntax)"));
    }
    Ok(tokens)
}

/// `true` when the next tokens look like `key:` or `key =>` (and not a `a::b` path).
fn starts_next_item(input: ParseStream<'_>) -> bool {
    input.peek(Ident::peek_any)
        && ((input.peek2(Token![:]) && !input.peek2(Token![::])) || input.peek2(Token![=>]))
}

/// A string literal value.
pub(super) fn parse_string(input: ParseStream<'_>) -> syn::Result<String> {
    Ok(input.parse::<LitStr>()?.value())
}

/// The `,` between entries: required unless the entry ended with a `{ ... }` block.
pub(super) fn separator(input: ParseStream<'_>, ended_with_brace: bool) -> syn::Result<()> {
    if input.is_empty() {
        return Ok(());
    }
    if input.peek(Token![,]) {
        input.parse::<Token![,]>()?;
        return Ok(());
    }
    if ended_with_brace {
        return Ok(());
    }
    Err(input.error("expected `,`"))
}

/// Read the next key and reject it when `seen` already holds it (unless it is `repeatable`).
pub(super) fn parse_key(
    input: ParseStream<'_>,
    seen: &mut Vec<String>,
    repeatable: &[&str],
) -> syn::Result<Ident> {
    if !input.peek(Ident::peek_any) {
        return Err(input.error("expected a key"));
    }
    let key = Ident::parse_any(input)?;
    let name = crate::unraw(&key);
    if seen.contains(&name) {
        return Err(syn::Error::new(key.span(), format!("duplicate key `{name}`")));
    }
    if !repeatable.contains(&name.as_str()) {
        seen.push(name);
    }
    Ok(key)
}

/// "unknown <what> `key`", with a did-you-mean hint or the list of valid keys.
pub(super) fn unknown_key(key: &Ident, what: &str, expected: &[&str]) -> syn::Error {
    let name = crate::unraw(key);
    let lower = name.to_ascii_lowercase();
    let hint = (expected.iter())
        .map(|c| (distance(&lower, &c.to_ascii_lowercase()), c))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map_or_else(
            || format!("expected one of: {}", expected.join(", ")),
            |(_, c)| format!("did you mean `{c}`?"),
        );
    syn::Error::new(key.span(), format!("unknown {what} `{name}`; {hint}"))
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            cur.push((prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Consume `:` and hand the stream back.
pub(super) fn colon(input: ParseStream<'_>) -> syn::Result<ParseStream<'_>> {
    input.parse::<Token![:]>()?;
    Ok(input)
}

/// Consume `=>` and hand the stream back.
pub(super) fn arrow(input: ParseStream<'_>) -> syn::Result<ParseStream<'_>> {
    input.parse::<Token![=>]>()?;
    Ok(input)
}

/// Consume an optional `:` before a `{ ... }` block and hand the stream back.
pub(super) fn block(input: ParseStream<'_>) -> syn::Result<ParseStream<'_>> {
    input.parse::<Option<Token![:]>>()?;
    Ok(input)
}

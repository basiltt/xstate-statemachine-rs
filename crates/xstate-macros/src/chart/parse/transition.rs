//! Transitions: `on` / `after` maps, `target [options] | target ...` candidate lists, targets
//! and transition options.

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{Ident, LitBool, LitInt, LitStr, Token, braced, bracketed, parenthesized, token};

use super::{action, parse_json, parse_key, parse_name, parse_path, parse_string, separator};
use crate::chart::ast::{Name, Target, TargetKind, Transition};

/// Options accepted inside `[ ... ]` after a target.
const OPTIONS: &[&str] = &["guard", "actions", "reenter", "description", "meta"];

/// `{ key => transitions, ... }` where `key` is an event (`what == "event"`) or a delay.
pub(super) fn parse_map(
    input: ParseStream<'_>,
    what: &str,
) -> syn::Result<Vec<(Name, Vec<Transition>)>> {
    let content;
    braced!(content in input);
    let mut entries: Vec<(Name, Vec<Transition>)> = Vec::new();
    while !content.is_empty() {
        let key = if what == "event" { parse_event(&content)? } else { parse_delay(&content)? };
        if entries.iter().any(|(k, _)| k.value == key.value) {
            return Err(key.error(format!("duplicate {what} `{}`", key.value)));
        }
        content.parse::<Token![=>]>()?;
        entries.push((key, parse_transitions(&content)?));
        separator(&content, false)?;
    }
    Ok(entries)
}

/// `NAME`, `order.paid`, `order.*`, `*` or `"any string"`.
fn parse_event(input: ParseStream<'_>) -> syn::Result<Name> {
    if input.peek(Token![*]) {
        let star: Token![*] = input.parse()?;
        return Ok(Name::new("*".to_owned(), star));
    }
    if input.peek(LitStr) {
        return parse_name(input);
    }
    if !input.peek(Ident::peek_any) {
        return Err(input.error("expected an event: `NAME`, `a.b`, `a.*`, `*` or a string"));
    }
    let mut tokens = TokenStream::new();
    let path = parse_path(input, &mut tokens, true)?;
    Ok(Name::new(path, tokens))
}

/// A delay in milliseconds (`1000`) or a named delay (`slow`, `"slow"`).
fn parse_delay(input: ParseStream<'_>) -> syn::Result<Name> {
    if input.peek(LitInt) {
        let lit: LitInt = input.parse()?;
        let ms: u64 = lit.base10_parse()?;
        return Ok(Name::new(ms.to_string(), lit));
    }
    parse_name(input)
}

/// `candidate | candidate | ...`, tried in order.
pub(super) fn parse_transitions(input: ParseStream<'_>) -> syn::Result<Vec<Transition>> {
    let mut list = vec![parse_transition(input)?];
    while input.peek(Token![|]) && !input.peek(Token![||]) {
        input.parse::<Token![|]>()?;
        list.push(parse_transition(input)?);
    }
    Ok(list)
}

/// `targets [option, ...]?`
fn parse_transition(input: ParseStream<'_>) -> syn::Result<Transition> {
    let mut transition = Transition { targets: parse_targets(input)?, ..Transition::default() };
    if input.peek(token::Bracket) {
        let content;
        bracketed!(content in input);
        parse_options(&content, &mut transition)?;
    }
    Ok(transition)
}

/// `none`, one target, or `(target, target, ...)` for parallel regions.
fn parse_targets(input: ParseStream<'_>) -> syn::Result<Vec<Target>> {
    if input.peek(token::Paren) {
        let content;
        let parens = parenthesized!(content in input);
        let list = Punctuated::<Target, Token![,]>::parse_terminated_with(&content, parse_target)?;
        if list.is_empty() {
            return Err(syn::Error::new(parens.span.join(), "expected at least one target"));
        }
        return Ok(list.into_iter().collect());
    }
    if input.fork().parse::<Ident>().is_ok_and(|ident| ident == "none") {
        input.parse::<Ident>()?;
        return Ok(Vec::new());
    }
    Ok(vec![parse_target(input)?])
}

/// `sibling.path`, `.child.path`, `#id.path` or a string literal holding any of them.
pub(super) fn parse_target(input: ParseStream<'_>) -> syn::Result<Target> {
    if input.peek(LitStr) {
        let name = parse_name(input)?;
        let kind = match name.value.chars().next() {
            Some('#') => TargetKind::Id,
            Some('.') => TargetKind::Child,
            _ => TargetKind::Plain,
        };
        if kind != TargetKind::Plain && name.value.len() == 1 {
            return Err(name.error("target path must not be empty"));
        }
        return Ok(Target { kind, name });
    }
    let mut tokens = TokenStream::new();
    let (kind, prefix) = if input.peek(Token![#]) {
        input.parse::<Token![#]>()?.to_tokens(&mut tokens);
        (TargetKind::Id, "#")
    } else if input.peek(Token![.]) {
        input.parse::<Token![.]>()?.to_tokens(&mut tokens);
        (TargetKind::Child, ".")
    } else {
        (TargetKind::Plain, "")
    };
    if !input.peek(Ident::peek_any) {
        let msg = "expected a target: `sibling`, `.child`, `#id.path`, `none` or a string literal";
        return Err(input.error(msg));
    }
    let path = parse_path(input, &mut tokens, false)?;
    Ok(Target { kind, name: Name::new(format!("{prefix}{path}"), tokens) })
}

/// `guard: g, actions: [a, b], reenter, description: "...", meta: { ... }`.
fn parse_options(input: ParseStream<'_>, transition: &mut Transition) -> syn::Result<()> {
    let mut seen = Vec::new();
    while !input.is_empty() {
        let key = parse_key(input, &mut seen, &[])?;
        match crate::unraw(&key).as_str() {
            "guard" => {
                input.parse::<Token![:]>()?;
                transition.guard = Some(action::parse_guard(input)?);
            }
            "actions" => {
                input.parse::<Token![:]>()?;
                transition.actions = Some(action::parse_actions(input)?);
            }
            "reenter" => {
                let explicit = input.parse::<Option<Token![:]>>()?.is_some();
                transition.reenter = Some(!explicit || input.parse::<LitBool>()?.value);
            }
            "description" => {
                input.parse::<Token![:]>()?;
                transition.description = Some(parse_string(input)?);
            }
            "meta" => {
                input.parse::<Token![:]>()?;
                transition.meta = Some(parse_json(input)?);
            }
            _ => return Err(super::unknown_key(&key, "transition option", OPTIONS)),
        }
        separator(input, false)?;
    }
    Ok(())
}

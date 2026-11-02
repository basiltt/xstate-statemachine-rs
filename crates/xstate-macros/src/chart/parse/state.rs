//! Child state lists and state kinds.

use proc_macro2::Span;
use syn::ext::IdentExt;
use syn::parse::ParseStream;
use syn::{Ident, braced, parenthesized, token};

use super::body::{Level, parse_body};
use super::{parse_name, separator};
use crate::chart::ast::{Body, Kind, StateDef};

/// `{ name { body }, bare_name, "quoted name" { body } }`; commas after `{ ... }` are optional.
pub(super) fn parse_states(input: ParseStream<'_>) -> syn::Result<Vec<StateDef>> {
    let content;
    braced!(content in input);
    let mut states: Vec<StateDef> = Vec::new();
    while !content.is_empty() {
        let name = parse_name(&content)?;
        if states.iter().any(|s| s.name.value == name.value) {
            return Err(name.error(format!("duplicate state `{}`", name.value)));
        }
        let braced = content.peek(token::Brace);
        let body = if braced {
            let inner;
            braced!(inner in content);
            parse_body(&inner, Level::State)?
        } else {
            Body::default()
        };
        states.push(StateDef { name, body });
        separator(&content, braced)?;
    }
    Ok(states)
}

/// `atomic | compound | final | parallel | history | history(shallow | deep)`.
pub(super) fn parse_kind(input: ParseStream<'_>) -> syn::Result<(Kind, Span)> {
    let ident = Ident::parse_any(input)?;
    let kind = match crate::unraw(&ident).as_str() {
        "atomic" => Kind::Atomic,
        "compound" => Kind::Compound,
        "final" => Kind::Final,
        "parallel" => Kind::Parallel,
        "history" => Kind::History(parse_history_depth(input)?),
        _ => {
            let msg = "expected `atomic`, `compound`, `final`, `parallel` or \
                       `history(shallow | deep)`";
            return Err(syn::Error::new(ident.span(), msg));
        }
    };
    Ok((kind, ident.span()))
}

fn parse_history_depth(input: ParseStream<'_>) -> syn::Result<Option<bool>> {
    if !input.peek(token::Paren) {
        return Ok(None);
    }
    let content;
    parenthesized!(content in input);
    let ident = Ident::parse_any(&content)?;
    if !content.is_empty() {
        return Err(content.error("expected `)`"));
    }
    match crate::unraw(&ident).as_str() {
        "shallow" => Ok(Some(false)),
        "deep" => Ok(Some(true)),
        _ => Err(syn::Error::new(ident.span(), "expected `shallow` or `deep`")),
    }
}

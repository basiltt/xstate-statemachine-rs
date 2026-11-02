//! Action lists, guards and tag lists.

use proc_macro2::TokenStream;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{Token, bracketed, parenthesized, token};

use super::parse_name;
use crate::chart::ast::{Action, Guard, Name};

/// `action` or `[action, action, ...]`, where `action` is `name` or `name(params)`.
pub(super) fn parse_actions(input: ParseStream<'_>) -> syn::Result<Vec<Action>> {
    if input.peek(token::Bracket) {
        let content;
        bracketed!(content in input);
        let list = Punctuated::<Action, Token![,]>::parse_terminated_with(&content, parse_action)?;
        return Ok(list.into_iter().collect());
    }
    Ok(vec![parse_action(input)?])
}

fn parse_action(input: ParseStream<'_>) -> syn::Result<Action> {
    let name = parse_name(input)?;
    Ok(Action { name, params: parse_params(input)? })
}

/// `name`, `!name`, `name(params)` or `!name(params)`.
pub(super) fn parse_guard(input: ParseStream<'_>) -> syn::Result<Guard> {
    let negated = input.parse::<Option<Token![!]>>()?.is_some();
    let name = parse_name(input)?;
    Ok(Guard { negated, name, params: parse_params(input)? })
}

/// Optional `( json )` parameters.
fn parse_params(input: ParseStream<'_>) -> syn::Result<Option<TokenStream>> {
    if !input.peek(token::Paren) {
        return Ok(None);
    }
    let content;
    let parens = parenthesized!(content in input);
    let params: TokenStream = content.parse()?;
    if params.is_empty() {
        let msg = "expected a JSON parameter value; drop the `()` for no parameters";
        return Err(syn::Error::new(parens.span.join(), msg));
    }
    Ok(Some(params))
}

/// `name` or `[name, name, ...]`.
pub(super) fn parse_names(input: ParseStream<'_>) -> syn::Result<Vec<Name>> {
    if input.peek(token::Bracket) {
        let content;
        bracketed!(content in input);
        let list = Punctuated::<Name, Token![,]>::parse_terminated_with(&content, parse_name)?;
        return Ok(list.into_iter().collect());
    }
    Ok(vec![parse_name(input)?])
}

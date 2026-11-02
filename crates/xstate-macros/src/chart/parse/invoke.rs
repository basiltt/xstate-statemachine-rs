//! `invoke { src: ..., id: ..., input: ..., systemId: ..., onDone => ..., onError => ... }`.

use syn::Ident;
use syn::braced;
use syn::parse::ParseStream;

use super::transition::parse_transitions;
use super::{arrow, colon, parse_json, parse_key, parse_name, separator};
use crate::chart::ast::Invoke;

/// Keys an `invoke` block accepts.
const INVOKE_KEYS: &[&str] = &["src", "id", "input", "systemId", "onDone", "onError"];

/// Parse the `{ ... }` after the `invoke` keyword `key`.
pub(super) fn parse_invoke(input: ParseStream<'_>, key: &Ident) -> syn::Result<Invoke> {
    let content;
    braced!(content in input);
    let (mut src, mut id, mut json, mut system_id, mut on_done, mut on_error) =
        (None, None, None, None, None, None);
    let mut seen = Vec::new();
    while !content.is_empty() {
        let item = parse_key(&content, &mut seen, &[])?;
        match crate::unraw(&item).as_str() {
            "src" => src = Some(parse_name(colon(&content)?)?),
            "id" => id = Some(parse_name(colon(&content)?)?),
            "systemId" => system_id = Some(parse_name(colon(&content)?)?),
            "input" => json = Some(parse_json(colon(&content)?)?),
            "onDone" => on_done = Some(parse_transitions(arrow(&content)?)?),
            "onError" => on_error = Some(parse_transitions(arrow(&content)?)?),
            _ => return Err(super::unknown_key(&item, "invoke key", INVOKE_KEYS)),
        }
        separator(&content, false)?;
    }
    let Some(src) = src else {
        return Err(syn::Error::new(key.span(), "`invoke` needs a `src: service_name`"));
    };
    Ok(Invoke { src, id, input: json, system_id, on_done, on_error })
}

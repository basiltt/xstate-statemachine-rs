//! State bodies: the comma-separated `key: value` / `key { ... }` / `key => ...` items of a
//! state and of the chart root.

use syn::Ident;
use syn::parse::ParseStream;

use super::separator;
use super::{action, invoke, state, transition};
use super::{
    arrow, block, colon, ends_with_brace, parse_json, parse_key, parse_name, parse_string,
};
use crate::chart::ast::Body;

/// Where a body appears; the root accepts the machine-level keys.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Level {
    Root,
    State,
}

/// Keys every state body accepts.
const STATE_KEYS: &[&str] = &[
    "type",
    "id",
    "initial",
    "entry",
    "exit",
    "on",
    "always",
    "after",
    "invoke",
    "onDone",
    "tags",
    "meta",
    "output",
    "description",
    "target",
    "states",
];

/// Keys only the chart root accepts; their values are passed to `json!` verbatim.
const ROOT_KEYS: &[&str] = &[
    "context",
    "version",
    "strict",
    "strictTargets",
    "strictConfig",
    "maxIterations",
    "actionErrorPolicy",
    "guardErrorPolicy",
    "onUnhandled",
];

/// Parse items until the stream is exhausted.
pub(super) fn parse_body(input: ParseStream<'_>, level: Level) -> syn::Result<Body> {
    let mut body = Body::default();
    let mut seen = Vec::new();
    while !input.is_empty() {
        let key = parse_key(input, &mut seen, &["invoke"])?;
        let ended_with_brace = parse_item(input, &mut body, &key, level)?;
        separator(input, ended_with_brace)?;
    }
    Ok(body)
}

/// Parse the value of one item into `body`; returns whether it ended with a `{ ... }` block.
fn parse_item(
    input: ParseStream<'_>,
    body: &mut Body,
    key: &Ident,
    level: Level,
) -> syn::Result<bool> {
    let name = crate::unraw(key);
    match name.as_str() {
        "type" => body.kind = Some(state::parse_kind(colon(input)?)?),
        "id" => body.id = Some(parse_name(colon(input)?)?),
        "initial" => body.initial = Some(parse_name(colon(input)?)?),
        "entry" => body.entry = Some(action::parse_actions(colon(input)?)?),
        "exit" => body.exit = Some(action::parse_actions(colon(input)?)?),
        "on" => body.on = Some(transition::parse_map(block(input)?, "event")?),
        "after" => body.after = Some(transition::parse_map(block(input)?, "delay")?),
        "invoke" => body.invoke.push(invoke::parse_invoke(block(input)?, key)?),
        "states" => body.states = Some(state::parse_states(block(input)?)?),
        "always" => body.always = Some(transition::parse_transitions(arrow(input)?)?),
        "onDone" => {
            body.on_done = Some((key.span(), transition::parse_transitions(arrow(input)?)?));
        }
        "tags" => body.tags = Some(action::parse_names(colon(input)?)?),
        "description" => body.description = Some(parse_string(colon(input)?)?),
        "target" if level == Level::State => {
            body.target = Some((key.span(), transition::parse_target(colon(input)?)?));
        }
        "meta" | "output" => {
            let json = parse_json(colon(input)?)?;
            let braced = ends_with_brace(&json);
            *if name == "meta" { &mut body.meta } else { &mut body.output } = Some(json);
            return Ok(braced);
        }
        _ if level == Level::Root && ROOT_KEYS.contains(&name.as_str()) => {
            let json = parse_json(colon(input)?)?;
            let braced = ends_with_brace(&json);
            body.root_keys.push((name, json));
            return Ok(braced);
        }
        _ => return Err(unknown(key, &name, level)),
    }
    Ok(matches!(name.as_str(), "on" | "after" | "invoke" | "states"))
}

fn unknown(key: &Ident, name: &str, level: Level) -> syn::Error {
    if level == Level::State && ROOT_KEYS.contains(&name) {
        return syn::Error::new(key.span(), format!("`{name}` is only valid at the chart root"));
    }
    if level == Level::Root && name == "target" {
        return syn::Error::new(key.span(), "`target` is only valid on history states");
    }
    let mut expected = STATE_KEYS.to_vec();
    if level == Level::Root {
        expected.retain(|k| *k != "target");
        expected.extend_from_slice(ROOT_KEYS);
    }
    super::unknown_key(key, "key", &expected)
}

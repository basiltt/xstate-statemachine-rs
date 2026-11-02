//! Expansion of a validated chart into a `serde_json::Value` expression.
//!
//! The chart skeleton is built from `serde_json::Value` constructors (a `json!` call per
//! document would hit the macro recursion limit on large charts); every user-written JSON
//! fragment (`context`, `meta`, `output`, `input`, parameters) goes through
//! `serde_json::json!` verbatim. Both `serde_json` and `xstate_core` are reached through the
//! path [`crate::krate`] resolves, so the calling crate needs neither as a direct dependency.

use proc_macro2::TokenStream;
use quote::quote;

use super::ast::{Action, Body, Chart, Guard, Invoke, Kind, Name, Transition};

/// A JSON document under construction.
enum J {
    Str(String),
    Bool(bool),
    Null,
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
    /// `serde_json::json!` input written by the user.
    Raw(TokenStream),
}

impl J {
    /// The tokens building this document; `sj` is the path of the `serde_json` crate.
    fn render(&self, sj: &TokenStream) -> TokenStream {
        let value = quote!(#sj::Value);
        match self {
            Self::Str(s) => quote!(#value::String(::std::string::String::from(#s))),
            Self::Bool(b) => quote!(#value::Bool(#b)),
            Self::Null => quote!(#value::Null),
            Self::Arr(items) => {
                let items = items.iter().map(|item| item.render(sj));
                quote!(#value::Array(::std::vec![#(#items),*]))
            }
            Self::Obj(entries) if entries.is_empty() => {
                quote!(#value::Object(#sj::Map::new()))
            }
            Self::Obj(entries) => {
                let keys = entries.iter().map(|(k, _)| k);
                let values = entries.iter().map(|(_, v)| v.render(sj));
                quote! {
                    #value::Object(::core::iter::Iterator::collect::<
                        #sj::Map<::std::string::String, #value>,
                    >(::core::iter::IntoIterator::into_iter([
                        #((::std::string::String::from(#keys), #values)),*
                    ])))
                }
            }
            Self::Raw(json) => quote!(#sj::json!(#json)),
        }
    }
}

/// `{ let value = <document>; Machine::from_value(&value) }`.
pub(crate) fn expand(chart: &Chart) -> TokenStream {
    let krate = crate::krate::resolve(chart.krate.as_ref());
    let sj = quote!(#krate::__private::serde_json);
    let mut root = vec![("id".to_owned(), J::Str(chart.id.value.clone()))];
    root.extend(chart.root.root_keys.iter().map(|(k, v)| (k.clone(), J::Raw(v.clone()))));
    root.extend(body(&chart.root));
    let document = J::Obj(root).render(&sj);
    quote! {
        {
            let value: #sj::Value = #document;
            #krate::Machine::from_value(&value)
        }
    }
}

/// The entries of one state object.
fn body(body: &Body) -> Vec<(String, J)> {
    let mut out: Vec<(String, J)> = Vec::new();
    let mut put = |key: &str, value: Option<J>| {
        if let Some(value) = value {
            out.push((key.to_owned(), value));
        }
    };
    put("id", body.id.as_ref().map(name));
    put("type", body.kind.map(|(k, _)| kind(k)));
    put("history", body.kind.and_then(|(k, _)| history(k)));
    put("target", body.target.as_ref().map(|(_, t)| name(&t.name)));
    put("initial", body.initial.as_ref().map(name));
    put("entry", body.entry.as_deref().map(actions));
    put("exit", body.exit.as_deref().map(actions));
    put("on", body.on.as_deref().map(handler_map));
    put("always", body.always.as_deref().map(transitions));
    put("after", body.after.as_deref().map(handler_map));
    put("invoke", invokes(&body.invoke));
    put("onDone", body.on_done.as_ref().map(|(_, ts)| transitions(ts)));
    put("tags", body.tags.as_ref().map(|tags| J::Arr(tags.iter().map(name).collect())));
    put("meta", body.meta.clone().map(J::Raw));
    put("output", body.output.clone().map(J::Raw));
    put("description", body.description.clone().map(J::Str));
    let states = body.states.as_ref().map(|states| {
        J::Obj(states.iter().map(|s| (s.name.value.clone(), J::Obj(self::body(&s.body)))).collect())
    });
    put("states", states);
    out
}

fn name(name: &Name) -> J {
    J::Str(name.value.clone())
}

fn kind(kind: Kind) -> J {
    J::Str(
        match kind {
            Kind::Atomic => "atomic",
            Kind::Compound => "compound",
            Kind::Final => "final",
            Kind::Parallel => "parallel",
            Kind::History(_) => "history",
        }
        .to_owned(),
    )
}

fn history(kind: Kind) -> Option<J> {
    match kind {
        Kind::History(Some(deep)) => Some(J::Str(if deep { "deep" } else { "shallow" }.to_owned())),
        _ => None,
    }
}

fn handler_map(entries: &[(Name, Vec<Transition>)]) -> J {
    J::Obj(entries.iter().map(|(key, ts)| (key.value.clone(), transitions(ts))).collect())
}

/// One candidate as itself, several as an array.
fn transitions(list: &[Transition]) -> J {
    match list {
        [single] => transition(single),
        many => J::Arr(many.iter().map(transition).collect()),
    }
}

/// A bare target string when there is nothing else to say, an object otherwise.
fn transition(t: &Transition) -> J {
    let plain = t.guard.is_none()
        && t.actions.is_none()
        && t.reenter.is_none()
        && t.description.is_none()
        && t.meta.is_none();
    if let ([target], true) = (t.targets.as_slice(), plain) {
        return name(&target.name);
    }
    let mut obj = Vec::new();
    match t.targets.as_slice() {
        [] => {}
        [one] => obj.push(("target".to_owned(), name(&one.name))),
        many => {
            let targets = many.iter().map(|x| name(&x.name)).collect();
            obj.push(("target".to_owned(), J::Arr(targets)));
        }
    }
    if let Some(g) = &t.guard {
        obj.push(("guard".to_owned(), guard(g)));
    }
    if let Some(list) = &t.actions {
        obj.push(("actions".to_owned(), actions(list)));
    }
    if let Some(reenter) = t.reenter {
        obj.push(("reenter".to_owned(), J::Bool(reenter)));
    }
    if let Some(d) = &t.description {
        obj.push(("description".to_owned(), J::Str(d.clone())));
    }
    if let Some(meta) = &t.meta {
        obj.push(("meta".to_owned(), J::Raw(meta.clone())));
    }
    if obj.is_empty() {
        // `{}` means "forbidden" to the parser; a targetless no-op says so explicitly.
        obj.push(("target".to_owned(), J::Null));
    }
    J::Obj(obj)
}

/// `"name"` / `"!name"`, or `{type, params}` (wrapped in `not` when negated).
fn guard(g: &Guard) -> J {
    let Some(params) = &g.params else {
        let bang = if g.negated { "!" } else { "" };
        return J::Str(format!("{bang}{}", g.name.value));
    };
    let inner = J::Obj(vec![
        ("type".to_owned(), name(&g.name)),
        ("params".to_owned(), J::Raw(params.clone())),
    ]);
    if !g.negated {
        return inner;
    }
    J::Obj(vec![
        ("type".to_owned(), J::Str("not".to_owned())),
        ("params".to_owned(), J::Obj(vec![("guard".to_owned(), inner)])),
    ])
}

fn actions(list: &[Action]) -> J {
    J::Arr(
        (list.iter())
            .map(|a| match &a.params {
                None => name(&a.name),
                Some(params) => J::Obj(vec![
                    ("type".to_owned(), name(&a.name)),
                    ("params".to_owned(), J::Raw(params.clone())),
                ]),
            })
            .collect(),
    )
}

/// One invoke as an object, several as an array.
fn invokes(list: &[Invoke]) -> Option<J> {
    let mut objects: Vec<J> = list.iter().map(invoke).collect();
    match objects.len() {
        0 => None,
        1 => objects.pop(),
        _ => Some(J::Arr(objects)),
    }
}

fn invoke(inv: &Invoke) -> J {
    let mut obj = vec![("src".to_owned(), name(&inv.src))];
    if let Some(id) = &inv.id {
        obj.push(("id".to_owned(), name(id)));
    }
    if let Some(input) = &inv.input {
        obj.push(("input".to_owned(), J::Raw(input.clone())));
    }
    if let Some(system_id) = &inv.system_id {
        obj.push(("systemId".to_owned(), name(system_id)));
    }
    if let Some(ts) = &inv.on_done {
        obj.push(("onDone".to_owned(), transitions(ts)));
    }
    if let Some(ts) = &inv.on_error {
        obj.push(("onError".to_owned(), transitions(ts)));
    }
    J::Obj(obj)
}

//! Syntax tree of the `statechart!` DSL.

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;

/// A name written as an identifier, a dotted path or a string literal, with the tokens it
/// came from (for error spans).
#[derive(Clone)]
pub(crate) struct Name {
    pub(crate) value: String,
    pub(crate) tokens: TokenStream,
}

impl Name {
    pub(crate) fn new(value: String, tokens: impl ToTokens) -> Self {
        Self { value, tokens: tokens.into_token_stream() }
    }

    /// A compile error pointing at this name.
    pub(crate) fn error(&self, message: impl std::fmt::Display) -> syn::Error {
        syn::Error::new_spanned(&self.tokens, message)
    }
}

/// The whole chart: the root state's body (which also holds the machine id and the root-only
/// keys).
pub(crate) struct Chart {
    pub(crate) id: Name,
    pub(crate) root: Body,
}

/// One child state: `name { body }` or a bare `name`.
pub(crate) struct StateDef {
    pub(crate) name: Name,
    pub(crate) body: Body,
}

/// The `type:` of a state.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Atomic,
    Compound,
    Final,
    Parallel,
    /// `history`, `history(shallow)` or `history(deep)`; `None` leaves the JSON default.
    History(Option<bool>),
}

/// Everything a state (or the root) may declare. `Option` distinguishes "absent" from
/// "written empty" so the JSON mirrors the source.
#[derive(Default)]
pub(crate) struct Body {
    pub(crate) kind: Option<(Kind, Span)>,
    /// Custom `#id` of a state (the machine id lives in [`Chart::id`]).
    pub(crate) id: Option<Name>,
    pub(crate) initial: Option<Name>,
    pub(crate) entry: Option<Vec<Action>>,
    pub(crate) exit: Option<Vec<Action>>,
    pub(crate) on: Option<Vec<(Name, Vec<Transition>)>>,
    pub(crate) always: Option<Vec<Transition>>,
    pub(crate) after: Option<Vec<(Name, Vec<Transition>)>>,
    pub(crate) invoke: Vec<Invoke>,
    pub(crate) on_done: Option<(Span, Vec<Transition>)>,
    pub(crate) tags: Option<Vec<Name>>,
    pub(crate) meta: Option<TokenStream>,
    pub(crate) output: Option<TokenStream>,
    pub(crate) description: Option<String>,
    /// Default target of a history state.
    pub(crate) target: Option<(Span, Target)>,
    pub(crate) states: Option<Vec<StateDef>>,
    /// Root-only keys (`context`, `version`, policies) in source order, as `json!` input.
    pub(crate) root_keys: Vec<(String, TokenStream)>,
}

/// One candidate transition.
#[derive(Default)]
pub(crate) struct Transition {
    /// Empty for a targetless (`none`) transition.
    pub(crate) targets: Vec<Target>,
    pub(crate) guard: Option<Guard>,
    pub(crate) actions: Option<Vec<Action>>,
    pub(crate) reenter: Option<bool>,
    pub(crate) description: Option<String>,
    pub(crate) meta: Option<TokenStream>,
}

/// How a target string is resolved.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TargetKind {
    /// `sibling` or `sibling.child`: resolved against the parent, then further ancestors.
    Plain,
    /// `.child`: a descendant of the source state.
    Child,
    /// `#id` or `#machine.full.path`.
    Id,
}

/// A transition target; `name.value` is the XState target string (`#a.b`, `.c`, `d`).
pub(crate) struct Target {
    pub(crate) kind: TargetKind,
    pub(crate) name: Name,
}

impl Target {
    /// The path after the `#` / `.` prefix.
    pub(crate) fn path(&self) -> &str {
        match self.kind {
            TargetKind::Plain => &self.name.value,
            TargetKind::Child | TargetKind::Id => &self.name.value[1..],
        }
    }
}

/// A guard reference: `name`, `!name`, `name(params)`.
pub(crate) struct Guard {
    pub(crate) negated: bool,
    pub(crate) name: Name,
    pub(crate) params: Option<TokenStream>,
}

/// An action reference: `name` or `name(params)`.
pub(crate) struct Action {
    pub(crate) name: Name,
    pub(crate) params: Option<TokenStream>,
}

/// An `invoke { ... }` block.
pub(crate) struct Invoke {
    pub(crate) src: Name,
    pub(crate) id: Option<Name>,
    pub(crate) input: Option<TokenStream>,
    pub(crate) system_id: Option<Name>,
    pub(crate) on_done: Option<Vec<Transition>>,
    pub(crate) on_error: Option<Vec<Transition>>,
}

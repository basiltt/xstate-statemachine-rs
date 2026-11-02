//! Procedural macros for [`xstate-core`](https://docs.rs/xstate-core).
//!
//! * [`TypedEvent`](derive@TypedEvent): an enum as the typed event vocabulary of a machine.
//! * [`TypedState`](derive@TypedState): an enum naming a machine's leaf states.
//! * [`statechart!`]: a Rust-literal statechart DSL, checked at compile time, that builds an
//!   `xstate_core::Machine`.
//!
//! The derives implement the `xstate_core::TypedEvent` / `xstate_core::TypedState` traits and
//! need `xstate-core` (plus `serde` for payload types) in the calling crate; `statechart!`
//! additionally needs `serde_json`.
//!
//! ```
//! use xstate_core::{MachineLogic, SyncInterpreter};
//! use xstate_macros::{TypedEvent, TypedState, statechart};
//!
//! #[derive(TypedEvent)]
//! #[event(rename_all = "SCREAMING_SNAKE_CASE")]
//! enum DoorEvent { Open, Close }
//!
//! #[derive(Debug, Clone, Copy, PartialEq, TypedState)]
//! #[state(machine = "door")]
//! enum Door { Closed, Opened }
//!
//! let machine = statechart! {
//!     id: "door",
//!     initial: closed,
//!     states: {
//!         closed { on { OPEN => opened } }
//!         opened { on { CLOSE => closed } }
//!     }
//! }?;
//! DoorEvent::check(&machine).map_err(xstate_core::Error::logic)?;
//! Door::check(&machine).map_err(xstate_core::Error::logic)?;
//!
//! let mut door = SyncInterpreter::new(machine, MachineLogic::dynamic());
//! door.start()?;
//! door.send(DoorEvent::Open)?;
//! assert!(Door::Opened.is_active(&door));
//! # Ok::<(), xstate_core::Error>(())
//! ```

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use syn::{DeriveInput, parse_macro_input};

mod case;
mod chart;
mod typed_event;
mod typed_state;

/// Derive the typed event vocabulary of a machine from an enum.
///
/// Every variant is one event. Its XState `type` is the variant name, re-spelled by
/// `#[event(rename_all = "...")]` on the enum (`"SCREAMING_SNAKE_CASE"`, `"snake_case"`,
/// `"kebab-case"`, `"camelCase"` or `"PascalCase"`) and overridden per variant by
/// `#[event(rename = "order.paid")]`. The payload follows the variant shape:
///
/// | variant | payload |
/// |---|---|
/// | `Checkout` / `Checkout()` | none (`null`) |
/// | `AddItem(Item)` | the serialised value |
/// | `SetNote { text: String }` | an object with one key per field (`r#type` is `"type"`) |
///
/// Payload types must be `serde::Serialize + serde::de::DeserializeOwned`. Generic enums,
/// tuple variants with more than one field and two variants sharing a `type` are compile
/// errors.
///
/// # Generated items
///
/// * `const fn event_type(&self) -> &'static str`
/// * `fn declared_events() -> &'static [&'static str]`, sorted
/// * `fn check(machine: &Machine) -> Result<(), String>`: lists the events the enum declares
///   that no transition of the chart handles (exactly or through a `*` / `prefix.*`
///   wildcard) and the events the chart handles that the enum lacks
/// * `impl From<Enum> for xstate_core::Event`, so a variant can be passed to `send`
/// * `impl TryFrom<xstate_core::Event> for Enum` with `Error = xstate_core::Error`
///   (`InvalidEvent` for an unknown `type` or a payload of the wrong shape; an absent field
///   reads as `null`, so `Option` fields may be omitted)
/// * `impl xstate_core::TypedEvent for Enum`
///
/// The inherent items take the enum's visibility.
///
/// # Example
///
/// ```
/// use serde::{Deserialize, Serialize};
/// use serde_json::json;
/// use xstate_core::Event;
/// use xstate_macros::TypedEvent;
///
/// #[derive(Debug, PartialEq, Serialize, Deserialize)]
/// struct Item { sku: String, qty: u32 }
///
/// #[derive(Debug, PartialEq, TypedEvent)]
/// #[event(rename_all = "SCREAMING_SNAKE_CASE")]
/// enum OrderEvent {
///     Checkout,
///     AddItem(Item),
///     #[event(rename = "order.paid")]
///     Paid { amount: f64, reference: Option<String> },
/// }
///
/// assert_eq!(OrderEvent::Checkout.event_type(), "CHECKOUT");
/// assert_eq!(OrderEvent::declared_events(), ["ADD_ITEM", "CHECKOUT", "order.paid"]);
///
/// let event: Event = OrderEvent::Paid { amount: 9.5, reference: None }.into();
/// assert_eq!(event.ty, "order.paid");
/// assert_eq!(event.payload, json!({ "amount": 9.5, "reference": null }));
///
/// let back = OrderEvent::try_from(Event::with_payload("ADD_ITEM", json!({ "sku": "a", "qty": 2 })))?;
/// assert_eq!(back, OrderEvent::AddItem(Item { sku: "a".into(), qty: 2 }));
/// assert!(OrderEvent::try_from(Event::new("NOPE")).is_err());
/// # Ok::<(), xstate_core::Error>(())
/// ```
#[proc_macro_derive(TypedEvent, attributes(event))]
pub fn derive_typed_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    typed_event::derive(&input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Derive an enum whose unit variants name a machine's leaf states.
///
/// Each variant stands for one full state id: `#[state(id = "door.locked.bolted")]` on the
/// variant, or by default `"<machine>.<variant>"` with the machine id from
/// `#[state(machine = "door")]` on the enum and the variant in `snake_case` (another
/// convention with `#[state(rename_all = "camelCase")]`; see
/// [`TypedEvent`](derive@TypedEvent) for the list). Nested states need an explicit `id`.
/// Variants with fields, generic enums and two variants naming the same id are compile errors.
///
/// # Generated items
///
/// * `const fn id(&self) -> &'static str`
/// * `fn from_id(id: &str) -> Option<Self>`
/// * `fn all() -> &'static [Self]`, in declaration order
/// * `fn is_active<C: Context>(&self, interp: &SyncInterpreter<C>) -> bool`
/// * `fn check(machine: &Machine) -> Result<(), String>`: lists ids that are not states of
///   the chart, ids of states that are not leaves, and leaf states the enum does not cover
/// * `impl xstate_core::TypedState for Enum`
///
/// # Example
///
/// ```
/// use xstate_core::{MachineLogic, SyncInterpreter};
/// use xstate_macros::{TypedState, statechart};
///
/// #[derive(Debug, Clone, Copy, PartialEq, TypedState)]
/// #[state(machine = "door")]
/// enum Door {
///     Closed,
///     #[state(id = "door.locked.bolted")]
///     Bolted,
/// }
///
/// let machine = statechart! {
///     id: "door",
///     initial: closed,
///     states: {
///         closed { on { LOCK => locked } }
///         locked { initial: bolted, states: { bolted {} } }
///     }
/// }?;
/// assert_eq!(Door::check(&machine), Ok(()));
/// assert_eq!(Door::from_id("door.closed"), Some(Door::Closed));
///
/// let mut interp = SyncInterpreter::new(machine, MachineLogic::dynamic());
/// interp.start()?;
/// interp.send("LOCK")?;
/// let active: Vec<Door> = Door::all().iter().copied().filter(|s| s.is_active(&interp)).collect();
/// assert_eq!(active, [Door::Bolted]);
/// # Ok::<(), xstate_core::Error>(())
/// ```
#[proc_macro_derive(TypedState, attributes(state))]
pub fn derive_typed_state(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    typed_state::derive(&input).unwrap_or_else(syn::Error::into_compile_error).into()
}

/// Build an `xstate_core::Machine` from a statechart written as Rust tokens.
///
/// The macro expands to an expression of type `Result<Machine, xstate_core::Error>`: the
/// chart becomes a `serde_json::Value` in XState v5 JSON form, passed to
/// `Machine::from_value`. Every JSON fragment you write (`context`, `meta`, `output`, invoke
/// `input`, action and guard parameters) goes through `::serde_json::json!` verbatim, so it
/// may interpolate Rust expressions; the calling crate needs `serde_json` as a dependency.
///
/// # Grammar
///
/// ```text
/// chart       := item,*                      // must contain `id` and `states`
/// item        := state-item | root-item      // root-items only at the top level
/// root-item   := id: name                    // the machine id
///              | context: JSON | version: JSON | strict: JSON | strictTargets: JSON
///              | strictConfig: JSON | maxIterations: JSON | actionErrorPolicy: JSON
///              | guardErrorPolicy: JSON | onUnhandled: JSON
/// state-item  := type: atomic | compound | final | parallel | history[(shallow | deep)]
///              | id: name                    // custom id, for `#id` targets
///              | initial: name
///              | entry: actions | exit: actions
///              | on { event => transitions,* }
///              | after { delay => transitions,* }
///              | always => transitions
///              | onDone => transitions
///              | invoke { invoke-item,* }    // may be repeated
///              | tags: names
///              | meta: JSON | output: JSON | description: "text"
///              | target: target              // default of a history state
///              | states { state* }
/// state       := name [{ state-item,* }]     // a bare name is an empty atomic state
/// invoke-item := src: name | id: name | systemId: name | input: JSON
///              | onDone => transitions | onError => transitions
/// transitions := transition (| transition)*  // candidates, tried in order
/// transition  := targets [ [option,*] ]
/// targets     := none                        // targetless
///              | target | (target, target, ...)
/// target      := sibling.path | .child.path | #id.path | "any target string"
/// option      := guard: guard | actions: actions | reenter [: bool]
///              | description: "text" | meta: JSON
/// guard       := [!] name [(JSON)]          // `!` negates; `(…)` are guard params
/// actions     := action | [action,*]
/// action      := name [(JSON)]              // `(…)` are action params
/// event       := NAME | a.b.c | a.* | * | "any string"
/// delay       := 1000 | delay_name | "delay name"
/// name        := identifier | "string"
/// names       := name | [name,*]
/// ```
///
/// Items are separated by commas; a comma after an item that ends in `{ ... }` is optional.
/// `JSON` is anything `serde_json::json!` accepts, up to the next top-level comma. Keys keep
/// their XState spelling (`onDone`, `systemId`, `strictTargets`). `none` is the targetless
/// keyword, so a state called `none` must be targeted as a string (`"none"`).
///
/// # Compile-time checks
///
/// Syntax errors, unknown keys and options (with a did-you-mean hint), duplicate keys in one
/// block, duplicate state names among siblings, duplicate events in `on`, duplicate delays in
/// `after`, duplicate custom ids, `initial` not naming a child, compound states with several
/// children but no `initial`, `final` / `history` / `atomic` states with children, `onDone`
/// on a state without children, `target` outside history states, and every transition target
/// that the runtime resolver would not find (siblings and ancestors' siblings for plain
/// paths, descendants for `.child`, custom or full ids for `#id`) or that names the machine
/// root. Everything else (built-in action parameters, policy values, guard semantics) is
/// checked by `Machine::from_value` and reported through the returned `Result`.
///
/// # Example
///
/// ```
/// use serde_json::json;
/// use xstate_core::{Machine, MachineLogic, SyncInterpreter};
/// use xstate_macros::statechart;
///
/// let max_opens = 3;
/// let machine: Machine = statechart! {
///     id: "door",
///     initial: closed,
///     context: { "opens": 0, "max": max_opens },
///     states: {
///         closed {
///             on {
///                 OPEN => opened [guard: isUnlocked, actions: [countOpen]] | jammed,
///                 KICK => broken,
///             }
///         }
///         opened {
///             tags: [lit],
///             after { 1000 => closed },
///             on { CLOSE => closed },
///         }
///         jammed { on { "jam.*" => none [actions: log], RESET => #door.closed } }
///         broken { type: final, output: { "reason": "kicked" } }
///     }
/// }?;
///
/// let as_json = machine.to_json();
/// assert_eq!(
///     as_json["states"]["closed"]["on"]["OPEN"],
///     json!([{ "target": "opened", "guard": "isUnlocked", "actions": ["countOpen"] }, "jammed"]),
/// );
///
/// let logic = MachineLogic::dynamic()
///     .guard("isUnlocked", |_, _, _| Ok(true))
///     .action("countOpen", |_| Ok(()))
///     .action("log", |_| Ok(()));
/// let mut door = SyncInterpreter::new(machine, logic);
/// door.start()?;
/// door.send("OPEN")?;
/// assert_eq!(door.state_ids(), ["door.opened"]);
/// # Ok::<(), xstate_core::Error>(())
/// ```
///
/// A hierarchical chart with a parallel state, history, an invoked service and multi-target
/// transitions:
///
/// ```
/// use xstate_macros::statechart;
///
/// let machine = statechart! {
///     id: "player",
///     initial: idle,
///     states: {
///         idle { on { PLAY => loading } }
///         loading {
///             invoke {
///                 src: fetchMedia,
///                 input: { "url": "https://example.test/a.mp3" },
///                 onDone => playing,
///                 onError => idle [actions: [reportError({ "level": "warn" })]],
///             }
///         }
///         playing {
///             type: parallel,
///             onDone => idle,
///             on { MUTE_AND_PAUSE => (.audio.muted, .transport.paused) },
///             states: {
///                 audio { initial: loud, states: { loud, muted, last { type: history(deep) } } }
///                 transport {
///                     initial: running,
///                     states: { running { on { END => done } }, paused, done { type: final } }
///                 }
///             }
///         }
///     }
/// }?;
/// assert!(machine.state_by_id("player.playing.audio.last").is_some());
/// # Ok::<(), xstate_core::Error>(())
/// ```
///
/// Mistakes in the chart's structure do not compile:
///
/// ```compile_fail
/// let machine = xstate_macros::statechart! {
///     id: "door",
///     initial: closed,
///     states: {
///         closed { on { OPEN => opend } } // error: unknown target `opend`
///         opened {}
///     }
/// };
/// ```
#[proc_macro]
pub fn statechart(input: TokenStream) -> TokenStream {
    chart::statechart(input.into())
        .unwrap_or_else(|errors| {
            // Several `compile_error!`s side by side are not an expression; a block is.
            let errors = errors.into_compile_error();
            quote::quote!({ #errors })
        })
        .into()
}

/// The identifier without a leading `r#`.
fn unraw(ident: &syn::Ident) -> String {
    let s = ident.to_string();
    s.strip_prefix("r#").map_or_else(|| s.clone(), str::to_owned)
}

/// The derives target concrete enums only.
fn reject_generics(generics: &syn::Generics, derive: &str) -> syn::Result<()> {
    if generics.params.is_empty() && generics.where_clause.is_none() {
        return Ok(());
    }
    Err(syn::Error::new_spanned(generics, format!("{derive} cannot be derived for generic enums")))
}

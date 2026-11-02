//! The per-request create → act → persist → discard cycle: `send`, `act`, `peek`, `exists`.
//!
//! Every cycle runs [`xstate_persistence::persisted`] on a blocking thread
//! (`tokio::task::spawn_blocking`): the stores and the interpreter are synchronous.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use xstate_core::engine::StepReport;
use xstate_core::{
    ActiveState, ErrorKind, Event, Machine, Observer, RestartTimers, SyncInterpreter,
};
use xstate_persistence::idempotency::default_key;
use xstate_persistence::receipts::{ReceiptError, ReceiptErrorKind};
use xstate_persistence::{
    Admission, Claim, IdempotencyGuard, ReceiptJson, Session, StoreError, load_interpreter,
    persisted, scope,
};

use super::{Registration, StatechartRegistry};
use crate::auth::principal_or_401;
use crate::body::refuse_reserved_send_keys;
use crate::problem::ApiError;
use crate::receipt::{
    StateView, is_internal_event, receipt_body, receipt_flags, receipt_to_status, state_body,
};

/// Body error that leaves `persisted` without saving (a replay, a refusal, a no-op).
const SKIP_SAVE: &str = "\u{0}xstate-axum: skip save";

fn skip_save() -> StoreError {
    StoreError::Other(SKIP_SAVE.to_owned())
}

/// One event to deliver to one instance.
#[derive(Debug, Clone)]
pub struct SendRequest {
    /// Registered machine name.
    pub machine: String,
    /// Instance key.
    pub key: String,
    /// Event type.
    pub event: String,
    /// Event payload (must not contain `wait` / `priority`).
    pub payload: Map<String, Value>,
    /// The caller (authorizer input; idempotency scope when the registry has an inbox).
    pub principal: Option<String>,
    /// The raw `Idempotency-Key` header value, validated by `send`.
    pub idempotency_key: Option<String>,
    /// Start a fresh instance for an unknown key (default `true`); otherwise 404.
    pub create_if_missing: bool,
}

impl SendRequest {
    /// A request with an empty payload, no principal and no idempotency key.
    #[must_use]
    pub fn new(
        machine: impl Into<String>,
        key: impl Into<String>,
        event: impl Into<String>,
    ) -> Self {
        Self {
            machine: machine.into(),
            key: key.into(),
            event: event.into(),
            payload: Map::new(),
            principal: None,
            idempotency_key: None,
            create_if_missing: true,
        }
    }
}

/// The result of a send: the receipt and its response body (`receipt_body` shape).
#[derive(Debug, Clone, PartialEq)]
pub struct SendOutcome {
    /// The receipt (cached form when `duplicate`).
    pub receipt: ReceiptJson,
    /// The JSON body sent to the client.
    pub body: Value,
}

impl SendOutcome {
    /// HTTP status of the receipt (see [`crate::receipt_to_status`]).
    #[must_use]
    pub fn status(&self) -> u16 {
        receipt_to_status(&self.receipt)
    }

    fn refusal(&self) -> Option<ApiError> {
        match self.receipt.error.as_ref().map(|e| &e.kind) {
            Some(ReceiptErrorKind::IdempotencyMismatch) => Some(ApiError::IdempotencyMismatch),
            Some(ReceiptErrorKind::IdempotencyInFlight) => Some(ApiError::IdempotencyInFlight),
            _ => None,
        }
    }
}

impl IntoResponse for SendOutcome {
    /// An idempotency refusal is a problem (422 / 409); everything else is the receipt body
    /// with [`SendOutcome::status`].
    fn into_response(self) -> Response {
        if let Some(refusal) = self.refusal() {
            return refusal.to_problem().into_response();
        }
        let status = StatusCode::from_u16(self.status()).unwrap_or(StatusCode::OK);
        (status, Json(self.body)).into_response()
    }
}

/// Collects what one cycle did: actions run (no-op detection) and the leaf ids after each
/// changed user event (fan-out).
#[derive(Debug, Default)]
struct Recorder {
    actions: AtomicUsize,
    changed: Mutex<Vec<Vec<String>>>,
}

impl Recorder {
    fn changed(&self) -> Vec<Vec<String>> {
        std::mem::take(&mut *self.changed.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl Observer<Value> for Recorder {
    fn on_start(&self, _m: &Machine, _s: &ActiveState<Value>, report: &StepReport) {
        self.actions.fetch_add(report.actions.len(), Ordering::Relaxed);
    }

    fn on_event(
        &self,
        machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<Value>,
    ) {
        self.actions.fetch_add(report.actions.len(), Ordering::Relaxed);
        if report.changed && !is_internal_event(&event.ty) {
            let ids = state.leaf_ids(machine);
            self.changed.lock().unwrap_or_else(PoisonError::into_inner).push(ids);
        }
    }
}

/// A validated send, ready for the blocking cycle.
struct Prepared {
    reg: Arc<Registration>,
    key: String,
    store_key: String,
    event: Event,
    guard: Option<(IdempotencyGuard, String)>,
    has_idempotency_key: bool,
    create_if_missing: bool,
}

/// What the body of one cycle decided.
enum Step {
    Save(SendOutcome, Vec<Value>),
    Skip(SendOutcome),
}

type ClaimSlot = Arc<Mutex<Option<Claim>>>;

fn take_claim(slot: &ClaimSlot) -> Option<Claim> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

async fn run_blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, ApiError> + Send + 'static,
) -> Result<R, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::Internal(e.to_string()))?
}

fn view_of<'a>(
    reg: &'a Registration,
    interp: &'a SyncInterpreter<Value>,
) -> Result<StateView<'a>, StoreError> {
    let state =
        interp.state().ok_or_else(|| StoreError::Other("interpreter not started".into()))?;
    Ok(StateView::new(&reg.machine, &reg.logic, state))
}

/// The snapshot minus its capture time: equal means nothing changed.
fn comparable(interp: &SyncInterpreter<Value>) -> Result<String, StoreError> {
    let mut snapshot = interp.snapshot()?;
    snapshot.taken_at = 0.0;
    Ok(snapshot.to_json()?)
}

/// Receipt bodies for the changed user events of a cycle, against the final state.
fn publish_bodies(
    reg: &Registration,
    view: &StateView<'_>,
    changed: Vec<Vec<String>>,
) -> Vec<Value> {
    changed
        .into_iter()
        .map(|state_ids| {
            let receipt = ReceiptJson { state_ids, changed: true, ..ReceiptJson::default() };
            receipt_body(view, &receipt, reg.serializer())
        })
        .collect()
}

impl StatechartRegistry {
    /// Validate and prepare a send: draining (503), authorize (403), reserved keys (422),
    /// `Idempotency-Key` (400 malformed, 501 without inbox), principal (401 with an inbox),
    /// strict mode (422).
    fn prepare_send(&self, request: SendRequest) -> Result<Prepared, ApiError> {
        if self.is_draining() {
            return Err(ApiError::ShuttingDown);
        }
        let reg = Arc::clone(self.reg(&request.machine)?);
        let SendRequest { key, event, mut payload, principal, .. } = request;
        self.authorize(principal.as_deref(), &reg.name, &key, Some(&event))?;
        refuse_reserved_send_keys(&payload)?;
        let store_key = self.store_key(&reg.name, &key)?;
        let idem = match request.idempotency_key.as_deref() {
            None | Some("") => None,
            Some(raw) => Some(validated_idempotency_key(raw)?),
        };
        if idem.is_some() && self.inbox.is_none() {
            return Err(ApiError::IdempotencyNotConfigured);
        }
        if let Some(k) = &idem {
            payload.insert("idempotency_key".into(), Value::from(k.clone()));
        }
        let guard = match &self.inbox {
            Some(inbox) => {
                let who = principal_or_401(principal.as_deref())?;
                let scope = scope(&who, reg.machine.id(), &store_key);
                Some((IdempotencyGuard::new(Arc::clone(inbox), who), scope))
            }
            None => None,
        };
        if reg.options.strict && !reg.machine.declared_events().contains(&event) {
            let machine = reg.machine.id().to_owned();
            return Err(xstate_core::Error::UnknownEvent { event, machine }.into());
        }
        let payload = if payload.is_empty() { Value::Null } else { Value::Object(payload) };
        Ok(Prepared {
            reg,
            key,
            store_key,
            event: Event::with_payload(event, payload),
            guard,
            has_idempotency_key: idem.is_some(),
            create_if_missing: request.create_if_missing,
        })
    }

    /// Authorize, honour `Idempotency-Key`, run one persisted cycle and return the receipt.
    ///
    /// A replay or idempotency refusal changes nothing and is **not** saved (saving would bump
    /// the version and make the original request lose with 409); neither is a send that ran no
    /// action and changed nothing on an up-to-date record (battle #275-a). Subscribers hear
    /// changed user events only after the save committed.
    ///
    /// # Errors
    /// Every request-level failure as an [`ApiError`]; idempotency refusals are an `Ok`
    /// outcome whose response is a problem.
    pub async fn send(self: &Arc<Self>, request: SendRequest) -> Result<SendOutcome, ApiError> {
        let prepared = self.prepare_send(request)?;
        let this = Arc::clone(self);
        run_blocking(move || this.send_blocking(&prepared)).await
    }

    /// [`send`](Self::send) answered as a response: the receipt (status from
    /// [`crate::receipt_to_status`]) or an RFC 9457 problem. Never fails.
    pub async fn send_event(self: &Arc<Self>, request: SendRequest) -> Response {
        let (name, key, event) =
            (request.machine.clone(), request.key.clone(), request.event.clone());
        match self.send(request).await {
            Ok(outcome) => outcome.into_response(),
            Err(error) => {
                if matches!(error, ApiError::Store(StoreError::Unavailable(_))) {
                    tracing::warn!(target: "xstate::axum", %name, %key, %event, %error, "store unavailable; answered 503");
                } else if error.status() >= 500 {
                    tracing::error!(target: "xstate::axum", %name, %key, %event, %error, "send failed");
                }
                error.to_problem().into_response()
            }
        }
    }

    fn send_blocking(&self, p: &Prepared) -> Result<SendOutcome, ApiError> {
        let reg = &p.reg;
        let mut options = (self.options)();
        options.create_if_missing = p.create_if_missing;
        let recorder = Arc::new(Recorder::default());
        options.observers.push(recorder.clone());
        options.observers.extend(self.observers_for(&p.store_key));
        let claim: ClaimSlot = Arc::default();
        let mut stash = None;
        let result = persisted(
            self.store.as_ref(),
            &p.store_key,
            &reg.machine,
            &reg.logic,
            &options,
            |interp, session| match self.step(p, interp, &recorder, &claim)? {
                Step::Skip(outcome) => {
                    stash = Some(outcome);
                    Err(skip_save())
                }
                Step::Save(outcome, bodies) => {
                    self.after_commit(session, p, &claim, &outcome.receipt, bodies);
                    stash = Some(outcome);
                    Ok(())
                }
            },
        );
        if let Some(unsettled) = take_claim(&claim)
            && let Err(error) = unsettled.release()
        {
            tracing::error!(target: "xstate::axum", %error, "idempotency claim release failed");
        }
        match result {
            Ok(()) => {}
            Err(StoreError::Other(m)) if m == SKIP_SAVE => {}
            Err(error) => {
                return Err(ApiError::from(error).with_machine_version(reg.machine.version()));
            }
        }
        stash.ok_or_else(|| ApiError::Internal("send produced no outcome".into()))
    }

    /// Settle the idempotency claim and publish to subscribers once the save committed.
    fn after_commit(
        &self,
        session: &mut Session,
        p: &Prepared,
        claim: &ClaimSlot,
        receipt: &ReceiptJson,
        bodies: Vec<Value>,
    ) {
        let (claim, receipt) = (Arc::clone(claim), receipt.clone());
        let subscribers = Arc::clone(self.subscribers());
        let (name, key) = (p.reg.name.clone(), p.key.clone());
        session.after_commit(move || {
            if let Some(claim) = take_claim(&claim) {
                claim.settle(&receipt)?;
            }
            if !bodies.is_empty() {
                subscribers.publish(&name, &key, bodies);
            }
            Ok(())
        });
    }

    /// The idempotency gate: `Some(outcome)` answers without processing.
    fn admit(
        p: &Prepared,
        interp: &SyncInterpreter<Value>,
        claim: &ClaimSlot,
    ) -> Result<Option<SendOutcome>, StoreError> {
        let Some((guard, scope)) = &p.guard else { return Ok(None) };
        let state_ids = interp.state_ids();
        let receipt = match guard.check(scope, &p.event) {
            Ok(Admission::Admit(admitted)) => {
                *claim.lock().unwrap_or_else(PoisonError::into_inner) = Some(admitted);
                return Ok(None);
            }
            Ok(Admission::Duplicate(receipt)) => receipt,
            Ok(refused) => {
                let key = default_key(&p.event).unwrap_or_default();
                refused.refusal_receipt(&key, &state_ids).unwrap_or_default()
            }
            Err(StoreError::Unavailable(message)) => ReceiptJson {
                state_ids,
                error: Some(ReceiptError::new(ReceiptErrorKind::InboxUnavailable, message)),
                ..ReceiptJson::default()
            },
            Err(error) => return Err(error),
        };
        let view = view_of(&p.reg, interp)?;
        let body = receipt_body(&view, &receipt, p.reg.serializer());
        Ok(Some(SendOutcome { receipt, body }))
    }

    fn step(
        &self,
        p: &Prepared,
        interp: &mut SyncInterpreter<Value>,
        recorder: &Recorder,
        claim: &ClaimSlot,
    ) -> Result<Step, StoreError> {
        if let Some(answer) = Self::admit(p, interp, claim)? {
            return Ok(Step::Skip(answer));
        }
        let reg = &p.reg;
        let before_state = interp
            .state()
            .cloned()
            .ok_or_else(|| StoreError::Other("interpreter not started".into()))?;
        let before = comparable(interp)?;
        let receipt = match interp.send(p.event.clone()) {
            Ok(r) => {
                let flags = receipt_flags(&reg.machine, &before_state, &p.event.ty, &r);
                ReceiptJson::from_receipt(&r, flags, None)
            }
            Err(e) if matches!(e.kind(), ErrorKind::UnknownEvent | ErrorKind::InvalidEvent) => {
                return Err(e.into());
            }
            Err(e) => ReceiptJson {
                state_ids: interp.state_ids(),
                error: Some(ReceiptError::from_engine(&e)),
                ..ReceiptJson::default()
            },
        };
        let view = view_of(reg, interp)?;
        let outcome =
            SendOutcome { body: receipt_body(&view, &receipt, reg.serializer()), receipt };
        let tracked = claim
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(Claim::is_tracked);
        if !tracked
            && !p.has_idempotency_key
            && recorder.actions.load(Ordering::Relaxed) == 0
            && comparable(interp)? == before
            && self.stored_is_current(&p.store_key, &reg.machine)?
        {
            return Ok(Step::Skip(outcome));
        }
        Ok(Step::Save(outcome, publish_bodies(reg, &view, recorder.changed())))
    }

    /// The stored record exists and was written by this machine version: a no-op needs no
    /// rewrite (a brand-new or lazily migrated instance must be saved even by a no-op).
    fn stored_is_current(&self, store_key: &str, machine: &Machine) -> Result<bool, StoreError> {
        Ok(self
            .store
            .load(store_key)?
            .is_some_and(|r| r.machine_version == machine.version().unwrap_or("")))
    }

    /// Run `f` on a started interpreter for `key` inside one persisted cycle (Python `act()`):
    /// saved with `expected_version` when `f` returns `Ok`, discarded when it returns `Err`.
    /// Changed user events are published to subscribers after the save.
    ///
    /// No authorization and no idempotency here: use [`send`](Self::send) for client events.
    ///
    /// # Errors
    /// [`ApiError::ShuttingDown`] while draining; store errors (409 conflict, 404 with
    /// `create_if_missing = false`, ...); `f`'s own error.
    pub async fn act<R, F>(
        self: &Arc<Self>,
        name: &str,
        key: &str,
        create_if_missing: bool,
        f: F,
    ) -> Result<R, ApiError>
    where
        R: Send + 'static,
        F: FnOnce(&mut SyncInterpreter<Value>) -> Result<R, StoreError> + Send + 'static,
    {
        if self.is_draining() {
            return Err(ApiError::ShuttingDown);
        }
        let reg = Arc::clone(self.reg(name)?);
        let store_key = self.store_key(name, key)?;
        let (this, key) = (Arc::clone(self), key.to_owned());
        run_blocking(move || this.act_blocking(&reg, &key, &store_key, create_if_missing, f)).await
    }

    fn act_blocking<R, F>(
        &self,
        reg: &Registration,
        key: &str,
        store_key: &str,
        create_if_missing: bool,
        f: F,
    ) -> Result<R, ApiError>
    where
        F: FnOnce(&mut SyncInterpreter<Value>) -> Result<R, StoreError>,
    {
        let mut options = (self.options)();
        options.create_if_missing = create_if_missing;
        let recorder = Arc::new(Recorder::default());
        options.observers.push(recorder.clone());
        options.observers.extend(self.observers_for(store_key));
        let mut f = Some(f);
        persisted(
            self.store.as_ref(),
            store_key,
            &reg.machine,
            &reg.logic,
            &options,
            |interp, session| {
                let f = f.take().ok_or_else(|| StoreError::Other("act body ran twice".into()))?;
                let result = f(interp)?;
                let bodies = publish_bodies(reg, &view_of(reg, interp)?, recorder.changed());
                let subscribers = Arc::clone(self.subscribers());
                let (name, key) = (reg.name.clone(), key.to_owned());
                session.after_commit(move || {
                    subscribers.publish(&name, &key, bodies);
                    Ok(())
                });
                Ok(result)
            },
        )
        .map_err(|e| ApiError::from(e).with_machine_version(reg.machine.version()))
    }

    /// The current state body **without** saving or running anything (GET, stream connect).
    /// A missing instance reports the machine's initial state, computed with the pure
    /// transition API (no entry action runs as a side effect of a read, battle #275-a).
    ///
    /// # Errors
    /// Store errors; [`ApiError::SnapshotDrift`] for a snapshot of another chart revision.
    pub async fn peek(self: &Arc<Self>, name: &str, key: &str) -> Result<Value, ApiError> {
        let reg = Arc::clone(self.reg(name)?);
        let store_key = self.store_key(name, key)?;
        let this = Arc::clone(self);
        run_blocking(move || {
            this.peek_blocking(&reg, &store_key)
                .map_err(|e| e.with_machine_version(reg.machine.version()))
        })
        .await
    }

    fn peek_blocking(&self, reg: &Registration, store_key: &str) -> Result<Value, ApiError> {
        let mut options = (self.options)();
        options.create_if_missing = false;
        options.restart_timers = RestartTimers::Parked;
        options.observers.clear();
        let clock = Arc::clone(&options.clock);
        match load_interpreter(
            self.store.as_ref(),
            store_key,
            &reg.machine,
            &reg.logic,
            clock,
            &options,
        ) {
            Ok((interp, _)) => Ok(state_body(&view_of(reg, &interp)?, reg.serializer())),
            Err(StoreError::NotFound(_)) => {
                let (state, _) =
                    xstate_core::pure::initial_transition(&reg.machine, &reg.logic, &Value::Null)?;
                Ok(state_body(&StateView::new(&reg.machine, &reg.logic, &state), reg.serializer()))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Whether instance `key` of `name` has a stored snapshot.
    ///
    /// # Errors
    /// Store errors; invalid key.
    pub async fn exists(self: &Arc<Self>, name: &str, key: &str) -> Result<bool, ApiError> {
        let store_key = self.store_key(name, key)?;
        let store = Arc::clone(&self.store);
        run_blocking(move || Ok(store.load(&store_key)?.is_some())).await
    }
}

fn validated_idempotency_key(raw: &str) -> Result<String, ApiError> {
    xstate_persistence::validate_idempotency_key(raw)
        .map_err(|_| ApiError::BadRequest("Invalid Idempotency-Key header".into()))?;
    Ok(raw.to_owned())
}

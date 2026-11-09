//! One envelope, synchronously: validate, look up, dedup, run the handler, dead-letter.

use std::panic::{AssertUnwindSafe, catch_unwind};

use serde_json::{Map, Value};
use xstate_core::redact::RedactKeys;
use xstate_core::{Event, Machine};
use xstate_persistence::idempotency::DEFAULT_KEY_FIELDS;
use xstate_persistence::{InboxEntry, fingerprint, scope};

use super::{
    Dispatch, DispatchResult, EnvelopeHandler, HandlerError, InboundDispatcher, OnUnknown,
};
use crate::dead_letter::{DeadLetter, ErrorEntry};
use crate::envelope::Envelope;
use crate::error::EnvelopeError;
use crate::py::{repr_str, wall_now};

/// A terminal outcome of one envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Processed,
    Duplicate,
    Ignored,
    DeadLettered(&'static str),
}

impl Outcome {
    fn label(self) -> String {
        match self {
            Self::Processed => "processed".into(),
            Self::Duplicate => "duplicate".into(),
            Self::Ignored => "ignored".into(),
            Self::DeadLettered(reason) => format!("dead_lettered:{reason}"),
        }
    }
}

/// Why an envelope was not settled.
enum Flow {
    /// A counted failure below `max_attempts`: requeue.
    Retry,
    /// An infrastructure failure (inbox / dead-letter store down): requeue, not counted.
    Requeue(String),
    /// A failure outside the handler (a panicking hook): counted, dead-lettered at the cap.
    Counted(String),
}

/// What the inbox says before the handler runs.
enum Gate {
    /// Run the handler; settle the claim `(scope, key)` afterwards, if any.
    Run(Option<(String, String)>),
    /// Answered without running.
    Answer(Result<Outcome, Flow>),
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".to_owned())
}

impl<H: EnvelopeHandler> InboundDispatcher<H> {
    /// Process one envelope synchronously; never fails and never panics on a panicking
    /// handler or hook.
    ///
    /// A transient failure below `max_attempts` is reported as `retried` (the caller requeues).
    /// A failure of the inbox or the dead-letter store is logged and also reported as
    /// `retried`, uncounted: an outage is not the message's fault (X0.8).
    pub fn handle(&self, envelope: &Envelope, topic: Option<&str>) -> DispatchResult {
        let flow = catch_unwind(AssertUnwindSafe(|| self.handle_inner(envelope, topic)))
            .unwrap_or_else(|p| Err(Flow::Counted(panic_message(p.as_ref()))));
        let outcome = match flow {
            Ok(outcome) => Some(outcome),
            Err(Flow::Retry) => None,
            Err(Flow::Requeue(why)) => {
                tracing::error!(target: "xstate::eda", envelope = %envelope.id, %why, "dispatch failed outside the machine; requeueing");
                None
            }
            Err(Flow::Counted(why)) => {
                tracing::error!(target: "xstate::eda", envelope = %envelope.id, %why, "dispatch hook panicked; requeueing");
                self.outside_failure(envelope, topic, &why)
            }
        };
        let mut res = DispatchResult::default();
        match outcome {
            None => res.retried += 1,
            Some(Outcome::Processed) => res.processed += 1,
            Some(Outcome::Duplicate) => res.duplicates += 1,
            Some(Outcome::Ignored) => res.ignored += 1,
            Some(Outcome::DeadLettered(_)) => res.dead_lettered += 1,
        }
        let label = outcome.map_or_else(|| "retry".to_owned(), Outcome::label);
        res.outcomes.push((envelope.id.clone(), label));
        res
    }

    /// Count a failure outside the handler; dead-letter at the cap (`None` = requeue).
    fn outside_failure(&self, env: &Envelope, topic: Option<&str>, why: &str) -> Option<Outcome> {
        if self.bump(env) < self.max_attempts {
            return None;
        }
        self.dead_letter(env, topic, "max_attempts", ("PanicError", why), None).ok()
    }

    fn prepare(&self, env: &Envelope) -> Result<Event, EnvelopeError> {
        env.validate()?;
        if env.subject.as_deref().is_none_or(str::is_empty) {
            return Err(EnvelopeError::corrupt("envelope has no subject (the instance key)"));
        }
        let name = self.event_type.as_ref().map(|f| f(env));
        env.to_event(name.as_deref())
    }

    fn handle_inner(&self, env: &Envelope, topic: Option<&str>) -> Result<Outcome, Flow> {
        let event = match self.prepare(env) {
            Ok(event) => event,
            Err(e) => {
                return self.dead_letter(
                    env,
                    topic,
                    "corrupt",
                    (e.type_name(), &e.to_string()),
                    None,
                );
            }
        };
        let Some(machine) = self.machine_for(&env.ty) else {
            if self.on_unknown == OnUnknown::Ignore {
                return Ok(Outcome::Ignored);
            }
            let message = format!("no machine handles type {}", repr_str(&env.ty));
            return self.dead_letter(env, topic, "unknown_event", ("LookupError", &message), None);
        };
        let key = (self.key_for)(env, &machine);
        let claim = match self.gate(env, &event, &machine, &key, topic) {
            Gate::Run(claim) => claim,
            Gate::Answer(answer) => return answer,
        };
        let dispatch = Dispatch { envelope: env, event, machine: &machine, key: &key, topic };
        let result = catch_unwind(AssertUnwindSafe(|| self.handler.handle(&dispatch)))
            .unwrap_or_else(|p| Err(HandlerError::failed("PanicError", panic_message(p.as_ref()))));
        self.settle_claim(claim.as_ref(), result.as_ref().ok());
        match result {
            Ok(_) => {
                self.forget(env);
                Ok(Outcome::Processed)
            }
            Err(HandlerError::InstanceDone { message }) => self.dead_letter(
                env,
                topic,
                "instance_done",
                ("InterpreterStoppedError", &message),
                Some(&machine),
            ),
            Err(HandlerError::Requeue { message }) => Err(Flow::Requeue(message)),
            Err(HandlerError::Failed { error_type, message }) => {
                self.failed(env, topic, (&error_type, &message), &machine)
            }
        }
    }

    /// Consult the inbox: answer a redelivery, or claim the key for this delivery.
    fn gate(
        &self,
        env: &Envelope,
        event: &Event,
        machine: &Machine,
        key: &str,
        topic: Option<&str>,
    ) -> Gate {
        let Some(inbox) = self.inbox.as_deref() else { return Gate::Run(None) };
        let Some(dkey) = (self.dedup_key)(env).filter(|k| !k.is_empty()) else {
            return Gate::Run(None);
        };
        let scope = scope(&env.source, machine.id(), key);
        let fp = fingerprint(&event.ty, &event.payload, &DEFAULT_KEY_FIELDS);
        let known =
            |entry: InboxEntry| Gate::Answer(self.known(env, topic, machine, &dkey, &fp, &entry));
        match inbox.get(&scope, &dkey) {
            Err(e) => return Gate::Answer(Err(Flow::Requeue(e.to_string()))),
            Ok(Some(entry)) => return known(entry),
            Ok(None) => {}
        }
        match inbox.claim(&scope, &dkey, &fp, self.inbox_ttl_s) {
            Err(e) => Gate::Answer(Err(Flow::Requeue(e.to_string()))),
            Ok(true) => Gate::Run(Some((scope, dkey.clone()))),
            Ok(false) => match inbox.get(&scope, &dkey) {
                Err(e) => Gate::Answer(Err(Flow::Requeue(e.to_string()))),
                Ok(Some(entry)) => known(entry),
                Ok(None) => Gate::Answer(Err(Flow::Retry)),
            },
        }
    }

    /// A live inbox entry: a mismatch is poison, a receipt a duplicate, an in-flight claim a
    /// counted retry (Python `IdempotencyInFlightError`).
    fn known(
        &self,
        env: &Envelope,
        topic: Option<&str>,
        machine: &Machine,
        dkey: &str,
        fp: &str,
        entry: &InboxEntry,
    ) -> Result<Outcome, Flow> {
        if entry.fingerprint != fp {
            return self.dead_letter(
                env,
                topic,
                "idempotency_mismatch",
                ("IdempotencyMismatchError", dkey),
                Some(machine),
            );
        }
        if entry.receipt_json.is_some() {
            self.forget(env);
            return Ok(Outcome::Duplicate);
        }
        let message =
            format!("Idempotency key {} is being processed; retry shortly.", repr_str(dkey));
        self.failed(env, topic, ("IdempotencyInFlightError", &message), machine)
    }

    /// Mark the claim with the receipt, or release it so a retry can claim again.
    fn settle_claim(
        &self,
        claim: Option<&(String, String)>,
        receipt: Option<&xstate_persistence::ReceiptJson>,
    ) {
        let (Some((scope, key)), Some(inbox)) = (claim, self.inbox.as_deref()) else { return };
        let res = match receipt {
            Some(r) => inbox.mark(scope, key, &r.to_json(), self.inbox_ttl_s),
            None => inbox.release(scope, key),
        };
        if let Err(e) = res {
            tracing::error!(target: "xstate::eda", %scope, %key, error = %e, "inbox settle failed");
        }
    }

    fn failed(
        &self,
        env: &Envelope,
        topic: Option<&str>,
        error: (&str, &str),
        machine: &Machine,
    ) -> Result<Outcome, Flow> {
        if self.bump(env) >= self.max_attempts {
            return self.dead_letter(env, topic, "max_attempts", error, Some(machine));
        }
        Err(Flow::Retry)
    }

    /// Write the dead letter (always redacted); a store failure requeues.
    fn dead_letter(
        &self,
        env: &Envelope,
        topic: Option<&str>,
        reason: &'static str,
        (ty, message): (&str, &str),
        machine: Option<&Machine>,
    ) -> Result<Outcome, Flow> {
        let record = self.record(
            env,
            topic,
            reason,
            ErrorEntry::new("dispatcher", reason, ty, message),
            machine,
        );
        let put_res = catch_unwind(AssertUnwindSafe(|| self.dead_letters.put(record)));
        match put_res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(Flow::Requeue(format!("dead-lettering failed: {e}"))),
            Err(p) => {
                return Err(Flow::Requeue(format!(
                    "dead-letter sink panicked: {}",
                    panic_message(p.as_ref())
                )));
            }
        }
        self.forget(env);
        Ok(Outcome::DeadLettered(reason))
    }

    fn record(
        &self,
        env: &Envelope,
        topic: Option<&str>,
        reason: &str,
        error: ErrorEntry,
        machine: Option<&Machine>,
    ) -> DeadLetter {
        let keys = RedactKeys::default();
        let payload = match &env.data {
            Value::Object(map) => keys.redact(&Value::Object(map.clone())),
            _ => Value::Object(Map::new()),
        };
        let mut event = Map::new();
        event.insert("type".into(), Value::from(env.ty.clone()));
        event.insert("payload".into(), payload);
        let machine_id = machine
            .map(|m| m.id().to_owned())
            .or_else(|| env.machineid.clone().filter(|m| !m.is_empty()))
            .unwrap_or_else(|| "?".to_owned());
        let mut rec = DeadLetter::new(machine_id, "", Value::Object(event), wall_now());
        rec.attempts = Some(i64::try_from(self.attempts_of(env).max(1)).unwrap_or(i64::MAX));
        rec.errors = vec![error];
        rec.id.clone_from(&env.id);
        reason.clone_into(&mut rec.reason);
        rec.envelope = Some(keys.redact(&Value::Object(env.to_dict())));
        rec.topic = topic.map(str::to_owned);
        rec.machine_hash = machine.map(|m| m.structure_hash().to_owned());
        rec.machine_version = machine.and_then(|m| m.version()).map(str::to_owned);
        rec
    }
}

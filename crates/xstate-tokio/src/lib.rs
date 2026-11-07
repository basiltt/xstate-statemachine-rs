//! Asynchronous interpreter for [`xstate_core`] machines on the tokio runtime.
//!
//! [`spawn`] runs a machine as a task that owns its state and drives the sans-I/O step
//! engine (ADR-0004): the mailbox, `after` timers (on `tokio::time`, so paused time works in
//! tests), asynchronous services, child machine actors, `sendTo`/`sendParent` routing and a
//! `systemId` registry. The returned [`Interpreter`] is a cheap, cloneable handle.
//!
//! ```
//! use xstate_core::{Machine, MachineLogic};
//! use xstate_tokio::{Options, spawn};
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> xstate_core::Result<()> {
//! let machine = Machine::from_json(r#"{"id":"t","initial":"off","states":{
//!   "off":{"on":{"TOGGLE":"on"}},"on":{"on":{"TOGGLE":"off"}}}}"#)?;
//! let actor = spawn(machine, MachineLogic::dynamic(), Options::default())?;
//! actor.send_and_wait("TOGGLE").await?;
//! assert_eq!(actor.view().state_ids, ["t.on"]);
//! actor.stop().await;
//! # Ok(()) }
//! ```

#![forbid(unsafe_code)]

mod actor;

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};
use xstate_core::{
    Context, DynContext, Error, Event, Machine, MachineLogic, Receipt, Result, Snapshot, Status,
};

pub use actor::System;

/// What to do when the bounded mailbox is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// `send` waits for room (default).
    #[default]
    Block,
    /// `send` drops the new event and reports it through observers.
    DropNewest,
    /// `send` fails with [`Error::LimitExceeded`].
    Reject,
}

/// Spawn options.
#[derive(Debug, Clone)]
pub struct Options {
    /// Mailbox capacity (events queued but not yet processed).
    pub mailbox_capacity: usize,
    /// Behaviour when the mailbox is full.
    pub overflow: Overflow,
    /// Actor input passed to the context factory.
    pub input: Value,
}

impl Default for Options {
    fn default() -> Self {
        Self { mailbox_capacity: 1024, overflow: Overflow::Block, input: Value::Null }
    }
}

/// A lightweight, always-current summary of an actor, available without messaging the task.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    /// Lifecycle status.
    pub status: Status,
    /// Active leaf state ids, sorted.
    pub state_ids: Vec<String>,
    /// Machine output once done.
    pub output: Value,
    /// Error message once errored.
    pub error: Option<String>,
}

pub(crate) enum Msg {
    Event(Event, Option<oneshot::Sender<Result<Receipt>>>),
    Snapshot(oneshot::Sender<Result<Snapshot>>),
    Stop(oneshot::Sender<()>),
}

/// Handle to a running machine actor. Cloning shares the same actor; the actor stops when it
/// completes, errors, is stopped, or every handle has been dropped.
pub struct Interpreter<C: Context = DynContext> {
    machine: Machine,
    tx: mpsc::Sender<Msg>,
    overflow: Overflow,
    view: watch::Receiver<View>,
    _marker: std::marker::PhantomData<fn() -> C>,
}

impl<C: Context> Clone for Interpreter<C> {
    fn clone(&self) -> Self {
        Self {
            machine: self.machine.clone(),
            tx: self.tx.clone(),
            overflow: self.overflow,
            view: self.view.clone(),
            _marker: std::marker::PhantomData,
        }
    }
}

impl<C: Context> std::fmt::Debug for Interpreter<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interpreter")
            .field("machine", &self.machine.id())
            .field("overflow", &self.overflow)
            .field("view", &*self.view.borrow())
            .finish_non_exhaustive()
    }
}

/// Start a machine as a tokio task on the current runtime. Logic is validated and the initial
/// configuration entered before this returns, so configuration problems surface here.
///
/// # Errors
/// Missing implementations, failing entry actions, or no tokio runtime.
pub fn spawn<C: Context>(
    machine: Machine,
    logic: MachineLogic<C>,
    options: Options,
) -> Result<Interpreter<C>> {
    actor::spawn_root(machine, Arc::new(logic), options, Vec::new())
}

/// Like [`spawn`], with observers attached before the machine starts.
///
/// # Errors
/// See [`spawn`].
pub fn spawn_with_observers<C: Context>(
    machine: Machine,
    logic: MachineLogic<C>,
    options: Options,
    observers: Vec<Arc<dyn xstate_core::Observer<C>>>,
) -> Result<Interpreter<C>> {
    actor::spawn_root(machine, Arc::new(logic), options, observers)
}

impl<C: Context> Interpreter<C> {
    /// The compiled machine.
    #[must_use]
    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Current summary (status, active states, output, error).
    #[must_use]
    pub fn view(&self) -> View {
        self.view.borrow().clone()
    }

    /// Watch channel of summaries, updated after every processed event.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<View> {
        self.view.clone()
    }

    /// Queue an event without waiting for it to be processed.
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] when the actor has stopped; [`Error::LimitExceeded`] under
    /// [`Overflow::Reject`] when the mailbox is full.
    pub async fn send(&self, event: impl Into<Event>) -> Result<()> {
        let event = event.into();
        match self.overflow {
            Overflow::Block => self.tx.send(Msg::Event(event, None)).await.map_err(|_| stopped()),
            Overflow::DropNewest | Overflow::Reject => self.try_send(event),
        }
    }

    /// Queue an event from synchronous code (never waits).
    ///
    /// # Errors
    /// [`Error::InvalidLifecycle`] when the actor has stopped; [`Error::LimitExceeded`] when the
    /// mailbox is full and the overflow policy is not `DropNewest`.
    pub fn try_send(&self, event: impl Into<Event>) -> Result<()> {
        match self.tx.try_send(Msg::Event(event.into(), None)) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => Err(stopped()),
            Err(mpsc::error::TrySendError::Full(Msg::Event(ev, _))) => {
                if self.overflow == Overflow::DropNewest {
                    tracing::warn!(target: "xstate", event = %ev.ty, "mailbox full; event dropped");
                    Ok(())
                } else {
                    Err(Error::LimitExceeded(format!(
                        "mailbox full ({} events)",
                        self.tx.max_capacity()
                    )))
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(Error::LimitExceeded("mailbox full".into()))
            }
        }
    }

    /// Queue an event and wait until it (and everything it triggered synchronously) has been
    /// processed.
    ///
    /// The overflow policy applies before waiting: under [`Overflow::Reject`] a full mailbox is
    /// [`Error::LimitExceeded`] (never a block), and under [`Overflow::DropNewest`] the event is
    /// dropped and the call returns `LimitExceeded` naming the drop because there is no receipt
    /// to wait for.
    ///
    /// # Errors
    /// Engine errors from processing the event, the actor stopping first, or the mailbox being
    /// full under a non-blocking overflow policy.
    pub async fn send_and_wait(&self, event: impl Into<Event>) -> Result<Receipt> {
        let event = event.into();
        let (reply_tx, reply_rx) = oneshot::channel();
        let msg = Msg::Event(event, Some(reply_tx));
        match self.overflow {
            Overflow::Block => {
                self.tx.send(msg).await.map_err(|_| stopped())?;
            }
            Overflow::DropNewest | Overflow::Reject => match self.tx.try_send(msg) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => return Err(stopped()),
                Err(mpsc::error::TrySendError::Full(Msg::Event(ev, _))) => {
                    if self.overflow == Overflow::DropNewest {
                        tracing::warn!(target: "xstate", event = %ev.ty, "mailbox full; event dropped");
                        return Err(Error::LimitExceeded("mailbox full; event dropped".into()));
                    }
                    return Err(Error::LimitExceeded(format!(
                        "mailbox full ({} events)",
                        self.tx.max_capacity()
                    )));
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    return Err(Error::LimitExceeded("mailbox full".into()));
                }
            },
        }
        reply_rx.await.map_err(|_| stopped())?
    }

    /// Capture a Python-compatible snapshot.
    ///
    /// # Errors
    /// The actor stopping before answering.
    pub async fn snapshot(&self) -> Result<Snapshot> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Snapshot(tx)).await.map_err(|_| stopped())?;
        rx.await.map_err(|_| stopped())?
    }

    /// Wait until the machine is no longer running (done, error or stopped).
    pub async fn wait_done(&self) -> View {
        let mut rx = self.view.clone();
        loop {
            let view = rx.borrow().clone();
            if view.status != Status::Running {
                return view;
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    }

    /// Stop the actor and its children; resolves once the task has shut down.
    pub async fn stop(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Stop(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }

    pub(crate) fn from_parts(
        machine: Machine,
        tx: mpsc::Sender<Msg>,
        overflow: Overflow,
        view: watch::Receiver<View>,
    ) -> Self {
        Self { machine, tx, overflow, view, _marker: std::marker::PhantomData }
    }

    pub(crate) fn sender(&self) -> mpsc::Sender<Msg> {
        self.tx.clone()
    }
}

fn stopped() -> Error {
    Error::InvalidLifecycle { status: "stopped", message: "the actor is no longer running".into() }
}

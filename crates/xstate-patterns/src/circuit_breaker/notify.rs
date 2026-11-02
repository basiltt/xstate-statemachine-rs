//! Observer notifications collected under the breaker's lock and delivered after it is
//! released, so an observer may call back into the breaker (`state()`, `call()`) without
//! deadlocking on the non-reentrant lock.

use std::sync::{Arc, Mutex, PoisonError};

use xstate_core::{ActiveState, DynContext, Error, Event, Machine, Observer, StepReport};

/// One recorded observer callback.
pub(super) enum Notice {
    Start(ActiveState<DynContext>, StepReport),
    Event(Event, StepReport, ActiveState<DynContext>),
    Dropped(Event, &'static str),
    /// `Error` is not `Clone`: observers receive an [`Error::Logic`] carrying the message.
    Error(Error),
    Stop(ActiveState<DynContext>),
}

/// Shared queue the [`Collector`] fills and the breaker drains.
pub(super) type Queue = Arc<Mutex<Vec<Notice>>>;

/// The only observer attached to the internal interpreter: it records instead of calling out.
pub(super) struct Collector {
    queue: Queue,
}

impl Collector {
    pub(super) const fn new(queue: Queue) -> Self {
        Self { queue }
    }

    fn push(&self, notice: Notice) {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner).push(notice);
    }
}

impl Observer<DynContext> for Collector {
    fn on_start(&self, _machine: &Machine, state: &ActiveState<DynContext>, report: &StepReport) {
        self.push(Notice::Start(state.clone(), report.clone()));
    }

    fn on_event(
        &self,
        _machine: &Machine,
        event: &Event,
        report: &StepReport,
        state: &ActiveState<DynContext>,
    ) {
        self.push(Notice::Event(event.clone(), report.clone(), state.clone()));
    }

    fn on_event_dropped(&self, _machine: &Machine, event: &Event, reason: &'static str) {
        self.push(Notice::Dropped(event.clone(), reason));
    }

    fn on_error(&self, _machine: &Machine, error: &Error) {
        self.push(Notice::Error(Error::logic(error)));
    }

    fn on_stop(&self, _machine: &Machine, state: &ActiveState<DynContext>) {
        self.push(Notice::Stop(state.clone()));
    }
}

/// Take everything queued so far.
pub(super) fn drain(queue: &Queue) -> Vec<Notice> {
    std::mem::take(&mut *queue.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Replay `notices` to the user's observers, in order.
pub(super) fn deliver(
    machine: &Machine,
    observers: &[Arc<dyn Observer<DynContext>>],
    notices: &[Notice],
) {
    for notice in notices {
        for observer in observers {
            match notice {
                Notice::Start(state, report) => observer.on_start(machine, state, report),
                Notice::Event(event, report, state) => {
                    observer.on_event(machine, event, report, state);
                }
                Notice::Dropped(event, reason) => observer.on_event_dropped(machine, event, reason),
                Notice::Error(error) => observer.on_error(machine, error),
                Notice::Stop(state) => observer.on_stop(machine, state),
            }
        }
    }
}

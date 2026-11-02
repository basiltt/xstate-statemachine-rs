//! The two adapters over [`FakeCore`] and a runtime-agnostic sleep for the async poll loop.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::FakeCore;
use crate::broker::{BrokerAdapter, Delivery, SyncBrokerAdapter};
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// How often an idle poll looks at the queue again (Python `_POLL_S`).
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// In-memory async [`BrokerAdapter`].
///
/// `poll(topic, None)` waits forever for traffic (like a real consumer); in a test that is a
/// hang, so pass a timeout (`Some(Duration::ZERO)` = only what is queued). A delivery left
/// unsettled stays in flight; ack / nack it or [`clear`](FakeCore::clear). The idle wait needs
/// no runtime (a helper thread wakes the task).
#[derive(Debug, Default)]
pub struct FakeBrokerAdapter {
    core: FakeCore,
}

/// In-memory blocking [`SyncBrokerAdapter`].
#[derive(Debug, Default)]
pub struct SyncFakeBrokerAdapter {
    core: FakeCore,
}

macro_rules! constructors {
    ($ty:ident) => {
        impl $ty {
            /// A fake broker with the default size cap.
            #[must_use]
            pub fn new() -> Self {
                Self::default()
            }

            /// A fake broker refusing envelopes over `max_bytes` on the wire.
            #[must_use]
            pub fn with_max_bytes(max_bytes: usize) -> Self {
                Self { core: FakeCore::new(max_bytes) }
            }
        }

        impl Deref for $ty {
            type Target = FakeCore;

            fn deref(&self) -> &FakeCore {
                &self.core
            }
        }
    };
}

constructors!(FakeBrokerAdapter);
constructors!(SyncFakeBrokerAdapter);

/// The instant `timeout` from now; `None` (no deadline) for none given or one too far away to
/// represent.
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|t| Instant::now().checked_add(t))
}

impl BrokerAdapter for FakeBrokerAdapter {
    fn publish(
        &self,
        topic: &str,
        envelope: &Envelope,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send {
        std::future::ready(self.core.do_publish(topic, envelope))
    }

    async fn poll(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        let deadline = deadline(timeout);
        loop {
            if let Some(d) = self.core.take(topic) {
                return Ok(Some(d));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            Sleep::new(POLL_INTERVAL).await;
        }
    }

    fn ack(&self, delivery: &Delivery) -> impl Future<Output = Result<(), BrokerError>> + Send {
        self.core.settle(delivery, true, false);
        std::future::ready(Ok(()))
    }

    fn nack(
        &self,
        delivery: &Delivery,
        requeue: bool,
    ) -> impl Future<Output = Result<(), BrokerError>> + Send {
        self.core.settle(delivery, false, requeue);
        std::future::ready(Ok(()))
    }
}

impl SyncBrokerAdapter for SyncFakeBrokerAdapter {
    fn publish(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        self.core.do_publish(topic, envelope)
    }

    fn poll(
        &self,
        topic: &str,
        timeout: Option<Duration>,
    ) -> Result<Option<Delivery>, BrokerError> {
        let deadline = deadline(timeout);
        loop {
            if let Some(d) = self.core.take(topic) {
                return Ok(Some(d));
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(None);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn ack(&self, delivery: &Delivery) -> Result<(), BrokerError> {
        self.core.settle(delivery, true, false);
        Ok(())
    }

    fn nack(&self, delivery: &Delivery, requeue: bool) -> Result<(), BrokerError> {
        self.core.settle(delivery, false, requeue);
        Ok(())
    }
}

/// A runtime-agnostic sleep: a helper thread wakes the latest waker at the deadline.
struct Sleep {
    deadline: Instant,
    waker: Option<Arc<Mutex<Waker>>>,
}

impl Sleep {
    fn new(duration: Duration) -> Self {
        // a duration too long to represent is clamped: the sleep ends at once, it never panics
        let now = Instant::now();
        Self { deadline: now.checked_add(duration).unwrap_or(now), waker: None }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if Instant::now() >= self.deadline {
            return Poll::Ready(());
        }
        if let Some(shared) = &self.waker {
            shared.lock().unwrap_or_else(PoisonError::into_inner).clone_from(cx.waker());
        } else {
            let shared = Arc::new(Mutex::new(cx.waker().clone()));
            let for_thread = Arc::clone(&shared);
            let deadline = self.deadline;
            std::thread::spawn(move || {
                std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                for_thread.lock().unwrap_or_else(PoisonError::into_inner).wake_by_ref();
            });
            self.waker = Some(shared);
        }
        Poll::Pending
    }
}

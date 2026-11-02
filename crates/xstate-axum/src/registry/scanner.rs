//! Lifecycle: [`StatechartRegistry::start`] / [`StatechartRegistry::shutdown`] and the
//! durable-timer scanner thread (Python `_ScannerMixin` + `lifespan`).
//!
//! The scanner wakes instances whose persisted `after` deadline matured
//! ([`xstate_persistence::DueTimerScanner`]). It runs on its own OS thread because the stores
//! are synchronous. Timer-driven transitions are saved by the scanner, not by a request, so
//! they are **not** published to SSE / WebSocket subscribers (the scanner exposes no observer
//! hook); a client sees them on its next read or reconnect.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::Value;
use xstate_persistence::DueTimerScanner;

use super::{KEY_SEP, RegistryError, StatechartRegistry, relock};

/// Wall-clock source for the scanner, epoch seconds.
pub type NowFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// A running scanner thread.
#[derive(Debug)]
pub(crate) struct ScannerHandle {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl StatechartRegistry {
    /// Startup: leave draining, start the timer scanner when configured
    /// ([`run_timers`](super::RegistryBuilder::run_timers)) and report ready. Idempotent.
    ///
    /// # Errors
    /// [`RegistryError::InvalidSetting`] when the scanner thread cannot be spawned.
    pub fn start(&self) -> Result<(), RegistryError> {
        self.draining.store(false, Ordering::Release);
        let mut slot = relock(&self.scanner);
        if slot.is_none()
            && let Some(interval) = self.settings.timers
        {
            *slot = Some(self.spawn_scanner(interval)?);
        }
        drop(slot);
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    fn spawn_scanner(&self, interval: Duration) -> Result<ScannerHandle, RegistryError> {
        let regs = Arc::clone(&self.regs);
        let mut scanner = DueTimerScanner::<Value>::new(
            Arc::clone(&self.store),
            Arc::new(move |store_key: &str| {
                let (name, _) = store_key.split_once(KEY_SEP)?;
                regs.get(name).map(|r| (r.machine.clone(), r.logic.clone()))
            }),
        );
        scanner.lock = (self.options)().lock;
        scanner.now.clone_from(&self.scanner_now);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("xsm-timer-scanner".into())
            .spawn(move || scanner.run_until(&flag, interval))
            .map_err(|e| {
                RegistryError::InvalidSetting(format!("cannot start the timer scanner: {e}"))
            })?;
        Ok(ScannerHandle { stop, thread })
    }

    /// `true` while the timer scanner runs.
    #[must_use]
    pub fn timers_running(&self) -> bool {
        relock(&self.scanner).is_some()
    }

    /// Graceful shutdown: refuse new streams and sends (503), end every SSE / WebSocket
    /// stream (sessions still open when the drain timeout runs out are aborted), and stop the
    /// timer scanner, all bounded by the drain timeout (X0.11).
    pub async fn shutdown(&self) {
        self.draining.store(true, Ordering::Release);
        self.started.store(false, Ordering::Release);
        self.subscribers().close_all();
        let budget = self.settings.drain_timeout;
        let deadline = tokio::time::Instant::now() + budget;
        self.end_sessions(deadline).await;
        let Some(handle) = relock(&self.scanner).take() else { return };
        handle.stop.store(true, Ordering::Release);
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let joined = tokio::task::spawn_blocking(move || {
            let deadline = Instant::now() + remaining;
            while !handle.thread.is_finished() {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            handle.thread.join().is_ok()
        })
        .await
        .unwrap_or(false);
        if !joined {
            tracing::warn!(target: "xstate::axum", ?budget, "timer scanner did not stop within the drain timeout");
        }
    }
}

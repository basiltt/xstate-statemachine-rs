//! [`SseSink`]: serve inspector messages to a browser over Server-Sent Events.
//!
//! A [`Sink`] that publishes every message to the Stately Inspector page it serves, using only
//! the standard library (a `TcpListener` thread and one thread per connection). Routes, as in
//! the Python `SseSink`:
//!
//! | route        | content                                                              |
//! |--------------|----------------------------------------------------------------------|
//! | `/`          | the page ([`INDEX_HTML`]); `?token=` here is the first load, see below |
//! | `/app.js`    | the page's script ([`APP_JS`]), so the CSP can forbid inline JavaScript |
//! | `/events`    | `text/event-stream`: the backlog, then one `data: <json>` frame per message |
//! | `/messages`  | the backlog as one JSON array                                        |
//!
//! # Security
//! The sink streams every event and whatever context the observer allow-lists, so treat it as
//! a data-exfiltration endpoint. It binds `127.0.0.1` by default, and a non-loopback host
//! **requires** an explicit token. The token is 32 random bytes (URL-safe base64) compared in
//! constant time. `?token=` is accepted only on the first page load (`GET /?token=…`), which
//! answers `303` with an `HttpOnly; SameSite=Strict` cookie named [`COOKIE_NAME`], so the token
//! does not stay in the address bar, the history or a `Referer`; API calls then use the cookie,
//! `Authorization: Bearer` or `X-XSM-Token`. Loopback binds check the `Host` header with a
//! strict loopback parser (DNS rebinding), and every request carrying an `Origin` must be
//! same-origin or allow-listed. The page is served with a strict `Content-Security-Policy` and
//! no inline script. Requests are parsed by a deliberately strict HTTP/1.1 reader: bounded
//! head, `GET` only, no body, no pipelining.
//!
//! A client that falls [`MAX_QUEUE`] frames behind is disconnected (it reconnects and replays
//! the backlog); the producing machine never blocks on a browser.
//!
//! ```
//! use xstate_inspect::{Sink, SseOptions, SseSink};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let sink = SseSink::bind(SseOptions::default())?; // 127.0.0.1, an ephemeral port
//! sink.send(&serde_json::json!({"type": "@xstate.event"}))?;
//! assert!(sink.url().starts_with("http://127.0.0.1:"));
//! sink.close();
//! # Ok(()) }
//! ```

mod auth;
mod http;
mod page;
mod server;

use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

pub use auth::{COOKIE_NAME, LOOPBACK_HOSTS};
pub use page::{APP_JS, INDEX_HTML, STATELY_URL};

use crate::sinks::{Sink, SinkError, encode_line};

/// Frames a client may have queued before it is disconnected.
pub const MAX_QUEUE: usize = 10_000;
/// Idle time between `: keep-alive` comments on an event stream.
pub const KEEPALIVE: Duration = Duration::from_secs(15);
/// Default number of past messages replayed to a newly connected client.
pub const DEFAULT_BACKLOG: usize = 1000;

/// Options of [`SseSink::bind`].
#[derive(Clone)]
pub struct SseOptions {
    /// Bind address; `127.0.0.1` by default.
    pub host: String,
    /// Port; `0` picks a free one (read it back from [`SseSink::local_addr`]).
    pub port: u16,
    /// Access token. Generated when `None` (or empty), but a non-loopback `host` requires one
    /// to be given. Explicit tokens may use only `A-Za-z0-9-._~`.
    pub token: Option<String>,
    /// Extra `Origin` values accepted besides same-origin (e.g. a dev UI on another port).
    pub allowed_origins: Vec<String>,
    /// How many past messages a newly connected client replays (and `/messages` returns).
    pub backlog: usize,
}

impl Default for SseOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: 0,
            token: None,
            allowed_origins: Vec::new(),
            backlog: DEFAULT_BACKLOG,
        }
    }
}

impl std::fmt::Debug for SseOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("allowed_origins", &self.allowed_origins)
            .field("backlog", &self.backlog)
            .finish()
    }
}

/// Why an [`SseSink`] could not be built.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SseError {
    /// A non-loopback host without an explicit token.
    #[error("refusing to serve the inspector on non-loopback host {0:?} without an explicit token")]
    TokenRequired(String),
    /// The explicit token cannot be carried verbatim in a URL, a cookie and a header.
    #[error("the token must be non-empty and use only A-Z a-z 0-9 - . _ ~")]
    InvalidToken,
    /// The listening socket could not be opened.
    #[error("cannot bind {host}:{port}: {source}")]
    Bind {
        /// Requested host.
        host: String,
        /// Requested port.
        port: u16,
        /// The operating-system error.
        source: std::io::Error,
    },
    /// The operating system could not provide randomness for the token.
    #[error("cannot generate a token: {0}")]
    Entropy(String),
}

/// Serves inspector messages as Server-Sent Events; a [`Sink`] for
/// [`crate::InspectorObserver`] and [`crate::replay_file`].
///
/// Dropping the sink stops the server.
pub struct SseSink {
    shared: Arc<Shared>,
    host: String,
    addr: SocketAddr,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for SseSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseSink")
            .field("addr", &self.addr)
            .field("clients", &self.client_count())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

/// Knobs that tests turn; production uses [`Tuning::default`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tuning {
    pub(crate) keepalive: Duration,
    pub(crate) max_queue: usize,
}

impl Default for Tuning {
    fn default() -> Self {
        Self { keepalive: KEEPALIVE, max_queue: MAX_QUEUE }
    }
}

impl SseSink {
    /// Bind the listener, start the server thread and return the running sink.
    ///
    /// # Errors
    /// [`SseError::TokenRequired`] for a non-loopback host without a token,
    /// [`SseError::InvalidToken`] for an unusable explicit token, [`SseError::Bind`] when the
    /// socket cannot be opened.
    pub fn bind(options: SseOptions) -> Result<Self, SseError> {
        Self::bind_tuned(options, Tuning::default())
    }

    pub(crate) fn bind_tuned(options: SseOptions, tuning: Tuning) -> Result<Self, SseError> {
        let loopback = auth::is_loopback_host(&options.host);
        let explicit = options.token.filter(|t| !t.is_empty());
        if !loopback && explicit.is_none() {
            return Err(SseError::TokenRequired(options.host));
        }
        let token = match explicit {
            Some(t) if auth::token_is_well_formed(&t) => t,
            Some(_) => return Err(SseError::InvalidToken),
            None => auth::generate_token().map_err(SseError::Entropy)?,
        };
        let bind_error =
            |source| SseError::Bind { host: options.host.clone(), port: options.port, source };
        let listener =
            TcpListener::bind((options.host.as_str(), options.port)).map_err(bind_error)?;
        let addr = listener.local_addr().map_err(bind_error)?;
        let shared = Arc::new(Shared::new(
            token,
            loopback,
            options.allowed_origins.into_iter().collect(),
            options.backlog,
            tuning,
        ));
        let worker = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("xsm-inspector-sse".to_owned())
            .spawn(move || server::accept_loop(&listener, &worker))
            .map_err(bind_error)?;
        Ok(Self { shared, host: options.host, addr, thread: Mutex::new(Some(handle)) })
    }

    /// The first-load URL, `http://<host>:<port>/?token=<token>`. It carries the token once;
    /// the page swaps it for a cookie.
    #[must_use]
    pub fn url(&self) -> String {
        let host =
            if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        format!("http://{host}:{}/?token={}", self.addr.port(), self.shared.token)
    }

    /// The address the server listens on (the real port when `0` was asked for).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The access token.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.shared.token
    }

    /// Connected `/events` clients.
    #[must_use]
    pub fn client_count(&self) -> usize {
        self.shared.lock().clients.len()
    }

    /// Clients disconnected for falling [`MAX_QUEUE`] frames behind.
    #[must_use]
    pub fn dropped_clients(&self) -> u64 {
        self.shared.lock().dropped
    }

    /// `true` after [`SseSink::close`].
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    /// Disconnect every client, stop the server thread and free the port. Idempotent; later
    /// sends fail with [`SinkError::Closed`].
    pub fn close(&self) {
        if !self.shared.shut_down() {
            return;
        }
        let handle = self.thread.lock().unwrap_or_else(PoisonError::into_inner).take();
        if server::wake(self.addr)
            && let Some(handle) = handle
        {
            // the accept loop saw the wake-up connection and the closed flag, and is leaving
            let _ = handle.join();
        }
    }
}

impl Drop for SseSink {
    fn drop(&mut self) {
        self.close();
    }
}

impl Sink for SseSink {
    fn send(&self, message: &Value) -> Result<(), SinkError> {
        self.shared.publish(&encode_line(message))
    }
}

/// One connected `/events` client.
struct Client {
    id: u64,
    tx: SyncSender<Arc<str>>,
}

#[derive(Default)]
struct State {
    /// Encoded JSON lines, oldest first, at most `backlog` of them.
    history: VecDeque<Arc<str>>,
    clients: Vec<Client>,
    seq: u64,
    next_client: u64,
    dropped: u64,
    closed: bool,
}

/// State shared by the sink handle, the accept loop and the connection threads.
pub(crate) struct Shared {
    token: String,
    bind_is_loopback: bool,
    allowed_origins: HashSet<String>,
    backlog: usize,
    tuning: Tuning,
    /// Connections being served right now (bounded by `server::MAX_CONNECTIONS`).
    connections: AtomicUsize,
    state: Mutex<State>,
}

impl Shared {
    fn new(
        token: String,
        bind_is_loopback: bool,
        allowed_origins: HashSet<String>,
        backlog: usize,
        tuning: Tuning,
    ) -> Self {
        Self {
            token,
            bind_is_loopback,
            allowed_origins,
            backlog,
            tuning,
            connections: AtomicUsize::new(0),
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Constant-time token check; `None` and the empty string never match.
    pub(crate) fn token_ok(&self, candidate: Option<&str>) -> bool {
        candidate.is_some_and(|c| {
            !c.is_empty() && auth::constant_time_eq(c.as_bytes(), self.token.as_bytes())
        })
    }

    /// Record `line` and queue it as an SSE frame for every client; a client whose queue is
    /// full is cut, never waited on.
    pub(crate) fn publish(&self, line: &str) -> Result<(), SinkError> {
        let mut state = self.lock();
        if state.closed {
            return Err(SinkError::Closed);
        }
        state.seq += 1;
        if self.backlog > 0 {
            state.history.push_back(Arc::from(line));
            while state.history.len() > self.backlog {
                state.history.pop_front();
            }
        }
        let frame: Arc<str> = Arc::from(format!("id: {}\ndata: {line}\n\n", state.seq));
        let mut cut = 0;
        state.clients.retain(|client| match client.tx.try_send(Arc::clone(&frame)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                cut += 1;
                false
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
        state.dropped += cut;
        Ok(())
    }

    /// The backlog as one JSON array.
    pub(crate) fn messages_json(&self) -> String {
        let state = self.lock();
        let lines: Vec<&str> = state.history.iter().map(AsRef::as_ref).collect();
        format!("[{}]", lines.join(","))
    }

    /// Register a client and return it with the backlog it must replay first; `None` once
    /// closed. Registering and snapshotting under one lock leaves no gap and no duplicate.
    pub(crate) fn subscribe(self: &Arc<Self>) -> Option<(Subscription, Vec<Arc<str>>)> {
        let mut state = self.lock();
        if state.closed {
            return None;
        }
        let (tx, rx) = std::sync::mpsc::sync_channel(self.tuning.max_queue.max(1));
        let id = state.next_client;
        state.next_client += 1;
        state.clients.push(Client { id, tx });
        let backlog = state.history.iter().cloned().collect();
        drop(state);
        Some((Subscription { shared: Arc::clone(self), id, rx }, backlog))
    }

    /// Mark closed and disconnect every client. `false` when it already was closed.
    fn shut_down(&self) -> bool {
        let mut state = self.lock();
        if state.closed {
            return false;
        }
        state.closed = true;
        state.clients.clear();
        true
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

/// A registered `/events` client; unregisters itself when dropped.
pub(crate) struct Subscription {
    shared: Arc<Shared>,
    id: u64,
    pub(crate) rx: Receiver<Arc<str>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.shared.lock().clients.retain(|c| c.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn new_shared(backlog: usize, max_queue: usize) -> Arc<Shared> {
        let tuning = Tuning { keepalive: KEEPALIVE, max_queue };
        Arc::new(Shared::new("tok".into(), true, HashSet::new(), backlog, tuning))
    }

    #[test]
    fn backlog_is_bounded_and_oldest_first() {
        let shared = new_shared(2, 8);
        for i in 0..5 {
            shared.publish(&encode_line(&json!({"i": i}))).unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(shared.messages_json(), r#"[{"i":3},{"i":4}]"#);
        assert_eq!(new_shared(0, 8).messages_json(), "[]");
    }

    #[test]
    fn subscriber_gets_backlog_then_live_frames_with_ids() {
        let shared = new_shared(10, 8);
        shared.publish("{\"a\":1}").unwrap_or_else(|e| panic!("{e}"));
        let (sub, backlog) = shared.subscribe().unwrap_or_else(|| panic!("closed"));
        assert_eq!(backlog.iter().map(AsRef::as_ref).collect::<Vec<&str>>(), ["{\"a\":1}"]);
        shared.publish("{\"b\":2}").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(sub.rx.try_recv().as_deref(), Ok("id: 2\ndata: {\"b\":2}\n\n"));
    }

    #[test]
    fn a_stalled_client_is_cut_without_blocking_the_producer() {
        let shared = new_shared(10, 2);
        let (sub, _) = shared.subscribe().unwrap_or_else(|| panic!("closed"));
        for _ in 0..3 {
            shared.publish("{}").unwrap_or_else(|e| panic!("{e}"));
        }
        assert_eq!(shared.lock().dropped, 1);
        assert!(shared.lock().clients.is_empty());
        assert!(sub.rx.try_recv().is_ok() && sub.rx.try_recv().is_ok());
        assert!(sub.rx.try_recv().is_err());
    }

    #[test]
    fn dropping_a_subscription_unregisters_it_and_close_refuses_new_ones() {
        let shared = new_shared(1, 2);
        let (sub, _) = shared.subscribe().unwrap_or_else(|| panic!("closed"));
        assert_eq!(shared.lock().clients.len(), 1);
        drop(sub);
        assert!(shared.lock().clients.is_empty());
        assert!(shared.shut_down());
        assert!(!shared.shut_down());
        assert!(shared.subscribe().is_none());
        assert!(matches!(shared.publish("{}"), Err(SinkError::Closed)));
    }

    #[test]
    fn idle_streams_get_keepalive_comments_and_dead_clients_are_reaped() {
        use std::io::{Read, Write};
        use std::time::Instant;
        let tuning = Tuning { keepalive: Duration::from_millis(40), max_queue: 4 };
        let sink =
            SseSink::bind_tuned(SseOptions::default(), tuning).unwrap_or_else(|e| panic!("{e}"));
        let mut tcp =
            std::net::TcpStream::connect(sink.local_addr()).unwrap_or_else(|e| panic!("{e}"));
        tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap_or_else(|e| panic!("{e}"));
        let request = format!(
            "GET /events HTTP/1.1\r\nHost: {}\r\nX-XSM-Token: {}\r\n\r\n",
            sink.local_addr(),
            sink.token()
        );
        tcp.write_all(request.as_bytes()).unwrap_or_else(|e| panic!("{e}"));
        let mut seen = String::new();
        let mut chunk = [0_u8; 256];
        while !seen.contains(": keep-alive\n\n") {
            let n = tcp.read(&mut chunk).unwrap_or_else(|e| panic!("{e}; saw {seen:?}"));
            assert!(n > 0, "stream ended; saw {seen:?}");
            seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
        assert_eq!(sink.client_count(), 1);
        drop(tcp);
        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.client_count() > 0 {
            assert!(Instant::now() < deadline, "the dead client was never reaped");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn construction_rules() {
        let non_loopback = |token: Option<&str>| SseOptions {
            host: "0.0.0.0".into(),
            token: token.map(str::to_owned),
            ..SseOptions::default()
        };
        assert!(matches!(SseSink::bind(non_loopback(None)), Err(SseError::TokenRequired(_))));
        assert!(matches!(SseSink::bind(non_loopback(Some(""))), Err(SseError::TokenRequired(_))));
        let bad = SseOptions { token: Some("a b".into()), ..SseOptions::default() };
        assert!(matches!(SseSink::bind(bad), Err(SseError::InvalidToken)));
        let rendered =
            format!("{:?}", SseOptions { token: Some("sekrit".into()), ..SseOptions::default() });
        assert!(!rendered.contains("sekrit"));
    }
}

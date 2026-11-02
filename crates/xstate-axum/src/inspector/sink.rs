//! [`WebSocketSink`]: fans Stately Inspector protocol messages out to connected WebSocket
//! clients (Python `inspector.WebSocketSink`).

use std::collections::VecDeque;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::extract::ws::{Message, WebSocket};
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use xstate_inspect::{Sink, SinkError};

use crate::routes::ws::{WS_TRY_AGAIN_LATER, close, send_json};

/// Cookie carrying the inspector token after the one-time `?token=` redirect.
pub const COOKIE_NAME: &str = "xsm_inspect";

struct Client {
    id: u64,
    tx: mpsc::Sender<Value>,
    cut: Arc<AtomicBool>,
}

struct Inner {
    history: VecDeque<Value>,
    clients: Vec<Client>,
    next_id: u64,
}

/// Records inspector messages (the newest `history`) and forwards each one to every connected
/// client. [`Sink::send`] is thread-safe and never blocks: a client that falls `max_queue`
/// messages behind is closed with 1013 (it reconnects and replays the history) and counted
/// in [`dropped`](Self::dropped) (battle #274).
pub struct WebSocketSink {
    token: String,
    history_cap: usize,
    max_queue: usize,
    dropped: AtomicU64,
    inner: Mutex<Inner>,
}

impl fmt::Debug for WebSocketSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketSink")
            .field("history_cap", &self.history_cap)
            .field("max_queue", &self.max_queue)
            .field("dropped", &self.dropped())
            .finish_non_exhaustive()
    }
}

/// 32 random bytes, URL-safe base64 without padding (Python `secrets.token_urlsafe(32)`).
#[must_use]
pub fn new_token() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let bytes: [u8; 32] = rand::random();
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            let index = (n >> (18 - 6 * i)) & 0x3f;
            out.push(char::from(ALPHABET[index as usize]));
        }
    }
    out
}

/// Constant-time equality (the length is not secret).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl WebSocketSink {
    /// A sink with `token` (a fresh random one when `None`), keeping the newest `history`
    /// messages and cutting clients `max_queue` messages behind (`max_queue >= 1`).
    #[must_use]
    pub fn new(token: Option<String>, history: usize, max_queue: usize) -> Self {
        Self {
            token: token.filter(|t| !t.is_empty()).unwrap_or_else(new_token),
            history_cap: history,
            max_queue: max_queue.max(1),
            dropped: AtomicU64::new(0),
            inner: Mutex::new(Inner { history: VecDeque::new(), clients: Vec::new(), next_id: 0 }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token to hand to the developer.
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Clients closed because they fell `max_queue` behind.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The recorded history, oldest first.
    #[must_use]
    pub fn messages(&self) -> Vec<Value> {
        self.lock().history.iter().cloned().collect()
    }

    /// Connected clients.
    #[must_use]
    pub fn clients(&self) -> usize {
        self.lock().clients.len()
    }

    /// Whether `candidate` equals the token (constant time; empty is never valid).
    #[must_use]
    pub fn token_ok(&self, candidate: Option<&str>) -> bool {
        candidate.is_some_and(|c| !c.is_empty() && ct_eq(c.as_bytes(), self.token.as_bytes()))
    }

    /// The token a request presented: `Authorization: Bearer`, then `X-XSM-Token`, then the
    /// inspector cookie.
    #[must_use]
    pub fn presented_token(headers: &HeaderMap) -> Option<String> {
        let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        if let Some(auth) = text(header::AUTHORIZATION.as_str())
            && auth.len() > 7
            && auth[..7].eq_ignore_ascii_case("bearer ")
        {
            return Some(auth[7..].trim().to_owned());
        }
        if let Some(token) = text("x-xsm-token").filter(|t| !t.is_empty()) {
            return Some(token);
        }
        headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|cookies| {
            cookies.split(';').find_map(|pair| {
                let (name, value) = pair.trim().split_once('=')?;
                (name == COOKIE_NAME).then(|| value.to_owned())
            })
        })
    }

    fn register(&self) -> (u64, Vec<Value>, mpsc::Receiver<Value>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel(self.max_queue);
        let cut = Arc::new(AtomicBool::new(false));
        let mut inner = self.lock();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.clients.push(Client { id, tx, cut: Arc::clone(&cut) });
        (id, inner.history.iter().cloned().collect(), rx, cut)
    }

    fn unregister(&self, id: u64) {
        self.lock().clients.retain(|c| c.id != id);
    }

    /// Pump messages to one accepted socket until it disconnects: the history first, then
    /// every new message.
    pub async fn serve(&self, socket: WebSocket) {
        let (id, backlog, mut rx, cut) = self.register();
        let (mut tx, mut inbound) = socket.split();
        for message in &backlog {
            if !send_json(&mut tx, message).await {
                return self.unregister(id);
            }
        }
        loop {
            tokio::select! {
                message = rx.recv() => {
                    if cut.load(Ordering::Acquire) {
                        close(&mut tx, WS_TRY_AGAIN_LATER).await;
                        break;
                    }
                    let Some(message) = message else { break };
                    if !send_json(&mut tx, &message).await {
                        break;
                    }
                }
                frame = inbound.next() => {
                    if matches!(frame, None | Some(Err(_) | Ok(Message::Close(_)))) {
                        break;
                    }
                }
            }
        }
        self.unregister(id);
    }
}

impl Sink for WebSocketSink {
    fn send(&self, message: &Value) -> Result<(), SinkError> {
        let mut inner = self.lock();
        if self.history_cap > 0 {
            if inner.history.len() >= self.history_cap {
                inner.history.pop_front();
            }
            inner.history.push_back(message.clone());
        }
        let before = inner.clients.len();
        inner.clients.retain(|client| match client.tx.try_send(message.clone()) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                client.cut.store(true, Ordering::Release);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        let cut = before - inner.clients.len();
        if cut > 0 {
            self.dropped.fetch_add(cut as u64, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_rules() {
        let sink = WebSocketSink::new(None, 10, 10);
        assert_eq!(sink.token().len(), 43);
        assert!(sink.token_ok(Some(sink.token())));
        assert!(!sink.token_ok(Some("")));
        assert!(!sink.token_ok(None));
        let mut headers = HeaderMap::new();
        let cookie = format!("a=b; {COOKIE_NAME}=xyz");
        headers.insert(header::COOKIE, cookie.parse().unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(WebSocketSink::presented_token(&headers).as_deref(), Some("xyz"));
        headers
            .insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap_or_else(|e| panic!("{e}")));
        assert_eq!(WebSocketSink::presented_token(&headers).as_deref(), Some("abc"));
    }

    #[test]
    fn history_is_bounded() {
        let sink = WebSocketSink::new(Some("t".into()), 2, 10);
        for n in 0..3 {
            let _ = sink.send(&Value::from(n));
        }
        assert_eq!(sink.messages(), [Value::from(1), Value::from(2)]);
    }
}

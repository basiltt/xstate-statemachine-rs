//! The accept loop, request routing and the `/events` pump.

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::auth;
use super::http::{self, Rejection, Request, Response};
use super::page::{APP_JS, INDEX_HTML};
use super::{COOKIE_NAME, Shared, Subscription};

/// The strict Content-Security-Policy of the page and its script.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                   connect-src 'self'; img-src 'self' data:; frame-src https://stately.ai; \
                   base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Simultaneous connections served; further ones are closed at once.
const MAX_CONNECTIONS: usize = 128;
/// Time a client may stall a write before its connection is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Pause after a transient `accept` failure (out of descriptors, aborted handshake).
const ACCEPT_RETRY: Duration = Duration::from_millis(10);
/// Time allowed for the wake-up connection that unblocks `accept` on close.
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);

/// Counts one live connection of a sink; released on drop.
struct ConnectionSlot(Arc<Shared>);

impl ConnectionSlot {
    fn acquire(shared: &Arc<Shared>) -> Option<Self> {
        if shared.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            shared.connections.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(shared)))
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Accept connections until the sink is closed, one thread per connection.
pub(super) fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for incoming in listener.incoming() {
        if shared.is_closed() {
            return;
        }
        let Ok(stream) = incoming else {
            std::thread::sleep(ACCEPT_RETRY);
            continue;
        };
        let Some(slot) = ConnectionSlot::acquire(shared) else { continue };
        let worker = Arc::clone(shared);
        // a failed spawn drops the stream (and the slot): the client sees a closed connection
        let _ =
            std::thread::Builder::new().name("xsm-inspector-conn".to_owned()).spawn(move || {
                let _slot = slot;
                serve_connection(stream, &worker);
            });
    }
}

/// Unblock a pending `accept` by connecting to the listener once. `false` when that failed.
pub(super) fn wake(addr: SocketAddr) -> bool {
    let ip = match addr.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        other => other,
    };
    TcpStream::connect_timeout(&SocketAddr::new(ip, addr.port()), WAKE_TIMEOUT).is_ok()
}

enum Route {
    Reply(Response),
    Events,
}

fn serve_connection(mut stream: TcpStream, shared: &Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    if stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err() {
        return;
    }
    let request = match http::read_request(&mut stream) {
        Ok(request) => request,
        Err(Rejection::Silent) => return,
        Err(Rejection::Status(code, reason)) => {
            let _ = Response::text(code, reason, reason).write_to(&mut stream);
            return;
        }
    };
    match route(shared, &request) {
        Route::Reply(response) => {
            let _ = response.write_to(&mut stream);
        }
        Route::Events => stream_events(&mut stream, shared),
    }
}

fn deny(code: u16, reason: &'static str, message: &str) -> Route {
    Route::Reply(Response::text(code, reason, message))
}

/// Decide what to do with `request`: method, `Host`, `Origin`, then the token.
fn route(shared: &Shared, request: &Request) -> Route {
    if request.method != "GET" {
        let refusal = Response::text(405, "Method Not Allowed", "method not allowed");
        return Route::Reply(refusal.header("Allow", "GET"));
    }
    let host = request.header("host");
    if !auth::host_ok(shared.bind_is_loopback, host) {
        return deny(421, "Misdirected Request", "misdirected request (Host)");
    }
    if !auth::origin_ok(request.header("origin"), host, &shared.allowed_origins) {
        return deny(403, "Forbidden", "cross-origin request refused");
    }
    if request.path == "/"
        && let Some(candidate) = auth::first_load_token(request.query.as_deref())
    {
        return Route::Reply(first_load(shared, &candidate));
    }
    if !shared.token_ok(auth::presented_token(request)) {
        return deny(401, "Unauthorized", "inspector token required");
    }
    match request.path.as_str() {
        "/" => Route::Reply(asset(INDEX_HTML.as_bytes(), "text/html")),
        "/app.js" => Route::Reply(asset(APP_JS.as_bytes(), "text/javascript")),
        "/messages" => {
            let body = shared.messages_json().into_bytes();
            Route::Reply(asset_owned(body, "application/json"))
        }
        "/events" => Route::Events,
        _ => deny(404, "Not Found", "not found"),
    }
}

/// `GET /?token=…`: swap the query token for an `HttpOnly` cookie and redirect to `/`.
fn first_load(shared: &Shared, candidate: &str) -> Response {
    if !shared.token_ok(Some(candidate)) {
        return Response::text(401, "Unauthorized", "inspector token required");
    }
    Response::new(303, "See Other")
        .header("Location", "/")
        .header(
            "Set-Cookie",
            format!("{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/", shared.token),
        )
        .header("Cache-Control", "no-store")
        .header("Referrer-Policy", "no-referrer")
}

fn asset(body: &'static [u8], content_type: &str) -> Response {
    with_security_headers(Response::new(200, "OK"), content_type)
        .body(std::borrow::Cow::Borrowed(body))
}

fn asset_owned(body: Vec<u8>, content_type: &str) -> Response {
    with_security_headers(Response::new(200, "OK"), content_type)
        .body(std::borrow::Cow::Owned(body))
}

fn with_security_headers(response: Response, content_type: &str) -> Response {
    response
        .header("Content-Type", format!("{content_type}; charset=utf-8"))
        .header("Content-Security-Policy", CSP)
        .header("X-Content-Type-Options", "nosniff")
        .header("Referrer-Policy", "no-referrer")
        .header("Cache-Control", "no-store")
}

const EVENT_STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\nServer: xsm-inspector\r\n\
     Content-Type: text/event-stream\r\nCache-Control: no-store\r\n\
     X-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n";

/// `/events`: the backlog, then live frames and keep-alive comments until the client goes
/// away, falls too far behind, or the sink closes.
fn stream_events(stream: &mut TcpStream, shared: &Arc<Shared>) {
    let Some((subscription, backlog)) = shared.subscribe() else { return };
    let mut opening = String::from(EVENT_STREAM_HEAD);
    for line in &backlog {
        opening.push_str("data: ");
        opening.push_str(line);
        opening.push_str("\n\n");
    }
    if stream.write_all(opening.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let _ = pump(stream, &subscription, shared.tuning.keepalive);
}

fn pump(
    stream: &mut TcpStream,
    subscription: &Subscription,
    keepalive: Duration,
) -> io::Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        match subscription.rx.recv_timeout(keepalive) {
            Ok(frame) => stream.write_all(frame.as_bytes())?,
            Err(RecvTimeoutError::Timeout) => stream.write_all(b": keep-alive\n\n")?,
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        stream.flush()?;
    }
}

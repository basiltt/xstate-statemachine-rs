//! End-to-end tests of `SseSink` over real sockets, with a hand-rolled HTTP/1.1 client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use xstate_inspect::sse::{APP_JS, COOKIE_NAME, INDEX_HTML};
use xstate_inspect::{Sink, SinkError, SseError, SseOptions, SseSink};

const TIMEOUT: Duration = Duration::from_secs(5);

/// A parsed HTTP response.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn sink() -> SseSink {
    SseSink::bind(SseOptions::default()).unwrap()
}

fn msg(n: u32) -> Value {
    json!({"type": "@xstate.event", "n": n})
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream
}

/// The request text for `GET path`, with `Host` of the sink unless `extra` sets one.
fn request_text(sink: &SseSink, path: &str, extra: &[(&str, &str)]) -> String {
    let mut text = format!("GET {path} HTTP/1.1\r\n");
    if !extra.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        let _ = write!(text, "Host: {}\r\n", sink.local_addr());
    }
    for (name, value) in extra {
        let _ = write!(text, "{name}: {value}\r\n");
    }
    text.push_str("\r\n");
    text
}

/// Send raw bytes and read the whole reply (the server closes after answering).
fn raw(sink: &SseSink, bytes: &[u8]) -> Reply {
    let mut stream = connect(sink.local_addr());
    stream.write_all(bytes).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    parse_reply(&String::from_utf8_lossy(&buf))
}

fn parse_reply(text: &str) -> Reply {
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text, ""));
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    Reply { status, headers, body: body.to_owned() }
}

fn get(sink: &SseSink, path: &str, extra: &[(&str, &str)]) -> Reply {
    raw(sink, request_text(sink, path, extra).as_bytes())
}

fn cookie(sink: &SseSink) -> String {
    format!("{COOKIE_NAME}={}", sink.token())
}

/// An open `/events` response: the socket plus everything read from it so far.
struct Events {
    tcp: TcpStream,
    seen: String,
}

impl Events {
    /// Read until `needle` appears in what has been received; returns everything seen.
    fn read_until(&mut self, needle: &str) -> &str {
        let deadline = Instant::now() + TIMEOUT;
        let mut chunk = [0_u8; 512];
        while !self.seen.contains(needle) {
            assert!(Instant::now() < deadline, "timed out for {needle:?}; saw {:?}", self.seen);
            match self.tcp.read(&mut chunk) {
                Ok(0) => panic!("stream ended waiting for {needle:?}; saw {:?}", self.seen),
                Ok(n) => self.seen.push_str(&String::from_utf8_lossy(&chunk[..n])),
                Err(e) => panic!("read failed waiting for {needle:?}: {e}; saw {:?}", self.seen),
            }
        }
        &self.seen
    }

    /// Everything after the response head.
    fn frames(&self) -> &str {
        self.seen.split_once("\r\n\r\n").map_or("", |(_, rest)| rest)
    }
}

/// Open `/events` with the cookie and read the response head.
fn open_events(sink: &SseSink) -> Events {
    let cookie = cookie(sink);
    let mut tcp = connect(sink.local_addr());
    tcp.write_all(request_text(sink, "/events", &[("Cookie", &cookie)]).as_bytes()).unwrap();
    let mut events = Events { tcp, seen: String::new() };
    events.read_until("\r\n\r\n");
    events
}

fn json_body(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap()
}

// ----------------------------------------------------------------------------- first load

#[test]
fn first_page_load_swaps_the_token_for_a_cookie_with_a_303() {
    let sink = sink();
    let reply = get(&sink, &format!("/?token={}", sink.token()), &[]);
    assert_eq!(reply.status, 303);
    assert_eq!(reply.header("Location"), Some("/"));
    let set_cookie = reply.header("Set-Cookie").unwrap();
    assert_eq!(
        set_cookie,
        format!("{COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/", sink.token())
    );
    assert_eq!(reply.header("Referrer-Policy"), Some("no-referrer"));
    assert_eq!(reply.header("Cache-Control"), Some("no-store"));
    assert_eq!(reply.body, "");
}

#[test]
fn the_url_carries_the_token_once_and_loads_the_page_through_the_redirect() {
    let sink = sink();
    let url = sink.url();
    assert_eq!(
        url,
        format!("http://127.0.0.1:{}/?token={}", sink.local_addr().port(), sink.token())
    );
    assert_eq!(sink.token().len(), 43);
    let path = &url[url.find("/?").unwrap()..];
    assert_eq!(get(&sink, path, &[]).status, 303);
    let page = get(&sink, "/", &[("Cookie", &cookie(&sink))]);
    assert_eq!(page.status, 200);
    assert_eq!(page.body, INDEX_HTML);
}

#[test]
fn first_load_with_the_wrong_or_empty_token_is_refused() {
    let sink = sink();
    assert_eq!(get(&sink, "/?token=nope", &[]).status, 401);
    // an empty `token=` is not a first load: it falls through to the credential check
    assert_eq!(get(&sink, "/?token=", &[]).status, 401);
}

#[test]
fn the_query_token_is_honoured_only_on_the_first_page_load() {
    let sink = sink();
    let query = format!("?token={}", sink.token());
    for path in ["/messages", "/events", "/app.js"] {
        assert_eq!(get(&sink, &format!("{path}{query}"), &[]).status, 401, "{path}");
    }
}

// ----------------------------------------------------------------------------------- auth

#[test]
fn api_is_refused_without_credentials() {
    let sink = sink();
    for path in ["/", "/app.js", "/messages", "/events", "/nope"] {
        let reply = get(&sink, path, &[]);
        assert_eq!(reply.status, 401, "{path}");
        assert_eq!(reply.body, "inspector token required");
        assert_eq!(reply.header("Cache-Control"), Some("no-store"));
    }
}

#[test]
fn api_accepts_the_cookie_a_bearer_token_or_the_custom_header() {
    let sink = sink();
    sink.send(&msg(1)).unwrap();
    let token = sink.token().to_owned();
    let cookie = cookie(&sink);
    let bearer = format!("Bearer {token}");
    let variants: [(&str, &str); 4] = [
        ("Cookie", &cookie),
        ("Authorization", &bearer),
        ("authorization", &bearer.replace("Bearer", "bearer")),
        ("X-XSM-Token", &token),
    ];
    for (name, value) in variants {
        let reply = get(&sink, "/messages", &[(name, value)]);
        assert_eq!(reply.status, 200, "{name}");
        assert_eq!(json_body(&reply), json!([msg(1)]));
    }
}

#[test]
fn wrong_credentials_are_refused() {
    let sink = sink();
    for (name, value) in [
        ("Authorization", "Bearer wrong"),
        ("Authorization", "Bearer "),
        ("Authorization", "Basic abc"),
        ("X-XSM-Token", "wrong"),
        ("Cookie", "xsm_inspect=wrong"),
        ("Cookie", "other=1"),
    ] {
        assert_eq!(get(&sink, "/messages", &[(name, value)]).status, 401, "{name}: {value}");
    }
}

#[test]
fn page_and_script_carry_the_strict_security_headers() {
    let sink = sink();
    let auth = [("X-XSM-Token", sink.token())];
    let page = get(&sink, "/", &auth);
    let csp = page.header("Content-Security-Policy").unwrap();
    assert!(csp.starts_with("default-src 'none'; script-src 'self';"));
    assert!(csp.contains("frame-src https://stately.ai"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(page.header("X-Content-Type-Options"), Some("nosniff"));
    assert_eq!(page.header("Content-Type"), Some("text/html; charset=utf-8"));
    assert!(page.body.contains(r#"<script src="/app.js"></script>"#));
    assert_eq!(page.body.matches("<script").count(), 1, "no inline script");
    let script = get(&sink, "/app.js", &auth);
    assert_eq!(script.header("Content-Type"), Some("text/javascript; charset=utf-8"));
    assert_eq!(script.body, APP_JS);
}

// ------------------------------------------------------------------------- host and origin

#[test]
fn a_foreign_host_header_is_refused_on_a_loopback_bind() {
    let sink = sink();
    let token = sink.token().to_owned();
    for host in ["evil.com", "127.0.0.1.evil.com", "127.0.0.1:port", "::1"] {
        let reply = get(&sink, "/messages", &[("Host", host), ("X-XSM-Token", &token)]);
        assert_eq!(reply.status, 421, "{host}");
    }
    let no_host = format!("GET /messages HTTP/1.0\r\nX-XSM-Token: {token}\r\n\r\n");
    assert_eq!(raw(&sink, no_host.as_bytes()).status, 421);
    let ok = get(&sink, "/messages", &[("Host", "localhost:1"), ("X-XSM-Token", &token)]);
    assert_eq!(ok.status, 200);
}

#[test]
fn a_foreign_origin_is_refused_even_with_valid_credentials() {
    let sink = sink();
    let token = sink.token().to_owned();
    for origin in ["http://evil.com", "null", "http://127.0.0.1"] {
        let reply = get(&sink, "/messages", &[("Origin", origin), ("X-XSM-Token", &token)]);
        assert_eq!(reply.status, 403, "{origin}");
    }
    let same = format!("http://{}", sink.local_addr());
    assert_eq!(get(&sink, "/messages", &[("Origin", &same), ("X-XSM-Token", &token)]).status, 200);
    // the first load is subject to the same check
    let first = format!("/?token={token}");
    assert_eq!(get(&sink, &first, &[("Origin", "http://evil.com")]).status, 403);
}

#[test]
fn allow_listed_origins_pass() {
    let options = SseOptions {
        allowed_origins: vec!["http://dev.local:3000".to_owned()],
        ..SseOptions::default()
    };
    let sink = SseSink::bind(options).unwrap();
    let reply = get(
        &sink,
        "/messages",
        &[("Origin", "http://dev.local:3000"), ("X-XSM-Token", sink.token())],
    );
    assert_eq!(reply.status, 200);
}

// ----------------------------------------------------------------------------- the stream

#[test]
fn messages_returns_the_backlog_bounded_by_its_size() {
    let sink = SseSink::bind(SseOptions { backlog: 3, ..SseOptions::default() }).unwrap();
    let auth = [("X-XSM-Token", sink.token())];
    assert_eq!(json_body(&get(&sink, "/messages", &auth)), json!([]));
    for n in 0..5 {
        sink.send(&msg(n)).unwrap();
    }
    let reply = get(&sink, "/messages", &auth);
    assert_eq!(reply.header("Content-Type"), Some("application/json; charset=utf-8"));
    assert_eq!(json_body(&reply), json!([msg(2), msg(3), msg(4)]));
}

#[test]
fn events_streams_a_message_sent_after_the_client_connected() {
    let sink = sink();
    let mut events = open_events(&sink);
    assert!(events.seen.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(events.seen.contains("Content-Type: text/event-stream\r\n"));
    assert!(events.seen.contains("Cache-Control: no-store\r\n"));
    assert_eq!(sink.client_count(), 1);
    sink.send(&msg(7)).unwrap();
    events.read_until("\n\n");
    let frame = events.frames();
    assert!(frame.starts_with("id: 1\ndata: "), "{frame:?}");
    let data = frame.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
    assert_eq!(serde_json::from_str::<Value>(data).unwrap(), msg(7));
}

#[test]
fn a_late_joiner_replays_the_backlog_before_live_frames() {
    let sink = SseSink::bind(SseOptions { backlog: 2, ..SseOptions::default() }).unwrap();
    for n in 0..4 {
        sink.send(&msg(n)).unwrap();
    }
    let mut events = open_events(&sink);
    sink.send(&msg(9)).unwrap();
    events.read_until("\"n\":9}\n\n");
    let frames: Vec<&str> = events.frames().split("\n\n").collect();
    assert_eq!(frames[0], r#"data: {"type":"@xstate.event","n":2}"#);
    assert_eq!(frames[1], r#"data: {"type":"@xstate.event","n":3}"#);
    assert_eq!(frames[2], "id: 5\ndata: {\"type\":\"@xstate.event\",\"n\":9}");
}

#[test]
fn non_ascii_payloads_are_encoded_like_the_json_lines_sink() {
    let sink = sink();
    sink.send(&json!({"type": "@xstate.event", "s": "caf\u{e9}\nx"})).unwrap();
    let reply = get(&sink, "/messages", &[("X-XSM-Token", sink.token())]);
    assert!(reply.body.contains(r"caf\u00e9\nx"), "{}", reply.body);
    assert!(reply.body.is_ascii());
}

// ----------------------------------------------------------------------------- lifecycle

#[test]
fn a_non_loopback_host_without_a_token_is_a_construction_error() {
    for host in ["0.0.0.0", "192.168.1.5", "example.com"] {
        let options = SseOptions { host: host.to_owned(), ..SseOptions::default() };
        assert!(matches!(SseSink::bind(options), Err(SseError::TokenRequired(h)) if h == host));
    }
    let empty = SseOptions {
        host: "0.0.0.0".to_owned(),
        token: Some(String::new()),
        ..SseOptions::default()
    };
    assert!(matches!(SseSink::bind(empty), Err(SseError::TokenRequired(_))));
}

#[test]
fn unusable_tokens_are_refused_and_an_explicit_token_is_used() {
    for token in ["has space", "semi;colon", "crlf\r\nSet-Cookie: x=1", "q\"uote"] {
        let options = SseOptions { token: Some(token.to_owned()), ..SseOptions::default() };
        assert!(matches!(SseSink::bind(options), Err(SseError::InvalidToken)), "{token:?}");
    }
    let options = SseOptions { token: Some("my-Secret_1.2~3".to_owned()), ..SseOptions::default() };
    let sink = SseSink::bind(options).unwrap();
    assert_eq!(sink.token(), "my-Secret_1.2~3");
    assert_eq!(get(&sink, "/messages", &[("X-XSM-Token", "my-Secret_1.2~3")]).status, 200);
}

#[test]
fn ipv6_loopback_binds_and_brackets_the_url() {
    let options = SseOptions { host: "::1".to_owned(), ..SseOptions::default() };
    // an IPv6-less machine cannot bind ::1; the rule under test is only the URL shape
    if let Ok(sink) = SseSink::bind(options) {
        assert!(sink.url().starts_with("http://[::1]:"), "{}", sink.url());
        assert_eq!(get(&sink, "/messages", &[("X-XSM-Token", sink.token())]).status, 200);
    }
}

#[test]
fn close_makes_later_sends_fail_and_frees_the_port() {
    let sink = sink();
    let addr = sink.local_addr();
    let mut events = open_events(&sink);
    assert!(sink.send(&msg(1)).is_ok());
    sink.close();
    assert!(sink.is_closed());
    assert!(matches!(sink.send(&msg(2)), Err(SinkError::Closed)));
    sink.close(); // idempotent
    assert_eq!(sink.client_count(), 0);
    // the connected client sees the frame sent before the close, then the stream ends
    let mut rest = Vec::new();
    let _ = events.tcp.read_to_end(&mut rest);
    events.seen.push_str(&String::from_utf8_lossy(&rest));
    assert!(events.frames().contains("\"n\":1}"), "{:?}", events.seen);
    assert!(!events.seen.contains("\"n\":2}"));
    assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
}

#[test]
fn dropping_the_sink_stops_the_server() {
    let sink = sink();
    let addr = sink.local_addr();
    assert_eq!(get(&sink, "/messages", &[("X-XSM-Token", sink.token())]).status, 200);
    drop(sink);
    assert!(TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err());
}

#[test]
fn many_clients_each_receive_every_message() {
    let sink = sink();
    let mut clients: Vec<Events> = (0..4).map(|_| open_events(&sink)).collect();
    assert_eq!(sink.client_count(), 4);
    sink.send(&msg(1)).unwrap();
    for client in &mut clients {
        assert!(client.read_until("\n\n").contains("\"n\":1}"));
    }
}

// ------------------------------------------------------------------------- strict parsing

#[test]
fn unusual_requests_are_refused() {
    let sink = sink();
    let token = sink.token().to_owned();
    let post = format!(
        "POST /messages HTTP/1.1\r\nHost: {}\r\nX-XSM-Token: {token}\r\n\r\n",
        sink.local_addr()
    );
    assert_eq!(raw(&sink, post.as_bytes()).status, 405);
    assert_eq!(raw(&sink, b"hello\r\n\r\n").status, 400);
    assert_eq!(raw(&sink, b"GET / HTTP/1.1\nHost: x\n\n").status, 400);
    assert_eq!(raw(&sink, b"GET / HTTP/3\r\n\r\n").status, 505);
    let body = format!(
        "GET /messages HTTP/1.1\r\nHost: {}\r\nContent-Length: 4\r\n\r\nabcd",
        sink.local_addr()
    );
    assert_eq!(raw(&sink, body.as_bytes()).status, 400);
    let twice = request_text(&sink, "/", &[("X-XSM-Token", &token), ("X-XSM-Token", "other")]);
    assert_eq!(raw(&sink, twice.as_bytes()).status, 400);
    let folded = request_text(&sink, "/", &[("X-A", "b\r\n continued")]);
    assert_eq!(raw(&sink, folded.as_bytes()).status, 400);
}

#[test]
fn oversized_heads_are_refused() {
    let sink = sink();
    let huge = request_text(&sink, "/", &[("X-Big", &"a".repeat(20_000))]);
    assert_eq!(raw(&sink, huge.as_bytes()).status, 431);
}

#[test]
fn a_stalled_client_does_not_block_other_requests() {
    let sink = sink();
    let mut slow = connect(sink.local_addr());
    slow.write_all(b"GET / HTT").unwrap();
    assert_eq!(get(&sink, "/messages", &[("X-XSM-Token", sink.token())]).status, 200);
}

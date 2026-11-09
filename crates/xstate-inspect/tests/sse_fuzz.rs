//! Deterministic, bounded fuzzing of the `SseSink` HTTP front end plus the connection cap and
//! header-injection attempts. No randomness: a fixed-seed LCG drives the byte stream, so the
//! suite is reproducible. A panic in any server thread is recorded by a global hook and fails the
//! test.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use xstate_inspect::sse::COOKIE_NAME;
use xstate_inspect::{SseOptions, SseSink};

const TIMEOUT: Duration = Duration::from_secs(5);

/// A small, deterministic LCG (Knuth's MMIX parameters).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn byte(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
}

fn sink() -> SseSink {
    SseSink::bind(SseOptions::default()).unwrap()
}

/// Write `bytes`, then read whatever the server sends back until EOF or a short deadline.
fn exchange(addr: std::net::SocketAddr, bytes: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(250))).unwrap();
    stream.write_all(bytes).unwrap();
    let mut out = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                if out.len() > 64 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    out
}

#[test]
fn random_bytes_never_panic_or_hang_the_server() {
    static PANICS: AtomicUsize = AtomicUsize::new(0);
    struct ResetHook;
    impl Drop for ResetHook {
        fn drop(&mut self) {
            let _ = std::panic::take_hook();
        }
    }
    let _guard = ResetHook;
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        prev(info);
    }));

    let sink = sink();
    let addr = sink.local_addr();
    let mut rng = Lcg(0x9e37_79b9_7f4a_7c15);
    for i in 0..300 {
        let len = (rng.next() % 513) as usize;
        let bytes = rng.bytes(len);
        let reply = exchange(addr, &bytes);
        if !reply.is_empty() {
            // The parser either answers with a well-formed HTTP status line or closes silently.
            assert!(
                reply.starts_with(b"HTTP/1.1 "),
                "iteration {i}: a reply that is not an HTTP status line: {:?}",
                String::from_utf8_lossy(&reply[..reply.len().min(64)])
            );
        }
    }

    // The server is still alive and functional after the fuzz, and no thread panicked.
    assert_eq!(PANICS.load(Ordering::Relaxed), 0, "a server thread panicked under fuzzing");
    let token = sink.token().to_owned();
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(stream, "GET /messages HTTP/1.1\r\nHost: {addr}\r\nX-XSM-Token: {token}\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 200 OK"), "{buf}");
}

#[test]
fn the_connection_cap_is_enforced_at_129() {
    let sink = sink();
    let addr = sink.local_addr();
    let token = sink.token().to_owned();
    let mut clients = Vec::new();
    for _ in 0..128 {
        let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
        tcp.set_read_timeout(Some(TIMEOUT)).unwrap();
        write!(tcp, "GET /events HTTP/1.1\r\nHost: {addr}\r\nX-XSM-Token: {token}\r\n\r\n")
            .unwrap();
        clients.push(tcp);
    }
    // Wait until all 128 are served (each holds a connection slot for the stream lifetime).
    let deadline = Instant::now() + TIMEOUT;
    while sink.client_count() < 128 {
        assert!(
            Instant::now() < deadline,
            "only {} of 128 clients subscribed",
            sink.client_count()
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The 129th connection is closed without being served.
    let mut extra = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    extra.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let write_res =
        write!(extra, "GET /messages HTTP/1.1\r\nHost: {addr}\r\nX-XSM-Token: {token}\r\n\r\n");
    match write_res {
        Ok(()) => {
            let mut byte = [0_u8; 1];
            match extra.read(&mut byte) {
                Ok(0) => {} // closed without a reply: refused at the cap
                Ok(_) => {
                    let mut rest = Vec::new();
                    let _ = extra.read_to_end(&mut rest);
                    panic!(
                        "the 129th connection was served, expected refusal: {:?}",
                        String::from_utf8_lossy(&rest)
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(e) => panic!("the 129th connection neither closed nor answered: {e}"),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(e) => panic!("write to 129th connection failed with unexpected error: {e}"),
    }
    drop(clients);
    drop(extra);
    sink.close();
}

#[test]
fn header_injection_cannot_break_out_of_a_header() {
    let sink = sink();
    let token = sink.token().to_owned();
    let addr = sink.local_addr();
    // A header value carrying a bare LF is refused by the strict reader (control byte).
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    tcp.set_read_timeout(Some(TIMEOUT)).unwrap();
    tcp.write_all(
        format!(
            "GET /messages HTTP/1.1\r\nHost: {addr}\r\nX-XSM-Token: {token}\r\nX-Evil: a\nSet-Cookie: pwn=1\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut buf = String::new();
    tcp.read_to_string(&mut buf).unwrap();
    assert!(buf.starts_with("HTTP/1.1 400"), "{buf}");

    // The token value is never reflected outside the first-load Set-Cookie and the URL: an
    // attacker-supplied cookie/header must not be echoed in any response header or body.
    let mut tcp = TcpStream::connect_timeout(&addr, TIMEOUT).unwrap();
    tcp.set_read_timeout(Some(TIMEOUT)).unwrap();
    tcp.write_all(
        format!(
            "GET / HTTP/1.1\r\nHost: {addr}\r\nCookie: {COOKIE_NAME}={token}; injected=evil\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut page = String::new();
    tcp.read_to_string(&mut page).unwrap();
    assert!(!page.contains("injected=evil"), "attacker header reflected:\n{page}");
    assert!(!page.contains("Set-Cookie: injected"), "attacker cookie echoed:\n{page}");
}

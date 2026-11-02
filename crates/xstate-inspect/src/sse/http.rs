//! A minimal, strict HTTP/1.1 request parser and response writer.
//!
//! Only what the inspector needs: `GET`-shaped requests with a bounded head, no body, no
//! pipelining, no keep-alive. Anything unusual is refused rather than interpreted: obsolete
//! line folding, bare line feeds, control or non-ASCII bytes, duplicate security-relevant
//! headers, a request body, a head larger than [`MAX_HEAD_BYTES`] or slower than
//! [`HEAD_TIMEOUT`].

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Largest request head (request line plus headers) accepted.
pub(super) const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Most header lines accepted.
const MAX_HEADERS: usize = 64;
/// Longest request target accepted.
const MAX_TARGET_BYTES: usize = 2048;
/// Time a client has to deliver the whole request head.
pub(super) const HEAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Headers that must appear at most once: a duplicate could make two parsers disagree about
/// who the caller is.
const SINGLETON_HEADERS: [&str; 7] = [
    "host",
    "origin",
    "authorization",
    "x-xsm-token",
    "cookie",
    "content-length",
    "transfer-encoding",
];

/// A parsed request head.
#[derive(Debug)]
pub(super) struct Request {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: Option<String>,
    headers: Vec<(String, String)>,
}

impl Request {
    /// The value of header `name` (lower case), if sent.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

/// Why a request was not served.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    /// Drop the connection without a reply (EOF, timeout, I/O failure).
    Silent,
    /// Reply with this status and reason, then close.
    Status(u16, &'static str),
}

const fn bad(reason: &'static str) -> Rejection {
    Rejection::Status(400, reason)
}

/// Read and parse one request head from `stream`.
pub(super) fn read_request(stream: &mut TcpStream) -> Result<Request, Rejection> {
    parse_head(&read_head(stream)?)
}

/// Read until the blank line that ends the head; returns the head without it.
fn read_head(stream: &mut TcpStream) -> Result<Vec<u8>, Rejection> {
    let deadline = Instant::now() + HEAD_TIMEOUT;
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(Rejection::Silent)?;
        stream.set_read_timeout(Some(left)).map_err(|_| Rejection::Silent)?;
        let read = match stream.read(&mut chunk) {
            Ok(0) => return Err(Rejection::Silent),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(Rejection::Silent),
        };
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..read]);
        if has_bare_lf(&buf) {
            return Err(bad("bare line feed in the request head"));
        }
        if let Some(at) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let end = from + at + 4;
            if buf.len() > end {
                return Err(bad("unexpected data after the request head"));
            }
            buf.truncate(end - 4);
            return Ok(buf);
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(Rejection::Status(431, "request header fields too large"));
        }
    }
}

/// A `\n` not preceded by `\r`. Such a head can never end in `\r\n\r\n` the way HTTP/1.1
/// requires, so it is refused at once instead of waiting for the timeout.
fn has_bare_lf(buf: &[u8]) -> bool {
    buf.first() == Some(&b'\n') || buf.windows(2).any(|w| w[1] == b'\n' && w[0] != b'\r')
}

/// `true` for an RFC 9110 `token` (method and header names).
fn is_token(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Parse a request head (without the final blank line).
pub(super) fn parse_head(head: &[u8]) -> Result<Request, Rejection> {
    let clean = |b: &u8| matches!(*b, b'\t' | b'\r' | b'\n') || (0x20..0x7f).contains(b);
    if !head.iter().all(clean) {
        return Err(bad("control or non-ASCII bytes in the request head"));
    }
    let text = std::str::from_utf8(head).map_err(|_| bad("request head is not ASCII"))?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let (method, target) = parse_request_line(request_line)?;
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if headers.len() >= MAX_HEADERS {
            return Err(Rejection::Status(431, "too many header fields"));
        }
        headers.push(parse_header_line(line, &headers)?);
    }
    reject_body(&headers)?;
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), Some(query.to_owned())),
        None => (target.to_owned(), None),
    };
    Ok(Request { method: method.to_owned(), path, query, headers })
}

fn parse_request_line(line: &str) -> Result<(&str, &str), Rejection> {
    if line.contains(['\r', '\n']) {
        return Err(bad("bare line feed in the request line"));
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("malformed request line"));
    };
    if !is_token(method) {
        return Err(bad("malformed method"));
    }
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(Rejection::Status(505, "HTTP version not supported"));
    }
    if !target.starts_with('/') || target.contains('#') {
        return Err(bad("request target must be an absolute path"));
    }
    if target.len() > MAX_TARGET_BYTES {
        return Err(Rejection::Status(414, "request target too long"));
    }
    Ok((method, target))
}

fn parse_header_line(line: &str, seen: &[(String, String)]) -> Result<(String, String), Rejection> {
    if line.contains(['\r', '\n']) {
        return Err(bad("bare line feed in a header"));
    }
    if line.starts_with([' ', '\t']) {
        return Err(bad("obsolete line folding"));
    }
    let Some((name, value)) = line.split_once(':') else {
        return Err(bad("malformed header line"));
    };
    if !is_token(name) {
        return Err(bad("malformed header name"));
    }
    let name = name.to_ascii_lowercase();
    if SINGLETON_HEADERS.contains(&name.as_str()) && seen.iter().any(|(k, _)| *k == name) {
        return Err(bad("duplicate header"));
    }
    Ok((name, value.trim_matches([' ', '\t']).to_owned()))
}

/// The inspector never reads a request body, so a request that announces one is refused.
fn reject_body(headers: &[(String, String)]) -> Result<(), Rejection> {
    let find = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if find("transfer-encoding").is_some() {
        return Err(bad("request bodies are not accepted"));
    }
    match find("content-length") {
        None | Some("0") => Ok(()),
        Some(_) => Err(bad("request bodies are not accepted")),
    }
}

/// A response with a fixed body.
#[derive(Debug)]
pub(super) struct Response {
    status: u16,
    reason: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Cow<'static, [u8]>,
}

impl Response {
    /// An empty response.
    pub(super) fn new(status: u16, reason: &'static str) -> Self {
        Self { status, reason, headers: Vec::new(), body: Cow::Borrowed(&[]) }
    }

    /// A `text/plain` refusal (`Cache-Control: no-store`, like the Python `_deny`).
    pub(super) fn text(status: u16, reason: &'static str, message: &str) -> Self {
        Self::new(status, reason)
            .header("Content-Type", "text/plain; charset=utf-8")
            .header("Cache-Control", "no-store")
            .body(Cow::Owned(message.as_bytes().to_vec()))
    }

    /// Append a header.
    #[must_use]
    pub(super) fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// Set the body.
    #[must_use]
    pub(super) fn body(mut self, body: Cow<'static, [u8]>) -> Self {
        self.body = body;
        self
    }

    /// Serialise and send; the connection is closed afterwards.
    pub(super) fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let mut head =
            format!("HTTP/1.1 {} {}\r\nServer: xsm-inspector\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        let _ = write!(head, "Content-Length: {}\r\nConnection: close\r\n\r\n", self.body.len());
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&self.body);
        out.write_all(&bytes)?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Request, Rejection> {
        parse_head(text.as_bytes())
    }

    #[test]
    fn parses_a_normal_request() {
        let req = parse("GET /events?x=1 HTTP/1.1\r\nHost: 127.0.0.1:1\r\nX-Thing:  a b \t")
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!((req.method.as_str(), req.path.as_str()), ("GET", "/events"));
        assert_eq!(req.query.as_deref(), Some("x=1"));
        assert_eq!(req.header("host"), Some("127.0.0.1:1"));
        assert_eq!(req.header("x-thing"), Some("a b"));
        assert_eq!(req.header("missing"), None);
    }

    #[test]
    fn refuses_unusual_requests() {
        let cases = [
            "GET / HTTP/1.1\r\nHost: a\r\n folded: x",
            "GET / HTTP/1.1\nHost: a",
            "GET / HTTP/1.1\r\nHost: a\r\nHost: b",
            "GET / HTTP/1.1\r\nCookie: a\r\nCookie: b",
            "GET  / HTTP/1.1",
            "GET / HTTP/1.1 extra",
            "GET http://evil/ HTTP/1.1",
            "GET /#frag HTTP/1.1",
            "GET /\u{e9} HTTP/1.1",
            "GET / HTTP/1.1\r\nHost: a\u{0}b",
            "G ET / HTTP/1.1",
            "GET / HTTP/1.1\r\nBad Name: x",
            "GET / HTTP/1.1\r\nno-colon",
            "GET / HTTP/1.1\r\nContent-Length: 5",
            "GET / HTTP/1.1\r\nTransfer-Encoding: chunked",
        ];
        for case in cases {
            assert!(matches!(parse(case), Err(Rejection::Status(400, _))), "{case:?}");
        }
        assert!(matches!(parse("GET / HTTP/2"), Err(Rejection::Status(505, _))));
        let long = format!("GET /{} HTTP/1.1", "a".repeat(MAX_TARGET_BYTES));
        assert!(matches!(parse(&long), Err(Rejection::Status(414, _))));
        let many = format!("GET / HTTP/1.1\r\n{}", "A: b\r\n".repeat(MAX_HEADERS + 1));
        assert!(matches!(parse(many.trim_end()), Err(Rejection::Status(431, _))));
        assert!(parse("GET / HTTP/1.1\r\nContent-Length: 0").is_ok());
    }

    #[test]
    fn writes_a_framed_response() {
        let mut out = Vec::new();
        Response::text(401, "Unauthorized", "no")
            .write_to(&mut out)
            .unwrap_or_else(|e| panic!("{e}"));
        let text = String::from_utf8(out).unwrap_or_default();
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized\r\nServer: xsm-inspector\r\n"));
        assert!(text.contains("Content-Length: 2\r\nConnection: close\r\n\r\nno"));
    }
}

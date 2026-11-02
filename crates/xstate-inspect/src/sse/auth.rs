//! The security rules of the inspector server: loopback detection, `Host` and `Origin`
//! checks, token extraction and comparison.
//!
//! Everything here is a pure function over header values, so it is unit-testable without a
//! socket. The rules are those of the Python `SseSink` (the "X0.7" hardening notes):
//!
//! * a non-loopback bind needs an explicit token (enforced at construction);
//! * a loopback bind checks `Host` against a strict loopback parser (DNS rebinding);
//! * every request carrying an `Origin` must be same-origin or allow-listed;
//! * the token comes from `Authorization: Bearer`, `X-XSM-Token` or the cookie, and is compared
//!   in constant time; `?token=` is honoured only by the first page load.

use std::collections::HashSet;
use std::net::IpAddr;

use super::http::Request;

/// Name of the `HttpOnly` cookie that carries the token after the first page load.
pub const COOKIE_NAME: &str = "xsm_inspect";

/// The loopback host names the inspector binds to by default.
pub const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

/// Bytes of entropy in a generated token (`secrets.token_urlsafe(32)`).
const TOKEN_BYTES: usize = 32;

/// `true` for `localhost` or a literal loopback IP, parsed strictly.
///
/// A `Host: 127.0.0.1.evil.com` header is an attacker-controlled DNS name, exactly the
/// DNS-rebinding request the `Host` check exists to refuse, so prefix matching is not enough.
pub(super) fn is_loopback_host(host: &str) -> bool {
    let lowered = host.trim().to_lowercase();
    let mut name = lowered.trim_end_matches('.');
    if let Some(inner) = name.strip_prefix('[').and_then(|n| n.strip_suffix(']')) {
        name = inner;
    }
    if name == "localhost" {
        return true;
    }
    name.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

fn all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The host part of a `Host` header value: `name`, `name:port`, `[v6]` or `[v6]:port`.
/// `None` for anything malformed (a bare IPv6 literal is not a valid `Host`).
fn host_name(header: &str) -> Option<&str> {
    if let Some(rest) = header.strip_prefix('[') {
        let (name, after) = rest.split_once(']')?;
        if after.is_empty() {
            return Some(name);
        }
        return all_digits(after.strip_prefix(':')?).then_some(name);
    }
    match header.matches(':').count() {
        0 => Some(header),
        1 => {
            let (name, port) = header.split_once(':')?;
            all_digits(port).then_some(name)
        }
        _ => None,
    }
}

/// Validate the `Host` header against DNS rebinding.
///
/// Any host passes on a non-loopback bind (the token is the control there); on a loopback bind
/// only a loopback host name does.
pub(super) fn host_ok(bind_is_loopback: bool, header: Option<&str>) -> bool {
    if !bind_is_loopback {
        return true;
    }
    header.and_then(|raw| host_name(raw.trim())).is_some_and(is_loopback_host)
}

/// Apply the cross-origin policy to an `Origin` header.
///
/// Requests without one (curl, native clients) pass; so do allow-listed origins and
/// same-origin requests (`http://<Host>`).
pub(super) fn origin_ok(
    origin: Option<&str>,
    host: Option<&str>,
    allowed: &HashSet<String>,
) -> bool {
    let Some(origin) = origin else { return true };
    if allowed.contains(origin) {
        return true;
    }
    host.is_some_and(|h| !h.is_empty() && origin == format!("http://{h}"))
}

/// Compare two byte strings without an early exit on the first difference.
pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = u8::from(a.len() != b.len());
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    std::hint::black_box(diff) == 0
}

/// The token the client presented on an API call: `Authorization: Bearer`, then
/// `X-XSM-Token`, then the cookie.
pub(super) fn presented_token(request: &Request) -> Option<&str> {
    if let Some(auth) = request.header("authorization")
        && auth.get(..7).is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer "))
    {
        return Some(auth[7..].trim());
    }
    if let Some(header) = request.header("x-xsm-token").filter(|h| !h.is_empty()) {
        return Some(header);
    }
    request.header("cookie").and_then(|raw| cookie_value(raw, COOKIE_NAME))
}

/// The value of cookie `name` in a `Cookie` header (first match wins; malformed pairs are
/// ignored; surrounding double quotes are removed).
fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then(|| {
            let value = value.trim();
            value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(value)
        })
    })
}

/// The non-empty `token` query parameter of a first page load, percent-decoded.
pub(super) fn first_load_token(query: Option<&str>) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (percent_decode(key) == "token").then(|| percent_decode(value)).filter(|v| !v.is_empty())
    })
}

/// `application/x-www-form-urlencoded` decoding (`+` is a space; a malformed `%` stays).
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => match bytes.get(i + 1..i + 3).and_then(hex_pair) {
                Some(decoded) => {
                    out.push(decoded);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            other => out.push(other),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(pair: &[u8]) -> Option<u8> {
    let digit = |b: u8| char::from(b).to_digit(16);
    let (hi, lo) = (digit(*pair.first()?)?, digit(*pair.get(1)?)?);
    u8::try_from(hi * 16 + lo).ok()
}

/// `true` when `token` is usable verbatim in a URL query, a cookie value and a header:
/// non-empty and made only of RFC 3986 unreserved characters (`A-Za-z0-9-._~`).
pub(super) fn token_is_well_formed(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// A fresh URL-safe token from the operating system's randomness (`token_urlsafe(32)`).
///
/// # Errors
/// The text of the OS randomness failure.
pub(super) fn generate_token() -> Result<String, String> {
    use rand::TryRngCore as _;
    let mut bytes = [0_u8; TOKEN_BYTES];
    rand::rngs::OsRng.try_fill_bytes(&mut bytes).map_err(|e| e.to_string())?;
    Ok(base64url(&bytes))
}

/// Unpadded base64url.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for shift in [18, 12, 6, 0].into_iter().take(chunk.len() + 1) {
            out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_detection_is_strict() {
        for ok in ["127.0.0.1", "localhost", "LOCALHOST.", "::1", "[::1]", "127.0.0.2", " ::1 "] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in
            ["127.0.0.1.evil.com", "0.0.0.0", "example.com", "", "192.168.1.1", "localhost.x"]
        {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }

    #[test]
    fn host_header_rules() {
        for ok in ["127.0.0.1:8765", "localhost", "[::1]:80", "[::1]", "LocalHost:1"] {
            assert!(host_ok(true, Some(ok)), "{ok}");
        }
        for bad in ["127.0.0.1.evil.com", "evil.com", "::1", "127.0.0.1:", "[::1]x", "[::1", ""] {
            assert!(!host_ok(true, Some(bad)), "{bad}");
        }
        assert!(!host_ok(true, None));
        assert!(host_ok(false, Some("anything.example")));
        assert!(host_ok(false, None));
    }

    #[test]
    fn origin_policy() {
        let allowed: HashSet<String> = ["http://dev.local:3000".to_owned()].into();
        assert!(origin_ok(None, Some("127.0.0.1:1"), &allowed));
        assert!(origin_ok(Some("http://127.0.0.1:1"), Some("127.0.0.1:1"), &allowed));
        assert!(origin_ok(Some("http://dev.local:3000"), Some("127.0.0.1:1"), &allowed));
        assert!(!origin_ok(Some("http://evil.com"), Some("127.0.0.1:1"), &allowed));
        assert!(!origin_ok(Some("null"), Some("127.0.0.1:1"), &allowed));
        assert!(!origin_ok(Some(""), None, &allowed));
    }

    #[test]
    fn constant_time_eq_matches_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn cookie_and_query_parsing() {
        assert_eq!(cookie_value("a=1; xsm_inspect=tok; b=2", COOKIE_NAME), Some("tok"));
        assert_eq!(cookie_value("xsm_inspect=\"q\"", COOKIE_NAME), Some("q"));
        assert_eq!(cookie_value("nope; other=1", COOKIE_NAME), None);
        assert_eq!(first_load_token(Some("x=1&token=abc%2Bd")), Some("abc+d".to_owned()));
        assert_eq!(first_load_token(Some("token=")), None);
        assert_eq!(first_load_token(Some("tokens=a")), None);
        assert_eq!(first_load_token(None), None);
        assert_eq!(percent_decode("a%zz%4"), "a%zz%4");
    }

    #[test]
    fn tokens_are_urlsafe_and_distinct() {
        let a = generate_token().unwrap_or_default();
        let b = generate_token().unwrap_or_default();
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(token_is_well_formed(&a));
        assert!(!token_is_well_formed("a b") && !token_is_well_formed("a;b"));
        assert!(!token_is_well_formed("") && !token_is_well_formed("a\r\nSet-Cookie: x"));
        assert_eq!(base64url(b"\xff\xfe"), "__4");
        assert_eq!(base64url(b"M"), "TQ");
    }
}

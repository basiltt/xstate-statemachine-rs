//! [`RedisInbox`]: the idempotency inbox on Redis, in the layout Python's `RedisInbox` writes.
//!
//! Python reference: `contrib/redis/inbox.py`. One hash per scope (field = idempotency key,
//! value = JSON entry) plus a sorted-set TTL index, so `purge_expired` is a range query. Every
//! expiry is computed and compared on the Redis server's `TIME` inside the Lua scripts, never
//! on a caller's clock: hosts with skewed clocks agree on whether a key is live.

use std::sync::{Arc, LazyLock};

use redis::Script;
use serde_json::Value;

use super::conn::{Pool, scan_all};
use super::keys::{Keys, escape_glob};
use crate::idempotency::{InboxEntry, InboxStore};
use crate::store::{Result, StoreError};

/// Shared Lua prelude: `now()` is the server clock in float seconds and `member(scope, key)` the
/// TTL-index member `"{len(scope)}:{scope}{key}"` (the length prefix makes the split unambiguous
/// for a scope containing any character).
const PRELUDE: &str = r"
local function now()
  local t = redis.call('TIME')
  return tonumber(t[1]) + tonumber(t[2]) / 1000000
end
local function member(scope, key)
  return string.len(scope) .. ':' .. scope .. key
end
local function expiry(ttl)
  if ttl == '' then return nil end
  return now() + tonumber(ttl)
end
local function put(hash, zset, scope, key, fp, receipt, ttl)
  local exp = expiry(ttl)
  local e = {fingerprint = fp, receipt_json = receipt}
  if exp then e.expires_at = string.format('%.6f', exp) end
  redis.call('HSET', hash, key, cjson.encode(e))
  if exp then
    redis.call('ZADD', zset, exp, member(scope, key))
  else
    redis.call('ZREM', zset, member(scope, key))
  end
end
local function live(raw)
  if not raw then return false end
  local e = cjson.decode(raw)
  return e.expires_at == nil or e.expires_at == cjson.null
    or tonumber(e.expires_at) > now()
end
";

/// KEYS: inbox hash, exp zset. ARGV: key, fingerprint, ttl or "", scope.
const CLAIM_BODY: &str = r"
if live(redis.call('HGET', KEYS[1], ARGV[1])) then return 0 end
put(KEYS[1], KEYS[2], ARGV[4], ARGV[1], ARGV[2], cjson.null, ARGV[3])
return 1
";

/// KEYS: inbox hash, exp zset. ARGV: key, receipt, ttl or "", scope.
const MARK_BODY: &str = r"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
local fp = ''
if raw then fp = cjson.decode(raw).fingerprint end
put(KEYS[1], KEYS[2], ARGV[4], ARGV[1], fp, ARGV[2], ARGV[3])
return 1
";

/// KEYS: inbox hash. ARGV: key. Returns the entry iff live, else nil.
const GET_BODY: &str = r"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if live(raw) then return raw end
return nil
";

/// KEYS: inbox hash, exp zset. ARGV: key, scope. Drops the entry only while it is in flight.
const RELEASE_BODY: &str = r"
local raw = redis.call('HGET', KEYS[1], ARGV[1])
if raw then
  local e = cjson.decode(raw)
  if e.receipt_json == cjson.null or e.receipt_json == nil then
    redis.call('HDEL', KEYS[1], ARGV[1])
    redis.call('ZREM', KEYS[2], member(ARGV[2], ARGV[1]))
    return 1
  end
end
return 0
";

/// KEYS: exp zset. ARGV: now or "" (server clock), hash-name prefix. Range-deletes expired
/// members and their hash fields.
const PURGE_BODY: &str = r"
local at = ARGV[1]
if at == '' then at = now() end
local members = redis.call('ZRANGEBYSCORE', KEYS[1], '-inf', at)
local n = 0
for _, m in ipairs(members) do
  local sep = string.find(m, ':', 1, true)
  local len = sep and tonumber(string.sub(m, 1, sep - 1))
  if len then
    local scope = string.sub(m, sep + 1, sep + len)
    local key = string.sub(m, sep + len + 1)
    n = n + redis.call('HDEL', ARGV[2] .. scope, key)
  end
  redis.call('ZREM', KEYS[1], m)
end
return n
";

/// KEYS: inbox hash, exp zset. ARGV: scope. Atomic tenant erase.
const FORGET_BODY: &str = r"
local fields = redis.call('HKEYS', KEYS[1])
for _, f in ipairs(fields) do
  redis.call('ZREM', KEYS[2], member(ARGV[1], f))
end
redis.call('DEL', KEYS[1])
return #fields
";

fn script(body: &str) -> Script {
    Script::new(&format!("{PRELUDE}{body}"))
}

static CLAIM: LazyLock<Script> = LazyLock::new(|| script(CLAIM_BODY));
static MARK: LazyLock<Script> = LazyLock::new(|| script(MARK_BODY));
static GET: LazyLock<Script> = LazyLock::new(|| script(GET_BODY));
static RELEASE: LazyLock<Script> = LazyLock::new(|| script(RELEASE_BODY));
static PURGE: LazyLock<Script> = LazyLock::new(|| script(PURGE_BODY));
static FORGET: LazyLock<Script> = LazyLock::new(|| script(FORGET_BODY));

/// The TTL as a script argument: `""` for none, else `repr(float(ttl_s))`.
fn ttl_arg(ttl_s: Option<f64>) -> Result<String> {
    match ttl_s {
        None => Ok(String::new()),
        Some(ttl) if ttl.is_finite() => Ok(format!("{ttl:?}")),
        Some(ttl) => Err(StoreError::Other(format!("ttl_s must be a finite number, got {ttl}"))),
    }
}

/// A float the Lua side wrote as a string (`'%.6f'`) or a number.
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn decode_entry(raw: &str) -> Result<InboxEntry> {
    let corrupt = |why: &str| StoreError::Corrupt(format!("Redis inbox entry: {why}"));
    let value: Value = serde_json::from_str(raw).map_err(|e| corrupt(&e.to_string()))?;
    let Value::Object(map) = value else { return Err(corrupt("not an object")) };
    let text = |name: &str| match map.get(name) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    Ok(InboxEntry {
        fingerprint: text("fingerprint").unwrap_or_default(),
        receipt_json: text("receipt_json"),
        expires_at: map.get("expires_at").and_then(number),
    })
}

/// [`InboxStore`] on Redis (blocking connections), byte-compatible with Python's `RedisInbox`.
///
/// Shares a namespace with a [`RedisStore`](super::RedisStore) given the same prefix;
/// `RedisStore::forget` does not touch inbox rows (they belong to a principal / scope, not to an
/// instance) — call [`InboxStore::forget`] with the scope.
///
/// A claim whose worker died before `mark` stays in flight (answering "duplicate") until its TTL
/// passes, as with the SQLite and in-memory inboxes. The mark is written right after the snapshot
/// save, not in one transaction with it (Redis has none spanning both): a crash in between is
/// covered by the in-snapshot processed ring.
#[derive(Debug)]
pub struct RedisInbox {
    pool: Arc<Pool>,
    keys: Keys,
}

impl RedisInbox {
    /// Connect to `url` and use the namespace `prefix` (non-empty, no whitespace).
    ///
    /// # Errors
    /// An invalid URL or prefix. Redis is contacted on first use.
    pub fn open(url: &str, prefix: &str) -> Result<Self> {
        Ok(Self { pool: Arc::new(Pool::open(url)?), keys: Keys::new(prefix)? })
    }

    fn keys_for(&self, scope: &str) -> (String, String) {
        (self.keys.inbox(scope), self.keys.inbox_exp())
    }

    /// Number of entries across every scope (a `SCAN` of the inbox hashes: not for hot paths).
    ///
    /// # Errors
    /// Backend failure.
    pub fn len(&self) -> Result<usize> {
        let pattern = format!("{}*", escape_glob(&self.keys.inbox("")));
        self.pool.run(|conn| {
            let mut total = 0usize;
            for name in scan_all(conn, None, &pattern)? {
                total += redis::cmd("HLEN").arg(name).query::<usize>(conn)?;
            }
            Ok(total)
        })
    }

    /// `true` when no scope holds an entry.
    ///
    /// # Errors
    /// Backend failure.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

impl InboxStore for RedisInbox {
    fn get(&self, scope: &str, key: &str) -> Result<Option<InboxEntry>> {
        let raw: Option<String> =
            self.pool.run(|c| GET.key(self.keys.inbox(scope)).arg(key).invoke(c))?;
        raw.as_deref().map(decode_entry).transpose()
    }

    fn claim(&self, scope: &str, key: &str, fingerprint: &str, ttl_s: Option<f64>) -> Result<bool> {
        let ttl = ttl_arg(ttl_s)?;
        let (hash, exp) = self.keys_for(scope);
        let won: i64 = self.pool.run(|c| {
            CLAIM.key(hash).key(exp).arg(key).arg(fingerprint).arg(ttl).arg(scope).invoke(c)
        })?;
        Ok(won == 1)
    }

    fn mark(&self, scope: &str, key: &str, receipt_json: &str, ttl_s: Option<f64>) -> Result<()> {
        let ttl = ttl_arg(ttl_s)?;
        let (hash, exp) = self.keys_for(scope);
        self.pool.run(|c| {
            MARK.key(hash).key(exp).arg(key).arg(receipt_json).arg(ttl).arg(scope).invoke::<i64>(c)
        })?;
        Ok(())
    }

    fn release(&self, scope: &str, key: &str) -> Result<()> {
        let (hash, exp) = self.keys_for(scope);
        self.pool.run(|c| RELEASE.key(hash).key(exp).arg(key).arg(scope).invoke::<i64>(c))?;
        Ok(())
    }

    fn purge_expired(&self, now: Option<f64>) -> Result<u64> {
        let at = match now {
            None => String::new(),
            Some(t) if t.is_finite() => format!("{t:?}"),
            Some(t) => {
                return Err(StoreError::Other(format!("now must be a finite number, got {t}")));
            }
        };
        let n: u64 = self
            .pool
            .run(|c| PURGE.key(self.keys.inbox_exp()).arg(at).arg(self.keys.inbox("")).invoke(c))?;
        Ok(n)
    }

    fn forget(&self, scope: &str) -> Result<u64> {
        let (hash, exp) = self.keys_for(scope);
        self.pool.run(|c| FORGET.key(hash).key(exp).arg(scope).invoke(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_argument_is_the_python_repr() {
        assert_eq!(ttl_arg(None).unwrap(), "");
        assert_eq!(ttl_arg(Some(604_800.0)).unwrap(), "604800.0");
        assert_eq!(ttl_arg(Some(0.5)).unwrap(), "0.5");
        assert!(ttl_arg(Some(f64::NAN)).is_err());
    }

    #[test]
    fn entries_decode_what_lua_writes() {
        let live = decode_entry(r#"{"fingerprint":"abc","receipt_json":null}"#).unwrap();
        assert_eq!(
            live,
            InboxEntry { fingerprint: "abc".into(), receipt_json: None, expires_at: None }
        );
        let done = decode_entry(
            r#"{"fingerprint":"abc","receipt_json":"{\"ok\": true}","expires_at":"1700000000.250000"}"#,
        )
        .unwrap();
        assert_eq!(done.receipt_json.as_deref(), Some(r#"{"ok": true}"#));
        assert_eq!(done.expires_at, Some(1_700_000_000.25));
        assert!(decode_entry("[1]").is_err());
        assert!(decode_entry("nope").is_err());
    }
}

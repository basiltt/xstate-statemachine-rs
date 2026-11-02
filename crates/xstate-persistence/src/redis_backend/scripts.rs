//! The Lua scripts of the snapshot store, copied byte for byte from Python's
//! `contrib/redis/_layout.py` so both runtimes mutate one namespace with identical atomicity.
//!
//! Each script is wrapped in a [`Script`], loaded lazily; the `redis` crate calls `EVALSHA` and
//! falls back to `SCRIPT LOAD` when the server has not seen it.

use std::sync::LazyLock;

use redis::Script;

/// KEYS: snap hash, keys set, dl hash, deadlines zset. ARGV: expected version or "", snapshot,
/// machine version, `updated_at`, key, ttl ms or "", then (member, due, body) triples. Returns
/// `{new_version, previous}`, or `{-1, actual}` on conflict (`actual` is -1 when absent).
const SAVE_SRC: &str = r"
local cur = redis.call('HGET', KEYS[1], 'version')
local current = cur and tonumber(cur) or 0
if ARGV[1] ~= '' then
  local expected = tonumber(ARGV[1])
  if expected ~= current then
    if cur then return {-1, current} else return {-1, -1} end
  end
end
local nv = current + 1
redis.call('HSET', KEYS[1], 'snapshot', ARGV[2], 'version', nv,
           'machine_version', ARGV[3], 'updated_at', ARGV[4])
redis.call('SADD', KEYS[2], ARGV[5])
-- deadlines: drop this key's members, then add the new ones
local old = redis.call('HKEYS', KEYS[3])
for _, f in ipairs(old) do
  redis.call('ZREM', KEYS[4], f)
  redis.call('ZREM', KEYS[4], ARGV[5] .. '|' .. f)  -- layout-1 member
end
redis.call('DEL', KEYS[3])
local i = 7
while i <= #ARGV do
  local field = ARGV[i]; local due = ARGV[i+1]; local body = ARGV[i+2]
  redis.call('HSET', KEYS[3], field, body)
  redis.call('ZADD', KEYS[4], due, field)
  i = i + 3
end
if ARGV[6] ~= '' then
  redis.call('PEXPIRE', KEYS[1], ARGV[6])
  redis.call('PEXPIRE', KEYS[3], ARGV[6])
end
return {nv, current}
";

/// KEYS: snap, keys set, dl, deadlines, lock, log stream. ARGV: key. Returns `{snapshots,
/// deadlines, locks, log entries}`.
const FORGET_SRC: &str = r"
local snaps = redis.call('DEL', KEYS[1])
redis.call('SREM', KEYS[2], ARGV[1])
local fields = redis.call('HKEYS', KEYS[3])
for _, f in ipairs(fields) do
  redis.call('ZREM', KEYS[4], f)
  redis.call('ZREM', KEYS[4], ARGV[1] .. '|' .. f)  -- layout-1 member
end
redis.call('DEL', KEYS[3])
local locks = redis.call('DEL', KEYS[5])
local entries = 0
if redis.call('EXISTS', KEYS[6]) == 1 then
  entries = redis.call('XLEN', KEYS[6])
  redis.call('DEL', KEYS[6])
end
return {snaps, #fields, locks, entries}
";

/// Same as `FORGET` but leaves the lock and the log alone.
const DELETE_SRC: &str = r"
local snaps = redis.call('DEL', KEYS[1])
redis.call('SREM', KEYS[2], ARGV[1])
local fields = redis.call('HKEYS', KEYS[3])
for _, f in ipairs(fields) do
  redis.call('ZREM', KEYS[4], f)
  redis.call('ZREM', KEYS[4], ARGV[1] .. '|' .. f)  -- layout-1 member
end
redis.call('DEL', KEYS[3])
return {snaps, #fields, 0}
";

/// KEYS: deadlines zset. ARGV: (member, snapshot hash) pairs. Drops index members whose snapshot
/// hash is gone and returns them.
const PRUNE_SRC: &str = r"
local gone = {}
local i = 1
while i <= #ARGV do
  if redis.call('EXISTS', ARGV[i+1]) == 0 then
    redis.call('ZREM', KEYS[1], ARGV[i])
    gone[#gone + 1] = ARGV[i]
  end
  i = i + 2
end
return gone
";

/// KEYS: lock. ARGV: token. Compare-and-delete: only the owner releases.
const UNLOCK_SRC: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
";

/// KEYS: schema. ARGV: this library's layout. Creates the marker or raises an older one; never
/// lowers a newer one. Returns the marker.
const SCHEMA_SRC: &str = r"
local cur = redis.call('GET', KEYS[1])
if not cur or (tonumber(cur) and tonumber(cur) < tonumber(ARGV[1])) then
  redis.call('SET', KEYS[1], ARGV[1])
  return ARGV[1]
end
return cur
";

pub(crate) static SAVE: LazyLock<Script> = LazyLock::new(|| Script::new(SAVE_SRC));
pub(crate) static FORGET: LazyLock<Script> = LazyLock::new(|| Script::new(FORGET_SRC));
pub(crate) static DELETE: LazyLock<Script> = LazyLock::new(|| Script::new(DELETE_SRC));
pub(crate) static PRUNE: LazyLock<Script> = LazyLock::new(|| Script::new(PRUNE_SRC));
pub(crate) static UNLOCK: LazyLock<Script> = LazyLock::new(|| Script::new(UNLOCK_SRC));
pub(crate) static SCHEMA: LazyLock<Script> = LazyLock::new(|| Script::new(SCHEMA_SRC));

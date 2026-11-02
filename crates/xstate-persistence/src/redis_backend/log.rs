//! [`RedisLog`]: the transition log as one Redis Stream per machine id, in the layout Python's
//! `RedisLog` writes.
//!
//! Python reference: `contrib/redis/log.py`. The stream entry id *is* the record's `seq`
//! (`"{seq}-0"`) and every append is one Lua compare-and-append that refuses a `seq` not above
//! the stream's top, so two writers can never store the same `seq`; `append_next` re-reads the
//! top and tries again when another writer won. `read` is an `XRANGE` from `after_seq + 1`:
//! O(log n + limit), not a scan.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use redis::Script;

use super::conn::{Pool, scan_all};
use super::keys::{Keys, escape_glob};
use crate::log::{TransitionLogStore, TransitionRecord};
use crate::store::{Result, StoreError};

/// How often `append_next` re-reads the top after losing a race before it gives up loudly.
const MAX_APPEND_RETRIES: usize = 1000;

/// Page size for `purge_older_than`'s walk of a stream.
const PURGE_PAGE: usize = 1000;

/// KEYS: stream. ARGV: ids. Deletes them and drops the stream when it is empty, so its
/// last-generated id does not block a restart at `seq` 1.
const XDEL_AND_DROP_SRC: &str = r"
local n = redis.call('XDEL', KEYS[1], unpack(ARGV))
if redis.call('XLEN', KEYS[1]) == 0 then redis.call('DEL', KEYS[1]) end
return n
";

/// KEYS: stream. ARGV: seq, ts, record JSON, maxlen or "". Appends at id `"{seq}-0"` iff `seq`
/// is above the top seq (compare-and-append); returns 0 when not.
const APPEND_SRC: &str = r"
local top = redis.call('XREVRANGE', KEYS[1], '+', '-', 'COUNT', 1)
if top[1] then
  local s = tonumber(string.match(top[1][1], '^(%d+)-'))
  if s and s >= tonumber(ARGV[1]) then return 0 end
end
local id = ARGV[1] .. '-0'
if ARGV[4] ~= '' then
  redis.call('XADD', KEYS[1], 'MAXLEN', '~', ARGV[4], id,
    'seq', ARGV[1], 'ts', ARGV[2], 'record', ARGV[3])
else
  redis.call('XADD', KEYS[1], id,
    'seq', ARGV[1], 'ts', ARGV[2], 'record', ARGV[3])
end
return 1
";

static XDEL_AND_DROP: LazyLock<Script> = LazyLock::new(|| Script::new(XDEL_AND_DROP_SRC));
static APPEND: LazyLock<Script> = LazyLock::new(|| Script::new(APPEND_SRC));

type Entry = (String, HashMap<String, String>);

/// Reject a record no store may persist: `seq < 1` or a non-finite `ts`.
fn check_append(record: &TransitionRecord) -> Result<()> {
    if record.seq < 1 {
        return Err(StoreError::Other(format!("seq must be an int >= 1, got {}", record.seq)));
    }
    if !record.ts.is_finite() {
        return Err(StoreError::Other(format!("ts must be a finite number, got {}", record.ts)));
    }
    Ok(())
}

/// The `seq` of a stream entry id `"<seq>-0"`.
fn seq_of(entry_id: &str) -> Result<u64> {
    entry_id
        .strip_suffix("-0")
        .and_then(|ms| ms.parse::<u64>().ok())
        .ok_or_else(|| StoreError::Corrupt(format!("stream id {entry_id:?} is not '<seq>-0'")))
}

/// The `ts` of a stream entry, or [`StoreError::Corrupt`] when it is missing or not numeric.
fn entry_ts(name: &str, id: &str, fields: &HashMap<String, String>) -> Result<f64> {
    fields
        .get("ts")
        .and_then(|raw| raw.trim().parse::<f64>().ok())
        .ok_or_else(|| StoreError::Corrupt(format!("{name}: entry {id:?} has no numeric 'ts'")))
}

fn decode(name: &str, machine_id: &str, (id, fields): &Entry) -> Result<TransitionRecord> {
    let where_ = format!("{name} id={id}");
    let raw = fields
        .get("record")
        .ok_or_else(|| StoreError::Corrupt(format!("{where_}: no 'record' field")))?;
    let record = TransitionRecord::from_json(raw)
        .map_err(|e| StoreError::Corrupt(format!("{where_}: {e}")))?;
    if record.seq != seq_of(id)? || record.machine_id != machine_id {
        return Err(StoreError::Corrupt(format!(
            "{where_}: entry id disagrees with its record ({:?}, {})",
            record.machine_id, record.seq
        )));
    }
    Ok(record)
}

/// [`TransitionLogStore`] on a Redis Stream per machine id (blocking connections),
/// byte-compatible with Python's `RedisLog`.
///
/// Not transactional with [`RedisStore`](super::RedisStore): there is no multi-key transaction
/// spanning the snapshot save, so a log may trail the snapshot after a crash, never lead it.
#[derive(Debug)]
pub struct RedisLog {
    pool: Arc<Pool>,
    keys: Keys,
    maxlen: Option<u64>,
}

impl RedisLog {
    /// Connect to `url` and use the namespace `prefix` (non-empty, no whitespace); streams are
    /// unbounded.
    ///
    /// # Errors
    /// An invalid URL or prefix. Redis is contacted on first use.
    pub fn open(url: &str, prefix: &str) -> Result<Self> {
        Ok(Self { pool: Arc::new(Pool::open(url)?), keys: Keys::new(prefix)?, maxlen: None })
    }

    /// Keep about `maxlen` newest entries per stream (`XADD MAXLEN ~`, approximate: at least
    /// `maxlen` are kept). Trimming drops the head of the audit trail: a replay then needs a
    /// snapshot taken at the first retained record.
    ///
    /// # Errors
    /// `maxlen` is 0.
    pub fn with_maxlen(mut self, maxlen: u64) -> Result<Self> {
        if maxlen < 1 {
            return Err(StoreError::Other("maxlen must be >= 1".into()));
        }
        self.maxlen = Some(maxlen);
        Ok(self)
    }

    /// Compare-and-append `record`; `false` when its `seq` is not above the stream's top.
    fn xadd(&self, record: &TransitionRecord) -> Result<bool> {
        let name = self.keys.log(&record.machine_id);
        let maxlen = self.maxlen.map_or_else(String::new, |n| n.to_string());
        let added: i64 = self.pool.run(|c| {
            APPEND
                .key(name)
                .arg(record.seq)
                .arg(format!("{:?}", record.ts))
                .arg(record.to_json())
                .arg(maxlen)
                .invoke(c)
        })?;
        Ok(added == 1)
    }

    /// Delete the entries of one stream older than `cutoff`, a page at a time.
    fn purge_stream(&self, name: &str, cutoff: f64) -> Result<u64> {
        let mut removed = 0u64;
        let mut start = "-".to_owned();
        loop {
            let page: Vec<Entry> = self.pool.run(|c| {
                redis::cmd("XRANGE")
                    .arg(name)
                    .arg(&start)
                    .arg("+")
                    .arg("COUNT")
                    .arg(PURGE_PAGE)
                    .query(c)
            })?;
            let Some((last, _)) = page.last() else { return Ok(removed) };
            let next = format!("({last}");
            let mut dead: Vec<&str> = Vec::new();
            for (id, fields) in &page {
                if entry_ts(name, id, fields)? < cutoff {
                    dead.push(id);
                }
            }
            if !dead.is_empty() {
                removed += self.pool.run(|c| {
                    let mut call = XDEL_AND_DROP.key(name);
                    for id in &dead {
                        call.arg(*id);
                    }
                    call.invoke::<u64>(c)
                })?;
            }
            start = next;
        }
    }
}

impl TransitionLogStore for RedisLog {
    fn append(&self, record: &TransitionRecord) -> Result<()> {
        check_append(record)?;
        if self.xadd(record)? {
            return Ok(());
        }
        Err(StoreError::Other(format!(
            "RedisLog: seq {} of {:?} is not above the stream's last seq (duplicate append refused)",
            record.seq, record.machine_id
        )))
    }

    fn append_next(&self, record: TransitionRecord) -> Result<TransitionRecord> {
        check_append(&TransitionRecord { seq: 1, ..record.clone() })?;
        for _ in 0..MAX_APPEND_RETRIES {
            let stored =
                TransitionRecord { seq: self.next_seq(&record.machine_id)?, ..record.clone() };
            if self.xadd(&stored)? {
                return Ok(stored);
            }
        }
        Err(StoreError::Other(format!(
            "RedisLog: could not append to {:?} after {MAX_APPEND_RETRIES} attempts (write contention)",
            record.machine_id
        )))
    }

    fn next_seq(&self, machine_id: &str) -> Result<u64> {
        let top: Vec<Entry> = self.pool.run(|c| {
            redis::cmd("XREVRANGE")
                .arg(self.keys.log(machine_id))
                .arg("+")
                .arg("-")
                .arg("COUNT")
                .arg(1)
                .query(c)
        })?;
        match top.first() {
            Some((id, _)) => Ok(seq_of(id)? + 1),
            None => Ok(1),
        }
    }

    fn read(
        &self,
        machine_id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<Vec<TransitionRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let name = self.keys.log(machine_id);
        let (head, rows): (Vec<Entry>, Vec<Entry>) = self.pool.run(|c| {
            let head = if after_seq == 0 {
                redis::cmd("XRANGE").arg(&name).arg("-").arg("+").arg("COUNT").arg(1).query(c)?
            } else {
                Vec::new()
            };
            let rows = redis::cmd("XRANGE")
                .arg(&name)
                .arg(format!("{}-0", after_seq.saturating_add(1)))
                .arg("+")
                .arg("COUNT")
                .arg(limit)
                .query(c)?;
            Ok((head, rows))
        })?;
        if let Some((id, _)) = head.first()
            && seq_of(id)? < 1
        {
            return Err(StoreError::Corrupt(format!("{name}: entry with seq < 1")));
        }
        rows.iter().map(|entry| decode(&name, machine_id, entry)).collect()
    }

    fn purge_older_than(&self, cutoff_ts: f64) -> Result<u64> {
        if cutoff_ts.is_nan() || cutoff_ts == f64::NEG_INFINITY {
            return Err(StoreError::Other(format!(
                "cutoff_ts must not be NaN / -inf: {cutoff_ts}"
            )));
        }
        let pattern = format!("{}*", escape_glob(&self.keys.log("")));
        let names = self.pool.run(|c| scan_all(c, None, &pattern))?;
        let mut removed = 0;
        for name in names {
            removed += self.purge_stream(&name, cutoff_ts)?;
        }
        Ok(removed)
    }

    fn forget(&self, machine_id: &str) -> Result<u64> {
        let name = self.keys.log(machine_id);
        let (count, _): (u64, i64) = self.pool.run(|c| {
            redis::pipe().atomic().cmd("XLEN").arg(&name).cmd("DEL").arg(&name).query(c)
        })?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ids_are_seq_dash_zero() {
        assert_eq!(seq_of("17-0").unwrap(), 17);
        assert!(seq_of("17-1").is_err());
        assert!(seq_of("x-0").is_err());
        assert!(seq_of("17").is_err());
    }
}

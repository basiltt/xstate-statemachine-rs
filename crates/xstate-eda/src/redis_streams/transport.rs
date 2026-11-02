//! The four native operations over Redis Streams: `send`, `fetch`, `ack` and the crash-recovery
//! reclaim, plus the key and shard naming.
//!
//! Python reference: `contrib/brokers/redis_streams.py` (`RedisStreamsTransport`).
//!
//! ```text
//! {prefix}:stream:{topic}        the stream (shards = 1)
//! {prefix}:stream:{topic}:{n}    shard n of `shards`
//! ```
//!
//! Every entry carries two fields: `ce` (the structured-mode CloudEvents JSON) and `subject`.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use redis::{Connection, RedisResult, Value};

use super::pool::{Pool, SOCKET_TIMEOUT};
use crate::envelope::Envelope;
use crate::error::BrokerError;

/// A stream and an entry id: what settles one delivery.
pub(super) type Native = (String, String);

/// One message as the transport fetched it.
#[derive(Debug)]
pub(super) struct Raw {
    /// The structured-mode CloudEvents JSON.
    pub(super) body: Vec<u8>,
    /// Where to ack it.
    pub(super) native: Native,
    /// Deliveries the broker already made before this one (0 for a first delivery).
    pub(super) attempts: u64,
}

/// The transport's settings (the broker options after validation).
#[derive(Debug, Clone)]
pub(super) struct Settings {
    pub(super) prefix: String,
    pub(super) group: String,
    pub(super) consumer: String,
    pub(super) shards: usize,
    pub(super) min_idle_ms: u64,
    pub(super) maxlen: Option<u64>,
    pub(super) batch: usize,
    pub(super) max_bytes: usize,
}

#[derive(Debug, Default)]
struct Book {
    /// Streams whose consumer group exists.
    groups: HashSet<String>,
    /// Entries fetched by this transport and not yet settled: never reclaimed from ourselves.
    held: HashSet<Native>,
    /// `XPENDING` cursor per stream: the scan resumes where it stopped, so entries past a run
    /// of our own held ones are reached.
    cursor: HashMap<String, String>,
}

/// Redis Streams operations for one consumer.
#[derive(Debug)]
pub(super) struct Transport {
    pub(super) pool: Pool,
    pub(super) settings: Settings,
    book: Mutex<Book>,
}

/// CRC-32 (IEEE 802.3), the checksum of Python's `zlib.crc32`: subjects hash to the same shard
/// in every process and in both runtimes.
pub(super) fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in data {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

impl Transport {
    pub(super) fn new(pool: Pool, settings: Settings) -> Self {
        Self { pool, settings, book: Mutex::new(Book::default()) }
    }

    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Entries idle this long may be reclaimed by another consumer.
    pub(super) fn redelivery_window(&self) -> Duration {
        Duration::from_millis(self.settings.min_idle_ms)
    }

    // -- keys ---------------------------------------------------------------------------

    /// The stream key for `topic` (and `shard`).
    pub(super) fn stream(&self, topic: &str, shard: usize) -> String {
        let base = format!("{}:stream:{topic}", self.settings.prefix);
        if self.settings.shards == 1 { base } else { format!("{base}:{shard}") }
    }

    /// The shard a subject always hashes to (`crc32(subject) % shards`).
    pub(super) fn shard_of(&self, subject: Option<&str>) -> usize {
        if self.settings.shards == 1 {
            return 0;
        }
        let hash = crc32(subject.unwrap_or_default().as_bytes());
        usize::try_from(hash).map_or(0, |h| h % self.settings.shards)
    }

    fn streams(&self, topic: &str) -> Vec<String> {
        (0..self.settings.shards).map(|n| self.stream(topic, n)).collect()
    }

    // -- operations ---------------------------------------------------------------------

    /// Publish `envelope` on `topic`: `XADD stream [MAXLEN ~ n] * ce <json> subject <subject>`.
    pub(super) fn send(&self, topic: &str, envelope: &Envelope) -> Result<(), BrokerError> {
        let body = envelope.to_json(self.settings.max_bytes)?;
        let stream = self.stream(topic, self.shard_of(envelope.subject.as_deref()));
        let subject = envelope.subject.as_deref().unwrap_or_default();
        let result = self.pool.run(|conn| {
            let mut cmd = redis::cmd("XADD");
            cmd.arg(&stream);
            if let Some(maxlen) = self.settings.maxlen {
                cmd.arg("MAXLEN").arg("~").arg(maxlen);
            }
            cmd.arg("*").arg("ce").arg(body.as_bytes()).arg("subject").arg(subject);
            cmd.query::<String>(conn)
        });
        match result {
            Ok(_) => Ok(()),
            Err(BrokerError::Other(why)) => Err(BrokerError::Publish(why)),
            Err(other) => Err(other),
        }
    }

    /// Settle a delivery for good (`XACK`).
    pub(super) fn ack(&self, native: &Native) -> Result<(), BrokerError> {
        let (stream, id) = native;
        let result = self.pool.run(|conn| {
            redis::cmd("XACK").arg(stream).arg(&self.settings.group).arg(id).query::<i64>(conn)
        });
        self.book().held.remove(native);
        result.map(|_| ())
    }

    /// Release a delivery locally without acking it: another consumer may reclaim it.
    pub(super) fn forget(&self, native: &Native) {
        self.book().held.remove(native);
    }

    /// Pull what is ready on `topic`, waiting at most `wait` for new entries: first the entries
    /// a dead consumer left pending, else a blocking read of new ones.
    pub(super) fn fetch(&self, topic: &str, wait: Duration) -> Result<Vec<Raw>, BrokerError> {
        let streams = self.streams(topic);
        let mut out = Vec::new();
        for stream in &streams {
            self.ensure_group(stream)?;
            out.extend(self.reclaim(stream)?);
        }
        if out.is_empty() {
            out = self.read_new(&streams, wait)?;
        }
        self.book().held.extend(out.iter().map(|r| r.native.clone()));
        Ok(out)
    }

    fn ensure_group(&self, stream: &str) -> Result<(), BrokerError> {
        if self.book().groups.contains(stream) {
            return Ok(());
        }
        let created = self.pool.run(|conn| {
            redis::cmd("XGROUP")
                .arg("CREATE")
                .arg(stream)
                .arg(&self.settings.group)
                .arg("0")
                .arg("MKSTREAM")
                .query::<()>(conn)
        });
        match created {
            // The group already exists: another consumer created it first.
            Err(BrokerError::Other(why)) if !why.contains("BUSYGROUP") => {
                return Err(BrokerError::Other(why));
            }
            Err(BrokerError::Other(_)) | Ok(()) => {}
            Err(other) => return Err(other),
        }
        self.book().groups.insert(stream.to_owned());
        Ok(())
    }

    fn read_new(&self, streams: &[String], wait: Duration) -> Result<Vec<Raw>, BrokerError> {
        let block_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        let reply = self.pool.run(|conn| {
            // A blocking read may legitimately outlast the socket timeout.
            conn.set_read_timeout(Some(wait + SOCKET_TIMEOUT))?;
            let mut cmd = redis::cmd("XREADGROUP");
            cmd.arg("GROUP").arg(&self.settings.group).arg(&self.settings.consumer);
            cmd.arg("COUNT").arg(self.settings.batch);
            // A BLOCK of 0 means "forever" to Redis: a zero wait is sent as no BLOCK at all.
            if block_ms > 0 {
                cmd.arg("BLOCK").arg(block_ms);
            }
            cmd.arg("STREAMS");
            for stream in streams {
                cmd.arg(stream);
            }
            for _ in streams {
                cmd.arg(">");
            }
            let reply = cmd.query::<Value>(conn);
            conn.set_read_timeout(Some(SOCKET_TIMEOUT))?;
            reply
        })?;
        let mut out = Vec::new();
        for (stream, entries) in read_reply(&reply) {
            for entry in entries {
                if let Some(fields) = entry.fields {
                    out.push(raw_of(&stream, entry.id, fields, 0));
                }
            }
        }
        Ok(out)
    }

    /// Claim entries idle for at least `min_idle_ms` (a dead consumer's pending list).
    ///
    /// One `XPENDING` lists the candidates with their delivery counts; entries this transport
    /// still holds are excluded before claiming (a claim bumps the delivery count: re-claiming
    /// our own would inflate attempts); one `XCLAIM` takes the rest. The scan resumes from a
    /// per-stream cursor.
    fn reclaim(&self, stream: &str) -> Result<Vec<Raw>, BrokerError> {
        let start = self.book().cursor.get(stream).cloned().unwrap_or_else(|| "-".to_owned());
        let pending: Vec<(String, String, u64, u64)> = self.pool.run(|conn| {
            redis::cmd("XPENDING")
                .arg(stream)
                .arg(&self.settings.group)
                .arg(&start)
                .arg("+")
                .arg(self.settings.batch * 4)
                .query(conn)
        })?;
        let Some((last, ..)) = pending.last() else {
            self.book().cursor.remove(stream); // wrapped: rescan from the top
            return Ok(Vec::new());
        };
        self.book().cursor.insert(stream.to_owned(), format!("({last}"));
        let counts = self.claimable(stream, &pending);
        if counts.is_empty() {
            return Ok(Vec::new());
        }
        let reply = self.pool.run(|conn| claim(conn, stream, &self.settings, &counts))?;
        let mut out = Vec::new();
        for entry in entries_of(&reply) {
            match entry.fields {
                Some(fields) => {
                    // the claim itself was a delivery: attempts = the count before it
                    let attempts = counts.iter().find(|(id, _)| *id == entry.id).map_or(0, |c| c.1);
                    out.push(raw_of(stream, entry.id, fields, attempts));
                }
                // deleted while pending: nothing to deliver
                None => self.ack(&(stream.to_owned(), entry.id))?,
            }
        }
        Ok(out)
    }

    /// `(id, deliveries so far)` of the pending entries another consumer may claim.
    fn claimable(
        &self,
        stream: &str,
        pending: &[(String, String, u64, u64)],
    ) -> Vec<(String, u64)> {
        let book = self.book();
        pending
            .iter()
            .filter(|(id, _, idle_ms, _)| {
                // idle is filtered here, not with XPENDING IDLE: identical on every Redis >= 5
                !book.held.contains(&(stream.to_owned(), id.clone()))
                    && *idle_ms >= self.settings.min_idle_ms
            })
            .map(|(id, _, _, delivered)| (id.clone(), *delivered))
            .take(self.settings.batch)
            .collect()
    }
}

fn claim(
    conn: &mut Connection,
    stream: &str,
    settings: &Settings,
    counts: &[(String, u64)],
) -> RedisResult<Value> {
    let mut cmd = redis::cmd("XCLAIM");
    cmd.arg(stream).arg(&settings.group).arg(&settings.consumer).arg(settings.min_idle_ms);
    for (id, _) in counts {
        cmd.arg(id);
    }
    cmd.query(conn)
}

fn raw_of(stream: &str, id: String, mut fields: HashMap<String, Vec<u8>>, attempts: u64) -> Raw {
    Raw { body: fields.remove("ce").unwrap_or_default(), native: (stream.to_owned(), id), attempts }
}

// -- reply parsing ----------------------------------------------------------------------

/// One stream entry; `fields` is `None` for an entry deleted while pending.
#[derive(Debug)]
struct Entry {
    id: String,
    fields: Option<HashMap<String, Vec<u8>>>,
}

fn bytes(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.clone().into_bytes()),
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    String::from_utf8(bytes(value)?).ok()
}

fn fields_of(value: &Value) -> Option<HashMap<String, Vec<u8>>> {
    let Value::Array(items) = value else { return None };
    let mut out = HashMap::new();
    for pair in items.chunks(2) {
        if let [name, content] = pair {
            out.insert(text(name)?, bytes(content)?);
        }
    }
    Some(out)
}

/// `[[id, [field, value, ...] | nil], ...]` (the shape of `XCLAIM` and the inner part of
/// `XREADGROUP`).
fn entries_of(value: &Value) -> Vec<Entry> {
    let Value::Array(items) = value else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let Value::Array(parts) = item else { return None };
            let id = text(parts.first()?)?;
            Some(Entry { id, fields: parts.get(1).and_then(fields_of) })
        })
        .collect()
}

/// `[[stream, entries], ...]` or nil when the block timed out.
fn read_reply(value: &Value) -> Vec<(String, Vec<Entry>)> {
    let Value::Array(streams) = value else { return Vec::new() };
    streams
        .iter()
        .filter_map(|item| {
            let Value::Array(parts) = item else { return None };
            Some((text(parts.first()?)?, entries_of(parts.get(1)?)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_is_zlibs() {
        // zlib.crc32(b"123456789") == 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn entries_parse_with_and_without_fields() {
        let reply = Value::Array(vec![
            Value::Array(vec![
                Value::BulkString(b"1-0".to_vec()),
                Value::Array(vec![
                    Value::BulkString(b"ce".to_vec()),
                    Value::BulkString(b"{}".to_vec()),
                ]),
            ]),
            Value::Array(vec![Value::BulkString(b"2-0".to_vec()), Value::Nil]),
        ]);
        let entries = entries_of(&reply);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].fields.as_ref().and_then(|f| f.get("ce")), Some(&b"{}".to_vec()));
        assert!(entries[1].fields.is_none());
        assert!(read_reply(&Value::Nil).is_empty());
    }
}

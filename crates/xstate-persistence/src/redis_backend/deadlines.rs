//! Deadline encoding and the `due_keys` read path of the Redis store.
//!
//! Python reference: `contrib/redis/_layout.py` (`_dl_member`, `_member_key`, `_dl_args`,
//! `_parse_deadlines`, `_due_rows`) and `store.py` (`due_keys`).

use std::collections::{HashMap, HashSet};

use redis::Connection;
use serde_json::{Map, Value, json};
use xstate_core::snapshot::Deadline;

use super::conn::Pool;
use super::keys::Keys;
use super::scripts::PRUNE;
use crate::memory_store::deadline_from_value;
use crate::receipts::pyjson;
use crate::store::Result;

/// The zset member and `dl` hash field of one deadline: the JSON array
/// `["key","state_id",entry_seq,"event_type"]`, compact and not ASCII-escaped, exactly as Python's
/// `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` writes it. Unambiguous for any key
/// or state id, unlike the layout-1 `"{key}|{field}"`.
pub(crate) fn member(key: &str, d: &Deadline) -> String {
    json!([key, d.state_id, d.entry_seq, d.event_type]).to_string()
}

/// The key a zset member belongs to: the first element of a layout-2 JSON array, else the part
/// before the first `|` of a layout-1 member.
pub(crate) fn member_key(member: &str) -> String {
    if member.starts_with('[')
        && let Ok(Value::Array(items)) = serde_json::from_str::<Value>(member)
        && let Some(Value::String(key)) = items.first()
    {
        return key.clone();
    }
    member.split_once('|').map_or(member, |(key, _)| key).to_owned()
}

/// The record body stored in the `dl` hash: `json.dumps(Deadline.to_dict())`, fields in
/// declaration order with the default `", "` / `": "` separators.
fn body(d: &Deadline) -> String {
    let mut map = Map::new();
    map.insert("state_id".into(), Value::String(d.state_id.clone()));
    map.insert("entry_seq".into(), Value::from(d.entry_seq));
    map.insert("due_at_wall".into(), Value::from(d.due_at_wall));
    map.insert("delay_ms".into(), d.delay_ms.clone());
    map.insert("event_type".into(), Value::String(d.event_type.clone()));
    pyjson::dumps(&Value::Object(map), false, false)
}

/// The `(member, due_at_wall, body)` script arguments of every deadline, flattened.
pub(crate) fn script_args(key: &str, deadlines: &[Deadline]) -> Vec<String> {
    let mut out = Vec::with_capacity(deadlines.len() * 3);
    for d in deadlines {
        out.push(member(key, d));
        out.push(format!("{:?}", d.due_at_wall));
        out.push(body(d));
    }
    out
}

/// Decode a `dl` hash. A value that is not a valid deadline record is skipped, as Python does;
/// the result is ordered by `(due_at_wall, state_id)`.
pub(crate) fn parse_all(rows: &HashMap<String, String>) -> Vec<Deadline> {
    let mut out: Vec<Deadline> = rows
        .values()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter_map(|rec| deadline_from_value(&rec).ok())
        .collect();
    out.sort_by(|a, b| a.due_at_wall.total_cmp(&b.due_at_wall).then(a.state_id.cmp(&b.state_id)));
    out
}

/// Drop zset members whose snapshot hash is gone (a `ttl_s` expiry leaves the member behind: Redis
/// cannot expire a zset member) and return them.
fn prune_orphans(
    conn: &mut Connection,
    keys: &Keys,
    members: &[String],
) -> redis::RedisResult<Vec<String>> {
    if members.is_empty() {
        return Ok(Vec::new());
    }
    let mut call = PRUNE.prepare_invoke();
    call.key(keys.deadlines());
    for m in members {
        call.arg(m.as_str()).arg(keys.snap(&member_key(m)));
    }
    call.invoke(conn)
}

/// `(key, due_at)` rows, soonest first, one per key, minus the pruned members.
fn due_rows(rows: &[(String, f64)], gone: &[String]) -> Vec<String> {
    let dead: HashSet<&str> = gone.iter().map(String::as_str).collect();
    let mut seen: HashMap<String, f64> = HashMap::new();
    for (m, score) in rows {
        if !dead.contains(m.as_str()) {
            seen.entry(member_key(m)).or_insert(*score);
        }
    }
    let mut sorted: Vec<(String, f64)> = seen.into_iter().collect();
    sorted.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    sorted.into_iter().map(|(key, _)| key).collect()
}

/// Keys with a deadline due at or before `until_wall`, soonest first, read from the zset index.
/// Orphaned members met on the way are pruned atomically and not reported; the read repeats
/// while it pruned a full page, so orphans can never starve live keys.
pub(crate) fn due_keys(
    pool: &Pool,
    keys: &Keys,
    until_wall: f64,
    limit: usize,
) -> Result<Vec<String>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    loop {
        let (rows, gone) = pool.run(|conn| {
            let rows: Vec<(String, f64)> = redis::cmd("ZRANGEBYSCORE")
                .arg(keys.deadlines())
                .arg("-inf")
                .arg(until_wall)
                .arg("WITHSCORES")
                .arg("LIMIT")
                .arg(0)
                .arg(limit)
                .query(conn)?;
            let members: Vec<String> = rows.iter().map(|(m, _)| m.clone()).collect();
            let gone = prune_orphans(conn, keys, &members)?;
            Ok((rows, gone))
        })?;
        if gone.is_empty() || rows.len() < limit {
            return Ok(due_rows(&rows, &gone));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deadline(state: &str, seq: u64, due: f64) -> Deadline {
        Deadline {
            state_id: state.to_owned(),
            entry_seq: seq,
            due_at_wall: due,
            delay_ms: json!(1500),
            event_type: format!("after.1500.{state}"),
        }
    }

    #[test]
    fn member_is_the_python_json_array() {
        let d = deadline("m.a", 3, 1.0);
        assert_eq!(member("order:1", &d), r#"["order:1","m.a",3,"after.1500.m.a"]"#);
        assert_eq!(member("ключ|x", &d), r#"["ключ|x","m.a",3,"after.1500.m.a"]"#);
    }

    #[test]
    fn member_key_reads_both_layouts() {
        assert_eq!(member_key(r#"["a|b","s",1,"e"]"#), "a|b");
        assert_eq!(member_key("a|m.s|3|ev"), "a");
        assert_eq!(member_key("[not json|x"), "[not json");
        assert_eq!(member_key("plain"), "plain");
    }

    #[test]
    fn body_matches_python_json_dumps() {
        assert_eq!(
            body(&deadline("m.a", 3, 1_700_000_000.5)),
            r#"{"state_id": "m.a", "entry_seq": 3, "due_at_wall": 1700000000.5, "delay_ms": 1500, "event_type": "after.1500.m.a"}"#
        );
    }

    #[test]
    fn due_rows_dedupe_and_skip_pruned() {
        let rows = vec![
            (r#"["b","s",1,"e"]"#.to_owned(), 5.0),
            (r#"["a","s",1,"e"]"#.to_owned(), 2.0),
            (r#"["a","t",2,"e"]"#.to_owned(), 9.0),
            (r#"["c","s",1,"e"]"#.to_owned(), 1.0),
        ];
        let gone = vec![r#"["c","s",1,"e"]"#.to_owned()];
        assert_eq!(due_rows(&rows, &gone), ["a", "b"]);
    }
}

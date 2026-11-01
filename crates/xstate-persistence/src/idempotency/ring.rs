//! The in-snapshot "processed ring": the last 64 `scope|key` ids and when / with what
//! fingerprint they were processed, kept inside the instance context so they travel with the
//! snapshot.

use serde_json::{Map, Value};

use super::{PROCESSED_AT_KEY, PROCESSED_IDS_KEY, PROCESSED_RING_SIZE};

/// The processed `scope|key` ids recorded in a context, oldest first (empty when the context
/// is not an object or the ring is malformed).
#[must_use]
pub fn processed_ids(context: &Value) -> Vec<String> {
    context
        .get(PROCESSED_IDS_KEY)
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// `true` when `ring_id` (`scope|key`) is in the context's processed ring.
#[must_use]
pub fn ring_contains(context: &Value, ring_id: &str) -> bool {
    processed_ids(context).iter().any(|id| id == ring_id)
}

/// Record a processed key in the context: append `scope|key` to the ring (last
/// [`PROCESSED_RING_SIZE`] kept) and store when and with which fingerprint it was processed,
/// for the ids still in the ring only. Does nothing when the context is not a JSON object.
///
/// Call it after the step settled and **before** the snapshot is taken, so the evidence travels
/// inside the snapshot.
pub fn record_processed(
    context: &mut Value,
    scope: &str,
    key: &str,
    fingerprint: &str,
    now_wall: f64,
) {
    let ring_id = format!("{scope}|{key}");
    let mut ring = processed_ids(context);
    let Some(object) = context.as_object_mut() else { return };
    ring.push(ring_id.clone());
    if ring.len() > PROCESSED_RING_SIZE {
        ring.drain(..ring.len() - PROCESSED_RING_SIZE);
    }
    let old = object.get(PROCESSED_AT_KEY).and_then(Value::as_object).cloned().unwrap_or_default();
    let mut at = Map::new();
    for id in &ring {
        if let Some(evidence) = old.get(id) {
            at.insert(id.clone(), evidence.clone());
        }
    }
    at.insert(ring_id, Value::Array(vec![Value::from(now_wall), Value::from(fingerprint)]));
    object.insert(
        PROCESSED_IDS_KEY.into(),
        Value::Array(ring.into_iter().map(Value::String).collect()),
    );
    object.insert(PROCESSED_AT_KEY.into(), Value::Object(at));
}

/// The fingerprint `ring_id` was processed with, if the snapshot says it was processed and the
/// evidence is younger than `ttl_s` (a genuine TTL expiry returns `None`). A ring entry without
/// evidence (a pre-evidence snapshot) also returns `None`.
#[must_use]
pub fn ring_evidence(
    context: &Value,
    ring_id: &str,
    ttl_s: Option<f64>,
    now_wall: f64,
) -> Option<String> {
    if !ring_contains(context, ring_id) {
        return None;
    }
    let record = context.get(PROCESSED_AT_KEY)?.get(ring_id)?.as_array()?;
    let [when, evidence] = record.as_slice() else { return None };
    let when = when.as_f64()?;
    if ttl_s.is_some_and(|ttl| when + ttl <= now_wall) {
        return None;
    }
    Some(match evidence {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })
}

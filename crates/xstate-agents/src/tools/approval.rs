//! Human approval: which calls are pending, whether a `HUMAN_APPROVED` names exactly them,
//! and the resume events a reviewer's decision becomes.
//!
//! Approval is per CALL ID of what is pending right now: a late or replayed
//! `HUMAN_APPROVED` for an earlier batch cannot approve a later one, and approvals never
//! outlive a batch (`recordModelResponse` / `appendToolResults` reset them).

use serde_json::{Value, json};
use xstate_core::Event;

use crate::pyfmt::py_str;

/// Event a reviewer sends to approve the pending batch.
pub const HUMAN_APPROVED: &str = "HUMAN_APPROVED";
/// Event a reviewer sends to reject the pending batch.
pub const HUMAN_REJECTED: &str = "HUMAN_REJECTED";

fn pending(ctx: &Value) -> &[Value] {
    ctx.get("pending_tool_calls").and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

/// The calls a reviewer is being asked to approve: `[{"id", "name", "arguments"}]`.
#[must_use]
pub fn pending_approval(ctx: &Value) -> Vec<Value> {
    pending(ctx).to_vec()
}

/// Ids of the pending calls, as text.
#[must_use]
pub fn pending_ids(ctx: &Value) -> Vec<String> {
    pending(ctx).iter().map(|c| c.get("id").map(py_str).unwrap_or_default()).collect()
}

/// `true` when `payload.call_ids` names EXACTLY the pending call ids (order-insensitive).
#[must_use]
pub fn approval_matches(ctx: &Value, payload: &Value) -> bool {
    let Some(ids) = payload.get("call_ids").and_then(Value::as_array) else { return false };
    let mut named: Vec<String> = ids.iter().map(py_str).collect();
    let mut want = pending_ids(ctx);
    named.sort();
    want.sort();
    named == want
}

/// The event a reviewer's decision becomes: `HUMAN_APPROVED` naming the currently pending
/// call ids (what the reviewer was shown), or `HUMAN_REJECTED`.
#[must_use]
pub fn decision_event(ctx: &Value, approve: bool) -> Event {
    if approve {
        Event::with_payload(HUMAN_APPROVED, json!({"call_ids": pending_ids(ctx)}))
    } else {
        Event::new(HUMAN_REJECTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_name_exactly_the_pending_batch() {
        let ctx =
            json!({"pending_tool_calls": [{"id": "b", "name": "x"}, {"id": "a", "name": "y"}]});
        assert!(approval_matches(&ctx, &json!({"call_ids": ["a", "b"]})));
        assert!(!approval_matches(&ctx, &json!({"call_ids": ["a"]})));
        assert!(!approval_matches(&ctx, &json!({"call_ids": "a"})));
        assert!(!approval_matches(&ctx, &Value::Null));
        assert_eq!(pending_approval(&ctx).len(), 2);
        assert_eq!(decision_event(&ctx, true).payload, json!({"call_ids": ["b", "a"]}));
        assert_eq!(decision_event(&ctx, false).ty.as_str(), HUMAN_REJECTED);
        assert_eq!(pending_ids(&json!({})), Vec::<String>::new());
    }
}

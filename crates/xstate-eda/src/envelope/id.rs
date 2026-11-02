//! Sortable ids: a 48-bit millisecond timestamp plus 74 random bits in the `UUIDv7` layout,
//! monotonic within one process (Python `new_id`).

use std::sync::{Mutex, PoisonError};

/// Mask of the 74-bit random tail.
const TAIL_MASK: u128 = (1 << 74) - 1;

/// `(last millisecond, last tail)`, so ids minted in the same millisecond keep increasing.
static LAST: Mutex<(u64, u128)> = Mutex::new((0, 0));

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A UUIDv7-style id, lexicographically sortable by creation time.
///
/// Within one millisecond the random tail is incremented, so ids minted by one process are
/// strictly increasing (the Python algorithm, bit for bit).
///
/// ```
/// let a = xstate_eda::new_id();
/// let b = xstate_eda::new_id();
/// assert!(a < b);
/// assert_eq!(&a[14..15], "7");
/// ```
#[must_use]
pub fn new_id() -> String {
    new_id_at(None)
}

/// [`new_id`] for a given millisecond timestamp (`None` = now).
#[must_use]
pub fn new_id_at(now_ms_override: Option<u64>) -> String {
    let mut ms = now_ms_override.unwrap_or_else(now_ms);
    let tail = {
        let mut last = LAST.lock().unwrap_or_else(PoisonError::into_inner);
        let tail = if ms <= last.0 {
            ms = last.0;
            last.1 + 1
        } else {
            rand::random::<u128>() >> 54
        } & TAIL_MASK;
        *last = (ms, tail);
        tail
    };
    let rand_a = tail >> 62;
    let rand_b = tail & ((1 << 62) - 1);
    let value = ((u128::from(ms) & ((1 << 48) - 1)) << 80)
        | (0x7 << 76)
        | (rand_a << 64)
        | (0b10 << 62)
        | rand_b;
    let h = format!("{value:032x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_v7_shaped_and_monotonic() {
        let ids: Vec<String> = (0..200).map(|_| new_id_at(Some(1_700_000_000_000))).collect();
        for pair in ids.windows(2) {
            assert!(pair[0] < pair[1], "{pair:?}");
        }
        let id = &ids[0];
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        // a timestamp in the past reuses the last millisecond: still increasing
        let earlier = new_id_at(Some(1));
        assert!(earlier > ids[199]);
    }
}

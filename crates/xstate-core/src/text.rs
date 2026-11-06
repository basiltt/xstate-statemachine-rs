//! Small string helpers for the hot paths.

/// Names up to this length are compared inline.
const INLINE_EQ_MAX: usize = 16;

/// Equality of two names. Event types and implementation names are short: comparing their
/// bytes inline beats a call into the platform `memcmp`, which is what `==` on strings of
/// unknown length compiles to.
#[inline]
pub(crate) fn name_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    if a.len() > INLINE_EQ_MAX {
        return a == b;
    }
    a.iter().zip(b).all(|(x, y)| x == y)
}

#[cfg(test)]
mod tests {
    use super::name_eq;

    #[test]
    fn compares_short_and_long_names() {
        assert!(name_eq("", ""));
        assert!(name_eq("NEXT", "NEXT"));
        assert!(!name_eq("NEXT", "NEXU"));
        assert!(!name_eq("NEXT", "NEX"));
        let long = "a.rather.long.event.name";
        let parts = ["a.rather.long", ".event.name"].concat();
        assert!(name_eq(long, &parts));
        assert!(!name_eq(long, "a.rather.long.event.namf"));
    }
}

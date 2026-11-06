//! A name → implementation table tuned for the handful of names a machine uses.

use foldhash::HashMap;

/// Above this many entries a hash index backs the lookups; below, a scan comparing lengths
/// first is faster than hashing the name, and an empty registry costs nothing to create.
const SCAN_LIMIT: usize = 16;

/// Implementations by name, in insertion order. Inserting an existing name replaces it.
pub(crate) struct Registry<V> {
    entries: Vec<(String, V)>,
    index: Option<HashMap<String, usize>>,
}

impl<V> Default for Registry<V> {
    fn default() -> Self {
        Self { entries: Vec::new(), index: None }
    }
}

impl<V: Clone> Clone for Registry<V> {
    fn clone(&self) -> Self {
        Self { entries: self.entries.clone(), index: self.index.clone() }
    }
}

impl<V> Registry<V> {
    /// Add `value` under `name`, replacing a previous entry of that name.
    pub(crate) fn insert(&mut self, name: String, value: V) {
        if let Some(i) = self.position(&name) {
            self.entries[i].1 = value;
            return;
        }
        let at = self.entries.len();
        if let Some(index) = &mut self.index {
            index.insert(name.clone(), at);
        }
        self.entries.push((name, value));
        if self.index.is_none() && self.entries.len() > SCAN_LIMIT {
            let index = self.entries.iter().enumerate().map(|(i, (k, _))| (k.clone(), i));
            self.index = Some(index.collect());
        }
    }

    /// The implementation registered under `name`.
    #[inline]
    pub(crate) fn get(&self, name: &str) -> Option<&V> {
        self.position(name).map(|i| &self.entries[i].1)
    }

    #[inline]
    fn position(&self, name: &str) -> Option<usize> {
        match &self.index {
            Some(index) => index.get(name).copied(),
            None => self.entries.iter().position(|(k, _)| k.len() == name.len() && k == name),
        }
    }

    /// The registered names, in insertion order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> + '_ {
        self.entries.iter().map(|(k, _)| k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_and_finds_on_both_sides_of_the_scan_limit() {
        let mut r = Registry::default();
        for i in 0..40 {
            r.insert(format!("n{i}"), i);
            r.insert("n0".to_owned(), 100 + i);
            assert_eq!(r.get("n0"), Some(&(100 + i)));
            assert_eq!(r.get(&format!("n{i}")), Some(&if i == 0 { 100 } else { i }));
            assert_eq!(r.get("missing"), None);
        }
        assert_eq!(r.keys().count(), 40);
    }
}

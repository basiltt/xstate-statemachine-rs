//! A small fixed-capacity bitset over state indices, stored inline for machines of up to 128
//! states so that starting an interpreter does not allocate for its configuration.

use std::fmt;

use smallvec::SmallVec;

use crate::ids::StateId;

const WORD: usize = u64::BITS as usize;

/// Set of state indices in `0..capacity`, iterated in ascending (document) order.
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct StateBits {
    words: SmallVec<[u64; 2]>,
}

impl StateBits {
    /// An empty set able to hold indices below `capacity`.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { words: smallvec::smallvec![0; capacity.div_ceil(WORD)] }
    }

    /// `true` when `index` is a member.
    #[inline]
    pub(crate) fn contains(&self, index: usize) -> bool {
        self.words.get(index / WORD).is_some_and(|w| (w >> (index % WORD)) & 1 == 1)
    }

    /// Add or remove `index` (ignored when out of capacity, like a missing state).
    #[inline]
    pub(crate) fn set(&mut self, index: usize, member: bool) {
        if let Some(w) = self.words.get_mut(index / WORD) {
            let bit = 1u64 << (index % WORD);
            if member {
                *w |= bit;
            } else {
                *w &= !bit;
            }
        }
    }

    /// Add every member of `other` (whose capacity is the same).
    pub(crate) fn union_with(&mut self, other: &StateBits) {
        for (w, o) in self.words.iter_mut().zip(other.words.iter()) {
            *w |= o;
        }
    }

    /// The largest member.
    #[inline]
    pub(crate) fn last(&self) -> Option<usize> {
        let (i, w) = self.words.iter().enumerate().rev().find(|(_, w)| **w != 0)?;
        Some(i * WORD + (WORD - 1 - w.leading_zeros() as usize))
    }

    /// Members in ascending order.
    #[inline]
    pub(crate) fn ones(&self) -> Ones<'_> {
        Ones {
            words: &self.words,
            word: 0,
            bits: self.words.first().copied().unwrap_or(0),
            end: usize::MAX,
        }
    }

    /// Members in `start..end`, ascending.
    #[inline]
    pub(crate) fn ones_in(&self, start: usize, end: usize) -> Ones<'_> {
        let word = start / WORD;
        let bits = self.words.get(word).map_or(0, |w| w & (u64::MAX << (start % WORD)));
        Ones { words: &self.words, word, bits, end }
    }

    /// Members as state ids, ascending.
    #[inline]
    pub(crate) fn states(&self) -> impl Iterator<Item = StateId> + '_ {
        self.ones().map(StateId::from_index)
    }

    /// Append the members of `self` that are also in `mask`, as state ids, ascending.
    #[inline]
    pub(crate) fn states_masked_into<A: smallvec::Array<Item = StateId>>(
        &self,
        mask: &StateBits,
        out: &mut SmallVec<A>,
    ) {
        for (i, (a, m)) in self.words.iter().zip(mask.words.iter()).enumerate() {
            let mut bits = a & m;
            while bits != 0 {
                out.push(StateId::from_index(i * WORD + bits.trailing_zeros() as usize));
                bits &= bits - 1;
            }
        }
    }
}

impl fmt::Debug for StateBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.ones()).finish()
    }
}

/// Iterator over the members of a [`StateBits`].
pub(crate) struct Ones<'a> {
    words: &'a [u64],
    word: usize,
    bits: u64,
    end: usize,
}

impl Iterator for Ones<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        while self.bits == 0 {
            self.word += 1;
            self.bits = *self.words.get(self.word)?;
        }
        let index = self.word * WORD + self.bits.trailing_zeros() as usize;
        if index >= self.end {
            self.bits = 0;
            self.word = self.words.len();
            return None;
        }
        self.bits &= self.bits - 1;
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_and_masks() {
        let mut b = StateBits::with_capacity(130);
        for i in [0, 3, 63, 64, 70, 129] {
            b.set(i, true);
        }
        b.set(3, false);
        assert_eq!(b.ones().collect::<Vec<_>>(), [0, 63, 64, 70, 129]);
        assert_eq!(b.ones_in(1, 70).collect::<Vec<_>>(), [63, 64]);
        assert_eq!(b.ones_in(64, 200).collect::<Vec<_>>(), [64, 70, 129]);
        assert!(b.contains(129) && !b.contains(3) && !b.contains(500));
        assert_eq!(b.last(), Some(129));
        assert_eq!(StateBits::with_capacity(70).last(), None);
        let mut m = StateBits::with_capacity(130);
        m.set(64, true);
        m.set(129, true);
        let mut masked: SmallVec<[StateId; 4]> = SmallVec::new();
        b.states_masked_into(&m, &mut masked);
        assert_eq!(masked.iter().map(|s| s.index()).collect::<Vec<_>>(), [64, 129]);
    }
}

//! The free ranges of one heap of the texture pool (`pool`): best fit, and
//! a range given back merges with its free neighbours.
//!
//! Every size and offset of one heap is a multiple of its class's
//! placement alignment, so a fit never needs padding. A buddy allocator
//! would round a 1.4 MB BC7 texture up to 2 MiB (43 % lost); TLSF fits
//! about as well as best fit, which is simpler at our counts (hundreds of
//! ranges a heap, a few hundred placements a second).

use std::collections::{BTreeMap, BTreeSet};

/// Free ranges of a heap of `size` bytes.
#[derive(Debug)]
pub(super) struct Ranges {
    size: u64,
    /// Free ranges: offset to length.
    by_offset: BTreeMap<u64, u64>,
    /// The same, by length then offset (the best fit is the first).
    by_size: BTreeSet<(u64, u64)>,
    free: u64,
}

impl Ranges {
    /// All of `size` bytes free.
    pub(super) fn new(size: u64) -> Ranges {
        let mut ranges = Ranges {
            size,
            by_offset: BTreeMap::new(),
            by_size: BTreeSet::new(),
            free: 0,
        };
        if size > 0 {
            ranges.insert(0, size);
        }
        ranges
    }

    fn insert(&mut self, offset: u64, len: u64) {
        self.by_offset.insert(offset, len);
        self.by_size.insert((len, offset));
        self.free += len;
    }

    fn remove(&mut self, offset: u64, len: u64) {
        self.by_offset.remove(&offset);
        self.by_size.remove(&(len, offset));
        self.free -= len;
    }

    /// The offset of `len` bytes taken from the smallest free range that
    /// holds them (the lowest offset among equals); `None` if none does.
    pub(super) fn take(&mut self, len: u64) -> Option<u64> {
        if len == 0 {
            return None;
        }
        let &(have, offset) = self.by_size.range((len, 0)..).next()?;
        self.remove(offset, have);
        if have > len {
            self.insert(offset + len, have - len);
        }
        Some(offset)
    }

    /// Gives back `len` bytes at `offset`, taken before; merged with the
    /// free ranges right before and after it.
    pub(super) fn give(&mut self, offset: u64, len: u64) {
        debug_assert!(offset + len <= self.size, "{offset} + {len} past {}", self.size);
        let (mut start, mut end) = (offset, offset + len);
        if let Some((&before, &n)) = self.by_offset.range(..offset).next_back()
            && before + n == offset
        {
            self.remove(before, n);
            start = before;
        }
        debug_assert!(
            self.by_offset
                .range(..offset)
                .next_back()
                .is_none_or(|(&o, &n)| o + n <= offset),
            "given back twice"
        );
        if let Some(&n) = self.by_offset.get(&end) {
            self.remove(end, n);
            end += n;
        }
        self.insert(start, end - start);
    }

    /// The length of the free range [`take`](Self::take) would cut `len`
    /// bytes from, if one holds them.
    pub(super) fn fit(&self, len: u64) -> Option<u64> {
        (len > 0)
            .then(|| self.by_size.range((len, 0)..).next().map(|&(have, _)| have))
            .flatten()
    }

    /// Free bytes.
    pub(super) fn free(&self) -> u64 {
        self.free
    }

    /// The longest free range.
    #[cfg(test)]
    pub(super) fn largest(&self) -> u64 {
        self.by_size.last().map_or(0, |&(len, _)| len)
    }

    /// Whether nothing is taken.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.free == self.size
    }

    /// How many free ranges there are (fragmentation, for the tests).
    #[cfg(test)]
    pub(super) fn pieces(&self) -> usize {
        self.by_offset.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 64 << 10;

    #[test]
    fn takes_the_best_fit_and_merges_what_comes_back() {
        let mut r = Ranges::new(16 * K);
        let a = r.take(4 * K).unwrap();
        let b = r.take(2 * K).unwrap();
        let c = r.take(4 * K).unwrap();
        assert_eq!((a, b, c), (0, 4 * K, 6 * K));
        assert_eq!(r.free(), 6 * K);
        // A hole of 2 between a and c: a request of 2 goes there, not at
        // the end (best fit).
        r.give(b, 2 * K);
        assert_eq!(r.pieces(), 2);
        assert_eq!(r.take(2 * K), Some(b));
        r.give(b, 2 * K);
        // Freeing a and c merges everything back into one range.
        r.give(a, 4 * K);
        assert_eq!(r.pieces(), 2, "a and b merged, then the end");
        r.give(c, 4 * K);
        assert_eq!(r.pieces(), 1);
        assert!(r.is_empty());
        assert_eq!(r.largest(), 16 * K);
    }

    #[test]
    fn refuses_what_does_not_fit() {
        let mut r = Ranges::new(4 * K);
        assert_eq!(r.take(5 * K), None);
        assert_eq!(r.take(0), None);
        assert_eq!(r.take(4 * K), Some(0));
        assert_eq!(r.take(K), None);
        assert_eq!(r.largest(), 0);
        assert!(Ranges::new(0).take(K).is_none());
    }

    #[test]
    fn a_range_given_back_is_reused() {
        let mut r = Ranges::new(8 * K);
        let a = r.take(3 * K).unwrap();
        let _b = r.take(5 * K).unwrap();
        r.give(a, 3 * K);
        assert_eq!(r.take(3 * K), Some(a), "the same place");
    }

    /// Against a model (one flag a unit), with a fixed pseudo-random mix of
    /// takes and gives: never two ranges over one unit, the free count
    /// right, and free neighbours always merged.
    #[test]
    fn matches_a_model_under_a_random_mix() {
        const UNITS: u64 = 256;
        let mut r = Ranges::new(UNITS * K);
        let mut used = [false; UNITS as usize];
        let mut held: Vec<(u64, u64)> = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            if next() % 3 != 0 || held.is_empty() {
                let len = (next() % 12 + 1) * K;
                if let Some(at) = r.take(len) {
                    assert_eq!(at % K, 0);
                    for u in at / K..(at + len) / K {
                        assert!(!used[u as usize], "unit {u} handed out twice");
                        used[u as usize] = true;
                    }
                    held.push((at, len));
                } else {
                    // Refused only when no free run of the model holds it.
                    let longest = used
                        .split(|&u| u)
                        .map(|run| run.len() as u64)
                        .max()
                        .unwrap_or(0);
                    assert!(longest * K < len, "refused {len} with {longest} units free in a row");
                }
            } else {
                let i = (next() % held.len() as u64) as usize;
                let (at, len) = held.swap_remove(i);
                r.give(at, len);
                for u in at / K..(at + len) / K {
                    used[u as usize] = false;
                }
            }
            let free = used.iter().filter(|&&u| !u).count() as u64 * K;
            assert_eq!(r.free(), free);
            let runs = used
                .split(|&u| u)
                .filter(|run| !run.is_empty())
                .count();
            assert_eq!(r.pieces(), runs, "free neighbours merged");
        }
    }
}

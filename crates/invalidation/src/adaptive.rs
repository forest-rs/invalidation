// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Maps from dense keys that use a table while the keys are compact.

use alloc::vec::Vec;
use core::hash::Hash;

use core::fmt;

use hashbrown::hash_map::Entry;
use hashbrown::{HashMap, HashSet};

use crate::channel::{Channel, ChannelSet};
use crate::drain::DenseKey;

/// How many table slots each entry may account for before keys go to a hash
/// map instead.
const SLOTS_PER_ENTRY: usize = 4;

/// Table slots allowed however few entries there are, so that small maps over
/// small keys never hash.
const FREE_SLOTS: usize = 256;

/// The most table slots that `entries` entries may account for.
pub(crate) fn table_budget(entries: usize) -> usize {
    entries
        .saturating_mul(SLOTS_PER_ENTRY)
        .saturating_add(FREE_SLOTS)
}

/// Marks an empty table slot.
const ABSENT: u32 = u32::MAX;

/// In-degrees for a drain's keys, which are all known up front.
///
/// Keys whose index is below the table's length live in a `Vec<u32>` indexed
/// by [`DenseKey::index`]; the rest live in a hash map. The table is sized
/// once, to cover the largest key but at most [`table_budget`] slots for the
/// number of keys, so compact keys never hash, and keys with huge indices
/// cost a hash map entry rather than a table reaching up to them: the table
/// takes at most about 16 bytes per key, whatever the indices.
pub(crate) struct InDegrees<K> {
    table: Vec<u32>,
    hashed: HashMap<K, u32>,
    /// Number of keys present.
    len: usize,
}

impl<K> InDegrees<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Creates an empty map sized for `keys`.
    pub(crate) fn for_keys(keys: &[K]) -> Self {
        let table_len = keys
            .iter()
            .map(|key| key.index().saturating_add(1))
            .max()
            .unwrap_or(0)
            .min(table_budget(keys.len()));
        let mut table = Vec::new();
        table.resize(table_len, ABSENT);
        Self {
            table,
            hashed: HashMap::new(),
            len: 0,
        }
    }

    /// Returns the number of keys present.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Adds `key` with in-degree 0, returning `false` if it was present.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let inserted = match self.table.get_mut(key.index()) {
            Some(slot) => {
                let absent = *slot == ABSENT;
                if absent {
                    *slot = 0;
                }
                absent
            }
            None => match self.hashed.entry(key) {
                Entry::Occupied(_) => false,
                Entry::Vacant(entry) => {
                    entry.insert(0);
                    true
                }
            },
        };
        self.len += usize::from(inserted);
        inserted
    }

    /// Returns `key`'s in-degree, if it is present.
    pub(crate) fn get(&self, key: K) -> Option<u32> {
        match self.table.get(key.index()) {
            Some(&degree) => (degree != ABSENT).then_some(degree),
            None => self.hashed.get(&key).copied(),
        }
    }

    /// Returns a mutable reference to `key`'s in-degree, if it is present.
    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut u32> {
        match self.table.get_mut(key.index()) {
            Some(degree) => (*degree != ABSENT).then_some(degree),
            None => self.hashed.get_mut(&key),
        }
    }

    /// Removes `key`.
    pub(crate) fn remove(&mut self, key: K) {
        let removed = match self.table.get_mut(key.index()) {
            Some(slot) => core::mem::replace(slot, ABSENT) != ABSENT,
            None => self.hashed.remove(&key).is_some(),
        };
        self.len -= usize::from(removed);
    }

    /// Returns the number of table slots, for tests of memory use.
    #[cfg(test)]
    pub(crate) fn table_slots(&self) -> usize {
        self.table.len()
    }
}

/// A set of `(key, channel)` pairs, for a traversal whose size is not known
/// up front.
///
/// Each key's channels are a [`ChannelSet`] in a table indexed by
/// [`DenseKey::index`], while the table stays within [`table_budget`] for the
/// keys present; other keys' channels live in a hash map. The table grows as
/// keys arrive, and hashed keys that a grown table covers move into it, so
/// every key is in exactly one of the two.
pub(crate) struct PairSet<K> {
    /// The channels of each key in the table; empty when the key is absent.
    table: Vec<ChannelSet>,
    hashed: HashMap<K, ChannelSet>,
    /// The smallest index in `hashed`, so that growing the table looks for
    /// keys to move only once it reaches one.
    min_hashed: usize,
    /// Number of keys with at least one channel.
    keys: usize,
}

impl<K> Default for PairSet<K> {
    fn default() -> Self {
        Self {
            table: Vec::new(),
            hashed: HashMap::new(),
            min_hashed: usize::MAX,
            keys: 0,
        }
    }
}

impl<K> PairSet<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Adds `(key, channel)`, returning `true` if it was absent.
    pub(crate) fn insert(&mut self, key: K, channel: Channel) -> bool {
        let idx = key.index();
        if idx >= self.table.len() && !self.hashed.contains_key(&key) {
            self.grow_for(idx);
        }
        let channels = match self.table.get_mut(idx) {
            Some(channels) => channels,
            None => {
                self.min_hashed = self.min_hashed.min(idx);
                self.hashed.entry(key).or_insert(ChannelSet::EMPTY)
            }
        };
        if channels.contains(channel) {
            return false;
        }
        self.keys += usize::from(channels.is_empty());
        channels.insert(channel);
        true
    }

    /// Returns `true` if `(key, channel)` is present.
    pub(crate) fn contains(&self, key: K, channel: Channel) -> bool {
        match self.table.get(key.index()) {
            Some(channels) => channels.contains(channel),
            None => self
                .hashed
                .get(&key)
                .is_some_and(|channels| channels.contains(channel)),
        }
    }

    /// Grows the table to cover `idx` if it stays within budget, moving
    /// hashed keys that it then covers into it.
    fn grow_for(&mut self, idx: usize) {
        let budget = table_budget(self.keys + 1);
        if idx >= budget {
            return;
        }
        // Grow geometrically, within the budget, so a table filled in
        // ascending key order is not resized for every key.
        let new_len = idx
            .saturating_add(1)
            .max(self.table.len().saturating_mul(2))
            .min(budget);
        self.table.resize(new_len, ChannelSet::EMPTY);
        if self.min_hashed >= new_len {
            return;
        }
        let table = &mut self.table;
        let mut min_hashed = usize::MAX;
        self.hashed.retain(|key, channels| {
            let idx = key.index();
            match table.get_mut(idx) {
                Some(slot) => {
                    *slot = *channels;
                    false
                }
                None => {
                    min_hashed = min_hashed.min(idx);
                    true
                }
            }
        });
        self.min_hashed = min_hashed;
    }

    /// Returns the number of table slots, for tests of memory use.
    #[cfg(test)]
    pub(crate) fn table_slots(&self) -> usize {
        self.table.len()
    }
}

/// A set of keys, for traversals whose size is not known up front.
///
/// Keys live in a bitset indexed by [`DenseKey::index`] while it stays within
/// the bytes [`table_budget`] allows for the keys present (32 bits per table
/// slot); other keys live in a hash set. The bitset grows as keys arrive, and
/// hashed keys it then covers move into it, so every key is in exactly one of
/// the two.
#[derive(Clone)]
pub(crate) struct KeySet<K> {
    bits: Vec<u64>,
    hashed: HashSet<K>,
    /// The smallest index in `hashed`, so that growing the bitset looks for
    /// keys to move only once it reaches one.
    min_hashed: usize,
    len: usize,
}

impl<K> Default for KeySet<K> {
    fn default() -> Self {
        Self {
            bits: Vec::new(),
            hashed: HashSet::new(),
            min_hashed: usize::MAX,
            len: 0,
        }
    }
}

impl<K> fmt::Debug for KeySet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySet")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl<K> KeySet<K> {
    /// Removes every key, keeping the allocated capacity.
    pub(crate) fn clear(&mut self) {
        self.bits.fill(0);
        self.hashed.clear();
        self.min_hashed = usize::MAX;
        self.len = 0;
    }
}

impl<K> KeySet<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Adds `key`, returning `true` if it was absent.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let idx = key.index();
        if idx / 64 >= self.bits.len() && !self.hashed.contains(&key) {
            self.grow_for(idx);
        }
        let inserted = match self.bits.get_mut(idx / 64) {
            Some(word) => {
                let bit = 1 << (idx % 64);
                let absent = *word & bit == 0;
                *word |= bit;
                absent
            }
            None => {
                self.min_hashed = self.min_hashed.min(idx);
                self.hashed.insert(key)
            }
        };
        self.len += usize::from(inserted);
        inserted
    }

    /// Returns `true` if `key` is present.
    pub(crate) fn contains(&self, key: K) -> bool {
        let idx = key.index();
        match self.bits.get(idx / 64) {
            Some(word) => word & (1 << (idx % 64)) != 0,
            None => self.hashed.contains(&key),
        }
    }

    /// Grows the bitset to cover `idx` if it stays within budget, moving
    /// hashed keys that it then covers into it.
    fn grow_for(&mut self, idx: usize) {
        let budget_words = table_budget(self.len + 1) / 2;
        let word = idx / 64;
        if word >= budget_words {
            return;
        }
        let new_len = (word + 1)
            .max(self.bits.len().saturating_mul(2))
            .min(budget_words);
        self.bits.resize(new_len, 0);
        if self.min_hashed / 64 >= new_len {
            return;
        }
        let bits = &mut self.bits;
        let mut min_hashed = usize::MAX;
        self.hashed.retain(|key| {
            let idx = key.index();
            match bits.get_mut(idx / 64) {
                Some(word) => {
                    *word |= 1 << (idx % 64);
                    false
                }
                None => {
                    min_hashed = min_hashed.min(idx);
                    true
                }
            }
        });
        self.min_hashed = min_hashed;
    }

    /// Returns the bytes of the bitset, for tests of memory use.
    #[cfg(test)]
    pub(crate) fn table_bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_keys_use_the_table() {
        let keys: Vec<u32> = (0..1000).rev().collect();
        let mut degrees = InDegrees::for_keys(&keys);
        for &key in &keys {
            assert!(degrees.insert(key));
        }
        assert!(!degrees.insert(5));
        assert_eq!(degrees.len(), 1000);
        assert!(degrees.hashed.is_empty());
        assert_eq!(degrees.table_slots(), 1000);
    }

    #[test]
    fn sparse_keys_are_hashed() {
        let keys: Vec<u32> = (0..100).map(|key| key * 1_000_000).collect();
        let mut degrees = InDegrees::for_keys(&keys);
        for &key in &keys {
            assert!(degrees.insert(key));
        }
        assert!(degrees.table_slots() <= table_budget(100));
        assert_eq!(degrees.hashed.len(), 99, "all but key 0");
        *degrees.get_mut(99_000_000).unwrap() += 3;
        assert_eq!(degrees.get(99_000_000), Some(3));
        assert_eq!(degrees.get(5), None);
    }

    #[test]
    fn remove() {
        let keys = [3_u32, u32::MAX - 1];
        let mut degrees = InDegrees::for_keys(&keys);
        degrees.insert(3);
        degrees.insert(u32::MAX - 1);
        degrees.remove(3);
        degrees.remove(3);
        assert_eq!(degrees.len(), 1);
        assert_eq!(degrees.get(3), None);
        degrees.remove(u32::MAX - 1);
        assert_eq!(degrees.len(), 0);
    }

    const A: Channel = Channel::new(0);
    const B: Channel = Channel::new(5);

    #[test]
    fn pair_set_growth_skips_hashed_keys_out_of_reach() {
        let mut pairs = PairSet::<u32>::default();
        for key in 0..2000 {
            pairs.insert((1 << 30) + key, A);
        }
        assert_eq!(pairs.min_hashed, 1 << 30);
        // Compact keys grow the table again and again; none of these growths
        // reach the hashed keys, so none of them look at them.
        for key in 0..5000 {
            pairs.insert(key, A);
        }
        assert_eq!(pairs.hashed.len(), 2000);
        assert!(pairs.contains((1 << 30) + 1999, A));
        assert!(pairs.contains(4999, A));
    }

    #[test]
    fn pair_set_keeps_compact_keys_in_the_table() {
        let mut pairs = PairSet::<u32>::default();
        for key in 0..2000 {
            assert!(pairs.insert(key, A));
        }
        assert!(pairs.insert(7, B));
        assert!(!pairs.insert(7, B));
        assert!(pairs.contains(7, A) && pairs.contains(7, B));
        assert!(!pairs.contains(8, B));
        assert!(pairs.hashed.is_empty());
        assert!(pairs.table_slots() <= table_budget(2000));
    }

    #[test]
    fn pair_set_hashes_sparse_keys_and_migrates_them() {
        let mut pairs = PairSet::<u32>::default();
        assert!(pairs.insert(1 << 30, A));
        assert!(pairs.insert(1000, B), "too far for an empty set; hashed");
        assert_eq!(pairs.hashed.len(), 2);
        for key in 0..1000 {
            pairs.insert(key, A);
        }
        assert!(pairs.table_slots() > 1000);
        assert_eq!(pairs.hashed.len(), 1, "1000 moved into the table");
        assert!(pairs.contains(1000, B) && !pairs.contains(1000, A));
        assert!(pairs.contains(1 << 30, A));
        assert!(pairs.table_slots() <= table_budget(1001));
    }

    #[test]
    fn key_set_compact_and_sparse() {
        let mut keys = KeySet::<u32>::default();
        for key in (0..5000).rev() {
            assert!(keys.insert(key));
        }
        assert!(!keys.insert(4999));
        assert!(keys.contains(0) && keys.contains(4999) && !keys.contains(5000));
        assert!(keys.hashed.is_empty(), "compact keys all fit the bitset");
        assert!(keys.table_bytes() <= table_budget(5000) * 4);

        let mut sparse = KeySet::<u32>::default();
        for key in 0..100 {
            assert!(sparse.insert(key * 10_000_000));
        }
        assert_eq!(sparse.len, 100);
        assert!(sparse.contains(990_000_000));
        assert!(sparse.table_bytes() <= table_budget(100) * 4);

        keys.clear();
        assert!(!keys.contains(10));
        assert!(keys.insert(10));

        // Growth that does not reach the hashed keys does not look at them.
        let mut mixed = KeySet::<u32>::default();
        for key in 0..2000 {
            mixed.insert((1 << 30) + key);
        }
        for key in 0..20_000 {
            mixed.insert(key);
        }
        assert_eq!(mixed.hashed.len(), 2000);
        assert_eq!(mixed.min_hashed, 1 << 30);
        assert!(mixed.contains(19_999) && mixed.contains((1 << 30) + 1999));
    }
}

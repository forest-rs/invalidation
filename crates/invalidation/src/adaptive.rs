// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Maps from dense keys that use a table while the keys are compact.

use alloc::vec::Vec;
use core::hash::Hash;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;

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
}

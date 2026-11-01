// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Interning helper for non-`Copy` keys.
//!
//! `invalidation` intentionally keeps the core APIs keyed by `K: Copy` to
//! protect hot paths from accidental cloning and allocation. Many embedders,
//! however, naturally have structured/owned keys (strings, compound IDs, etc.).
//!
//! This module provides a small, `no_std + alloc` interner that maps owned keys
//! to a compact [`InternId`] that can be used with [`InvalidationTracker`](crate::InvalidationTracker).
//!
//! ## Example
//!
//! ```rust
//! use invalidation::{intern::Interner, Channel, InvalidationTracker, InternId, LazyPolicy};
//!
//! const LAYOUT: Channel = Channel::new(0);
//!
//! #[derive(Clone, Debug, Eq, PartialEq, Hash)]
//! struct ResourceKey(&'static str);
//!
//! let mut ids = Interner::<ResourceKey>::with_capacity(2);
//! let a: InternId = ids.intern(ResourceKey("a"));
//! let b: InternId = ids.intern(ResourceKey("b"));
//!
//! let mut tracker = InvalidationTracker::<InternId>::new();
//! tracker.reserve_keys(LAYOUT, ids.len());
//! tracker.add_dependency(b, a, LAYOUT).unwrap();
//! tracker.mark_with(a, LAYOUT, &LazyPolicy);
//!
//! let order: Vec<_> = tracker.drain_affected_sorted(LAYOUT).collect();
//! assert_eq!(order, vec![a, b]);
//!
//! // Best-effort debug lookup:
//! assert_eq!(ids.get(a).unwrap().0, "a");
//! ```

use alloc::vec::Vec;
use core::hash::{BuildHasher, Hash};

use hashbrown::DefaultHashBuilder;
use hashbrown::HashTable;

/// A compact, interned identifier.
///
/// This is intended to be used as the `K` parameter for `invalidation`
/// core types like [`InvalidationTracker`](crate::InvalidationTracker).
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct InternId(u32);

impl InternId {
    /// Returns this id as a `usize` index (for tables keyed by intern ids).
    #[inline]
    #[must_use]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Returns the raw numeric id.
    #[inline]
    #[must_use]
    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl crate::DenseKey for InternId {
    #[inline]
    fn index(self) -> usize {
        self.as_usize()
    }
}

/// Interns owned keys into compact [`InternId`] handles.
///
/// Keys are stored once in an owned-key table. A hash table stores only their
/// ids and compares candidates against the owned keys, so distinct keys with
/// colliding hashes remain distinct without allocating per-key candidate lists.
#[derive(Debug, Clone)]
pub struct Interner<K> {
    keys: Vec<K>,
    index: HashTable<InternId>,
    build_hasher: DefaultHashBuilder,
}

impl<K> Default for Interner<K>
where
    K: Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> Interner<K>
where
    K: Eq + Hash,
{
    /// Creates an empty interner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            keys: Vec::new(),
            index: HashTable::new(),
            build_hasher: DefaultHashBuilder::default(),
        }
    }

    /// Creates an empty interner with space for at least `capacity` keys.
    ///
    /// Preallocates the owned-key table and id index. Inserting that many
    /// distinct keys does not grow either table. Allocations owned by `K`
    /// itself are separate from the interner's storage.
    ///
    /// # Panics
    ///
    /// Panics if the requested storage exceeds addressable capacity. An
    /// allocation failure is handled by the allocator.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            keys: Vec::with_capacity(capacity),
            index: HashTable::with_capacity(capacity),
            build_hasher: DefaultHashBuilder::default(),
        }
    }

    /// Reserves space for at least `additional` new distinct keys.
    ///
    /// Reserves the owned-key table and id index without changing existing
    /// keys or their [`InternId`] values. Existing storage is never shrunk.
    ///
    /// # Panics
    ///
    /// Panics if the requested storage exceeds addressable capacity. An
    /// allocation failure is handled by the allocator.
    pub fn reserve(&mut self, additional: usize) {
        self.keys.reserve(additional);
        self.index.reserve(additional, |id| {
            self.build_hasher.hash_one(&self.keys[id.as_usize()])
        });
    }

    /// Returns the number of interned keys.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Returns `true` if the interner contains no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Returns the key for an interned id, if the id is in-range.
    #[must_use]
    pub fn get(&self, id: InternId) -> Option<&K> {
        self.keys.get(id.as_usize())
    }

    /// Interns `key` and returns its [`InternId`].
    ///
    /// If an equal key was already interned, this returns the existing id and
    /// drops `key`.
    pub fn intern(&mut self, key: K) -> InternId {
        let hash = self.build_hasher.hash_one(&key);
        if let Some(&id) = self.index.find(hash, |id| self.keys[id.as_usize()] == key) {
            return id;
        }

        let id = InternId(
            u32::try_from(self.keys.len()).expect("too many interned keys for InternId (u32)"),
        );
        // Growing the index hashes existing keys and can unwind. Reserve
        // before storing the candidate so a failed growth leaves no orphan.
        self.index.reserve(1, |id| {
            self.build_hasher.hash_one(&self.keys[id.as_usize()])
        });
        self.keys.push(key);
        self.index.insert_unique(hash, id, |id| {
            self.build_hasher.hash_one(&self.keys[id.as_usize()])
        });
        id
    }

    /// Clears all interned keys.
    ///
    /// This drops all stored keys and invalidates any previously returned ids.
    /// The owned-key table and id index retain their capacity for reuse.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.index.clear();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[derive(Debug, Eq, PartialEq, Hash)]
    struct Key(&'static str);

    #[test]
    #[cfg(panic = "unwind")]
    fn hash_panic_during_growth_does_not_commit_the_candidate() {
        use core::sync::atomic::{AtomicBool, Ordering};
        use std::panic::{AssertUnwindSafe, catch_unwind};

        static PANIC_ON_OLD_KEY: AtomicBool = AtomicBool::new(false);

        #[derive(Debug, PartialEq, Eq)]
        struct PanicKey(u32);

        impl Hash for PanicKey {
            fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
                if self.0 == 0 && PANIC_ON_OLD_KEY.swap(false, Ordering::Relaxed) {
                    panic!("one-shot hash panic during index growth");
                }
                self.0.hash(state);
            }
        }

        let mut ids = Interner::with_capacity(1);
        let capacity = u32::try_from(ids.index.capacity()).unwrap();
        let old_ids: Vec<_> = (0..capacity).map(|key| ids.intern(PanicKey(key))).collect();
        PANIC_ON_OLD_KEY.store(true, Ordering::Relaxed);
        let result = catch_unwind(AssertUnwindSafe(|| ids.intern(PanicKey(capacity))));
        assert!(result.is_err());
        assert_eq!(ids.len(), capacity as usize);

        for (key, id) in (0..capacity).zip(old_ids) {
            assert_eq!(ids.get(id), Some(&PanicKey(key)));
            assert_eq!(ids.intern(PanicKey(key)), id);
        }
        let retried = ids.intern(PanicKey(capacity));
        assert_eq!(retried.as_u32(), capacity);
        assert_eq!(ids.intern(PanicKey(capacity)), retried);
        assert_eq!(ids.len(), (capacity + 1) as usize);
    }

    #[test]
    fn reservation_preserves_ids_and_covers_additional_keys() {
        let mut ids = Interner::with_capacity(16);
        assert!(ids.is_empty());
        let initial_capacities = (ids.keys.capacity(), ids.index.capacity());
        for key in 0..16_u32 {
            assert_eq!(ids.intern(key).as_u32(), key);
        }
        assert_eq!(
            initial_capacities,
            (ids.keys.capacity(), ids.index.capacity())
        );

        ids.reserve(32);
        let capacities = (ids.keys.capacity(), ids.index.capacity());
        for key in 0..48_u32 {
            let id = ids.intern(key);
            assert_eq!(id.as_u32(), key);
            assert_eq!(ids.get(id), Some(&key));
        }
        assert_eq!(capacities, (ids.keys.capacity(), ids.index.capacity()));
        assert_eq!(ids.len(), 48);
        ids.clear();
        ids.reserve(0);
        assert_eq!(ids.intern(99).as_u32(), 0);
        assert_eq!(capacities, (ids.keys.capacity(), ids.index.capacity()));
    }

    #[test]
    fn collisions_survive_growth_reservation_clone_and_clear() {
        #[derive(Clone, Debug, PartialEq, Eq)]
        struct Colliding(u32);

        impl Hash for Colliding {
            fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
                0_u8.hash(state);
            }
        }

        let mut ids = Interner::with_capacity(2);
        let a = ids.intern(Colliding(1));
        let b = ids.intern(Colliding(2));
        ids.reserve(128);
        for key in 3..=512 {
            assert_eq!(ids.intern(Colliding(key)).as_u32(), key - 1);
        }
        assert_eq!(ids.intern(Colliding(1)), a);
        assert_eq!(ids.intern(Colliding(2)), b);
        assert_eq!(ids.get(a), Some(&Colliding(1)));
        assert_eq!(ids.get(b), Some(&Colliding(2)));
        assert_eq!(ids.len(), 512);

        let mut cloned = ids.clone();
        cloned.reserve(512);
        for key in 1..=512 {
            let id = cloned.intern(Colliding(key));
            assert_eq!(id.as_u32(), key - 1);
            assert_eq!(cloned.get(id), Some(&Colliding(key)));
        }
        assert_eq!(cloned.len(), 512);
        ids.clear();
        assert!(ids.is_empty());
        assert_eq!(ids.intern(Colliding(99)).as_u32(), 0);
        assert_eq!(cloned.get(a), Some(&Colliding(1)));
    }

    #[test]
    fn interns_duplicates_to_same_id() {
        let mut i = Interner::<Key>::new();
        let a0 = i.intern(Key("a"));
        let a1 = i.intern(Key("a"));
        let b = i.intern(Key("b"));

        assert_eq!(a0, a1);
        assert_ne!(a0, b);
        assert_eq!(i.get(a0).unwrap(), &Key("a"));
        assert_eq!(i.get(b).unwrap(), &Key("b"));
    }
}

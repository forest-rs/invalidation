// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Invalidation set: accumulated invalidated keys per channel.

use core::hash::Hash;

use hashbrown::HashSet;

use crate::channel::Channel;

/// Maximum number of channels supported (64).
const MAX_CHANNELS: usize = 64;

/// Accumulated invalidated keys per channel with generation tracking.
///
/// `InvalidationSet` maintains a set of invalidated keys for each channel, along with a
/// generation counter that increments on every mutation. The generation can
/// be used to detect stale computations or cache invalidation.
///
/// # Type Parameters
///
/// - `K`: The key type, typically a node identifier. Must be `Copy + Eq + Hash`.
///   If your natural key is owned/structured, see [`intern::Interner`](crate::intern::Interner).
///
/// # Example
///
/// ```
/// use invalidation::{Channel, InvalidationSet};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
///
/// let mut invalidated = InvalidationSet::<u32>::new();
///
/// // Mark nodes invalidated
/// invalidated.mark(1, LAYOUT);
/// invalidated.mark(2, LAYOUT);
/// invalidated.mark(1, PAINT);
///
/// assert!(invalidated.is_invalidated(1, LAYOUT));
/// assert!(invalidated.is_invalidated(2, LAYOUT));
/// assert!(invalidated.is_invalidated(1, PAINT));
/// assert!(!invalidated.is_invalidated(2, PAINT));
///
/// // Drain returns and clears invalidated keys for a channel
/// let layout_invalidated: Vec<_> = invalidated.drain(LAYOUT).collect();
/// assert_eq!(layout_invalidated.len(), 2);
/// assert!(!invalidated.is_invalidated(1, LAYOUT));
/// ```
///
/// # See Also
///
/// - [`InvalidationTracker`](crate::InvalidationTracker): Convenience wrapper combining a [`InvalidationGraph`](crate::InvalidationGraph)
///   and `InvalidationSet`.
/// - [`drain_sorted`](crate::drain_sorted): Drain a [`InvalidationSet`] in topological order given a graph.
/// - [`drain_affected_sorted`](crate::drain_affected_sorted): Drain affected keys (roots + dependents), useful with [`LazyPolicy`](crate::LazyPolicy).
#[derive(Debug)]
pub struct InvalidationSet<K>
where
    K: Copy + Eq + Hash,
{
    /// Per-channel invalidated key sets.
    channels: [HashSet<K>; MAX_CHANNELS],
    /// Generation counter, incremented on each mutation.
    generation: u64,
}

impl<K> Default for InvalidationSet<K>
where
    K: Copy + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> InvalidationSet<K>
where
    K: Copy + Eq + Hash,
{
    /// Creates a new empty invalidation set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            channels: core::array::from_fn(|_| HashSet::new()),
            generation: 0,
        }
    }

    /// Returns the current generation.
    ///
    /// The generation is incremented on every mutation (mark, clear, drain).
    /// This can be used to detect whether the invalidation set has changed since a
    /// previous observation.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Marks a key as invalidated in the given channel.
    ///
    /// Returns `true` if the key was newly inserted, `false` if it was already invalidated.
    #[inline]
    pub fn mark(&mut self, key: K, channel: Channel) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.channels[channel.index() as usize].insert(key)
    }

    /// Returns `true` if the key is invalidated in the given channel.
    #[inline]
    #[must_use]
    pub fn is_invalidated(&self, key: K, channel: Channel) -> bool {
        self.channels[channel.index() as usize].contains(&key)
    }

    /// Returns `true` if there are any invalidated keys in the given channel.
    #[inline]
    #[must_use]
    pub fn has_invalidated(&self, channel: Channel) -> bool {
        !self.channels[channel.index() as usize].is_empty()
    }

    /// Returns `true` if there are no invalidated keys in any channel.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.iter().all(HashSet::is_empty)
    }

    /// Returns the number of invalidated keys in the given channel.
    #[must_use]
    pub fn len(&self, channel: Channel) -> usize {
        self.channels[channel.index() as usize].len()
    }

    /// Returns an iterator over the invalidated keys in the given channel.
    ///
    /// This does not clear the invalidation state. Use [`drain`](Self::drain) to
    /// consume and clear.
    pub fn iter(&self, channel: Channel) -> impl Iterator<Item = K> + '_ {
        self.channels[channel.index() as usize].iter().copied()
    }

    /// Drains and returns the invalidated keys for the given channel.
    ///
    /// After this call, the channel will have no invalidated keys.
    #[inline]
    pub fn drain(&mut self, channel: Channel) -> impl Iterator<Item = K> + '_ {
        self.generation = self.generation.wrapping_add(1);
        self.channels[channel.index() as usize].drain()
    }

    /// Removes `key` from the given channel.
    ///
    /// Returns `true` if `key` was present.
    #[inline]
    pub fn take(&mut self, key: K, channel: Channel) -> bool {
        let removed = self.channels[channel.index() as usize].remove(&key);
        if removed {
            self.generation = self.generation.wrapping_add(1);
        }
        removed
    }

    /// Clears all invalidated keys in the given channel.
    pub fn clear(&mut self, channel: Channel) {
        self.generation = self.generation.wrapping_add(1);
        self.channels[channel.index() as usize].clear();
    }

    /// Clears all invalidated keys in all channels.
    pub fn clear_all(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        for set in &mut self.channels {
            set.clear();
        }
    }

    /// Removes a specific key from all channels.
    ///
    /// This is useful when a node is removed from the tree entirely.
    pub fn remove_key(&mut self, key: K) {
        let mut removed = false;
        for set in &mut self.channels {
            removed |= set.remove(&key);
        }
        if removed {
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

impl<K> Clone for InvalidationSet<K>
where
    K: Copy + Eq + Hash,
{
    fn clone(&self) -> Self {
        Self {
            channels: core::array::from_fn(|i| self.channels[i].clone()),
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);

    #[test]
    fn mark_and_query() {
        let mut invalidated = InvalidationSet::<u32>::new();

        assert!(!invalidated.is_invalidated(1, LAYOUT));
        assert!(invalidated.is_empty());

        let inserted = invalidated.mark(1, LAYOUT);
        assert!(inserted);
        assert!(invalidated.is_invalidated(1, LAYOUT));
        assert!(!invalidated.is_empty());
        assert!(invalidated.has_invalidated(LAYOUT));

        // Marking again returns false
        let inserted_again = invalidated.mark(1, LAYOUT);
        assert!(!inserted_again);
    }

    #[test]
    fn channel_independence() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, PAINT);

        assert!(invalidated.is_invalidated(1, LAYOUT));
        assert!(!invalidated.is_invalidated(1, PAINT));
        assert!(!invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(2, PAINT));
    }

    #[test]
    fn drain_clears_channel() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, LAYOUT);
        invalidated.mark(1, PAINT);

        let drained: Vec<_> = invalidated.drain(LAYOUT).collect();
        assert_eq!(drained.len(), 2);
        assert!(!invalidated.has_invalidated(LAYOUT));
        assert!(invalidated.has_invalidated(PAINT));
    }

    #[test]
    fn take_removes_single_key() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, LAYOUT);

        assert!(invalidated.take(1, LAYOUT));
        assert!(!invalidated.is_invalidated(1, LAYOUT));
        assert!(invalidated.is_invalidated(2, LAYOUT));

        // Taking again returns false.
        assert!(!invalidated.take(1, LAYOUT));
    }

    #[test]
    fn clear_specific_channel() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(1, PAINT);

        invalidated.clear(LAYOUT);
        assert!(!invalidated.has_invalidated(LAYOUT));
        assert!(invalidated.has_invalidated(PAINT));
    }

    #[test]
    fn clear_all() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, PAINT);

        invalidated.clear_all();
        assert!(invalidated.is_empty());
    }

    #[test]
    fn remove_key_from_all_channels() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(1, PAINT);
        invalidated.mark(2, LAYOUT);

        invalidated.remove_key(1);
        assert!(!invalidated.is_invalidated(1, LAYOUT));
        assert!(!invalidated.is_invalidated(1, PAINT));
        assert!(invalidated.is_invalidated(2, LAYOUT));
    }

    #[test]
    fn generation_increments() {
        let mut invalidated = InvalidationSet::<u32>::new();
        let initial = invalidated.generation();

        invalidated.mark(1, LAYOUT);
        assert_eq!(invalidated.generation(), initial + 1);

        invalidated.mark(2, LAYOUT);
        assert_eq!(invalidated.generation(), initial + 2);

        let _ = invalidated.drain(LAYOUT).count();
        assert_eq!(invalidated.generation(), initial + 3);

        invalidated.clear(PAINT);
        assert_eq!(invalidated.generation(), initial + 4);
    }

    #[test]
    fn len_and_iter() {
        let mut invalidated = InvalidationSet::<u32>::new();

        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, LAYOUT);
        invalidated.mark(3, LAYOUT);

        assert_eq!(invalidated.len(LAYOUT), 3);
        assert_eq!(invalidated.len(PAINT), 0);

        let keys: Vec<_> = invalidated.iter(LAYOUT).collect();
        assert_eq!(keys.len(), 3);
    }
}

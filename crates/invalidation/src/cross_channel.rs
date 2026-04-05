// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cross-channel edges connecting (key, channel) pairs across channels.
//!
//! [`CrossChannelEdges`] stores sparse edges of the form
//! `(key_a, channel_a) → (key_b, channel_b)`, modeling dependencies like
//! "node A's LAYOUT output feeds node B's PAINT input".

use alloc::vec::Vec;
use core::hash::Hash;

use hashbrown::HashMap;

use crate::channel::Channel;

/// Sparse cross-channel dependency edges.
///
/// Each edge connects `(from_key, from_channel)` to `(to_key, to_channel)`,
/// expressing that invalidation of `from_key` on `from_channel` should also
/// mark `to_key` on `to_channel`.
///
/// Cross-channel edges are expected to be uncommon relative to same-channel
/// edges, so storage is sparse (hash maps).
///
/// # Example
///
/// ```
/// use invalidation::{Channel, CrossChannelEdges};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
///
/// let mut edges = CrossChannelEdges::<u32>::new();
///
/// // Node 1's LAYOUT invalidation feeds node 2's PAINT.
/// edges.add_edge(1, LAYOUT, 2, PAINT);
///
/// let deps: Vec<_> = edges.dependents(1, LAYOUT).collect();
/// assert_eq!(deps, vec![(2, PAINT)]);
/// ```
#[derive(Debug, Clone)]
pub struct CrossChannelEdges<K> {
    /// Forward map: `(from_key, from_channel)` → list of `(to_key, to_channel)`.
    forward: HashMap<(K, Channel), Vec<(K, Channel)>>,
    /// Reverse map: `(to_key, to_channel)` → list of `(from_key, from_channel)`.
    reverse: HashMap<(K, Channel), Vec<(K, Channel)>>,
}

impl<K> Default for CrossChannelEdges<K>
where
    K: Copy + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> CrossChannelEdges<K>
where
    K: Copy + Eq + Hash,
{
    /// Creates a new empty cross-channel edge set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Returns `true` if there are no cross-channel edges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Adds a cross-channel edge.
    ///
    /// Returns `true` if the edge was newly added, `false` if it already existed.
    pub fn add_edge(&mut self, from_key: K, from_ch: Channel, to_key: K, to_ch: Channel) -> bool {
        let from = (from_key, from_ch);
        let to = (to_key, to_ch);

        let fwd = self.forward.entry(from).or_default();
        if fwd.contains(&to) {
            return false;
        }
        fwd.push(to);

        self.reverse.entry(to).or_default().push(from);
        true
    }

    /// Removes a cross-channel edge.
    ///
    /// Returns `true` if the edge existed and was removed.
    pub fn remove_edge(
        &mut self,
        from_key: K,
        from_ch: Channel,
        to_key: K,
        to_ch: Channel,
    ) -> bool {
        let from = (from_key, from_ch);
        let to = (to_key, to_ch);

        let removed = if let Some(fwd) = self.forward.get_mut(&from) {
            if let Some(pos) = fwd.iter().position(|e| *e == to) {
                fwd.swap_remove(pos);
                if fwd.is_empty() {
                    self.forward.remove(&from);
                }
                true
            } else {
                false
            }
        } else {
            false
        };

        if removed
            && let Some(rev) = self.reverse.get_mut(&to)
            && let Some(pos) = rev.iter().position(|e| *e == from)
        {
            rev.swap_remove(pos);
            if rev.is_empty() {
                self.reverse.remove(&to);
            }
        }

        removed
    }

    /// Returns an iterator over the cross-channel dependents of `(key, channel)`.
    ///
    /// These are the `(to_key, to_channel)` pairs that should be invalidated
    /// when `key` is invalidated on `channel`.
    pub fn dependents(&self, key: K, ch: Channel) -> impl Iterator<Item = (K, Channel)> + '_ {
        self.forward
            .get(&(key, ch))
            .into_iter()
            .flat_map(|v| v.iter())
            .copied()
    }

    /// Returns an iterator over the cross-channel dependencies of `(key, channel)`.
    ///
    /// These are the `(from_key, from_channel)` pairs whose invalidation would
    /// cause `key` on `channel` to be invalidated.
    pub fn dependencies(&self, key: K, ch: Channel) -> impl Iterator<Item = (K, Channel)> + '_ {
        self.reverse
            .get(&(key, ch))
            .into_iter()
            .flat_map(|v| v.iter())
            .copied()
    }

    /// Removes all cross-channel edges involving `key` (on any channel).
    pub fn remove_key(&mut self, key: K) {
        // Collect all forward entries for this key on any channel, then remove
        // reverse entries.
        let fwd_keys: Vec<(K, Channel)> = self
            .forward
            .keys()
            .filter(|(k, _)| *k == key)
            .copied()
            .collect();

        for fwd_key in &fwd_keys {
            if let Some(targets) = self.forward.remove(fwd_key) {
                for to in targets {
                    if let Some(rev) = self.reverse.get_mut(&to) {
                        if let Some(pos) = rev.iter().position(|e| *e == *fwd_key) {
                            rev.swap_remove(pos);
                        }
                        if rev.is_empty() {
                            self.reverse.remove(&to);
                        }
                    }
                }
            }
        }

        // Collect all reverse entries for this key on any channel, then remove
        // forward entries.
        let rev_keys: Vec<(K, Channel)> = self
            .reverse
            .keys()
            .filter(|(k, _)| *k == key)
            .copied()
            .collect();

        for rev_key in &rev_keys {
            if let Some(sources) = self.reverse.remove(rev_key) {
                for from in sources {
                    if let Some(fwd) = self.forward.get_mut(&from) {
                        if let Some(pos) = fwd.iter().position(|e| *e == *rev_key) {
                            fwd.swap_remove(pos);
                        }
                        if fwd.is_empty() {
                            self.forward.remove(&from);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);
    const COMPOSITE: Channel = Channel::new(2);

    #[test]
    fn new_is_empty() {
        let edges = CrossChannelEdges::<u32>::new();
        assert!(edges.is_empty());
    }

    #[test]
    fn add_and_query_edge() {
        let mut edges = CrossChannelEdges::<u32>::new();
        assert!(edges.add_edge(1, LAYOUT, 2, PAINT));
        assert!(!edges.is_empty());

        let deps: Vec<_> = edges.dependents(1, LAYOUT).collect();
        assert_eq!(deps, vec![(2, PAINT)]);

        let sources: Vec<_> = edges.dependencies(2, PAINT).collect();
        assert_eq!(sources, vec![(1, LAYOUT)]);
    }

    #[test]
    fn add_duplicate_returns_false() {
        let mut edges = CrossChannelEdges::<u32>::new();
        assert!(edges.add_edge(1, LAYOUT, 2, PAINT));
        assert!(!edges.add_edge(1, LAYOUT, 2, PAINT));
    }

    #[test]
    fn remove_edge() {
        let mut edges = CrossChannelEdges::<u32>::new();
        edges.add_edge(1, LAYOUT, 2, PAINT);
        edges.add_edge(1, LAYOUT, 3, COMPOSITE);

        assert!(edges.remove_edge(1, LAYOUT, 2, PAINT));
        assert!(!edges.remove_edge(1, LAYOUT, 2, PAINT));

        let deps: Vec<_> = edges.dependents(1, LAYOUT).collect();
        assert_eq!(deps, vec![(3, COMPOSITE)]);
    }

    #[test]
    fn remove_key_clears_all_channels() {
        let mut edges = CrossChannelEdges::<u32>::new();
        // Key 1 as source.
        edges.add_edge(1, LAYOUT, 2, PAINT);
        edges.add_edge(1, PAINT, 3, COMPOSITE);
        // Key 1 as target.
        edges.add_edge(5, COMPOSITE, 1, LAYOUT);

        edges.remove_key(1);

        assert!(edges.dependents(1, LAYOUT).next().is_none());
        assert!(edges.dependents(1, PAINT).next().is_none());
        assert!(edges.dependencies(1, LAYOUT).next().is_none());
        // Reverse links cleaned up.
        assert!(edges.dependents(5, COMPOSITE).next().is_none());
    }

    #[test]
    fn multiple_edges_from_same_source() {
        let mut edges = CrossChannelEdges::<u32>::new();
        edges.add_edge(1, LAYOUT, 2, PAINT);
        edges.add_edge(1, LAYOUT, 3, COMPOSITE);
        edges.add_edge(1, LAYOUT, 4, PAINT);

        let deps: Vec<_> = edges.dependents(1, LAYOUT).collect();
        assert_eq!(deps.len(), 3);
    }

    #[test]
    fn no_dependents_returns_empty() {
        let edges = CrossChannelEdges::<u32>::new();
        assert_eq!(edges.dependents(1, LAYOUT).count(), 0);
        assert_eq!(edges.dependencies(1, LAYOUT).count(), 0);
    }
}

// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cross-channel edges connecting (key, channel) pairs across channels.
//!
//! [`CrossChannelEdges`] stores sparse edges of the form
//! `(key_a, channel_a) → (key_b, channel_b)`, modeling dependencies like
//! "node A's LAYOUT output feeds node B's PAINT input".
//!
//! Most callers should configure cross-channel edges through
//! [`InvalidationTracker::add_cross_dependency`](crate::InvalidationTracker::add_cross_dependency)
//! so the tracker follows them during [`mark_with`](crate::InvalidationTracker::mark_with).
//! Use [`CrossChannelEdges`] directly when embedding these edges in a custom
//! coordinator.

use alloc::vec::Vec;
use core::fmt;
use core::hash::Hash;

use hashbrown::HashMap;

use crate::channel::{Channel, ChannelNames, ChannelSet};

/// Sparse cross-channel dependency edges.
///
/// Each edge connects `(from_key, from_channel)` to `(to_key, to_channel)`,
/// expressing that invalidation of `from_key` on `from_channel` should also
/// mark `to_key` on `to_channel`.
///
/// Cross-channel edges are expected to be uncommon relative to same-channel
/// edges, so storage is sparse (hash maps).
///
/// # Common tracker usage
///
/// ```
/// use invalidation::{Channel, EagerPolicy, InvalidationTracker};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
///
/// let mut tracker = InvalidationTracker::<u32>::new();
///
/// // Node 1's LAYOUT invalidation feeds node 2's PAINT.
/// tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
///
/// tracker.mark_with(1, LAYOUT, &EagerPolicy);
/// assert!(tracker.is_invalidated(1, LAYOUT));
/// assert!(tracker.is_invalidated(2, PAINT));
/// ```
///
/// # Standalone usage
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
#[derive(Clone)]
pub struct CrossChannelEdges<K> {
    /// Forward map: `(from_key, from_channel)` → list of `(to_key, to_channel)`.
    forward: HashMap<(K, Channel), Vec<(K, Channel)>>,
    /// Reverse map: `(to_key, to_channel)` → list of `(from_key, from_channel)`.
    reverse: HashMap<(K, Channel), Vec<(K, Channel)>>,
    /// Number of `(key, channel)` sources in `forward`, per channel, so that
    /// whether a channel has outgoing edges is a constant-time question.
    sources: [u32; 64],
}

impl<K> fmt::Debug for CrossChannelEdges<K>
where
    K: Copy + Eq + Hash + fmt::Debug,
{
    /// Lists the edges as `(key, channel) -> (key, channel)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CrossChannelEdges ")?;
        fmt::Debug::fmt(&self.debug_with(&ChannelNames::new()), f)
    }
}

/// Formats cross-channel edges as a list of `(key, channel) -> (key, channel)`.
struct CrossEdges<'a, K> {
    edges: &'a CrossChannelEdges<K>,
    names: &'a ChannelNames,
}

impl<K> fmt::Debug for CrossEdges<'_, K>
where
    K: Copy + Eq + Hash + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names = self.names;
        f.debug_list()
            .entries(self.edges.edges().map(|((fk, fc), (tk, tc))| {
                crate::graph::Edge((fk, names.display(fc)), (tk, names.display(tc)))
            }))
            .finish()
    }
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
            sources: [0; 64],
        }
    }

    /// Returns `true` if there are no cross-channel edges.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Returns the number of cross-channel edges.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.values().map(Vec::len).sum()
    }

    /// Returns the channels that have at least one edge leading out of them.
    ///
    /// This is a constant-time query.
    #[must_use]
    pub fn source_channels(&self) -> ChannelSet {
        let mut channels = ChannelSet::EMPTY;
        for channel in ChannelSet::ALL {
            if self.sources[channel.index() as usize] > 0 {
                channels.insert(channel);
            }
        }
        channels
    }

    /// Returns `true` if any edge leads out of `channel`.
    #[must_use]
    pub fn has_sources_in(&self, channel: Channel) -> bool {
        self.sources[channel.index() as usize] > 0
    }

    /// Returns an iterator over every edge, as `(from, to)` pairs of
    /// `(key, channel)`.
    ///
    /// The iteration order is not specified.
    pub fn edges(&self) -> impl Iterator<Item = ((K, Channel), (K, Channel))> + '_ {
        self.forward
            .iter()
            .flat_map(|(&from, targets)| targets.iter().map(move |&to| (from, to)))
    }

    /// Formats the edges, naming channels with `names`.
    pub(crate) fn debug_with<'a>(&'a self, names: &'a ChannelNames) -> impl fmt::Debug + 'a
    where
        K: fmt::Debug,
    {
        CrossEdges { edges: self, names }
    }

    /// Adds a cross-channel edge.
    ///
    /// Returns `true` if the edge was newly added, `false` if it already existed.
    pub fn add_edge(&mut self, from_key: K, from_ch: Channel, to_key: K, to_ch: Channel) -> bool {
        let from = (from_key, from_ch);
        let to = (to_key, to_ch);

        if !self.forward.contains_key(&from) {
            self.sources[from_ch.index() as usize] += 1;
        }
        let fwd = self.forward.entry(from).or_default();
        if fwd.contains(&to) {
            return false;
        }
        fwd.push(to);

        self.reverse.entry(to).or_default().push(from);
        true
    }

    /// Replaces all cross-channel dependents of `(from_key, from_ch)`.
    ///
    /// This is a batch convenience for the common "set all outgoing
    /// cross-channel edges" workflow.
    ///
    /// - Dependents present in the old set but missing from `targets` are removed.
    /// - Dependents present in `targets` but missing from the old set are added.
    /// - Dependents present in both sets are left unchanged.
    /// - Duplicate `(key, channel)` pairs in `targets` are ignored.
    ///
    /// # Returns
    ///
    /// - `true` if the dependent set changed.
    /// - `false` if the dependent set was already equal to `targets`.
    pub fn replace_dependents(
        &mut self,
        from_key: K,
        from_ch: Channel,
        targets: impl IntoIterator<Item = (K, Channel)>,
    ) -> bool {
        let mut new_set: Vec<(K, Channel)> = Vec::new();
        for target in targets {
            if !new_set.contains(&target) {
                new_set.push(target);
            }
        }

        let from = (from_key, from_ch);
        let old: &[(K, Channel)] = self.forward.get(&from).map_or(&[], Vec::as_slice);
        let unchanged =
            old.len() == new_set.len() && old.iter().all(|target| new_set.contains(target));
        if unchanged {
            return false;
        }

        self.clear_dependents(from_key, from_ch);
        for (to_key, to_ch) in new_set {
            self.add_edge(from_key, from_ch, to_key, to_ch);
        }

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
                    self.sources[from_ch.index() as usize] -= 1;
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

    /// Removes all cross-channel dependents of `(from_key, from_ch)`.
    ///
    /// Returns `true` if any edges existed and were removed.
    pub fn clear_dependents(&mut self, from_key: K, from_ch: Channel) -> bool {
        let from = (from_key, from_ch);
        let Some(targets) = self.forward.remove(&from) else {
            return false;
        };
        self.sources[from_ch.index() as usize] -= 1;

        for to in targets {
            if let Some(rev) = self.reverse.get_mut(&to) {
                if let Some(pos) = rev.iter().position(|e| *e == from) {
                    rev.swap_remove(pos);
                }
                if rev.is_empty() {
                    self.reverse.remove(&to);
                }
            }
        }

        true
    }

    /// Removes all cross-channel dependencies of `(to_key, to_ch)`.
    ///
    /// Returns `true` if any edges existed and were removed.
    pub fn clear_dependencies(&mut self, to_key: K, to_ch: Channel) -> bool {
        let to = (to_key, to_ch);
        let Some(sources) = self.reverse.remove(&to) else {
            return false;
        };

        for from in sources {
            if let Some(fwd) = self.forward.get_mut(&from) {
                if let Some(pos) = fwd.iter().position(|e| *e == to) {
                    fwd.swap_remove(pos);
                }
                if fwd.is_empty() {
                    self.forward.remove(&from);
                    self.sources[from.1.index() as usize] -= 1;
                }
            }
        }

        true
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
                self.sources[fwd_key.1.index() as usize] -= 1;
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
                            self.sources[from.1.index() as usize] -= 1;
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
    fn replace_dependents_updates_outgoing_edges() {
        let mut edges = CrossChannelEdges::<u32>::new();
        edges.add_edge(1, LAYOUT, 2, PAINT);
        edges.add_edge(1, LAYOUT, 3, COMPOSITE);

        assert!(edges.replace_dependents(1, LAYOUT, [(3, COMPOSITE), (4, PAINT), (4, PAINT)]));

        let deps: Vec<_> = edges.dependents(1, LAYOUT).collect();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&(3, COMPOSITE)));
        assert!(deps.contains(&(4, PAINT)));

        assert!(edges.dependencies(2, PAINT).next().is_none());
        assert_eq!(
            edges.dependencies(4, PAINT).collect::<Vec<_>>(),
            vec![(1, LAYOUT)]
        );

        assert!(!edges.replace_dependents(1, LAYOUT, [(4, PAINT), (3, COMPOSITE)]));
    }

    #[test]
    fn clear_dependents_removes_outgoing_edges() {
        let mut edges = CrossChannelEdges::<u32>::new();
        edges.add_edge(1, LAYOUT, 2, PAINT);
        edges.add_edge(1, LAYOUT, 3, COMPOSITE);

        assert!(edges.clear_dependents(1, LAYOUT));
        assert!(!edges.clear_dependents(1, LAYOUT));

        assert!(edges.dependents(1, LAYOUT).next().is_none());
        assert!(edges.dependencies(2, PAINT).next().is_none());
        assert!(edges.dependencies(3, COMPOSITE).next().is_none());
    }

    #[test]
    fn clear_dependencies_removes_incoming_edges() {
        let mut edges = CrossChannelEdges::<u32>::new();
        edges.add_edge(1, LAYOUT, 3, PAINT);
        edges.add_edge(2, COMPOSITE, 3, PAINT);

        assert!(edges.clear_dependencies(3, PAINT));
        assert!(!edges.clear_dependencies(3, PAINT));

        assert!(edges.dependencies(3, PAINT).next().is_none());
        assert!(edges.dependents(1, LAYOUT).next().is_none());
        assert!(edges.dependents(2, COMPOSITE).next().is_none());
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

    #[test]
    fn source_channels_track_every_mutation() {
        let mut seed = 0x50_u64;
        let mut next = |bound: u32| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            u32::try_from(seed >> 33).unwrap() % bound
        };
        let ch = |i: u32| Channel::new(u8::try_from(i).unwrap());
        let mut edges = CrossChannelEdges::<u32>::new();
        for _ in 0..2000 {
            let (a, b) = (next(6), next(6));
            let (x, y) = (ch(next(3)), ch(next(3)));
            match next(7) {
                0 | 1 => {
                    edges.add_edge(a, x, b, y);
                }
                2 => {
                    edges.remove_edge(a, x, b, y);
                }
                3 => {
                    edges.clear_dependents(a, x);
                }
                4 => {
                    edges.clear_dependencies(b, y);
                }
                5 => {
                    edges.replace_dependents(a, x, [(b, y), (a, y)]);
                }
                _ => edges.remove_key(a),
            }
            let mut expected = ChannelSet::EMPTY;
            for ((_, from_ch), _) in edges.edges() {
                expected.insert(from_ch);
            }
            assert_eq!(edges.source_channels(), expected);
        }
    }
}

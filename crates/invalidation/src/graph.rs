// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Dependency graph for invalidation tracking.

use alloc::vec::Vec;
use core::fmt;
use core::hash::Hash;

use hashbrown::{HashMap, HashSet};

use crate::channel::{Channel, ChannelNames, ChannelSet};
use crate::drain::{DenseKey, prepare_dense_growth};
use crate::scratch::TraversalScratch;

/// Maximum number of channels supported (64).
const MAX_CHANNELS: usize = 64;

/// Error returned when a cycle would be created by adding a dependency.
///
/// The error carries the existing chain of dependencies that the new edge
/// would close, so the cycle can be reported and broken without searching the
/// graph again.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CycleError<K> {
    /// The key that would depend on another.
    pub from: K,
    /// The key that would be depended upon.
    pub to: K,
    /// The channel where the cycle would occur.
    pub channel: Channel,
    /// The existing dependency chain from `to` back to `from`.
    ///
    /// It starts with `to` and ends with `from`, and each key depends on the
    /// next one, so `from -> to` followed by this chain is the cycle. For a
    /// self-dependency (`from == to`) it is just `[from]`.
    pub path: Vec<K>,
}

impl<K: fmt::Debug> fmt::Debug for CycleError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CycleError")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("channel", &self.channel)
            .field("path", &self.path)
            .finish()
    }
}

impl<K: fmt::Debug> fmt::Display for CycleError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "adding dependency {:?} -> {:?} in {:?} would create a cycle: {:?}",
            self.from, self.to, self.channel, self.from
        )?;
        for key in &self.path {
            write!(f, " -> {key:?}")?;
        }
        Ok(())
    }
}

impl<K: fmt::Debug> core::error::Error for CycleError<K> {}

/// What [`InvalidationGraph::add_dependency`] did with a dependency.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum AddDependency {
    /// The dependency was added.
    Added,
    /// The dependency already existed; the graph is unchanged.
    AlreadyPresent,
    /// The dependency would have created a cycle and was dropped, as
    /// [`CycleHandling::Ignore`] (or [`CycleHandling::DebugAssert`] in release
    /// builds) asks. A self-dependency is dropped this way even under
    /// [`CycleHandling::Allow`]. The graph is unchanged.
    CycleIgnored,
}

impl AddDependency {
    /// Returns `true` if the dependency was added.
    #[must_use]
    pub fn is_added(self) -> bool {
        self == Self::Added
    }
}

/// What [`InvalidationGraph::replace_dependencies`] changed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DependencyChanges<K> {
    /// Dependencies that were added.
    pub added: Vec<K>,
    /// Dependencies that were removed.
    pub removed: Vec<K>,
    /// Requested dependencies that were dropped because they would have
    /// created a cycle (see [`AddDependency::CycleIgnored`]).
    pub cycles_ignored: Vec<K>,
}

impl<K> Default for DependencyChanges<K> {
    fn default() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
            cycles_ignored: Vec::new(),
        }
    }
}

impl<K> DependencyChanges<K> {
    /// Returns `true` if the dependency set changed.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// How to handle cycle detection when adding dependencies.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum CycleHandling {
    /// Panic in debug builds, silently ignore in release builds.
    ///
    /// This is the default behavior: catches bugs during development and
    /// returns `Ok(AddDependency::CycleIgnored)` in release builds if a cycle is detected.
    ///
    /// This still performs the reachability check. Use [`Allow`](Self::Allow)
    /// only when the caller guarantees acyclic input or intentionally permits
    /// cycles.
    #[default]
    DebugAssert,
    /// Return an error if a cycle would be created.
    Error,
    /// Silently ignore the dependency if it would create a cycle.
    Ignore,
    /// Allow cycles (skip cycle detection entirely).
    ///
    /// This is useful when the caller guarantees no cycles, or when cycles
    /// are intentionally allowed. This has a small performance benefit as
    /// no reachability check is performed.
    Allow,
}

/// Dependency graph: "A depends on B" edges per channel.
///
/// `InvalidationGraph` stores bidirectional dependency edges, allowing O(1) queries
/// for both "what does A depend on?" and "what depends on A?". Dependencies
/// are stored per-channel, so layout dependencies can be independent of
/// paint dependencies.
///
/// # Type Parameters
///
/// - `K`: The key type, typically a node identifier. Must be `Copy + Eq + Hash + DenseKey`.
///   If your natural key is owned/structured, see [`intern::Interner`](crate::intern::Interner).
///
/// # Example
///
/// ```
/// use invalidation::{Channel, CycleHandling, InvalidationGraph};
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
///
/// // Node 2 depends on node 1 for layout
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// // Node 3 depends on node 2 for layout
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// // Query dependencies
/// assert!(graph.dependencies(2, LAYOUT).any(|k| k == 1));
/// assert!(graph.dependents(1, LAYOUT).any(|k| k == 2));
///
/// // Transitive dependents of node 1: [2, 3]
/// let transitive: Vec<_> = graph.transitive_dependents(1, LAYOUT).collect();
/// assert!(transitive.contains(&2));
/// assert!(transitive.contains(&3));
/// ```
///
/// # See Also
///
/// - [`InvalidationTracker`](crate::InvalidationTracker): Coordinator for graph,
///   set, cascade, and cross-channel workflows.
/// - [`CycleHandling`]: Cycle policy used by [`add_dependency`](Self::add_dependency).
/// - [`DrainSorted`](crate::DrainSorted): Drains invalidated keys in dependency order.
#[derive(Clone)]
pub struct InvalidationGraph<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Forward edges: `forward[channel][key.index()] -> keys` that `key` depends on.
    forward: [Vec<Vec<K>>; MAX_CHANNELS],
    /// Reverse edges: `reverse[channel][key.index()] -> keys` that depend on `key`.
    reverse: [Vec<Vec<K>>; MAX_CHANNELS],
    /// Cached channels where `key` has any dependencies.
    forward_channels: Vec<ChannelSet>,
    /// Cached channels where `key` has any dependents.
    reverse_channels: Vec<ChannelSet>,
}

impl<K> Default for InvalidationGraph<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn default() -> Self {
        Self::new()
    }
}

/// Ensures `vec` has at least `idx + 1` elements, filling with defaults.
#[inline]
fn grow<T: Default>(vec: &mut Vec<T>, idx: usize) {
    if idx >= vec.len() {
        let target_len = prepare_dense_growth(vec, idx, "dependency graph adjacency");
        vec.resize_with(target_len, T::default);
    }
}

/// Ensures the channel-set vec has at least `idx + 1` elements.
#[inline]
fn grow_channels(vec: &mut Vec<ChannelSet>, idx: usize) {
    if idx >= vec.len() {
        let target_len = prepare_dense_growth(vec, idx, "dependency graph channel cache");
        vec.resize(target_len, ChannelSet::EMPTY);
    }
}

impl<K> InvalidationGraph<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Creates a new empty dependency graph.
    #[must_use]
    pub fn new() -> Self {
        Self {
            forward: core::array::from_fn(|_| Vec::new()),
            reverse: core::array::from_fn(|_| Vec::new()),
            forward_channels: Vec::new(),
            reverse_channels: Vec::new(),
        }
    }

    /// Returns `true` if the graph has no dependencies.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward_channels.iter().all(|cs| cs.is_empty())
    }

    /// Adds a dependency: `from` depends on `to` in the given channel.
    ///
    /// When `to` becomes invalidated, `from` should be recomputed (in that channel).
    ///
    /// # Cycle Handling
    ///
    /// The `handling` parameter controls behavior when adding this dependency
    /// would create a cycle:
    ///
    /// - [`CycleHandling::DebugAssert`]: Panics in debug builds, ignores in release.
    /// - [`CycleHandling::Error`]: Returns `Err(CycleError)`.
    /// - [`CycleHandling::Ignore`]: Silently ignores the dependency.
    /// - [`CycleHandling::Allow`]: Skips cycle detection entirely.
    ///
    /// # Returns
    ///
    /// - `Ok(AddDependency::Added)` if the dependency was newly added.
    /// - `Ok(AddDependency::AlreadyPresent)` if the dependency already existed.
    /// - `Ok(AddDependency::CycleIgnored)` if a cycle would be created and
    ///   `handling` dropped the dependency.
    /// - `Err(CycleError)` if a cycle would be created and `handling` is `Error`.
    ///
    /// # See Also
    ///
    /// - [`CycleHandling`]: How cycles are treated.
    /// - [`CycleError`]: Returned when `handling` is [`CycleHandling::Error`].
    pub fn add_dependency(
        &mut self,
        from: K,
        to: K,
        channel: Channel,
        handling: CycleHandling,
    ) -> Result<AddDependency, CycleError<K>> {
        // Self-dependency is a trivial cycle
        if from == to {
            return self.handle_cycle(from, to, channel, handling);
        }

        // Check for cycles unless explicitly allowed
        if handling != CycleHandling::Allow && self.would_create_cycle(from, to, channel) {
            return self.handle_cycle(from, to, channel, handling);
        }

        let ch = channel.index() as usize;
        let from_idx = from.index();
        let to_idx = to.index();

        // Dedup: check if already present
        let fwd = &mut self.forward[ch];
        grow(fwd, from_idx);
        if fwd[from_idx].contains(&to) {
            return Ok(AddDependency::AlreadyPresent);
        }

        // Add forward edge
        fwd[from_idx].push(to);

        // Add reverse edge
        let rev = &mut self.reverse[ch];
        grow(rev, to_idx);
        rev[to_idx].push(from);

        // Update channel sets
        grow_channels(&mut self.forward_channels, from_idx);
        self.forward_channels[from_idx].insert(channel);

        grow_channels(&mut self.reverse_channels, to_idx);
        self.reverse_channels[to_idx].insert(channel);

        Ok(AddDependency::Added)
    }

    fn handle_cycle(
        &self,
        from: K,
        to: K,
        channel: Channel,
        handling: CycleHandling,
    ) -> Result<AddDependency, CycleError<K>> {
        match handling {
            CycleHandling::DebugAssert => {
                if cfg!(debug_assertions) {
                    let path = self.cycle_path(from, to, channel);
                    panic!(
                        "adding dependency {} -> {} in {channel:?} would create a cycle: {}{} \
                         (keys shown by DenseKey index); use CycleHandling::Error to handle \
                         cycles as errors",
                        from.index(),
                        to.index(),
                        from.index(),
                        IndexPath(&path),
                    );
                }
                Ok(AddDependency::CycleIgnored)
            }
            CycleHandling::Error => Err(CycleError {
                from,
                to,
                channel,
                path: self.cycle_path(from, to, channel),
            }),
            CycleHandling::Ignore | CycleHandling::Allow => Ok(AddDependency::CycleIgnored),
        }
    }

    /// The existing chain from `to` back to `from` that `from -> to` would close.
    fn cycle_path(&self, from: K, to: K, channel: Channel) -> Vec<K> {
        if from == to {
            return alloc::vec![from];
        }
        self.dependency_path(to, from, channel)
            .expect("a cycle was detected, so `to` depends on `from`")
    }

    /// Checks whether adding `from -> to` would create a cycle.
    ///
    /// This performs a DFS from `to` to see if `from` is reachable.
    fn would_create_cycle(&self, from: K, to: K, channel: Channel) -> bool {
        // A cycle would be created if `from` is reachable from `to`
        // (i.e., `to` already transitively depends on `from`)
        let mut visited = HashSet::new();
        let mut stack = Vec::new();
        stack.push(to);

        while let Some(current) = stack.pop() {
            if current == from {
                return true;
            }
            if !visited.insert(current) {
                continue;
            }

            // Follow forward edges from current
            let ch = channel.index() as usize;
            let fwd = &self.forward[ch];
            let idx = current.index();
            if idx < fwd.len() {
                stack.extend(fwd[idx].iter().copied());
            }
        }

        false
    }

    /// Removes a dependency: `from` no longer depends on `to` in the given channel.
    ///
    /// Returns `true` if the dependency existed and was removed.
    pub fn remove_dependency(&mut self, from: K, to: K, channel: Channel) -> bool {
        let ch = channel.index() as usize;
        let from_idx = from.index();
        let to_idx = to.index();

        // Remove forward edge
        let fwd = &mut self.forward[ch];
        let removed = if from_idx < fwd.len() {
            if let Some(pos) = fwd[from_idx].iter().position(|&k| k == to) {
                fwd[from_idx].swap_remove(pos);
                true
            } else {
                false
            }
        } else {
            false
        };

        if !removed {
            return false;
        }

        // Remove reverse edge
        let rev = &mut self.reverse[ch];
        if to_idx < rev.len()
            && let Some(pos) = rev[to_idx].iter().position(|&k| k == from)
        {
            rev[to_idx].swap_remove(pos);
        }

        // Update channel sets if the adjacency list became empty
        if fwd[from_idx].is_empty() && from_idx < self.forward_channels.len() {
            self.forward_channels[from_idx].remove(channel);
        }
        if to_idx < rev.len() && rev[to_idx].is_empty() && to_idx < self.reverse_channels.len() {
            self.reverse_channels[to_idx].remove(channel);
        }

        true
    }

    /// Replaces all direct dependencies of `from` in `channel`.
    ///
    /// This is a batch convenience for the common "set all deps" workflow.
    ///
    /// - Dependencies present in the old set but missing from `to` are removed.
    /// - Dependencies present in `to` but missing from the old set are added.
    /// - Dependencies present in both sets are left unchanged.
    /// - Duplicate keys in `to` are ignored.
    ///
    /// # Cycle Handling
    ///
    /// Cycle handling is applied while adding new dependencies. If adding a
    /// dependency returns `Err(CycleError)`, this method rolls back to the
    /// previous dependency set and returns the error.
    ///
    /// # Returns
    ///
    /// - `Ok(changes)` listing the dependencies added, removed, and dropped
    ///   because they would have created a cycle. When the set already equals
    ///   `to`, all three are empty.
    /// - `Err(CycleError)` if a cycle would be created and `handling` is `Error`
    ///   (in which case no changes are retained).
    pub fn replace_dependencies(
        &mut self,
        from: K,
        channel: Channel,
        to: impl IntoIterator<Item = K>,
        handling: CycleHandling,
    ) -> Result<DependencyChanges<K>, CycleError<K>> {
        // Collect new deps, deduplicating.
        let mut new_set: Vec<K> = Vec::new();
        for k in to {
            if !new_set.contains(&k) {
                new_set.push(k);
            }
        }

        let ch = channel.index() as usize;
        let from_idx = from.index();
        let fwd = &self.forward[ch];
        let old = if from_idx < fwd.len() {
            fwd[from_idx].as_slice()
        } else {
            &[]
        };

        let unchanged = old.len() == new_set.len() && old.iter().all(|dep| new_set.contains(dep));
        if unchanged {
            return Ok(DependencyChanges::default());
        }

        let mut to_remove: Vec<K> = Vec::new();
        for &dep in old {
            if !new_set.contains(&dep) {
                to_remove.push(dep);
            }
        }

        let mut to_add: Vec<K> = Vec::new();
        for &dep in &new_set {
            if !old.contains(&dep) {
                to_add.push(dep);
            }
        }

        // Remove stale edges first so cycle checks for additions observe the post-update graph.
        let mut removed: Vec<K> = Vec::new();
        for dep in to_remove.iter().copied() {
            if self.remove_dependency(from, dep, channel) {
                removed.push(dep);
            }
        }

        // Add new edges and roll back diff mutations on error.
        let mut added: Vec<K> = Vec::new();
        let mut cycles_ignored: Vec<K> = Vec::new();
        for dep in to_add.iter().copied() {
            match self.add_dependency(from, dep, channel, handling) {
                Ok(AddDependency::Added) => added.push(dep),
                Ok(AddDependency::AlreadyPresent) => {}
                Ok(AddDependency::CycleIgnored) => cycles_ignored.push(dep),
                Err(e) => {
                    for d in added {
                        let _ = self.remove_dependency(from, d, channel);
                    }
                    for d in removed {
                        let _ = self.add_dependency(from, d, channel, CycleHandling::Allow);
                    }
                    return Err(e);
                }
            }
        }

        Ok(DependencyChanges {
            added,
            removed,
            cycles_ignored,
        })
    }

    /// Removes a key entirely from the graph.
    ///
    /// This removes all dependencies involving `key`, both as a dependent
    /// and as a dependency.
    pub fn remove_key(&mut self, key: K) {
        let key_idx = key.index();

        // Remove forward edges: for each channel where key has dependencies
        let fwd_channels = if key_idx < self.forward_channels.len() {
            self.forward_channels[key_idx]
        } else {
            ChannelSet::EMPTY
        };
        for channel in fwd_channels {
            let ch = channel.index() as usize;

            // Collect deps before mutating
            let deps: Vec<K> = if key_idx < self.forward[ch].len() {
                core::mem::take(&mut self.forward[ch][key_idx])
            } else {
                Vec::new()
            };

            // Remove reverse entries for each dep
            for dep in deps {
                let dep_idx = dep.index();
                let rev = &mut self.reverse[ch];
                if dep_idx < rev.len() {
                    if let Some(pos) = rev[dep_idx].iter().position(|&k| k == key) {
                        rev[dep_idx].swap_remove(pos);
                    }
                    if rev[dep_idx].is_empty() && dep_idx < self.reverse_channels.len() {
                        self.reverse_channels[dep_idx].remove(channel);
                    }
                }
            }
        }
        if key_idx < self.forward_channels.len() {
            self.forward_channels[key_idx] = ChannelSet::EMPTY;
        }

        // Remove reverse edges: for each channel where key has dependents
        let rev_channels = if key_idx < self.reverse_channels.len() {
            self.reverse_channels[key_idx]
        } else {
            ChannelSet::EMPTY
        };
        for channel in rev_channels {
            let ch = channel.index() as usize;

            // Collect dependents before mutating
            let dependents: Vec<K> = if key_idx < self.reverse[ch].len() {
                core::mem::take(&mut self.reverse[ch][key_idx])
            } else {
                Vec::new()
            };

            // Remove forward entries for each dependent
            for dependent in dependents {
                let dep_idx = dependent.index();
                let fwd = &mut self.forward[ch];
                if dep_idx < fwd.len() {
                    if let Some(pos) = fwd[dep_idx].iter().position(|&k| k == key) {
                        fwd[dep_idx].swap_remove(pos);
                    }
                    if fwd[dep_idx].is_empty() && dep_idx < self.forward_channels.len() {
                        self.forward_channels[dep_idx].remove(channel);
                    }
                }
            }
        }
        if key_idx < self.reverse_channels.len() {
            self.reverse_channels[key_idx] = ChannelSet::EMPTY;
        }
    }

    /// Returns an iterator over the direct dependencies of `key` in the given channel.
    ///
    /// These are the keys that `key` depends on (i.e., if they become invalidated,
    /// `key` should be recomputed).
    ///
    /// The iteration order is not specified and may vary across runs or platforms.
    #[inline]
    pub fn dependencies(&self, key: K, channel: Channel) -> impl Iterator<Item = K> + '_ {
        let ch = channel.index() as usize;
        let fwd = &self.forward[ch];
        let idx = key.index();
        let slice = if idx < fwd.len() {
            fwd[idx].as_slice()
        } else {
            &[]
        };
        slice.iter().copied()
    }

    /// Returns an iterator over the direct dependents of `key` in the given channel.
    ///
    /// These are the keys that depend on `key` (i.e., if `key` becomes invalidated,
    /// they should be recomputed).
    ///
    /// The iteration order is not specified and may vary across runs or platforms.
    #[inline]
    pub fn dependents(&self, key: K, channel: Channel) -> impl Iterator<Item = K> + '_ {
        let ch = channel.index() as usize;
        let rev = &self.reverse[ch];
        let idx = key.index();
        let slice = if idx < rev.len() {
            rev[idx].as_slice()
        } else {
            &[]
        };
        slice.iter().copied()
    }

    /// Returns a shortest chain of dependencies from `from` to `to`, if `from`
    /// depends on `to` directly or transitively in `channel`.
    ///
    /// The chain starts with `from` and ends with `to`, and each key depends on
    /// the next. It answers "why does `from` depend on `to`?", and, read
    /// backwards, "why would invalidating `to` invalidate `from`?". A key
    /// trivially reaches itself: `dependency_path(k, k, ch)` is `Some([k])`.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, CycleHandling, InvalidationGraph};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut graph = InvalidationGraph::<u32>::new();
    /// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
    /// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
    ///
    /// assert_eq!(graph.dependency_path(3, 1, LAYOUT), Some(vec![3, 2, 1]));
    /// assert_eq!(graph.dependency_path(1, 3, LAYOUT), None);
    /// ```
    #[must_use]
    pub fn dependency_path(&self, from: K, to: K, channel: Channel) -> Option<Vec<K>> {
        // Breadth-first over forward edges, remembering how each key was reached.
        let mut reached_from: HashMap<K, K> = HashMap::new();
        let mut frontier = alloc::collections::VecDeque::new();
        frontier.push_back(from);
        reached_from.insert(from, from);

        while let Some(current) = frontier.pop_front() {
            if current == to {
                let mut path = alloc::vec![to];
                let mut key = to;
                while key != from {
                    key = reached_from[&key];
                    path.push(key);
                }
                path.reverse();
                return Some(path);
            }
            for dep in self.dependencies(current, channel) {
                if let hashbrown::hash_map::Entry::Vacant(e) = reached_from.entry(dep) {
                    e.insert(current);
                    frontier.push_back(dep);
                }
            }
        }

        None
    }

    /// Returns an iterator over every dependency edge in `channel`, as
    /// `(from, to)` pairs where `from` depends on `to`.
    ///
    /// The iteration order is not specified.
    pub fn edges(&self, channel: Channel) -> impl Iterator<Item = (K, K)> + '_ {
        let ch = channel.index() as usize;
        let reverse = &self.reverse[ch];
        self.forward[ch]
            .iter()
            .enumerate()
            .filter(|(_, deps)| !deps.is_empty())
            .flat_map(move |(from_idx, deps)| {
                // Keys cannot be rebuilt from indices; `from` is stored in the
                // reverse list of any of its dependencies.
                let from = reverse[deps[0].index()]
                    .iter()
                    .copied()
                    .find(|key| key.index() == from_idx)
                    .expect("every forward edge has a reverse edge");
                deps.iter().map(move |&to| (from, to))
            })
    }

    /// Returns the number of dependency edges in `channel`.
    ///
    /// This walks the channel's adjacency lists, so it costs time proportional
    /// to the key space; it is meant for diagnostics, not hot paths.
    #[must_use]
    pub fn edge_count(&self, channel: Channel) -> usize {
        self.forward[channel.index() as usize]
            .iter()
            .map(Vec::len)
            .sum()
    }

    /// Returns the channels that have at least one dependency edge.
    #[must_use]
    pub fn channels(&self) -> ChannelSet {
        self.forward_channels
            .iter()
            .fold(ChannelSet::EMPTY, |all, &channels| all | channels)
    }

    /// Formats the graph's edges per channel, naming channels with `names`.
    pub(crate) fn debug_with<'a>(&'a self, names: &'a ChannelNames) -> impl fmt::Debug + 'a
    where
        K: fmt::Debug,
    {
        GraphEdges { graph: self, names }
    }

    /// Returns an iterator over all transitive dependents of `key` in the given channel.
    ///
    /// This performs a DFS traversal and yields all keys that directly or
    /// indirectly depend on `key`.
    ///
    /// The iteration order is not specified and may vary across runs or platforms.
    pub fn transitive_dependents(&self, key: K, channel: Channel) -> impl Iterator<Item = K> + '_ {
        TransitiveDependentsIter::new(self, key, channel)
    }

    /// Calls `f` for each transitive dependent of `key`, using reusable scratch buffers.
    ///
    /// This is equivalent to iterating [`transitive_dependents`](Self::transitive_dependents),
    /// but allows the caller to reuse allocations across traversals.
    ///
    /// The iteration order is not specified and may vary across runs or platforms.
    ///
    /// # See Also
    ///
    /// - [`TraversalScratch`]: Reusable storage for this traversal.
    /// - [`EagerPolicy::propagate_with_scratch`](crate::EagerPolicy::propagate_with_scratch): Uses this helper.
    pub fn for_each_transitive_dependent(
        &self,
        key: K,
        channel: Channel,
        scratch: &mut TraversalScratch<K>,
        mut f: impl FnMut(K),
    ) {
        scratch.reset();
        scratch.stack.extend(self.dependents(key, channel));

        while let Some(next) = scratch.stack.pop() {
            if scratch.visited.insert(next) {
                f(next);
                scratch.stack.extend(self.dependents(next, channel));
            }
        }
    }

    /// Returns the set of channels in which `key` has any dependencies.
    #[must_use]
    pub fn dependency_channels(&self, key: K) -> ChannelSet {
        let idx = key.index();
        if idx < self.forward_channels.len() {
            self.forward_channels[idx]
        } else {
            ChannelSet::EMPTY
        }
    }

    /// Returns the set of channels in which `key` has any dependents.
    #[must_use]
    pub fn dependent_channels(&self, key: K) -> ChannelSet {
        let idx = key.index();
        if idx < self.reverse_channels.len() {
            self.reverse_channels[idx]
        } else {
            ChannelSet::EMPTY
        }
    }

    /// Returns `true` if `key` has any dependencies in the given channel.
    #[inline]
    #[must_use]
    pub fn has_dependencies(&self, key: K, channel: Channel) -> bool {
        let ch = channel.index() as usize;
        let fwd = &self.forward[ch];
        let idx = key.index();
        idx < fwd.len() && !fwd[idx].is_empty()
    }

    /// Returns `true` if `key` has any dependents in the given channel.
    #[must_use]
    pub fn has_dependents(&self, key: K, channel: Channel) -> bool {
        let ch = channel.index() as usize;
        let rev = &self.reverse[ch];
        let idx = key.index();
        idx < rev.len() && !rev[idx].is_empty()
    }

    /// Returns the in-degree of `key` in the given channel.
    ///
    /// The in-degree is the number of keys that `key` depends on.
    #[must_use]
    pub fn in_degree(&self, key: K, channel: Channel) -> usize {
        let ch = channel.index() as usize;
        let fwd = &self.forward[ch];
        let idx = key.index();
        if idx < fwd.len() { fwd[idx].len() } else { 0 }
    }

    /// Returns the out-degree of `key` in the given channel.
    ///
    /// The out-degree is the number of keys that depend on `key`.
    #[must_use]
    pub fn out_degree(&self, key: K, channel: Channel) -> usize {
        let ch = channel.index() as usize;
        let rev = &self.reverse[ch];
        let idx = key.index();
        if idx < rev.len() { rev[idx].len() } else { 0 }
    }

    /// Returns an iterator over all unique keys that have dependencies or dependents.
    ///
    /// Each key is yielded at most once, even if it appears in both the forward
    /// and reverse edge maps.
    ///
    /// The iteration order is not specified and may vary across runs or platforms.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        let mut seen = HashSet::new();
        let mut all_keys: Vec<K> = Vec::new();

        // `DenseKey` can map keys to indices, but cannot reconstruct keys from
        // indices. Every graph key appears in at least one opposite adjacency
        // list, so walking all stored edges is enough to recover the set.
        for ch in 0..MAX_CHANNELS {
            for inner in &self.forward[ch] {
                for &k in inner {
                    if seen.insert(k) {
                        all_keys.push(k);
                    }
                }
            }
            for inner in &self.reverse[ch] {
                for &k in inner {
                    if seen.insert(k) {
                        all_keys.push(k);
                    }
                }
            }
        }

        all_keys.into_iter()
    }
}

/// Iterator over transitive dependents using DFS.
struct TransitiveDependentsIter<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
    visited: HashSet<K>,
    stack: Vec<K>,
}

impl<'a, K> TransitiveDependentsIter<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn new(graph: &'a InvalidationGraph<K>, start: K, channel: Channel) -> Self {
        let mut iter = Self {
            graph,
            channel,
            visited: HashSet::new(),
            stack: Vec::new(),
        };
        // Initialize with direct dependents
        iter.stack.extend(graph.dependents(start, channel));
        iter
    }
}

impl<K> Iterator for TransitiveDependentsIter<'_, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(key) = self.stack.pop() {
            if self.visited.insert(key) {
                // Push dependents of this key
                self.stack.extend(self.graph.dependents(key, self.channel));
                return Some(key);
            }
        }
        None
    }
}

impl<K> fmt::Debug for InvalidationGraph<K>
where
    K: Copy + Eq + Hash + DenseKey + fmt::Debug,
{
    /// Lists the edges of each channel that has any, as `from -> to`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InvalidationGraph ")?;
        fmt::Debug::fmt(&self.debug_with(&ChannelNames::new()), f)
    }
}

/// Formats a graph as a map from channel to its edges.
struct GraphEdges<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    graph: &'a InvalidationGraph<K>,
    names: &'a ChannelNames,
}

impl<K> fmt::Debug for GraphEdges<'_, K>
where
    K: Copy + Eq + Hash + DenseKey + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.graph.channels().iter().map(|channel| {
                (
                    self.names.display(channel),
                    DebugEdges {
                        graph: self.graph,
                        channel,
                    },
                )
            }))
            .finish()
    }
}

/// Formats one channel's edges as a list of `from -> to`.
struct DebugEdges<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
}

impl<K> fmt::Debug for DebugEdges<'_, K>
where
    K: Copy + Eq + Hash + DenseKey + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(
                self.graph
                    .edges(self.channel)
                    .map(|(from, to)| Edge(from, to)),
            )
            .finish()
    }
}

/// A dependency edge formatted as `from -> to`.
pub(crate) struct Edge<K>(pub(crate) K, pub(crate) K);

impl<K: fmt::Debug> fmt::Debug for Edge<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} -> {:?}", self.0, self.1)
    }
}

/// Formats a cycle's continuation as ` -> a -> b` using dense key indices,
/// for messages about keys that need not implement `Debug`.
struct IndexPath<'a, K>(&'a [K]);

impl<K: DenseKey> fmt::Display for IndexPath<'_, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for key in self.0 {
            write!(f, " -> {}", key.index())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);
    const A11Y: Channel = Channel::new(2);

    #[test]
    fn add_and_query_dependencies() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Node 2 depends on node 1
        assert!(graph.dependencies(2, LAYOUT).any(|k| k == 1));
        // Node 1 has dependent node 2
        assert!(graph.dependents(1, LAYOUT).any(|k| k == 2));
        // Node 2 has dependent node 3
        assert!(graph.dependents(2, LAYOUT).any(|k| k == 3));
    }

    #[test]
    fn replace_dependencies_updates_in_place() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(10, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(10, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let changed = graph
            .replace_dependencies(10, LAYOUT, [3, 4], CycleHandling::Error)
            .unwrap();
        assert!(changed.changed());
        assert_eq!(changed.added, vec![3, 4]);
        let mut removed = changed.removed.clone();
        removed.sort_unstable();
        assert_eq!(removed, vec![1, 2]);

        let deps: Vec<_> = graph.dependencies(10, LAYOUT).collect();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&3));
        assert!(deps.contains(&4));
        assert!(!deps.contains(&1));
        assert!(!deps.contains(&2));
    }

    #[test]
    fn replace_dependencies_rolls_back_on_cycle_error() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 2 depends on 1.
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        // 1 depends on 3 (old dependency set for 1).
        graph
            .add_dependency(1, 3, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Replacing deps for 1 with [2] would create a 1 <-> 2 cycle.
        let err = graph
            .replace_dependencies(1, LAYOUT, [2], CycleHandling::Error)
            .unwrap_err();
        assert_eq!(err.from, 1);
        assert_eq!(err.to, 2);

        // Old deps of 1 are restored.
        let deps: Vec<_> = graph.dependencies(1, LAYOUT).collect();
        assert_eq!(deps, vec![3]);
        assert!(!graph.dependencies(1, LAYOUT).any(|k| k == 2));

        // Unrelated edges are unchanged.
        assert!(graph.dependencies(2, LAYOUT).any(|k| k == 1));
    }

    #[test]
    fn replace_dependencies_noop_when_set_unchanged_returns_false() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(10, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(10, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Duplicates and ordering differences should not count as a change.
        let changed = graph
            .replace_dependencies(10, LAYOUT, [2, 1, 2], CycleHandling::Error)
            .unwrap();
        assert!(!changed.changed());

        let deps: Vec<_> = graph.dependencies(10, LAYOUT).collect();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&1));
        assert!(deps.contains(&2));
    }

    #[test]
    fn replace_dependencies_rolls_back_mixed_delta_on_cycle_error() {
        let mut graph = InvalidationGraph::<u32>::new();
        // Make adding 1 -> 2 a cycle by first adding 2 -> 1.
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        // Old deps for 1 are {3, 4}.
        graph
            .add_dependency(1, 3, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(1, 4, LAYOUT, CycleHandling::Error)
            .unwrap();

        // New set would be {4, 2}: remove 3, keep 4, add 2 (cycle).
        let err = graph
            .replace_dependencies(1, LAYOUT, [4, 2], CycleHandling::Error)
            .unwrap_err();
        assert_eq!(err.from, 1);
        assert_eq!(err.to, 2);

        // Old set is restored after failed mixed-delta update.
        let deps: Vec<_> = graph.dependencies(1, LAYOUT).collect();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&3));
        assert!(deps.contains(&4));
        assert!(!deps.contains(&2));
    }

    #[test]
    fn replace_dependencies_is_channel_scoped() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(7, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(7, 9, PAINT, CycleHandling::Error)
            .unwrap();

        graph
            .replace_dependencies(7, LAYOUT, [2, 3], CycleHandling::Error)
            .unwrap();

        let layout: Vec<_> = graph.dependencies(7, LAYOUT).collect();
        assert_eq!(layout.len(), 2);
        assert!(layout.contains(&2));
        assert!(layout.contains(&3));
        assert!(!layout.contains(&1));

        // PAINT dependencies are unchanged.
        let paint: Vec<_> = graph.dependencies(7, PAINT).collect();
        assert_eq!(paint, vec![9]);
    }

    #[test]
    fn cycle_detection_error() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Adding 1 -> 3 would create a cycle: 1 -> 3 -> 2 -> 1
        let result = graph.add_dependency(1, 3, LAYOUT, CycleHandling::Error);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.from, 1);
        assert_eq!(err.to, 3);
        assert_eq!(err.path, vec![3, 2, 1]);
        assert_eq!(
            alloc::format!("{err}"),
            "adding dependency 1 -> 3 in Channel(0) would create a cycle: 1 -> 3 -> 2 -> 1"
        );
    }

    #[test]
    fn self_dependency_is_cycle() {
        let mut graph = InvalidationGraph::<u32>::new();

        let result = graph.add_dependency(1, 1, LAYOUT, CycleHandling::Error);
        assert_eq!(result.unwrap_err().path, vec![1]);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(
        expected = "adding dependency 1 -> 3 in Channel(0) would create a cycle: 1 -> 3 -> 2 -> 1"
    )]
    fn debug_assert_cycle_panic_names_the_cycle() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        let _ = graph.add_dependency(1, 3, LAYOUT, CycleHandling::DebugAssert);
    }

    #[test]
    fn dependency_path_is_shortest_and_channel_scoped() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 4 -> 3 -> 2 -> 1, and a shortcut 4 -> 1.
        for (from, to) in [(4, 3), (3, 2), (2, 1), (4, 1)] {
            graph
                .add_dependency(from, to, LAYOUT, CycleHandling::Error)
                .unwrap();
        }
        assert_eq!(graph.dependency_path(4, 1, LAYOUT), Some(vec![4, 1]));
        assert_eq!(graph.dependency_path(3, 1, LAYOUT), Some(vec![3, 2, 1]));
        assert_eq!(graph.dependency_path(2, 2, LAYOUT), Some(vec![2]));
        assert_eq!(graph.dependency_path(1, 4, LAYOUT), None);
        assert_eq!(graph.dependency_path(4, 1, PAINT), None);
    }

    #[test]
    fn cycle_ignore() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Ignore)
            .unwrap();

        // Self-cycle is silently ignored
        let result = graph.add_dependency(1, 1, LAYOUT, CycleHandling::Ignore);
        assert_eq!(result, Ok(AddDependency::CycleIgnored));
    }

    #[test]
    fn add_dependency_tells_duplicates_from_ignored_cycles() {
        let mut graph = InvalidationGraph::<u32>::new();
        let add = |graph: &mut InvalidationGraph<u32>, from, to| {
            graph.add_dependency(from, to, LAYOUT, CycleHandling::Ignore)
        };
        assert_eq!(add(&mut graph, 2, 1), Ok(AddDependency::Added));
        assert_eq!(add(&mut graph, 2, 1), Ok(AddDependency::AlreadyPresent));
        assert_eq!(add(&mut graph, 1, 2), Ok(AddDependency::CycleIgnored));
        assert_eq!(graph.dependency_path(1, 2, LAYOUT), None);
    }

    #[test]
    fn replace_dependencies_reports_ignored_cycles() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        let changes = graph
            .replace_dependencies(1, LAYOUT, [2, 3], CycleHandling::Ignore)
            .unwrap();
        assert_eq!(changes.added, vec![3]);
        assert_eq!(changes.cycles_ignored, vec![2]);
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn cycle_allow() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Allow)
            .unwrap();

        // Cycle is allowed
        let result = graph.add_dependency(1, 3, LAYOUT, CycleHandling::Allow);
        assert_eq!(result, Ok(AddDependency::Added));
    }

    #[test]
    fn remove_dependency() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        assert!(graph.dependencies(2, LAYOUT).any(|k| k == 1));

        let removed = graph.remove_dependency(2, 1, LAYOUT);
        assert!(removed);
        assert!(!graph.dependencies(2, LAYOUT).any(|k| k == 1));

        // Removing again returns false
        assert!(!graph.remove_dependency(2, 1, LAYOUT));
    }

    #[test]
    fn remove_key() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(2, 1, PAINT, CycleHandling::Error)
            .unwrap();

        graph.remove_key(2);

        // Node 2's dependencies are gone
        assert!(!graph.dependencies(2, LAYOUT).any(|_| true));
        // Node 1's dependents are gone
        assert!(!graph.dependents(1, LAYOUT).any(|_| true));
        // Node 3's dependencies are gone
        assert!(!graph.dependencies(3, LAYOUT).any(|_| true));
    }

    #[test]
    fn transitive_dependents() {
        let mut graph = InvalidationGraph::<u32>::new();

        // 1 <- 2 <- 3
        //      ^
        //      |
        //      4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let transitive: Vec<_> = graph.transitive_dependents(1, LAYOUT).collect();
        assert_eq!(transitive.len(), 3);
        assert!(transitive.contains(&2));
        assert!(transitive.contains(&3));
        assert!(transitive.contains(&4));
    }

    #[test]
    fn channel_independence() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        assert!(graph.has_dependencies(2, LAYOUT));
        assert!(!graph.has_dependencies(2, PAINT));
        assert!(graph.has_dependents(1, LAYOUT));
        assert!(!graph.has_dependents(1, PAINT));
    }

    #[test]
    fn in_out_degree() {
        let mut graph = InvalidationGraph::<u32>::new();

        // 3 depends on 1 and 2
        graph
            .add_dependency(3, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Node 3 has in-degree 2 (depends on 2 nodes)
        assert_eq!(graph.in_degree(3, LAYOUT), 2);
        // Node 1 has out-degree 1 (1 node depends on it)
        assert_eq!(graph.out_degree(1, LAYOUT), 1);
    }

    #[test]
    fn dependency_channels() {
        let mut graph = InvalidationGraph::<u32>::new();

        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(2, 1, PAINT, CycleHandling::Error)
            .unwrap();

        let channels = graph.dependency_channels(2);
        assert!(channels.contains(LAYOUT));
        assert!(channels.contains(PAINT));
        assert!(!channels.contains(A11Y));
    }

    #[test]
    fn keys_are_unique() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        let keys: Vec<_> = graph.keys().collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&1));
        assert!(keys.contains(&2));
    }

    #[test]
    #[should_panic(expected = "DenseKey index")]
    fn add_dependency_rejects_sparse_key_space() {
        let mut graph = InvalidationGraph::<usize>::new();

        let _ = graph.add_dependency(usize::MAX, 7, LAYOUT, CycleHandling::Error);
    }
}

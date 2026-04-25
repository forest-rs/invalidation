// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Combined invalidation tracker: graph + set convenience type.

use alloc::vec::Vec;
use core::hash::Hash;

use crate::cascade::{CascadeCycleError, ChannelCascade};
use crate::channel::Channel;
use crate::cross_channel::CrossChannelEdges;
use crate::drain::{DenseKey, DrainSorted, DrainSortedDeterministic};
use crate::drain_builder::{AnyOrder, DrainBuilder};
use crate::graph::{CycleError, CycleHandling, InvalidationGraph};
use crate::policy::PropagationPolicy;
use crate::scratch::TraversalScratch;
use crate::set::InvalidationSet;
use crate::trace::InvalidationTrace;

/// Combined invalidation tracker with dependency graph and invalidation set.
///
/// `InvalidationTracker` is a convenience type that bundles a [`InvalidationGraph`] and
/// [`InvalidationSet`] together, providing a unified API for common invalidation
/// operations.
///
/// # Type Parameters
///
/// - `K`: The key type, typically a node identifier. Must be `Copy + Eq + Hash + DenseKey`.
///   If your natural key is owned/structured, see [`intern::Interner`](crate::intern::Interner).
///
/// # Example
///
/// ```
/// use invalidation::{Channel, CycleHandling, InvalidationTracker, EagerPolicy};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
///
/// let mut tracker = InvalidationTracker::<u32>::new();
///
/// // Build dependency graph: 3 depends on 2, 2 depends on 1
/// tracker.add_dependency(2, 1, LAYOUT).unwrap();
/// tracker.add_dependency(3, 2, LAYOUT).unwrap();
///
/// // Mark with eager propagation (marks 1, 2, 3)
/// tracker.mark_with(1, LAYOUT, &EagerPolicy);
///
/// // Or mark manually without propagation
/// tracker.mark(1, PAINT);
///
/// // Drain in topological order: 1, 2, 3
/// let order: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
/// assert_eq!(order, vec![1, 2, 3]);
/// ```
///
/// # See Also
///
/// - [`InvalidationGraph`] and [`InvalidationSet`]: The underlying components.
/// - [`EagerPolicy`](crate::EagerPolicy) and [`LazyPolicy`](crate::LazyPolicy): Built-in propagation strategies.
/// - [`drain_sorted`](crate::drain_sorted) and [`drain_affected_sorted`](crate::drain_affected_sorted): Free-function drain helpers.
#[derive(Debug, Clone)]
pub struct InvalidationTracker<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// The dependency graph.
    graph: InvalidationGraph<K>,
    /// The invalidation set.
    invalidated: InvalidationSet<K>,
    /// How to handle cycles when adding dependencies.
    cycle_handling: CycleHandling,
    /// Channel-to-channel cascade rules.
    cascade: ChannelCascade,
    /// Cross-channel edges connecting (key, channel) pairs.
    cross_channel: CrossChannelEdges<K>,
}

impl<K> Default for InvalidationTracker<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K> InvalidationTracker<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Creates a configurable drain builder.
    ///
    /// This is the preferred entrypoint for combining options like determinism,
    /// targeted drains, and tracing without multiplying `drain_*` methods.
    ///
    /// # Example
    ///
    /// ```rust
    /// use invalidation::{
    ///     Channel, CycleHandling, InvalidationTracker, OneParentRecorder, TraversalScratch,
    /// };
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
    /// // 1 <- 2 <- 3
    /// tracker.add_dependency(2, 1, LAYOUT).unwrap();
    /// tracker.add_dependency(3, 2, LAYOUT).unwrap();
    ///
    /// // Mark only the root; dependents are expanded lazily at drain-time.
    /// tracker.mark_with(1, LAYOUT, &invalidation::LazyPolicy);
    ///
    /// // Unrelated invalidated roots outside the target remain invalidated.
    /// tracker.mark(9, LAYOUT);
    ///
    /// let mut scratch = TraversalScratch::new();
    /// let mut trace = OneParentRecorder::new();
    ///
    /// let order: Vec<_> = tracker
    ///     .drain(LAYOUT)
    ///     .affected()
    ///     .within_dependencies_of(3)
    ///     .deterministic()
    ///     .trace(&mut scratch, &mut trace)
    ///     .run()
    ///     .collect();
    ///
    /// assert_eq!(order, vec![1, 2, 3]);
    /// assert!(tracker.is_invalidated(9, LAYOUT));
    /// assert_eq!(trace.explain_path(3, LAYOUT).unwrap(), vec![1, 2, 3]);
    /// ```
    pub fn drain(&mut self, channel: Channel) -> DrainBuilder<'_, '_, '_, K, AnyOrder> {
        DrainBuilder::new(&mut self.invalidated, &self.graph, channel)
    }

    /// Creates a new empty invalidation tracker with default cycle handling.
    #[must_use]
    pub fn new() -> Self {
        Self::with_cycle_handling(CycleHandling::default())
    }

    /// Creates a new empty invalidation tracker with the specified cycle handling.
    #[must_use]
    pub fn with_cycle_handling(cycle_handling: CycleHandling) -> Self {
        Self {
            graph: InvalidationGraph::new(),
            invalidated: InvalidationSet::new(),
            cycle_handling,
            cascade: ChannelCascade::new(),
            cross_channel: CrossChannelEdges::new(),
        }
    }

    /// Creates a tracker from an existing dependency graph.
    ///
    /// This is useful when graph construction is owned by a separate setup
    /// phase, but invalidation state should still be managed by the tracker.
    ///
    /// The tracker uses the default cycle handling mode for future dependency
    /// mutations. The supplied graph is not revalidated.
    #[must_use]
    pub fn from_graph(graph: InvalidationGraph<K>) -> Self {
        Self::from_graph_with_cycle_handling(graph, CycleHandling::default())
    }

    /// Creates a tracker from an existing dependency graph and cycle policy.
    ///
    /// The `cycle_handling` value is used for future calls that mutate
    /// dependencies through the tracker. The supplied graph is not revalidated.
    #[must_use]
    pub fn from_graph_with_cycle_handling(
        graph: InvalidationGraph<K>,
        cycle_handling: CycleHandling,
    ) -> Self {
        Self {
            graph,
            invalidated: InvalidationSet::new(),
            cycle_handling,
            cascade: ChannelCascade::new(),
            cross_channel: CrossChannelEdges::new(),
        }
    }

    /// Returns a reference to the underlying dependency graph.
    #[inline]
    #[must_use]
    pub fn graph(&self) -> &InvalidationGraph<K> {
        &self.graph
    }

    /// Returns a reference to the underlying invalidation set.
    #[inline]
    #[must_use]
    pub fn invalidated(&self) -> &InvalidationSet<K> {
        &self.invalidated
    }

    /// Returns a mutable reference to the underlying invalidation set.
    #[inline]
    #[must_use]
    pub fn invalidated_mut(&mut self) -> &mut InvalidationSet<K> {
        &mut self.invalidated
    }

    /// Returns the current operation generation of the invalidation set.
    ///
    /// See [`InvalidationSet::generation`] for details.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.invalidated.generation()
    }

    /// Returns the current cycle handling mode.
    #[inline]
    #[must_use]
    pub fn cycle_handling(&self) -> CycleHandling {
        self.cycle_handling
    }

    /// Sets the cycle handling mode for future operations.
    #[inline]
    pub fn set_cycle_handling(&mut self, handling: CycleHandling) {
        self.cycle_handling = handling;
    }

    /// Adds a dependency: `from` depends on `to` in the given channel.
    ///
    /// Uses the tracker's configured cycle handling mode.
    ///
    /// See [`InvalidationGraph::add_dependency`] for details.
    pub fn add_dependency(
        &mut self,
        from: K,
        to: K,
        channel: Channel,
    ) -> Result<bool, CycleError<K>> {
        self.graph
            .add_dependency(from, to, channel, self.cycle_handling)
    }

    /// Adds a dependency with explicit cycle handling.
    ///
    /// See [`InvalidationGraph::add_dependency`] for details.
    pub fn add_dependency_with(
        &mut self,
        from: K,
        to: K,
        channel: Channel,
        handling: CycleHandling,
    ) -> Result<bool, CycleError<K>> {
        self.graph.add_dependency(from, to, channel, handling)
    }

    /// Removes a dependency: `from` no longer depends on `to`.
    ///
    /// See [`InvalidationGraph::remove_dependency`] for details.
    pub fn remove_dependency(&mut self, from: K, to: K, channel: Channel) -> bool {
        self.graph.remove_dependency(from, to, channel)
    }

    /// Removes a key from the graph, invalidation set, and cross-channel edges.
    ///
    /// This is useful when a node is removed from the tree entirely.
    pub fn remove_key(&mut self, key: K) {
        self.graph.remove_key(key);
        self.invalidated.remove_key(key);
        self.cross_channel.remove_key(key);
    }

    /// Replaces all direct dependencies of `from` in `channel`.
    ///
    /// This is a convenience wrapper around
    /// [`InvalidationGraph::replace_dependencies`](crate::InvalidationGraph::replace_dependencies) that uses the
    /// tracker's configured cycle handling mode.
    pub fn replace_dependencies(
        &mut self,
        from: K,
        channel: Channel,
        to: impl IntoIterator<Item = K>,
    ) -> Result<bool, CycleError<K>> {
        self.graph
            .replace_dependencies(from, channel, to, self.cycle_handling)
    }

    /// Replaces all direct dependencies of `from` in `channel`, with explicit cycle handling.
    ///
    /// See [`InvalidationGraph::replace_dependencies`](crate::InvalidationGraph::replace_dependencies) for
    /// behavior and rollback semantics.
    pub fn replace_dependencies_with(
        &mut self,
        from: K,
        channel: Channel,
        to: impl IntoIterator<Item = K>,
        handling: CycleHandling,
    ) -> Result<bool, CycleError<K>> {
        self.graph.replace_dependencies(from, channel, to, handling)
    }

    /// Adds a channel cascade rule: invalidation on `from` also marks `to`.
    ///
    /// Returns `Ok(true)` if the rule was newly added, `Ok(false)` if it
    /// already existed, or `Err(CascadeCycleError)` if adding the rule would
    /// create a cycle.
    ///
    /// See [`ChannelCascade::add_cascade`] for details.
    pub fn add_cascade(&mut self, from: Channel, to: Channel) -> Result<bool, CascadeCycleError> {
        self.cascade.add_cascade(from, to)
    }

    /// Removes a channel cascade rule.
    ///
    /// Returns `true` if the rule existed and was removed.
    pub fn remove_cascade(&mut self, from: Channel, to: Channel) -> bool {
        self.cascade.remove_cascade(from, to)
    }

    /// Returns a reference to the channel cascade rules.
    #[inline]
    #[must_use]
    pub fn cascade(&self) -> &ChannelCascade {
        &self.cascade
    }

    /// Adds a cross-channel dependency edge.
    ///
    /// When `from_key` is invalidated on `from_ch`, `to_key` will also be
    /// marked on `to_ch` (when using [`mark_with`](Self::mark_with)).
    ///
    /// Returns `true` if the edge was newly added.
    pub fn add_cross_dependency(
        &mut self,
        from_key: K,
        from_ch: Channel,
        to_key: K,
        to_ch: Channel,
    ) -> bool {
        self.cross_channel
            .add_edge(from_key, from_ch, to_key, to_ch)
    }

    /// Removes a cross-channel dependency edge.
    ///
    /// Returns `true` if the edge existed and was removed.
    pub fn remove_cross_dependency(
        &mut self,
        from_key: K,
        from_ch: Channel,
        to_key: K,
        to_ch: Channel,
    ) -> bool {
        self.cross_channel
            .remove_edge(from_key, from_ch, to_key, to_ch)
    }

    /// Returns a reference to the cross-channel edges.
    #[inline]
    #[must_use]
    pub fn cross_channel(&self) -> &CrossChannelEdges<K> {
        &self.cross_channel
    }

    /// Marks a key as invalidated without propagation.
    ///
    /// When cascade rules are configured, also marks the key on all
    /// transitively cascaded channels.
    ///
    /// Returns `true` if the key was newly marked invalidated on the
    /// primary channel (cascade marks are not reflected in the return value).
    #[inline]
    pub fn mark(&mut self, key: K, channel: Channel) -> bool {
        let result = self.invalidated.mark(key, channel);
        let cascaded = self.cascade.cascades_from(channel);
        if !cascaded.is_empty() {
            for ch in cascaded {
                self.invalidated.mark(key, ch);
            }
        }
        result
    }

    /// Marks a key as invalidated using the given propagation policy.
    ///
    /// The policy determines how invalidation spreads through the dependency
    /// graph. See [`PropagationPolicy`] for details.
    ///
    /// When cascade rules or cross-channel edges are configured, `mark_with`
    /// extends propagation across channels:
    ///
    /// 1. Runs the policy on `(key, channel)` and all cascaded channels.
    /// 2. For the root key and every key reachable via
    ///    [`InvalidationGraph::transitive_dependents`] that was actually
    ///    marked, follows cascade rules and cross-channel edges.
    /// 3. Repeats until no new `(key, channel)` pairs are discovered.
    ///
    /// This covers the built-in policies completely: [`EagerPolicy`](crate::EagerPolicy)
    /// marks exactly the graph-reachable dependents, and [`LazyPolicy`](crate::LazyPolicy)
    /// marks only the root. For custom policies, cross-channel follow-up is
    /// only defined for the root key and for graph-reachable keys on that
    /// channel that the policy actually marked. Keys marked outside the
    /// graph's reachability set will not have their cascade or cross-channel
    /// edges followed automatically.
    ///
    /// When no cascades or cross-channel edges are configured, this reduces
    /// to a single `policy.propagate()` call with no extra overhead.
    pub fn mark_with<P>(&mut self, key: K, channel: Channel, policy: &P)
    where
        P: PropagationPolicy<K>,
    {
        // Fast path: no cascades, no cross-channel edges — pure single-channel.
        if self.cascade.cascades_from(channel).is_empty() && self.cross_channel.is_empty() {
            policy.propagate(key, channel, &self.graph, &mut self.invalidated);
            return;
        }

        // Slow path: unified closure across channels.
        //
        // Worklist of (key, channel) pairs to propagate. For each pair we:
        // 1. Run the policy (same-channel propagation).
        // 2. For each key that ended up invalidated on that channel
        //    (root + dependents), check cascades and cross-channel edges.
        // 3. Enqueue any new (key, channel) pairs discovered.
        let mut worklist: Vec<(K, Channel)> = Vec::new();
        worklist.push((key, channel));
        let mut processed = hashbrown::HashSet::<(K, Channel)>::new();

        while let Some((k, ch)) = worklist.pop() {
            if !processed.insert((k, ch)) {
                continue;
            }

            // 1. Run the propagation policy on this (key, channel).
            policy.propagate(k, ch, &self.graph, &mut self.invalidated);

            // 2. Discover cross-channel targets from k and all of its
            //    transitive dependents that were actually marked.
            self.enqueue_cross_successors(k, ch, &processed, &mut worklist);

            // Enqueue targets for each transitive dependent of k on ch
            // that was actually marked (eager marks them, lazy does not).
            for dep in self.graph.transitive_dependents(k, ch) {
                if self.invalidated.is_invalidated(dep, ch) {
                    self.enqueue_cross_successors(dep, ch, &processed, &mut worklist);
                }
            }
        }
    }

    /// Returns `true` if the key is invalidated in the given channel.
    #[inline]
    #[must_use]
    pub fn is_invalidated(&self, key: K, channel: Channel) -> bool {
        self.invalidated.is_invalidated(key, channel)
    }

    /// Returns `true` if there are any invalidated keys in the given channel.
    #[inline]
    #[must_use]
    pub fn has_invalidated(&self, channel: Channel) -> bool {
        self.invalidated.has_invalidated(channel)
    }

    /// Returns `true` if there are no invalidated keys in any channel.
    #[inline]
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.invalidated.is_empty()
    }

    /// Drains invalidated keys in topological order using Kahn's algorithm.
    ///
    /// Keys are yielded in dependency order: a key is only yielded after
    /// all of its invalidated dependencies have been yielded. This ensures that
    /// when processing the invalidation set, a node is only processed after all
    /// of its dependencies have been processed.
    ///
    /// The channel is cleared eagerly when this iterator is created.
    ///
    /// Note: if the dependency subgraph induced by the invalidated keys contains a
    /// cycle, the drain will stall and some keys will not be yielded. You can
    /// detect this by exhausting the iterator and checking
    /// [`DrainSorted::completion`], or by using
    /// [`DrainSorted::collect_with_completion`].
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, InvalidationTracker, EagerPolicy};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.add_dependency(2, 1, LAYOUT).unwrap();
    /// tracker.add_dependency(3, 2, LAYOUT).unwrap();
    ///
    /// tracker.mark_with(1, LAYOUT, &EagerPolicy);
    ///
    /// // Process in order: 1, 2, 3
    /// for key in tracker.drain_sorted(LAYOUT) {
    ///     // recompute_layout(key);
    /// }
    /// ```
    pub fn drain_sorted(&mut self, channel: Channel) -> DrainSorted<'_, K> {
        // Keep this as a small, discoverable "easy mode" wrapper.
        //
        // For advanced drain workflows (determinism, targeted drains, tracing,
        // scratch reuse), prefer [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
        self.drain(channel).invalidated_only().run()
    }

    /// Drains all affected keys in topological order.
    ///
    /// Unlike [`drain_sorted`](Self::drain_sorted), this method first expands
    /// the invalidation set to include all transitive dependents of the marked keys.
    /// This is the correct drain method to use with [`LazyPolicy`](crate::LazyPolicy).
    ///
    /// Note: the yielded order is only deterministic up to dependency ordering.
    /// When multiple keys are simultaneously ready, the relative order among
    /// them is not specified and may vary across runs or platforms.
    ///
    /// # Algorithm
    ///
    /// 1. Collect all keys currently marked invalidated (the "roots").
    /// 2. Compute all transitive dependents of each root.
    /// 3. Return a topologically sorted drain over: roots ∪ dependents.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, InvalidationTracker, LazyPolicy};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.add_dependency(2, 1, LAYOUT).unwrap();
    /// tracker.add_dependency(3, 2, LAYOUT).unwrap();
    ///
    /// // Mark only the root with lazy policy
    /// tracker.mark_with(1, LAYOUT, &LazyPolicy);
    ///
    /// // drain_affected_sorted expands to all affected keys: 1, 2, 3
    /// let order: Vec<_> = tracker.drain_affected_sorted(LAYOUT).collect();
    /// assert_eq!(order, vec![1, 2, 3]);
    /// ```
    pub fn drain_affected_sorted(&mut self, channel: Channel) -> DrainSorted<'_, K> {
        // Keep this as a small, discoverable "easy mode" wrapper.
        //
        // For advanced drain workflows (determinism, targeted drains, tracing,
        // scratch reuse), prefer [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
        self.drain(channel).affected().run()
    }

    /// Drains all affected keys in topological order, while recording a trace.
    ///
    /// This is a convenience wrapper around
    /// [`drain_affected_sorted_with_trace`](crate::drain_affected_sorted_with_trace).
    pub fn drain_affected_sorted_with_trace<T>(
        &mut self,
        channel: Channel,
        scratch: &mut TraversalScratch<K>,
        trace: &mut T,
    ) -> DrainSorted<'_, K>
    where
        T: InvalidationTrace<K>,
    {
        // For advanced drain workflows, prefer [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
        self.drain(channel).affected().trace(scratch, trace).run()
    }

    /// Collects invalidated keys and returns a [`DrainSorted`] iterator.
    ///
    /// Unlike [`drain_sorted`](Self::drain_sorted), this method does not
    /// clear the invalidation set. It's useful when you need to iterate multiple
    /// times or want to keep the invalidation state.
    #[must_use]
    pub fn peek_sorted(&self, channel: Channel) -> DrainSorted<'_, K> {
        let cap = self.invalidated.len(channel);
        DrainSorted::from_iter_with_capacity(
            self.invalidated.iter(channel),
            cap,
            &self.graph,
            channel,
        )
    }

    /// Clears all invalidated keys in the given channel.
    pub fn clear(&mut self, channel: Channel) {
        self.invalidated.clear(channel);
    }

    /// Clears all invalidated keys in all channels.
    pub fn clear_all(&mut self) {
        self.invalidated.clear_all();
    }

    /// Drains invalidated keys across channels in the given order.
    ///
    /// Within each channel, keys are yielded in topological order (like
    /// [`drain_sorted`](Self::drain_sorted)). Results are tagged with their
    /// channel.
    ///
    /// Channels with no invalidated keys are silently skipped.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, InvalidationTracker, EagerPolicy};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    /// const PAINT: Channel = Channel::new(1);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.add_dependency(2, 1, LAYOUT).unwrap();
    /// tracker.mark_with(1, LAYOUT, &EagerPolicy);
    /// tracker.mark(5, PAINT);
    ///
    /// let results = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
    /// assert_eq!(results, vec![(LAYOUT, 1), (LAYOUT, 2), (PAINT, 5)]);
    /// ```
    pub fn drain_channels_sorted(&mut self, order: &[Channel]) -> Vec<(Channel, K)> {
        let mut results = Vec::new();
        for &ch in order {
            for key in self.drain_sorted(ch) {
                results.push((ch, key));
            }
        }
        results
    }

    /// Returns all transitive dependents following same-channel edges,
    /// cascades, and cross-channel edges.
    ///
    /// At each visited `(key, channel)` the traversal:
    /// 1. Follows same-channel dependents from the [`InvalidationGraph`].
    /// 2. Applies cascade rules (same key, cascaded channels).
    /// 3. Follows cross-channel edges from [`CrossChannelEdges`].
    ///
    /// The iteration order is not specified and may vary across runs.
    /// The result is collected into a `Vec` to avoid complex iterator
    /// lifetime issues.
    pub fn transitive_dependents_cross(&self, key: K, channel: Channel) -> Vec<(K, Channel)> {
        use hashbrown::HashSet;

        let mut visited = HashSet::new();
        let mut queue = Vec::new();
        let mut result = Vec::new();

        // Seed with (key, channel).
        queue.push((key, channel));
        visited.insert((key, channel));

        while let Some((k, ch)) = queue.pop() {
            // 1. Same-channel dependents.
            for dep in self.graph.dependents(k, ch) {
                if visited.insert((dep, ch)) {
                    result.push((dep, ch));
                    queue.push((dep, ch));
                }
            }

            // 2. Cascade and cross-channel successors.
            self.for_each_cross_successor(k, ch, |next_key, next_ch| {
                if visited.insert((next_key, next_ch)) {
                    result.push((next_key, next_ch));
                    queue.push((next_key, next_ch));
                }
            });
        }

        result
    }

    /// Calls `f` for each cascade or cross-channel successor of `(key, channel)`.
    fn for_each_cross_successor(&self, key: K, channel: Channel, mut f: impl FnMut(K, Channel)) {
        for cascade_ch in self.cascade.cascades_from(channel) {
            f(key, cascade_ch);
        }

        for (to_key, to_ch) in self.cross_channel.dependents(key, channel) {
            f(to_key, to_ch);
        }
    }

    /// Enqueues cascade and cross-channel successors for `mark_with`, skipping
    /// pairs that have already run the propagation policy.
    fn enqueue_cross_successors(
        &self,
        key: K,
        channel: Channel,
        processed: &hashbrown::HashSet<(K, Channel)>,
        worklist: &mut Vec<(K, Channel)>,
    ) {
        self.for_each_cross_successor(key, channel, |to_key, to_ch| {
            if !processed.contains(&(to_key, to_ch)) {
                worklist.push((to_key, to_ch));
            }
        });
    }
}

impl<K> InvalidationTracker<K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    /// Drains invalidated keys in deterministic topological order.
    ///
    /// This is equivalent to [`drain_sorted`](Self::drain_sorted), but when
    /// multiple keys are simultaneously ready it yields them in ascending key
    /// order (`Ord`).
    pub fn drain_sorted_deterministic(
        &mut self,
        channel: Channel,
    ) -> DrainSortedDeterministic<'_, K> {
        // Keep this as a small, discoverable "easy mode" wrapper.
        //
        // For advanced drain workflows (targeted drains, tracing, scratch reuse),
        // prefer [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
        self.drain(channel).invalidated_only().deterministic().run()
    }

    /// Drains all affected keys in deterministic topological order.
    ///
    /// This is equivalent to [`drain_affected_sorted`](Self::drain_affected_sorted), but when
    /// multiple keys are simultaneously ready it yields them in ascending key
    /// order (`Ord`).
    pub fn drain_affected_sorted_deterministic(
        &mut self,
        channel: Channel,
    ) -> DrainSortedDeterministic<'_, K> {
        // Keep this as a small, discoverable "easy mode" wrapper.
        //
        // For advanced drain workflows (targeted drains, tracing, scratch reuse),
        // prefer [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
        self.drain(channel).affected().deterministic().run()
    }

    /// Collects invalidated keys and returns a deterministic [`DrainSortedDeterministic`] iterator.
    ///
    /// Unlike [`drain_sorted_deterministic`](Self::drain_sorted_deterministic), this method does
    /// not clear the invalidation set.
    #[must_use]
    pub fn peek_sorted_deterministic(&self, channel: Channel) -> DrainSortedDeterministic<'_, K> {
        let cap = self.invalidated.len(channel);
        DrainSortedDeterministic::from_iter_with_capacity(
            self.invalidated.iter(channel),
            cap,
            &self.graph,
            channel,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::policy::{EagerPolicy, LazyPolicy, PropagationPolicy};

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);

    struct OffGraphMarkPolicy {
        extra_key: u32,
    }

    impl PropagationPolicy<u32> for OffGraphMarkPolicy {
        fn propagate(
            &self,
            key: u32,
            channel: Channel,
            _graph: &InvalidationGraph<u32>,
            invalidated: &mut InvalidationSet<u32>,
        ) {
            invalidated.mark(key, channel);
            invalidated.mark(self.extra_key, channel);
        }
    }

    #[test]
    fn basic_workflow() {
        let mut tracker = InvalidationTracker::<u32>::new();

        // Build graph
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();

        // Mark with eager policy
        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
        assert!(tracker.is_invalidated(3, LAYOUT));

        // Drain in topological order
        let order: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
        assert_eq!(order, vec![1, 2, 3]);

        // Channel is now clean
        assert!(!tracker.has_invalidated(LAYOUT));
    }

    #[test]
    fn can_seed_tracker_from_graph() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut tracker =
            InvalidationTracker::from_graph_with_cycle_handling(graph, CycleHandling::Error);

        assert_eq!(tracker.cycle_handling(), CycleHandling::Error);
        assert!(tracker.graph().dependents(1, LAYOUT).any(|key| key == 2));

        tracker.mark_with(1, LAYOUT, &EagerPolicy);
        let order: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn manual_mark_no_propagation() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();

        // Manual mark - no propagation
        tracker.mark(1, LAYOUT);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(!tracker.is_invalidated(2, LAYOUT));
    }

    #[test]
    fn replace_dependencies_uses_configured_cycle_handling() {
        let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);

        // Self-dependency is a trivial cycle; this should error when the tracker
        // is configured with `CycleHandling::Error`.
        let err = tracker.replace_dependencies(1, LAYOUT, [1]).unwrap_err();
        assert_eq!(err.from, 1);
        assert_eq!(err.to, 1);
    }

    #[test]
    fn lazy_policy() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();

        // Lazy mark - only marks the key itself
        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(!tracker.is_invalidated(2, LAYOUT));
        assert!(!tracker.is_invalidated(3, LAYOUT));
    }

    #[test]
    fn remove_key() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, LAYOUT);

        tracker.remove_key(2);

        // Node 2 is gone from both graph and invalidation set
        assert!(!tracker.graph().dependents(1, LAYOUT).any(|_| true));
        assert!(!tracker.is_invalidated(2, LAYOUT));
        assert!(tracker.is_invalidated(1, LAYOUT));
    }

    #[test]
    fn peek_sorted_preserves_state() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, LAYOUT);

        // Peek does not clear
        let order: Vec<_> = tracker.peek_sorted(LAYOUT).collect();
        assert_eq!(order, vec![1, 2]);

        // Still invalidated
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
    }

    #[test]
    fn generation_tracking() {
        let mut tracker = InvalidationTracker::<u32>::new();
        let initial = tracker.generation();

        tracker.mark(1, LAYOUT);
        assert_eq!(tracker.generation(), initial + 1);

        tracker.mark(2, LAYOUT);
        assert_eq!(tracker.generation(), initial + 2);
    }

    #[test]
    fn cycle_handling_modes() {
        let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);

        tracker.add_dependency(2, 1, LAYOUT).unwrap();

        // Self-cycle should error
        let result = tracker.add_dependency(1, 1, LAYOUT);
        assert!(result.is_err());

        // Change to ignore mode
        tracker.set_cycle_handling(CycleHandling::Ignore);
        let result = tracker.add_dependency(1, 1, LAYOUT);
        assert!(result.is_ok());
    }

    #[test]
    fn multiple_channels() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(2, 1, PAINT).unwrap();

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // Only LAYOUT is invalidated
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
        assert!(!tracker.is_invalidated(1, PAINT));
        assert!(!tracker.is_invalidated(2, PAINT));

        tracker.mark_with(1, PAINT, &EagerPolicy);

        // Now both are invalidated
        assert!(tracker.is_invalidated(1, PAINT));
        assert!(tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn clear_specific_channel() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.mark(1, LAYOUT);
        tracker.mark(1, PAINT);

        tracker.clear(LAYOUT);

        assert!(!tracker.has_invalidated(LAYOUT));
        assert!(tracker.has_invalidated(PAINT));
    }

    #[test]
    fn clear_all() {
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.mark(1, LAYOUT);
        tracker.mark(1, PAINT);

        tracker.clear_all();

        assert!(tracker.is_clean());
    }

    const COMPOSITE: Channel = Channel::new(2);

    #[test]
    fn cascade_mark_propagates_to_cascaded_channels() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        tracker.mark(1, LAYOUT);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(1, PAINT));
    }

    #[test]
    fn cascade_mark_with_eager_propagates_across_channels() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(2, 1, PAINT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // LAYOUT: 1 and 2 (eager propagation).
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
        // PAINT: also 1 and 2 (cascade + eager propagation on PAINT).
        assert!(tracker.is_invalidated(1, PAINT));
        assert!(tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn cascade_mark_with_lazy() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        // Lazy: only marks the key itself, but on both channels.
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(1, PAINT));
        assert!(!tracker.is_invalidated(2, LAYOUT));
        assert!(!tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn cascade_transitive_chain() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cascade(PAINT, COMPOSITE).unwrap();

        tracker.mark(1, LAYOUT);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(1, PAINT));
        assert!(tracker.is_invalidated(1, COMPOSITE));
    }

    #[test]
    fn no_cascade_zero_overhead() {
        // Without cascades, mark should only affect the specified channel.
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.mark(1, LAYOUT);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(!tracker.is_invalidated(1, PAINT));
    }

    #[test]
    fn cascade_add_remove() {
        let mut tracker = InvalidationTracker::<u32>::new();
        assert!(tracker.add_cascade(LAYOUT, PAINT).unwrap());
        assert!(!tracker.add_cascade(LAYOUT, PAINT).unwrap()); // duplicate

        assert!(tracker.remove_cascade(LAYOUT, PAINT));
        assert!(!tracker.remove_cascade(LAYOUT, PAINT)); // already removed

        // After removal, no cascade.
        tracker.mark(1, LAYOUT);
        assert!(!tracker.is_invalidated(1, PAINT));
    }

    #[test]
    fn cascade_accessor() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        assert!(tracker.cascade().cascades_from(LAYOUT).contains(PAINT));
    }

    #[test]
    fn cross_channel_mark_with_follows_edges() {
        let mut tracker = InvalidationTracker::<u32>::new();
        // Node 1's LAYOUT output feeds node 2's PAINT input.
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // Node 1 on LAYOUT.
        assert!(tracker.is_invalidated(1, LAYOUT));
        // Node 2 on PAINT (via cross-channel edge).
        assert!(tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn cross_channel_with_same_channel_deps() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(3, 2, PAINT).unwrap();
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // Cross-channel: 2 on PAINT, then same-channel: 3 on PAINT.
        assert!(tracker.is_invalidated(2, PAINT));
        assert!(tracker.is_invalidated(3, PAINT));
    }

    #[test]
    fn cross_channel_with_cascade() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(1, PAINT, 2, COMPOSITE);

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // LAYOUT -> PAINT cascade marks 1 on PAINT.
        assert!(tracker.is_invalidated(1, PAINT));
        // Cross-channel from (1, PAINT) -> (2, COMPOSITE).
        assert!(tracker.is_invalidated(2, COMPOSITE));
    }

    #[test]
    fn cross_channel_remove_edge() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        assert!(tracker.remove_cross_dependency(1, LAYOUT, 2, PAINT));
        assert!(!tracker.remove_cross_dependency(1, LAYOUT, 2, PAINT));

        tracker.mark_with(1, LAYOUT, &EagerPolicy);
        assert!(!tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn cross_channel_remove_key_cleans_edges() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        tracker.mark(1, LAYOUT);

        tracker.remove_key(1);

        // Cross-channel edges are cleaned up.
        assert!(
            tracker
                .cross_channel()
                .dependents(1, LAYOUT)
                .next()
                .is_none()
        );
    }

    #[test]
    fn cross_channel_accessor() {
        let mut tracker = InvalidationTracker::<u32>::new();
        assert!(tracker.cross_channel().is_empty());
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        assert!(!tracker.cross_channel().is_empty());
    }

    #[test]
    fn drain_channels_sorted_basic() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.mark_with(1, LAYOUT, &EagerPolicy);
        tracker.mark(5, PAINT);

        let results = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
        assert_eq!(results, vec![(LAYOUT, 1), (LAYOUT, 2), (PAINT, 5)]);
    }

    #[test]
    fn drain_channels_sorted_empty_channels_skipped() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.mark(1, PAINT);

        let results = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
        assert_eq!(results, vec![(PAINT, 1)]);
    }

    #[test]
    fn drain_channels_sorted_respects_order() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, PAINT);

        // PAINT first, then LAYOUT.
        let results = tracker.drain_channels_sorted(&[PAINT, LAYOUT]);
        assert_eq!(results, vec![(PAINT, 2), (LAYOUT, 1)]);
    }

    #[test]
    fn drain_channels_sorted_clears_channels() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, PAINT);

        let _ = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);

        assert!(!tracker.has_invalidated(LAYOUT));
        assert!(!tracker.has_invalidated(PAINT));
    }

    #[test]
    fn transitive_dependents_cross_same_channel_only() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();

        let deps = tracker.transitive_dependents_cross(1, LAYOUT);
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&(2, LAYOUT)));
        assert!(deps.contains(&(3, LAYOUT)));
    }

    #[test]
    fn transitive_dependents_cross_with_cascade() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_dependency(2, 1, PAINT).unwrap();

        let deps = tracker.transitive_dependents_cross(1, LAYOUT);
        // Cascade: (1, LAYOUT) -> (1, PAINT).
        // Same-channel on PAINT: (1, PAINT) -> (2, PAINT).
        assert!(deps.contains(&(1, PAINT)));
        assert!(deps.contains(&(2, PAINT)));
    }

    #[test]
    fn transitive_dependents_cross_with_cross_edges() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        tracker.add_dependency(3, 2, PAINT).unwrap();

        let deps = tracker.transitive_dependents_cross(1, LAYOUT);
        // Cross-channel: (1, LAYOUT) -> (2, PAINT).
        // Same-channel on PAINT: (2, PAINT) -> (3, PAINT).
        assert!(deps.contains(&(2, PAINT)));
        assert!(deps.contains(&(3, PAINT)));
    }

    #[test]
    fn transitive_dependents_cross_combined() {
        let mut tracker = InvalidationTracker::<u32>::new();
        // Same-channel deps on LAYOUT.
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        // Cascade LAYOUT -> PAINT.
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        // Cross-channel: (2, LAYOUT) -> (3, COMPOSITE).
        tracker.add_cross_dependency(2, LAYOUT, 3, COMPOSITE);

        let deps = tracker.transitive_dependents_cross(1, LAYOUT);

        // Same-channel: (2, LAYOUT).
        assert!(deps.contains(&(2, LAYOUT)));
        // Cascade: (1, PAINT), (2, PAINT) (from visiting 1 and 2 on LAYOUT).
        assert!(deps.contains(&(1, PAINT)));
        assert!(deps.contains(&(2, PAINT)));
        // Cross-channel: (3, COMPOSITE).
        assert!(deps.contains(&(3, COMPOSITE)));
    }

    #[test]
    fn transitive_dependents_cross_no_duplicates() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        // Diamond: both cascade and cross-channel lead to (1, PAINT).
        tracker.add_cross_dependency(1, LAYOUT, 1, PAINT);

        let deps = tracker.transitive_dependents_cross(1, LAYOUT);
        // (1, PAINT) should appear exactly once.
        let paint_count = deps
            .iter()
            .filter(|&&(k, ch)| k == 1 && ch == PAINT)
            .count();
        assert_eq!(paint_count, 1);
    }

    #[test]
    fn cross_channel_from_propagated_dependent_not_just_root() {
        // Bug regression: cross-channel edges from eagerly-propagated
        // (non-root) keys must fire, not just edges from the root.
        let mut tracker = InvalidationTracker::<u32>::new();

        // 0 -> 1 on LAYOUT (1 depends on 0).
        tracker.add_dependency(1, 0, LAYOUT).unwrap();
        // Cross-channel edge from key 1 (not the root!).
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);

        tracker.mark_with(0, LAYOUT, &EagerPolicy);

        // Eager propagation marks 0 and 1 on LAYOUT.
        assert!(tracker.is_invalidated(0, LAYOUT));
        assert!(tracker.is_invalidated(1, LAYOUT));
        // The cross-channel edge from (1, LAYOUT) must fire.
        assert!(
            tracker.is_invalidated(2, PAINT),
            "cross-channel edge from propagated dependent must fire"
        );
    }

    #[test]
    fn cascade_applies_to_all_propagated_dependents_not_just_root() {
        // Bug regression: when eager propagation marks dependents, cascades
        // must apply to ALL of them, not just the root key.
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        // Both 1 and 2 are marked on LAYOUT (eager propagation).
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
        // Cascade must apply to BOTH keys, not just root.
        assert!(
            tracker.is_invalidated(1, PAINT),
            "cascade must apply to root"
        );
        assert!(
            tracker.is_invalidated(2, PAINT),
            "cascade must apply to propagated dependent"
        );
    }

    #[test]
    fn cascade_fires_from_cross_channel_target() {
        // Cross-channel edge leads to (2, PAINT), and PAINT cascades to
        // COMPOSITE. The cascade on the target channel must fire.
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        tracker.add_cascade(PAINT, COMPOSITE).unwrap();

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        assert!(tracker.is_invalidated(2, PAINT));
        assert!(
            tracker.is_invalidated(2, COMPOSITE),
            "cascade on cross-channel target channel must fire"
        );
    }

    #[test]
    fn chained_cross_channel_edges() {
        // (1, LAYOUT) -> (2, PAINT) -> (3, COMPOSITE)
        // mark_with(1, LAYOUT) must realize the full chain.
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        tracker.add_cross_dependency(2, PAINT, 3, COMPOSITE);

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        assert!(tracker.is_invalidated(2, PAINT));
        assert!(
            tracker.is_invalidated(3, COMPOSITE),
            "chained cross-channel edges must be followed transitively"
        );
    }

    #[test]
    fn mark_with_parity_with_transitive_dependents_cross() {
        // The mark-time closure must match the query-time closure.
        // Every (key, channel) returned by transitive_dependents_cross
        // must be invalidated after mark_with with eager policy.
        let mut tracker = InvalidationTracker::<u32>::new();

        // Build a complex graph.
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(2, LAYOUT, 4, COMPOSITE);
        tracker.add_cross_dependency(4, COMPOSITE, 5, PAINT);
        tracker.add_dependency(6, 5, PAINT).unwrap();

        // Query the expected closure.
        let expected = tracker.transitive_dependents_cross(1, LAYOUT);

        // Mark and verify parity.
        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        for (k, ch) in &expected {
            assert!(
                tracker.is_invalidated(*k, *ch),
                "mark_with must realize ({k}, {ch:?}) from transitive_dependents_cross"
            );
        }
    }

    #[test]
    fn lazy_policy_does_not_cascade_or_cross_channel_dependents() {
        // With lazy policy, only the root key is marked. Dependents are
        // NOT marked, so cascades/cross-channel from dependents must NOT fire.
        let mut tracker = InvalidationTracker::<u32>::new();

        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(2, LAYOUT, 3, COMPOSITE);

        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        // Root key 1 is marked on LAYOUT and cascaded to PAINT.
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(1, PAINT));
        // Key 2 is NOT marked (lazy doesn't propagate).
        assert!(!tracker.is_invalidated(2, LAYOUT));
        // Cross-channel from (2, LAYOUT) must NOT fire (2 not invalidated).
        assert!(!tracker.is_invalidated(3, COMPOSITE));
    }

    #[test]
    fn custom_policy_off_graph_marks_do_not_trigger_cross_channel_follow_up() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(9, LAYOUT, 10, PAINT);

        let policy = OffGraphMarkPolicy { extra_key: 9 };
        tracker.mark_with(1, LAYOUT, &policy);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(9, LAYOUT));
        assert!(
            !tracker.is_invalidated(10, PAINT),
            "cross-channel traversal is not defined for off-graph keys marked by a custom policy"
        );
    }
}

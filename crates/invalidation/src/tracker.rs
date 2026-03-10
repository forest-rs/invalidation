// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Combined invalidation tracker: graph + set convenience type.

use core::hash::Hash;

use crate::channel::Channel;
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
        }
    }
    /// Returns a reference to the underlying dependency graph.
    #[inline]
    #[must_use]
    pub fn graph(&self) -> &InvalidationGraph<K> {
        &self.graph
    }

    /// Returns a mutable reference to the underlying dependency graph.
    #[inline]
    #[must_use]
    pub fn graph_mut(&mut self) -> &mut InvalidationGraph<K> {
        &mut self.graph
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

    /// Returns the current generation of the invalidation set.
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

    // -------------------------------------------------------------------------
    // Graph operations
    // -------------------------------------------------------------------------

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

    /// Removes a key from both the graph and the invalidation set.
    ///
    /// This is useful when a node is removed from the tree entirely.
    pub fn remove_key(&mut self, key: K) {
        self.graph.remove_key(key);
        self.invalidated.remove_key(key);
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

    // -------------------------------------------------------------------------
    // Invalidation marking
    // -------------------------------------------------------------------------

    /// Marks a key as invalidated without propagation.
    ///
    /// Returns `true` if the key was newly marked invalidated.
    #[inline]
    pub fn mark(&mut self, key: K, channel: Channel) -> bool {
        self.invalidated.mark(key, channel)
    }

    /// Marks a key as invalidated using the given propagation policy.
    ///
    /// The policy determines how invalidation spreads through the dependency
    /// graph. See [`PropagationPolicy`] for details.
    pub fn mark_with<P>(&mut self, key: K, channel: Channel, policy: &P)
    where
        P: PropagationPolicy<K>,
    {
        policy.propagate(key, channel, &self.graph, &mut self.invalidated);
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

    // -------------------------------------------------------------------------
    // Draining
    // -------------------------------------------------------------------------

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

    use crate::policy::{EagerPolicy, LazyPolicy};

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);

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
}

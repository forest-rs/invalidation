// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Propagation policies for invalidation marking.

use core::hash::Hash;

use crate::channel::Channel;
use crate::drain::DenseKey;
use crate::graph::InvalidationGraph;
use crate::scratch::TraversalScratch;
use crate::set::InvalidationSet;
use crate::trace::InvalidationTrace;

/// Trait for invalidation propagation policies.
///
/// A propagation policy determines how invalidation spreads through the
/// dependency graph. When a key is marked invalidated, the policy can choose
/// to immediately propagate to all dependents (eager), defer propagation
/// (lazy), or implement custom strategies.
///
/// A [`PropagationPolicy`] only controls same-channel propagation. When used
/// with [`InvalidationTracker::mark_with`](crate::InvalidationTracker::mark_with),
/// any cross-channel follow-up is performed by the tracker after the policy has
/// marked keys in the current channel.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, InvalidationGraph, InvalidationSet, EagerPolicy, PropagationPolicy,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// let eager = EagerPolicy;
///
/// // Mark node 1 invalidated with eager propagation
/// eager.propagate(1, LAYOUT, &graph, &mut invalidated);
///
/// // All transitive dependents are now invalidated
/// assert!(invalidated.is_invalidated(1, LAYOUT));
/// assert!(invalidated.is_invalidated(2, LAYOUT));
/// assert!(invalidated.is_invalidated(3, LAYOUT));
/// ```
pub trait PropagationPolicy<K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Propagates invalidation from `key` through the dependency graph.
    ///
    /// The policy is responsible for marking `key` itself and any dependents
    /// it determines should become invalidated. Both built-in policies
    /// ([`EagerPolicy`], [`LazyPolicy`]) mark the root key; custom
    /// implementations should do the same.
    ///
    /// This trait does not directly control cross-channel traversal. In
    /// [`InvalidationTracker::mark_with`](crate::InvalidationTracker::mark_with),
    /// cross-channel follow-up is only defined for the root key and for keys
    /// that are graph-reachable on the current channel and actually marked.
    ///
    /// # Parameters
    ///
    /// - `key`: The key to mark invalidated and propagate from.
    /// - `channel`: The channel in which invalidation occurs.
    /// - `graph`: The dependency graph (read-only).
    /// - `invalidated`: The invalidation set to mark keys in.
    fn propagate(
        &self,
        key: K,
        channel: Channel,
        graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
    );
}

/// Eager propagation policy: immediately mark all transitive dependents.
///
/// When a key is marked invalidated, `EagerPolicy` performs a DFS traversal of
/// the dependency graph and marks all transitive dependents as invalidated.
///
/// This is useful when you want to know the full invalidation set immediately
/// after marking, without waiting for drain time.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, InvalidationGraph, InvalidationSet, EagerPolicy, PropagationPolicy,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// // Chain: 1 <- 2 <- 3
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// let eager = EagerPolicy;
///
/// // Mark node 1, propagates to 2 and 3
/// eager.propagate(1, LAYOUT, &graph, &mut invalidated);
///
/// assert!(invalidated.is_invalidated(1, LAYOUT));
/// assert!(invalidated.is_invalidated(2, LAYOUT));
/// assert!(invalidated.is_invalidated(3, LAYOUT));
/// ```
#[derive(Copy, Clone, Debug, Default)]
pub struct EagerPolicy;

impl<K> PropagationPolicy<K> for EagerPolicy
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn propagate(
        &self,
        key: K,
        channel: Channel,
        graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
    ) {
        // Mark the key itself
        invalidated.mark(key, channel);

        // DFS to mark all transitive dependents
        for dependent in graph.transitive_dependents(key, channel) {
            invalidated.mark(dependent, channel);
        }
    }
}

impl EagerPolicy {
    /// Propagates using reusable scratch buffers.
    ///
    /// This is equivalent to calling [`PropagationPolicy::propagate`] for
    /// [`EagerPolicy`], but avoids per-call allocations by reusing `scratch`.
    ///
    /// # See Also
    ///
    /// - [`TraversalScratch`]: Reusable traversal storage.
    /// - [`InvalidationGraph::for_each_transitive_dependent`]: Scratch-powered traversal.
    pub fn propagate_with_scratch<K>(
        &self,
        key: K,
        channel: Channel,
        graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
        scratch: &mut TraversalScratch<K>,
    ) where
        K: Copy + Eq + Hash + DenseKey,
    {
        invalidated.mark(key, channel);
        graph.for_each_transitive_dependent(key, channel, scratch, |dependent| {
            invalidated.mark(dependent, channel);
        });
    }

    /// Propagates while recording a best-effort explanation trace.
    ///
    /// This performs an eager traversal over transitive dependents (like
    /// [`PropagationPolicy::propagate`]) while calling `trace` with the explicit
    /// root and one edge per discovered dependent.
    ///
    /// The trace is intended for debugging/explainability (e.g. "why is this key
    /// invalidated?"). It is not a complete provenance system: it records the
    /// traversal observed by this call.
    ///
    /// To avoid per-call allocations in hot loops, this method reuses the given
    /// [`TraversalScratch`].
    pub fn propagate_with_trace<K, T>(
        &self,
        key: K,
        channel: Channel,
        graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
        scratch: &mut TraversalScratch<K>,
        trace: &mut T,
    ) where
        K: Copy + Eq + Hash + DenseKey,
        T: InvalidationTrace<K>,
    {
        let newly_invalidated = invalidated.mark(key, channel);
        trace.root(key, channel, newly_invalidated);

        scratch.reset();
        scratch.stack.push(key);
        scratch.visited.insert(key);

        while let Some(current) = scratch.stack.pop() {
            for dependent in graph.dependents(current, channel) {
                if !scratch.visited.insert(dependent) {
                    continue;
                }
                let newly_invalidated = invalidated.mark(dependent, channel);
                trace.caused_by(dependent, current, channel, newly_invalidated);
                scratch.stack.push(dependent);
            }
        }
    }
}

/// Lazy propagation policy: only marks the key itself, no propagation.
///
/// `LazyPolicy` does not propagate invalidation at mark time. Only the
/// explicitly marked key is added to the invalidation set. To process all affected
/// keys (marked roots + their transitive dependents), use
/// [`drain_affected_sorted`](crate::drain_affected_sorted) or
/// [`InvalidationTracker::drain_affected_sorted`](crate::InvalidationTracker::drain_affected_sorted)
/// at drain time.
///
/// This is useful when many marks happen in succession and you want to
/// avoid redundant traversals. The tradeoff is that [`InvalidationSet::is_invalidated`]
/// will not reflect transitive invalidation state; only the explicitly marked
/// roots are in the invalidation set.
///
/// # Important
///
/// - Use [`drain_affected_sorted`](crate::drain_affected_sorted) (not `drain_sorted`)
///   to correctly process all affected keys when using `LazyPolicy`.
/// - Using `drain_sorted` with `LazyPolicy` will only process the marked roots,
///   not their dependents.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, InvalidationGraph, InvalidationSet, LazyPolicy, PropagationPolicy,
///     drain_affected_sorted,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// let lazy = LazyPolicy;
///
/// // Mark node 1 with lazy policy
/// lazy.propagate(1, LAYOUT, &graph, &mut invalidated);
///
/// // Only node 1 is marked (dependents not marked yet)
/// assert!(invalidated.is_invalidated(1, LAYOUT));
/// assert!(!invalidated.is_invalidated(2, LAYOUT));
///
/// // Use drain_affected_sorted to expand and process all affected keys
/// let affected: Vec<_> = drain_affected_sorted(&mut invalidated, &graph, LAYOUT).collect();
/// assert_eq!(affected, vec![1, 2, 3]); // All affected keys in topological order
/// ```
#[derive(Copy, Clone, Debug, Default)]
pub struct LazyPolicy;

impl<K> PropagationPolicy<K> for LazyPolicy
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn propagate(
        &self,
        key: K,
        channel: Channel,
        _graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
    ) {
        // Just mark the key, no propagation
        invalidated.mark(key, channel);
    }
}

/// Blanket implementation for boxed policies.
impl<K, P> PropagationPolicy<K> for &P
where
    K: Copy + Eq + Hash + DenseKey,
    P: PropagationPolicy<K> + ?Sized,
{
    fn propagate(
        &self,
        key: K,
        channel: Channel,
        graph: &InvalidationGraph<K>,
        invalidated: &mut InvalidationSet<K>,
    ) {
        (*self).propagate(key, channel, graph, invalidated);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    use crate::graph::CycleHandling;

    const LAYOUT: Channel = Channel::new(0);

    fn setup_chain_graph() -> InvalidationGraph<u32> {
        let mut graph = InvalidationGraph::<u32>::new();
        // Chain: 1 <- 2 <- 3 <- 4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
    }

    #[test]
    fn eager_policy_marks_all_dependents() {
        let graph = setup_chain_graph();
        let mut invalidated = InvalidationSet::new();
        let eager = EagerPolicy;

        eager.propagate(1, LAYOUT, &graph, &mut invalidated);

        assert!(invalidated.is_invalidated(1, LAYOUT));
        assert!(invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(3, LAYOUT));
        assert!(invalidated.is_invalidated(4, LAYOUT));
    }

    #[test]
    fn eager_policy_from_middle() {
        let graph = setup_chain_graph();
        let mut invalidated = InvalidationSet::new();
        let eager = EagerPolicy;

        eager.propagate(2, LAYOUT, &graph, &mut invalidated);

        // Node 1 is NOT invalidated (not a dependent)
        assert!(!invalidated.is_invalidated(1, LAYOUT));
        // Nodes 2, 3, 4 are invalidated
        assert!(invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(3, LAYOUT));
        assert!(invalidated.is_invalidated(4, LAYOUT));
    }

    #[test]
    fn lazy_policy_only_marks_key() {
        let graph = setup_chain_graph();
        let mut invalidated = InvalidationSet::new();
        let lazy = LazyPolicy;

        lazy.propagate(1, LAYOUT, &graph, &mut invalidated);

        assert!(invalidated.is_invalidated(1, LAYOUT));
        assert!(!invalidated.is_invalidated(2, LAYOUT));
        assert!(!invalidated.is_invalidated(3, LAYOUT));
        assert!(!invalidated.is_invalidated(4, LAYOUT));
    }

    #[test]
    fn policy_through_reference() {
        let graph = setup_chain_graph();
        let mut invalidated = InvalidationSet::new();
        let eager = EagerPolicy;
        let policy: &dyn PropagationPolicy<u32> = &eager;

        policy.propagate(1, LAYOUT, &graph, &mut invalidated);

        let invalidated_keys: Vec<_> = invalidated.iter(LAYOUT).collect();
        assert_eq!(invalidated_keys.len(), 4);
    }

    #[test]
    fn eager_handles_diamond() {
        let mut graph = InvalidationGraph::<u32>::new();
        // Diamond: 1 <- 2, 1 <- 3, 2 <- 4, 3 <- 4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        EagerPolicy.propagate(1, LAYOUT, &graph, &mut invalidated);

        assert!(invalidated.is_invalidated(1, LAYOUT));
        assert!(invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(3, LAYOUT));
        assert!(invalidated.is_invalidated(4, LAYOUT));
        // Node 4 should only appear once in the invalidation set
        assert_eq!(invalidated.len(LAYOUT), 4);
    }
}

// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Topologically sorted drain iterator.
//!
//! For more advanced drain workflows (determinism, targeted drains, tracing,
//! scratch reuse), prefer the builder-based API via
//! [`InvalidationTracker::drain`](crate::InvalidationTracker::drain) / [`DrainBuilder`].

use alloc::collections::BinaryHeap;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cmp::Reverse;
use core::hash::Hash;

use core::fmt;

use crate::DrainBuilder;
use crate::adaptive::InDegrees;
use crate::channel::Channel;
use crate::graph::InvalidationGraph;
use crate::scratch::TraversalScratch;
use crate::set::InvalidationSet;
use crate::trace::InvalidationTrace;

/// Trait for keys that can be used as dense `Vec` indices.
///
/// [`InvalidationGraph`] stores adjacency in `Vec`s indexed by key, and the
/// drains keep in-degrees the same way while the keys they drain are compact,
/// eliminating hashing from the hot path.
///
/// Keys with edges must map to compact sequential `usize` indices (typically
/// starting from 0): the graph's storage grows with the largest index that
/// has an edge, and very large indices panic if that storage would exceed
/// addressable capacity or the available memory budget. Keys without edges
/// can be anything. Use
/// [`intern::Interner`](crate::intern::Interner) when your natural key space
/// is not already compact.
///
/// [`InternId`](crate::InternId) and `u32` implement this trait.
pub trait DenseKey: Copy {
    /// Returns this key as a `usize` index for dense `Vec` storage.
    fn index(self) -> usize;
}

impl DenseKey for u32 {
    #[inline]
    fn index(self) -> usize {
        self as usize
    }
}

impl DenseKey for usize {
    #[inline]
    fn index(self) -> usize {
        self
    }
}

#[inline]
pub(crate) fn prepare_dense_growth<T>(vec: &mut Vec<T>, idx: usize, storage: &str) -> usize {
    let target_len = idx.checked_add(1).unwrap_or_else(|| {
        panic!("DenseKey index {idx} overflows addressable capacity for {storage}")
    });

    if target_len > vec.len() {
        vec.try_reserve_exact(target_len - vec.len()).unwrap_or_else(|err| {
            panic!(
                "DenseKey index {idx} requires growing {storage} to length {target_len}: {err:?}; use a compact dense key space or intern::Interner"
            )
        });
    }

    target_len
}

/// Indicates whether a drain finished normally or stalled due to a cycle.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DrainCompletion {
    /// All reachable keys were yielded.
    Complete,
    /// The drain stalled: some keys remained with non-zero in-degree (cycle).
    ///
    /// The keys that could not be yielded are listed by the drain's
    /// `remaining_keys`, and stay invalidated once the drain is dropped.
    Stalled {
        /// Number of keys that could not be yielded.
        remaining: usize,
    },
}

/// Iterator that yields invalidated keys in topological order.
///
/// Uses Kahn's algorithm to yield dependencies before their dependents.
/// This ensures that when processing the invalidation set, a node is only processed
/// after all of its dependencies have been processed.
///
/// # Algorithm
///
/// 1. Collect all invalidated keys and their in-degrees (within the invalidated subset).
/// 2. Initialize a queue with keys that have no invalidated dependencies.
/// 3. Repeatedly dequeue a key, yield it, and decrement in-degrees of its
///    invalidated dependents. When a dependent's in-degree reaches zero, enqueue it.
///
/// # Performance
///
/// - Time complexity: O(V + E) where V is the number of invalidated keys and E is
///   the number of edges between them.
/// - Space complexity: O(V). In-degrees live in a table indexed by
///   [`DenseKey::index`] while the drained keys are compact, and in a hash map
///   for keys too sparse for that, so a few keys with huge indices cost no
///   more than they would in a hash map.
///
/// # Keys Not Yielded
///
/// A drain takes its keys out of the invalidation set when it is created, and
/// owns them until it yields them. When the drain is dropped, every key it
/// has not yielded is marked invalidated again in its channel: a drain stopped
/// early (for example because processing a key failed) or stalled on a cycle
/// leaves that work pending rather than losing it. Use
/// [`remaining_keys`](Self::remaining_keys) to see which keys those are, and
/// clear the channel afterwards if the work should be discarded.
///
/// Drains created by [`InvalidationTracker::peek_sorted`](crate::InvalidationTracker::peek_sorted)
/// never took their keys, so they put nothing back.
///
/// # Important Notes
///
/// - **Duplicates**: If `invalidated_keys` contains duplicates, they are deduplicated
///   internally. Each key is yielded at most once.
/// - **Cycles**: This iterator assumes the dependency subgraph induced by the
///   invalidated keys is acyclic (a DAG). If cycles exist, keys involved in cycles
///   will never have their in-degree reach zero and will not be yielded.
///   You can detect this by exhausting the iterator and then checking
///   [`is_stalled`](Self::is_stalled) / [`completion`](Self::completion), or by
///   using [`collect_with_completion`](Self::collect_with_completion).
///   Use [`CycleHandling::Error`](crate::CycleHandling::Error) when building the graph
///   to prevent cycles, or ensure cycles are intentional.
/// - **Nondeterminism**: When multiple keys have in-degree zero simultaneously,
///   the order among them depends on the invalidated set's hash iteration
///   order and is not guaranteed to be deterministic across runs or platforms.
///
/// # Example
///
/// ```
/// use invalidation::{drain_sorted, Channel, CycleHandling, InvalidationGraph, InvalidationSet};
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// // 1 <- 2 <- 3
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// invalidated.mark(1, LAYOUT);
/// invalidated.mark(2, LAYOUT);
/// invalidated.mark(3, LAYOUT);
///
/// let sorted: Vec<_> = drain_sorted(&mut invalidated, &graph, LAYOUT).collect();
/// assert_eq!(sorted, vec![1, 2, 3]); // Dependencies before dependents
/// ```
#[derive(Debug)]
pub struct DrainSorted<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    kahn: Kahn<'a, K>,
    /// Keys with zero in-degree, ready to yield.
    ready: VecDeque<K>,
}

/// Deterministic variant of [`DrainSorted`].
///
/// When multiple keys are simultaneously ready, this drain yields the smallest
/// key first (according to `Ord`).
///
/// Like [`DrainSorted`], it marks the keys it has not yielded invalidated
/// again when it is dropped.
#[derive(Debug)]
pub struct DrainSortedDeterministic<'a, K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    kahn: Kahn<'a, K>,
    /// Keys with zero in-degree, ready to yield (min-heap via `Reverse`).
    ready: BinaryHeap<Reverse<K>>,
}

/// The state both drains share: Kahn's algorithm over the drained keys.
///
/// In-degrees live in a table indexed by [`DenseKey::index`] for compact
/// keys, so the hot path does no hashing, and in a hash map for keys too
/// sparse for that (see [`InDegrees`]).
struct Kahn<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
    /// In-degree of each key not yielded yet, within the drained subset.
    in_degree: InDegrees<K>,
    /// Every key the drain took, deduplicated.
    keys: Vec<K>,
    stalled: bool,
    /// The set that keys not yet yielded return to on drop; `None` for peeks.
    owner: Option<&'a mut InvalidationSet<K>>,
}

impl<K> fmt::Debug for Kahn<'_, K>
where
    K: Copy + Eq + Hash + DenseKey + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Kahn")
            .field("channel", &self.channel)
            .field("remaining", &self.in_degree.len())
            .field("stalled", &self.stalled)
            .finish_non_exhaustive()
    }
}

impl<'a, K> Kahn<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Takes `keys`, computes their in-degrees within the drained subset, and
    /// queues each key that is ready right away.
    fn new(
        mut keys: Vec<K>,
        graph: &'a InvalidationGraph<K>,
        channel: Channel,
        ready: &mut impl ReadyQueue<K>,
    ) -> Self {
        // Deduplicate the keys in place via the in-degree map.
        let mut in_degree = InDegrees::for_keys(&keys);
        keys.retain(|&key| in_degree.insert(key));
        let unique_keys = keys;

        // Compute in-degrees within the drained subset.
        for &key in &unique_keys {
            let degree = graph
                .dependencies(key, channel)
                .filter(|&dep| in_degree.get(dep).is_some())
                .count();
            *in_degree.get_mut(key).expect("every key was inserted") =
                u32::try_from(degree).expect("in-degree fits in u32");
        }

        // Reserve the queue only now that the input is consumed, so the two
        // are never allocated at once.
        ready.reserve(unique_keys.len());
        for &key in &unique_keys {
            if in_degree.get(key) == Some(0) {
                ready.push(key);
            }
        }

        Self {
            graph,
            channel,
            in_degree,
            keys: unique_keys,
            stalled: false,
            owner: None,
        }
    }

    /// Returns the number of keys not yielded yet.
    fn remaining(&self) -> usize {
        self.in_degree.len()
    }

    /// Yields `key`, queueing each dependent it makes ready.
    fn yield_key(&mut self, key: K, ready: &mut impl ReadyQueue<K>) {
        self.in_degree.remove(key);
        for dependent in self.graph.dependents(key, self.channel) {
            if let Some(degree) = self.in_degree.get_mut(dependent) {
                *degree -= 1;
                if *degree == 0 {
                    ready.push(dependent);
                }
            }
        }
    }

    /// Records that the ready queue ran dry.
    fn exhausted(&mut self) {
        if self.remaining() > 0 {
            self.stalled = true;
        }
    }

    /// Returns the keys not yielded yet.
    fn pending(&self) -> impl Iterator<Item = K> + '_ {
        self.keys
            .iter()
            .copied()
            .filter(|&key| self.in_degree.get(key).is_some())
    }

    fn remaining_keys(&self) -> Vec<K> {
        self.pending().collect()
    }

    fn completion(&self) -> DrainCompletion {
        if self.stalled {
            DrainCompletion::Stalled {
                remaining: self.remaining(),
            }
        } else {
            DrainCompletion::Complete
        }
    }
}

/// The queue of keys ready to yield: first-in, first-out for [`DrainSorted`],
/// smallest first for [`DrainSortedDeterministic`].
trait ReadyQueue<K> {
    fn reserve(&mut self, additional: usize);
    fn push(&mut self, key: K);
}

impl<K> ReadyQueue<K> for VecDeque<K> {
    fn reserve(&mut self, additional: usize) {
        Self::reserve(self, additional);
    }

    fn push(&mut self, key: K) {
        self.push_back(key);
    }
}

impl<K: Ord> ReadyQueue<K> for BinaryHeap<Reverse<K>> {
    fn reserve(&mut self, additional: usize) {
        Self::reserve(self, additional);
    }

    fn push(&mut self, key: K) {
        Self::push(self, Reverse(key));
    }
}

impl<K> Drop for Kahn<'_, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn drop(&mut self) {
        if self.remaining() == 0 {
            return;
        }
        if let Some(owner) = self.owner.take() {
            for key in self.pending() {
                owner.mark(key, self.channel);
            }
        }
    }
}

impl<'a, K> DrainSorted<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    pub(crate) fn from_keys(
        keys: Vec<K>,
        graph: &'a InvalidationGraph<K>,
        channel: Channel,
    ) -> Self {
        let mut ready = VecDeque::new();
        let kahn = Kahn::new(keys, graph, channel, &mut ready);
        Self { kahn, ready }
    }

    /// Makes the drain return the keys it has not yielded to `owner` on drop.
    pub(crate) fn returning_to(mut self, owner: &'a mut InvalidationSet<K>) -> Self {
        self.kahn.owner = Some(owner);
        self
    }

    /// Returns `true` if there are no more keys to yield.
    ///
    /// Note: if this returns `true` while [`remaining`](Self::remaining) is
    /// non-zero, the drain has stalled due to a cycle; see
    /// [`is_stalled`](Self::is_stalled).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    /// Returns the number of keys not yielded yet.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.kahn.remaining()
    }

    /// Returns the keys this drain has not yielded, in unspecified order.
    ///
    /// After a stall these are the keys on or behind a cycle. They are marked
    /// invalidated again when the drain is dropped, unless it is a peek,
    /// which never took them.
    #[must_use]
    pub fn remaining_keys(&self) -> Vec<K> {
        self.kahn.remaining_keys()
    }

    /// Returns `true` if the drain has stalled due to a cycle.
    ///
    /// This becomes `true` once `next()` has returned `None` while there were
    /// still keys remaining.
    ///
    /// This is only meaningful after the iterator has been exhausted.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        self.kahn.stalled
    }

    /// Returns whether the drain completed or stalled due to a cycle.
    ///
    /// If the drain stalled, this returns how many keys could not be yielded.
    ///
    /// This is only meaningful after the iterator has been exhausted.
    /// Prefer [`collect_with_completion`](Self::collect_with_completion) to
    /// avoid accidental early checks.
    #[must_use]
    pub fn completion(&self) -> DrainCompletion {
        self.kahn.completion()
    }

    /// Collects all yielded keys and returns completion status.
    #[must_use]
    pub fn collect_with_completion(mut self) -> (Vec<K>, DrainCompletion) {
        let mut out = Vec::with_capacity(self.kahn.remaining());
        out.extend(&mut self);
        let completion = self.completion();
        (out, completion)
    }
}

impl<'a, K> DrainSortedDeterministic<'a, K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    pub(crate) fn from_keys(
        keys: Vec<K>,
        graph: &'a InvalidationGraph<K>,
        channel: Channel,
    ) -> Self {
        let mut ready = BinaryHeap::new();
        let kahn = Kahn::new(keys, graph, channel, &mut ready);
        Self { kahn, ready }
    }

    /// Makes the drain return the keys it has not yielded to `owner` on drop.
    pub(crate) fn returning_to(mut self, owner: &'a mut InvalidationSet<K>) -> Self {
        self.kahn.owner = Some(owner);
        self
    }

    /// Returns the keys this drain has not yielded, in ascending order.
    ///
    /// After a stall these are the keys on or behind a cycle. They are marked
    /// invalidated again when the drain is dropped, unless it is a peek,
    /// which never took them.
    #[must_use]
    pub fn remaining_keys(&self) -> Vec<K> {
        let mut keys = self.kahn.remaining_keys();
        keys.sort_unstable();
        keys
    }

    /// Returns `true` if there are no more keys to yield.
    ///
    /// Note: if this returns `true` while [`remaining`](Self::remaining) is
    /// non-zero, the drain has stalled due to a cycle.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty()
    }

    /// Returns the number of keys not yielded yet.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.kahn.remaining()
    }

    /// Returns `true` if the drain has stalled due to a cycle.
    ///
    /// This becomes `true` once `next()` has returned `None` while there were
    /// still keys remaining.
    ///
    /// This is only meaningful after the iterator has been exhausted.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        self.kahn.stalled
    }

    /// Returns whether the drain completed or stalled due to a cycle.
    #[must_use]
    pub fn completion(&self) -> DrainCompletion {
        self.kahn.completion()
    }

    /// Collects all yielded keys and returns completion status.
    #[must_use]
    pub fn collect_with_completion(mut self) -> (Vec<K>, DrainCompletion) {
        let mut out = Vec::with_capacity(self.kahn.remaining());
        out.extend(&mut self);
        let completion = self.completion();
        (out, completion)
    }
}

impl<K> Iterator for DrainSorted<'_, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(key) = self.ready.pop_front() else {
            self.kahn.exhausted();
            return None;
        };
        self.kahn.yield_key(key, &mut self.ready);
        Some(key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Ready keys will all be yielded; blocked keys may never be, if they
        // sit on or behind a cycle.
        (self.ready.len(), Some(self.kahn.remaining()))
    }
}

impl<K> Iterator for DrainSortedDeterministic<'_, K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    type Item = K;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(Reverse(key)) = self.ready.pop() else {
            self.kahn.exhausted();
            return None;
        };
        self.kahn.yield_key(key, &mut self.ready);
        Some(key)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Ready keys will all be yielded; blocked keys may never be, if they
        // sit on or behind a cycle.
        (self.ready.len(), Some(self.kahn.remaining()))
    }
}

/// Creates a topologically sorted drain from a invalidation set.
///
/// This is a convenience function that takes the invalidated keys out of an
/// [`InvalidationSet`], and returns a [`DrainSorted`] iterator over them. The
/// iterator borrows the set: keys it does not yield are marked invalidated
/// again when it is dropped.
///
/// See [`DrainSorted`] for notes on cycles and nondeterministic ordering.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, InvalidationGraph, InvalidationSet, drain_sorted,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// invalidated.mark(1, LAYOUT);
/// invalidated.mark(2, LAYOUT);
///
/// // Drain in topological order
/// let sorted: Vec<_> = drain_sorted(&mut invalidated, &graph, LAYOUT).collect();
/// assert_eq!(sorted, vec![1, 2]);
///
/// // Channel is now empty
/// assert!(!invalidated.has_invalidated(LAYOUT));
/// ```
pub fn drain_sorted<'a, K>(
    invalidated: &'a mut InvalidationSet<K>,
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
) -> DrainSorted<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    DrainBuilder::new(invalidated, graph, channel)
        .invalidated_only()
        .run()
}

/// Creates a deterministic, topologically sorted drain from a invalidation set.
///
/// This is equivalent to [`drain_sorted`], but when multiple keys are ready
/// simultaneously it yields them in ascending key order (`Ord`).
pub fn drain_sorted_deterministic<'a, K>(
    invalidated: &'a mut InvalidationSet<K>,
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
) -> DrainSortedDeterministic<'a, K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    DrainBuilder::new(invalidated, graph, channel)
        .invalidated_only()
        .deterministic()
        .run()
}

/// Creates a topologically sorted drain that includes all affected keys.
///
/// Unlike [`drain_sorted`], this function expands the invalidation set to include
/// all transitive dependents of the marked keys before sorting. This is the
/// correct drain function to use with [`LazyPolicy`](crate::LazyPolicy),
/// which only marks roots at mark-time.
///
/// # Algorithm
///
/// 1. Collect all keys currently marked invalidated in the channel.
/// 2. For each invalidated key, compute all transitive dependents.
/// 3. Build the affected set: invalidated keys ∪ all transitive dependents.
/// 4. Return a topologically sorted drain over the affected set.
///
/// See [`DrainSorted`] for notes on cycles and nondeterministic ordering.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, InvalidationGraph, InvalidationSet, drain_affected_sorted,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// // 1 <- 2 <- 3
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// // Only mark the root
/// invalidated.mark(1, LAYOUT);
///
/// // drain_affected_sorted expands to include all dependents
/// let sorted: Vec<_> = drain_affected_sorted(&mut invalidated, &graph, LAYOUT).collect();
/// assert_eq!(sorted, vec![1, 2, 3]);
///
/// // Channel is now empty
/// assert!(!invalidated.has_invalidated(LAYOUT));
/// ```
pub fn drain_affected_sorted<'a, K>(
    invalidated: &'a mut InvalidationSet<K>,
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
) -> DrainSorted<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    DrainBuilder::new(invalidated, graph, channel)
        .affected()
        .run()
}

/// Creates a topologically sorted drain of all affected keys, while recording a trace.
///
/// This is a lazy-friendly explainability helper: it records one plausible
/// cause edge for each affected key **during** the drain-time expansion step.
///
/// Like [`drain_affected_sorted`], this takes the channel's keys and returns a
/// drain over: roots ∪ transitive dependents.
///
/// The trace is best-effort: when a key is reachable from multiple roots or via
/// multiple paths, this records the first path discovered by the traversal.
pub fn drain_affected_sorted_with_trace<'a, K, T>(
    invalidated: &'a mut InvalidationSet<K>,
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
    scratch: &mut TraversalScratch<K>,
    trace: &mut T,
) -> DrainSorted<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
    T: InvalidationTrace<K>,
{
    DrainBuilder::new(invalidated, graph, channel)
        .affected()
        .trace(scratch, trace)
        .run()
}

/// Creates a deterministic, topologically sorted drain that includes all
/// affected keys.
///
/// This is equivalent to [`drain_affected_sorted`], but when multiple keys are
/// ready simultaneously it yields them in ascending key order (`Ord`).
pub fn drain_affected_sorted_deterministic<'a, K>(
    invalidated: &'a mut InvalidationSet<K>,
    graph: &'a InvalidationGraph<K>,
    channel: Channel,
) -> DrainSortedDeterministic<'a, K>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    DrainBuilder::new(invalidated, graph, channel)
        .affected()
        .deterministic()
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    use crate::TraversalScratch;
    use crate::graph::CycleHandling;
    use crate::set::InvalidationSet;
    use crate::trace::OneParentRecorder;

    const LAYOUT: Channel = Channel::new(0);

    #[test]
    fn topological_order_chain() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2 <- 3 <- 4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Error)
            .unwrap();

        let invalidated_keys = vec![4, 2, 1, 3]; // Out of order
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // Must be in topological order
        assert_eq!(sorted, vec![1, 2, 3, 4]);
    }

    #[test]
    fn topological_order_diamond() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2, 1 <- 3, 2 <- 4, 3 <- 4
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

        let invalidated_keys = vec![4, 3, 2, 1];
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // 1 must come first, 4 must come last, 2 and 3 can be in either order
        assert_eq!(sorted[0], 1);
        assert_eq!(sorted[3], 4);
        assert!(sorted[1] == 2 || sorted[1] == 3);
        assert!(sorted[2] == 2 || sorted[2] == 3);
    }

    #[test]
    fn partial_invalidated_set() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2 <- 3
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Only 2 and 3 are invalidated (not 1)
        let invalidated_keys = vec![3, 2];
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // 2 should come before 3, but 1 is not in the invalidation set
        assert_eq!(sorted, vec![2, 3]);
    }

    #[test]
    fn no_dependencies() {
        let graph = InvalidationGraph::<u32>::new();
        let invalidated_keys = vec![3, 1, 2];
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // All have in-degree 0, so order is based on iteration order (set-dependent)
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn drain_sorted_function() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, LAYOUT);

        let sorted: Vec<_> = drain_sorted(&mut invalidated, &graph, LAYOUT).collect();
        assert_eq!(sorted, vec![1, 2]);

        // Channel should be empty
        assert!(!invalidated.has_invalidated(LAYOUT));
    }

    #[test]
    fn empty_invalidated_set() {
        let graph = InvalidationGraph::<u32>::new();
        let invalidated_keys: Vec<u32> = vec![];
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();
        assert!(sorted.is_empty());
    }

    #[test]
    fn size_hint_bounds() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        let invalidated_keys = vec![1, 2];
        let mut drain = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT);

        // Only 1 is ready; 2 waits on it.
        assert_eq!(drain.size_hint(), (1, Some(2)));
        assert_eq!(drain.remaining(), 2);

        let _ = drain.next();
        assert_eq!(drain.size_hint(), (1, Some(1)));

        let _ = drain.next();
        assert_eq!(drain.size_hint(), (0, Some(0)));
        assert!(drain.is_empty());
    }

    #[test]
    fn duplicate_keys_deduplicated() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Duplicates in input
        let invalidated_keys = vec![1, 2, 1, 2, 1];
        let sorted: Vec<_> = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // Should deduplicate to just [1, 2]
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted, vec![1, 2]);
    }

    #[test]
    fn cycles_stall_drain() {
        let mut graph = InvalidationGraph::<u32>::new();
        // Create a cycle: 1 <- 2 <- 3 <- 1 (with Allow)
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(1, 3, LAYOUT, CycleHandling::Allow)
            .unwrap();

        // All three are invalidated
        let invalidated_keys = vec![1, 2, 3];
        let mut drain = DrainSorted::from_keys(invalidated_keys, &graph, LAYOUT);
        let sorted: Vec<_> = drain.by_ref().collect();

        // All keys are in a cycle, so no key has in-degree 0, nothing is yielded
        assert!(
            sorted.is_empty(),
            "cycle should prevent any keys from being yielded"
        );
        assert!(drain.is_stalled());
        assert_eq!(
            drain.completion(),
            DrainCompletion::Stalled { remaining: 3 }
        );
    }

    #[test]
    fn cycles_stall_drain_collect_with_completion() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(1, 3, LAYOUT, CycleHandling::Allow)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        invalidated.mark(1, LAYOUT);
        invalidated.mark(2, LAYOUT);
        invalidated.mark(3, LAYOUT);

        let (sorted, completion) =
            drain_sorted(&mut invalidated, &graph, LAYOUT).collect_with_completion();
        assert!(sorted.is_empty());
        assert_eq!(completion, DrainCompletion::Stalled { remaining: 3 });
        assert_eq!(
            invalidated.len(LAYOUT),
            3,
            "keys a stalled drain could not yield stay invalidated"
        );
    }

    /// 1 <- 2 <- 3, plus a cycle 4 <-> 5 hanging off 3.
    fn graph_with_cycle_behind_chain() -> InvalidationGraph<u32> {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(4, 5, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
            .add_dependency(5, 4, LAYOUT, CycleHandling::Allow)
            .unwrap();
        graph
    }

    #[test]
    fn stalled_drain_names_the_keys_it_could_not_yield() {
        let graph = graph_with_cycle_behind_chain();
        let mut invalidated = InvalidationSet::new();
        for key in 1..=5 {
            invalidated.mark(key, LAYOUT);
        }

        let mut drain = drain_sorted_deterministic(&mut invalidated, &graph, LAYOUT);
        let yielded: Vec<_> = drain.by_ref().collect();
        assert_eq!(yielded, vec![1, 2, 3]);
        assert_eq!(
            drain.completion(),
            DrainCompletion::Stalled { remaining: 2 }
        );
        assert_eq!(drain.remaining_keys(), vec![4, 5]);
        drop(drain);

        let mut left: Vec<_> = invalidated.iter(LAYOUT).collect();
        left.sort_unstable();
        assert_eq!(left, vec![4, 5], "stalled keys stay invalidated");
    }

    #[test]
    fn stalled_unordered_drain_names_the_keys_it_could_not_yield() {
        let graph = graph_with_cycle_behind_chain();
        let mut invalidated = InvalidationSet::new();
        for key in 1..=5 {
            invalidated.mark(key, LAYOUT);
        }

        let mut drain = drain_sorted(&mut invalidated, &graph, LAYOUT);
        assert_eq!(drain.by_ref().count(), 3);
        let mut remaining = drain.remaining_keys();
        remaining.sort_unstable();
        assert_eq!(remaining, vec![4, 5]);
        drop(drain);

        assert!(invalidated.is_invalidated(4, LAYOUT));
        assert!(invalidated.is_invalidated(5, LAYOUT));
        assert!(!invalidated.is_invalidated(1, LAYOUT));
    }

    #[test]
    fn drain_stopped_early_keeps_unyielded_keys_invalidated() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        for key in 1..=3 {
            invalidated.mark(key, LAYOUT);
        }
        let first = drain_sorted(&mut invalidated, &graph, LAYOUT).next();
        assert_eq!(first, Some(1));
        assert!(!invalidated.is_invalidated(1, LAYOUT), "yielded keys leave");
        assert!(invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(3, LAYOUT));

        let first = drain_sorted_deterministic(&mut invalidated, &graph, LAYOUT).next();
        assert_eq!(first, Some(2));
        assert!(!invalidated.is_invalidated(2, LAYOUT));
        assert!(invalidated.is_invalidated(3, LAYOUT));
    }

    #[test]
    fn affected_drain_stopped_early_keeps_expanded_dependents_invalidated() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        // Only the root is marked, as with `LazyPolicy`.
        let mut invalidated = InvalidationSet::new();
        invalidated.mark(1, LAYOUT);
        let first = drain_affected_sorted(&mut invalidated, &graph, LAYOUT).next();
        assert_eq!(first, Some(1));

        // The drain took responsibility for 2 and 3; they are pending now.
        let mut left: Vec<_> = invalidated.iter(LAYOUT).collect();
        left.sort_unstable();
        assert_eq!(left, vec![2, 3]);
    }

    #[test]
    fn drain_affected_sorted_expands_dependents() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2 <- 3 <- 4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        // Only mark the root
        invalidated.mark(1, LAYOUT);

        // drain_affected_sorted should expand to include all dependents
        let sorted: Vec<_> = drain_affected_sorted(&mut invalidated, &graph, LAYOUT).collect();
        assert_eq!(sorted, vec![1, 2, 3, 4]);

        // Channel should be empty
        assert!(!invalidated.has_invalidated(LAYOUT));
    }

    #[test]
    fn drain_affected_sorted_multiple_roots() {
        let mut graph = InvalidationGraph::<u32>::new();
        // Two chains: 1 <- 2, 3 <- 4
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(4, 3, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        invalidated.mark(1, LAYOUT);
        invalidated.mark(3, LAYOUT);

        let sorted: Vec<_> = drain_affected_sorted(&mut invalidated, &graph, LAYOUT).collect();
        // Should include all 4 keys (order between chains is nondeterministic)
        assert_eq!(sorted.len(), 4);
    }

    #[test]
    fn deterministic_topological_order_diamond_is_total() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2, 1 <- 3, 2 <- 4, 3 <- 4
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

        let invalidated_keys: Vec<u32> = vec![4, 3, 2, 1];
        let sorted: Vec<_> =
            DrainSortedDeterministic::from_keys(invalidated_keys, &graph, LAYOUT).collect();

        // Deterministic tie-breaker yields 2 before 3.
        assert_eq!(sorted, vec![1, 2, 3, 4]);
    }

    #[test]
    fn drains_take_sparse_keys() {
        // Edges among small keys, and marked keys with huge indices. The
        // in-degree table covers only the compact keys.
        let mut graph = InvalidationGraph::<usize>::new();
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        let mut invalidated = InvalidationSet::new();
        for key in [2, 1, usize::MAX, usize::MAX / 2] {
            invalidated.mark(key, LAYOUT);
        }

        let mut drain = drain_sorted_deterministic(&mut invalidated, &graph, LAYOUT);
        assert!(
            drain.kahn.in_degree.table_slots() < 1024,
            "huge keys are hashed, not given table slots"
        );
        let first: Vec<_> = drain.by_ref().take(2).collect();
        assert_eq!(first, vec![1, 2]);
        assert_eq!(drain.remaining_keys(), vec![usize::MAX / 2, usize::MAX]);
        drop(drain);
        assert_eq!(invalidated.len(LAYOUT), 2, "the sparse keys were put back");

        let mut rest: Vec<_> = drain_sorted(&mut invalidated, &graph, LAYOUT).collect();
        rest.sort_unstable();
        assert_eq!(rest, vec![usize::MAX / 2, usize::MAX]);
        assert!(!invalidated.has_invalidated(LAYOUT));
    }

    #[test]
    fn affected_sorted_with_trace_records_one_path() {
        let mut graph = InvalidationGraph::<u32>::new();
        // 1 <- 2 <- 3
        graph
            .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        graph
            .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::new();
        invalidated.mark(1, LAYOUT);

        let mut scratch = TraversalScratch::new();
        let mut rec = OneParentRecorder::new();
        let sorted: Vec<_> = drain_affected_sorted_with_trace(
            &mut invalidated,
            &graph,
            LAYOUT,
            &mut scratch,
            &mut rec,
        )
        .collect();

        assert_eq!(sorted, vec![1, 2, 3]);
        assert_eq!(
            rec.explain_path(3, LAYOUT).unwrap(),
            vec![(1, LAYOUT), (2, LAYOUT), (3, LAYOUT)]
        );
    }

    #[test]
    fn size_hint_lower_bound_holds_on_cycles() {
        let graph = graph_with_cycle_behind_chain();
        let mut invalidated = InvalidationSet::new();
        for key in 1..=5 {
            invalidated.mark(key, LAYOUT);
        }

        // The lower bound must never exceed what is actually yielded.
        let mut drain = drain_sorted(&mut invalidated, &graph, LAYOUT);
        loop {
            let (lower, upper) = drain.size_hint();
            let rest = drain.remaining();
            assert_eq!(upper, Some(rest));
            match drain.next() {
                Some(_) => assert!(lower <= rest),
                None => {
                    assert_eq!(lower, 0, "nothing is ready when the drain stops");
                    break;
                }
            }
        }
        drop(drain);

        let mut drain = drain_sorted_deterministic(&mut invalidated, &graph, LAYOUT);
        assert_eq!(
            drain.size_hint(),
            (0, Some(2)),
            "only 4 and 5 are left, both blocked"
        );
        assert_eq!(drain.next(), None);
        assert_eq!(drain.by_ref().count(), 0);
    }
}

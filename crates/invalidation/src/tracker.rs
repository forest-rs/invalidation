// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Invalidation coordinator for graph, set, cascades, and cross-channel edges.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::hash::Hash;

use crate::cascade::{CascadeCycleError, ChannelCascade};
use crate::channel::{Channel, ChannelNames, ChannelSet};
use crate::cross_channel::CrossChannelEdges;
use crate::drain::{DenseKey, DrainSorted, DrainSortedDeterministic};
use crate::drain_builder::{AnyOrder, CrossRules, DrainBuilder};
use crate::graph::{
    AddDependency, CycleError, CycleHandling, DependencyChanges, InvalidationGraph,
};
use crate::policy::PropagationPolicy;
use crate::scratch::TraversalScratch;
use crate::set::InvalidationSet;
use crate::trace::{InvalidationCause, InvalidationTrace};

/// Coordinates graph dependencies, invalidation state, and cross-channel rules.
///
/// `InvalidationTracker` owns the common orchestration layer for invalidation:
/// a same-channel [`InvalidationGraph`], an [`InvalidationSet`], optional
/// [`ChannelCascade`] rules, and optional [`CrossChannelEdges`]. Use it when
/// marks should flow through those pieces as one coordinated operation.
///
/// If your embedder already owns propagation or scheduling policy, use
/// [`InvalidationGraph`] and [`InvalidationSet`] directly instead.
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
/// - [`InvalidationGraph`] and [`InvalidationSet`]: Lower-level primitives.
/// - [`ChannelCascade`] and [`CrossChannelEdges`]: Cross-channel coordination.
/// - [`EagerPolicy`](crate::EagerPolicy) and [`LazyPolicy`](crate::LazyPolicy): Built-in propagation strategies.
/// - [`drain_sorted`](crate::drain_sorted) and [`drain_affected_sorted`](crate::drain_affected_sorted): Free-function drain helpers.
#[derive(Clone)]
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
    /// Names for channels in diagnostics.
    channel_names: ChannelNames,
}

impl<K> core::fmt::Debug for InvalidationTracker<K>
where
    K: Copy + Eq + Hash + DenseKey + core::fmt::Debug,
{
    /// Shows the invalidated keys, dependency edges, cascades, and
    /// cross-channel edges of each channel that has any, with channels named
    /// by [`channel_names`](Self::channel_names).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let names = &self.channel_names;
        f.debug_struct("InvalidationTracker")
            .field("invalidated", &self.invalidated.debug_with(names))
            .field("generation", &self.invalidated.generation())
            .field("dependencies", &self.graph.debug_with(names))
            .field("cascades", &self.cascade.debug_with(names))
            .field("cross_channel", &self.cross_channel.debug_with(names))
            .field("cycle_handling", &self.cycle_handling)
            .finish()
    }
}

/// What [`InvalidationTracker::mark_with`] marked.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MarkReport {
    /// How many `(key, channel)` pairs became invalidated. Pairs that were
    /// already invalidated are not counted, so `0` means the call changed
    /// nothing.
    pub newly_invalidated: usize,
    /// The channels in which at least one key became invalidated.
    pub channels: ChannelSet,
    /// How many `(key, channel)` pairs the closure propagated from: the root,
    /// and each cascade or cross-channel target not already covered by an
    /// earlier step. `1` when no cascade or cross-channel edge was followed.
    pub steps: usize,
    /// How many dependents the closure walked, across all steps. With a
    /// policy that marks all dependents (see
    /// [`PropagationPolicy::marks_all_dependents`]) the closure marks them
    /// itself, so this is all of the traversal work; with other policies it
    /// is the walk for cascades and cross-channel edges to follow, and the
    /// policy's own traversal is not counted. `0` on the single-channel fast
    /// path, where the policy does all the work.
    pub keys_scanned: usize,
}

impl MarkReport {
    /// Returns `true` if no key became invalidated.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.newly_invalidated == 0
    }

    /// Runs `mark` on `channel` and records the keys it newly invalidated.
    fn record<K>(
        &mut self,
        invalidated: &mut InvalidationSet<K>,
        channel: Channel,
        mark: impl FnOnce(&mut InvalidationSet<K>),
    ) where
        K: Copy + Eq + Hash,
    {
        // Marking only ever adds keys, so the growth of the channel is the
        // number of keys newly invalidated in it.
        let before = invalidated.len(channel);
        mark(invalidated);
        self.add(channel, invalidated.len(channel).saturating_sub(before));
    }

    /// Records `added` keys newly invalidated in `channel`.
    fn add(&mut self, channel: Channel, added: usize) {
        if added > 0 {
            self.newly_invalidated += added;
            self.channels.insert(channel);
        }
    }
}

/// The result of [`InvalidationTracker::drain_channels_sorted`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DrainedChannels<K> {
    /// The drained keys in processing order, tagged with their channel.
    pub keys: Vec<(Channel, K)>,
    /// Channels whose drain stalled on a cycle. The keys they could not yield
    /// are still invalidated.
    pub stalled: ChannelSet,
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
    /// assert_eq!(
    ///     trace.explain_path(3, LAYOUT).unwrap(),
    ///     vec![(1, LAYOUT), (2, LAYOUT), (3, LAYOUT)]
    /// );
    /// ```
    pub fn drain(&mut self, channel: Channel) -> DrainBuilder<'_, '_, '_, K, AnyOrder> {
        let builder = DrainBuilder::new(&mut self.invalidated, &self.graph, channel);
        if self.cascade.is_empty() && self.cross_channel.is_empty() {
            return builder;
        }
        builder.with_rules(CrossRules {
            cascade: &self.cascade,
            edges: &self.cross_channel,
        })
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
            channel_names: ChannelNames::new(),
        }
    }

    /// Creates a tracker with static channel cascade rules.
    ///
    /// This is a convenience for systems with a fixed phase graph. Edges are
    /// applied with [`ChannelCascade::from_edges`]; duplicate edges are ignored,
    /// and cycles return [`CascadeCycleError`].
    pub fn with_cascades(
        cascades: impl IntoIterator<Item = (Channel, Channel)>,
    ) -> Result<Self, CascadeCycleError> {
        Ok(Self {
            graph: InvalidationGraph::new(),
            invalidated: InvalidationSet::new(),
            cycle_handling: CycleHandling::default(),
            cascade: ChannelCascade::from_edges(cascades)?,
            cross_channel: CrossChannelEdges::new(),
            channel_names: ChannelNames::new(),
        })
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
            channel_names: ChannelNames::new(),
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

    /// Returns the current operation generation of the invalidation set.
    ///
    /// See [`InvalidationSet::generation`] for details.
    #[inline]
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.invalidated.generation()
    }

    /// Names channels in this tracker's `Debug` output.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, ChannelNames, InvalidationTracker};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.set_channel_names(ChannelNames::new().with(LAYOUT, "layout"));
    /// tracker.mark(1, LAYOUT);
    /// assert!(format!("{tracker:?}").contains("invalidated: {layout: {1}}"));
    /// ```
    pub fn set_channel_names(&mut self, names: ChannelNames) {
        self.channel_names = names;
    }

    /// Returns the channel names used in `Debug` output.
    #[must_use]
    pub fn channel_names(&self) -> &ChannelNames {
        &self.channel_names
    }

    /// Returns the current cycle handling mode.
    #[inline]
    #[must_use]
    pub fn cycle_handling(&self) -> CycleHandling {
        self.cycle_handling
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
    ) -> Result<AddDependency, CycleError<K>> {
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
    ) -> Result<AddDependency, CycleError<K>> {
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
    ) -> Result<DependencyChanges<K>, CycleError<K>> {
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
    ) -> Result<DependencyChanges<K>, CycleError<K>> {
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

    /// Replaces all cross-channel dependents of `(from_key, from_ch)`.
    ///
    /// This updates the outgoing cross-channel edge set from one `(key,
    /// channel)` pair. Existing targets not present in `targets` are removed,
    /// new targets are added, and duplicates in `targets` are ignored.
    ///
    /// Returns `true` if the dependent set changed.
    pub fn replace_cross_dependents(
        &mut self,
        from_key: K,
        from_ch: Channel,
        targets: impl IntoIterator<Item = (K, Channel)>,
    ) -> bool {
        self.cross_channel
            .replace_dependents(from_key, from_ch, targets)
    }

    /// Removes all outgoing cross-channel edges from `(from_key, from_ch)`.
    ///
    /// Returns `true` if any edges existed and were removed.
    pub fn clear_cross_dependents(&mut self, from_key: K, from_ch: Channel) -> bool {
        self.cross_channel.clear_dependents(from_key, from_ch)
    }

    /// Removes all incoming cross-channel edges to `(to_key, to_ch)`.
    ///
    /// Returns `true` if any edges existed and were removed.
    pub fn clear_cross_dependencies(&mut self, to_key: K, to_ch: Channel) -> bool {
        self.cross_channel.clear_dependencies(to_key, to_ch)
    }

    /// Returns a reference to the cross-channel edges.
    #[inline]
    #[must_use]
    pub fn cross_channel(&self) -> &CrossChannelEdges<K> {
        &self.cross_channel
    }

    /// Marks a key as invalidated without graph or cross-channel propagation.
    ///
    /// This is a direct state update: it marks `(key, channel)` and, when
    /// channel cascade rules are configured, the same key on all transitively
    /// cascaded channels. It does not traverse same-channel graph dependents
    /// and does not follow [`CrossChannelEdges`].
    ///
    /// For normal "this key changed; mark the full invalidation closure"
    /// workflows, prefer [`mark_with`](Self::mark_with) with
    /// [`EagerPolicy`](crate::EagerPolicy) or [`LazyPolicy`](crate::LazyPolicy).
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

    /// Marks a key as invalidated and follows the full propagation closure.
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
    /// channel that are invalidated when the closure first walks past them.
    /// The closure walks past each key once per channel, so a key that a
    /// policy invalidates only in a later step (a policy that marks
    /// dependents to a limited depth, say) does not have its edges followed.
    /// Keys marked outside the graph's reachability set will not have their
    /// cascade or cross-channel edges followed automatically either.
    ///
    /// The closure walks each key's dependents at most once per channel, and
    /// with a policy whose
    /// [`marks_all_dependents`](PropagationPolicy::marks_all_dependents) is
    /// `true` it does not run the policy again on keys an earlier step
    /// already covered, so its work grows with the size of the closure.
    ///
    /// When no cascades or cross-channel edges are configured, this reduces
    /// to a single `policy.propagate()` call with no extra overhead.
    ///
    /// The returned [`MarkReport`] says how many `(key, channel)` pairs became
    /// invalidated, and in which channels.
    pub fn mark_with<P>(&mut self, key: K, channel: Channel, policy: &P) -> MarkReport
    where
        P: PropagationPolicy<K>,
    {
        self.mark_closure([(key, channel)], policy, None)
    }

    /// Marks many roots as one closure, like calling
    /// [`mark_with`](Self::mark_with) on each.
    ///
    /// The roots share one closure, so dependents, cascades, and
    /// cross-channel edges that several roots reach are walked once rather
    /// than once per root. With [`EagerPolicy`](crate::EagerPolicy), marking
    /// a batch of changed keys this way costs the size of their combined
    /// closure. The returned [`MarkReport`] covers the whole batch.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{Channel, EagerPolicy, InvalidationTracker};
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.add_dependency(3, 1, LAYOUT).unwrap();
    /// tracker.add_dependency(3, 2, LAYOUT).unwrap();
    ///
    /// let report = tracker.mark_many_with([(1, LAYOUT), (2, LAYOUT)], &EagerPolicy);
    /// assert_eq!(report.newly_invalidated, 3);
    /// assert_eq!(report.keys_scanned, 1, "3 is walked once, not once per root");
    /// ```
    pub fn mark_many_with<P>(
        &mut self,
        roots: impl IntoIterator<Item = (K, Channel)>,
        policy: &P,
    ) -> MarkReport
    where
        P: PropagationPolicy<K>,
    {
        self.mark_closure(roots, policy, None)
    }

    /// Marks a key like [`mark_with`](Self::mark_with), while reporting each
    /// `(key, channel)` the closure reaches to `trace`.
    ///
    /// Every step of the closure is reported with its cause: the root, a
    /// same-channel dependency (as reported by the policy's
    /// [`propagate_with_trace`](PropagationPolicy::propagate_with_trace)), a
    /// channel cascade, or a cross-channel edge. With a
    /// [`OneParentRecorder`](crate::OneParentRecorder),
    /// [`explain_path`](crate::OneParentRecorder::explain_path) then answers
    /// why any pair is invalidated, across channels. `scratch` is reused for
    /// the policy's traversals.
    ///
    /// # Example
    ///
    /// ```
    /// use invalidation::{
    ///     Channel, EagerPolicy, InvalidationCause, InvalidationTracker, OneParentRecorder,
    ///     TraversalScratch,
    /// };
    ///
    /// const LAYOUT: Channel = Channel::new(0);
    /// const PAINT: Channel = Channel::new(1);
    ///
    /// let mut tracker = InvalidationTracker::<u32>::new();
    /// tracker.add_cascade(LAYOUT, PAINT).unwrap();
    ///
    /// let mut scratch = TraversalScratch::new();
    /// let mut trace = OneParentRecorder::new();
    /// tracker.mark_with_trace(1, LAYOUT, &EagerPolicy, &mut scratch, &mut trace);
    ///
    /// assert_eq!(
    ///     trace.cause(1, PAINT),
    ///     Some(InvalidationCause::Cascade { channel: LAYOUT })
    /// );
    /// ```
    pub fn mark_with_trace<P, T>(
        &mut self,
        key: K,
        channel: Channel,
        policy: &P,
        scratch: &mut TraversalScratch<K>,
        trace: &mut T,
    ) -> MarkReport
    where
        P: PropagationPolicy<K>,
        T: InvalidationTrace<K>,
    {
        self.mark_closure([(key, channel)], policy, Some((scratch, trace)))
    }

    /// The closure behind [`mark_with`](Self::mark_with),
    /// [`mark_with_trace`](Self::mark_with_trace), and
    /// [`mark_many_with`](Self::mark_many_with).
    fn mark_closure<P>(
        &mut self,
        roots: impl IntoIterator<Item = (K, Channel)>,
        policy: &P,
        mut tracing: Option<(&mut TraversalScratch<K>, &mut dyn InvalidationTrace<K>)>,
    ) -> MarkReport
    where
        P: PropagationPolicy<K>,
    {
        let mut report = MarkReport::default();
        let mut roots = roots.into_iter();
        let Some((key, channel)) = roots.next() else {
            return report;
        };
        let second = roots.next();

        // Fast path: one root, no cascades, no cross-channel edges — pure
        // single-channel.
        if second.is_none()
            && self.cascade.cascades_from(channel).is_empty()
            && self.cross_channel.is_empty()
        {
            report.steps = 1;
            report.record(&mut self.invalidated, channel, |set| {
                run_policy(
                    policy,
                    key,
                    channel,
                    InvalidationCause::Root,
                    &self.graph,
                    set,
                    tracing.as_mut(),
                );
            });
            return report;
        }

        // Slow path: unified closure across channels.
        //
        // Worklist of (key, channel) pairs to propagate, with the cause that
        // reached them. For each pair we:
        // 1. Mark it and its dependents in its channel.
        // 2. For each key that ended up invalidated on that channel
        //    (root + dependents), check cascades and cross-channel edges.
        // 3. Enqueue any new (key, channel) pairs discovered.
        //
        // `scanned` holds the pairs whose dependents have already been walked.
        // A walk visits a key's whole subtree, so a later walk can stop at a
        // scanned key: every key below it that was invalidated then has had
        // its successors enqueued, and with the built-in policies nothing
        // below it becomes invalidated later except as a step of its own.
        //
        // The worklist is first-in, first-out so that a channel's shallower
        // keys run before the deeper keys they cover.
        let marks_all_dependents = policy.marks_all_dependents();
        let Self {
            graph,
            invalidated,
            cascade,
            cross_channel,
            ..
        } = self;
        let enqueue =
            |key: K,
             ch: Channel,
             processed: &hashbrown::HashSet<(K, Channel)>,
             worklist: &mut VecDeque<(K, Channel, InvalidationCause<K>)>| {
                for_each_cross_successor(
                    cascade,
                    cross_channel,
                    key,
                    ch,
                    |to_key, to_ch, cause| {
                        if !processed.contains(&(to_key, to_ch)) {
                            worklist.push_back((to_key, to_ch, cause));
                        }
                    },
                );
            };
        let mut worklist: VecDeque<(K, Channel, InvalidationCause<K>)> = VecDeque::new();
        for (key, channel) in core::iter::once((key, channel)).chain(second).chain(roots) {
            worklist.push_back((key, channel, InvalidationCause::Root));
        }
        let mut processed = hashbrown::HashSet::<(K, Channel)>::new();
        let mut scanned = hashbrown::HashSet::<(K, Channel)>::new();
        let mut stack: Vec<K> = Vec::new();

        while let Some((k, ch, cause)) = worklist.pop_front() {
            if !processed.insert((k, ch)) {
                continue;
            }

            if marks_all_dependents {
                // The policy would mark the key and every dependent, so the
                // closure does that itself, in one walk that all steps share:
                // a key an earlier step walked past is already marked along
                // with everything below it, and its successors are enqueued.
                let newly = !scanned.contains(&(k, ch));
                if !newly {
                    if let Some((_, trace)) = tracing.as_mut() {
                        trace.record(k, ch, cause, false);
                    }
                    continue;
                }
                scanned.insert((k, ch));
                report.steps += 1;
                let before = invalidated.len(ch);
                let newly_invalidated = invalidated.mark(k, ch);
                if let Some((_, trace)) = tracing.as_mut() {
                    trace.record(k, ch, cause, newly_invalidated);
                }
                enqueue(k, ch, &processed, &mut worklist);
                stack.push(k);
                while let Some(current) = stack.pop() {
                    for dep in graph.dependents(current, ch) {
                        if !scanned.insert((dep, ch)) {
                            continue;
                        }
                        report.keys_scanned += 1;
                        let newly_invalidated = invalidated.mark(dep, ch);
                        if let Some((_, trace)) = tracing.as_mut() {
                            trace.record(
                                dep,
                                ch,
                                InvalidationCause::Dependency { key: current },
                                newly_invalidated,
                            );
                        }
                        enqueue(dep, ch, &processed, &mut worklist);
                        stack.push(dep);
                    }
                }
                report.add(ch, invalidated.len(ch).saturating_sub(before));
                continue;
            }

            // 1. Run the propagation policy on this (key, channel).
            report.steps += 1;
            report.record(invalidated, ch, |set| {
                run_policy(policy, k, ch, cause, graph, set, tracing.as_mut());
            });

            // 2. Discover cross-channel targets from k and all of its
            //    transitive dependents that were actually marked.
            enqueue(k, ch, &processed, &mut worklist);
            if !scanned.insert((k, ch)) {
                continue;
            }
            stack.push(k);
            while let Some(current) = stack.pop() {
                for dep in graph.dependents(current, ch) {
                    if !scanned.insert((dep, ch)) {
                        continue;
                    }
                    report.keys_scanned += 1;
                    // Lazy marks only the root; keys below it are followed
                    // only if they were already invalidated.
                    if invalidated.is_invalidated(dep, ch) {
                        enqueue(dep, ch, &processed, &mut worklist);
                    }
                    stack.push(dep);
                }
            }
        }

        report
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
    /// The drain takes the channel's keys when it is created, and keys it does
    /// not yield are marked invalidated again when it is dropped, so stopping
    /// early leaves the rest of the work pending.
    ///
    /// Note: if the dependency subgraph induced by the invalidated keys contains a
    /// cycle, the drain will stall and some keys will not be yielded. You can
    /// detect this by exhausting the iterator and checking
    /// [`DrainSorted::completion`], or by using
    /// [`DrainSorted::collect_with_completion`];
    /// [`DrainSorted::remaining_keys`] names the stuck keys.
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
        DrainSorted::from_keys(
            self.invalidated.iter(channel).collect(),
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
    /// channel. Channels with no invalidated keys are skipped.
    ///
    /// A channel whose drain stalls on a cycle is listed in
    /// [`DrainedChannels::stalled`], and the keys it could not yield stay
    /// invalidated.
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
    /// let drained = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
    /// assert_eq!(drained.keys, vec![(LAYOUT, 1), (LAYOUT, 2), (PAINT, 5)]);
    /// assert!(drained.stalled.is_empty());
    /// ```
    pub fn drain_channels_sorted(&mut self, order: &[Channel]) -> DrainedChannels<K> {
        let mut drained = DrainedChannels {
            keys: Vec::new(),
            stalled: ChannelSet::EMPTY,
        };
        for &ch in order {
            let mut drain = self.drain_sorted(ch);
            drained.keys.extend((&mut drain).map(|key| (ch, key)));
            if drain.is_stalled() {
                drained.stalled.insert(ch);
            }
        }
        drained
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
            self.for_each_cross_successor(k, ch, |next_key, next_ch, _cause| {
                if visited.insert((next_key, next_ch)) {
                    result.push((next_key, next_ch));
                    queue.push((next_key, next_ch));
                }
            });
        }

        result
    }

    /// Calls `f` for each cascade or cross-channel successor of `(key, channel)`,
    /// with the cause that leads to it.
    fn for_each_cross_successor(
        &self,
        key: K,
        channel: Channel,
        f: impl FnMut(K, Channel, InvalidationCause<K>),
    ) {
        for_each_cross_successor(&self.cascade, &self.cross_channel, key, channel, f);
    }
}

/// Calls `f` for each cascade or cross-channel successor of `(key, channel)`,
/// with the cause that leads to it.
fn for_each_cross_successor<K>(
    cascade: &ChannelCascade,
    cross_channel: &CrossChannelEdges<K>,
    key: K,
    channel: Channel,
    mut f: impl FnMut(K, Channel, InvalidationCause<K>),
) where
    K: Copy + Eq + Hash,
{
    for cascade_ch in cascade.cascades_from(channel) {
        f(key, cascade_ch, InvalidationCause::Cascade { channel });
    }

    for (to_key, to_ch) in cross_channel.dependents(key, channel) {
        f(
            to_key,
            to_ch,
            InvalidationCause::CrossChannel { key, channel },
        );
    }
}

/// Runs `policy` on `(key, channel)`, reporting to the trace when there is one.
///
/// The policy reports `key` as a root; `cause` is what actually reached it in
/// the tracker's closure (a cascade or a cross-channel edge, for steps after
/// the first).
fn run_policy<K, P>(
    policy: &P,
    key: K,
    channel: Channel,
    cause: InvalidationCause<K>,
    graph: &InvalidationGraph<K>,
    invalidated: &mut InvalidationSet<K>,
    tracing: Option<&mut (&mut TraversalScratch<K>, &mut dyn InvalidationTrace<K>)>,
) where
    K: Copy + Eq + Hash + DenseKey,
    P: PropagationPolicy<K>,
{
    match tracing {
        None => policy.propagate(key, channel, graph, invalidated),
        Some((scratch, trace)) => {
            let mut step = StepTrace {
                inner: &mut **trace,
                key,
                channel,
                cause,
            };
            policy.propagate_with_trace(key, channel, graph, invalidated, scratch, &mut step);
        }
    }
}

/// Reports one step of the tracker's closure, replacing the policy's `Root`
/// for the step's own key with the cause that reached it.
struct StepTrace<'a, K> {
    inner: &'a mut dyn InvalidationTrace<K>,
    key: K,
    channel: Channel,
    cause: InvalidationCause<K>,
}

impl<K: Copy + Eq> InvalidationTrace<K> for StepTrace<'_, K> {
    fn record(
        &mut self,
        key: K,
        channel: Channel,
        cause: InvalidationCause<K>,
        newly_invalidated: bool,
    ) {
        let cause =
            if cause == InvalidationCause::Root && key == self.key && channel == self.channel {
                self.cause
            } else {
                cause
            };
        self.inner.record(key, channel, cause, newly_invalidated);
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
        DrainSortedDeterministic::from_keys(
            self.invalidated.iter(channel).collect(),
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
    fn can_seed_tracker_with_static_cascades() {
        let mut tracker =
            InvalidationTracker::<u32>::with_cascades([(LAYOUT, PAINT), (PAINT, COMPOSITE)])
                .unwrap();

        tracker.mark(1, LAYOUT);

        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(1, PAINT));
        assert!(tracker.is_invalidated(1, COMPOSITE));
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
    fn explicit_cycle_handling_can_override_tracker_default() {
        let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);

        tracker.add_dependency(2, 1, LAYOUT).unwrap();

        let result = tracker.add_dependency(1, 1, LAYOUT);
        assert!(result.is_err());

        let result = tracker.add_dependency_with(1, 1, LAYOUT, CycleHandling::Ignore);
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
    fn cross_channel_replace_dependents() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);
        tracker.add_cross_dependency(1, LAYOUT, 3, COMPOSITE);

        assert!(tracker.replace_cross_dependents(1, LAYOUT, [(3, COMPOSITE), (4, PAINT)]));
        assert!(!tracker.replace_cross_dependents(1, LAYOUT, [(4, PAINT), (3, COMPOSITE)]));

        tracker.mark_with(1, LAYOUT, &EagerPolicy);

        assert!(!tracker.is_invalidated(2, PAINT));
        assert!(tracker.is_invalidated(3, COMPOSITE));
        assert!(tracker.is_invalidated(4, PAINT));
    }

    #[test]
    fn cross_channel_clear_dependents_and_dependencies() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cross_dependency(1, LAYOUT, 3, PAINT);
        tracker.add_cross_dependency(2, COMPOSITE, 3, PAINT);

        assert!(tracker.clear_cross_dependents(1, LAYOUT));
        assert!(!tracker.clear_cross_dependents(1, LAYOUT));
        assert!(
            tracker
                .cross_channel()
                .dependencies(3, PAINT)
                .eq([(2, COMPOSITE)])
        );

        assert!(tracker.clear_cross_dependencies(3, PAINT));
        assert!(!tracker.clear_cross_dependencies(3, PAINT));
        assert!(
            tracker
                .cross_channel()
                .dependents(2, COMPOSITE)
                .next()
                .is_none()
        );
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
        assert_eq!(results.keys, vec![(LAYOUT, 1), (LAYOUT, 2), (PAINT, 5)]);
    }

    #[test]
    fn drain_channels_sorted_empty_channels_skipped() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.mark(1, PAINT);

        let results = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
        assert_eq!(results.keys, vec![(PAINT, 1)]);
    }

    #[test]
    fn drain_channels_sorted_respects_order() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, PAINT);

        // PAINT first, then LAYOUT.
        let results = tracker.drain_channels_sorted(&[PAINT, LAYOUT]);
        assert_eq!(results.keys, vec![(PAINT, 2), (LAYOUT, 1)]);
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

    #[test]
    fn drain_channels_sorted_reports_stalled_channels() {
        let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Allow);
        tracker.add_dependency(1, 2, LAYOUT).unwrap();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.mark(1, LAYOUT);
        tracker.mark(2, LAYOUT);
        tracker.mark(7, PAINT);

        let drained = tracker.drain_channels_sorted(&[LAYOUT, PAINT]);
        assert_eq!(drained.keys, vec![(PAINT, 7)]);
        assert_eq!(drained.stalled, ChannelSet::from(LAYOUT));
        assert!(tracker.is_invalidated(1, LAYOUT));
        assert!(tracker.is_invalidated(2, LAYOUT));
    }

    #[test]
    fn peek_puts_nothing_back() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.mark(2, LAYOUT);
        // A peek never took its keys; dropping it must not mark anything.
        tracker.peek_sorted(LAYOUT).next();
        tracker.peek_sorted_deterministic(LAYOUT).next();
        assert!(!tracker.is_invalidated(1, LAYOUT));
        assert_eq!(tracker.invalidated().len(LAYOUT), 1);
    }

    #[test]
    fn mark_with_reports_what_it_newly_invalidated() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();

        let report = tracker.mark_with(1, LAYOUT, &EagerPolicy);
        assert_eq!(report.newly_invalidated, 3);
        assert_eq!(report.channels, ChannelSet::from(LAYOUT));

        let again = tracker.mark_with(2, LAYOUT, &EagerPolicy);
        assert!(again.is_noop(), "everything was already invalidated");
        assert!(again.channels.is_empty());

        let lazy = tracker.mark_with(7, LAYOUT, &LazyPolicy);
        assert_eq!(lazy.newly_invalidated, 1);
    }

    #[test]
    fn mark_with_report_counts_cascades_and_cross_channel_edges() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(2, PAINT, 9, LAYOUT);

        let report = tracker.mark_with(1, LAYOUT, &EagerPolicy);
        // (1, L), (2, L), (1, P), (2, P), (9, L), (9, P).
        assert_eq!(report.newly_invalidated, 6);
        let mut expected = ChannelSet::from(LAYOUT);
        expected.insert(PAINT);
        assert_eq!(report.channels, expected);
    }

    #[test]
    fn mark_with_trace_explains_cascades_and_cross_channel_edges() {
        use crate::trace::{InvalidationCause, OneParentRecorder};

        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(2, PAINT, 9, LAYOUT);

        let mut scratch = TraversalScratch::new();
        let mut trace = OneParentRecorder::new();
        let report = tracker.mark_with_trace(1, LAYOUT, &EagerPolicy, &mut scratch, &mut trace);

        // Tracing does not change what is marked.
        assert_eq!(report.newly_invalidated, 6);
        assert_eq!(trace.len(), 6, "every marked pair has a cause");
        assert_eq!(trace.cause(1, LAYOUT), Some(InvalidationCause::Root));
        assert_eq!(
            trace.cause(2, LAYOUT),
            Some(InvalidationCause::Dependency { key: 1 })
        );
        assert_eq!(
            trace.cause(9, LAYOUT),
            Some(InvalidationCause::CrossChannel {
                key: 2,
                channel: PAINT
            })
        );
        assert_eq!(
            trace.explain_path(9, PAINT),
            Some(vec![
                (1, LAYOUT),
                (2, LAYOUT),
                (2, PAINT),
                (9, LAYOUT),
                (9, PAINT)
            ])
        );
    }

    #[test]
    fn mark_with_trace_matches_mark_with() {
        use crate::trace::OneParentRecorder;

        let build = || {
            let mut tracker = InvalidationTracker::<u32>::new();
            tracker.add_dependency(2, 1, LAYOUT).unwrap();
            tracker.add_dependency(3, 2, PAINT).unwrap();
            tracker.add_cascade(LAYOUT, PAINT).unwrap();
            tracker.add_cross_dependency(3, PAINT, 1, Channel::new(2));
            tracker
        };
        for lazy in [false, true] {
            let mut plain = build();
            let mut traced = build();
            let mut scratch = TraversalScratch::new();
            let mut trace = OneParentRecorder::new();
            let (a, b) = if lazy {
                (
                    plain.mark_with(2, LAYOUT, &LazyPolicy),
                    traced.mark_with_trace(2, LAYOUT, &LazyPolicy, &mut scratch, &mut trace),
                )
            } else {
                (
                    plain.mark_with(2, LAYOUT, &EagerPolicy),
                    traced.mark_with_trace(2, LAYOUT, &EagerPolicy, &mut scratch, &mut trace),
                )
            };
            assert_eq!(a, b);
            for ch in [LAYOUT, PAINT, Channel::new(2)] {
                let mut x: Vec<_> = plain.invalidated().iter(ch).collect();
                let mut y: Vec<_> = traced.invalidated().iter(ch).collect();
                x.sort_unstable();
                y.sort_unstable();
                assert_eq!(x, y, "lazy: {lazy}, channel {ch:?}");
                for key in y {
                    assert!(trace.explain_path(key, ch).is_some());
                }
            }
        }
    }

    #[test]
    fn mark_report_survives_a_policy_that_removes_keys() {
        // Breaks the contract (policies only add keys), but must not panic.
        struct Taking;
        impl PropagationPolicy<u32> for Taking {
            fn propagate(
                &self,
                key: u32,
                channel: Channel,
                _graph: &InvalidationGraph<u32>,
                invalidated: &mut InvalidationSet<u32>,
            ) {
                invalidated.clear(channel);
                invalidated.mark(key, channel);
            }
        }
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.mark(5, LAYOUT);
        tracker.mark(6, LAYOUT);
        let report = tracker.mark_with(1, LAYOUT, &Taking);
        assert!(tracker.is_invalidated(1, LAYOUT));
        // Each step cleared its channel before marking, so neither grew.
        assert_eq!(report.newly_invalidated, 0);
        assert!(report.channels.is_empty());
    }

    #[test]
    fn custom_policy_trace_defaults_to_reporting_the_root() {
        use crate::trace::{InvalidationCause, OneParentRecorder};

        let mut tracker = InvalidationTracker::<u32>::new();
        let mut scratch = TraversalScratch::new();
        let mut trace = OneParentRecorder::new();
        tracker.mark_with_trace(
            1,
            LAYOUT,
            &OffGraphMarkPolicy { extra_key: 5 },
            &mut scratch,
            &mut trace,
        );
        assert_eq!(trace.cause(1, LAYOUT), Some(InvalidationCause::Root));
        assert_eq!(
            trace.cause(5, LAYOUT),
            None,
            "the default reports the root only"
        );
        assert!(tracker.is_invalidated(5, LAYOUT));
    }

    #[test]
    fn summaries_describe_the_tracker() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(3, PAINT, 9, LAYOUT);
        tracker.mark(1, PAINT);

        let graph = tracker.graph();
        assert_eq!(graph.channels(), ChannelSet::from(LAYOUT));
        assert_eq!(graph.edge_count(LAYOUT), 2);
        assert_eq!(graph.edge_count(PAINT), 0);
        let mut edges: Vec<_> = graph.edges(LAYOUT).collect();
        edges.sort_unstable();
        assert_eq!(edges, vec![(2, 1), (3, 1)]);

        assert_eq!(tracker.invalidated().channels(), ChannelSet::from(PAINT));
        assert_eq!(tracker.cross_channel().len(), 1);
        assert_eq!(
            tracker.cross_channel().edges().collect::<Vec<_>>(),
            vec![((3, PAINT), (9, LAYOUT))]
        );
    }

    #[test]
    fn debug_shows_only_channels_in_use_by_name() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.add_cross_dependency(2, PAINT, 9, LAYOUT);
        tracker.mark(7, PAINT);
        tracker.set_channel_names(
            ChannelNames::new()
                .with(LAYOUT, "layout")
                .with(PAINT, "paint"),
        );
        assert_eq!(
            alloc::format!("{tracker:?}"),
            "InvalidationTracker { invalidated: {paint: {7}}, generation: 1, \
             dependencies: {layout: [2 -> 1]}, cascades: {layout: {paint}}, \
             cross_channel: [(2, paint) -> (9, layout)], cycle_handling: DebugAssert }"
        );
        assert_eq!(
            alloc::format!("{:?}", tracker.graph()),
            "InvalidationGraph {Channel(0): [2 -> 1]}"
        );
    }

    #[test]
    fn mark_report_counts_closure_work() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 2, LAYOUT).unwrap();
        let report = tracker.mark_with(1, LAYOUT, &EagerPolicy);
        assert_eq!(report.steps, 1);
        assert_eq!(report.keys_scanned, 0, "the fast path does not scan");

        tracker.clear_all();
        tracker.add_cross_dependency(1, LAYOUT, 1, PAINT);
        let report = tracker.mark_with(1, LAYOUT, &EagerPolicy);
        // (1, LAYOUT) and (1, PAINT); the layout step scans 2 and 3.
        assert_eq!(report.steps, 2);
        assert_eq!(report.keys_scanned, 2);
    }

    /// The closure as `mark_with` computed it before it pruned repeated walks:
    /// run the policy on every reached pair, and walk all transitive
    /// dependents of each.
    fn reference_closure<P: PropagationPolicy<u32>>(
        tracker: &InvalidationTracker<u32>,
        invalidated: &mut InvalidationSet<u32>,
        key: u32,
        channel: Channel,
        policy: &P,
    ) {
        let graph = tracker.graph();
        let successors = |k: u32, ch: Channel| {
            let mut out: Vec<(u32, Channel)> = tracker
                .cascade()
                .cascades_from(ch)
                .iter()
                .map(|c| (k, c))
                .collect();
            out.extend(tracker.cross_channel().dependents(k, ch));
            out
        };
        let mut worklist = vec![(key, channel)];
        let mut processed = hashbrown::HashSet::new();
        while let Some((k, ch)) = worklist.pop() {
            if !processed.insert((k, ch)) {
                continue;
            }
            policy.propagate(k, ch, graph, invalidated);
            worklist.extend(successors(k, ch));
            for dep in graph.transitive_dependents(k, ch) {
                if invalidated.is_invalidated(dep, ch) {
                    worklist.extend(successors(dep, ch));
                }
            }
        }
    }

    #[test]
    fn pruned_closure_matches_reference_on_random_graphs() {
        let mut seed = 0x5EED_u64;
        let mut next = |bound: u32| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            u32::try_from(seed >> 33).unwrap() % bound
        };
        let channels = [LAYOUT, PAINT, Channel::new(2)];

        for case in 0..300 {
            let n = 2 + next(24);
            let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Allow);
            for ch in channels {
                for _ in 0..next(3 * n) {
                    let _ = tracker.add_dependency(next(n), next(n), ch).unwrap();
                }
            }
            for _ in 0..next(3) {
                let (a, b) = (next(3) as usize, next(3) as usize);
                if a < b {
                    let _ = tracker.add_cascade(channels[a], channels[b]);
                }
            }
            for _ in 0..next(6) {
                tracker.add_cross_dependency(
                    next(n),
                    channels[next(3) as usize],
                    next(n),
                    channels[next(3) as usize],
                );
            }
            for _ in 0..next(4) {
                tracker.mark(next(n), channels[next(3) as usize]);
            }
            let (root, root_ch) = (next(n), channels[next(3) as usize]);

            for eager in [true, false] {
                let mut expected = tracker.invalidated().clone();
                let mut actual = tracker.clone();
                if eager {
                    reference_closure(&tracker, &mut expected, root, root_ch, &EagerPolicy);
                    actual.mark_with(root, root_ch, &EagerPolicy);
                } else {
                    reference_closure(&tracker, &mut expected, root, root_ch, &LazyPolicy);
                    actual.mark_with(root, root_ch, &LazyPolicy);
                }
                for ch in channels {
                    let mut want: Vec<_> = expected.iter(ch).collect();
                    let mut got: Vec<_> = actual.invalidated().iter(ch).collect();
                    want.sort_unstable();
                    got.sort_unstable();
                    assert_eq!(got, want, "case {case}, eager {eager}, channel {ch:?}");
                }
            }
        }
    }

    #[test]
    fn eager_cascade_closure_does_linear_work() {
        let n = 512;
        let mut tracker = InvalidationTracker::<u32>::new();
        for i in 1..n {
            tracker.add_dependency(i, i - 1, LAYOUT).unwrap();
            tracker.add_dependency(i, i - 1, PAINT).unwrap();
        }
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        let report = tracker.mark_with(0, LAYOUT, &EagerPolicy);
        assert_eq!(report.newly_invalidated, 2 * n as usize);
        // Each key's dependents are walked once per channel, and the policy
        // does not re-run on keys an earlier step covered.
        assert!(report.keys_scanned < 2 * n as usize, "{report:?}");
        assert!(report.steps <= 2, "{report:?}");
    }

    #[test]
    fn lazy_affected_drain_passes_work_to_cascaded_channels() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();

        tracker.mark_with(1, LAYOUT, &LazyPolicy);
        let layout: Vec<_> = tracker
            .drain_affected_sorted_deterministic(LAYOUT)
            .collect();
        assert_eq!(layout, vec![1, 2]);
        let paint: Vec<_> = tracker.drain_affected_sorted_deterministic(PAINT).collect();
        assert_eq!(paint, vec![1, 2], "relaying out 2 repaints it");
        assert!(tracker.is_clean());
    }

    #[test]
    fn lazy_affected_drain_follows_cross_channel_edges_transitively() {
        const A11Y: Channel = Channel::new(2);
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cross_dependency(2, LAYOUT, 9, PAINT);
        tracker.add_cascade(PAINT, A11Y).unwrap();

        tracker.mark_with(1, LAYOUT, &LazyPolicy);
        let _ = tracker.drain_affected_sorted(LAYOUT).count();
        assert!(tracker.is_invalidated(9, PAINT));
        assert!(
            tracker.is_invalidated(9, A11Y),
            "the target's cascades follow too"
        );
        assert!(!tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn affected_drain_follows_edges_of_keys_it_did_not_expand() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        // A direct mark follows no cross-channel edges, and this edge did not
        // exist when 2 was marked; the drain follows it anyway.
        tracker.mark(2, LAYOUT);
        tracker.add_cross_dependency(2, LAYOUT, 9, PAINT);
        let _ = tracker.drain_affected_sorted(LAYOUT).count();
        assert!(tracker.is_invalidated(9, PAINT));
    }

    #[test]
    fn work_passed_on_by_a_drain_is_marked_as_roots() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cross_dependency(2, LAYOUT, 9, PAINT);
        tracker.add_dependency(10, 9, PAINT).unwrap();
        tracker.mark(1, LAYOUT);
        let _ = tracker.drain_affected_sorted(LAYOUT).count();

        assert!(tracker.is_invalidated(9, PAINT));
        assert!(
            !tracker.is_invalidated(10, PAINT),
            "a root, not its dependents"
        );
        let paint: Vec<_> = tracker.drain_affected_sorted_deterministic(PAINT).collect();
        assert_eq!(paint, vec![9, 10], "an affected drain expands it");
    }

    #[test]
    fn affected_drain_does_not_requeue_same_channel_keys_it_drains() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 1, LAYOUT).unwrap();
        // A same-channel cross edge into a key this drain yields anyway.
        tracker.add_cross_dependency(2, LAYOUT, 3, LAYOUT);
        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        let drained: Vec<_> = tracker
            .drain_affected_sorted_deterministic(LAYOUT)
            .collect();
        assert_eq!(drained, vec![1, 2, 3]);
        assert!(tracker.is_clean(), "3 was drained, not left pending");
    }

    #[test]
    fn retained_out_of_scope_keys_follow_cascades() {
        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_dependency(3, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        let mut retained = Vec::new();
        let drained: Vec<_> = tracker
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(2)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(drained, vec![1, 2]);
        assert_eq!(retained, vec![(3, 1)]);
        assert!(tracker.is_invalidated(3, LAYOUT));
        assert!(
            tracker.is_invalidated(3, PAINT),
            "the retained key repaints too"
        );
        assert!(tracker.is_invalidated(2, PAINT));
    }

    #[test]
    fn affected_drain_traces_work_passed_to_other_channels() {
        use crate::trace::{InvalidationCause, OneParentRecorder};

        let mut tracker = InvalidationTracker::<u32>::new();
        tracker.add_dependency(2, 1, LAYOUT).unwrap();
        tracker.add_cascade(LAYOUT, PAINT).unwrap();
        tracker.mark_with(1, LAYOUT, &LazyPolicy);

        let mut scratch = TraversalScratch::new();
        let mut trace = OneParentRecorder::new();
        let _ = tracker
            .drain_affected_sorted_with_trace(LAYOUT, &mut scratch, &mut trace)
            .count();
        assert_eq!(
            trace.cause(2, PAINT),
            Some(InvalidationCause::Cascade { channel: LAYOUT })
        );
        assert_eq!(
            trace.explain_path(2, PAINT),
            Some(vec![(1, LAYOUT), (2, LAYOUT), (2, PAINT)])
        );
    }

    #[test]
    fn reconvergent_cross_channel_marks_do_linear_work() {
        // n layout keys feed one n-key paint chain, deepest point first.
        let n = 256_u32;
        let mut tracker = InvalidationTracker::<u32>::new();
        for key in 1..=n {
            tracker.add_dependency(key, 0, LAYOUT).unwrap();
        }
        for i in 1..n {
            tracker.add_dependency(n + 1 + i, n + i, PAINT).unwrap();
        }
        for key in 1..=n {
            tracker.add_cross_dependency(key, LAYOUT, 2 * n + 1 - key, PAINT);
        }

        let report = tracker.mark_with(0, LAYOUT, &EagerPolicy);
        assert_eq!(report.newly_invalidated, 2 * n as usize + 1);
        // Every dependent is walked once in all, whatever order the steps run.
        assert!(report.keys_scanned <= 2 * n as usize, "{report:?}");
    }

    #[test]
    fn mark_many_with_matches_marking_each_root() {
        let mut seed = 0xBA7C_u64;
        let mut next = |bound: u32| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            u32::try_from(seed >> 33).unwrap() % bound
        };
        let channels = [LAYOUT, PAINT, Channel::new(2)];
        for _ in 0..100 {
            let n = 2 + next(20);
            let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Allow);
            for ch in channels {
                for _ in 0..next(3 * n) {
                    let _ = tracker.add_dependency(next(n), next(n), ch).unwrap();
                }
            }
            if next(2) == 0 {
                tracker.add_cascade(LAYOUT, PAINT).unwrap();
            }
            for _ in 0..next(4) {
                tracker.add_cross_dependency(
                    next(n),
                    channels[next(3) as usize],
                    next(n),
                    channels[next(3) as usize],
                );
            }
            let roots: Vec<_> = (0..next(6))
                .map(|_| (next(n), channels[next(3) as usize]))
                .collect();

            for eager in [true, false] {
                let mut each = tracker.clone();
                let mut many = tracker.clone();
                let report = if eager {
                    for &(k, ch) in &roots {
                        each.mark_with(k, ch, &EagerPolicy);
                    }
                    many.mark_many_with(roots.iter().copied(), &EagerPolicy)
                } else {
                    for &(k, ch) in &roots {
                        each.mark_with(k, ch, &LazyPolicy);
                    }
                    many.mark_many_with(roots.iter().copied(), &LazyPolicy)
                };
                let mut total = 0;
                for ch in channels {
                    let mut want: Vec<_> = each.invalidated().iter(ch).collect();
                    let mut got: Vec<_> = many.invalidated().iter(ch).collect();
                    want.sort_unstable();
                    got.sort_unstable();
                    assert_eq!(got, want, "eager {eager}, roots {roots:?}");
                    total += got.len();
                }
                assert_eq!(report.newly_invalidated, total);
            }
        }
    }

    /// Runs random operation sequences and checks that no work is lost.
    ///
    /// Every mark obliges the tracker to deliver its closure (the root and
    /// everything reachable from it through dependencies, cascades, and
    /// cross-channel edges) through some later drain. Drains here are all
    /// affected drains, scoped ones keep out-of-scope work, and some are
    /// abandoned part way. Edges are only added, so an obligation never goes
    /// away except by being drained.
    fn run_sequences(cycles: bool, cases: u32) {
        let mut seed = if cycles { 0xC1C1_u64 } else { 0xACAC_u64 };
        let mut next = |bound: u32| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            u32::try_from(seed >> 33).unwrap() % bound
        };
        let channels = [LAYOUT, PAINT, Channel::new(2)];
        let handling = if cycles {
            CycleHandling::Allow
        } else {
            CycleHandling::Ignore
        };

        for case in 0..cases {
            let n = 2 + next(16);
            let mut tracker = InvalidationTracker::<u32>::with_cycle_handling(handling);
            let mut owed = hashbrown::HashSet::<(u32, Channel)>::new();
            let mut log = Vec::new();

            let drain = |tracker: &mut InvalidationTracker<u32>,
                         owed: &mut hashbrown::HashSet<(u32, Channel)>,
                         ch: Channel,
                         scope: u32,
                         target: u32,
                         take: Option<u32>| {
                let keys: Vec<u32> = (0..n).filter(|k| k % 3 == target % 3).collect();
                let mut retained = Vec::new();
                let builder = tracker.drain(ch).affected();
                let builder = match scope {
                    0 => builder,
                    1 => builder
                        .within_keys(&keys)
                        .retain_out_of_scope(&mut retained),
                    _ => builder
                        .within_dependencies_of(target)
                        .retain_out_of_scope(&mut retained),
                };
                let mut drain = builder.deterministic().run();
                let mut taken = 0;
                while take.is_none_or(|limit| taken < limit) {
                    let Some(key) = drain.next() else { break };
                    owed.remove(&(key, ch));
                    taken += 1;
                }
            };

            for _ in 0..40 {
                let ch = channels[next(3) as usize];
                match next(9) {
                    0 | 1 => {
                        let (a, b) = (next(n), next(n));
                        log.push(alloc::format!("dep {a}->{b} {ch:?}"));
                        let _ = tracker.add_dependency(a, b, ch).unwrap();
                    }
                    2 => {
                        let (a, b) = (next(3) as usize, next(3) as usize);
                        if a < b {
                            log.push(alloc::format!("cascade {a}->{b}"));
                            let _ = tracker.add_cascade(channels[a], channels[b]);
                        }
                    }
                    3 => {
                        let (a, b) = (next(n), next(n));
                        let to = channels[next(3) as usize];
                        // Like cascades, cross-channel edges that only point
                        // to later channels cannot form a cycle.
                        if !cycles && to.index() <= ch.index() {
                            continue;
                        }
                        log.push(alloc::format!("cross ({a},{ch:?})->({b},{to:?})"));
                        tracker.add_cross_dependency(a, ch, b, to);
                    }
                    4 | 5 => {
                        let key = next(n);
                        let eager = next(2) == 0;
                        log.push(alloc::format!("mark {key} {ch:?} eager={eager}"));
                        owed.insert((key, ch));
                        owed.extend(tracker.transitive_dependents_cross(key, ch));
                        if eager {
                            tracker.mark_with(key, ch, &EagerPolicy);
                        } else {
                            tracker.mark_with(key, ch, &LazyPolicy);
                        }
                    }
                    6 => {
                        let roots: Vec<_> = (0..1 + next(3))
                            .map(|_| (next(n), channels[next(3) as usize]))
                            .collect();
                        log.push(alloc::format!("mark_many {roots:?}"));
                        for &(key, root_ch) in &roots {
                            owed.insert((key, root_ch));
                            owed.extend(tracker.transitive_dependents_cross(key, root_ch));
                        }
                        tracker.mark_many_with(roots, &LazyPolicy);
                    }
                    _ => {
                        let (scope, target) = (next(3), next(n));
                        let take = (next(2) == 0).then(|| next(4));
                        log.push(alloc::format!(
                            "drain {ch:?} scope={scope} target={target} take={take:?}"
                        ));
                        drain(&mut tracker, &mut owed, ch, scope, target, take);
                    }
                }
            }

            // Flush: drain every channel until nothing is left, or only
            // keys on cycles are.
            for _ in 0..8 {
                for ch in channels {
                    drain(&mut tracker, &mut owed, ch, 0, 0, None);
                }
            }

            for &(key, ch) in &owed {
                if cycles {
                    assert!(
                        tracker.is_invalidated(key, ch),
                        "case {case}: ({key}, {ch:?}) was neither drained nor kept\n{}",
                        log.join("\n")
                    );
                } else {
                    panic!(
                        "case {case}: ({key}, {ch:?}) was never drained\n{}",
                        log.join("\n")
                    );
                }
            }
            if !cycles {
                assert!(tracker.is_clean(), "case {case}: {tracker:?}");
            }
        }
    }

    #[test]
    fn operation_sequences_lose_no_work() {
        run_sequences(false, 400);
    }

    #[test]
    fn operation_sequences_with_cycles_keep_undrained_work() {
        run_sequences(true, 400);
    }
}

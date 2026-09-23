// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Builder-based drain API.
//!
//! This API is intended for embedders who need more control than the
//! convenience drain helpers provide (e.g. determinism, targeted drains,
//! scratch reuse, and explainability hooks).
//!
//! The key idea is that drain behavior is configured via a small builder, and
//! only the selected options impose additional trait bounds:
//!
//! - Default order: `Any` (no `Ord` bound).
//! - Deterministic order: opt in via [`DrainBuilder::deterministic`] (requires `K: Ord + DenseKey`).
//!
//! Reach for `DrainBuilder` when the one-shot helpers are too narrow:
//!
//! - `drain_sorted` for “all currently invalidated keys”
//! - `drain_affected_sorted` for “roots plus dependents”
//! - `DrainBuilder` when you also need targeted scope, deterministic ordering,
//!   scratch reuse, or trace capture
//!
//! `DrainBuilder` is intentionally additive: the extra trait bounds and work
//! only appear for the capabilities you opt into.

use alloc::vec::Vec;
use core::hash::Hash;
use core::marker::PhantomData;

use hashbrown::HashSet;

use crate::Channel;
use crate::DenseKey;
use crate::DrainSorted;
use crate::DrainSortedDeterministic;
use crate::InvalidationGraph;
use crate::InvalidationSet;
use crate::TraversalScratch;
use crate::trace::InvalidationTrace;

/// Type-level marker for “any” drain ordering (ties are not specified).
#[derive(Copy, Clone, Debug, Default)]
pub struct AnyOrder;

/// Type-level marker for deterministic drain ordering (ties broken by `Ord`).
#[derive(Copy, Clone, Debug, Default)]
pub struct DeterministicOrder;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum DrainMode {
    InvalidatedOnly,
    Affected,
}

#[derive(Copy, Clone, Debug)]
enum Within<'w, K> {
    All,
    Keys(&'w [K]),
    DependenciesOf(K),
}

/// A builder that configures and performs a drain.
///
/// Construct this via [`InvalidationTracker::drain`](crate::InvalidationTracker::drain).
///
/// # Targeted drains
///
/// The `within_*` methods provide targeted drains that do **not** require the
/// “global drain then restore” pattern: invalidated roots outside the target
/// remain invalidated for subsequent drains.
///
/// An affected targeted drain expands dependents only inside its scope. Use
/// [`DrainBuilder::retain_out_of_scope`] to keep dependents of drained keys
/// that lie outside the scope invalidated, rather than losing their work.
pub struct DrainBuilder<'d, 'g, 's, K, O = AnyOrder>
where
    K: Copy + Eq + Hash + DenseKey,
{
    invalidated: &'d mut InvalidationSet<K>,
    graph: &'g InvalidationGraph<K>,
    channel: Channel,
    mode: DrainMode,
    within: Within<'d, K>,
    out_of_scope: Option<&'d mut Vec<(K, K)>>,
    scratch: Option<&'s mut TraversalScratch<K>>,
    trace: Option<&'s mut dyn InvalidationTrace<K>>,
    _order: PhantomData<O>,
}

impl<K, O> core::fmt::Debug for DrainBuilder<'_, '_, '_, K, O>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DrainBuilder")
            .field("channel", &self.channel)
            .field("mode", &self.mode)
            .field("retains_out_of_scope", &self.out_of_scope.is_some())
            .finish_non_exhaustive()
    }
}

impl<'d, 'g, K> DrainBuilder<'d, 'g, 'd, K, AnyOrder>
where
    K: Copy + Eq + Hash + DenseKey,
{
    pub(crate) fn new(
        invalidated: &'d mut InvalidationSet<K>,
        graph: &'g InvalidationGraph<K>,
        channel: Channel,
    ) -> Self {
        Self {
            invalidated,
            graph,
            channel,
            mode: DrainMode::InvalidatedOnly,
            within: Within::All,
            out_of_scope: None,
            scratch: None,
            trace: None,
            _order: PhantomData,
        }
    }
}

impl<'d, 'g, 's, K, O> DrainBuilder<'d, 'g, 's, K, O>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Drains exactly the keys currently marked invalidated (topologically sorted).
    ///
    /// This is the default; it is included for symmetry with
    /// [`DrainBuilder::affected`].
    #[must_use]
    pub fn invalidated_only(mut self) -> Self {
        self.mode = DrainMode::InvalidatedOnly;
        self
    }

    /// Drains roots plus all transitive dependents (“affected” keys), then
    /// topologically sorts the result.
    ///
    /// This is the “lazy at mark-time, eager at drain-time” workflow, intended
    /// for use with [`LazyPolicy`](crate::LazyPolicy).
    #[must_use]
    pub fn affected(mut self) -> Self {
        self.mode = DrainMode::Affected;
        self
    }

    /// Restricts the drain to keys contained in `keys`.
    ///
    /// Invalidated roots outside `keys` remain invalidated for later drains.
    ///
    /// Note: `keys` is borrowed for the lifetime of the builder, so it must
    /// outlive the drain call.
    #[must_use]
    pub fn within_keys(mut self, keys: &'d [K]) -> Self {
        self.within = Within::Keys(keys);
        self
    }

    /// Restricts the drain to the transitive dependency-closure of `key` (plus
    /// `key` itself) in this channel.
    ///
    /// Invalidated roots outside the closure remain invalidated for later drains.
    #[must_use]
    pub fn within_dependencies_of(mut self, key: K) -> Self {
        self.within = Within::DependenciesOf(key);
        self
    }

    /// Keeps dependents of drained keys that lie outside a targeted drain's
    /// scope invalidated, and reports them.
    ///
    /// An [`affected`](DrainBuilder::affected) drain restricted by
    /// [`within_keys`](DrainBuilder::within_keys) or
    /// [`within_dependencies_of`](DrainBuilder::within_dependencies_of) takes
    /// every invalidated root inside the scope but expands dependents only
    /// inside it. Without this option, a key outside the scope that depends on
    /// a drained key (for example a sibling of the target reading the same
    /// root) is never marked, so under [`LazyPolicy`](crate::LazyPolicy) its
    /// pending work is lost.
    ///
    /// With this option, every **direct** dependent of a drained key that lies
    /// outside the scope is marked invalidated in this channel, and each such
    /// `(dependent, drained key)` edge is appended to `out_of_scope`. Lazy
    /// expansion from those marks covers their own dependents on a later
    /// affected drain. The pairs let callers attach causes, for example to
    /// explain the retained work from its real root. With
    /// [`deterministic`](DrainBuilder::deterministic) the appended pairs are
    /// sorted by `(dependent, drained key)`; otherwise their order is
    /// unspecified.
    ///
    /// This has no effect on untargeted drains, which leave nothing outside
    /// their scope, or on [`invalidated_only`](DrainBuilder::invalidated_only)
    /// drains, which never take responsibility for dependents. Marking is
    /// idempotent, so keys already invalidated (for example by
    /// [`EagerPolicy`](crate::EagerPolicy)) are simply reported again.
    ///
    /// Retained keys are marked directly in the invalidated set, without the
    /// tracker's channel cascades: a key that
    /// [`InvalidationTracker::mark`](crate::InvalidationTracker::mark) would
    /// cascade to other channels is marked in this channel only. They are
    /// also not reported to a [`trace`](DrainBuilder::trace) recorder; the
    /// pairs appended to `out_of_scope` are the only record of why they were
    /// retained.
    #[must_use]
    pub fn retain_out_of_scope(mut self, out_of_scope: &'d mut Vec<(K, K)>) -> Self {
        self.out_of_scope = Some(out_of_scope);
        self
    }

    /// Reuses `scratch` for internal traversals (affected expansion, targeted
    /// dependency closure computation).
    ///
    /// If you want tracing, prefer [`DrainBuilder::trace`], which also
    /// configures scratch reuse.
    #[must_use]
    pub fn scratch<'s2>(
        self,
        scratch: &'s2 mut TraversalScratch<K>,
    ) -> DrainBuilder<'d, 'g, 's2, K, O> {
        let DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            trace,
            ..
        } = self;
        debug_assert!(
            trace.is_none(),
            "calling `DrainBuilder::scratch` after configuring trace is not supported; call `DrainBuilder::trace` instead",
        );
        DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            scratch: Some(scratch),
            trace: None,
            _order: PhantomData,
        }
    }

    /// Records a best-effort explanation while expanding affected keys.
    ///
    /// This records **one plausible cause path** (a spanning forest): when a
    /// key is reachable via multiple roots or paths, the first discovered path
    /// wins.
    ///
    /// This also configures scratch reuse; you do not need to call
    /// [`DrainBuilder::scratch`] separately.
    #[must_use]
    pub fn trace<'s2, T>(
        self,
        scratch: &'s2 mut TraversalScratch<K>,
        trace: &'s2 mut T,
    ) -> DrainBuilder<'d, 'g, 's2, K, O>
    where
        T: InvalidationTrace<K>,
    {
        let DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            ..
        } = self;
        DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            scratch: Some(scratch),
            trace: Some(trace),
            _order: PhantomData,
        }
    }
}

impl<'d, 'g, 's, K> DrainBuilder<'d, 'g, 's, K, AnyOrder>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Switches the drain to deterministic tie-breaking (`Ord`).
    #[must_use]
    pub fn deterministic(self) -> DrainBuilder<'d, 'g, 's, K, DeterministicOrder>
    where
        K: Ord + DenseKey,
    {
        let DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            scratch,
            trace,
            ..
        } = self;
        DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            scratch,
            trace,
            _order: PhantomData,
        }
    }
}

impl<'d, 'g, 's, K, O> DrainBuilder<'d, 'g, 's, K, O>
where
    K: Copy + Eq + Hash + DenseKey,
{
    fn is_allowed(within: &Within<'d, K>, key: K, allowed: Option<&HashSet<K>>) -> bool {
        match *within {
            Within::All => true,
            Within::Keys(keys) => keys.contains(&key),
            Within::DependenciesOf(_) => allowed.is_some_and(|set| set.contains(&key)),
        }
    }

    fn compute_allowed_dependencies(
        graph: &InvalidationGraph<K>,
        channel: Channel,
        key: K,
        scratch: Option<&mut TraversalScratch<K>>,
    ) -> HashSet<K> {
        let mut allowed: HashSet<K> = HashSet::new();
        allowed.insert(key);

        match scratch {
            Some(s) => {
                s.reset();
                s.stack.push(key);
                while let Some(next) = s.stack.pop() {
                    for dep in graph.dependencies(next, channel) {
                        if allowed.insert(dep) {
                            s.stack.push(dep);
                        }
                    }
                }
            }
            None => {
                let mut stack = Vec::new();
                stack.push(key);
                while let Some(next) = stack.pop() {
                    for dep in graph.dependencies(next, channel) {
                        if allowed.insert(dep) {
                            stack.push(dep);
                        }
                    }
                }
            }
        }

        allowed
    }

    fn take_roots(
        invalidated: &mut InvalidationSet<K>,
        channel: Channel,
        within: &Within<'d, K>,
        allowed: Option<&HashSet<K>>,
    ) -> Vec<K> {
        match within {
            Within::All => invalidated.drain(channel).collect(),
            Within::Keys(_) | Within::DependenciesOf(_) => {
                let roots: Vec<K> = invalidated
                    .iter(channel)
                    .filter(|&k| Self::is_allowed(within, k, allowed))
                    .collect();
                for &k in &roots {
                    let _ = invalidated.take(k, channel);
                }
                roots
            }
        }
    }

    /// Marks and reports dependents of `drained` that lie outside `within`.
    ///
    /// Returns the index in `out` where this drain's pairs start.
    fn retain_out_of_scope_dependents(
        invalidated: &mut InvalidationSet<K>,
        graph: &InvalidationGraph<K>,
        channel: Channel,
        within: &Within<'d, K>,
        allowed: Option<&HashSet<K>>,
        drained: &[K],
        out: &mut Vec<(K, K)>,
    ) -> usize {
        let start = out.len();
        if matches!(within, Within::All) {
            return start;
        }
        for &because in drained {
            for dependent in graph.dependents(because, channel) {
                if !Self::is_allowed(within, dependent, allowed) {
                    out.push((dependent, because));
                }
            }
        }
        for &(dependent, _) in &out[start..] {
            let _ = invalidated.mark(dependent, channel);
        }
        start
    }

    fn collect_affected<'t>(
        graph: &InvalidationGraph<K>,
        channel: Channel,
        roots: Vec<K>,
        within: &Within<'d, K>,
        allowed: Option<&HashSet<K>>,
        scratch: Option<&'t mut TraversalScratch<K>>,
        mut trace: Option<&'t mut dyn InvalidationTrace<K>>,
    ) -> Vec<K> {
        // Affected drains need a visited set that persists across roots.
        match scratch {
            Some(s) => {
                s.reset();
                Self::collect_affected_with_state(
                    graph,
                    channel,
                    roots,
                    within,
                    allowed,
                    &mut s.stack,
                    &mut s.visited,
                    &mut trace,
                )
            }
            None => {
                let mut visited: HashSet<K> = HashSet::new();
                let mut stack: Vec<K> = Vec::new();
                Self::collect_affected_with_state(
                    graph,
                    channel,
                    roots,
                    within,
                    allowed,
                    &mut stack,
                    &mut visited,
                    &mut trace,
                )
            }
        }
    }

    fn collect_affected_with_state(
        graph: &InvalidationGraph<K>,
        channel: Channel,
        roots: Vec<K>,
        within: &Within<'d, K>,
        allowed: Option<&HashSet<K>>,
        stack: &mut Vec<K>,
        visited: &mut HashSet<K>,
        trace: &mut Option<&mut dyn InvalidationTrace<K>>,
    ) -> Vec<K> {
        let mut out = Vec::new();

        for root in roots {
            if !Self::is_allowed(within, root, allowed) {
                continue;
            }
            let newly = visited.insert(root);
            if newly {
                out.push(root);
                stack.push(root);
            }
            if let Some(t) = trace.as_deref_mut() {
                t.root(root, channel, newly);
            }
        }

        while let Some(because) = stack.pop() {
            for dependent in graph.dependents(because, channel) {
                if !Self::is_allowed(within, dependent, allowed) {
                    continue;
                }
                let newly = visited.insert(dependent);
                if let Some(t) = trace.as_deref_mut() {
                    t.caused_by(dependent, because, channel, newly);
                }
                if newly {
                    out.push(dependent);
                    stack.push(dependent);
                }
            }
        }

        out
    }
}

impl<'d, 'g, 's, K> DrainBuilder<'d, 'g, 's, K, AnyOrder>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Executes the drain and returns an iterator in topological order.
    pub fn run(self) -> DrainSorted<'g, K> {
        let DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            mut scratch,
            trace,
            ..
        } = self;

        let allowed_set_storage;
        let allowed = match within {
            Within::DependenciesOf(key) => {
                allowed_set_storage =
                    Self::compute_allowed_dependencies(graph, channel, key, scratch.as_deref_mut());
                Some(&allowed_set_storage)
            }
            Within::All | Within::Keys(_) => None,
        };

        let roots = Self::take_roots(invalidated, channel, &within, allowed);

        let keys = match mode {
            DrainMode::InvalidatedOnly => roots,
            DrainMode::Affected => {
                let keys =
                    Self::collect_affected(graph, channel, roots, &within, allowed, scratch, trace);
                if let Some(out) = out_of_scope {
                    let _ = Self::retain_out_of_scope_dependents(
                        invalidated,
                        graph,
                        channel,
                        &within,
                        allowed,
                        &keys,
                        out,
                    );
                }
                keys
            }
        };

        let cap = keys.len();
        DrainSorted::from_iter_with_capacity(keys.into_iter(), cap, graph, channel)
    }
}

impl<'d, 'g, 's, K> DrainBuilder<'d, 'g, 's, K, DeterministicOrder>
where
    K: Copy + Eq + Hash + Ord + DenseKey,
{
    /// Executes the drain and returns an iterator in deterministic topological order.
    pub fn run(self) -> DrainSortedDeterministic<'g, K> {
        let DrainBuilder {
            invalidated,
            graph,
            channel,
            mode,
            within,
            out_of_scope,
            mut scratch,
            trace,
            ..
        } = self;

        let allowed_set_storage;
        let allowed = match within {
            Within::DependenciesOf(key) => {
                allowed_set_storage =
                    Self::compute_allowed_dependencies(graph, channel, key, scratch.as_deref_mut());
                Some(&allowed_set_storage)
            }
            Within::All | Within::Keys(_) => None,
        };

        let roots = Self::take_roots(invalidated, channel, &within, allowed);

        let keys = match mode {
            DrainMode::InvalidatedOnly => roots,
            DrainMode::Affected => {
                let keys =
                    Self::collect_affected(graph, channel, roots, &within, allowed, scratch, trace);
                if let Some(out) = out_of_scope {
                    let start = Self::retain_out_of_scope_dependents(
                        invalidated,
                        graph,
                        channel,
                        &within,
                        allowed,
                        &keys,
                        out,
                    );
                    out[start..].sort_unstable();
                }
                keys
            }
        };

        let cap = keys.len();
        DrainSortedDeterministic::from_iter_with_capacity(keys.into_iter(), cap, graph, channel)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use alloc::vec;

    use crate::CycleHandling;
    use crate::InvalidationTracker;
    use crate::LazyPolicy;
    use crate::trace::OneParentRecorder;

    const LAYOUT: Channel = Channel::new(0);

    #[test]
    fn within_keys_does_not_clear_outside_roots() {
        let mut t = InvalidationTracker::<u32>::new();
        t.mark(1, LAYOUT);
        t.mark(2, LAYOUT);

        let subset = [1];
        let order: Vec<_> = t
            .drain(LAYOUT)
            .invalidated_only()
            .within_keys(&subset)
            .run()
            .collect();
        assert_eq!(order, vec![1]);
        assert!(t.is_invalidated(2, LAYOUT));
    }

    #[test]
    fn within_dependencies_of_filters_invalidated_only() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // 1 <- 2 <- 3 and unrelated 9.
        t.add_dependency(2, 1, LAYOUT).unwrap();
        t.add_dependency(3, 2, LAYOUT).unwrap();

        t.mark(1, LAYOUT);
        t.mark(2, LAYOUT);
        t.mark(3, LAYOUT);
        t.mark(9, LAYOUT);

        let order: Vec<_> = t
            .drain(LAYOUT)
            .invalidated_only()
            .within_dependencies_of(3)
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 3]);
        assert!(t.is_invalidated(9, LAYOUT));
    }

    #[test]
    fn affected_with_trace_records_one_plausible_path() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // 1 <- 2 <- 3
        t.add_dependency(2, 1, LAYOUT).unwrap();
        t.add_dependency(3, 2, LAYOUT).unwrap();

        t.mark(1, LAYOUT);

        let mut scratch = TraversalScratch::new();
        let mut rec = OneParentRecorder::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .trace(&mut scratch, &mut rec)
            .run()
            .collect();

        assert_eq!(order, vec![1, 2, 3]);
        assert_eq!(rec.explain_path(3, LAYOUT).unwrap(), vec![1, 2, 3]);
    }

    /// `a` and `b` both depend on `x`, `c` depends on `a`, and `q` depends on
    /// `b`. A drain scoped to `c` takes `x` but must not lose `b` (and so `q`).
    fn siblings(t: &mut InvalidationTracker<u32>) {
        let (x, a, b, c, q) = (1, 2, 3, 4, 5);
        t.add_dependency(a, x, LAYOUT).unwrap();
        t.add_dependency(b, x, LAYOUT).unwrap();
        t.add_dependency(c, a, LAYOUT).unwrap();
        t.add_dependency(q, b, LAYOUT).unwrap();
    }

    #[test]
    fn scoped_affected_drain_loses_siblings_by_default() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        siblings(&mut t);
        t.mark_with(1, LAYOUT, &LazyPolicy);

        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(4)
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 4]);
        // The default keeps the historical behavior: `b` was never marked.
        assert!(!t.is_invalidated(3, LAYOUT));
        assert_eq!(t.drain(LAYOUT).affected().run().count(), 0);
    }

    #[test]
    fn retain_out_of_scope_keeps_siblings_and_their_dependents() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        siblings(&mut t);
        t.mark_with(1, LAYOUT, &LazyPolicy);

        let mut retained = Vec::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(4)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 4]);
        assert_eq!(retained, vec![(3, 1)]);
        assert!(t.is_invalidated(3, LAYOUT));

        // Lazy expansion from the retained mark reaches `q` behind it.
        let rest: Vec<_> = t.drain(LAYOUT).affected().deterministic().run().collect();
        assert_eq!(rest, vec![3, 5]);
    }

    #[test]
    fn retain_out_of_scope_reports_every_boundary_edge_sorted() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // Target 10 reads 1 and 2; outside readers: 20 reads 1 and 2, 30 reads 2.
        for (from, to) in [(10, 1), (10, 2), (20, 1), (20, 2), (30, 2)] {
            t.add_dependency(from, to, LAYOUT).unwrap();
        }
        t.mark_with(1, LAYOUT, &LazyPolicy);
        t.mark_with(2, LAYOUT, &LazyPolicy);

        let mut retained = vec![(99, 99)];
        let _ = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(10)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .count();
        // Existing contents are kept; this drain's pairs are appended in order.
        assert_eq!(retained, vec![(99, 99), (20, 1), (20, 2), (30, 2)]);
    }

    #[test]
    fn retain_out_of_scope_applies_to_within_keys() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        siblings(&mut t);
        t.mark_with(1, LAYOUT, &LazyPolicy);

        let scope = [1, 2, 4];
        let mut retained = Vec::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_keys(&scope)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 4]);
        assert_eq!(retained, vec![(3, 1)]);
        assert!(t.is_invalidated(3, LAYOUT));
    }

    #[test]
    fn retain_out_of_scope_covers_scopes_not_closed_under_dependencies() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // x <- a <- c, but the scope names only x and c.
        let (x, a, c) = (1, 2, 3);
        t.add_dependency(a, x, LAYOUT).unwrap();
        t.add_dependency(c, a, LAYOUT).unwrap();
        t.mark_with(x, LAYOUT, &LazyPolicy);

        let scope = [x, c];
        let mut retained = Vec::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_keys(&scope)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        // Expansion stops at `a`, which lies outside the scope; `c` is only
        // reachable through it.
        assert_eq!(order, vec![x]);
        assert_eq!(retained, vec![(a, x)]);

        let rest: Vec<_> = t.drain(LAYOUT).affected().deterministic().run().collect();
        assert_eq!(rest, vec![a, c]);
    }

    #[test]
    fn chained_scoped_drains_drain_every_key_once() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // x <- a <- t1, a <- b <- t2, b <- q.
        let (x, a, t1, b, t2, q) = (1, 2, 3, 4, 5, 6);
        t.add_dependency(a, x, LAYOUT).unwrap();
        t.add_dependency(t1, a, LAYOUT).unwrap();
        t.add_dependency(b, a, LAYOUT).unwrap();
        t.add_dependency(t2, b, LAYOUT).unwrap();
        t.add_dependency(q, b, LAYOUT).unwrap();
        t.mark_with(x, LAYOUT, &LazyPolicy);

        let mut retained = Vec::new();
        let first: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(t1)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(first, vec![x, a, t1]);
        assert_eq!(retained, vec![(b, a)]);

        retained.clear();
        let second: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(t2)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(second, vec![b, t2]);
        assert_eq!(retained, vec![(q, b)]);

        let rest: Vec<_> = t.drain(LAYOUT).affected().deterministic().run().collect();
        assert_eq!(rest, vec![q]);
    }

    #[test]
    fn retain_out_of_scope_is_idempotent_under_eager_marking() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        siblings(&mut t);
        // Eager marking already invalidated every dependent, including `b` and `q`.
        t.mark_with(1, LAYOUT, &crate::EagerPolicy);

        let mut retained = Vec::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .within_dependencies_of(4)
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 4]);
        assert_eq!(retained, vec![(3, 1)]);
        let rest: Vec<_> = t
            .drain(LAYOUT)
            .invalidated_only()
            .deterministic()
            .run()
            .collect();
        assert_eq!(rest, vec![3, 5]);
    }

    #[test]
    fn retain_out_of_scope_ignores_untargeted_and_invalidated_only_drains() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        siblings(&mut t);
        t.mark_with(1, LAYOUT, &LazyPolicy);

        let mut retained = Vec::new();
        let order: Vec<_> = t
            .drain(LAYOUT)
            .invalidated_only()
            .within_dependencies_of(4)
            .retain_out_of_scope(&mut retained)
            .run()
            .collect();
        assert_eq!(order, vec![1]);
        assert!(retained.is_empty());

        t.mark_with(1, LAYOUT, &LazyPolicy);
        let all: Vec<_> = t
            .drain(LAYOUT)
            .affected()
            .retain_out_of_scope(&mut retained)
            .deterministic()
            .run()
            .collect();
        assert_eq!(all, vec![1, 2, 3, 4, 5]);
        assert!(retained.is_empty());
        assert_eq!(t.drain(LAYOUT).affected().run().count(), 0);
    }

    #[test]
    fn deterministic_diamond_is_total() {
        let mut t = InvalidationTracker::<u32>::with_cycle_handling(CycleHandling::Error);
        // 1 <- 2, 1 <- 3, 2 <- 4, 3 <- 4
        t.add_dependency(2, 1, LAYOUT).unwrap();
        t.add_dependency(3, 1, LAYOUT).unwrap();
        t.add_dependency(4, 2, LAYOUT).unwrap();
        t.add_dependency(4, 3, LAYOUT).unwrap();

        t.mark(1, LAYOUT);
        t.mark(2, LAYOUT);
        t.mark(3, LAYOUT);
        t.mark(4, LAYOUT);

        let order: Vec<_> = t
            .drain(LAYOUT)
            .invalidated_only()
            .deterministic()
            .run()
            .collect();
        assert_eq!(order, vec![1, 2, 3, 4]);
    }
}

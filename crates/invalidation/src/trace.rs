// Copyright 2025 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Explainability helpers for invalidation propagation.
//!
//! The core invalidation structures intentionally do not store provenance for
//! why a key became invalidated. For many embedders, it is useful to answer
//! questions like: "Why is this key invalidated?".
//!
//! Operations that propagate invalidation can report what they do to an
//! [`InvalidationTrace`]: each `(key, channel)` they reach, with its
//! [`InvalidationCause`]. The causes cover every way invalidation travels: an
//! explicit mark, a same-channel dependency, a channel cascade, and a
//! cross-channel edge. The traced operations are
//! [`InvalidationTracker::mark_with_trace`](crate::InvalidationTracker::mark_with_trace)
//! (the full closure), [`PropagationPolicy::propagate_with_trace`](crate::PropagationPolicy::propagate_with_trace)
//! (one channel), and affected drains with
//! [`DrainBuilder::trace`](crate::DrainBuilder::trace).
//!
//! [`OneParentRecorder`] stores **one plausible cause** per `(key, channel)`
//! (a spanning forest) and walks it back to a root.
//!
//! If you need “all roots / all paths”, that is a separate, explicitly-scoped
//! feature: it can be much more expensive in both time and memory.
//!
//! ```
//! use invalidation::{
//!     Channel, EagerPolicy, InvalidationTracker, OneParentRecorder, TraversalScratch,
//! };
//!
//! const LAYOUT: Channel = Channel::new(0);
//! const PAINT: Channel = Channel::new(1);
//!
//! let mut tracker = InvalidationTracker::<u32>::new();
//! tracker.add_dependency(2, 1, LAYOUT).unwrap();
//! tracker.add_cascade(LAYOUT, PAINT).unwrap();
//! tracker.add_cross_dependency(2, PAINT, 7, LAYOUT);
//!
//! let mut scratch = TraversalScratch::new();
//! let mut trace = OneParentRecorder::new();
//! tracker.mark_with_trace(1, LAYOUT, &EagerPolicy, &mut scratch, &mut trace);
//!
//! // Why is 7 invalidated for layout? Layout of 1 invalidated layout of 2,
//! // which cascaded to paint of 2, which feeds layout of 7.
//! assert_eq!(
//!     trace.explain_path(7, LAYOUT),
//!     Some(vec![(1, LAYOUT), (2, LAYOUT), (2, PAINT), (7, LAYOUT)])
//! );
//! ```

use alloc::vec::Vec;
use core::hash::Hash;

use hashbrown::{HashMap, HashSet};

use crate::Channel;

/// The recorded cause of invalidation for a `(key, channel)` pair.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum InvalidationCause<K> {
    /// The traced operation started here: the key was explicitly marked, or
    /// was already invalidated when the operation (such as an affected drain)
    /// began.
    Root,
    /// The key depends on `key` in the same channel, which was invalidated.
    Dependency {
        /// The dependency that caused this key to become invalidated.
        key: K,
    },
    /// The same key was invalidated in `channel`, which cascades to this
    /// channel directly or through other channels (see
    /// [`ChannelCascade::cascades_from`](crate::ChannelCascade::cascades_from)).
    /// A path through intermediate channels is reported as one step.
    Cascade {
        /// The channel the invalidation cascaded from.
        channel: Channel,
    },
    /// A cross-channel edge from `(key, channel)` leads here (see
    /// [`CrossChannelEdges`](crate::CrossChannelEdges)).
    CrossChannel {
        /// The key the edge comes from.
        key: K,
        /// The channel the edge comes from.
        channel: Channel,
    },
}

impl<K: Copy> InvalidationCause<K> {
    /// Returns the `(key, channel)` that caused `(key, channel)` to become
    /// invalidated, or `None` for a [`Root`](Self::Root).
    #[must_use]
    pub fn source(self, key: K, channel: Channel) -> Option<(K, Channel)> {
        match self {
            Self::Root => None,
            Self::Dependency { key: because } => Some((because, channel)),
            Self::Cascade { channel: from } => Some((key, from)),
            Self::CrossChannel {
                key: because,
                channel: from,
            } => Some((because, from)),
        }
    }
}

/// A sink for propagation traces.
///
/// Traced operations call [`record`](Self::record) for every `(key, channel)`
/// they reach, including pairs that were already invalidated, so a recorder
/// sees each cause the operation followed. See the [module docs](self).
pub trait InvalidationTrace<K> {
    /// Called when `(key, channel)` is reached because of `cause`.
    ///
    /// `newly_invalidated` indicates whether the pair was newly inserted into
    /// the invalidation set, or was already invalidated.
    fn record(
        &mut self,
        key: K,
        channel: Channel,
        cause: InvalidationCause<K>,
        newly_invalidated: bool,
    );
}

/// Records one cause per invalidated `(key, channel)` (a spanning forest).
///
/// This stores a best-effort explanation path for *some* cause chain. When a
/// pair has multiple possible upstream causes, the first one observed wins.
///
/// # Example
///
/// ```
/// use invalidation::{
///     Channel, CycleHandling, EagerPolicy, InvalidationCause, InvalidationGraph,
///     InvalidationSet, OneParentRecorder, PropagationPolicy, TraversalScratch,
/// };
///
/// const LAYOUT: Channel = Channel::new(0);
///
/// let mut graph = InvalidationGraph::<u32>::new();
/// graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
/// graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
///
/// let mut invalidated = InvalidationSet::new();
/// let mut scratch = TraversalScratch::new();
/// let mut trace = OneParentRecorder::new();
///
/// EagerPolicy.propagate_with_trace(1, LAYOUT, &graph, &mut invalidated, &mut scratch, &mut trace);
///
/// assert_eq!(trace.cause(1, LAYOUT), Some(InvalidationCause::Root));
/// assert_eq!(trace.cause(3, LAYOUT), Some(InvalidationCause::Dependency { key: 2 }));
/// assert_eq!(
///     trace.explain_path(3, LAYOUT),
///     Some(vec![(1, LAYOUT), (2, LAYOUT), (3, LAYOUT)])
/// );
/// ```
#[derive(Debug, Default, Clone)]
pub struct OneParentRecorder<K>
where
    K: Copy + Eq + Hash,
{
    causes: HashMap<(K, Channel), InvalidationCause<K>>,
}

impl<K> OneParentRecorder<K>
where
    K: Copy + Eq + Hash,
{
    /// Creates an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            causes: HashMap::new(),
        }
    }

    /// Clears all recorded causes.
    pub fn clear(&mut self) {
        self.causes.clear();
    }

    /// Returns the number of `(key, channel)` pairs with a recorded cause.
    #[must_use]
    pub fn len(&self) -> usize {
        self.causes.len()
    }

    /// Returns `true` if no cause has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.causes.is_empty()
    }

    /// Returns the recorded cause for `(key, channel)`, if any.
    #[must_use]
    pub fn cause(&self, key: K, channel: Channel) -> Option<InvalidationCause<K>> {
        self.causes.get(&(key, channel)).copied()
    }

    /// Returns one plausible path from a root to `(key, channel)`.
    ///
    /// The returned vector is ordered from the root to `(key, channel)`
    /// (inclusive); consecutive entries are linked by the recorded
    /// [`InvalidationCause`]s. It returns `None` if some pair on the way has
    /// no recorded cause.
    #[must_use]
    pub fn explain_path(&self, key: K, channel: Channel) -> Option<Vec<(K, Channel)>> {
        let mut out = Vec::new();
        let mut seen: HashSet<(K, Channel)> = HashSet::new();

        let mut current = (key, channel);
        loop {
            if !seen.insert(current) {
                return None;
            }
            out.push(current);

            match self
                .cause(current.0, current.1)?
                .source(current.0, current.1)
            {
                None => break,
                Some(source) => current = source,
            }
        }

        out.reverse();
        Some(out)
    }
}

impl<K> InvalidationTrace<K> for OneParentRecorder<K>
where
    K: Copy + Eq + Hash,
{
    fn record(
        &mut self,
        key: K,
        channel: Channel,
        cause: InvalidationCause<K>,
        _newly_invalidated: bool,
    ) {
        self.causes.entry((key, channel)).or_insert(cause);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::{
        CycleHandling, EagerPolicy, InvalidationGraph, InvalidationSet, PropagationPolicy,
        TraversalScratch,
    };
    use alloc::vec;

    const LAYOUT: Channel = Channel::new(0);

    #[test]
    fn records_one_parent_path() {
        // 1 <- 2 <- 3
        let mut g = InvalidationGraph::<u32>::new();
        g.add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        g.add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::<u32>::new();
        let mut scratch = TraversalScratch::new();
        let mut rec = OneParentRecorder::new();

        EagerPolicy.propagate_with_trace(1, LAYOUT, &g, &mut invalidated, &mut scratch, &mut rec);

        assert_eq!(
            rec.explain_path(3, LAYOUT).unwrap(),
            vec![(1, LAYOUT), (2, LAYOUT), (3, LAYOUT)]
        );
    }

    #[test]
    fn can_fill_in_missing_causes_for_already_invalidated_keys() {
        // 1 <- 2 <- 3
        let mut g = InvalidationGraph::<u32>::new();
        g.add_dependency(2, 1, LAYOUT, CycleHandling::Error)
            .unwrap();
        g.add_dependency(3, 2, LAYOUT, CycleHandling::Error)
            .unwrap();

        let mut invalidated = InvalidationSet::<u32>::new();
        // Pretend this was invalidated without tracing.
        invalidated.mark(2, LAYOUT);

        let mut scratch = TraversalScratch::new();
        let mut rec = OneParentRecorder::new();
        EagerPolicy.propagate_with_trace(1, LAYOUT, &g, &mut invalidated, &mut scratch, &mut rec);

        assert_eq!(
            rec.explain_path(2, LAYOUT).unwrap(),
            vec![(1, LAYOUT), (2, LAYOUT)]
        );
    }
}

// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Channel-to-channel cascade rules.
//!
//! A [`ChannelCascade`] encodes a DAG of "if a key is invalidated on channel A,
//! also mark it on channel B" rules. The transitive closure is precomputed on
//! every mutation so that [`cascades_from`](ChannelCascade::cascades_from) is a
//! single bitfield read at mark time.
//!
//! Most callers should configure cascades through
//! [`InvalidationTracker::add_cascade`](crate::InvalidationTracker::add_cascade)
//! so the tracker applies them whenever keys are marked. Use
//! [`ChannelCascade`] directly when embedding these rules in a custom
//! coordinator.

use core::fmt;

use crate::channel::{Channel, ChannelSet};

/// Maximum number of channels supported (64).
const MAX_CHANNELS: usize = 64;

/// Error returned when adding a cascade would create a cycle.
#[derive(Clone, PartialEq, Eq)]
pub struct CascadeCycleError {
    /// The source channel of the cascade that would create a cycle.
    pub from: Channel,
    /// The target channel of the cascade that would create a cycle.
    pub to: Channel,
}

impl fmt::Debug for CascadeCycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CascadeCycleError {{ from: {:?}, to: {:?} }}",
            self.from, self.to
        )
    }
}

impl fmt::Display for CascadeCycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "adding cascade {:?} -> {:?} would create a cycle",
            self.from, self.to
        )
    }
}

impl core::error::Error for CascadeCycleError {}

/// Channel-to-channel cascade rules (same key, different channels).
///
/// A `ChannelCascade` models a DAG over channels: "if a key is invalidated on
/// channel A, also mark it on channels B, C, …". For example, layout changes
/// might cascade to paint, and paint to compositing.
///
/// The transitive closure is precomputed on every [`add_cascade`](Self::add_cascade)
/// / [`remove_cascade`](Self::remove_cascade) call (at most 64×64 = 4096 ops),
/// so [`cascades_from`](Self::cascades_from) is a single bitfield read.
///
/// # Common tracker usage
///
/// ```
/// use invalidation::{Channel, InvalidationTracker};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
///
/// let mut tracker = InvalidationTracker::<u32>::new();
/// tracker.add_cascade(LAYOUT, PAINT).unwrap();
///
/// tracker.mark(1, LAYOUT);
/// assert!(tracker.is_invalidated(1, LAYOUT));
/// assert!(tracker.is_invalidated(1, PAINT));
/// ```
///
/// # Standalone usage
///
/// ```
/// use invalidation::{Channel, ChannelCascade};
///
/// const LAYOUT: Channel = Channel::new(0);
/// const PAINT: Channel = Channel::new(1);
/// const COMPOSITE: Channel = Channel::new(2);
///
/// let mut cascade = ChannelCascade::new();
/// cascade.add_cascade(LAYOUT, PAINT).unwrap();
/// cascade.add_cascade(PAINT, COMPOSITE).unwrap();
///
/// // Transitive: layout cascades to both paint and composite.
/// let targets = cascade.cascades_from(LAYOUT);
/// assert!(targets.contains(PAINT));
/// assert!(targets.contains(COMPOSITE));
///
/// // Direct: layout only directly cascades to paint.
/// let direct = cascade.direct_cascades_from(LAYOUT);
/// assert!(direct.contains(PAINT));
/// assert!(!direct.contains(COMPOSITE));
/// ```
#[derive(Clone)]
pub struct ChannelCascade {
    /// Direct cascade targets per channel.
    direct: [ChannelSet; MAX_CHANNELS],
    /// Precomputed transitive closure per channel.
    transitive: [ChannelSet; MAX_CHANNELS],
}

impl fmt::Debug for ChannelCascade {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelCascade")
            .field("direct", &self.direct)
            .field("transitive", &self.transitive)
            .finish()
    }
}

impl Default for ChannelCascade {
    fn default() -> Self {
        Self::new()
    }
}

impl ChannelCascade {
    /// Creates a new empty cascade with no rules.
    #[must_use]
    pub fn new() -> Self {
        Self {
            direct: [ChannelSet::EMPTY; MAX_CHANNELS],
            transitive: [ChannelSet::EMPTY; MAX_CHANNELS],
        }
    }

    /// Adds a cascade rule: invalidation on `from` also marks `to`.
    ///
    /// Returns `Ok(true)` if the rule was newly added, `Ok(false)` if it
    /// already existed, or `Err(CascadeCycleError)` if adding the rule would
    /// create a cycle.
    ///
    /// Self-cascades (`from == to`) are treated as cycles.
    pub fn add_cascade(&mut self, from: Channel, to: Channel) -> Result<bool, CascadeCycleError> {
        // Self-cascade is a trivial cycle.
        if from == to {
            return Err(CascadeCycleError { from, to });
        }

        // Already present?
        if self.direct[from.index() as usize].contains(to) {
            return Ok(false);
        }

        // Would adding from -> to create a cycle?
        // That happens if `from` is reachable from `to` via existing direct edges.
        if self.is_reachable(to, from) {
            return Err(CascadeCycleError { from, to });
        }

        self.direct[from.index() as usize].insert(to);
        self.recompute_transitive();
        Ok(true)
    }

    /// Removes a cascade rule.
    ///
    /// Returns `true` if the rule existed and was removed.
    pub fn remove_cascade(&mut self, from: Channel, to: Channel) -> bool {
        let idx = from.index() as usize;
        if !self.direct[idx].contains(to) {
            return false;
        }
        self.direct[idx].remove(to);
        self.recompute_transitive();
        true
    }

    /// Returns the transitive cascade targets for a channel.
    ///
    /// If channel A cascades to B and B cascades to C, then
    /// `cascades_from(A)` contains both B and C.
    ///
    /// Returns [`ChannelSet::EMPTY`] when no cascades are configured,
    /// making the check a single `u64 == 0` branch.
    #[inline]
    #[must_use]
    pub fn cascades_from(&self, channel: Channel) -> ChannelSet {
        self.transitive[channel.index() as usize]
    }

    /// Returns the direct (non-transitive) cascade targets for a channel.
    #[inline]
    #[must_use]
    pub fn direct_cascades_from(&self, channel: Channel) -> ChannelSet {
        self.direct[channel.index() as usize]
    }

    /// Returns `true` if no cascade rules are configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.direct.iter().all(|cs| cs.is_empty())
    }

    /// Checks whether `target` is reachable from `start` via direct edges.
    fn is_reachable(&self, start: Channel, target: Channel) -> bool {
        // BFS over the 64-node channel graph.
        let mut visited = ChannelSet::EMPTY;
        let mut queue = ChannelSet::EMPTY;
        queue.insert(start);

        while !queue.is_empty() {
            // Pop the first channel from the queue.
            let ch = queue.iter().next().unwrap();
            queue.remove(ch);

            if ch == target {
                return true;
            }

            if visited.contains(ch) {
                continue;
            }
            visited.insert(ch);

            // Enqueue direct targets not yet visited.
            let targets = self.direct[ch.index() as usize];
            // Add targets that haven't been visited.
            let new_targets = targets & !visited;
            queue |= new_targets;
        }

        false
    }

    /// Recomputes the transitive closure for all channels.
    ///
    /// For each channel, follows direct edges transitively to build the
    /// complete set of reachable channels. At most 64 channels × 64 channels
    /// = 4096 ops.
    fn recompute_transitive(&mut self) {
        for i in 0..MAX_CHANNELS {
            let mut reachable = ChannelSet::EMPTY;
            let mut frontier = self.direct[i];

            while !frontier.is_empty() {
                reachable |= frontier;
                let mut next_frontier = ChannelSet::EMPTY;
                for ch in frontier {
                    // Add direct targets of `ch` that aren't already reachable.
                    let targets = self.direct[ch.index() as usize] & !reachable;
                    next_frontier |= targets;
                }
                frontier = next_frontier;
            }

            self.transitive[i] = reachable;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: Channel = Channel::new(0);
    const PAINT: Channel = Channel::new(1);
    const COMPOSITE: Channel = Channel::new(2);
    const A11Y: Channel = Channel::new(3);

    #[test]
    fn new_cascade_is_empty() {
        let cascade = ChannelCascade::new();
        assert!(cascade.is_empty());
        assert!(cascade.cascades_from(LAYOUT).is_empty());
        assert!(cascade.direct_cascades_from(LAYOUT).is_empty());
    }

    #[test]
    fn add_single_cascade() {
        let mut cascade = ChannelCascade::new();
        let added = cascade.add_cascade(LAYOUT, PAINT).unwrap();
        assert!(added);
        assert!(!cascade.is_empty());

        assert!(cascade.direct_cascades_from(LAYOUT).contains(PAINT));
        assert!(cascade.cascades_from(LAYOUT).contains(PAINT));

        // Reverse direction is not set.
        assert!(!cascade.cascades_from(PAINT).contains(LAYOUT));
    }

    #[test]
    fn add_duplicate_returns_false() {
        let mut cascade = ChannelCascade::new();
        assert!(cascade.add_cascade(LAYOUT, PAINT).unwrap());
        assert!(!cascade.add_cascade(LAYOUT, PAINT).unwrap());
    }

    #[test]
    fn transitive_closure() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.add_cascade(PAINT, COMPOSITE).unwrap();

        // Direct: LAYOUT -> PAINT only.
        let direct = cascade.direct_cascades_from(LAYOUT);
        assert!(direct.contains(PAINT));
        assert!(!direct.contains(COMPOSITE));

        // Transitive: LAYOUT -> {PAINT, COMPOSITE}.
        let transitive = cascade.cascades_from(LAYOUT);
        assert!(transitive.contains(PAINT));
        assert!(transitive.contains(COMPOSITE));

        // PAINT -> COMPOSITE only (not LAYOUT).
        let paint_trans = cascade.cascades_from(PAINT);
        assert!(paint_trans.contains(COMPOSITE));
        assert!(!paint_trans.contains(LAYOUT));
    }

    #[test]
    fn self_cascade_is_cycle() {
        let mut cascade = ChannelCascade::new();
        let err = cascade.add_cascade(LAYOUT, LAYOUT).unwrap_err();
        assert_eq!(err.from, LAYOUT);
        assert_eq!(err.to, LAYOUT);
    }

    #[test]
    fn direct_cycle_detected() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();

        let err = cascade.add_cascade(PAINT, LAYOUT).unwrap_err();
        assert_eq!(err.from, PAINT);
        assert_eq!(err.to, LAYOUT);
    }

    #[test]
    fn transitive_cycle_detected() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.add_cascade(PAINT, COMPOSITE).unwrap();

        // COMPOSITE -> LAYOUT would create LAYOUT -> PAINT -> COMPOSITE -> LAYOUT.
        let err = cascade.add_cascade(COMPOSITE, LAYOUT).unwrap_err();
        assert_eq!(err.from, COMPOSITE);
        assert_eq!(err.to, LAYOUT);
    }

    #[test]
    fn remove_cascade() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.add_cascade(PAINT, COMPOSITE).unwrap();

        // Remove the middle link.
        assert!(cascade.remove_cascade(LAYOUT, PAINT));

        // LAYOUT no longer cascades to anything.
        assert!(cascade.cascades_from(LAYOUT).is_empty());
        assert!(cascade.direct_cascades_from(LAYOUT).is_empty());

        // PAINT still cascades to COMPOSITE.
        assert!(cascade.cascades_from(PAINT).contains(COMPOSITE));
    }

    #[test]
    fn remove_nonexistent_returns_false() {
        let mut cascade = ChannelCascade::new();
        assert!(!cascade.remove_cascade(LAYOUT, PAINT));
    }

    #[test]
    fn remove_allows_previously_cyclic_edge() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.add_cascade(PAINT, COMPOSITE).unwrap();

        // COMPOSITE -> LAYOUT would be a cycle.
        assert!(cascade.add_cascade(COMPOSITE, LAYOUT).is_err());

        // Remove LAYOUT -> PAINT to break the chain.
        cascade.remove_cascade(LAYOUT, PAINT);

        // Now COMPOSITE -> LAYOUT is allowed.
        assert!(cascade.add_cascade(COMPOSITE, LAYOUT).is_ok());
    }

    #[test]
    fn diamond_cascade() {
        let mut cascade = ChannelCascade::new();
        // LAYOUT -> PAINT, LAYOUT -> A11Y, PAINT -> COMPOSITE, A11Y -> COMPOSITE
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.add_cascade(LAYOUT, A11Y).unwrap();
        cascade.add_cascade(PAINT, COMPOSITE).unwrap();
        cascade.add_cascade(A11Y, COMPOSITE).unwrap();

        let targets = cascade.cascades_from(LAYOUT);
        assert!(targets.contains(PAINT));
        assert!(targets.contains(A11Y));
        assert!(targets.contains(COMPOSITE));
        assert_eq!(targets.len(), 3);
    }

    #[test]
    fn is_empty_after_remove_all() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(LAYOUT, PAINT).unwrap();
        cascade.remove_cascade(LAYOUT, PAINT);
        assert!(cascade.is_empty());
    }

    #[test]
    fn cascade_cycle_error_display() {
        let err = CascadeCycleError {
            from: LAYOUT,
            to: PAINT,
        };
        let msg = alloc::format!("{err}");
        assert!(msg.contains("cascade"));
        assert!(msg.contains("cycle"));
    }
}

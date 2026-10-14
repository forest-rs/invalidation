// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Cycles through the tracker's combined graph.
//!
//! An [`InvalidationTracker`](crate::InvalidationTracker) moves invalidation
//! between `(key, channel)` pairs three ways: a dependency within a channel, a
//! channel cascade (same key), and a cross-channel edge. The graph keeps each
//! channel's dependencies acyclic and the cascade keeps channels acyclic, so a
//! cycle through all three must run through at least one cross-channel edge.
//! This module finds such cycles: a cheap channel-level check first, and a
//! search over pairs only when that check cannot rule a cycle out.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt;
use core::hash::Hash;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;

use crate::cascade::ChannelCascade;
use crate::channel::{Channel, ChannelSet};
use crate::cross_channel::CrossChannelEdges;
use crate::drain::DenseKey;
use crate::graph::InvalidationGraph;

/// Error returned when adding a cross-channel edge would create a cycle.
///
/// Invalidation would flow from `from` to `to` over the new edge, and the
/// existing rules already carry it from `to` back to `from`.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CrossCycleError<K> {
    /// The pair the new edge would lead from.
    pub from: (K, Channel),
    /// The pair the new edge would lead to.
    pub to: (K, Channel),
    /// The existing chain from `to` back to `from`.
    ///
    /// It starts with `to` and ends with `from`, and invalidating each pair
    /// invalidates the next, through a dependency, a channel cascade, or a
    /// cross-channel edge; so `from -> to` followed by this chain is the
    /// cycle. For an edge from a pair to itself it is just `[from]`.
    pub path: Vec<(K, Channel)>,
}

impl<K: fmt::Debug> fmt::Debug for CrossCycleError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CrossCycleError")
            .field("from", &self.from)
            .field("to", &self.to)
            .field("path", &self.path)
            .finish()
    }
}

impl<K: fmt::Debug> fmt::Display for CrossCycleError<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (from_key, from_ch) = &self.from;
        let (to_key, to_ch) = &self.to;
        write!(
            f,
            "adding cross-channel edge ({from_key:?}, {from_ch:?}) -> ({to_key:?}, {to_ch:?}) \
             would create a cycle: ({from_key:?}, {from_ch:?})"
        )?;
        for (key, channel) in &self.path {
            write!(f, " -> ({key:?}, {channel:?})")?;
        }
        Ok(())
    }
}

impl<K: fmt::Debug> core::error::Error for CrossCycleError<K> {}

/// Formats a chain of pairs as ` -> (index, channel)`, for messages about
/// keys that need not implement `Debug`.
pub(crate) struct IndexPairs<'a, K>(pub(crate) &'a [(K, Channel)]);

impl<K: DenseKey> fmt::Display for IndexPairs<'_, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (key, channel) in self.0 {
            write!(f, " -> ({}, {channel:?})", key.index())?;
        }
        Ok(())
    }
}

/// Which channels invalidation can reach from which, through cascades and
/// cross-channel edges, ignoring keys.
#[derive(Clone, Debug)]
pub(crate) struct ChannelReach {
    /// The cross-channel pair generation this was computed for.
    generation: u64,
    /// The channels reachable from each channel, itself included.
    reach: [ChannelSet; 64],
    /// The channels reachable from each channel by a path that uses at least
    /// one cross-channel edge.
    crossing: [ChannelSet; 64],
}

impl ChannelReach {
    /// Computes reachability from the cascade and the cross-channel pairs.
    pub(crate) fn new<K>(cascade: &ChannelCascade, cross: &CrossChannelEdges<K>) -> Self
    where
        K: Copy + Eq + Hash,
    {
        let mut cross_out = [ChannelSet::EMPTY; 64];
        for (from, to) in cross.channel_pairs() {
            cross_out[from.index() as usize].insert(to);
        }

        let mut reach = [ChannelSet::EMPTY; 64];
        for start in ChannelSet::ALL {
            let mut seen = start.into_set();
            let mut frontier = seen;
            while !frontier.is_empty() {
                let mut next = ChannelSet::EMPTY;
                for channel in frontier {
                    next |= cascade.direct_cascades_from(channel);
                    next |= cross_out[channel.index() as usize];
                }
                frontier = next & !seen;
                seen |= next;
            }
            reach[start.index() as usize] = seen;
        }

        let mut crossing = [ChannelSet::EMPTY; 64];
        for start in ChannelSet::ALL {
            let mut through = ChannelSet::EMPTY;
            for source in reach[start.index() as usize] {
                for target in cross_out[source.index() as usize] {
                    through |= reach[target.index() as usize];
                }
            }
            crossing[start.index() as usize] = through;
        }

        Self {
            generation: cross.pair_generation(),
            reach,
            crossing,
        }
    }

    /// Returns `true` if this was computed for `cross`'s current pairs.
    pub(crate) fn is_current<K>(&self, cross: &CrossChannelEdges<K>) -> bool
    where
        K: Copy + Eq + Hash,
    {
        self.generation == cross.pair_generation()
    }

    /// Returns `true` if invalidation in `from` can reach `to`.
    pub(crate) fn reaches(&self, from: Channel, to: Channel) -> bool {
        self.reach[from.index() as usize].contains(to)
    }

    /// Returns `true` if invalidation in `from` can reach `to` through at
    /// least one cross-channel edge.
    pub(crate) fn reaches_crossing(&self, from: Channel, to: Channel) -> bool {
        self.crossing[from.index() as usize].contains(to)
    }
}

/// The tracker's rules, for searching the combined graph.
pub(crate) struct Rules<'a, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    pub(crate) graph: &'a InvalidationGraph<K>,
    pub(crate) cascade: &'a ChannelCascade,
    pub(crate) cross: &'a CrossChannelEdges<K>,
    pub(crate) reach: &'a ChannelReach,
}

impl<K> Rules<'_, K>
where
    K: Copy + Eq + Hash + DenseKey,
{
    /// Returns a shortest chain along which invalidating `start` invalidates
    /// `goal`, from `start` to `goal` inclusive, if there is one.
    ///
    /// The search skips channels from which `goal`'s channel is unreachable.
    pub(crate) fn flow_path(
        &self,
        start: (K, Channel),
        goal: (K, Channel),
    ) -> Option<Vec<(K, Channel)>> {
        if !self.reach.reaches(start.1, goal.1) {
            return None;
        }
        let mut reached_from: HashMap<(K, Channel), (K, Channel)> = HashMap::new();
        let mut frontier = VecDeque::new();
        reached_from.insert(start, start);
        frontier.push_back(start);

        while let Some(current) = frontier.pop_front() {
            if current == goal {
                let mut path = alloc::vec![goal];
                let mut pair = goal;
                while pair != start {
                    pair = reached_from[&pair];
                    path.push(pair);
                }
                path.reverse();
                return Some(path);
            }
            let (key, channel) = current;
            let mut visit = |next: (K, Channel)| {
                if !self.reach.reaches(next.1, goal.1) {
                    return;
                }
                if let Entry::Vacant(entry) = reached_from.entry(next) {
                    entry.insert(current);
                    frontier.push_back(next);
                }
            };
            for dependent in self.graph.dependents(key, channel) {
                visit((dependent, channel));
            }
            for to in self.cascade.direct_cascades_from(channel) {
                visit((key, to));
            }
            for next in self.cross.dependents(key, channel) {
                visit(next);
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CycleHandling;

    const L: Channel = Channel::new(0);
    const P: Channel = Channel::new(1);
    const S: Channel = Channel::new(2);

    #[test]
    fn channel_reach_separates_paths_through_cross_edges() {
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(L, P).unwrap();
        let mut cross = CrossChannelEdges::<u32>::new();
        cross.add_edge(1, P, 2, S);

        let reach = ChannelReach::new(&cascade, &cross);
        assert!(reach.reaches(L, S));
        assert!(reach.reaches_crossing(L, S));
        assert!(reach.reaches(L, P));
        assert!(
            !reach.reaches_crossing(L, P),
            "only the cascade leads there"
        );
        assert!(!reach.reaches(S, L));
        assert!(!reach.reaches_crossing(L, L));

        cross.add_edge(3, S, 4, L);
        assert!(!reach.is_current(&cross));
        let reach = ChannelReach::new(&cascade, &cross);
        assert!(reach.reaches_crossing(L, L), "L -> P -> S -> L");
    }

    #[test]
    fn flow_path_follows_all_three_kinds_of_rule() {
        let mut graph = InvalidationGraph::<u32>::new();
        graph.add_dependency(2, 1, L, CycleHandling::Error).unwrap();
        let mut cascade = ChannelCascade::new();
        cascade.add_cascade(L, P).unwrap();
        let mut cross = CrossChannelEdges::<u32>::new();
        cross.add_edge(2, P, 7, S);
        let reach = ChannelReach::new(&cascade, &cross);
        let rules = Rules {
            graph: &graph,
            cascade: &cascade,
            cross: &cross,
            reach: &reach,
        };

        assert_eq!(
            rules.flow_path((1, L), (7, S)),
            Some(alloc::vec![(1, L), (2, L), (2, P), (7, S)])
        );
        assert_eq!(rules.flow_path((7, S), (1, L)), None);
        assert_eq!(rules.flow_path((1, L), (1, L)), Some(alloc::vec![(1, L)]));
    }
}

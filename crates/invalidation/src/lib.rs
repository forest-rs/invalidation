// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Invalidation: generic invalidation and invalidation primitives.
//!
//! This crate provides building blocks for incremental computation systems
//! where changes to upstream data must propagate to downstream consumers.
//! It models invalidation as a combination of:
//!
//! - **Channels** ([`Channel`], [`ChannelSet`]): Named domains for invalidation
//!   tracking (for example, layout, paint, accessibility).
//! - **Dependency graphs** ([`InvalidationGraph`]): DAG of "A depends on B" edges,
//!   with cycle detection and bidirectional traversal.
//! - **Invalidation sets** ([`InvalidationSet`]): Accumulated invalidated keys
//!   per channel with
//!   generation tracking for stale-computation detection.
//! - **Propagation policies** ([`PropagationPolicy`], [`EagerPolicy`], [`LazyPolicy`]):
//!   Pluggable strategies for how invalidation spreads through the graph.
//! - **Topological drain** ([`DrainSorted`]): Kahn's algorithm to yield invalidated
//!   keys in dependency order.
//! - **Scratch buffers** ([`TraversalScratch`]): Reusable traversal state for
//!   tight loops to avoid repeated allocations.
//!
//! ## Quick Start
//!
//! ```rust
//! use invalidation::{Channel, InvalidationTracker, EagerPolicy};
//!
//! const LAYOUT: Channel = Channel::new(0);
//! const PAINT: Channel = Channel::new(1);
//!
//! let mut tracker = InvalidationTracker::<u32>::new();
//! // `u32` is fine for compact 0-based IDs. Sparse/external IDs should be
//! // interned first so dense storage grows with node count, not key magnitude.
//!
//! // Build dependency graph: 3 depends on 2, 2 depends on 1
//! tracker.add_dependency(2, 1, LAYOUT).unwrap();
//! tracker.add_dependency(3, 2, LAYOUT).unwrap();
//!
//! // Mark with eager propagation (marks 1, 2, 3)
//! tracker.mark_with(1, LAYOUT, &EagerPolicy);
//!
//! // Or mark manually without propagation
//! tracker.mark(1, PAINT);
//!
//! // Drain in topological order: 1, 2, 3
//! for key in tracker.drain_sorted(LAYOUT) {
//!     let _ = key;
//! }
//! ```
//!
//! ## Using Components Separately
//!
//! While [`InvalidationTracker`] provides a convenient combined API, you can also
//! use the underlying types directly for more control:
//!
//! ```rust
//! use invalidation::{
//!     Channel, CycleHandling, InvalidationGraph, InvalidationSet, EagerPolicy, PropagationPolicy,
//! };
//!
//! const LAYOUT: Channel = Channel::new(0);
//!
//! // Build the dependency graph
//! let mut graph = InvalidationGraph::<u32>::new();
//! // Dense storage expects compact key spaces; sparse/owned keys should be
//! // interned first.
//! graph.add_dependency(2, 1, LAYOUT, CycleHandling::Error).unwrap();
//! graph.add_dependency(3, 2, LAYOUT, CycleHandling::Error).unwrap();
//!
//! // Maintain invalidation state separately
//! let mut invalidated = InvalidationSet::new();
//! let eager = EagerPolicy;
//!
//! // Propagate invalidation marks
//! eager.propagate(1, LAYOUT, &graph, &mut invalidated);
//!
//! assert!(invalidated.is_invalidated(1, LAYOUT));
//! assert!(invalidated.is_invalidated(2, LAYOUT));
//! assert!(invalidated.is_invalidated(3, LAYOUT));
//! ```
//!
//! ## Propagation Policies
//!
//! The crate provides two built-in policies:
//!
//! - [`EagerPolicy`]: Immediately marks all transitive dependents when a key
//!   is marked invalidated. Use this when you need to know the full invalidation set
//!   immediately after marking. Use with [`InvalidationTracker::drain_sorted`].
//! - [`LazyPolicy`]: Only marks the key itself at mark time; no propagation
//!   occurs. Use [`InvalidationTracker::drain_affected_sorted`] to expand and process
//!   all affected keys at drain time.
//!
//! You can implement [`PropagationPolicy`] for custom strategies.
//!
//! ## Choosing a Drain Function
//!
//! - [`drain_sorted`] / [`InvalidationTracker::drain_sorted`]: Drain exactly the keys
//!   that are currently marked invalidated, in topological order.
//! - [`drain_affected_sorted`] / [`InvalidationTracker::drain_affected_sorted`]:
//!   Expand the invalidation set to include all transitive dependents before draining.
//!
//! ## Cycle Detection
//!
//! [`InvalidationGraph::add_dependency`] supports configurable cycle handling via
//! [`CycleHandling`]:
//!
//! - `DebugAssert` (default): Panic in debug builds, ignore in release.
//! - `Error`: Return `Err(CycleError)` if a cycle would be created.
//! - `Ignore`: Silently ignore the dependency.
//! - `Allow`: Skip cycle detection entirely.
//!
//! ## `no_std` Support
//!
//! This crate is `no_std` and uses `alloc`. It does not depend on `std`.

#![no_std]

extern crate alloc;

mod channel;
mod drain;
mod drain_builder;
mod graph;
pub mod intern;
mod policy;
mod scratch;
mod set;
pub mod trace;
mod tracker;

pub use channel::{Channel, ChannelSet, ChannelSetIter};
pub use drain::{
    DenseKey, DrainCompletion, DrainSorted, DrainSortedDeterministic, drain_affected_sorted,
    drain_affected_sorted_deterministic, drain_affected_sorted_with_trace, drain_sorted,
    drain_sorted_deterministic,
};
pub use drain_builder::{AnyOrder, DeterministicOrder, DrainBuilder};
pub use graph::{CycleError, CycleHandling, InvalidationGraph};
pub use intern::InternId;
pub use policy::{EagerPolicy, LazyPolicy, PropagationPolicy};
pub use scratch::TraversalScratch;
pub use set::InvalidationSet;
pub use trace::{InvalidationCause, InvalidationTrace, OneParentRecorder};
pub use tracker::InvalidationTracker;

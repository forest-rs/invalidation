<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Invalidation release is [0.2.0](#020-2026-04-25) which was released on 2026-04-25.
You can find its changes [documented below](#020-2026-04-25).

## [Unreleased]

This release has an [MSRV][] of 1.88.

### Added

- Added `DrainBuilder::retain_out_of_scope`. A targeted affected drain
  (`within_keys` or `within_dependencies_of`) expands dependents only inside
  its scope, so under `LazyPolicy` a key outside the scope that depends on a
  drained key lost its pending work. With this option the drain marks each such
  direct dependent invalidated and reports the `(dependent, drained key)` edges
  so callers can attach causes. The default behavior is unchanged.
- Added `remaining_keys` to `DrainSorted` and `DrainSortedDeterministic`,
  naming the keys a drain has not yielded, such as those behind a cycle after a
  stall.
- Added `InvalidationTracker::mark_with_trace`, which records why each
  `(key, channel)` in the closure became invalidated: the root, a dependency, a
  channel cascade, or a cross-channel edge.
- Added summary queries: `InvalidationSet::channels` (channels with pending
  work), `InvalidationGraph::edges`, `edge_count`, and `channels`, and
  `CrossChannelEdges::edges` and `len`.
- Channels combine into sets with `|`: `LAYOUT | PAINT`, `set | PAINT`, and
  `set |= PAINT`.
- Added `ChannelNames`, a `const`-buildable table of channel names, and
  `InvalidationTracker::set_channel_names` so the tracker's `Debug` output
  names channels.
- Added `InvalidationGraph::dependency_path`, a shortest chain of dependencies
  between two keys, answering "why does this key depend on that one?".

### Fixed

- With `LazyPolicy`, work that crossed channels was lost: a tracker's affected
  drain expanded dependents in its own channel only, so with layout cascading
  to paint, a relaid-out dependent was never repainted, and a cross-channel
  edge from it was never followed. A tracker's affected drain now follows the
  cascades and cross-channel edges of every key it takes, and of the keys
  `retain_out_of_scope` keeps, marking their targets in their channels as
  roots.
- The sorted drains' `size_hint` claimed every remaining key as a lower bound,
  but keys on or behind a cycle are never yielded; with a two-key cycle a drain
  promised at least two items and yielded none. The lower bound is now the
  number of keys ready to yield.

### Changed

- Breaking: a drain no longer loses the keys it does not yield. Keys still
  leave the invalidated set when a drain is created, but when the drain is
  dropped every key it has not yielded is marked invalidated again, so a drain
  stopped early or stalled on a cycle leaves that work pending. The drain
  iterators therefore borrow the `InvalidationSet` (and the free drain
  functions take it for as long as the drain lives). Peeks are unaffected.
- Breaking: `InvalidationTracker::drain_channels_sorted` returns
  `DrainedChannels`, which lists the channels whose drain stalled alongside the
  drained keys.
- Breaking: `CycleError` and `CascadeCycleError` carry the existing chain the
  rejected edge would close (`path`), and their messages print the whole cycle.
  Both are `#[non_exhaustive]`. With the default `CycleHandling::DebugAssert`,
  the debug-build panic now names the keys, channel, and cycle instead of only
  saying that one exists.
- Breaking: `add_dependency` (on the graph and the tracker) returns
  `AddDependency`, which tells a new edge from one that already existed and
  from one dropped because it would have created a cycle; all three used to be
  `true` or `false`. `replace_dependencies` returns `DependencyChanges`, listing
  the dependencies it added, removed, and dropped as cycles.
- `InvalidationTracker::mark_with` returns a `MarkReport` with the number of
  `(key, channel)` pairs it newly invalidated and the channels they are in, so
  a caller can tell a no-op from a change without comparing the set before and
  after. It also counts the closure's work: the steps it propagated from, and
  the dependents it walked.
- Breaking: traces now explain invalidation across channels.
  `InvalidationCause` has `Root`, `Dependency`, `Cascade`, and `CrossChannel`
  variants (`Because` became `Dependency`) and is `#[non_exhaustive]`;
  `InvalidationTrace` has a single `record` method taking the cause; and
  `OneParentRecorder::explain_path` returns `(key, channel)` pairs.
  `EagerPolicy::propagate_with_trace` moved to the `PropagationPolicy` trait,
  whose default reports only the root.
- `mark_with` no longer does quadratic work when it follows cascades or
  cross-channel edges. It walked every reached key's dependents again, and
  re-ran `EagerPolicy` on keys an earlier step had already covered; marking the
  head of a 4096-key DAG with a layout-to-paint cascade took 124 ms and now
  takes under 1 ms. `PropagationPolicy` gains `marks_all_dependents` (default
  `false`, `true` for `EagerPolicy`); when it is true the tracker marks the
  dependents itself, in one walk shared by every step of the closure, so
  cross-channel edges that reconverge on a shared tail no longer re-walk it
  once per edge. The closure now takes its steps first-in, first-out, so the
  first cause a trace records for a pair may differ from before. With a
  custom policy that marks some dependents, the closure follows the edges of
  keys that are invalidated when its walk first reaches them; a key such a
  policy invalidates only in a later step no longer has its edges followed.
- `DrainBuilder::scratch` after `DrainBuilder::trace` replaces the scratch and
  keeps the trace, instead of failing a debug assertion and dropping the trace
  in release builds.
- `Debug` for `InvalidationTracker`, `InvalidationGraph`, `InvalidationSet`,
  `ChannelCascade`, and `CrossChannelEdges` shows only channels in use, with
  edges as `from -> to`, instead of dumping 64 slots per direction.

## [0.2.0][] (2026-04-25)

This release has an [MSRV][] of 1.88.

### Added

- Added bulk cross-channel edge mutation APIs:
  `CrossChannelEdges::replace_dependents`,
  `CrossChannelEdges::clear_dependents`,
  `CrossChannelEdges::clear_dependencies`, and the corresponding
  `InvalidationTracker` wrappers.
- Added `ChannelCascade::from_edges` and
  `InvalidationTracker::with_cascades` for static cascade setup.
- Added runnable examples for cascade/cross-channel workflows and retained
  node-scoped multi-phase invalidation.

### Changed

- Updated docs to position `InvalidationTracker` as the common entry point and
  clarify tracker orchestration, invalidation generation semantics, channel
  edge usage, and `mark` versus `mark_with`.
- Updated `hashbrown` to 0.17.

### Removed

- Removed `InvalidationTracker::graph_mut`. Use
  `InvalidationTracker::from_graph` or
  `InvalidationTracker::from_graph_with_cycle_handling` to seed a tracker from
  an existing graph while keeping later graph mutations behind the tracker API.
- Removed `InvalidationTracker::invalidated_mut`. Use the tracker's `mark`,
  `mark_with`, `clear`, `clear_all`, and drain methods for coordinated
  invalidation state changes, or use a standalone `InvalidationSet` with the
  free drain helpers when bypassing tracker orchestration is intentional.
- Removed `InvalidationTracker::set_cycle_handling`. Use
  `InvalidationTracker::with_cycle_handling` or
  `InvalidationTracker::from_graph_with_cycle_handling` to choose the tracker's
  default, and use `add_dependency_with` or `replace_dependencies_with` when an
  operation needs a different cycle policy.

## [0.1.1][] (2026-04-05)

This release has an [MSRV][] of 1.88.

### Added

- Added standalone `ChannelCascade` and `CrossChannelEdges` primitives for
  modeling channel-to-channel and cross-key cross-channel invalidation. Added
  cross-channel invalidation support to `InvalidationTracker`, including
  multi-channel draining and cross-channel reachability queries. ([#2][] by [@waywardmonkeys][])

### Changed

- `InvalidationTracker::mark` and `InvalidationTracker::mark_with` now apply
  configured channel cascades, and `mark_with` follows configured
  cross-channel edges for the built-in eager and lazy policies. ([#2][] by [@waywardmonkeys][])

## [0.1.0][] (2026-03-11)

This release has an [MSRV][] of 1.92.

This is the initial release.

[#2]: https://github.com/forest-rs/invalidation/pull/2

[@waywardmonkeys]: https://github.com/waywardmonkeys

[Unreleased]: https://github.com/forest-rs/invalidation/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/forest-rs/invalidation/releases/tag/v0.2.0
[0.1.1]: https://github.com/forest-rs/invalidation/releases/tag/v0.1.1
[0.1.0]: https://github.com/forest-rs/invalidation/releases/tag/v0.1.0

[MSRV]: README.md#minimum-supported-rust-version-msrv

<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Invalidation release is [0.1.1](#011-2026-04-05) which was released on 2026-04-05.
You can find its changes [documented below](#011-2026-04-05).

## [Unreleased]

### Added

- Added bulk cross-channel edge mutation APIs:
  `CrossChannelEdges::replace_dependents`,
  `CrossChannelEdges::clear_dependents`,
  `CrossChannelEdges::clear_dependencies`, and the corresponding
  `InvalidationTracker` wrappers.
- Added `ChannelCascade::from_edges` and
  `InvalidationTracker::with_cascades` for static cascade setup.

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

[Unreleased]: https://github.com/forest-rs/invalidation/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/forest-rs/invalidation/releases/tag/v0.1.1
[0.1.0]: https://github.com/forest-rs/invalidation/releases/tag/v0.1.0

[MSRV]: README.md#minimum-supported-rust-version-msrv

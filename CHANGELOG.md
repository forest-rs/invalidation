<!-- Instructions

This changelog follows the patterns described here: <https://keepachangelog.com/en/>.

Subheadings to categorize changes are `added, changed, deprecated, removed, fixed, security`.

-->

# Changelog

The latest published Invalidation release is [0.1.0](#010-2026-03-11) which was released on 2026-03-11.
You can find its changes [documented below](#010-2026-03-11).

## [Unreleased]

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

[Unreleased]: https://github.com/forest-rs/invalidation/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/forest-rs/invalidation/releases/tag/v0.1.0

[MSRV]: README.md#minimum-supported-rust-version-msrv

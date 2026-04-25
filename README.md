# invalidation

`invalidation` is a small Rust workspace centered on
[`crates/invalidation`](./crates/invalidation), a `no_std` crate for generic
dependency-aware invalidation.

It is intended for incremental systems where upstream changes must mark
downstream work dirty, then process that work in a clear dependency order.

Most applications should start with `InvalidationTracker`. It coordinates the
dependency graph, invalidated keys, channel cascades, cross-channel edges, and
drain entry points. Lower-level pieces such as `InvalidationGraph`,
`InvalidationSet`, and `DrainBuilder` are available when an embedder already
owns part of that coordination.

## Quick Start

```rust
use invalidation::{Channel, EagerPolicy, InvalidationTracker};

const LAYOUT: Channel = Channel::new(0);

let mut tracker = InvalidationTracker::<u32>::new();
tracker.add_dependency(2, 1, LAYOUT).unwrap();
tracker.add_dependency(3, 2, LAYOUT).unwrap();

tracker.mark_with(1, LAYOUT, &EagerPolicy);

let ordered: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
assert_eq!(ordered, vec![1, 2, 3]);
```

## Common Workflows

| Goal | Use |
| --- | --- |
| Mark and eagerly propagate one changed key | `InvalidationTracker::mark_with` + `EagerPolicy` |
| Mark roots now and expand affected work later | `LazyPolicy` + `drain_affected_sorted` |
| Process work in dependency order | `drain_sorted` |
| Break ties deterministically | `tracker.drain(channel).deterministic().run()` |
| Limit a drain to part of the graph | `DrainBuilder::within_keys` or `DrainBuilder::within_dependencies_of` |
| Cascade one key across channels | `InvalidationTracker::add_cascade` |
| Connect different keys across channels | `InvalidationTracker::add_cross_dependency` |
| Use owned or sparse domain keys | `intern::Interner` |
| Explain why a key was invalidated | `OneParentRecorder` with tracing APIs |

## Examples

Runnable examples:

- `cargo run -p invalidation_examples --bin tracker_basics`
- `cargo run -p invalidation_examples --bin eager_vs_lazy`
- `cargo run -p invalidation_examples --bin tracing`
- `cargo run -p invalidation_examples --bin interner`

## Gotchas

- Edge direction matters: `add_dependency(a, b, ...)` means `a` depends on `b`.
- `LazyPolicy` should usually be paired with affected drains, not
  `drain_sorted`.
- Dense deterministic drains assume a compact key space.
- If you allow cycles, topological drains can stall.

## Minimum supported Rust Version (MSRV)

This version of Invalidation has been verified to compile with **Rust 1.88** and later.

Future versions of Invalidation might increase the Rust version requirement.
It will not be treated as a breaking change and as such can even happen with small patch releases.

# invalidation

`invalidation` is a small Rust workspace centered on
[`crates/invalidation`](./crates/invalidation), a `no_std` crate for generic
dependency-aware invalidation.

It is intended for incremental systems where upstream changes propagate through
a dependency graph and downstream work should be processed in explicit order.

Core pieces:

- `Channel` and `ChannelSet`
- `InvalidationGraph`
- `InvalidationSet`
- `EagerPolicy` and `LazyPolicy`
- `InvalidationTracker`
- `DrainBuilder` and sorted drain helpers

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

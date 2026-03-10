# invalidation

`invalidation` provides generic invalidation primitives for incremental
systems.

The crate is `#![no_std]` and centered on a small set of explicit building
blocks:

- `Channel` and `ChannelSet` for named invalidation domains
- `InvalidationGraph` for dependency edges and cycle handling
- `InvalidationSet` for accumulated invalidated keys
- `EagerPolicy` and `LazyPolicy` for propagation strategy
- `InvalidationTracker` for the combined convenience API
- `DrainBuilder` and deterministic drain helpers for ordered processing

## Quick Start

```rust
use invalidation::{Channel, InvalidationTracker, EagerPolicy};

const LAYOUT: Channel = Channel::new(0);

let mut tracker = InvalidationTracker::<u32>::new();
tracker.add_dependency(2, 1, LAYOUT).unwrap();
tracker.add_dependency(3, 2, LAYOUT).unwrap();

tracker.mark_with(1, LAYOUT, &EagerPolicy);

let ordered: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
assert_eq!(ordered, vec![1, 2, 3]);
```

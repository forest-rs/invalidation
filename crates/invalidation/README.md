# invalidation

`invalidation` provides dependency-aware invalidation primitives for
incremental systems.

It is a small `#![no_std]` crate for situations where upstream changes must
flow through a dependency graph and downstream work should be processed in a
clear, explicit order.

The crate is centered on a small set of building blocks:

- `Channel` and `ChannelSet` for named invalidation domains
- `InvalidationGraph` for dependency edges and cycle handling
- `InvalidationSet` for accumulated invalidated keys
- `EagerPolicy` and `LazyPolicy` for propagation strategy
- `InvalidationTracker` for the combined convenience API
- `DrainBuilder` and deterministic drain helpers for ordered processing

It intentionally does not own recomputation, caching, or scheduling. Those
remain in your application.

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

## Concepts

- key: a node identifier in your system
- channel: an invalidation domain such as layout or paint
- dependency: `A depends on B` means `B` must be processed before `A`
- invalidated root: a key you explicitly mark invalidated
- affected key: an invalidated root or one of its transitive dependents
- drain: consume invalidated work in dependency order and clear it

## Choosing The Main API

- Use `InvalidationTracker` for the most direct “just give me the pieces
  together” workflow.
- Use `InvalidationGraph` plus `InvalidationSet` separately if your embedder
  already owns state and only wants the primitives.
- Use `DrainBuilder` when you need deterministic ordering, targeted drains,
  scratch reuse, or tracing.
- Use `intern::Interner` when your natural keys are strings or other non-`Copy`
  values.

## Eager vs Lazy

Two workflows are intentionally first-class:

- `EagerPolicy`: propagate immediately at mark time, then use `drain_sorted`.
- `LazyPolicy`: mark only roots at change time, then expand with
  `drain_affected_sorted` or `DrainBuilder::affected`.

```rust
use invalidation::{Channel, EagerPolicy, InvalidationTracker, LazyPolicy};

const LAYOUT: Channel = Channel::new(0);

let mut eager = InvalidationTracker::<u32>::new();
eager.add_dependency(2, 1, LAYOUT).unwrap();
eager.add_dependency(3, 2, LAYOUT).unwrap();
eager.mark_with(1, LAYOUT, &EagerPolicy);
assert!(eager.invalidated().is_invalidated(3, LAYOUT));

let mut lazy = InvalidationTracker::<u32>::new();
lazy.add_dependency(2, 1, LAYOUT).unwrap();
lazy.add_dependency(3, 2, LAYOUT).unwrap();
lazy.mark_with(1, LAYOUT, &LazyPolicy);
assert!(!lazy.invalidated().is_invalidated(3, LAYOUT));

let ordered: Vec<_> = lazy.drain_affected_sorted(LAYOUT).collect();
assert_eq!(ordered, vec![1, 2, 3]);
```

## Deterministic And Targeted Drains

When ties must be stable, or you only want to process part of the invalidated
region, use `DrainBuilder`:

```rust
use invalidation::{Channel, CycleHandling, InvalidationTracker};

const LAYOUT: Channel = Channel::new(0);

let mut tracker = InvalidationTracker::<u32>::new();
tracker.add_dependency(2, 1, LAYOUT).unwrap();
tracker.add_dependency(3, 1, LAYOUT).unwrap();
tracker.add_dependency(4, 2, LAYOUT).unwrap();
tracker.add_dependency(5, 3, LAYOUT).unwrap();

tracker.mark(1, LAYOUT);
tracker.mark(2, LAYOUT);
tracker.mark(3, LAYOUT);
tracker.mark(4, LAYOUT);
tracker.mark(5, LAYOUT);

let focused: Vec<_> = tracker
    .drain(LAYOUT)
    .within_dependencies_of(4)
    .deterministic()
    .collect();

assert_eq!(focused, vec![1, 2, 4]);
assert!(tracker.invalidated().is_invalidated(3, LAYOUT));
assert!(tracker.invalidated().is_invalidated(5, LAYOUT));
```

## Non-`Copy` Keys

`invalidation` keeps its core APIs keyed by `K: Copy` so hot paths stay
predictable. If your natural keys are strings or other owned values, intern
them first:

```rust
use invalidation::{Channel, InvalidationTracker, LazyPolicy, intern::Interner};

const STYLE: Channel = Channel::new(0);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key(&'static str);

let mut ids = Interner::<Key>::new();
let stylesheet = ids.intern(Key("styles.css"));
let button = ids.intern(Key("button"));

let mut tracker = InvalidationTracker::new();
tracker.add_dependency(button, stylesheet, STYLE).unwrap();
tracker.mark_with(stylesheet, STYLE, &LazyPolicy);

let affected: Vec<_> = tracker.drain_affected_sorted(STYLE).collect();
assert_eq!(affected, vec![stylesheet, button]);
```

## Gotchas

- `add_dependency(a, b, ...)` means `a` depends on `b`, not the reverse.
- `LazyPolicy` and `drain_sorted` are usually the wrong pair.
- Deterministic dense drains assume a compact key space; use `Interner` when
  keys are sparse or structured.
- If cycles are allowed, topological drains can stall.

## Minimum Supported Rust Version (MSRV)

This version of `invalidation` has been verified to compile with **Rust
1.88** and later.

Future versions might increase the Rust version requirement. That is not
treated as a breaking change and may happen in minor or patch releases.

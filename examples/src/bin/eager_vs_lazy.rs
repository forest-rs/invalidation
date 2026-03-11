// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Compare eager invalidation at mark time with lazy invalidation at drain time.

use invalidation::{Channel, EagerPolicy, InvalidationTracker, LazyPolicy};

const LAYOUT: Channel = Channel::new(0);

fn main() {
    let mut eager = InvalidationTracker::<u32>::new();
    eager.add_dependency(2, 1, LAYOUT).unwrap();
    eager.add_dependency(3, 2, LAYOUT).unwrap();
    eager.mark_with(1, LAYOUT, &EagerPolicy);

    assert!(
        eager.invalidated().is_invalidated(1, LAYOUT),
        "eager propagation should keep the root invalidated"
    );
    assert!(
        eager.invalidated().is_invalidated(2, LAYOUT),
        "eager propagation should immediately mark transitive dependents"
    );
    assert!(
        eager.invalidated().is_invalidated(3, LAYOUT),
        "eager propagation should reach the end of the chain"
    );

    let mut lazy = InvalidationTracker::<u32>::new();
    lazy.add_dependency(2, 1, LAYOUT).unwrap();
    lazy.add_dependency(3, 2, LAYOUT).unwrap();
    lazy.mark_with(1, LAYOUT, &LazyPolicy);

    assert!(
        lazy.invalidated().is_invalidated(1, LAYOUT),
        "lazy propagation still marks the explicit root"
    );
    assert!(
        !lazy.invalidated().is_invalidated(2, LAYOUT),
        "lazy propagation should not mark dependents yet"
    );
    assert!(
        !lazy.invalidated().is_invalidated(3, LAYOUT),
        "lazy propagation should defer transitive work until drain time"
    );

    let affected: Vec<_> = lazy.drain_affected_sorted(LAYOUT).collect();
    assert_eq!(
        affected,
        vec![1, 2, 3],
        "affected drain should expand lazy roots into the full affected region"
    );

    println!("eager invalidated 1, 2, and 3 immediately");
    println!("lazy kept only the root invalidated until drain time");
    println!("lazy affected drain: {affected:?}");
}

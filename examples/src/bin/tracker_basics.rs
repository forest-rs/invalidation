// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Basic `InvalidationTracker` workflow with eager propagation and sorted drain.

use invalidation::{Channel, EagerPolicy, InvalidationTracker};

const LAYOUT: Channel = Channel::new(0);

fn main() {
    let mut tracker = InvalidationTracker::<u32>::new();

    // 1 -> 2 -> 3 as dependency order, encoded as:
    // 2 depends on 1, 3 depends on 2.
    tracker.add_dependency(2, 1, LAYOUT).unwrap();
    tracker.add_dependency(3, 2, LAYOUT).unwrap();

    tracker.mark_with(1, LAYOUT, &EagerPolicy);

    let ordered: Vec<_> = tracker.drain_sorted(LAYOUT).collect();
    assert_eq!(
        ordered,
        vec![1, 2, 3],
        "a chain should drain in dependency order"
    );

    println!("drained in dependency order: {ordered:?}");
}

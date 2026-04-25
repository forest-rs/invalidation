// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Combine same-key channel cascades with cross-key cross-channel dependencies.

use invalidation::{Channel, EagerPolicy, InvalidationTracker};

const LAYOUT: Channel = Channel::new(0);
const PAINT: Channel = Channel::new(1);

fn main() {
    let mut tracker = InvalidationTracker::<u32>::new();

    // Same key, different channel: layout invalidation also invalidates paint.
    tracker.add_cascade(LAYOUT, PAINT).unwrap();

    // Different key and channel: node 1's layout output feeds node 2's paint input.
    tracker.add_cross_dependency(1, LAYOUT, 2, PAINT);

    tracker.mark_with(1, LAYOUT, &EagerPolicy);

    assert!(
        tracker.is_invalidated(1, LAYOUT),
        "the original layout root should be invalidated"
    );
    assert!(
        tracker.is_invalidated(1, PAINT),
        "the layout-to-paint cascade should invalidate the same key on paint"
    );
    assert!(
        tracker.is_invalidated(2, PAINT),
        "the cross-channel edge should invalidate the dependent paint key"
    );

    let paint_work: Vec<_> = tracker.drain(PAINT).deterministic().run().collect();
    assert_eq!(
        paint_work,
        vec![1, 2],
        "paint work should include the cascaded key and cross-channel target"
    );

    println!("paint work after layout change: {paint_work:?}");
}

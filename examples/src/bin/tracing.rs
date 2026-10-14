// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Record why each `(key, channel)` became invalidated, across channels.

use invalidation::{
    Channel, EagerPolicy, InvalidationTracker, OneParentRecorder, TraversalScratch,
};

const LAYOUT: Channel = Channel::new(0);
const PAINT: Channel = Channel::new(1);

fn main() {
    let mut tracker = InvalidationTracker::<u32>::new();
    // 2 and 3 lay out from 1; a layout change repaints the same node; and
    // node 3's paint feeds node 7's layout (say, a measured thumbnail).
    tracker.add_dependency(2, 1, LAYOUT).unwrap();
    tracker.add_dependency(3, 2, LAYOUT).unwrap();
    tracker.add_cascade(LAYOUT, PAINT).unwrap();
    tracker.add_cross_dependency(3, PAINT, 7, LAYOUT).unwrap();

    let mut scratch = TraversalScratch::new();
    let mut trace = OneParentRecorder::new();
    let report = tracker.mark_with_trace(1, LAYOUT, &EagerPolicy, &mut scratch, &mut trace);
    println!(
        "marking layout of 1 invalidated {} (key, channel) pairs",
        report.newly_invalidated
    );

    let path = trace.explain_path(7, PAINT).unwrap();
    assert_eq!(
        path,
        vec![
            (1, LAYOUT),
            (2, LAYOUT),
            (3, LAYOUT),
            (3, PAINT),
            (7, LAYOUT),
            (7, PAINT)
        ],
        "the recorded cause path should lead from the root to the queried pair"
    );

    println!("why is 7 invalidated for paint?");
    for (key, channel) in path {
        println!("  {key} {channel:?}");
    }
}

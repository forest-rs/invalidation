// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Record one plausible cause path for eager invalidation propagation.

use invalidation::{
    Channel, CycleHandling, EagerPolicy, InvalidationGraph, InvalidationSet, OneParentRecorder,
    TraversalScratch,
};

const LAYOUT: Channel = Channel::new(0);

fn main() {
    let mut graph = InvalidationGraph::<u32>::new();
    graph
        .add_dependency(2, 1, LAYOUT, CycleHandling::Error)
        .unwrap();
    graph
        .add_dependency(3, 2, LAYOUT, CycleHandling::Error)
        .unwrap();

    let mut invalidated = InvalidationSet::new();
    let mut scratch = TraversalScratch::new();
    let mut trace = OneParentRecorder::new();

    EagerPolicy.propagate_with_trace(
        1,
        LAYOUT,
        &graph,
        &mut invalidated,
        &mut scratch,
        &mut trace,
    );

    let path = trace.explain_path(3, LAYOUT).unwrap();
    assert_eq!(
        path,
        vec![1, 2, 3],
        "the recorded cause path should lead from the root to the queried key"
    );

    println!("why is 3 invalidated? {path:?}");
}

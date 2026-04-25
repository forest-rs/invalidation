// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Model retained UI-style phase invalidation on node IDs.

use invalidation::{Channel, InvalidationTracker, LazyPolicy};

const SELECTOR_INPUTS: Channel = Channel::new(0);
const STYLE: Channel = Channel::new(1);
const MEASURE: Channel = Channel::new(2);
const ARRANGE: Channel = Channel::new(3);
const SURFACE: Channel = Channel::new(4);
const PAINT: Channel = Channel::new(5);
const SEMANTICS: Channel = Channel::new(6);

const BUTTON: u32 = 1;
const LABEL: u32 = 2;
const TOOLTIP: u32 = 3;

fn main() {
    let mut tracker = InvalidationTracker::<u32>::with_cascades([
        (SELECTOR_INPUTS, STYLE),
        (STYLE, MEASURE),
        (STYLE, PAINT),
        (STYLE, SEMANTICS),
        (MEASURE, ARRANGE),
        (ARRANGE, SURFACE),
        (ARRANGE, PAINT),
    ])
    .unwrap();

    // Same-channel graph dependency: arranging the button affects its label.
    tracker.add_dependency(LABEL, BUTTON, ARRANGE).unwrap();

    // Sparse derived relationship: the button anchors a tooltip surface.
    tracker.add_cross_dependency(BUTTON, ARRANGE, TOOLTIP, SURFACE);

    // A pseudo-class or selector input changed on the button. Lazy marking
    // records phase roots now; affected drains expand same-channel dependents
    // when a phase is processed.
    tracker.mark_with(BUTTON, SELECTOR_INPUTS, &LazyPolicy);

    assert!(
        tracker.is_invalidated(BUTTON, STYLE),
        "selector input changes should cascade to style"
    );
    assert!(
        tracker.is_invalidated(BUTTON, ARRANGE),
        "style and measure changes should cascade to arrange"
    );
    assert!(
        tracker.is_invalidated(BUTTON, PAINT),
        "style and arrange changes should cascade to paint"
    );
    assert!(
        tracker.is_invalidated(BUTTON, SEMANTICS),
        "style changes should cascade to semantics"
    );
    assert!(
        tracker.is_invalidated(TOOLTIP, SURFACE),
        "button arrange should invalidate the anchored tooltip surface"
    );
    assert!(
        !tracker.is_invalidated(LABEL, ARRANGE),
        "lazy marking defers same-channel graph expansion until drain time"
    );

    let arrange_work: Vec<_> = tracker
        .drain(ARRANGE)
        .affected()
        .deterministic()
        .run()
        .collect();
    assert_eq!(
        arrange_work,
        vec![BUTTON, LABEL],
        "arrange drains expand from the changed node to same-channel dependents"
    );

    let surface_work: Vec<_> = tracker.drain(SURFACE).deterministic().run().collect();
    assert_eq!(
        surface_work,
        vec![BUTTON, TOOLTIP],
        "surface work includes the cascaded button and anchored tooltip"
    );

    println!("arrange work: {arrange_work:?}");
    println!("surface work: {surface_work:?}");
}

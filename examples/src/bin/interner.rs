// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Use `intern::Interner` to drive invalidation with non-`Copy` domain keys.

use invalidation::{Channel, InvalidationTracker, LazyPolicy, intern::Interner};

const STYLE: Channel = Channel::new(0);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResourceKey(&'static str);

fn main() {
    let mut ids = Interner::<ResourceKey>::new();
    let styles = ids.intern(ResourceKey("styles.css"));
    let button = ids.intern(ResourceKey("button"));
    let card = ids.intern(ResourceKey("card"));

    let mut tracker = InvalidationTracker::new();
    tracker.add_dependency(button, styles, STYLE).unwrap();
    tracker.add_dependency(card, styles, STYLE).unwrap();

    tracker.mark_with(styles, STYLE, &LazyPolicy);
    let affected: Vec<_> = tracker
        .drain(STYLE)
        .affected()
        .deterministic()
        .run()
        .collect();

    assert_eq!(
        affected,
        vec![styles, button, card],
        "deterministic affected drain should return the stylesheet before its dependents"
    );

    let labels: Vec<_> = affected
        .into_iter()
        .map(|id| ids.get(id).unwrap().0)
        .collect();

    println!("affected resources: {labels:?}");
}

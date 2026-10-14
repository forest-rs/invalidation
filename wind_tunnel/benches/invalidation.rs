// Copyright 2026 the Invalidation Authors
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! Criterion benchmarks for invalidation propagation and drain behavior.

use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use invalidation::{
    Channel, CycleHandling, EagerPolicy, InvalidationGraph, InvalidationSet, InvalidationTracker,
    LazyPolicy, TraversalScratch, drain_sorted,
};

const LAYOUT: Channel = Channel::new(0);
const PAINT: Channel = Channel::new(1);

#[derive(Clone)]
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u32(&mut self) -> u32 {
        // Numerical Recipes LCG parameters.
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
        u32::try_from(self.0 >> 32).expect("upper 32 bits fit in u32")
    }

    fn gen_range_usize(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive == 0 {
            return 0;
        }
        (self.next_u32() as usize) % upper_exclusive
    }
}

fn build_dag_graph(n: u32, edges_per_node: u32, seed: u64) -> InvalidationGraph<u32> {
    let mut graph = InvalidationGraph::new();
    let mut rng = Lcg::new(seed);

    // Ensure a DAG by only adding edges `from -> to` where `to < from`.
    for from in 0..n {
        if from == 0 {
            continue;
        }
        let out = edges_per_node.min(from);
        for _ in 0..out {
            let to = u32::try_from(rng.gen_range_usize(from as usize))
                .expect("selected dependency index fits in u32");
            let _ = graph
                .add_dependency(from, to, LAYOUT, CycleHandling::Allow)
                .expect("CycleHandling::Allow never errors");
        }
    }

    graph
}

fn build_dag_tracker(n: u32, edges_per_node: u32, seed: u64) -> InvalidationTracker<u32> {
    let graph = build_dag_graph(n, edges_per_node, seed);
    InvalidationTracker::from_graph(graph)
}

/// A DAG with the same edges in layout and paint, and layout cascading to
/// paint: the common "relayout means repaint" setup.
fn build_cascade_tracker(n: u32, edges_per_node: u32, seed: u64) -> InvalidationTracker<u32> {
    let layout = build_dag_graph(n, edges_per_node, seed);
    let mut tracker = InvalidationTracker::from_graph(layout.clone());
    for (from, to) in layout.edges(LAYOUT) {
        let _ = tracker
            .add_dependency_with(from, to, PAINT, CycleHandling::Allow)
            .expect("CycleHandling::Allow never errors");
    }
    tracker
        .add_cascade(LAYOUT, PAINT)
        .expect("one cascade cannot form a cycle");
    tracker
}

/// `n` layout keys hanging off key 0, each with a cross-channel edge into a
/// different point of one `n`-key paint chain. With `reversed`, the edges
/// reach deeper points of the chain first.
fn build_reconvergent_tracker(n: u32, reversed: bool) -> InvalidationTracker<u32> {
    let mut tracker = InvalidationTracker::new();
    for key in 1..=n {
        let _ = tracker
            .add_dependency(key, 0, LAYOUT)
            .expect("a fan-out has no cycles");
    }
    for i in 1..n {
        let _ = tracker
            .add_dependency(n + 1 + i, n + i, PAINT)
            .expect("a chain has no cycles");
    }
    for key in 1..=n {
        let target = if reversed { 2 * n + 1 - key } else { n + key };
        tracker.add_cross_dependency(key, LAYOUT, target, PAINT);
    }
    tracker
}

fn bench_cross_channel(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_channel");
    group.sample_size(10);

    for &(n, edges_per_node) in &[(256_u32, 4_u32), (4_096_u32, 4_u32)] {
        group.bench_function(
            format!("eager_mark_cascade(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || build_cascade_tracker(n, edges_per_node, 0xD1A7_0000_0000_0010),
                    |mut tracker| {
                        black_box(tracker.mark_with(0, LAYOUT, &EagerPolicy));
                        black_box(tracker);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("lazy_mark_cascade(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || build_cascade_tracker(n, edges_per_node, 0xD1A7_0000_0000_0011),
                    |mut tracker| {
                        black_box(tracker.mark_with(0, LAYOUT, &LazyPolicy));
                        black_box(tracker);
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    for &n in &[256_u32, 4_096_u32] {
        group.bench_function(format!("lazy_mark_every_key_cascade(n={n},e=4)"), |b| {
            b.iter_batched(
                || build_cascade_tracker(n, 4, 0xD1A7_0000_0000_0012),
                |mut tracker| {
                    for key in 0..n {
                        black_box(tracker.mark_with(key, LAYOUT, &LazyPolicy));
                    }
                    black_box(tracker);
                },
                BatchSize::LargeInput,
            );
        });
    }

    for &n in &[256_u32, 4_096_u32] {
        for reversed in [false, true] {
            group.bench_function(
                format!("eager_mark_reconvergent(n={n},reversed={reversed})"),
                |b| {
                    b.iter_batched(
                        || build_reconvergent_tracker(n, reversed),
                        |mut tracker| {
                            black_box(tracker.mark_with(0, LAYOUT, &EagerPolicy));
                            black_box(tracker);
                        },
                        BatchSize::LargeInput,
                    );
                },
            );
        }
    }

    group.finish();
}

fn roots_repeating(unique_roots: u32, marks: u32) -> impl Iterator<Item = u32> {
    (0..marks).map(move |i| i % unique_roots)
}

fn bench_invalidation(c: &mut Criterion) {
    let mut group = c.benchmark_group("invalidation");
    group.sample_size(50);

    for &(n, edges_per_node) in &[
        (256_u32, 1_u32),
        (256_u32, 4_u32),
        (4_096_u32, 1_u32),
        (4_096_u32, 4_u32),
    ] {
        group.bench_function(format!("eager_mark(n={n},e={edges_per_node})"), |b| {
            b.iter_batched(
                || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0001),
                |mut tracker| {
                    tracker.mark_with(0, LAYOUT, &EagerPolicy);
                    black_box(tracker);
                },
                BatchSize::LargeInput,
            );
        });

        group.bench_function(
            format!("eager_mark_with_scratch(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || build_dag_graph(n, edges_per_node, 0xD1A7_0000_0000_0005),
                    |graph| {
                        let mut invalidated = InvalidationSet::new();
                        let mut scratch = TraversalScratch::with_capacity(n as usize / 2);
                        EagerPolicy.propagate_with_scratch(
                            0,
                            LAYOUT,
                            &graph,
                            &mut invalidated,
                            &mut scratch,
                        );
                        black_box(invalidated);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("eager_mark_and_drain(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0002),
                    |mut tracker| {
                        tracker.mark_with(0, LAYOUT, &EagerPolicy);
                        let sum: u64 = tracker
                            .drain_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("lazy_mark_and_drain_affected(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0003),
                    |mut tracker| {
                        tracker.mark_with(0, LAYOUT, &LazyPolicy);
                        let sum: u64 = tracker
                            .drain_affected_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("drain_sorted_all_invalidated(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || {
                        let mut tracker =
                            build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0004);
                        for k in 0..n {
                            tracker.mark(k, LAYOUT);
                        }
                        tracker
                    },
                    |mut tracker| {
                        let sum: u64 = tracker
                            .drain_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("drain_sorted_deterministic_all_invalidated(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || {
                        let mut tracker =
                            build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0004);
                        for k in 0..n {
                            tracker.mark(k, LAYOUT);
                        }
                        tracker
                    },
                    |mut tracker| {
                        let sum: u64 = tracker
                            .drain_sorted_deterministic(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!("peek_sorted_all_invalidated_sum(n={n},e={edges_per_node})"),
            |b| {
                b.iter_batched(
                    || {
                        let mut tracker =
                            build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0007);
                        for k in 0..n {
                            tracker.mark(k, LAYOUT);
                        }
                        tracker
                    },
                    |tracker| {
                        let sum: u64 = tracker
                            .peek_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    for &(n, edges_per_node, marks, unique_roots) in &[
        (4_096_u32, 4_u32, 1_024_u32, 1_u32),
        (4_096_u32, 4_u32, 1_024_u32, 8_u32),
    ] {
        group.bench_function(
            format!(
                "redundant_marks_then_drain_eager(n={n},e={edges_per_node},marks={marks},roots={unique_roots})"
            ),
            |b| {
                b.iter_batched(
                    || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0010),
                    |mut tracker| {
                        for root in roots_repeating(unique_roots, marks) {
                            tracker.mark_with(root, LAYOUT, &EagerPolicy);
                        }
                        let sum: u64 = tracker
                            .drain_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!(
                "redundant_marks_then_drain_eager_many(n={n},e={edges_per_node},marks={marks},roots={unique_roots})"
            ),
            |b| {
                b.iter_batched(
                    || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0010),
                    |mut tracker| {
                        let roots = roots_repeating(unique_roots, marks).map(|root| (root, LAYOUT));
                        black_box(tracker.mark_many_with(roots, &EagerPolicy));
                        let sum: u64 = tracker
                            .drain_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!(
                "redundant_marks_then_drain_eager_with_scratch(n={n},e={edges_per_node},marks={marks},roots={unique_roots})"
            ),
            |b| {
                b.iter_batched(
                    || build_dag_graph(n, edges_per_node, 0xD1A7_0000_0000_0030),
                    |graph| {
                        let mut invalidated = InvalidationSet::new();
                        let mut scratch = TraversalScratch::with_capacity(n as usize / 2);
                        for root in roots_repeating(unique_roots, marks) {
                            EagerPolicy.propagate_with_scratch(
                                root,
                                LAYOUT,
                                &graph,
                                &mut invalidated,
                                &mut scratch,
                            );
                        }
                        let sum: u64 = drain_sorted(&mut invalidated, &graph, LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );

        group.bench_function(
            format!(
                "redundant_marks_then_drain_lazy(n={n},e={edges_per_node},marks={marks},roots={unique_roots})"
            ),
            |b| {
                b.iter_batched(
                    || build_dag_tracker(n, edges_per_node, 0xD1A7_0000_0000_0020),
                    |mut tracker| {
                        for root in roots_repeating(unique_roots, marks) {
                            tracker.mark_with(root, LAYOUT, &LazyPolicy);
                        }
                        let sum: u64 = tracker
                            .drain_affected_sorted(LAYOUT)
                            .fold(0_u64, |acc, k| acc + u64::from(k));
                        black_box(sum);
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_invalidation, bench_cross_channel);
criterion_main!(benches);

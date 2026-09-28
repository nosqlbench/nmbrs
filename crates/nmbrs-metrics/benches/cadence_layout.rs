// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Cadence reporter end-to-end overhead benches.
//!
//! Drives `CadenceReporter::ingest` against a realistic `1s,10s,1m,5m,15m`
//! cadence layout, varying:
//!
//! - component count (how many distinct `Labels` → separate per-
//!   component window state),
//! - instrument mix (counter-only / histogram-only / mixed),
//! - subscriber fan-out at the 10s cadence.
//!
//! Each bench iteration simulates 300 smallest-cadence (1s) ticks,
//! i.e. 5 minutes of wall-clock time, which exercises promotions
//! through the 1s → 10s → 1m → 5m path (15m never promotes). Time
//! advance is driven by `MetricSet.interval()`, so benches run
//! without real sleeps.
//!
//! Run:
//! ```text
//! cargo bench -p nmbrs-metrics --bench cadence_layout
//! cargo bench -p nmbrs-metrics --bench cadence_layout -- cascade/baseline
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use hdrhistogram::Histogram as HdrHistogram;

use nmbrs_metrics::cadence::{CadenceTree, Cadences, DEFAULT_MAX_FAN_IN};
use nmbrs_metrics::cadence_reporter::{CadenceReporter, SubscriptionOpts};
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;

const TICKS_PER_ITER: usize = 300; // simulates 5 minutes of 1s ticks
const HIST_SAMPLES_PER_TICK: usize = 1000; // per-tick histogram fill (ops/s)

/// The canonical layout under test.
fn build_reporter() -> Arc<CadenceReporter> {
    let cadences = Cadences::parse("1s,10s,1m,5m,15m").unwrap();
    let tree = CadenceTree::plan_validated(cadences, DEFAULT_MAX_FAN_IN, Duration::from_secs(1))
        .expect("valid plan");
    Arc::new(CadenceReporter::new(tree))
}

/// Pre-build a set of Labels for N components. Keyed by phase id
/// so each gets a separate `(component_path, layer)` store slot.
fn component_labels(count: usize) -> Vec<Labels> {
    (0..count)
        .map(|i| Labels::of("phase", format!("c{i}")))
        .collect()
}

/// Fast MetricSet builder for a counter-only tick.
fn counter_tick(i: u64) -> MetricSet {
    let mut s = MetricSet::new(Duration::from_secs(1));
    s.insert_counter("ops", Labels::default(), i, Instant::now());
    s
}

/// MetricSet with one histogram filled with N samples.
fn histogram_tick(i: u64) -> MetricSet {
    let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    for j in 0..HIST_SAMPLES_PER_TICK {
        h.record(((i + j as u64) * 1_009).wrapping_add(1)).unwrap();
    }
    let mut s = MetricSet::new(Duration::from_secs(1));
    s.insert_histogram("rt", Labels::default(), h, Instant::now());
    s
}

/// MetricSet with a mix: counter + gauge + histogram (like a real
/// ActivityMetrics capture_delta).
fn mixed_tick(i: u64) -> MetricSet {
    let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    for j in 0..HIST_SAMPLES_PER_TICK {
        h.record(((i + j as u64) * 1_009).wrapping_add(1)).unwrap();
    }
    let mut s = MetricSet::new(Duration::from_secs(1));
    s.insert_counter("ops", Labels::default(), i, Instant::now());
    s.insert_gauge("depth", Labels::default(), i as f64, Instant::now());
    s.insert_histogram("rt", Labels::default(), h, Instant::now());
    s
}

// =========================================================================
// cascade/baseline — 1 component, counter-only
// =========================================================================

fn bench_cascade_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("cascade/baseline");
    group.throughput(Throughput::Elements(TICKS_PER_ITER as u64));
    group.bench_function("1c_counter", |b| {
        b.iter_batched(
            build_reporter,
            |reporter| {
                let labels = Labels::of("phase", "bench");
                for i in 0..TICKS_PER_ITER as u64 {
                    reporter.ingest(&labels, counter_tick(i));
                }
                black_box(reporter.component_labels().len());
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

// =========================================================================
// cascade/components/{1,10,100} — counter-only, vary component count
// =========================================================================

fn bench_cascade_components(c: &mut Criterion) {
    let mut group = c.benchmark_group("cascade/components");
    for &n in &[1usize, 10, 100] {
        let total_ingests = TICKS_PER_ITER * n;
        group.throughput(Throughput::Elements(total_ingests as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let labels_vec = component_labels(n);
            b.iter_batched(
                build_reporter,
                |reporter| {
                    for i in 0..TICKS_PER_ITER as u64 {
                        for labels in &labels_vec {
                            reporter.ingest(labels, counter_tick(i));
                        }
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// =========================================================================
// cascade/instruments/{counter_only, histogram_only, mixed}
// Fixed 10 components, vary the per-tick MetricSet shape.
// =========================================================================

fn bench_cascade_instruments(c: &mut Criterion) {
    let mut group = c.benchmark_group("cascade/instruments");
    let n_components = 10;
    let total = TICKS_PER_ITER * n_components;
    group.throughput(Throughput::Elements(total as u64));

    for (name, tick_fn) in &[
        ("counter_only", counter_tick as fn(u64) -> MetricSet),
        ("histogram_only", histogram_tick as fn(u64) -> MetricSet),
        ("mixed", mixed_tick as fn(u64) -> MetricSet),
    ] {
        let tick_fn = *tick_fn;
        group.bench_function(*name, |b| {
            let labels_vec = component_labels(n_components);
            b.iter_batched(
                build_reporter,
                |reporter| {
                    for i in 0..TICKS_PER_ITER as u64 {
                        for labels in &labels_vec {
                            reporter.ingest(labels, tick_fn(i));
                        }
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// =========================================================================
// cascade/subscribers/{0,1,4} — vary fan-out at the 10s cadence.
// =========================================================================

struct NoopReporter {
    count: Arc<AtomicU64>,
}
impl Reporter for NoopReporter {
    fn report(&mut self, _snapshot: &MetricSet) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

fn bench_cascade_subscribers(c: &mut Criterion) {
    let mut group = c.benchmark_group("cascade/subscribers");
    let n_components = 10;
    let total = TICKS_PER_ITER * n_components;
    group.throughput(Throughput::Elements(total as u64));

    for &subscribers in &[0usize, 1, 4] {
        group.bench_with_input(
            BenchmarkId::from_parameter(subscribers),
            &subscribers,
            |b, &subscribers| {
                let labels_vec = component_labels(n_components);
                b.iter_batched(
                    || {
                        let reporter = build_reporter();
                        for _ in 0..subscribers {
                            let count = Arc::new(AtomicU64::new(0));
                            let _ = reporter.subscribe(
                                Duration::from_secs(10),
                                Box::new(NoopReporter { count }),
                                SubscriptionOpts::default(),
                            );
                        }
                        reporter
                    },
                    |reporter| {
                        for i in 0..TICKS_PER_ITER as u64 {
                            for labels in &labels_vec {
                                reporter.ingest(labels, mixed_tick(i));
                            }
                        }
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// =========================================================================
// coalesce/direct — isolate MetricSet::coalesce from ingest overhead
// =========================================================================

fn bench_coalesce_direct(c: &mut Criterion) {
    let mut group = c.benchmark_group("coalesce/direct");
    group.throughput(Throughput::Elements(1));

    // Counter-only two-set coalesce.
    group.bench_function("two_counters", |b| {
        let a = counter_tick(1);
        let c = counter_tick(2);
        b.iter(|| {
            let merged = MetricSet::coalesce(
                std::slice::from_ref(&a)
                    .iter()
                    .chain(std::slice::from_ref(&c).iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            black_box(merged);
        });
    });

    // Histogram coalesce: single-sample reservoirs (cheapest HDR
    // add path).
    group.bench_function("two_histograms_small", |b| {
        let build = || {
            let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            h.record(1_000_000).unwrap();
            let mut s = MetricSet::new(Duration::from_secs(1));
            s.insert_histogram("rt", Labels::default(), h, Instant::now());
            s
        };
        let a = build();
        let c = build();
        b.iter(|| {
            let merged = MetricSet::coalesce(
                std::slice::from_ref(&a)
                    .iter()
                    .chain(std::slice::from_ref(&c).iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            black_box(merged);
        });
    });

    // Histogram coalesce with a fully-populated reservoir (1000
    // distinct values so buckets actually fill).
    group.bench_function("two_histograms_1000s", |b| {
        let build = || {
            let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            for i in 0..1000u64 {
                h.record(i * 1_009 + 1).unwrap();
            }
            let mut s = MetricSet::new(Duration::from_secs(1));
            s.insert_histogram("rt", Labels::default(), h, Instant::now());
            s
        };
        let a = build();
        let c = build();
        b.iter(|| {
            let merged = MetricSet::coalesce(
                std::slice::from_ref(&a)
                    .iter()
                    .chain(std::slice::from_ref(&c).iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .as_slice(),
            );
            black_box(merged);
        });
    });
    group.finish();
}

// =========================================================================
// cascade/ingest_only — ingest with PRE-BUILT MetricSets, to isolate
// coalesce+cascade overhead from the per-tick MetricSet construction
// (which dominates the earlier histogram bench).
// =========================================================================

fn bench_cascade_ingest_only(c: &mut Criterion) {
    let mut group = c.benchmark_group("cascade/ingest_only");
    let n_metrics = 1000;
    group.throughput(Throughput::Elements(n_metrics as u64));

    // Counter-only: 1000 distinct metrics ingested in one simulated
    // 1s tick — pre-built snapshots, no construction cost in the
    // timed path.
    group.bench_function("1000_counters_per_tick", |b| {
        let labels_vec = component_labels(n_metrics);
        let snap = counter_tick(1);
        b.iter_batched(
            build_reporter,
            |reporter| {
                for l in &labels_vec {
                    reporter.ingest(l, snap.clone());
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Histogram-only: 1000 distinct metrics, each ingest carries a
    // small pre-built reservoir. Clone cost of the Arc<HdrHistogram>
    // is small; the actual coalesce happens on 2nd+ tick for each.
    group.bench_function("1000_histograms_small_per_tick", |b| {
        let labels_vec = component_labels(n_metrics);
        let snap = {
            let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            h.record(1_000_000).unwrap();
            let mut s = MetricSet::new(Duration::from_secs(1));
            s.insert_histogram("rt", Labels::default(), h, Instant::now());
            s
        };
        b.iter_batched(
            build_reporter,
            |reporter| {
                for l in &labels_vec {
                    reporter.ingest(l, snap.clone());
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Histogram with 1000 samples: realistic Timer histogram for a
    // 1kOps/s workload.
    group.bench_function("1000_histograms_1000samples_per_tick", |b| {
        let labels_vec = component_labels(n_metrics);
        let snap = {
            let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            for i in 0..1000u64 {
                h.record(i * 1_009 + 1).unwrap();
            }
            let mut s = MetricSet::new(Duration::from_secs(1));
            s.insert_histogram("rt", Labels::default(), h, Instant::now());
            s
        };
        b.iter_batched(
            build_reporter,
            |reporter| {
                for l in &labels_vec {
                    reporter.ingest(l, snap.clone());
                }
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_cascade_baseline,
    bench_cascade_components,
    bench_cascade_instruments,
    bench_cascade_subscribers,
    bench_coalesce_direct,
    bench_cascade_ingest_only,
);
criterion_main!(benches);

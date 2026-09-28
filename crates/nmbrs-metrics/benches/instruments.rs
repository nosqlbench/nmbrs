// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-instrument throughput benches over N concurrent writers.
//!
//! Each instrument (Counter, Gauge, Histogram, Timer) is exercised
//! by a fixed number of worker threads contending on a single shared
//! instance for a fixed iteration count. Criterion reports
//! per-iteration latency (and throughput in elements/sec) so results
//! can be compared across instrument types and contention levels.
//!
//! Concurrency sweeps at 1, 4, 16, 64 threads. Run subsets with the
//! standard Criterion filter, e.g.:
//!
//! ```text
//! cargo bench -p nmbrs-metrics --bench instruments -- counter
//! cargo bench -p nmbrs-metrics --bench instruments -- 'timer/16'
//! ```

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};

use nmbrs_metrics::instruments::counter::Counter;
use nmbrs_metrics::instruments::gauge::ValueGauge;
use nmbrs_metrics::instruments::histogram::Histogram;
use nmbrs_metrics::instruments::timer::Timer;
use nmbrs_metrics::labels::Labels;

const CONCURRENCY_LEVELS: &[usize] = &[1, 4, 16, 64];
/// Total ops per bench iteration — split across worker threads.
/// Kept modest so the benchmark wall-clock stays reasonable; the
/// throughput/iter number is what matters, not the absolute count.
const OPS_PER_ITER: usize = 10_000;

/// Run `work` on `threads` worker threads, each invoking it
/// `ops_per_thread` times. Uses a start barrier (via `AtomicBool`)
/// so the threads race into the critical section together —
/// measures contention, not per-thread startup overhead.
fn run_concurrent<F>(threads: usize, ops_per_thread: usize, work: F)
where
    F: Fn(usize) + Send + Sync + 'static + Clone,
{
    let ready = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::with_capacity(threads);
    for tid in 0..threads {
        let ready = ready.clone();
        let work = work.clone();
        handles.push(thread::spawn(move || {
            while !ready.load(Ordering::Acquire) {
                thread::yield_now();
            }
            for i in 0..ops_per_thread {
                work(tid.wrapping_mul(ops_per_thread).wrapping_add(i));
            }
        }));
    }
    ready.store(true, Ordering::Release);
    for h in handles {
        let _ = h.join();
    }
}

// =========================================================================
// Counter
// =========================================================================

fn bench_counter(c: &mut Criterion) {
    let mut group = c.benchmark_group("counter");
    group.throughput(Throughput::Elements(OPS_PER_ITER as u64));
    for &threads in CONCURRENCY_LEVELS {
        let per_thread = OPS_PER_ITER / threads;
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || Arc::new(Counter::new(Labels::of("name", "ops"))),
                    |counter| {
                        let c = counter.clone();
                        run_concurrent(threads, per_thread, move |_i| {
                            c.inc();
                            black_box(c.get());
                        });
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// =========================================================================
// Gauge
// =========================================================================

fn bench_gauge(c: &mut Criterion) {
    let mut group = c.benchmark_group("gauge");
    group.throughput(Throughput::Elements(OPS_PER_ITER as u64));
    for &threads in CONCURRENCY_LEVELS {
        let per_thread = OPS_PER_ITER / threads;
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || Arc::new(ValueGauge::new(Labels::of("name", "depth"))),
                    |gauge| {
                        let g = gauge.clone();
                        run_concurrent(threads, per_thread, move |i| {
                            g.set((i as f64) * 0.01);
                            black_box(g.get());
                        });
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// =========================================================================
// Histogram
// =========================================================================

fn bench_histogram(c: &mut Criterion) {
    let mut group = c.benchmark_group("histogram");
    group.throughput(Throughput::Elements(OPS_PER_ITER as u64));
    for &threads in CONCURRENCY_LEVELS {
        let per_thread = OPS_PER_ITER / threads;
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || Arc::new(Histogram::new(Labels::of("name", "rt"))),
                    |h| {
                        let hist = h.clone();
                        run_concurrent(threads, per_thread, move |i| {
                            // Spread values across the HDR range so
                            // the reservoir gets multi-bucket usage
                            // rather than hammering one slot.
                            hist.record((i as u64 * 997).wrapping_add(1));
                        });
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// =========================================================================
// Timer — start + record_since pattern
// =========================================================================

fn bench_timer(c: &mut Criterion) {
    let mut group = c.benchmark_group("timer");
    group.throughput(Throughput::Elements(OPS_PER_ITER as u64));
    for &threads in CONCURRENCY_LEVELS {
        let per_thread = OPS_PER_ITER / threads;
        group.bench_with_input(
            BenchmarkId::from_parameter(threads),
            &threads,
            |b, &threads| {
                b.iter_batched(
                    || Arc::new(Timer::new(Labels::of("name", "servicetime"))),
                    |t| {
                        let timer = t.clone();
                        run_concurrent(threads, per_thread, move |i| {
                            // Record a fabricated elapsed value so
                            // we measure the instrument's write path
                            // without dragging Instant::now() and a
                            // real sleep into every iteration.
                            timer.record((i as u64 * 1_009).wrapping_add(1));
                        });
                    },
                    criterion::BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

// =========================================================================
// Histogram peek vs snapshot — non-destructive read cost
// =========================================================================

fn bench_histogram_read_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("histogram_read");
    let h = Histogram::new(Labels::of("name", "read"));
    for i in 1..=10_000u64 {
        h.record(i * 97);
    }
    group.bench_function("peek_snapshot", |b| {
        b.iter(|| {
            black_box(h.peek_snapshot());
        });
    });
    group.bench_function("snapshot_drain", |b| {
        // Refill after drain so each iteration measures the same
        // sized reservoir. Subtracts the refill cost from the
        // measured snapshot timing by using iter_batched.
        b.iter_batched(
            || {
                let hf = Histogram::new(Labels::of("name", "read"));
                for i in 1..=10_000u64 {
                    hf.record(i * 97);
                }
                hf
            },
            |hf| {
                black_box(hf.snapshot());
            },
            criterion::BatchSize::SmallInput,
        );
    });
    group.finish();
}

// =========================================================================
// Live-window ring — hot-path overhead + peek cost
// =========================================================================

fn bench_live_window(c: &mut Criterion) {
    use nmbrs_metrics::summaries::live_window::LiveWindowConfig;

    let mut group = c.benchmark_group("live_window");

    // Baseline: Timer::record with the live window DISABLED.
    // Measures the additional null-check cost on the hot path.
    group.bench_function("record_disabled", |b| {
        let t = Timer::new(Labels::of("name", "lw"));
        let mut i: u64 = 1;
        b.iter(|| {
            t.record(i.wrapping_mul(1_009).wrapping_add(1));
            i = i.wrapping_add(1);
            black_box(());
        });
    });

    // Same Timer::record with the live window ENABLED. Delta vs
    // `record_disabled` is the per-record live-ring overhead.
    group.bench_function("record_enabled", |b| {
        let t = Timer::new(Labels::of("name", "lw"));
        let _ring = t.enable_live_window(LiveWindowConfig::default());
        let mut i: u64 = 1;
        b.iter(|| {
            t.record(i.wrapping_mul(1_009).wrapping_add(1));
            i = i.wrapping_add(1);
            black_box(());
        });
    });

    // Peek cost after the ring has been populated with 1000 samples
    // — represents a TUI-style per-render pull.
    group.bench_function("peek_1000_samples", |b| {
        let t = Timer::new(Labels::of("name", "lw"));
        let _ring = t.enable_live_window(LiveWindowConfig::default());
        for i in 0..1000u64 {
            t.record(i * 1_009 + 1);
        }
        b.iter(|| {
            black_box(t.peek_live_window());
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_counter,
    bench_gauge,
    bench_histogram,
    bench_timer,
    bench_histogram_read_paths,
    bench_live_window,
);
criterion_main!(benches);

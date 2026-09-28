// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Comprehensive API-surface test for `nmbrs-metrics`.
//!
//! Exercises every public type across the crate end-to-end — the
//! data model (`snapshot`), cadence planning (`cadence`), the
//! single-writer cadence reporter, the single-reader `MetricsQuery`,
//! the component tree, the instruments, and the scheduler wire-up.
//! Deliberately scripted top-down as a reader's introduction to the
//! library.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hdrhistogram::Histogram as HdrHistogram;

use nmbrs_metrics::cadence::{
    CadenceTree, CadenceTreeError, Cadences, DEFAULT_MAX_FAN_IN, format_duration_short,
};
use nmbrs_metrics::cadence_reporter::{CadenceReporter, HISTORY_RING_CAP};
use nmbrs_metrics::component::{Component, ComponentState, InstrumentRef, attach, capture_tree};
use nmbrs_metrics::instruments::counter::Counter;
use nmbrs_metrics::instruments::gauge::ValueGauge;
use nmbrs_metrics::instruments::histogram::Histogram;
use nmbrs_metrics::instruments::timer::Timer;
use nmbrs_metrics::labels::Labels;
use nmbrs_metrics::metrics_query::{MetricsQuery, SelectError, Selection};
use nmbrs_metrics::scheduler::{Reporter, SchedulerBuilder};
use nmbrs_metrics::snapshot::{
    Bucket, BucketBound, CombineMode, CounterValue, Exemplar, GaugeValue, HistogramValue, Metric,
    MetricFamily, MetricPoint, MetricSet, MetricType, MetricValue, combine_hdr, combine_into,
    counter_family, gauge_family, histogram_family, split_name_label,
};

// =========================================================================
// labels
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn labels_compose_without_mutation() {
    let base = Labels::of("session", "s1");
    let child = base.with("phase", "load");
    assert_eq!(base.get("phase"), None);
    assert_eq!(child.get("phase"), Some("load"));
    assert_eq!(child.get("session"), Some("s1"));
    assert_eq!(child.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn labels_extend_child_overrides_parent() {
    let parent = Labels::of("activity", "a").with("phase", "load");
    let child = Labels::of("phase", "verify");
    let merged = parent.extend(&child);
    assert_eq!(merged.get("activity"), Some("a"));
    assert_eq!(merged.get("phase"), Some("verify"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn labels_identity_hash_is_order_sensitive_but_value_stable() {
    let a = Labels::of("k1", "v1").with("k2", "v2");
    let b = Labels::of("k1", "v1").with("k2", "v2");
    assert_eq!(a.identity_hash(), b.identity_hash());
    let c = Labels::of("k1", "v1").with("k2", "v3");
    assert_ne!(a.identity_hash(), c.identity_hash());
}

// =========================================================================
// instruments
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn counter_inc_by_accumulates() {
    let c = Counter::new(Labels::of("name", "ops"));
    c.inc();
    c.inc_by(9);
    assert_eq!(c.get(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gauge_set_replaces_value() {
    let g = ValueGauge::new(Labels::of("name", "depth"));
    g.set(1.5);
    g.set(3.25);
    assert_eq!(g.get(), 3.25);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn histogram_record_and_peek_snapshot_preserve_samples() {
    let h = Histogram::new(Labels::of("name", "rt"));
    for v in [1_000, 2_000, 3_000] {
        h.record(v);
    }
    let peek = h.peek_snapshot();
    assert_eq!(peek.len(), 3);
    // Peek is non-draining.
    let peek2 = h.peek_snapshot();
    assert_eq!(peek2.len(), 3);
    // snapshot() drains.
    let drained = h.snapshot();
    assert_eq!(drained.len(), 3);
    let after = h.peek_snapshot();
    assert_eq!(after.len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timer_record_snapshot_has_count_and_histogram() {
    let t = Timer::new(Labels::of("name", "servicetime"));
    t.record(10_000_000);
    t.record(50_000_000);
    let snap = t.snapshot();
    assert_eq!(snap.count, 2);
    assert!(snap.histogram.max() >= 50_000_000);
}

// =========================================================================
// snapshot — data model
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metric_set_tracks_capture_metadata() {
    let interval = Duration::from_secs(5);
    let s = MetricSet::new(interval);
    assert_eq!(s.interval(), interval);
    assert!(s.captured_at().elapsed() < Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insert_counter_gauge_histogram_build_families_by_name() {
    let now = Instant::now();
    let mut s = MetricSet::at(now, Duration::from_secs(1));
    s.insert_counter("ops", Labels::of("phase", "load"), 17, now);
    s.insert_gauge("queue_depth", Labels::default(), 4.0, now);
    let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    h.record(5_000).unwrap();
    s.insert_histogram("latency", Labels::default(), h, now);

    assert_eq!(s.len(), 3);
    assert_eq!(s.family("ops").unwrap().r#type(), MetricType::Counter);
    assert_eq!(s.family("queue_depth").unwrap().r#type(), MetricType::Gauge);
    assert_eq!(s.family("latency").unwrap().r#type(), MetricType::Histogram);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metric_point_carries_timestamp_or_not() {
    let now = Instant::now();
    let timed = MetricPoint::new(MetricValue::Gauge(GaugeValue::new(1.0)), now);
    assert_eq!(timed.timestamp(), Some(now));
    let untimed = MetricPoint::untimed(MetricValue::Gauge(GaugeValue::new(1.0)));
    assert_eq!(untimed.timestamp(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn histogram_value_projects_to_open_metrics_buckets_with_inf_terminal() {
    let mut h = HdrHistogram::<u64>::new_with_bounds(1, 1_000_000, 3).unwrap();
    for v in [50_u64, 500, 5_000, 50_000] {
        h.record(v).unwrap();
    }
    let hv = HistogramValue::from_hdr(h);
    let buckets: Vec<Bucket> = hv.project_buckets(&[100, 1_000, 10_000]);
    assert_eq!(buckets.len(), 4);
    assert_eq!(buckets[3].upper_bound, BucketBound::PositiveInfinity);
    assert_eq!(buckets[3].cumulative_count, hv.count);
    // Monotonic.
    for w in buckets.windows(2) {
        assert!(w[0].cumulative_count <= w[1].cumulative_count);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exemplar_attaches_to_counter_value() {
    let trace = Exemplar::new(Labels::of("trace_id", "abc"), 1.0);
    let cv = CounterValue::new(5).with_exemplar(trace);
    assert!(cv.exemplar.as_ref().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combine_counter_aggregate_sums_keeps_earlier_created() {
    let t1 = Instant::now();
    let t0 = t1 - Duration::from_secs(60);
    let mut a = MetricPoint::new(
        MetricValue::Counter(CounterValue::new(10).with_created(t1)),
        t1,
    );
    let b = MetricPoint::new(
        MetricValue::Counter(CounterValue::new(25).with_created(t0)),
        t1,
    );
    combine_into(&mut a, &b, CombineMode::Aggregate).unwrap();
    match a.value() {
        MetricValue::Counter(c) => {
            assert_eq!(
                c.cumulative, 35,
                "aggregate sums cumulative across components"
            );
            assert_eq!(c.created, Some(t0));
        }
        _ => panic!("wrong type"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combine_histogram_merges_reservoirs() {
    let mut h1 = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    h1.record(1_000_000).unwrap();
    let mut h2 = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    h2.record(2_000_000).unwrap();
    let merged = combine_hdr(&h1, &h2).unwrap();
    assert_eq!(merged.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metric_set_coalesce_keeps_latest_counter_sum_intervals() {
    // Time-coalesce keeps the latest (window-end) cumulative — never a
    // sum — and sums the intervals.
    let mut a = MetricSet::new(Duration::from_secs(1));
    a.insert_counter("ops", Labels::default(), 5, Instant::now());
    let mut b = MetricSet::new(Duration::from_secs(2));
    b.insert_counter("ops", Labels::default(), 7, Instant::now());
    let merged = MetricSet::coalesce(&[a, b]);
    assert_eq!(merged.interval(), Duration::from_secs(3));
    let cumulative = match merged
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!("wrong type"),
    };
    assert_eq!(cumulative, 7, "latest cumulative, not the sum");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn split_name_label_extracts_family_name() {
    let labels = Labels::of("name", "cycles_total").with("activity", "write");
    let (name, rest) = split_name_label(&labels);
    assert_eq!(name, "cycles_total");
    assert_eq!(rest.get("name"), None);
    assert_eq!(rest.get("activity"), Some("write"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn convenience_constructors_build_single_point_families() {
    let ts = Instant::now();
    let cf = counter_family("ops", Labels::default(), 1, ts);
    assert_eq!(cf.r#type(), MetricType::Counter);
    assert_eq!(cf.len(), 1);
    let gf = gauge_family("depth", Labels::default(), 2.0, ts);
    assert_eq!(gf.r#type(), MetricType::Gauge);
    let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
    h.record(100).unwrap();
    let hf = histogram_family("rt", Labels::default(), h, ts);
    assert_eq!(hf.r#type(), MetricType::Histogram);
    let _meta_test: &MetricFamily = &cf;
    let _metric_test: &Metric = cf.metrics().next().unwrap();
}

// =========================================================================
// cadence — Cadences + CadenceTree
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadences_parse_and_order_are_preserved() {
    let c = Cadences::parse("1m,10s,5m").unwrap();
    let got: Vec<Duration> = c.iter().collect();
    assert_eq!(
        got,
        vec![
            Duration::from_secs(60),
            Duration::from_secs(10),
            Duration::from_secs(300),
        ]
    );
    assert_eq!(c.smallest(), Duration::from_secs(10));
    assert_eq!(c.largest(), Duration::from_secs(300));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_tree_plan_validated_enforces_base_interval() {
    let c = Cadences::new(&[Duration::from_millis(500), Duration::from_secs(1)]).unwrap();
    let err =
        CadenceTree::plan_validated(c, DEFAULT_MAX_FAN_IN, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err, CadenceTreeError::BelowBase { .. }));

    let c2 = Cadences::new(&[Duration::from_millis(1500)]).unwrap();
    let err2 =
        CadenceTree::plan_validated(c2, DEFAULT_MAX_FAN_IN, Duration::from_secs(1)).unwrap_err();
    assert!(matches!(err2, CadenceTreeError::NotMultiple { .. }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_tree_inserts_hidden_intermediates_for_large_ratios() {
    let c = Cadences::parse("10s,10h").unwrap();
    let tree = CadenceTree::plan(c, DEFAULT_MAX_FAN_IN);
    assert!(tree.hidden().count() >= 1);
    let declared: Vec<Duration> = tree.declared().iter().collect();
    let all: Vec<Duration> = tree.layers().iter().map(|l| l.interval).collect();
    for d in declared {
        assert!(all.contains(&d));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn format_duration_short_reads_as_expected() {
    assert_eq!(format_duration_short(Duration::from_secs(10)), "10s");
    assert_eq!(format_duration_short(Duration::from_secs(90)), "1m30s");
    assert_eq!(format_duration_short(Duration::from_secs(3600)), "1h");
}

// =========================================================================
// cadence reporter — streaming cascade
// =========================================================================

fn ts_counter_set(interval: Duration, v: u64) -> MetricSet {
    let mut s = MetricSet::new(interval);
    s.insert_counter("ops", Labels::default(), v, Instant::now());
    s
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_reporter_promotes_on_interval_boundary() {
    let cadences =
        Cadences::new(&[Duration::from_millis(100), Duration::from_millis(400)]).unwrap();
    let tree = CadenceTree::plan_default(cadences);
    let reporter = CadenceReporter::new(tree);
    let labels = Labels::of("phase", "load");

    // A counter climbing 5→20 over four 100ms windows; the promoted
    // 400ms window holds the latest cumulative (coalesce keeps latest).
    for v in [5, 10, 15, 20] {
        reporter.ingest(&labels, ts_counter_set(Duration::from_millis(100), v));
    }
    // Ingest is fire-and-forget (SRD-02 §"No Blocking Primitives
    // in Async Contexts"); the actor publishes the snapshot
    // asynchronously. Tests synchronize via `flush_for_tests`
    // to observe in-flight effects deterministically.
    reporter.flush_for_tests();
    let latest_400 = reporter
        .latest(&labels, Duration::from_millis(400))
        .unwrap();
    let cumulative = match latest_400
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!(),
    };
    assert_eq!(
        cumulative, 20,
        "promoted window holds the latest cumulative"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_reporter_force_close_publishes_trailing_partial() {
    let cadences = Cadences::new(&[Duration::from_millis(1000)]).unwrap();
    let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
    let labels = Labels::of("phase", "tail");
    reporter.ingest(&labels, ts_counter_set(Duration::from_millis(200), 3));
    assert!(
        reporter
            .latest(&labels, Duration::from_millis(1000))
            .is_none()
    );

    reporter.shutdown_flush().await;
    let partial = reporter
        .latest(&labels, Duration::from_millis(1000))
        .unwrap();
    assert!(partial.interval() < Duration::from_millis(1000));
    let total = match partial
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!(),
    };
    assert_eq!(total, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cadence_reporter_ring_retains_recent_by_time() {
    // SRD-90 §M1: the smallest-cadence ring is TIME-bounded, not slot-capped.
    // HISTORY_RING_CAP+5 windows all fall within the counter horizon, so every
    // one is retained (the old fixed slot cap would have dropped the oldest 5).
    let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
    let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
    let labels = Labels::of("phase", "ring");
    for i in 0..(HISTORY_RING_CAP + 5) {
        reporter.ingest(
            &labels,
            ts_counter_set(Duration::from_millis(50), (i as u64) + 1),
        );
    }
    reporter.flush_for_tests();
    let ring = reporter.ring(&labels, Duration::from_millis(50));
    assert_eq!(ring.len(), HISTORY_RING_CAP + 5);
}

// =========================================================================
// metrics query — the four modes
// =========================================================================

/// Test helper: register a counter holding `value` under family
/// `name` on the given component.
fn install_test_counter(c: &mut Component, family: &str, value: u64) {
    let counter = Arc::new(Counter::new(Labels::of("name", family)));
    counter.inc_by(value);
    c.register_instrument(family, InstrumentRef::Counter(counter))
        .unwrap();
}

fn build_query_fixture(
    cadences: Cadences,
) -> (Arc<RwLock<Component>>, Arc<CadenceReporter>, MetricsQuery) {
    let root = Component::root(Labels::of("session", "s1"), HashMap::new());
    let phase = Arc::new(RwLock::new(Component::new(
        Labels::of("phase", "load"),
        HashMap::new(),
    )));
    attach(&root, &phase);
    {
        let mut p = phase.write().unwrap();
        p.set_state(ComponentState::Running);
        install_test_counter(&mut p, "ops", 42);
    }
    let tree = CadenceTree::plan_default(cadences);
    let reporter = Arc::new(CadenceReporter::new(tree));
    let query = MetricsQuery::new(reporter.clone(), root.clone());
    (root, reporter, query)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_query_now_reads_smallest_cadence_window() {
    // Per SRD-42, `now` reads the smallest declared cadence's last
    // closed window, not the live tree. Before any window closes,
    // `now` is empty. After an ingest of data at that cadence,
    // `now` returns it.
    let (_root, reporter, q) =
        build_query_fixture(Cadences::new(&[Duration::from_millis(100)]).unwrap());
    assert!(
        q.now(&Selection::family("ops")).is_empty(),
        "pre-close now should be empty"
    );

    let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
    let mut s = MetricSet::new(Duration::from_millis(100));
    s.insert_counter("ops", Labels::default(), 42, Instant::now());
    reporter.ingest(&labels, s);
    reporter.flush_for_tests();

    let snap = q.now(&Selection::family("ops"));
    let total = match snap
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!(),
    };
    assert_eq!(total, 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_query_cadence_window_returns_latest_closed_snapshot() {
    let (_root, reporter, q) =
        build_query_fixture(Cadences::new(&[Duration::from_millis(100)]).unwrap());
    // Ingest a closed snapshot directly — MetricsQuery sees it.
    let phase_labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
    let mut ingest = MetricSet::new(Duration::from_millis(100));
    ingest.insert_counter("ops", Labels::default(), 99, Instant::now());
    reporter.ingest(&phase_labels, ingest);
    reporter.flush_for_tests();

    let snap = q.cadence_window(Duration::from_millis(100), &Selection::family("ops"));
    let total = match snap
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!(),
    };
    assert_eq!(total, 99);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_query_selection_filters_by_labels() {
    let (_root, reporter, q) =
        build_query_fixture(Cadences::new(&[Duration::from_millis(100)]).unwrap());
    let phase_labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
    let mut ingest = MetricSet::new(Duration::from_millis(100));
    ingest.insert_counter("ops", Labels::of("kind", "a"), 1, Instant::now());
    ingest.insert_counter("ops", Labels::of("kind", "b"), 5, Instant::now());
    reporter.ingest(&phase_labels, ingest);
    reporter.flush_for_tests();

    let snap = q.cadence_window(
        Duration::from_millis(100),
        &Selection::family("ops").with_label("kind", "b"),
    );
    let family = snap.family("ops").unwrap();
    assert_eq!(family.len(), 1);
    let total = match family.metrics().next().unwrap().point().unwrap().value() {
        MetricValue::Counter(c) => c.cumulative,
        _ => panic!(),
    };
    assert_eq!(total, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_query_select_one_hard_errors_on_zero_or_many() {
    let (_root, reporter, q) =
        build_query_fixture(Cadences::new(&[Duration::from_millis(100)]).unwrap());
    // Zero matches
    let err = q
        .select_one(|qi| {
            qi.cadence_window(Duration::from_millis(100), &Selection::family("missing"))
        })
        .unwrap_err();
    assert_eq!(err, SelectError::NoMatch);

    // Ingest two distinct series.
    let phase_labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
    let mut ingest = MetricSet::new(Duration::from_millis(100));
    ingest.insert_counter("ops", Labels::of("k", "a"), 1, Instant::now());
    ingest.insert_counter("ops", Labels::of("k", "b"), 2, Instant::now());
    reporter.ingest(&phase_labels, ingest);
    reporter.flush_for_tests();

    let err = q
        .select_one(|qi| qi.cadence_window(Duration::from_millis(100), &Selection::family("ops")))
        .unwrap_err();
    assert_eq!(err, SelectError::MultipleMatches(2));

    // Tight filter matches exactly one.
    let ok = q.select_one(|qi| {
        qi.cadence_window(
            Duration::from_millis(100),
            &Selection::family("ops").with_label("k", "a"),
        )
    });
    assert!(ok.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_query_session_lifetime_includes_in_flight_partial() {
    let (_root, reporter, q) =
        build_query_fixture(Cadences::new(&[Duration::from_secs(1)]).unwrap());
    let phase_labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
    // Only 200ms of data — doesn't promote at the 1s cadence, but
    // session_lifetime should still see it through the prebuffer peek.
    let mut partial = MetricSet::new(Duration::from_millis(200));
    partial.insert_counter("ops", Labels::default(), 7, Instant::now());
    reporter.ingest(&phase_labels, partial);
    reporter.flush_for_tests();

    let snap = q.session_lifetime(&Selection::family("ops"));
    let total = match snap
        .family("ops")
        .unwrap()
        .metrics()
        .next()
        .unwrap()
        .point()
        .unwrap()
        .value()
    {
        MetricValue::Counter(c) => c.cumulative,
        // session_lifetime merges the live-now counter too, so the
        // observed total may be larger than 7 (adds the stub's 42).
        _ => panic!(),
    };
    assert!(
        total >= 7,
        "session_lifetime should include in-flight prebuffer: got {total}"
    );
}

// =========================================================================
// component tree + capture_tree
// =========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_tree_visits_only_running_components() {
    let root = Component::root(Labels::of("session", "s1"), HashMap::new());
    let phase = Arc::new(RwLock::new(Component::new(
        Labels::of("phase", "load"),
        HashMap::new(),
    )));
    attach(&root, &phase);

    // Stopped phase — not captured.
    {
        let mut p = phase.write().unwrap();
        p.set_state(ComponentState::Stopped);
        install_test_counter(&mut p, "ops", 1);
    }
    let captured = capture_tree(&root, Duration::from_secs(1));
    assert_eq!(captured.len(), 0);

    {
        let mut p = phase.write().unwrap();
        p.set_state(ComponentState::Running);
    }
    let captured = capture_tree(&root, Duration::from_secs(1));
    assert_eq!(captured.len(), 1);
}

// =========================================================================
// scheduler + reporter trait
// =========================================================================

struct CountingReporter {
    count: Arc<AtomicU64>,
}
impl Reporter for CountingReporter {
    fn report(&mut self, _snapshot: &MetricSet) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduler_feeds_cadence_reporter_and_delivers_to_external_reporter() {
    let tree = CadenceTree::plan_default(Cadences::new(&[Duration::from_millis(100)]).unwrap());
    let cr = Arc::new(CadenceReporter::new(tree));
    let external_count = Arc::new(AtomicU64::new(0));
    let ec = external_count.clone();

    let handle = SchedulerBuilder::new()
        .base_interval(Duration::from_millis(100))
        .with_cadence_reporter(cr.clone())
        .add_reporter(Duration::from_millis(100), CountingReporter { count: ec })
        .build(Box::new(move || {
            let mut s = MetricSet::new(Duration::from_millis(100));
            s.insert_counter("ops", Labels::default(), 1, Instant::now());
            vec![(Labels::of("phase", "x"), s)]
        }));

    let stop = handle.start();
    std::thread::sleep(Duration::from_millis(350));
    stop.stop().await;

    // External reporter saw several ticks.
    assert!(external_count.load(Ordering::Relaxed) >= 2);
    // Cadence reporter tracks the phase component.
    assert_eq!(cr.component_labels().len(), 1);
}

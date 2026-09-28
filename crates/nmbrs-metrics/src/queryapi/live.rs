// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The live in-process [`MetricAccess`] backend: reads the current
//! session's cadence ring through [`MetricsQuery`].
//!
//! ## Time model
//!
//! Cadence windows carry a monotonic [`std::time::Instant`]
//! ([`crate::snapshot::MetricSet::captured_at`]), not a wall-clock
//! stamp; the query API works in Unix-ms. Each window is converted as
//! `now_ms − captured_at().elapsed()`, with `now_ms` read **once per
//! call** so every window is stamped against one clock — the same
//! approximation the sqlite writer makes (`reporters::sqlite` stamps
//! `SystemTime::now()` at write time). A window is captured before any
//! query, so `window_ms ≤ now`, and the caller's `end_ms` is also
//! ~now — the freshest window is never excluded.
//!
//! ## Coverage
//!
//! Series are drawn from the **smallest declared cadence**'s ring +
//! latest + in-flight prebuffer (deduped by capture instant); the
//! queryable lookback is bounded by the ring depth. `__name__` is the
//! metric family name, with the component's scope tags merged in.
//! `Counter`/`Gauge`/`Histogram` project to f64; `Info`/`StateSet` are
//! skipped (proper histogram `_count`/`_sum`/`_bucket` decomposition is
//! a follow-up).

use std::sync::Arc;
use std::time::SystemTime;

/// A window's Unix-ms stamp against `now_ms`, read once per call. The
/// elapsed time is rounded **up**: `now_ms` is already floored, and
/// subtracting a floored elapsed could stamp a window up to 1 ms after
/// its capture — past a caller's `end_ms` read in that same millisecond,
/// which would drop the freshest window. Rounding up keeps
/// `window_ms ≤ capture`.
fn stamp_ms(now_ms: i64, captured_at: std::time::Instant) -> i64 {
    now_ms - captured_at.elapsed().as_micros().div_ceil(1000) as i64
}

use super::{Matcher, MetricAccess, QueryError, Sample, Series, Vector};
use crate::metrics_query::MetricsQuery;
use crate::snapshot::MetricValue;

/// The live in-process access backend. Cheap to construct (wraps the
/// shared `Arc`).
pub struct MetricsQueryAccess {
    query: Arc<MetricsQuery>,
}

impl MetricsQueryAccess {
    /// Wrap the session's live metrics query.
    pub fn new(query: Arc<MetricsQuery>) -> Self {
        Self { query }
    }

    /// SRD-90 §M5 — the oldest sample-time this in-memory tier can answer
    /// (Unix-ms), for the [`super::HybridStore`] coverage walk: the minimum
    /// `captured_at` across every component's smallest-cadence retention ring,
    /// converted on the same clock as [`Self::select_range`]. `None` when
    /// nothing is retained yet (empty store / no cadence) — the hybrid then
    /// never short-circuits the colder tail.
    pub fn earliest_ms(&self) -> Option<i64> {
        let reporter = self.query.reporter();
        let cadence = reporter.declared_cadences().smallest();
        if cadence.is_zero() {
            return None;
        }
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut earliest: Option<i64> = None;
        for component in reporter.component_labels() {
            // The ring is oldest-first; its first entry is the oldest retained.
            // One window-view lookup, then read `ring.first()` directly — no
            // cloning the whole ring just to peek the head.
            if let Some(oldest) = reporter
                .window_view(&component, cadence)
                .and_then(|v| v.ring.first().cloned())
            {
                let ms = stamp_ms(now_ms, oldest.captured_at());
                earliest = Some(earliest.map_or(ms, |e: i64| e.min(ms)));
            }
        }
        earliest
    }
}

impl super::HorizonAware for MetricsQueryAccess {
    fn earliest_ms(&self) -> Option<i64> {
        MetricsQueryAccess::earliest_ms(self)
    }
}

impl MetricAccess for MetricsQueryAccess {
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, QueryError> {
        let reporter = self.query.reporter();
        let cadence = reporter.declared_cadences().smallest();
        if cadence.is_zero() {
            return Ok(Vector::default());
        }
        let now_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // SRD-89 §3b / SRD-90 §M6 — execution scoping is a **uniform dimensional
        // label**: `exec_id` arrives as a matcher (injected by
        // `ExecScopedAccess` for the reading execution) and is applied by the
        // matcher check below against each series' `exec_id` scope tag — there
        // is no separate post-filter. A `None`-scope read (single-run) injects
        // nothing, so it sees the sole execution's series (A1). Each series here
        // carries `exec_id` merged in from the component scope tags, which is
        // what the matcher matches against.
        let mut out: Vec<Series> = Vec::new();
        for component in reporter.component_labels() {
            // Closed-window history + freshest closed window + in-flight
            // partial, deduped by capture instant. ONE window-view lookup per
            // component (was three: ring + latest + prebuffer each rebuilt the
            // component-path string and re-hashed it); the partial is an `Arc`
            // clone off the view, not a deep `MetricSet` copy.
            let Some(view) = reporter.window_view(&component, cadence) else {
                continue;
            };
            let mut windows: Vec<Arc<crate::snapshot::MetricSet>> =
                Vec::with_capacity(view.ring.len() + 2);
            windows.extend(view.ring.iter().cloned());
            if let Some(l) = &view.latest {
                windows.push(l.clone());
            }
            if let Some(p) = &view.prebuffer {
                windows.push(p.clone());
            }
            windows.sort_by_key(|w| w.captured_at());
            windows.dedup_by_key(|w| w.captured_at());

            for window in &windows {
                let window_ms = stamp_ms(now_ms, window.captured_at());
                if window_ms < start_ms || window_ms > end_ms {
                    continue;
                }
                for family in window.families() {
                    let name = family.name();
                    for metric in family.metrics() {
                        let labels = series_labels(name, &component, metric.labels());
                        if !matchers.iter().all(|m| m.matches(&labels)) {
                            continue;
                        }
                        let Some(value) = metric.point().and_then(|p| value_to_f64(p.value()))
                        else {
                            continue;
                        };
                        push_sample(
                            &mut out,
                            labels,
                            Sample {
                                timestamp_ms: window_ms,
                                value,
                            },
                        );
                    }
                }
            }
        }

        for s in &mut out {
            s.samples.sort_by_key(|x| x.timestamp_ms);
        }
        Ok(Vector::new(out))
    }

    // `select_instant` uses the trait default (range + latest-per-series).
}

/// Build a series label set: `__name__` (front), then the union of the
/// instrument's `metric` labels and the `component` (ring-key) scope
/// tags. Instrument labels win on key collision; both normally agree.
fn series_labels(
    name: &str,
    component: &crate::labels::Labels,
    metric: &crate::labels::Labels,
) -> Vec<(String, String)> {
    let mut out = vec![("__name__".to_string(), name.to_string())];
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert("__name__".to_string());
    for (k, v) in metric.iter().chain(component.iter()) {
        if seen.insert(k.to_string()) {
            out.push((k.to_string(), v.to_string()));
        }
    }
    out
}

/// Project a [`MetricValue`] to the f64 the query API exposes. `None`
/// for variants with no single scalar projection (skipped by callers).
fn value_to_f64(v: &MetricValue) -> Option<f64> {
    match v {
        // Counters expose the cumulative running total (Prometheus/VM
        // semantics) so MetricsQL rate()/increase/*_over_time compute
        // Δ/Δt correctly — see the cumulative-counter note.
        MetricValue::Counter(c) => Some(c.cumulative as f64),
        MetricValue::Gauge(g) => Some(g.value),
        // Histogram count is exposed cumulative too (its lifetime total),
        // so rate()/increase over a histogram count is PromQL-correct.
        MetricValue::Histogram(h) => Some(h.cumulative_count as f64),
        MetricValue::BucketedHistogram(b) => Some(b.cumulative_count as f64),
        _ => None,
    }
}

/// Append a sample to the series with this exact label set, creating it
/// if new.
fn push_sample(out: &mut Vec<Series>, labels: Vec<(String, String)>, sample: Sample) {
    if let Some(existing) = out.iter_mut().find(|s| s.labels == labels) {
        existing.samples.push(sample);
    } else {
        out.push(Series {
            labels,
            samples: vec![sample],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn value_projection_per_variant() {
        use crate::snapshot::{CounterValue, GaugeValue, HistogramValue};
        use hdrhistogram::Histogram as HdrHistogram;
        assert_eq!(
            value_to_f64(&MetricValue::Counter(CounterValue::new(7))),
            Some(7.0)
        );
        assert_eq!(
            value_to_f64(&MetricValue::Gauge(GaugeValue::new(3.5))),
            Some(3.5)
        );
        // A histogram projects its CUMULATIVE count (lifetime total), not the
        // per-window reservoir count — here a 2-sample window over a lifetime
        // of 42.
        let mut hdr = HdrHistogram::<u64>::new(3).unwrap();
        hdr.record(5).unwrap();
        hdr.record(6).unwrap();
        let hv = HistogramValue::from_hdr(hdr).with_cumulative_count(42);
        assert_eq!(value_to_f64(&MetricValue::Histogram(hv)), Some(42.0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn series_labels_promote_name_and_merge_scope() {
        let component = crate::labels::Labels::empty().with("phase", "saturate");
        let metric = crate::labels::Labels::empty().with("__name__", "stale");
        let out = series_labels("errors_total", &component, &metric);
        assert_eq!(out[0], ("__name__".to_string(), "errors_total".to_string()));
        assert_eq!(out.iter().filter(|(k, _)| k == "__name__").count(), 1);
        assert!(out.iter().any(|(k, v)| k == "phase" && v == "saturate"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_instant_returns_a_live_counter() {
        use crate::cadence::{CadenceTree, Cadences};
        use crate::cadence_reporter::CadenceReporter;
        use crate::labels::Labels;
        use crate::snapshot::MetricSet;
        use std::collections::HashMap;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        // Populate the ring synchronously.
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let comp = Labels::of("phase", "saturate");
        let mut delta = MetricSet::new(Duration::from_millis(100));
        delta.insert_counter("ops", Labels::default(), 7, Instant::now());
        reporter.scope_close(&comp, delta);
        reporter.flush_for_tests();

        let root = crate::component::Component::root(Labels::of("session", "s1"), HashMap::new());
        let access = MetricsQueryAccess::new(Arc::new(MetricsQuery::new(reporter.clone(), root)));

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let v = access
            .select_instant(&[Matcher::eq("__name__", "ops")], now_ms, Some(60_000))
            .expect("select");

        assert_eq!(v.len(), 1, "one series, got {v:?}");
        let s = &v.series()[0];
        assert!(
            s.labels
                .iter()
                .any(|(k, val)| k == "__name__" && val == "ops")
        );
        assert!(
            s.labels
                .iter()
                .any(|(k, val)| k == "phase" && val == "saturate"),
            "merged scope tag: {:?}",
            s.labels
        );
        assert_eq!(s.samples.len(), 1, "instant vector: one sample per series");
        assert_eq!(s.samples[0].value, 7.0);
    }
}

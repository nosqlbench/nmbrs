// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Unified metrics read API (SRD-42 §"MetricsQuery").
//!
//! Every consumer (TUI, summary report, SQLite emitter, GK
//! `metric()`/`metric_window()` nodes, programmatic callers) reads
//! through this single interface. There is no per-consumer access
//! layer — the query speaks the metrics system's native types
//! ([`MetricSet`] / [`MetricFamily`] / [`Metric`] / [`MetricPoint`]),
//! and exposes these query modes — the method name says whether a value is a
//! running **total**, a span **increase** (delta), or a **distribution**:
//!
//! - [`MetricsQuery::now`] — running-total snapshot at the live cadence.
//! - [`MetricsQuery::cadence_window`] — the last full closed window's running
//!   totals for a declared cadence.
//! - [`MetricsQuery::session_lifetime`] — the session's running totals,
//!   walking the cascade down at read time so no in-flight data is missed.
//! - [`MetricsQuery::increase_over`] — the counter **increase** over the last
//!   `span` (PromQL `increase`; a rate is `increase / span`), differenced
//!   from the retained finest ring at the finest covering resolution.
//! - [`MetricsQuery::distribution_over`] — the merged latency/value
//!   **distribution** (histogram reservoir) over the last `span`.
//!
//! ## Selection
//!
//! Every query takes a [`Selection`] — a label-based filter applied
//! to each `(component_labels, metric_labels)` pair as the query
//! walks the store and the live tree. Identity for combine /
//! deduplication is `(family.name, label_set)` per OpenMetrics
//! §4.5.1.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::cadence_reporter::CadenceReporter;
use crate::component::Component;
use crate::labels::Labels;
use crate::snapshot::{CounterValue, Metric, MetricFamily, MetricPoint, MetricSet, MetricValue};

/// A label-based filter for selecting which metrics a query operates
/// on. Composes by AND — every constraint must match.
#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Required metric family name. `None` matches any family.
    family: Option<String>,
    /// Required label `(key, value)` pairs on the metric's `LabelSet`.
    /// All pairs must match.
    label_eq: Vec<(String, String)>,
    /// Required label `(key, value_substring)` pairs. The value
    /// must contain the substring.
    label_contains: Vec<(String, String)>,
}

impl Selection {
    pub fn all() -> Self {
        Self::default()
    }

    /// Match any series in the named family.
    pub fn family(name: impl Into<String>) -> Self {
        Self {
            family: Some(name.into()),
            ..Default::default()
        }
    }

    /// Builder form: constrain this selection to the named family.
    pub fn with_family(mut self, name: impl Into<String>) -> Self {
        self.family = Some(name.into());
        self
    }

    /// Restrict to series whose `LabelSet` contains `key=value`.
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.label_eq.push((key.into(), value.into()));
        self
    }

    /// Restrict to series whose label value at `key` contains
    /// `substring` (operator-friendly for path-style labels).
    pub fn with_label_containing(
        mut self,
        key: impl Into<String>,
        substring: impl Into<String>,
    ) -> Self {
        self.label_contains.push((key.into(), substring.into()));
        self
    }

    /// True when the selection's family constraint matches the
    /// candidate, or there is no family constraint.
    pub fn matches_family(&self, family_name: &str) -> bool {
        self.family
            .as_deref()
            .map(|f| f == family_name)
            .unwrap_or(true)
    }

    /// True when every label constraint is satisfied by `labels`.
    pub fn matches_labels(&self, labels: &Labels) -> bool {
        for (k, v) in &self.label_eq {
            if labels.get(k) != Some(v.as_str()) {
                return false;
            }
        }
        for (k, sub) in &self.label_contains {
            match labels.get(k) {
                Some(value) if value.contains(sub.as_str()) => {}
                _ => return false,
            }
        }
        true
    }
}

/// Errors returned by selection-required queries.
#[derive(Debug, PartialEq, Eq)]
pub enum SelectError {
    /// The selection matched no metric instance.
    NoMatch,
    /// The selection matched more than one instance — caller
    /// requested exactly one.
    MultipleMatches(usize),
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMatch => write!(f, "selection matched no metric instance"),
            Self::MultipleMatches(n) => {
                write!(f, "selection matched {n} instances, expected exactly one")
            }
        }
    }
}

impl std::error::Error for SelectError {}

/// The unified metrics read interface. Constructed once at session
/// start with references to the cadence reporter (for closed
/// windows + cascade peeks) and the component tree root (for the
/// `now` mode's live instrument walk).
pub struct MetricsQuery {
    reporter: Arc<CadenceReporter>,
    /// Session-scope component root. Held for component-tree
    /// structural queries (e.g. "how many phases are running?")
    /// performed by display code like the TUI's Focus-LOD
    /// placeholder logic.
    component_root: Arc<RwLock<Component>>,
}

impl MetricsQuery {
    pub fn new(reporter: Arc<CadenceReporter>, component_root: Arc<RwLock<Component>>) -> Self {
        Self {
            reporter,
            component_root,
        }
    }

    /// Reference to the cadence reporter — exposed so consumers that
    /// need to enumerate declared cadences (e.g., per-cadence
    /// columns) can ask it directly.
    pub fn reporter(&self) -> &Arc<CadenceReporter> {
        &self.reporter
    }

    /// Count of phases currently in `Running` state anywhere in the
    /// session's component tree. A pure structural query — no
    /// metric data involved. Used by display code that needs to
    /// decide "live vs waiting vs done" without re-implementing
    /// that logic over its own state mirror.
    pub fn running_phase_count(&self) -> usize {
        self.component_root
            .read()
            .map(|c| c.running_descendant_count())
            .unwrap_or(0)
    }

    // ---- now: live instrument peek -------------------------------------

    /// Recent snapshot at the smallest declared cadence, filtered
    /// by `selection`.
    ///
    /// Reads [`Self::cadence_window`] at the smallest declared
    /// cadence (1 s for default configurations) — the last fully-
    /// closed window of that cadence. Does NOT pass through to
    /// the live instruments: counter values, gauge values, and
    /// histogram reservoirs all come from the cadence-reporter
    /// store, which the scheduler populates via its per-tick
    /// coalesce.
    ///
    /// Why not a live-instrument peek? Counters are absolute
    /// atomics (peek is free), but histogram peeks return "samples
    /// accumulated since the scheduler's last drain" — a partial,
    /// drifting sub-interval window. The 1 s cadence window is a
    /// stable, sample-weighted view that matches what every other
    /// reader (summary, SQLite, cadence subscribers) sees for the
    /// same time slice.
    ///
    /// Returns an empty `MetricSet` before the first window of the
    /// smallest declared cadence closes (i.e., during the first
    /// `1 s` of a run). Callers that need true sub-second live data
    /// for a specific Timer should use
    /// [`crate::instruments::timer::Timer::peek_live_window`].
    pub fn now(&self, selection: &Selection) -> MetricSet {
        let smallest = self.reporter.declared_cadences().smallest();
        if smallest.is_zero() {
            return MetricSet::at(Instant::now(), Duration::ZERO);
        }
        self.cadence_window(smallest, selection)
    }

    /// Build a [`MetricHandle`] that caches the `(selection,
    /// cadence)` pair for repeated cheap reads. The handle reads
    /// the smallest declared cadence's last closed window on every
    /// `read_now` — no component-tree walk, no live-instrument
    /// access.
    ///
    /// Callers that want a specific cadence (not the smallest) can
    /// use [`Self::resolve_at`].
    pub fn resolve(&self, selection: Selection) -> MetricHandle {
        let cadence = self.reporter.declared_cadences().smallest();
        MetricHandle {
            reporter: self.reporter.clone(),
            selection,
            cadence,
        }
    }

    /// Same as [`Self::resolve`] but pins the handle to a specific
    /// cadence — use for per-cadence columns in summary reports
    /// or for explicit longer-horizon readers.
    pub fn resolve_at(&self, selection: Selection, cadence: Duration) -> MetricHandle {
        MetricHandle {
            reporter: self.reporter.clone(),
            selection,
            cadence,
        }
    }

    // ---- cadence_window: last closed snapshot --------------------------

    /// Latest fully-closed snapshot for the named cadence, filtered
    /// by `selection`. Returns an empty snapshot when no closed
    /// window has been published yet (early in a run).
    ///
    /// Walks every component tracked by the cadence reporter,
    /// merging matching metrics into one result. Identity follows
    /// OpenMetrics §4.5.1 — same `(family.name, label_set)` combines.
    pub fn cadence_window(&self, cadence: Duration, selection: &Selection) -> MetricSet {
        let mut out = MetricSet::at(Instant::now(), cadence);
        for component in self.reporter.component_labels() {
            let Some(snap) = self.reporter.latest(&component, cadence) else {
                continue;
            };
            for family in snap.families() {
                if !selection.matches_family(family.name()) {
                    continue;
                }
                for metric in family.metrics() {
                    if !selection.matches_labels(metric.labels()) {
                        continue;
                    }
                    insert_metric_into(&mut out, family, metric);
                }
            }
        }
        out
    }

    // ---- increase_over / distribution_over: sliding span derivations ----

    /// The finest declared cadence whose retained ring (`HISTORY_RING_CAP`
    /// windows) covers `span`, so a sliding lookback stays at the finest
    /// available resolution. `None` if no cadence is declared.
    fn finest_cadence_covering(&self, span: Duration) -> Option<Duration> {
        let cap = crate::cadence_reporter::HISTORY_RING_CAP as u128;
        let mut chosen: Option<Duration> = None;
        for layer in self.reporter.layers() {
            if layer.hidden {
                continue;
            }
            chosen = Some(layer.interval);
            if layer.interval.as_nanos().saturating_mul(cap) >= span.as_nanos() {
                break;
            }
        }
        chosen
    }

    /// The counter **increase** over the trailing `span` (PromQL `increase`):
    /// for each matched counter, `cum[now] − cum[now−span]` differenced from
    /// the retained finest-cadence ring at the finest covering resolution — a
    /// continuous/sliding window (e.g. "the last 10 s" off the 1 s ring).
    /// Emits **counters only**, and their values are DELTAS — contrast the
    /// running-total readers [`Self::now`] / [`Self::cadence_window`] /
    /// [`Self::session_lifetime`]. A rate is `increase / span`. For the recent
    /// latency/value distribution use [`Self::distribution_over`].
    pub fn increase_over(&self, span: Duration, selection: &Selection) -> MetricSet {
        let mut out = MetricSet::at(Instant::now(), span);
        let Some(cadence) = self.finest_cadence_covering(span) else {
            return out;
        };
        let windows = ((span.as_nanos().max(1)) / (cadence.as_nanos().max(1))).max(1) as usize;
        let now = Instant::now();
        for component in self.reporter.component_labels() {
            let ring = self.reporter.ring(&component, cadence);
            if ring.is_empty() {
                continue;
            }
            let end = ring.len();
            let start = end.saturating_sub(windows);
            let per = coalesce_component_windows(
                ring[start..end].iter().map(|a| a.as_ref()),
                selection,
                now,
                span,
            );
            // Baseline = the running total just BEFORE the span (window at
            // start-1); subtracting it turns a counter's window-end cumulative
            // into the span increase. `None` (→ 0) at the ring's start.
            let baseline = start.checked_sub(1).map(|i| ring[i].clone());
            for family in per.families() {
                for metric in family.metrics() {
                    insert_counter_increase_into(&mut out, family, metric, baseline.as_deref());
                }
            }
        }
        out
    }

    /// The merged latency/value **distribution** over the trailing `span`:
    /// for each matched histogram, the HDR reservoir merged across the windows
    /// in the span (read `p50`/`p99`/`mean` from it — the PromQL `*_over_time`
    /// quantile family). Same finest-covering-resolution sliding window as
    /// [`Self::increase_over`]. Emits **histograms only** — the recent
    /// distribution, never a counter increase.
    pub fn distribution_over(&self, span: Duration, selection: &Selection) -> MetricSet {
        let mut out = MetricSet::at(Instant::now(), span);
        let Some(cadence) = self.finest_cadence_covering(span) else {
            return out;
        };
        let windows = ((span.as_nanos().max(1)) / (cadence.as_nanos().max(1))).max(1) as usize;
        let now = Instant::now();
        for component in self.reporter.component_labels() {
            let ring = self.reporter.ring(&component, cadence);
            if ring.is_empty() {
                continue;
            }
            let end = ring.len();
            let start = end.saturating_sub(windows);
            let per = coalesce_component_windows(
                ring[start..end].iter().map(|a| a.as_ref()),
                selection,
                now,
                span,
            );
            for family in per.families() {
                for metric in family.metrics() {
                    if matches!(
                        metric.point().map(|p| p.value()),
                        Some(MetricValue::Histogram(_)) | Some(MetricValue::BucketedHistogram(_))
                    ) {
                        insert_metric_into(&mut out, family, metric);
                    }
                }
            }
        }
        out
    }

    // ---- session_lifetime: full canonical span ------------------------

    /// Full canonical session span as of *now*, filtered by
    /// `selection`. Walks the cascade *down* at read time:
    ///
    /// Per component, COALESCEs the cascade's disjoint time-slices — every
    /// layer's in-flight prebuffer plus the largest cadence's retained
    /// accumulator (the lifetime buffer) and its last-closed window — then
    /// AGGREGATEs the per-component results across components. Coalescing
    /// the time dimension keeps the latest `cumulative` and merges
    /// reservoirs, so a session-cumulative counter is the latest running
    /// total — not multiplied by the number of cascade sources it appears in.
    ///
    /// Per SRD-42 §"Cost rule for recent_window", only matched metric
    /// instances combine — same shape as `increase_over` / `distribution_over`.
    pub fn session_lifetime(&self, selection: &Selection) -> MetricSet {
        let session_age = self.reporter.started_at().elapsed();
        let now = Instant::now();
        let mut out = MetricSet::at(now, session_age);
        let largest = self.reporter.layers().last().map(|l| l.interval);

        for component in self.reporter.component_labels() {
            // Gather this component's disjoint cascade sources.
            let mut sources: Vec<MetricSet> = Vec::new();
            for layer in self.reporter.layers() {
                if let Some(pre) = self.reporter.prebuffer(&component, layer.interval) {
                    sources.push(pre);
                }
                // Only the LARGEST cadence's last-closed window is read: a
                // smaller layer's closed window already folded into the next
                // layer's prebuffer (reading it too would double-count); the
                // largest cadence has no parent to fold into. (The earlier
                // `now`/closed-window top-up re-read a promoted window — the
                // source of the session-cumulative overcount.)
                if Some(layer.interval) == largest
                    && let Some(latest) = self.reporter.latest(&component, layer.interval)
                {
                    sources.push((*latest).clone());
                }
            }
            if sources.is_empty() {
                continue;
            }
            // Coalesce the time dimension, then aggregate across components.
            let per = coalesce_component_windows(sources.iter(), selection, now, session_age);
            for family in per.families() {
                for metric in family.metrics() {
                    insert_metric_into(&mut out, family, metric);
                }
            }
        }

        out
    }

    // ---- expect-exactly-one helpers ------------------------------------

    /// Run a query mode and assert exactly one matching `Metric` per
    /// the SRD's "specific metric" semantics. Returns `Err` if 0 or
    /// >1 matches.
    pub fn select_one<F>(&self, mode: F) -> Result<MetricSet, SelectError>
    where
        F: FnOnce(&Self) -> MetricSet,
    {
        let snap = mode(self);
        let total: usize = snap.families().map(|f| f.len()).sum();
        match total {
            0 => Err(SelectError::NoMatch),
            1 => Ok(snap),
            n => Err(SelectError::MultipleMatches(n)),
        }
    }
}

/// Memoized pull handle — resolved once via [`MetricsQuery::resolve`],
/// then reused for cheap per-draw / per-frame reads.
///
/// The handle caches the `(selection, cadence)` pair the caller
/// asked for. Each `read_now` issues a `cadence_window` query
/// against the reporter — O(components) per call, no component-tree
/// walk, no instrument access.
///
/// Per SRD-42 (revised), "recent info" always routes through the
/// cadence-reporter store, never through live instruments. The
/// handle's `read_now` reads the smallest declared cadence's last
/// closed window; callers who need true sub-second live data for a
/// specific Timer should use
/// [`crate::instruments::timer::Timer::peek_live_window`] directly.
pub struct MetricHandle {
    reporter: Arc<CadenceReporter>,
    selection: Selection,
    cadence: Duration,
}

impl MetricHandle {
    /// Read the last closed window at this handle's cadence,
    /// filtered by the handle's selection. Non-mutating, safe to
    /// call arbitrarily often.
    pub fn read_now(&self) -> MetricSet {
        let mut out = MetricSet::at(Instant::now(), self.cadence);
        for component in self.reporter.component_labels() {
            let Some(snap) = self.reporter.latest(&component, self.cadence) else {
                continue;
            };
            for family in snap.families() {
                if !self.selection.matches_family(family.name()) {
                    continue;
                }
                for metric in family.metrics() {
                    if !self.selection.matches_labels(metric.labels()) {
                        continue;
                    }
                    insert_metric_into(&mut out, family, metric);
                }
            }
        }
        out
    }

    /// No-op retained for API compatibility with callers that
    /// expect to "refresh" after phase transitions. The new handle
    /// reads through the cadence reporter's store, which already
    /// reflects the current set of tracked components — no resync
    /// needed.
    pub fn refresh(&mut self) {}

    /// The selection this handle was resolved against.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Cadence this handle reads from (the smallest declared
    /// cadence at resolve time).
    pub fn cadence(&self) -> Duration {
        self.cadence
    }

    /// Number of components currently tracked by the reporter —
    /// informational. Not cached; queried fresh each call.
    pub fn source_count(&self) -> usize {
        self.reporter.component_labels().len()
    }
}

/// Insert one `(family, metric)` pair into `out`, merging an existing
/// same-identity entry (OpenMetrics §4.5.1) as a **cross-component
/// aggregate** ([`CombineMode::Aggregate`]): counter `cumulative` and
/// histogram `cumulative_count` SUM across the matching series from
/// different components.
fn insert_metric_into(out: &mut MetricSet, family: &MetricFamily, metric: &Metric) {
    insert_metric_with_mode(out, family, metric, crate::snapshot::CombineMode::Aggregate);
}

/// Insert one `(family, metric)` pair into `out`, merging an existing
/// same-identity entry under `mode`. Use [`CombineMode::Coalesce`] to
/// fold the **time dimension** (consecutive windows / cascade layers of
/// one series: keep the latest `cumulative`, merge reservoirs) and
/// [`CombineMode::Aggregate`] to fold **across components** (sum). Mixing
/// them up double-counts a cumulative value — see `session_lifetime` /
/// `increase_over`.
fn insert_metric_with_mode(
    out: &mut MetricSet,
    family: &MetricFamily,
    metric: &Metric,
    mode: crate::snapshot::CombineMode,
) {
    let Some(point) = metric.point() else { return };
    let existing = out
        .family(family.name())
        .and_then(|f| f.metric_with_labels(metric.labels()))
        .is_some();
    if existing {
        // Combine into existing — requires owned access via a
        // rebuild. Simpler pattern: drop the existing family and
        // rebuild with a coalesced replacement. For Phase 7 v1 we
        // accept the cost and use `MetricSet::coalesce` over a
        // two-element slice.
        let mut tmp = MetricSet::at(out.captured_at(), out.interval());
        tmp.insert_metric(
            family.name().to_string(),
            family.r#type(),
            metric.labels().clone(),
            point.value().clone(),
            point.timestamp().unwrap_or(out.captured_at()),
        );
        let merged = MetricSet::coalesce_with_mode(
            std::slice::from_ref(out)
                .iter()
                .chain(std::slice::from_ref(&tmp).iter())
                .cloned()
                .collect::<Vec<_>>()
                .as_slice(),
            mode,
        );
        *out = merged;
    } else {
        out.insert_metric(
            family.name().to_string(),
            family.r#type(),
            metric.labels().clone(),
            point.value().clone(),
            point.timestamp().unwrap_or(out.captured_at()),
        );
    }
}

/// Fold one component's time-ordered window `sources` into a single
/// per-component snapshot, COALESCING the time dimension (keep the latest
/// `cumulative`, merge reservoirs) over the matched selection only. The
/// caller then AGGREGATEs the result across components. This split keeps a
/// cumulative counter at its latest running total rather than multiplied
/// by the number of cascade sources it appears in.
fn coalesce_component_windows<'a>(
    sources: impl IntoIterator<Item = &'a MetricSet>,
    selection: &Selection,
    captured_at: Instant,
    interval: Duration,
) -> MetricSet {
    let mut per = MetricSet::at(captured_at, interval);
    for src in sources {
        for family in src.families() {
            if !selection.matches_family(family.name()) {
                continue;
            }
            for metric in family.metrics() {
                if !selection.matches_labels(metric.labels()) {
                    continue;
                }
                insert_metric_with_mode(
                    &mut per,
                    family,
                    metric,
                    crate::snapshot::CombineMode::Coalesce,
                );
            }
        }
    }
    per
}

/// Insert a counter's span **increase** into `out`, converting its
/// window-end cumulative into `cum[end] − cum[before span]` by subtracting
/// the `baseline` running total (the window just before the span), then
/// aggregating (summing) across components. **Counters only** — non-counter
/// points are skipped (the distribution lives in `distribution_over`). This
/// is the consumer-side derivation `increase_over` applies.
fn insert_counter_increase_into(
    out: &mut MetricSet,
    family: &MetricFamily,
    metric: &Metric,
    baseline: Option<&MetricSet>,
) {
    let Some(point) = metric.point() else { return };
    let MetricValue::Counter(c) = point.value() else {
        return;
    };
    let base = baseline
        .and_then(|b| b.family(family.name()))
        .and_then(|f| f.metric_with_labels(metric.labels()))
        .and_then(|m| m.point())
        .and_then(|p| match p.value() {
            MetricValue::Counter(bc) => Some(bc.cumulative),
            _ => None,
        })
        .unwrap_or(0);
    let increase = c.cumulative.saturating_sub(base);
    let inc = Metric::single(
        metric.labels().clone(),
        MetricPoint::new(
            MetricValue::Counter(CounterValue::new(increase)),
            point.timestamp().unwrap_or(out.captured_at()),
        ),
    );
    insert_metric_into(out, family, &inc);
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cadence::{CadenceTree, Cadences};
    use crate::component::{Component, ComponentState, InstrumentRef, attach};
    use crate::instruments::counter::Counter;
    use crate::snapshot::MetricValue;
    use std::collections::HashMap;

    fn build_one_component_query() -> (Arc<RwLock<Component>>, Arc<CadenceReporter>, MetricsQuery) {
        let root = Component::root(Labels::of("session", "s1"), HashMap::new());
        let phase = Arc::new(RwLock::new(Component::new(
            Labels::of("phase", "load"),
            HashMap::new(),
        )));
        attach(&root, &phase);
        {
            let mut p = phase.write().unwrap();
            p.set_state(ComponentState::Running);
            let counter = Arc::new(Counter::new(Labels::of("name", "ops")));
            counter.inc_by(7);
            p.register_instrument("ops", InstrumentRef::Counter(counter))
                .unwrap();
        }

        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let tree = CadenceTree::plan_default(cadences);
        let reporter = Arc::new(CadenceReporter::new(tree));
        let query = MetricsQuery::new(reporter.clone(), root.clone());
        (root, reporter, query)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn now_reads_smallest_cadence_window() {
        // `now` no longer walks the live tree — it reads
        // `cadence_window(smallest_declared)`. So we must ingest a
        // closed window first; before any close, `now` is empty.
        let (_root, reporter, query) = build_one_component_query();
        assert!(
            query.now(&Selection::family("ops")).is_empty(),
            "pre-close now should be empty"
        );

        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::default(), 42, Instant::now());
        reporter.ingest(&labels, s);
        reporter.flush_for_tests();

        let snap = query.now(&Selection::family("ops"));
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
            _ => panic!("not a counter"),
        };
        assert_eq!(total, 42);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cadence_window_returns_latest_closed_snapshot() {
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        // Inject a closed window via the reporter.
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::default(), 99, Instant::now());
        reporter.ingest(&labels, s);
        reporter.flush_for_tests();

        let snap = query.cadence_window(Duration::from_millis(100), &Selection::family("ops"));
        let f = snap
            .family("ops")
            .expect("ops family in cadence_window result");
        match f.metrics().next().unwrap().point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 99),
            _ => panic!("not a counter"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_lifetime_does_not_overcount_a_single_counter() {
        // session_lifetime walks the cascade down (every layer's prebuffer
        // + the largest's latest) and combines. With ONE ingested counter
        // the canonical value is its cumulative — not a multiple from the
        // same value appearing in several cascade sources.
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::default(), 42, Instant::now());
        reporter.ingest(&labels, s);
        reporter.flush_for_tests();

        let snap = query.session_lifetime(&Selection::family("ops"));
        let cumulative = match snap
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
            _ => panic!("not a counter"),
        };
        assert_eq!(
            cumulative, 42,
            "session_lifetime cumulative overcounted (got {cumulative})"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn increase_over_gives_the_span_increment() {
        // Two windows of a counter — cumulative 10 then 20, no prior data.
        // The recent-span value is the increment over the span (cum[end] −
        // cum[before span] = 20 − 0 = 20), derived from the running totals.
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));

        let mut s1 = MetricSet::new(Duration::from_millis(100));
        s1.insert_counter("ops", Labels::default(), 10, Instant::now());
        reporter.ingest(&labels, s1);
        reporter.flush_for_tests();
        std::thread::sleep(Duration::from_millis(2)); // distinct window timestamps
        let mut s2 = MetricSet::new(Duration::from_millis(100));
        s2.insert_counter("ops", Labels::default(), 20, Instant::now());
        reporter.ingest(&labels, s2);
        reporter.flush_for_tests();

        let snap = query.increase_over(Duration::from_millis(250), &Selection::family("ops"));
        let value = match snap
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
            _ => panic!("not a counter"),
        };
        assert_eq!(
            value, 20,
            "span increment over the recent window (got {value})"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn increase_over_subtracts_prior_cumulative() {
        // A counter climbing 100→110→120 over three windows. The recent
        // window's value is the INCREASE over the span — the running total
        // BEFORE the span is subtracted — not the latest cumulative.
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        for v in [100u64, 110, 120] {
            let mut s = MetricSet::new(Duration::from_millis(100));
            s.insert_counter("ops", Labels::default(), v, Instant::now());
            reporter.ingest(&labels, s);
            reporter.flush_for_tests();
            std::thread::sleep(Duration::from_millis(2));
        }
        let read = |span| match query
            .increase_over(span, &Selection::family("ops"))
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
            _ => panic!("not a counter"),
        };
        assert_eq!(
            read(Duration::from_millis(100)),
            10,
            "last window increase = 120−110"
        );
        assert_eq!(
            read(Duration::from_millis(200)),
            20,
            "last two windows increase = 120−100"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn distribution_over_merges_histogram_windows() {
        use hdrhistogram::Histogram as HdrHistogram;
        // Two windows of a 2-sample histogram; the recent distribution over a
        // span covering both is the MERGED reservoir (4 samples).
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        for _ in 0..2 {
            let mut s = MetricSet::new(Duration::from_millis(100));
            let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
            h.record(1_000_000).unwrap();
            h.record(2_000_000).unwrap();
            s.insert_histogram("latency", Labels::default(), h, Instant::now());
            reporter.ingest(&labels, s);
            reporter.flush_for_tests();
            std::thread::sleep(Duration::from_millis(2));
        }
        let snap =
            query.distribution_over(Duration::from_millis(250), &Selection::family("latency"));
        let count = match snap
            .family("latency")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value()
        {
            MetricValue::Histogram(h) => h.count,
            _ => panic!("not a histogram"),
        };
        assert_eq!(count, 4, "two windows of 2 samples merge to 4");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn selection_filter_excludes_non_matching_labels() {
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::of("kind", "a"), 5, Instant::now());
        s.insert_counter("ops", Labels::of("kind", "b"), 9, Instant::now());
        reporter.ingest(&labels, s);
        reporter.flush_for_tests();

        let snap = query.cadence_window(
            Duration::from_millis(100),
            &Selection::family("ops").with_label("kind", "b"),
        );
        let f = snap.family("ops").expect("ops family");
        assert_eq!(f.len(), 1);
        match f.metrics().next().unwrap().point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 9),
            _ => panic!("not a counter"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_one_errors_on_zero_matches() {
        let (_root, _reporter, query) = build_one_component_query();
        let result = query.select_one(|q| {
            q.cadence_window(
                Duration::from_millis(100),
                &Selection::family("nonexistent"),
            )
        });
        assert_eq!(result.unwrap_err(), SelectError::NoMatch);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn select_one_succeeds_on_exact_match() {
        let (_root, reporter, query) = build_one_component_query();
        let labels = Labels::of("session", "s1").extend(&Labels::of("phase", "load"));
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::default(), 1, Instant::now());
        reporter.ingest(&labels, s);
        reporter.flush_for_tests();

        let result = query.select_one(|q| {
            q.cadence_window(Duration::from_millis(100), &Selection::family("ops"))
        });
        assert!(result.is_ok());
    }
}

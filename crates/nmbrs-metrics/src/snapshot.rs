// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenMetrics-aligned snapshot data model (SRD-42 §"Snapshot data
//! model"). Mirrors the OpenMetrics specification 1:1 so external
//! consumers (Prometheus scrape, OTel translation, third-party
//! dashboards) get a near-trivial projection.
//!
//! ## Container hierarchy (spec terms verbatim)
//!
//! | Layer | OpenMetrics §  | Type |
//! |---|---|---|
//! | top-level | §4.1 `MetricSet`     | [`MetricSet`] |
//! | family    | §4.4 `MetricFamily`  | [`MetricFamily`] |
//! | series    | §4.5 `Metric`        | [`Metric`] |
//! | point     | §4.6 `MetricPoint`   | [`MetricPoint`] |
//!
//! A time series is identified by `(MetricFamily.name, LabelSet)`
//! per spec §4.5.1 — the same identity used by cascade-time combine
//! (matching identity → matching reservoir / counter / gauge → combine
//! permitted).
//!
//! ## Histograms
//!
//! Internally we keep the HDR reservoir as the source of truth on
//! [`HistogramValue`] — that's what combines correctly across
//! cascade folds and ephemeral merges. The OpenMetrics-shaped
//! cumulative `Bucket` list is **derived on demand at exposition
//! time** against the consumer-requested bucket layout. `sum` /
//! `count` are also derivable but maintained alongside the reservoir
//! for O(1) access.
//!
//! ## Naming convention
//!
//! Suffix rules from spec §4.4.1 / §5.x (`_total`, `_count`, `_sum`,
//! `_bucket`, `_created`, `_info`) are **exposition-time concerns,
//! not stored**. A counter is named `cycles` in memory; the
//! exposition layer appends `_total` per spec.
//!
//! ## Initial coverage
//!
//! `Counter`, `Gauge`, `Histogram` are implemented. `Summary`,
//! `Info`, `StateSet`, `Unknown`, `GaugeHistogram` are listed in
//! [`MetricType`] but their value variants are added when a real
//! consumer needs them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram as HdrHistogram;

use crate::labels::Labels;

// =========================================================================
// MetricSet — top-level snapshot (OpenMetrics §4.1)
// =========================================================================

/// Top-level snapshot container per OpenMetrics §4.1. Holds zero or
/// more [`MetricFamily`] entries, names unique within the set.
///
/// Snapshots are immutable once published. Producers build a new
/// `MetricSet` per cadence-window close; consumers read the
/// `Arc<MetricSet>` published into the cadence reporter's store.
///
/// `MetricSet` carries two pieces of nmbrs internal metadata that
/// don't appear in the OpenMetrics spec but are needed by the
/// scheduler's coalesce path:
///
/// - `captured_at` — wall-clock instant the snapshot was sealed.
/// - `interval` — duration the snapshot represents (cadence window
///   length for cadence-window snapshots; `Duration::ZERO` for
///   instantaneous `now` reads).
///
/// These are intentionally not in any `MetricPoint`; consumers that
/// project to OpenMetrics on-wire format should read the per-point
/// timestamps and `_created` fields instead.
/// SRD-93 M4/A6 — why a snapshot's window was sealed by a lifecycle
/// boundary rather than closing naturally on its cadence. Typed so
/// durable sinks act on the *reason*, never inferring lifecycle from
/// the `partial` flag (`Quiesce` seals partials without ending scope
/// and MUST NOT produce exit events). Variant order is severity —
/// coalesce keeps the strongest reason across inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CloseReason {
    /// Mid-session quiesce: windows sealed so readers see complete
    /// data; the owning components continue. Never an exit signal.
    Quiesce,
    /// The owning component is being torn down (`scope_close`).
    ScopeClose,
    /// Session shutdown flush: every path seals for the last time.
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct MetricSet {
    captured_at: Instant,
    interval: Duration,
    /// SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close flush":
    /// a snapshot is `partial=true` when it was sealed before its
    /// cadence window naturally closed — typically because the
    /// component owning the contributing instruments is being torn
    /// down between cadence pulses. Partial snapshots fold into the
    /// next full window via the same `coalesce` rules as normal
    /// pulse-flushed samples (Counter latest-cumulative, Gauge
    /// last-write-wins, Histogram HDR-merge); the flag is
    /// preserved so downstream tooling can distinguish the
    /// scope-close contribution if needed. Coalesce result is
    /// partial whenever **any** contributing input was partial.
    partial: bool,
    /// SRD-93 M4 — the lifecycle reason this window was sealed, when
    /// sealed by a boundary (`None` for naturally-closed windows).
    /// Rides beside `partial` rather than replacing it: `partial`
    /// answers "is this window shorter than its cadence", `close`
    /// answers "what lifecycle event sealed it".
    close: Option<CloseReason>,
    /// SRD-102 §6: the nominal cadence deadline this snapshot was
    /// *scheduled* to fire at, when produced by the cadence scheduler.
    /// `captured_at` is the *actual* fire instant; the pair lets
    /// downstream tooling see schedule-vs-reality. `None` for
    /// event-driven (non-cadence) snapshots. Logical/cadence
    /// processing keeps using the prescribed `interval`/cadence — this
    /// is a record of divergence, not an input to windowing.
    scheduled_ts: Option<Instant>,
    families: Vec<MetricFamily>,
}

impl Default for MetricSet {
    fn default() -> Self {
        Self {
            captured_at: Instant::now(),
            interval: Duration::ZERO,
            partial: false,
            close: None,
            scheduled_ts: None,
            families: Vec::new(),
        }
    }
}

impl MetricSet {
    /// Construct an empty snapshot stamped with the current instant
    /// and the given window interval.
    pub fn new(interval: Duration) -> Self {
        Self {
            captured_at: Instant::now(),
            interval,
            partial: false,
            close: None,
            scheduled_ts: None,
            families: Vec::new(),
        }
    }

    /// Construct an empty snapshot stamped with an explicit
    /// `captured_at` (e.g., for tests reproducing a known instant).
    pub fn at(captured_at: Instant, interval: Duration) -> Self {
        Self {
            captured_at,
            interval,
            partial: false,
            close: None,
            scheduled_ts: None,
            families: Vec::new(),
        }
    }

    pub fn captured_at(&self) -> Instant {
        self.captured_at
    }

    /// The *actual* instant this snapshot fired/was captured (alias of
    /// [`captured_at`](Self::captured_at), named for the SRD-102
    /// scheduled/actual timestamp pair).
    pub fn actual_ts(&self) -> Instant {
        self.captured_at
    }

    /// The nominal cadence deadline this snapshot was scheduled for, if
    /// produced by the cadence scheduler (SRD-102 §6). `None` for
    /// event-driven snapshots.
    pub fn scheduled_ts(&self) -> Option<Instant> {
        self.scheduled_ts
    }

    /// Stamp the nominal cadence deadline (the scheduler sets this after
    /// coalescing a tick's component snapshots).
    pub fn set_scheduled_ts(&mut self, scheduled: Instant) {
        self.scheduled_ts = Some(scheduled);
    }
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// True if this snapshot represents a partial cadence window
    /// (sealed before the window naturally closed — typically a
    /// `scope_close` flush at component teardown). See SRD-42
    /// §"Component lifecycle: scope_close flush" for the
    /// semantics.
    pub fn is_partial(&self) -> bool {
        self.partial
    }

    /// Mark this snapshot as a partial-window contribution.
    /// Idempotent. Once set, `coalesce` carries the flag forward —
    /// any output that includes this snapshot will be partial too.
    pub fn mark_partial(&mut self) {
        self.partial = true;
    }

    /// SRD-93 M4 — the lifecycle reason this window was sealed, if a
    /// boundary (rather than the cadence) sealed it.
    pub fn close_reason(&self) -> Option<CloseReason> {
        self.close
    }

    /// Stamp the lifecycle close reason. Monotone by severity: a
    /// stronger reason overwrites a weaker one, never the reverse
    /// (matching the coalesce fold, so stamp order can't matter).
    pub fn mark_close(&mut self, reason: CloseReason) {
        self.close = Some(self.close.map_or(reason, |c| c.max(reason)));
    }

    /// Set the represented interval. Used when a coalesce path
    /// promotes a snapshot to a coarser cadence's window.
    pub fn set_interval(&mut self, interval: Duration) {
        self.interval = interval;
    }

    /// Iterator over all families in this set.
    pub fn families(&self) -> impl Iterator<Item = &MetricFamily> {
        self.families.iter()
    }

    /// Lookup a family by name. `None` if no family has that name.
    pub fn family(&self, name: &str) -> Option<&MetricFamily> {
        self.families.iter().find(|f| f.name() == name)
    }

    /// Number of families in this set.
    pub fn len(&self) -> usize {
        self.families.len()
    }
    pub fn is_empty(&self) -> bool {
        self.families.is_empty()
    }

    /// True if this set carries any HDR-reservoir distribution family
    /// (`Histogram` / `GaugeHistogram`) — the memory-heavy part of a snapshot.
    ///
    /// SRD-90: cumulative counters/gauges cost ~nothing to retain across the
    /// sub-interval history (a point is just `(timestamp, value)`), but a
    /// distribution point carries a whole reservoir snapshot. The cadence
    /// retention uses this to keep counter/gauge sub-windows over a long
    /// horizon while distributions are kept only over a short one.
    pub fn has_distributions(&self) -> bool {
        self.families.iter().any(|f| {
            matches!(
                f.r#type(),
                MetricType::Histogram | MetricType::GaugeHistogram
            )
        })
    }

    /// A clone of this set with the heavy distribution families
    /// (`Histogram` / `GaugeHistogram`) dropped, keeping the cheap cumulative
    /// `Counter`/`Gauge` families. Used by the cadence retention to compact an
    /// aged sub-window past the distribution horizon without losing the
    /// counter history a windowed `rate()`/`increase()` derives from
    /// (SRD-90 §M1; counters are cheap, histograms are "a different matter").
    pub fn without_distributions(&self) -> MetricSet {
        MetricSet {
            captured_at: self.captured_at,
            interval: self.interval,
            partial: self.partial,
            close: self.close,
            scheduled_ts: self.scheduled_ts,
            families: self
                .families
                .iter()
                .filter(|f| {
                    !matches!(
                        f.r#type(),
                        MetricType::Histogram | MetricType::GaugeHistogram
                    )
                })
                .cloned()
                .collect(),
        }
    }

    /// Insert a family. Panics if a family of the same name already
    /// exists — spec §4.1 requires unique names within a `MetricSet`.
    pub fn insert(&mut self, family: MetricFamily) {
        assert!(
            !self.families.iter().any(|f| f.name() == family.name()),
            "MetricSet already contains family '{}' — names must be unique (OpenMetrics §4.1)",
            family.name(),
        );
        self.families.push(family);
    }

    /// Coalesce multiple snapshots into one. Used by the scheduler
    /// to fold smaller-cadence snapshots into a larger-cadence
    /// window per SRD-42 §"Streaming coalesce semantics".
    ///
    /// Combine rules per SRD-42 §"Combine semantics — algebraic
    /// uniformity":
    ///
    /// - Counter `total` sums; `created` keeps the earliest;
    ///   exemplar most-recent-wins.
    /// - Gauge values are LAST-WRITE-WINS by timestamp — the
    ///   OpenMetrics/Prometheus/OTel gauge contract: a sample is the
    ///   last-written scalar as of window end, and any summarization
    ///   (avg/min/max over time) happens at the query point via
    ///   `*_over_time` rollups. (Until 2026-08-08 gauges coalesced as
    ///   interval-weighted means, which silently redefined what every
    ///   metricsql rollup meant against this store — and turned
    ///   set-once facts stored as gauges into fractions.)
    /// - Histogram reservoirs add (`HdrHistogram::add`); `count`/`sum`
    ///   re-derive from the merged reservoir; bucket exemplars
    ///   most-recent-wins per index.
    /// - Identity: `(family.name, LabelSet)` — matching identity
    ///   combines, others append. Type mismatch on matching identity
    ///   is a hard error (panic).
    ///
    /// `captured_at` of the result is the latest contributing
    /// snapshot's `captured_at`; `interval` is the sum of contributing
    /// intervals.
    pub fn coalesce(snapshots: &[MetricSet]) -> MetricSet {
        // Time-coalesce of consecutive windows (the cadence cascade):
        // counter `cumulative` keeps the latest. Cross-component
        // aggregation uses [`Self::coalesce_with_mode`] with `Aggregate`.
        Self::coalesce_with_mode(snapshots, CombineMode::Coalesce)
    }

    /// [`coalesce`](Self::coalesce) with an explicit [`CombineMode`] —
    /// the only difference is how counter `cumulative` folds (latest vs.
    /// sum). See the cumulative-counter note.
    pub fn coalesce_with_mode(snapshots: &[MetricSet], mode: CombineMode) -> MetricSet {
        if snapshots.is_empty() {
            return MetricSet::default();
        }
        if snapshots.len() == 1 {
            return snapshots[0].clone();
        }

        let captured_at = snapshots.iter().map(|s| s.captured_at).max().unwrap();
        let interval: Duration = snapshots.iter().map(|s| s.interval).sum();
        // Partial flag is sticky — if any contributing input was
        // a scope_close partial, the merged result is too. SRD-42
        // §"Component lifecycle: scope_close flush" / SRD-40b §11.2.
        let partial = snapshots.iter().any(|s| s.partial);
        // A coalesced set spans multiple inputs, so no single nominal
        // deadline applies; the scheduler stamps `scheduled_ts` on the
        // combined tick snapshot after coalescing (SRD-102 §6).
        let scheduled_ts = None;

        // SRD-93 M4 — the strongest lifecycle reason across inputs
        // survives the fold (severity order on the enum).
        let close = snapshots.iter().filter_map(|s| s.close).max();
        let mut out = MetricSet {
            captured_at,
            interval,
            partial,
            close,
            scheduled_ts,
            families: Vec::new(),
        };

        // Family identity is `name`. For each unique family name
        // across inputs, fold its metrics in identity order.
        let mut seen_family: Vec<String> = Vec::new();
        for s in snapshots {
            for f in &s.families {
                if !seen_family.contains(&f.name) {
                    seen_family.push(f.name.clone());
                }
            }
        }

        for fname in seen_family {
            let mut acc: Option<MetricFamily> = None;

            for s in snapshots {
                let Some(src_family) = s.families.iter().find(|f| f.name == fname) else {
                    continue;
                };
                if acc.is_none() {
                    // Every kind takes the uniform fold path. Gauges
                    // included: `combine_into`'s gauge arm is
                    // most-recent-wins by timestamp, which — folded in
                    // snapshot order — IS last-write-wins over the
                    // coalesced window. A series absent from later
                    // snapshots keeps its previously-written value,
                    // exactly as a Prometheus scrape would report it.
                    acc = Some(MetricFamily {
                        name: src_family.name.clone(),
                        r#type: src_family.r#type,
                        unit: src_family.unit.clone(),
                        help: src_family.help.clone(),
                        metrics: src_family.metrics.clone(),
                    });
                    continue;
                }
                let dst = acc.as_mut().unwrap();
                for m in &src_family.metrics {
                    let dst_metric = dst.metrics.iter_mut().find(|d| d.labels == m.labels);
                    match dst_metric {
                        Some(dm) => {
                            let (Some(dp), Some(sp)) = (dm.points.first_mut(), m.points.first())
                            else {
                                continue;
                            };
                            combine_into(dp, sp, mode).expect("matching identity must combine");
                        }
                        None => {
                            dst.metrics.push(m.clone());
                        }
                    }
                }
            }

            if let Some(family) = acc {
                out.families.push(family);
            }
        }

        out
    }
}

// =========================================================================
// MetricFamily (OpenMetrics §4.4)
// =========================================================================

/// One metric family — a set of [`Metric`] series sharing a name,
/// type, optional unit, and optional help text. OpenMetrics §4.4.
#[derive(Clone, Debug)]
pub struct MetricFamily {
    name: String,
    r#type: MetricType,
    unit: Option<String>,
    help: Option<String>,
    metrics: Vec<Metric>,
}

impl MetricFamily {
    /// Construct an empty family.
    pub fn new(name: impl Into<String>, r#type: MetricType) -> Self {
        Self {
            name: name.into(),
            r#type,
            unit: None,
            help: None,
            metrics: Vec::new(),
        }
    }

    /// Attach a unit to this family.
    ///
    /// SRD-40b §1 / SRD-40a §4.3: when a unit is set it lands in
    /// **two** surfaces — concatenated onto the family name as an
    /// `_<unit>` suffix (per OpenMetrics §4.4) **and** stored in
    /// the `unit` field for structured access. Both surfaces flow
    /// from this single declaration so they cannot drift.
    ///
    /// If the family name already ends with `_<unit>` (or has
    /// `_<unit>` immediately before a known exposition suffix
    /// like `_total` / `_count` per [`crate::validation::check_unit_suffix`]),
    /// the name is left unchanged — the invariant is already met.
    /// Otherwise the suffix is appended: `overscan` + `ratio` →
    /// `overscan_ratio`.
    ///
    /// Empty unit is treated as no-op for the name; the unit is
    /// still stored as `Some("")` so callers can distinguish
    /// "explicitly empty" from "unset" if they care.
    pub fn with_unit(mut self, unit: impl Into<String>) -> Self {
        let unit_str: String = unit.into();
        if !unit_str.is_empty()
            && crate::validation::check_unit_suffix(&self.name, Some(&unit_str)).is_err()
        {
            self.name = format!("{}_{}", self.name, unit_str);
        }
        self.unit = Some(unit_str);
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn r#type(&self) -> MetricType {
        self.r#type
    }
    pub fn unit(&self) -> Option<&str> {
        self.unit.as_deref()
    }
    pub fn help(&self) -> Option<&str> {
        self.help.as_deref()
    }

    /// Iterator over the family's series.
    pub fn metrics(&self) -> impl Iterator<Item = &Metric> {
        self.metrics.iter()
    }

    pub fn len(&self) -> usize {
        self.metrics.len()
    }
    pub fn is_empty(&self) -> bool {
        self.metrics.is_empty()
    }

    /// Look up the series with the given LabelSet, if any. Identity
    /// per spec §4.5.1: `(family.name, label_set)`.
    pub fn metric_with_labels(&self, labels: &Labels) -> Option<&Metric> {
        self.metrics.iter().find(|m| m.labels() == labels)
    }

    /// Insert a series. Panics if a series with the same LabelSet
    /// already exists — spec §4.5 requires unique LabelSets within
    /// a family.
    pub fn insert(&mut self, metric: Metric) {
        assert!(
            !self.metrics.iter().any(|m| m.labels() == metric.labels()),
            "MetricFamily '{}' already contains a Metric with labels {:?} — LabelSets must be unique (OpenMetrics §4.5)",
            self.name,
            metric.labels(),
        );
        self.metrics.push(metric);
    }
}

/// OpenMetrics metric type per spec §4.4. Stored on every
/// [`MetricFamily`] and projected verbatim to exposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricType {
    Counter,
    Gauge,
    Histogram,
    GaugeHistogram,
    Summary,
    Info,
    StateSet,
    Unknown,
}

impl MetricType {
    /// The exposition-format token for this type (spec §4.4).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
            Self::GaugeHistogram => "gaugehistogram",
            Self::Summary => "summary",
            Self::Info => "info",
            Self::StateSet => "stateset",
            Self::Unknown => "unknown",
        }
    }
}

// =========================================================================
// Metric (OpenMetrics §4.5)
// =========================================================================

/// One labeled time series within a [`MetricFamily`]. Identity is
/// `(family.name, labels)` per spec §4.5.1.
///
/// Carries an ordered list of [`MetricPoint`]s — typically one in a
/// snapshot, but the spec permits multiple when several observations
/// belong to the same series.
#[derive(Clone, Debug)]
pub struct Metric {
    labels: Labels,
    points: Vec<MetricPoint>,
}

impl Metric {
    pub fn new(labels: Labels, points: Vec<MetricPoint>) -> Self {
        Self { labels, points }
    }

    /// Convenience for the common single-point case.
    pub fn single(labels: Labels, point: MetricPoint) -> Self {
        Self {
            labels,
            points: vec![point],
        }
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Iterator over the series' points.
    pub fn points(&self) -> impl Iterator<Item = &MetricPoint> {
        self.points.iter()
    }

    /// First point — convenience for the typical single-point case.
    /// Returns `None` if the series is empty (which violates the
    /// spec but is permitted at construction time so consumers
    /// don't have to handle Result).
    pub fn point(&self) -> Option<&MetricPoint> {
        self.points.first()
    }
}

// =========================================================================
// MetricPoint (OpenMetrics §4.6)
// =========================================================================

/// One observation for a [`Metric`], plus an optional timestamp.
/// OpenMetrics §4.6.
///
/// `timestamp` is **always populated** in nmbrs snapshots (the
/// cadence-window-close instant for cadence-window points, the live
/// read instant for `now` points, the merge instant for ephemeral
/// `increase_over` / `session_lifetime` points) — even though the
/// spec marks it optional.
#[derive(Clone, Debug)]
pub struct MetricPoint {
    value: MetricValue,
    timestamp: Option<Instant>,
}

impl MetricPoint {
    pub fn new(value: MetricValue, timestamp: Instant) -> Self {
        Self {
            value,
            timestamp: Some(timestamp),
        }
    }

    /// Construct without a timestamp — for cases (typically tests)
    /// where the timestamp is unknown or irrelevant. Production
    /// snapshots always set one.
    pub fn untimed(value: MetricValue) -> Self {
        Self {
            value,
            timestamp: None,
        }
    }

    pub fn value(&self) -> &MetricValue {
        &self.value
    }
    pub fn timestamp(&self) -> Option<Instant> {
        self.timestamp
    }
}

/// The typed value carried by a [`MetricPoint`]. Variants mirror
/// OpenMetrics §5.x. `Histogram` is the HDR-reservoir-backed
/// summary shape (percentile-based — semantically a Summary
/// per OpenMetrics §5.5). [`MetricValue::BucketedHistogram`]
/// is the explicit-`le`-bucket Histogram shape (§5.3) and
/// [`MetricType::GaugeHistogram`] (§5.4) reuses it under a
/// different family-type tag.
#[derive(Clone, Debug)]
pub enum MetricValue {
    /// OpenMetrics §5.1: monotonic counter.
    Counter(CounterValue),
    /// OpenMetrics §5.2: instantaneous gauge.
    Gauge(GaugeValue),
    /// OpenMetrics §5.5: φ-quantile summary backed by an
    /// HDR reservoir. (Note: the variant name is historical;
    /// per OpenMetrics taxonomy this shape is a Summary.)
    Histogram(HistogramValue),
    /// OpenMetrics §5.3 (Histogram) / §5.4 (GaugeHistogram):
    /// explicit cumulative `le`-keyed buckets. The owning
    /// [`MetricFamily::r#type()`] tag (`Histogram` vs
    /// `GaugeHistogram`) decides whether bucket counts are
    /// constrained monotonic — see `nmbrs-metrics::validation`.
    BucketedHistogram(BucketedHistogramValue),
    /// OpenMetrics §5.6: descriptive metadata. The label
    /// set carries the data; the value is conceptually 1.
    Info(InfoValue),
    /// OpenMetrics §5.7: named-state indicator set. Each
    /// state renders as its own Metric in exposition.
    StateSet(StateSetValue),
}

// ---- CounterValue (§5.1.1) ---------------------------------------------

/// OpenMetrics §5.1.1 counter point. Sample name carries the
/// spec-required `_total` suffix on exposition (not stored here).
#[derive(Clone, Debug)]
pub struct CounterValue {
    /// The counter's **cumulative** running total at this point's
    /// timestamp — the single canonical, Prometheus/VM-schematic
    /// monotonic value. Per-interval deltas are DERIVED by differencing
    /// consecutive samples (the metricsql engine, the sqlite `_rate`
    /// suffix, windowed-throughput readers), never stored. Time-coalesce
    /// keeps the latest (monotonic ⇒ window-end); cross-component
    /// aggregate sums it. See `docs/SRD/notes/cumulative_counter_model.md`.
    pub cumulative: u64,
    /// Series start time per spec §5.1; lets external consumers
    /// detect counter resets. Optional in spec.
    pub created: Option<Instant>,
    /// Optional exemplar per spec §4.6.1. At most one per
    /// `CounterValue`.
    pub exemplar: Option<Exemplar>,
}

impl CounterValue {
    /// A counter point at the given cumulative running total.
    pub fn new(cumulative: u64) -> Self {
        Self {
            cumulative,
            created: None,
            exemplar: None,
        }
    }

    pub fn with_created(mut self, t: Instant) -> Self {
        self.created = Some(t);
        self
    }

    pub fn with_exemplar(mut self, e: Exemplar) -> Self {
        self.exemplar = Some(e);
        self
    }
}

// ---- GaugeValue (§5.2.1) -----------------------------------------------

/// OpenMetrics §5.2.1 gauge point.
#[derive(Clone, Debug)]
pub struct GaugeValue {
    pub value: f64,
}

impl GaugeValue {
    pub fn new(value: f64) -> Self {
        Self { value }
    }
}

// ---- HistogramValue (§5.3.1) -------------------------------------------

/// OpenMetrics §5.3.1 histogram point. Internally carries the HDR
/// reservoir as the source of truth — OpenMetrics-shaped cumulative
/// buckets are derived on demand at exposition time.
///
/// `sum` / `count` are derivable from the reservoir but cached for
/// O(1) access. `created` is the series start time (component start)
/// per spec.
///
/// Per-bucket exemplars are NOT stored on the reservoir — they live
/// in [`HistogramValue::bucket_exemplars`] keyed by bucket index of
/// the consumer's eventual bucket layout. (Combine semantics:
/// most-recent-wins by `MetricPoint.timestamp`, per SRD-42
/// §"Exemplars → Combine semantics".)
#[derive(Clone, Debug)]
pub struct HistogramValue {
    /// HDR reservoir — the lossless source of truth for combining
    /// across cascade folds and ephemeral merges.
    pub reservoir: Arc<HdrHistogram<u64>>,
    /// Cached observation count. Equal to `reservoir.len()` — the
    /// per-window count in a delta snapshot.
    pub count: u64,
    /// The **cumulative** total observation count at this point's
    /// timestamp (monotonic, Prometheus/VM-schematic), carried
    /// alongside the per-window `count` exactly as `CounterValue`
    /// carries `cumulative` alongside `total`. The queryapi exposes
    /// this so MetricsQL `rate()`/`increase`/`*_over_time` over a
    /// histogram's count are PromQL-correct. Time-coalesce keeps the
    /// latest; cross-component aggregate sums it. (The percentile
    /// reservoir stays windowed — see the cumulative-counter note.)
    pub cumulative_count: u64,
    /// Cached observation sum (nanoseconds for latency timers).
    pub sum: f64,
    /// Series start time per spec §5.3; optional.
    pub created: Option<Instant>,
    /// Sampled exemplars, one per bucket of an eventual exposition
    /// layout. Sparse: an empty slot means "no exemplar for that
    /// bucket". See SRD-42 §"Exemplars".
    pub bucket_exemplars: Vec<Option<Exemplar>>,
}

impl HistogramValue {
    pub fn from_hdr(reservoir: HdrHistogram<u64>) -> Self {
        let count = reservoir.len();
        let sum = hdr_sum(&reservoir);
        Self {
            reservoir: Arc::new(reservoir),
            count,
            // Defaults to the per-window count (the value is both for a
            // one-shot snapshot); the cadence capture path overrides it
            // with the accumulated absolute via `with_cumulative_count`.
            cumulative_count: count,
            sum,
            created: None,
            bucket_exemplars: Vec::new(),
        }
    }

    /// Override the cumulative observation count (the cadence capture
    /// path sets the accumulated absolute total here).
    pub fn with_cumulative_count(mut self, cumulative_count: u64) -> Self {
        self.cumulative_count = cumulative_count;
        self
    }

    pub fn with_created(mut self, t: Instant) -> Self {
        self.created = Some(t);
        self
    }

    pub fn with_bucket_exemplars(mut self, exemplars: Vec<Option<Exemplar>>) -> Self {
        self.bucket_exemplars = exemplars;
        self
    }

    /// Project to OpenMetrics-shaped cumulative buckets at the given
    /// upper bounds. Per spec §5.3, the final bucket MUST have
    /// `upper_bound = +Inf`; this helper appends it automatically
    /// if not present.
    ///
    /// `bounds` should be sorted ascending. Returns `(upper_bound,
    /// cumulative_count)` pairs.
    pub fn project_buckets(&self, bounds: &[u64]) -> Vec<Bucket> {
        let mut out = Vec::with_capacity(bounds.len() + 1);
        for &le in bounds {
            let cumulative = self.reservoir.count_between(0, le);
            out.push(Bucket {
                upper_bound: BucketBound::Finite(le),
                cumulative_count: cumulative,
                exemplar: None,
            });
        }
        out.push(Bucket {
            upper_bound: BucketBound::PositiveInfinity,
            cumulative_count: self.count,
            exemplar: None,
        });
        out
    }
}

/// One cumulative bucket projected from a [`HistogramValue`] for
/// exposition. Per spec §5.3, the final bucket MUST have
/// `upper_bound = +Inf`.
#[derive(Clone, Debug)]
pub struct Bucket {
    pub upper_bound: BucketBound,
    pub cumulative_count: u64,
    pub exemplar: Option<Exemplar>,
}

/// Bucket upper bound — `+Inf` is the spec-required final bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketBound {
    Finite(u64),
    PositiveInfinity,
}

// ---- BucketedHistogramValue (§5.3 / §5.4) -------------------------------

/// OpenMetrics §5.3 (Histogram) and §5.4 (GaugeHistogram)
/// explicit-bucket value: cumulative observation counts at
/// producer-chosen `le` boundaries, plus optional
/// `_sum` / `_count` / `_created` siblings.
///
/// Histogram (§5.3) bucket counts are required to be
/// monotonically non-decreasing; GaugeHistogram (§5.4)
/// permits decreases. The owning [`MetricFamily::r#type()`]
/// tag distinguishes which constraint applies; the validation
/// helper in `crate::validation::check_bucket_monotonicity`
/// enforces it for `Histogram` families.
///
/// Difference from [`HistogramValue`]: that variant is the
/// HDR-reservoir-backed Summary (§5.5), exposing percentile
/// columns. This variant is the bucket-keyed form — what
/// OpenMetrics calls a Histogram strictly.
#[derive(Clone, Debug)]
pub struct BucketedHistogramValue {
    /// Cumulative `(le, count)` pairs in ascending `le`
    /// order. The final pair SHOULD have `le = +Inf` and
    /// `count == self.count` per spec §5.3; producers that
    /// omit `+Inf` are tolerated but exposition is
    /// permitted to synthesize one.
    pub buckets: Vec<(BucketBound, u64)>,
    /// Optional sum of all observations.
    pub sum: Option<f64>,
    /// Total observation count (== last bucket count) — the
    /// per-window total in a delta snapshot.
    pub count: u64,
    /// The **cumulative** total observation count (monotonic,
    /// Prometheus/VM-schematic) carried alongside the per-window
    /// `count`, mirroring [`HistogramValue::cumulative_count`] and
    /// `CounterValue::cumulative`. (The per-bucket `le` counts in
    /// `buckets` are the separate OpenMetrics bucket-cumulative
    /// thing and are unchanged.)
    pub cumulative_count: u64,
    /// Series start time per spec §5.3 / §5.4.
    pub created: Option<Instant>,
    /// Optional per-bucket exemplars, parallel to `buckets`.
    /// Sparse: missing slot ⇒ no exemplar for that bucket.
    pub bucket_exemplars: Vec<Option<Exemplar>>,
}

impl BucketedHistogramValue {
    /// Construct from cumulative `(le, count)` pairs. `count`
    /// is the final bucket's count when present, else the
    /// max across the supplied pairs.
    pub fn new(buckets: Vec<(BucketBound, u64)>) -> Self {
        let count = buckets.iter().map(|(_, c)| *c).max().unwrap_or(0);
        Self {
            buckets,
            sum: None,
            count,
            cumulative_count: count,
            created: None,
            bucket_exemplars: Vec::new(),
        }
    }

    /// Override the cumulative total observation count.
    pub fn with_cumulative_count(mut self, cumulative_count: u64) -> Self {
        self.cumulative_count = cumulative_count;
        self
    }

    pub fn with_sum(mut self, sum: f64) -> Self {
        self.sum = Some(sum);
        self
    }

    pub fn with_created(mut self, t: Instant) -> Self {
        self.created = Some(t);
        self
    }

    pub fn with_bucket_exemplars(mut self, ex: Vec<Option<Exemplar>>) -> Self {
        self.bucket_exemplars = ex;
        self
    }
}

// ---- InfoValue (§5.6) ---------------------------------------------------

/// OpenMetrics §5.6: descriptive info metric. The label set
/// of the owning [`Metric`] carries the data; the value is
/// conceptually always `1`. This struct has no payload —
/// the variant tag itself is the marker.
#[derive(Clone, Debug, Default)]
pub struct InfoValue;

impl InfoValue {
    pub fn new() -> Self {
        Self
    }
}

// ---- StateSetValue (§5.7) -----------------------------------------------

/// OpenMetrics §5.7: named-state indicator set. Each
/// `(state, active)` pair renders as a separate Metric in
/// exposition with the state name carried as a label.
///
/// Spec §5.7 requires that exactly one state in a StateSet
/// MAY be true at a time when the StateSet encodes an enum
/// (vs a free bitset). This struct doesn't enforce the
/// "exactly one" constraint — that's a producer-side
/// convention.
#[derive(Clone, Debug, Default)]
pub struct StateSetValue {
    /// Each entry is `(state_name, active_bool)`. State
    /// names are arbitrary strings (no ABNF restriction
    /// beyond label-value rules).
    pub states: Vec<(String, bool)>,
}

impl StateSetValue {
    pub fn new(states: Vec<(String, bool)>) -> Self {
        Self { states }
    }

    pub fn with_state(mut self, name: impl Into<String>, active: bool) -> Self {
        self.states.push((name.into(), active));
        self
    }
}

// =========================================================================
// Exemplar (OpenMetrics §4.6.1, §4.7)
// =========================================================================

/// OpenMetrics §4.6.1 exemplar: a labeled link from a metric
/// observation to an external context (typically a trace/span ID,
/// workload cycle number, or sample identifier).
///
/// Per spec §4.7 the serialized LabelSet MUST be ≤ 128 UTF-8
/// characters. Validation lives at exposition (a stored exemplar
/// that exceeds the limit is dropped from the wire; the recording
/// path is allowed to be permissive).
#[derive(Clone, Debug)]
pub struct Exemplar {
    pub labels: Labels,
    pub value: f64,
    pub timestamp: Option<Instant>,
}

impl Exemplar {
    pub fn new(labels: Labels, value: f64) -> Self {
        Self {
            labels,
            value,
            timestamp: None,
        }
    }

    pub fn with_timestamp(mut self, t: Instant) -> Self {
        self.timestamp = Some(t);
        self
    }
}

// =========================================================================
// Combine — algebraic uniformity (SRD-42 §"Combine semantics")
// =========================================================================

/// How two matching metric points combine. The counter `cumulative`
/// field is the only thing that differs (see the cumulative-counter
/// note): time-coalesce keeps the latest cumulative (monotonic), a
/// cross-component aggregate sums it. Everything else — counter `total`
/// (delta), gauges, histograms — is identical in both modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CombineMode {
    /// Consecutive windows of one series (the cadence cascade): counter
    /// `cumulative` = most-recent by timestamp.
    Coalesce,
    /// Same family+labels across components (cross-component totals):
    /// counter `cumulative` = sum.
    Aggregate,
}

/// In-place combine of `other` into `self`. Both must have the same
/// identity `(family.name, labels)` and the same value variant —
/// otherwise this is a hard error (panic) per the SRD's "matching
/// identity → matching combine" rule.
///
/// Combine rules per SRD-42 §"Combine semantics":
/// - Counter `total` (delta) sums in both modes; `cumulative` folds per
///   `mode` (latest on `Coalesce`, sum on `Aggregate`); `created` keeps
///   the earliest; exemplar most-recent-wins by `MetricPoint.timestamp`.
/// - Gauge values are last-write-wins (newer timestamp wins) — the
///   OpenMetrics/Prometheus gauge contract; summarization belongs at
///   the query point (`*_over_time`).
/// - Histogram reservoirs add via `HdrHistogram::add`; sum/count
///   re-derive; bucket exemplars most-recent-wins per index.
pub fn combine_into(
    dst: &mut MetricPoint,
    src: &MetricPoint,
    mode: CombineMode,
) -> Result<(), CombineError> {
    match (&mut dst.value, &src.value) {
        (MetricValue::Counter(a), MetricValue::Counter(b)) => {
            // A counter is its monotonic running total. Time-coalescing
            // consecutive windows of one series keeps the LATEST (the
            // window-end cumulative — never a sum); aggregating the same
            // series across components sums the running totals. Per-window
            // deltas are derived downstream by differencing samples.
            a.cumulative = match mode {
                CombineMode::Coalesce => {
                    let take_src = dst.timestamp.is_none()
                        || src
                            .timestamp
                            .map(|s| Some(s) >= dst.timestamp)
                            .unwrap_or(false);
                    if take_src { b.cumulative } else { a.cumulative }
                }
                CombineMode::Aggregate => a.cumulative.saturating_add(b.cumulative),
            };
            a.created = match (a.created, b.created) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
            a.exemplar = pick_more_recent_exemplar(
                a.exemplar.take(),
                b.exemplar.clone(),
                dst.timestamp,
                src.timestamp,
            );
        }
        (MetricValue::Gauge(a), MetricValue::Gauge(b)) => {
            // Most-recent-wins by timestamp; same-or-missing → src.
            if dst.timestamp.is_none()
                || src
                    .timestamp
                    .map(|s| Some(s) >= dst.timestamp)
                    .unwrap_or(false)
            {
                a.value = b.value;
            }
        }
        (MetricValue::Histogram(a), MetricValue::Histogram(b)) => {
            let merged = combine_hdr(&a.reservoir, &b.reservoir)?;
            a.count = merged.len();
            a.sum = hdr_sum(&merged);
            a.reservoir = Arc::new(merged);
            // `cumulative_count` (the monotonic lifetime count) follows the
            // counter rule: keep the latest when coalescing consecutive
            // windows of one series; sum when aggregating across components.
            a.cumulative_count = match mode {
                CombineMode::Coalesce => {
                    let take_src = dst.timestamp.is_none()
                        || src
                            .timestamp
                            .map(|s| Some(s) >= dst.timestamp)
                            .unwrap_or(false);
                    if take_src {
                        b.cumulative_count
                    } else {
                        a.cumulative_count
                    }
                }
                CombineMode::Aggregate => a.cumulative_count.saturating_add(b.cumulative_count),
            };
            a.created = match (a.created, b.created) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
            combine_bucket_exemplars(
                &mut a.bucket_exemplars,
                &b.bucket_exemplars,
                dst.timestamp,
                src.timestamp,
            );
        }
        (MetricValue::BucketedHistogram(a), MetricValue::BucketedHistogram(b)) => {
            // Bucket layouts must be compatible. If they
            // share boundaries we sum element-wise; mismatched
            // layouts are a type error (the producer is
            // responsible for emitting consistent buckets per
            // series).
            if a.buckets.len() != b.buckets.len()
                || a.buckets
                    .iter()
                    .zip(b.buckets.iter())
                    .any(|((la, _), (lb, _))| la != lb)
            {
                return Err(CombineError::TypeMismatch);
            }
            for (i, (_, count_b)) in b.buckets.iter().enumerate() {
                a.buckets[i].1 = a.buckets[i].1.saturating_add(*count_b);
            }
            a.count = a.count.saturating_add(b.count);
            // `cumulative_count` (the monotonic lifetime total) follows the
            // counter rule, like the HDR histogram above: latest when
            // coalescing one series' consecutive windows, summed when
            // aggregating across components. (The per-bucket `le` counts are
            // the separate OpenMetrics bucket-cumulative thing, summed above.)
            a.cumulative_count = match mode {
                CombineMode::Coalesce => {
                    let take_src = dst.timestamp.is_none()
                        || src
                            .timestamp
                            .map(|s| Some(s) >= dst.timestamp)
                            .unwrap_or(false);
                    if take_src {
                        b.cumulative_count
                    } else {
                        a.cumulative_count
                    }
                }
                CombineMode::Aggregate => a.cumulative_count.saturating_add(b.cumulative_count),
            };
            a.sum = match (a.sum, b.sum) {
                (Some(sa), Some(sb)) => Some(sa + sb),
                (Some(s), None) | (None, Some(s)) => Some(s),
                (None, None) => None,
            };
            a.created = match (a.created, b.created) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            };
            combine_bucket_exemplars(
                &mut a.bucket_exemplars,
                &b.bucket_exemplars,
                dst.timestamp,
                src.timestamp,
            );
        }
        (MetricValue::Info(_), MetricValue::Info(_)) => {
            // Info is always-1; combining is a no-op apart
            // from the timestamp update at the bottom.
        }
        (MetricValue::StateSet(a), MetricValue::StateSet(b)) => {
            // Most-recent-wins on state values: walk `b`'s
            // states and overwrite `a`'s entries by name,
            // appending unknown states.
            for (name, active) in &b.states {
                if let Some(slot) = a.states.iter_mut().find(|(n, _)| n == name) {
                    slot.1 = *active;
                } else {
                    a.states.push((name.clone(), *active));
                }
            }
        }
        _ => return Err(CombineError::TypeMismatch),
    }
    if let Some(src_ts) = src.timestamp {
        dst.timestamp = Some(match dst.timestamp {
            Some(d) if d >= src_ts => d,
            _ => src_ts,
        });
    }
    Ok(())
}

/// Combine two `HistogramValue` reservoirs into a new owned HDR
/// histogram. Used both by `combine_into` and by ephemeral
/// `increase_over` / `session_lifetime` queries that fold many
/// reservoirs without mutating any of them.
pub fn combine_hdr(
    a: &HdrHistogram<u64>,
    b: &HdrHistogram<u64>,
) -> Result<HdrHistogram<u64>, CombineError> {
    let mut out = a.clone();
    out.add(b).map_err(|_| CombineError::HdrAddFailed)?;
    Ok(out)
}

fn pick_more_recent_exemplar(
    a: Option<Exemplar>,
    b: Option<Exemplar>,
    a_ts: Option<Instant>,
    b_ts: Option<Instant>,
) -> Option<Exemplar> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(ax), Some(bx)) => {
            if b_ts.map(|s| Some(s) >= a_ts).unwrap_or(false) {
                Some(bx)
            } else {
                Some(ax)
            }
        }
    }
}

fn combine_bucket_exemplars(
    dst: &mut Vec<Option<Exemplar>>,
    src: &[Option<Exemplar>],
    dst_ts: Option<Instant>,
    src_ts: Option<Instant>,
) {
    if dst.len() < src.len() {
        dst.resize(src.len(), None);
    }
    for (i, src_ex) in src.iter().enumerate() {
        let dst_slot = dst[i].take();
        dst[i] = pick_more_recent_exemplar(dst_slot, src_ex.clone(), dst_ts, src_ts);
    }
}

/// Errors from [`combine_into`] / [`combine_hdr`].
#[derive(Debug, PartialEq, Eq)]
pub enum CombineError {
    /// The two `MetricPoint`s have different value variants
    /// (e.g., Counter vs Gauge). Indicates a programming error —
    /// only matching identity should ever combine.
    TypeMismatch,
    /// HDR `add` failed (typically because reservoirs have
    /// incompatible bounds).
    HdrAddFailed,
}

impl std::fmt::Display for CombineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeMismatch => write!(f, "MetricPoint value type mismatch"),
            Self::HdrAddFailed => write!(f, "HDR histogram add failed"),
        }
    }
}

impl std::error::Error for CombineError {}

// =========================================================================
// Quantiles
// =========================================================================

/// Standard quantiles reported for histogram samples by reporters
/// that emit a fixed quantile set (Prometheus summary-shape, CSV
/// percentile columns, etc.).
pub const QUANTILES: &[f64] = &[0.5, 0.75, 0.90, 0.95, 0.98, 0.99, 0.999];

// =========================================================================
// Migration helpers — extract `name` label, build single-point families
// =========================================================================

/// Split a [`Labels`] value into `(family_name, residual_labels)` by
/// extracting the `name` label. Producers that historically embedded
/// the metric name in `Labels` (the pre-snapshot pattern) use this
/// to feed [`MetricSet::insert_metric`].
///
/// Panics if `name` is missing — every metric MUST have a family
/// name per OpenMetrics §4.4.
pub fn split_name_label(labels: &Labels) -> (String, Labels) {
    let name = labels
        .get("name")
        .map(|s| s.to_string())
        .expect("every metric must have a 'name' label");
    let mut residual = Labels::default();
    for (k, v) in labels.iter() {
        if k != "name" {
            residual = residual.with(k, v);
        }
    }
    (name, residual)
}

impl MetricSet {
    /// Insert a single observation, looking up or creating its
    /// `MetricFamily` by name and appending one [`Metric`]/[`MetricPoint`]
    /// pair. Convenience for producers that build snapshots one
    /// observation at a time.
    ///
    /// Panics if the family already exists with a different
    /// [`MetricType`] (per identity rules) or if the LabelSet
    /// already exists within the family (spec §4.5).
    pub fn insert_metric(
        &mut self,
        family_name: impl Into<String>,
        family_type: MetricType,
        labels: Labels,
        value: MetricValue,
        timestamp: Instant,
    ) {
        let name = family_name.into();
        let point = MetricPoint::new(value, timestamp);
        if let Some(fam) = self.families.iter_mut().find(|f| f.name == name) {
            assert_eq!(
                fam.r#type, family_type,
                "family '{}' already exists as {:?}; cannot insert as {:?}",
                name, fam.r#type, family_type,
            );
            fam.insert(Metric::single(labels, point));
        } else {
            let mut fam = MetricFamily::new(name, family_type);
            fam.insert(Metric::single(labels, point));
            self.families.push(fam);
        }
    }

    /// Insert one observation, looking up or creating the
    /// [`MetricFamily`] by name with the given unit. When `unit`
    /// is set, the family name picks up the OpenMetrics
    /// `_<unit>` suffix at creation (per
    /// [`MetricFamily::with_unit`]) and the unit is also stored
    /// in the `unit` field. Subsequent inserts for the same bare
    /// `(family_name, unit)` pair find the same family by the
    /// suffixed name.
    ///
    /// Equivalent to [`insert_metric`] when `unit` is `None`.
    pub fn insert_metric_with_unit(
        &mut self,
        family_name: impl Into<String>,
        family_type: MetricType,
        unit: Option<&str>,
        labels: Labels,
        value: MetricValue,
        timestamp: Instant,
    ) {
        let bare = family_name.into();
        let template = MetricFamily::new(bare.clone(), family_type);
        let template = match unit {
            Some(u) => template.with_unit(u),
            None => template,
        };
        let effective_name = template.name().to_string();
        let point = MetricPoint::new(value, timestamp);
        if let Some(fam) = self.families.iter_mut().find(|f| f.name == effective_name) {
            assert_eq!(
                fam.r#type, family_type,
                "family '{}' already exists as {:?}; cannot insert as {:?}",
                effective_name, fam.r#type, family_type,
            );
            fam.insert(Metric::single(labels, point));
        } else {
            let mut fam = template;
            fam.insert(Metric::single(labels, point));
            self.families.push(fam);
        }
    }

    /// Insert a counter observation. Convenience over
    /// [`insert_metric`] that builds the [`CounterValue`] for you.
    pub fn insert_counter(
        &mut self,
        family_name: impl Into<String>,
        labels: Labels,
        cumulative: u64,
        timestamp: Instant,
    ) {
        self.insert_metric(
            family_name,
            MetricType::Counter,
            labels,
            MetricValue::Counter(CounterValue::new(cumulative)),
            timestamp,
        );
    }

    /// Counter variant of [`insert_metric_with_unit`]. `cumulative` is the
    /// counter's running total (the cadence capture path passes the
    /// instrument's absolute); per-interval deltas are derived downstream.
    pub fn insert_counter_with_unit(
        &mut self,
        family_name: impl Into<String>,
        unit: Option<&str>,
        labels: Labels,
        cumulative: u64,
        timestamp: Instant,
    ) {
        self.insert_metric_with_unit(
            family_name,
            MetricType::Counter,
            unit,
            labels,
            MetricValue::Counter(CounterValue::new(cumulative)),
            timestamp,
        );
    }

    /// Insert a gauge observation. Convenience over [`insert_metric`].
    pub fn insert_gauge(
        &mut self,
        family_name: impl Into<String>,
        labels: Labels,
        value: f64,
        timestamp: Instant,
    ) {
        self.insert_metric(
            family_name,
            MetricType::Gauge,
            labels,
            MetricValue::Gauge(GaugeValue::new(value)),
            timestamp,
        );
    }

    /// Gauge variant of [`insert_metric_with_unit`].
    pub fn insert_gauge_with_unit(
        &mut self,
        family_name: impl Into<String>,
        unit: Option<&str>,
        labels: Labels,
        value: f64,
        timestamp: Instant,
    ) {
        self.insert_metric_with_unit(
            family_name,
            MetricType::Gauge,
            unit,
            labels,
            MetricValue::Gauge(GaugeValue::new(value)),
            timestamp,
        );
    }

    /// Insert a histogram observation. Convenience over
    /// [`insert_metric`] that wraps the HDR reservoir into a
    /// [`HistogramValue`] and computes `count`/`sum` from it.
    pub fn insert_histogram(
        &mut self,
        family_name: impl Into<String>,
        labels: Labels,
        reservoir: HdrHistogram<u64>,
        timestamp: Instant,
    ) {
        self.insert_metric(
            family_name,
            MetricType::Histogram,
            labels,
            MetricValue::Histogram(HistogramValue::from_hdr(reservoir)),
            timestamp,
        );
    }

    /// Histogram variant of [`insert_metric_with_unit`].
    pub fn insert_histogram_with_unit(
        &mut self,
        family_name: impl Into<String>,
        unit: Option<&str>,
        labels: Labels,
        reservoir: HdrHistogram<u64>,
        timestamp: Instant,
    ) {
        self.insert_metric_with_unit(
            family_name,
            MetricType::Histogram,
            unit,
            labels,
            MetricValue::Histogram(HistogramValue::from_hdr(reservoir)),
            timestamp,
        );
    }

    /// Like [`insert_histogram_with_unit`] but stamps the **cumulative**
    /// observation count (the instrument's lifetime total) onto the
    /// value, so `HistogramValue::cumulative_count` is the authoritative
    /// running total — like the counter's `cumulative`. The cadence
    /// capture path uses this; the queryapi then exposes a cumulative
    /// histogram count over which `rate()` is PromQL-correct.
    pub fn insert_histogram_with_unit_cumulative(
        &mut self,
        family_name: impl Into<String>,
        unit: Option<&str>,
        labels: Labels,
        reservoir: HdrHistogram<u64>,
        cumulative_count: u64,
        timestamp: Instant,
    ) {
        self.insert_metric_with_unit(
            family_name,
            MetricType::Histogram,
            unit,
            labels,
            MetricValue::Histogram(
                HistogramValue::from_hdr(reservoir).with_cumulative_count(cumulative_count),
            ),
            timestamp,
        );
    }
}

// =========================================================================
// Helpers
// =========================================================================

/// Approximate observation sum from an HDR histogram. HDR doesn't
/// store a true sum — we estimate by `mean × count`. Sufficient for
/// OpenMetrics `_sum` exposition; consumers who need an exact sum
/// have to record it independently.
fn hdr_sum(h: &HdrHistogram<u64>) -> f64 {
    h.mean() * h.len() as f64
}

/// Convenience: build a single-point Counter family.
pub fn counter_family(
    name: impl Into<String>,
    labels: Labels,
    total: u64,
    timestamp: Instant,
) -> MetricFamily {
    let mut f = MetricFamily::new(name, MetricType::Counter);
    f.insert(Metric::single(
        labels,
        MetricPoint::new(MetricValue::Counter(CounterValue::new(total)), timestamp),
    ));
    f
}

/// Convenience: build a single-point Gauge family.
pub fn gauge_family(
    name: impl Into<String>,
    labels: Labels,
    value: f64,
    timestamp: Instant,
) -> MetricFamily {
    let mut f = MetricFamily::new(name, MetricType::Gauge);
    f.insert(Metric::single(
        labels,
        MetricPoint::new(MetricValue::Gauge(GaugeValue::new(value)), timestamp),
    ));
    f
}

/// Convenience: build a single-point Histogram family from an HDR
/// reservoir.
pub fn histogram_family(
    name: impl Into<String>,
    labels: Labels,
    reservoir: HdrHistogram<u64>,
    timestamp: Instant,
) -> MetricFamily {
    let mut f = MetricFamily::new(name, MetricType::Histogram);
    f.insert(Metric::single(
        labels,
        MetricPoint::new(
            MetricValue::Histogram(HistogramValue::from_hdr(reservoir)),
            timestamp,
        ),
    ));
    f
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn ts() -> Instant {
        Instant::now()
    }
    fn empty_set() -> MetricSet {
        MetricSet::new(Duration::from_secs(1))
    }

    #[test]
    fn metric_set_is_empty_by_default() {
        let m = empty_set();
        assert_eq!(m.len(), 0);
        assert!(m.is_empty());
        assert!(m.family("anything").is_none());
        assert_eq!(m.interval(), Duration::from_secs(1));
    }

    #[test]
    fn metric_set_inserts_and_looks_up_by_name() {
        let mut m = empty_set();
        m.insert(counter_family(
            "cycles",
            Labels::of("phase", "load"),
            100,
            ts(),
        ));
        m.insert(gauge_family(
            "temp",
            Labels::of("phase", "load"),
            42.0,
            ts(),
        ));

        assert_eq!(m.len(), 2);
        assert!(m.family("cycles").is_some());
        assert!(m.family("temp").is_some());
        assert!(m.family("missing").is_none());
    }

    #[test]
    #[should_panic(expected = "names must be unique")]
    fn metric_set_rejects_duplicate_family_names() {
        let mut m = empty_set();
        m.insert(counter_family("cycles", Labels::of("a", "1"), 1, ts()));
        m.insert(counter_family("cycles", Labels::of("a", "2"), 2, ts()));
    }

    fn make_counter_set(interval: Duration, value: u64) -> MetricSet {
        let mut s = MetricSet::new(interval);
        s.insert(counter_family(
            "cycles",
            Labels::of("name", "ops"),
            value,
            Instant::now(),
        ));
        s
    }

    fn make_histogram_set(interval: Duration, values: &[u64]) -> MetricSet {
        let mut h = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        for v in values {
            h.record(*v).unwrap();
        }
        let mut s = MetricSet::new(interval);
        s.insert(histogram_family(
            "latency",
            Labels::of("name", "rt"),
            h,
            Instant::now(),
        ));
        s
    }

    fn make_gauge_set(interval: Duration, value: f64) -> MetricSet {
        let mut s = MetricSet::new(interval);
        s.insert(gauge_family(
            "temp",
            Labels::of("name", "x"),
            value,
            Instant::now(),
        ));
        s
    }

    #[test]
    fn coalesce_empty_returns_empty() {
        let merged = MetricSet::coalesce(&[]);
        assert!(merged.is_empty());
    }

    #[test]
    fn coalesce_single_clones() {
        let s = make_counter_set(Duration::from_secs(1), 10);
        let m = MetricSet::coalesce(std::slice::from_ref(&s));
        assert_eq!(m.interval(), Duration::from_secs(1));
        let f = m.family("cycles").unwrap();
        let c = match f.metrics().next().unwrap().point().unwrap().value() {
            MetricValue::Counter(c) => c.cumulative,
            _ => panic!("wrong type"),
        };
        assert_eq!(c, 10);
    }

    #[test]
    fn coalesce_counters_keep_latest_cumulative_sum_intervals() {
        // Time-coalesce of one series keeps the latest (window-end)
        // cumulative — never a sum — and sums the intervals. (Monotonic
        // cumulatives in window order; the last one wins.)
        let merged = MetricSet::coalesce(&[
            make_counter_set(Duration::from_secs(1), 10),
            make_counter_set(Duration::from_secs(1), 25),
            make_counter_set(Duration::from_secs(1), 42),
        ]);
        assert_eq!(merged.interval(), Duration::from_secs(3));
        let cumulative = match merged
            .family("cycles")
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
        assert_eq!(cumulative, 42, "latest window's cumulative, not the sum");
    }

    #[test]
    fn coalesce_histograms_merge_reservoirs() {
        let merged = MetricSet::coalesce(&[
            make_histogram_set(Duration::from_secs(1), &[1_000, 2_000, 3_000]),
            make_histogram_set(Duration::from_secs(1), &[4_000, 5_000]),
        ]);
        let hv = match merged
            .family("latency")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value()
        {
            MetricValue::Histogram(h) => h.clone(),
            _ => panic!("wrong type"),
        };
        assert_eq!(hv.count, 5);
        assert!(hv.reservoir.max() >= 4_900);
    }

    #[test]
    fn coalesce_gauges_last_write_wins() {
        // Two snapshots: (1s @ 10.0) then (2s @ 20.0). The coalesced
        // window's sample is the LAST-WRITTEN value — the
        // OpenMetrics/Prometheus/OTel gauge contract: a sample is the
        // scalar as of window end; summarization happens at the query
        // point via *_over_time. (This replaced interval-weighted
        // averaging, which stored 16.67 here — a value never written —
        // and turned set-once facts stored as gauges into fractions,
        // 2026-08-08.)
        let merged = MetricSet::coalesce(&[
            make_gauge_set(Duration::from_secs(1), 10.0),
            make_gauge_set(Duration::from_secs(2), 20.0),
        ]);
        let v = match merged
            .family("temp")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value()
        {
            MetricValue::Gauge(g) => g.value,
            _ => panic!("wrong type"),
        };
        assert_eq!(v, 20.0, "last written value wins, got {v}");
    }

    #[test]
    fn coalesce_disjoint_label_sets_appended() {
        // Same family name, different LabelSets — should NOT combine.
        let mut a = MetricSet::new(Duration::from_secs(1));
        a.insert(counter_family(
            "cycles",
            Labels::of("phase", "load"),
            100,
            ts(),
        ));
        let mut b = MetricSet::new(Duration::from_secs(1));
        b.insert(counter_family(
            "cycles",
            Labels::of("phase", "verify"),
            50,
            ts(),
        ));

        let merged = MetricSet::coalesce(&[a, b]);
        let f = merged.family("cycles").unwrap();
        assert_eq!(f.len(), 2);
        let load = f.metric_with_labels(&Labels::of("phase", "load")).unwrap();
        let verify = f
            .metric_with_labels(&Labels::of("phase", "verify"))
            .unwrap();
        match load.point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 100),
            _ => panic!(),
        }
        match verify.point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 50),
            _ => panic!(),
        }
    }

    #[test]
    fn metric_family_records_type_and_optional_metadata() {
        // SRD-40b §1 / SRD-40a §4.3: `with_unit` appends the
        // `_<unit>` suffix to the family name when the invariant
        // is not already met. Both surfaces (name + unit) derive
        // from this single declaration.
        let f = MetricFamily::new("latency", MetricType::Histogram)
            .with_unit("nanoseconds")
            .with_help("End-to-end op latency");
        assert_eq!(f.name(), "latency_nanoseconds");
        assert_eq!(f.r#type(), MetricType::Histogram);
        assert_eq!(f.unit(), Some("nanoseconds"));
        assert_eq!(f.help(), Some("End-to-end op latency"));
    }

    #[test]
    fn with_unit_preserves_name_when_suffix_already_present() {
        // No double-suffixing: if the caller already wrote the
        // canonical name, `with_unit` is a no-op for the name.
        let f = MetricFamily::new("memory_bytes", MetricType::Gauge).with_unit("bytes");
        assert_eq!(f.name(), "memory_bytes");
        assert_eq!(f.unit(), Some("bytes"));
    }

    #[test]
    fn with_unit_preserves_name_when_unit_precedes_exposition_suffix() {
        // OpenMetrics §4.4 / §5.x: the unit may sit before a
        // known exposition suffix (e.g. `_total`).
        let f = MetricFamily::new("process_cpu_seconds_total", MetricType::Counter)
            .with_unit("seconds");
        assert_eq!(f.name(), "process_cpu_seconds_total");
        assert_eq!(f.unit(), Some("seconds"));
    }

    #[test]
    #[should_panic(expected = "LabelSets must be unique")]
    fn metric_family_rejects_duplicate_labelsets() {
        let mut f = MetricFamily::new("cycles", MetricType::Counter);
        f.insert(Metric::single(
            Labels::of("phase", "load"),
            MetricPoint::untimed(MetricValue::Counter(CounterValue::new(1))),
        ));
        f.insert(Metric::single(
            Labels::of("phase", "load"),
            MetricPoint::untimed(MetricValue::Counter(CounterValue::new(2))),
        ));
    }

    #[test]
    fn metric_lookup_by_labels_matches_identity() {
        let mut f = MetricFamily::new("cycles", MetricType::Counter);
        f.insert(Metric::single(
            Labels::of("phase", "load"),
            MetricPoint::untimed(MetricValue::Counter(CounterValue::new(10))),
        ));
        f.insert(Metric::single(
            Labels::of("phase", "verify"),
            MetricPoint::untimed(MetricValue::Counter(CounterValue::new(20))),
        ));

        let load = f.metric_with_labels(&Labels::of("phase", "load")).unwrap();
        match load.point().unwrap().value() {
            MetricValue::Counter(c) => assert_eq!(c.cumulative, 10),
            _ => panic!("wrong type"),
        }
        assert!(
            f.metric_with_labels(&Labels::of("phase", "missing"))
                .is_none()
        );
    }

    #[test]
    fn metric_type_strings_match_open_metrics_spec() {
        assert_eq!(MetricType::Counter.as_str(), "counter");
        assert_eq!(MetricType::Gauge.as_str(), "gauge");
        assert_eq!(MetricType::Histogram.as_str(), "histogram");
        assert_eq!(MetricType::GaugeHistogram.as_str(), "gaugehistogram");
        assert_eq!(MetricType::Summary.as_str(), "summary");
        assert_eq!(MetricType::Info.as_str(), "info");
        assert_eq!(MetricType::StateSet.as_str(), "stateset");
        assert_eq!(MetricType::Unknown.as_str(), "unknown");
    }

    #[test]
    fn counter_aggregate_sums_cumulative_keeps_earlier_created() {
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
        // Aggregate (cross-component): the running totals sum across the
        // matching series from different components.
        combine_into(&mut a, &b, CombineMode::Aggregate).unwrap();
        match a.value() {
            MetricValue::Counter(c) => {
                assert_eq!(
                    c.cumulative, 35,
                    "aggregate sums cumulative across components"
                );
                assert_eq!(c.created, Some(t0), "earliest created wins");
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn counter_coalesce_keeps_latest_cumulative() {
        let t1 = Instant::now();
        let t2 = t1 + Duration::from_secs(1);
        // Two consecutive windows of ONE series: cumulative 100 then 113.
        // Time-coalesce keeps the latest (window-end) running total — never
        // a sum. Per-window deltas are derived downstream by differencing.
        let mut a = MetricPoint::new(MetricValue::Counter(CounterValue::new(100)), t1);
        let b = MetricPoint::new(MetricValue::Counter(CounterValue::new(113)), t2);
        combine_into(&mut a, &b, CombineMode::Coalesce).unwrap();
        match a.value() {
            MetricValue::Counter(c) => assert_eq!(
                c.cumulative, 113,
                "coalesce keeps the latest cumulative (no summing)"
            ),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn histogram_combine_adds_reservoirs_re_derives_count_and_sum() {
        let mut h1 = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        h1.record(1_000_000).unwrap();
        h1.record(2_000_000).unwrap();
        let mut h2 = HdrHistogram::<u64>::new_with_bounds(1, 3_600_000_000_000, 3).unwrap();
        h2.record(3_000_000).unwrap();

        let mut a = MetricPoint::new(
            MetricValue::Histogram(HistogramValue::from_hdr(h1)),
            Instant::now(),
        );
        let b = MetricPoint::new(
            MetricValue::Histogram(HistogramValue::from_hdr(h2)),
            Instant::now(),
        );
        combine_into(&mut a, &b, CombineMode::Coalesce).unwrap();
        match a.value() {
            MetricValue::Histogram(h) => {
                assert_eq!(h.count, 3);
                assert!(h.sum > 0.0);
                assert!(h.reservoir.max() >= 3_000_000);
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn gauge_combine_keeps_most_recent_value() {
        let t1 = Instant::now();
        let t2 = t1 + Duration::from_secs(1);
        let mut a = MetricPoint::new(MetricValue::Gauge(GaugeValue::new(5.0)), t1);
        let b = MetricPoint::new(MetricValue::Gauge(GaugeValue::new(9.0)), t2);
        combine_into(&mut a, &b, CombineMode::Coalesce).unwrap();
        match a.value() {
            MetricValue::Gauge(g) => assert_eq!(g.value, 9.0),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn combine_type_mismatch_is_hard_error() {
        let mut a = MetricPoint::untimed(MetricValue::Counter(CounterValue::new(1)));
        let b = MetricPoint::untimed(MetricValue::Gauge(GaugeValue::new(1.0)));
        let err = combine_into(&mut a, &b, CombineMode::Coalesce).unwrap_err();
        assert_eq!(err, CombineError::TypeMismatch);
    }

    #[test]
    fn exemplar_most_recent_wins_on_combine() {
        let t1 = Instant::now();
        let t2 = t1 + Duration::from_secs(1);
        let ex_old = Exemplar::new(Labels::of("trace_id", "old"), 1.0).with_timestamp(t1);
        let ex_new = Exemplar::new(Labels::of("trace_id", "new"), 2.0).with_timestamp(t2);

        let mut a = MetricPoint::new(
            MetricValue::Counter(CounterValue::new(5).with_exemplar(ex_old)),
            t1,
        );
        let b = MetricPoint::new(
            MetricValue::Counter(CounterValue::new(5).with_exemplar(ex_new.clone())),
            t2,
        );
        combine_into(&mut a, &b, CombineMode::Coalesce).unwrap();
        match a.value() {
            MetricValue::Counter(c) => {
                let e = c.exemplar.as_ref().expect("exemplar should survive");
                assert_eq!(e.labels.get("trace_id"), Some("new"));
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn histogram_projects_to_open_metrics_buckets() {
        let mut h = HdrHistogram::<u64>::new_with_bounds(1, 1_000_000, 3).unwrap();
        for v in [10u64, 50, 100, 500, 1000, 5000, 50_000].iter() {
            h.record(*v).unwrap();
        }
        let hv = HistogramValue::from_hdr(h);

        let buckets = hv.project_buckets(&[100, 1000, 10_000]);
        // 3 finite + 1 +Inf = 4 total
        assert_eq!(buckets.len(), 4);
        assert_eq!(buckets[0].upper_bound, BucketBound::Finite(100));
        assert_eq!(buckets[3].upper_bound, BucketBound::PositiveInfinity);

        // Cumulative: ≤100 should include 10, 50, 100 — count ≥ 3
        assert!(buckets[0].cumulative_count >= 3);
        // Final +Inf bucket equals total count
        assert_eq!(buckets[3].cumulative_count, hv.count);
        // Cumulative is monotonically non-decreasing
        for w in buckets.windows(2) {
            assert!(w[0].cumulative_count <= w[1].cumulative_count);
        }
    }

    #[test]
    fn metric_point_timestamp_propagates_on_construction() {
        let now = Instant::now();
        let p = MetricPoint::new(MetricValue::Gauge(GaugeValue::new(1.0)), now);
        assert_eq!(p.timestamp(), Some(now));

        let untimed = MetricPoint::untimed(MetricValue::Gauge(GaugeValue::new(1.0)));
        assert_eq!(untimed.timestamp(), None);
    }
}

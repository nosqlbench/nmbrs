// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Catalog: enumerable backend introspection for metrics.
//!
//! [`MetricAccess`](super::MetricAccess) is for **fetching
//! values** given a selector. This module is for **enumerating
//! what's available**: which metric families exist, what
//! labels they carry, what values those labels take. The split
//! mirrors VictoriaMetrics' own surface (`/api/v1/query` and
//! `/api/v1/labels` endpoints); a remote backend implements
//! both traits against the same wire protocol.
//!
//! ## Why not put enumeration on `DataSource`?
//!
//! Two reasons:
//!
//! 1. **Separable lifetimes.** Catalog data is small (≤ a few
//!    thousand entries even on big sessions) and slow-changing.
//!    Sample data is large and time-windowed. Cache strategies
//!    diverge — see [`CachedCatalog`].
//! 2. **Implementation effort.** A backend may serve catalog
//!    enumeration from cheaply-aggregable indices but require
//!    significant query work for sample fetching. Letting an
//!    impl provide one without the other is the right composition.
//!
//! ## OpenMetrics fidelity
//!
//! [`MetricFamilyMeta`] mirrors the OpenMetrics 1.0 metadata
//! envelope: name + type + unit + help. [`MetricType`]
//! enumerates every type the OpenMetrics specification
//! recognises; backends that use a smaller subset map their
//! types into [`MetricType::Unknown`] when no faithful match
//! exists.
//!
//! ## Autocompletion building block
//!
//! `nmbrs::completion` consumes this trait via the
//! [`MetricCatalog`] object trait to surface metric / label
//! suggestions for `--metric`, `--over`, `--by`, `--where`,
//! and (planned) for autocompletion *inside metricsql
//! expressions*. The four enumeration methods cover everything
//! a metricsql-aware completion engine needs:
//!
//! | What user types | Catalog method called |
//! |---|---|
//! | bare metric name | [`MetricCatalog::metric_families`] |
//! | inside `{...}`, key | [`MetricCatalog::label_keys`] |
//! | inside `{...}`, value | [`MetricCatalog::label_values`] |
//! | series-existence check | [`MetricCatalog::series`] |

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{MatchOp as MatcherOp, Matcher, QueryError as DataSourceError};

/// One metric family's OpenMetrics metadata. Identifies the
/// family by name and surfaces its type / unit / help.
///
/// The OpenMetrics envelope:
///
/// ```text
/// # TYPE process_cpu_seconds_total counter
/// # UNIT process_cpu_seconds_total seconds
/// # HELP process_cpu_seconds_total Total user and system CPU time spent in seconds.
/// ```
///
/// becomes:
///
/// ```rust,ignore
/// MetricFamilyMeta {
///     name: "process_cpu_seconds_total".into(),
///     ty: MetricType::Counter,
///     unit: Some("seconds".into()),
///     help: Some("Total user and system CPU time spent in seconds.".into()),
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricFamilyMeta {
    /// Family name (e.g. `http_requests_total`,
    /// `nmbrs_phase_duration_seconds`). For histograms /
    /// summaries this is the **family** name; the time-series
    /// names emitted on the wire (`<name>_bucket`,
    /// `<name>_sum`, `<name>_count`) are derived siblings, not
    /// distinct families.
    pub name: String,
    /// Metric type per OpenMetrics 1.0.
    pub ty: MetricType,
    /// Optional unit (`seconds`, `bytes`, etc.). Used by
    /// completion to surface a hint on the metric name, and
    /// by the metricsql evaluator to validate
    /// type-incompatible binary ops.
    pub unit: Option<String>,
    /// Optional help string, lifted verbatim from the
    /// OpenMetrics `# HELP` metadata.
    pub help: Option<String>,
}

/// Metric types per the OpenMetrics 1.0 specification.
///
/// Implementations whose underlying schema only knows the
/// Prometheus subset (counter / gauge / histogram / summary /
/// untyped) map their types into the corresponding variant
/// here; the OpenMetrics 1.0 additions ([`Self::GaugeHistogram`],
/// [`Self::Info`], [`Self::StateSet`]) become [`Self::Unknown`]
/// in those backends.
///
/// The string form (`as_str` / `parse`) matches the
/// OpenMetrics text-format keyword exactly. Round-trip
/// parse/format is identity for every named variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricType {
    /// Monotonically-increasing counter; resets on process
    /// restart. `rate()` and `increase()` operate on this.
    Counter,
    /// Instantaneous reading that can rise or fall.
    Gauge,
    /// Cumulative bucketed histogram with `_bucket{le=...}`,
    /// `_sum`, `_count` siblings.
    Histogram,
    /// OpenMetrics 1.0: histogram whose buckets can decrease.
    GaugeHistogram,
    /// φ-quantile summary with `{quantile=...}` siblings plus
    /// `_sum` and `_count`.
    Summary,
    /// Always-1 metric whose label set carries descriptive
    /// info. Joins onto other metrics via `* on (labels)`.
    Info,
    /// Set of named states represented as 0/1 indicators.
    /// OpenMetrics 1.0.
    StateSet,
    /// Type unspecified or unmappable — Prometheus's "untyped".
    Unknown,
}

impl MetricType {
    /// Canonical lower-case keyword (matches OpenMetrics
    /// text format).
    pub fn as_str(&self) -> &'static str {
        match self {
            MetricType::Counter => "counter",
            MetricType::Gauge => "gauge",
            MetricType::Histogram => "histogram",
            MetricType::GaugeHistogram => "gaugehistogram",
            MetricType::Summary => "summary",
            MetricType::Info => "info",
            MetricType::StateSet => "stateset",
            MetricType::Unknown => "unknown",
        }
    }

    /// Parse an OpenMetrics-style type keyword. Unknown
    /// keywords map to [`Self::Unknown`] rather than failing —
    /// a metric whose backend uses a non-standard type stays
    /// queryable (the metricsql evaluator treats it as a
    /// generic series) instead of disappearing.
    pub fn parse(s: &str) -> MetricType {
        match s.trim().to_ascii_lowercase().as_str() {
            "counter" => MetricType::Counter,
            "gauge" => MetricType::Gauge,
            "histogram" => MetricType::Histogram,
            "gaugehistogram" => MetricType::GaugeHistogram,
            "summary" => MetricType::Summary,
            "info" => MetricType::Info,
            "stateset" => MetricType::StateSet,
            _ => MetricType::Unknown,
        }
    }

    /// True if this metric type implies derived time-series
    /// names with the family-name prefix. For histogram +
    /// summary the catalog's enumeration of *time series* will
    /// surface `<name>_bucket`, `<name>_sum`, `<name>_count`
    /// (and for summary: the family name itself). Selectors
    /// can target any of these directly. Used by completion
    /// to expand a family-name suggestion into its OpenMetrics
    /// siblings when the user asks.
    pub fn has_derived_series(&self) -> bool {
        matches!(
            self,
            MetricType::Histogram | MetricType::GaugeHistogram | MetricType::Summary,
        )
    }

    /// The label key OpenMetrics implies for this type's
    /// derived series (`le` for histograms, `quantile` for
    /// summaries). Returns `None` for types without an implied
    /// label.
    pub fn implied_label(&self) -> Option<&'static str> {
        match self {
            MetricType::Histogram | MetricType::GaugeHistogram => Some("le"),
            MetricType::Summary => Some("quantile"),
            _ => None,
        }
    }
}

/// One series identifier — the label set that distinguishes a
/// time series within its metric family. Returned by
/// [`MetricCatalog::series`]. Pairs are in declaration order
/// the backend chooses; consumers that need a stable order
/// sort themselves.
pub type LabelSet = Vec<(String, String)>;

/// One exemplar per OpenMetrics §4.6.1. Anchored to a
/// specific sample observation by the (series identity +
/// sample timestamp) pair, with its own value, optional
/// timestamp, and label set (trace ids, span ids, …).
///
/// Returned by [`MetricCatalog::exemplars`].
#[derive(Debug, Clone, PartialEq)]
pub struct ExemplarPoint {
    /// The series this exemplar attaches to. Includes the
    /// synthetic `__name__` label so callers can rebuild
    /// the full selector.
    pub series: LabelSet,
    /// Timestamp of the sample observation the exemplar
    /// pairs with. The catalog reader's join key.
    pub sample_timestamp_ms: i64,
    /// The exemplar's observed value — typically the raw
    /// sample value the exemplar describes (the bucket
    /// boundary that captured it for histograms; the
    /// counter increment value for counters).
    pub value: f64,
    /// Optional exemplar timestamp, distinct from
    /// `sample_timestamp_ms`. Spec §4.6.1 leaves this
    /// optional so producers that don't track timestamps
    /// can omit it.
    pub timestamp_ms: Option<i64>,
    /// Exemplar's own labels — trace context, span ids,
    /// arbitrary diagnostic envelope.
    pub labels: Vec<(String, String)>,
}

/// Backend catalog trait.
///
/// All four methods may be called concurrently from multiple
/// threads. Implementations either provide their own
/// concurrency story (mutex, connection pool) or compose with
/// [`CachedCatalog`] which serialises through a single
/// per-key lock.
///
/// Errors surface via [`DataSourceError`] — same channel
/// [`super::MetricAccess`] uses, so callers that already
/// handle one can handle the other uniformly.
pub trait MetricCatalog: Send + Sync {
    /// Every metric family known to the backend, with its
    /// OpenMetrics metadata. Order is backend-chosen
    /// (sqlite-backed impls return alphabetical; remote
    /// backends typically don't promise an order).
    fn metric_families(&self) -> Result<Vec<MetricFamilyMeta>, DataSourceError>;

    /// Distinct label keys observed across the catalog,
    /// optionally restricted to one metric family by exact
    /// name. `family_filter = None` returns every key the
    /// backend has seen anywhere.
    ///
    /// Excludes synthetic keys: `__name__` is implicit
    /// (it's the family name) and is never returned.
    fn label_keys(&self, family_filter: Option<&str>) -> Result<Vec<String>, DataSourceError>;

    /// Distinct values observed for `key`, optionally
    /// restricted to one metric family. Returns an empty list
    /// if the key has no values observed (or doesn't exist) —
    /// no error, since "no values yet" is a normal state for
    /// a fresh session.
    ///
    /// For implied labels (`le` on histograms, `quantile` on
    /// summaries) the values are the bucket boundaries /
    /// quantile φ values, in the canonical numeric order the
    /// underlying schema records.
    fn label_values(
        &self,
        key: &str,
        family_filter: Option<&str>,
    ) -> Result<Vec<String>, DataSourceError>;

    /// Distinct label sets in the catalog matching every
    /// [`Matcher`]. The result is the *series identity*
    /// surface — one entry per `(family + label set)` tuple
    /// that satisfies the selector. Used by callers that need
    /// to know "which concrete series exist that satisfy this
    /// shape" without fetching their samples.
    ///
    /// Each returned [`LabelSet`] **includes** the synthetic
    /// `__name__` label so the caller can reconstruct the
    /// full selector. Backends that natively store
    /// per-instance labels project the family name on the
    /// way out.
    ///
    /// Empty matcher list returns every series. Backends MAY
    /// reject excessively wide queries with
    /// [`DataSourceError`] — local sqlite tolerates it,
    /// remote backends often don't.
    fn series(&self, matchers: &[Matcher]) -> Result<Vec<LabelSet>, DataSourceError>;

    /// OpenMetrics §4.6.1 exemplars matching `matchers`,
    /// optionally restricted to a `[start_ms, end_ms]`
    /// window on `sample_timestamp_ms`. Returns one
    /// [`ExemplarPoint`] per stored exemplar; series with
    /// no exemplars contribute nothing.
    ///
    /// **Default implementation returns empty.** Backends
    /// without exemplar storage (e.g. a remote VM endpoint
    /// that doesn't expose them) leave this as the default
    /// and don't have to fail. Backends that DO store them
    /// override the method.
    fn exemplars(
        &self,
        matchers: &[Matcher],
        time_range: Option<(i64, i64)>,
    ) -> Result<Vec<ExemplarPoint>, DataSourceError> {
        let _ = (matchers, time_range);
        Ok(Vec::new())
    }
}

// =====================================================================
// Caching wrapper
// =====================================================================

/// Cache layer for any [`MetricCatalog`] implementation.
///
/// Catalog data is small and slow-changing (per-session: it
/// can only **grow**, never shrink). The cache absorbs the
/// repeated queries that completion fires on every keystroke
/// and amortises the backend round-trip across them. For
/// sqlite-backed sessions the speedup is modest (the queries
/// are already cheap); for remote backends it's the
/// difference between "completion feels instant" and "every
/// tab spawns an HTTP roundtrip."
///
/// ## Invalidation
///
/// Three layers, evaluated in order:
///
/// 1. **Time-based TTL** — every cached entry expires after
///    [`CachedCatalog::ttl`] (default: 1 second). Aggressive
///    by design: completion users tap repeatedly, so the
///    cache absorbs bursts but yields to fresh data quickly.
/// 2. **Generation counter** — [`CachedCatalog::invalidate`]
///    bumps a process-local generation to force the next
///    read. Used by integration tests + by callers that
///    *know* the underlying state changed (e.g. a writer
///    just landed a new metric family).
/// 3. **Backend-supplied mtime** — when the
///    [`CachedCatalog`] holds a `mtime_fn`, every read
///    consults it; if the timestamp moved past the cached
///    snapshot's read time, the entry expires immediately.
///    This is the path the sqlite adapter uses to detect
///    on-disk db changes (writer flushed).
///
/// The cache is a soft hint — if any layer says "stale,"
/// the next call refetches and rebuilds. There's no
/// invariant that two concurrent readers of the same
/// just-invalidated key see the same fresh result; both may
/// run the underlying query. That's acceptable because
/// catalog reads are idempotent and side-effect-free.
pub struct CachedCatalog<C: MetricCatalog + ?Sized> {
    inner: Arc<C>,
    ttl: Duration,
    /// `mtime_fn` returns the latest backend-mtime as a
    /// monotonic instant. The default is `None` (no mtime
    /// invalidation; rely on TTL + generation only). The
    /// sqlite adapter wires this to the db file's
    /// `metadata().modified()`.
    mtime_fn: Option<Box<dyn Fn() -> Option<Instant> + Send + Sync>>,
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    generation: u64,
    families: Option<CacheEntry<Vec<MetricFamilyMeta>>>,
    label_keys: std::collections::HashMap<Option<String>, CacheEntry<Vec<String>>>,
    label_values: std::collections::HashMap<(String, Option<String>), CacheEntry<Vec<String>>>,
    /// `series` is keyed by a stable encoding of the matcher
    /// list (sorted on label, op, value). Computing it
    /// per-call is cheap relative to the underlying query.
    series: std::collections::HashMap<String, CacheEntry<Vec<LabelSet>>>,
    /// `exemplars` keyed by `(encoded matchers, time-range)`.
    /// The time range is part of the identity since two
    /// queries on the same selector but different windows
    /// can resolve to different exemplar sets.
    exemplars: std::collections::HashMap<String, CacheEntry<Vec<ExemplarPoint>>>,
}

struct CacheEntry<T> {
    /// Generation at which this entry was recorded.
    generation: u64,
    /// Wall-clock `Instant` when the entry was filled.
    filled_at: Instant,
    /// Backend mtime at fill time (when known). On read, if
    /// the current mtime is newer, the entry is stale.
    backend_mtime: Option<Instant>,
    value: T,
}

impl<C: MetricCatalog + ?Sized + 'static> CachedCatalog<C> {
    /// Wrap an inner catalog with a default 1-second TTL and
    /// no backend-mtime hook.
    pub fn new(inner: Arc<C>) -> Self {
        Self {
            inner,
            ttl: Duration::from_secs(1),
            mtime_fn: None,
            state: Mutex::new(CacheState::default()),
        }
    }

    /// Builder: set the TTL window. Setting `Duration::ZERO`
    /// disables the TTL layer (entries still expire on
    /// generation bump or mtime change).
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Builder: install a backend-mtime hook. The closure
    /// returns the latest backend-side mtime as a monotonic
    /// `Instant`; entries cached *before* the latest mtime
    /// are considered stale on the next read.
    ///
    /// `None` from the closure (e.g. the underlying file
    /// disappeared) keeps the existing entries — the
    /// behaviour matches "mtime unknown ⇒ trust the TTL."
    pub fn with_mtime_fn<F>(mut self, mtime_fn: F) -> Self
    where
        F: Fn() -> Option<Instant> + Send + Sync + 'static,
    {
        self.mtime_fn = Some(Box::new(mtime_fn));
        self
    }

    /// Force the next read of every key to refetch.
    /// Internally bumps the generation counter; existing
    /// entries become stale at next access.
    pub fn invalidate(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.generation = s.generation.wrapping_add(1);
        }
    }

    /// True if `entry` is still fresh according to all three
    /// invalidation layers. Caller holds the state lock.
    fn is_fresh<T>(&self, entry: &CacheEntry<T>, gen_now: u64) -> bool {
        if entry.generation != gen_now {
            return false;
        }
        if !self.ttl.is_zero() && entry.filled_at.elapsed() >= self.ttl {
            return false;
        }
        if let Some(f) = &self.mtime_fn
            && let Some(now_mtime) = f()
        {
            match entry.backend_mtime {
                Some(prev) if now_mtime > prev => return false,
                None => return false,
                _ => {}
            }
        }
        true
    }

    fn current_mtime(&self) -> Option<Instant> {
        self.mtime_fn.as_ref().and_then(|f| f())
    }
}

impl<C: MetricCatalog + ?Sized + 'static> MetricCatalog for CachedCatalog<C> {
    fn metric_families(&self) -> Result<Vec<MetricFamilyMeta>, DataSourceError> {
        let gen_now;
        {
            let state = self
                .state
                .lock()
                .map_err(|_| DataSourceError::new("cache poisoned"))?;
            gen_now = state.generation;
            if let Some(entry) = &state.families
                && self.is_fresh(entry, gen_now)
            {
                return Ok(entry.value.clone());
            }
        }
        let value = self.inner.metric_families()?;
        let entry = CacheEntry {
            generation: gen_now,
            filled_at: Instant::now(),
            backend_mtime: self.current_mtime(),
            value: value.clone(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.families = Some(entry);
        }
        Ok(value)
    }

    fn label_keys(&self, family_filter: Option<&str>) -> Result<Vec<String>, DataSourceError> {
        let key = family_filter.map(|s| s.to_string());
        let gen_now;
        {
            let state = self
                .state
                .lock()
                .map_err(|_| DataSourceError::new("cache poisoned"))?;
            gen_now = state.generation;
            if let Some(entry) = state.label_keys.get(&key)
                && self.is_fresh(entry, gen_now)
            {
                return Ok(entry.value.clone());
            }
        }
        let value = self.inner.label_keys(family_filter)?;
        let entry = CacheEntry {
            generation: gen_now,
            filled_at: Instant::now(),
            backend_mtime: self.current_mtime(),
            value: value.clone(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.label_keys.insert(key, entry);
        }
        Ok(value)
    }

    fn label_values(
        &self,
        key: &str,
        family_filter: Option<&str>,
    ) -> Result<Vec<String>, DataSourceError> {
        let cache_key = (key.to_string(), family_filter.map(|s| s.to_string()));
        let gen_now;
        {
            let state = self
                .state
                .lock()
                .map_err(|_| DataSourceError::new("cache poisoned"))?;
            gen_now = state.generation;
            if let Some(entry) = state.label_values.get(&cache_key)
                && self.is_fresh(entry, gen_now)
            {
                return Ok(entry.value.clone());
            }
        }
        let value = self.inner.label_values(key, family_filter)?;
        let entry = CacheEntry {
            generation: gen_now,
            filled_at: Instant::now(),
            backend_mtime: self.current_mtime(),
            value: value.clone(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.label_values.insert(cache_key, entry);
        }
        Ok(value)
    }

    fn series(&self, matchers: &[Matcher]) -> Result<Vec<LabelSet>, DataSourceError> {
        let cache_key = encode_matchers(matchers);
        let gen_now;
        {
            let state = self
                .state
                .lock()
                .map_err(|_| DataSourceError::new("cache poisoned"))?;
            gen_now = state.generation;
            if let Some(entry) = state.series.get(&cache_key)
                && self.is_fresh(entry, gen_now)
            {
                return Ok(entry.value.clone());
            }
        }
        let value = self.inner.series(matchers)?;
        let entry = CacheEntry {
            generation: gen_now,
            filled_at: Instant::now(),
            backend_mtime: self.current_mtime(),
            value: value.clone(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.series.insert(cache_key, entry);
        }
        Ok(value)
    }

    fn exemplars(
        &self,
        matchers: &[Matcher],
        time_range: Option<(i64, i64)>,
    ) -> Result<Vec<ExemplarPoint>, DataSourceError> {
        // Cache key = matcher encoding ⊕ time range. The time
        // range is part of the identity because two callers
        // asking for different windows on the same selector
        // need different cached results.
        let mut cache_key = encode_matchers(matchers);
        if let Some((s, e)) = time_range {
            cache_key.push_str(&format!("@{s}..{e}"));
        }
        let gen_now;
        {
            let state = self
                .state
                .lock()
                .map_err(|_| DataSourceError::new("cache poisoned"))?;
            gen_now = state.generation;
            if let Some(entry) = state.exemplars.get(&cache_key)
                && self.is_fresh(entry, gen_now)
            {
                return Ok(entry.value.clone());
            }
        }
        let value = self.inner.exemplars(matchers, time_range)?;
        let entry = CacheEntry {
            generation: gen_now,
            filled_at: Instant::now(),
            backend_mtime: self.current_mtime(),
            value: value.clone(),
        };
        if let Ok(mut state) = self.state.lock() {
            state.exemplars.insert(cache_key, entry);
        }
        Ok(value)
    }
}

/// Stable string encoding of a matcher list — used as a
/// cache key for `series` queries. Sorted by label so two
/// matcher lists differing only in order map to the same key.
fn encode_matchers(matchers: &[Matcher]) -> String {
    let mut sorted: Vec<&Matcher> = matchers.iter().collect();
    sorted.sort_by(|a, b| a.label.cmp(&b.label));
    let mut out = String::new();
    for m in sorted {
        let op = match m.op {
            MatcherOp::Eq => "=",
            MatcherOp::Ne => "!=",
            MatcherOp::EqRegex => "=~",
            MatcherOp::NeRegex => "!~",
        };
        out.push_str(&m.label);
        out.push_str(op);
        out.push_str(&m.value);
        out.push('\x1f'); // unit separator — won't appear in legit matchers
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use MatcherOp;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Simple in-memory catalog used to drive the cache tests.
    /// Counts how many times each method is called so the
    /// tests can assert "this call hit the inner catalog" or
    /// "this one was served from cache."
    struct MockCatalog {
        families: Vec<MetricFamilyMeta>,
        keys: Vec<(Option<String>, Vec<String>)>,
        values: Vec<(String, Option<String>, Vec<String>)>,
        series: Vec<LabelSet>,
        family_calls: AtomicUsize,
        key_calls: AtomicUsize,
        value_calls: AtomicUsize,
        series_calls: AtomicUsize,
    }

    impl MockCatalog {
        fn new() -> Self {
            Self {
                families: vec![
                    MetricFamilyMeta {
                        name: "ops_total".into(),
                        ty: MetricType::Counter,
                        unit: None,
                        help: Some("ops".into()),
                    },
                    MetricFamilyMeta {
                        name: "latency".into(),
                        ty: MetricType::Histogram,
                        unit: Some("seconds".into()),
                        help: None,
                    },
                ],
                keys: vec![
                    (None, vec!["phase".into(), "scenario".into()]),
                    (Some("ops_total".into()), vec!["phase".into()]),
                ],
                values: vec![("phase".into(), None, vec!["setup".into(), "run".into()])],
                series: vec![vec![
                    ("__name__".into(), "ops_total".into()),
                    ("phase".into(), "setup".into()),
                ]],
                family_calls: AtomicUsize::new(0),
                key_calls: AtomicUsize::new(0),
                value_calls: AtomicUsize::new(0),
                series_calls: AtomicUsize::new(0),
            }
        }
    }

    impl MetricCatalog for MockCatalog {
        fn metric_families(&self) -> Result<Vec<MetricFamilyMeta>, DataSourceError> {
            self.family_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.families.clone())
        }
        fn label_keys(&self, filter: Option<&str>) -> Result<Vec<String>, DataSourceError> {
            self.key_calls.fetch_add(1, Ordering::SeqCst);
            for (f, v) in &self.keys {
                if f.as_deref() == filter {
                    return Ok(v.clone());
                }
            }
            Ok(Vec::new())
        }
        fn label_values(
            &self,
            key: &str,
            filter: Option<&str>,
        ) -> Result<Vec<String>, DataSourceError> {
            self.value_calls.fetch_add(1, Ordering::SeqCst);
            for (k, f, v) in &self.values {
                if k == key && f.as_deref() == filter {
                    return Ok(v.clone());
                }
            }
            Ok(Vec::new())
        }
        fn series(&self, _matchers: &[Matcher]) -> Result<Vec<LabelSet>, DataSourceError> {
            self.series_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.series.clone())
        }
    }

    #[test]
    fn metric_type_round_trips_via_str() {
        for ty in [
            MetricType::Counter,
            MetricType::Gauge,
            MetricType::Histogram,
            MetricType::GaugeHistogram,
            MetricType::Summary,
            MetricType::Info,
            MetricType::StateSet,
            MetricType::Unknown,
        ] {
            assert_eq!(MetricType::parse(ty.as_str()), ty);
        }
    }

    #[test]
    fn metric_type_parse_unknown_keyword_yields_unknown() {
        assert_eq!(MetricType::parse("garbage"), MetricType::Unknown);
        assert_eq!(MetricType::parse(""), MetricType::Unknown);
    }

    #[test]
    fn metric_type_implied_labels_match_openmetrics() {
        assert_eq!(MetricType::Histogram.implied_label(), Some("le"));
        assert_eq!(MetricType::GaugeHistogram.implied_label(), Some("le"));
        assert_eq!(MetricType::Summary.implied_label(), Some("quantile"));
        assert_eq!(MetricType::Counter.implied_label(), None);
        assert_eq!(MetricType::Gauge.implied_label(), None);
    }

    #[test]
    fn metric_type_has_derived_series_only_for_histograms_and_summary() {
        assert!(MetricType::Histogram.has_derived_series());
        assert!(MetricType::GaugeHistogram.has_derived_series());
        assert!(MetricType::Summary.has_derived_series());
        assert!(!MetricType::Counter.has_derived_series());
        assert!(!MetricType::Gauge.has_derived_series());
        assert!(!MetricType::Info.has_derived_series());
    }

    #[test]
    fn cache_serves_repeat_calls_from_cache() {
        let inner = Arc::new(MockCatalog::new());
        let cache = CachedCatalog::new(inner.clone());
        let _ = cache.metric_families().unwrap();
        let _ = cache.metric_families().unwrap();
        let _ = cache.metric_families().unwrap();
        assert_eq!(
            inner.family_calls.load(Ordering::SeqCst),
            1,
            "cache should have served repeated calls"
        );
    }

    #[test]
    fn cache_invalidate_forces_refetch() {
        let inner = Arc::new(MockCatalog::new());
        let cache = CachedCatalog::new(inner.clone());
        let _ = cache.metric_families().unwrap();
        cache.invalidate();
        let _ = cache.metric_families().unwrap();
        assert_eq!(
            inner.family_calls.load(Ordering::SeqCst),
            2,
            "invalidate should have forced a refetch"
        );
    }

    #[test]
    fn cache_zero_ttl_still_caches_via_generation() {
        let inner = Arc::new(MockCatalog::new());
        // TTL = 0 is "TTL disabled, fall through to generation
        // / mtime layers." Without an mtime hook, generation
        // alone keeps things cached.
        let cache = CachedCatalog::new(inner.clone()).with_ttl(Duration::ZERO);
        let _ = cache.metric_families().unwrap();
        let _ = cache.metric_families().unwrap();
        assert_eq!(
            inner.family_calls.load(Ordering::SeqCst),
            1,
            "TTL=0 alone still caches via generation"
        );
    }

    #[test]
    fn cache_distinguishes_label_keys_by_family_filter() {
        let inner = Arc::new(MockCatalog::new());
        let cache = CachedCatalog::new(inner.clone());
        let _ = cache.label_keys(None).unwrap();
        let _ = cache.label_keys(Some("ops_total")).unwrap();
        let _ = cache.label_keys(None).unwrap();
        let _ = cache.label_keys(Some("ops_total")).unwrap();
        assert_eq!(
            inner.key_calls.load(Ordering::SeqCst),
            2,
            "different family filters should be cached separately"
        );
    }

    #[test]
    fn cache_distinguishes_label_values_by_key_and_filter() {
        let inner = Arc::new(MockCatalog::new());
        let cache = CachedCatalog::new(inner.clone());
        let _ = cache.label_values("phase", None).unwrap();
        let _ = cache.label_values("phase", Some("ops_total")).unwrap();
        let _ = cache.label_values("phase", None).unwrap();
        assert_eq!(inner.value_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cache_series_keyed_by_matchers_independent_of_order() {
        let inner = Arc::new(MockCatalog::new());
        let cache = CachedCatalog::new(inner.clone());
        let m1 = vec![
            Matcher {
                label: "a".into(),
                op: MatcherOp::Eq,
                value: "1".into(),
            },
            Matcher {
                label: "b".into(),
                op: MatcherOp::Eq,
                value: "2".into(),
            },
        ];
        let m2 = vec![
            Matcher {
                label: "b".into(),
                op: MatcherOp::Eq,
                value: "2".into(),
            },
            Matcher {
                label: "a".into(),
                op: MatcherOp::Eq,
                value: "1".into(),
            },
        ];
        let _ = cache.series(&m1).unwrap();
        let _ = cache.series(&m2).unwrap();
        assert_eq!(
            inner.series_calls.load(Ordering::SeqCst),
            1,
            "matcher-order shouldn't fragment the cache"
        );
    }

    #[test]
    fn cache_mtime_hook_invalidates_on_advance() {
        let inner = Arc::new(MockCatalog::new());
        let mtime = Arc::new(Mutex::new(Instant::now()));
        let mtime_clone = mtime.clone();
        let cache = CachedCatalog::new(inner.clone())
            .with_mtime_fn(move || Some(*mtime_clone.lock().unwrap()));
        let _ = cache.metric_families().unwrap();
        // First repeat is fresh.
        let _ = cache.metric_families().unwrap();
        assert_eq!(inner.family_calls.load(Ordering::SeqCst), 1);
        // Advance the mtime — next call should refetch.
        *mtime.lock().unwrap() += Duration::from_millis(1);
        let _ = cache.metric_families().unwrap();
        assert_eq!(
            inner.family_calls.load(Ordering::SeqCst),
            2,
            "advanced mtime should have invalidated the cache"
        );
    }

    #[test]
    fn encode_matchers_sorts_by_label() {
        let m1 = vec![
            Matcher {
                label: "a".into(),
                op: MatcherOp::Eq,
                value: "1".into(),
            },
            Matcher {
                label: "b".into(),
                op: MatcherOp::EqRegex,
                value: ".*".into(),
            },
        ];
        let m2 = vec![
            Matcher {
                label: "b".into(),
                op: MatcherOp::EqRegex,
                value: ".*".into(),
            },
            Matcher {
                label: "a".into(),
                op: MatcherOp::Eq,
                value: "1".into(),
            },
        ];
        assert_eq!(encode_matchers(&m1), encode_matchers(&m2));
    }
}

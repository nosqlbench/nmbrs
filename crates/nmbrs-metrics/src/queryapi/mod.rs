// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The metrics **query API** — the data-access *service* boundary
//! (SRD-86 §"The metric-reader surface").
//!
//! `nmbrs-metrics` is the foundational data-access library; this module
//! exposes its query surface as a service. It provides the native
//! result shape ([`Vector`] — multiple [`Series`], each with sample
//! points), the selector semantics ([`Matcher`]), and a small access
//! contract ([`MetricAccess`]) whose signatures map **1:1 onto a
//! MetricsQL parsed selector**:
//!
//! - a bare / instant selector → [`MetricAccess::select_instant`];
//! - a range selector (`m[w]`) → [`MetricAccess::select_range`].
//!
//! Deliberately **not** here: aggregation, rollups, arithmetic — the
//! "bells and whistles" of the query language. Those stay in the
//! MetricsQL engine, layered *over* this access surface. Keeping the
//! contract this thin is what lets the engine sit on any data service.
//!
//! ## Service location (a "data service object")
//!
//! Consumers (the MetricsQL engine) **locate** a service at runtime
//! rather than binding a concrete impl:
//!
//! - the **live in-process** service wraps the session's `MetricsQuery`
//!   (which is not static), so the runner [`install_live_access`]es it
//!   per session and consumers read it via [`live_access`];
//! - **file / external** backends (e.g. the sqlite reader) register an
//!   [`AccessProvider`] via `inventory`, so a consumer can [`provider`]
//!   one by scheme and open it — without the engine depending on the
//!   backend's crate or features.

pub mod catalog;
mod hybrid;
mod live;
mod shapes;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use catalog::{
    CachedCatalog, ExemplarPoint, LabelSet, MetricCatalog, MetricFamilyMeta, MetricType,
};
pub use hybrid::{HorizonAware, HybridStore, Tier};
pub use live::MetricsQueryAccess;
pub use shapes::{MatchOp, Matcher, Sample, Series, Vector};

use std::sync::{Arc, LazyLock, OnceLock};

use arc_swap::ArcSwapOption;

/// Error from a metrics access backend. Backends own their taxonomy;
/// the engine treats these as opaque from a flow-control standpoint.
#[derive(Debug, Clone)]
pub struct QueryError {
    pub message: String,
}

impl QueryError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "metrics query: {}", self.message)
    }
}

impl std::error::Error for QueryError {}

/// The metrics data-access **service**. Backends implement it; a query
/// layer locates one at runtime and reads `Vector`s through it. See the
/// module docs for the access/aggregation cut line.
pub trait MetricAccess: Send + Sync {
    /// Range selection: series matching every `matcher`, with every
    /// sample in `[start_ms, end_ms]` (ascending). Yields a range
    /// vector — the input a rollup (`rate(m[w])`, `*_over_time`)
    /// consumes. This is the one required method; [`select_instant`]
    /// derives from it.
    ///
    /// [`select_instant`]: MetricAccess::select_instant
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, QueryError>;

    /// Instant selection: series matching every `matcher`, each reduced
    /// to its latest sample within `[at_ms - lookback, at_ms]` (the
    /// PromQL stale-tolerance window; `lookback_ms = None` means a
    /// strict `[at_ms, at_ms]`). Yields an instant vector — one sample
    /// per series.
    ///
    /// Default: `select_range` over the lookback window, then the
    /// latest sample per series. A backend can override if it can do
    /// the reduction more cheaply (e.g. push it into SQL).
    fn select_instant(
        &self,
        matchers: &[Matcher],
        at_ms: i64,
        lookback_ms: Option<i64>,
    ) -> Result<Vector, QueryError> {
        let start_ms = at_ms - lookback_ms.unwrap_or(0);
        let range = self.select_range(matchers, start_ms, at_ms)?;
        let reduced = range
            .into_series()
            .into_iter()
            .filter_map(|s| {
                s.samples.last().copied().map(|last| Series {
                    labels: s.labels,
                    samples: vec![last],
                })
            })
            .collect();
        Ok(Vector::new(reduced))
    }
}

// ---------------------------------------------------------------------
// Runtime service location
// ---------------------------------------------------------------------

/// Sized holder for the trait-object service, so it can live in an
/// `ArcSwapOption` (whose pointee must be `Sized`; a bare `dyn MetricAccess`
/// is not).
struct LiveHolder(Arc<dyn MetricAccess>);

/// The live in-process access service for the current session. Wraps a
/// per-session `MetricsQuery`, so it's *installed* (not static).
///
/// SRD-90 §M4 — an `ArcSwapOption`, not a `Mutex`: `live_access()` is on the
/// metricsql read hot path (every settle pulse / TUI refresh resolves it), so
/// the read is a single lock-free atomic load, never a mutex acquire that could
/// contend under concurrent executions.
static LIVE: LazyLock<ArcSwapOption<LiveHolder>> = LazyLock::new(ArcSwapOption::empty);

/// Install the live in-process access service. Called once by the
/// runner when the session's `MetricsQuery` is built.
pub fn install_live_access(service: Arc<dyn MetricAccess>) {
    LIVE.store(Some(Arc::new(LiveHolder(service))));
}

/// Drop the installed live-access service and everything it owns —
/// crucially the HybridStore's sqlite "cold tier" reader connection on
/// `metrics.db`. The runner calls this at session shutdown BEFORE the
/// SQLite reporter consolidates the WAL: `PRAGMA journal_mode=DELETE` needs
/// an EXCLUSIVE lock on the db, which a still-open reader connection on the
/// same file blocks ("database is locked"). `swap` + explicit `drop` so the
/// holder's Arc chain is released here rather than deferred to the next
/// `store`. Idempotent; a no-op if nothing was installed.
pub fn uninstall_live_access() {
    drop(LIVE.swap(None));
}

/// The live in-process access service, if a session has installed one.
/// Lock-free: one atomic `ArcSwap` load.
pub fn live_access() -> Option<Arc<dyn MetricAccess>> {
    LIVE.load_full().map(|h| h.0.clone())
}

/// Resolves the **reading execution's** `exec_id`, so a live metric read
/// can scope itself to its own execution's series instead of every
/// execution sharing the session (SRD-88 encapsulation — without this, an
/// optimizer's `sum(rate(errors_total[…]))` would sum a concurrent
/// neighbour's errors too). `exec_id` lives in nmbrs-runtime's task-local
/// `ExecutionContext`, a layer above this crate, so the runtime installs
/// a small resolver hook here. `None` ⇒ no scope (single-run / outside any
/// execution — read everything, A1).
static READ_EXEC_ID_HOOK: OnceLock<fn() -> Option<u64>> = OnceLock::new();

/// Install the reading-execution `exec_id` resolver (idempotent — first
/// wins). The runtime calls this once with a fn that reads its task-local
/// execution context.
pub fn install_read_exec_id_hook(hook: fn() -> Option<u64>) {
    let _ = READ_EXEC_ID_HOOK.set(hook);
}

/// The reading execution's `exec_id`, if a hook is installed and a scope
/// is active. `None` ⇒ live reads are unscoped (A1 single-run).
pub fn current_read_exec_id() -> Option<u64> {
    READ_EXEC_ID_HOOK.get().and_then(|h| h())
}

/// SRD-89 §3b / SRD-90 §M6 — scope every read to its **reading execution** by
/// injecting `exec_id` as a uniform **dimensional-label matcher**, so each
/// interior backend applies it wherever `exec_id` lives (the in-memory tier's
/// label set, the sqlite tier's `exec_id` column) with the same value. This
/// replaces the per-backend special-casing (the live tier's bespoke post-filter,
/// a sqlite selection mode) with one mechanism: `exec_id` is just a label.
///
/// `None` (single-run / outside any execution scope) ⇒ no injection — the read
/// is unscoped and sees the sole execution's data, byte-identical to before
/// (axiom A1). If the caller already constrained `exec_id`, nothing is injected.
pub struct ExecScopedAccess {
    inner: Arc<dyn MetricAccess>,
}

impl ExecScopedAccess {
    pub fn new(inner: Arc<dyn MetricAccess>) -> Self {
        Self { inner }
    }

    /// The matcher set with the reading execution's `exec_id` injected (when a
    /// scope is active and the caller hasn't already constrained it).
    fn scoped(&self, matchers: &[Matcher]) -> Option<Vec<Matcher>> {
        let id = current_read_exec_id()?;
        if matchers.iter().any(|m| m.label == "exec_id") {
            return None; // caller already scoped — don't double-inject
        }
        let mut v = matchers.to_vec();
        v.push(Matcher::eq("exec_id", &id.to_string()));
        Some(v)
    }
}

impl MetricAccess for ExecScopedAccess {
    fn select_range(
        &self,
        matchers: &[Matcher],
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vector, QueryError> {
        match self.scoped(matchers) {
            Some(scoped) => self.inner.select_range(&scoped, start_ms, end_ms),
            None => self.inner.select_range(matchers, start_ms, end_ms),
        }
    }

    fn select_instant(
        &self,
        matchers: &[Matcher],
        at_ms: i64,
        lookback_ms: Option<i64>,
    ) -> Result<Vector, QueryError> {
        match self.scoped(matchers) {
            Some(scoped) => self.inner.select_instant(&scoped, at_ms, lookback_ms),
            None => self.inner.select_instant(matchers, at_ms, lookback_ms),
        }
    }
}

/// A pluggable access backend, discovered at runtime via `inventory`.
/// A consumer opens a service for a scheme-specific `target` (e.g. a
/// db path). The sqlite reader registers one; future backends can too,
/// without the query engine depending on them.
pub struct AccessProvider {
    /// The scheme this provider answers for (e.g. `"sqlite"`).
    pub scheme: &'static str,
    /// Open an access service for `target` (scheme-specific).
    pub open: fn(target: &str) -> Result<Box<dyn MetricAccess>, QueryError>,
}

inventory::collect!(AccessProvider);

/// Locate a registered [`AccessProvider`] by scheme.
pub fn provider(scheme: &str) -> Option<&'static AccessProvider> {
    inventory::iter::<AccessProvider>
        .into_iter()
        .find(|p| p.scheme == scheme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_access_installs_and_reads_back() {
        struct Stub;
        impl MetricAccess for Stub {
            fn select_instant(
                &self,
                _: &[Matcher],
                _: i64,
                _: Option<i64>,
            ) -> Result<Vector, QueryError> {
                Ok(Vector::default())
            }
            fn select_range(&self, _: &[Matcher], _: i64, _: i64) -> Result<Vector, QueryError> {
                Ok(Vector::default())
            }
        }
        install_live_access(Arc::new(Stub));
        assert!(live_access().is_some());
    }

    #[test]
    fn unknown_provider_scheme_is_none() {
        assert!(provider("no-such-scheme").is_none());
    }
}

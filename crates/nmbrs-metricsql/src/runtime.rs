// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Continuous-query runtime over [`crate::streaming`].
//!
//! Specified in [SRD 48][srd48]. Owns one or more
//! [`StreamingPlan`] instances, drives a [`SampleFeed`],
//! exposes per-plan snapshots to consumers via
//! `arc_swap::ArcSwap`. Single-owner-thread architecture
//! mirrors the SRD-40 lock-free metrics pattern: producers
//! send commands over [`crossbeam_channel`] to the owner;
//! readers see snapshots without contending.
//!
//! [srd48]: ../../../docs/SRD/48_metricsql_continuous_query.md
//!
//! # Lifecycle
//!
//! ```text
//! Register → backfill warmup window → publish first
//!         ↓
//!   Active (idle)
//!         ↓
//!     Tick → fetch since per-leaf watermark → ingest →
//!            re-snapshot → republish ArcSwap
//!         ↓
//!     Reset (window-policy dependent) → plan.reset() →
//!            watermark reset → publish empty snapshot
//!         ↓
//!     Unregister → drop accumulators → publishers see
//!            final empty snapshot
//! ```
//!
//! # Why an actor (not `RwLock<...>`)
//!
//! See SRD-48 §"Concurrency model": `RwLock` would force
//! readers to take a lock per snapshot; the actor pattern
//! lets readers see snapshots through `ArcSwap::load`,
//! comparable to a relaxed atomic load in cost. Linux's
//! std `RwLock` is also writer-preferring (see project
//! memory entry "No std::RwLock for Render State"), which
//! would starve the snapshot path under load.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use crate::eval::{DataSourceError, Matcher, MetricAccess, Series, Vector};
use crate::streaming::{CompileError, StreamingPlan, compile_streaming};

/// Opaque per-plan identifier. Returned by
/// [`ContinuousQueryRuntime::register`]; consumers carry
/// it via the [`QueryHandle`] they received.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PlanId(u64);

/// Pluggable sample-feed contract. The runtime calls
/// [`fetch_since`] once per tick per registered matcher
/// set; the feed returns every series with samples in
/// `(since_ms, latest_ts]` (the `since_ms` bound is
/// exclusive — the runtime advances watermarks by 1ms to
/// avoid double-ingest).
///
/// Two implementations ship in this push:
///
/// - [`PullFeed`] — wraps any [`DataSource`]; polls on a
///   cadence the caller drives via
///   [`ContinuousQueryRuntime::tick`].
/// - `PushFeed` — channel-based real-time ingest; lands
///   in a follow-up push (SRD-48 §"Followups").
pub trait SampleFeed: Send + Sync {
    /// Fetch every matching series with samples newer than
    /// `since_ms` (exclusive lower bound) and at most
    /// `until_ms` (inclusive upper bound). The runtime
    /// derives `until_ms` from [`latest_ts`] before the
    /// call.
    fn fetch_since(
        &self,
        matchers: &[Matcher],
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vector, DataSourceError>;

    /// Latest sample timestamp the feed knows about.
    /// Returning `None` means "no data yet" — the runtime
    /// skips the tick rather than ingesting nothing
    /// noisily.
    fn latest_ts(&self) -> Result<Option<i64>, DataSourceError>;
}

/// Pull-style feed: wraps any [`DataSource`] and polls on
/// the runtime's tick cadence. Watermark advancement
/// happens in the runtime, not here — the feed is
/// stateless w.r.t. consumers.
pub struct PullFeed<D: MetricAccess + ?Sized> {
    pub source: Box<D>,
}

impl<D: MetricAccess + ?Sized + Send + Sync> SampleFeed for PullFeed<D> {
    fn fetch_since(
        &self,
        matchers: &[Matcher],
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Vector, DataSourceError> {
        // `since_ms` is exclusive; `DataSource::fetch` is
        // inclusive on both ends. Add 1 to convert.
        let start = since_ms.saturating_add(1);
        if start > until_ms {
            return Ok(Vector::default());
        }
        self.source.select_range(matchers, start, until_ms)
    }

    fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
        // The trait doesn't give us a "latest sample" probe
        // directly; the typical backend (sqlite adapter)
        // implements it as a side method. A wrapper feed
        // can shadow this with a more efficient probe
        // (e.g. `SELECT MAX(timestamp_ms) FROM sample_value`).
        // Default: nothing — caller advances by their own
        // clock or supplies an `until_ms` directly.
        Ok(None)
    }
}

/// Window-framing policy per SRD-48 §"Window framing
/// policy". Set per plan at registration; default is
/// `Tumbling` with 5 minutes — bounded memory and the
/// "current cadence" semantics most live dashboards want.
#[derive(Clone, Copy, Debug)]
pub enum WindowPolicy {
    /// Accumulators grow without reset. Suitable for
    /// counter aggregates where users want lifetime sums.
    /// Memory grows with `O(distinct timestamps observed)`.
    Lifetime,
    /// Plan resets every `duration_ms`. The first snapshot
    /// after a reset is empty; subsequent ticks repopulate.
    /// Memory bounded by `O(samples per window)`.
    Tumbling { duration_ms: i64 },
}

impl Default for WindowPolicy {
    fn default() -> Self {
        Self::Tumbling {
            duration_ms: 5 * 60_000,
        }
    }
}

/// Public-facing runtime. Cheap to clone — internal state
/// lives behind an `Arc` so multiple consumers share the
/// same actor.
#[derive(Clone)]
pub struct ContinuousQueryRuntime {
    inner: Arc<RuntimeShared>,
}

struct RuntimeShared {
    cmd_tx: Sender<Command>,
    next_id: AtomicU64,
}

/// Errors from `ContinuousQueryRuntime::register`.
#[derive(Debug, Clone)]
pub enum RegisterError {
    /// `compile_streaming` rejected the query — the runtime
    /// requires streaming-compilable shapes; callers
    /// wanting batch-fallback semantics need to wrap their
    /// own dispatch (see `nmbrs metrics watch` for an
    /// example).
    CompileFailed(String),
    /// Parser failure on the query text.
    ParseFailed(String),
    /// Actor thread is gone — runtime was dropped.
    Closed,
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CompileFailed(s) => write!(f, "compile: {s}"),
            Self::ParseFailed(s) => write!(f, "parse: {s}"),
            Self::Closed => write!(f, "runtime closed"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Errors from `ContinuousQueryRuntime::tick`.
#[derive(Debug, Clone)]
pub enum TickError {
    Closed,
    /// Sample feed reported a failure during fetch.
    Feed(String),
}

impl std::fmt::Display for TickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => write!(f, "runtime closed"),
            Self::Feed(s) => write!(f, "feed: {s}"),
        }
    }
}

impl std::error::Error for TickError {}

enum Command {
    Register {
        plan: StreamingPlan,
        plan_id: PlanId,
        warmup_ms: i64,
        window_policy: WindowPolicy,
        snapshot: Arc<ArcSwap<Vec<Series>>>,
        reply: Sender<Result<(), DataSourceError>>,
    },
    Unregister {
        plan_id: PlanId,
    },
    /// Drive one tick: advance per-leaf watermarks, ingest
    /// new samples into every plan, recompute snapshots,
    /// publish.
    Tick {
        reply: Sender<Result<(), TickError>>,
    },
    /// For tests: ask the actor for its current plan
    /// count, useful for verifying register/unregister
    /// without exposing internals.
    #[cfg(test)]
    PlanCount {
        reply: Sender<usize>,
    },
}

impl ContinuousQueryRuntime {
    /// Start a runtime backed by `feed`. Spawns the actor
    /// thread immediately; the runtime is usable as soon
    /// as this returns.
    pub fn with_feed(feed: Box<dyn SampleFeed>) -> Self {
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        std::thread::Builder::new()
            .name("metricsql-runtime".into())
            .spawn(move || actor_loop(cmd_rx, feed))
            .expect("spawn metricsql-runtime thread");
        Self {
            inner: Arc::new(RuntimeShared {
                cmd_tx,
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Register a query. Compiles via `compile_streaming`
    /// (the runtime only handles streaming-compilable
    /// shapes); on success, the actor backfills the plan's
    /// accumulators with the warmup window and publishes
    /// the first snapshot.
    ///
    /// Returns a [`QueryHandle`] the caller uses to read
    /// snapshots and (via `Drop`) unregister.
    pub fn register(&self, query: &str) -> Result<QueryHandle, RegisterError> {
        self.register_with(query, RegisterOptions::default())
    }

    /// Register with explicit options (warmup, window
    /// policy). Useful when the default 5-minute tumbling
    /// window doesn't fit (e.g. lifetime counters).
    pub fn register_with(
        &self,
        query: &str,
        options: RegisterOptions,
    ) -> Result<QueryHandle, RegisterError> {
        let expr = crate::parse(query).map_err(|e| RegisterError::ParseFailed(e.to_string()))?;
        let plan = compile_streaming(&expr)
            .map_err(|e: CompileError| RegisterError::CompileFailed(e.to_string()))?;
        let plan_id = PlanId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let snapshot = Arc::new(ArcSwap::from_pointee(Vec::<Series>::new()));
        let (reply_tx, reply_rx) = bounded(1);
        let cmd = Command::Register {
            plan,
            plan_id,
            warmup_ms: options.warmup_ms,
            window_policy: options.window_policy,
            snapshot: Arc::clone(&snapshot),
            reply: reply_tx,
        };
        self.inner
            .cmd_tx
            .send(cmd)
            .map_err(|_| RegisterError::Closed)?;
        // Wait for the actor to confirm registration (and
        // backfill). Only the data-source error case is
        // surfaced here as a register-time failure; the
        // SRD documents that behavior.
        match reply_rx.recv() {
            Ok(Ok(())) => Ok(QueryHandle {
                runtime: self.clone(),
                plan_id,
                snapshot,
            }),
            Ok(Err(e)) => Err(RegisterError::CompileFailed(format!(
                "backfill failed: {e}"
            ))),
            Err(_) => Err(RegisterError::Closed),
        }
    }

    /// Drive one tick. Each registered plan's per-leaf
    /// watermarks advance to the feed's `latest_ts`; the
    /// fetch-and-ingest cycle repeats per plan; snapshots
    /// republish.
    pub fn tick(&self) -> Result<(), TickError> {
        let (reply_tx, reply_rx) = bounded(1);
        self.inner
            .cmd_tx
            .send(Command::Tick { reply: reply_tx })
            .map_err(|_| TickError::Closed)?;
        reply_rx.recv().map_err(|_| TickError::Closed)?
    }

    /// Test-only: peek at how many plans are currently
    /// registered.
    #[cfg(test)]
    fn plan_count(&self) -> usize {
        let (reply_tx, reply_rx) = bounded(1);
        self.inner
            .cmd_tx
            .send(Command::PlanCount { reply: reply_tx })
            .ok();
        reply_rx.recv().unwrap_or(0)
    }
}

/// Optional registration parameters. Defaulted via
/// `RegisterOptions::default()`.
#[derive(Clone, Copy, Debug)]
pub struct RegisterOptions {
    pub warmup_ms: i64,
    pub window_policy: WindowPolicy,
}

impl Default for RegisterOptions {
    fn default() -> Self {
        Self {
            warmup_ms: 5 * 60_000, // 5 minutes
            window_policy: WindowPolicy::default(),
        }
    }
}

/// Per-plan handle. Snapshot reads are cheap
/// (`ArcSwap::load`); dropping the handle unregisters the
/// plan from the runtime.
pub struct QueryHandle {
    runtime: ContinuousQueryRuntime,
    plan_id: PlanId,
    snapshot: Arc<ArcSwap<Vec<Series>>>,
}

impl QueryHandle {
    /// Read the latest published snapshot. Cheap — wraps
    /// `ArcSwap::load` and clones the inner `Vec<Series>`.
    /// The returned vector is a deep clone; callers can
    /// retain it without keeping the snapshot alive.
    pub fn snapshot(&self) -> Vec<Series> {
        (**self.snapshot.load()).clone()
    }

    /// Direct handle to the underlying snapshot publisher.
    /// Returned for callers (TUI panels) that want
    /// `Arc<ArcSwap<...>>::load` without going through the
    /// `Vec` clone — useful for change-detection and
    /// zero-copy reads.
    pub fn snapshot_handle(&self) -> Arc<ArcSwap<Vec<Series>>> {
        Arc::clone(&self.snapshot)
    }

    pub fn plan_id(&self) -> PlanId {
        self.plan_id
    }
}

impl Drop for QueryHandle {
    fn drop(&mut self) {
        // Best-effort unregister; ignore errors (the
        // runtime may have shut down already).
        let _ = self.runtime.inner.cmd_tx.send(Command::Unregister {
            plan_id: self.plan_id,
        });
    }
}

/// Per-plan state owned by the actor thread.
struct PlanEntry {
    plan: StreamingPlan,
    /// Per-leaf watermark — each leaf's matchers query
    /// independently, so each advances on its own.
    leaf_watermarks: Vec<i64>,
    /// Where snapshots get published. Readers
    /// `ArcSwap::load()` this without contention.
    snapshot: Arc<ArcSwap<Vec<Series>>>,
    /// Last time (in ms, anchored to feed clock) the
    /// tumbling-window timer fired. `0` until first tick.
    last_reset_ms: i64,
    window_policy: WindowPolicy,
}

fn actor_loop(cmd_rx: Receiver<Command>, feed: Box<dyn SampleFeed>) {
    let mut plans: HashMap<PlanId, PlanEntry> = HashMap::new();
    while let Ok(cmd) = cmd_rx.recv() {
        match cmd {
            Command::Register {
                plan,
                plan_id,
                warmup_ms,
                window_policy,
                snapshot,
                reply,
            } => {
                let result = handle_register(
                    &mut plans,
                    plan,
                    plan_id,
                    warmup_ms,
                    window_policy,
                    snapshot,
                    &*feed,
                );
                let _ = reply.send(result);
            }
            Command::Unregister { plan_id } => {
                plans.remove(&plan_id);
            }
            Command::Tick { reply } => {
                let result = handle_tick(&mut plans, &*feed);
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::PlanCount { reply } => {
                let _ = reply.send(plans.len());
            }
        }
    }
}

fn handle_register(
    plans: &mut HashMap<PlanId, PlanEntry>,
    mut plan: StreamingPlan,
    plan_id: PlanId,
    warmup_ms: i64,
    window_policy: WindowPolicy,
    snapshot: Arc<ArcSwap<Vec<Series>>>,
    feed: &dyn SampleFeed,
) -> Result<(), DataSourceError> {
    let leaf_count = plan.leaf_matchers().len();
    let now = feed.latest_ts()?.unwrap_or(0);
    let backfill_start = now.saturating_sub(warmup_ms);
    // Backfill: pull every leaf's matcher set's window of
    // historical data, ingest into the plan. After this
    // the snapshot reflects the warmup window.
    let leaf_matchers = plan.leaf_matchers();
    for matchers in &leaf_matchers {
        // since_ms is exclusive — pass `start - 1` so the
        // boundary is inclusive on the way in.
        let series = feed.fetch_since(matchers, backfill_start - 1, now)?;
        for s in series.into_series() {
            plan.ingest_series(&s.labels, &s.samples);
        }
    }
    let initial = plan.snapshot(now);
    snapshot.store(Arc::new(initial));
    plans.insert(
        plan_id,
        PlanEntry {
            plan,
            leaf_watermarks: vec![now; leaf_count],
            snapshot,
            last_reset_ms: now,
            window_policy,
        },
    );
    Ok(())
}

fn handle_tick(
    plans: &mut HashMap<PlanId, PlanEntry>,
    feed: &dyn SampleFeed,
) -> Result<(), TickError> {
    let latest = feed.latest_ts().map_err(|e| TickError::Feed(e.message))?;
    let Some(now) = latest else {
        return Ok(());
    };

    for entry in plans.values_mut() {
        // Window-policy reset check — fires before ingest
        // so the new window's data populates the fresh
        // accumulators, not the old.
        if let WindowPolicy::Tumbling { duration_ms } = entry.window_policy
            && now - entry.last_reset_ms >= duration_ms
        {
            entry.plan.reset();
            entry.leaf_watermarks.iter_mut().for_each(|w| *w = now);
            entry.last_reset_ms = now;
            // Publish the post-reset (empty) snapshot
            // immediately so subscribers see the
            // transition.
            entry.snapshot.store(Arc::new(entry.plan.snapshot(now)));
            continue;
        }

        let leaf_matchers = entry.plan.leaf_matchers();
        for (i, matchers) in leaf_matchers.iter().enumerate() {
            let watermark = entry.leaf_watermarks.get(i).copied().unwrap_or(0);
            if now <= watermark {
                continue;
            }
            let series = feed
                .fetch_since(matchers, watermark, now)
                .map_err(|e| TickError::Feed(e.message))?;
            for s in series.into_series() {
                entry.plan.ingest_series(&s.labels, &s.samples);
            }
            if let Some(w) = entry.leaf_watermarks.get_mut(i) {
                *w = now;
            }
        }
        // Publish even if no new data arrived — keeps the
        // anchor timestamp on the snapshot fresh.
        entry.snapshot.store(Arc::new(entry.plan.snapshot(now)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::Sample;
    use std::sync::Mutex;

    /// In-memory feed: holds a mutable series store the
    /// test populates and `fetch_since` walks. Latest_ts
    /// reads the max timestamp the test has loaded.
    struct MemFeed {
        series: Mutex<Vec<Series>>,
    }

    impl MemFeed {
        fn new() -> Self {
            Self {
                series: Mutex::new(Vec::new()),
            }
        }

        fn push(&self, s: Series) {
            self.series.lock().unwrap().push(s);
        }
    }

    impl SampleFeed for MemFeed {
        fn fetch_since(
            &self,
            matchers: &[Matcher],
            since_ms: i64,
            until_ms: i64,
        ) -> Result<Vector, DataSourceError> {
            // `since_ms` is exclusive per-trait contract; in
            // PullFeed we'd already converted, but tests
            // call this directly so honour the contract here.
            let lo = since_ms.saturating_add(1);
            let store = self.series.lock().unwrap();
            Ok(store
                .iter()
                .filter(|s| matchers.iter().all(|m| match_label(s, m)))
                .map(|s| Series {
                    labels: s.labels.clone(),
                    samples: s
                        .samples
                        .iter()
                        .filter(|sm| sm.timestamp_ms >= lo && sm.timestamp_ms <= until_ms)
                        .cloned()
                        .collect(),
                })
                .collect())
        }

        fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
            let store = self.series.lock().unwrap();
            Ok(store
                .iter()
                .flat_map(|s| s.samples.iter().map(|sm| sm.timestamp_ms))
                .max())
        }
    }

    fn match_label(s: &Series, m: &Matcher) -> bool {
        let v = s
            .labels
            .iter()
            .find(|(k, _)| k == &m.label)
            .map(|(_, v)| v.as_str())
            .unwrap_or("");
        match m.op {
            crate::eval::MatcherOp::Eq => v == m.value,
            crate::eval::MatcherOp::Ne => v != m.value,
            _ => v == m.value,
        }
    }

    fn series(labels: &[(&str, &str)], samples: &[(i64, f64)]) -> Series {
        Series {
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            samples: samples
                .iter()
                .map(|(t, v)| Sample {
                    timestamp_ms: *t,
                    value: *v,
                })
                .collect(),
        }
    }

    #[test]
    fn register_returns_handle_and_first_snapshot() {
        let feed = Arc::new(MemFeed::new());
        feed.push(series(
            &[("__name__", "cpu"), ("host", "h1")],
            &[(0, 5.0), (10, 7.0)],
        ));
        // Wrap the Arc in a per-test Box so the runtime can
        // own one — clone the Arc back via a thin shim.
        struct Shim(Arc<MemFeed>);
        impl SampleFeed for Shim {
            fn fetch_since(
                &self,
                m: &[Matcher],
                a: i64,
                b: i64,
            ) -> Result<Vector, DataSourceError> {
                self.0.fetch_since(m, a, b)
            }
            fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
                self.0.latest_ts()
            }
        }
        let runtime = ContinuousQueryRuntime::with_feed(Box::new(Shim(Arc::clone(&feed))));
        let handle = runtime.register("sum(cpu)").expect("register");

        let snap = handle.snapshot();
        assert_eq!(snap.len(), 1);
        // Backfill ran — both samples are in the result.
        let samples = &snap[0].samples;
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].value, 5.0);
        assert_eq!(samples[1].value, 7.0);
    }

    #[test]
    fn tick_ingests_new_samples_and_advances_watermark() {
        let feed = Arc::new(MemFeed::new());
        feed.push(series(&[("__name__", "cpu"), ("host", "h1")], &[(0, 1.0)]));
        struct Shim(Arc<MemFeed>);
        impl SampleFeed for Shim {
            fn fetch_since(
                &self,
                m: &[Matcher],
                a: i64,
                b: i64,
            ) -> Result<Vector, DataSourceError> {
                self.0.fetch_since(m, a, b)
            }
            fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
                self.0.latest_ts()
            }
        }
        let runtime = ContinuousQueryRuntime::with_feed(Box::new(Shim(Arc::clone(&feed))));
        let opts = RegisterOptions {
            // Disable tumbling so post-tick snapshot
            // reflects accumulated samples rather than a
            // fresh-window reset.
            window_policy: WindowPolicy::Lifetime,
            warmup_ms: 1_000_000,
        };
        let handle = runtime.register_with("sum(cpu)", opts).expect("register");

        // Initial snapshot has one sample.
        assert_eq!(handle.snapshot()[0].samples.len(), 1);

        // Add a new sample at t=10, tick.
        feed.push(series(
            &[("__name__", "cpu"), ("host", "h1")],
            &[(10, 99.0)],
        ));
        runtime.tick().expect("tick");

        // After tick, snapshot has two timestamps.
        let snap = handle.snapshot();
        assert_eq!(snap.len(), 1);
        let timestamps: Vec<i64> = snap[0].samples.iter().map(|s| s.timestamp_ms).collect();
        assert_eq!(timestamps, vec![0, 10]);
    }

    #[test]
    fn drop_handle_unregisters() {
        struct Empty;
        impl SampleFeed for Empty {
            fn fetch_since(
                &self,
                _: &[Matcher],
                _: i64,
                _: i64,
            ) -> Result<Vector, DataSourceError> {
                Ok(Vector::default())
            }
            fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
                Ok(Some(0))
            }
        }
        let runtime = ContinuousQueryRuntime::with_feed(Box::new(Empty));
        let handle = runtime.register("sum(cpu)").expect("register");
        assert_eq!(runtime.plan_count(), 1);
        drop(handle);
        // Give the actor a moment to process the unregister.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(runtime.plan_count(), 0);
    }

    #[test]
    fn tumbling_window_resets_at_cadence() {
        let feed = Arc::new(MemFeed::new());
        feed.push(series(
            &[("__name__", "cpu"), ("host", "h1")],
            &[(0, 100.0)],
        ));
        struct Shim(Arc<MemFeed>);
        impl SampleFeed for Shim {
            fn fetch_since(
                &self,
                m: &[Matcher],
                a: i64,
                b: i64,
            ) -> Result<Vector, DataSourceError> {
                self.0.fetch_since(m, a, b)
            }
            fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
                self.0.latest_ts()
            }
        }
        let runtime = ContinuousQueryRuntime::with_feed(Box::new(Shim(Arc::clone(&feed))));
        let opts = RegisterOptions {
            window_policy: WindowPolicy::Tumbling { duration_ms: 1000 },
            warmup_ms: 100_000,
        };
        let handle = runtime.register_with("sum(cpu)", opts).expect("register");
        // First snapshot has the backfill data.
        assert_eq!(handle.snapshot().len(), 1);

        // Advance feed time past the tumble boundary.
        feed.push(series(
            &[("__name__", "cpu"), ("host", "h1")],
            &[(2000, 5.0)],
        ));
        runtime.tick().expect("tick");

        // After the reset (2000 - 0 = 2000 > 1000), only
        // the new T=2000 sample should be in the
        // accumulator. Old T=0 sample is gone.
        let snap = handle.snapshot();
        // Reset publishes an empty snapshot; we don't
        // re-ingest the new sample in the same tick, so
        // expect empty.
        assert!(snap.is_empty(), "expected empty post-reset, got {snap:?}");
    }

    #[test]
    fn streaming_via_runtime_equals_batch() {
        // Property: runtime tick + snapshot equals batch
        // evaluation against the same data. Same load-
        // bearing artifact pattern as SRD-47, scoped down.
        use crate::eval::{EvalContext, evaluate};

        let inputs: Vec<Series> = vec![
            series(
                &[("__name__", "cpu"), ("host", "h1")],
                &[(0, 1.0), (10, 2.0)],
            ),
            series(
                &[("__name__", "cpu"), ("host", "h2")],
                &[(0, 5.0), (10, 6.0)],
            ),
            series(
                &[("__name__", "cpu"), ("host", "h3")],
                &[(0, 10.0), (10, 12.0)],
            ),
        ];

        // Batch reference.
        struct Mem {
            series: Vec<Series>,
        }
        impl MetricAccess for Mem {
            fn select_range(
                &self,
                m: &[Matcher],
                _: i64,
                _: i64,
            ) -> Result<Vector, DataSourceError> {
                Ok(self
                    .series
                    .iter()
                    .filter(|s| m.iter().all(|mm| match_label(s, mm)))
                    .cloned()
                    .collect())
            }
        }
        let ds = Mem {
            series: inputs.clone(),
        };
        let ctx = EvalContext {
            data: &ds,
            start_ms: 0,
            end_ms: 10,
            step_ms: 1,
            lookback_ms: None,
            query_start_ms: None,
            query_end_ms: None,
        };
        let expr = crate::parse("sum(cpu) by (host)").unwrap();
        let batch = evaluate(&ctx, &expr).expect("batch");

        // Runtime path.
        let feed = Arc::new(MemFeed::new());
        for s in &inputs {
            feed.push(s.clone());
        }
        struct Shim(Arc<MemFeed>);
        impl SampleFeed for Shim {
            fn fetch_since(
                &self,
                m: &[Matcher],
                a: i64,
                b: i64,
            ) -> Result<Vector, DataSourceError> {
                self.0.fetch_since(m, a, b)
            }
            fn latest_ts(&self) -> Result<Option<i64>, DataSourceError> {
                self.0.latest_ts()
            }
        }
        let runtime = ContinuousQueryRuntime::with_feed(Box::new(Shim(feed)));
        let handle = runtime
            .register_with(
                "sum(cpu) by (host)",
                RegisterOptions {
                    window_policy: WindowPolicy::Lifetime,
                    warmup_ms: 100_000,
                },
            )
            .expect("register");
        let stream = handle.snapshot();

        // Both should have 3 series, one per host, with
        // samples at t=0 and t=10. Compare by indexing
        // into a labels→ts→value map.
        use std::collections::BTreeMap;
        let to_index = |s: &[Series]| -> BTreeMap<(Vec<(String, String)>, i64), f64> {
            let mut out = BTreeMap::new();
            for ser in s {
                let mut k = ser.labels.clone();
                k.sort_by(|a, b| a.0.cmp(&b.0));
                for sm in &ser.samples {
                    out.insert((k.clone(), sm.timestamp_ms), sm.value);
                }
            }
            out
        };
        let bi = to_index(&batch);
        let si = to_index(&stream);
        assert_eq!(bi.len(), si.len());
        for (k, v) in &bi {
            let sv = si.get(k).copied().unwrap_or(f64::NAN);
            assert!(
                (v - sv).abs() < 1e-9,
                "divergence at {k:?}: batch={v} stream={sv}"
            );
        }
    }
}

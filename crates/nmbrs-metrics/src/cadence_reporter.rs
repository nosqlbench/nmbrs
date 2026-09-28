// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Cadence reporter — single writer of windowed snapshots
//! (SRD-42 §"Wire-Up → Cadence reporter").
//!
//! On every smallest-cadence tick the reporter:
//!
//! 1. Receives the captured per-component [`MetricSet`]s from the
//!    scheduler.
//! 2. Folds them into the smallest-cadence accumulator.
//! 3. When that accumulator's interval is satisfied, seals it into an
//!    immutable `Arc<MetricSet>`, publishes it into the per-cadence
//!    store, and folds it into the next-larger cadence's prebuffer
//!    (per the streaming cascade in SRD-42 §"Streaming coalesce
//!    semantics").
//!
//! The published snapshots are read by [`crate::metrics_query::MetricsQuery`]
//! — the single read API for every consumer.
//!
//! ## Phase 7b status
//!
//! This module is the sole home for the windowed snapshot store.
//! The legacy per-component `WindowedMetrics` and `InProcessMetricsStore`
//! are deleted as of Phase 7b; all writes go through this type and
//! all reads go through [`crate::metrics_query::MetricsQuery`].

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use crate::cadence::{CadenceLayer, CadenceTree, Cadences};
use crate::labels::Labels;
use crate::scheduler::Reporter;
use crate::snapshot::{CloseReason, MetricSet};

/// Distribution (HDR-reservoir) retention, expressed as a count of
/// closed windows per `(component_path, cadence)` ring. The heavy part
/// of a snapshot is the histogram reservoir, so it is held to this many
/// windows (SRD-90: "histograms are a different matter"). `≥ max fan-in`
/// so the cascade always has its inputs. Counter/gauge sub-interval
/// history is kept *longer* and far more cheaply — see
/// [`CadenceWindow::retain`].
pub const HISTORY_RING_CAP: usize = 32;

/// SRD-90 §M1 — counter/gauge sub-interval retention floor. Cumulative
/// counters/gauges cost ~nothing to retain (a point is `(timestamp,
/// value)`), so the smallest cadence keeps at least this much wall-clock
/// of distinct sub-window points, well past the distribution bound, so a
/// short-timeframe `rate()`/`increase()`/trend read is fully qualified
/// from the running totals even when the scheduler tick drifts. Stage 4
/// will demand-derive this from the workload's declared query windows;
/// for now it is a fixed floor (raised per-window to never fall below the
/// distribution bound). The effective horizon is
/// `max(this, cadence × HISTORY_RING_CAP)`.
pub const COUNTER_RETAIN_FLOOR: Duration = Duration::from_secs(60);

/// Hard backstop on ring length, independent of the time horizons — guards
/// against a pathological dense-capture × large-horizon combination. The
/// time-based eviction is the primary bound; this only ever trips for a
/// degenerate configuration.
pub const HARD_RING_CAP: usize = 200_000;

/// Default bounded-channel capacity for subscription dispatch. A
/// subscriber that can't drain this many snapshots in
/// `2 × cadence_interval` has fallen behind enough to trip the
/// timeout path.
pub const DEFAULT_SUBSCRIPTION_CHANNEL_CAPACITY: usize = 8;

// =========================================================================
// CadenceReporter — the single writer
// =========================================================================

/// Per-component, per-cadence streaming accumulator + retention ring.
///
/// One instance per `(component_path, cadence)` pair. Owns:
///
/// - the in-flight prebuffer for this cadence (accumulating folds
///   from the next-smaller cadence — or the smallest cadence's
///   accumulating window when this is the smallest cadence),
/// - the most-recently-closed snapshot, exposed as `Arc` to readers,
/// - a bounded ring of past closed snapshots for `increase_over` and
///   `past(span)` queries.
struct CadenceWindow {
    cadence: Duration,
    /// Accumulated duration in the prebuffer.
    accumulated: Duration,
    /// In-flight prebuffer being assembled toward the next close.
    /// `None` until the first input arrives.
    prebuffer: Option<MetricSet>,
    /// Latest closed (immutable) snapshot.
    latest: Option<Arc<MetricSet>>,
    /// Time-bounded ring of past closed snapshots, newest at the back
    /// (SRD-90 §M1). Counter/gauge points are kept up to [`Self::retain`];
    /// the heavy distribution families are stripped (via
    /// [`MetricSet::without_distributions`]) once a window ages past
    /// [`Self::hist_retain`], so distribution memory matches the old
    /// `HISTORY_RING_CAP` bound while cheap cumulative history runs longer.
    ring: VecDeque<Arc<MetricSet>>,
    /// Counter/gauge retention horizon (relative to the newest closed
    /// window's `captured_at`).
    retain: Duration,
    /// Distribution (HDR-reservoir) retention horizon — `≤ retain`.
    hist_retain: Duration,
}

impl CadenceWindow {
    fn new(cadence: Duration) -> Self {
        // Distributions: keep the historical `HISTORY_RING_CAP` windows'
        // worth of wall-clock (no regression, no balloon). Counters/gauges:
        // at least the floor, never below the distribution bound. `saturating`
        // so a lifetime-scale cadence (interval ≥ session) doesn't overflow —
        // its ring stays empty anyway (it accumulates in the prebuffer).
        let hist_retain = cadence.saturating_mul(HISTORY_RING_CAP as u32);
        let retain = COUNTER_RETAIN_FLOOR.max(hist_retain);
        Self {
            cadence,
            accumulated: Duration::ZERO,
            prebuffer: None,
            latest: None,
            ring: VecDeque::new(),
            retain,
            hist_retain,
        }
    }

    /// Evict and compact the ring after a new window was pushed at the back
    /// (SRD-90 §M1). Time-bounded relative to the newest window's
    /// `captured_at` (monotonic `Instant`, so independent of wall-clock and
    /// of idle gaps): drop whole windows past `retain`, and strip the heavy
    /// distribution families from windows past `hist_retain` (keeping their
    /// cheap counter/gauge points). A hard count cap is the final backstop.
    fn evict_and_compact(&mut self) {
        let Some(newest) = self.ring.back().map(|w| w.captured_at()) else {
            return;
        };
        // 1. Drop whole windows older than the counter/gauge horizon.
        if let Some(cutoff) = newest.checked_sub(self.retain) {
            while self.ring.front().is_some_and(|w| w.captured_at() < cutoff) {
                self.ring.pop_front();
            }
        }
        // 2. Compact: past the distribution horizon, keep only counter/gauge
        //    points. Already-stripped windows have no distributions, so this is
        //    amortized to the few windows that newly cross the boundary.
        if self.retain > self.hist_retain
            && let Some(hist_cutoff) = newest.checked_sub(self.hist_retain)
        {
            for slot in self.ring.iter_mut() {
                if slot.captured_at() >= hist_cutoff {
                    break; // ring is time-ordered; nothing newer needs stripping
                }
                if slot.has_distributions() {
                    *slot = Arc::new(slot.without_distributions());
                }
            }
        }
        // 3. Backstop.
        while self.ring.len() > HARD_RING_CAP {
            self.ring.pop_front();
        }
    }

    /// Fold an incoming snapshot into the prebuffer. Returns the
    /// just-closed snapshot if this fold completed the cadence's
    /// interval (caller propagates upstream), else `None`.
    fn ingest(&mut self, snapshot: MetricSet) -> Option<Arc<MetricSet>> {
        self.accumulated += snapshot.interval();
        match &mut self.prebuffer {
            None => self.prebuffer = Some(snapshot),
            Some(buf) => {
                let merged = MetricSet::coalesce(&[buf.clone(), snapshot]);
                *buf = merged;
            }
        }

        if self.accumulated >= self.cadence {
            let mut closed = self.prebuffer.take().expect("prebuffer present after fold");
            // Stamp the cadence interval explicitly so consumers
            // never see "interval grew past cadence" rounding.
            closed.set_interval(self.cadence);
            let arc = Arc::new(closed);
            self.latest = Some(arc.clone());
            self.ring.push_back(arc.clone());
            self.evict_and_compact();
            self.accumulated = Duration::ZERO;
            Some(arc)
        } else {
            None
        }
    }

    /// Force-close whatever's accumulated so the trailing partial
    /// window is published with `interval < cadence`. Used at
    /// shutdown per SRD-42 §"Streaming coalesce → Shutdown" and at
    /// component teardown via `close_path` (SRD-42 §"Component
    /// lifecycle: scope_close flush"). The published snapshot is
    /// stamped `partial=true` so downstream consumers can
    /// distinguish a scope-close contribution from a naturally
    /// pulse-flushed window.
    fn force_close(&mut self, reason: CloseReason) -> Option<Arc<MetricSet>> {
        let mut buf = self.prebuffer.take()?;
        if buf.is_empty() {
            return None;
        }
        // `interval` already reflects accumulated time from the
        // input snapshots' interval sum. Stamp the partial flag —
        // by definition force_close fires before the cadence
        // window naturally closed — and the typed lifecycle reason
        // (SRD-93 M4) so durable sinks act on the reason instead of
        // inferring lifecycle from `partial`.
        buf.mark_partial();
        buf.mark_close(reason);
        let arc = Arc::new(buf);
        self.latest = Some(arc.clone());
        self.ring.push_back(arc.clone());
        self.evict_and_compact();
        self.accumulated = Duration::ZERO;
        Some(arc)
    }

    /// Latest closed snapshot for this cadence, if any.
    fn latest(&self) -> Option<Arc<MetricSet>> {
        self.latest.clone()
    }

    /// Read-only ring of past closed snapshots, oldest first.
    fn ring(&self) -> impl Iterator<Item = &Arc<MetricSet>> {
        self.ring.iter()
    }

    /// Read-only access to the in-flight prebuffer (clone-friendly
    /// since `MetricSet` is `Clone`). Used by `session_lifetime` to
    /// peek partials without disturbing the cascade.
    fn prebuffer_clone(&self) -> Option<MetricSet> {
        self.prebuffer.clone()
    }
}

// =========================================================================
// Subscriptions — async push dispatch (SRD-42 §"Notification dispatch")
// =========================================================================

/// Opaque identifier for a subscription registered via
/// [`CadenceReporter::subscribe`]. Used to [`unsubscribe`](CadenceReporter::unsubscribe)
/// the subscriber later.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriberId(u64);

/// Errors returned by [`CadenceReporter::subscribe`].
#[derive(Debug)]
pub enum SubscribeError {
    /// The requested cadence is not one of the reporter's declared
    /// or hidden layer intervals.
    UnknownCadence(Duration),
    /// Failed to spawn the dispatch thread for the subscription.
    SpawnFailed(String),
}

impl std::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCadence(d) => write!(f, "cadence {d:?} is not a layer of this reporter"),
            Self::SpawnFailed(e) => write!(f, "failed to spawn subscription dispatch thread: {e}"),
        }
    }
}

impl std::error::Error for SubscribeError {}

/// Wraps a delivery fiber's future so it runs inside a caller-supplied
/// context (e.g. nmbrs-runtime's per-execution `ExecutionContext` scope),
/// since that context is a layer above this crate. Identity when a
/// subscriber has no context to apply (session-level reporters). Applied
/// to every ephemeral per-notification delivery fiber.
pub type ContextWrap = Arc<
    dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Per-subscription configuration.
#[derive(Clone)]
pub struct SubscriptionOpts {
    /// Bounded channel capacity for this subscription. When the
    /// channel is full, `try_send` fails and the snapshot is dropped
    /// for this subscriber (cascade continues). Default:
    /// [`DEFAULT_SUBSCRIPTION_CHANNEL_CAPACITY`].
    pub channel_capacity: usize,
    /// Timeout — if `Instant::now() - last_successful_delivery`
    /// exceeds this, fire `on_timeout` once and arm re-firing only
    /// after the next successful delivery. Default: `2 ×
    /// cadence_interval` per SRD-42.
    pub timeout: Option<Duration>,
    /// Called (off the scheduler thread, on the dispatch thread)
    /// when `timeout` expires. Receives a [`TimeoutEvent`] with the
    /// cadence, subscriber id, undelivered-snapshot age, and
    /// consecutive-drop count so the cadence manager can decide
    /// whether to log, escalate, or unsubscribe.
    pub on_timeout: Option<TimeoutCallback>,
    /// Wraps each delivery fiber so it runs in the subscriber's context
    /// (SRD-88 — the optimizer settle subscriber carries its execution
    /// context so its objective read scopes to its own `exec_id`).
    /// `None` ⇒ session-level subscriber, delivered in the bare runtime.
    pub context_wrap: Option<ContextWrap>,
}

impl Default for SubscriptionOpts {
    fn default() -> Self {
        Self {
            channel_capacity: DEFAULT_SUBSCRIPTION_CHANNEL_CAPACITY,
            timeout: None,
            on_timeout: None,
            context_wrap: None,
        }
    }
}

/// Error callback invoked when a subscription's delivery timeout
/// expires. Runs on the dispatcher thread — must not block.
pub type TimeoutCallback = Arc<dyn Fn(TimeoutEvent) + Send + Sync>;

/// Delivery-timeout event surfaced to a subscription's
/// [`SubscriptionOpts::on_timeout`] callback.
#[derive(Clone, Debug)]
pub struct TimeoutEvent {
    pub subscriber_id: SubscriberId,
    pub cadence: Duration,
    /// Age of the snapshot that failed to deliver (relative to
    /// `Instant::now()`).
    pub snapshot_age: Duration,
    /// Count of consecutive `try_send` failures for this subscription
    /// since the last successful delivery.
    pub consecutive_drops: u64,
}

/// Per-subscription mutable bookkeeping. Shared between the reporter
/// (which calls `try_send`) and the dispatch thread (which updates
/// `last_delivered`).
struct SubscriptionState {
    last_delivered: Mutex<Instant>,
    consecutive_drops: AtomicU64,
    timeout_fired: AtomicBool,
    /// Snapshots successfully queued to this subscriber's channel by
    /// the owner (incremented on each `try_send` success). Paired with
    /// [`Self::delivered`] so [`CadenceReporter::quiesce`] can wait for
    /// the dispatch worker to drain everything the owner queued —
    /// without tearing the subscriber down (SRD-88: per-execution
    /// flush-to-store, non-terminal).
    sent: AtomicU64,
    /// Snapshots a delivery fiber has `report()`ed (committed to the
    /// sink). Converges to [`Self::sent`] once all in-flight fibers run.
    delivered: AtomicU64,
    /// Set by a delivery fiber once `Reporter::finished()` returns true
    /// (a one-shot subscriber — e.g. a settle evaluator that reached a
    /// terminal disposition). The owner stops spawning delivery fibers
    /// for a finished subscription; it's reaped by `unsubscribe`.
    finished: AtomicBool,
}

impl SubscriptionState {
    fn new() -> Self {
        Self {
            last_delivered: Mutex::new(Instant::now()),
            consecutive_drops: AtomicU64::new(0),
            timeout_fired: AtomicBool::new(false),
            sent: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
            finished: AtomicBool::new(false),
        }
    }

    fn mark_delivered(&self) {
        if let Ok(mut t) = self.last_delivered.lock() {
            *t = Instant::now();
        }
        self.consecutive_drops.store(0, Ordering::Relaxed);
        self.timeout_fired.store(false, Ordering::Relaxed);
    }

    /// Snapshots queued but not yet committed by the dispatch worker.
    fn pending(&self) -> u64 {
        self.sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.delivered.load(Ordering::Relaxed))
    }
}

/// A registered subscription. Holds the sender the cascade writes to
/// plus bookkeeping for timeout escalation.
struct Subscription {
    id: SubscriberId,
    cadence: Duration,
    /// The sink, shared with the ephemeral delivery fibers. Each delivery
    /// `lock().await`s it, so windows are delivered **serialized and in
    /// order, losslessly** (a durable sink — e.g. SQLite — must not drop
    /// windows). Backpressure is bounded at the fanout: while
    /// `pending()` (in-flight deliveries) is at `channel_capacity`, new
    /// windows are dropped + the stall timeout escalates — the same
    /// bounded-then-lossy contract the channel had. `flush` runs once, at
    /// unsubscribe/shutdown (after `quiesce`, so the lock is uncontended).
    reporter: Arc<tokio::sync::Mutex<Box<dyn Reporter>>>,
    /// Wraps each delivery fiber in the subscriber's context (`None` for
    /// session-level subscribers).
    context_wrap: Option<ContextWrap>,
    state: Arc<SubscriptionState>,
    opts: SubscriptionOpts,
}

/// Run a brief blocking wait without starving the runtime: inside a
/// multi-threaded runtime, `block_in_place` lets tokio spawn a
/// replacement worker so the tasks/fibers this wait depends on (the
/// owner task, delivery fibers) still get scheduled. Outside a runtime,
/// runs `f` directly. The lifecycle waits (`quiesce`, `flush_for_tests`,
/// the scheduler's done-signal) are session-end / test-only, not the
/// hot path.
pub(crate) fn block_compensated<R>(f: impl FnOnce() -> R) -> R {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(f)
    } else {
        f()
    }
}

/// Component path key — derived from a `Labels` value by joining
/// every `(k, v)` pair into a stable string.
pub fn component_path_of(labels: &Labels) -> String {
    let mut parts: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.sort_unstable();
    parts.join(",")
}

// =========================================================================
// Lock-free architecture (SRD-42 §"Lock-free consolidation lifecycle")
// =========================================================================
//
// The reporter follows the single-writer actor pattern:
//
// - All mutation of the windows map happens inside a single owner
//   thread that drains a non-blocking command channel.
// - Public `ingest`/`close_path` calls send commands and return
//   immediately (or, for `close_path`, optionally wait on an ack
//   channel — the deterministic-publish lifecycle boundary).
// - Reads (`latest`/`prebuffer`/`ring`/`component_labels`) hit an
//   `ArcSwap<ReaderState>` that the owner publishes after each
//   batch of commands. No lock; readers never block writers and
//   writers never block readers.
//
// The historic `windows: RwLock<HashMap<…>>` and
// `component_labels_by_path: RwLock<HashMap<…>>` are gone —
// those caused the post-fiber phase-transition hangs when a TUI
// reader's read guard happened to coincide with an ingest's
// write request.

/// Snapshot of one window's externally-observable state, published
/// atomically by the owner thread for read paths. Exposed `pub(crate)` so a
/// multi-field reader (e.g. `MetricsQueryAccess::select_range`) can fetch the
/// whole view with ONE lookup ([`CadenceReporter::window_view`]) instead of
/// paying a component-path build + siphash per field.
#[derive(Clone)]
pub(crate) struct WindowReaderView {
    pub(crate) latest: Option<Arc<MetricSet>>,
    pub(crate) prebuffer: Option<Arc<MetricSet>>,
    pub(crate) ring: Arc<Vec<Arc<MetricSet>>>,
}

impl Default for WindowReaderView {
    fn default() -> Self {
        Self {
            latest: None,
            prebuffer: None,
            ring: Arc::new(Vec::new()),
        }
    }
}

/// Atomically-published reader state. The owner thread builds a
/// fresh instance after each command batch and stores it via
/// [`ArcSwap`]. Readers do `state.load_full()` — one atomic load,
/// one Arc clone, no lock.
#[derive(Default)]
struct ReaderState {
    component_labels: HashMap<String, Labels>,
    windows: HashMap<(String, usize), Arc<WindowReaderView>>,
}

/// Commands sent to the owner thread.
///
/// Every variant carries an optional `ack` channel. Public APIs
/// that want deterministic-publish semantics (current callers
/// effectively all do — every ingest is at a phase-boundary, not
/// the cycle hot path) attach an ack and wait for it before
/// returning. The owner sends `()` after the corresponding state
/// is published to the reader-state ArcSwap.
enum Cmd {
    Ingest {
        path: String,
        labels: Labels,
        snapshot: MetricSet,
        ack: Option<crossbeam_channel::Sender<()>>,
    },
    ClosePath {
        path: String,
        ack: Option<crossbeam_channel::Sender<()>>,
        /// SRD-93 M4 — the lifecycle reason stamped on the sealed
        /// windows (`ScopeClose` from `scope_close`, `Quiesce` from
        /// a bare `close_path` boundary seal).
        reason: CloseReason,
    },
    /// Force-close every prebuffer and fan out to every subscriber.
    /// Used by `shutdown_flush` (sync) and `quiesce` (async); the sender
    /// waits on `ack`, which may be a blocking (sync) or awaitable (async)
    /// channel so the same command serves both runtime flavors.
    ShutdownFlushAll {
        ack: FlushAck,
        /// SRD-93 M4 — `Shutdown` from `shutdown_flush`, `Quiesce`
        /// from `quiesce`; the owner can no longer conflate the two.
        reason: CloseReason,
    },
    /// No-op barrier used by tests / explicit synchronization
    /// callers. The owner thread acks after publishing the
    /// reader-state, guaranteeing all prior FIFO commands have
    /// been processed.
    Barrier { ack: crossbeam_channel::Sender<()> },
}

/// An acknowledgement sink the owner signals after publishing reader state.
///
/// `Sync` is a `crossbeam_channel` the caller blocks on (multi-threaded /
/// out-of-runtime callers, via [`block_compensated`]); `Async` is a
/// `tokio::sync::oneshot` the caller `.await`s — required on a current-thread
/// runtime, where a blocking wait would either panic (`block_in_place`) or
/// deadlock the single thread against the owner task it's waiting for.
enum FlushAck {
    Sync(crossbeam_channel::Sender<()>),
    Async(tokio::sync::oneshot::Sender<()>),
}

impl FlushAck {
    /// Signal completion. Send errors (receiver dropped — caller gave up /
    /// timed out) are ignored: the ack is best-effort.
    fn signal(self) {
        match self {
            FlushAck::Sync(s) => {
                let _ = s.send(());
            }
            FlushAck::Async(s) => {
                let _ = s.send(());
            }
        }
    }
}

/// The cadence reporter: per-component + per-cadence accumulator,
/// store, and ring.
pub struct CadenceReporter {
    /// The cadence tree the scheduler is built against. Layers in
    /// order from smallest to largest.
    layers: Vec<CadenceLayer>,
    /// User-declared cadences (subset of `layers`, in declaration
    /// order). Used by consumers that want to enumerate
    /// human-visible cadences.
    declared: Cadences,
    /// Owner-thread command channel. Non-blocking from the writer's
    /// perspective (unbounded; allocations bounded by the ingest
    /// rate, which is one message per phase boundary in practice —
    /// not the cycle hot path).
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    /// Atomically-published reader state. Owner publishes; readers
    /// `load_full`.
    state: Arc<ArcSwap<ReaderState>>,
    /// Active subscriptions, keyed by subscriber id. Each
    /// subscription has a dedicated dispatch thread and a bounded
    /// channel so a slow subscriber can never stall the cascade.
    /// Shared `Arc<Mutex<…>>` because the owner thread reads it
    /// during fanout and the public subscribe/unsubscribe API
    /// writes it from the caller thread. Subscribe/unsubscribe is
    /// not on the hot path — the brief mutex acquire there is
    /// fine.
    subscriptions: Arc<Mutex<HashMap<SubscriberId, Subscription>>>,
    /// Monotonic id generator for subscriptions.
    next_subscriber_id: AtomicU64,
    /// Run start instant — used by `session_lifetime` queries to
    /// build the result's `interval`.
    started_at: Instant,
    /// Owner **task** join handle (the single-writer actor runs as a
    /// tokio task on the shared runtime — no dedicated thread). Aborted
    /// in `Drop`; it also self-terminates when `cmd_tx` is dropped and
    /// its `recv()` returns `None`.
    owner_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CadenceReporter {
    /// Build a new reporter from a planned cadence tree.
    ///
    /// Spawns the owner thread that processes ingest/close commands
    /// in serial order. Public `ingest`/`close_path` calls send via
    /// `cmd_tx` and never take any lock related to the windows map.
    pub fn new(tree: CadenceTree) -> Self {
        let layers = tree.layers().to_vec();
        let declared = tree.declared().clone();

        let state: Arc<ArcSwap<ReaderState>> =
            Arc::new(ArcSwap::from_pointee(ReaderState::default()));
        let subscriptions: Arc<Mutex<HashMap<SubscriberId, Subscription>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Unbounded sender so `ingest` never blocks. Backpressure
        // (if needed) is the user's responsibility — but with the
        // typical "one ingest per phase boundary" pattern the
        // queue depth stays trivially bounded.
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();

        let owner_layers = layers.clone();
        let owner_state = state.clone();
        let owner_subs = subscriptions.clone();
        // The single-writer actor runs as a tokio task on the shared
        // runtime — no dedicated thread. Requires a runtime to be active
        // at construction (always true in production; tests run under
        // `#[tokio::test(multi_thread)]`). Delivery fibers it spawns use
        // the ambient runtime (it runs on it), so no handle to thread
        // through.
        let owner_task = tokio::spawn(run_owner(cmd_rx, owner_layers, owner_state, owner_subs));

        Self {
            layers,
            declared,
            cmd_tx,
            state,
            subscriptions,
            next_subscriber_id: AtomicU64::new(1),
            started_at: Instant::now(),
            owner_task: Mutex::new(Some(owner_task)),
        }
    }

    /// All layers (declared + hidden), smallest first.
    pub fn layers(&self) -> &[CadenceLayer] {
        &self.layers
    }

    /// User-declared cadences in original order.
    pub fn declared_cadences(&self) -> &Cadences {
        &self.declared
    }

    /// Wall-clock instant the reporter was constructed (run start).
    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    /// Ingest a per-component snapshot at the smallest cadence.
    /// Cascades the close-then-promote chain per SRD-42, fanning out
    /// every closed snapshot to subscribers at that cadence via
    /// [`Self::subscribe`].
    ///
    /// **Fire-and-forget, lock-free, never blocks**: enqueues an
    /// `Ingest` command and returns immediately. The owner thread
    /// processes commands in FIFO order, so any subsequent call
    /// (`close_path`, another `ingest`, etc.) will observe this
    /// command's effects in turn. `latest`/`prebuffer`/`ring` may
    /// race against an in-flight ingest — that's the trade-off
    /// for keeping the writer fully non-blocking on the tokio
    /// hot path. Tests that need synchronous semantics call
    /// [`Self::flush_for_tests`].
    ///
    /// CRITICAL: this method must NEVER block on a sync primitive.
    /// Earlier versions waited on a `crossbeam_channel::Receiver`
    /// for an ack — that blocks the OS thread (and therefore a
    /// tokio worker if called from async context), which led to
    /// runtime starvation and the post-fiber drain-loop hang.
    /// See `feedback_no_blocking_in_async.md` in the project
    /// memory.
    pub fn ingest(&self, labels: &Labels, snapshot: MetricSet) {
        let path = component_path_of(labels);
        // Unbounded sender — never blocks; only fails if the owner
        // thread has dropped (during shutdown). In that case
        // dropping the snapshot is correct behavior.
        let _ = self.cmd_tx.send(Cmd::Ingest {
            path,
            labels: labels.clone(),
            snapshot,
            ack: None,
        });
    }

    /// Test-only synchronous barrier. Sends a `Barrier` command
    /// and waits for the owner to ack — a guarantee that every
    /// prior FIFO command has been processed AND its effects are
    /// visible via [`Self::latest`] / [`Self::ring`] /
    /// [`Self::prebuffer`].
    ///
    /// **Production code MUST NOT call this** — it blocks the
    /// OS thread, which negates the lock-free design of
    /// [`Self::ingest`] / [`Self::close_path`]. Production
    /// callers don't need the synchronous barrier; the owner
    /// processes commands in FIFO order, so a follow-up call
    /// (next phase's ingest, shutdown_flush, etc.) sees prior
    /// effects naturally.
    pub fn flush_for_tests(&self) {
        let (ack_tx, ack_rx) = crossbeam_channel::bounded::<()>(1);
        let _ = self.cmd_tx.send(Cmd::Barrier { ack: ack_tx });
        // The owner is a runtime task; block_in_place so it gets a worker
        // to process the barrier while this call waits.
        block_compensated(|| {
            let _ = ack_rx.recv();
        });
    }

    /// Register a push subscriber for the given cadence. Each closed
    /// window is delivered to it on an **ephemeral fiber** spawned by the
    /// owner — no dedicated dispatch thread sits parked per subscriber.
    /// Returns a [`SubscriberId`] for later [`Self::unsubscribe`].
    ///
    /// `cadence` MUST match one of this reporter's declared or
    /// hidden layer intervals — unknown cadences return an error.
    pub fn subscribe(
        self: &Arc<Self>,
        cadence: Duration,
        reporter: Box<dyn Reporter>,
        mut opts: SubscriptionOpts,
    ) -> Result<SubscriberId, SubscribeError> {
        if !self.layers.iter().any(|l| l.interval == cadence) {
            return Err(SubscribeError::UnknownCadence(cadence));
        }
        if opts.timeout.is_none() {
            opts.timeout = Some(cadence.saturating_mul(2));
        }

        let id = SubscriberId(self.next_subscriber_id.fetch_add(1, Ordering::Relaxed));
        let context_wrap = opts.context_wrap.clone();
        let sub = Subscription {
            id,
            cadence,
            reporter: Arc::new(tokio::sync::Mutex::new(reporter)),
            context_wrap,
            state: Arc::new(SubscriptionState::new()),
            opts,
        };
        self.subscriptions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sub);
        Ok(id)
    }

    /// Drop a subscription, flushing its sink on the way out. No worker
    /// thread to join — delivery is via ephemeral fibers, which at a
    /// lifecycle boundary (after this execution's `quiesce` drained the
    /// in-flight fibers) are no longer running, so the `flush` lock is
    /// uncontended.
    pub fn unsubscribe(&self, id: SubscriberId) {
        let sub = {
            let mut map = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
            map.remove(&id)
        };
        if let Some(sub) = sub {
            if let Ok(mut g) = sub.reporter.try_lock() {
                g.flush();
            }
        }
    }

    /// Full shutdown: flush trailing partials through the cascade, fan
    /// them out, then flush every subscriber sink. Callers that read
    /// from a reporter sink (e.g. SQLite for a summary report) MUST call
    /// this before reading — otherwise the last window of data is still
    /// in transit.
    pub async fn shutdown(&self) {
        // Force-close + fan out trailing partials, then let any in-flight
        // delivery fibers land (bounded so a stuck sink can't hang teardown).
        // `quiesce` both flushes and drains, awaiting the owner/fibers — so
        // this works on a current-thread runtime too.
        self.quiesce(Duration::from_secs(5)).await;
        let subs = {
            let mut map = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *map)
        };
        for (_id, sub) in subs {
            if let Ok(mut g) = sub.reporter.try_lock() {
                g.flush();
            }
        }
    }

    /// Close every window for the given path, promote partials up the
    /// cascade, and fan the resulting closed snapshots out to every
    /// subscriber. Intended to be called at phase-end lifecycle
    /// boundaries — once a phase's labels will never receive another
    /// ingest, the window may as well publish now instead of idling
    /// until the next cadence tick.
    ///
    /// **Fire-and-forget, lock-free, never blocks**: same contract
    /// as [`Self::ingest`]. The deterministic-publish guarantee at
    /// session shutdown is provided by [`Self::shutdown_flush`],
    /// which is the only call in the lifecycle that's allowed to
    /// block — and it's called once at the end of the run, not
    /// from any tokio worker on the hot path.
    pub fn close_path(&self, labels: &Labels) {
        // A bare path seal is a boundary flush, not a component
        // teardown — `Quiesce`, never an exit signal (SRD-93 A6).
        self.close_path_reason(labels, CloseReason::Quiesce);
    }

    fn close_path_reason(&self, labels: &Labels, reason: CloseReason) {
        let path = component_path_of(labels);
        let _ = self.cmd_tx.send(Cmd::ClosePath {
            path,
            ack: None,
            reason,
        });
    }

    /// SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close flush".
    /// Fused teardown helper: stamp `partial_delta` as `partial=true`,
    /// ingest it into the smallest cadence (so a Counter/Gauge/Histogram
    /// fold uses the standard combine rules), and close_path so the
    /// trailing partial promotes through the cascade immediately.
    ///
    /// This is the canonical API for short-lived components (phases
    /// shorter than the smallest cadence, op-dispensers torn down
    /// between cycles, scope-bounded Polydat Kernels). The legacy idiom
    /// — manual `ingest(delta)` + `close_path(labels)` — still
    /// works; this helper just packages the recipe and adds the
    /// `partial=true` annotation that SRD-42 mandates.
    ///
    /// Same fire-and-forget contract as [`Self::ingest`] /
    /// [`Self::close_path`]: never blocks, never takes a lock the
    /// caller can race against. Empty deltas are still passed
    /// through close_path so any in-flight prebuffer for the path
    /// gets force-closed even when this scope contributed nothing.
    pub fn scope_close(&self, labels: &Labels, mut partial_delta: MetricSet) {
        partial_delta.mark_partial();
        // Stamp the reason on the DELTA too (not just the ClosePath):
        // if the delta's ingest happens to seal the smallest window at
        // its natural cadence boundary before the ClosePath lands, the
        // published window still inherits ScopeClose through the
        // coalesce fold — no exit-scope signal lost to that race.
        partial_delta.mark_close(CloseReason::ScopeClose);
        if !partial_delta.is_empty() {
            self.ingest(labels, partial_delta);
        }
        self.close_path_reason(labels, CloseReason::ScopeClose);
    }

    /// Force-close every prebuffer in cascade order at shutdown.
    /// Trailing partials are published with `interval < cadence`,
    /// and crucially — fanned out to every subscriber at their
    /// cadence so sinks like the SQLite reporter actually see the
    /// final window of activity.
    ///
    /// **Blocks the calling thread** until the owner thread has
    /// processed every queued command (every prior ingest /
    /// close_path) AND the final force-close has fanned out. This
    /// is the ONLY place in the public API that synchronously
    /// waits, and it's intended to be called once at session end
    /// from the tokio runtime's main task, AFTER all other tokio
    /// tasks have completed — so blocking the OS thread for the
    /// duration of the flush is safe.
    pub async fn shutdown_flush(&self) {
        // AWAIT the owner's ack rather than blocking — the owner is a tokio
        // task sharing the runtime, so on a current-thread runtime a blocking
        // wait would deadlock against (or `block_in_place`-panic before) the
        // very task that produces the ack. Awaiting yields the thread so the
        // owner runs. Callers (the scheduler's final flush) are async.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(Cmd::ShutdownFlushAll {
            ack: FlushAck::Async(ack_tx),
            reason: CloseReason::Shutdown,
        });
        let _ = ack_rx.await;
    }

    /// SRD-88 — **non-terminal** flush-to-store. Force-close every
    /// window through the cascade, fan the trailing partials out to
    /// every subscriber, and **wait until each subscriber's dispatch
    /// worker has committed them to its sink** — but leave every
    /// subscriber alive and subscribed.
    ///
    /// This is the per-execution analogue of [`Self::shutdown`]: an
    /// execution sharing a session's reporter calls this at its end so
    /// a report can run against the complete metrics store for its
    /// workload, WITHOUT tearing the reporter down for the concurrent
    /// siblings. The session-tier [`Self::shutdown`] (drain + join
    /// subscribers) still runs once, at session end.
    ///
    /// Blocks the calling thread. Intended to be called at an
    /// execution boundary from the runtime's main task, not the hot
    /// path. Bounded by `max_wait` so a stalled sink can't hang the
    /// run — on timeout it returns with whatever has been committed
    /// (same lossy-under-backpressure contract as the `try_send`
    /// cascade).
    pub async fn quiesce(&self, max_wait: Duration) {
        // 1. Force-close all paths + fan out, then AWAIT the owner's ack.
        //    Awaiting (not blocking) is what makes this work on a
        //    current-thread runtime: the owner task and delivery fibers
        //    share the thread, so a blocking wait would deadlock / panic.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        let _ = self.cmd_tx.send(Cmd::ShutdownFlushAll {
            ack: FlushAck::Async(ack_tx),
            reason: CloseReason::Quiesce,
        });
        let _ = ack_rx.await;
        // 2. Wait for every in-flight delivery fiber to land what was
        //    queued, yielding between polls so the fibers get scheduled.
        //    Bounded by `max_wait` so a stalled sink can't hang the run.
        let start = Instant::now();
        loop {
            let pending: u64 = {
                let map = self.subscriptions.lock().unwrap_or_else(|e| e.into_inner());
                map.values().map(|s| s.state.pending()).sum()
            };
            if pending == 0 || start.elapsed() >= max_wait {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    /// Latest closed snapshot for `(labels, cadence)`, if any.
    /// **Lock-free**: one atomic `ArcSwap::load_full` + a HashMap
    /// lookup against the published reader state.
    pub fn latest(&self, labels: &Labels, cadence: Duration) -> Option<Arc<MetricSet>> {
        let path = component_path_of(labels);
        let idx = self.layer_index(cadence)?;
        let state = self.state.load_full();
        state.windows.get(&(path, idx))?.latest.clone()
    }

    /// Clone of the in-flight prebuffer for `(labels, cadence)`.
    /// Used by `session_lifetime` to peek partials without
    /// disturbing the cascade.
    pub fn prebuffer(&self, labels: &Labels, cadence: Duration) -> Option<MetricSet> {
        let path = component_path_of(labels);
        let idx = self.layer_index(cadence)?;
        let state = self.state.load_full();
        state
            .windows
            .get(&(path, idx))?
            .prebuffer
            .as_ref()
            .map(|arc| (**arc).clone())
    }

    /// Read the past-snapshot ring for `(labels, cadence)`. Returns
    /// the snapshots oldest-first so callers can `rev().take(n)` to
    /// merge the most-recent N.
    pub fn ring(&self, labels: &Labels, cadence: Duration) -> Vec<Arc<MetricSet>> {
        let path = component_path_of(labels);
        let Some(idx) = self.layer_index(cadence) else {
            return Vec::new();
        };
        let state = self.state.load_full();
        state
            .windows
            .get(&(path, idx))
            .map(|w| (*w.ring).clone())
            .unwrap_or_default()
    }

    /// The whole published window view for `(labels, cadence)` in ONE lock-free
    /// lookup. A reader that needs more than one of latest/prebuffer/ring should
    /// use this and read the fields off the returned view, rather than calling
    /// `latest` + `prebuffer` + `ring` separately — each of those rebuilds the
    /// component-path string and re-hashes it, so three calls pay that cost
    /// three times for the same key.
    pub(crate) fn window_view(
        &self,
        labels: &Labels,
        cadence: Duration,
    ) -> Option<Arc<WindowReaderView>> {
        let path = component_path_of(labels);
        let idx = self.layer_index(cadence)?;
        self.state.load_full().windows.get(&(path, idx)).cloned()
    }

    /// All `(component_labels)` keys currently tracked.
    pub fn component_labels(&self) -> Vec<Labels> {
        let state = self.state.load_full();
        state.component_labels.values().cloned().collect()
    }

    fn layer_index(&self, cadence: Duration) -> Option<usize> {
        self.layers.iter().position(|l| l.interval == cadence)
    }
}

// =========================================================================
// MetricSink — the write-side dual of MetricAccess (SRD-90 §M7 / A7)
// =========================================================================

/// The write-side dual of [`crate::queryapi::MetricAccess`]: accept a labeled,
/// timestamped snapshot for storage. One trait, used at every submit point.
///
/// SRD-90 §A7/M7. The [`CadenceReporter`] implements it at its **inlet**
/// (`submit` = [`CadenceReporter::ingest`]) — that is where the scheduler hands
/// snapshots in. The **durable / coarse fan-out sink** role (sqlite, console,
/// csv, VM) is the existing [`Reporter`](crate::scheduler::Reporter) trait, fed
/// *coalesced cadence windows* by the subscription dispatch — so sqlite stays
/// downstream and coarse, never fed raw sub-interval ticks (A7-i). `submit` is
/// **non-blocking** (A7-ii): the cadence inlet is a lock-free actor enqueue, and
/// any durable serialization is a sink's own implementation detail, never
/// imposed on the contract.
pub trait MetricSink: Send + Sync {
    /// Submit a snapshot for the component identified by `labels`. Non-blocking.
    fn submit(&self, labels: &Labels, snapshot: MetricSet);
    /// Flush any buffered state at a lifecycle boundary. Default no-op — the
    /// cadence inlet's durability is the session lifecycle
    /// (`shutdown_flush`/`quiesce`); a durable sink overrides to checkpoint.
    fn flush(&self) {}
}

impl MetricSink for CadenceReporter {
    fn submit(&self, labels: &Labels, snapshot: MetricSet) {
        self.ingest(labels, snapshot);
    }
}

/// Compose writes: submit one snapshot to **every** interior sink — the
/// write-side dual of `HybridStore` composing reads. A producer fans a snapshot
/// to, e.g., the cadence inlet plus a future direct durable sink, with one call.
pub struct FanOutSink {
    sinks: Vec<Arc<dyn MetricSink>>,
}

impl FanOutSink {
    pub fn new(sinks: Vec<Arc<dyn MetricSink>>) -> Self {
        Self { sinks }
    }
}

impl MetricSink for FanOutSink {
    fn submit(&self, labels: &Labels, snapshot: MetricSet) {
        // Each sink may retain the snapshot, so hand each its own clone.
        for sink in &self.sinks {
            sink.submit(labels, snapshot.clone());
        }
    }
    fn flush(&self) {
        for sink in &self.sinks {
            sink.flush();
        }
    }
}

impl Drop for CadenceReporter {
    fn drop(&mut self) {
        // Disconnect the command channel so the owner task's `recv()`
        // returns `None` and it exits. We can't drop `self.cmd_tx`
        // directly (it's a field), so swap in a fresh sender whose
        // receiver is immediately dropped; the original `cmd_tx` drops
        // here, dropping the last sender of the owner's receiver.
        let (dummy_tx, dummy_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        drop(dummy_rx);
        let _ = std::mem::replace(&mut self.cmd_tx, dummy_tx);

        // Abort the owner task (it's exiting on its own now too). We're
        // in a sync `drop`, so we can't await it; abort is immediate.
        if let Ok(mut guard) = self.owner_task.lock()
            && let Some(task) = guard.take()
        {
            task.abort();
        }

        // Drain subscriptions, flushing each sink. Delivery fibers are
        // ephemeral and the owner is stopped, so the `flush` lock is
        // uncontended.
        let subs = self
            .subscriptions
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        for (_id, sub) in subs {
            if let Ok(mut g) = sub.reporter.try_lock() {
                g.flush();
            }
        }
    }
}

// =========================================================================
// Owner thread — single writer of the windows map
// =========================================================================

/// Drive the cadence consolidation lifecycle from a single thread.
/// Drains the command channel, mutates the windows map exclusively
/// (no lock), and republishes a fresh `ReaderState` after each batch
/// of commands so readers always see a consistent view.
async fn run_owner(
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    layers: Vec<CadenceLayer>,
    state_pub: Arc<ArcSwap<ReaderState>>,
    subscriptions: Arc<Mutex<HashMap<SubscriberId, Subscription>>>,
) {
    let mut windows: HashMap<(String, usize), CadenceWindow> = HashMap::new();
    let mut component_labels: HashMap<String, Labels> = HashMap::new();

    'outer: loop {
        // Await the first message of a batch.
        let mut cmd = match cmd_rx.recv().await {
            Some(c) => c,
            None => break 'outer, // all senders dropped → shutdown
        };

        // Drain commands without blocking; publish once at the end
        // of the batch so readers see all changes atomically. Acks
        // are collected and fired AFTER the publish so callers
        // observe the state they requested.
        let mut acks: Vec<FlushAck> = Vec::new();
        loop {
            match cmd {
                Cmd::Ingest {
                    path,
                    labels,
                    snapshot,
                    ack,
                } => {
                    component_labels.entry(path.clone()).or_insert(labels);
                    let closed_by_cadence = ingest_cascade(&mut windows, &layers, path, snapshot);
                    fanout_owner(&subscriptions, &closed_by_cadence);
                    if let Some(a) = ack {
                        acks.push(FlushAck::Sync(a));
                    }
                }
                Cmd::ClosePath { path, ack, reason } => {
                    let closed_by_cadence =
                        close_path_cascade(&mut windows, &layers, &path, reason);
                    fanout_owner(&subscriptions, &closed_by_cadence);
                    if let Some(a) = ack {
                        acks.push(FlushAck::Sync(a));
                    }
                }
                Cmd::ShutdownFlushAll { ack, reason } => {
                    let paths: Vec<String> = {
                        let mut set: std::collections::HashSet<String> =
                            std::collections::HashSet::new();
                        for (p, _) in windows.keys() {
                            set.insert(p.clone());
                        }
                        set.into_iter().collect()
                    };
                    let mut all_closed = Vec::new();
                    for path in &paths {
                        all_closed.extend(close_path_cascade(&mut windows, &layers, path, reason));
                    }
                    fanout_owner(&subscriptions, &all_closed);
                    acks.push(ack);
                }
                Cmd::Barrier { ack } => {
                    // No state mutation — just synchronization.
                    // The ack fires after the post-batch publish,
                    // so the caller sees the cumulative effect of
                    // every prior FIFO command.
                    acks.push(FlushAck::Sync(ack));
                }
            }

            cmd = match cmd_rx.try_recv() {
                Ok(c) => c,
                Err(_) => break, // batch drained (Empty) or closed
            };
        }

        publish_reader_state(&windows, &component_labels, &state_pub);

        // Fire all acks AFTER publish so each waiter sees its own
        // command's effects in the reader state.
        for ack in acks.drain(..) {
            ack.signal();
        }
    }

    // Final publish so any remaining state is visible to late readers
    // (e.g., a shutdown query after the channel closed).
    publish_reader_state(&windows, &component_labels, &state_pub);
}

/// Apply a single ingest to the cascade. Returns the closed
/// snapshots in ascending-cadence order.
fn ingest_cascade(
    windows: &mut HashMap<(String, usize), CadenceWindow>,
    layers: &[CadenceLayer],
    path: String,
    snapshot: MetricSet,
) -> Vec<(Duration, Arc<MetricSet>)> {
    let mut closed_by_cadence: Vec<(Duration, Arc<MetricSet>)> = Vec::new();
    let smallest_idx = 0usize;
    let mut to_propagate: Option<(usize, Arc<MetricSet>)> = None;

    let key = (path.clone(), smallest_idx);
    let entry = windows
        .entry(key)
        .or_insert_with(|| CadenceWindow::new(layers[smallest_idx].interval));
    if let Some(closed) = entry.ingest(snapshot) {
        closed_by_cadence.push((layers[smallest_idx].interval, closed.clone()));
        to_propagate = Some((smallest_idx + 1, closed));
    }

    while let Some((idx, snapshot_arc)) = to_propagate.take() {
        if idx >= layers.len() {
            break;
        }
        let key = (path.clone(), idx);
        let entry = windows
            .entry(key)
            .or_insert_with(|| CadenceWindow::new(layers[idx].interval));
        if let Some(closed) = entry.ingest((*snapshot_arc).clone()) {
            closed_by_cadence.push((layers[idx].interval, closed.clone()));
            to_propagate = Some((idx + 1, closed));
        }
    }
    closed_by_cadence
}

/// Force-close every cascade layer's window for one path, promoting
/// partials. Returns closed snapshots in ascending-cadence order.
fn close_path_cascade(
    windows: &mut HashMap<(String, usize), CadenceWindow>,
    layers: &[CadenceLayer],
    path: &str,
    reason: CloseReason,
) -> Vec<(Duration, Arc<MetricSet>)> {
    let mut closed: Vec<(Duration, Arc<MetricSet>)> = Vec::new();
    let mut to_propagate: Option<(usize, Arc<MetricSet>)> = None;
    for idx in 0..layers.len() {
        let key = (path.to_string(), idx);
        if let Some((carry_idx, carry)) = to_propagate.take()
            && carry_idx == idx
        {
            let entry = windows
                .entry(key.clone())
                .or_insert_with(|| CadenceWindow::new(layers[idx].interval));
            let _ = entry.ingest((*carry).clone());
        }
        if let Some(window) = windows.get_mut(&key)
            && let Some(snap) = window.force_close(reason)
        {
            closed.push((layers[idx].interval, snap.clone()));
            if idx + 1 < layers.len() {
                to_propagate = Some((idx + 1, snap));
            }
        }
    }
    closed
}

/// Owner-side fanout: for each closed snapshot, spawn an **ephemeral
/// delivery fiber** per matching subscriber. The owner never blocks
/// (spawn returns immediately) and no dedicated per-subscriber thread
/// sits parked — the signaling layer is now the cadence command stream
/// plus short-lived fibers. The fiber `try_lock`s the subscriber's sink
/// (a busy lock = a prior delivery still running ⇒ drop this window,
/// the same lossy-under-backpressure contract the bounded channel had)
/// and always bumps `delivered` so `quiesce` (pending == 0) converges.
/// Each fiber runs inside the subscriber's `context_wrap` so a workload
/// subscriber's report executes as its own execution (SRD-88).
fn fanout_owner(
    subscriptions: &Arc<Mutex<HashMap<SubscriberId, Subscription>>>,
    closed: &[(Duration, Arc<MetricSet>)],
) {
    if closed.is_empty() {
        return;
    }
    // Runs inside the owner task, so delivery fibers spawn onto the
    // ambient runtime.
    let Ok(map) = subscriptions.lock() else {
        return;
    };
    for (cadence, snapshot) in closed {
        for sub in map.values() {
            if sub.cadence != *cadence {
                continue;
            }
            // Stop feeding a one-shot subscriber that already terminated.
            if sub.state.finished.load(Ordering::Relaxed) {
                continue;
            }

            // Bounded backpressure: while in-flight deliveries (pending =
            // sent − delivered) are at capacity, drop this window and
            // escalate the stall timeout — the same policy the bounded
            // channel had when it filled. Keeps a wedged sink from piling
            // up unbounded `lock().await` waiters.
            if sub.state.pending() >= sub.opts.channel_capacity as u64 {
                let drops = sub.state.consecutive_drops.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(timeout) = sub.opts.timeout {
                    let last = sub
                        .state
                        .last_delivered
                        .lock()
                        .map(|g| *g)
                        .unwrap_or_else(|_| Instant::now());
                    let age = last.elapsed();
                    if age >= timeout && !sub.state.timeout_fired.swap(true, Ordering::Relaxed) {
                        if let Some(cb) = &sub.opts.on_timeout {
                            cb(TimeoutEvent {
                                subscriber_id: sub.id,
                                cadence: sub.cadence,
                                snapshot_age: age,
                                consecutive_drops: drops,
                            });
                        } else {
                            crate::diag::warn(&format!(
                                "metrics subscription {:?} at cadence {:?} has stalled for \
                                 {age:?} ({drops} consecutive drops)",
                                sub.id, sub.cadence,
                            ));
                        }
                    }
                }
                continue;
            }

            sub.state.sent.fetch_add(1, Ordering::Relaxed);
            let reporter = sub.reporter.clone();
            let state = sub.state.clone();
            let snapshot = snapshot.clone();

            let fut = async move {
                // Serialize losslessly: wait for any prior delivery to
                // finish, then report this window in order.
                let mut g = reporter.lock().await;
                g.report(&snapshot);
                if g.finished() {
                    state.finished.store(true, Ordering::Relaxed);
                }
                drop(g);
                state.mark_delivered(); // resets consecutive_drops + timeout
                // Closes out this in-flight delivery so `quiesce`
                // (pending == 0) and the capacity gate converge.
                state.delivered.fetch_add(1, Ordering::Relaxed);
            };

            let fut: Pin<Box<dyn Future<Output = ()> + Send>> = match &sub.context_wrap {
                Some(w) => w(Box::pin(fut)),
                None => Box::pin(fut),
            };
            tokio::spawn(fut);
        }
    }
}

/// Build a fresh `ReaderState` from the owner's private windows map
/// and publish it via `ArcSwap`. Readers see this as a single
/// atomic transition.
fn publish_reader_state(
    windows: &HashMap<(String, usize), CadenceWindow>,
    component_labels: &HashMap<String, Labels>,
    state_pub: &Arc<ArcSwap<ReaderState>>,
) {
    let mut win_views: HashMap<(String, usize), Arc<WindowReaderView>> =
        HashMap::with_capacity(windows.len());
    for (key, win) in windows.iter() {
        let view = WindowReaderView {
            latest: win.latest(),
            prebuffer: win.prebuffer_clone().map(Arc::new),
            ring: Arc::new(win.ring().cloned().collect()),
        };
        win_views.insert(key.clone(), Arc::new(view));
    }
    let new_state = ReaderState {
        component_labels: component_labels.clone(),
        windows: win_views,
    };
    state_pub.store(Arc::new(new_state));
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::MetricValue;

    fn counter_set(interval: Duration, value: u64) -> MetricSet {
        let mut s = MetricSet::new(interval);
        s.insert_counter("ops", Labels::default(), value, Instant::now());
        s
    }

    fn first_counter_total(snap: &MetricSet) -> u64 {
        let f = snap.family("ops").expect("ops family");
        let m = f.metrics().next().expect("series");
        match m.point().unwrap().value() {
            MetricValue::Counter(c) => c.cumulative,
            _ => panic!("not a counter"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ingest_promotes_at_smallest_cadence_boundary() {
        let cadences =
            Cadences::new(&[Duration::from_millis(100), Duration::from_millis(400)]).unwrap();
        let tree = CadenceTree::plan_default(cadences);
        let reporter = CadenceReporter::new(tree);
        let labels = Labels::of("phase", "load");

        // 4× 100ms snapshots of a counter climbing 5→20. Each cadence's
        // latest is the latest cumulative (coalesce keeps latest, no sum).
        for v in [5, 10, 15, 20] {
            reporter.ingest(&labels, counter_set(Duration::from_millis(100), v));
        }
        reporter.flush_for_tests();

        let latest_100 = reporter
            .latest(&labels, Duration::from_millis(100))
            .expect("100ms cadence should have a latest");
        assert_eq!(
            first_counter_total(&latest_100),
            20,
            "last 100ms window's cumulative"
        );

        let latest_400 = reporter
            .latest(&labels, Duration::from_millis(400))
            .expect("400ms cadence should have promoted after 4 ticks");
        assert_eq!(
            first_counter_total(&latest_400),
            20,
            "promoted 400ms window holds the latest cumulative"
        );
    }

    /// Build a counter snapshot stamped at an explicit `captured_at`, so a
    /// test can drive the time-based retention without sleeping.
    fn counter_set_at(captured_at: Instant, interval: Duration, value: u64) -> MetricSet {
        let mut s = MetricSet::at(captured_at, interval);
        s.insert_counter("ops", Labels::default(), value, captured_at);
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ring_retains_by_time_not_slot_count() {
        // SRD-90 §M1: the smallest-cadence ring is TIME-bounded, not
        // slot-bounded. Pushing more than HISTORY_RING_CAP windows, all within
        // the counter horizon, retains every one (the old slot cap would have
        // dropped the oldest 5) — that extra cheap counter history is what
        // short-timeframe trending reads.
        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "x");

        let base = Instant::now();
        for i in 0..(HISTORY_RING_CAP + 5) {
            let at = base + Duration::from_millis(i as u64);
            reporter.ingest(
                &labels,
                counter_set_at(at, Duration::from_millis(50), (i as u64) + 1),
            );
        }
        reporter.flush_for_tests();

        let ring = reporter.ring(&labels, Duration::from_millis(50));
        assert_eq!(
            ring.len(),
            HISTORY_RING_CAP + 5,
            "time-bounded ring keeps all recent windows, not just HISTORY_RING_CAP",
        );
        let newest_total = first_counter_total(ring.last().unwrap());
        assert_eq!(newest_total, (HISTORY_RING_CAP as u64) + 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ring_evicts_windows_past_the_counter_horizon() {
        // A window older than the counter retain horizon (relative to the
        // newest) is evicted — the retention is bounded by wall-clock, not by
        // run length.
        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "x");

        let base = Instant::now();
        // One ancient window, then five "100s-later" windows. The ancient one
        // is past the 60s horizon relative to the newest → evicted on the next
        // append; the five recent ones remain.
        reporter.ingest(&labels, counter_set_at(base, Duration::from_millis(50), 1));
        for i in 1..=5u64 {
            let at = base + Duration::from_secs(100) + Duration::from_millis(i);
            reporter.ingest(
                &labels,
                counter_set_at(at, Duration::from_millis(50), 1 + i),
            );
        }
        reporter.flush_for_tests();

        let ring = reporter.ring(&labels, Duration::from_millis(50));
        assert_eq!(
            ring.len(),
            5,
            "the window past the retain horizon is evicted"
        );
        assert_eq!(
            first_counter_total(ring.first().unwrap()),
            2,
            "oldest retained window is the first of the recent cluster",
        );
    }

    /// A snapshot carrying BOTH a cumulative counter and an HDR histogram,
    /// stamped at an explicit `captured_at`.
    fn counter_and_hist_at(captured_at: Instant, interval: Duration, value: u64) -> MetricSet {
        use hdrhistogram::Histogram as HdrHistogram;
        let mut h = HdrHistogram::<u64>::new(3).unwrap();
        h.record(value).unwrap();
        let mut s = MetricSet::at(captured_at, interval);
        s.insert_counter("ops", Labels::default(), value, captured_at);
        s.insert_histogram("latency", Labels::default(), h, captured_at);
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn distributions_stripped_past_hist_horizon_but_counters_kept() {
        // SRD-90 §M1 / the "histograms are a different matter" rule: past the
        // tight distribution horizon (50ms × HISTORY_RING_CAP = 1.6s here), a
        // retained window sheds its heavy HDR reservoir but keeps its cheap
        // cumulative counter — so counter trending runs long while histogram
        // memory stays bounded.
        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "h");

        let base = Instant::now();
        // Old window (both families), then one 2s later: 2s > 1.6s hist horizon
        // but well under the 60s counter horizon.
        reporter.ingest(
            &labels,
            counter_and_hist_at(base, Duration::from_millis(50), 1),
        );
        reporter.ingest(
            &labels,
            counter_and_hist_at(base + Duration::from_secs(2), Duration::from_millis(50), 2),
        );
        reporter.flush_for_tests();

        let ring = reporter.ring(&labels, Duration::from_millis(50));
        assert_eq!(ring.len(), 2, "both windows are within the counter horizon");
        let old = ring.first().unwrap();
        assert!(
            old.family("ops").is_some(),
            "old window keeps its cumulative counter"
        );
        assert!(
            old.family("latency").is_none(),
            "old window's HDR histogram is stripped past the distribution horizon"
        );
        let recent = ring.last().unwrap();
        assert!(
            recent.family("latency").is_some(),
            "the recent window keeps its histogram"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_flush_is_retained_as_a_distinct_window() {
        // SRD-90 §M2: a sub-cadence `scope_close` partial is STACKED as its own
        // timestamped window in the smallest-tier ring — distinct from the
        // pulse-closed windows, carrying its own `is_partial` flag and interval
        // — not folded away. (Within-window sub-cadence ingests still coalesce
        // into their window; it is each *close* that is a distinct point.)
        let cadences = Cadences::new(&[Duration::from_secs(1)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "p");
        let base = Instant::now();

        // A full 1s window → a regular (non-partial) closed window.
        reporter.ingest(&labels, counter_set_at(base, Duration::from_secs(1), 10));
        // A mid-cadence teardown flushes a 200ms partial right after.
        let mut partial = MetricSet::at(base + Duration::from_secs(1), Duration::from_millis(200));
        partial.insert_counter("ops", Labels::default(), 5, base + Duration::from_secs(1));
        reporter.scope_close(&labels, partial);
        reporter.flush_for_tests();

        let ring = reporter.ring(&labels, Duration::from_secs(1));
        assert_eq!(
            ring.len(),
            2,
            "the pulse window AND the partial flush are both retained"
        );
        assert!(!ring[0].is_partial(), "first is the pulse-closed window");
        assert_eq!(first_counter_total(&ring[0]), 10);
        assert!(
            ring[1].is_partial(),
            "second is the scope_close partial, flagged"
        );
        assert_eq!(first_counter_total(&ring[1]), 5);
        assert!(
            ring[1].interval() < Duration::from_secs(1),
            "partial carries its own sub-cadence interval"
        );
        assert!(
            ring[0].captured_at() < ring[1].captured_at(),
            "distinct timestamps, not coalesced"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn metric_sink_fans_out_and_cadence_reporter_is_a_sink() {
        // SRD-90 §M7/A7: `CadenceReporter` implements `MetricSink` at its inlet
        // (`submit` = `ingest`), and `FanOutSink` delivers one snapshot to every
        // interior sink — the write-side dual of `HybridStore` composing reads.
        use std::sync::atomic::{AtomicU64, Ordering};

        struct Recorder(Arc<AtomicU64>);
        impl MetricSink for Recorder {
            fn submit(&self, _labels: &Labels, snapshot: MetricSet) {
                self.0
                    .fetch_add(first_counter_total(&snapshot), Ordering::SeqCst);
            }
        }

        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let rec_a = Arc::new(AtomicU64::new(0));
        let rec_b = Arc::new(AtomicU64::new(0));

        let fan = FanOutSink::new(vec![
            reporter.clone() as Arc<dyn MetricSink>,
            Arc::new(Recorder(rec_a.clone())),
            Arc::new(Recorder(rec_b.clone())),
        ]);
        let labels = Labels::of("phase", "f");
        fan.submit(&labels, counter_set(Duration::from_millis(50), 7));
        reporter.flush_for_tests();

        assert_eq!(
            rec_a.load(Ordering::SeqCst),
            7,
            "fan-out reached recorder A"
        );
        assert_eq!(
            rec_b.load(Ordering::SeqCst),
            7,
            "fan-out reached recorder B"
        );
        let latest = reporter
            .latest(&labels, Duration::from_millis(50))
            .expect("cadence reporter ingested via MetricSink::submit");
        assert_eq!(first_counter_total(&latest), 7);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn force_close_publishes_partial_at_shutdown() {
        let cadences = Cadences::new(&[Duration::from_millis(1000)]).unwrap();
        let tree = CadenceTree::plan_default(cadences);
        let reporter = CadenceReporter::new(tree);
        let labels = Labels::of("phase", "trail");

        // Only 200ms of data — won't naturally promote at 1000ms.
        reporter.ingest(&labels, counter_set(Duration::from_millis(200), 3));
        reporter.flush_for_tests();
        assert!(
            reporter
                .latest(&labels, Duration::from_millis(1000))
                .is_none()
        );

        reporter.shutdown_flush().await;
        let partial = reporter
            .latest(&labels, Duration::from_millis(1000))
            .expect("shutdown must publish trailing partial");
        assert_eq!(first_counter_total(&partial), 3);
        assert!(
            partial.interval() < Duration::from_millis(1000),
            "partial interval must be < cadence: {:?}",
            partial.interval()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prebuffer_visible_for_in_flight_data() {
        let cadences = Cadences::new(&[Duration::from_millis(1000)]).unwrap();
        let tree = CadenceTree::plan_default(cadences);
        let reporter = CadenceReporter::new(tree);
        let labels = Labels::of("phase", "p");

        reporter.ingest(&labels, counter_set(Duration::from_millis(300), 7));
        reporter.ingest(&labels, counter_set(Duration::from_millis(300), 8));
        reporter.flush_for_tests();

        let pb = reporter
            .prebuffer(&labels, Duration::from_millis(1000))
            .expect("prebuffer present");
        assert_eq!(
            first_counter_total(&pb),
            8,
            "in-flight prebuffer holds the latest cumulative"
        );
        // Latest still empty — no full cadence elapsed.
        assert!(
            reporter
                .latest(&labels, Duration::from_millis(1000))
                .is_none()
        );
    }

    // ---- subscription tests -----------------------------------

    struct CountingReporter {
        count: Arc<AtomicU64>,
    }
    impl crate::scheduler::Reporter for CountingReporter {
        fn report(&mut self, _snapshot: &MetricSet) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_receives_snapshots_on_dispatch_thread() {
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let labels = Labels::of("phase", "sub");

        let count = Arc::new(AtomicU64::new(0));
        let _id = reporter
            .subscribe(
                Duration::from_millis(100),
                Box::new(CountingReporter {
                    count: count.clone(),
                }),
                SubscriptionOpts::default(),
            )
            .unwrap();

        for _ in 0..5 {
            reporter.ingest(&labels, counter_set(Duration::from_millis(100), 1));
        }
        // Drain: wait up to 500ms for the dispatch thread to catch up.
        let deadline = Instant::now() + Duration::from_millis(500);
        while count.load(Ordering::Relaxed) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(count.load(Ordering::Relaxed), 5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn quiesce_synchronously_drains_subscriber_and_is_non_terminal() {
        // SRD-88 — `quiesce` is the per-execution flush-to-store: after
        // it returns, the subscriber (the sink, e.g. SQLite) has
        // committed every closed window, WITHOUT the subscriber being
        // torn down (so concurrent siblings keep flowing).
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let labels = Labels::of("phase", "q");
        let count = Arc::new(AtomicU64::new(0));
        let _id = reporter
            .subscribe(
                Duration::from_millis(100),
                Box::new(CountingReporter {
                    count: count.clone(),
                }),
                SubscriptionOpts::default(),
            )
            .unwrap();

        // A few partials — the 100ms window hasn't elapsed, so nothing
        // has closed by cadence yet.
        for _ in 0..3 {
            reporter.ingest(&labels, counter_set(Duration::from_millis(100), 1));
        }
        // quiesce force-closes the open window, fans it out, and waits
        // for the worker to commit it — so the assertion needs NO drain
        // loop, unlike `subscribe_receives_snapshots_on_dispatch_thread`.
        reporter.quiesce(Duration::from_secs(2)).await;
        let after_first = count.load(Ordering::Relaxed);
        assert!(
            after_first >= 1,
            "quiesce must synchronously deliver the force-closed window; got {after_first}"
        );

        // Non-terminal: the subscriber is still registered — more data flows.
        reporter.ingest(&labels, counter_set(Duration::from_millis(100), 1));
        reporter.quiesce(Duration::from_secs(2)).await;
        assert!(
            count.load(Ordering::Relaxed) > after_first,
            "subscriber must remain alive + receiving after quiesce"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn subscribe_rejects_unknown_cadence() {
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));

        let err = reporter
            .subscribe(
                Duration::from_millis(250),
                Box::new(CountingReporter {
                    count: Arc::new(AtomicU64::new(0)),
                }),
                SubscriptionOpts::default(),
            )
            .unwrap_err();
        assert!(matches!(err, SubscribeError::UnknownCadence(_)));
    }

    /// A reporter that blocks on a parking lot — simulates a slow
    /// HTTP sink.
    struct SlowReporter {
        block: Arc<AtomicBool>,
    }
    impl crate::scheduler::Reporter for SlowReporter {
        fn report(&mut self, _snapshot: &MetricSet) {
            while self.block.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_subscriber_fires_timeout_without_blocking_cascade() {
        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let labels = Labels::of("phase", "stall");

        let block = Arc::new(AtomicBool::new(true));
        let fired = Arc::new(AtomicU64::new(0));
        let fired_for_cb = fired.clone();

        let opts = SubscriptionOpts {
            channel_capacity: 1, // fill fast
            timeout: Some(Duration::from_millis(100)),
            on_timeout: Some(Arc::new(move |_ev| {
                fired_for_cb.fetch_add(1, Ordering::Relaxed);
            })),
            context_wrap: None,
        };
        let _id = reporter
            .subscribe(
                Duration::from_millis(50),
                Box::new(SlowReporter {
                    block: block.clone(),
                }),
                opts,
            )
            .unwrap();

        // Pump ingests — the cascade must keep running even though
        // the subscriber is blocked on its first snapshot.
        let start = Instant::now();
        for _ in 0..20 {
            reporter.ingest(&labels, counter_set(Duration::from_millis(50), 1));
            std::thread::sleep(Duration::from_millis(20));
        }
        // Cascade wall-clock should be roughly 20 * 20ms = 400ms; if
        // the subscriber were synchronous it would be >20 * cadence.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cascade took {:?} — subscriber must have blocked it",
            start.elapsed()
        );
        // Timeout callback should have fired at least once.
        assert!(
            fired.load(Ordering::Relaxed) >= 1,
            "expected timeout callback to fire; got {}",
            fired.load(Ordering::Relaxed)
        );

        // Unblock so the worker can drain before shutdown.
        block.store(false, Ordering::Relaxed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unsubscribe_stops_delivery() {
        let cadences = Cadences::new(&[Duration::from_millis(50)]).unwrap();
        let reporter = Arc::new(CadenceReporter::new(CadenceTree::plan_default(cadences)));
        let labels = Labels::of("phase", "unsub");

        let count = Arc::new(AtomicU64::new(0));
        let id = reporter
            .subscribe(
                Duration::from_millis(50),
                Box::new(CountingReporter {
                    count: count.clone(),
                }),
                SubscriptionOpts::default(),
            )
            .unwrap();

        reporter.ingest(&labels, counter_set(Duration::from_millis(50), 1));
        // Wait briefly for delivery.
        let deadline = Instant::now() + Duration::from_millis(200);
        while count.load(Ordering::Relaxed) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(count.load(Ordering::Relaxed), 1);

        reporter.unsubscribe(id);

        reporter.ingest(&labels, counter_set(Duration::from_millis(50), 1));
        reporter.ingest(&labels, counter_set(Duration::from_millis(50), 1));
        std::thread::sleep(Duration::from_millis(100));
        // Still only 1 — unsubscribe dropped the sender.
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    // =====================================================================
    // SRD-40b §11 / SRD-42 §"Component lifecycle: scope_close flush"
    //
    // A short-lived component drops between cadence pulses. Its
    // last-tick deltas must reach the streamer marked partial=true,
    // and fold into the next full window via the standard combine
    // rules (Counter sum, Gauge weighted-avg with last-write
    // fallback for zero-interval, Histogram HDR-merge).
    // =====================================================================

    fn gauge_set(interval: Duration, value: f64) -> MetricSet {
        let mut s = MetricSet::new(interval);
        s.insert_gauge("temp", Labels::default(), value, Instant::now());
        s
    }

    fn histogram_set(interval: Duration, samples: &[u64]) -> MetricSet {
        use hdrhistogram::Histogram as HdrHistogram;
        let mut h = HdrHistogram::<u64>::new(3).unwrap();
        for v in samples {
            h.record(*v).unwrap();
        }
        let mut s = MetricSet::new(interval);
        s.insert_histogram("latency", Labels::default(), h, Instant::now());
        s
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_close_marks_partial_and_publishes_immediately() {
        // Cadence is 1s. Component contributes 200ms of activity then
        // tears down. scope_close must publish a partial-annotated
        // snapshot at the smallest cadence right away, so a query
        // before the next pulse sees the flushed data.
        let cadences = Cadences::new(&[Duration::from_secs(1)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "short");

        let mut delta = MetricSet::new(Duration::from_millis(200));
        delta.insert_counter("ops", Labels::default(), 7, Instant::now());

        reporter.scope_close(&labels, delta);
        reporter.flush_for_tests();

        let latest = reporter
            .latest(&labels, Duration::from_secs(1))
            .expect("scope_close must publish a partial snapshot at smallest cadence");
        assert!(
            latest.is_partial(),
            "scope-close snapshot must be marked partial"
        );
        assert_eq!(first_counter_total(&latest), 7);
        assert!(
            latest.interval() < Duration::from_secs(1),
            "partial interval must be < cadence, got {:?}",
            latest.interval()
        );
        assert_eq!(
            latest.close_reason(),
            Some(CloseReason::ScopeClose),
            "scope_close must stamp the typed reason (SRD-93 M4)"
        );
    }

    /// SRD-93 M4/A6 — the typed close reason distinguishes the three
    /// sealers: `scope_close` → ScopeClose, `quiesce` → Quiesce (a
    /// seal, NEVER an exit signal), `shutdown_flush` → Shutdown; and
    /// severity survives the coalesce fold (Shutdown ≥ ScopeClose ≥
    /// Quiesce). A naturally-closed window carries no reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_reason_names_the_sealer_not_the_partial_flag() {
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));

        // Natural close: two 50ms deltas fill the 100ms window.
        let nat = Labels::of("phase", "natural");
        for _ in 0..2 {
            let mut d = MetricSet::new(Duration::from_millis(50));
            d.insert_counter("ops", Labels::default(), 1, Instant::now());
            reporter.ingest(&nat, d);
        }
        reporter.flush_for_tests();
        let w = reporter.latest(&nat, Duration::from_millis(100)).unwrap();
        assert_eq!(
            w.close_reason(),
            None,
            "a cadence-closed window carries no lifecycle reason"
        );

        // Quiesce seals a partial but is not an exit signal.
        let qui = Labels::of("phase", "quiescing");
        let mut d = MetricSet::new(Duration::from_millis(10));
        d.insert_counter("ops", Labels::default(), 2, Instant::now());
        reporter.ingest(&qui, d);
        reporter.quiesce(Duration::from_secs(5)).await;
        let w = reporter.latest(&qui, Duration::from_millis(100)).unwrap();
        assert!(w.is_partial());
        assert_eq!(w.close_reason(), Some(CloseReason::Quiesce));

        // Shutdown outranks: a later shutdown_flush over a fresh
        // partial stamps Shutdown.
        let end = Labels::of("phase", "ending");
        let mut d = MetricSet::new(Duration::from_millis(10));
        d.insert_counter("ops", Labels::default(), 3, Instant::now());
        reporter.ingest(&end, d);
        reporter.shutdown_flush().await;
        let w = reporter.latest(&end, Duration::from_millis(100)).unwrap();
        assert_eq!(w.close_reason(), Some(CloseReason::Shutdown));

        // Severity ordering is what coalesce leans on.
        assert!(CloseReason::Shutdown > CloseReason::ScopeClose);
        assert!(CloseReason::ScopeClose > CloseReason::Quiesce);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_close_counter_partial_carries_through_cascade() {
        // SRD-42 §"Component lifecycle: scope_close flush" — a
        // scope_close partial cascades through every cadence layer
        // marked partial=true. The current implementation publishes
        // each layer at close_path time (force_close on every
        // window after the partial propagates up the chain), so
        // the larger cadence's `latest` is the partial that
        // cascaded up. Subsequent natural pulses produce their
        // own (non-partial) windows in fresh accumulators.
        //
        // Verifies:
        //   1. Smallest-cadence latest = partial(5).
        //   2. Largest-cadence latest = partial(5) (cascaded).
        //   3. After 4 natural 100ms pulses of 3 each, 400ms
        //      latest = 12 (4×3) and is NOT partial — fresh
        //      window after the partial was already published.
        let cadences =
            Cadences::new(&[Duration::from_millis(100), Duration::from_millis(400)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "burst");

        let mut partial = MetricSet::new(Duration::from_millis(50));
        partial.insert_counter("ops", Labels::default(), 5, Instant::now());
        reporter.scope_close(&labels, partial);
        reporter.flush_for_tests();

        let p100 = reporter
            .latest(&labels, Duration::from_millis(100))
            .unwrap();
        assert_eq!(first_counter_total(&p100), 5);
        assert!(
            p100.is_partial(),
            "smallest-cadence publish must be partial"
        );

        let p400 = reporter
            .latest(&labels, Duration::from_millis(400))
            .expect("close_path cascade publishes at every layer");
        assert_eq!(
            first_counter_total(&p400),
            5,
            "partial cascades unchanged through the chain"
        );
        assert!(
            p400.is_partial(),
            "partial flag is sticky across the cascade fold"
        );

        // Natural cadence pulse: a counter climbing 6→15 over four 100ms
        // windows. The 400ms close holds the latest cumulative — no sum,
        // no partial carryover — and is NOT marked partial.
        for v in [6, 9, 12, 15] {
            reporter.ingest(&labels, counter_set(Duration::from_millis(100), v));
        }
        reporter.flush_for_tests();

        let np400 = reporter
            .latest(&labels, Duration::from_millis(400))
            .unwrap();
        assert_eq!(
            first_counter_total(&np400),
            15,
            "natural-pulse window holds the latest cumulative"
        );
        assert!(
            !np400.is_partial(),
            "natural-pulse close must NOT be marked partial"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_close_gauge_partials_use_combine_rules() {
        // Gauge combine is LAST-WRITE-WINS (the OpenMetrics/
        // Prometheus gauge contract; summarization is a query-point
        // concern). With two ingests of gauge values 4.0 then 8.0,
        // the merged value must be 8.0 — the fold goes through
        // coalesce, just like a normal cadence-pulse fold.
        let cadences = Cadences::new(&[Duration::from_secs(1)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "g");

        reporter.scope_close(&labels, gauge_set(Duration::from_millis(200), 4.0));
        reporter.flush_for_tests();
        // close_path force-closes immediately, but a second
        // scope_close opens a new prebuffer entry. To test the
        // combine, instead: ingest two gauges WITHOUT scope_close,
        // then scope_close once at the end so the partial fold
        // shows the combine rule.
        let cadences = Cadences::new(&[Duration::from_secs(1)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        reporter.ingest(&labels, gauge_set(Duration::from_millis(200), 4.0));
        reporter.ingest(&labels, gauge_set(Duration::from_millis(200), 8.0));
        // Tear down: scope_close with empty delta still triggers
        // the partial publication of the prebuffer.
        reporter.scope_close(&labels, MetricSet::new(Duration::ZERO));
        reporter.flush_for_tests();

        let latest = reporter.latest(&labels, Duration::from_secs(1)).unwrap();
        assert!(
            latest.is_partial(),
            "scope_close must publish prebuffer as partial"
        );
        let g = latest
            .family("temp")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value();
        let value = match g {
            MetricValue::Gauge(g) => g.value,
            _ => panic!("expected gauge, got {:?}", g),
        };
        assert!(
            (value - 8.0).abs() < 1e-9,
            "expected last-written 8.0, got {value}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_close_histogram_partials_hdr_merge() {
        // Histogram combine = HdrHistogram::add. Two partials each
        // recording disjoint values; the merged reservoir must
        // include all of them.
        use hdrhistogram::Histogram as HdrHistogram;
        let cadences = Cadences::new(&[Duration::from_secs(1)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "h");

        reporter.ingest(
            &labels,
            histogram_set(Duration::from_millis(100), &[10, 20, 30]),
        );
        reporter.ingest(
            &labels,
            histogram_set(Duration::from_millis(100), &[100, 200, 300]),
        );
        reporter.scope_close(&labels, MetricSet::new(Duration::ZERO));
        reporter.flush_for_tests();

        let latest = reporter.latest(&labels, Duration::from_secs(1)).unwrap();
        assert!(latest.is_partial());
        let v = latest
            .family("latency")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value();
        let h: &HdrHistogram<u64> = match v {
            MetricValue::Histogram(h) => h.reservoir.as_ref(),
            _ => panic!("expected histogram"),
        };
        // All six samples must be present in the merged reservoir.
        assert_eq!(h.len(), 6, "all six histogram samples must merge");
        assert!(h.value_at_quantile(0.0) <= 10);
        assert!(h.value_at_quantile(1.0) >= 300);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scope_close_only_runs_when_component_is_running() {
        // scope_close on the COMPONENT (vs. on the reporter direct)
        // skips Stopped/Stopping/Starting components per the
        // ComponentState gate — covered in the component module
        // test below. Here we just verify scope_close on the
        // reporter is benign when no prior ingest has happened
        // (no path → close_path is a no-op).
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let reporter = CadenceReporter::new(CadenceTree::plan_default(cadences));
        let labels = Labels::of("phase", "never");

        reporter.scope_close(&labels, MetricSet::new(Duration::ZERO));
        reporter.flush_for_tests();

        // No data was ever ingested — nothing to publish. latest
        // returns None.
        assert!(
            reporter
                .latest(&labels, Duration::from_millis(100))
                .is_none(),
            "empty-delta scope_close on never-seen path must not invent a snapshot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn separate_components_keyed_independently() {
        let cadences = Cadences::new(&[Duration::from_millis(100)]).unwrap();
        let tree = CadenceTree::plan_default(cadences);
        let reporter = CadenceReporter::new(tree);

        reporter.ingest(
            &Labels::of("phase", "a"),
            counter_set(Duration::from_millis(100), 1),
        );
        reporter.ingest(
            &Labels::of("phase", "b"),
            counter_set(Duration::from_millis(100), 99),
        );
        reporter.flush_for_tests();

        assert_eq!(
            first_counter_total(
                &reporter
                    .latest(&Labels::of("phase", "a"), Duration::from_millis(100))
                    .unwrap()
            ),
            1
        );
        assert_eq!(
            first_counter_total(
                &reporter
                    .latest(&Labels::of("phase", "b"), Duration::from_millis(100))
                    .unwrap()
            ),
            99
        );
    }
}

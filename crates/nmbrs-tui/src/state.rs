// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Shared state between the executor and the TUI display.
//!
//! `RunState` is mutated only by the [`crate::run_state_actor`]
//! single-owner thread. Observers (executor side) and the TUI
//! both send typed [`crate::run_state_actor::RunStateCmd`]
//! messages into the actor inbox; the actor publishes
//! immutable `Arc<RunState>` snapshots through an
//! `arc_swap::ArcSwap` after every applied command. The TUI
//! reads at its render cadence (4 Hz) via
//! `RunStateHandle::load()` — a single atomic op that cannot
//! wait on the writer. There is intentionally no shared
//! `RwLock<RunState>` between the planes; see SRD-02 §"Display
//! and Diagnostic Decoupling" for the rationale.
//!
//! The methods on `RunState` (`set_phase_running`, `push_log`,
//! `install_tree`, …) are inherent helpers used by the actor's
//! command-application path; tests can also call them directly
//! against an unwrapped `RunState` to exercise the data-model
//! semantics in isolation.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use nmbrs_metrics::summaries::binomial_summary::BinomialSummary;
use nmbrs_metrics::summaries::ewma::Ewma;
use nmbrs_metrics::summaries::peak_tracker::PeakTracker;

/// Composite key for the active-phase map (SRD-100 §4):
/// `(exec_id, name, labels)`. `exec_id` (SRD-88) partitions
/// concurrent in-process executions so two executions running the
/// same phase don't collide on one slot; `name`+`labels` address a
/// specific phase iteration within an execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ActivePhaseId {
    pub exec_id: u64,
    pub name: String,
    pub labels: String,
}

impl ActivePhaseId {
    pub fn new(exec_id: u64, name: impl Into<String>, labels: impl Into<String>) -> Self {
        Self {
            exec_id,
            name: name.into(),
            labels: labels.into(),
        }
    }
}

/// Transitional alias — existing `PhaseKey` type positions resolve
/// to [`ActivePhaseId`]; construction sites use the struct directly.
pub type PhaseKey = ActivePhaseId;

/// Log severity level for display coloring.
///
/// Variants are ordered from least to most severe, so the
/// derived `Ord`/`PartialOrd` reads as a "min level" comparison
/// in the TUI's level filter (`severity >= filter` = "show this
/// entry").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogSeverity {
    Debug,
    Info,
    Warn,
    Error,
}

/// A log message with severity for display coloring.
///
/// Carries the wall-clock instant the entry was produced
/// so the failure-dump path can surface time-of-arrival
/// alongside the message — without it, "recent log
/// messages" reads as an opaque list with no clue which
/// lines came seconds vs. minutes apart, or which line
/// was the one closest to the failure.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub severity: LogSeverity,
    pub message: String,
    /// Provenance tag — two orthogonal axes (see
    /// [`nmbrs_runtime::observer::EventTag`]): the execution-
    /// lifecycle boundary the entry attaches to (`None` =
    /// in-flight) and its semantic category. Sinks derive their
    /// presentation rules from the axes — e.g. the terminal sink
    /// keeps `PhaseStart`-attached renders out of its scrollback
    /// (the managed phase-history region shows them instead).
    pub tag: EventTag,
    /// Wall-clock at log-entry creation. The dump uses
    /// this directly; the live TUI ignores it (it has its
    /// own scroll-based ordering).
    pub at: std::time::SystemTime,
}

// `EntryKind` and `PhaseStatus` now live on the canonical
// [`nmbrs_runtime::scene_tree`] types — re-exported here so existing
// `crate::state::EntryKind::Phase` references keep working without
// touching every call site.
pub use nmbrs_runtime::scene_tree::NodeKind as EntryKind;
pub use nmbrs_runtime::scene_tree::PhaseStatus;
pub use nmbrs_runtime::scene_tree::{SceneNode, SceneNodeId, SceneTree};

// Provenance tag for log entries, owned by the observer layer.
// Re-exported so `crate::state::EventTag` / `EventCategory` read
// naturally at the sink call sites (mirrors the `EntryKind` /
// `PhaseStatus` re-export pattern above).
pub use nmbrs_runtime::observer::{EventCategory, EventTag};

/// End-of-phase metrics snapshot attached to a completed phase.
/// Mirrors the live progress bar so an expanded tree entry shows the
/// same fields a non-TUI run would print on stderr.
#[derive(Clone, Debug, Default)]
pub struct PhaseSummary {
    /// Total ops finished during the phase.
    pub ops_finished: u64,
    /// Ops that succeeded (no error after retry).
    pub ops_ok: u64,
    /// SKIPPED ops (`skips_total`) — excluded from the ok% denominator.
    pub skips: u64,
    /// Ops started — used with `ops_finished` to compute active/pending.
    pub ops_started: u64,
    /// Errors observed (includes retries).
    pub errors: u64,
    /// Retries attempted.
    pub retries: u64,
    /// Fibers the phase was run with (concurrency).
    pub fibers: usize,
    /// Average ops/s over the phase duration.
    pub ops_per_sec: f64,
    /// Service-time percentiles in nanoseconds (latest sample).
    pub min_nanos: u64,
    pub p50_nanos: u64,
    pub p99_nanos: u64,
    pub max_nanos: u64,
    /// Primary cursor: name and total extent at phase end.
    pub cursor_name: String,
    pub cursor_extent: u64,
    /// Adapter-specific status counters: (name, total, rate) at phase end.
    pub adapter_counters: Vec<(String, u64, f64)>,
    /// Average rows per batch (if batching — else 0).
    pub rows_per_batch: f64,
    /// Count of cycles consumed from each input cursor, in the order
    /// the source dispatch produced them.
    pub cursors: Vec<(String, u64)>,
    /// Final relevancy aggregates per metric. Same shape as
    /// `ActivePhase::relevancy` but captured at phase_completed time.
    pub relevancy: Vec<(String, f64, f64, u64, usize)>,
    /// Frozen snapshot of the phase's throughput sparkline at
    /// completion — a clone of the `BinomialSummary`'s sample
    /// buffer. The detail block renders this instead of the
    /// (now-discarded) live `Arc<BinomialSummary>` so a scrolled-
    /// back completed phase still shows the shape of its
    /// throughput curve. Empty when the phase produced no
    /// samples (no `phase_progress` updates).
    pub throughput_samples: Vec<f64>,
}

/// Live metrics for the currently running phase.
#[derive(Clone, Debug)]
pub struct ActivePhase {
    pub name: String,
    pub labels: String,
    pub cursor_name: String,
    pub cursor_extent: u64,
    /// SRD-82 Part 6 — this phase runs as a DAEMON (off the foreground
    /// budget, open-extent cursor). Excluded from AGGREGATE progress:
    /// the footer gutter bar averages the NON-daemon phases, since a
    /// daemon's percent-of-budget is not meaningful run progress.
    pub daemon: bool,
    /// Cursor ordinals consumed / cursor extent for a data-driven
    /// phase (polydat `global_consumed()` / `global_extent()`). Both
    /// `0` for non-cursor phases (plain `cycles:`). `rows_total > 0`
    /// drives the row-denominated `rows:{consumed}/{total}` status
    /// chip in place of the op-denominated `cycles:` chip.
    pub rows_consumed: u64,
    pub rows_total: u64,
    pub fibers: usize,
    pub started_at: Instant,
    /// Session-clock reading (see [`RunState::elapsed_secs`]) when this
    /// phase entered Running — the SINGLE time basis every displayed
    /// phase timer derives from, so `session_started + leaf-time` always
    /// equals the session column shown beside it.
    pub session_started: f64,

    // Snapshot counters (updated by progress thread)
    pub ops_started: u64,
    pub ops_finished: u64,
    pub ops_ok: u64,
    /// SKIPPED ops (`skips_total`) — excluded from the ok% denominator.
    pub skips: u64,
    pub errors: u64,
    pub retries: u64,
    pub ops_per_sec: f64,

    // Adapter-specific
    pub adapter_counters: Vec<(String, u64, f64)>, // (name, total, rate)
    pub rows_per_batch: f64,

    /// Live relevancy aggregates — one entry per metric (e.g. `recall@10`).
    /// `(name, window_mean, total_mean, total_count, window_len)`
    pub relevancy: Vec<(String, f64, f64, u64, usize)>,

    /// Phase-scoped throughput sparkline storage. One sample per
    /// `phase_progress` tick; capacity caps at the sparkline's
    /// horizontal cell count so the ring never outgrows the
    /// render width. Wrapped in `Arc` so cloning `ActivePhase`
    /// (for the pause snapshot) shares the summary instead of
    /// duplicating its buffer. See SRD 62 §"Design notes →
    /// Per-phase sparkline".
    pub throughput_summary: Arc<BinomialSummary>,
    /// Smoothed cursor-advance rate. The raw `ops_per_sec` from
    /// the progress thread bounces frame-to-frame; the EWMA
    /// gives the detail-block readout a stable number that
    /// matches what a human would call "the current rate".
    pub rate_ewma: Arc<Ewma>,
    /// Rolling max latency over the last 5 seconds — drives the
    /// `╪` 5s-peak cross-bar marker on the latency range row.
    pub latency_peak_5s: Arc<PeakTracker>,
    /// Rolling max latency over the last 10 seconds — drives the
    /// `╫` 10s-peak cross-bar marker.
    pub latency_peak_10s: Arc<PeakTracker>,

    /// SRD-100 P2 — the per-phase live render handle. `Some` once the
    /// executor has attached it (after the activity's metrics/binder
    /// exist); `None` for the brief window between `phase_starting` and
    /// the attach, and for phases with no live display fold. The
    /// consumer-side status renderers fold `active_phases` and call
    /// [`crate::status_fold::render_phase_status`] on this handle to
    /// re-derive *this phase's* status line — replacing the retired
    /// inline-status producer threads + the single `status_render` scalar.
    pub render: Option<nmbrs_runtime::observer::PhaseRenderHandle>,
}

/// A single entry rendered in the TUI's phase list — a flat
/// projection over the scene tree's DFS order. Carries the scene-
/// node id so callers can cross-reference parent / children, plus
/// the heavy `PhaseSummary` (only meaningful for completed
/// phases). Held as a `Cow`-equivalent: produced by walking the
/// tree at iter time, not stored.
#[derive(Clone, Debug)]
pub struct PhaseEntry {
    pub node_id: SceneNodeId,
    pub name: String,
    pub labels: String,
    pub status: PhaseStatus,
    pub kind: EntryKind,
    pub op_count: usize,
    pub duration_secs: Option<f64>,
    /// Session cumulative time (seconds) captured at this leaf's
    /// terminal boundary — the session clock reading when the phase
    /// completed/failed. Persisted so a finished row keeps showing the
    /// session time at which it finished. `None` while still running or
    /// pending (the renderer substitutes the live session clock).
    pub session_elapsed: Option<f64>,
    /// Session-clock reading when the phase entered Running; the time
    /// basis for the leaf timer (`session_elapsed - session_started` =
    /// displayed duration, exactly).
    pub session_started: Option<f64>,
    pub depth: usize,
    pub summary: Option<PhaseSummary>,
    /// Stanza op-template names. Populated for `Phase` entries
    /// from the scene tree at install time. Used by the TUI's
    /// drilled stanza view (Right arrow on an expanded phase).
    pub op_names: Vec<String>,
    /// 1-based pre-map sequence number. Populated only on
    /// `Phase` entries; `None` for `Scope` rows. Source of truth
    /// for the `[N/total]` prefix shown next to phase names and
    /// the `phase X/Y` counter in the TUI header (see
    /// [`crate::scene_tree::SceneNode::seq`]).
    pub seq: Option<usize>,
}

/// An op-level execution leaf (SRD-63), shown as a timed status line
/// nested under its parent phase when the op declares `readout: visible`.
/// Mirrors the phase's status-margin fields so the same renderer/slots
/// apply, specialized: the count slot is `[seq/total]` within the phase,
/// the leaf time is this op's execution time, the session time is the
/// session clock at the op.
#[derive(Clone)]
pub struct OpEntry {
    pub name: String,
    pub status: PhaseStatus,
    /// Start of the current execution (for the live timer while running).
    pub started_at: Instant,
    /// Session-clock reading at op start — the display time basis (see
    /// [`ActivePhase::session_started`]).
    pub session_started: f64,
    /// Persisted op execution time, set at completion/failure.
    /// Computed as a session-clock delta (`session_elapsed -
    /// session_started`) so it reconciles with the session column.
    pub duration_secs: Option<f64>,
    /// The op's key measurable, rendered from its `measure:` template at
    /// completion ("12 sstables", "1.4 GiB"). Shown in the leaf's gutter cell
    /// ahead of the duration: a reader usually wants what the step PRODUCED,
    /// and the duration alone cannot say.
    pub measure: Option<String>,
    /// Session clock captured at the op's terminal boundary.
    pub session_elapsed: Option<f64>,
    /// Arrival order within the parent phase (0-based).
    pub seq: usize,
}

/// Top-level run state shared between executor and TUI.
#[derive(Clone)]
pub struct RunState {
    pub workload_file: String,
    pub scenario_name: String,
    pub adapter: String,
    pub started_at: Instant,
    /// Wall-clock at session start. Captured alongside the
    /// monotonic `started_at` so the failure-dump path can
    /// (a) print a UTC header naming the absolute moment
    /// `0.00000` corresponds to, and (b) compute per-entry
    /// deltas against it. The monotonic `started_at` stays
    /// authoritative for elapsed-time comparisons; this one
    /// is for human-readable display only.
    pub started_at_utc: std::time::SystemTime,
    pub profiler: String,
    pub limit: String,

    /// Canonical scene tree: every concrete phase + scope header
    /// wired by parent / children pointers. The pre-mapped
    /// scenario shape lands here once via `scenario_pre_mapped`;
    /// lifecycle callbacks (`set_phase_running` / `_completed` /
    /// `_failed`) mutate node statuses in place.
    pub tree: SceneTree,
    /// Heavy per-phase metrics produced at completion. Keyed by
    /// `SceneNodeId` so scope-level renderers can also look up
    /// child summaries cheaply. Side-map (rather than baked into
    /// `SceneNode`) so the tree itself stays small / serializable.
    pub summaries: HashMap<SceneNodeId, PhaseSummary>,
    /// Latest sysmon sample window (host utilization), when the session-level
    /// sampler is enabled. Refreshed every sample interval; `None` when the
    /// sampler is off, so display surfaces add nothing.
    pub sysmon: Option<nmbrs_runtime::sysmon::SysmonSample>,
    /// Side-map of the session clock reading captured when each leaf
    /// reached a terminal state (completed/failed), keyed by
    /// `SceneNodeId`. Mirrors `summaries`; feeds `PhaseEntry::session_elapsed`
    /// so a finished row persists the session time at which it finished.
    pub phase_session_elapsed: HashMap<SceneNodeId, f64>,
    /// Session clock at each phase's Running transition — see
    /// [`PhaseEntry::session_started`]. Keyed like
    /// `phase_session_elapsed`.
    pub phase_session_started: HashMap<SceneNodeId, f64>,
    /// Op-level status leaves keyed by their parent phase's `SceneNodeId`,
    /// in arrival order. Populated only for ops declaring `readout: visible`
    /// (via the `readout` wrapper's op-lifecycle events); empty otherwise, so
    /// there is no cost for the common case.
    pub phase_ops: HashMap<SceneNodeId, Vec<OpEntry>>,
    /// Denormalized DFS view of `tree` in display order, kept in
    /// sync by `rebuild_phases()` after every tree mutation. Read
    /// by the renderer hot paths via `state.phases` indexing —
    /// rebuilding once per mutation beats walking the tree per
    /// frame, and the cost is negligible (mutations are a handful
    /// per phase lifecycle, not per-op).
    pub phases: Vec<PhaseEntry>,

    /// Phase-count denominator from the pre-map walk —
    /// pinned at `install_tree` time and never mutated by
    /// runtime phase materialization. The renderer's `X/N`
    /// margin display reads this as the stable Y, so an
    /// operator watching the progress sees a fixed total
    /// rather than one that drifts upward as `for_each`
    /// expansion lands new phases at runtime.
    ///
    /// When the runtime materializes more phases than the
    /// pre-map enumerated (param-driven `for_each` whose iter
    /// source the structural walker couldn't resolve), the
    /// numerator can exceed `expected_total_phases` — that's
    /// the honest signal that the planning walk under-counted,
    /// surfaced as `N/Y` with `N > Y` rather than papered
    /// over by a moving denominator.
    pub expected_total_phases: usize,

    /// Every phase currently in flight, keyed by (name, labels).
    /// Empty between phases. Multi-phase scenarios (stanza-level
    /// parallelism, multi-activity sessions) populate more than
    /// one entry. Most read sites today still assume at most one
    /// running phase — those use [`Self::first_active`] as a
    /// compatibility shim over the map.
    pub active_phases: HashMap<PhaseKey, ActivePhase>,

    /// Latency percentiles from last capture (nanoseconds).
    pub min_nanos: u64,
    pub p50_nanos: u64,
    pub p90_nanos: u64,
    pub p99_nanos: u64,
    pub p999_nanos: u64,
    pub max_nanos: u64,

    /// Log ring buffer (last 200 entries). Displayed in TUI log panel.
    pub log_messages: Vec<LogEntry>,
    /// Monotonic sequence of every push to `log_messages`, *never*
    /// decremented. The ring keeps only the last 200 entries; this
    /// counter keeps growing forever so [`crate::display_sink`]
    /// implementations can detect "new entries since I last
    /// drained" without wrestling the ring. A renderer remembers
    /// the last value it printed; on each tick it looks at
    /// `log_seq_total - last_printed` and reads that many entries
    /// off the tail of `log_messages`. Overflow (renderer slower
    /// than 200 events / poll) is detectable as `delta >
    /// log_messages.len()` and the renderer can emit a "dropped N
    /// log lines" notice.
    pub log_seq_total: u64,

    /// Rolling ops/s history for sparkline (last 120 samples).
    pub ops_history: Vec<f64>,
    /// Rolling secondary-counter history for sparkline. The counter
    /// sampled is whichever adapter counter is first in
    /// `active.adapter_counters` — only populated when an adapter
    /// actually reports one, so it's never a hardcoded "rows".
    pub rows_history: Vec<f64>,
    /// Display label for the secondary sparkline (e.g. "rows/s" or
    /// "inserted/s"). `None` when no adapter counter is being tracked.
    pub rows_sparkline_label: Option<String>,
    /// Rolling max-latency history (nanoseconds). Sampled every drain
    /// tick (~250ms). Used by the latency panel to mark windowed peaks
    /// (e.g., "last 5s", "last 10s") with cross-bar glyphs.
    pub max_history: Vec<u64>,
    /// Rolling per-percentile histories, one push per frame delivered
    /// by the metrics scheduler (≈1 Hz). Fed to the time-series latency
    /// view and the short-window (5s / 15s max) variants on the
    /// barchart. Bounded at HISTORY_CAP so memory doesn't grow with
    /// the run — for true lifetime statistics use the `*_lifetime`
    /// aggregates below.
    pub min_history: Vec<u64>,
    pub p50_history: Vec<u64>,
    pub p90_history: Vec<u64>,
    pub p99_history: Vec<u64>,
    pub p999_history: Vec<u64>,

    /// Set to true when the run is complete.
    pub finished: bool,
}

impl RunState {
    pub fn new(workload_file: &str, scenario_name: &str, adapter: &str) -> Self {
        Self {
            workload_file: workload_file.to_string(),
            scenario_name: scenario_name.to_string(),
            adapter: adapter.to_string(),
            started_at: Instant::now(),
            started_at_utc: std::time::SystemTime::now(),
            profiler: "off".to_string(),
            limit: "none".to_string(),
            tree: SceneTree::new(),
            summaries: HashMap::new(),
            sysmon: None,
            phase_session_elapsed: HashMap::new(),
            phase_session_started: HashMap::new(),
            phase_ops: HashMap::new(),
            phases: Vec::new(),
            expected_total_phases: 0,
            active_phases: HashMap::new(),
            log_messages: Vec::new(),
            log_seq_total: 0,
            min_nanos: 0,
            p50_nanos: 0,
            p90_nanos: 0,
            p99_nanos: 0,
            p999_nanos: 0,
            max_nanos: 0,
            ops_history: Vec::new(),
            rows_history: Vec::new(),
            rows_sparkline_label: None,
            max_history: Vec::new(),
            min_history: Vec::new(),
            p50_history: Vec::new(),
            p90_history: Vec::new(),
            p99_history: Vec::new(),
            p999_history: Vec::new(),
            finished: false,
        }
    }

    /// Borrow any one currently-running phase, if any exist.
    /// Compatibility shim for call sites that still assume a single
    /// running phase (ETA display, header labels, …). Multi-phase
    /// call sites should iterate [`Self::active_phases`] directly.
    pub fn first_active(&self) -> Option<&ActivePhase> {
        self.active_phases.values().next()
    }

    /// Borrow the active-phase entry matching a specific (name,
    /// labels) pair — used when the caller already knows which
    /// phase row it's rendering detail for.
    pub fn active_phase(&self, name: &str, labels: &str) -> Option<&ActivePhase> {
        // Compat lookup by (name, labels) — display detail paths only have
        // a scene-tree row, not yet an exec_id (SRD-100 P1c threads exec_id
        // through the scene tree). First match: correct for a single
        // execution; ambiguous under concurrent executions of the same
        // phase (same as the pre-SRD-100 (name,labels) key), resolved in P1c.
        self.active_phases
            .values()
            .find(|a| a.name == name && a.labels == labels)
    }

    /// Mutable borrow of the active-phase entry for a specific
    /// (exec_id, name, labels) key. Used by the observer's progress
    /// callback to update in place.
    pub fn active_phase_mut(
        &mut self,
        exec_id: u64,
        name: &str,
        labels: &str,
    ) -> Option<&mut ActivePhase> {
        self.active_phases
            .get_mut(&ActivePhaseId::new(exec_id, name, labels))
    }

    /// Push a log entry to the ring buffer (capped at 200).
    /// `log_seq_total` increments unconditionally so display sinks
    /// can detect new-since-last-drain without inspecting the ring.
    pub fn push_log(&mut self, severity: LogSeverity, message: String) {
        self.push_log_tagged(severity, EventTag::default(), message);
    }

    /// [`Self::push_log`] with an explicit [`EventTag`]. The tag
    /// travels with the entry into the ring so the terminal sink
    /// can derive its filtering (boundary-attached renders out of
    /// scrollback) from the axes at drain time.
    pub fn push_log_tagged(&mut self, severity: LogSeverity, tag: EventTag, message: String) {
        self.push_log_entry(LogEntry {
            severity,
            message,
            tag,
            at: std::time::SystemTime::now(),
        });
    }

    /// Push a pre-built [`LogEntry`] into the ring and return the
    /// new [`Self::log_seq_total`] it was assigned. The ring stays
    /// bounded (last 200) — it backs the inspector `log` view, the
    /// TUI log panel, and the post-run failure dump, none of which
    /// need unbounded history. The **durable, no-drop scrollback
    /// stream** is a separate channel the actor feeds in lock-step
    /// with this push (see
    /// [`crate::run_state_actor::spawn_run_state_actor`]); the
    /// returned seq tags the matching stream item so a
    /// terminal-mode sink can skip lines already emitted elsewhere
    /// (pre-handoff stderr, or a prior sink's scrollback).
    pub fn push_log_entry(&mut self, entry: LogEntry) -> u64 {
        self.log_messages.push(entry);
        if self.log_messages.len() > 200 {
            self.log_messages.remove(0);
        }
        self.log_seq_total = self.log_seq_total.saturating_add(1);
        self.log_seq_total
    }

    /// Push an ops/s sample to the sparkline history (capped at 120).
    pub fn push_ops_sample(&mut self, ops_per_sec: f64) {
        self.ops_history.push(ops_per_sec);
        if self.ops_history.len() > 120 {
            self.ops_history.remove(0);
        }
    }

    /// Push a rows/s sample to the sparkline history.
    pub fn push_rows_sample(&mut self, rows_per_sec: f64) {
        self.rows_history.push(rows_per_sec);
        if self.rows_history.len() > 120 {
            self.rows_history.remove(0);
        }
    }

    /// Replace the scene tree wholesale — called once from the
    /// observer's `scenario_pre_mapped` hook with the fully
    /// resolved pre-map. Existing summaries are dropped (they
    /// never apply across pre-maps).
    pub fn install_tree(&mut self, tree: SceneTree) {
        self.tree = tree;
        self.summaries.clear();
        self.phase_session_elapsed.clear();
        self.phase_session_started.clear();
        self.phase_ops.clear();
        self.rebuild_phases();
        // Pin the pre-map's phase count. Read by the margin
        // renderer as the stable denominator so a refine /
        // for_each / runtime-materialized phase doesn't drift
        // the displayed total. The pre-map walker's
        // `pre_map_only` flag (executor.rs) suppresses sentinel
        // status mutations during the pre-map pass, so this
        // count reflects pending phases only.
        self.expected_total_phases = self.tree.total_phases();
    }

    /// Add a pending phase to the tree at the synthetic root —
    /// fallback path for sources that don't pre-map.
    pub fn add_phase(&mut self, name: &str, labels: &str, _depth: usize) {
        let root = self.tree.root();
        self.tree.push(root, EntryKind::Phase, name, labels);
        self.rebuild_phases();
    }

    /// Add a visual grouping header at the synthetic root.
    pub fn add_scope(&mut self, label: &str, _depth: usize) {
        let root = self.tree.root();
        self.tree.push(root, EntryKind::Scope, label, "");
        self.rebuild_phases();
    }

    /// Resolve the scene-tree node id to mutate for a lifecycle
    /// transition (SRD-100 P1c). Prefers the dispatch-time
    /// `scene_node_id` threaded from the executor — race-safe under
    /// concurrent same-name dispatch — when it addresses a matching
    /// `Phase` node **in the wanted state**. Falling back to the
    /// by-name [`SceneTree::find_phase`] (filtered by `want`)
    /// otherwise covers two cases the threaded id can't:
    ///
    /// 1. **Runtime-materialized phases** absent from this tree
    ///    (pushed by the executor after `scenario_pre_mapped`
    ///    snapshotted it).
    /// 2. **Sequential loop re-runs** (`do_while` / `do_until` /
    ///    phase-level `for_each`) where the executor reuses ONE
    ///    global node id across iterations (`SceneTree::push` is
    ///    idempotent by name). After iteration 1 completes, that id
    ///    is no longer `Pending` — so the next `set_phase_running`
    ///    falls through to `find_phase(Pending)`, misses, and
    ///    appends a fresh per-iteration node, preserving the
    ///    one-row-per-iteration rollup the post-run summary shows.
    ///
    /// The `want` filter is what distinguishes (2) — a re-used id
    /// whose node is already terminal — from a genuine concurrent
    /// sibling, whose own node is still in the wanted state when its
    /// transition fires.
    fn resolve_phase_node(
        &self,
        scene_node_id: SceneNodeId,
        name: &str,
        labels: &str,
        want: Option<&PhaseStatus>,
    ) -> Option<SceneNodeId> {
        let id_addresses_live_phase = self.tree.nodes.get(scene_node_id).is_some_and(|n| {
            n.kind == EntryKind::Phase
                && n.name == name
                && match want {
                    // Running/completed transitions: the threaded id wins
                    // only when its node is in the exact wanted state.
                    Some(w) => &n.status == w,
                    // Failure (want=None): the id wins for an *active*
                    // (Pending/Running) node — the phase that just failed.
                    // A terminal node at this id is a re-used loop id, so
                    // fall through to the by-name lookup instead.
                    None => !matches!(n.status, PhaseStatus::Completed | PhaseStatus::Failed(_)),
                }
        });
        if id_addresses_live_phase {
            return Some(scene_node_id);
        }
        // Fallback: runtime-materialized phase, or a re-used loop id whose node
        // is no longer in the wanted state.
        match want {
            // Failure: target the LIVE iteration — Running, else Pending — so a
            // failing loop iteration N marks ITS row, not iteration 1's already
            // -completed one (`find_phase(.., None)` returns the first-by-DFS
            // node, which is the stale terminal row). Last-resort any-status
            // match keeps a degenerate single-row tree working.
            None => self
                .tree
                .find_phase(name, labels, Some(&PhaseStatus::Running))
                .or_else(|| {
                    self.tree
                        .find_phase(name, labels, Some(&PhaseStatus::Pending))
                })
                .or_else(|| self.tree.find_phase(name, labels, None)),
            some => self.tree.find_phase(name, labels, some),
        }
    }

    /// Mark a phase as running, keyed by the dispatch-time
    /// `scene_node_id` (SRD-100 P1c). Pre-mapped phases flip their
    /// own node directly; a phase the pre-map never enumerated is
    /// pushed dynamically so it still gets a tree slot.
    pub fn set_phase_running(
        &mut self,
        scene_node_id: SceneNodeId,
        name: &str,
        labels: &str,
        op_count: usize,
    ) {
        let session_now = self.elapsed_secs();
        if let Some(id) =
            self.resolve_phase_node(scene_node_id, name, labels, Some(&PhaseStatus::Pending))
        {
            self.tree.set_phase_running_at(id, op_count);
            self.phase_session_started.insert(id, session_now);
        } else {
            // Not found — add dynamically and mark running. Phases
            // that weren't pre-mapped (e.g. unresolvable for_each
            // at pre-map time) still need a tree slot.
            let root = self.tree.root();
            let id = self.tree.push(root, EntryKind::Phase, name, labels);
            self.tree.set_phase_running_at(id, op_count);
            self.phase_session_started.insert(id, session_now);
        }
        self.rebuild_phases();
    }

    /// Mark a phase as completed and attach a metrics summary,
    /// keyed by the dispatch-time `scene_node_id` (SRD-100 P1c).
    /// Final per-op timing leaves for a phase at its completion —
    /// the durable form of the footer's SRD-63 op rows, preserved
    /// into scrollback under the ✓ outcome block. Terminal ops carry
    /// their execution time and session finish-stamp; an op still
    /// mid-flight at phase end (a cancelled daemon probe) shows `—`.
    /// Empty when the phase declared no `readout: visible` ops.
    /// Returns `(line, duration_cell)` pairs: the leaf body carries
    /// icon · name · `[i/N]` · `@ session-stamp`; the node's own
    /// execution DURATION is the second element, rendered as the
    /// leaf row's default gutter cell (SRD-92: a visible node with
    /// no declared gutter gets its execution time as the cell —
    /// the scannable per-step time ledger). Single placement: the
    /// duration appears only in the cell, the session stamp only
    /// in the body.
    pub fn final_op_leaves(&self, name: &str, labels: &str) -> Vec<(String, String)> {
        let node_id = match self.phases.iter().find(|e| {
            e.name == name && e.labels == labels && matches!(e.status, PhaseStatus::Running)
        }) {
            Some(e) => e.node_id,
            None => return Vec::new(),
        };
        let ops = match self.phase_ops.get(&node_id) {
            Some(ops) if !ops.is_empty() => ops,
            _ => return Vec::new(),
        };
        let total = ops.len();
        ops.iter()
            .map(|op| {
                let icon = match &op.status {
                    PhaseStatus::Completed => "✓",
                    PhaseStatus::Failed(_) => "✗",
                    _ => "—",
                };
                // Same form as the live leaf: what the step produced, then how
                // long it took. The settled scrollback row is where this matters
                // most — it is the copy a reader comes back to.
                let duration_cell = op
                    .duration_secs
                    .map(|d| match op.measure.as_deref() {
                        Some(m) => format!("[{m}] {}", crate::widgets::format_dur_compact(d)),
                        None => crate::widgets::format_dur_compact(d),
                    })
                    .unwrap_or_else(|| "—".to_string());
                let stamp = match op.session_elapsed {
                    Some(v) => format!("  @ {}", crate::widgets::format_dur_compact(v)),
                    None => String::new(),
                };
                let mut line = format!(
                    "    {icon} {name}  [{seq}/{total}]{stamp}",
                    name = op.name,
                    seq = op.seq + 1,
                );
                if let PhaseStatus::Failed(err) = &op.status {
                    line.push_str("  ");
                    line.push_str(err);
                }
                (line, duration_cell)
            })
            .collect()
    }

    pub fn set_phase_completed(
        &mut self,
        scene_node_id: SceneNodeId,
        name: &str,
        labels: &str,
        duration_secs: f64,
        summary: PhaseSummary,
    ) {
        // Capture the session clock BEFORE the mutable-borrow block so a
        // finished leaf persists the session time at which it finished.
        let session_now = self.elapsed_secs();
        // `skipped_phases=elide|prune`: a FULLY-SKIPPED phase (every op
        // `if:`-gated off, nothing measured) leaves no completed-tree
        // entry — remove its node instead of folding it in. `mark` (the
        // default) keeps it, rendered with the runtime's ⊘ outcome.
        let fully_skipped = summary.skips > 0
            && summary.ops_finished > 0
            && summary.skips >= summary.ops_finished
            && summary.ops_ok == 0
            && summary.errors == 0;
        if fully_skipped
            && matches!(
                nmbrs_runtime::observer::skipped_phase_display(),
                nmbrs_runtime::observer::SkippedPhaseDisplay::Elide
                    | nmbrs_runtime::observer::SkippedPhaseDisplay::Prune
            )
        {
            if let Some(id) =
                self.resolve_phase_node(scene_node_id, name, labels, Some(&PhaseStatus::Running))
            {
                self.tree.remove_node(id);
                self.summaries.remove(&id);
                self.phase_session_started.remove(&id);
                self.rebuild_phases();
            }
            return;
        }
        if let Some(id) =
            self.resolve_phase_node(scene_node_id, name, labels, Some(&PhaseStatus::Running))
        {
            // Displayed duration is a session-clock delta so
            // `session_started + duration == session_elapsed` holds
            // exactly on every rendered row. The executor-measured
            // `duration_secs` is the fallback when the start was never
            // observed (runtime-materialized phase).
            let display_duration = self
                .phase_session_started
                .get(&id)
                .map(|s| (session_now - s).max(0.0))
                .unwrap_or(duration_secs);
            self.tree.set_phase_completed_at(id, display_duration);
            self.summaries.insert(id, summary);
            self.phase_session_elapsed.insert(id, session_now);
            self.rebuild_phases();
        }
    }

    /// Mark a phase as failed, keyed by the dispatch-time
    /// `scene_node_id` (SRD-100 P1c).
    pub fn set_phase_failed(
        &mut self,
        scene_node_id: SceneNodeId,
        name: &str,
        labels: &str,
        error: &str,
    ) {
        let session_now = self.elapsed_secs();
        if let Some(id) = self.resolve_phase_node(scene_node_id, name, labels, None) {
            self.tree.set_phase_failed_at(id, error);
            self.phase_session_elapsed.insert(id, session_now);
            self.rebuild_phases();
        }
    }

    /// An op-level status leaf started (SRD-63). Creates or re-arms the entry
    /// (keyed by name within the parent phase) as Running with a fresh timer.
    pub fn op_starting(&mut self, parent: SceneNodeId, name: &str) {
        let session_now = self.elapsed_secs();
        let ops = self.phase_ops.entry(parent).or_default();
        if let Some(e) = ops.iter_mut().find(|e| e.name == name) {
            e.status = PhaseStatus::Running;
            e.started_at = Instant::now();
            e.session_started = session_now;
            e.duration_secs = None;
            e.session_elapsed = None;
        } else {
            let seq = ops.len();
            ops.push(OpEntry {
                measure: None,
                name: name.to_string(),
                status: PhaseStatus::Running,
                started_at: Instant::now(),
                session_started: session_now,
                duration_secs: None,
                session_elapsed: None,
                seq,
            });
        }
    }

    /// An op-level status leaf completed; persists its execution time and the
    /// session clock at completion.
    /// Record an op's key measurable. Arrives just before completion, so the
    /// leaf row has it the moment it settles.
    pub fn op_measure(&mut self, parent: SceneNodeId, op_name: &str, text: &str) {
        if let Some(ops) = self.phase_ops.get_mut(&parent)
            && let Some(op) = ops.iter_mut().find(|o| o.name == op_name)
        {
            op.measure = Some(text.to_string());
        }
    }

    pub fn op_completed(&mut self, parent: SceneNodeId, name: &str, duration_secs: f64) {
        let session_now = self.elapsed_secs();
        if let Some(ops) = self.phase_ops.get_mut(&parent) {
            if let Some(e) = ops.iter_mut().find(|e| e.name == name) {
                e.status = PhaseStatus::Completed;
                // Session-clock delta, not the executor's own measure —
                // see PhaseEntry::session_started. Executor value is the
                // fallback for a start that was never observed.
                let _ = duration_secs;
                e.duration_secs = Some((session_now - e.session_started).max(0.0));
                e.session_elapsed = Some(session_now);
            }
        }
    }

    /// An op-level status leaf failed; freezes its elapsed time and session clock.
    pub fn op_failed(&mut self, parent: SceneNodeId, name: &str, error: &str) {
        let session_now = self.elapsed_secs();
        if let Some(ops) = self.phase_ops.get_mut(&parent) {
            if let Some(e) = ops.iter_mut().find(|e| e.name == name) {
                e.duration_secs = Some((session_now - e.session_started).max(0.0));
                e.status = PhaseStatus::Failed(error.to_string());
                e.session_elapsed = Some(session_now);
            }
        }
    }

    /// Elapsed time since run started.
    pub fn elapsed_secs(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64()
    }

    /// The uncolored status-margin body (`session · [n/N] · leaf`)
    /// for the CURRENT state. The run-state actor stamps this onto
    /// every scrollback line at Log-command processing time, so a
    /// line's gutter is computed from exactly the state that
    /// preceded it in the command stream — it can never disagree
    /// with the event the line describes (the old render-time
    /// margin was one snapshot for a whole drained batch, and raced
    /// the very transitions the lines reported).
    pub fn margin_body_stamp(&self) -> String {
        let secs = self.elapsed_secs();
        let phase_only: Vec<_> = self
            .phases
            .iter()
            .filter(|p| matches!(p.kind, EntryKind::Phase))
            .collect();
        let total = self.expected_total_phases;
        let running = phase_only
            .iter()
            .find(|p| matches!(p.status, PhaseStatus::Running));
        let running_seq = running.and_then(|p| p.seq);
        let latest_done_seq = phase_only
            .iter()
            .filter(|p| !matches!(p.status, PhaseStatus::Pending))
            .filter_map(|p| p.seq)
            .max();
        let fallback_done = phase_only
            .iter()
            .filter(|p| !matches!(p.status, PhaseStatus::Pending))
            .count();
        let count = match (running_seq, latest_done_seq, total) {
            (Some(s), _, n) if n > 0 => format!("[{s}/{n}]"),
            (None, Some(s), n) if n > 0 => format!("[{s}/{n}]"),
            (None, None, n) if n > 0 && fallback_done > 0 => format!("[{fallback_done}/{n}]"),
            (_, _, n) if n > 0 => format!("[0/{n}]"),
            _ => "[-/-]".to_string(),
        };
        let leaf = running
            .and_then(|p| self.active_phase(&p.name, &p.labels))
            .map(|a| (secs - a.session_started).max(0.0));
        crate::widgets::margin_body(total, &count, leaf, Some(secs))
    }

    /// Rebuild the denormalized `phases` view from the current
    /// tree. Called after every mutation that affects the DFS
    /// order or any node's display fields.
    fn rebuild_phases(&mut self) {
        self.phases = self
            .tree
            .dfs()
            .filter(|n| n.kind != EntryKind::Root)
            .map(|n| PhaseEntry {
                node_id: n.id,
                name: n.name.clone(),
                labels: n.labels.clone(),
                status: n.status.clone(),
                kind: n.kind,
                op_count: n.op_count,
                duration_secs: n.duration_secs,
                // SceneNode depths count the synthetic root as 1 —
                // subtract so top-level scenario entries land at
                // depth 0 (matching pre-tree behavior).
                depth: n.depth.saturating_sub(1),
                summary: self.summaries.get(&n.id).cloned(),
                session_elapsed: self.phase_session_elapsed.get(&n.id).copied(),
                session_started: self.phase_session_started.get(&n.id).copied(),
                op_names: n.op_names.clone(),
                seq: n.seq,
            })
            .collect();
    }

    /// Total `Phase` entries in the pre-mapped scene tree —
    /// denominator for the `[N/total]` and `phase X/Y` displays.
    pub fn total_phases(&self) -> usize {
        self.tree.total_phases()
    }
}

#[cfg(test)]
mod resolve_tests {
    //! SRD-100 P1c — `resolve_phase_node` is the reconciliation between
    //! id-keyed routing (race-safe under concurrent distinct-node dispatch)
    //! and the by-name fallback (runtime-materialized phases + sequential
    //! loop id-reuse). These drive it through the public `set_phase_*` API.
    use super::*;

    /// A two-cell distinct-node tree: two same-name `p` phases under DISTINCT
    /// parent scopes (the topology scenario-level `for_each` /
    /// `for_combinations` / nesting produce, where each cell gets its own id).
    fn two_cell_tree() -> (SceneTree, SceneNodeId, SceneNodeId) {
        let mut t = SceneTree::new();
        let s1 = t.push(t.root(), EntryKind::Scope, "x=1", "");
        let p1 = t.push(s1, EntryKind::Phase, "p", "x=1");
        let s2 = t.push(t.root(), EntryKind::Scope, "x=2", "");
        let p2 = t.push(s2, EntryKind::Phase, "p", "x=2");
        (t, p1, p2)
    }

    fn row(s: &RunState, id: SceneNodeId) -> &PhaseEntry {
        s.phases
            .iter()
            .find(|e| e.node_id == id)
            .expect("row for id")
    }

    fn running_count(s: &RunState) -> usize {
        s.phases
            .iter()
            .filter(|e| matches!(e.status, PhaseStatus::Running))
            .count()
    }

    /// The id-keyed-wins branch: two distinct same-name nodes, completed in
    /// REVERSED order, each keep their own op_count / duration / status. With
    /// the pre-P1c by-name routing the first completion would land on the
    /// first-DFS node and mis-attribute.
    #[test]
    fn id_keyed_wins_under_reordered_completion() {
        let (tree, p1, p2) = two_cell_tree();
        let mut s = RunState::new("", "", "");
        s.install_tree(tree);
        s.set_phase_running(p1, "p", "x=1", 3);
        s.set_phase_running(p2, "p", "x=2", 7);
        s.set_phase_completed(p2, "p", "x=2", 10.0, PhaseSummary::default());
        s.set_phase_completed(p1, "p", "x=1", 5.0, PhaseSummary::default());
        // Displayed durations are session-clock deltas (NOT the
        // executor-passed 10.0/5.0): each row must reconcile exactly
        // against its own session columns, the single-time-basis
        // invariant the gutter readout depends on.
        for p in [p1, p2] {
            let r = row(&s, p);
            let d = r.duration_secs.expect("completed row has duration");
            let reconciled = r.session_elapsed.expect("session_elapsed set")
                - r.session_started.expect("session_started set");
            assert!(
                (d - reconciled).abs() < 1e-9,
                "duration {d} must equal session_elapsed - session_started {reconciled}"
            );
        }
        assert_eq!(row(&s, p1).op_count, 3);
        assert_eq!(row(&s, p2).op_count, 7);
        assert!(matches!(row(&s, p1).status, PhaseStatus::Completed));
        assert!(matches!(row(&s, p2).status, PhaseStatus::Completed));
    }

    /// Sequential loop id-reuse: the executor reuses ONE node id across
    /// iterations (push is idempotent by name). Iteration 2's `running`
    /// falls through to append a fresh live row; a FAILURE on iteration 2
    /// (reported with the reused, now-Completed id) must mark the LIVE row,
    /// leaving iteration 1's completed row intact and nothing stranded
    /// Running. Fails against the naive `find_phase(.., None)` failure
    /// fallback (which grabs the first-DFS terminal row).
    #[test]
    fn reused_loop_id_appends_and_failure_targets_live_row() {
        // The looped phase sits under a scope (a `do_while` / `for_each`
        // header), as it does in a real run — so the append-on-miss fallback
        // (which pushes under root) yields a node distinct from the reused
        // scoped one, rather than colliding with it via push idempotency.
        let mut t = SceneTree::new();
        let scope = t.push(t.root(), EntryKind::Scope, "do_while", "");
        let p = t.push(scope, EntryKind::Phase, "p", "");
        let mut s = RunState::new("", "", "");
        s.install_tree(t);
        // iteration 1 reuses the pre-mapped node `p`
        s.set_phase_running(p, "p", "i=1", 1);
        s.set_phase_completed(p, "p", "i=1", 1.0, PhaseSummary::default());
        // iteration 2 reuses the same id (now Completed) -> append a live row
        s.set_phase_running(p, "p", "i=2", 1);
        assert_eq!(
            running_count(&s),
            1,
            "iteration 2 appended a distinct live row"
        );
        // iteration 2 fails, reported with the reused id `p`
        s.set_phase_failed(p, "p", "i=2", "boom");
        assert!(
            matches!(row(&s, p).status, PhaseStatus::Completed),
            "iteration 1's row stays Completed, got {:?}",
            row(&s, p).status
        );
        let failed = s
            .phases
            .iter()
            .filter(|e| matches!(e.status, PhaseStatus::Failed(_)))
            .count();
        assert_eq!(failed, 1, "exactly the live iteration-2 row failed");
        assert_eq!(running_count(&s), 0, "no row left stranded Running");
    }
}

#[cfg(test)]
mod measure_tests {
    use super::*;

    /// A settled leaf row carries WHAT the step produced ahead of how long it
    /// took. Duration alone answers "did it hang" — the less interesting
    /// question once a step is done.
    #[test]
    fn settled_leaf_shows_measure_before_duration() {
        let mut st = RunState::new("w.yaml", "s", "stdout");
        let node: SceneNodeId = 1;
        st.phases.push(PhaseEntry {
            node_id: node,
            name: "finalize".into(),
            labels: String::new(),
            status: PhaseStatus::Running,
            kind: EntryKind::Phase,
            op_count: 1,
            duration_secs: None,
            session_elapsed: None,
            session_started: None,
            depth: 0,
            summary: None,
            op_names: Vec::new(),
            seq: None,
        });
        st.op_starting(node, "read_sstables");
        st.op_measure(node, "read_sstables", "12 sst");
        st.op_completed(node, "read_sstables", 0.3);

        let leaves = st.final_op_leaves("finalize", "");
        assert_eq!(leaves.len(), 1);
        let (_, cell) = &leaves[0];
        assert!(
            cell.starts_with("[12 sst] "),
            "measure precedes the duration in the gutter cell: {cell:?}"
        );
    }

    /// An op with no `measure:` renders exactly as before — the feature is
    /// additive, and every existing workload keeps its current output.
    #[test]
    fn leaf_without_measure_is_unchanged() {
        let mut st = RunState::new("w.yaml", "s", "stdout");
        let node: SceneNodeId = 1;
        st.phases.push(PhaseEntry {
            node_id: node,
            name: "finalize".into(),
            labels: String::new(),
            status: PhaseStatus::Running,
            kind: EntryKind::Phase,
            op_count: 1,
            duration_secs: None,
            session_elapsed: None,
            session_started: None,
            depth: 0,
            summary: None,
            op_names: Vec::new(),
            seq: None,
        });
        st.op_starting(node, "plain");
        st.op_completed(node, "plain", 0.3);

        let (_, cell) = &st.final_op_leaves("finalize", "")[0];
        assert!(
            !cell.contains('['),
            "no brackets without a measure: {cell:?}"
        );
    }
}

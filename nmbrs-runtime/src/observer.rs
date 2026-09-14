// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Run observer: callback trait for phase lifecycle events.
//!
//! The executor notifies observers when phases start, complete,
//! or fail. The TUI implements this to update its display state.
//! The default stderr observer prints phase progress lines.

/// Log level for diagnostic messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    /// Highest-volume per-cycle/per-event tracing (recall pipeline,
    /// adapter call traces). Below `Debug` so the existing file
    /// sink default (Debug) does not pick up trace traffic. Trace
    /// events instead route through the [`crate::trace_router`]
    /// to optionally-filtered per-label files.
    Trace,
    /// Detailed diagnostics (parse notes, connection info)
    Debug,
    /// Normal operational messages (phase info, metrics paths)
    Info,
    /// Warnings (CQL driver warnings, recoverable errors)
    Warn,
    /// Errors (phase failures, binding errors)
    Error,
}

/// Provenance tag carried alongside a log message so display
/// sinks can route by *kind*, not by sniffing message text.
///
/// The only consumer today is the `tui=terminal` log sink: it
/// owns a managed phase-history region (the idempotent catch-up
/// projection of the scene tree), so the phase start/end readout
/// lines that would otherwise scroll past in the log stream are
/// tagged `attached: PhaseStart` and suppressed from that
/// stream — they live in the region instead. Every other surface
/// (`session.log`, the failure dump, the full TUI panel) treats
/// all events alike; the tag is purely additive.
///
/// The tag is TWO ORTHOGONAL AXES (they were one conflated enum
/// once, with `Diagnostic` — "anything else" — riding beside
/// boundary-attached kinds, and sinks keying presentation rules
/// off the conflation):
///
/// - [`EventTag::attached`] — WHERE on the execution lifecycle the
///   event belongs, in the canonical lifecycle vocabulary
///   ([`crate::lifecycle::EventType`]: session/scope/each/phase
///   boundaries). `None` = in-flight — emitted from the running
///   body, bound to no boundary.
/// - [`EventTag::category`] — WHAT the event is about
///   ([`EventCategory`]): the semantic domain, independent of when
///   it fired.
///
/// Presentation rules derive from the axes instead of naming
/// bundles: "hide phase-start renders from scrollback" keys on
/// `attached == Some(PhaseStart)`; "the TUI log panel shows only
/// in-flight diagnostics" keys on `attached.is_none()`; "detail
/// rows fold with their block" keys on `attached == Some(PhaseEnd)
/// && category != Outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventTag {
    /// The execution-lifecycle boundary this event is attached to;
    /// `None` = in-flight.
    pub attached: Option<crate::lifecycle::EventType>,
    /// Semantic category.
    pub category: EventCategory,
}

impl EventTag {
    /// An in-flight event of the given category (no boundary).
    pub const fn in_flight(category: EventCategory) -> Self {
        Self {
            attached: None,
            category,
        }
    }
    /// An event attached to a lifecycle boundary.
    pub const fn at(moment: crate::lifecycle::EventType, category: EventCategory) -> Self {
        Self {
            attached: Some(moment),
            category,
        }
    }
}

/// The semantic domain of a structured event — orthogonal to the
/// lifecycle moment it attaches to. Extend as producers appear;
/// a category earns a variant when some consumer (sink filter,
/// counter, panel) needs to dispatch on it without string-matching
/// rendered prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventCategory {
    /// Ordinary diagnostics with no more specific domain (the
    /// overwhelming majority).
    #[default]
    General,
    /// An outcome projection (the `✓`/`✗` phase_outcome block).
    /// SRD-81: a display projection, not a diagnostic.
    Outcome,
    /// Evaluation detail attached to an outcome (relevancy
    /// `mean=/p50=` rows, verify summaries). SRD-92: detail rows
    /// belong to the block above them.
    Evaluation,
    /// Retry-plane visibility: first-sighting advisories and
    /// sampled counter-exemplars (`exec_events`).
    Retry,
    /// Adaptive backpressure governance (SRD-83 Part 9
    /// `throttle:`).
    Throttle,
    /// Reference resolution (SRD-85 nearest-first shadowing
    /// warnings).
    Resolution,
    /// Report-block warnings (SRD-46).
    Report,
}

/// Kind of pre-mapped scenario entry. Re-export of
/// [`crate::scene_tree::NodeKind`] for callers that already
/// imported it via the observer module.
pub use crate::scene_tree::NodeKind as PreMapKind;

/// Lifecycle events from the executor.
pub trait RunObserver: Send + Sync {
    /// A completed sysmon sample window (session-level host utilization).
    /// Display surfaces override; everything else ignores it.
    fn sysmon_update(&self, _sample: &crate::sysmon::SysmonSample) {}

    /// The session directory now exists. Display surfaces that persist a
    /// transcript open it here — the runner creates the directory well after
    /// the observer is built, so this is the first moment the path is known.
    fn session_dir_ready(&self, _dir: &std::path::Path) {}

    /// A phase is about to start executing.
    ///
    /// `op_templates` is the count of op definitions in the phase
    /// (typically 1 for query workloads). `total_cycles` is the
    /// number of times the stanza will iterate. Both are reported
    /// because they answer different questions: the first describes
    /// the *shape* of the phase, the second describes the *amount*
    /// of work it represents.
    ///
    /// `scene_node_id` is the phase's dispatch-time
    /// [`crate::scene_tree::SceneNodeId`] (SRD-100 P1c) — the stable
    /// row key for lifecycle routing. Observers flip *that* node
    /// directly instead of re-matching by `(name, status)` in DFS
    /// order, which races under concurrent same-name dispatch
    /// (sweep cells, comprehension iterations, daemon+foreground).
    fn phase_starting(
        &self,
        scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        op_templates: usize,
        total_cycles: u64,
        concurrency: usize,
    );

    /// A phase completed successfully. `scene_node_id` keys the
    /// node to flip (see [`Self::phase_starting`]).
    fn phase_completed(
        &self,
        scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        duration_secs: f64,
    );

    /// A phase failed. `scene_node_id` keys the node to flip (see
    /// [`Self::phase_starting`]).
    fn phase_failed(
        &self,
        scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        error: &str,
    );

    /// Update live metrics for the active phase (called at progress tick rate).
    fn phase_progress(&self, update: &PhaseProgressUpdate);

    /// SRD-63 — an op-level execution leaf became visible (`readout: visible`,
    /// enabled by the `readout` wrapper). Nests under `parent_phase` in the
    /// hierarchic status view; the consumer assigns its sequence within the
    /// phase from arrival order. Default no-op so observers that don't render
    /// op-level leaves (and the zero-cost non-readout path) are unaffected.
    fn op_starting(&self, _parent_phase: crate::scene_tree::SceneNodeId, _op_name: &str) {}

    /// An op-level execution leaf completed. `duration_secs` is the op's own
    /// cumulative execution time. Default no-op.
    fn op_completed(
        &self,
        _parent_phase: crate::scene_tree::SceneNodeId,
        _op_name: &str,
        _duration_secs: f64,
    ) {
    }

    /// The op's KEY MEASURABLE, rendered from its `measure:` template at
    /// completion — "12 sstables", "1.4 GiB", "98.1%".
    ///
    /// A leaf row already carries how LONG a step took; this carries what it
    /// actually produced, which is the number a reader is usually after. Sent
    /// as its own hook (rather than widening `op_completed`) so every existing
    /// observer keeps compiling and ignoring it costs nothing.
    fn op_measure(
        &self,
        _parent_phase: crate::scene_tree::SceneNodeId,
        _op_name: &str,
        _text: &str,
    ) {
    }

    /// An op-level execution leaf failed. Default no-op.
    fn op_failed(
        &self,
        _parent_phase: crate::scene_tree::SceneNodeId,
        _op_name: &str,
        _error: &str,
    ) {
    }

    /// SRD-100 P2 — attach the per-phase [`PhaseRenderHandle`] to the live
    /// display fold, **once**, after the activity's metrics/binder exist
    /// (the executor calls this on-task at progress-setup time). Observers
    /// that own a run-state actor (TUI / log-only) route it to an
    /// `AttachPhaseRender` mutation so the consumer can fold `active_phases`
    /// and re-derive each phase's status line itself; the no-op default is
    /// correct for surfaces with no live status fold (Stderr / Headless).
    fn phase_render_attach(&self, _handle: PhaseRenderHandle) {}

    /// The entire run is complete.
    fn run_finished(&self);

    /// Diagnostic log message. Routed to stderr in CLI mode,
    /// to a ring buffer in TUI mode. All `eprintln!` in the
    /// runtime should go through this instead.
    fn log(&self, level: LogLevel, message: &str);

    /// Log a message carrying an explicit [`EventTag`]. The
    /// default ignores the tag and delegates to [`Self::log`] —
    /// correct for every observer whose surface treats all events
    /// alike. Observers that feed a tag-aware sink (the run-state
    /// actor ring) override this to retain both axes.
    fn log_tagged(&self, level: LogLevel, _tag: EventTag, message: &str) {
        self.log(level, message);
    }

    // SRD-100 P2 — `set_status_line` is removed. The status line is no
    // longer produced by submitting a pre-rendered string up to the actor;
    // each display surface folds `active_phases` and re-derives the status
    // itself (`nmbrs_tui::status_fold`). The producer-side render handle now
    // travels via `phase_render_attach` instead.

    /// Whether to suppress the inline stderr progress line
    /// (because the TUI is handling display).
    fn suppresses_stderr(&self) -> bool {
        false
    }

    /// Optional shared flag mirroring [`Self::suppresses_stderr`]
    /// that the runner threads into long-lived components
    /// (e.g. the activity's inline status thread) so they can
    /// react to dismissal mid-run rather than honoring a
    /// snapshot taken at construction. When `None`, the
    /// activity uses a fresh `AtomicBool(false)` (never
    /// suppress). Implementations that go through a TUI
    /// (and only those) typically expose their internal
    /// "tui_active" flag here.
    fn live_suppress_flag(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
        None
    }

    /// Optional reporter to register on the metrics scheduler.
    /// The runner calls this once during setup. Return None for
    /// observers that don't need metrics frames (like StderrObserver).
    ///
    /// Kept for back-compat; for observers that want multiple reporters
    /// at different cadences, override [`reporters`] instead — the
    /// default impl forwards this single reporter as the base cadence.
    fn reporter(&self) -> Option<Box<dyn nmbrs_metrics::scheduler::Reporter>> {
        None
    }

    /// Multiple reporters with explicit cadences. The runner calls
    /// this once during setup. Each `(interval, reporter)` entry is
    /// registered with the scheduler at that interval. The default
    /// implementation returns whatever [`reporter`] produced at the
    /// base 1s cadence, so existing observers work unchanged.
    fn reporters(
        &self,
    ) -> Vec<(
        std::time::Duration,
        Box<dyn nmbrs_metrics::scheduler::Reporter>,
    )> {
        match self.reporter() {
            Some(r) => vec![(std::time::Duration::from_secs(1), r)],
            None => vec![],
        }
    }

    /// User-declared cadences for this observer's consumers (SRD-42).
    ///
    /// When present, the runner uses these to plan the cadence tree
    /// passed to the scheduler's [`nmbrs_metrics::cadence_reporter::CadenceReporter`].
    /// The reporter writes all windowed snapshots into a single store
    /// that every consumer reads through [`nmbrs_metrics::metrics_query::MetricsQuery`].
    ///
    /// Observers that don't need windowed views (e.g. StderrObserver)
    /// return `None` and the runner falls back to
    /// `Cadences::defaults()`.
    fn cadences(&self) -> Option<nmbrs_metrics::cadence::Cadences> {
        None
    }

    /// Callback invoked once the runner has built the shared
    /// [`nmbrs_metrics::metrics_query::MetricsQuery`]. Observers that
    /// render metrics (TUI, CLI status) capture this handle to read
    /// cadence windows, `now` values, and session-lifetime aggregates.
    fn on_metrics_query(&self, _query: std::sync::Arc<nmbrs_metrics::metrics_query::MetricsQuery>) {
    }

    /// Pre-populated scenario tree.
    ///
    /// Called once before execution begins with the full
    /// [`crate::scene_tree::SceneTree`] — synthetic root, every
    /// concrete phase, and every scope header (`for_each`,
    /// `for_combinations`, `do_while`, `do_until`) wired up by
    /// parent / children pointers.
    ///
    /// The TUI uses this to show all phases as Pending from the
    /// start; renderers that want hierarchical features (collapse,
    /// scope-level aggregate status) walk the tree directly. The
    /// callee may store the tree (e.g. behind an `RwLock`) and
    /// mutate node statuses in place via the lifecycle callbacks.
    fn scenario_pre_mapped(&self, _tree: &crate::scene_tree::SceneTree) {}
}

/// Live metrics snapshot for progress updates.
#[derive(Clone, Debug)]
pub struct PhaseProgressUpdate {
    /// The execution this update belongs to (SRD-88 `exec_id`,
    /// SRD-100 §4). With `name`+`labels` it forms the live routing
    /// key, so concurrent executions of the same phase route to
    /// distinct `ActivePhase` slots. `1` for the single-execution
    /// case (SRD-88 A1).
    pub exec_id: u64,
    /// Phase name this update belongs to — matches the `name`
    /// passed to [`RunObserver::phase_starting`]. Present so
    /// observers that track multiple concurrent phases can route
    /// the update to the correct per-phase slot.
    pub name: String,
    /// Phase dimensional labels (e.g. `profile=label_00, k=10`) —
    /// together with `name` this uniquely identifies one phase
    /// iteration.
    pub labels: String,
    pub cursor_name: String,
    pub cursor_extent: u64,
    /// SRD-82 Part 6 — whether this phase runs as a DAEMON (off the
    /// foreground budget, open-extent cursor, stopped by its group's
    /// foreground completing). Display consumers exclude daemon phases
    /// from AGGREGATE progress — the shared footer gutter bar averages
    /// the non-daemon phases — since a daemon's percent-of-budget is
    /// not meaningful progress toward the run.
    pub daemon: bool,
    /// Cursor ordinals CONSUMED so far for a data-driven phase
    /// (polydat `DataSourceFactory::global_consumed()`) — the
    /// authoritative row-level progress. `0` for non-cursor phases
    /// (plain `cycles:`), where the display keeps the op-denominated
    /// `cycles:` chip.
    pub rows_consumed: u64,
    /// Cursor ordinal EXTENT for a data-driven phase
    /// (`global_extent()`). `0` for non-cursor phases — distinct from
    /// `cursor_extent`, which falls back to the phase's `cycles:`
    /// bound for sourceless phases; this stays `0` so the display can
    /// tell a declared cursor from the synthesized `range(0, cycles)`
    /// every phase gets. `rows_total > 0` is the "show `rows:`" signal.
    pub rows_total: u64,
    pub fibers: usize,
    pub ops_started: u64,
    pub ops_finished: u64,
    pub ops_ok: u64,
    /// SKIPPED ops (`skips_total`) — excluded from the ok% denominator
    /// (a skip is neither a success nor a failure).
    pub skips: u64,
    pub errors: u64,
    pub retries: u64,
    pub ops_per_sec: f64,
    pub adapter_counters: Vec<(String, u64, f64)>,
    pub rows_per_batch: f64,
    /// Live relevancy aggregates — one entry per relevancy metric (e.g.
    /// `recall@10`). Each has a moving-window mean over the last N
    /// recall calculations and a whole-activity running mean.
    pub relevancy: Vec<crate::validation::RelevancyLive>,
}

/// SRD-100 P2 — the per-phase **live render handle**, attached to the
/// display fold's `ActivePhase` exactly once (after the `Activity` and its
/// metrics exist — executor.rs creates the activity well after
/// `phase_starting`, so this cannot ride that callback). It carries
/// everything a display surface needs to re-derive *this phase's* status
/// line **at the consumer** by folding the snapshot, replacing the retired
/// per-phase inline-status producer threads (SRD-100 §6).
///
/// Why a handle of live shared state rather than a pre-rendered string:
/// the consumer calls [`crate::readout_context::build_inline_refresh_context`]
/// verbatim against `metrics` and fires `bodies`, so single-run output is
/// **byte-identical** to the old producer path (SRD-100 §12 A1) by code
/// reuse — and §11 mandates that "each phase owns its `Arc<ActivityMetrics>`".
/// `BakedBody` is `Send + Sync` (its `Readout` handles are), so the format
/// template rides the ArcSwap snapshot as pure data; only the `!Sync`
/// *binder* is kept out of the snapshot (the consumer fires bodies with
/// `&self`).
#[derive(Clone)]
pub struct PhaseRenderHandle {
    /// Routing key — together with `name`+`labels`, selects the
    /// `ActivePhase` slot to attach to (mirrors [`PhaseProgressUpdate`]).
    pub exec_id: u64,
    pub name: String,
    pub labels: String,
    /// The activity's display name (`activity.config.name`, which may carry
    /// the leaf coord, e.g. `"phase (k=10)"`). Fed verbatim to
    /// `build_inline_refresh_context` so the consumer's render is
    /// byte-identical to the retired producer thread's.
    pub activity_name: String,
    /// Live atomic counters for this phase (SRD-100 §11). Read lock-free
    /// at render time so the status stays fresh without a producer thread.
    pub metrics: Arc<crate::activity::ActivityMetrics>,
    /// Resolved `on_update`-slot render template (workload binding + CLI
    /// override + built-in `phase_status` default). Immutable, fired with
    /// `&self` by the consumer; shared, never cloned per render.
    pub bodies: Arc<Vec<crate::readouts::BakedBody>>,
    /// Live memo header (the memo-wrapper `before:`/`after:` state),
    /// snapshotted into the context each render.
    pub memo: Arc<arc_swap::ArcSwap<String>>,
    /// Live gutter-cell spec (the gutter-wrapper state). `None` in
    /// the slot ⇒ the display derives the cell automatically.
    pub gutter: Arc<arc_swap::ArcSwapOption<crate::wrappers::gutter::GutterSpec>>,
    /// `status_metrics:` selection for the per-phase status chips.
    pub status_metrics: Arc<[String]>,
    /// Fiber count at attach time (the inline context's `concurrency`).
    pub concurrency: usize,
    /// `(seq, total)` pre-map coordinate, resolved from the dispatch-time
    /// `scene_node_id` (NOT a racy by-name DFS match — SRD-100 P1c).
    pub seq: Option<(usize, usize)>,
    /// Depth indent string (`" ".repeat(depth-1)`), resolved from the
    /// scene node at attach time.
    pub depth_indent: String,
}

impl std::fmt::Debug for PhaseRenderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately shallow: `ActivityMetrics` / `ArcSwap` are not
        // `Debug`, and a render handle in a command log only needs its
        // identity, not its live counters.
        f.debug_struct("PhaseRenderHandle")
            .field("exec_id", &self.exec_id)
            .field("name", &self.name)
            .field("labels", &self.labels)
            .field("seq", &self.seq)
            .field("bodies", &self.bodies.len())
            .finish_non_exhaustive()
    }
}

/// SRD-? — how a FULLY-SKIPPED phase (every cycle `if:`-gated off, or
/// pre-entry pruned) is represented across the plan, traversal, and
/// readout surfaces. Selected once per run via the `skipped_phases`
/// param; read by the runtime's completion fire, the executor's
/// pre-entry gate, and the TUI's tree fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SkippedPhaseDisplay {
    /// Traversed as normal, but elided from the readout and the
    /// completed-phase tree — a gated-off phase leaves no trace.
    Elide,
    /// Traversed as normal and kept in the plan/readout, explicitly
    /// rendered as skipped (`⊘ [name] gated off`). The default:
    /// visible but unmistakable.
    #[default]
    Mark,
    /// Not even traversed: when every op declares `if:` and all
    /// gates evaluate false at phase entry, the phase is skipped
    /// before dispatch — elided from plan, traversal, and readout.
    /// (Post-hoc fully-skipped phases — gates that opened false
    /// mid-run — are elided as in `Elide`.)
    Prune,
}

impl SkippedPhaseDisplay {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "elide" => Some(Self::Elide),
            "mark" => Some(Self::Mark),
            "prune" => Some(Self::Prune),
            _ => None,
        }
    }
}

static SKIPPED_PHASE_DISPLAY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// Set the run's skipped-phase display mode (runner, at param parse).
pub fn set_skipped_phase_display(mode: SkippedPhaseDisplay) {
    let v = match mode {
        SkippedPhaseDisplay::Elide => 0,
        SkippedPhaseDisplay::Mark => 1,
        SkippedPhaseDisplay::Prune => 2,
    };
    SKIPPED_PHASE_DISPLAY.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// The run's skipped-phase display mode. Default [`SkippedPhaseDisplay::Mark`].
pub fn skipped_phase_display() -> SkippedPhaseDisplay {
    match SKIPPED_PHASE_DISPLAY.load(std::sync::atomic::Ordering::Relaxed) {
        0 => SkippedPhaseDisplay::Elide,
        2 => SkippedPhaseDisplay::Prune,
        _ => SkippedPhaseDisplay::Mark,
    }
}

/// SRD-92 R5 — how much of a completed node's block is retained in
/// scrollback. Completion is never a full collapse: the header line
/// always lands; this selects whether the detail rows and op leaves
/// land with it. Selected once per run via the `completed_phases=`
/// param; session.log keeps every line unconditionally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CompletedPhaseDisplay {
    /// Header + detail rows (counters, key metrics, memo) + op
    /// leaves — the full block, in contract shape. The default.
    #[default]
    Full,
    /// Header row only.
    Headers,
}

impl CompletedPhaseDisplay {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(Self::Full),
            "headers" => Some(Self::Headers),
            _ => None,
        }
    }
}

static COMPLETED_PHASE_DISPLAY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Set the run's completed-phase display mode (runner, at param parse).
pub fn set_completed_phase_display(mode: CompletedPhaseDisplay) {
    let v = match mode {
        CompletedPhaseDisplay::Full => 0,
        CompletedPhaseDisplay::Headers => 1,
    };
    COMPLETED_PHASE_DISPLAY.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// The run's completed-phase display mode. Default [`CompletedPhaseDisplay::Full`].
pub fn completed_phase_display() -> CompletedPhaseDisplay {
    match COMPLETED_PHASE_DISPLAY.load(std::sync::atomic::Ordering::Relaxed) {
        1 => CompletedPhaseDisplay::Headers,
        _ => CompletedPhaseDisplay::Full,
    }
}

/// Global observer for code that can't thread the observer through.
/// Set once at run start, remains for the process lifetime.
static GLOBAL_OBSERVER: std::sync::OnceLock<Arc<dyn RunObserver>> = std::sync::OnceLock::new();

/// Set the global observer. Called once by the runner at startup.
pub fn set_global_observer(observer: Arc<dyn RunObserver>) {
    let _ = GLOBAL_OBSERVER.set(observer);
}

/// The observer the current code should route through. Used by code that
/// needs the observer without threading it through every call site (e.g. the
/// activity's inline-status refresh thread publishing into
/// [`RunObserver::set_status_line`]).
///
/// SRD-88: a fiber running inside an [`ExecutionContext`](crate::execution_context)
/// resolves to ITS execution's observer (so concurrent executions route
/// lifecycle/log independently); outside any execution scope — or when the
/// scoped context set no observer — it falls back to the process-global
/// `GLOBAL_OBSERVER` (the single-run / CLI / test default; axiom A1).
pub fn global_observer() -> Option<Arc<dyn RunObserver>> {
    if let Some(obs) = crate::execution_context::current_observer() {
        return Some(obs);
    }
    GLOBAL_OBSERVER.get().cloned()
}

/// Direct the log sink to a file. Opens for append-writes,
/// installs the [`crate::log_sink`] async writer thread.
/// Producers thereafter `try_send` and never block — see SRD-02
/// §"Display and Diagnostic Decoupling". Silently no-ops on a
/// second call — the first session wins (one run per process).
pub fn set_log_file(path: &std::path::Path) -> std::io::Result<()> {
    crate::log_sink::init(path)
}

/// Log a diagnostic message through the global observer and
/// append to the async log sink (if initialized). Safe to call
/// from anywhere — falls back to stderr if no observer is set.
///
/// The file write is non-blocking: the line is enqueued onto a
/// bounded channel consumed by the dedicated `log-sink` thread.
/// On overflow the line is dropped and the sink's `dropped_count`
/// is bumped — never blocks the caller, even on a stalled disk.
/// Whether ANSI color escapes are appropriate for stderr.
/// `true` only when stderr is a TTY and the operator hasn't
/// disabled color via the conventional `NO_COLOR` env var
/// (https://no-color.org). Pipelined / CI contexts return
/// `false` so log archives stay readable. Cached on first
/// call; the answer doesn't change over a process's lifetime.
pub fn use_color() -> bool {
    use std::io::IsTerminal;
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        std::io::stderr().is_terminal()
    })
}

/// SRD — explainer-overlay toggle state. Process-global so the
/// TUI keystroke layer (which holds the watcher) and the
/// readout binder (which holds the render thread) can rendezvous
/// without threading a channel through every readout call.
///
/// Value semantics: wall-clock nanos at which the overlay
/// auto-reverts. `0` means "off"; any value > `now_nanos()`
/// means "render `ContentMode::Explanation` until the deadline."
///
/// Toggle model (not hold): a single `?` press flips the
/// overlay on with a 10 s auto-revert deadline. A second press
/// while on flips it back off immediately. Auto-revert ensures
/// the operator can't leave the overlay stuck on after walking
/// away.
static EXPLAIN_HELD_UNTIL_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Wall-clock nanos of the most recent `?` press. Drives the
/// auto-repeat debounce — terminals in raw mode send a stream
/// of keystrokes while `?` is held, and without this the
/// second auto-repeat would flip the overlay off again
/// 30 ms after the operator's first press.
static EXPLAIN_LAST_PRESS_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long the overlay stays on after a `?` toggle. Picked
/// to be long enough that the operator can read the explainer
/// surface without timing out mid-read, short enough that a
/// forgotten toggle reverts on its own. 10 s lands in the
/// middle of "long enough to be useful" and "short enough that
/// the operator notices the auto-revert."
const EXPLAIN_AUTO_REVERT_MS: u64 = 10_000;

/// Auto-repeat debounce window. 250 ms swallows the 30 Hz
/// auto-repeat stream while still allowing a deliberate
/// second tap to take effect.
const EXPLAIN_TOGGLE_DEBOUNCE_MS: u64 = 250;

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Toggle the explainer overlay. First press → on (with a 10 s
/// auto-revert deadline). Second press while on → off
/// immediately. Auto-repeat-safe via a 250 ms debounce.
pub fn toggle_explain() {
    let now = now_nanos();
    let last_press = EXPLAIN_LAST_PRESS_NS.load(std::sync::atomic::Ordering::Acquire);
    if last_press != 0 && now.saturating_sub(last_press) < EXPLAIN_TOGGLE_DEBOUNCE_MS * 1_000_000 {
        return;
    }
    EXPLAIN_LAST_PRESS_NS.store(now, std::sync::atomic::Ordering::Release);
    let currently_on = {
        let deadline = EXPLAIN_HELD_UNTIL_NS.load(std::sync::atomic::Ordering::Acquire);
        deadline != 0 && now < deadline
    };
    if currently_on {
        EXPLAIN_HELD_UNTIL_NS.store(0, std::sync::atomic::Ordering::Release);
    } else {
        let deadline = now.saturating_add(EXPLAIN_AUTO_REVERT_MS * 1_000_000);
        EXPLAIN_HELD_UNTIL_NS.store(deadline, std::sync::atomic::Ordering::Release);
    }
}

/// True iff the explainer overlay is currently on (toggled on
/// within the last `EXPLAIN_AUTO_REVERT_MS` and not yet
/// toggled off). Read by the readout binder on each `fire()`
/// to decide whether to dispatch with
/// `ContentMode::Explanation` instead of `Value`.
pub fn is_explain_held() -> bool {
    let deadline = EXPLAIN_HELD_UNTIL_NS.load(std::sync::atomic::Ordering::Acquire);
    deadline != 0 && now_nanos() < deadline
}

/// Readout pause — the `p` key's copy-support freeze. While set,
/// readout frontends stop writing to the terminal (the line-mode
/// sink freezes its log drain and status redraw; buffered output
/// flushes on resume) so the operator can select and copy text
/// without the surface repainting underneath the selection. The
/// full-TUI app keeps its own snapshot-based pause; this global
/// serves the line-mode frontends, which have no App state.
static READOUT_PAUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Toggle the readout pause; returns the NEW state (`true` =
/// now paused).
pub fn toggle_readout_pause() -> bool {
    !READOUT_PAUSED.fetch_xor(true, std::sync::atomic::Ordering::AcqRel)
}

/// True while the readout pause is on (see
/// [`toggle_readout_pause`]).
pub fn readouts_paused() -> bool {
    READOUT_PAUSED.load(std::sync::atomic::Ordering::Acquire)
}

/// Minimum severity that reaches the file sink
/// (`session.log`). Default `Debug` — the file gets every
/// non-trivial entry. The display threshold (per-observer
/// `min_level`) is the SEPARATE knob that controls what
/// reaches stderr; it defaults to `Info`. The two are
/// configured independently via:
///
/// - `loglevel-retain=` / `--log-retain-level` /
///   `NMBRS_LOG_RETAIN_LEVEL` — what the file gets.
/// - `loglevel=` / `--log-display-level` /
///   `NMBRS_LOG_DISPLAY_LEVEL` — what stderr gets.
///
/// The runner installs the user-supplied retain level via
/// [`set_retain_level`] at startup; readers consult
/// [`retain_level`] before sending to the sink.
static RETAIN_LEVEL: std::sync::OnceLock<LogLevel> = std::sync::OnceLock::new();

/// Install the file-sink retention threshold. Called once
/// by the runner; subsequent calls are silent no-ops
/// (matching the "first wins" pattern of the rest of the
/// global observer surface).
pub fn set_retain_level(level: LogLevel) {
    let _ = RETAIN_LEVEL.set(level);
}

/// Effective retention threshold. Defaults to `Debug` so
/// pre-runner-init log calls (very early startup) still
/// reach the file sink.
pub fn retain_level() -> LogLevel {
    *RETAIN_LEVEL.get().unwrap_or(&LogLevel::Debug)
}

/// Companion to [`RETAIN_LEVEL`]: the *display* threshold
/// that gates console emission. Each observer carries its
/// own `min_level` for the live log path; this global
/// captures the effective value so secondary surfaces
/// (the failure-dump path in `nmbrs-tui::observer`) can
/// honour the same threshold without plumbing the observer
/// reference everywhere.
static DISPLAY_LEVEL: std::sync::OnceLock<LogLevel> = std::sync::OnceLock::new();

/// Install the console display threshold. Called by the
/// runner alongside [`set_retain_level`] at startup.
pub fn set_display_level(level: LogLevel) {
    let _ = DISPLAY_LEVEL.set(level);
}

/// Effective console display threshold. Defaults to
/// `Info` — same default the live observers use.
pub fn display_level() -> LogLevel {
    *DISPLAY_LEVEL.get().unwrap_or(&LogLevel::Info)
}

pub fn log(level: LogLevel, message: &str) {
    log_tagged(level, EventTag::default(), message);
}

/// [`log`] with an explicit [`EventTag`]. The session-log write
/// (unconditional, all tags) and the fallback stderr path are
/// identical to [`log`]; the tag only changes how a tag-aware
/// display sink files the message — e.g. the readout engine
/// attaches phase-start renders to `PhaseStart` so the terminal
/// sink keeps them out of its scrollback (they show in its managed
/// phase-history region instead).
pub fn log_tagged(level: LogLevel, tag: EventTag, message: &str) {
    if level >= retain_level()
        && let Some(sink) = crate::log_sink::global()
    {
        let tag = match level {
            LogLevel::Trace => "TRC",
            LogLevel::Debug => "DBG",
            LogLevel::Info => "INF",
            LogLevel::Warn => "WRN",
            LogLevel::Error => "ERR",
        };
        // Human-readable wall-clock timestamp from the session
        // formatter — matches the session id's date/time style
        // so log lines correlate visually with the session
        // directory.
        let ts = crate::session::now_log_timestamp();
        // The durable session.log is plain text — strip any ANSI
        // a colored readout render carried in `message` (notably
        // the phase `✓` outcome, SRD-81 push 1b). The live ring
        // keeps the colored version for the terminal scrollback,
        // and the replay capture is untouched; only this file
        // projection is stripped.
        let line = format!(
            "{ts} {tag} {}\n",
            crate::readouts::snapshot::strip_ansi(message)
        )
        .into_bytes();
        let _ = sink.try_send(line);
    }
    // SRD-88: route through the current execution's observer (task-local) or
    // the process-global default; `global_observer()` resolves both.
    if let Some(obs) = global_observer() {
        obs.log_tagged(level, tag, message);
    } else {
        // No observer yet (bootstrap): project straight to the log bucket —
        // the channel owns the fd (SRD-87 §5), falling back to stderr when no
        // channel is installed either.
        crate::output_channel::log_to_surface(level, message);
    }
}

/// Emit a line of adapter **op output** (SRD-41 / "console belongs to the
/// adapter"). Delegates to the installed [`crate::output_channel::OutputChannel`]
/// (SRD-87): the **op-output bucket**'s impl decides where the bytes land —
/// raw to the owned stdout (a console-owning adapter or a pipe, via
/// [`op_output_raw`]) or routed through the live display (an interactive
/// dashboard, avoiding the raw-mode staircase). Before any channel is installed
/// (bootstrap / unit tests with no run), falls back to the raw path so early
/// output is never lost.
pub fn op_output(line: &str) {
    if let Some(channel) = crate::output_channel::installed() {
        channel.op_output(line);
        return;
    }
    op_output_raw(line);
}

/// Write an op-output line RAW to the stdout the producer owns (so
/// `nmbrs run | grep`, `> file`, AND a console-owning adapter's interactive
/// screen all show it) and capture it to `session.log` at INFO (the same
/// durable projection [`log_tagged`] writes). The SRD-87
/// [`crate::output_channel::RawStdoutChannel`] op-output bucket and the
/// no-channel bootstrap fallback both route through here.
pub(crate) fn op_output_raw(line: &str) {
    if LogLevel::Info >= retain_level()
        && let Some(sink) = crate::log_sink::global()
    {
        let ts = crate::session::now_log_timestamp();
        let bytes =
            format!("{ts} INF {}\n", crate::readouts::snapshot::strip_ansi(line)).into_bytes();
        let _ = sink.try_send(bytes);
    }
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// ANSI-colorize a log line by severity for console
/// output. Always applied at the producer side — every
/// console emission of a log entry runs through this so
/// `DBG`/`INF`/`WRN`/`ERR` are visually distinct without
/// the operator having to squint at message bodies.
/// Falls through to the bare message when stderr isn't a
/// TTY or `NO_COLOR` is set (per [`use_color`]); pipeline
/// captures stay readable.
pub fn colorize_log_line(level: LogLevel, message: &str) -> String {
    if !use_color() {
        return message.to_string();
    }
    let (color, reset) = match level {
        // Faintest grey for trace — even more de-emphasized
        // than debug; rarely reaches console anyway.
        LogLevel::Trace => ("\x1b[2;90m", "\x1b[0m"),
        // Dim grey for debug — present but de-emphasized.
        LogLevel::Debug => ("\x1b[2m", "\x1b[0m"),
        // Default-color for info — the baseline; no
        // override so user-themed terminals show their
        // preferred default.
        LogLevel::Info => ("", ""),
        // Yellow for warn.
        LogLevel::Warn => ("\x1b[33m", "\x1b[0m"),
        // Bold red for error.
        LogLevel::Error => ("\x1b[1;31m", "\x1b[0m"),
    };
    if color.is_empty() {
        message.to_string()
    } else {
        format!("{color}{message}{reset}")
    }
}

/// Convenience macros for logging through the global observer.
#[macro_export]
macro_rules! diag {
    ($level:expr, $($arg:tt)*) => {
        $crate::observer::log($level, &format!($($arg)*))
    };
}

/// Emit a [`LogLevel::Trace`] event carrying the component's
/// labels through the [`crate::trace_router`]. Returns
/// immediately when no `--trace=<spec>` was configured (the
/// router is empty), so the hot-path cost of an unused trace
/// site is one atomic load + branch.
///
/// Trace events DO NOT flow to `session.log` — they are routed
/// to dedicated trace files configured by `--trace=` so the
/// main session log stays usable as a Debug-and-up record.
pub fn trace(labels: &nmbrs_metrics::labels::Labels, message: &str) {
    crate::trace_router::log(labels, message);
}

/// True iff the trace router has at least one configured sink.
/// Hot-path guard so callers can skip expensive message
/// formatting when tracing is off.
pub fn trace_enabled() -> bool {
    crate::trace_router::enabled()
}

/// Format-and-emit a trace event. Same shape as [`diag!`] but
/// takes a labels handle first and only fires when the trace
/// router is active.
#[macro_export]
macro_rules! trace_event {
    ($labels:expr, $($arg:tt)*) => {
        if $crate::observer::trace_enabled() {
            $crate::observer::trace($labels, &format!($($arg)*));
        }
    };
}

use std::sync::Arc;

/// Default observer: prints to stderr.
///
/// `min_level` controls the minimum severity that reaches
/// stderr — Info by default, matching the TUI log panel's
/// default filter (so high-cadence Debug instrumentation
/// doesn't drown the signal in either mode). Override via
/// `loglevel=debug|info|warn|error` on the workload command
/// line. The async log sink (session.log) still receives
/// every level regardless of this filter.
pub struct StderrObserver {
    pub min_level: LogLevel,
}

impl Default for StderrObserver {
    fn default() -> Self {
        Self {
            min_level: LogLevel::Info,
        }
    }
}

impl StderrObserver {
    /// Build a stderr observer with the given min severity.
    pub fn with_min_level(min_level: LogLevel) -> Self {
        Self { min_level }
    }
}

impl RunObserver for StderrObserver {
    fn phase_starting(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        name: &str,
        _labels: &str,
        op_templates: usize,
        total_cycles: u64,
        concurrency: usize,
    ) {
        // Route through the canonical event channel so the line
        // lands in `session.log` AND on stderr (via the recursive
        // call back into `StderrObserver::log` below).
        let template_word = if op_templates == 1 {
            "op template"
        } else {
            "op templates"
        };
        let cycle_word = if total_cycles == 1 { "cycle" } else { "cycles" };
        crate::observer::log(
            LogLevel::Info,
            &format!(
                "phase '{name}': {op_templates} {template_word}, {total_cycles} {cycle_word}, concurrency={concurrency}"
            ),
        );
    }

    fn phase_completed(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        _duration_secs: f64,
    ) {
        // No-op — the executor's own diag emits a fully-formatted
        // "phase 'X' complete (Ns)" line via the log path. Doing
        // it here too produced a duplicate (and a less
        // informative one — no duration). The structured
        // callback stays for non-stderr consumers.
    }

    fn phase_failed(
        &self,
        _scene_node_id: crate::scene_tree::SceneNodeId,
        _name: &str,
        _labels: &str,
        _error: &str,
    ) {
        // Same reasoning as phase_completed — the executor diags
        // already emit "phase 'X' stopped by error handler (Ns)"
        // (or other failure messages) right before calling this.
        // Re-emitting here was a duplicate.
    }

    fn phase_progress(&self, _update: &PhaseProgressUpdate) {
        // The inline status line in activity.rs handles this
    }

    fn run_finished(&self) {
        // Same routing as `phase_starting` — through `observer::log`
        // so session.log captures the run-end marker.
        crate::observer::log(LogLevel::Info, "all phases complete");
    }

    fn log(&self, level: LogLevel, message: &str) {
        // Severity filter: only entries `>= min_level` reach
        // stderr. The session log file still gets every level
        // via the async log sink — this filter only affects
        // what the operator sees on screen.
        if level >= self.min_level {
            // Cosmetic: when the runtime announces a Ctrl-C-
            // initiated graceful shutdown, the terminal has just
            // echoed `^C` on the current line. A leading blank
            // line makes the announcement visually clear that
            // marker without leaving a stray newline in the
            // structured session.log (which never sees this
            // path). Same idea for force-exit on second Ctrl-C.
            if message.starts_with("session: graceful shutdown requested")
                || message.starts_with("session: force-exit")
            {
                eprintln!();
            }
            // SRD-87 §5: the live-surface write goes through the log bucket
            // (the channel owns the fd); the `min_level` gate above stays here.
            crate::output_channel::log_to_surface(level, message);
        }
    }
}

#[cfg(test)]
mod display_mode_tests {
    use super::*;

    #[test]
    fn completed_phase_display_parses_and_rejects() {
        assert_eq!(
            CompletedPhaseDisplay::parse("full"),
            Some(CompletedPhaseDisplay::Full)
        );
        assert_eq!(
            CompletedPhaseDisplay::parse(" HEADERS "),
            Some(CompletedPhaseDisplay::Headers)
        );
        assert_eq!(CompletedPhaseDisplay::parse("collapse"), None);
        // Default is Full — completion is never a full collapse
        // (SRD-92 R5).
        assert_eq!(
            CompletedPhaseDisplay::default(),
            CompletedPhaseDisplay::Full
        );
    }
}

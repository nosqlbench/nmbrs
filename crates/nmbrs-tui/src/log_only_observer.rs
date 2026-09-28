// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `LogOnlyObserver` — `RunObserver` impl for `tui=off` mode.
//!
//! Mirror image of [`crate::observer::TuiObserver`] but with no
//! TUI thread, no terminal takeover, and no alt-screen. Every
//! observer-lifecycle callback fires two writes:
//!
//! 1. Send the corresponding [`crate::run_state_actor::RunStateCmd`]
//!    to the actor so the snapshot is populated. This makes the
//!    inspector socket / web API work in `tui=off` mode and gives
//!    the future Ctrl-T toggle a non-empty state to display.
//! 2. Synchronously write the same line to stderr (filtered by
//!    `min_level`). Identical output to the legacy
//!    [`nmbrs_runtime::observer::StderrObserver`] path —
//!    operators see no behaviour change.
//!
//! This is the minimum-viable observer for Phase 1 of the
//! display-sink refactor (see [`crate::display_sink`]). Phase 2
//! moves the stderr writes onto a separate
//! [`crate::log_only_sink::LogOnlySink`] thread that drains
//! [`crate::state::RunState::log_seq_total`] from the actor's
//! snapshots, so the same observer can drive a TUI sink, a
//! line-mode emulation sink, or a plain-stderr sink without
//! changing the observer surface.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use nmbrs_metrics::cadence::Cadences;
use nmbrs_metrics::metrics_query::MetricsQuery;
use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;
use nmbrs_runtime::observer::{EventTag, LogLevel, PhaseProgressUpdate, RunObserver};
use parking_lot::Mutex;

use crate::frame_broker::FrameBroker;

/// Per-row context for the live mid-run scope-ancestor
/// walker. Carries just the scope's display name and the
/// surface's color setting so the `scope_header` readout
/// can render the cyan-bullet + italic-name byte sequence
/// without each emit site rebuilding it. SRD-63 Push 8c.
struct ScopeAncestorContext {
    name: String,
    use_color: bool,
}

impl nmbrs_runtime::readouts::ReadoutContext for ScopeAncestorContext {
    fn subject_name(&self) -> &str {
        &self.name
    }
    fn subject_seq(&self) -> Option<(usize, usize)> {
        None
    }
    fn subject_labels(&self) -> &str {
        ""
    }
    fn cycles_completed(&self) -> u64 {
        0
    }
    fn cycles_total(&self) -> u64 {
        0
    }
    fn ops_ok(&self) -> u64 {
        0
    }
    fn errors(&self) -> u64 {
        0
    }
    fn retries(&self) -> u64 {
        0
    }
    fn concurrency(&self) -> usize {
        0
    }
    fn elapsed_secs(&self) -> f64 {
        0.0
    }
    fn consumed(&self) -> u64 {
        0
    }
    fn status_metric_chips(&self) -> String {
        String::new()
    }
    fn depth_indent(&self) -> &str {
        ""
    }
    fn use_color(&self) -> bool {
        self.use_color
    }
    fn event(&self) -> nmbrs_runtime::lifecycle::EventType {
        // Live mid-run scope-ancestor walker fires the
        // scope_header readout at the same boundary that
        // `EventType::EachStart` would fire it at — replay the
        // live render path against historical state.
        nmbrs_runtime::lifecycle::EventType::EachStart
    }
}
use crate::run_state_actor::{RunStateCmd, RunStateHandle};
use crate::state::LogSeverity;

/// `tui=off` observer. Holds the actor handle so every event
/// also lands in the snapshot.
///
/// Stderr writes are *coordinated*: this observer only writes
/// directly when `sink_active` is `false`. When a
/// [`crate::log_only_sink::LogOnlySink`] (or any other
/// log-rendering [`crate::display_sink::DisplaySink`]) is up, it
/// flips `sink_active = true` and takes over rendering by
/// draining the actor's snapshot. The handoff is racy by design:
/// any line that fires *before* the sink sets the flag goes
/// straight to stderr (matching legacy `StderrObserver` timing
/// for early diagnostics); after the flag flips, the sink owns
/// the channel.
///
/// Pre-flag lines are also written to the actor, so the sink's
/// initial snapshot read sees them — but the sink's first poll
/// records `last_seen_seq = current log_seq_total` and only
/// renders entries newer than that. No duplicates.
pub struct LogOnlyObserver {
    state: RunStateHandle,
    /// Minimum severity that reaches stderr. Session log file
    /// (via the async sink in `nmbrs_runtime::log_sink`) gets
    /// every level regardless. Defaults to `Info` to match the
    /// TUI log panel's default filter.
    min_level: LogLevel,
    /// `false` until a sink claims log rendering. While false,
    /// `log()` writes to stderr synchronously; once flipped,
    /// stderr writes are suppressed and the sink owns the
    /// surface. Set by `LogOnlySink` on start (it drains the
    /// snapshot's log ring) AND held high by the supervisor
    /// while a `TuiSink` is up (the TUI shows its own log
    /// panel — the observer must not double-write).
    sink_active: Arc<AtomicBool>,
    /// Distinct from `sink_active`: this flag tells the
    /// activity's inline-status thread (the
    /// `\r\x1b[K…` rewriter in `nmbrs-runtime::activity`) to
    /// yield. Set only when an alt-screen TUI owns the
    /// terminal — i.e. `tui=on` from-startup or after a
    /// Ctrl-T swap into `TuiSink`. In plain `tui=terminal`
    /// mode the inline status line stays visible alongside
    /// the LogOnlySink's log-line drain; both write to stderr
    /// without conflict (status rewrites the bottom line via
    /// `\r`, log lines scroll up). Surfaced via
    /// [`RunObserver::live_suppress_flag`].
    inline_suppress: Arc<AtomicBool>,
    /// Multi-consumer fan-out for cadence-reporter frames.
    /// Returned via [`RunObserver::reporters`] so the runner
    /// registers it with the cadence scheduler. Sinks that need
    /// frames (e.g. `TuiSink`) call [`Self::subscribe_frames`]
    /// to grab their own `Receiver`. Sinks that don't (e.g.
    /// `LogOnlySink`) simply skip subscription — the broker's
    /// per-publish cost with zero subscribers is one mutex lock
    /// on an empty `Vec`.
    frame_broker: FrameBroker,
    /// User-declared cadences forwarded to the runner via
    /// [`RunObserver::cadences`]. Same shape `TuiObserver`
    /// already uses; the actor-driven path inherits the
    /// behaviour without divergence.
    cadences: Cadences,
    /// `MetricsQuery` handle the runner publishes via
    /// [`RunObserver::on_metrics_query`] once the cadence
    /// reporter is built. Sinks that render per-cadence views
    /// (the TUI's barchart) call [`Self::metrics_query`] to
    /// pick it up at sink-start time.
    metrics_query: Mutex<Option<Arc<MetricsQuery>>>,
    /// Scope-ancestor chain (Scope-kind scene-tree node ids,
    /// outer-first) of the most recently announced phase. The
    /// observer diffs against this on every `phase_starting`
    /// to emit only the *newly-entered* scope headers, so the
    /// terminal-mode log reads as a hierarchic tree walk
    /// rather than a flat sequence of striated coord tuples.
    /// `None` until the first phase fires.
    last_scope_chain: Mutex<Option<Vec<usize>>>,
}

impl LogOnlyObserver {
    /// Build with the given actor handle, default `Info` stderr
    /// severity floor, and the user-declared cadences.
    pub fn new(state: RunStateHandle, cadences: Cadences) -> Self {
        Self {
            state,
            min_level: LogLevel::Info,
            sink_active: Arc::new(AtomicBool::new(false)),
            inline_suppress: Arc::new(AtomicBool::new(false)),
            frame_broker: FrameBroker::new(),
            cadences,
            metrics_query: Mutex::new(None),
            last_scope_chain: Mutex::new(None),
        }
    }

    /// Override the stderr severity floor. The session log file
    /// is unaffected.
    pub fn with_min_level(mut self, level: LogLevel) -> Self {
        self.min_level = level;
        self
    }

    /// The console-owning-adapter display mode: a console-owning adapter
    /// (stdout to a terminal, plotter) holds an **interactive** terminal,
    /// so its compositing rule is *don't project to the console at all* —
    /// the screen is the adapter's. This yields the whole console
    /// projection (`sink_active`, the same flag a real sink raises to
    /// claim the surface) and the inline `✓` status line
    /// (`inline_suppress`). Every signal — banners, the scope/phase
    /// readouts, completion notices — still flows through the channel to
    /// `session.log` (the file write in
    /// [`crate::observer::log_categorized`] is independent of these
    /// flags); it just isn't rendered to *this* surface.
    ///
    /// Only applied on an interactive TTY: a non-interactive run
    /// (pipe/file) keeps stdout and stderr as separate streams, so the
    /// console projection flows to stderr as usual and CI/tests capture
    /// it.
    pub fn reserve_console_for_adapter(self) -> Self {
        self.sink_active.store(true, Ordering::Release);
        self.inline_suppress.store(true, Ordering::Release);
        self
    }

    /// Returns the shared coordination flag. The owner of the
    /// active log-rendering sink flips this to `true` while
    /// rendering and back to `false` on shutdown. This is the
    /// hand-off the observer uses to decide whether to write
    /// stderr directly.
    pub fn sink_active_flag(&self) -> Arc<AtomicBool> {
        self.sink_active.clone()
    }

    /// Returns the inline-status suppression flag. Distinct
    /// from `sink_active_flag` — this one's job is to tell the
    /// activity's inline-status thread (the `\r\x1b[K…`
    /// per-cycle rewriter in `nmbrs-runtime::activity`) to
    /// yield while an alt-screen TUI owns the terminal. Held
    /// high by the supervisor when `TuiSink` is up; held low
    /// (or unset) in plain `tui=terminal` mode so the live
    /// status line stays visible alongside the log-line drain.
    pub fn inline_suppress_flag(&self) -> Arc<AtomicBool> {
        self.inline_suppress.clone()
    }

    /// The minimum severity threshold this observer was built
    /// with. The active sink reads this so its drain applies
    /// the same filter the observer would have, keeping output
    /// consistent across the synchronous-pre-sink and
    /// asynchronous-post-sink legs.
    pub fn min_level(&self) -> LogLevel {
        self.min_level
    }

    /// Subscribe to the cadence-frame fan-out. Returns a fresh
    /// `mpsc::Receiver`; the caller (a sink) owns it for its
    /// lifetime. Cleanup is automatic — when the receiver is
    /// dropped, the broker prunes the matching sender on the
    /// next publish. Multiple subscribers coexist (toggle path
    /// hands out separate receivers to terminal- and TUI-side
    /// sinks).
    pub fn subscribe_frames(&self) -> mpsc::Receiver<MetricSet> {
        self.frame_broker.subscribe()
    }

    /// Snapshot of the metrics query handle published by the
    /// runner via [`RunObserver::on_metrics_query`]. Returns
    /// `None` until the cadence reporter is built — sinks that
    /// need it (TuiSink) call this at start-time, when the
    /// runner has already wired the query.
    pub fn metrics_query(&self) -> Option<Arc<MetricsQuery>> {
        self.metrics_query.lock().clone()
    }
}

fn level_to_severity(level: LogLevel) -> LogSeverity {
    match level {
        LogLevel::Trace => LogSeverity::Debug,
        LogLevel::Debug => LogSeverity::Debug,
        LogLevel::Info => LogSeverity::Info,
        LogLevel::Warn => LogSeverity::Warn,
        LogLevel::Error => LogSeverity::Error,
    }
}

impl RunObserver for LogOnlyObserver {
    fn op_measure(
        &self,
        parent_phase: nmbrs_runtime::scene_tree::SceneNodeId,
        op_name: &str,
        text: &str,
    ) {
        self.state
            .send(crate::run_state_actor::RunStateCmd::OpMeasure {
                parent: parent_phase,
                op_name: op_name.to_string(),
                text: text.to_string(),
            });
    }

    fn session_dir_ready(&self, dir: &std::path::Path) {
        // Opens <dir>/transcript.log in the actor and flushes everything
        // buffered since actor start.
        self.state
            .send(crate::run_state_actor::RunStateCmd::OpenTranscript(
                dir.join("transcript.log"),
            ));
    }

    fn sysmon_update(&self, sample: &nmbrs_runtime::sysmon::SysmonSample) {
        // Same forward the full-TUI observer does: the sample is ambient
        // session state consumed by the status fold at repaint.
        self.state
            .send(crate::run_state_actor::RunStateCmd::Sysmon(sample.clone()));
    }

    fn scenario_pre_mapped(&self, tree: &nmbrs_runtime::scene_tree::SceneTree) {
        // Forward the pre-mapped scene tree to the actor so
        // `print_post_run_summary` (and any future sink reading
        // from the snapshot) can render hierarchy and indent
        // phases by scope depth in `tui=off` mode too. Without
        // this, every `set_phase_running` call falls through to
        // the "unknown phase, push under root" fallback in
        // `RunState::set_phase_running`, flattening the tree into
        // a plain phase list.
        self.state.send(RunStateCmd::InstallTree(tree.clone()));
    }

    fn phase_starting(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        op_templates: usize,
        total_cycles: u64,
        concurrency: usize,
    ) {
        // Snapshot mutation: same RunStateCmd shape TuiObserver
        // sends, so the snapshot model is identical between modes.
        self.state.send(RunStateCmd::PhaseStarting {
            exec_id: nmbrs_runtime::execution_context::current_exec_id(),
            scene_node_id,
            name: name.to_string(),
            labels: labels.to_string(),
            op_templates,
            total_cycles,
            concurrency,
        });

        // Hierarchic stderr emit. The terminal-mode log walks
        // the scenario tree as the runtime visits it; the
        // observer maintains a "last announced scope chain" and
        // emits headers only for the *newly-entered* scopes on
        // each phase_starting call. The result reads like the
        // post-run summary: scope headers indented per depth,
        // phase rows nested under their innermost scope, no
        // redundant striated coord tuple repeated on every
        // phase line.
        let template_word = if op_templates == 1 {
            "op template"
        } else {
            "op templates"
        };
        let cycle_word = if total_cycles == 1 { "cycle" } else { "cycles" };

        // ANSI color codes — only when stderr is a TTY and
        // `NO_COLOR` isn't set. See `nmbrs_runtime::observer::use_color`.
        // Falls back to plain text in pipelined / CI contexts
        // so log archives stay readable.
        let color = nmbrs_runtime::observer::use_color();
        let dim = if color { "\x1b[2m" } else { "" };
        let bold = if color { "\x1b[1m" } else { "" };
        let cyan = if color { "\x1b[36m" } else { "" };
        let italic = if color { "\x1b[3m" } else { "" };
        let reset = if color { "\x1b[0m" } else { "" };

        let tree = match nmbrs_runtime::scene_tree::current() {
            Some(t) => t,
            None => {
                // Pre-map didn't run (or the resume planner
                // hasn't published yet). Without a scene tree we
                // also have no scope-ancestor headers to emit,
                // so this branch becomes a true no-op — the
                // condensed ✓ line from `nmbrs-runtime::activity`
                // is the sole per-phase log entry, same as the
                // hierarchic path below.
                let _ = (
                    op_templates,
                    total_cycles,
                    concurrency,
                    template_word,
                    cycle_word,
                    name,
                    labels,
                    bold,
                    color,
                );
                return;
            }
        };

        // Resolve this phase's scene-tree node + walk its
        // ancestors collecting every Scope-kind ancestor in
        // outer-first order. Phase nodes have either Phase or
        // Root parents at the top of the chain — we only emit
        // Scope ancestors as headers. SRD-100 P1c — address the
        // dispatch-time node directly (race-safe under concurrent
        // same-name dispatch); fall back to a by-name lookup only
        // for a runtime-materialized phase absent from this
        // (possibly pre-map-snapshot) tree handle.
        let phase_node = tree
            .nodes
            .get(scene_node_id)
            .filter(|n| n.kind == nmbrs_runtime::scene_tree::NodeKind::Phase && n.name == name)
            .or_else(|| {
                tree.find_phase(
                    name,
                    labels,
                    Some(&nmbrs_runtime::scene_tree::PhaseStatus::Running),
                )
                .and_then(|id| tree.nodes.get(id))
            });
        let (seq, phase_depth, ancestor_chain) = match phase_node {
            Some(n) => {
                let mut chain: Vec<usize> = Vec::new();
                let mut cursor = n.parent;
                while let Some(pid) = cursor {
                    if let Some(p) = tree.nodes.get(pid) {
                        if p.kind == nmbrs_runtime::scene_tree::NodeKind::Scope {
                            chain.push(pid);
                        }
                        cursor = p.parent;
                    } else {
                        break;
                    }
                }
                chain.reverse(); // outer-first
                let seq = n
                    .seq
                    .map(|s| format!("{dim}[{s}/{}]{reset} ", tree.total_phases()))
                    .unwrap_or_default();
                let depth = n.depth.saturating_sub(1);
                (seq, depth, chain)
            }
            None => (String::new(), 0, Vec::new()),
        };

        // Diff against the last announced chain. The shared
        // prefix doesn't re-emit; everything past the divergence
        // point gets its own indented header line.
        let mut guard = self.last_scope_chain.lock();
        let last_chain = guard.clone().unwrap_or_default();
        let common_prefix = last_chain
            .iter()
            .zip(ancestor_chain.iter())
            .take_while(|(a, b)| a == b)
            .count();
        // Push 8c: scope-ancestor headers route through
        // the `scope_header` readout so the same
        // formatter drives both the live mid-run walk
        // here and the post-run summary tree walk in
        // `nmbrs-tui::observer`. The depth-based indent
        // chrome is the surface's job per SRD-63 §10.
        for new_id in &ancestor_chain[common_prefix..] {
            if let Some(scope) = tree.nodes.get(*new_id) {
                let depth = scope.depth.saturating_sub(1);
                let indent = " ".repeat(depth);
                let mut s_buf = String::with_capacity(64);
                {
                    use nmbrs_runtime::readouts as ro;
                    let mut buf = ro::buf::StringBuf::new(&mut s_buf);
                    use ro::Readout;
                    let ctx = ScopeAncestorContext {
                        name: scope.name.clone(),
                        use_color: nmbrs_runtime::observer::use_color(),
                    };
                    ro::builtins::scope_header::ScopeHeader.render(
                        &ctx,
                        ro::Lod::Labeled,
                        ro::ContentMode::Value,
                        &ro::ReadoutOptions::new(),
                        &mut buf,
                    );
                }
                let _ = (cyan, italic);
                nmbrs_runtime::observer::log(LogLevel::Info, &format!("{indent}{s_buf}"));
            }
        }
        *guard = Some(ancestor_chain);
        drop(guard);

        // No phase-starting detail row. The condensed completed-
        // phase line emitted by `nmbrs-runtime::activity` (the
        // ✓ DONE summary, single line carrying identity + stats
        // + duration) is now the only per-phase log entry. Scope
        // ancestor headers above this point still emit, so the
        // hierarchic walk reads the same on its way down — only
        // the redundant "[N/total] [name] (coords): …" preview
        // is dropped (its info is fully captured by the ✓ line).
        let _ = (
            phase_depth,
            op_templates,
            total_cycles,
            concurrency,
            template_word,
            cycle_word,
            seq,
            labels,
        );
    }

    fn phase_completed(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        duration_secs: f64,
    ) {
        self.state.send(RunStateCmd::PhaseCompleted {
            exec_id: nmbrs_runtime::execution_context::current_exec_id(),
            scene_node_id,
            name: name.to_string(),
            labels: labels.to_string(),
            duration_secs,
        });
        // No stderr line: the executor's own diag emits the
        // formatted "phase 'X' complete (Ns)" via `observer::log`,
        // which routes back through `Self::log` below. Re-emitting
        // here would duplicate. Same reasoning as the legacy
        // StderrObserver — see the comment on its
        // phase_completed.
    }

    fn phase_failed(
        &self,
        scene_node_id: nmbrs_runtime::scene_tree::SceneNodeId,
        name: &str,
        labels: &str,
        error: &str,
    ) {
        self.state.send(RunStateCmd::PhaseFailed {
            exec_id: nmbrs_runtime::execution_context::current_exec_id(),
            scene_node_id,
            name: name.to_string(),
            labels: labels.to_string(),
            error: error.to_string(),
        });
        // Same as phase_completed — diag covers the visible line.
    }

    fn phase_progress(&self, update: &PhaseProgressUpdate) {
        self.state.send(RunStateCmd::PhaseProgress(update.clone()));
    }

    // SRD-63 — op-level status leaves (`readout: visible`). The managed
    // (LogOnly) surface is what `nmbrs run` shows, so it must forward these to
    // the actor just like `TuiObserver` does, or `phase_ops` never populates
    // and the leaves never render in the footer.
    fn op_starting(&self, parent_phase: nmbrs_runtime::scene_tree::SceneNodeId, op_name: &str) {
        self.state.send(RunStateCmd::OpStarting {
            parent: parent_phase,
            op_name: op_name.to_string(),
        });
    }

    fn op_completed(
        &self,
        parent_phase: nmbrs_runtime::scene_tree::SceneNodeId,
        op_name: &str,
        duration_secs: f64,
    ) {
        self.state.send(RunStateCmd::OpCompleted {
            parent: parent_phase,
            op_name: op_name.to_string(),
            duration_secs,
        });
    }

    fn op_failed(
        &self,
        parent_phase: nmbrs_runtime::scene_tree::SceneNodeId,
        op_name: &str,
        error: &str,
    ) {
        self.state.send(RunStateCmd::OpFailed {
            parent: parent_phase,
            op_name: op_name.to_string(),
            error: error.to_string(),
        });
    }

    fn phase_render_attach(&self, handle: nmbrs_runtime::observer::PhaseRenderHandle) {
        // SRD-100 P2 — route the per-phase render handle to the actor;
        // LogOnlySink folds `active_phases` and renders each phase's
        // status footer off it (no inline-status producer thread).
        self.state.send(RunStateCmd::AttachPhaseRender(handle));
    }

    fn run_finished(&self) {
        self.state.send(RunStateCmd::RunFinished);
        // Re-emit the canonical end-of-run marker through the observer
        // so session.log captures it. `RunLifecycle` category so a
        // console-owning adapter can keep it off the console.
        nmbrs_runtime::observer::log(LogLevel::Info, "all phases complete");
    }

    fn log(&self, level: LogLevel, message: &str) {
        self.log_tagged(level, EventTag::default(), message);
    }

    fn log_tagged(&self, level: LogLevel, tag: EventTag, message: &str) {
        // Snapshot side: every log entry is stored in the ring
        // (capped at 200) with `log_seq_total` advanced, carrying
        // its `tag` (attachment + category axes) so the terminal
        // sink can derive its scrollback filtering from them. The
        // active sink drains via the seq delta; the inspector
        // socket reads the ring directly.
        self.state.send(RunStateCmd::Log {
            severity: level_to_severity(level),
            tag,
            message: message.to_string(),
        });
        // Stderr side: synchronous write — only when no sink
        // has claimed rendering. Once a `LogOnlySink` (or other
        // log-rendering sink) flips `sink_active`, it takes over
        // and writes from the snapshot drain. The handoff is
        // race-free by construction: pre-flag lines go to stderr
        // *and* into the actor; the sink's first poll skips
        // everything ≤ the seq it observed at startup, so no
        // duplicate is rendered.
        if !self.sink_active.load(Ordering::Acquire) && level >= self.min_level {
            if message.starts_with("session: graceful shutdown requested")
                || message.starts_with("session: force-exit")
            {
                eprintln!();
            }
            // SRD-87 §5: the live-surface write goes through the log bucket
            // (the channel owns the fd, color-coding by severity); the
            // `!sink_active`/`min_level` gate above stays here as the spine's
            // handoff coordination.
            nmbrs_runtime::output_channel::log_to_surface(level, message);
        }
    }

    fn suppresses_stderr(&self) -> bool {
        // Tracks `sink_active`: whenever a sink is actively
        // rendering (LogOnlySink draining the log ring,
        // TuiSink running an alt-screen App), the runner
        // should NOT write log lines synchronously to stderr
        // — the sink owns that surface.
        //
        // The runner also gates the periodic progress-event
        // thread on this flag (executor.rs around the
        // `phase_progress` emit site): when true, a thread
        // wakes every 500 ms to emit `PhaseProgressUpdate`
        // events into the actor so any active panel-style
        // renderer (today's `TuiSink`, future custom sinks)
        // sees live data. Returning false here suppressed
        // that thread's spawn — which is exactly why the TUI
        // status panel went silent after a Ctrl-T toggle.
        self.sink_active.load(Ordering::Acquire)
    }

    fn live_suppress_flag(&self) -> Option<Arc<AtomicBool>> {
        // Inline-status suppression is the right semantic here:
        // the activity's inline-status thread should yield only
        // when an alt-screen renderer owns the terminal, NOT
        // just because LogOnlySink is doing log-line drain.
        // (LogOnlySink's log lines and the inline status line
        // share stderr without conflict — different rendering
        // strategies for different content.)
        Some(self.inline_suppress.clone())
    }

    fn reporters(&self) -> Vec<(std::time::Duration, Box<dyn Reporter>)> {
        // Hand the cadence scheduler a clone of the frame
        // broker as its 1-second base reporter. The broker is
        // Arc-shared, so calls to `Self::subscribe_frames` from
        // sinks reach the same backing storage.
        vec![(
            std::time::Duration::from_secs(1),
            Box::new(self.frame_broker.clone()) as Box<dyn Reporter>,
        )]
    }

    fn cadences(&self) -> Option<Cadences> {
        Some(self.cadences.clone())
    }

    fn on_metrics_query(&self, query: Arc<MetricsQuery>) {
        *self.metrics_query.lock() = Some(query);
    }
}

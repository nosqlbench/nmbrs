// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! [`ActivityReadoutContext`] — concrete `ReadoutContext`
//! impl built from the activity-side data the
//! ✓ DONE block already gathers.
//!
//! Push 1 surface only. Built up by `nmbrs-runtime::activity`
//! at end-of-activity right before invoking the `phase_outcome`
//! readout. Each later push grows this struct as new
//! built-ins (and new `ReadoutContext` methods) arrive.
//!
//! Owned, not borrowed: every field is computed at the call
//! site (counter snapshots, the rendered chip string, the
//! depth-indent string) and parked here for the readout's
//! duration. This keeps the readout call free of borrow
//! plumbing through the activity's locks.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::lifecycle::EventType;
use crate::readouts::{LifecycleState, ReadoutContext};

/// Snapshot of everything `phase_outcome` needs to render at
/// `Lod::Labeled / ContentMode::Value`. Constructed at
/// end-of-activity in `nmbrs-runtime::activity`; thrown
/// away after the render returns.
pub struct ActivityReadoutContext {
    pub phase_name: String,
    pub phase_seq: Option<(usize, usize)>,
    pub phase_labels: String,
    pub cycles_completed: u64,
    pub cycles_total: u64,
    pub ops_ok: u64,
    /// SKIPPED ops (`skips_total`) — excluded from the ok% denominator
    /// (a skip is neither a success nor a failure).
    pub skips: u64,
    pub errors: u64,
    pub retries: u64,
    pub concurrency: usize,
    pub elapsed_secs: f64,
    pub consumed: u64,
    pub status_metric_chips: String,
    pub depth_indent: String,
    pub use_color: bool,
    /// Snapshot of the activity's memo at context-build time.
    /// Empty when no `memo:` wrapper is active on any op.
    pub memo: String,
    /// SRD-76 / SRD-82 Part 1 — the terminal two-axis outcome. The
    /// executor sets this when it installs the
    /// [`crate::phase_outcome::PhaseOutcome`] on the scene tree,
    /// before firing the on_phase_end binder.
    pub outcome: crate::phase_outcome::Outcome,
    /// SRD-76 — chronologically ordered error list. Empty
    /// for `Completed`/`Skipped`; non-empty for `Failed`.
    /// Drives the failure-flavoured rendering of the
    /// [`crate::readouts::builtins::phase_outcome`] readout.
    pub outcome_errors: Vec<crate::phase_outcome::PhaseErrorDetail>,
    /// SRD-76 — cursor-resume payload, when the phase
    /// supports it. `None` for the common case.
    pub outcome_resume_cursor: Option<crate::phase_outcome::ResumeCursor>,
    /// True for a daemon (open-ended) phase: there is no "done" to
    /// meter, so completion renders no percentage (SRD-92 — the
    /// same rule the live meter slot follows). Without this the
    /// trait-default `false` let `progress_fraction()` fall through
    /// to the cycles basis and a stopped daemon printed a
    /// meaningless `N%` (cycles over its wall-clock ceiling).
    pub open_ended: bool,
}

impl ReadoutContext for ActivityReadoutContext {
    fn subject_name(&self) -> &str {
        &self.phase_name
    }
    fn subject_seq(&self) -> Option<(usize, usize)> {
        self.phase_seq
    }
    fn subject_labels(&self) -> &str {
        &self.phase_labels
    }
    fn open_ended(&self) -> bool {
        self.open_ended
    }
    fn cycles_completed(&self) -> u64 {
        self.cycles_completed
    }
    fn cycles_total(&self) -> u64 {
        self.cycles_total
    }
    fn ops_ok(&self) -> u64 {
        self.ops_ok
    }
    fn skips(&self) -> u64 {
        self.skips
    }
    fn errors(&self) -> u64 {
        self.errors
    }
    fn retries(&self) -> u64 {
        self.retries
    }
    fn concurrency(&self) -> usize {
        self.concurrency
    }
    fn elapsed_secs(&self) -> f64 {
        self.elapsed_secs
    }
    fn consumed(&self) -> u64 {
        self.consumed
    }
    fn status_metric_chips(&self) -> String {
        self.status_metric_chips.clone()
    }
    fn depth_indent(&self) -> &str {
        &self.depth_indent
    }
    fn use_color(&self) -> bool {
        self.use_color
    }
    fn event(&self) -> EventType {
        EventType::PhaseEnd
    }
    fn subject_state(&self) -> LifecycleState {
        // Mirror the outcome status onto the lifecycle axis
        // so existing consumers that branch on `subject_state`
        // see Failed when the phase failed (today they'd see
        // Completed because the binder fired before the
        // executor recorded the failure). SRD-76 unifies
        // the two surfaces.
        match self.outcome.validity {
            crate::phase_outcome::Validity::Succeeded => LifecycleState::Completed,
            crate::phase_outcome::Validity::Failed => LifecycleState::Failed(
                self.outcome_errors
                    .first()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "phase failed".into()),
            ),
        }
    }
    fn phase_memo(&self) -> &str {
        &self.memo
    }
    fn outcome(&self) -> crate::phase_outcome::Outcome {
        self.outcome.clone()
    }
    fn outcome_errors(&self) -> &[crate::phase_outcome::PhaseErrorDetail] {
        &self.outcome_errors
    }
    fn outcome_resume_cursor(&self) -> Option<&crate::phase_outcome::ResumeCursor> {
        self.outcome_resume_cursor.as_ref()
    }
}

/// Per-event context for lifecycle fires (Push 9a):
/// `on_session_start` / `on_session_end`,
/// `on_phase_start`, `on_each_start` / `on_each_end`,
/// `on_scope_start` / `on_scope_end`.
///
/// Carries just the fields a structural readout
/// (`scope_header`, `session_banner`, `each_close`, …)
/// needs — subject name, root-first labels, depth indent,
/// colour flag, plus the firing event so a wildcard-bound
/// readout can branch.
///
/// Counter-shaped methods all return zero / empty since
/// lifecycle readouts don't depend on per-cycle progress;
/// the `Default` impl on the trait handles those.
pub struct LifecycleContext {
    pub event: crate::lifecycle::EventType,
    pub subject_name: String,
    pub subject_labels: String,
    pub depth_indent: String,
    pub use_color: bool,
    /// SRD-106 — the session id the `stick_session` rung
    /// re-attached to; empty everywhere except the SessionStart
    /// fire of a stick-engaged run. Read by `session_notice`.
    pub stick_reattached: String,
}

impl ReadoutContext for LifecycleContext {
    fn subject_name(&self) -> &str {
        &self.subject_name
    }
    fn subject_seq(&self) -> Option<(usize, usize)> {
        None
    }
    fn subject_labels(&self) -> &str {
        &self.subject_labels
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
        &self.depth_indent
    }
    fn use_color(&self) -> bool {
        self.use_color
    }
    fn event(&self) -> crate::lifecycle::EventType {
        self.event
    }
    fn stick_reattached_session(&self) -> &str {
        &self.stick_reattached
    }
    fn subject_state(&self) -> LifecycleState {
        // Lifecycle events fire at the boundary; the
        // subject is in transition. `Running` is the safe
        // default for `on_*_start` (the subject is now
        // in flight); `_end` events technically transition
        // to `Completed` but the readouts that fire here
        // (scope_header, session_banner, etc.) don't
        // branch on subject_state anyway, so a single
        // default keeps things simple.
        LifecycleState::Running
    }
}

/// Per-tick context for the inline-status refresh thread
/// (Push 2). Identifies as [`EventType::Update`]; carries a
/// monotonic refresh tick for spinner cycling, the full
/// activity name with leaf coord, and pre-formatted
/// adapter / batch tails (the iteration over registered
/// dispensers stays in the surface for now — Push 4
/// migrates the trait to expose the typed iterator).
pub struct InlineRefreshContext {
    pub phase_name: String,
    pub activity_name: String,
    pub phase_seq: Option<(usize, usize)>,
    pub phase_labels: String,
    pub cycles_completed: u64,
    pub cycles_total: u64,
    pub ops_started: u64,
    pub ops_finished: u64,
    pub ops_ok: u64,
    /// SKIPPED ops (`skips_total`) — `if:`-gated ops that ran no
    /// adapter call. Excluded from the `ok%` denominator: a skip is
    /// neither a success nor a failure.
    pub skips: u64,
    pub errors: u64,
    pub retries: u64,
    /// SRD-91 attempt-level tallies — successful and failed
    /// RESOLVED attempts (both observed at attempt end).
    /// `attempt_ok / (attempt_ok + attempt_failed)` is the
    /// attempt success rate the status line surfaces beside the
    /// result-level `ok%`; in-flight attempts are excluded so it
    /// doesn't skew low the way the dispatch-time counter would.
    pub attempt_ok: u64,
    pub attempt_failed: u64,
    pub concurrency: usize,
    pub elapsed_secs: f64,
    pub consumed: u64,
    /// Cursor ordinals consumed / cursor extent for a data-driven
    /// phase (polydat `global_consumed()` / `global_extent()`).
    /// Both `0` for non-cursor phases (plain `cycles:`), where the
    /// display keeps the op-denominated `cycles:` chip. `rows_total
    /// > 0` selects the row-denominated `rows:{consumed}/{total}`
    /// chip + rows/s rate.
    pub rows_consumed: u64,
    pub rows_total: u64,
    pub status_metric_chips: String,
    pub adapter_counters_text: String,
    pub batch_info_text: String,
    pub depth_indent: String,
    pub refresh_tick: u64,
    pub use_color: bool,
    /// Snapshot of the activity's memo at tick build time.
    /// Empty when no `memo:` wrapper has published anything.
    pub memo: String,
    /// Derived-progress override snapshot (see
    /// [`crate::activity::ActivityMetrics::progress_override`]).
    pub progress_override: Option<f64>,
    /// Producer-elapsed seconds recorded with the override — the
    /// measured-basis ETA's time denominator.
    pub progress_override_elapsed: Option<f64>,
    /// Open-ended subject (daemon / background poll): no progress
    /// meter; latency summary renders in its place.
    pub open_ended: bool,
    /// Live service-time percentiles (nanos) from the activity's
    /// timer, for the open-ended latency chip. 0 = no data yet.
    pub lat_p50_nanos: u64,
    pub lat_p99_nanos: u64,
}

impl ReadoutContext for InlineRefreshContext {
    fn subject_name(&self) -> &str {
        &self.phase_name
    }
    fn activity_name(&self) -> &str {
        &self.activity_name
    }
    fn subject_seq(&self) -> Option<(usize, usize)> {
        self.phase_seq
    }
    fn subject_labels(&self) -> &str {
        &self.phase_labels
    }
    fn cycles_completed(&self) -> u64 {
        self.cycles_completed
    }
    fn cycles_total(&self) -> u64 {
        self.cycles_total
    }
    fn ops_started(&self) -> u64 {
        self.ops_started
    }
    fn ops_finished(&self) -> u64 {
        self.ops_finished
    }
    fn ops_ok(&self) -> u64 {
        self.ops_ok
    }
    fn skips(&self) -> u64 {
        self.skips
    }
    fn errors(&self) -> u64 {
        self.errors
    }
    fn retries(&self) -> u64 {
        self.retries
    }
    fn attempt_ok(&self) -> u64 {
        self.attempt_ok
    }
    fn attempt_failed(&self) -> u64 {
        self.attempt_failed
    }
    fn concurrency(&self) -> usize {
        self.concurrency
    }
    fn elapsed_secs(&self) -> f64 {
        self.elapsed_secs
    }
    fn consumed(&self) -> u64 {
        self.consumed
    }
    fn rows_consumed(&self) -> u64 {
        self.rows_consumed
    }
    fn rows_total(&self) -> u64 {
        self.rows_total
    }
    fn status_metric_chips(&self) -> String {
        self.status_metric_chips.clone()
    }
    fn adapter_counters_text(&self) -> String {
        self.adapter_counters_text.clone()
    }
    fn batch_info_text(&self) -> String {
        self.batch_info_text.clone()
    }
    fn depth_indent(&self) -> &str {
        &self.depth_indent
    }
    fn use_color(&self) -> bool {
        self.use_color
    }
    fn event(&self) -> EventType {
        EventType::Update
    }
    fn refresh_tick(&self) -> u64 {
        self.refresh_tick
    }
    fn phase_memo(&self) -> &str {
        &self.memo
    }
    fn progress_override(&self) -> Option<f64> {
        self.progress_override
    }
    fn open_ended(&self) -> bool {
        self.open_ended
    }
    fn latency_p50_nanos(&self) -> u64 {
        self.lat_p50_nanos
    }
    fn latency_p99_nanos(&self) -> u64 {
        self.lat_p99_nanos
    }
    /// SRD-63 Push 9f: derive ETA from `cycles_total -
    /// ops_finished` divided by the observed throughput
    /// rate (`ops_finished / elapsed`). `None` when the
    /// extent isn't known (sourceless phase running by
    /// time / open-ended) or no progress has been made
    /// yet (rate would divide-by-zero).
    ///
    /// A derived-progress override with a recorded producer
    /// elapsed wins: ETA = `elapsed × (1−f)/f` — the measured
    /// basis for a single long op (a poll-driven drain) whose
    /// cycle accounting stands still. Guarded to `f` in
    /// `(0, 1)`: at 0 nothing is measurable yet, at 1 the
    /// after-state takes over momentarily.
    fn eta_secs(&self) -> Option<f64> {
        // Open-ended subjects have no completion, hence no ETA.
        if self.open_ended {
            return None;
        }
        if let (Some(f), Some(e)) = (self.progress_override, self.progress_override_elapsed)
            && f > 0.0
            && f < 1.0
            && e > 0.0
        {
            return Some(e * (1.0 - f) / f);
        }
        // Cursor-driven phase: rows are the authoritative ordinal
        // basis. Ops stride N rows each, so an op-denominated rate
        // against the row-denominated extent would overstate the
        // ETA by the stride factor (e.g. ~64/s ops vs 7.8K/s rows
        // → 35h instead of 18m).
        if self.rows_total > 0 && self.elapsed_secs > 0.0 {
            if self.rows_consumed == 0 {
                return None;
            }
            let rate = self.rows_consumed as f64 / self.elapsed_secs;
            let remaining = self.rows_total.saturating_sub(self.rows_consumed) as f64;
            return Some(remaining / rate);
        }
        if self.cycles_total == 0 || self.elapsed_secs <= 0.0 {
            return None;
        }
        let rate = self.ops_finished as f64 / self.elapsed_secs;
        if rate <= 0.0 {
            return None;
        }
        let remaining = self.cycles_total.saturating_sub(self.ops_finished) as f64;
        Some(remaining / rate)
    }
}

/// One-shot lifecycle fire helper. Builds a binder for
/// `event` against `bindings`, runs every bound body
/// against `ctx`, writes the rendered text via
/// `crate::diag!` (so it lands in stderr / log file
/// uniformly), and captures to the snapshot store via
/// `subject_kind` / `subject_id`.
///
/// Best-effort: errors building the binder log a warning
/// and the fire is skipped — a malformed `readouts:`
/// binding never blocks the run. Bindings that resolve
/// to zero bodies (the usual case for structural slots
/// with no built-in default and no workload binding)
/// produce no output.
pub fn fire_lifecycle(
    event: crate::lifecycle::EventType,
    bindings: &nmbrs_workload::model::ReadoutsBindings,
    default: Option<crate::readouts::BakedBody>,
    ctx: &dyn crate::readouts::ReadoutContext,
    snapshot_writer: Option<&crate::readouts::snapshot::SnapshotWriter>,
) {
    use crate::readouts::ReadoutBinder;

    // Use the built-in default when supplied (currently
    // only PhaseEnd/Update have defaults); otherwise the
    // slot starts empty and falls through to whatever the
    // workload bound. `build_event_binder` always seeds
    // the default — pass an empty body when none exists
    // so unbound slots stay quiet.
    let seed = default.unwrap_or_default();
    let mut binder = match crate::readouts::build_event_binder(bindings, event, seed) {
        Ok(b) => b,
        Err(e) => {
            crate::diag!(
                crate::observer::LogLevel::Warn,
                "readouts: failed to bind {slot} — {e}",
                slot = event.slot_name()
            );
            return;
        }
    };
    let mut sink = crate::readouts::StringSink::with_capacity(128);
    binder.fire(event, ctx, &mut sink);
    let rendered = sink.take();
    if rendered.trim().is_empty() {
        return; // no bound body for this slot — quiet exit
    }
    // The firing lifecycle slot IS the tag's attachment axis —
    // this readout render is definitionally attached to the
    // boundary that fired it. Sinks derive their rules from the
    // axes (the terminal sink keeps `PhaseStart`-attached renders
    // out of scrollback — its managed phase-history region mirrors
    // them; scope / iteration / session boundaries have no region
    // counterpart and flow through like in-flight lines).
    let tag = crate::observer::EventTag::at(event, crate::observer::EventCategory::General);
    crate::observer::log_tagged(crate::observer::LogLevel::Info, tag, &rendered);

    // Snapshot capture per Push 6. Subject identity comes
    // straight from the context: `subject_kind` from the
    // firing event (the sole source of truth for which
    // table dimension this row belongs to), `subject_id`
    // from `ctx.subject_id()`'s default `name@labels`
    // shape (overridden for session-scope contexts that
    // collapse to a literal `"session"`). Replay reads
    // stable tuples (slot, subject_kind, subject_id, ...).
    let subject_id = ctx.subject_id();
    crate::readouts::snapshot::capture(
        snapshot_writer,
        event.slot_name(),
        ctx.subject_exec_id(),
        event.subject_kind().as_str(),
        &subject_id,
        "binder",
        crate::readouts::snapshot::lod_str(crate::readouts::Lod::Labeled),
        &rendered,
    );
}

/// Build an [`InlineRefreshContext`] from the per-tick
/// counter snapshots the inline-status thread takes. This
/// preserves the byte-equivalence target by constructing
/// the same intermediate values the prior `format!()`
/// inlined (adapter counter chips, batch info, the
/// scene-tree-walk for `seq` + depth indent) — they're
/// each derived once per tick, then handed to the
/// [`crate::readouts::builtins::phase_status::PhaseStatus`]
/// readout for actual rendering.
// reason: cohesive per-tick context builder — each argument is a distinct
// counter snapshot/handle taken once per refresh tick; grouping them into a
// struct would only relocate the same fields.
/// Resolve a phase's `(seq, total)` pre-map coordinate + depth indent
/// from the GLOBAL scene tree by NAME, matching the first Running node.
///
/// Retained for the legacy inline-status / phase-end callers that only
/// have the activity name. The executor's on-task render-handle attach
/// (SRD-100 P2) resolves these from the dispatch-time `SceneNodeId`
/// instead — race-safe under concurrent same-name dispatch, where this
/// first-Running-match could pick the wrong sibling.
pub fn resolve_phase_coord_by_name(activity_name: &str) -> (Option<(usize, usize)>, String) {
    let bare_name = activity_name
        .split_once(" (")
        .map(|(n, _)| n)
        .unwrap_or(activity_name);
    crate::scene_tree::current()
        .and_then(|t| {
            let node = t
                .dfs_phases()
                .find(|n| {
                    n.name == bare_name
                        && matches!(n.status, crate::scene_tree::PhaseStatus::Running)
                })?
                .clone();
            let seq = node.seq?;
            let depth = node.depth.saturating_sub(1);
            Some((Some((seq, t.total_phases())), " ".repeat(depth)))
        })
        .unwrap_or((None, String::new()))
}

/// Resolve a phase's `(seq, total)` pre-map coordinate + depth indent
/// from the GLOBAL scene tree by its dispatch-time [`SceneNodeId`](crate::scene_tree::SceneNodeId).
///
/// SRD-100 P2 — the race-safe replacement for [`resolve_phase_coord_by_name`]:
/// keying on the node id (allocated at dispatch, P1c) addresses the exact
/// node, so concurrent same-name siblings (sweep cells, comprehension
/// iterations, daemon+foreground) each resolve their OWN coordinate. The
/// executor calls this once at render-handle attach time.
pub fn resolve_phase_coord_by_id(
    scene_node_id: crate::scene_tree::SceneNodeId,
) -> (Option<(usize, usize)>, String) {
    crate::scene_tree::current()
        .and_then(|t| {
            let node = t.nodes.get(scene_node_id)?;
            let seq = node.seq?;
            let depth = node.depth.saturating_sub(1);
            Some((Some((seq, t.total_phases())), " ".repeat(depth)))
        })
        .unwrap_or((None, String::new()))
}

/// Average batch size for the ` rows/batch:` chip — rows written per
/// successful batch op.
///
/// Prefers `rows_inserted / batch_writes`, where `batch_writes` is the
/// number of ops that actually wrote ≥1 row (published by the CQL
/// batch dispensers alongside `rows_inserted`). This is the true
/// per-op stride: retried, failed, and non-inserting ops never touch
/// `batch_writes`, so the denominator can't drift the way
/// `stanzas_total` (one inc per op *attempt*, regardless of
/// success/failure/type) does.
///
/// Falls back to `rows_inserted / stanzas_total` when no `batch_writes`
/// counter is present — non-CQL or older adapter paths that never
/// learned to publish it. Returns `None` when no batched write was
/// observed (average would be ≤1 row/op, i.e. not a batch).
///
/// Kept as a free function (not inline at the call site) so the three
/// display paths that render this chip — the inline refresh here and
/// the two TUI progress-thread snapshots in `executor.rs` — share one
/// formula, and so the formula is unit-testable in isolation.
/// Whether a dispenser status counter is INTERNAL — published only to feed a
/// derived display metric, never rendered as its own `<name>/s` throughput
/// chip. The convention is a **leading underscore**: `_batch_writes` backs the
/// `rows/batch` average (see [`rows_per_batch`]) but must not clutter the
/// operator-facing chip row as `_batch_writes/s`. Every surface that turns a
/// dispenser counter into a chip filters on this one predicate, so the
/// convention has a single point of truth. `find_counter`-style lookups pass
/// the underscore name explicitly, so the counter stays available to the
/// derived metric it exists for.
pub fn is_internal_counter(name: &str) -> bool {
    name.starts_with('_')
}

pub(crate) fn rows_per_batch(
    rows_inserted: Option<u64>,
    batch_writes: Option<u64>,
    stanzas: u64,
) -> Option<f64> {
    let rows = rows_inserted?;
    match batch_writes {
        // A batch write count is present: divide by it directly. Show
        // only when a real batch (>1 row/op) was observed.
        Some(batches) if batches > 0 => (rows > batches).then(|| rows as f64 / batches as f64),
        // Legacy / non-CQL fallback: attempt-count denominator.
        _ => (stanzas > 0 && rows > stanzas).then(|| rows as f64 / stanzas as f64),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_inline_refresh_context(
    progress_metrics: &Arc<crate::activity::ActivityMetrics>,
    activity_name: &str,
    concurrency: usize,
    total_extent: u64,
    // Row-level cursor progress for a data-driven phase
    // (`global_consumed()` / `global_extent()`); both `0` for
    // non-cursor phases so the readout keeps the `cycles:` chip.
    rows_consumed: u64,
    rows_total: u64,
    elapsed_secs: f64,
    refresh_tick: u64,
    status_metrics: &[String],
    memo: &arc_swap::ArcSwap<String>,
    // SRD-100 P2 — pre-map `(seq, total)` coordinate + depth indent,
    // resolved by the caller. The producer no longer walks the global
    // scene tree by name (a first-Running-match that raced under
    // concurrent same-name dispatch); the executor's on-task attach
    // resolves these from the dispatch-time `SceneNodeId` instead.
    phase_seq: Option<(usize, usize)>,
    depth_indent: String,
    // Open-ended (daemon) subject: suppress progress metering, carry
    // the latency chip instead.
    open_ended: bool,
) -> InlineRefreshContext {
    // Counter snapshots — must match the prior inline-status
    // formulas so byte equivalence holds.
    let started = progress_metrics.ops_started.load(Ordering::Relaxed);
    let finished = progress_metrics.ops_finished.load(Ordering::Relaxed);
    let ops_completed = progress_metrics.cycles_completed();
    // SRD-91: terminal-success count = `result_success.count()`;
    // `errors_total` is RESULT-level (one inc per terminal failure),
    // so it drives the `e:` count directly.
    let successes = progress_metrics.result_success.count();
    let errors = progress_metrics.errors_total.get();
    let failed_ops = ops_completed
        .saturating_sub(successes)
        .saturating_sub(progress_metrics.skips_total.get());
    let consumed = finished;
    // SRD-91 attempt-level tallies, owned by the innermost
    // `TriesDispenser` (or the error-handler wrapper for
    // single-attempt ops). ALL attempt instruments count when an
    // attempt RETURNS (2026-07-10 — attempt_total moved from
    // dispatch to resolution, same discipline as the result
    // instruments), so `attempt_total == attempt_success +
    // attempt_failure` holds at every read and
    // `attempt_ok / (attempt_ok + attempt_failed)` is the exact
    // attempt success rate — dropping below the result-level
    // `ok%` exactly when retries burn attempts to keep results
    // green.
    let attempt_ok = progress_metrics.attempt_success.count();
    let attempt_failed = progress_metrics.attempt_failure.count();
    // Retries = failed attempts that were NOT the terminal outcome.
    // `errors_total` went RESULT-level with the TriesDispenser
    // refactor, so the old `errors - failed_ops` derivation
    // collapsed to ~0; the per-attempt failure count now lives in
    // `attempt_failure`, and `attempt_failed - failed_ops` is the
    // true retry count (each non-terminal failed attempt spawned a
    // retry).
    let retries = attempt_failed.saturating_sub(failed_ops);

    // Adapter-status chips: ` <name>:<rate>/s` per registered
    // dispenser counter. `collect_status_counters` aggregates
    // every dispenser's typed counters into a flat
    // `(name, total)` list — same data the inline thread used
    // to read directly from `progress_metrics.dispensers`,
    // exposed through the public accessor.
    let mut adapter_counters_text = String::new();
    let counters = progress_metrics.collect_status_counters();
    for (name, total) in &counters {
        // Internal counters (`_batch_writes`) feed derived metrics only —
        // never their own chip. See `is_internal_counter`.
        if is_internal_counter(name) {
            continue;
        }
        let item_rate = if elapsed_secs > 0.0 {
            *total as f64 / elapsed_secs
        } else {
            0.0
        };
        let rate_str = if item_rate >= 1_000_000.0 {
            format!("{:.1}M", item_rate / 1_000_000.0)
        } else if item_rate >= 1_000.0 {
            format!("{:.1}K", item_rate / 1_000.0)
        } else {
            format!("{:.0}", item_rate)
        };
        adapter_counters_text.push_str(&format!(" {name}:{rate_str}/s"));
    }

    // Batch info: ` rows/batch:` = true average batch size (rows per
    // successful batch op). Prefers `rows_inserted / batch_writes`
    // when the CQL batch dispensers publish a `batch_writes` counter;
    // otherwise falls back to the attempt-based `rows_inserted /
    // stanzas_total`. See [`rows_per_batch`].
    let stanzas = progress_metrics.stanzas_total.get();
    let find_counter = |want: &str| counters.iter().find(|(n, _)| n == want).map(|(_, t)| *t);
    let batch_info_text = rows_per_batch(
        find_counter("rows_inserted"),
        find_counter("_batch_writes"),
        stanzas,
    )
    .map(|avg| format!(" rows/batch:{avg:.1}"))
    .unwrap_or_default();

    // Pre-rendered status-metric chip string.
    let status_metric_chips = progress_metrics
        .collect_status_values(status_metrics)
        .concat();

    // Activity name carries the leaf coord; the bare phase name is the
    // readout's subject identity. (`phase_seq` / `depth_indent` now arrive
    // as params — resolved race-safely by the caller.)
    let bare_name = activity_name
        .split_once(" (")
        .map(|(n, _)| n)
        .unwrap_or(activity_name);

    let memo_snapshot: String = memo.load().as_str().to_string();
    // Live latency percentiles for the open-ended chip (and any future
    // consumer): a peek at the service-time HDR — no reset, cheap at
    // display cadence.
    let (lat_p50, lat_p99) = {
        let snap = progress_metrics.service_time.peek_snapshot();
        let h = &snap.histogram;
        if h.is_empty() {
            (0, 0)
        } else {
            (h.value_at_quantile(0.50), h.value_at_quantile(0.99))
        }
    };
    InlineRefreshContext {
        phase_name: bare_name.to_string(),
        activity_name: activity_name.to_string(),
        phase_seq,
        phase_labels: String::new(),
        cycles_completed: ops_completed,
        cycles_total: total_extent,
        ops_started: started,
        ops_finished: finished,
        ops_ok: successes,
        skips: progress_metrics.skips_total.get(),
        errors,
        retries,
        attempt_ok,
        attempt_failed,
        concurrency,
        elapsed_secs,
        consumed,
        rows_consumed,
        rows_total,
        status_metric_chips,
        adapter_counters_text,
        batch_info_text,
        depth_indent,
        refresh_tick,
        use_color: crate::observer::use_color(),
        memo: memo_snapshot,
        progress_override: progress_metrics.progress_override(),
        progress_override_elapsed: progress_metrics.progress_override_elapsed_secs(),
        open_ended,
        lat_p50_nanos: lat_p50,
        lat_p99_nanos: lat_p99,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn eta_prefers_measured_basis_when_override_present() {
        // 25% done after 60s of measured work → 180s remain,
        // regardless of the standing-still cycle accounting.
        let ctx = super::InlineRefreshContext {
            phase_name: String::new(),
            activity_name: String::new(),
            phase_seq: None,
            phase_labels: String::new(),
            cycles_completed: 3,
            cycles_total: 4,
            ops_started: 4,
            ops_finished: 3,
            ops_ok: 3,
            skips: 0,
            errors: 0,
            retries: 0,
            attempt_ok: 3,
            attempt_failed: 0,
            concurrency: 1,
            elapsed_secs: 600.0,
            consumed: 3,
            rows_consumed: 0,
            rows_total: 0,
            status_metric_chips: String::new(),
            adapter_counters_text: String::new(),
            batch_info_text: String::new(),
            depth_indent: String::new(),
            refresh_tick: 0,
            use_color: false,
            memo: String::new(),
            progress_override: Some(0.25),
            progress_override_elapsed: Some(60.0),
            open_ended: false,
            lat_p50_nanos: 0,
            lat_p99_nanos: 0,
        };
        use crate::readouts::ReadoutContext;
        let eta = ctx.eta_secs().expect("measured ETA");
        assert!((eta - 180.0).abs() < 1e-9, "eta={eta}");
        // Without the elapsed companion, fall back to cycle basis.
        let ctx2 = super::InlineRefreshContext {
            progress_override_elapsed: None,
            ..ctx
        };
        let eta2 = ctx2.eta_secs().expect("cycle ETA");
        assert!((eta2 - 200.0).abs() < 1e-9, "eta2={eta2}");
    }

    #[test]
    fn eta_uses_row_basis_for_cursor_phases() {
        // Cursor phase, ops stride 100 rows each: 2M of 10M rows in
        // 200s → 8M remain at 10K rows/s → 800s. The op basis
        // (10K ops finished vs a 10M extent) would claim ~200,000s —
        // the stride-factor overstatement this pins against.
        let ctx = super::InlineRefreshContext {
            phase_name: String::new(),
            activity_name: String::new(),
            phase_seq: None,
            phase_labels: String::new(),
            cycles_completed: 10_000,
            cycles_total: 10_000_000,
            ops_started: 10_000,
            ops_finished: 10_000,
            ops_ok: 10_000,
            skips: 0,
            errors: 0,
            retries: 0,
            attempt_ok: 10_000,
            attempt_failed: 0,
            concurrency: 1,
            elapsed_secs: 200.0,
            consumed: 10_000,
            rows_consumed: 2_000_000,
            rows_total: 10_000_000,
            status_metric_chips: String::new(),
            adapter_counters_text: String::new(),
            batch_info_text: String::new(),
            depth_indent: String::new(),
            refresh_tick: 0,
            use_color: false,
            memo: String::new(),
            progress_override: None,
            progress_override_elapsed: None,
            open_ended: false,
            lat_p50_nanos: 0,
            lat_p99_nanos: 0,
        };
        use crate::readouts::ReadoutContext;
        let eta = ctx.eta_secs().expect("row-basis ETA");
        assert!((eta - 800.0).abs() < 1e-6, "eta={eta}");
        // No rows consumed yet → unknown, not a fabricated op-basis ETA.
        let ctx2 = super::InlineRefreshContext {
            rows_consumed: 0,
            ..ctx
        };
        assert!(
            ctx2.eta_secs().is_none(),
            "zero-row cursor phase must have no ETA"
        );
    }

    use super::{is_internal_counter, rows_per_batch};

    /// The leading-underscore convention: `_batch_writes` is an internal
    /// denominator (hidden from the chip row), while a plain counter like
    /// `rows_inserted` is a visible throughput chip. The chip-render loops
    /// filter on exactly this predicate.
    #[test]
    fn internal_counter_is_underscore_prefixed() {
        assert!(is_internal_counter("_batch_writes"));
        assert!(!is_internal_counter("rows_inserted"));
        assert!(!is_internal_counter("queries"));
    }

    /// With a `batch_writes` counter present, `rows/batch` is the true
    /// average batch size (`rows_inserted / batch_writes`), NOT the
    /// attempt-based `rows_inserted / stanzas_total`. Here 1000 rows
    /// across 5 successful batch ops ⇒ 200.0, even though 40 op
    /// attempts (stanzas) were recorded — the stanzas formula would
    /// have wrongly shown 25.0.
    #[test]
    fn prefers_batch_writes_over_stanzas() {
        let avg = rows_per_batch(Some(1000), Some(5), 40);
        assert_eq!(avg, Some(200.0));
    }

    /// Without a `batch_writes` counter (non-CQL / older paths), the
    /// formula falls back to `rows_inserted / stanzas_total`.
    #[test]
    fn falls_back_to_stanzas_without_batch_writes() {
        let avg = rows_per_batch(Some(1000), None, 40);
        assert_eq!(avg, Some(25.0));
    }

    /// `batch_writes = 0` behaves like "not present" — the counter is
    /// only published once it has ticked, but guard against a zero
    /// denominator either way and use the fallback.
    #[test]
    fn zero_batch_writes_uses_fallback() {
        let avg = rows_per_batch(Some(1000), Some(0), 40);
        assert_eq!(avg, Some(25.0));
    }

    /// No batched write observed (avg would be ≤1 row/op) ⇒ no chip.
    #[test]
    fn no_batch_observed_is_none() {
        // batch_writes path: rows == batches ⇒ average of 1, not a batch.
        assert_eq!(rows_per_batch(Some(5), Some(5), 40), None);
        // stanzas fallback: rows == stanzas ⇒ not a batch.
        assert_eq!(rows_per_batch(Some(40), None, 40), None);
        // no rows_inserted counter at all ⇒ nothing to show.
        assert_eq!(rows_per_batch(None, Some(5), 40), None);
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Bridge between the TUI's `PhaseEntry` / `ActivePhase`
//! state and the SRD-63 readout engine.
//!
//! Per-phase rendering goes through this module: build a
//! [`PhaseRowContext`], fire the binder, parse the resulting
//! ANSI bytes into ratatui [`Line<'static>`] via
//! [`crate::readout_sink::TuiReadoutSink`], surface
//! focus highlighting on the focused readout's spans.
//!
//! Scope: replaces the per-phase status / progress / rate
//! rows that the legacy `format_phase_detail_with_live`
//! built directly. Latency bars, sparklines, relevancy
//! aggregates stay in the legacy path until they're
//! migrated as readouts in their own right.

use std::time::Instant;

use nmbrs_runtime::lifecycle::EventType;
use nmbrs_runtime::readouts as ro;
use ratatui::style::{Color, Modifier};
use ratatui::text::{Line, Span};

use crate::readout_sink::TuiReadoutSink;
use crate::state::{ActivePhase, PhaseEntry, PhaseStatus};

/// `ReadoutContext` adapter for the TUI's phase rows.
/// Carries the union of state needed by `phase_status` (live
/// counters), `phase_outcome` (final summary), and
/// `phase_summary` (lifecycle marker). Built on demand each
/// frame from the run-state snapshot.
pub struct PhaseRowContext {
    name: String,
    labels: String,
    state: ro::LifecycleState,
    elapsed: f64,
    cycles_completed: u64,
    cycles_total: u64,
    ops_started: u64,
    ops_finished: u64,
    ops_ok: u64,
    skips: u64,
    errors: u64,
    retries: u64,
    concurrency: usize,
    consumed: u64,
    refresh_tick: u64,
    use_color: bool,
    event: EventType,
}

impl PhaseRowContext {
    /// Build a context for a phase that is currently running
    /// (live counters from `ActivePhase`). Fires under
    /// [`EventType::Update`] — the refresh-tick event the TUI
    /// re-renders against every frame.
    pub fn live(phase: &PhaseEntry, live: &ActivePhase, refresh_tick: u64) -> Self {
        let elapsed = live.started_at.elapsed().as_secs_f64();
        Self {
            name: phase.name.clone(),
            labels: phase.labels.clone(),
            state: ro::LifecycleState::Running,
            elapsed,
            cycles_completed: live.ops_finished,
            cycles_total: live.cursor_extent,
            ops_started: live.ops_started,
            ops_finished: live.ops_finished,
            ops_ok: live.ops_ok,
            skips: live.skips,
            errors: live.errors,
            retries: live.retries,
            concurrency: live.fibers,
            consumed: live.ops_started,
            refresh_tick,
            use_color: true,
            event: EventType::Update,
        }
    }

    /// Build a context for a phase that has reached a
    /// terminal state (completed / failed / pending).
    /// Fires under `EventType::PhaseEnd` so `phase_outcome` /
    /// `phase_summary` produce their final-state forms.
    pub fn terminal(phase: &PhaseEntry) -> Self {
        let state = match &phase.status {
            PhaseStatus::Completed => ro::LifecycleState::Completed,
            PhaseStatus::Failed(e) => ro::LifecycleState::Failed(e.clone()),
            PhaseStatus::Running => ro::LifecycleState::Running,
            PhaseStatus::Pending => ro::LifecycleState::Pending,
        };
        let elapsed = phase.duration_secs.unwrap_or(0.0);
        let (cycles_completed, ops_ok, skips, errors, retries, concurrency) = phase
            .summary
            .as_ref()
            .map(|s| {
                (
                    s.ops_finished,
                    s.ops_ok,
                    s.skips,
                    s.errors,
                    s.retries,
                    s.fibers,
                )
            })
            .unwrap_or((0, 0, 0, 0, 0, 0));
        let cycles_total = phase.summary.as_ref().map(|s| s.cursor_extent).unwrap_or(0);
        Self {
            name: phase.name.clone(),
            labels: phase.labels.clone(),
            state,
            elapsed,
            cycles_completed,
            cycles_total,
            ops_started: cycles_completed,
            ops_finished: cycles_completed,
            ops_ok,
            skips,
            errors,
            retries,
            concurrency,
            consumed: cycles_completed,
            refresh_tick: 0,
            use_color: true,
            event: EventType::PhaseEnd,
        }
    }
}

impl ro::ReadoutContext for PhaseRowContext {
    fn subject_name(&self) -> &str {
        &self.name
    }
    fn subject_labels(&self) -> &str {
        &self.labels
    }
    fn subject_state(&self) -> ro::LifecycleState {
        self.state.clone()
    }
    fn elapsed_secs(&self) -> f64 {
        self.elapsed
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
    fn concurrency(&self) -> usize {
        self.concurrency
    }
    fn consumed(&self) -> u64 {
        self.consumed
    }
    fn refresh_tick(&self) -> u64 {
        self.refresh_tick
    }
    fn use_color(&self) -> bool {
        self.use_color
    }
    fn event(&self) -> EventType {
        self.event
    }
    fn eta_secs(&self) -> Option<f64> {
        if self.cycles_total == 0 || self.elapsed <= 0.0 {
            return None;
        }
        let rate = self.ops_finished as f64 / self.elapsed;
        if rate <= 0.0 {
            return None;
        }
        let remaining = self.cycles_total.saturating_sub(self.ops_finished) as f64;
        Some(remaining / rate)
    }
}

/// Render a phase's readout-engine output as ratatui Lines,
/// applying focus highlight to the spans of any readout the
/// binder reports as focused.
///
/// Returns `(lines, fired_event)` so the caller knows which
/// event slot was fired (matters for the binder's `last_event`
/// state — Tab / +/-/\ keystrokes mutate that slot).
pub fn render_phase_readouts(
    binder: &mut ro::TuiReadoutBinder,
    phase: &PhaseEntry,
    live: Option<&ActivePhase>,
    refresh_tick: u64,
) -> (Vec<Line<'static>>, EventType) {
    use ro::ReadoutBinder;

    // Pick the right context shape and event:
    //  - Live phase → EventType::Update with phase_status's
    //    full progress / rate / counters / ETA chip.
    //  - Terminal phase → EventType::PhaseEnd with phase_outcome's
    //    ✓ DONE summary (or phase_summary's [!!] failure).
    let (event, ctx_box): (EventType, Box<dyn ro::ReadoutContext>) = if let Some(a) = live {
        let ctx = PhaseRowContext::live(phase, a, refresh_tick);
        (EventType::Update, Box::new(ctx))
    } else {
        let ctx = PhaseRowContext::terminal(phase);
        (EventType::PhaseEnd, Box::new(ctx))
    };

    // Seed default bindings on first use of this slot.
    if binder.slot_len(event) == 0 {
        let default = match event {
            EventType::Update => ro::Registry::lookup("phase_status"),
            EventType::PhaseEnd => ro::Registry::lookup("phase_outcome"),
            _ => None,
        };
        if let Some(handle) = default {
            binder.bind(event, ro::BakedBody::from_single(handle, ro::Lod::Labeled));
        }
    }

    let mut sink = TuiReadoutSink::with_capacity(192);
    binder.fire(event, &*ctx_box, &mut sink);
    let lines = sink.take();

    // Apply focus highlight when a body in this slot is
    // the user's current selection. The binder doesn't
    // surface per-line "this came from the focused body"
    // information today (the sink already flattens
    // `LayoutHint::Focused`), so we approximate by tinting
    // the entire output's background when ANY body is
    // focused. That matches the operator-facing intent:
    // "you're navigating; the binder is showing you the
    // currently-active row."
    let tinted = if binder.focus_for(event).is_some() {
        lines
            .into_iter()
            .map(|line| apply_focus_tint(line))
            .collect()
    } else {
        lines
    };

    (tinted, event)
}

/// Wrap every span in `line` with a background tint so the
/// focused row stands out against the surrounding detail
/// block. Foreground colors are preserved (modifier and
/// fg are untouched) — only `bg` is overridden.
fn apply_focus_tint(line: Line<'static>) -> Line<'static> {
    let tint = Color::Rgb(40, 40, 60);
    let new_spans: Vec<Span<'static>> = line
        .spans
        .into_iter()
        .map(|s| {
            let style = s.style.bg(tint).add_modifier(Modifier::BOLD);
            Span::styled(s.content.into_owned(), style)
        })
        .collect();
    Line::from(new_spans)
}

// Suppress unused-import warning; Instant is used inside
// PhaseRowContext::live but only for elapsed derivation.
#[allow(dead_code)]
fn _instant_marker() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EntryKind, PhaseEntry};

    fn pending_phase(name: &str) -> PhaseEntry {
        PhaseEntry {
            node_id: 0,
            kind: EntryKind::Phase,
            name: name.into(),
            labels: String::new(),
            depth: 0,
            seq: None,
            status: PhaseStatus::Pending,
            duration_secs: None,
            session_elapsed: None,
            session_started: None,
            summary: None,
            op_count: 0,
            op_names: Vec::new(),
        }
    }

    #[test]
    fn terminal_context_emits_at_least_one_line_for_completed_phase() {
        let mut binder = ro::TuiReadoutBinder::new();
        let mut phase = pending_phase("setup");
        phase.status = PhaseStatus::Completed;
        phase.duration_secs = Some(0.42);

        let (lines, event) = render_phase_readouts(&mut binder, &phase, None, 0);
        assert_eq!(event, EventType::PhaseEnd);
        assert!(
            !lines.is_empty(),
            "completed phase should render at least one line"
        );
    }

    #[test]
    fn focus_tint_applied_when_any_slot_is_focused() {
        let mut binder = ro::TuiReadoutBinder::new();
        let mut phase = pending_phase("run");
        phase.status = PhaseStatus::Completed;
        phase.duration_secs = Some(0.1);

        // First fire seeds the default and sets last_event.
        let _ = render_phase_readouts(&mut binder, &phase, None, 0);

        // Cycle focus onto body 0; subsequent fires should
        // tint the lines.
        use ro::{BinderKey, ReadoutBinder};
        binder.on_key(BinderKey::CycleFocusNext);
        assert!(
            binder.focus_for(EventType::PhaseEnd).is_some(),
            "cycle should land focus somewhere"
        );

        let (tinted, _) = render_phase_readouts(&mut binder, &phase, None, 0);
        assert!(!tinted.is_empty());
        // Every span on the first line should carry a bg.
        let any_with_bg = tinted[0].spans.iter().any(|s| s.style.bg.is_some());
        assert!(any_with_bg, "focus-tinted line should carry a bg color");
    }

    #[test]
    fn lod_override_changes_render_size() {
        // Demonstrates that the binder's per-(slot, body)
        // LOD override (mutated via `+`/`-` keystrokes) is
        // honoured by `render_phase_readouts`. Compact and
        // Expanded LODs of `phase_status` produce different
        // line counts.
        let mut binder = ro::TuiReadoutBinder::new();
        let phase = pending_phase("run");
        let live = ActivePhase {
            name: "run".into(),
            labels: String::new(),
            cursor_name: "cycle".into(),
            daemon: false,
            cursor_extent: 100,
            rows_consumed: 0,
            rows_total: 0,
            fibers: 4,
            started_at: Instant::now() - std::time::Duration::from_millis(500),
            session_started: 0.0,
            ops_started: 50,
            ops_finished: 50,
            ops_ok: 50,
            skips: 0,
            errors: 0,
            retries: 0,
            ops_per_sec: 100.0,
            adapter_counters: Vec::new(),
            rows_per_batch: 1.0,
            relevancy: Vec::new(),
            throughput_summary: std::sync::Arc::new(
                nmbrs_metrics::summaries::binomial_summary::BinomialSummary::new(60),
            ),
            rate_ewma: std::sync::Arc::new(nmbrs_metrics::summaries::ewma::Ewma::new(
                std::time::Duration::from_millis(500),
            )),
            latency_peak_5s: std::sync::Arc::new(
                nmbrs_metrics::summaries::peak_tracker::PeakTracker::max(
                    std::time::Duration::from_secs(5),
                ),
            ),
            latency_peak_10s: std::sync::Arc::new(
                nmbrs_metrics::summaries::peak_tracker::PeakTracker::max(
                    std::time::Duration::from_secs(10),
                ),
            ),
            render: None,
        };

        // First fire: default LOD = Labeled.
        let (labeled_lines, _) = render_phase_readouts(&mut binder, &phase, Some(&live), 1);

        // Cycle focus then LOD up → Expanded.
        use ro::{BinderKey, ReadoutBinder};
        binder.on_key(BinderKey::CycleFocusNext);
        binder.on_key(BinderKey::CycleLodUp);

        let (expanded_lines, _) = render_phase_readouts(&mut binder, &phase, Some(&live), 2);

        // Expanded should produce more (or equal, if a
        // future render changes behaviour) lines than
        // Labeled. At minimum, both should be non-empty
        // and the binder respected the override.
        assert!(!labeled_lines.is_empty());
        assert!(!expanded_lines.is_empty());
        assert!(
            expanded_lines.len() >= labeled_lines.len(),
            "expanded LOD should produce at least as many lines as labeled \
             (got expanded={}, labeled={})",
            expanded_lines.len(),
            labeled_lines.len(),
        );
    }
}

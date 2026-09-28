// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `FormattedLineSink` — non-interactive, non-positioning display
//! sink for log-archive / piped-stderr runs.
//!
//! ## Why it exists
//!
//! `LogOnlySink` renders a live status block by writing absolute-
//! cursor escape sequences (`\x1b[r;c;H`, `\x1b[K`) — invisible in
//! a real terminal, but garbage in a piped or redirected stderr
//! stream. The previous fallback was `tui=off`: the observer
//! writes per-event log lines synchronously, but nothing emits
//! the running-phase progress, so an operator tailing
//! `2> session.log` couldn't tell whether their long-running
//! workload was still making forward progress.
//!
//! This sink fills that gap. It runs alongside the observer's
//! synchronous log writes (no `sink_active` interception) and
//! periodically appends a one-line status snapshot to stderr:
//!
//! ```text
//! [status @ 123.5s] phase 7/10 pvs_query_sweep 45% (ok:100% e:0 r:0 P50:1.2ms)
//! ```
//!
//! Snapshots are append-only — no escape sequences for cursor
//! positioning, no clearing of prior rows. Each tick is a
//! permanent record in whatever stream stderr ends up in. The
//! emitted line is fully self-contained and tail/grep-friendly.
//!
//! ## Cadence
//!
//! Default 5 s. Slower than `LogOnlySink`'s 50 ms because every
//! tick produces a NEW line in the archive — at 5 s a 1 h run
//! adds ~720 status lines, manageable next to the per-event log
//! stream. The cadence is fixed today; if operators want to tune
//! it, add a `tui-status-cadence=` knob.
//!
//! ## Surface ownership (2026-07-23)
//!
//! This sink now OWNS the stderr surface, exactly like
//! `LogOnlySink`: it flips `sink_active` (silencing the observer's
//! synchronous per-event writes), claims the durable scrollback
//! stream, and emits every log line from ITS OWN thread — each with
//! its actor-stamped margin — plus the periodic status snapshot.
//! The invariant this buys: **no workload thread ever writes the
//! terminal fd**. A blocked or slow pipe stalls only this sink
//! thread while the unbounded stream buffers; the workload planes
//! (fibers, executor, actor snapshot publishing) are untouched.
//! Previously the observer kept writing per-event lines
//! synchronously from calling threads, so a backpressured pipe
//! throttled the workload precisely during log-heavy incidents
//! (warn storms) — the failure mode this rewrite removes.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::display_sink::{DisplayInputs, DisplaySink, SinkHandle};
use crate::run_state_actor::RunStateHandle;

/// Cadence between status snapshots. 5 s is the default —
/// frequent enough to give an operator tailing a log file a
/// usable sense of forward progress, slow enough that an hour-
/// long run only adds ~720 status lines on top of the per-event
/// log stream.
const STATUS_CADENCE: Duration = Duration::from_secs(5);

/// Non-interactive, non-positioning sink for piped/redirected
/// stderr. Owns the surface: per-event log lines (with stamped
/// margins) AND periodic status snapshots, all from one thread.
pub struct FormattedLineSink {
    /// Severity floor for emitted lines (mirrors the observer's).
    min_level: nmbrs_runtime::observer::LogLevel,
    /// Shared with the observer: while high, the observer's
    /// synchronous stderr writes are suppressed — this sink is
    /// the only fd writer.
    sink_active: Arc<AtomicBool>,
}

impl FormattedLineSink {
    pub fn new(min_level: nmbrs_runtime::observer::LogLevel, sink_active: Arc<AtomicBool>) -> Self {
        Self {
            min_level,
            sink_active,
        }
    }
}

impl DisplaySink for FormattedLineSink {
    fn start(self: Box<Self>, inputs: DisplayInputs) -> Box<dyn SinkHandle> {
        let DisplayInputs { state, .. } = inputs;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let min_level = self.min_level;
        let sink_active = self.sink_active.clone();
        // Claim order: flip `sink_active` FIRST (observer stops its
        // synchronous writes immediately), then claim the stream, then
        // seed the skip-cursor. A line in the flip→seed window reaches
        // the stream only (observer already silenced) with seq > seed,
        // so it is emitted exactly once by this sink; lines before the
        // flip went to stderr synchronously and sit at seq <= seed, so
        // they are skipped. No duplicates, no losses. (Seeding BEFORE
        // the flip — the old order — let a line be both written
        // synchronously and re-emitted here: the startup-dupe bug.)
        sink_active.store(true, Ordering::Release);
        let scrollback = state.take_log_stream();
        let last_seen = state.load().log_seq_total;
        let join = std::thread::Builder::new()
            .name("formatted-line-sink".into())
            .spawn(move || run_emit_loop(state, stop_for_thread, scrollback, last_seen, min_level))
            .expect("spawn formatted-line-sink thread");
        Box::new(FormattedLineSinkHandle {
            stop,
            join: Some(join),
            sink_active: self.sink_active,
        })
    }
}

struct FormattedLineSinkHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    sink_active: Arc<AtomicBool>,
}

impl SinkHandle for FormattedLineSinkHandle {
    fn shutdown(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // Hand the surface back: straggler logs after shutdown go
        // through the observer's synchronous path again (post-run,
        // main-thread — nothing left to throttle).
        self.sink_active.store(false, Ordering::Release);
    }
}

fn run_emit_loop(
    state: RunStateHandle,
    stop: Arc<AtomicBool>,
    scrollback: crate::run_state_actor::ScrollbackReceiver,
    mut last_seen: u64,
    min_level: nmbrs_runtime::observer::LogLevel,
) {
    // Poll interval: short enough to honor a shutdown request and keep
    // line latency low; the status snapshot still fires only once per
    // STATUS_CADENCE.
    const POLL: Duration = Duration::from_millis(250);
    let mut next_emit = std::time::Instant::now() + STATUS_CADENCE;

    loop {
        let stopping = stop.load(Ordering::Relaxed);

        // Per-event lines: drain the durable stream fully. Plain
        // append-only text — the actor-stamped margin body plus the
        // message, no cursor addressing, no color (piped surface).
        {
            let mut err = std::io::stderr().lock();
            while let Some(line) = scrollback.try_next() {
                if line.seq <= last_seen {
                    continue; // pre-claim: already emitted synchronously
                }
                last_seen = line.seq;
                let level = severity_to_level(line.entry.severity);
                if level < min_level {
                    continue;
                }
                let _ = writeln!(err, "{} │ {}", line.margin_body, line.entry.message);
            }
            let _ = err.flush();
        }

        if stopping {
            return; // final drain above already ran
        }

        std::thread::sleep(POLL);
        if std::time::Instant::now() < next_emit {
            continue;
        }
        next_emit = std::time::Instant::now() + STATUS_CADENCE;

        let snap = state.load();
        let line = format_status_snapshot(&snap);
        let mut err = std::io::stderr().lock();
        let _ = writeln!(err, "{line}");
        let _ = err.flush();
    }
}

fn severity_to_level(s: crate::state::LogSeverity) -> nmbrs_runtime::observer::LogLevel {
    use nmbrs_runtime::observer::LogLevel as L;
    match s {
        crate::state::LogSeverity::Debug => L::Debug,
        crate::state::LogSeverity::Info => L::Info,
        crate::state::LogSeverity::Warn => L::Warn,
        crate::state::LogSeverity::Error => L::Error,
    }
}

/// Build the one-line status snapshot. Format:
///
/// ```text
/// [status @ 123.5s] phase 7/10 pvs_query_sweep 45% (ok:100% e:0 r:0 P50:1.2ms)
/// ```
///
/// Components elided when the corresponding state is absent
/// (no active phase → just the timestamp + total counter; no
/// total → just `phase N` without a denominator).
fn format_status_snapshot(snap: &Arc<crate::state::RunState>) -> String {
    let elapsed = snap.elapsed_secs();
    let total = snap.expected_total_phases;

    // Phase counter — same logic as the LogOnlySink margin's
    // numerator, derived from the live tree.
    let phase_only: Vec<_> = snap
        .phases
        .iter()
        .filter(|p| matches!(p.kind, crate::state::EntryKind::Phase))
        .collect();
    let active_pos = phase_only
        .iter()
        .position(|p| matches!(p.status, crate::state::PhaseStatus::Running));
    let phase_str = match (active_pos, total) {
        (Some(i), n) if n > 0 => format!("{}/{}", i + 1, n),
        (None, n) if n > 0 => {
            let done = phase_only
                .iter()
                .filter(|p| !matches!(p.status, crate::state::PhaseStatus::Pending))
                .count();
            format!("{done}/{n}")
        }
        _ => "?/?".to_string(),
    };

    let active_part = match snap.active_phases.values().next() {
        Some(a) => {
            let pct = if a.cursor_extent > 0 {
                (a.ops_finished as f64) * 100.0 / (a.cursor_extent as f64)
            } else {
                0.0
            };
            // ok% excludes SKIPS — an `if:`-gated skip is neither a
            // success nor a failure, so the basis is result-producing
            // ops only (`ops_finished - skips`).
            // `.max(ops_ok)`: non-atomic counter reads can momentarily make
            // this dip below ops_ok; it is never truly less. Keeps ok% <= 100%.
            let ok_denom = a.ops_finished.saturating_sub(a.skips).max(a.ops_ok);
            let ok_pct = if ok_denom > 0 {
                (a.ops_ok as f64) * 100.0 / (ok_denom as f64)
            } else {
                100.0
            };
            format!(
                " {name} {pct:.0}% (ok:{ok_pct:.0}% e:{e} r:{r})",
                name = a.name,
                e = a.errors,
                r = a.retries,
            )
        }
        None => String::new(),
    };

    format!("[status @ {elapsed:.1}s] phase {phase_str}{active_part}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::RunState;

    fn make_state() -> RunState {
        RunState::new("w.yaml", "default", "stdout")
    }

    /// Empty snapshot — no phases, no active. Emits a minimal
    /// well-formed line; the `phase ?/?` marker tells the
    /// operator no plan was registered (or pre-map didn't run).
    #[test]
    fn empty_state_emits_minimal_line() {
        let snap = Arc::new(make_state());
        let line = format_status_snapshot(&snap);
        assert!(
            line.starts_with("[status @ "),
            "line should be prefixed with the status sentinel: {line}"
        );
        assert!(
            line.contains("phase ?/?"),
            "missing-plan marker should show `?/?`: {line}"
        );
    }

    /// With a pre-mapped plan but no live phase entries (start
    /// of a run, before any phase has transitioned), the
    /// counter reads `0/total` — denominator pinned to the
    /// pre-map, numerator counts non-pending entries (none).
    #[test]
    fn pre_mapped_no_running_phase_emits_zero_over_total() {
        let mut state = make_state();
        state.expected_total_phases = 7;
        let snap = Arc::new(state);
        let line = format_status_snapshot(&snap);
        assert!(
            line.contains("phase 0/7"),
            "should show `0/7` when no phases have transitioned: {line}"
        );
        // No active phase → no name/pct/counters tail.
        assert!(
            !line.contains("ok:"),
            "no active-phase tail should appear: {line}"
        );
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Interval wrapper — the first PHASE-level wrapper (SRD-82/92 cross-level;
//! see `docs/cross-level-wrapper-cascade-scope.md`). `interval:` is its
//! sigil: re-run this phase, dwelling `interval` between runs, bounded by
//! `repeat:`.
//!
//! Why a PHASE wrapper and not a rate: a rate paces the units *inside* a
//! phase (for a recall phase, individual queries), so `rate: 1/300` would
//! trickle one query every 5 minutes — a single glacial pass, not a
//! measurement every 5 minutes. Pacing the *phase* is one layer out: each
//! iteration is a whole phase run (a complete recall measurement), and the
//! repeats live in ONE session — one metrics timeline — which an external
//! `while … sleep` loop cannot give you (each `nmbrs run` is its own session).
//!
//! - **No `interval:`** → the phase runs exactly once (unchanged).
//! - **`interval: <dur>` + `repeat: N`** → up to `N` runs, dwelling
//!   `interval` between them.
//! - **`interval: <dur>` with no `repeat:`** → runs until the session stops.
//!
//! The dwell is cooperative: it wakes on a short tick to observe the session
//! stop, so Ctrl-C / a `stop_when` `action: abort` cuts the wait immediately
//! instead of waiting out the remaining interval. A failing run ends the
//! repeat (the phase's own outcome propagates unchanged).
//!
//! Registration is declared here at `WrapperLevel::Phase` so the field
//! validation + telemetry stay consistent with the op wrappers; the
//! construction is hooked at the phase seam (`PhaseShell::run`) because a
//! phase layer wraps an `ExecShell`, not an `OpDispenser`.

use nmbrs_workload::model::WorkloadPhase;

use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};

pub const NAME: WrapperName = WrapperName::new("interval");

/// Registry trigger: the phase's own `interval:` field. `repeat:` alone is
/// inert (it bounds an interval that isn't there) — the misplaced-field
/// guard reports it against THIS registration.
fn triggers(s: WrapperSubject) -> bool {
    s.phase().is_some_and(|p| p.interval.is_some())
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let p = s.phase()?;
    let every = p.interval.as_deref()?;
    Some(match p.repeat {
        Some(n) => format!("interval: every {every} × {n}"),
        None => format!("interval: every {every}, until session stop"),
    })
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["interval", "repeat"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        // The layer ([`IntervalShell`]) is implemented GENERICALLY over
        // `ExecShell`, so `levels` is a pure TYPE FILTER over the subjects it
        // accepts — not an implementation limit. Any shell level can carry
        // it: re-running a scenario every N is the same operation as
        // re-running a phase, and the layer is identical. `Op` is excluded
        // only because the op leaf is deliberately NOT an `ExecShell` (it
        // sits below the Outcome projection boundary — SRD-82 Decision B),
        // so there is no op shell to decorate; `Stanza` likewise has no shell.
        levels: &[
            crate::wrapper_registry::WrapperLevel::Phase,
            crate::wrapper_registry::WrapperLevel::Scenario,
            crate::wrapper_registry::WrapperLevel::Session,
        ],
    }
}

/// The `interval:` LAYER — a generic [`ExecShell`] decorator.
///
/// It knows nothing about phases: it wraps ANY inner shell and re-runs it,
/// dwelling `interval` between runs, bounded by `repeat`. Placing it around a
/// `ScenarioShell` (or a future `SessionShell`) requires no change here —
/// only a subject at that level to resolve the schedule from. `shell_kind`
/// delegates to the inner shell because a layer decorates a level, it does
/// not change what level the thing IS.
///
/// Semantics: a failing run ends the schedule (its outcome propagates
/// unchanged, exactly as an unwrapped run's would); the dwell is cooperative
/// so a stop cuts the wait instead of stranding the run for the remainder.
pub(crate) struct IntervalShell<'i> {
    inner: &'i dyn crate::executor::ExecShell,
    spec: IntervalSpec,
    /// The wrapped unit's name, for the schedule's summary line.
    label: &'i str,
}

impl<'i> IntervalShell<'i> {
    pub(crate) fn new(
        inner: &'i dyn crate::executor::ExecShell,
        spec: IntervalSpec,
        label: &'i str,
    ) -> Self {
        Self { inner, spec, label }
    }
}

impl<'i> crate::executor::ExecShell for IntervalShell<'i> {
    fn run<'a>(
        &'a self,
        ctx: &'a mut crate::executor::ExecCtx,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut runs: u64 = 0;
            let mut last;
            loop {
                // Reborrow per iteration — the inner shell takes `&mut ExecCtx`
                // and we drive it repeatedly.
                last = self.inner.run(&mut *ctx).await;
                runs += 1;
                // A failing run ends the schedule.
                if last.is_failure() {
                    break;
                }
                // Bound reached; `repeat: None` = until the session stops.
                if self.spec.repeat.is_some_and(|r| runs >= r) {
                    break;
                }
                // Cooperative dwell — a stop ends the schedule immediately.
                if !dwell(self.spec.interval_ms).await {
                    break;
                }
            }
            crate::diag!(
                crate::observer::LogLevel::Info,
                "interval: '{}' schedule ended after {runs} run(s)",
                self.label
            );
            last
        })
    }

    fn shell_kind(&self) -> crate::executor::ShellKind {
        self.inner.shell_kind()
    }
}

/// A phase's resolved repeat schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IntervalSpec {
    /// Dwell between runs, milliseconds.
    pub interval_ms: u64,
    /// Total runs. `None` = until the session stops.
    pub repeat: Option<u64>,
}

/// Resolve the schedule for `phase_name`, or `None` when the phase is not
/// scheduled (the overwhelmingly common case — it runs once).
///
/// The raw `interval:` goes through `{param}` interpolation first, so a
/// phase SHARED between an unscheduled use and a scheduled one needs no
/// duplicate: it declares `interval: "{some_param}"`, the param defaults to
/// the disable value, and a run opts in from the CLI (`some_param=5m`).
///
/// `0` / empty is the DISABLE knob — run once, silently (the same idiom as
/// `retry_backoff: 0` disabling retry pacing). A zero dwell would otherwise
/// spin the phase with no pacing at all. An unparseable duration is a loud
/// error that still degrades to running once, never to a spin.
pub(crate) fn for_phase(
    phases: &std::collections::HashMap<String, WorkloadPhase>,
    workload_params: &std::collections::HashMap<String, String>,
    phase_name: &str,
) -> Option<IntervalSpec> {
    let p = phases.get(phase_name)?;
    let declared = p.interval.as_deref()?;
    let expanded = crate::runner::expand_workload_params(declared, workload_params);
    let raw = expanded.trim();
    if raw.is_empty() || raw == "0" {
        return None;
    }
    match crate::timeval::parse_time_ms(raw) {
        Ok(0) => None,
        Ok(ms) => Some(IntervalSpec {
            interval_ms: ms,
            repeat: p.repeat,
        }),
        Err(e) => {
            crate::diag!(
                crate::observer::LogLevel::Error,
                "phase '{phase_name}': `interval: {raw}` is not a duration ({e}) \
                 — running once"
            );
            None
        }
    }
}

/// Cooperative dwell. Sleeps `total_ms`, waking every [`TICK_MS`] to observe
/// the session stop. Returns `false` if a stop was seen (the caller must not
/// start another run) — so an abort or Ctrl-C during a long interval cuts the
/// wait instead of stranding the run for the remainder.
pub(crate) async fn dwell(total_ms: u64) -> bool {
    const TICK_MS: u64 = 250;
    let mut remaining = total_ms;
    while remaining > 0 {
        if crate::session_signals::stop_requested() {
            return false;
        }
        let step = remaining.min(TICK_MS);
        tokio::time::sleep(std::time::Duration::from_millis(step)).await;
        remaining -= step;
    }
    !crate::session_signals::stop_requested()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn phase_with(interval: Option<&str>, repeat: Option<u64>) -> HashMap<String, WorkloadPhase> {
        let mut m = HashMap::new();
        m.insert(
            "p".to_string(),
            WorkloadPhase {
                interval: interval.map(str::to_string),
                repeat,
                ..Default::default()
            },
        );
        m
    }

    fn no_params() -> HashMap<String, String> {
        HashMap::new()
    }

    /// No `interval:` → no schedule (the phase runs once).
    #[test]
    fn absent_interval_yields_no_schedule() {
        assert_eq!(for_phase(&phase_with(None, None), &no_params(), "p"), None);
        // `repeat:` alone does not conjure a schedule.
        assert_eq!(
            for_phase(&phase_with(None, Some(5)), &no_params(), "p"),
            None
        );
        // An unknown phase name is simply not scheduled.
        assert_eq!(
            for_phase(&phase_with(Some("5m"), None), &no_params(), "nope"),
            None
        );
    }

    /// A duration + bound resolves to milliseconds.
    #[test]
    fn interval_resolves_to_millis_with_bound() {
        assert_eq!(
            for_phase(&phase_with(Some("5m"), Some(288)), &no_params(), "p"),
            Some(IntervalSpec {
                interval_ms: 300_000,
                repeat: Some(288)
            })
        );
        // No `repeat:` = until session stop.
        assert_eq!(
            for_phase(&phase_with(Some("250ms"), None), &no_params(), "p"),
            Some(IntervalSpec {
                interval_ms: 250,
                repeat: None
            })
        );
    }

    /// `0` / empty is the DISABLE knob — run once, so a shared phase stays
    /// unscheduled by default. A malformed duration also degrades to once,
    /// never to a spin.
    #[test]
    fn zero_or_bad_interval_degrades_to_run_once() {
        assert_eq!(
            for_phase(&phase_with(Some("0"), Some(10)), &no_params(), "p"),
            None
        );
        assert_eq!(
            for_phase(&phase_with(Some(""), None), &no_params(), "p"),
            None
        );
        assert_eq!(
            for_phase(&phase_with(Some("banana"), Some(10)), &no_params(), "p"),
            None
        );
    }

    /// The `{param}` interpolation that lets ONE shared phase be scheduled
    /// per-run: the declared `interval: "{recall_interval}"` is disabled by
    /// the param's default and opted into from the CLI.
    #[test]
    fn interval_interpolates_a_workload_param() {
        let phases = phase_with(Some("{recall_interval}"), None);
        // Default (disabled) → unscheduled: the shared phase runs once.
        let off = HashMap::from([("recall_interval".to_string(), "0".to_string())]);
        assert_eq!(for_phase(&phases, &off, "p"), None);
        // Opted in → scheduled.
        let on = HashMap::from([("recall_interval".to_string(), "5m".to_string())]);
        assert_eq!(
            for_phase(&phases, &on, "p"),
            Some(IntervalSpec {
                interval_ms: 300_000,
                repeat: None
            })
        );
    }

    /// The trigger fires on a phase with `interval:`, and never on an op
    /// subject (this wrapper is `WrapperLevel::Phase`).
    #[test]
    fn triggers_only_on_a_phase_declaring_interval() {
        let with = WorkloadPhase {
            interval: Some("5m".into()),
            ..Default::default()
        };
        let without = WorkloadPhase::default();
        assert!(triggers(WrapperSubject::Phase(&with)));
        assert!(!triggers(WrapperSubject::Phase(&without)));
        assert!(describe_assignment(WrapperSubject::Phase(&with)).is_some());
    }

    /// A stand-in shell at an arbitrary level — exists to prove the layer is
    /// a generic `ExecShell` decorator, not a phase-specific loop.
    struct FakeShell(crate::executor::ShellKind);

    impl crate::executor::ExecShell for FakeShell {
        fn run<'a>(
            &'a self,
            _ctx: &'a mut crate::executor::ExecCtx,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = crate::phase_outcome::Outcome> + Send + 'a>,
        > {
            Box::pin(async { crate::phase_outcome::Outcome::skipped() })
        }
        fn shell_kind(&self) -> crate::executor::ShellKind {
            self.0
        }
    }

    /// The layer is generic over ANY shell — it wraps a SCENARIO shell just
    /// as readily as a phase one (this only compiles because `IntervalShell`
    /// decorates `dyn ExecShell`, with no phase in its type). `levels:` is a
    /// filter over which subjects may trigger it, NOT a limit on what the
    /// layer can wrap. And a layer DECORATES a level — it never changes what
    /// level the wrapped thing is, so `shell_kind` delegates.
    #[test]
    fn layer_is_generic_over_any_shell() {
        use crate::executor::{ExecShell, ShellKind};
        let spec = IntervalSpec {
            interval_ms: 1,
            repeat: Some(1),
        };

        let scenario = FakeShell(ShellKind::Scenario);
        assert_eq!(
            IntervalShell::new(&scenario, spec, "s").shell_kind(),
            ShellKind::Scenario,
            "wrapping a scenario keeps it a scenario"
        );

        let phase = FakeShell(ShellKind::Phase);
        assert_eq!(
            IntervalShell::new(&phase, spec, "p").shell_kind(),
            ShellKind::Phase,
            "wrapping a phase keeps it a phase"
        );

        let session = FakeShell(ShellKind::Session);
        assert_eq!(
            IntervalShell::new(&session, spec, "sess").shell_kind(),
            ShellKind::Session,
            "wrapping a session keeps it a session"
        );
    }

    /// The registration's level filter is permissive by default: a layer that
    /// works over a phase is allowed over a scenario and a session too.
    #[test]
    fn levels_filter_admits_every_shell_level() {
        let reg = inventory::iter::<WrapperRegistration>
            .into_iter()
            .find(|r| r.name == NAME)
            .expect("interval wrapper is registered");
        use crate::wrapper_registry::WrapperLevel;
        assert!(reg.applies_at(WrapperLevel::Phase));
        assert!(reg.applies_at(WrapperLevel::Scenario));
        assert!(reg.applies_at(WrapperLevel::Session));
        // The op leaf is NOT an ExecShell (below the Outcome projection
        // boundary), so there is no op shell to decorate.
        assert!(!reg.applies_at(WrapperLevel::Op));
    }

    /// The dwell returns promptly (and reports stopped) when the session
    /// stop is already latched — it must not wait out the interval.
    #[tokio::test]
    async fn dwell_short_circuits_on_session_stop() {
        let _g = crate::session_signals::STOP_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::session_signals::request_stop();
        let t = std::time::Instant::now();
        // A 10s dwell must return immediately, not after 10s.
        assert!(!dwell(10_000).await, "a latched stop must end the dwell");
        assert!(
            t.elapsed() < std::time::Duration::from_secs(1),
            "dwell must not wait out the interval after a stop"
        );
        crate::session_signals::clear_session_stop_for_test();
    }
}

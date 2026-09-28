// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 §"Settling" — a cadence-pulse phase evaluator.
//!
//! A [`PhaseStopEvaluator`] is a callback registered on the **metrics
//! cadence feed** ([`nmbrs_metrics::cadence_reporter::CadenceReporter::subscribe`],
//! SRD-42). It runs once per **cadence pulse** against the running
//! phase's current state + metrics, and at any pulse may set a
//! **terminal disposition** on the phase — stopping it (cooperatively,
//! by raising the phase stop flag, which the activity loop reads at its
//! next cycle boundary) and recording *which* [`Outcome`] disposition it
//! stopped with. Once it has set a terminal disposition it
//! **unregisters itself**: it reports [`finished`](Reporter::finished),
//! so the cadence-feed dispatch worker stops delivering further pulses
//! (no self-join deadlock — the evaluator runs on that worker thread).
//!
//! The per-pulse decision is a [`PulseEvaluator`]: it returns
//! `Some(outcome)` to stop the phase with that disposition, or `None` to
//! let it keep running. The settle detector
//! ([`super::settle::SettleEvaluator`]) is the first implementation;
//! the mechanism is general (any cadence-driven phase-stop policy).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;

use crate::phase_outcome::Outcome;

/// Per-pulse evaluation of a running phase against the just-published
/// cadence window. Returns `Some(outcome)` to terminally stop the phase
/// with that disposition, or `None` to let it keep running.
///
/// Implementors read the phase's live state however they need — the
/// `window` is the freshly-closed [`MetricSet`] that triggered this
/// pulse (a metrics reader may instead re-read the live feed, which
/// resolves to the same window the cadence feed just published).
pub trait PulseEvaluator: Send {
    fn evaluate(&mut self, window: &MetricSet) -> Option<Outcome>;
}

/// The terminal disposition a [`PhaseStopEvaluator`] publishes. The
/// executor reads it at phase completion: `None` ⇒ the evaluator never
/// fired (the phase ran to its natural end); `Some(outcome)` ⇒ the
/// evaluator stopped the phase with that disposition.
pub type StopOutcomeCell = Arc<ArcSwap<Option<Outcome>>>;

/// A cadence-feed callback that drives a [`PulseEvaluator`] against a
/// running phase and terminally stops it on the first verdict. See the
/// module docs.
pub struct PhaseStopEvaluator {
    eval: Box<dyn PulseEvaluator>,
    stop_flag: Arc<AtomicBool>,
    outcome: StopOutcomeCell,
    done: bool,
}

impl PhaseStopEvaluator {
    /// Build an evaluator over the phase's `stop_flag` (raised
    /// cooperatively when the evaluator yields a terminal disposition).
    pub fn new(eval: Box<dyn PulseEvaluator>, stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            eval,
            stop_flag,
            outcome: Arc::new(ArcSwap::from_pointee(None)),
            done: false,
        }
    }

    /// The cell the executor reads at phase completion for the terminal
    /// disposition. Shared (cloned `Arc`) so the executor holds a handle
    /// while the evaluator runs on the cadence-feed worker.
    pub fn outcome_cell(&self) -> StopOutcomeCell {
        self.outcome.clone()
    }
}

impl Reporter for PhaseStopEvaluator {
    fn report(&mut self, window: &MetricSet) {
        if self.done {
            return;
        }
        // The delivery fiber already runs inside this subscription's
        // execution context (SRD-88 — set as the cadence subscription's
        // `context_wrap` at subscribe time), so the objective read here
        // scopes to the owning execution without any per-call rebinding.
        if let Some(outcome) = self.eval.evaluate(window) {
            self.outcome.store(Arc::new(Some(outcome)));
            // Cooperative stop: the activity loop reads the flag at its
            // next cycle boundary.
            self.stop_flag.store(true, Ordering::Relaxed);
            self.done = true;
        }
    }

    fn finished(&self) -> bool {
        self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_outcome::{Disposition, Validity};
    use std::time::Duration;

    /// An evaluator that fires a fixed outcome on the Nth pulse.
    struct FireOnNth {
        n: u64,
        seen: u64,
        outcome: Outcome,
    }
    impl PulseEvaluator for FireOnNth {
        fn evaluate(&mut self, _w: &MetricSet) -> Option<Outcome> {
            self.seen += 1;
            (self.seen >= self.n).then(|| self.outcome.clone())
        }
    }

    fn empty_window() -> MetricSet {
        MetricSet::new(Duration::from_secs(1))
    }

    #[test]
    fn fires_once_then_reports_finished_and_no_ops() {
        let stop = Arc::new(AtomicBool::new(false));
        let mut ev = PhaseStopEvaluator::new(
            Box::new(FireOnNth {
                n: 3,
                seen: 0,
                outcome: Outcome::interrupted(),
            }),
            stop.clone(),
        );
        let cell = ev.outcome_cell();

        ev.report(&empty_window());
        ev.report(&empty_window());
        assert!(!stop.load(Ordering::Relaxed), "no stop before the verdict");
        assert!(!ev.finished(), "not finished before the verdict");
        assert!(cell.load().is_none(), "no outcome before the verdict");

        ev.report(&empty_window()); // third pulse fires
        assert!(
            stop.load(Ordering::Relaxed),
            "stop flag raised on the verdict"
        );
        assert!(ev.finished(), "self-unregisters after the verdict");
        let got = (**cell.load()).clone().expect("outcome published");
        assert_eq!(got.disposition, Disposition::Interrupted);
        assert_eq!(got.validity, Validity::Succeeded);

        // Further pulses are no-ops — the disposition is not overwritten.
        ev.report(&empty_window());
        assert_eq!(
            (**cell.load()).clone().expect("still set").disposition,
            Disposition::Interrupted
        );
    }

    #[test]
    fn timeout_disposition_differs_from_settle() {
        // A failed() verdict (timeout) publishes Interrupted+Failed.
        let stop = Arc::new(AtomicBool::new(false));
        let mut ev = PhaseStopEvaluator::new(
            Box::new(FireOnNth {
                n: 1,
                seen: 0,
                outcome: Outcome::failed(),
            }),
            stop.clone(),
        );
        let cell = ev.outcome_cell();
        ev.report(&empty_window());
        let got = (**cell.load()).clone().expect("outcome published");
        assert_eq!(got.disposition, Disposition::Interrupted);
        assert_eq!(
            got.validity,
            Validity::Failed,
            "timeout is the untrustworthy quadrant"
        );
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Structured exec-event exemplars — the wrapper-facing tap onto the
//! system's structured event sink (the observer surface, SRD-81/-88).
//!
//! Wrappers OPT IN by implementing [`ExecEventSubscriber`], a decorator
//! service in the established wrapper style (a dyn-safe trait whose
//! default methods ARE the service — the same shape as
//! `WrappingDispenser`): implementing it grants the wrapper the
//! canonical submission surface, and nothing else changes about the
//! wrapper's construction or registration. The default routing is the
//! single chokepoint [`submit_exemplar`]: one rendered projection per
//! event through [`crate::observer::log_tagged`], which fans out
//! to every installed sink — the durable `session.log` always, the
//! live display per its level gates. No wrapper hand-rolls its own
//! event formatting or reaches for a sink directly.
//!
//! # Exemplars, not streams
//!
//! An [`ExecExemplar`] is a SAMPLED counter-exemplar: a concrete
//! specimen of an error class that is otherwise visible only as a
//! counter (e.g. `attempt_failure` inside the retry loop, where the
//! error policy never sees the message because the attempt recovers).
//! Sampling is the submitting wrapper's job via [`ExemplarSampler`]:
//! a fraction (`rate`, default 0.0 = off) decides which caught errors
//! become exemplars, and a frequency ceiling (`max_hz`) squelches
//! bursts. Squelched admissions are COUNTED, never dropped silently:
//! the next emitted exemplar carries `(+N squelched)`, and any
//! leftover tally is flushed at Debug when the sampler drops.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// One sampled error specimen from an execution wrapper.
pub struct ExecExemplar<'a> {
    /// Op template name the error occurred under.
    pub op_name: &'a str,
    /// Cycle whose attempt produced the error.
    pub cycle: u64,
    /// 1-based attempt number that failed.
    pub attempt_no: u32,
    /// The op's total-attempts budget.
    pub tries_budget: u32,
    /// Adapter error class (`error_name`).
    pub error_class: &'a str,
    /// Full adapter error message.
    pub message: &'a str,
    /// True when the failed attempt will be retried (the class of
    /// error that is otherwise invisible outside counters).
    pub will_retry: bool,
    /// Admissions squelched by the frequency ceiling since the last
    /// emitted exemplar — carried on this line so the squelch is
    /// visible, never silent.
    pub squelched_since_last: u64,
}

/// Render an exemplar to its one-line session projection. Pure —
/// separated from [`submit_exemplar`] so the format is testable
/// without an observer.
pub fn render_exemplar(ex: &ExecExemplar<'_>) -> String {
    let retry_note = if ex.will_retry {
        "retrying"
    } else {
        "terminal"
    };
    let squelch_note = if ex.squelched_since_last > 0 {
        format!(" (+{} squelched)", ex.squelched_since_last)
    } else {
        String::new()
    };
    format!(
        "retry exemplar: op '{}' attempt {}/{} cycle {} ({retry_note}): \
         [{}] {}{squelch_note}",
        ex.op_name, ex.attempt_no, ex.tries_budget, ex.cycle, ex.error_class, ex.message,
    )
}

/// The canonical submission chokepoint: one projection through the
/// observer's categorized log surface at Warn (an exemplar IS an
/// error specimen the operator asked to see).
pub fn submit_exemplar(ex: &ExecExemplar<'_>) {
    crate::observer::log_tagged(
        crate::observer::LogLevel::Warn,
        crate::observer::EventTag::in_flight(crate::observer::EventCategory::Retry),
        &render_exemplar(ex),
    );
}

/// Decorator service: a wrapper subscribes to the structured event
/// sink by implementing this trait (dyn-safe; default methods are
/// the whole service). Override nothing to get the canonical
/// routing; the trait exists so the subscription is a declared,
/// greppable property of the wrapper type rather than an ad-hoc
/// call into logging.
pub trait ExecEventSubscriber {
    /// Submit one sampled exemplar to the structured sink.
    fn submit_exemplar(&self, ex: &ExecExemplar<'_>) {
        submit_exemplar(ex)
    }

    /// Submit one first-sighting retry advisory (default-on signal;
    /// the per-phase [`AdvisoryGate`] bounds it).
    fn submit_advisory(
        &self,
        op_name: &str,
        cycle: u64,
        tries_budget: u32,
        error_class: &str,
        message: &str,
    ) {
        crate::observer::log_tagged(
            crate::observer::LogLevel::Warn,
            crate::observer::EventTag::in_flight(crate::observer::EventCategory::Retry),
            &render_advisory(op_name, cycle, tries_budget, error_class, message),
        );
    }
}

/// Render a first-sighting retry advisory. Pure — testable without
/// an observer.
pub fn render_advisory(
    op_name: &str,
    cycle: u64,
    tries_budget: u32,
    error_class: &str,
    message: &str,
) -> String {
    format!(
        "retry advisory: op '{op_name}' hit its first retryable \
         [{error_class}] at cycle {cycle}: {message} — further \
         occurrences are absorbed by the tries budget ({tries_budget}) \
         and appear only as att:%/r: chips and attempt_* metrics; \
         sample live specimens via the retry_exemplar_rate control, \
         or silence this line with retry_advisory: off"
    )
}

/// Per-phase advisory gate: by DEFAULT (no exemplar sampling opted
/// in) the operator still gets at least SOME signal when the retry
/// loop starts absorbing errors — one advisory per error class per
/// phase, capped, so a retry storm identifies itself without
/// flooding the session output. Shared per activity (like
/// [`ExemplarConfig`]) so many ops in one phase share the budget.
pub struct AdvisoryGate {
    seen: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Max distinct classes advised per phase; beyond it the gate
    /// closes (the classes are countable in `errors_total` labels).
    cap: usize,
}

impl Default for AdvisoryGate {
    fn default() -> Self {
        Self::new()
    }
}

impl AdvisoryGate {
    pub fn new() -> Self {
        Self {
            seen: std::sync::Mutex::new(std::collections::HashSet::new()),
            cap: 3,
        }
    }

    /// True exactly once per error class (under the cap) — the
    /// caller emits the advisory for that sighting.
    pub fn first_sighting(&self, class: &str) -> bool {
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= self.cap && !seen.contains(class) {
            return false;
        }
        seen.insert(class.to_string())
    }
}

/// splitmix64 — cheap deterministic hash shared by replayable
/// sampling decisions (and the tries wrapper's backoff jitter): the
/// same (cycle, attempt) always makes the same choice, so a replay
/// reproduces the same exemplars.
pub(crate) fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The sampling configuration cell: rate and frequency ceiling as
/// shared atomics, so a dynamic control can move every sampler
/// reading the cell with ONE store — push-on-set, no polling, no
/// per-op control traffic (the `cql_trace_rate` pattern). Readers
/// pay one atomic load, and only on the retry path.
///
/// Scoping: the activity owns one shared cell for every tries
/// wrapper that does not pin its own values; an op that declares
/// `retry_exemplar_*` params gets a private cell the controls
/// deliberately do not move (authored matter wins).
pub struct ExemplarConfig {
    /// f64 bits of the sampling fraction ∈ [0, 1]. 0.0 = off.
    rate_bits: AtomicU64,
    /// Minimum nanos between emissions (derived from `max_hz` at
    /// SET time so the read path never divides). 0 = no ceiling.
    min_interval_nanos: AtomicU64,
}

impl ExemplarConfig {
    pub fn new(rate: f64, max_hz: f64) -> Self {
        let cfg = Self {
            rate_bits: AtomicU64::new(0),
            min_interval_nanos: AtomicU64::new(0),
        };
        cfg.set_rate(rate);
        cfg.set_max_hz(max_hz);
        cfg
    }

    /// Publish a new sampling fraction (clamped to [0, 1];
    /// non-finite = off). One atomic store — this IS the dynamic
    /// control's applier body.
    pub fn set_rate(&self, rate: f64) {
        let rate = if rate.is_finite() {
            rate.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.rate_bits.store(rate.to_bits(), Ordering::Release);
    }

    /// Publish a new frequency ceiling (events/sec; `0` or
    /// non-finite = uncapped). Converted to an interval here so
    /// admission never divides.
    pub fn set_max_hz(&self, max_hz: f64) {
        let interval = if max_hz.is_finite() && max_hz > 0.0 {
            (1_000_000_000f64 / max_hz) as u64
        } else {
            0
        };
        self.min_interval_nanos.store(interval, Ordering::Release);
    }

    fn rate(&self) -> f64 {
        f64::from_bits(self.rate_bits.load(Ordering::Acquire))
    }

    fn min_interval(&self) -> u64 {
        self.min_interval_nanos.load(Ordering::Acquire)
    }
}

/// Sampling + squelch gate for exemplar submission.
///
/// Two independent controls compose (read live from the
/// [`ExemplarConfig`] cell, so a dynamic control moves them
/// mid-run):
/// - `rate` ∈ [0, 1] — the fraction of caught errors that become
///   exemplar candidates. `0.0` (the default) disables the sampler
///   entirely; the caller's hot path pays one atomic load.
///   The roll is DETERMINISTIC on (cycle, attempt) so runs replay.
/// - `max_hz` — ceiling on emitted exemplars per second. Candidates
///   over the ceiling are squelched and COUNTED; the count drains
///   onto the next admitted exemplar, and any leftover flushes at
///   Debug on drop. `0` or non-finite = no ceiling.
pub struct ExemplarSampler {
    cfg: std::sync::Arc<ExemplarConfig>,
    base: Instant,
    /// Elapsed nanos (since `base`) of the last admitted exemplar,
    /// +1 so that 0 means "never admitted".
    last_admit: AtomicU64,
    squelched: AtomicU64,
}

impl ExemplarSampler {
    /// A sampler over its own private cell — the authored-pin form
    /// (op-level `retry_exemplar_*` params); dynamic controls do
    /// not move it.
    pub fn pinned(rate: f64, max_hz: f64) -> Self {
        Self::shared(std::sync::Arc::new(ExemplarConfig::new(rate, max_hz)))
    }

    /// A sampler over a shared cell — the default form; the cell's
    /// owner (the activity) wires it to the `retry_exemplar_rate` /
    /// `retry_exemplar_max_hz` dynamic controls.
    pub fn shared(cfg: std::sync::Arc<ExemplarConfig>) -> Self {
        Self {
            cfg,
            base: Instant::now(),
            last_admit: AtomicU64::new(0),
            squelched: AtomicU64::new(0),
        }
    }

    /// True when any sampling can happen at all — one atomic load,
    /// paid only on the retry path.
    pub fn enabled(&self) -> bool {
        self.cfg.rate() > 0.0
    }

    /// Decide one caught error. `None` = not sampled (failed the
    /// roll, or over the frequency ceiling — the latter counted).
    /// `Some(n)` = admitted, draining `n` squelched admissions to
    /// report on this exemplar's line.
    pub fn admit(&self, cycle: u64, attempt_no: u32) -> Option<u64> {
        let rate = self.cfg.rate();
        if rate <= 0.0 {
            return None;
        }
        // Deterministic roll on (cycle, attempt) — replayable, and
        // uniform enough for a sampling fraction.
        let h = splitmix64(cycle ^ ((attempt_no as u64) << 48) ^ 0xE0E0_5EED);
        if (h as f64 / u64::MAX as f64) >= rate {
            return None;
        }
        let min_interval = self.cfg.min_interval();
        if min_interval == 0 {
            // Uncapped — but still stamp the gate so a ceiling
            // applied LIVE measures from real emission history
            // rather than treating the next admission as first.
            let now = self.base.elapsed().as_nanos() as u64 + 1;
            self.last_admit.store(now, Ordering::Release);
            return Some(self.squelched.swap(0, Ordering::AcqRel));
        }
        let now = self.base.elapsed().as_nanos() as u64 + 1;
        loop {
            let last = self.last_admit.load(Ordering::Acquire);
            if last != 0 && now.saturating_sub(last) < min_interval {
                self.squelched.fetch_add(1, Ordering::AcqRel);
                return None;
            }
            if self
                .last_admit
                .compare_exchange(last, now, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(self.squelched.swap(0, Ordering::AcqRel));
            }
        }
    }
}

impl Drop for ExemplarSampler {
    fn drop(&mut self) {
        // Leftover squelch tally: surfaced, never silently lost.
        let leftover = self.squelched.load(Ordering::Acquire);
        if leftover > 0 {
            crate::diag!(
                crate::observer::LogLevel::Debug,
                "exemplar sampler retired with {leftover} squelched \
                 admission(s) unreported (frequency ceiling)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rate: 0.0` (the default) admits nothing and stays cheap.
    #[test]
    fn zero_rate_is_off() {
        let s = ExemplarSampler::pinned(0.0, 1000.0);
        assert!(!s.enabled());
        for c in 0..1000 {
            assert!(s.admit(c, 1).is_none());
        }
    }

    /// `rate: 1.0` with no ceiling admits every caught error.
    #[test]
    fn full_rate_uncapped_admits_all() {
        let s = ExemplarSampler::pinned(1.0, 0.0);
        for c in 0..100 {
            assert_eq!(s.admit(c, 1), Some(0), "cycle {c}");
        }
    }

    /// A fractional rate admits roughly its share, deterministically:
    /// the same (cycle, attempt) keys always make the same choice.
    #[test]
    fn fractional_rate_samples_deterministically() {
        let s1 = ExemplarSampler::pinned(0.25, 0.0);
        let s2 = ExemplarSampler::pinned(0.25, 0.0);
        let picks1: Vec<bool> = (0..4000).map(|c| s1.admit(c, 3).is_some()).collect();
        let picks2: Vec<bool> = (0..4000).map(|c| s2.admit(c, 3).is_some()).collect();
        assert_eq!(picks1, picks2, "sampling must be replayable");
        let hits = picks1.iter().filter(|b| **b).count();
        assert!(
            (600..=1400).contains(&hits),
            "0.25 of 4000 should land near 1000, got {hits}"
        );
    }

    /// The frequency ceiling squelches bursts, counts what it
    /// squelched, and drains the count onto the next admission.
    #[test]
    fn frequency_ceiling_squelches_and_counts() {
        // 1 event per 10 seconds: within a fast test, exactly one
        // admission fits; the rest of the burst is squelched.
        let s = ExemplarSampler::pinned(1.0, 0.1);
        assert_eq!(s.admit(0, 1), Some(0), "first admission passes");
        let mut squelched = 0u64;
        for c in 1..50 {
            if s.admit(c, 1).is_none() {
                squelched += 1;
            }
        }
        assert_eq!(squelched, 49, "burst over the ceiling is squelched");
        assert_eq!(s.squelched.load(Ordering::Acquire), 49);
    }

    /// A shared cell moves LIVE samplers push-on-set: flipping the
    /// rate through the cell (what the dynamic control's applier
    /// does) enables/disables an already-constructed sampler with
    /// no reconstruction and no polling.
    #[test]
    fn shared_cell_moves_live_samplers_on_set() {
        let cfg = std::sync::Arc::new(ExemplarConfig::new(0.0, 0.0));
        let s = ExemplarSampler::shared(cfg.clone());
        assert!(!s.enabled(), "starts off");
        assert!(s.admit(1, 1).is_none());

        cfg.set_rate(1.0); // the control applier's one atomic store
        assert!(s.enabled(), "flips on push-on-set");
        assert_eq!(s.admit(1, 1), Some(0));

        cfg.set_max_hz(0.001); // ceiling: next admissions squelch
        assert!(s.admit(2, 1).is_none());
        assert!(s.admit(3, 1).is_none());

        cfg.set_rate(0.0); // and off again, live
        assert!(!s.enabled());
    }

    /// One advisory per class per phase, capped at 3 classes —
    /// a storm identifies itself without flooding the output.
    #[test]
    fn advisory_gate_is_once_per_class_and_capped() {
        let g = AdvisoryGate::new();
        assert!(g.first_sighting("Overload"));
        assert!(!g.first_sighting("Overload"), "once per class");
        assert!(g.first_sighting("Timeout"));
        assert!(g.first_sighting("Unavailable"));
        assert!(!g.first_sighting("FourthClass"), "cap closes the gate");
        assert!(!g.first_sighting("Overload"), "seen classes stay closed");
    }

    /// The advisory names the class, the budget, and both paths
    /// forward (sampling control, opt-out).
    #[test]
    fn advisory_line_is_actionable() {
        let line = render_advisory("insert", 42, 21, "Overload", "in_flight=9 > 8");
        assert!(line.contains("op 'insert'"), "{line}");
        assert!(line.contains("[Overload]"), "{line}");
        assert!(line.contains("cycle 42"), "{line}");
        assert!(line.contains("(21)"), "{line}");
        assert!(line.contains("retry_exemplar_rate"), "{line}");
        assert!(line.contains("retry_advisory: off"), "{line}");
    }

    /// The rendered line carries every field an operator needs to
    /// act on the specimen, including the squelch tally.
    #[test]
    fn rendered_line_is_self_describing() {
        let line = render_exemplar(&ExecExemplar {
            op_name: "insert",
            cycle: 12345,
            attempt_no: 3,
            tries_budget: 21,
            error_class: "Overload",
            message: "simulated overload: in_flight=9 > 8",
            will_retry: true,
            squelched_since_last: 7,
        });
        assert!(line.contains("op 'insert'"), "{line}");
        assert!(line.contains("attempt 3/21"), "{line}");
        assert!(line.contains("cycle 12345"), "{line}");
        assert!(line.contains("retrying"), "{line}");
        assert!(line.contains("[Overload]"), "{line}");
        assert!(line.contains("in_flight=9 > 8"), "{line}");
        assert!(line.contains("(+7 squelched)"), "{line}");
    }
}

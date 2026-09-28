// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Time-weighted exponentially-weighted moving average.
//!
//! An [`Ewma`] holds a single scalar estimate of a stream and
//! updates it on every [`Self::record`] using a time-decayed
//! weight: samples taken close together weigh nearly equal;
//! samples separated by more than the configured half-life
//! weigh markedly less than fresh ones.
//!
//! Parameter: **half-life**. The weight assigned to older data
//! halves every `half_life` of elapsed time. Pick a half-life
//! that matches the time scale the display cares about — 1 s
//! for "current rate", 10 s for "settled rate", 1 min for
//! "sustained rate".
//!
//! ## When to prefer this over [`super::binomial_summary::BinomialSummary`]
//!
//! A `BinomialSummary` keeps a bounded ring of samples so callers
//! can render a sparkline. An `Ewma` keeps *one number*. If the
//! display is a single readout ("the current rate is 1342/s"),
//! an `Ewma` is cheaper and renders without flicker — it never
//! oscillates even when the raw stream does. Both are valid
//! summaries; they answer different questions.
//!
//! ## Time-weighted decay
//!
//! For each `record(value, now)` call after the first:
//!
//! ```text
//! dt = now - last_update
//! alpha = 1 - exp(-dt / half_life * ln(2))
//! current = alpha * value + (1 - alpha) * current
//! ```
//!
//! The `ln(2)` scaling makes `half_life` interpretable literally
//! as "the elapsed time at which an old sample's weight halves".
//! First sample initializes `current` to the sample itself
//! (no decay from an undefined zero).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Time-weighted exponentially-weighted moving average.
///
/// Thread-safe via a single `Mutex`. `record` is the only
/// mutating operation; `peek` is a read-only copy.
#[derive(Debug)]
pub struct Ewma {
    /// Half-life in seconds — the elapsed time at which an old
    /// sample's effective weight halves. Must be positive.
    half_life_secs: f64,
    state: Mutex<EwmaState>,
}

#[derive(Debug)]
struct EwmaState {
    /// Current EMA estimate. `None` until the first sample.
    current: Option<f64>,
    /// Instant of the last `record` — used to compute `dt` for
    /// the next update. `None` at construction, `Some` after the
    /// first sample.
    last: Option<Instant>,
}

impl Ewma {
    /// Build an Ewma with the given half-life.
    /// The half-life must be positive and strictly greater than
    /// zero — a zero or negative half-life has no meaningful
    /// interpretation.
    pub fn new(half_life: Duration) -> Self {
        let secs = half_life.as_secs_f64();
        assert!(secs > 0.0, "Ewma half-life must be > 0");
        Self {
            half_life_secs: secs,
            state: Mutex::new(EwmaState {
                current: None,
                last: None,
            }),
        }
    }

    /// Configured half-life.
    pub fn half_life(&self) -> Duration {
        Duration::from_secs_f64(self.half_life_secs)
    }

    /// Record a sample taken at `now`. The first call initializes
    /// the estimate to the sample value directly; subsequent calls
    /// blend the new value in with a time-decayed weight.
    pub fn record(&self, value: f64, now: Instant) {
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match (g.current, g.last) {
            (None, _) | (_, None) => {
                g.current = Some(value);
                g.last = Some(now);
            }
            (Some(curr), Some(last)) => {
                // `saturating_duration_since` would underflow if
                // `now < last` (clock skew); treat that as dt=0.
                let dt_secs = now.saturating_duration_since(last).as_secs_f64();
                // alpha = 1 - 2^(-dt / half_life)  (equivalent to the
                // ln(2) form in the module docs, but avoids one FP op).
                let alpha = 1.0 - 2f64.powf(-dt_secs / self.half_life_secs);
                let blended = alpha * value + (1.0 - alpha) * curr;
                g.current = Some(blended);
                g.last = Some(now);
            }
        }
    }

    /// Convenience wrapper around [`Self::record`] that uses
    /// `Instant::now()` for the timestamp. Prefer the explicit
    /// form in deterministic contexts (tests, replay).
    pub fn record_now(&self, value: f64) {
        self.record(value, Instant::now());
    }

    /// Current EMA estimate. Returns `None` until the first
    /// sample has been recorded — callers should treat that as
    /// "no data yet" rather than a zero reading.
    pub fn peek(&self) -> Option<f64> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).current
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_initializes_without_decay() {
        let e = Ewma::new(Duration::from_secs(1));
        assert!(e.peek().is_none());
        let t0 = Instant::now();
        e.record(42.0, t0);
        assert_eq!(e.peek(), Some(42.0));
    }

    #[test]
    fn zero_dt_updates_take_full_weight() {
        let e = Ewma::new(Duration::from_secs(1));
        let t0 = Instant::now();
        e.record(10.0, t0);
        // Zero dt → alpha = 0 → new value has NO weight,
        // current stays at the previous reading. This is the
        // correct interpretation: "no time has elapsed, so the
        // new reading is a duplicate observation, weighting it
        // the same as the prior reading".
        e.record(20.0, t0);
        assert_eq!(e.peek(), Some(10.0));
    }

    #[test]
    fn half_life_behavior() {
        // With half_life = 1s, a sample one half-life later blends
        // 50/50 with the previous value.
        let e = Ewma::new(Duration::from_secs(1));
        let t0 = Instant::now();
        e.record(0.0, t0);
        e.record(10.0, t0 + Duration::from_secs(1));
        let v = e.peek().unwrap();
        assert!((v - 5.0).abs() < 1e-9, "expected 5.0, got {v}");
    }

    #[test]
    fn converges_to_steady_value() {
        // A sustained stream converges toward the sample value.
        let e = Ewma::new(Duration::from_millis(100));
        let mut t = Instant::now();
        for _ in 0..200 {
            e.record(100.0, t);
            t += Duration::from_millis(10);
        }
        // After many half-lives of 100.0 samples, EMA ≈ 100.0.
        let v = e.peek().unwrap();
        assert!((v - 100.0).abs() < 1e-6, "expected ~100, got {v}");
    }

    #[test]
    fn clock_skew_treated_as_zero_dt() {
        // If now < last (backward clock), saturating_duration_since
        // returns 0 → alpha = 0 → current unchanged. This keeps
        // the estimate sane across NTP adjustments.
        let e = Ewma::new(Duration::from_secs(1));
        let t0 = Instant::now();
        e.record(5.0, t0 + Duration::from_secs(10));
        e.record(999.0, t0);
        // Backward dt → alpha 0 → current holds at 5.0.
        assert_eq!(e.peek(), Some(5.0));
    }

    #[test]
    #[should_panic(expected = "half-life must be > 0")]
    fn zero_half_life_rejected() {
        let _ = Ewma::new(Duration::ZERO);
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Retained (non-draining) HDR histogram.
//!
//! Primary instruments are **delta-shaped**:
//! [`crate::instruments::histogram::Histogram`] and
//! [`crate::instruments::timer::Timer`] reset their reservoir on
//! every `snapshot()` so the scheduler cascade always sees a
//! fresh delta. That's the right shape for a streaming pipeline,
//! but it means there is no direct way to get lossless
//! percentiles over a scope — "p99 for this whole phase" requires
//! merging every intermediate snapshot manually, or a round-trip
//! through the cadence reporter / SQLite.
//!
//! An [`HdrSummary`] fills the gap: a scope-bounded, non-draining
//! HDR histogram that retains every sample fed into it and
//! produces lossless percentiles on demand. Create one per scope
//! (phase, analysis window, report cell), feed samples from
//! whatever source you like (a `Timer` observer, `record_relevancy`
//! site, or manually from a scheduler callback), and drop it
//! when the scope ends.
//!
//! ## Sizing
//!
//! HDR memory is determined entirely by the
//! [`super::live_window::HdrBounds`] configuration — independent
//! of sample count. At 3 significant digits over 1 µs .. 60 s
//! (the `LATENCY_DEFAULT` preset) one HDR is ≈ 200 KiB, which
//! easily dominates a long phase's summary overhead. Pick a
//! tighter bound if you know the range in advance.

use std::sync::Mutex;

use hdrhistogram::Histogram as HdrHistogram;

use super::live_window::HdrBounds;

/// Non-draining HDR histogram with exact-percentile reads over
/// the whole scope of the instance's lifetime.
///
/// Thread-safe via a single `Mutex`. `record` is the only
/// mutating operation; `peek_snapshot` returns a clone.
#[derive(Debug)]
pub struct HdrSummary {
    bounds: HdrBounds,
    hist: Mutex<HdrHistogram<u64>>,
}

impl HdrSummary {
    /// Build a fresh summary with the given HDR bounds + precision.
    pub fn new(bounds: HdrBounds) -> Self {
        Self {
            hist: Mutex::new(bounds_build(&bounds)),
            bounds,
        }
    }

    /// Latency-tuned constructor (1 µs .. 60 s, 3 sig digits).
    pub fn latency() -> Self {
        Self::new(HdrBounds::LATENCY_DEFAULT)
    }

    /// Configured bounds.
    pub fn bounds(&self) -> HdrBounds {
        self.bounds
    }

    /// Record one sample. Values outside the configured bounds
    /// are clamped into range (HDR would silently drop otherwise).
    pub fn record(&self, value: u64) {
        let v = value.clamp(self.bounds.low, self.bounds.high);
        let mut g = self.hist.lock().unwrap_or_else(|e| e.into_inner());
        // `record` can only fail for out-of-range values, which
        // we've already clamped — ignore the Result for the hot path.
        let _ = g.record(v);
    }

    /// Non-mutating snapshot of the accumulated distribution.
    /// Clones the underlying HDR so callers can compute
    /// percentiles without holding the lock.
    pub fn peek_snapshot(&self) -> HdrSnapshot {
        let g = self.hist.lock().unwrap_or_else(|e| e.into_inner());
        HdrSnapshot { hist: g.clone() }
    }

    /// Total sample count so far.
    pub fn len(&self) -> u64 {
        self.hist.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when no samples have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Immutable snapshot returned by [`HdrSummary::peek_snapshot`].
/// Holds a clone of the underlying HDR so percentile queries
/// don't contend with concurrent `record` calls.
#[derive(Debug, Clone)]
pub struct HdrSnapshot {
    hist: HdrHistogram<u64>,
}

impl HdrSnapshot {
    /// Total sample count.
    pub fn len(&self) -> u64 {
        self.hist.len()
    }

    /// True if the snapshot has no samples.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Arithmetic mean over recorded samples.
    pub fn mean(&self) -> f64 {
        self.hist.mean()
    }

    /// Minimum observed value. Zero if empty.
    pub fn min(&self) -> u64 {
        self.hist.min()
    }

    /// Maximum observed value. Zero if empty.
    pub fn max(&self) -> u64 {
        self.hist.max()
    }

    /// Percentile at quantile `q ∈ [0.0, 100.0]`.
    pub fn percentile(&self, q: f64) -> u64 {
        self.hist.value_at_quantile(q / 100.0)
    }

    pub fn p50(&self) -> u64 {
        self.percentile(50.0)
    }
    pub fn p90(&self) -> u64 {
        self.percentile(90.0)
    }
    pub fn p99(&self) -> u64 {
        self.percentile(99.0)
    }
    pub fn p999(&self) -> u64 {
        self.percentile(99.9)
    }
}

fn bounds_build(b: &HdrBounds) -> HdrHistogram<u64> {
    HdrHistogram::new_with_bounds(b.low, b.high, b.sig_digits).expect("HDR bounds must be valid")
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retains_across_reads() {
        let s = HdrSummary::latency();
        for v in [1_000_000u64, 2_000_000, 3_000_000] {
            s.record(v);
        }
        let a = s.peek_snapshot();
        let b = s.peek_snapshot();
        assert_eq!(a.len(), 3);
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn percentile_accuracy_within_hdr_resolution() {
        let s = HdrSummary::latency();
        // 1000 samples at 1 ms each.
        for _ in 0..1_000 {
            s.record(1_000_000);
        }
        let snap = s.peek_snapshot();
        assert_eq!(snap.len(), 1_000);
        // HDR at 3 sig-digit over 1 µs .. 60 s resolves 1 ms
        // to within ~0.1% bucket width. Allow 1 bucket of slack.
        let p50 = snap.p50();
        assert!((999_000..=1_001_000).contains(&p50), "p50 = {p50}");
    }

    #[test]
    fn clamps_out_of_range_values() {
        let bounds = HdrBounds {
            low: 1_000,
            high: 60_000_000_000,
            sig_digits: 3,
        };
        let s = HdrSummary::new(bounds);
        s.record(0); // below low — clamped up to 1_000
        s.record(u64::MAX); // above high — clamped down to 60 s
        let snap = s.peek_snapshot();
        assert_eq!(snap.len(), 2);
        // HDR quantizes to bucket boundaries; exact 1_000 / 60e9
        // matches aren't guaranteed. Verify the values land
        // inside the configured range (nothing dropped) and that
        // the two samples are on opposite ends of the distribution.
        assert!(
            snap.min() > 0,
            "min = {} should be > 0 after clamp",
            snap.min()
        );
        assert!(
            snap.min() < snap.max() / 1_000_000,
            "min/max should be far apart: min={}, max={}",
            snap.min(),
            snap.max()
        );
        assert!(
            snap.max() <= 60_000_000_000 + 60_000_000,
            "max = {} should be ≤ 60s + one bucket",
            snap.max()
        );
    }

    #[test]
    fn snapshot_is_independent_of_further_records() {
        let s = HdrSummary::latency();
        s.record(1_000_000);
        let snap = s.peek_snapshot();
        s.record(2_000_000);
        assert_eq!(snap.len(), 1);
        assert_eq!(s.peek_snapshot().len(), 2);
    }

    #[test]
    fn empty_summary_reports_empty_snapshot() {
        let s = HdrSummary::latency();
        let snap = s.peek_snapshot();
        assert!(snap.is_empty());
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn concurrent_record_is_serialized() {
        use std::sync::Arc;
        use std::thread;

        let s = Arc::new(HdrSummary::latency());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s = s.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    s.record(1_000_000);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(s.len(), 8_000);
    }
}

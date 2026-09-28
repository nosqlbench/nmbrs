// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Timer: histogram + counter for per-operation latency recording.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use crate::instruments::histogram::Histogram;
use crate::labels::Labels;
use crate::summaries::live_window::{LiveWindowConfig, LiveWindowHistogram};
use hdrhistogram::Histogram as HdrHistogram;

pub struct Timer {
    labels: Labels,
    histogram: Histogram,
    count: AtomicU64,
    /// Opt-in sliding-window view. `None` until
    /// [`Self::enable_live_window`] is called, at which point
    /// [`Self::record`] writes to both the main delta reservoir
    /// and the live-window ring. Hot path cost when unused: one
    /// atomic load + null check (no branch misprediction in the
    /// common case).
    live_window: OnceLock<Arc<LiveWindowHistogram>>,
}

/// Snapshot of a timer's state for a single interval.
pub struct TimerSnapshot {
    pub histogram: HdrHistogram<u64>,
    pub count: u64,
}

impl Timer {
    pub fn new(labels: Labels) -> Self {
        Self::with_sigdigs(labels, crate::instruments::histogram::DEFAULT_HDR_SIGDIGS)
    }

    /// Construct a Timer with explicit HDR significant-digits
    /// precision. Use this from a call site that already knows
    /// the desired precision.
    pub fn with_sigdigs(labels: Labels, sigdigs: u8) -> Self {
        Self {
            labels: labels.clone(),
            histogram: crate::instruments::histogram::Histogram::with_sigdigs(labels, sigdigs),
            count: AtomicU64::new(0),
            live_window: OnceLock::new(),
        }
    }

    /// Construct a Timer whose underlying histogram uses the
    /// HDR significant-digits precision configured on the
    /// nearest ancestor of `component` that declares
    /// [`crate::instruments::histogram::HDR_SIGDIGS_PROP`].
    /// Falls back to the default if no ancestor declares it.
    /// SRD 40 §"HDR significant digits — subtree-scoped setting".
    pub fn with_sigdigs_from(labels: Labels, component: &crate::component::Component) -> Self {
        let sigdigs = crate::instruments::histogram::resolve_hdr_sigdigs(component);
        Self::with_sigdigs(labels, sigdigs)
    }

    /// Record a duration in nanoseconds. If the live-window ring
    /// has been activated via [`Self::enable_live_window`] it
    /// receives the sample too — otherwise the cold path is a
    /// single atomic load + null check.
    pub fn record(&self, duration_nanos: u64) {
        self.histogram.record(duration_nanos);
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Some(live) = self.live_window.get() {
            live.record(duration_nanos);
        }
    }

    /// Snapshot: returns the delta histogram and the current count.
    ///
    /// The count returned is the absolute total (not delta) — reporters
    /// compute deltas by comparing with their previous snapshot.
    pub fn snapshot(&self) -> TimerSnapshot {
        TimerSnapshot {
            histogram: self.histogram.snapshot(),
            count: self.count.load(Ordering::Relaxed),
        }
    }

    /// Non-draining snapshot — clones the histogram without resetting
    /// it. Use for "live read-through" consumers (e.g., the `now()`
    /// path in the windowed metrics layer) that shouldn't steal data
    /// from the next `snapshot()`. See [`Histogram::peek_snapshot`].
    pub fn peek_snapshot(&self) -> TimerSnapshot {
        TimerSnapshot {
            histogram: self.histogram.peek_snapshot(),
            count: self.count.load(Ordering::Relaxed),
        }
    }

    /// Explicitly activate the sliding-window live view with a
    /// non-default config. Idempotent — second and subsequent calls
    /// return the existing ring, ignoring the passed config.
    ///
    /// Callers who are happy with [`LiveWindowConfig::default`]
    /// **do not need to call this** — [`Self::peek_live_window`]
    /// auto-activates with defaults on first use. This method is
    /// only for callers that want tighter bounds, a different
    /// window size, or more slots before the first peek fires.
    pub fn enable_live_window(&self, config: LiveWindowConfig) -> Arc<LiveWindowHistogram> {
        self.live_window
            .get_or_init(|| Arc::new(LiveWindowHistogram::new(config)))
            .clone()
    }

    /// Peek the sliding-window view. Auto-activates the ring with
    /// [`LiveWindowConfig::default`] on first call — subsequent
    /// calls just read.
    ///
    /// Cost model:
    /// - Before the first peek ever: zero hot-path overhead on
    ///   `record` (the OnceLock is empty, the record path branches
    ///   past it).
    /// - Starting with the first peek: every subsequent `record`
    ///   on this Timer writes to the ring too (+~63 ns per record
    ///   at default config; see the `live_window/record_enabled`
    ///   bench).
    ///
    /// Returns the full rolling window merged into a fresh HDR
    /// histogram. Empty on the very first call (ring just created,
    /// no records captured yet).
    pub fn peek_live_window(&self) -> HdrHistogram<u64> {
        let ring = self
            .live_window
            .get_or_init(|| Arc::new(LiveWindowHistogram::new(LiveWindowConfig::default())));
        ring.peek()
    }

    /// True when the live-window ring has been activated (either
    /// explicitly via [`Self::enable_live_window`] or lazily via a
    /// first [`Self::peek_live_window`] call). Useful for
    /// diagnostics / benches — does **not** trigger activation.
    pub fn live_window_active(&self) -> bool {
        self.live_window.get().is_some()
    }

    /// Direct access to the live-window ring if it's already active.
    /// Unlike [`Self::peek_live_window`], does NOT lazily activate —
    /// returns `None` if nothing has peeked yet. Useful for callers
    /// that want to read the ring's config / `len()` without
    /// triggering activation.
    pub fn live_window(&self) -> Option<Arc<LiveWindowHistogram>> {
        self.live_window.get().cloned()
    }

    /// Current total count (without snapshotting).
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_record_and_snapshot() {
        let t = Timer::new(Labels::of("name", "servicetime"));
        t.record(1_000_000);
        t.record(2_000_000);

        let snap = t.snapshot();
        assert_eq!(snap.histogram.len(), 2);
        assert_eq!(snap.count, 2);
    }

    #[test]
    fn timer_delta_histogram() {
        let t = Timer::new(Labels::of("name", "test"));
        t.record(1_000);
        let snap1 = t.snapshot();
        assert_eq!(snap1.histogram.len(), 1);

        t.record(2_000);
        t.record(3_000);
        let snap2 = t.snapshot();
        assert_eq!(snap2.histogram.len(), 2); // delta: only new records
        assert_eq!(snap2.count, 3); // count is cumulative
    }

    #[test]
    fn timer_count_monotonic() {
        let t = Timer::new(Labels::of("name", "c"));
        assert_eq!(t.count(), 0);
        t.record(100);
        assert_eq!(t.count(), 1);
        t.record(200);
        assert_eq!(t.count(), 2);
    }

    #[test]
    fn live_window_inactive_until_first_peek() {
        let t = Timer::new(Labels::of("name", "lw"));
        t.record(100_000);
        t.record(200_000);
        // Records before any peek must NOT enable the ring — this
        // is the zero-overhead default for headless runs.
        assert!(!t.live_window_active());
    }

    #[test]
    fn first_peek_lazily_activates_with_defaults() {
        let t = Timer::new(Labels::of("name", "lw"));
        // Records before any peek are NOT captured (ring doesn't
        // exist yet).
        t.record(50_000);
        let first = t.peek_live_window();
        assert!(t.live_window_active());
        // First peek returns an empty merged result — the ring just
        // came into existence.
        assert_eq!(first.len(), 0);
        // From now on, records are captured.
        t.record(100_000);
        t.record(200_000);
        let snap = t.peek_live_window();
        assert_eq!(snap.len(), 2);
        assert!(snap.max() >= 200_000);
    }

    #[test]
    fn explicit_enable_preempts_lazy_init() {
        let t = Timer::new(Labels::of("name", "lw"));
        let _ring = t.enable_live_window(Default::default());
        t.record(100_000);
        t.record(200_000);
        t.record(300_000);
        // Since enable fired before any record, all three are in.
        let snap = t.peek_live_window();
        assert_eq!(snap.len(), 3);
        assert!(snap.max() >= 300_000);
    }

    #[test]
    fn live_window_does_not_drain_main_reservoir() {
        let t = Timer::new(Labels::of("name", "lw"));
        let _ring = t.enable_live_window(Default::default());
        t.record(500_000);
        // Peek the live ring multiple times; main reservoir is
        // untouched.
        let _ = t.peek_live_window();
        let _ = t.peek_live_window();
        let main_snap = t.snapshot();
        assert_eq!(main_snap.count, 1);
        assert_eq!(main_snap.histogram.len(), 1);
    }
}

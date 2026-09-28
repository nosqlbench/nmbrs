// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Rolling max (or min) over a wall-clock window.
//!
//! A [`PeakTracker`] answers "what's the largest (or smallest)
//! value observed in the last N seconds?" — the primitive behind
//! the TUI's 5 s / 10 s peak cross-bar markers on the latency
//! panel, and a common shape for any "rolling peak" display.
//!
//! ## Cost
//!
//! O(1) amortized per `record` and per `peek`, using a monotonic
//! deque: entries are kept in timestamp order, and the deque
//! drops internal entries that can never be the peak again. For a
//! max-tracker, every `record(v)` drops trailing entries whose
//! value is `<= v` — those samples are strictly dominated by the
//! new one for every window that contains both.
//!
//! Memory is bounded by the arrival rate within one window. For
//! a monotonically rising stream the deque holds one entry; for
//! a strictly monotonically decreasing stream it holds one entry
//! per sample in-window (old samples are always larger than new
//! ones for a max-tracker, so none get dominated).
//!
//! ## Direction
//!
//! [`PeakDir::Max`] tracks the rolling maximum; [`PeakDir::Min`]
//! the rolling minimum. Both use the same deque machinery —
//! only the ≤/≥ comparison differs.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Direction of the peak tracker — rolling max or rolling min.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeakDir {
    /// Rolling maximum — evict entries dominated by newer larger ones.
    Max,
    /// Rolling minimum — evict entries dominated by newer smaller ones.
    Min,
}

/// Rolling peak tracker over a wall-clock window.
///
/// Stores `(instant, value)` entries in a monotonic deque and
/// prunes both (a) entries older than the window on read/write
/// and (b) trailing entries dominated by a new record.
#[derive(Debug)]
pub struct PeakTracker {
    window: Duration,
    dir: PeakDir,
    entries: Mutex<VecDeque<(Instant, u64)>>,
}

impl PeakTracker {
    /// Build a rolling peak tracker. `window` must be non-zero.
    pub fn new(window: Duration, dir: PeakDir) -> Self {
        assert!(!window.is_zero(), "PeakTracker window must be > 0");
        Self {
            window,
            dir,
            entries: Mutex::new(VecDeque::new()),
        }
    }

    /// Max-tracker convenience constructor.
    pub fn max(window: Duration) -> Self {
        Self::new(window, PeakDir::Max)
    }

    /// Min-tracker convenience constructor.
    pub fn min(window: Duration) -> Self {
        Self::new(window, PeakDir::Min)
    }

    /// Configured window.
    pub fn window(&self) -> Duration {
        self.window
    }

    /// Configured direction.
    pub fn direction(&self) -> PeakDir {
        self.dir
    }

    /// Record a sample taken at `now`.
    ///
    /// Drops trailing entries dominated by `value` (for Max:
    /// entries with value ≤ new; for Min: value ≥ new) before
    /// appending. Also evicts any front entries older than the
    /// window so the deque stays bounded.
    pub fn record(&self, value: u64, now: Instant) {
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());

        // Evict stale entries from the front (oldest first).
        self.evict_stale(&mut g, now);

        // Drop trailing entries that can no longer be the peak.
        match self.dir {
            PeakDir::Max => {
                while let Some(&(_, v)) = g.back() {
                    if v <= value {
                        g.pop_back();
                    } else {
                        break;
                    }
                }
            }
            PeakDir::Min => {
                while let Some(&(_, v)) = g.back() {
                    if v >= value {
                        g.pop_back();
                    } else {
                        break;
                    }
                }
            }
        }

        g.push_back((now, value));
    }

    /// [`Self::record`] with `Instant::now()` as the timestamp.
    pub fn record_now(&self, value: u64) {
        self.record(value, Instant::now());
    }

    /// Current rolling peak. Returns `None` if no in-window
    /// samples have been recorded.
    pub fn peek(&self, now: Instant) -> Option<u64> {
        let mut g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        self.evict_stale(&mut g, now);
        g.front().map(|&(_, v)| v)
    }

    /// [`Self::peek`] with `Instant::now()`.
    pub fn peek_now(&self) -> Option<u64> {
        self.peek(Instant::now())
    }

    /// Number of entries currently held (for diagnostics /
    /// tests). Bounded by arrival rate within one window in the
    /// worst case.
    pub fn len(&self) -> usize {
        let g = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        g.len()
    }

    /// True when no in-window samples remain.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ---- helpers -------------------------------------------------

    fn evict_stale(&self, q: &mut VecDeque<(Instant, u64)>, now: Instant) {
        while let Some(&(t, _)) = q.front() {
            if now.duration_since(t) >= self.window {
                q.pop_front();
            } else {
                break;
            }
        }
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_on_construction() {
        let p = PeakTracker::max(Duration::from_secs(1));
        assert!(p.peek_now().is_none());
    }

    #[test]
    fn reports_max_over_window() {
        let p = PeakTracker::max(Duration::from_secs(1));
        let t0 = Instant::now();
        p.record(10, t0);
        p.record(30, t0 + Duration::from_millis(100));
        p.record(20, t0 + Duration::from_millis(200));
        assert_eq!(p.peek(t0 + Duration::from_millis(300)), Some(30));
    }

    #[test]
    fn reports_min_over_window() {
        let p = PeakTracker::min(Duration::from_secs(1));
        let t0 = Instant::now();
        p.record(10, t0);
        p.record(5, t0 + Duration::from_millis(100));
        p.record(20, t0 + Duration::from_millis(200));
        assert_eq!(p.peek(t0 + Duration::from_millis(300)), Some(5));
    }

    #[test]
    fn drops_dominated_entries_from_back() {
        // Record a small value, then a bigger one — the small is
        // dominated and should be evicted from the back.
        let p = PeakTracker::max(Duration::from_secs(10));
        let t0 = Instant::now();
        p.record(5, t0);
        p.record(7, t0 + Duration::from_millis(10));
        assert_eq!(p.len(), 1);
        assert_eq!(p.peek(t0 + Duration::from_millis(20)), Some(7));
    }

    #[test]
    fn drops_stale_entries_from_front() {
        let p = PeakTracker::max(Duration::from_millis(100));
        let t0 = Instant::now();
        p.record(50, t0);
        // Well after the window — the old entry should be evicted
        // by the time we peek.
        let later = t0 + Duration::from_millis(200);
        assert!(p.peek(later).is_none());
        assert_eq!(p.len(), 0);
    }

    #[test]
    fn monotonically_decreasing_keeps_all() {
        // No entry dominates its predecessor, so all stay in the
        // deque until they age out.
        let p = PeakTracker::max(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..5 {
            p.record(100 - i, t0 + Duration::from_millis(i * 10));
        }
        assert_eq!(p.len(), 5);
        assert_eq!(p.peek(t0 + Duration::from_millis(100)), Some(100));
    }

    #[test]
    fn monotonically_increasing_keeps_one() {
        // Each new entry dominates all prior — back of deque
        // collapses to one element.
        let p = PeakTracker::max(Duration::from_secs(10));
        let t0 = Instant::now();
        for i in 0..10 {
            p.record(i, t0 + Duration::from_millis(i * 10));
        }
        assert_eq!(p.len(), 1);
        assert_eq!(p.peek(t0 + Duration::from_millis(200)), Some(9));
    }

    #[test]
    fn peek_mid_window_drops_stale_without_record() {
        let p = PeakTracker::max(Duration::from_millis(50));
        let t0 = Instant::now();
        p.record(123, t0);
        // 200 ms later without any record — peek itself evicts.
        assert!(p.peek(t0 + Duration::from_millis(200)).is_none());
    }

    #[test]
    #[should_panic(expected = "window must be > 0")]
    fn zero_window_rejected() {
        let _ = PeakTracker::max(Duration::ZERO);
    }
}

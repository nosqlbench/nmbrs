// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Sliding-window histogram with a pre-allocated lazy-reset ring.
//!
//! A [`LiveWindowHistogram`] holds a ring of N sub-bucket histograms
//! covering a rolling window (e.g., 10 × 100ms = 1s). It's intended
//! as an **opt-in live-view sibling** to the delta reservoir that the
//! scheduler drains — readers pulling through
//! [`crate::metrics_query::MetricHandle`] see a smooth 1s window that
//! doesn't dip to zero just after each scheduler tick.
//!
//! ## Design
//!
//! - **Pre-allocated.** N sub-bucket HDRs are built at construction.
//!   No allocation on the hot `record` path.
//! - **Lazy reset.** A slot is only `reset()`ed when the first record
//!   of a new cycle lands in it — never on a sweeper thread, never
//!   on the peek path. Slots that go unused stay untouched until
//!   the next write.
//! - **Pure-read peek.** `peek()` merges every fresh slot into a
//!   pre-allocated scratch buffer. No mutation of slot state; stale
//!   slots are identified by `started_at` and skipped, not reset.
//! - **Per-slot mutex.** Record takes one slot's mutex only —
//!   threads writing at different time boundaries don't contend.
//!
//! ## Memory
//!
//! At 3-digit precision over `1µs..60s` each slot is ~200 KiB. The
//! default 10-slot ring is ~2 MiB per activated instrument. Callers
//! that need tighter memory should widen the slot duration or
//! narrow the HDR bounds.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram as HdrHistogram;

/// HDR reservoir bounds + precision for a [`LiveWindowHistogram`]'s
/// sub-bucket histograms.
#[derive(Clone, Copy, Debug)]
pub struct HdrBounds {
    pub low: u64,
    pub high: u64,
    pub sig_digits: u8,
}

impl HdrBounds {
    /// Defaults tuned for operation-latency display: 1 µs .. 60 s
    /// at 3 significant digits. ≈ 200 KiB per slot, ≈ 2 MiB per
    /// 10-slot ring.
    pub const LATENCY_DEFAULT: Self = Self {
        low: 1_000,           // 1 µs in ns
        high: 60_000_000_000, // 60 s in ns
        sig_digits: 3,
    };

    fn build(&self) -> HdrHistogram<u64> {
        HdrHistogram::new_with_bounds(self.low, self.high, self.sig_digits)
            .expect("HDR bounds must be valid")
    }
}

/// Configuration for a [`LiveWindowHistogram`].
#[derive(Clone, Copy, Debug)]
pub struct LiveWindowConfig {
    /// Total length of the rolling window (e.g., 1 s).
    pub window: Duration,
    /// Number of sub-slots the window is split into. More slots =
    /// finer time resolution, more memory, more merges per peek.
    pub slot_count: usize,
    /// HDR bounds + precision for each slot.
    pub bounds: HdrBounds,
}

impl Default for LiveWindowConfig {
    /// 1 s window × 10 sub-slots × 3-digit latency HDR.
    fn default() -> Self {
        Self {
            window: Duration::from_secs(1),
            slot_count: 10,
            bounds: HdrBounds::LATENCY_DEFAULT,
        }
    }
}

struct Slot {
    /// Start of the current cycle's time window for this slot.
    /// Stale when `now - started_at >= config.window`.
    started_at: Instant,
    hist: HdrHistogram<u64>,
}

/// A sliding-window histogram with lazy per-slot reset.
///
/// Typical usage: attach to a [`crate::instruments::timer::Timer`]
/// (or `Histogram`) via `enable_live_window`; writers call
/// [`Self::record`]; readers call [`Self::peek`] to get the full
/// window's merged distribution.
pub struct LiveWindowHistogram {
    config: LiveWindowConfig,
    slot_duration_ns: u128,
    /// Pre-allocated slots, never reallocated after construction.
    slots: Vec<Mutex<Slot>>,
    /// Pre-allocated merge scratch. `peek` resets it, `add`s every
    /// fresh slot into it, then clones the result for the caller.
    scratch: Mutex<HdrHistogram<u64>>,
    /// Reference instant used to compute slot indices — every
    /// `record`/`peek` relative to this anchor so slot identity is
    /// stable across threads without a shared clock.
    anchor: Instant,
}

impl LiveWindowHistogram {
    /// Construct a new ring. All N sub-bucket histograms are built
    /// now — the hot path never allocates.
    pub fn new(config: LiveWindowConfig) -> Self {
        assert!(config.slot_count > 0, "slot_count must be > 0");
        assert!(config.window.as_nanos() > 0, "window must be > 0");
        let slot_duration = config.window / config.slot_count as u32;
        let anchor = Instant::now();
        // Initialise each slot's started_at to the anchor minus the
        // window, so every slot reads as "stale" on first use and
        // will be reset + stamped on its first record.
        let stale_start = anchor.checked_sub(config.window).unwrap_or(anchor);
        let slots = (0..config.slot_count)
            .map(|_| {
                Mutex::new(Slot {
                    started_at: stale_start,
                    hist: config.bounds.build(),
                })
            })
            .collect();
        let scratch = Mutex::new(config.bounds.build());
        Self {
            config,
            slot_duration_ns: slot_duration.as_nanos(),
            slots,
            scratch,
            anchor,
        }
    }

    pub fn config(&self) -> &LiveWindowConfig {
        &self.config
    }

    /// Record one sample. Locks exactly one slot's mutex; resets
    /// the slot lazily if it's from a prior cycle.
    pub fn record(&self, value: u64) {
        let now = Instant::now();
        let idx = self.slot_index(now);
        let boundary = self.slot_boundary_for(now, idx);
        // `unwrap_or_else(into_inner)` rides a poisoned mutex — the
        // hot path never panics just because a peeker panicked.
        let mut slot = self.slots[idx].lock().unwrap_or_else(|e| e.into_inner());
        if slot.started_at < boundary {
            slot.hist.reset();
            slot.started_at = boundary;
        }
        // Clamp so an out-of-range value doesn't silently fail —
        // HDR bounds are generous for latency, so this is rare.
        let v = value.min(self.config.bounds.high);
        let _ = slot.hist.record(v.max(self.config.bounds.low));
    }

    /// Merge every fresh slot into the pre-allocated scratch buffer
    /// and return a clone of the result.
    ///
    /// Slots whose `started_at` is older than `now - window` are
    /// skipped (their data is from a prior cycle). Every fresh slot
    /// contributes — including the current partially-filled slot —
    /// so the result reflects the full window of data that has
    /// actually been recorded, up to the instant of the call.
    pub fn peek(&self) -> HdrHistogram<u64> {
        let now = Instant::now();
        let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.reset();
        for slot_mu in &self.slots {
            let slot = slot_mu.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(slot.started_at) < self.config.window {
                let _ = scratch.add(&slot.hist);
            }
            // Stale slots are left alone — their next record will
            // reset them.
        }
        scratch.clone()
    }

    /// Total number of samples currently represented in the window.
    /// Equivalent to `peek().len()` but without the merge clone —
    /// useful when callers only need a populated/empty check.
    pub fn len(&self) -> u64 {
        let now = Instant::now();
        let mut total = 0u64;
        for slot_mu in &self.slots {
            let slot = slot_mu.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(slot.started_at) < self.config.window {
                total = total.saturating_add(slot.hist.len());
            }
        }
        total
    }

    /// Whether the window currently holds no samples. Cheaper than
    /// `len() == 0` only in intent — both walk the live slots.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ---- helpers -------------------------------------------------

    fn slot_index(&self, now: Instant) -> usize {
        let ns = now.duration_since(self.anchor).as_nanos();
        ((ns / self.slot_duration_ns) as usize) % self.config.slot_count
    }

    /// The start of the current cycle's time window for the slot at
    /// `idx`, given the current instant `now`. Used to decide
    /// whether a slot is fresh or needs reset.
    fn slot_boundary_for(&self, now: Instant, idx: usize) -> Instant {
        let ns = now.duration_since(self.anchor).as_nanos();
        let current_slot_start_ns = (ns / self.slot_duration_ns) * self.slot_duration_ns;
        let current_idx = ((ns / self.slot_duration_ns) as usize) % self.config.slot_count;
        // offset = how many slot-durations back is idx vs current_idx
        let offset = ((current_idx + self.config.slot_count) - idx) % self.config.slot_count;
        let boundary_ns =
            current_slot_start_ns.saturating_sub((offset as u128) * self.slot_duration_ns);
        self.anchor + Duration::from_nanos(boundary_ns as u64)
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fast_config() -> LiveWindowConfig {
        LiveWindowConfig {
            window: Duration::from_millis(500),
            slot_count: 5,
            bounds: HdrBounds::LATENCY_DEFAULT,
        }
    }

    #[test]
    fn peek_returns_merged_fresh_slots() {
        let ring = LiveWindowHistogram::new(fast_config());
        ring.record(1_000_000);
        ring.record(2_000_000);
        ring.record(3_000_000);
        let snap = ring.peek();
        assert_eq!(snap.len(), 3);
        assert!(snap.max() >= 3_000_000);
    }

    #[test]
    fn peek_does_not_drain_slots() {
        let ring = LiveWindowHistogram::new(fast_config());
        ring.record(100_000);
        let a = ring.peek();
        let b = ring.peek();
        assert_eq!(a.len(), b.len(), "peek must be idempotent");
    }

    #[test]
    fn len_matches_peek_len() {
        let ring = LiveWindowHistogram::new(fast_config());
        for _ in 0..50 {
            ring.record(100_000);
        }
        assert_eq!(ring.len(), ring.peek().len());
    }

    #[test]
    fn stale_slots_do_not_contribute() {
        let cfg = LiveWindowConfig {
            window: Duration::from_millis(200),
            slot_count: 4,
            bounds: HdrBounds::LATENCY_DEFAULT,
        };
        let ring = LiveWindowHistogram::new(cfg);
        ring.record(1_000_000);
        assert_eq!(ring.len(), 1);
        // Wait longer than the window so every slot goes stale.
        std::thread::sleep(Duration::from_millis(260));
        let late = ring.peek();
        assert_eq!(
            late.len(),
            0,
            "every slot older than window should be skipped"
        );
    }

    #[test]
    fn lazy_reset_only_on_next_record_into_slot() {
        // After a long quiet period, the first record into any slot
        // resets that slot — its older counts must NOT contribute.
        let cfg = LiveWindowConfig {
            window: Duration::from_millis(150),
            slot_count: 3,
            bounds: HdrBounds::LATENCY_DEFAULT,
        };
        let ring = LiveWindowHistogram::new(cfg);
        ring.record(500_000);
        assert_eq!(ring.len(), 1);
        std::thread::sleep(Duration::from_millis(200));
        ring.record(700_000);
        // Only the new record should be visible — the old one's
        // slot either aged out on peek or was reset on record.
        let snap = ring.peek();
        assert_eq!(snap.len(), 1, "stale slots must not leak old samples");
        assert!(snap.max() >= 700_000);
    }

    #[test]
    fn record_bounds_clamp_out_of_range_values() {
        let ring = LiveWindowHistogram::new(fast_config());
        // Below low bound — must be clamped to something in-range
        // (HDR quantization may round the reported min slightly).
        ring.record(0);
        // Above high bound — must be clamped, not silently dropped.
        ring.record(u64::MAX);
        let snap = ring.peek();
        assert_eq!(snap.len(), 2);
        // HDR reports bucketed values; allow one bucket of slack.
        assert!(snap.max() <= 60_000_000_000 * 2);
        assert!(
            snap.min() > 0,
            "below-low clamp must produce a positive recorded value"
        );
    }
}

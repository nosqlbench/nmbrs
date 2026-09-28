// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Resolution-bounded sparkline summary with binomial reduction.
//!
//! A [`BinomialSummary`] holds a bounded number of scalar samples
//! — typically one per horizontal pixel/column a UI wants to render.
//! When the buffer fills, it **pairwise-merges adjacent buckets** to
//! halve its effective resolution, freeing room for new samples
//! without ever exceeding the caller-supplied capacity.
//!
//! Intended use: attach one to a scalar-valued metric (a rate, a
//! gauge, a counter's rate-of-change) for the lifetime of whatever
//! context wants to display it — typically a TUI phase detail
//! block, where the capacity is the width in cells of the sparkline
//! column, or 2× that when rendering with braille characters.
//!
//! Once the source stops feeding the summary (phase end, session
//! shutdown), the rendered sparkline is naturally frozen — the
//! instrument simply receives no further samples. See
//! SRD 62 §"Design notes → Per-phase sparkline: binomial-reduction
//! summary instrument".
//!
//! ## Reduction semantics
//!
//! `record` appends to the tail; when `samples.len() == capacity`,
//! the whole buffer is replaced with a half-length version built
//! by averaging every pair of adjacent samples. An odd trailing
//! element carries through unchanged. This preserves the temporal
//! ordering (oldest → newest) while compressing the horizontal
//! scale: a buffer that has been reduced N times represents
//! 2^N × capacity original samples in capacity cells.
//!
//! The reduction is *monotone*: old samples are downsampled, never
//! re-sampled from fresh data. A reader scanning the buffer sees
//! smoother values at the left edge (more reductions) and
//! progressively finer granularity toward the right. That matches
//! the human-readable intuition "older data is less resolved."

use std::sync::Mutex;

/// Resolution-bounded scalar summary with binomial reduction.
///
/// Thread-safe via a single `Mutex<Vec<f64>>`. `record` is the only
/// mutating operation; `snapshot` / `len` are read-only clones.
#[derive(Debug)]
pub struct BinomialSummary {
    capacity: usize,
    samples: Mutex<Vec<f64>>,
    /// Number of times the buffer has been pairwise-halved. Lets
    /// readers reason about the original-sample-count represented
    /// by the current buffer: `2^reductions × samples.len()`.
    reductions: Mutex<u32>,
}

impl BinomialSummary {
    /// Build a fresh summary with the given horizontal capacity.
    /// `capacity` must be at least 2 (reduction can't produce a
    /// meaningful buffer otherwise).
    pub fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "BinomialSummary capacity must be >= 2");
        Self {
            capacity,
            samples: Mutex::new(Vec::with_capacity(capacity)),
            reductions: Mutex::new(0),
        }
    }

    /// Number of cells the buffer may ever hold simultaneously.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Record one sample at the tail. If the buffer is already at
    /// capacity, the whole buffer is pairwise-merged in place first
    /// (halving its length) and then the new sample is appended.
    pub fn record(&self, value: f64) {
        let mut g = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= self.capacity {
            let mut reduced: Vec<f64> = Vec::with_capacity(self.capacity / 2 + 1);
            let mut it = g.iter().copied();
            loop {
                match (it.next(), it.next()) {
                    (Some(a), Some(b)) => reduced.push((a + b) * 0.5),
                    (Some(a), None) => {
                        reduced.push(a);
                        break;
                    }
                    (None, _) => break,
                }
            }
            *g = reduced;
            let mut r = self.reductions.lock().unwrap_or_else(|e| e.into_inner());
            *r += 1;
        }
        g.push(value);
    }

    /// Clone of the current buffer, oldest-first. Safe to call
    /// arbitrarily often without perturbing the state — `record`
    /// is the only mutator.
    pub fn snapshot(&self) -> Vec<f64> {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Number of samples currently held (never exceeds `capacity`).
    pub fn len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// True when no samples have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of pairwise reductions performed so far. Each
    /// reduction halves the buffer's effective time resolution —
    /// callers that want to label the x-axis can multiply the
    /// per-cell wall time by `2^reductions()` to recover the
    /// original window's span.
    pub fn reductions(&self) -> u32 {
        *self.reductions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_up_to_capacity() {
        let b = BinomialSummary::new(8);
        for i in 0..5 {
            b.record(i as f64);
        }
        assert_eq!(b.len(), 5);
        assert_eq!(b.reductions(), 0);
        assert_eq!(b.snapshot(), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn reduces_on_overflow() {
        let b = BinomialSummary::new(4);
        // Fill to capacity.
        for i in 0..4 {
            b.record(i as f64);
        }
        assert_eq!(b.snapshot(), vec![0.0, 1.0, 2.0, 3.0]);
        assert_eq!(b.reductions(), 0);

        // Next record triggers a pairwise halving: (0+1)/2=0.5,
        // (2+3)/2=2.5 → [0.5, 2.5], then append 4.0.
        b.record(4.0);
        assert_eq!(b.snapshot(), vec![0.5, 2.5, 4.0]);
        assert_eq!(b.reductions(), 1);
        assert!(b.len() <= b.capacity());
    }

    #[test]
    fn odd_length_carries_last() {
        let b = BinomialSummary::new(3);
        // Fill: [1, 2, 3]
        for i in 1..=3 {
            b.record(i as f64);
        }
        // Record 4 → reduce pairs of 3: (1+2)/2=1.5, leftover 3 → [1.5, 3]
        // then append 4 → [1.5, 3, 4]
        b.record(4.0);
        assert_eq!(b.snapshot(), vec![1.5, 3.0, 4.0]);
    }

    #[test]
    fn capacity_never_exceeded_across_many_records() {
        let cap = 16usize;
        let b = BinomialSummary::new(cap);
        for i in 0..1_000 {
            b.record(i as f64);
        }
        assert!(b.len() <= cap, "len {} > capacity {}", b.len(), cap);
        // At least a few reductions should have fired — a lot more
        // in practice. Exact count depends on fill pattern.
        assert!(b.reductions() >= 1);
    }

    #[test]
    fn snapshot_does_not_drain() {
        let b = BinomialSummary::new(4);
        b.record(10.0);
        b.record(20.0);
        let a = b.snapshot();
        let c = b.snapshot();
        assert_eq!(a, c);
        assert_eq!(b.len(), 2);
    }

    #[test]
    #[should_panic(expected = "capacity must be >= 2")]
    fn rejects_tiny_capacity() {
        let _ = BinomialSummary::new(1);
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Lossless f64 statistics accumulator.
//!
//! Collects f64 values and computes exact statistics: mean, min, max,
//! stddev, and percentiles via sorted array. No quantization, no
//! bucket rounding — every value is preserved at full f64 precision.
//!
//! Designed for relevancy scores (recall, precision) where lossy
//! tracking is unacceptable.

use crate::labels::Labels;
use std::sync::Mutex;

/// Thread-safe f64 statistics accumulator with delta semantics.
pub struct F64Stats {
    labels: Labels,
    values: Mutex<Vec<f64>>,
}

/// Snapshot of accumulated f64 statistics.
pub struct F64Snapshot {
    sorted: Vec<f64>,
}

impl F64Stats {
    pub fn new(labels: Labels) -> Self {
        Self {
            labels,
            values: Mutex::new(Vec::new()),
        }
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    /// Record a value.
    pub fn record(&self, value: f64) {
        self.values
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(value);
    }

    /// Take a snapshot and reset (delta semantics).
    pub fn snapshot(&self) -> F64Snapshot {
        let mut guard = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut sorted = std::mem::take(&mut *guard);
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        F64Snapshot { sorted }
    }
}

impl F64Snapshot {
    /// Number of values.
    pub fn len(&self) -> usize {
        self.sorted.len()
    }

    /// Whether the snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// Exact arithmetic mean.
    pub fn mean(&self) -> f64 {
        if self.sorted.is_empty() {
            return 0.0;
        }
        self.sorted.iter().sum::<f64>() / self.sorted.len() as f64
    }

    /// Minimum value.
    pub fn min(&self) -> f64 {
        self.sorted.first().copied().unwrap_or(0.0)
    }

    /// Maximum value.
    pub fn max(&self) -> f64 {
        self.sorted.last().copied().unwrap_or(0.0)
    }

    /// Standard deviation (population).
    pub fn stddev(&self) -> f64 {
        if self.sorted.len() < 2 {
            return 0.0;
        }
        let mean = self.mean();
        let var =
            self.sorted.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / self.sorted.len() as f64;
        var.sqrt()
    }

    /// Percentile (0.0 to 1.0). Uses nearest-rank method.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.sorted.is_empty() {
            return 0.0;
        }
        let idx = ((p * self.sorted.len() as f64).ceil() as usize)
            .saturating_sub(1)
            .min(self.sorted.len() - 1);
        self.sorted[idx]
    }

    pub fn p50(&self) -> f64 {
        self.percentile(0.50)
    }
    pub fn p75(&self) -> f64 {
        self.percentile(0.75)
    }
    pub fn p90(&self) -> f64 {
        self.percentile(0.90)
    }
    pub fn p95(&self) -> f64 {
        self.percentile(0.95)
    }
    pub fn p99(&self) -> f64 {
        self.percentile(0.99)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_stats() {
        let stats = F64Stats::new(Labels::of("name", "test"));
        for i in 0..100 {
            stats.record(i as f64 / 100.0);
        }
        let snap = stats.snapshot();
        assert_eq!(snap.len(), 100);
        assert!((snap.mean() - 0.495).abs() < 0.001);
        assert_eq!(snap.min(), 0.0);
        assert!((snap.max() - 0.99).abs() < 0.001);
        assert!(snap.p50() >= 0.49 && snap.p50() <= 0.51);
    }

    #[test]
    fn exact_recall_scores() {
        let stats = F64Stats::new(Labels::of("name", "recall"));
        stats.record(0.95);
        stats.record(0.98);
        stats.record(1.0);
        stats.record(0.87);
        let snap = stats.snapshot();
        assert_eq!(snap.len(), 4);
        assert_eq!(snap.min(), 0.87);
        assert_eq!(snap.max(), 1.0);
        assert!((snap.mean() - 0.95).abs() < 0.001);
    }

    #[test]
    fn delta_semantics() {
        let stats = F64Stats::new(Labels::of("name", "test"));
        stats.record(1.0);
        stats.record(2.0);
        let snap1 = stats.snapshot();
        assert_eq!(snap1.len(), 2);

        stats.record(3.0);
        let snap2 = stats.snapshot();
        assert_eq!(snap2.len(), 1);
        assert_eq!(snap2.mean(), 3.0);
    }
}

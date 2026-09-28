// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Gauge: instantaneous value, either settable or function-based.

use crate::labels::Labels;
use std::sync::atomic::{AtomicU64, Ordering};

/// A gauge backed by a closure (sampled at capture time).
pub struct FnGauge {
    labels: Labels,
    f: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl FnGauge {
    pub fn new(labels: Labels, f: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        Self {
            labels,
            f: Box::new(f),
        }
    }

    pub fn sample(&self) -> f64 {
        (self.f)()
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

/// A gauge backed by a settable atomic value.
pub struct ValueGauge {
    labels: Labels,
    /// Stored as f64 bits in a u64 for atomic access.
    bits: AtomicU64,
}

impl ValueGauge {
    pub fn new(labels: Labels) -> Self {
        Self {
            labels,
            bits: AtomicU64::new(0.0f64.to_bits()),
        }
    }

    pub fn set(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f64 {
        f64::from_bits(self.bits.load(Ordering::Relaxed))
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn fn_gauge_samples() {
        let counter = Arc::new(AtomicU64::new(42));
        let c = counter.clone();
        let g = FnGauge::new(Labels::of("name", "active"), move || {
            c.load(Ordering::Relaxed) as f64
        });
        assert_eq!(g.sample(), 42.0);
        counter.store(100, Ordering::Relaxed);
        assert_eq!(g.sample(), 100.0);
    }

    #[test]
    fn value_gauge_set_get() {
        let g = ValueGauge::new(Labels::of("name", "temp"));
        assert_eq!(g.get(), 0.0);
        g.set(72.5);
        assert_eq!(g.get(), 72.5);
    }
}

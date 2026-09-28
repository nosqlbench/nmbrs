// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Monotonic u64 counter.

use crate::labels::Labels;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Counter {
    labels: Labels,
    value: AtomicU64,
}

impl Counter {
    pub fn new(labels: Labels) -> Self {
        Self {
            labels,
            value: AtomicU64::new(0),
        }
    }

    pub fn inc(&self) {
        self.value.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inc_by(&self, n: u64) {
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn labels(&self) -> &Labels {
        &self.labels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_inc() {
        let c = Counter::new(Labels::of("name", "test"));
        assert_eq!(c.get(), 0);
        c.inc();
        assert_eq!(c.get(), 1);
        c.inc_by(5);
        assert_eq!(c.get(), 6);
    }

    #[test]
    fn counter_labels() {
        let c = Counter::new(Labels::of("name", "ops_total"));
        assert_eq!(c.labels().get("name"), Some("ops_total"));
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! TUI reporter: receives base-cadence metrics snapshots and forwards
//! them to the TUI render thread via an mpsc channel. Used for
//! sparkline feeds, history rings, and the live percentile display.
//!
//! For per-window (10s / 1m / …) sample-weighted views the TUI
//! reads from a shared [`nmbrs_metrics::metrics_query::MetricsQuery`] —
//! see SRD-42.

use std::sync::mpsc;

use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;

/// A metrics reporter that forwards snapshots to the TUI via a channel.
pub struct TuiReporter {
    sender: mpsc::Sender<MetricSet>,
}

impl TuiReporter {
    /// Create a reporter + its receiving end.
    pub fn channel() -> (Self, mpsc::Receiver<MetricSet>) {
        let (sender, receiver) = mpsc::channel();
        (Self { sender }, receiver)
    }
}

impl Reporter for TuiReporter {
    fn report(&mut self, snapshot: &MetricSet) {
        // Non-blocking send — if the TUI is slow, drop snapshots.
        let _ = self.sender.send(snapshot.clone());
    }
}

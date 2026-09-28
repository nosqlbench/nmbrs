// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `FrameBroker` — single-source, multi-consumer fan-out for
//! [`MetricSet`] frames.
//!
//! ## Why
//!
//! The cadence reporter exposes its base-cadence frames through
//! `nmbrs_metrics::scheduler::Reporter::report(&MetricSet)`. The
//! existing [`crate::reporter::TuiReporter`] takes those frames
//! into a single `mpsc::Sender` whose `Receiver` belongs to the
//! TUI thread. That works for one consumer.
//!
//! Phase 2 of the [`crate::display_sink`] refactor introduces
//! mid-run sink swaps (Ctrl-T toggle): the active renderer can
//! change from a [`crate::log_only_sink::LogOnlySink`] to a
//! `TuiSink` (and back) without restarting the run. Each sink
//! that wants frames needs its own `Receiver`; `mpsc::Receiver`
//! is not clonable. Rather than build an opinionated fan-out
//! into the cadence reporter, this module supplies a tiny
//! [`Reporter`] wrapper that publishes each incoming frame to
//! every subscriber's `mpsc::Sender` and prunes any whose
//! `Receiver` has been dropped.
//!
//! Sinks that don't render frames at all (the current
//! `LogOnlySink`) simply don't subscribe — the broker's per-tick
//! cost is then a single `Mutex::lock` on an empty subscriber
//! list. Sinks that do subscribe pay one `MetricSet::clone` per
//! tick per subscriber. At a base cadence of 1 Hz with a handful
//! of sinks, that's well under any threshold worth optimising.
//!
//! ## Lifecycle
//!
//! ```text
//!   broker = FrameBroker::new()
//!   runner registers broker as the cadence reporter via
//!     observer.reporters() -> [(1s, Box<broker_clone>)]
//!   sink_a.start() ── broker.subscribe() → Receiver_a
//!   ... (sink_a runs, drains Receiver_a) ...
//!   sink_a.shutdown() — drops Receiver_a; broker prunes on next publish
//!   sink_b.start() ── broker.subscribe() → Receiver_b
//! ```

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;

use nmbrs_metrics::scheduler::Reporter;
use nmbrs_metrics::snapshot::MetricSet;

/// Multi-consumer fan-out for [`MetricSet`] frames.
///
/// Cloning a `FrameBroker` is cheap — it's an `Arc` to the
/// shared subscriber list — so the runner can register the
/// broker as a [`Reporter`] *and* hand a clone to the sink
/// supervisor for `subscribe()` calls.
#[derive(Clone, Default)]
pub struct FrameBroker {
    inner: Arc<Mutex<Vec<mpsc::Sender<MetricSet>>>>,
}

impl FrameBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a subscriber. Returns the receiving end of a fresh
    /// `mpsc` channel; the sink owns it for its lifetime. When
    /// the receiver is dropped (sink shutdown), the next
    /// `publish` prunes the dead sender from the broker.
    pub fn subscribe(&self) -> mpsc::Receiver<MetricSet> {
        let (tx, rx) = mpsc::channel();
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        rx
    }

    /// Send a frame to every live subscriber. Called by the
    /// `Reporter` impl from the cadence-scheduler thread; also
    /// directly callable by tests / synthetic drivers.
    pub fn publish(&self, frame: &MetricSet) {
        let mut subs = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // `retain` so dropped-receiver senders are pruned in the
        // same pass — no separate GC step. `MetricSet::clone` is
        // expected here; per the module-level comment the cost
        // is bounded by base-cadence frequency × subscriber count.
        subs.retain(|tx| tx.send(frame.clone()).is_ok());
    }

    /// Number of currently registered subscribers. Diagnostic /
    /// test surface only — do not branch on this from runtime code.
    #[allow(dead_code)] // used by unit tests
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Reporter for FrameBroker {
    fn report(&mut self, snapshot: &MetricSet) {
        // `Reporter` takes `&mut self`, but `publish` only needs
        // shared access (the inner `Mutex` provides interior
        // mutability). The signature mismatch is fine; the
        // cadence-scheduler thread is the sole `&mut self`
        // caller.
        self.publish(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn frame_reaches_every_subscriber() {
        let broker = FrameBroker::new();
        let rx_a = broker.subscribe();
        let rx_b = broker.subscribe();
        assert_eq!(broker.subscriber_count(), 2);

        let frame = MetricSet::at(Instant::now(), Duration::ZERO);
        broker.publish(&frame);

        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());
    }

    #[test]
    fn dropped_receiver_is_pruned_on_publish() {
        let broker = FrameBroker::new();
        let rx_a = broker.subscribe();
        let rx_b = broker.subscribe();
        assert_eq!(broker.subscriber_count(), 2);

        drop(rx_b);
        // `rx_b`'s sender is still in the list; `publish` prunes
        // it after the failed `send`.
        let frame = MetricSet::at(Instant::now(), Duration::ZERO);
        broker.publish(&frame);

        assert_eq!(broker.subscriber_count(), 1);
        assert!(rx_a.try_recv().is_ok());
    }

    #[test]
    fn subscribers_can_be_added_after_publishes() {
        let broker = FrameBroker::new();
        let frame = MetricSet::at(Instant::now(), Duration::ZERO);

        // No subscribers — frame is dropped silently.
        broker.publish(&frame);

        // Late subscriber sees only post-subscribe frames.
        let rx = broker.subscribe();
        broker.publish(&frame);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }
}

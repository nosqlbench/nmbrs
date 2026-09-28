// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Metrics snapshot scheduler with hierarchical frame coalescing.
//!
//! A dedicated thread captures frames at the base interval from the
//! component tree. Each reporter is registered at its own interval
//! (must be an exact multiple of the base). Schedule nodes accumulate
//! and coalesce frames for slower reporters.
//!
//! At every tick the scheduler also feeds the installed
//! [`CadenceReporter`] (SRD-42), which owns the windowed snapshot
//! store read by every consumer through
//! [`crate::metrics_query::MetricsQuery`].

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::cadence_reporter::CadenceReporter;
use crate::labels::Labels;
use crate::snapshot::MetricSet;

/// Trait for metrics reporters (external consumers: SQLite, CSV, etc.).
pub trait Reporter: Send + 'static {
    fn report(&mut self, snapshot: &MetricSet);
    fn flush(&mut self) {}

    /// Self-termination signal. After a [`report`](Reporter::report)
    /// that leaves this `true`, the subscriber's cadence-feed dispatch
    /// worker exits its loop (calling [`flush`](Reporter::flush) on the
    /// way out) — the subscriber receives no further pulses. A one-shot
    /// subscriber — e.g. a settle / stop evaluator that has set a
    /// terminal phase disposition — uses this to **unregister itself**
    /// without a self-join deadlock (it runs on the worker thread, so it
    /// cannot call `unsubscribe` on itself directly). Default `false`
    /// (a long-lived subscriber that never self-terminates).
    fn finished(&self) -> bool {
        false
    }
}

/// Capture function that produces per-component delta snapshots
/// from the component tree.
///
/// Returns one `(effective_labels, delta_snapshot)` per RUNNING
/// component that has instruments with data.
pub type CaptureFunc = Box<dyn Fn() -> Vec<(Labels, MetricSet)> + Send>;

/// A node in the schedule tree that accumulates and coalesces snapshots.
struct ScheduleNode {
    interval: Duration,
    accumulated: Vec<MetricSet>,
    accumulated_duration: Duration,
    reporters: Vec<Box<dyn Reporter>>,
    children: Vec<ScheduleNode>,
}

impl ScheduleNode {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            accumulated: Vec::new(),
            accumulated_duration: Duration::ZERO,
            reporters: Vec::new(),
            children: Vec::new(),
        }
    }

    /// Ingest a combined snapshot. Accumulate, and when the
    /// interval is satisfied, coalesce and emit.
    fn ingest(&mut self, snapshot: MetricSet) {
        self.accumulated_duration += snapshot.interval();
        self.accumulated.push(snapshot);

        if self.accumulated_duration >= self.interval {
            let coalesced = MetricSet::coalesce(&self.accumulated);
            self.accumulated.clear();
            self.accumulated_duration = Duration::ZERO;

            for reporter in &mut self.reporters {
                reporter.report(&coalesced);
            }
            for child in &mut self.children {
                child.ingest(coalesced.clone());
            }
        }
    }
}

/// Configuration for the snapshot scheduler.
pub struct SchedulerConfig {
    pub base_interval: Duration,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            base_interval: Duration::from_secs(1),
        }
    }
}

/// Builder for constructing a scheduler with reporters.
pub struct SchedulerBuilder {
    config: SchedulerConfig,
    reporters: Vec<(Duration, Box<dyn Reporter>)>,
    cadence_reporter: Option<Arc<CadenceReporter>>,
    cadence_tree: Option<crate::cadence::CadenceTree>,
}

impl Default for SchedulerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedulerBuilder {
    pub fn new() -> Self {
        Self {
            config: SchedulerConfig::default(),
            reporters: Vec::new(),
            cadence_reporter: None,
            cadence_tree: None,
        }
    }

    pub fn base_interval(mut self, interval: Duration) -> Self {
        self.config.base_interval = interval;
        self
    }

    pub fn add_reporter(mut self, interval: Duration, reporter: impl Reporter) -> Self {
        self.reporters.push((interval, Box::new(reporter)));
        self
    }

    /// Install the cadence reporter that owns the windowed snapshot
    /// store. On every scheduler tick, captured per-component
    /// snapshots are fed into this reporter, which cascades them
    /// up the cadence tree and publishes closed windows to
    /// [`crate::metrics_query::MetricsQuery`] readers.
    pub fn with_cadence_reporter(mut self, reporter: Arc<CadenceReporter>) -> Self {
        self.cadence_reporter = Some(reporter);
        self
    }

    /// Install a cadence tree (SRD-42 §"Tree Construction"). When set,
    /// `build()` constructs a chained schedule where each layer feeds
    /// the next via [`ScheduleNode::ingest`] rather than coalescing
    /// from base frames independently. Hidden layers participate in
    /// accumulation but have no reporters of their own.
    ///
    /// Reporters at intervals matching a tree layer attach at that
    /// layer; reporters at intervals outside the tree continue to
    /// attach as flat children of root (backward-compatible).
    pub fn with_cadence_tree(mut self, tree: crate::cadence::CadenceTree) -> Self {
        self.cadence_tree = Some(tree);
        self
    }

    /// Build the schedule tree and return a handle.
    ///
    /// The scheduler is not yet running — call `start()` on the handle.
    pub fn build(self, capture: CaptureFunc) -> SchedulerHandle {
        let base = self.config.base_interval;
        let mut root = ScheduleNode::new(base);

        let mut by_interval: std::collections::BTreeMap<Duration, Vec<Box<dyn Reporter>>> =
            std::collections::BTreeMap::new();
        for (interval, reporter) in self.reporters {
            by_interval.entry(interval).or_default().push(reporter);
        }

        // Reporters that match the base interval always live on root.
        if let Some(reps) = by_interval.remove(&base) {
            root.reporters.extend(reps);
        }

        // If a cadence tree was provided, build the chained sub-tree.
        // Walking layers largest → smallest builds the chain from the
        // leaf inward, so each node owns its single child.
        if let Some(tree) = self.cadence_tree {
            let mut chain: Option<ScheduleNode> = None;
            for layer in tree.layers().iter().rev() {
                if layer.interval == base {
                    // Base-interval "layer" is just the root itself —
                    // any reporters at that interval are already on
                    // root. Skip without nesting.
                    continue;
                }
                assert!(
                    layer.interval.as_millis() % base.as_millis() == 0,
                    "cadence layer {:?} must be an exact multiple of base {:?}",
                    layer.interval,
                    base,
                );
                let mut node = ScheduleNode::new(layer.interval);
                if !layer.hidden
                    && let Some(reps) = by_interval.remove(&layer.interval)
                {
                    node.reporters = reps;
                }
                if let Some(child) = chain.take() {
                    node.children.push(child);
                }
                chain = Some(node);
            }
            if let Some(top) = chain {
                root.children.push(top);
            }
        }

        // Reporters not consumed by the tree (intervals outside it,
        // or no tree at all) attach as flat children of root — same
        // behavior as before this layering existed.
        for (interval, reporters) in by_interval {
            assert!(
                interval.as_millis() % base.as_millis() == 0,
                "reporter interval {:?} must be an exact multiple of base {:?}",
                interval,
                base
            );
            let mut node = ScheduleNode::new(interval);
            node.reporters = reporters;
            root.children.push(node);
        }

        SchedulerHandle {
            root: Arc::new(Mutex::new(root)),
            capture,
            base_interval: base,
            running: Arc::new(Mutex::new(false)),
            cadence_reporter: self.cadence_reporter,
        }
    }
}

/// Handle to a running (or startable) scheduler.
pub struct SchedulerHandle {
    root: Arc<Mutex<ScheduleNode>>,
    capture: CaptureFunc,
    base_interval: Duration,
    running: Arc<Mutex<bool>>,
    cadence_reporter: Option<Arc<CadenceReporter>>,
}

impl SchedulerHandle {
    /// Reference to the installed cadence reporter, if any.
    pub fn cadence_reporter(&self) -> Option<&Arc<CadenceReporter>> {
        self.cadence_reporter.as_ref()
    }

    /// Flush a retiring component's final delta through the
    /// cadence reporter (if present). Called from the executor
    /// thread when a phase completes, outside the scheduler tick
    /// loop.
    pub fn flush_component(&self, labels: &Labels, final_delta: MetricSet) {
        if let Some(reporter) = &self.cadence_reporter {
            reporter.ingest(labels, final_delta);
        }
    }

    /// Start the scheduler on a dedicated thread.
    ///
    /// Returns a `StopHandle` that can be used to shut down.
    pub fn start(self) -> StopHandle {
        let root = self.root.clone();
        let root_for_stop = self.root;
        let capture = self.capture;
        let interval = self.base_interval;
        let running = self.running.clone();
        let cadence_reporter = self.cadence_reporter.clone();
        let cadence_reporter_for_stop = self.cadence_reporter.clone();

        let (frame_tx, frame_rx) = std::sync::mpsc::channel::<MetricSet>();
        // Hot-path split (SRD-102 §6): the `timing` thread only *captures*
        // deltas and enqueues here; a single ordered `io`-pool worker drains
        // this channel and does the potentially-slow reporter delivery
        // (report()/ingest — CSV/SQLite/HTTP), keeping the timing thread's
        // critical section to capture + enqueue.
        let (io_tx, io_rx) = std::sync::mpsc::channel::<MetricSet>();
        // Stop signal: `running` (a Mutex<bool>) gates the loop and the
        // Condvar wakes the timing thread out of its inter-tick wait
        // immediately on stop instead of dwelling a full base interval.
        // Condvar + Arc<Mutex> are `Sync`, so a session host can still stop
        // through a shared `Arc<StopHandle>` now that the wait is a std
        // primitive rather than a tokio `Notify`.
        let stop_cv = Arc::new(Condvar::new());
        let stop_cv_thread = stop_cv.clone();
        // The timing thread fires `done` AFTER its final flush, so the sync
        // `stop()` can wait for the trailing window to land (summary reports
        // read complete data).
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        *running.lock().unwrap_or_else(|e| e.into_inner()) = true;

        let stop_running = running.clone();
        // Capture a runtime handle (start() is called from the async runner)
        // so the std timing thread can `block_on` the one async shutdown call
        // (`cadence_reporter.shutdown_flush`). The timing thread is never a
        // runtime worker, so blocking on it is safe.
        let rt_handle = tokio::runtime::Handle::try_current().ok();

        // The ordered `io`-pool reporter worker. Exits when the timing thread
        // drops `io_tx` on shutdown, after it has delivered every enqueued
        // snapshot. A single consumer preserves reporter delivery order (the
        // `io` pool's thread *count* is capacity for other future consumers).
        let root_io = root.clone();
        let io_handle = crate::thread_pools::global()
            .spawn("io", "reporter", move || {
                while let Ok(snapshot) = io_rx.recv() {
                    let mut node = root_io.lock().unwrap_or_else(|e| e.into_inner());
                    for reporter in &mut node.reporters {
                        reporter.report(&snapshot);
                    }
                    for child in &mut node.children {
                        child.ingest(snapshot.clone());
                    }
                }
            })
            .expect("spawn io reporter thread");

        // The cadence tick loop runs on a dedicated `timing`-pool OS thread
        // (SRD-102): realtime scheduling policy + affinity applied at spawn,
        // never sharing duty with the async worker runtime, so timer wake-ups
        // are not queued behind workload fibers.
        let sched_thread = crate::thread_pools::global()
            .spawn_timing("cadence", move || {
                // Divergence surveillance (SRD-102 §6): compare the nominal
                // deadline (`scheduled_ts`) to the actual fire instant. If
                // they diverge by more than 250 ms the timing thread is being
                // delayed (CPU starvation / oversleep). Warn — rate-limited —
                // so the operator can correlate anomalies with scheduler
                // health. Recorded snapshot intervals stay at the nominal
                // cadence (canonical cadence as a matter of record); the
                // divergence is reported out-of-band and stamped on the
                // snapshot as scheduled_ts vs actual_ts.
                let divergence_threshold = Duration::from_millis(250);
                let divergence_warn_min_interval = Duration::from_secs(60);
                let mut last_divergence_warn: Option<Instant> = None;
                let mut next_tick = Instant::now() + interval;
                loop {
                    // Interruptible wait to the absolute `next_tick`. Holds the
                    // `running` guard across `wait_timeout` (which atomically
                    // releases + reacquires), so a stop set by `stop()` is seen
                    // the instant the Condvar wakes us.
                    let fire = {
                        let mut guard = stop_running.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if !*guard {
                                break false;
                            }
                            let now = Instant::now();
                            if now >= next_tick {
                                break true;
                            }
                            let (g, _) = stop_cv_thread
                                .wait_timeout(guard, next_tick - now)
                                .unwrap_or_else(|e| e.into_inner());
                            guard = g;
                        }
                    };
                    if !fire {
                        break;
                    }

                    let scheduled = next_tick;
                    // Fixed-rate: advance by the nominal interval regardless of
                    // when we actually woke, so cadence does not drift.
                    next_tick += interval;
                    let actual = Instant::now();

                    if let Some(divergence) = divergence_warning(
                        scheduled,
                        actual,
                        divergence_threshold,
                        divergence_warn_min_interval,
                        last_divergence_warn,
                    ) {
                        last_divergence_warn = Some(actual);
                        crate::diag::warn(&format!(
                            "scheduler cadence divergence: scheduled vs actual off \
                             by {:?} (>250ms) — snapshots still recorded at nominal \
                             cadence; the `timing` pool thread is being delayed \
                             (CPU starvation / oversleep)",
                            divergence,
                        ));
                    }

                    // Drain async snapshot channel (lifecycle flushes from
                    // executor) → offload delivery to the io worker.
                    while let Ok(snapshot) = frame_rx.try_recv() {
                        let _ = io_tx.send(snapshot);
                    }

                    // Capture per-component deltas from the tree.
                    let component_snapshots = (capture)();

                    // Feed each per-component delta into the cadence reporter
                    // (single writer of windowed snapshots — a non-blocking
                    // crossbeam send, kept on the timing thread as part of
                    // capture).
                    if let Some(ref cr) = cadence_reporter {
                        for (labels, snapshot) in &component_snapshots {
                            cr.ingest(labels, snapshot.clone());
                        }
                    }

                    // Merge component snapshots into one combined snapshot for
                    // the scheduler-tree reporters (CSV / SQLite / etc.), stamp
                    // the scheduled/actual timestamp pair, and hand off to io.
                    let all_snapshots: Vec<MetricSet> = component_snapshots
                        .into_iter()
                        .map(|(_, snapshot)| snapshot)
                        .collect();
                    let mut combined = if all_snapshots.is_empty() {
                        MetricSet::new(interval)
                    } else {
                        let mut merged = MetricSet::coalesce(&all_snapshots);
                        // Interval reflects the scheduler interval, not the sum
                        // from coalesce (which sums intervals).
                        merged.set_interval(interval);
                        merged
                    };
                    combined.set_scheduled_ts(scheduled);
                    let _ = io_tx.send(combined);
                }

                // Final capture before shutdown: ensures short-lived phases
                // that completed between ticks get their data to reporters.
                {
                    let component_snapshots = (capture)();
                    if let Some(ref cr) = cadence_reporter {
                        for (labels, snapshot) in &component_snapshots {
                            cr.ingest(labels, snapshot.clone());
                        }
                    }
                    let all_snapshots: Vec<MetricSet> = component_snapshots
                        .into_iter()
                        .map(|(_, snapshot)| snapshot)
                        .collect();
                    if !all_snapshots.is_empty() {
                        let mut merged = MetricSet::coalesce(&all_snapshots);
                        merged.set_interval(interval);
                        let _ = io_tx.send(merged);
                    }
                }

                // No more steady-state deliveries: close the io channel and
                // join the reporter worker so every enqueued snapshot has
                // landed before the final flush. After this the timing thread
                // is the sole toucher of `root`.
                drop(io_tx);
                let _ = io_handle.join();

                // Force-close any unpromoted cadence partials so the trailing
                // window is not lost. The only async call — `block_on` on this
                // dedicated (non-runtime) thread.
                if let (Some(h), Some(cr)) = (rt_handle.as_ref(), cadence_reporter.as_ref()) {
                    h.block_on(cr.shutdown_flush());
                }

                // Drain any remaining async frames directly (io worker joined).
                while let Ok(snapshot) = frame_rx.try_recv() {
                    let mut r = root.lock().unwrap_or_else(|e| e.into_inner());
                    for reporter in &mut r.reporters {
                        reporter.report(&snapshot);
                    }
                    for child in &mut r.children {
                        child.ingest(snapshot.clone());
                    }
                }
                // Flush all reporters on shutdown.
                flush_tree(&mut root.lock().unwrap_or_else(|e| e.into_inner()));
                // Trailing window has landed — release a waiting `stop()`.
                let _ = done_tx.send(());
            })
            .expect("spawn timing scheduler thread");

        StopHandle {
            running: self.running,
            cadence_reporter: cadence_reporter_for_stop,
            root: root_for_stop,
            task: Mutex::new(Some(sched_thread)),
            frame_tx,
            stop_cv,
            done_rx: Mutex::new(Some(done_rx)),
        }
    }
}

/// SRD-102 §6 divergence-warning decision (extracted for testability). The
/// nominal `scheduled` deadline vs the `actual` fire instant must diverge by
/// more than `threshold`, AND `min_interval` must have elapsed since the last
/// warning (rate-limit so sustained drift warns once per window, not every
/// tick). Returns the divergence magnitude when a warning is due.
fn divergence_warning(
    scheduled: Instant,
    actual: Instant,
    threshold: Duration,
    min_interval: Duration,
    last_warn: Option<Instant>,
) -> Option<Duration> {
    let divergence = if actual >= scheduled {
        actual - scheduled
    } else {
        scheduled - actual
    };
    let due = last_warn
        .map(|t| actual.duration_since(t) >= min_interval)
        .unwrap_or(true);
    (divergence > threshold && due).then_some(divergence)
}

fn flush_tree(node: &mut ScheduleNode) {
    for reporter in &mut node.reporters {
        reporter.flush();
    }
    for child in &mut node.children {
        flush_tree(child);
    }
}

/// Handle to stop a running scheduler.
pub struct StopHandle {
    running: Arc<Mutex<bool>>,
    cadence_reporter: Option<Arc<CadenceReporter>>,
    #[allow(dead_code)] // retained for future direct-flush access
    root: Arc<Mutex<ScheduleNode>>,
    /// The scheduler thread handle — a dedicated `timing`-pool OS thread
    /// (SRD-102), not a runtime task. Interior-mutable so the session host
    /// can stop the scheduler through a shared `Arc<StopHandle>` (SRD-88 —
    /// host owns the session-tier scheduler; executions only `report_frame`).
    /// `take`n by whichever of `stop` / `drop` runs first; the other sees
    /// `None` and is a no-op (idempotent).
    task: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Channel for async frame delivery — the executor sends frames here
    /// instead of writing to reporters inline. The scheduler thread drains
    /// this channel on each tick.
    frame_tx: std::sync::mpsc::Sender<MetricSet>,
    /// Wakes the timing thread out of its inter-tick Condvar wait so shutdown
    /// is prompt instead of waiting out a base interval. Paired with the
    /// `running` Mutex the thread waits on.
    stop_cv: Arc<Condvar>,
    /// Signalled by the task after its final flush; `stop()` waits on it
    /// so the trailing window is committed before it returns.
    done_rx: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl StopHandle {
    /// Stop the scheduler and join the capture thread. `&self` +
    /// interior-mutable `thread` so a session host holding a shared
    /// `Arc<StopHandle>` can stop it without sole ownership.
    /// Idempotent — a second call (or `drop` after) sees `thread`
    /// already taken and no-ops.
    ///
    /// ASYNC on purpose — the wait must YIELD to the runtime, never
    /// block it. The timing thread's final flush `block_on`s
    /// `CadenceReporter::shutdown_flush`, whose ack comes from the
    /// reporter's OWNER — a tokio task on the caller's runtime. A
    /// blocking wait here deadlocked a CURRENT-THREAD runtime three
    /// ways: this (the only runtime) thread parked in `recv()`, the
    /// timing thread parked awaiting the owner's ack, and the owner
    /// task unable to run on the parked runtime. Multi-thread runtimes
    /// escaped via `block_in_place` + spare workers, which is why only
    /// single-threaded harnesses hung. The yield-poll below lets
    /// same-runtime tasks progress on every flavor; this is a
    /// session-end one-shot, so the 2 ms poll cadence touches no hot
    /// path.
    pub async fn stop(&self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.stop_cv.notify_all(); // wake the inter-tick wait
        // Wait for the task's final flush to land (the trailing window).
        let done = self
            .done_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(done) = done {
            loop {
                match done.try_recv() {
                    Ok(()) | Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                    }
                }
            }
        }
        // The task has finished; drop its handle (no abort needed).
        let _ = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Reference to the cadence reporter, if any.
    ///
    /// Remains valid and queryable after the scheduler is stopped.
    pub fn cadence_reporter(&self) -> Option<&Arc<CadenceReporter>> {
        self.cadence_reporter.as_ref()
    }

    /// Deliver a frame to reporters asynchronously.
    ///
    /// The frame is enqueued on a channel and processed by the
    /// scheduler thread on its next tick. This never blocks the
    /// caller — safe to call from tokio worker threads.
    pub fn report_frame(&self, snapshot: &MetricSet) {
        let _ = self.frame_tx.send(snapshot.clone());
    }
}

impl Drop for StopHandle {
    fn drop(&mut self) {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = false;
        self.stop_cv.notify_all(); // wake the inter-tick wait
        let done = self
            .done_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(done) = done {
            // Drop is sync, so it can only WAIT where blocking is safe:
            // OUTSIDE any tokio runtime, a plain blocking recv (the timing
            // thread's `done` needs no progress from this thread — except
            // through the CadenceReporter owner task, which lives on a
            // runtime this thread is not part of). INSIDE a runtime,
            // blocking would starve exactly the task the timing thread's
            // final flush awaits (see `stop`'s doc), so DETACH instead:
            // the timing thread completes its flush on its own; only the
            // ordering guarantee ("flush landed before return") is
            // forfeited, and a caller that needs that guarantee uses the
            // async `stop()`.
            if tokio::runtime::Handle::try_current().is_err() {
                let _ = done.recv();
            }
        }
        // The timing thread signals `done` as its last act, then returns —
        // drop the join handle (detach); no abort exists for an OS thread and
        // none is needed.
        let _ = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::MetricValue;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct CountingReporter {
        count: Arc<AtomicU64>,
    }

    impl Reporter for CountingReporter {
        fn report(&mut self, _snapshot: &MetricSet) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn mock_capture() -> Vec<(Labels, MetricSet)> {
        let mut s = MetricSet::new(Duration::from_millis(100));
        s.insert_counter("ops", Labels::default(), 10, Instant::now());
        vec![(Labels::of("phase", "test"), s)]
    }

    fn empty_snapshot(interval: Duration) -> MetricSet {
        MetricSet::new(interval)
    }

    #[test]
    fn divergence_under_threshold_does_not_warn() {
        let scheduled = Instant::now();
        let actual = scheduled + Duration::from_millis(100); // < 250ms
        assert!(
            divergence_warning(
                scheduled,
                actual,
                Duration::from_millis(250),
                Duration::from_secs(60),
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn divergence_over_threshold_warns_first_time() {
        let scheduled = Instant::now();
        let actual = scheduled + Duration::from_millis(300); // > 250ms
        let d = divergence_warning(
            scheduled,
            actual,
            Duration::from_millis(250),
            Duration::from_secs(60),
            None,
        );
        assert_eq!(d, Some(Duration::from_millis(300)));
    }

    #[test]
    fn divergence_is_rate_limited_within_window() {
        let scheduled = Instant::now();
        let actual = scheduled + Duration::from_millis(400);
        // A warning fired 10s ago; the 60s window has not elapsed → suppressed.
        let last_warn = Some(actual - Duration::from_secs(10));
        assert!(
            divergence_warning(
                scheduled,
                actual,
                Duration::from_millis(250),
                Duration::from_secs(60),
                last_warn,
            )
            .is_none()
        );
        // Once the window elapses, it warns again.
        let last_warn = Some(actual - Duration::from_secs(61));
        assert!(
            divergence_warning(
                scheduled,
                actual,
                Duration::from_millis(250),
                Duration::from_secs(60),
                last_warn,
            )
            .is_some()
        );
    }

    #[test]
    fn scheduled_ts_is_stamped_on_tick_snapshots() {
        // The scheduler stamps `scheduled_ts` on the combined snapshot each
        // tick (captured_at is the actual fire instant). Verify the MetricSet
        // carries the pair.
        let mut s = MetricSet::new(Duration::from_millis(100));
        assert!(s.scheduled_ts().is_none());
        let sched = Instant::now();
        s.set_scheduled_ts(sched);
        assert_eq!(s.scheduled_ts(), Some(sched));
        // actual_ts aliases captured_at.
        assert_eq!(s.actual_ts(), s.captured_at());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_builds_and_reports() {
        let count = Arc::new(AtomicU64::new(0));
        let c = count.clone();
        let handle = SchedulerBuilder::new()
            .base_interval(Duration::from_millis(100))
            .add_reporter(Duration::from_millis(100), CountingReporter { count: c })
            .build(Box::new(mock_capture));

        let stop = handle.start();
        tokio::time::sleep(Duration::from_millis(350)).await;
        stop.stop().await;

        let c = count.load(Ordering::Relaxed);
        assert!((2..=5).contains(&c), "expected ~3 reports, got {c}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_feeds_cadence_reporter() {
        use crate::cadence::{CadenceTree, Cadences};

        let tree = CadenceTree::plan_default(Cadences::new(&[Duration::from_millis(100)]).unwrap());
        let reporter = Arc::new(CadenceReporter::new(tree));
        let handle = SchedulerBuilder::new()
            .base_interval(Duration::from_millis(100))
            .with_cadence_reporter(reporter.clone())
            .build(Box::new(mock_capture));

        let stop = handle.start();
        tokio::time::sleep(Duration::from_millis(350)).await;
        stop.stop().await;

        // Reporter received ingests — has the component tracked.
        let components = reporter.component_labels();
        assert_eq!(components.len(), 1);
        // The 100ms cadence should have at least one closed snapshot.
        let component = &components[0];
        let latest = reporter
            .latest(component, Duration::from_millis(100))
            .expect("cadence reporter should have a closed 100ms snapshot");
        let ops_total = match latest
            .family("ops")
            .unwrap()
            .metrics()
            .next()
            .unwrap()
            .point()
            .unwrap()
            .value()
        {
            MetricValue::Counter(c) => c.cumulative,
            _ => panic!("expected counter"),
        };
        assert_eq!(ops_total, 10, "one tick = 10");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_coalesces_for_slow_reporter() {
        let fast_count = Arc::new(AtomicU64::new(0));
        let slow_count = Arc::new(AtomicU64::new(0));
        let fc = fast_count.clone();
        let sc = slow_count.clone();

        let handle = SchedulerBuilder::new()
            .base_interval(Duration::from_millis(50))
            .add_reporter(Duration::from_millis(50), CountingReporter { count: fc })
            .add_reporter(Duration::from_millis(200), CountingReporter { count: sc })
            .build(Box::new(|| {
                vec![(
                    Labels::of("phase", "test"),
                    empty_snapshot(Duration::from_millis(50)),
                )]
            }));

        let stop = handle.start();
        tokio::time::sleep(Duration::from_millis(450)).await;
        stop.stop().await;

        let fast = fast_count.load(Ordering::Relaxed);
        let slow = slow_count.load(Ordering::Relaxed);
        assert!(fast >= 6, "fast should get many reports, got {fast}");
        assert!((1..=3).contains(&slow), "slow should get ~2, got {slow}");
    }

    /// With a CadenceTree installed, a slow reporter at the largest
    /// declared cadence is fed *through* the chain (root → smallest
    /// → … → largest). Functionally indistinguishable from the flat
    /// arrangement at the consumer level — same number of reports,
    /// same coalesced data — but internally the largest layer's
    /// accumulation is bounded by the next-smaller cadence, not by
    /// every base frame.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_chained_tree_delivers_to_largest_cadence() {
        use crate::cadence::{CadenceTree, Cadences};

        let small_count = Arc::new(AtomicU64::new(0));
        let large_count = Arc::new(AtomicU64::new(0));
        let sc = small_count.clone();
        let lc = large_count.clone();

        // Cadences: 100ms (smallest declared) and 400ms (largest).
        // Ratio 4 — well under default fan-in, no hidden inserts.
        let tree = CadenceTree::plan_default(
            Cadences::new(&[Duration::from_millis(100), Duration::from_millis(400)]).unwrap(),
        );

        let handle = SchedulerBuilder::new()
            .base_interval(Duration::from_millis(100))
            .with_cadence_tree(tree)
            .add_reporter(Duration::from_millis(100), CountingReporter { count: sc })
            .add_reporter(Duration::from_millis(400), CountingReporter { count: lc })
            .build(Box::new(|| {
                vec![(
                    Labels::of("phase", "test"),
                    empty_snapshot(Duration::from_millis(100)),
                )]
            }));

        let stop = handle.start();
        tokio::time::sleep(Duration::from_millis(900)).await;
        stop.stop().await;

        let small = small_count.load(Ordering::Relaxed);
        let large = large_count.load(Ordering::Relaxed);
        // ~9 base ticks → smallest fires every tick (≥6) and
        // largest fires every 4 (≥1, ≤3).
        assert!(small >= 6, "smallest cadence reports = {small}");
        assert!(
            (1..=3).contains(&large),
            "largest cadence reports = {large}"
        );
    }

    /// Hidden intermediate layers (auto-inserted by the planner)
    /// participate in accumulation but never deliver to a reporter.
    /// Verify that a reporter only at the *largest* declared cadence
    /// still gets its expected report count even when a hidden
    /// layer sits between it and the smallest cadence.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scheduler_hidden_layers_pass_through_to_visible_reporters() {
        use crate::cadence::{CadenceTree, Cadences};

        let large_count = Arc::new(AtomicU64::new(0));
        let lc = large_count.clone();

        // 50ms → 1500ms is ratio 30 — exceeds default K=20, so the
        // planner inserts a hidden intermediate. Ensures the chain
        // flows through it correctly.
        let tree = CadenceTree::plan_default(
            Cadences::new(&[Duration::from_millis(50), Duration::from_millis(1500)]).unwrap(),
        );
        // Sanity check the planner actually inserted one.
        let inserted: Vec<_> = tree.hidden().collect();
        assert!(!inserted.is_empty(), "test relies on hidden insertion");

        let handle = SchedulerBuilder::new()
            .base_interval(Duration::from_millis(50))
            .with_cadence_tree(tree)
            .add_reporter(Duration::from_millis(1500), CountingReporter { count: lc })
            .build(Box::new(|| {
                vec![(
                    Labels::of("phase", "test"),
                    empty_snapshot(Duration::from_millis(50)),
                )]
            }));

        let stop = handle.start();
        tokio::time::sleep(Duration::from_millis(3300)).await;
        stop.stop().await;

        let large = large_count.load(Ordering::Relaxed);
        assert!(large >= 1, "largest reporter saw 0 frames — chain broken");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn flush_component_routes_to_cadence_reporter() {
        use crate::cadence::{CadenceTree, Cadences};

        let tree = CadenceTree::plan_default(Cadences::new(&[Duration::from_secs(1)]).unwrap());
        let reporter = Arc::new(CadenceReporter::new(tree));
        let handle = SchedulerBuilder::new()
            .with_cadence_reporter(reporter.clone())
            .build(Box::new(Vec::new));

        // Flush without starting — simulates lifecycle retirement
        let labels = Labels::of("phase", "done");
        let mut snapshot = MetricSet::new(Duration::from_secs(1));
        snapshot.insert_counter("final_ops", Labels::default(), 42, Instant::now());
        handle.flush_component(&labels, snapshot);
        reporter.flush_for_tests();

        // The flush went straight into the reporter's smallest
        // cadence accumulator and promoted (interval matched).
        let latest = reporter
            .latest(&labels, Duration::from_secs(1))
            .expect("flush should produce a closed snapshot");
        assert!(latest.family("final_ops").is_some());
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Phase-end trigger registry.
//!
//! A simple event/callback registry that any subsystem can
//! attach a callback to. Callbacks fire after every
//! `phase_completed` and `phase_failed` event from the
//! executor. Triggers are dispatched on a background worker
//! thread so a slow callback (e.g. re-rendering a plot
//! against the live session.db) never blocks the run loop.
//!
//! ## Why a separate registry instead of new `RunObserver`
//! methods
//!
//! `RunObserver` is the surface for *display* observers (TUI,
//! log-only, stderr). A trigger is *behavioral* — it runs work
//! in response to lifecycle events without contributing to
//! display rendering. Trying to merge the two via a generic
//! observer interface forces every plot author to implement
//! the noisy `phase_starting` / `set_status_line` / `reporters`
//! surface. The registry is the focused alternative.
//!
//! ## Lifecycle
//!
//! - [`register`] adds a trigger and returns a [`TriggerId`].
//! - [`unregister`] removes a trigger by id (no-op if absent).
//! - The executor calls [`fire_phase_completed`] /
//!   [`fire_phase_failed`] immediately after the observer's
//!   matching callback. Triggers run on a single worker
//!   thread in FIFO registration order so a panic in one
//!   trigger doesn't take down the others (each call is
//!   `catch_unwind`-guarded).
//!
//! ## Synchronization
//!
//! The registry sits behind a `std::sync::Mutex` for
//! registration; the worker thread snapshots the trigger
//! list on each event so a registration / unregistration mid-
//! event won't dirty the dispatch. Total cost per phase end
//! is one channel send + one Vec clone of `Arc<Trigger>`.

use std::any::Any;
use std::panic;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Trigger callback. Implementors fire whenever a phase
/// completes (success or failure). The implementation should
/// be cheap to set up — long work runs on the worker thread,
/// not the executor.
pub trait PhaseEndTrigger: Send + Sync + 'static {
    /// Fire the trigger. `event` carries the phase identity
    /// and outcome; the implementation reads whatever live
    /// state it needs (the session db, the metrics dir) on
    /// its own.
    fn fire(&self, event: &PhaseEndEvent);

    /// Human-readable trigger name for logging / debugging.
    /// Default returns the type name via `Any` downcast hint.
    fn name(&self) -> &str {
        "phase-end-trigger"
    }
}

/// What the executor knows about a finished phase.
#[derive(Debug, Clone)]
pub struct PhaseEndEvent {
    pub phase_name: String,
    pub phase_labels: String,
    pub outcome: PhaseOutcome,
    pub duration_secs: f64,
}

/// Outcome flavor — success, failure, or skip-equivalent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhaseOutcome {
    Completed,
    Failed { error: String },
}

/// Opaque registration handle returned by [`register`]. Pass
/// to [`unregister`] to remove the trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TriggerId(u64);

struct Entry {
    id: TriggerId,
    trigger: Arc<dyn PhaseEndTrigger>,
}

struct Registry {
    next_id: u64,
    triggers: Vec<Entry>,
    dispatch: Option<mpsc::Sender<PhaseEndEvent>>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();

/// Lock-free mirror of `Registry::triggers.len()`, kept in sync under the
/// registry lock by [`register`] / [`unregister`]. [`fire`] reads this
/// FIRST so the common case — no triggers registered, which is every
/// concurrent-workload session today — is a single relaxed atomic load
/// with **no lock**: concurrent executions never synchronize on the
/// global registry when firing phase-end events (SRD-88 "session
/// services are lock-free for concurrent workloads").
static TRIGGER_COUNT: AtomicUsize = AtomicUsize::new(0);

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            next_id: 1,
            triggers: Vec::new(),
            dispatch: None,
        })
    })
}

/// Register a phase-end trigger. Returns a [`TriggerId`] the
/// caller can pass to [`unregister`] later. Idempotent within
/// a single registration — the same trigger object registered
/// twice fires twice.
///
/// The first registration also spawns the worker thread; it
/// stays alive for the rest of the process so subsequent
/// registrations are cheap.
pub fn register(trigger: Arc<dyn PhaseEndTrigger>) -> TriggerId {
    let mut reg = registry()
        .lock()
        .expect("phase-end-triggers registry poisoned");
    let id = TriggerId(reg.next_id);
    reg.next_id += 1;
    reg.triggers.push(Entry { id, trigger });
    TRIGGER_COUNT.store(reg.triggers.len(), Ordering::Release);
    // Lazy worker startup — only when there's at least one
    // trigger. The thread reads events forever; it shuts down
    // when the channel sender is dropped (process exit).
    if reg.dispatch.is_none() {
        let (tx, rx) = mpsc::channel::<PhaseEndEvent>();
        reg.dispatch = Some(tx);
        std::thread::Builder::new()
            .name("phase-end-trigger-worker".into())
            .spawn(move || dispatch_loop(rx))
            .expect("spawn phase-end-trigger worker");
    }
    id
}

/// Remove a previously-registered trigger. No-op when the id
/// doesn't match anything currently registered (already
/// removed, never registered, or freed by another caller).
pub fn unregister(id: TriggerId) {
    let mut reg = registry()
        .lock()
        .expect("phase-end-triggers registry poisoned");
    reg.triggers.retain(|e| e.id != id);
    TRIGGER_COUNT.store(reg.triggers.len(), Ordering::Release);
}

/// Drain the registry — used by integration tests that need
/// trigger isolation between cases. Production code should
/// not call this.
#[cfg(test)]
pub fn reset_for_tests() {
    let mut reg = registry()
        .lock()
        .expect("phase-end-triggers registry poisoned");
    reg.triggers.clear();
    TRIGGER_COUNT.store(0, Ordering::Release);
    reg.next_id = 1;
    // We deliberately leave the dispatch channel alive — the
    // worker thread is fine sitting idle on an empty channel.
}

/// Fire the trigger chain for a successful phase. Called by
/// the executor right after `observer.phase_completed(...)`.
pub fn fire_phase_completed(name: &str, labels: &str, duration_secs: f64) {
    fire(PhaseEndEvent {
        phase_name: name.to_string(),
        phase_labels: labels.to_string(),
        outcome: PhaseOutcome::Completed,
        duration_secs,
    });
}

/// Fire the trigger chain for a failed phase. Called by the
/// executor right after `observer.phase_failed(...)`.
pub fn fire_phase_failed(name: &str, labels: &str, error: &str) {
    fire(PhaseEndEvent {
        phase_name: name.to_string(),
        phase_labels: labels.to_string(),
        outcome: PhaseOutcome::Failed {
            error: error.to_string(),
        },
        duration_secs: 0.0,
    });
}

fn fire(event: PhaseEndEvent) {
    // Lock-free fast path: no triggers registered ⇒ a relaxed atomic
    // load and return. Concurrent executions firing phase-end events
    // never contend on the global registry lock in the common case.
    if TRIGGER_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let reg = registry()
        .lock()
        .expect("phase-end-triggers registry poisoned");
    if reg.triggers.is_empty() {
        return;
    }
    if let Some(tx) = reg.dispatch.as_ref() {
        // Send to the worker. A full channel would only
        // happen if the worker is wedged for many seconds;
        // we'd rather drop a single event than block the
        // executor.
        let _ = tx.send(event);
    }
}

fn dispatch_loop(rx: mpsc::Receiver<PhaseEndEvent>) {
    // Bounded snapshot to limit memory in long-running runs.
    loop {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(event) => {
                // Snapshot the trigger list so a registration
                // / unregistration during dispatch can't
                // mutate the slice underneath us.
                let snap: Vec<Arc<dyn PhaseEndTrigger>> = {
                    let reg = match registry().lock() {
                        Ok(r) => r,
                        Err(_) => return, // registry poisoned — worker exits
                    };
                    reg.triggers.iter().map(|e| e.trigger.clone()).collect()
                };
                for trigger in snap {
                    // Per-trigger catch_unwind so a panic in
                    // one doesn't drop subsequent ones.
                    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                        trigger.fire(&event);
                    }));
                    if let Err(payload) = result {
                        let msg = payload_to_message(payload);
                        // Surface the failure but don't take
                        // the worker down. The session log
                        // sink picks this up via diag!.
                        crate::diag!(
                            crate::observer::LogLevel::Warn,
                            "phase-end trigger '{name}' panicked: {msg}",
                            name = trigger.name(),
                        );
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Idle wakeup — loop back. Keeps the thread
                // responsive to a clean process shutdown
                // without spamming CPU on busy waits.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Sender dropped — process is exiting. Quiet
                // exit.
                return;
            }
        }
    }
}

fn payload_to_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    /// Counter trigger — records how many times it fired and
    /// the names of the phases it saw.
    struct CountingTrigger {
        count: Arc<AtomicUsize>,
        names: Arc<StdMutex<Vec<String>>>,
        name: &'static str,
    }
    impl PhaseEndTrigger for CountingTrigger {
        fn name(&self) -> &str {
            self.name
        }
        fn fire(&self, event: &PhaseEndEvent) {
            self.count.fetch_add(1, Ordering::Release);
            self.names.lock().unwrap().push(event.phase_name.clone());
        }
    }

    /// Wait for `count` to reach `target` or `timeout` to
    /// elapse. The worker is async — tests synchronize via
    /// the counter, not via channel polling.
    fn wait_for(count: &AtomicUsize, target: usize, timeout: Duration) -> bool {
        let start = Instant::now();
        while count.load(Ordering::Acquire) < target {
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// Locking across test cases — the registry is global, so
    /// concurrent tests would step on each other.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn registered_trigger_fires_for_completed_phase() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let count = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(StdMutex::new(Vec::new()));
        let trig = Arc::new(CountingTrigger {
            count: count.clone(),
            names: names.clone(),
            name: "test",
        });
        let _id = register(trig);

        fire_phase_completed("setup", "", 1.5);
        assert!(
            wait_for(&count, 1, Duration::from_secs(2)),
            "trigger did not fire within 2s"
        );
        assert_eq!(*names.lock().unwrap(), vec!["setup".to_string()]);
        reset_for_tests();
    }

    #[test]
    fn registered_trigger_fires_for_failed_phase() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let count = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(StdMutex::new(Vec::new()));
        let trig = Arc::new(CountingTrigger {
            count: count.clone(),
            names: names.clone(),
            name: "test",
        });
        let _id = register(trig);

        fire_phase_failed("query", "k=10", "timeout");
        assert!(wait_for(&count, 1, Duration::from_secs(2)));
        assert_eq!(*names.lock().unwrap(), vec!["query".to_string()]);
        reset_for_tests();
    }

    #[test]
    fn unregister_stops_subsequent_dispatches() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let count = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(StdMutex::new(Vec::new()));
        let trig = Arc::new(CountingTrigger {
            count: count.clone(),
            names: names.clone(),
            name: "test",
        });
        let id = register(trig);

        fire_phase_completed("a", "", 0.1);
        assert!(wait_for(&count, 1, Duration::from_secs(2)));

        unregister(id);
        fire_phase_completed("b", "", 0.2);
        // Worker is async — give it time to (not) fire.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            count.load(Ordering::Acquire),
            1,
            "trigger fired after unregister"
        );
        reset_for_tests();
    }

    #[test]
    fn multiple_triggers_fire_in_registration_order() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        let count_a = Arc::new(AtomicUsize::new(0));
        let count_b = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(StdMutex::new(Vec::new()));
        let _id_a = register(Arc::new(CountingTrigger {
            count: count_a.clone(),
            names: names.clone(),
            name: "a",
        }));
        let _id_b = register(Arc::new(CountingTrigger {
            count: count_b.clone(),
            names: names.clone(),
            name: "b",
        }));

        fire_phase_completed("phase1", "", 0.0);
        assert!(wait_for(&count_a, 1, Duration::from_secs(2)));
        assert!(wait_for(&count_b, 1, Duration::from_secs(2)));
        assert_eq!(
            *names.lock().unwrap(),
            vec!["phase1".to_string(), "phase1".to_string()]
        );
        reset_for_tests();
    }

    #[test]
    fn panic_in_one_trigger_does_not_stop_others() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        struct PanickingTrigger;
        impl PhaseEndTrigger for PanickingTrigger {
            fn name(&self) -> &str {
                "panicker"
            }
            fn fire(&self, _: &PhaseEndEvent) {
                panic!("boom");
            }
        }
        let count = Arc::new(AtomicUsize::new(0));
        let names = Arc::new(StdMutex::new(Vec::new()));
        let _a = register(Arc::new(PanickingTrigger));
        let _b = register(Arc::new(CountingTrigger {
            count: count.clone(),
            names: names.clone(),
            name: "after-panic",
        }));
        fire_phase_completed("phase", "", 0.0);
        // The downstream counter must still see its fire.
        assert!(
            wait_for(&count, 1, Duration::from_secs(2)),
            "downstream trigger lost to upstream panic"
        );
        reset_for_tests();
    }

    #[test]
    fn fire_with_no_triggers_is_noop() {
        let _g = TEST_LOCK.lock().unwrap();
        reset_for_tests();
        // No registrations, no panic, no channel send.
        fire_phase_completed("x", "", 1.0);
        fire_phase_failed("y", "", "oops");
        // Nothing to assert — the call returns without
        // touching the worker. Implicit pass.
    }
}

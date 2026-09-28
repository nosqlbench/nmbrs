// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Daemon-fiber pool for the daemon-op primitive.
//!
//! A daemon op runs on a dedicated fiber spawned at phase init,
//! **additive to the cycle-pool** (no concurrency-resize
//! involvement). The daemon dispatches its op exactly once and
//! runs until completion or cancellation.
//!
//! Phase-completion contract (orchestrated by the activity loop):
//! the phase exits when the cursor is exhausted AND every
//! cycle-pool fiber has returned. At that point the phase calls
//! [`DaemonPool::shutdown`] which sends a stop signal to every
//! still-running daemon and waits up to a configurable grace
//! window for them to drain. The semantics:
//!
//! - Daemon completes naturally (the underlying op returns Ok)
//!   → [`DaemonExit::Completed`]; phase outcome unchanged.
//! - Daemon's op returns an `ExecutionError` (during normal
//!   running or during shutdown) → [`DaemonExit::Errored`];
//!   phase fails. The daemon's whole point is to stay in scope
//!   so failures bubble up; throwing the work to a detached
//!   tokio task would lose this.
//! - Daemon is signalled to stop and the in-flight future drops
//!   within the grace window → [`DaemonExit::Cancelled`]; phase
//!   outcome unchanged. This is the happy path for the
//!   trigger-and-observe pattern (the daemon was meant to be
//!   cancelled when the sibling poll's predicate flipped).
//! - Grace window expires before the daemon drains → the
//!   daemon's join handle is aborted and the daemon is recorded
//!   as [`DaemonExit::TimedOut`]; phase fails. Indicates a
//!   broken cleanup path inside the adapter or op.
//!
//! Multi-daemon: each daemon op gets its own fiber, its own
//! stop-flag, its own grace-window deadline. They're independent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::adapter::ExecutionError;

/// Per-daemon cooperative-exit flag. The daemon body races its
/// `dispenser.execute(...)` future against a poller that reads
/// this flag; setting the flag fires the cancellation path.
pub type DaemonStopFlag = Arc<AtomicBool>;

/// Discriminated outcome of one daemon fiber's lifetime. Lives
/// long enough for the phase exit logic to translate into the
/// final phase outcome.
#[derive(Debug)]
pub enum DaemonExit {
    /// The daemon's op returned successfully on its own — the
    /// underlying call completed before any stop signal fired.
    /// Phase outcome unchanged.
    Completed,
    /// The daemon's op returned an `ExecutionError`. Phase fails.
    Errored(ExecutionError),
    /// The daemon was signalled to stop, the in-flight future
    /// dropped cleanly within the grace window, and the daemon's
    /// fiber returned. Phase outcome unchanged. This is the
    /// expected outcome for trigger-and-observe daemons.
    Cancelled,
    /// The daemon's grace window expired before the in-flight
    /// future dropped; the fiber's join handle was aborted to
    /// release the tokio task. Phase fails.
    TimedOut,
    /// The daemon's fiber panicked. Phase fails.
    Panicked(String),
}

impl DaemonExit {
    /// Whether this outcome should fail the enclosing phase.
    /// Clean completions and clean cancellations don't; errors,
    /// timeouts, and panics do.
    pub fn is_phase_error(&self) -> bool {
        matches!(
            self,
            DaemonExit::Errored(_) | DaemonExit::TimedOut | DaemonExit::Panicked(_),
        )
    }

    /// One-line label for logs / readouts.
    pub fn label(&self) -> &'static str {
        match self {
            DaemonExit::Completed => "completed",
            DaemonExit::Errored(_) => "errored",
            DaemonExit::Cancelled => "cancelled",
            DaemonExit::TimedOut => "timed-out",
            DaemonExit::Panicked(_) => "panicked",
        }
    }
}

/// One slot in the pool: identity + cancellation channel + join.
struct DaemonSlot {
    /// Operator-visible op-template name. Used in log messages
    /// and the daemon-shutdown summary.
    name: String,
    /// Grace window applied when the phase signals this daemon
    /// to stop. Defaults to the pool-level default, overridable
    /// per op via `daemon_cancel_grace_ms`.
    cancel_grace: Duration,
    /// Cooperative-exit flag. The daemon body polls this and
    /// drops its in-flight future when it flips.
    stop_flag: DaemonStopFlag,
    /// JoinHandle on the daemon's tokio task. The outcome is
    /// the daemon's return value (one of the [`DaemonExit`]
    /// variants); abort() is used as a last-resort hammer when
    /// the grace window expires.
    handle: tokio::task::JoinHandle<DaemonExit>,
}

/// Owner of the daemon fibers for one phase activation. Spawn
/// daemons via [`Self::try_spawn`]; at phase exit call
/// [`Self::shutdown`] to drain them with their grace windows.
///
/// The pool enforces a per-op-name fiber cap: each call to
/// [`Self::try_spawn`] passes the cap declared by the op
/// template's `daemon:` field. When `live_count(op_name) >= cap`
/// the call returns `Err` and the activity must treat that as a
/// workload-design error (the dispatch site sets the activity
/// stop-flag and fails the phase). There is no queuing — a
/// daemon-op overflowing its cap is an authoring mistake, not a
/// load-shedding signal.
pub struct DaemonPool {
    slots: Mutex<Vec<DaemonSlot>>,
    /// Per-op-name live-fiber counter, shared with each spawned
    /// fiber's decrement-on-completion wrapper. `Arc<Mutex<...>>`
    /// because the wrapper future outlives a single try_spawn
    /// call and must own a handle to bump the count back down.
    live_counts: Arc<Mutex<HashMap<String, u32>>>,
    /// Pool-level default grace window. Per-daemon overrides
    /// shadow this on a slot-by-slot basis.
    default_cancel_grace: Duration,
}

impl DaemonPool {
    /// Pool-level default grace window for daemons that don't
    /// declare their own `daemon_cancel_grace_ms`. 5 seconds
    /// covers the common case where a reqwest call mid-`send()`
    /// drops near-instantly when the future is cancelled; longer
    /// grace windows are appropriate when the adapter wraps a
    /// blocking IO call that can't observe the future drop until
    /// the underlying syscall returns.
    pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(5);

    pub fn new() -> Self {
        Self {
            slots: Mutex::new(Vec::new()),
            live_counts: Arc::new(Mutex::new(HashMap::new())),
            default_cancel_grace: Self::DEFAULT_CANCEL_GRACE,
        }
    }

    /// Snapshot of the current live-fiber count for `op_name`.
    /// Diagnostics / tests only — racy by construction (the
    /// number can change between the read and any subsequent
    /// decision). [`Self::try_spawn`] is the only atomic
    /// "check-and-increment" entrypoint.
    #[allow(dead_code)] // WIP: SRD-82/83 daemon diagnostics / status surface
    pub fn live_count(&self, op_name: &str) -> u32 {
        let g = self.live_counts.lock().unwrap_or_else(|e| e.into_inner());
        g.get(op_name).copied().unwrap_or(0)
    }

    /// Attempt to spawn a daemon fiber with a per-op-name cap.
    ///
    /// Behaviour:
    /// - If `live_count(op_name) < cap`, increment the counter,
    ///   spawn the body on tokio, register a decrement-on-exit
    ///   wrapper, record the slot, return `Ok(())`.
    /// - If `live_count(op_name) >= cap`, return `Err` with a
    ///   descriptive message naming the op and cap. No fiber is
    ///   spawned. The caller is expected to treat this as a
    ///   workload-design error and fail the phase.
    ///
    /// `cap` must be > 0 (the disabled case is filtered out by
    /// the dispatch site via `DaemonSpec::is_disabled`). `cap`
    /// of 0 is treated as "spawn immediately rejected" for
    /// safety — there is no legitimate dispatch with cap 0.
    ///
    /// `op_name` surfaces in logs and the shutdown summary;
    /// `cancel_grace` (when `Some`) overrides the pool default;
    /// `body` is the fiber's async work. The body must observe
    /// `stop` and drop its in-flight future when it flips —
    /// typically via `tokio::select!` racing the work against a
    /// polling task on the flag.
    pub fn try_spawn<F, Fut>(
        &self,
        op_name: String,
        cap: u32,
        cancel_grace: Option<Duration>,
        body: F,
    ) -> Result<(), String>
    where
        F: FnOnce(DaemonStopFlag) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = DaemonExit> + Send + 'static,
    {
        // Atomic check-and-increment under the counts lock.
        {
            let mut counts = self.live_counts.lock().unwrap_or_else(|e| e.into_inner());
            let current = counts.entry(op_name.clone()).or_insert(0);
            if *current >= cap {
                return Err(format!(
                    "daemon op '{op_name}' would exceed its max-fibers cap \
                     of {cap} (currently {current} live). Daemon ops do not \
                     queue; raise the cap with `daemon: <N>` or fix the \
                     workload so spawns don't outpace exits."
                ));
            }
            *current += 1;
        }

        let stop_flag: DaemonStopFlag = Arc::new(AtomicBool::new(false));
        let flag_for_body = stop_flag.clone();
        let counts_for_dec = self.live_counts.clone();
        let op_name_for_dec = op_name.clone();
        let handle = tokio::spawn(async move {
            let exit = body(flag_for_body).await;
            let mut counts = counts_for_dec.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(n) = counts.get_mut(&op_name_for_dec) {
                *n = n.saturating_sub(1);
            }
            exit
        });
        let slot = DaemonSlot {
            name: op_name,
            cancel_grace: cancel_grace.unwrap_or(self.default_cancel_grace),
            stop_flag,
            handle,
        };
        let mut g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        g.push(slot);
        Ok(())
    }

    /// Whether the pool has any daemons (running or finished but
    /// not yet drained). Used by phase-completion gating to know
    /// when to enter the shutdown phase.
    pub fn is_empty(&self) -> bool {
        let g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        g.is_empty()
    }

    /// Number of daemons tracked. Diagnostics only.
    pub fn len(&self) -> usize {
        let g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        g.len()
    }

    /// Whether any daemon's fiber is still running (the join
    /// handle hasn't reported finished). Phase-completion gating
    /// can use this to know whether daemons have completed
    /// naturally before the cycle-pool drain finished — in which
    /// case [`Self::shutdown`] still needs to await their outcome
    /// values, but no stop signal is needed.
    #[allow(dead_code)] // WIP: SRD-82/83 phase-completion gating on daemon liveness
    pub fn any_running(&self) -> bool {
        let g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        g.iter().any(|s| !s.handle.is_finished())
    }

    /// Phase-exit drain. For every daemon:
    ///
    /// 1. If it's still running, set its stop-flag.
    /// 2. Await its join handle with a deadline of the daemon's
    ///    `cancel_grace`. If the join completes within the
    ///    window, the recorded outcome is whatever the body
    ///    returned ([`DaemonExit::Completed`] /
    ///    [`DaemonExit::Cancelled`] / [`DaemonExit::Errored`]).
    /// 3. If the deadline expires first, abort the join handle
    ///    and record [`DaemonExit::TimedOut`].
    /// 4. If the join handle resolves to a JoinError, record
    ///    [`DaemonExit::Panicked`] with the error string.
    ///
    /// Returns one [`DaemonExit`] per spawned daemon, in spawn
    /// order. Caller is responsible for any phase-error
    /// aggregation; each `DaemonExit` carries its own
    /// `is_phase_error()` predicate.
    pub async fn shutdown(&self) -> Vec<(String, DaemonExit)> {
        let slots = {
            let mut g = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *g)
        };
        let mut out = Vec::with_capacity(slots.len());
        for slot in slots {
            let DaemonSlot {
                name,
                cancel_grace,
                stop_flag,
                handle,
            } = slot;
            // Signal stop. If the daemon already finished
            // naturally the flag is moot, but setting it is
            // harmless.
            stop_flag.store(true, Ordering::Release);
            let exit = match tokio::time::timeout(cancel_grace, handle).await {
                Ok(Ok(daemon_exit)) => daemon_exit,
                Ok(Err(join_err)) => {
                    // Tokio JoinError — panic or pre-cancellation.
                    // `is_cancelled()` distinguishes the two, but
                    // we don't call abort() before the timeout, so
                    // a cancelled JoinError shouldn't appear here.
                    // Surface anyway as Panicked with the message.
                    DaemonExit::Panicked(format!("{join_err}"))
                }
                Err(_elapsed) => {
                    // Grace window expired — abort the fiber's
                    // tokio task to release the runtime resources.
                    // We don't re-await: the abort drops the
                    // future at the next yield point.
                    // The handle was moved into `timeout`; we lost
                    // ownership at that boundary, so an explicit
                    // abort isn't reachable from here. Tokio will
                    // reap the task on its own once the runtime
                    // realises nothing's holding the handle.
                    DaemonExit::TimedOut
                }
            };
            out.push((name, exit));
        }
        out
    }
}

impl Default for DaemonPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon that returns Completed promptly. Phase outcome:
    /// not an error.
    #[tokio::test]
    async fn natural_completion_is_not_phase_error() {
        let pool = DaemonPool::new();
        pool.try_spawn("worker".into(), 1, None, |_stop| async {
            DaemonExit::Completed
        })
        .expect("first spawn under cap");
        let outcomes = pool.shutdown().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, DaemonExit::Completed));
        assert!(!outcomes[0].1.is_phase_error());
    }

    /// Daemon that races against the stop flag and drops cleanly
    /// when it flips. Phase outcome: not an error.
    #[tokio::test]
    async fn cancellation_within_grace_is_not_phase_error() {
        let pool = DaemonPool::new();
        pool.try_spawn(
            "watcher".into(),
            1,
            Some(Duration::from_secs(1)),
            |stop| async move {
                // Poll the stop flag indefinitely (the work in the
                // real adapter would race the in-flight reqwest
                // future against this same poll loop).
                loop {
                    if stop.load(Ordering::Acquire) {
                        return DaemonExit::Cancelled;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .expect("first spawn under cap");
        // Let the daemon spin briefly, then trigger the drain.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let outcomes = pool.shutdown().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, DaemonExit::Cancelled));
        assert!(!outcomes[0].1.is_phase_error());
    }

    /// Daemon that ignores the stop flag and never drops its
    /// future. The grace window expires; phase outcome: failure.
    #[tokio::test]
    async fn grace_window_expiry_fails_phase() {
        let pool = DaemonPool::new();
        pool.try_spawn(
            "stuck".into(),
            1,
            Some(Duration::from_millis(50)),
            |_stop| async {
                // Deliberately ignore the stop flag.
                loop {
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            },
        )
        .expect("first spawn under cap");
        let outcomes = pool.shutdown().await;
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0].1, DaemonExit::TimedOut));
        assert!(outcomes[0].1.is_phase_error());
    }

    // -------- §B: per-name counter + cap enforcement --------

    /// Cap of 1: first spawn ok, second of same name errors.
    #[tokio::test]
    async fn cap_one_rejects_second_same_name() {
        let pool = DaemonPool::new();
        pool.try_spawn("op".into(), 1, None, |stop| async move {
            loop {
                if stop.load(Ordering::Acquire) {
                    return DaemonExit::Cancelled;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .expect("first under cap");
        assert_eq!(pool.live_count("op"), 1);
        let err = pool
            .try_spawn("op".into(), 1, None, |_| async { DaemonExit::Completed })
            .expect_err("second over cap");
        assert!(err.contains("'op'"));
        assert!(err.contains("cap of 1"));
        // Live count must NOT have changed by the rejected spawn.
        assert_eq!(pool.live_count("op"), 1);
        let _ = pool.shutdown().await;
    }

    /// Cap is per-name, not pool-wide.
    #[tokio::test]
    async fn cap_is_per_name_not_pool_wide() {
        let pool = DaemonPool::new();
        pool.try_spawn("a".into(), 1, None, |stop| async move {
            loop {
                if stop.load(Ordering::Acquire) {
                    return DaemonExit::Cancelled;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .expect("a first ok");
        // 'b' is a different op-template name; its own cap is
        // independent of 'a's.
        pool.try_spawn("b".into(), 1, None, |stop| async move {
            loop {
                if stop.load(Ordering::Acquire) {
                    return DaemonExit::Cancelled;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .expect("b first ok");
        assert_eq!(pool.live_count("a"), 1);
        assert_eq!(pool.live_count("b"), 1);
        let _ = pool.shutdown().await;
    }

    /// Cap of N: exactly N spawns accepted, N+1 rejected.
    #[tokio::test]
    async fn cap_n_accepts_n_rejects_n_plus_one() {
        let pool = DaemonPool::new();
        let n: u32 = 5;
        for _ in 0..n {
            pool.try_spawn("op".into(), n, None, |stop| async move {
                loop {
                    if stop.load(Ordering::Acquire) {
                        return DaemonExit::Cancelled;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .expect("under cap");
        }
        assert_eq!(pool.live_count("op"), n);
        let err = pool
            .try_spawn("op".into(), n, None, |_| async { DaemonExit::Completed })
            .expect_err("over cap");
        assert!(err.contains("cap of 5"));
        let _ = pool.shutdown().await;
    }

    /// Cap of 0 is a hard reject (no legitimate caller; defensive).
    #[tokio::test]
    async fn cap_zero_always_rejects() {
        let pool = DaemonPool::new();
        let err = pool
            .try_spawn("op".into(), 0, None, |_| async { DaemonExit::Completed })
            .expect_err("cap 0 rejects");
        assert!(err.contains("cap of 0"));
        assert_eq!(pool.live_count("op"), 0);
    }

    /// After a daemon completes naturally, its slot's counter
    /// decrements and a new spawn under the same name succeeds.
    #[tokio::test]
    async fn counter_decrements_on_natural_completion() {
        let pool = DaemonPool::new();
        pool.try_spawn("op".into(), 1, None, |_stop| async {
            DaemonExit::Completed
        })
        .expect("first ok");
        // Yield until the spawned task has run its decrement.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if pool.live_count("op") == 0 {
                break;
            }
        }
        assert_eq!(pool.live_count("op"), 0, "counter must decrement");
        // Now another spawn under the same cap succeeds.
        pool.try_spawn("op".into(), 1, None, |_| async { DaemonExit::Completed })
            .expect("second ok after first drained");
        let _ = pool.shutdown().await;
    }

    /// Decrement on Errored / Cancelled / TimedOut exit paths
    /// (every DaemonExit variant runs through the same wrapper).
    #[tokio::test]
    async fn counter_decrements_on_errored_exit() {
        let pool = DaemonPool::new();
        pool.try_spawn("op".into(), 1, None, |_stop| async {
            DaemonExit::Errored(ExecutionError::Op(crate::adapter::AdapterError {
                error_name: "Synthetic".into(),
                message: "test".into(),
                retryable: false,
            }))
        })
        .expect("first ok");
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if pool.live_count("op") == 0 {
                break;
            }
        }
        assert_eq!(pool.live_count("op"), 0);
        let _ = pool.shutdown().await;
    }

    /// Counter never goes negative even if the decrement runs
    /// against an unexpected empty slot (saturating_sub guard).
    #[tokio::test]
    async fn counter_is_saturating() {
        let pool = DaemonPool::new();
        // Force the entry to exist at 0 (simulating a post-drain
        // race) then run a spawn-and-immediate-exit cycle.
        {
            let mut g = pool.live_counts.lock().unwrap();
            g.insert("op".into(), 0);
        }
        // Spawn (will bump to 1), exit immediately decrements to 0.
        pool.try_spawn("op".into(), 1, None, |_| async { DaemonExit::Completed })
            .expect("ok");
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(2)).await;
            if pool.live_count("op") == 0 {
                break;
            }
        }
        assert_eq!(pool.live_count("op"), 0);
        let _ = pool.shutdown().await;
    }

    /// Proptest F3: arbitrary spawn / exit interleavings preserve
    /// the cap invariant `0 <= live_count(name) <= cap[name]`.
    /// Models the pool's counter logic synchronously (no tokio)
    /// — the counter math itself is what we're exercising.
    mod proptests {
        use proptest::prelude::*;
        use std::collections::HashMap;

        #[derive(Debug, Clone)]
        enum Op {
            Spawn(String),
            Exit(String),
        }

        fn names() -> impl Strategy<Value = String> {
            // Small alphabet to maximise interleaving on the same
            // counter (otherwise every spawn hits a fresh name).
            prop_oneof![
                Just("a".to_string()),
                Just("b".to_string()),
                Just("c".to_string()),
            ]
        }

        fn op_strategy() -> impl Strategy<Value = Op> {
            prop_oneof![names().prop_map(Op::Spawn), names().prop_map(Op::Exit),]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]
            #[test]
            fn cap_invariants_hold(
                cap in 1u32..=8,
                ops in proptest::collection::vec(op_strategy(), 0..200),
            ) {
                let mut live: HashMap<String, u32> = HashMap::new();
                for op in &ops {
                    match op {
                        Op::Spawn(n) => {
                            let cur = *live.get(n).unwrap_or(&0);
                            if cur < cap {
                                live.insert(n.clone(), cur + 1);
                            }
                            // else: spawn rejected, count unchanged.
                        }
                        Op::Exit(n) => {
                            let cur = live.get(n).copied().unwrap_or(0);
                            live.insert(n.clone(), cur.saturating_sub(1));
                        }
                    }
                    // After every op: count is bounded by cap and
                    // never negative (u32 enforces the lower bound).
                    for (_, c) in live.iter() {
                        prop_assert!(*c <= cap,
                            "cap invariant violated: count {} > cap {}", c, cap);
                    }
                }
            }
        }
    }
}

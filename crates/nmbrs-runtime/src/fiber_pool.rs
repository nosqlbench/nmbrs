// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Fiber pool primitive supporting live resize via a dynamic
//! concurrency control (SRD 23 §"Fiber executor").
//!
//! The pool owns one stop-flag per fiber. A fiber checks its
//! flag at every cycle boundary and exits cooperatively when
//! the flag is set — no mid-op termination, ever. The pool
//! tracks the count of *intended-active* fibers; the applier
//! responds to a write on the `concurrency` control by either
//! spawning new fibers (scale-up) or flagging the most recently
//! spawned ones for exit (scale-down).
//!
//! Pool ownership lives in the activity executor; the applier
//! is registered on the activity's `concurrency` control. The
//! cooperative-exit window is bounded by one cycle of work —
//! the longest a scaled-down fiber stays alive is the time
//! its current op takes to complete plus one acquire from
//! the rate limiter.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use nmbrs_metrics::controls::ControlApplier;

/// Per-fiber cooperative-exit flag. Each fiber owns one of
/// these and checks it at the top of its cycle loop.
pub type StopFlag = Arc<AtomicBool>;

/// Spawner closure: builds and `tokio::spawn`s a fiber given
/// its stop-flag. Returns the join handle so the pool can wait
/// for the fiber to exit if needed.
pub type FiberSpawner = Box<dyn Fn(StopFlag) -> tokio::task::JoinHandle<()> + Send + Sync>;

/// Owner of the running set of fibers for one activity. The
/// pool exposes `resize(target)` for the applier and
/// `register_applier(...)` to wire the control's write surface.
pub struct FiberPool {
    /// FIFO of (stop_flag, join_handle) — newest spawn at the
    /// back. Scale-down signals from the back; scale-up appends.
    fibers: Mutex<Vec<(StopFlag, tokio::task::JoinHandle<()>)>>,
    spawner: FiberSpawner,
}

impl FiberPool {
    /// Create a pool with no fibers. Use [`Self::spawn_initial`]
    /// to seed N fibers at activity start.
    pub fn new(spawner: FiberSpawner) -> Self {
        Self {
            fibers: Mutex::new(Vec::new()),
            spawner,
        }
    }

    /// Seed the pool with `count` fibers. Call once at activity
    /// start, after the rate limiter and other shared state is
    /// in place. Subsequent changes go through [`Self::resize`].
    pub fn spawn_initial(&self, count: usize) {
        for _ in 0..count {
            self.spawn_one();
        }
    }

    /// Live count of intended-active fibers (those whose
    /// stop-flag has not been set). May briefly diverge from
    /// the actual running count while a flagged fiber is still
    /// completing its current op.
    pub fn active_count(&self) -> usize {
        let g = self.fibers.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .filter(|(flag, _)| !flag.load(Ordering::Relaxed))
            .count()
    }

    /// Total count including fibers that have been flagged for
    /// exit but haven't drained yet. Useful for diagnostics.
    pub fn tracked_count(&self) -> usize {
        let g = self.fibers.lock().unwrap_or_else(|e| e.into_inner());
        g.len()
    }

    /// Reconcile the pool to `target` intended-active fibers.
    ///
    /// - If `target > active_count()`: spawn `target - active`
    ///   new fibers via the registered spawner.
    /// - If `target < active_count()`: signal
    ///   `active - target` of the most recently spawned fibers
    ///   to exit. They wind down at their next cycle boundary.
    /// - Otherwise: no-op.
    ///
    /// Returns the new active count (may be the same as before
    /// if no change was needed).
    pub fn resize(&self, target: usize) -> usize {
        let active = self.active_count();
        if target == active {
            return active;
        }
        if target > active {
            for _ in 0..(target - active) {
                self.spawn_one();
            }
            target
        } else {
            // Signal the most-recently-spawned `active - target`
            // fibers to exit. We walk from the back, skipping
            // any that are already flagged.
            let mut g = self.fibers.lock().unwrap_or_else(|e| e.into_inner());
            let mut to_signal = active - target;
            for (flag, _) in g.iter_mut().rev() {
                if to_signal == 0 {
                    break;
                }
                if !flag.load(Ordering::Relaxed) {
                    flag.store(true, Ordering::Release);
                    to_signal -= 1;
                }
            }
            target
        }
    }

    /// Reap any fibers whose join handles report `is_finished()`.
    /// Safe to call periodically for diagnostics; not required
    /// for correctness.
    pub fn reap_finished(&self) {
        let mut g = self.fibers.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|(_flag, handle)| !handle.is_finished());
    }

    fn spawn_one(&self) {
        let flag: StopFlag = Arc::new(AtomicBool::new(false));
        let handle = (self.spawner)(flag.clone());
        let mut g = self.fibers.lock().unwrap_or_else(|e| e.into_inner());
        g.push((flag, handle));
    }
}

/// Applier that reconciles a [`FiberPool`] to a `Control<u32>`
/// concurrency write. Register one on the activity's
/// `concurrency` control:
///
/// ```ignore
/// concurrency_control.register_applier(
///     ConcurrencyApplier::new(pool.clone()),
/// );
/// ```
pub struct ConcurrencyApplier {
    pool: Arc<FiberPool>,
}

impl ConcurrencyApplier {
    pub fn new(pool: Arc<FiberPool>) -> Self {
        Self { pool }
    }
}

impl ControlApplier<u32> for ConcurrencyApplier {
    fn apply(&self, value: u32) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            // resize itself is sync; wrap in async to satisfy
            // the trait. Spawning new tasks happens inside the
            // resize call, which is fine to do from any tokio
            // context.
            let actual = pool.resize(value as usize);
            if actual == value as usize {
                Ok(())
            } else {
                Err(format!(
                    "concurrency reconcile reached {actual}, target was {value}",
                ))
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn worker_spawner(ops_counter: Arc<AtomicUsize>) -> FiberSpawner {
        Box::new(move |stop: StopFlag| {
            let counter = ops_counter.clone();
            tokio::spawn(async move {
                while !stop.load(Ordering::Acquire) {
                    counter.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
        })
    }

    #[tokio::test]
    async fn spawn_initial_seeds_target_count() {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = FiberPool::new(worker_spawner(counter.clone()));
        pool.spawn_initial(4);
        // Give the workers a tick to start incrementing.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.active_count(), 4);
        assert!(counter.load(Ordering::Relaxed) >= 4);
        // Cleanup.
        pool.resize(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn resize_up_spawns_additional_fibers() {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = FiberPool::new(worker_spawner(counter.clone()));
        pool.spawn_initial(2);
        let new_count = pool.resize(5);
        assert_eq!(new_count, 5);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.active_count(), 5);
        pool.resize(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn resize_down_flags_newest_fibers_to_exit() {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = FiberPool::new(worker_spawner(counter.clone()));
        pool.spawn_initial(5);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.active_count(), 5);

        let new_count = pool.resize(2);
        assert_eq!(new_count, 2);
        // Active count drops immediately because the flag is
        // set on three of them.
        assert_eq!(pool.active_count(), 2);

        // Wait for the flagged fibers to drain.
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            pool.reap_finished();
            if pool.tracked_count() == 2 {
                break;
            }
        }
        assert_eq!(pool.tracked_count(), 2);

        pool.resize(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn resize_to_same_count_is_noop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = FiberPool::new(worker_spawner(counter.clone()));
        pool.spawn_initial(3);
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.resize(3), 3);
        assert_eq!(pool.tracked_count(), 3);
        pool.resize(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    #[tokio::test]
    async fn applier_drives_pool_through_control() {
        use nmbrs_metrics::controls::{ControlBuilder, ControlOrigin};
        let counter = Arc::new(AtomicUsize::new(0));
        let pool = Arc::new(FiberPool::new(worker_spawner(counter.clone())));
        pool.spawn_initial(2);
        tokio::time::sleep(Duration::from_millis(20)).await;

        let control: nmbrs_metrics::controls::Control<u32> =
            ControlBuilder::new("concurrency", 2u32)
                .reify_as_gauge(|v| Some(*v as f64))
                .from_f64(|v| Ok(v as u32))
                .build();
        control.register_applier(ConcurrencyApplier::new(pool.clone()));

        // Bump concurrency through the control surface.
        control.set(6, ControlOrigin::Test).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.active_count(), 6);

        // Drop back down.
        control.set(1, ControlOrigin::Test).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pool.active_count(), 1);

        pool.resize(0);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Async-ready token bucket rate limiter with time-scaled permits.
//!
//! Matches the nosqlbench SimRate design:
//! - Active pool (tokio Semaphore) for immediate dispatch
//! - Waiting pool (AtomicI64) for backlog tracking
//! - Refill task replenishes permits every 10ms
//! - Burst recovery moves tokens from waiting → active proportionally
//! - Wait time tracking for coordinated omission metrics

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::Semaphore;
use tokio::time::{self, Duration};

use crate::spec::RateSpec;

/// The maximum active pool size in ticks (approximately 1 second in nanos).
const MAX_ACTIVE_POOL: u32 = 1_000_000_000;

/// Refill interval in milliseconds.
const REFILL_INTERVAL_MS: u64 = 10;

/// Shared state between the limiter and its refill task.
struct SharedState {
    active_pool: Semaphore,
    waiting_pool: AtomicI64,
    running: AtomicBool,
    last_refill_nanos: AtomicU64,
    start_time: Instant,
    blocks: AtomicU64,
    /// Read on every [`RateLimiter::acquire`]. Updated atomically
    /// by [`RateLimiter::reconfigure`]; the next acquire picks up
    /// the new cost immediately.
    ticks_per_op: AtomicU32,
    /// Burst/unit parameters. Updated only by reconfigure and
    /// read by the refill loop — RwLock is fine under that
    /// contention profile (write ≪ read, both low-frequency).
    refill_cfg: RwLock<RefillCfg>,
    /// Current spec, kept for `rate()` and diagnostics. Always
    /// in sync with `ticks_per_op` and `refill_cfg`.
    spec: RwLock<RateSpec>,
}

#[derive(Clone, Copy)]
struct RefillCfg {
    max_active: u32,
    max_over_active: u32,
    burst_pool_size: u32,
    unit: crate::spec::TimeUnit,
}

impl RefillCfg {
    fn from_spec(spec: &RateSpec) -> Self {
        let max_over_active = (MAX_ACTIVE_POOL as f64 * spec.burst_ratio) as u32;
        let burst_pool_size = max_over_active.saturating_sub(MAX_ACTIVE_POOL);
        Self {
            max_active: MAX_ACTIVE_POOL,
            max_over_active,
            burst_pool_size,
            unit: spec.unit,
        }
    }
}

/// An async-ready rate limiter.
///
/// Call `acquire().await` before each operation. If the system is
/// ahead of the target rate, it returns immediately. If behind, it
/// awaits until a permit is available. The blocked time is the
/// **wait_time** that surfaces coordinated omission.
pub struct RateLimiter {
    state: Arc<SharedState>,
    refill_handle: Option<tokio::task::JoinHandle<()>>,
}

impl RateLimiter {
    /// Create and start a rate limiter from a spec.
    pub fn start(spec: RateSpec) -> Self {
        let ticks_per_op = spec.ticks_per_op();
        let refill_cfg = RefillCfg::from_spec(&spec);

        let state = Arc::new(SharedState {
            active_pool: Semaphore::new(ticks_per_op as usize), // prime with one op
            waiting_pool: AtomicI64::new(0),
            running: AtomicBool::new(true),
            last_refill_nanos: AtomicU64::new(0),
            start_time: Instant::now(),
            blocks: AtomicU64::new(0),
            ticks_per_op: AtomicU32::new(ticks_per_op),
            refill_cfg: RwLock::new(refill_cfg),
            spec: RwLock::new(spec),
        });

        // Record start time
        state.last_refill_nanos.store(
            state.start_time.elapsed().as_nanos() as u64,
            Ordering::Relaxed,
        );

        let refill_state = state.clone();
        let refill_handle = tokio::spawn(async move {
            refill_loop(refill_state).await;
        });

        Self {
            state,
            refill_handle: Some(refill_handle),
        }
    }

    /// Acquire one operation permit. Blocks (async) if rate-limited.
    ///
    /// Returns the current backlog in ticks (waiting pool value).
    ///
    /// Reads `ticks_per_op` atomically, so a concurrent
    /// [`Self::reconfigure`] takes effect on the next call.
    pub async fn acquire(&self) -> i64 {
        self.state.blocks.fetch_add(1, Ordering::Relaxed);
        let permits = self.state.ticks_per_op.load(Ordering::Relaxed);

        // Acquire permits from the semaphore — this is the blocking point.
        // forget() the permit so tokens are permanently consumed (not
        // returned on drop). The refill task is the only source of new
        // permits.
        let permit = self
            .state
            .active_pool
            .acquire_many(permits)
            .await
            .expect("semaphore closed unexpectedly");
        permit.forget();

        self.state.waiting_pool.load(Ordering::Relaxed)
    }

    /// Current wait time in nanoseconds (backlog converted to nanos).
    pub fn wait_time_nanos(&self) -> u64 {
        let ticks = self.state.waiting_pool.load(Ordering::Relaxed);
        if ticks <= 0 {
            return 0;
        }
        let unit = self
            .state
            .refill_cfg
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .unit;
        unit.ticks_to_nanos(ticks as u32)
    }

    /// Total number of acquire calls.
    pub fn total_blocks(&self) -> u64 {
        self.state.blocks.load(Ordering::Relaxed)
    }

    /// Current ops/sec target.
    pub fn rate(&self) -> f64 {
        self.state
            .spec
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ops_per_sec
    }

    /// Current full spec snapshot.
    pub fn spec(&self) -> RateSpec {
        self.state
            .spec
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Live reconfigure. Atomically swaps the target rate / burst
    /// ratio / unit without stopping the refill task. The next
    /// [`Self::acquire`] call reads the new `ticks_per_op`; the
    /// next refill cycle reads the new burst config.
    ///
    /// In-flight backlog (`waiting_pool`) and already-issued
    /// active-pool permits are preserved. If the new rate is much
    /// higher than the old one the active pool will fill up over
    /// the next few refill ticks; if much lower, already-granted
    /// permits still drain before pressure builds.
    ///
    /// Validation of the new spec is the caller's responsibility —
    /// a negative or zero `ops_per_sec` will panic in
    /// `ticks_per_op`. Callers that wire this up through a
    /// `Control<RateSpec>` should install a validator on the
    /// control to reject bad values before this method is called.
    pub fn reconfigure(&self, spec: RateSpec) -> Result<(), String> {
        if spec.ops_per_sec <= 0.0 {
            return Err(format!("rate must be > 0, got {}", spec.ops_per_sec,));
        }
        if spec.burst_ratio < 1.0 {
            return Err(format!(
                "burst_ratio must be >= 1.0, got {}",
                spec.burst_ratio,
            ));
        }
        let new_ticks_per_op = spec.ticks_per_op();
        let new_cfg = RefillCfg::from_spec(&spec);

        // Update the refill config first so the next refill tick
        // sees the new pool sizes. Then update ticks_per_op so the
        // next acquire pulls the new cost. Finally update the spec
        // cache. Writers are serialized by `reconfigure` being the
        // only one that writes these fields, so ordering between
        // the three is loose.
        *self
            .state
            .refill_cfg
            .write()
            .unwrap_or_else(|e| e.into_inner()) = new_cfg;
        self.state
            .ticks_per_op
            .store(new_ticks_per_op, Ordering::Relaxed);
        *self.state.spec.write().unwrap_or_else(|e| e.into_inner()) = spec;
        Ok(())
    }

    /// Stop the rate limiter and its refill task.
    pub async fn stop(mut self) {
        self.state.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.refill_handle.take() {
            let _ = handle.await;
        }
    }
}

impl Drop for RateLimiter {
    fn drop(&mut self) {
        self.state.running.store(false, Ordering::Relaxed);
    }
}

/// The refill loop that runs on its own tokio task.
async fn refill_loop(state: Arc<SharedState>) {
    let mut interval = time::interval(Duration::from_millis(REFILL_INTERVAL_MS));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    while state.running.load(Ordering::Relaxed) {
        interval.tick().await;
        refill(&state);
    }
}

/// Single refill cycle: compute elapsed time, add permits, handle burst.
fn refill(state: &SharedState) {
    let now_nanos = state.start_time.elapsed().as_nanos() as u64;
    let last = state.last_refill_nanos.swap(now_nanos, Ordering::Relaxed);
    let elapsed_nanos = now_nanos.saturating_sub(last);

    if elapsed_nanos == 0 {
        return;
    }

    // Snapshot the current refill config. A concurrent
    // reconfigure only flips the lock between ticks, so reading
    // once at the top keeps this cycle internally consistent.
    let cfg = *state.refill_cfg.read().unwrap_or_else(|e| e.into_inner());

    // Convert elapsed time to ticks
    let new_ticks = cfg.unit.nanos_to_ticks(elapsed_nanos);
    if new_ticks == 0 {
        return;
    }

    // Current available permits in the semaphore
    let available = state.active_pool.available_permits() as u32;

    // Step 1: Fill active pool up to max
    let room = cfg.max_active.saturating_sub(available);
    let to_active = new_ticks.min(room);
    if to_active > 0 {
        state.active_pool.add_permits(to_active as usize);
    }

    // Step 2: Overflow goes to waiting pool
    let overflow = new_ticks.saturating_sub(to_active);
    if overflow > 0 {
        state
            .waiting_pool
            .fetch_add(overflow as i64, Ordering::Relaxed);
    }

    // Step 3: Burst recovery — move tokens from waiting → active
    let available_after = state.active_pool.available_permits() as u32;
    let refill_factor = (new_ticks as f64 / cfg.max_active as f64).min(1.0);
    let burst_allowed = (refill_factor * cfg.burst_pool_size as f64) as u32;
    let burst_room = cfg.max_over_active.saturating_sub(available_after);
    let burst_from_waiting = burst_allowed
        .min(burst_room)
        .min(state.waiting_pool.load(Ordering::Relaxed).max(0) as u32);

    if burst_from_waiting > 0 {
        state
            .waiting_pool
            .fetch_sub(burst_from_waiting as i64, Ordering::Relaxed);
        state.active_pool.add_permits(burst_from_waiting as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn limiter_starts_and_stops() {
        let spec = RateSpec::new(1000.0);
        let limiter = RateLimiter::start(spec);
        assert_eq!(limiter.rate(), 1000.0);
        limiter.stop().await;
    }

    #[tokio::test]
    async fn limiter_acquire_returns() {
        let spec = RateSpec::new(10000.0);
        let limiter = RateLimiter::start(spec);

        // Should be able to acquire at least one immediately (primed)
        let _backlog = limiter.acquire().await;
        assert_eq!(limiter.total_blocks(), 1);

        limiter.stop().await;
    }

    #[tokio::test]
    async fn limiter_rate_limits() {
        let spec = RateSpec::new(100.0); // 100 ops/s
        let limiter = RateLimiter::start(spec);

        // Let the refill task run a bit
        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = Instant::now();
        let mut count = 0u64;

        // Try to do 20 ops — at 100 ops/s this should take ~200ms
        for _ in 0..20 {
            limiter.acquire().await;
            count += 1;
        }

        let elapsed = start.elapsed();
        limiter.stop().await;

        assert_eq!(count, 20);
        // Should have taken at least 100ms (rate limited)
        // Being generous with timing tolerance for CI
        assert!(
            elapsed.as_millis() >= 50,
            "expected rate limiting, took {}ms for 20 ops at 100/s",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn limiter_high_rate_is_fast() {
        let spec = RateSpec::new(1_000_000.0); // 1M ops/s
        let limiter = RateLimiter::start(spec);

        tokio::time::sleep(Duration::from_millis(50)).await;

        let start = Instant::now();
        for _ in 0..100 {
            limiter.acquire().await;
        }
        let elapsed = start.elapsed();
        limiter.stop().await;

        // 100 ops at 1M/s should be nearly instant
        assert!(
            elapsed.as_millis() < 500,
            "high rate should be fast, took {}ms",
            elapsed.as_millis()
        );
    }

    #[tokio::test]
    async fn limiter_wait_time_grows_under_load() {
        let spec = RateSpec::new(100.0); // 100 ops/s
        let limiter = RateLimiter::start(spec);

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Rapidly acquire many — should build up backlog
        for _ in 0..50 {
            limiter.acquire().await;
        }

        // Wait time should be measurable (we consumed faster than refill)
        let _wt = limiter.wait_time_nanos();
        // This is the accumulated backlog — may or may not be large
        // depending on timing. Just verify the API works.
        assert!(limiter.total_blocks() == 50);

        limiter.stop().await;
    }

    #[tokio::test]
    async fn limiter_spec_parsing() {
        let spec = RateSpec::parse("500, 1.5, start").unwrap();
        let limiter = RateLimiter::start(spec);
        assert_eq!(limiter.rate(), 500.0);
        limiter.stop().await;
    }

    // ---- Reconfigure -----------------------------------------

    #[tokio::test]
    async fn reconfigure_updates_rate_and_spec() {
        let limiter = RateLimiter::start(RateSpec::new(1_000.0));
        assert_eq!(limiter.rate(), 1_000.0);

        limiter.reconfigure(RateSpec::new(5_000.0)).unwrap();
        assert_eq!(limiter.rate(), 5_000.0);
        assert_eq!(limiter.spec().ops_per_sec, 5_000.0);

        limiter.stop().await;
    }

    #[tokio::test]
    async fn reconfigure_rejects_nonpositive_rate() {
        let limiter = RateLimiter::start(RateSpec::new(1_000.0));
        let bad = RateSpec {
            ops_per_sec: 0.0,
            ..RateSpec::new(1_000.0)
        };
        assert!(limiter.reconfigure(bad).is_err());
        assert_eq!(limiter.rate(), 1_000.0);
        limiter.stop().await;
    }

    #[tokio::test]
    async fn reconfigure_rejects_subunit_burst_ratio() {
        let limiter = RateLimiter::start(RateSpec::new(1_000.0));
        let bad = RateSpec {
            burst_ratio: 0.5,
            ..RateSpec::new(1_000.0)
        };
        assert!(limiter.reconfigure(bad).is_err());
        limiter.stop().await;
    }

    #[tokio::test]
    async fn reconfigure_preserves_total_blocks_and_keeps_running() {
        // Across a reconfigure, the acquire counter and the
        // refill task keep going — the only state that changes
        // is the rate cost per op and the burst pool sizes.
        let limiter = RateLimiter::start(RateSpec::new(10_000.0));
        tokio::time::sleep(Duration::from_millis(30)).await;

        for _ in 0..5 {
            limiter.acquire().await;
        }
        let blocks_before = limiter.total_blocks();
        assert!(blocks_before >= 5);

        limiter.reconfigure(RateSpec::new(50_000.0)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        for _ in 0..5 {
            limiter.acquire().await;
        }
        let blocks_after = limiter.total_blocks();
        assert!(
            blocks_after >= blocks_before + 5,
            "acquire counter should continue across reconfigure"
        );

        limiter.stop().await;
    }

    #[tokio::test]
    async fn reconfigure_changes_observed_throughput() {
        // Slow then fast: the second batch should take less time
        // than the first batch of the same size.
        let limiter = RateLimiter::start(RateSpec::new(200.0));
        tokio::time::sleep(Duration::from_millis(30)).await;

        let t0 = Instant::now();
        for _ in 0..8 {
            limiter.acquire().await;
        }
        let slow = t0.elapsed();

        limiter.reconfigure(RateSpec::new(100_000.0)).unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        let t1 = Instant::now();
        for _ in 0..8 {
            limiter.acquire().await;
        }
        let fast = t1.elapsed();

        assert!(
            fast < slow,
            "expected faster after reconfigure: slow={slow:?} fast={fast:?}",
        );
        limiter.stop().await;
    }
}

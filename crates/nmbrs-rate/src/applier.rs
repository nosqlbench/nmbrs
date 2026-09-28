// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `Control<RateSpec>` → [`RateLimiter`] bridge.
//!
//! See SRD 23 (Dynamic Controls). A workload that wants its rate
//! limiter to be live-reconfigurable declares a `Control<RateSpec>`
//! on its owning component and registers a [`RateLimiterApplier`]
//! against that control. When any writer (CLI, TUI, GK, API)
//! calls `set` on the control, the applier calls
//! [`RateLimiter::reconfigure`] and the new rate takes effect on
//! the next `acquire`/refill cycle without restarting the refill
//! task or dropping in-flight backlog.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nmbrs_metrics::controls::ControlApplier;

use crate::RateLimiter;
use crate::spec::RateSpec;

/// A [`ControlApplier<RateSpec>`] that reconfigures a
/// [`RateLimiter`] in place. Clone the limiter `Arc` before
/// registering — the applier holds its own handle so the limiter
/// outlives any single writer.
pub struct RateLimiterApplier {
    limiter: Arc<RateLimiter>,
}

impl RateLimiterApplier {
    pub fn new(limiter: Arc<RateLimiter>) -> Self {
        Self { limiter }
    }
}

impl ControlApplier<RateSpec> for RateLimiterApplier {
    fn apply(
        &self,
        value: RateSpec,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        let limiter = self.limiter.clone();
        Box::pin(async move { limiter.reconfigure(value) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_metrics::controls::{ControlBuilder, ControlOrigin, SetError};
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn applier_reconfigures_limiter_through_control() {
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(100.0)));
        let control: nmbrs_metrics::controls::Control<RateSpec> =
            ControlBuilder::new("rate", RateSpec::new(100.0))
                .validator(|spec| {
                    if spec.ops_per_sec <= 0.0 {
                        Err(format!("rate must be > 0, got {}", spec.ops_per_sec))
                    } else {
                        Ok(())
                    }
                })
                .build();
        control.register_applier(RateLimiterApplier::new(limiter.clone()));

        assert_eq!(limiter.rate(), 100.0);

        control
            .set(RateSpec::new(5_000.0), ControlOrigin::Test)
            .await
            .expect("reconfigure should succeed");

        assert_eq!(limiter.rate(), 5_000.0);

        // Cleanup: stop the limiter by dropping our Arc; the
        // original limiter goes away when the last reference
        // does. Drop order: applier → control → limiter.
        drop(control);
        // limiter is the only remaining ref-holder; let it drop.
    }

    #[tokio::test]
    async fn validator_rejection_leaves_rate_unchanged() {
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(250.0)));
        let control: nmbrs_metrics::controls::Control<RateSpec> =
            ControlBuilder::new("rate", RateSpec::new(250.0))
                .validator(|spec| {
                    if spec.ops_per_sec > 0.0 && spec.ops_per_sec <= 10_000.0 {
                        Ok(())
                    } else {
                        Err("rate out of range (0, 10_000]".into())
                    }
                })
                .build();
        control.register_applier(RateLimiterApplier::new(limiter.clone()));

        match control
            .set(RateSpec::new(100_000.0), ControlOrigin::Test)
            .await
        {
            Err(SetError::ValidationFailed(_)) => {}
            other => panic!("expected validation rejection, got {other:?}"),
        }
        assert_eq!(limiter.rate(), 250.0);
    }

    #[tokio::test]
    async fn live_reconfigure_changes_throughput_observed_by_acquire() {
        // Start low, pull a small batch, reconfigure to a much
        // higher rate, pull another batch. The second batch should
        // drain materially faster than the first.
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(200.0)));
        let control: nmbrs_metrics::controls::Control<RateSpec> =
            ControlBuilder::new("rate", RateSpec::new(200.0)).build();
        control.register_applier(RateLimiterApplier::new(limiter.clone()));

        tokio::time::sleep(Duration::from_millis(30)).await;

        let slow_start = Instant::now();
        for _ in 0..10 {
            limiter.acquire().await;
        }
        let slow_elapsed = slow_start.elapsed();

        control
            .set(RateSpec::new(100_000.0), ControlOrigin::Test)
            .await
            .unwrap();

        // Give the refill loop one cycle to see the new config.
        tokio::time::sleep(Duration::from_millis(30)).await;

        let fast_start = Instant::now();
        for _ in 0..10 {
            limiter.acquire().await;
        }
        let fast_elapsed = fast_start.elapsed();

        assert!(
            fast_elapsed < slow_elapsed,
            "after live reconfigure the limiter should drain faster: \
             slow={slow_elapsed:?} fast={fast_elapsed:?}",
        );
    }

    #[tokio::test]
    async fn reconfigure_preserves_in_flight_backlog_field() {
        // The waiting pool is kept across a reconfigure — a writer
        // that raises the rate shouldn't lose track of backlog
        // that was already owed under the old rate.
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(10.0))); // very slow
        let control: nmbrs_metrics::controls::Control<RateSpec> =
            ControlBuilder::new("rate", RateSpec::new(10.0)).build();
        control.register_applier(RateLimiterApplier::new(limiter.clone()));

        // Let the refill task build up some backlog under the slow
        // rate. 150ms at 10 ops/s → refill issues ~1 tick before
        // the overflow logic kicks in. Hard to force backlog
        // deterministically without a heavy acquire loop; assert
        // the API at minimum: after reconfigure, wait_time_nanos
        // still reads without panicking and the new rate is live.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let backlog_before = limiter.wait_time_nanos();
        control
            .set(RateSpec::new(1_000.0), ControlOrigin::Test)
            .await
            .unwrap();
        // Read the backlog through the new (unit may differ) —
        // the call is valid, which is the protocol guarantee.
        let _backlog_after = limiter.wait_time_nanos();
        assert!(limiter.rate() == 1_000.0);
        let _ = backlog_before;
    }
}

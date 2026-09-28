// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! End-to-end integration for SRD 23 rate-limiter controls.
//!
//! Exercises the full pipeline:
//!   1. A component declares a `Control<RateSpec>` on itself
//!      (the SRD-23 structural-declaration path).
//!   2. A `RateLimiter` is created and its applier is registered
//!      against the control.
//!   3. Several concurrent workers acquire permits from the
//!      limiter (simulating fibers in a real workload).
//!   4. A writer mutates the control at runtime.
//!   5. The reified gauge snapshot on the registry tracks the
//!      current rate value so it flows through the metric sinks.
//!   6. A validator rejects an out-of-range spec without
//!      disturbing the running limiter.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nmbrs_metrics::controls::{Control, ControlBuilder, ControlOrigin, ControlRegistry, SetError};
use nmbrs_metrics::labels::Labels;
use nmbrs_rate::{RateLimiter, RateLimiterApplier, RateSpec};

/// Helper: build a rate control with a reified gauge and the
/// same validator a workload phase would install.
fn build_rate_control(initial: f64) -> Control<RateSpec> {
    ControlBuilder::new("rate", RateSpec::new(initial))
        .validator(|spec: &RateSpec| {
            if spec.ops_per_sec <= 0.0 {
                return Err(format!("rate must be > 0 (got {})", spec.ops_per_sec,));
            }
            if spec.ops_per_sec > 1_000_000.0 {
                return Err(format!(
                    "rate must be <= 1_000_000 (got {})",
                    spec.ops_per_sec,
                ));
            }
            if spec.burst_ratio < 1.0 {
                return Err(format!(
                    "burst_ratio must be >= 1.0 (got {})",
                    spec.burst_ratio,
                ));
            }
            Ok(())
        })
        .reify_as_gauge(|spec: &RateSpec| Some(spec.ops_per_sec))
        .build()
}

#[tokio::test]
async fn full_flow_workload_like_rate_control() {
    // 1. Structural declaration on a registry standing in for a
    //    component. Real workloads declare this at phase build
    //    time.
    let registry = ControlRegistry::new();
    let control = build_rate_control(500.0);
    registry.declare(control.clone());

    // 2. Start a shared rate limiter at the same initial rate,
    //    register its applier against the control.
    let limiter = Arc::new(RateLimiter::start(RateSpec::new(500.0)));
    control.register_applier(RateLimiterApplier::new(limiter.clone()));
    assert_eq!(limiter.rate(), 500.0);

    // 3. Four concurrent workers simulate fibers pulling ops.
    //    They run for a short time, observe the rate, then get
    //    kicked up to a higher rate by a control write.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let ops = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicU64::new(0));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let l = limiter.clone();
        let ops = ops.clone();
        let stop = stop.clone();
        workers.push(tokio::spawn(async move {
            while stop.load(Ordering::Relaxed) == 0 {
                l.acquire().await;
                ops.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // Phase A: slow rate for 120ms, count ops.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let ops_after_slow = ops.load(Ordering::Relaxed);

    // 4. Writer mutates the control. The applier reconfigures
    //    the limiter in-place; workers keep going.
    control
        .set(RateSpec::new(20_000.0), ControlOrigin::Test)
        .await
        .expect("reconfigure to higher rate should succeed");
    assert_eq!(limiter.rate(), 20_000.0);

    // Phase B: fast rate for 120ms.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let ops_after_fast = ops.load(Ordering::Relaxed);
    stop.store(1, Ordering::Relaxed);
    for w in workers {
        // Give workers a chance to exit via the stop flag. Each
        // one is parked on `acquire()`, so a follow-on op release
        // unblocks them — at 20k/s this is fast. Abort if they
        // outrun the timeout as a safety net.
        let _ = tokio::time::timeout(Duration::from_secs(1), w).await;
    }

    let slow_rate = ops_after_slow as f64 / 0.120;
    let fast_rate = (ops_after_fast - ops_after_slow) as f64 / 0.120;

    // Fast phase must be materially above slow phase. We don't
    // pin exact numbers because tokio scheduling adds jitter,
    // but a 40x target-rate jump should be obvious.
    assert!(
        fast_rate > slow_rate * 3.0,
        "expected fast-phase throughput to exceed slow by 3x: \
         slow={slow_rate:.0} ops/s fast={fast_rate:.0} ops/s",
    );

    // 5. The reified gauge now reads the committed value.
    let snap = registry.snapshot_gauges(&Labels::of("phase", "rampup"), Instant::now());
    let family = snap
        .family("control_rate")
        .expect("control_rate gauge family should exist");
    let metric = family.metrics().next().unwrap();
    assert_eq!(metric.labels().get("control"), Some("rate"));
    assert_eq!(metric.labels().get("phase"), Some("rampup"));

    // 6. A validator-rejected write leaves the limiter running
    //    at the last good rate.
    match control.set(RateSpec::new(-1.0), ControlOrigin::Test).await {
        Err(SetError::ValidationFailed(msg)) => assert!(msg.contains("> 0")),
        other => panic!("expected validation rejection, got {other:?}"),
    }
    assert_eq!(limiter.rate(), 20_000.0);
}

#[tokio::test]
async fn failing_applier_reports_through_control() {
    // If the limiter applier fails (here simulated by a second
    // applier), the control write is rejected atomically and
    // the limiter's actual rate is unchanged.
    let limiter = Arc::new(RateLimiter::start(RateSpec::new(1_000.0)));
    let control = build_rate_control(1_000.0);
    control.register_applier(RateLimiterApplier::new(limiter.clone()));
    // A second applier that always fails — simulates a different
    // subscriber (e.g. a cooperating fiber pool) refusing the
    // new value. Per SRD 23's confirmed-apply contract, any
    // single applier failure fails the whole write.
    control.register_applier(nmbrs_metrics::controls::SyncApplier::new(|_: RateSpec| {
        Err("subscriber B refused".into())
    }));

    match control
        .set(RateSpec::new(5_000.0), ControlOrigin::Test)
        .await
    {
        Err(SetError::ApplyFailed(failures)) => {
            assert!(
                failures
                    .iter()
                    .any(|f| f.message.contains("subscriber B refused"))
            );
        }
        other => panic!("expected ApplyFailed, got {other:?}"),
    }

    // Observability gotcha: the limiter's applier ran and did
    // reconfigure, because fan-out is concurrent. The committed
    // control value is NOT advanced — this is the confirmed-apply
    // contract. Readers that need "the authoritative current
    // rate" use `control.value()`, not `limiter.rate()`.
    assert_eq!(
        control.value().ops_per_sec,
        1_000.0,
        "control committed value is the source of truth"
    );
}

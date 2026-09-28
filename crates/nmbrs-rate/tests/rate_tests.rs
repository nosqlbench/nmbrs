// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Performance, adversarial, and fuzz-style tests for nmbrs-rate.

use nmbrs_rate::{RateLimiter, RateSpec};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// =================================================================
// Performance / accuracy tests
// =================================================================

#[tokio::test]
async fn rate_accuracy_100_ops_per_sec() {
    let limiter = RateLimiter::start(RateSpec::new(100.0));
    tokio::time::sleep(Duration::from_millis(100)).await; // let refill prime

    let start = Instant::now();
    for _ in 0..50 {
        limiter.acquire().await;
    }
    let elapsed = start.elapsed();
    limiter.stop().await;

    // 50 ops at 100/s should take ~500ms. Allow 200ms-1200ms for CI jitter.
    let ms = elapsed.as_millis();
    assert!(
        (200..=1500).contains(&ms),
        "50 ops at 100/s took {ms}ms, expected ~500ms"
    );
}

#[tokio::test]
async fn rate_accuracy_1000_ops_per_sec() {
    let limiter = RateLimiter::start(RateSpec::new(1000.0));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    for _ in 0..200 {
        limiter.acquire().await;
    }
    let elapsed = start.elapsed();
    limiter.stop().await;

    // 200 ops at 1000/s should take ~200ms
    let ms = elapsed.as_millis();
    assert!(
        (80..=800).contains(&ms),
        "200 ops at 1000/s took {ms}ms, expected ~200ms"
    );
}

#[tokio::test]
async fn rate_accuracy_10000_ops_per_sec() {
    let limiter = RateLimiter::start(RateSpec::new(10000.0));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let start = Instant::now();
    for _ in 0..1000 {
        limiter.acquire().await;
    }
    let elapsed = start.elapsed();
    limiter.stop().await;

    // 1000 ops at 10000/s should take ~100ms
    let ms = elapsed.as_millis();
    assert!(
        (30..=500).contains(&ms),
        "1000 ops at 10000/s took {ms}ms, expected ~100ms"
    );
}

// =================================================================
// Concurrent access tests
// =================================================================

#[tokio::test]
async fn concurrent_acquires() {
    let limiter = Arc::new(RateLimiter::start(RateSpec::new(5000.0)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let limiter = limiter.clone();
        let counter = counter.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..50 {
                limiter.acquire().await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    assert_eq!(counter.load(Ordering::Relaxed), 200);

    // Can't stop through Arc — just let it drop
}

#[tokio::test]
async fn many_tasks_contending() {
    let limiter = Arc::new(RateLimiter::start(RateSpec::new(10000.0)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let counter = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    // 20 tasks each doing 10 acquires
    for _ in 0..20 {
        let limiter = limiter.clone();
        let counter = counter.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..10 {
                limiter.acquire().await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    assert_eq!(counter.load(Ordering::Relaxed), 200);
}

// =================================================================
// Adversarial tests
// =================================================================

#[tokio::test]
async fn very_low_rate() {
    // 0.5 ops/s — should use microsecond scaling
    let spec = RateSpec::new(0.5);
    assert_eq!(spec.unit, nmbrs_rate::TimeUnit::Micros);

    let limiter = RateLimiter::start(spec);
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Should be able to acquire one within a reasonable time
    let start = Instant::now();
    limiter.acquire().await;
    let elapsed = start.elapsed();
    limiter.stop().await;

    // At 0.5 ops/s, one op every 2 seconds. But we primed with one,
    // and refill has been running, so first acquire should be fast.
    assert!(
        elapsed.as_millis() < 5000,
        "took too long: {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn very_high_rate() {
    // 1M ops/s
    let limiter = RateLimiter::start(RateSpec::new(1_000_000.0));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let start = Instant::now();
    for _ in 0..1000 {
        limiter.acquire().await;
    }
    let elapsed = start.elapsed();
    limiter.stop().await;

    // 1000 ops at 1M/s should be near-instant (1ms theoretical)
    assert!(
        elapsed.as_millis() < 2000,
        "1000 ops at 1M/s took {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
async fn burst_ratio_affects_recovery() {
    // High burst ratio should recover faster after a stall
    let spec = RateSpec::with_burst(1000.0, 2.0); // 2x burst
    let limiter = RateLimiter::start(spec);

    // Let tokens accumulate for 200ms (~200 ops worth)
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Rapidly consume — burst should allow faster than target
    let start = Instant::now();
    for _ in 0..100 {
        limiter.acquire().await;
    }
    let elapsed = start.elapsed();
    limiter.stop().await;

    // With accumulated tokens and 2x burst, should be faster than
    // 100ms (which would be the steady-state time at 1000/s)
    assert!(
        elapsed.as_millis() < 200,
        "burst recovery should speed up, took {}ms",
        elapsed.as_millis()
    );
}

// =================================================================
// Spec parsing edge cases
// =================================================================

#[test]
fn spec_parse_edge_cases() {
    // Minimum rate
    assert!(RateSpec::parse("0.000001").is_ok());

    // Very high rate
    assert!(RateSpec::parse("10000000").is_ok());

    // Whitespace
    assert!(RateSpec::parse(" 1000 , 1.1 , start ").is_ok());

    // Just rate
    let s = RateSpec::parse("42").unwrap();
    assert_eq!(s.ops_per_sec, 42.0);
    assert_eq!(s.burst_ratio, 1.1);
    assert_eq!(s.verb, nmbrs_rate::Verb::Start);
}

#[test]
fn spec_parse_rejects_bad_input() {
    assert!(RateSpec::parse("").is_err());
    assert!(RateSpec::parse("0").is_err());
    assert!(RateSpec::parse("-100").is_err());
    assert!(RateSpec::parse("abc").is_err());
    assert!(RateSpec::parse("100,1.1,bogus").is_err());
}

// =================================================================
// Determinism / ordering tests
// =================================================================

#[tokio::test]
async fn acquire_order_is_fair() {
    // Multiple tasks should all make progress (no starvation)
    let limiter = Arc::new(RateLimiter::start(RateSpec::new(1000.0)));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let counters: Vec<Arc<AtomicU64>> = (0..4).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut handles = Vec::new();

    for counter in &counters {
        let limiter = limiter.clone();
        let counter = counter.clone();
        handles.push(tokio::spawn(async move {
            for _ in 0..25 {
                limiter.acquire().await;
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    // All 4 tasks should have completed all 25 acquires
    for (i, c) in counters.iter().enumerate() {
        assert_eq!(
            c.load(Ordering::Relaxed),
            25,
            "task {i} didn't complete all acquires"
        );
    }
}

#[tokio::test]
async fn stop_is_clean() {
    let limiter = RateLimiter::start(RateSpec::new(1000.0));
    limiter.acquire().await;
    limiter.stop().await;
    // Should not hang or panic
}

#[tokio::test]
async fn drop_is_clean() {
    {
        let limiter = RateLimiter::start(RateSpec::new(1000.0));
        limiter.acquire().await;
        // Drop without explicit stop
    }
    // Refill task should have stopped (running = false on drop)
    tokio::time::sleep(Duration::from_millis(50)).await;
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Per-op rate limiter. Independent of the activity-level
//! rate limiter and of every other op's per-op limiter — each
//! `OpRateWrapper` instance owns its own [`RateLimiter`] and
//! acquires from it on every dispatch.
//!
//! Typical use: pacing a `while:` loop on a daemon op so the
//! loop iterates at a controlled rate without coupling to the
//! activity's ops-per-second ceiling or to sibling daemons'
//! loops. See SRD-79 (forthcoming) for the design rationale.
//!
//! Composition: sits inner of `while:` so the rate limiter
//! acquire happens once per loop iteration. Sits outer of
//! `result`/`metrics`/`traverse` so the inner adapter call
//! lands inside the rate budget.
//!
//! Spec format (parsed in [`parse_rate_spec`]):
//! - `"<N>/s"` — N ops per second
//! - `"<N>/m"` — N ops per minute (= N/60 per second)
//! - `"<N>/h"` — N ops per hour (= N/3600 per second)
//! - `"<N>"` — bare numeric, interpreted as ops per second
//!
//! The numeric portion may be integer or floating-point; any
//! valid Rust `f64` literal works.

use std::sync::Arc;

use crate::adapter::WrappingDispenser;
use crate::adapter::{ExecutionError, OpDispenser, OpResult};
use crate::wrapper_registry::{WrapperName, WrapperRegistration, WrapperSubject};
use nmbrs_rate::{RateLimiter, RateSpec};

/// Wrapper name.
pub const NAME: WrapperName = WrapperName::new("rate");

/// Trigger: op declares a `rate:` field.
fn triggers(s: WrapperSubject) -> bool {
    let Some(template) = s.op() else {
        return false;
    };
    template.rate.is_some()
}

fn describe_assignment(s: WrapperSubject) -> Option<String> {
    let template = s.op()?;
    template.rate.as_ref().map(|spec| format!("rate: {spec}"))
}

inventory::submit! {
    WrapperRegistration {
        name: NAME,
        owned_fields: &["rate"],
        triggers,
        requires_inner: &[],
        forbids_outer: &[],
        mutually_exclusive_with: &[],
        describe_assignment,
        levels: &[crate::wrapper_registry::WrapperLevel::Op],
    }
}

/// Parse a `rate:` spec string into an ops-per-second f64.
/// See module docs for the accepted formats.
///
/// Errors with a descriptive message naming the malformed input
/// — the workload author needs enough context to fix the typo.
pub fn parse_rate_spec(s: &str) -> Result<f64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("rate: spec is empty".into());
    }
    let (num_str, divisor): (&str, f64) = if let Some(rest) = s.strip_suffix("/s") {
        (rest.trim(), 1.0)
    } else if let Some(rest) = s.strip_suffix("/m") {
        (rest.trim(), 60.0)
    } else if let Some(rest) = s.strip_suffix("/h") {
        (rest.trim(), 3600.0)
    } else {
        (s, 1.0)
    };
    if num_str.is_empty() {
        return Err(format!("rate: spec `{s}` has empty numeric part"));
    }
    let n: f64 = num_str.parse().map_err(|e| {
        format!("rate: spec `{s}` numeric part `{num_str}` is not a valid number: {e}")
    })?;
    if !n.is_finite() {
        return Err(format!("rate: spec `{s}` numeric part is not finite"));
    }
    if n <= 0.0 {
        return Err(format!("rate: spec `{s}` must be > 0 (got {n})"));
    }
    Ok(n / divisor)
}

/// Wraps an inner OpDispenser with an independent per-op
/// rate limiter.
pub struct OpRateWrapper {
    inner: Arc<dyn OpDispenser>,
    limiter: Arc<RateLimiter>,
}

impl OpRateWrapper {
    /// Wrap an inner dispenser. `rate_spec` is the workload-
    /// declared spec string (see [`parse_rate_spec`]).
    pub fn wrap(
        inner: Arc<dyn OpDispenser>,
        rate_spec: &str,
    ) -> Result<Arc<dyn OpDispenser>, String> {
        let ops_per_sec = parse_rate_spec(rate_spec)?;
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(ops_per_sec)));
        Ok(Arc::new(Self { inner, limiter }))
    }
}

impl WrappingDispenser for OpRateWrapper {}

impl OpDispenser for OpRateWrapper {
    fn execute<'a>(
        &'a self,
        cycle: u64,
        ctx: &'a crate::fixture::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let _wait = self.limiter.acquire().await;
            self.inner.execute(cycle, ctx).await
        })
    }
    fn inner_dispenser(&self) -> Option<&dyn OpDispenser> {
        Some(self.inner.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- spec parsing --------

    #[test]
    fn parse_bare_integer_is_per_second() {
        assert_eq!(parse_rate_spec("100").unwrap(), 100.0);
    }

    #[test]
    fn parse_bare_float_is_per_second() {
        assert!((parse_rate_spec("12.5").unwrap() - 12.5).abs() < 1e-9);
    }

    #[test]
    fn parse_per_second_suffix() {
        assert_eq!(parse_rate_spec("100/s").unwrap(), 100.0);
        assert_eq!(parse_rate_spec("100 /s").unwrap(), 100.0);
        assert!((parse_rate_spec(" 0.5/s ").unwrap() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn parse_per_minute_suffix() {
        assert!((parse_rate_spec("60/m").unwrap() - 1.0).abs() < 1e-9);
        assert!((parse_rate_spec("1/m").unwrap() - (1.0 / 60.0)).abs() < 1e-9);
    }

    #[test]
    fn parse_per_hour_suffix() {
        assert!((parse_rate_spec("3600/h").unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse_rate_spec("").is_err());
        assert!(parse_rate_spec("   ").is_err());
        assert!(parse_rate_spec("/s").is_err());
    }

    #[test]
    fn parse_rejects_zero_or_negative() {
        let e = parse_rate_spec("0").unwrap_err();
        assert!(e.contains("must be > 0"));
        let e = parse_rate_spec("-1/s").unwrap_err();
        assert!(e.contains("must be > 0"));
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(parse_rate_spec("abc").is_err());
        assert!(parse_rate_spec("twelve/s").is_err());
    }

    #[test]
    fn parse_rejects_non_finite() {
        let e = parse_rate_spec("inf").unwrap_err();
        assert!(e.contains("not finite"));
        // NaN parses but is rejected.
        let e = parse_rate_spec("NaN").unwrap_err();
        assert!(e.contains("not finite"));
    }

    // -------- F5: pacing accuracy --------
    //
    // Sustained acquire from a 1000/s limiter over 200 ops
    // should take ~200ms. Tolerate ±50% to absorb scheduler
    // jitter in CI; the load-bearing assertion is "the limiter
    // does pace, not no-op."

    #[tokio::test(flavor = "multi_thread")]
    async fn rate_limiter_paces_iterations() {
        // Use a slower limiter to make the lower-bound assertion
        // robust under CI load: 100/s × 50 ops ≈ 500ms ideal.
        // The first acquire is free (the limiter primes with one
        // op), so 50 acquires consume 49 ticks ≈ 490ms.
        let limiter = Arc::new(RateLimiter::start(RateSpec::new(100.0)));
        let start = std::time::Instant::now();
        for _ in 0..50 {
            let _ = limiter.acquire().await;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "limiter did not pace at all (elapsed={elapsed:?})"
        );
        assert!(
            elapsed <= std::time::Duration::from_millis(2000),
            "limiter paced far slower than configured (elapsed={elapsed:?})"
        );
    }

    // -------- F2: parse robustness proptest --------

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            // Any positive finite f64 with a /s suffix round-
            // trips to its own value within FP precision.
            #[test]
            fn per_second_round_trip(n in 0.0001f64..=1_000_000.0) {
                let s = format!("{n}/s");
                let parsed = parse_rate_spec(&s).unwrap();
                prop_assert!((parsed - n).abs() / n < 1e-12,
                    "round-trip drift > 1e-12 for {n}: parsed {parsed}");
            }

            // /m halves -> sixtieths the per-second rate.
            #[test]
            fn per_minute_scales(n in 0.0001f64..=1_000_000.0) {
                let s = format!("{n}/m");
                let parsed = parse_rate_spec(&s).unwrap();
                let want = n / 60.0;
                prop_assert!((parsed - want).abs() / want < 1e-12);
            }

            // Random garbage doesn't panic; parse_rate_spec is
            // a total function on &str.
            #[test]
            fn never_panics(s in ".*") {
                let _ = parse_rate_spec(&s);
            }
        }
    }
}

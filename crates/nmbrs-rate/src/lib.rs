// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-rate
//!
//! Contract & axioms: [SRD 06](../../docs/SRD/06_rate_limiter.md).
//!
//! Async-ready token-bucket rate limiter built on
//! [`tokio::sync::Semaphore`]. Designed for the nmbrs op-dispatch
//! loop but usable anywhere you need a rate cap that surfaces
//! *coordinated omission* — i.e., reports the time a caller spent
//! waiting for a permit, not just the time their op spent
//! executing.
//!
//! ## Design at a glance
//!
//! - A **spec** ([`RateSpec`]) declares the target ops/sec, an
//!   optional burst-recovery ratio, and a [`TimeUnit`] precision
//!   for the internal tick representation.
//! - A **limiter** ([`RateLimiter`]) is a long-lived handle that
//!   spawns a tokio refill task when started. Each
//!   [`RateLimiter::acquire`] call awaits a permit; the elapsed
//!   wait is exposed via [`RateLimiter::wait_time_nanos`].
//! - Live retarget via [`RateLimiter::reconfigure`] swaps the
//!   spec atomically without stopping the refill task — the next
//!   acquire reads the new tick-per-op count.
//!
//! ## Quick start
//!
//! ```no_run
//! use nmbrs_rate::{RateLimiter, RateSpec};
//!
//! # async fn run() {
//! // 1000 ops/sec target, default burst.
//! let limiter = RateLimiter::start(RateSpec::new(1_000.0));
//!
//! for _ in 0..10_000 {
//!     let backlog_ticks = limiter.acquire().await;
//!     // ... do work ...
//!     # let _ = backlog_ticks;
//! }
//!
//! // Live retarget: bump the ceiling 10x without stopping.
//! limiter.reconfigure(RateSpec::new(10_000.0)).unwrap();
//! # }
//! ```
//!
//! ## Spec syntax
//!
//! [`RateSpec::parse`] accepts comma-separated forms used by
//! workload params and CLI flags:
//!
//! ```text
//! 1000              # 1000 ops/s, default burst (1.1x), start verb
//! 1000,1.5          # 1000 ops/s, 1.5x burst recovery
//! 1000,1.1,restart  # full form with explicit verb
//! ```
//!
//! ```
//! use nmbrs_rate::RateSpec;
//!
//! let spec = RateSpec::parse("1000,1.5").unwrap();
//! assert_eq!(spec.ops_per_sec, 1000.0);
//! assert!((spec.burst_ratio - 1.5).abs() < 1e-9);
//! ```
//!
//! See `docs/SRD/notes/19_rate_limiter.md` for the design brief and
//! the coordinated-omission rationale.

mod applier;
mod limiter;
mod spec;

pub use applier::RateLimiterApplier;
pub use limiter::RateLimiter;
pub use spec::{RateSpec, TimeUnit, Verb};

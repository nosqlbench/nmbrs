// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs-optimizers` — standalone, runtime-free black-box optimization.
//!
//! SRD-86. This crate is the algorithm library behind the optimizer
//! seam: a set of derivative-free (non-gradient) multi-factor optimizers
//! that maximize an abstract [`Objective`] over a [`SearchSpace`]. It
//! depends on **nothing** in the nmbrs runtime (A2) — the runtime's
//! `PhaseFeed` implements [`Objective`] to bridge a probe to a phase run,
//! but this crate never sees a kernel, a metric, or a workload. That
//! keeps every optimizer unit-testable against synthetic manifold models
//! ([`testmodels`]) and the subsystem independently extractable.
//!
//! ## Conventions
//! - Optimizers **maximize** [`Observation::value`]. A minimization
//!   problem `f` is wrapped as `value = -f(x)` (see [`testmodels`]).
//! - Each [`Axis`] is [`AxisKind::Continuous`] (a real range) or
//!   [`AxisKind::Discrete`] (an explicit detent list). The optimizer
//!   searches the real box; [`SearchSpace::realize`] snaps/clamps a raw
//!   point to a realizable coordinate before each query.
//! - Algorithmic state (simplex, surrogate, trust region) lives in the
//!   optimizer (A3); the objective and search domain are the only inputs.
//!
//! ## Registry
//! [`by_name`] resolves an optimizer by its registered name (default
//! `"sweep"`). See [`registry`] for the full set.

pub mod algos;
pub mod docs;
pub mod optimizer;
pub mod registry;
pub mod rng;
pub mod space;
pub mod testmodels;

/// SRD-86 — the inventory bridge that registers these optimizers against the
/// core `nmbrs-runtime` contract. Enabled by the `runtime` feature; the
/// algorithm core (everything else) needs no runtime dependency.
#[cfg(feature = "runtime")]
pub mod bridge;

pub use optimizer::{AxisImpact, Budget, Objective, Observation, Optimizer, Report, StopReason};
pub use registry::{OptimizerParams, by_name, registered_names};
pub use space::{Axis, AxisKind, Changeover, SearchSpace};

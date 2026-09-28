// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-86 — the optimizer seam on the runtime side.
//!
//! The **contract** (the [`Optimizer`] / [`Objective`] traits, the search-space
//! types, the registry) lives in [`contract`] — defined in the core with no
//! dependency on any optimizer-algorithm crate. Algorithm crates (e.g.
//! `nmbrs-optimizers`) register implementations via `inventory` and are
//! discovered through [`contract::by_name`] / [`contract::describe`] at link
//! time; the core never names them.
//!
//! The live optimizer integration is owned by the **executor** (`crate::executor`):
//! `dispatch_optimization` builds the search space from a phase's `for_each`
//! comprehension and drives the chosen optimizer, actuating each axis by its
//! changeover class —
//!
//! - **Coordinate / Fixture** → phase-rerun: the adaptive loop runs the phase
//!   once per proposed coordinate (`CoordEval::Enumerated` looks a discrete
//!   coordinate up in the grid; `CoordEval::Synthesized` binds a sampled
//!   continuous coordinate);
//! - **Control** → the in-phase servoing daemon ([`servo`]), `tokio::join!`'d with
//!   one continuous phase, live-retargeting the SRD-23 control per setting;
//! - **mixed** → the hybrid (`run_hybrid_search`): coordinate axes form the outer
//!   rerun grid, control axes servo interior to each cell.
//!
//! Settling a windowed objective per evaluation is [`settle`], driven by the
//! cadence-pulse [`phase_pulse`] callback. This module proper is the contract
//! re-export surface over those submodules.

pub mod contract;
pub mod phase_pulse;
pub mod servo;
pub mod settle;

pub use contract::{
    Axis, AxisImpact, AxisKind, AxisValue, Budget, Changeover, Coord, CoordinateSource,
    FeedbackSource, LexSource, Objective, Observation, Optimizer, OptimizerInfo, OptimizerParams,
    OptimizerRegistration, PullOnly, PullSource, Report, SearchSpace, StopReason, SweepOptimizer,
    by_name, describe, registered_names,
};

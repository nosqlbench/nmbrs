// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Primary metric instruments.
//!
//! Instruments here are **recorded into on the hot path** and
//! **drained by the scheduler** on each tick. They capture raw
//! measurements — counters, histograms, gauges, and compound
//! timers — from the op-execution loop.
//!
//! For downstream retained views of instrument output (sparkline
//! buffers, live windows, lossless accumulators), see the sibling
//! [`crate::summaries`] module.

pub mod counter;
pub mod gauge;
pub mod histogram;
pub mod outcome;
pub mod timer;

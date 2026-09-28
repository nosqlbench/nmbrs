// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! The `metricsql_*` polydat node family (SRD-86 §"The metric-reader
//! surface").
//!
//! These nodes are the MetricsQL surface for polydat workloads: parse a
//! query, locate a metrics-access service
//! ([`nmbrs_metrics::queryapi`]), evaluate it through this crate's
//! engine, and project the result [`Vector`](nmbrs_metrics::queryapi::Vector)
//! into a polydat `Value` by result-type affinity ([`project`]).
//!
//! They live here (not in `nmbrs-metrics`) because they need the engine
//! *and* the queryapi *and* polydat together; `nmbrs-metrics` never
//! depends on this crate. Behind the `polydat-nodes` feature so the
//! engine stays polydat-free for parse/evaluate-only consumers.

pub mod nodes;
pub mod project;

pub use project::{ProjectError, Shape, project};

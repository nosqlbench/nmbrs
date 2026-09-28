// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Default status metrics for CQL adapters.
//!
//! Surfaced on the TUI status line and in the post-run summary.
//! Engine adapters call this from
//! [`DriverAdapter::default_status_metrics`](nmbrs_runtime::adapter::DriverAdapter::default_status_metrics)
//! so every CQL engine displays the same status row regardless
//! of which driver is active.

use nmbrs_runtime::adapter::{StatusMetric, StatusRender};

/// Status metrics every CQL engine surfaces by default.
///
/// Today: `rows_inserted` rendered as a rate (rows/s). Adding a
/// new metric here automatically picks up across every CQL engine
/// adapter that consumes this crate.
pub fn default_status_metrics() -> Vec<StatusMetric> {
    vec![StatusMetric {
        metric_name: "rows_inserted".to_string(),
        display: "rows/s".to_string(),
        render: StatusRender::Rate,
    }]
}

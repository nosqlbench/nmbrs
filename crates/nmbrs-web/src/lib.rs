// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-web
//!
//! Contract & axioms: [SRD 54](../../docs/SRD/54_web_ui.md).
//!
//! Axum + htmx web dashboard for nmbrs. Provides a browser-based
//! UI for monitoring running workloads, browsing Polydat functions
//! and stdlib modules, interacting with dynamic controls, and
//! viewing DAG visualizations.
//!
//! ## Pieces
//!
//! - [`server::build_router`] — assembles the Axum `Router` with
//!   all routes and the [`ws::MetricsBroadcast`] state attached.
//!   Used in two modes:
//!   - **Embedded**: `nmbrs run --web` registers the broadcast's
//!     reporter with the metrics scheduler so live frames flow
//!     to the WebSocket while the same process drives a
//!     workload.
//!   - **Standalone**: `nmbrs web` starts the router with no
//!     publisher; the dashboard accepts metric pushes via
//!     `POST /api/v1/import/prometheus`.
//! - [`routes`] — page handlers (dashboard, functions, stdlib,
//!   DAG viewer, graph editor) and JSON API endpoints (controls
//!   read/write, scope-tree snapshot, metric ingest, graph
//!   compile/eval/plot).
//! - [`ws`] — WebSocket fanout of metric frames.
//! - [`graph`] — interactive graph-editor backend: palette,
//!   compile, eval, plot.
//! - [`models`] — view-model types shared between Askama
//!   templates and the JSON endpoints.
//!
//! ## Routes (selected)
//!
//! | Path | Purpose |
//! |------|---------|
//! | `GET /` | Dashboard page (htmx-driven) |
//! | `GET /functions` | Polydat function reference |
//! | `GET /stdlib` | Stdlib `.polydat` module browser |
//! | `GET /dag` | DAG visualization page |
//! | `GET /graph` | Interactive graph editor |
//! | `GET /api/controls` | List dynamic controls (SRD 23) |
//! | `POST /api/control/{name}` | Write a control value |
//! | `GET /api/scope-tree` | Live scenario / scope tree (SRD 18b) |
//! | `POST /api/v1/import/prometheus` | Push OpenMetrics snapshot |
//! | `GET /ws/metrics` | WebSocket metric stream |
//!
//! ## See also
//!
//! - SRD 23 — dynamic controls (read/write surface)
//! - SRD 18b — scope tree exposed at `/api/scope-tree`
//! - SRD 42 — metrics cadence, reporter subscriptions

pub mod graph;
pub mod models;
pub mod routes;
pub mod server;
pub mod ws;

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-tui
//!
//! Contract & axioms: [SRD 59](../../docs/SRD/59_tui_contract.md).
//!
//! Terminal UI for nmbrs, built on [ratatui]. Provides a live
//! dashboard showing the scenario tree, per-phase progress and
//! detail blocks, throughput sparklines, latency percentile
//! histograms, an optional log panel, and a help overlay.
//!
//! ## Architecture
//!
//! - [`state::RunState`] — shared state between the executor
//!   thread and the render thread. Holds the canonical
//!   `SceneTree` (parent / children / status), the
//!   side-mapped `PhaseSummary` per node, active-phase live
//!   counters, log ring buffer, ops-history sparkline data,
//!   etc.
//! - [`app::App`] — owns the render loop. Reads `RunState` at
//!   the tick rate (default 4 Hz) and produces a frame.
//!   Handles keyboard input (LOD cycle, log toggle, pause,
//!   help, control-edit prompt).
//! - [`reporter`] — a `nmbrs_metrics::scheduler::Reporter`
//!   implementation that observers register so per-tick
//!   `MetricSet` snapshots flow into `RunState`'s sparkline /
//!   percentile histories.
//! - [`widgets`] — small custom widgets where ratatui's
//!   built-ins fall short.
//!
//! Lifecycle: `nmbrs` builds a [`run_state_actor::RunStateHandle`]
//! (the actor owns a private `RunState` and publishes immutable
//! `Arc<RunState>` snapshots via `arc_swap::ArcSwap`), hands the
//! handle to the runner-side executor via
//! [`observer::TuiObserver`] and to the TUI thread, and spawns
//! the TUI on a dedicated `std::thread` (not tokio — the TUI
//! never blocks the async runtime). Observers and the TUI both
//! mutate state by sending typed [`run_state_actor::RunStateCmd`]
//! messages into the actor's inbox; nothing ever takes a write
//! lock on shared state. See SRD-02 §"Display and Diagnostic
//! Decoupling".
//!
//! ## See also
//!
//! - SRD 62 (`docs/SRD/62_tui_layout.md`) — layout, LOD
//!   cycle, phase status glyphs, spinner cadence, "scenario
//!   done?" semantics.
//! - SRD 18b — scene tree (the canonical structure
//!   `RunState.tree` mirrors).
//!
//! [ratatui]: https://crates.io/crates/ratatui

pub mod app;
pub mod display_sink;
pub mod formatted_line_sink;
pub(crate) mod frame_broker;
pub mod inspector_repl;
pub mod inspector_server;
pub mod key_watcher;
pub mod log_only_observer;
pub mod log_only_sink;
pub mod observer;
pub(crate) mod prompt_state;
pub(crate) mod readout_panel;
pub(crate) mod readout_sink;
pub mod repl_state;
pub mod reporter;
pub mod run_state_actor;
pub mod sink_supervisor;
pub mod state;
pub mod status_fold;
pub(crate) mod tui_sink;
pub(crate) mod widgets;

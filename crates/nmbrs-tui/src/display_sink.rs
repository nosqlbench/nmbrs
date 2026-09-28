// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `DisplaySink` — uniform consumer interface for the run-state
//! snapshot stream.
//!
//! ## Architecture
//!
//! The runtime publishes state through one canonical channel:
//!
//! ```text
//!   RunObserver (lifecycle callbacks)
//!         │
//!         ▼ RunStateCmd
//!   RunState actor (mpsc → ArcSwap<Arc<RunState>>)
//!         │
//!         ▼ snapshot() / mpsc::Receiver<MetricSet>
//!   DisplaySink   ←──  the trait this module defines
//! ```
//!
//! Every consumer that wants to render the live run — terminal
//! TUI, line-mode "fake TUI" emulation, log-only stderr, future
//! external dashboards, the inspector socket — implements this
//! trait. The runtime stays oblivious to which sink is wired in;
//! it only knows about the actor.
//!
//! Three impls are planned:
//!
//! | Impl                | Surface | Owns terminal? |
//! |---------------------|---------|----------------|
//! | `TuiSink` (Phase 2) | `app::App` raw-mode + alt-screen | **yes** |
//! | `FakeTuiSink` (P2)  | line-mode emulation, stderr        | no |
//! | `LogOnlySink`       | stderr line-stream (current `tui=off` shape) | no |
//!
//! Subsystems that *don't* consume the live stream (plotter,
//! post-run summary reports — they read `metrics.db` after the
//! run finishes) sit outside this trait. The trait is for
//! live-stream consumers only.
//!
//! ## Lifecycle
//!
//! ```text
//!   sink = SomeSink::new(inputs);
//!   handle = sink.start();   // takes ownership; spawns thread(s)
//!   // ... runtime fires events into the actor ...
//!   handle.shutdown();        // graceful drain + teardown
//! ```
//!
//! `shutdown` is the cooperative-stop signal; the sink thread
//! flushes any pending buffered output, restores any terminal
//! state it modified (raw mode, alt-screen), and exits. The
//! returned thread join-handle on `start` lets the host wait for
//! a clean teardown.
//!
//! ## Why a trait, not an enum
//!
//! - Each sink lives in its own file, with its own dependencies.
//!   `TuiSink` needs `crossterm` + `ratatui`; `LogOnlySink`
//!   doesn't.
//! - Future external sinks (web dashboard, IPC bridge) can be
//!   added without touching every existing impl.
//! - The runner-side wiring code switches on `tui=` mode once at
//!   startup and hands the trait object to a single executor —
//!   one polymorphic dispatch on shutdown, none on the hot path.

use std::sync::Arc;
use std::sync::mpsc;

use nmbrs_metrics::metrics_query::MetricsQuery;
use nmbrs_metrics::snapshot::MetricSet;

use crate::run_state_actor::RunStateHandle;

/// Inputs every sink consumes. Cheap to construct: every field
/// is either an `Arc` clone or an `mpsc::Receiver` (single-
/// consumer, owned by the sink that takes it).
///
/// The two non-state fields are optional: a [`LogOnlySink`]
/// that just renders log lines doesn't need either; the eventual
/// [`crate::app::App`]-backed `TuiSink` and the future
/// `FakeTuiSink` need both. `None` means "this sink doesn't
/// consume frames / query the cadence store" — the runner-side
/// wiring is responsible for not registering a reporter when no
/// sink will drain it.
///
/// [`LogOnlySink`]: crate::log_only_sink::LogOnlySink
pub struct DisplayInputs {
    /// Read-only access to the actor's published `Arc<RunState>`.
    /// Sinks load snapshots via `state.load()` on their own
    /// schedule (TUI app polls at `tick_rate`; log-only sinks
    /// poll on log-seq deltas).
    pub state: RunStateHandle,
    /// Base-cadence metrics frames from the cadence reporter.
    /// `None` for sinks that don't render frames.
    pub frame_rx: Option<mpsc::Receiver<MetricSet>>,
    /// Cadence-windowed query handle. `None` for sinks that
    /// don't render per-cadence views.
    pub metrics_query: Option<Arc<MetricsQuery>>,
}

/// Sink for the run-state snapshot stream.
///
/// Implementors live on their own thread (or group of threads).
/// The trait surface is intentionally tiny: construct, start,
/// shut down. The runtime never reaches into the sink for
/// per-event delivery — events flow through the actor; the sink
/// drains.
pub trait DisplaySink: Send {
    /// Take ownership of the sink and spawn the rendering thread
    /// (or threads). Returns the join handle so the runner can
    /// wait on a clean teardown after `shutdown`. `inputs`
    /// carries everything the sink needs to read state and
    /// metrics — see [`DisplayInputs`].
    fn start(self: Box<Self>, inputs: DisplayInputs) -> Box<dyn SinkHandle>;
}

/// Handle returned by [`DisplaySink::start`]. The runner uses it
/// to signal shutdown and wait for the rendering thread to exit.
/// Holding this handle alive keeps the thread running; dropping
/// it without `shutdown` is a coding bug — the thread may leave
/// the terminal in raw mode or fail to flush its output.
pub trait SinkHandle: Send {
    /// Cooperative stop. Returns when the sink's thread has
    /// flushed its output and restored any terminal state it
    /// modified. Idempotent: a second call after the thread has
    /// already exited is a no-op.
    fn shutdown(self: Box<Self>);

    /// Whether this sink currently owns exclusive terminal access
    /// (raw mode, alt-screen). Honest answer per impl:
    /// `LogOnlySink` always returns `false`; `TuiSink` returns
    /// `true` while the alt-screen is up. Used by the runner to
    /// decide whether stdout reports (final summaries) should be
    /// deferred until after the sink shuts down vs. printed live.
    fn owns_terminal(&self) -> bool {
        false
    }
}

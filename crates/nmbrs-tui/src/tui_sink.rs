// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `TuiSink` — `DisplaySink` impl that runs the full TUI app.
//!
//! ## Role
//!
//! Phase 2 of the [`crate::display_sink`] refactor abstracts the
//! TUI startup / teardown behind the same trait that
//! [`crate::log_only_sink::LogOnlySink`] implements, so the
//! supervisor in `nmbrs/run` can swap between line-mode rendering
//! and the full TUI mid-run on a Ctrl-T toggle.
//!
//! Each `start()` call is a fresh TUI session: alt-screen,
//! mouse capture, raw mode, App thread. Each `shutdown()` tears
//! that down cleanly via [`crate::app::App::external_quit`] and
//! the App's existing `TerminalGuard::Drop`. A subsequent
//! `start()` (next toggle) creates a brand-new TUI session
//! against the same actor / broker / metrics-query — the
//! observer is unchanged, only the renderer comes and goes.
//!
//! ## What about TuiObserver?
//!
//! [`crate::observer::TuiObserver`] is the legacy single-shot
//! TUI driver: it embeds a `TuiReporter` channel, lazy-spawns
//! the app on first `phase_starting`, and is meant for
//! `tui=on`-from-startup. `TuiSink` replaces it for the
//! toggle-capable `tui=terminal` mode, where the observer is
//! [`crate::log_only_observer::LogOnlyObserver`] (always
//! present) and the *renderer* is whichever sink the supervisor
//! has up. The two coexist; `TuiObserver` stays for the legacy
//! direct-`tui=on` startup path until the supervisor subsumes
//! that case too.
//!
//! ## Yield vs. completion
//!
//! When the App's event loop exits, the supervisor wants to
//! distinguish "user hit Ctrl-T (or `q`) — bring the terminal
//! sink back" from "run is over — shut everything down". The
//! sink handle exposes
//! [`TuiSinkHandle::join_and_take_yield`] which joins the App
//! thread and returns the App's `yielded_to_terminal` flag.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use nmbrs_metrics::metrics_query::MetricsQuery;
use nmbrs_metrics::snapshot::MetricSet;

use crate::app::App;
use crate::display_sink::{DisplayInputs, DisplaySink, SinkHandle};

/// Externally-observable state of a running TUI sink. The
/// supervisor holds a clone of the inner `Arc`s so it can poll
/// the yield flag (Ctrl-T / `q` exit) without needing to
/// downcast the [`SinkHandle`] trait object.
#[derive(Clone, Default)]
pub struct TuiSinkSync {
    /// The supervisor flips this `true` to force the App's
    /// event loop to exit at its next iteration. The App
    /// reads it as `external_quit`.
    pub external_quit: Arc<AtomicBool>,
    /// The App writes `true` here just before returning if
    /// `yielded_to_terminal` was set (Ctrl-T or `q` from
    /// inside the TUI). The supervisor polls this to detect
    /// "user wants to swap back to terminal mode" without
    /// joining the thread (joining blocks).
    pub yielded: Arc<AtomicBool>,
}

pub struct TuiSink {
    /// Frame channel for the App. Subscribed from the broker by
    /// the supervisor before constructing the sink.
    frame_rx: Option<mpsc::Receiver<MetricSet>>,
    /// Cadence-windowed query handle for the App's barchart
    /// view. Required — the App constructor takes it
    /// unconditionally.
    metrics_query: Arc<MetricsQuery>,
    /// Shared coordination flags. The supervisor has a clone;
    /// the sink writes to / reads from the same `Arc`s.
    sync: TuiSinkSync,
}

impl TuiSink {
    /// Build with the broker subscription, metrics query, and a
    /// pre-shared [`TuiSinkSync`] the supervisor will poll for
    /// the yield flag and flip on shutdown.
    pub fn new(
        frame_rx: mpsc::Receiver<MetricSet>,
        metrics_query: Arc<MetricsQuery>,
        sync: TuiSinkSync,
    ) -> Self {
        Self {
            frame_rx: Some(frame_rx),
            metrics_query,
            sync,
        }
    }
}

impl DisplaySink for TuiSink {
    fn start(self: Box<Self>, inputs: DisplayInputs) -> Box<dyn SinkHandle> {
        let DisplayInputs {
            state,
            frame_rx: _,
            metrics_query: _,
        } = inputs;
        // The trait surface carries `frame_rx` / `metrics_query`
        // through `DisplayInputs`, but TuiSink owns its own
        // (collected at construction by the supervisor) because
        // the broker subscription has to happen *before*
        // `start` so the receiver outlives multiple toggles.
        // The trait fields are ignored here.
        let TuiSink {
            frame_rx,
            metrics_query,
            sync,
        } = *self;
        let frame_rx = frame_rx.expect("TuiSink must be constructed with frame_rx");

        let external_quit_for_thread = sync.external_quit.clone();
        let yielded_for_thread = sync.yielded.clone();

        let join: JoinHandle<()> = std::thread::Builder::new()
            .name("tui-sink".into())
            .spawn(move || {
                let mut app = App::new(frame_rx, state, metrics_query);
                app.external_quit = external_quit_for_thread;
                if let Err(e) = app.run() {
                    eprintln!("TUI error: {e}");
                }
                // Publish the yield flag *before* the thread
                // returns, so a busy-poll reader sees the
                // result the moment the App is done.
                yielded_for_thread.store(app.yielded_to_terminal, Ordering::Release);
            })
            .expect("spawn tui-sink thread");

        Box::new(TuiSinkHandle {
            sync,
            join: Some(join),
        })
    }
}

pub struct TuiSinkHandle {
    sync: TuiSinkSync,
    join: Option<JoinHandle<()>>,
}

impl SinkHandle for TuiSinkHandle {
    fn shutdown(mut self: Box<Self>) {
        // Force the App's event loop to exit at its next
        // iteration. `TerminalGuard::Drop` restores the
        // terminal as the thread unwinds; we just wait for the
        // join.
        self.sync.external_quit.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }

    fn owns_terminal(&self) -> bool {
        true
    }
}

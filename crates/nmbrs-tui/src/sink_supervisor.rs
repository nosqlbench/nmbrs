// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `SinkSupervisor` — coordinates the active
//! [`crate::display_sink::DisplaySink`] and the
//! [`crate::key_watcher::KeyWatcher`] across a `tui=terminal`
//! run.
//!
//! ## Why
//!
//! `tui=terminal` is interactive: the operator can hit Ctrl-T
//! at any moment to swap from line-mode rendering
//! (`LogOnlySink`) up to the full TUI (`TuiSink`), and Ctrl-T
//! again (or `q`) inside the TUI to swap back. The supervisor
//! is the one component that watches for those signals and
//! drives the sink lifecycle.
//!
//! ## State machine
//!
//! ```text
//!   ┌─ Terminal ──── Ctrl-T ──→  TUI ───────┐
//!   │   • LogOnlySink                       │
//!   │   • KeyWatcher (raw stdin)            │
//!   │                                       │
//!   └────────── Ctrl-T or q ←───────────────┘
//!                              (App writes
//!                              `yielded_to_terminal`)
//! ```
//!
//! Stdin ownership is exclusive: only one of {KeyWatcher, App}
//! has raw-mode read access at a time. The transition function
//! always tears the outgoing side down before bringing the
//! incoming side up.
//!
//! ## Lifetime
//!
//! Owns its own OS thread (not async). The runner's async
//! future runs in parallel; when it completes, the runner
//! drops a sentinel into the supervisor's "run done" channel
//! and the supervisor tears the active sink + watcher down
//! before exiting.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::display_sink::{DisplayInputs, DisplaySink, SinkHandle};
use crate::key_watcher::{KeyWatcher, WatcherSignal};
use crate::log_only_observer::LogOnlyObserver;
use crate::log_only_sink::{LogOnlySink, fresh_resume_cursor};
use crate::run_state_actor::RunStateHandle;
use crate::tui_sink::{TuiSink, TuiSinkSync};

/// Handle held by the runner-side caller. The supervisor runs
/// on its own OS thread; [`Self::shutdown`] is called once
/// the runner future completes.
pub struct SinkSupervisor {
    /// Sentinel channel: drop the sender (in `shutdown`) and
    /// the supervisor's `try_recv` returns `Disconnected`,
    /// breaking the loop.
    done_tx: Option<mpsc::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl SinkSupervisor {
    /// Spawn the supervisor. Initial state is the terminal sink
    /// (`LogOnlySink` + `KeyWatcher`). If stdin isn't a TTY,
    /// the watcher refuses to spawn and the supervisor exits
    /// immediately — leaving the observer's synchronous
    /// stderr write path active (legacy `tui=off` behaviour).
    pub fn spawn(
        observer: Arc<LogOnlyObserver>,
        state: RunStateHandle,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Self {
        // SRD-93 M7 — arm the console-loss demote before any sink
        // touches the terminal, so a SIGHUP at any point in the
        // supervised lifetime lands on the flag the loop polls.
        install_console_loss_hook();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let join = std::thread::Builder::new()
            .name("sink-supervisor".into())
            .spawn(move || run_supervision(observer, state, done_rx, runtime))
            .expect("spawn sink-supervisor thread");
        Self {
            done_tx: Some(done_tx),
            join: Some(join),
        }
    }

    /// Tell the supervisor the run has finished. Tears down
    /// the active sink + watcher and joins. Idempotent.
    pub fn shutdown(mut self) {
        drop(self.done_tx.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// What the supervisor is currently driving. Tear-down + bring-
/// up between modes is sequential — never side-by-side stdin.
enum ActiveSink {
    Terminal {
        sink_handle: Box<dyn SinkHandle>,
        /// `None` when stdin isn't a TTY — the LogOnlySink is
        /// still rendering to the stderr terminal, just without
        /// interactive key handling / prompt. Auto-degraded path
        /// for piped-stdin invocations.
        watcher: Option<KeyWatcher>,
        signal_rx: mpsc::Receiver<WatcherSignal>,
        /// Forwarder for prompt-bound signals. The supervisor's
        /// match arms route `Key` / `GrowPrompt` / `ShrinkPrompt`
        /// / `ToggleHelp` here; the `LogOnlySink` owns the
        /// receiver and drives its embedded `PromptState`.
        /// Unused in the degraded (no-watcher) mode — kept on
        /// the struct so the enum variant doesn't need
        /// per-mode shape, but no traffic ever flows.
        prompt_tx: mpsc::Sender<WatcherSignal>,
    },
    Tui {
        sink_handle: Box<dyn SinkHandle>,
        sync: TuiSinkSync,
    },
}

/// SRD-93 M7 — set by the console-loss hook (SIGHUP received by the
/// signal dispatcher: the controlling pty/ssh is GONE). The
/// supervisor polls it each tick and demotes to headless: the run
/// continues, only the display dies. Process-global and one-way,
/// like the console itself.
static CONSOLE_LOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Install the M7 console-loss hook. Idempotent (first install
/// wins in `session_signals`); called at supervisor spawn so every
/// `tui=terminal` run demotes cleanly instead of rendering to a
/// dead terminal. The hook runs on the signal-dispatch thread —
/// one relaxed store, nothing blocking.
pub fn install_console_loss_hook() {
    nmbrs_runtime::session_signals::set_console_loss_hook(Box::new(|| {
        CONSOLE_LOST.store(true, std::sync::atomic::Ordering::Relaxed);
    }));
}

fn console_lost() -> bool {
    CONSOLE_LOST.load(std::sync::atomic::Ordering::Relaxed)
}

fn run_supervision(
    observer: Arc<LogOnlyObserver>,
    state: RunStateHandle,
    done_rx: mpsc::Receiver<()>,
    runtime: Option<tokio::runtime::Handle>,
) {
    // Two distinct flags the supervisor manages:
    //   • sink_active     — observer's synchronous-stderr
    //                       suppression. Held high while *any*
    //                       sink is rendering (LogOnlySink
    //                       sets/clears it on its own; the
    //                       supervisor re-asserts during the
    //                       TUI swap).
    //   • inline_suppress — activity's inline-status thread
    //                       suppression. Held high *only*
    //                       while an alt-screen TUI owns the
    //                       terminal — i.e. during the TuiSink
    //                       window. Cleared in plain
    //                       `tui=terminal` so the per-cycle
    //                       status line keeps rendering
    //                       alongside the LogOnlySink's log
    //                       drain (they share stderr without
    //                       conflict).
    let sink_active_flag = observer.sink_active_flag();
    let inline_suppress = observer.inline_suppress_flag();
    // Cross-swap log cursor: each LogOnlySink writes its final
    // `last_seen` here on shutdown and the next one (after a TUI
    // swap) resumes from it, so the lines that scrolled under the
    // alternate screen are re-emitted into the restored scrollback.
    // Starts `RESUME_FRESH` so the very first sink seeds from the
    // live `log_seq_total` instead.
    let resume_from = fresh_resume_cursor();
    let mut active = match start_terminal(&observer, &state, runtime.clone(), &resume_from) {
        Some(a) => a,
        None => {
            // No TTY — KeyWatcher refused. Fall through:
            // the observer's synchronous stderr-write path is
            // already active (sink_active stays false), so the
            // operator gets the same output the legacy
            // `tui=off` mode delivered.
            wait_for_done(&done_rx);
            return;
        }
    };

    const TICK: Duration = Duration::from_millis(100);

    loop {
        // Runner finished?
        match done_rx.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        // SRD-93 M7 — console lost (SIGHUP): the pty/ssh is gone and
        // is never coming back. Demote to headless: permanently
        // suppress every terminal-bound write (logs continue through
        // the session.log sink), tear the active sink down
        // best-effort, restore the (dead) terminal's modes for
        // hygiene, and hold until the RUNNER — which is unaffected —
        // finishes. Before this seam existed, a pty-spawned run
        // survived SIGHUP per the M7 contract but kept rendering to
        // the dead terminal; the PTY test harnesses then deadlocked
        // behind a child that never exited (observed 2026-08-04:
        // 39-minute orphaned test runs at 100% CPU).
        if console_lost() {
            sink_active_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            inline_suppress.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = crossterm::terminal::disable_raw_mode();
            match active {
                ActiveSink::Terminal { sink_handle, .. } => sink_handle.shutdown(),
                ActiveSink::Tui { sink_handle, .. } => sink_handle.shutdown(),
            }
            wait_for_done(&done_rx);
            return;
        }

        // Per-state handling. Each branch may swap `active`
        // and `continue`-loop, restarting the polling cycle.
        let mut swap_to: Option<Transition> = None;
        match &active {
            ActiveSink::Terminal {
                signal_rx,
                prompt_tx,
                ..
            } => {
                while let Ok(sig) = signal_rx.try_recv() {
                    match sig {
                        WatcherSignal::ToggleTui => {
                            swap_to = Some(Transition::TerminalToTui);
                            break;
                        }
                        WatcherSignal::Interrupt => {
                            // In raw mode the terminal's Ctrl-C→SIGINT
                            // translation is OFF, so this keystroke is NOT a
                            // signal. Drive the shutdown LADDER directly
                            // rather than re-raising SIGINT: the TUI's
                            // `install_signal_terminal_restore` sigaction
                            // handler intercepts SIGINT and hard-terminates
                            // (restoring the terminal) before the runtime's
                            // graceful handler can run — so a re-raise here
                            // loses both the operator notice and the metrics
                            // flush. Each keystroke advances one rung:
                            // graceful → cancel in-flight ops (process
                            // cleanup continues) → force-exit. Only the
                            // force-exit rung is handled here (it owns the
                            // terminal restore); the first two rungs are the
                            // shared `escalate_shutdown` ladder.
                            if nmbrs_runtime::session_signals::shutdown_level() >= 2 {
                                let _ = crossterm::terminal::disable_raw_mode();
                                std::process::exit(130);
                            } else {
                                nmbrs_runtime::session_signals::escalate_shutdown(
                                    nmbrs_runtime::session_signals::ShutdownOrigin::CtrlC,
                                );
                            }
                        }
                        WatcherSignal::Suspend => {
                            // Honour the user's expectation that
                            // Ctrl-Z suspends the process, just
                            // as a cooked-mode shell would. We
                            // briefly drop raw mode so the
                            // foreground/background dance behaves
                            // sensibly, raise SIGTSTP, and re-
                            // enable raw mode after the OS
                            // resumes us via `fg`.
                            // Job control is a Unix concept —
                            // on Windows Ctrl-Z is just a byte
                            // (EOF in cooked mode); ignore it.
                            #[cfg(unix)]
                            {
                                let _ = crossterm::terminal::disable_raw_mode();
                                unsafe {
                                    libc::raise(libc::SIGTSTP);
                                }
                                // Execution resumes here when `fg`
                                // delivers SIGCONT. Re-arm the
                                // terminal for keystroke detection.
                                let _ = crossterm::terminal::enable_raw_mode();
                            }
                        }
                        WatcherSignal::Redraw => {
                            // Standard cooked-terminal Ctrl-L:
                            // clear the screen and park the
                            // cursor at the home position. The
                            // LogOnlySink will continue printing
                            // new lines on its normal cadence —
                            // no replay of historical buffer
                            // (matches what users see from a
                            // cooked-shell `clear`).
                            use std::io::Write;
                            let mut err = std::io::stderr();
                            let _ = err.write_all(b"\x1b[2J\x1b[H");
                            let _ = err.flush();
                        }
                        WatcherSignal::ExplainPulse => {
                            // `?` keystroke: toggle the global
                            // explainer overlay. First press
                            // turns it on with a 10 s auto-
                            // revert deadline; second press
                            // while on turns it off. Auto-
                            // repeat-safe via the debounce
                            // inside `toggle_explain`.
                            nmbrs_runtime::observer::toggle_explain();
                        }
                        WatcherSignal::ReplToggleBar => {
                            // `~` keystroke: cycle REPL
                            // visibility (Hidden ↔ Bar; any
                            // visible state → Hidden). The
                            // global state has its own
                            // auto-repeat debounce, so holding
                            // the key doesn't strobe.
                            crate::repl_state::toggle_bar();
                        }
                        WatcherSignal::ReplToggleWindow => {
                            // `Ctrl-~` keystroke: open or close
                            // the full-screen REPL window.
                            crate::repl_state::toggle_window();
                        }
                        // Prompt-bound signals. The
                        // `LogOnlySink` owns the receiver and
                        // applies these directly to its
                        // `PromptState`. A disconnected
                        // receiver (sink shutting down) is
                        // not fatal here — we just drop the
                        // signal; the sink will catch up on
                        // its next start.
                        sig @ (WatcherSignal::Key(_)
                        | WatcherSignal::GrowPrompt
                        | WatcherSignal::ShrinkPrompt
                        | WatcherSignal::ToggleHelp) => {
                            let _ = prompt_tx.send(sig);
                        }
                    }
                }
            }
            ActiveSink::Tui { sync, .. } => {
                if sync.yielded.load(Ordering::Acquire) {
                    swap_to = Some(Transition::TuiToTerminal);
                }
            }
        }

        if let Some(t) = swap_to {
            active = match t {
                Transition::TerminalToTui => {
                    swap_to_tui(&observer, &state, active, runtime.clone(), &resume_from)
                }
                Transition::TuiToTerminal => {
                    // Wait for the App thread to fully exit and
                    // restore the terminal before bringing the
                    // KeyWatcher back up (otherwise both might
                    // claim raw mode at once).
                    teardown(active);
                    // Release the inline-status suppression now
                    // that the alt-screen is gone — the
                    // LogOnlySink's log drain and the inline
                    // status line coexist on stderr without
                    // conflict, so the per-cycle status should
                    // be visible again. `sink_active` is
                    // re-asserted by `start_terminal` →
                    // `LogOnlySink::start`.
                    observer
                        .inline_suppress_flag()
                        .store(false, Ordering::Release);
                    match start_terminal(&observer, &state, runtime.clone(), &resume_from) {
                        Some(a) => a,
                        None => {
                            // Lost the TTY (unexpected). Fall
                            // through to no-supervisor; runner
                            // continues with synchronous stderr.
                            return;
                        }
                    }
                }
            };
            continue;
        }

        std::thread::sleep(TICK);
    }

    teardown(active);
    // Final clear: whichever sink we tore down might or might
    // not have left the flag set (LogOnlySink clears
    // sink_active on shutdown; TuiSink doesn't touch it).
    // After supervisor exit there's nothing rendering, so the
    // inline-status thread (if still alive) and any straggler
    // synchronous-stderr writes from the observer should be
    // unsuppressed.
    sink_active_flag.store(false, Ordering::Release);
    inline_suppress.store(false, Ordering::Release);
}

fn wait_for_done(done_rx: &mpsc::Receiver<()>) {
    // No supervised state — just block until the runner
    // signals completion.
    let _ = done_rx.recv();
}

enum Transition {
    TerminalToTui,
    TuiToTerminal,
}

fn start_terminal(
    observer: &Arc<LogOnlyObserver>,
    state: &RunStateHandle,
    runtime: Option<tokio::runtime::Handle>,
    resume_from: &Arc<AtomicU64>,
) -> Option<ActiveSink> {
    // Stderr-only TTY case (stdin is piped/redirected — common
    // for `nmbrs run ... 2>&1 | tee log.txt` or invocation under
    // a parent process that captures stdin): KeyWatcher refuses
    // to spawn (no raw-mode access), but the stderr terminal can
    // still render the status block via absolute positioning. We
    // start LogOnlySink without key plumbing — same readouts,
    // same in-place updates, just no interactive keys/prompt.
    let (signal_tx, signal_rx) = mpsc::channel::<WatcherSignal>();
    let watcher = KeyWatcher::spawn(signal_tx);

    // Prompt-bound channel: only wired when KeyWatcher is up.
    // The supervisor's match forwards prompt-bound signals
    // here; in the watcher-less degraded mode this channel
    // never carries traffic, so `prompt_tx` is `None` and
    // the sink is constructed without a key receiver.
    let (prompt_tx, prompt_rx) = mpsc::channel::<WatcherSignal>();

    let min_level = observer.min_level();
    let sink_active = observer.sink_active_flag();
    let mut sink = LogOnlySink::new(min_level, sink_active).with_resume(resume_from.clone());
    if watcher.is_some() {
        sink = sink.with_keys(prompt_rx, runtime);
    }
    let sink_handle = Box::new(sink).start(DisplayInputs {
        state: state.clone(),
        frame_rx: None,
        metrics_query: None,
    });

    Some(ActiveSink::Terminal {
        sink_handle,
        watcher,
        signal_rx,
        prompt_tx,
    })
}

fn swap_to_tui(
    observer: &Arc<LogOnlyObserver>,
    state: &RunStateHandle,
    active: ActiveSink,
    runtime: Option<tokio::runtime::Handle>,
    resume_from: &Arc<AtomicU64>,
) -> ActiveSink {
    // Tear down terminal mode first — the watcher disables
    // raw mode + releases stdin so the App can claim it.
    if let ActiveSink::Terminal {
        sink_handle,
        watcher,
        prompt_tx,
        ..
    } = active
    {
        // Dropping `prompt_tx` disconnects the prompt receiver
        // inside the sink so its drain loop exits cleanly; the
        // sink_handle.shutdown() that follows then joins.
        drop(prompt_tx);
        sink_handle.shutdown();
        if let Some(w) = watcher {
            w.shutdown();
        }
    } else {
        unreachable!("swap_to_tui called outside Terminal state");
    }

    let frame_rx = observer.subscribe_frames();
    let query = match observer.metrics_query() {
        Some(q) => q,
        None => {
            // Cadence reporter not yet wired by the runner —
            // this can only happen if the user hits Ctrl-T
            // before the first phase fires. Fall back to
            // terminal mode with a one-line notice.
            let _ = std::io::Write::write_all(
                &mut std::io::stderr(),
                b"Ctrl-T: TUI not yet ready (metrics scheduler pending) - retry once the run is underway\r\n",
            );
            return start_terminal(observer, state, runtime, resume_from)
                .expect("re-entering terminal mode after deferred swap");
        }
    };

    // Two flags now flip on the way into TUI mode:
    //   • sink_active — re-asserted because LogOnlySink's
    //     shutdown cleared it, and the TUI also "renders log
    //     lines" (inside its own panel) so the observer must
    //     stay quiet on stderr.
    //   • inline_suppress — first becomes true here. The TUI
    //     owns the alt-screen; the inline-status thread's
    //     `\r\x1b[K…` would otherwise overwrite the TUI's
    //     bottom-row content.
    observer.sink_active_flag().store(true, Ordering::Release);
    observer
        .inline_suppress_flag()
        .store(true, Ordering::Release);

    let sync = TuiSinkSync::default();
    let sink = Box::new(TuiSink::new(frame_rx, query, sync.clone()));
    let sink_handle = sink.start(DisplayInputs {
        state: state.clone(),
        frame_rx: None,
        metrics_query: None,
    });

    ActiveSink::Tui { sink_handle, sync }
}

fn teardown(active: ActiveSink) {
    match active {
        ActiveSink::Terminal {
            sink_handle,
            watcher,
            ..
        } => {
            sink_handle.shutdown();
            if let Some(w) = watcher {
                w.shutdown();
            }
        }
        ActiveSink::Tui { sink_handle, .. } => {
            sink_handle.shutdown();
        }
    }
}

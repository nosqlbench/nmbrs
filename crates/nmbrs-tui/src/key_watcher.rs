// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Stdin keystroke watcher for terminal-mode display.
//!
//! Single responsibility: read single keystrokes from stdin and
//! fire a signal when the operator hits **Ctrl-T**, which the
//! [supervisor] uses to toggle between
//! [`crate::log_only_sink::LogOnlySink`] (the line-mode default)
//! and the full TUI sink.
//!
//! [supervisor]: crate::sink_supervisor
//!
//! ## Why a separate module
//!
//! Stdin can have only one reader at a time. The full TUI app
//! takes raw-mode stdin while it owns the screen; in terminal
//! mode the supervisor wants to listen for Ctrl-T without
//! interfering. Splitting the watcher out lets the supervisor
//! start it when in terminal mode and drop it (its thread
//! exits) when handing stdin off to the TUI.
//!
//! ## TTY-only
//!
//! The watcher refuses to construct itself unless stdin is a
//! TTY. Piped / CI invocations get [`Self::new`] returning
//! `None` and the supervisor degrades to the no-toggle path
//! (legacy `tui=off`-equivalent rendering, no Ctrl-T sensing).
//! Same isatty check the runner already uses to pick the
//! default mode.
//!
//! ## Raw-mode coordination
//!
//! Crossterm's raw mode is process-global. The watcher enables
//! raw mode on `start` and disables on `shutdown`. While the
//! watcher is up, stderr writes need `\r\n` line endings — see
//! the [`render_line`] helper in [`crate::log_only_sink`] which
//! handles that.
//!
//! [`render_line`]: crate::log_only_sink

use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

/// Signals the watcher emits.
///
/// `tui=terminal` runs with stdin in raw mode so the watcher
/// can detect the toggle key (Ctrl-T). Raw mode also disables
/// the kernel's normal handling of "shell control" keystrokes
/// — Ctrl-Z, Ctrl-L, Ctrl-C — which would otherwise be
/// translated to signals or interpreted by the line discipline.
/// The watcher recognises those keys explicitly and emits a
/// signal so the supervisor can reproduce the expected
/// behaviour (suspend, redraw, graceful shutdown) — anything a
/// user would normally expect from those keys keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatcherSignal {
    /// Ctrl-T pressed. Supervisor reaction: swap the active
    /// display sink (terminal ↔ TUI).
    ToggleTui,
    /// Ctrl-C pressed. Supervisor reaction: forward as a
    /// graceful-shutdown to the runtime, same path the
    /// `session_signals` SIGINT handler triggers.
    Interrupt,
    /// Ctrl-Z pressed. Supervisor reaction: disable raw mode,
    /// raise SIGTSTP, the OS suspends the whole process. On
    /// resume (`fg`), the supervisor re-enables raw mode and
    /// the watcher continues. Same UX a cooked-mode shell would
    /// have given the user.
    Suspend,
    /// Ctrl-L pressed. Supervisor reaction: clear the screen
    /// — the standard "redraw" behaviour for cooked terminals.
    Redraw,
    /// Alt+Up pressed. Supervisor reaction: grow the prompt
    /// window by one row.
    GrowPrompt,
    /// Alt+Down pressed. Supervisor reaction: shrink the
    /// prompt window by one row.
    ShrinkPrompt,
    /// Ctrl-/ pressed. Supervisor reaction: toggle the
    /// prompt's keystroke-help overlay.
    ToggleHelp,
    /// `?` pressed. Supervisor reaction: call
    /// `observer::toggle_explain()` — toggle the explainer
    /// overlay on (with a 10 s auto-revert deadline) or off
    /// (if it was already on). Auto-repeat from holding the
    /// key is swallowed by the toggle's own debounce.
    ExplainPulse,
    /// `~` pressed. Supervisor reaction: cycle the REPL
    /// visibility through Hidden → Bar → Hidden (and Window
    /// → Hidden if currently Window). Tilde is the universal
    /// "toggle off" key — pressing it from any visible state
    /// returns to Hidden. See [`crate::repl_state`] for the
    /// full state machine.
    ReplToggleBar,
    /// Backtick (`` ` ``) pressed. Supervisor reaction: open
    /// the full-screen REPL window (or close it if already
    /// open). Chosen instead of `Ctrl-~` because the latter
    /// proved unreliable across terminals — many terminal
    /// emulators intercept or fail to deliver the chord. A
    /// bare key with a single keysym + no modifier dependency
    /// works uniformly across stock terminals. See
    /// [`crate::repl_state`] for the full state machine.
    ReplToggleWindow,
    /// Any other keypress — printable chars, line-editing
    /// chords, history nav, Enter, etc. Forwarded to the
    /// prompt without interpretation. Supervisor receives the
    /// raw [`crossterm::event::KeyEvent`] inside [`Self::Key`]
    /// so the [`crate::prompt_state::PromptState`] can apply
    /// the same key translation as a terminal-native editor.
    Key(crossterm::event::KeyEvent),
}

/// Handle the supervisor holds while the watcher is running.
/// Dropping (or calling `shutdown`) tears the thread down,
/// disables raw mode, and stops keystroke delivery.
pub struct KeyWatcher {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl KeyWatcher {
    /// Spawn the watcher. Returns `None` when stdin is not a
    /// TTY (piped / CI / backgrounded — no keystrokes possible).
    /// The signal channel `tx` receives one [`WatcherSignal`]
    /// per recognised keystroke; the supervisor owns the
    /// receiver.
    ///
    /// On construction, raw mode is enabled on the process
    /// terminal. Do not call this if another component
    /// (e.g. an active TUI app) currently owns stdin — use
    /// the supervisor's start/stop dance instead.
    pub fn spawn(tx: mpsc::Sender<WatcherSignal>) -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        // Capture the cooked-mode termios *before* flipping to
        // raw mode, then install the async-signal-safe terminal-
        // restore handler the App's TerminalGuard already uses.
        // Without this, a Ctrl-C → graceful-shutdown → second
        // Ctrl-C → force-exit path leaves the shell in raw mode
        // because none of the cooperative `Drop` paths get to
        // run before `std::process::exit`. Both helpers are
        // idempotent — calling them twice (e.g. terminal mode
        // with a later toggle to TUI which constructs a real
        // `TerminalGuard`) is safe.
        crate::app::save_pretui_termios();
        crate::app::install_signal_terminal_restore();
        // Restore the terminal on PANIC too, not just on signals. Without this a
        // panic during a run (e.g. a kernel coercion failure) prints its message
        // while the tty is still in raw mode, stair-stepping every line (`\n`
        // with no `\r`). Idempotent with the full TUI's identical call.
        crate::app::install_tui_panic_hook();
        if let Err(e) = crossterm::terminal::enable_raw_mode() {
            eprintln!("key_watcher: enable_raw_mode failed: {e} — toggle disabled");
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let join = std::thread::Builder::new()
            .name("key-watcher".into())
            .spawn(move || run_loop(tx, stop_for_thread))
            .expect("spawn key-watcher thread");
        Some(Self {
            stop,
            join: Some(join),
        })
    }

    /// Cooperative stop. Disables raw mode after the polling
    /// thread exits so subsequent stderr / stdout writes use
    /// normal line endings again. Idempotent — safe to call
    /// after the thread has already exited (the second call is
    /// a no-op).
    ///
    /// The join is BOUNDED. The watcher thread can be wedged in
    /// a kernel `read()` on a tty that will never deliver
    /// another byte — observed under PTY test harnesses, where
    /// crossterm's poll reports readiness but the subsequent
    /// blocking read never returns. A thread stuck in `read()`
    /// cannot be interrupted portably; waiting on it wedged the
    /// entire teardown chain (sink-supervisor → run teardown →
    /// process exit) behind a keystroke that will never come.
    /// After the grace window the thread is deliberately leaked
    /// — process exit reaps it.
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let deadline = std::time::Instant::now() + Duration::from_millis(500);
            loop {
                if join.is_finished() {
                    let _ = join.join();
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    // No logging here on purpose: this runs during
                    // display-plane teardown, when the log sinks may
                    // already be gone. The leak is silent and benign
                    // — process exit reaps the thread.
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

fn run_loop(tx: mpsc::Sender<WatcherSignal>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        // 100 ms poll cadence: fast enough that a Ctrl-T feels
        // immediate, slow enough that idle CPU is negligible.
        //
        // On error, sleep the same cadence before retrying: a
        // dead terminal (PTY master closed under a test harness,
        // hangup on a real tty) makes poll/read fail INSTANTLY,
        // and a bare `continue` turns this loop into a 100%-CPU
        // spin for as long as the process lingers.
        let ready = match event::poll(Duration::from_millis(100)) {
            Ok(b) => b,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if !ready {
            continue;
        }
        let evt = match event::read() {
            Ok(e) => e,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        match evt {
            Event::Key(k) if k.kind == KeyEventKind::Press => {
                // Global hotkeys first — these never reach the
                // prompt because they have process-wide meaning
                // (interrupt, suspend, redraw, mode toggle,
                // window resize, help overlay). Everything else
                // is forwarded as a raw key for the prompt to
                // interpret with its own keymap.
                let signal = match (k.code, k.modifiers) {
                    (KeyCode::Char('t'), m) | (KeyCode::Char('T'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        WatcherSignal::ToggleTui
                    }
                    (KeyCode::Char('c'), m) | (KeyCode::Char('C'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        WatcherSignal::Interrupt
                    }
                    (KeyCode::Char('z'), m) | (KeyCode::Char('Z'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        WatcherSignal::Suspend
                    }
                    (KeyCode::Char('l'), m) | (KeyCode::Char('L'), m)
                        if m.contains(KeyModifiers::CONTROL) =>
                    {
                        WatcherSignal::Redraw
                    }
                    (KeyCode::Up, m) if m.contains(KeyModifiers::ALT) => WatcherSignal::GrowPrompt,
                    (KeyCode::Down, m) if m.contains(KeyModifiers::ALT) => {
                        WatcherSignal::ShrinkPrompt
                    }
                    (KeyCode::Char('/'), m) if m.contains(KeyModifiers::CONTROL) => {
                        WatcherSignal::ToggleHelp
                    }
                    // `?` (no modifier) — explainer-overlay hold.
                    // Each press / auto-repeat extends the
                    // deadline; the readout binder reads
                    // `observer::is_explain_held()` on each
                    // fire and renders `ContentMode::Explanation`
                    // while the deadline hasn't passed. Shift
                    // is implicit on US layouts; we match the
                    // `?` character regardless of modifier so
                    // alternate layouts that produce `?`
                    // without Shift work too.
                    (KeyCode::Char('?'), _) => WatcherSignal::ExplainPulse,
                    // `~` toggles the bar (Hidden ↔ Bar,
                    // collapses Window → Hidden). Auto-repeat-
                    // safe via the supervisor's debounce.
                    (KeyCode::Char('~'), _) => WatcherSignal::ReplToggleBar,
                    // Backtick `` ` `` toggles the full-screen
                    // REPL window. Picked over `Ctrl-~` because
                    // terminals deliver the bare chord more
                    // reliably than the Ctrl-modified one.
                    (KeyCode::Char('`'), _) => WatcherSignal::ReplToggleWindow,
                    _ => WatcherSignal::Key(k),
                };
                // Supervisor receiver dropped → run is over,
                // exit the loop cleanly.
                if tx.send(signal).is_err() {
                    return;
                }
            }
            _ => {}
        }
    }
}

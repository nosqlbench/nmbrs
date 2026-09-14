// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Test-only harness that drives the `tui=terminal` display
//! ([`nmbrs_tui::log_only_sink::LogOnlySink`]) from a **mock data
//! source** — the run-state actor — instead of a real workload.
//!
//! ## Why this exists
//!
//! The display renders purely from the snapshot the run-state actor
//! publishes (`display = f(snapshot)`). Validating the fragile
//! cursor/region accounting against a *real* run is the wrong tool —
//! a real run paces itself, emits adapter output, and can't be
//! advanced one step at a time. This harness feeds the actor a
//! controlled sequence of [`RunStateCmd`]s read line-by-line from
//! stdin, so a [`shadow_terminal`] test can lock-step the display:
//! send one state change, observe the rendered cells, send the next.
//!
//! It uses only `nmbrs-tui`'s public API — no production code is
//! altered to make the display observable.
//!
//! ## Protocol (one command per stdin line)
//!
//! - `tree <name>...`   — install a scene tree of the named phases.
//! - `start <name>`     — mark a phase Running.
//! - `done <name>`      — mark a phase Completed.
//! - `log <text...>`    — push a diagnostic log line (scrollback);
//!   `|` splits it into lines for a multi-line block (`||` = a blank
//!   row, as a phase-outcome error readout has).
//! - `status <a|b|c>`   — publish the live status block; `|` splits
//!   it into rows. Empty text clears it.
//! - `out <text...>`    — push console (REPL) transcript output.
//! - `bar` / `window`   — toggle the REPL bar / window.
//! - `swap`             — simulate a **Ctrl-T cycle**: tear the sink
//!   down (it clears its status rows), save+restore the primary via
//!   the alternate screen exactly as the full TUI does, then bring a
//!   **fresh** sink up on the same resume cursor. This reproduces the
//!   one path a single sink can't: a first-paint over a surface the
//!   previous sink already touched.
//! - `quit`             — shut the sink down and exit.
//!
//! The sink renders on its own ~50 ms poll cadence; the driving test
//! waits for the expected cells to appear after each command.

use std::io::{Read, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::mpsc;

use nmbrs_runtime::observer::LogLevel;
use nmbrs_tui::display_sink::{DisplayInputs, DisplaySink, SinkHandle};
use nmbrs_tui::key_watcher::WatcherSignal;
use nmbrs_tui::log_only_sink::{LogOnlySink, fresh_resume_cursor};
use nmbrs_tui::run_state_actor::{RunStateCmd, RunStateHandle, spawn_run_state_actor};
use nmbrs_tui::state::{EntryKind, LogSeverity, RunState, SceneTree};

/// Put fd 0 (stdin) into a non-canonical, no-echo mode so the
/// driving test's command bytes are delivered immediately and are
/// NOT echoed back onto the rendered surface. Returns the prior
/// settings for restoration.
#[cfg(unix)]
fn set_raw_stdin() -> Option<libc::termios> {
    unsafe {
        let mut prev: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut prev) != 0 {
            return None;
        }
        let mut raw = prev;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &raw);
        Some(prev)
    }
}

#[cfg(unix)]
fn restore_stdin(prev: Option<libc::termios>) {
    if let Some(prev) = prev {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &prev);
        }
    }
}

/// Windows analogue of the termios tweak: clear the console
/// input handle's line-input + echo bits so command bytes are
/// delivered immediately and never echo onto the rendered
/// surface. Returns the prior mode for restoration.
#[cfg(windows)]
fn set_raw_stdin() -> Option<u32> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const ENABLE_LINE_INPUT: u32 = 0x2;
    const ENABLE_ECHO_INPUT: u32 = 0x4;
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut prev = 0u32;
        if h.is_null() || GetConsoleMode(h, &mut prev) == 0 {
            return None;
        }
        let raw = prev & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
        if SetConsoleMode(h, raw) == 0 {
            return None;
        }
        Some(prev)
    }
}

#[cfg(windows)]
fn restore_stdin(prev: Option<u32>) {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(std_handle: u32) -> *mut c_void;
        fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
    }
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    if let Some(mode) = prev {
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            if !h.is_null() {
                SetConsoleMode(h, mode);
            }
        }
    }
}

/// Start a fresh terminal-mode sink on `state`, sharing `resume_from`
/// across swaps. Returns the handle plus the key sender (kept alive
/// so the sink keeps its `PromptState`). A fresh key channel per sink
/// mirrors the supervisor, which rebuilds the KeyWatcher each swap.
fn start_sink(
    state: &RunStateHandle,
    resume_from: &Arc<AtomicU64>,
) -> (Box<dyn SinkHandle>, mpsc::Sender<WatcherSignal>) {
    let sink_active = Arc::new(AtomicBool::new(false));
    let (key_tx, key_rx) = mpsc::channel::<WatcherSignal>();
    let sink = LogOnlySink::new(LogLevel::Info, sink_active)
        .with_keys(key_rx, None)
        .with_resume(resume_from.clone());
    let handle = Box::new(sink).start(DisplayInputs {
        state: state.clone(),
        frame_rx: None,
        metrics_query: None,
    });
    (handle, key_tx)
}

/// Apply one non-lifecycle command. Returns `true` to request quit.
fn dispatch(line: &str, handle: &RunStateHandle) -> bool {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("tree") => {
            let mut tree = SceneTree::new();
            let root = tree.root();
            for name in parts {
                tree.push(root, EntryKind::Phase, name, "");
            }
            handle.send(RunStateCmd::InstallTree(tree));
        }
        Some("start") => {
            if let Some(name) = parts.next() {
                handle.send(RunStateCmd::PhaseStarting {
                    exec_id: 1,
                    // Manual by-name driver: route via find_phase against
                    // the installed tree (scene_node_id = 0 = root). SRD-100 P1c.
                    scene_node_id: 0,
                    name: name.to_string(),
                    labels: String::new(),
                    op_templates: 1,
                    total_cycles: 100,
                    concurrency: 1,
                });
            }
        }
        Some("sysmon") => {
            // `sysmon <disk> <cpu> <maxcore> <mem> <cache>` — fractions.
            let mut f = || {
                parts
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0)
            };
            let (disk, cpu, maxcore, mem, cache) = (f(), f(), f(), f(), f());
            handle.send(RunStateCmd::Sysmon(nmbrs_runtime::sysmon::SysmonSample {
                cpu: Some(nmbrs_runtime::sysmon::CpuReading {
                    mean: cpu,
                    max_core: maxcore,
                    top_core: 7,
                    // No display surface reads the per-core quartiles
                    // yet; whoever adds one should give the harness
                    // representative values rather than these zeros.
                    ..Default::default()
                }),
                io: Some(("nvme1n1".into(), disk)),
                ram: Some((mem, cache)),
                rambw: None,
                storage: None,
            }));
        }
        Some("done") => {
            if let Some(name) = parts.next() {
                handle.send(RunStateCmd::PhaseCompleted {
                    exec_id: 1,
                    scene_node_id: 0,
                    name: name.to_string(),
                    labels: String::new(),
                    duration_secs: 1.23,
                });
            }
        }
        Some("log") => {
            // `|` splits the message into lines, so a test can drive a
            // multi-line log block — e.g. a phase-outcome error readout
            // (a CQL error + statement + a `(+N more)` tail), embedded
            // blank rows included (`||`).
            let msg: String = parts.collect::<Vec<_>>().join(" ").replace('|', "\n");
            handle.send(RunStateCmd::Log {
                severity: LogSeverity::Info,
                tag: nmbrs_tui::state::EventTag::default(),
                message: msg,
            });
        }
        Some("status") => {
            // The live status block. `|` separates rows so a test can
            // drive a multi-row status (which is what makes the
            // bottom-region scroll-on-growth fire).
            //
            // SRD-100 P2 — the status line is no longer a scalar the
            // harness sets directly; it is folded at the consumer from
            // `active_phases`. So drive it through the real fold: inject a
            // synthetic active phase whose `on_update` body is the literal
            // text. The consumer (`status_fold::render_active_status`)
            // fires that body and produces exactly the same bytes the old
            // `SetStatusLine(text)` did — the footer geometry under test is
            // unchanged. Empty text removes the phase, so the fold clears.
            const LIVE: &str = "·live·";
            let text: String = parts.collect::<Vec<_>>().join(" ");
            if text.is_empty() {
                handle.send(RunStateCmd::PhaseCompleted {
                    exec_id: 1,
                    scene_node_id: 0,
                    name: LIVE.to_string(),
                    labels: String::new(),
                    duration_secs: 0.0,
                });
            } else {
                let rendered = text.replace('|', "\n");
                // (Re)create the synthetic phase, then attach a literal-body
                // render handle carrying the text.
                handle.send(RunStateCmd::PhaseStarting {
                    exec_id: 1,
                    scene_node_id: 0,
                    name: LIVE.to_string(),
                    labels: String::new(),
                    op_templates: 1,
                    total_cycles: 100,
                    concurrency: 1,
                });
                let body = nmbrs_runtime::readouts::BakedBody::from_steps(vec![
                    nmbrs_runtime::readouts::RenderStep::Literal(rendered),
                ]);
                handle.send(RunStateCmd::AttachPhaseRender(
                    nmbrs_runtime::observer::PhaseRenderHandle {
                        exec_id: 1,
                        name: LIVE.to_string(),
                        labels: String::new(),
                        activity_name: LIVE.to_string(),
                        metrics: Arc::new(nmbrs_runtime::activity::ActivityMetrics::new(
                            &nmbrs_metrics::labels::Labels::empty(),
                        )),
                        bodies: Arc::new(vec![body]),
                        memo: Arc::new(arc_swap::ArcSwap::from_pointee(String::new())),
                        gutter: Arc::new(arc_swap::ArcSwapOption::empty()),
                        status_metrics: Arc::from(Vec::<String>::new()),
                        concurrency: 1,
                        seq: None,
                        depth_indent: String::new(),
                    },
                ));
            }
        }
        Some("out") => {
            let text: String = parts.collect::<Vec<_>>().join(" ");
            nmbrs_tui::repl_state::push_transcript_line(&text);
        }
        Some("bar") => {
            nmbrs_tui::repl_state::toggle_bar();
        }
        Some("window") => {
            nmbrs_tui::repl_state::toggle_window();
        }
        Some("quit") => return true,
        _ => {}
    }
    false
}

fn main() {
    // Startup marker so the driving shadow terminal's `start()`
    // (which blocks until the screen is non-empty) returns promptly.
    {
        let mut err = std::io::stderr();
        let _ = write!(err, "display-harness ready\r\n");
        let _ = err.flush();
    }

    let prev_termios = set_raw_stdin();

    // Mock data source: a standalone run-state actor. No runner,
    // no executor, no adapter — just the display's upstream.
    let initial = RunState::new("harness", "default", "stdout");
    let (handle, _actor) = spawn_run_state_actor(initial);

    // Cross-swap resume cursor, shared across every sink we bring up
    // — exactly as the supervisor shares one across Ctrl-T swaps.
    let resume_from = fresh_resume_cursor();
    let (h0, tx0) = start_sink(&handle, &resume_from);
    let mut sink_handle: Option<Box<dyn SinkHandle>> = Some(h0);
    let mut key_tx = tx0;

    let mut stdin = std::io::stdin();
    let mut byte = [0u8; 1];
    let mut line = String::new();
    loop {
        match stdin.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let c = byte[0];
                if c != b'\n' && c != b'\r' {
                    line.push(c as char);
                    continue;
                }
                let trimmed = line.trim().to_string();
                line.clear();
                if trimmed.is_empty() {
                    continue;
                }
                if trimmed == "swap" {
                    // Tear the current sink down (it clears its status
                    // rows), save+restore the primary surface via the
                    // alternate screen — the same save/restore the full
                    // TUI does on Ctrl-T — then bring up a fresh sink on
                    // the shared resume cursor.
                    if let Some(h) = sink_handle.take() {
                        h.shutdown();
                    }
                    {
                        // Cycle the alternate screen as the full TUI
                        // does on Ctrl-T: enter, use it (clear + a
                        // marker), leave — the terminal saves the
                        // primary on enter and restores it on leave.
                        let mut err = std::io::stderr();
                        let _ = err.write_all(b"\x1b[?1049h\x1b[2J\x1b[H[harness-tui]\x1b[?1049l");
                        let _ = err.flush();
                    }
                    let (h, tx) = start_sink(&handle, &resume_from);
                    sink_handle = Some(h);
                    key_tx = tx; // drops the prior sender
                    continue;
                }
                if dispatch(&trimmed, &handle) {
                    break;
                }
            }
        }
    }

    drop(key_tx);
    if let Some(h) = sink_handle.take() {
        h.shutdown();
    }
    restore_stdin(prev_termios);
}

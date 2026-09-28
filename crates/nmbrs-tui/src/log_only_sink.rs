// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `LogOnlySink` — `DisplaySink` impl that renders every
//! log entry to stderr.
//!
//! ## Role
//!
//! In Phase 2 of the display-sink refactor (see
//! [`crate::display_sink`]) this sink is the canonical renderer
//! for `tui=off` mode. The
//! [`crate::log_only_observer::LogOnlyObserver`] sends every
//! log event to the actor; this sink polls the actor's published
//! [`crate::state::RunState`] snapshot, drains anything new from
//! the log ring (tracked via
//! [`crate::state::RunState::log_seq_total`]), applies the
//! observer-supplied severity filter, and emits stderr lines
//! identical to the legacy `StderrObserver` path.
//!
//! ## Coordination with the observer
//!
//! The sink takes a shared `sink_active` flag from the observer
//! it's paired with (see
//! [`crate::log_only_observer::LogOnlyObserver::sink_active_flag`]).
//! The startup sequence is:
//!
//! 1. `LogOnlySink::new` records `last_seen_seq = 0` (provisional).
//! 2. `start()` reads the actor's current snapshot, sets
//!    `last_seen_seq = snapshot.log_seq_total` so any pre-sink
//!    entries already on stderr (from the observer's synchronous
//!    write) aren't re-rendered.
//! 3. `start()` sets `sink_active = true` — from this moment the
//!    observer suppresses its own stderr writes and the sink owns
//!    the surface.
//! 4. The render thread polls every ~50 ms, drains **every** log
//!    line the actor has queued on the durable scrollback stream
//!    since the last tick (see
//!    [`crate::run_state_actor::LogLine`]), prints those that pass
//!    `min_level`, and advances `last_seen_seq`.
//!
//! `shutdown()` clears `sink_active` so the observer resumes
//! synchronous writes for any straggler logs that fire after the
//! sink is gone.
//!
//! ## Polling cadence
//!
//! 50 ms. Fast enough that a human operator doesn't perceive lag
//! between an event and its line appearing; slow enough that
//! idle ticks have negligible CPU cost. The cadence reporter's
//! frame channel is also drained on the same loop so it never
//! reports a full / disconnected channel.
//!
//! ## No drop
//!
//! Log scrollback is sequential DATA — every line must be
//! delivered, in order, exactly once. It rides a durable,
//! **unbounded** stream (an `mpsc::channel` fed by the actor per
//! [`crate::run_state_actor::RunStateCmd::Log`]) that the render
//! thread drains FULLY every tick, NOT the bounded `RunState`
//! log ring. The ring is a latest-wins snapshot buffer (fine for
//! the inspector view, the TUI log panel, and the failure dump)
//! but it can evict an un-emitted line when the renderer lags the
//! actor by more than its capacity — the drop this stream removes.
//! A slow terminal lets the stream grow during a burst (bounded by
//! the run's total log volume); the renderer drains it on the next
//! lull. `session.log` still captures every level unconditionally
//! (the async sink in `nmbrs_runtime::log_sink`, see SRD 02
//! §"Display and Diagnostic Decoupling").

use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

use nmbrs_runtime::observer::LogLevel;

use crate::display_sink::{DisplayInputs, DisplaySink, SinkHandle};
use crate::key_watcher::WatcherSignal;
use crate::prompt_state::{PromptAction, PromptState};
use crate::state::LogSeverity;

const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Sentinel `resume_from` value meaning "fresh start": the sink
/// seeds `last_seen` from the live `log_seq_total` so it doesn't
/// re-emit anything the observer already printed. Any other value
/// is a swap re-entry — the sink resumes from that exact seq so the
/// log lines that scrolled while the TUI owned the alternate screen
/// are re-printed into the restored scrollback (terminal history IS
/// the log stream, reliable across mode swaps, with no managed
/// screen-buffer to reconstruct).
pub(crate) const RESUME_FRESH: u64 = u64::MAX;

/// A fresh cross-swap resume cursor to share across a sink's
/// lifetime — starts at [`RESUME_FRESH`] so the first sink seeds
/// from the live log position and each post-swap sink resumes from
/// the prior sink's final cursor. Used by the supervisor and the
/// `tui_display_harness` example (which simulates a Ctrl-T swap by
/// tearing a sink down and starting a fresh one on the same cursor).
/// See [`LogOnlySink::with_resume`].
pub fn fresh_resume_cursor() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(RESUME_FRESH))
}

/// `tui=off` log-stream sink.
pub struct LogOnlySink {
    /// Severity floor. The paired observer's filter, propagated
    /// here so pre-sink and post-sink stderr output use the same
    /// rule. Lines below `min_level` are silently dropped (still
    /// captured by the async session-log sink).
    min_level: LogLevel,
    /// Coordination flag shared with
    /// [`crate::log_only_observer::LogOnlyObserver`]. The sink
    /// flips it `true` on `start`, `false` on `shutdown`.
    sink_active: Arc<AtomicBool>,
    /// Channel from the supervisor's key forwarding. When
    /// present, the sink renders an interactive prompt at the
    /// bottom of the surface and dispatches Enter-submitted
    /// commands to the inspector handler. `None` falls back to
    /// the legacy "log stream + status region" layout used by
    /// piped / CI / no-tty runs.
    key_rx: Option<mpsc::Receiver<WatcherSignal>>,
    /// Tokio runtime handle threaded through to the inspector
    /// dispatcher so prompt-submitted `set <name> <value>`
    /// commands can write controls. `None` disables the `set`
    /// command at the prompt (everything else still works).
    runtime: Option<tokio::runtime::Handle>,
    /// Cross-swap log cursor shared with the supervisor. The sink
    /// seeds `last_seen` from it on `start` (unless it holds
    /// [`RESUME_FRESH`]) and writes its final `last_seen` back to it
    /// on shutdown, so the next terminal-mode sink the supervisor
    /// brings up after a TUI swap re-emits exactly the lines that
    /// scrolled while the alternate screen was up. `None` for
    /// standalone (non-supervised) sinks, which always start fresh.
    resume_from: Option<Arc<AtomicU64>>,
}

impl LogOnlySink {
    pub fn new(min_level: LogLevel, sink_active: Arc<AtomicBool>) -> Self {
        Self {
            min_level,
            sink_active,
            key_rx: None,
            runtime: None,
            resume_from: None,
        }
    }

    /// Share a cross-swap log cursor with the supervisor so this
    /// sink re-emits the lines that scrolled while a TUI swap held
    /// the alternate screen, instead of skipping them (see
    /// [`RESUME_FRESH`]).
    pub fn with_resume(mut self, resume_from: Arc<AtomicU64>) -> Self {
        self.resume_from = Some(resume_from);
        self
    }

    /// Wire an interactive prompt into this sink. The receiver
    /// will be drained on every render tick; each
    /// `WatcherSignal::Key(...)` feeds the [`PromptState`]'s
    /// keymap and the resize / help / interrupt chords adjust
    /// layout directly. Without a key receiver the sink runs
    /// in its legacy log-only mode.
    pub fn with_keys(
        mut self,
        key_rx: mpsc::Receiver<WatcherSignal>,
        runtime: Option<tokio::runtime::Handle>,
    ) -> Self {
        self.key_rx = Some(key_rx);
        self.runtime = runtime;
        self
    }
}

/// Choose the initial log cursor for a (re)started terminal-mode
/// sink. With no shared cursor, or one still holding
/// [`RESUME_FRESH`], the sink seeds from the live `current`
/// `log_seq_total` so it skips history the observer already
/// printed. Any other shared value is a swap re-entry: the sink
/// resumes from that exact prior cursor so the lines that scrolled
/// while the TUI's alternate screen was up get re-emitted into the
/// restored scrollback.
fn seed_last_seen(resume: Option<u64>, current: u64) -> u64 {
    match resume {
        Some(v) if v != RESUME_FRESH => v,
        _ => current,
    }
}

fn severity_to_level(s: LogSeverity) -> LogLevel {
    match s {
        LogSeverity::Debug => LogLevel::Debug,
        LogSeverity::Info => LogLevel::Info,
        LogSeverity::Warn => LogLevel::Warn,
        LogSeverity::Error => LogLevel::Error,
    }
}

impl DisplaySink for LogOnlySink {
    fn start(self: Box<Self>, inputs: DisplayInputs) -> Box<dyn SinkHandle> {
        let DisplayInputs {
            state,
            frame_rx,
            metrics_query: _,
        } = inputs;
        let LogOnlySink {
            min_level,
            sink_active,
            key_rx,
            runtime,
            resume_from,
        } = *self;

        // Claim the durable scrollback stream FIRST — this flips the
        // actor's "buffer scrollback deltas" latch on (see
        // [`crate::run_state_actor::RunStateHandle::take_log_stream`]).
        // Claiming BEFORE the seed read below guarantees every line the
        // sink will emit (seq greater than the seed) is already in the
        // stream: no line sent during the handoff falls in a gap. The
        // guard restores the receiver to the shared cell on teardown so
        // a Ctrl-T swap-back resumes the same stream.
        let scrollback = state.take_log_stream();

        // Seed the log cursor. A fresh sink snapshots the current
        // `log_seq_total` so it doesn't re-emit anything the observer
        // already printed pre-flag. A swap re-entry (the supervisor's
        // `resume_from` holds the prior sink's final cursor, not
        // `RESUME_FRESH`) resumes from that exact seq so the lines
        // that scrolled under the TUI's alternate screen are
        // re-printed into the restored scrollback.
        let initial_seq = seed_last_seen(
            resume_from.as_ref().map(|a| a.load(Ordering::Acquire)),
            state.load().log_seq_total,
        );
        sink_active.store(true, Ordering::Release);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let sink_active_for_thread = sink_active.clone();
        let state_for_thread = state.clone();

        let join = std::thread::Builder::new()
            .name("log-only-sink".into())
            .spawn(move || {
                run_render_loop(
                    state_for_thread,
                    frame_rx,
                    initial_seq,
                    min_level,
                    stop_for_thread,
                    key_rx,
                    runtime,
                    resume_from,
                    scrollback,
                );
                // Render thread exited (stop signaled or channel
                // disconnected). Clear the flag so the observer
                // resumes synchronous writes.
                sink_active_for_thread.store(false, Ordering::Release);
            })
            .expect("spawn log-only-sink thread");

        Box::new(LogOnlySinkHandle {
            stop,
            join: Some(join),
            sink_active,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn run_render_loop(
    state: crate::run_state_actor::RunStateHandle,
    frame_rx: Option<std::sync::mpsc::Receiver<nmbrs_metrics::snapshot::MetricSet>>,
    mut last_seen: u64,
    min_level: LogLevel,
    stop: Arc<AtomicBool>,
    key_rx: Option<mpsc::Receiver<WatcherSignal>>,
    runtime: Option<tokio::runtime::Handle>,
    resume_from: Option<Arc<AtomicU64>>,
    scrollback: crate::run_state_actor::ScrollbackReceiver,
) {
    let mut stderr = io::stderr();
    // The raw status string most recently published by the
    // actor and reflected on the terminal. We compare against
    // *this* (not the clamped form actually written to the
    // surface) so identity checks are stable across ticks.
    let mut status_published: Option<String> = None;
    // True until this sink's first footer paint commits. The
    // return-to-footer-top + clear in step (A) must NOT run on the
    // first paint: no footer is drawn yet and the cursor sits just
    // after the last emitted log (or on a freshly restored surface),
    // so the first paint just emits logs and draws the footer in
    // place. On a fresh surface there is nothing above the cursor to
    // clear; clearing to end-of-screen from a higher cursor would
    // wipe live content.
    let mut first_paint = true;
    // Follow-the-log sticky footer: rows the cursor sits BELOW the
    // footer's first row after a committed redraw (the prompt input
    // row when an inline bar is shown, else the last status row). The
    // next tick climbs back up by this much to reach the footer top
    // before clearing it. The terminal's `?1049` cursor save/restore
    // keeps this valid across a console alt-screen excursion, so it
    // needs no separate save/restore.
    let mut footer_return_up: u16 = 0;
    // nb-shell prompt — owned by the render thread when a key
    // channel is wired in. Tracks line buffer, history, window
    // rows, and help overlay. `None` falls back to the legacy
    // log-only behaviour.
    let mut prompt: Option<PromptState> = key_rx.as_ref().map(|_| PromptState::new());
    let mut prompt_dirty = prompt.is_some();
    // Tracks the REPL visibility that the last redraw committed
    // to. Drives a redraw on the tick after a `~` / `Ctrl-~`
    // toggle so the prompt show/hide takes effect promptly.
    let mut repl_visibility_drawn = crate::repl_state::current();
    // Console-on-alternate-screen state. While the REPL is visible
    // (Bar or Window) the console renders on the alternate screen,
    // exactly like the Ctrl-T TUI: the terminal saves the primary
    // surface (logs + status block + scrollback) on enter and
    // restores it byte-exact on leave — including the cursor, so the
    // follow-the-log `footer_return_up` stays valid across the
    // excursion. Opening / closing the console can't scroll the
    // primary or leave a residual gap.
    let mut console_alt = false;
    // Per-phase gutter display state (rolling sample rings + lifetime
    // decimating trend buffers). Keyed by phase name@labels; sink-local
    // so no actor/state plumbing is needed. One p50 sample per footer
    // redraw tick.
    let mut gutter_state = GutterState::default();
    // Force a fresh REPL paint on the alt-screen (set on enter and
    // on a Bar<->Window change).
    let mut repl_alt_dirty = false;
    // Transcript line count last painted on the alt-screen console.
    // A change (from a dispatched command, a completion row, or any
    // other source) triggers a console repaint.
    let mut transcript_len_drawn: usize = 0;
    // Durable scrollback stream — the no-drop log-delivery path,
    // claimed by `LogOnlySink::start` before the seed read and moved
    // in here. The guard restores the receiver to the shared cell on
    // teardown so the next terminal-mode sink brought up after a
    // Ctrl-T swap resumes the SAME stream and flushes every line that
    // buffered while the TUI owned the alternate screen. `last_seen`
    // filters lines already emitted on stderr before the display
    // handoff (fresh start) or by a prior sink (swap re-entry) — see
    // `seed_last_seen`.
    while !stop.load(Ordering::Acquire) {
        // Drain any metrics frames that arrived since the last
        // tick — only when a frame channel was actually wired in.
        // For pure log-only mode no reporter is registered, so
        // `frame_rx == None` and there's nothing to drain. Phase
        // 2b's FakeTuiSink will use these to drive a periodic
        // status line; the LogOnlySink discards them.
        if let Some(rx) = &frame_rx {
            loop {
                match rx.try_recv() {
                    Ok(_frame) => {}
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                }
            }
        }

        // Drain key events: prompt mutations, window-resize
        // chords, help toggle, interrupt. Every event marks
        // the prompt dirty so the redraw at the bottom of the
        // tick picks up the change.
        let mut submitted_commands: Vec<String> = Vec::new();
        let mut completion_lists: Vec<Vec<String>> = Vec::new();
        if let (Some(rx), Some(p)) = (key_rx.as_ref(), prompt.as_mut()) {
            loop {
                match rx.try_recv() {
                    Ok(WatcherSignal::Key(ke)) => {
                        // Bare `p` with the console hidden toggles the
                        // readout pause (copy support): the drain/redraw
                        // below freezes so a text selection isn't
                        // repainted out from under the operator. Only
                        // intercepted while the REPL is Hidden — with a
                        // visible prompt, `p` is a letter being typed.
                        if matches!(
                            ke.code,
                            crossterm::event::KeyCode::Char('p')
                                | crossterm::event::KeyCode::Char('P')
                        ) && ke.modifiers.is_empty()
                            && matches!(
                                crate::repl_state::current(),
                                crate::repl_state::ReplVisibility::Hidden
                            )
                        {
                            let paused = nmbrs_runtime::observer::toggle_readout_pause();
                            // Raw mode needs explicit `\r\n`. The notice
                            // itself is the one write allowed while
                            // paused — it tells the operator how to get
                            // their readout back.
                            let _ = write!(
                                stderr,
                                "\r\n{}\r\n",
                                if paused {
                                    "⏸ readout paused for copy — press 'p' to resume"
                                } else {
                                    "▶ readout resumed"
                                }
                            );
                            let _ = stderr.flush();
                            continue;
                        }
                        match p.handle_key(ke) {
                            PromptAction::Continue => {}
                            PromptAction::Submit(line) => submitted_commands.push(line),
                            PromptAction::ShowCompletions(list) => completion_lists.push(list),
                            PromptAction::GrowWindow => {
                                p.grow_window();
                            }
                            PromptAction::ShrinkWindow => {
                                p.shrink_window();
                            }
                            PromptAction::ToggleHelp => {
                                p.toggle_help();
                            }
                            PromptAction::Interrupt => {
                                // Match supervisor's Ctrl-C path: re-
                                // raise SIGINT so the runtime's
                                // graceful-shutdown handler picks it
                                // up. (The supervisor itself also
                                // forwards Ctrl-C separately; we get
                                // here only when a Ctrl-C lands in
                                // the key stream rather than as
                                // WatcherSignal::Interrupt.)
                                unsafe {
                                    libc::raise(libc::SIGINT);
                                }
                            }
                        }
                        prompt_dirty = true;
                    }
                    Ok(WatcherSignal::GrowPrompt) => {
                        p.grow_window();
                        prompt_dirty = true;
                    }
                    Ok(WatcherSignal::ShrinkPrompt) => {
                        p.shrink_window();
                        prompt_dirty = true;
                    }
                    Ok(WatcherSignal::ToggleHelp) => {
                        p.toggle_help();
                        prompt_dirty = true;
                    }
                    Ok(_) => { /* ignored — supervisor handles other signals */ }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        // Supervisor torn down — drop the prompt
                        // and continue rendering log-only.
                        prompt = None;
                        prompt_dirty = false;
                        break;
                    }
                }
            }
        }

        // Readout pause (`p`, copy support): freeze every terminal
        // write — log drain, status fold, footer redraw — while the
        // operator selects text. Mirrors the console-on-alt-screen
        // freeze below: `last_seen` stays put so lines buffered in
        // the durable stream flush in order on resume. The key drain
        // above stays live so the resume keystroke is seen.
        if nmbrs_runtime::observer::readouts_paused() {
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        // Load the latest snapshot for the STATUS footer only
        // (latest-wins ArcSwap — only the newest status matters). Log
        // scrollback is DATA (every line, in order, exactly once) and
        // comes from the durable stream drained further down, NOT from
        // this snapshot's bounded ring — the ring can evict an
        // un-emitted line, which was the drop bug.
        let snap = state.load();
        // SRD-100 P2 — fold the live `active_phases` snapshot into the
        // status block at the consumer, rather than reading a single
        // pre-rendered scalar that N producer threads stomped. Single-phase
        // output is byte-identical (same builder + bodies); multi-phase
        // stacks per-phase renders (P3 adds the cap / multi-running counter).
        let folded = crate::status_fold::render_active_status_with_gutters(&snap);
        let next_status: Option<String> = folded.as_ref().map(|(t, _)| t.clone());
        let status_gutters: Vec<crate::status_fold::RowGutter> =
            folded.map(|(_, g)| g).unwrap_or_default();
        let status_changed = next_status != status_published;

        // Console (REPL) output is CONTAINED in the frame: it
        // goes into the transcript ring, never to stderr
        // scrollback, so a command's echo/response/completions
        // can't scroll the terminal state. The frame (Bar grown
        // or Window) renders the transcript tail in its bounded
        // region with internal scrollback. `dispatched_lines`
        // still flags a redraw so the frame picks the new lines
        // up this tick.
        let dispatched_lines = !submitted_commands.is_empty() || !completion_lists.is_empty();
        for list in &completion_lists {
            // One space-separated suggestion row into the frame;
            // column grouping is a future polish.
            crate::repl_state::push_transcript_line(&list.join("  "));
        }
        // Dispatch any submitted commands through the inspector
        // handler. Command echo + each response line land in the
        // transcript ring; the frame renders them. Nothing is
        // written to the shared stderr stream here.
        for line in &submitted_commands {
            let response = crate::inspector_server::dispatch(line, &state, runtime.as_ref());
            crate::repl_state::push_transcript(line, &response);
        }

        // === Console on the alternate screen ===
        // When the REPL is visible (Bar or Window) the console
        // renders on the alternate screen, exactly like the Ctrl-T
        // TUI: entering saves the primary surface (logs + managed
        // region + scrollback); leaving restores it byte-exact.
        // This is the "backing buffer" — opening / closing the
        // console can't scroll the primary or leave a residual gap,
        // and there's no incremental re-stream on close. While the
        // console is up the log drain is frozen (logs accumulate in
        // the ring) and flushed on leave.
        let repl_now = crate::repl_state::current();
        let console_visible = !matches!(repl_now, crate::repl_state::ReplVisibility::Hidden);

        if console_visible && !console_alt {
            // Enter: switch to the alternate screen (the terminal saves
            // the primary surface AND cursor — so `footer_return_up`
            // stays valid for the post-close redraw).
            let _ = write!(stderr, "\x1b[?1049h\x1b[2J\x1b[H");
            let _ = stderr.flush();
            console_alt = true;
            repl_alt_dirty = true;
        } else if !console_visible && console_alt {
            // Leave: restore the primary surface (the terminal brings
            // back logs + status block + scrollback + cursor byte-
            // exact). Sync `repl_visibility_drawn` to the now-current
            // Hidden state so the primary path does NOT treat this as a
            // visibility change — when nothing else changed the
            // restored surface is left untouched (byte-exact, no
            // re-stream). Real catch-up (logs buffered during the
            // console, phase progress, the session timer) flows through
            // the normal dirty signals below: `need_log_emit`
            // (last_seen was frozen) and `status_changed`.
            let _ = write!(stderr, "\x1b[?1049l");
            let _ = stderr.flush();
            repl_visibility_drawn = repl_now;
            console_alt = false;
        }

        if console_alt {
            // Render the console full-screen on the alternate
            // surface. Redraw only on change (a key edit, a
            // dispatched command, a Bar<->Window switch, or first
            // entry). The log drain below is skipped — `last_seen`
            // stays put so buffered logs flush after we leave.
            let repl_switch = repl_now != repl_visibility_drawn;
            repl_visibility_drawn = repl_now;
            let tlen = crate::repl_state::transcript_len();
            let transcript_changed = tlen != transcript_len_drawn;
            if repl_alt_dirty
                || prompt_dirty
                || dispatched_lines
                || repl_switch
                || transcript_changed
            {
                let (cols, rows) = terminal_size_via_ioctl().unwrap_or((200, 50));
                redraw_console_altscreen(&mut stderr, prompt.as_ref(), cols, rows);
                repl_alt_dirty = false;
                prompt_dirty = false;
                transcript_len_drawn = tlen;
            }
            status_published = next_status;
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }

        // === Drain the durable scrollback stream FULLY ===
        // Reached only when the console is Hidden (a visible console
        // `continue`d above with the drain frozen, so lines buffered
        // in the unbounded channel). Pull EVERY line the actor has
        // queued since the last tick — no cap, so a renderer that
        // lagged the actor by more than the old ring capacity can
        // never lose a line. `last_seen` skips lines already emitted
        // elsewhere: on a fresh start it was seeded from the live
        // `log_seq_total` (the observer's pre-handoff stderr writes),
        // and on a swap re-entry from the prior sink's final cursor
        // (its scrollback). Both are `seq <= last_seen` and dropped
        // from re-emission here; everything newer is emitted below.
        let mut new_logs: Vec<(
            crate::state::LogEntry,
            String,
            Option<nmbrs_runtime::wrappers::gutter::GutterSpec>,
        )> = Vec::new();
        while let Some(line) = scrollback.try_next() {
            if line.seq > last_seen {
                last_seen = line.seq;
                new_logs.push((line.entry, line.margin_body, line.detail_gutter));
            }
            // else: already on the surface (pre-handoff stderr / prior
            // sink) — skip re-emitting, matching the old seq-cursor.
        }
        let need_log_emit = !new_logs.is_empty();

        // Region anchored at the bottom of the terminal:
        // [logs ...] [status] [separator] [prompt input].
        // The status + prompt rows are absolutely-positioned
        // every redraw so the relative-cursor accounting (which
        // is fragile across scrolls) goes away. Log lines flow
        // upward naturally — they're written at the current
        // cursor position, which after a clear sits at the top
        // of the bottom region.
        //
        // We only touch the screen when something actually
        // changed this tick (logs, status, dispatched command,
        // prompt key event) OR when this is the first render of
        // the prompt (initial bring-up). A steady-state tick
        // with no changes leaves the cursor and screen alone —
        // critical fix to keep the prompt from drifting down
        // the screen as `\r\n` separators stack up.
        // REPL visibility transitions drive a redraw too —
        // toggling `~` flips whether the prompt row exists,
        // shifting the status block's absolute position; the
        // existing dirty signals can't catch it because they're
        // all derived from log/prompt content, not visibility.
        // Reached only when the console is Hidden — a visible
        // console renders on the alternate screen above and
        // `continue`s — so `repl_now` is `Hidden` here and the
        // prompt / window branches below collapse to the plain
        // logs + status layout. On the tick right after leaving the
        // console `repl_visibility_drawn` was already synced to
        // Hidden, so `repl_changed` is false and an unchanged
        // restored surface is left byte-exact.
        let repl_changed = repl_now != repl_visibility_drawn;
        repl_visibility_drawn = repl_now;
        let must_redraw =
            need_log_emit || status_changed || dispatched_lines || prompt_dirty || repl_changed;

        if must_redraw {
            // Compute the new footer content + geometry, then run the
            // follow-the-log pass below: return to the footer top,
            // clear, emit new logs (they scroll), redraw the footer
            // just beneath them. A steady-state tick with no changes
            // never reaches here, so nothing drifts down the screen.
            let (cols, rows) = terminal_size_via_ioctl().unwrap_or((200, 50));

            // Re-format the margin at draw time so the session
            // timer + phase counter tick along with the running
            // phase, matching the log lines above.
            let status_margin = format_margin_prefix(&snap, nmbrs_runtime::observer::use_color());
            let margin_visible_width = visible_width(&status_margin) as u16;
            // Row-2 margin for the running-phase status block:
            // progress bar + ETA + spinner replacing the `│`
            // divider, padded to the row-1 margin width so the
            // divider columns align. Empty when no phase runs.
            // (contextual gutters now arrive per-row from status_fold —
            // see `status_gutters`; the old single row-2 bar margin is
            // retired.)
            let status_cols = (cols as usize).saturating_sub(margin_visible_width as usize);

            // REPL visibility (sampled once this tick as `repl_now`):
            // - Hidden: no prompt; status fills the bottom region.
            // - Bar: single-row prompt; status block above it.
            // - Window: full-screen console — the prompt expands,
            //   the live status is swapped for a one-line REPL
            //   header, and the phase-history region is suppressed
            //   (the console owns the surface; closing it repaints
            //   the region via the `repl_changed` redraw).
            let window_mode = matches!(repl_now, crate::repl_state::ReplVisibility::Window);
            let repl_visible = !matches!(repl_now, crate::repl_state::ReplVisibility::Hidden);

            let new_status_text: Option<String> = if window_mode {
                let color = nmbrs_runtime::observer::use_color();
                let dim = if color { "\x1b[2m" } else { "" };
                let reset = if color { "\x1b[0m" } else { "" };
                Some(format!("{dim}REPL · ` close · ~ hide{reset}"))
            } else {
                next_status
                    .as_ref()
                    .map(|s| clamp_multiline(s, status_cols.saturating_sub(1)))
            };
            // Window mode expands the prompt to fill the screen
            // minus the header row. `set_window_rows` is the single
            // geometry chokepoint; re-applied each tick so the
            // REPL-mode override wins over Alt-Up / Alt-Down.
            if window_mode && let Some(p) = prompt.as_mut() {
                let target = rows.saturating_sub(2).max(1);
                if p.window_rows() != target {
                    p.set_window_rows(target);
                }
            }
            let prompt_input = if repl_visible {
                prompt.as_ref().map(|p| {
                    let win_rows = p.window_rows() as usize;
                    // Any multi-row console frame (Window, or a Bar
                    // grown past one row) composes its `rendered`
                    // bytes by hand: the top rows are the contained
                    // transcript tail (oldest at top, newest just
                    // above the input) with internal scrollback; the
                    // bottom row is the standard prompt-input render.
                    // A single-row Bar falls through to the plain
                    // input render (its output lives in the ring,
                    // viewable by growing the bar or opening the
                    // window — it never scrolls the terminal).
                    let rendered = if win_rows > 1 {
                        let transcript_rows = win_rows.saturating_sub(1);
                        let tail = crate::repl_state::transcript_tail(transcript_rows);
                        let cols_usize = cols as usize;
                        let mut composed = String::with_capacity(cols_usize * win_rows);
                        // Top-pad with blanks when the transcript is
                        // shorter than the window so the input stays
                        // anchored at the bottom row rather than
                        // rising as history grows.
                        let blanks = transcript_rows.saturating_sub(tail.len());
                        for _ in 0..blanks {
                            composed.push_str("\r\n");
                        }
                        for line in tail.iter() {
                            let row = nmbrs_runtime::activity::truncate_to_width(line, cols_usize);
                            composed.push_str(&row);
                            composed.push_str("\r\n");
                        }
                        // Final row: the prompt's own single-row
                        // render of the input buffer (`❯ <buffer>`
                        // plus cursor) — the last row of its
                        // multi-row render is what we want.
                        let mut input_row = String::with_capacity(128);
                        p.render(&mut input_row, cols_usize, true);
                        let last = input_row.split('\n').next_back().unwrap_or("");
                        composed.push_str(last.strip_suffix('\r').unwrap_or(last));
                        composed
                    } else {
                        let mut f = String::with_capacity(128);
                        p.render(&mut f, cols as usize, true);
                        f
                    };
                    PromptInput {
                        rendered,
                        window_rows: p.window_rows(),
                        cursor_col: p.cursor_col() as u16,
                    }
                })
            } else {
                None
            };

            // FOLLOW-THE-LOG sticky footer.
            //
            // The status block floats at the bottom by being cleared
            // and reprinted just below the log stream every tick; the
            // log stream owns everything above it and scrolls
            // naturally. There is no absolute positioning and no
            // scroll-on-growth — a height change is absorbed by the
            // terminal's own scroll as the footer is reprinted, so a
            // blank is never stranded between the logs and the status.
            // (The absolute-positioned approach left the cursor on a
            // trailing blank line after the last log; the moment a
            // status appeared or changed height that blank got scrolled
            // up and baked in, accumulating one gap per status op —
            // and a log emit colliding with a height change could even
            // drop the log. See the `tui_display_harness` tests.)
            //
            // Invariant across ticks: after a committed redraw the
            // cursor sits `footer_return_up` rows BELOW the footer's
            // first row.

            // (A) Return to the footer top and clear it plus anything
            //     below. Skipped on the first paint: no footer is
            //     drawn yet and the cursor sits just after the last
            //     emitted log (or on a freshly restored surface), so a
            //     clear-to-end-of-screen from there is correct.
            if !first_paint {
                if footer_return_up > 0 {
                    let _ = write!(stderr, "\x1b[{footer_return_up}A");
                }
                let _ = write!(stderr, "\r\x1b[J");
            }

            // (B) Emit any new log lines. Each ends with `\r\n`, so it
            //     scrolls the surface up and leaves the cursor on the
            //     next fresh line — exactly where the footer begins,
            //     so the footer's first row reclaims that line with no
            //     gap.
            if need_log_emit {
                // Every line drained from the durable stream this tick
                // is emitted — no cap, no drop. The stream carries them
                // in order, exactly once; a slow renderer simply drains
                // a longer burst on this tick.
                // Each line carries its actor-stamped margin body —
                // exact for the state that preceded it — rather than
                // one render-time margin shared by the whole batch
                // (which raced the very transitions the lines report:
                // a completion line could sit beside a stale [n/N]).
                let color = nmbrs_runtime::observer::use_color();
                for (entry, margin_body, detail_gutter) in &new_logs {
                    let margin = format_margin_from_body(margin_body, color);
                    let margin = &margin;
                    let entry_level = severity_to_level(entry.severity);
                    if entry_level < min_level {
                        continue;
                    }
                    // Phase-START lifecycle lines are suppressed from
                    // scrollback: the live status block already shows
                    // the running phase, and the rich per-phase ✓
                    // summary (a Diagnostic log, not tagged here) is
                    // the completion marker — a phase-start line on
                    // top would be redundant noise. All entries are
                    // kept in session.log unconditionally.
                    if entry.tag.attached.is_some()
                        && entry.tag.category == crate::state::EventCategory::General
                    {
                        continue;
                    }
                    // Match the observer's cosmetic blank line before
                    // the Ctrl-C / force-exit banners.
                    if entry
                        .message
                        .starts_with("session: graceful shutdown requested")
                        || entry.message.starts_with("session: force-exit")
                    {
                        let _ = write!(stderr, "\r\n");
                    }
                    // Colorize by severity. `colorize_log_line` is a
                    // no-op on non-tty / NO_COLOR so log captures stay
                    // clean. Split on embedded `\n` so multi-line
                    // messages get `\r\n` (raw mode needs the explicit
                    // `\r`); each row gets the margin and a trailing
                    // newline so it scrolls the surface.
                    let painted =
                        nmbrs_runtime::observer::colorize_log_line(entry_level, &entry.message);
                    // SRD-92 margin assignment for scrollback: the
                    // HEADER row of every entry gets the actor-stamped
                    // triad; detail rows get the blank divider margin
                    // — a multi-row entry never repeats the triad —
                    // except cells explicitly attached (gutter/final on
                    // the ✓ block's standard detail row). A PhaseDetail
                    // entry (e.g. the relevancy summary line) is a
                    // detail row of the block above it, so its single
                    // row renders under the blank margin too.
                    let w = visible_width(margin);
                    let cell_w = w.saturating_sub(2);
                    let blank_margin = if w == 0 {
                        String::new()
                    } else {
                        format!("\x1b[2m{:<cell_w$}│\x1b[0m ", "")
                    };
                    let final_cell: Option<String> =
                        detail_gutter.as_ref().map(|spec| match spec {
                            nmbrs_runtime::wrappers::gutter::GutterSpec::Labeled {
                                name,
                                value,
                            } => labeled_gutter(name, value, cell_w, color),
                            nmbrs_runtime::wrappers::gutter::GutterSpec::Text(t) => {
                                text_gutter(t, cell_w, color)
                            }
                            nmbrs_runtime::wrappers::gutter::GutterSpec::Bar(f) => {
                                bar_gutter(*f, cell_w, color)
                            }
                            nmbrs_runtime::wrappers::gutter::GutterSpec::Spark(v) => {
                                text_gutter(&format!("{v:.2}"), cell_w, color)
                            }
                        });
                    // `completed_phases=headers` (SRD-92 R5): retain
                    // only the header row of completion blocks in
                    // scrollback (details/leaves stay in session.log).
                    let headers_only = matches!(
                        nmbrs_runtime::observer::completed_phase_display(),
                        nmbrs_runtime::observer::CompletedPhaseDisplay::Headers
                    );
                    let is_detail_entry = entry.tag.attached.is_some()
                        && entry.tag.category == crate::state::EventCategory::Evaluation;
                    if headers_only && is_detail_entry {
                        continue;
                    }
                    let rows: Vec<&str> = painted.split('\n').collect();
                    let roles = crate::status_fold::classify_block(&rows);
                    for (i, row) in rows.iter().enumerate() {
                        let role = roles[i];
                        if headers_only
                            && entry.tag.category == crate::state::EventCategory::Outcome
                            && role != crate::status_fold::RowRole::Header
                        {
                            continue;
                        }
                        let row_margin: &str = if is_detail_entry {
                            // Detail entries (op leaves, relevancy
                            // summaries) never wear a triad; an
                            // attached cell (the leaf's execution
                            // duration) takes the margin, else blank.
                            final_cell.as_deref().unwrap_or(&blank_margin)
                        } else {
                            match role {
                                crate::status_fold::RowRole::Header => margin,
                                crate::status_fold::RowRole::Standard => {
                                    final_cell.as_deref().unwrap_or(&blank_margin)
                                }
                                _ => &blank_margin,
                            }
                        };
                        let _ = write!(stderr, "{row_margin}{row}\r\n");
                    }
                }
                // `last_seen` already advanced during the stream drain
                // above (one bump per delivered line).
            }

            // (C) Draw the footer (status [+ inline prompt]) at the
            //     cursor the log emit left us on, top-aligned and with
            //     NO trailing newline. It returns how far below the
            //     footer top the cursor was left, for the next tick's
            //     climb-back in (A).
            let (_drawn_rows, cursor_below_top) = draw_footer_at_cursor(
                &mut stderr,
                new_status_text.as_deref(),
                prompt_input.as_ref(),
                cols,
                &status_margin,
                margin_visible_width,
                &status_gutters,
                &mut gutter_state,
            );
            footer_return_up = cursor_below_top;
            let _ = _drawn_rows;
            first_paint = false;
            let _ = stderr.flush();
            prompt_dirty = false;
        }
        status_published = next_status;

        // Publish the combined render height so the signal
        // handler in `crate::app` can walk past everything we
        std::thread::sleep(POLL_INTERVAL);
    }

    // Sink shutting down. If the console was up we're on the
    // alternate screen — leave it first so the terminal isn't
    // stranded there. The terminal restores the primary surface AND
    // cursor, so `footer_return_up` still describes where the footer
    // sits for the clear below.
    if console_alt {
        let _ = write!(stderr, "\x1b[?1049l");
        let _ = stderr.flush();
    }
    // Wipe the footer we own so the post-run terminal state isn't
    // littered with our final tick's text. Follow-the-log left the
    // cursor `footer_return_up` rows below the footer's first row (the
    // `?1049` cursor save/restore preserved this across any console
    // excursion above), so climb back to the footer top and clear to
    // end of screen. The signal-handler cleanup path in `crate::app`
    // instead uses `\x1b[999;1H` to park at the viewport bottom, so it
    // needs no row count of ours.
    if footer_return_up > 0 {
        let _ = write!(stderr, "\x1b[{footer_return_up}A");
    }
    let _ = write!(stderr, "\r\x1b[J");
    let _ = stderr.flush();

    // Hand our final log cursor to the supervisor. The next
    // terminal-mode sink it brings up after a TUI swap seeds
    // `last_seen` from here and re-emits the lines that scrolled
    // while the alternate screen was up, so the restored scrollback
    // catches up to the full phase history. Harmless at run-end —
    // no successor sink reads it.
    if let Some(a) = &resume_from {
        a.store(last_seen, Ordering::Release);
    }
}

/// Build the agreed left-margin gutter: a fixed-width
/// `session-time · [n/N] · phase-time │` triad — the margin BODY comes
/// from [`crate::state::RunState::margin_body_stamp`] (single source,
/// shared with the actor's per-line stamps) and this wraps it in the
/// color + divider dressing.
fn format_margin_prefix(snap: &std::sync::Arc<crate::state::RunState>, color: bool) -> String {
    format_margin_from_body(&snap.margin_body_stamp(), color)
}

/// Colorize a margin body (stamped or live) and append the `│ ` divider.
fn format_margin_from_body(body: &str, color: bool) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    format!("{dim}{body} \u{2502}{reset} ")
}

/// Direct `TIOCGWINSZ` ioctl on `fd 2` (stderr). Returns
/// `(cols, rows)` when stderr is a terminal and the call
/// succeeds; `None` otherwise. Mirrors
/// [`nmbrs_runtime::activity::terminal_cols`] but also captures
/// the row count we need for absolute positioning of the
/// bottom region.
#[cfg(unix)]
fn terminal_size_via_ioctl() -> Option<(u16, u16)> {
    #[repr(C)]
    struct WinSize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }
    let mut ws = WinSize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut ws as *mut _) };
    if rc < 0 || ws.ws_col == 0 || ws.ws_row == 0 {
        return None;
    }
    Some((ws.ws_col, ws.ws_row))
}

/// Windows equivalent (no ioctl there): crossterm's
/// `terminal::size`, which asks the console host for the visible
/// window extent. Only used off-Unix, where stderr-vs-stdout fd
/// distinction doesn't apply to the console query.
#[cfg(not(unix))]
fn terminal_size_via_ioctl() -> Option<(u16, u16)> {
    crossterm::terminal::size()
        .ok()
        .filter(|&(c, r)| c != 0 && r != 0)
}

/// Prompt-rendering inputs handed to [`redraw_bottom_region`].
/// Decoupled from `PromptState` so the renderer is testable
/// with simple data.
pub(crate) struct PromptInput {
    /// The prompt's render() output — the bytes to write for
    /// the prompt window's rows, separated by `\r\n` (carriage
    /// return + newline) between rows.
    pub rendered: String,
    /// How many rows the prompt window occupies.
    pub window_rows: u16,
    /// Cursor column within the input row (0-based; the
    /// renderer adds 1 to translate to 1-based escape coords).
    pub cursor_col: u16,
}

/// Draw the bottom "footer" — the live status block, plus the inline
/// console prompt when one is shown — starting at the CURRENT cursor
/// row (wherever the log emit left it), top-aligned, joining rows with
/// `\r\n` and leaving NO trailing newline. Returns
/// `(rows_drawn, cursor_rows_below_footer_top)`; the caller climbs back
/// up by the latter on the next tick to reach the footer top.
///
/// This is the follow-the-log counterpart to the absolute-positioned
/// [`redraw_bottom_region`]: drawing relative to where the log stream
/// left the cursor keeps the footer contiguous with the logs above it,
/// and a height change is absorbed by the terminal's own scroll as the
/// footer is reprinted — never stranding a blank row between the logs
/// and the status. Each row is `\r`-homed and erase-to-EOL'd so residue
/// from a wider prior render is scrubbed.
///
/// Row 0 of the status carries `status_margin` (timer + phase counter +
/// `│ ` divider); rows 1+ carry `row2_margin` (the running-phase
/// progress/ETA/spinner variant, padded so the divider column aligns)
/// when present, else the standard margin.
pub(crate) fn draw_footer_at_cursor<W: Write>(
    out: &mut W,
    status_text: Option<&str>,
    prompt: Option<&PromptInput>,
    term_cols: u16,
    status_margin: &str,
    margin_width: u16,
    gutters: &[crate::status_fold::RowGutter],
    gutter_state: &mut GutterState,
) -> (u16, u16) {
    let status_lines: Vec<&str> = status_text
        .map(|s| s.split('\n').collect())
        .unwrap_or_default();
    let status_rows_n = status_lines.len() as u16;
    let prompt_rows = prompt.map(|p| p.window_rows).unwrap_or(0);
    let total = status_rows_n + prompt_rows;
    if total == 0 {
        return (0, 0);
    }

    // The return contract (`cursor_below_top`) counts LOGICAL rows, and
    // the next tick's climb-back (`\x1b[{n}A`) trusts it. A status row
    // wider than the terminal WRAPS into extra physical rows the count
    // doesn't see — the climb-back then lands mid-footer, the `\x1b[J`
    // clear starts too low, and footer rows flicker in and out as log
    // emits reclaim the difference. Truncate every status row to the
    // width budget so one logical row is always exactly one physical
    // row. (Prompt rows already render width-aware via
    // `PromptState::render(cols)`.)
    let avail = (term_cols as usize).saturating_sub(margin_width as usize);

    let use_color_now = nmbrs_runtime::observer::use_color();
    // Standard blank-aligned continuation gutter (divider only) for
    // rows without contextual content. A zero-width margin means the
    // sink runs marginless (piped/plain) — no divider column exists,
    // so continuation rows get nothing, gutters included.
    let blank_w = (margin_width as usize).saturating_sub(2);
    let blank_margin_owned = if margin_width == 0 {
        String::new()
    } else {
        format!("\x1b[2m{:<blank_w$}│\x1b[0m ", "")
    };
    let blank_margin: &str = &blank_margin_owned;

    let mut first = true;
    for (i, row) in status_lines.iter().enumerate() {
        if !first {
            let _ = write!(out, "\r\n");
        }
        first = false;
        // SRD-92 margin assignment: every node HEADER row carries its
        // own timing triad (not just footer row 0 — each concurrent
        // phase's header wears its own [n/N]/clock); each DETAIL row
        // renders its own gutter cell (completion bar, latency trend,
        // metric macro, workload cell) or the blank divider. Single
        // placement: each value appears in exactly one cell. Row 0
        // falls back to the workload-level triad when the fold didn't
        // classify it (no active phase blocks).
        let ctx_margin_owned;
        let margin = if margin_width == 0 {
            status_margin
        } else {
            match gutters.get(i) {
                Some(crate::status_fold::RowGutter::Header(body)) => {
                    ctx_margin_owned = format_margin_from_body(body, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Metric { key, name, value }) => {
                    ctx_margin_owned = metric_gutter(
                        gutter_state.rings.entry(key.clone()).or_default(),
                        name,
                        *value,
                        blank_w,
                        use_color_now,
                    );
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Bar(f)) => {
                    ctx_margin_owned = bar_gutter(*f, blank_w, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Latency {
                    key,
                    p50,
                    p99,
                    count,
                }) => {
                    let cell = gutter_state.latency.entry(key.clone()).or_default();
                    if *count != cell.last_count
                        || cell.last_w != blank_w
                        || cell.last_render.is_empty()
                    {
                        cell.last_render =
                            latency_gutter(&mut cell.ring, *p50, *p99, blank_w, use_color_now);
                        cell.last_count = *count;
                        cell.last_w = blank_w;
                    }
                    ctx_margin_owned = cell.last_render.clone();
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::LatencyHist { key, p50, count }) => {
                    let cell = gutter_state.hists.entry(key.clone()).or_insert_with(|| {
                        HistCellState {
                            trend: crate::widgets::DecimatingTrend::new(
                                // Matches the helper's spark region:
                                // cell width minus the min∕max label
                                // (≤11 chars) and its separator space,
                                // so no lifetime cell is ever clipped.
                                blank_w.saturating_sub(12).max(4),
                            ),
                            last_count: 0,
                            last_w: 0,
                            last_render: String::new(),
                        }
                    });
                    // Sample-gated: one trend sample per NEW op, not per
                    // redraw tick — a slow poller keeps a stable cell
                    // between ops.
                    if *count != cell.last_count
                        || cell.last_w != blank_w
                        || cell.last_render.is_empty()
                    {
                        cell.last_render =
                            latency_hist_gutter(&mut cell.trend, *p50, blank_w, use_color_now);
                        cell.last_count = *count;
                        cell.last_w = blank_w;
                    }
                    ctx_margin_owned = cell.last_render.clone();
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Text(t)) => {
                    ctx_margin_owned = text_gutter(t, blank_w, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Sysmon { items }) => {
                    ctx_margin_owned = sysmon_gutter(items, blank_w, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Labeled { name, value }) => {
                    ctx_margin_owned = labeled_gutter(name, value, blank_w, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::BarText { frac, text }) => {
                    ctx_margin_owned = bar_text_gutter(*frac, text, blank_w, use_color_now);
                    &ctx_margin_owned
                }
                Some(crate::status_fold::RowGutter::Spark { key, value }) => {
                    ctx_margin_owned = spark_gutter(
                        gutter_state.rings.entry(key.clone()).or_default(),
                        *value,
                        blank_w,
                        use_color_now,
                    );
                    &ctx_margin_owned
                }
                _ if i == 0 => status_margin,
                _ => blank_margin,
            }
        };
        let fitted = nmbrs_runtime::activity::truncate_to_width(row, avail);
        // A truncation can strand an open SGR (the reset was past the
        // cut) — close it so the tint can't bleed into the margin of
        // the next row.
        let reset_tail = if fitted.len() != row.len() {
            "\x1b[0m"
        } else {
            ""
        };
        let _ = write!(out, "\r\x1b[K{margin}{fitted}{reset_tail}");
    }
    // Prompt rows — present only for the inline console bar (the full
    // console renders on the alternate screen, not here). The prompt's
    // `>` sits at the same column as the log content (right of the
    // margin) so the operator reads the input row as the bottom of the
    // same column.
    if let Some(p) = prompt {
        for row in p.rendered.split('\n') {
            if !first {
                let _ = write!(out, "\r\n");
            }
            first = false;
            let row = row.strip_suffix('\r').unwrap_or(row);
            let row = row.strip_prefix('\r').unwrap_or(row);
            let _ = write!(out, "\r\x1b[K{status_margin}{row}");
        }
        // Park the cursor at the prompt's input column on the last
        // (current) row for typing — `margin_width` shifts it past the
        // margin into the content area. `\x1b[NG` is column-absolute.
        let target_col = margin_width + p.cursor_col + 1;
        let _ = write!(out, "\x1b[{target_col}G");
    }
    (total, total.saturating_sub(1))
}

/// Render the console (REPL) full-screen on the alternate screen.
///
/// Layout: a one-line header at the top, the transcript tail
/// filling the middle (oldest at top, newest just above the
/// input), and the prompt input on the bottom row. Each row is
/// absolutely positioned and erase-to-EOL'd so residue from a
/// prior, wider paint is scrubbed. The cursor parks at the input
/// column. The caller owns the alternate-screen enter/leave (the
/// terminal saves / restores the primary surface), so this only
/// composes the console surface itself.
fn redraw_console_altscreen<W: Write>(
    out: &mut W,
    prompt: Option<&PromptState>,
    cols: u16,
    rows: u16,
) {
    let color = nmbrs_runtime::observer::use_color();
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let cols_usize = cols as usize;

    // Header (row 1).
    let _ = write!(
        out,
        "\x1b[1;1H\x1b[K{dim}REPL · ~ or ` to close · ↑↓ history{reset}"
    );

    // Transcript tail fills rows 2 ..= rows-1 — oldest at the top,
    // top-padded with blanks so the newest line sits just above the
    // input even when the history is short.
    let body_rows = (rows as usize).saturating_sub(2);
    let tail = crate::repl_state::transcript_tail(body_rows);
    let blanks = body_rows.saturating_sub(tail.len());
    let mut row: u16 = 2;
    for _ in 0..blanks {
        let _ = write!(out, "\x1b[{row};1H\x1b[K");
        row += 1;
    }
    for line in &tail {
        let painted = nmbrs_runtime::activity::truncate_to_width(line, cols_usize);
        let _ = write!(out, "\x1b[{row};1H\x1b[K{painted}");
        row += 1;
    }

    // Input row (bottom): the prompt's own single-line input
    // render (last row of its multi-row render).
    let mut cursor_col: u16 = 0;
    if let Some(p) = prompt {
        let mut input = String::with_capacity(128);
        p.render(&mut input, cols_usize, true);
        let last = input.split('\n').next_back().unwrap_or("");
        let last = last.strip_suffix('\r').unwrap_or(last);
        let _ = write!(out, "\x1b[{rows};1H\x1b[K{last}");
        cursor_col = p.cursor_col() as u16;
    } else {
        let _ = write!(out, "\x1b[{rows};1H\x1b[K");
    }
    // Park the cursor at the input column (1-based).
    let _ = write!(out, "\x1b[{rows};{}H", cursor_col + 1);
    let _ = out.flush();
}

/// Build the continuation (row-2+) margin for the footer status block: a blank
/// gutter the same visible width as the row-1 [`format_margin_prefix`], with the
/// `│` divider in the same column. A phase's detail rows — its metric line and
/// its op-leaf lines — sit under this so they line up beneath the count/time
/// triad, matching the interactive tree (only an entry's leading row shows the
/// triad; its detail rows are blank under the divider). Empty when no phase is

/// Completion-bar gutter cell (the house braille bar + divider).
fn bar_gutter(frac: f64, w: usize, color: bool) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let bg = if color { "\x1b[48;2;50;50;50m" } else { "" };
    let bright = if color { "\x1b[97m" } else { "" };
    let bar = nmbrs_runtime::readouts::format::braille_bar(frac * 100.0, w);
    format!("{bg}{bright}{bar}{reset}{dim}│{reset} ")
}

/// Latency-trend gutter cell for open-ended pollers: a p50 sparkline
/// (one sample per redraw tick) with the current p50∕p99 to its
/// right, sized to exactly the bar's footprint. Compact by design —
/// the trend shape carries the signal; the two numbers anchor it.
fn latency_gutter(
    ring: &mut std::collections::VecDeque<f64>,
    p50: u64,
    p99: u64,
    w: usize,
    color: bool,
) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let cyan = if color { "\x1b[36m" } else { "" };
    let fmt = |n: u64| {
        let ms = n as f64 / 1e6;
        if ms >= 100.0 {
            format!("{ms:.0}")
        } else if ms >= 10.0 {
            format!("{ms:.1}")
        } else {
            format!("{ms:.2}")
        }
    };
    let text = format!("{}∕{}ms", fmt(p50), fmt(p99));
    let spark_w = w.saturating_sub(text.chars().count() + 1).max(4);
    ring.push_back(p50 as f64 / 1e6);
    while ring.len() > spark_w {
        ring.pop_front();
    }
    let samples: Vec<f64> = ring.iter().copied().collect();
    let spark = crate::widgets::sparkline_str(&samples, spark_w);
    format!(
        "{cyan}{spark}{reset} {dim}{text:>tw$}│{reset} ",
        tw = w.saturating_sub(spark_w + 1)
    )
}

/// Key-metric gutter cell (SRD-92 R4): the metric macro's live view —
/// a sparkline TREND of the primary key metric in the KEY-METRIC
/// ACCENT (bright magenta, the palette's dedicated key-metric
/// color), labeled by the metric's short name. The current numeric
/// is single-placed in the row body's chips; the cell carries the
/// history the body can't.
fn metric_gutter(
    ring: &mut std::collections::VecDeque<f64>,
    name: &str,
    value: f64,
    w: usize,
    color: bool,
) -> String {
    let accent = if color { "\x1b[1;95m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let label = nmbrs_runtime::activity::truncate_to_width(name, w.saturating_sub(5).max(4));
    let label_w = label.chars().count();
    let spark_w = w.saturating_sub(label_w + 1).max(4);
    ring.push_back(value);
    while ring.len() > spark_w {
        ring.pop_front();
    }
    let samples: Vec<f64> = ring.iter().copied().collect();
    let spark = crate::widgets::sparkline_str(&samples, spark_w);
    format!(
        "{accent}{spark}{reset} {dim}{label:>lw$}│{reset} ",
        lw = w.saturating_sub(spark_w + 1)
    )
}

/// Composed cell (SRD-92 R3): the phase's completion bar beside a
/// workload-declared text readout — a custom cell on a metered phase
/// keeps the progress indicator. The bar takes a fixed sub-cell (8,
/// shrinking on narrow margins); the text gets the remainder,
/// truncated.
fn bar_text_gutter(frac: f64, text: &str, w: usize, color: bool) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let bg = if color { "\x1b[48;2;50;50;50m" } else { "" };
    let bright = if color { "\x1b[97m" } else { "" };
    let bar_w = if w >= 14 { 8 } else { (w / 3).max(3) };
    let text_w = w.saturating_sub(bar_w + 1);
    let bar = nmbrs_runtime::readouts::format::braille_bar(frac * 100.0, bar_w);
    let fitted = nmbrs_runtime::activity::truncate_to_width(text, text_w);
    let pad = text_w.saturating_sub(fitted.chars().filter(|c| !c.is_control()).count());
    format!("{bg}{bright}{bar}{reset} {dim}{fitted}{:pad$}│{reset} ", "")
}

/// Workload-declared layout-text gutter cell (`gutter: "<template>"`), and the
/// leaf duration cell.
///
/// RIGHT-aligned against the divider, which is the house convention for a
/// VALUE: the node-header row already right-aligns its elapsed time there, so
/// a left-aligned leaf duration put the same quantity in a different column
/// depending on the row type. Values now line up vertically whatever the row.
/// (The doc-comment always claimed right alignment; the code did not.)
/// Braille utilization meter: two cells, so 4 dot-columns × 4 dot-rows =
/// 16 dots of resolution (6.25% per dot). Fill is column-major left→right,
/// each column bottom-up, so the meter grows like a bar.
///
/// Braille dot numbering puts dots 1-3 + 7 in a cell's left column (top to
/// bottom) and 4-6 + 8 in the right; the bottom-up bit orders below are
/// exactly those columns reversed.
fn braille_meter(frac: f64) -> [char; 2] {
    const COL_BOTTOM_UP: [[u8; 4]; 2] = [
        [0x40, 0x04, 0x02, 0x01], // dots 7,3,2,1 — a cell's left column
        [0x80, 0x20, 0x10, 0x08], // dots 8,6,5,4 — a cell's right column
    ];
    let dots = (frac.clamp(0.0, 1.0) * 16.0).round() as usize;
    let mut cells = [0u8; 2];
    for col in 0..4 {
        let filled = dots.saturating_sub(col * 4).min(4);
        for bit in &COL_BOTTOM_UP[col % 2][..filled] {
            cells[col / 2] |= bit;
        }
    }
    [
        char::from_u32(0x2800 + cells[0] as u32).expect("braille block"),
        char::from_u32(0x2800 + cells[1] as u32).expect("braille block"),
    ]
}

/// Utilization color ramp: bright orange above 90%, dim orange above 80%,
/// yellow above 50%, bright white above 25%, terminal-normal below. Orange
/// has no basic-ANSI slot, so the two orange tiers are 256-color (208
/// bold-bright / 166 dim); the rest stay on basic codes.
fn util_color(frac: f64) -> &'static str {
    if frac > 0.90 {
        "\x1b[1;38;5;208m"
    } else if frac > 0.80 {
        "\x1b[38;5;166m"
    } else if frac > 0.50 {
        "\x1b[33m"
    } else if frac > 0.25 {
        "\x1b[97m"
    } else {
        ""
    }
}

/// The sysmon strip: per item a single-cell glyph + a two-cell braille
/// meter, colored by that item's utilization, space-separated and
/// right-aligned against the divider like every other gutter cell.
fn sysmon_gutter(items: &[(char, f64)], w: usize, color: bool) -> String {
    let reset = if color { "\x1b[0m" } else { "" };
    let mut body = String::new();
    let mut visible = 0usize;
    for (i, (glyph, frac)) in items.iter().enumerate() {
        if i > 0 {
            body.push(' ');
            visible += 1;
        }
        let c = if color { util_color(*frac) } else { "" };
        let [m0, m1] = braille_meter(*frac);
        if !c.is_empty() {
            body.push_str(c);
        }
        body.push(*glyph);
        body.push(m0);
        body.push(m1);
        if !c.is_empty() {
            body.push_str(reset);
        }
        visible += 3;
    }
    // Same shape as `text_gutter`: one column of right margin before the
    // divider; visible width counted OURSELVES because the body carries
    // per-item color escapes that a char count would miscount.
    let inner = w.saturating_sub(1);
    let pad = inner.saturating_sub(visible);
    format!("{:pad$}{body} │ ", "")
}

fn text_gutter(text: &str, w: usize, color: bool) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    // One column of right margin before the divider, matching the header cell
    // (`body │`). Without it a leaf's duration ended flush against the divider
    // while the header's phase time stopped a column short, so the same quantity
    // — how long the thing took — sat in two different columns depending on which
    // row you were reading:
    //     0.0s│     ✓ flush  [1/4]
    //    45.0s │    ✓ [concurrent_query]
    let inner = w.saturating_sub(1);
    let fitted = nmbrs_runtime::activity::truncate_to_width(text, inner);
    let pad = inner.saturating_sub(fitted.chars().filter(|c| !c.is_control()).count());
    format!("{dim}{:pad$}{fitted} │{reset} ", "")
}

/// Key-metric gutter cell: NAME left, VALUE right, filling the cell.
///
/// The pairing is the point. A key metric is the row's headline number, so the
/// value sits against the divider like every other value, and its label sits at
/// the far edge — one glance finds the number, another finds what it measures,
/// and both stay in the same columns across rows. The value is accented; the
/// label stays dim so it recedes.
fn labeled_gutter(name: &str, value: &str, w: usize, color: bool) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let accent = if color { "\x1b[1;92m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    // Bracketed and centred, because this cell shares a column with the phase
    // TIMINGS. Aligning the metric the same way they are aligned made it read as
    // one more timing field; the brackets span the cell so the row announces
    // itself as something else entirely before you read the number.
    let inner = w.saturating_sub(1); // shared right margin, as above
    let text_w = inner.saturating_sub(2); // the two bracket columns
    let plain = format!("{name}: {value}");
    if text_w < 3 {
        // No room to be decorative — keep the number, drop the frame.
        let val = nmbrs_runtime::activity::truncate_to_width(value, inner);
        let pad = inner.saturating_sub(val.chars().filter(|c| !c.is_control()).count());
        return format!("{dim}{:pad$}{reset}{accent}{val}{reset}{dim} │{reset} ", "");
    }
    let fitted = nmbrs_runtime::activity::truncate_to_width(&plain, text_w);
    let fitted_w = fitted.chars().filter(|c| !c.is_control()).count();
    let slack = text_w.saturating_sub(fitted_w);
    let left = slack / 2;
    let right = slack - left;
    // The label stays dim and the value is accented, as before — the frame is
    // structure, not emphasis.
    let (lbl, val) = match fitted.split_once(": ") {
        Some((l, v)) => (format!("{l}: "), v.to_string()),
        None => (String::new(), fitted.clone()),
    };
    format!(
        "{dim}[{:left$}{lbl}{reset}{accent}{val}{reset}{dim}{:right$}] │{reset} ",
        "", ""
    )
}

/// Workload-declared trend gutter cell (`gutter: {spark: ...}`): a
/// sparkline of published samples with the current value to its
/// right — same footprint as the latency cell, generic units.
fn spark_gutter(
    ring: &mut std::collections::VecDeque<f64>,
    value: f64,
    w: usize,
    color: bool,
) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let cyan = if color { "\x1b[36m" } else { "" };
    let text = if value.abs() >= 100.0 {
        format!("{value:.0}")
    } else if value.abs() >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    };
    let spark_w = w.saturating_sub(text.chars().count() + 1).max(4);
    ring.push_back(value);
    while ring.len() > spark_w {
        ring.pop_front();
    }
    let samples: Vec<f64> = ring.iter().copied().collect();
    let spark = crate::widgets::sparkline_str(&samples, spark_w);
    format!(
        "{cyan}{spark}{reset} {dim}{text:>tw$}│{reset} ",
        tw = w.saturating_sub(spark_w + 1)
    )
}

/// Per-key persistent display state for the contextual gutter
/// cells: rolling sample rings (`Latency` / `Spark`) and lifetime
/// decimating trend buffers (`LatencyHist`). Owned by the render
/// loop, threaded into each footer draw.
#[derive(Default)]
pub(crate) struct GutterState {
    rings: std::collections::HashMap<String, std::collections::VecDeque<f64>>,
    latency: std::collections::HashMap<String, LatencyCellState>,
    hists: std::collections::HashMap<String, HistCellState>,
}

/// Rolling-latency cell state: the sample ring plus the SAMPLE-GATED
/// render cache — when the timer's lifetime count hasn't advanced
/// since the last draw (a 1 op/s poller between ops), the previous
/// rendering is reused verbatim: no new trend sample, no re-render.
#[derive(Default)]
struct LatencyCellState {
    ring: std::collections::VecDeque<f64>,
    last_count: u64,
    last_w: usize,
    last_render: String,
}

/// Lifetime-histogram cell state; same sample-gated render cache.
struct HistCellState {
    trend: crate::widgets::DecimatingTrend,
    last_count: u64,
    last_w: usize,
    last_render: String,
}

/// LIFETIME-histogram latency cell for open-ended pollers — the
/// distinct renderable form beside `latency_gutter`'s rolling
/// window. Two regions, LEFT-STACKED:
///
/// - averaged history (cyan): `DecimatingTrend` buckets, re-averaged
///   at half resolution only when the width fills;
/// - raw tail (bright yellow): the most recent samples, one cell per
///   sample, growing rightward into the free margin until the next
///   resample folds them into history.
///
/// Bar heights normalize across BOTH regions together so the eye
/// compares averaged past against raw present on one scale. Labels
/// are the DISCRETE lifetime min∕max of the pushed samples.
fn latency_hist_gutter(
    trend: &mut crate::widgets::DecimatingTrend,
    p50: u64,
    w: usize,
    color: bool,
) -> String {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let cyan = if color { "\x1b[36m" } else { "" };
    let accent = if color { "\x1b[93m" } else { "" };
    let fmt = |ms: f64| {
        if ms >= 100.0 {
            format!("{ms:.0}")
        } else if ms >= 10.0 {
            format!("{ms:.1}")
        } else {
            format!("{ms:.2}")
        }
    };
    let text_probe = format!("{}∕{}ms", fmt(trend.min), fmt(trend.max));
    let spark_w = w.saturating_sub(text_probe.chars().count() + 1).max(4);
    // Size the trend to the cell BEFORE pushing, so the history/tail split is
    // taken at the width actually being drawn (and follows a terminal resize).
    trend.resize(spark_w);
    trend.push(p50 as f64 / 1e6);
    let text = format!("{}∕{}ms", fmt(trend.min), fmt(trend.max));
    // Fixed split: the history occupies a constant `hist_cap` columns (the power
    // of two nearest halfway) and the tail the remainder, one sample per cell.
    // Both are scaled against the SAME extrema so the halves are visually
    // comparable — the whole point of pinning the divider.
    let (hist_cells, raw_cells) = trend.split_view();
    let mut visible: Vec<f64> = Vec::with_capacity(spark_w);
    visible.extend_from_slice(&hist_cells);
    visible.extend_from_slice(&raw_cells);
    let cut = visible.len().saturating_sub(spark_w);
    let visible = &visible[cut..];
    let glyphs = crate::widgets::sparkline_str(visible, visible.len().max(1));
    let hist_n = hist_cells.len().saturating_sub(cut);
    let hist_part: String = glyphs.chars().take(hist_n).collect();
    let raw_part: String = glyphs.chars().skip(hist_n).collect();
    let pad = spark_w.saturating_sub(visible.len());
    format!(
        "{cyan}{hist_part}{reset}{accent}{raw_part}{reset}{:pad$} {dim}{text:>tw$}│{reset} ",
        "",
        tw = w.saturating_sub(spark_w + 1)
    )
}

/// Approximate visible width of a string with ANSI SGR escape
/// codes stripped. SGR sequences (`\x1b[...m`) carry no
/// columns; everything else counts as one column per char.
/// Good enough for the margin prefix, which has no
/// double-width glyphs.
fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            // Escapes end on any final byte (`m`, `K`, `J`, …),
            // not just SGR's `m`.
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            continue;
        }
        // Control chars (`\r`, …) occupy no columns.
        if ch.is_control() {
            continue;
        }
        width += 1;
    }
    width
}

/// Clamp each `\n`-delimited row of `s` to `max_cols` columns
/// independently, then rejoin with `\n`. `\n` itself is not a
/// visible column and must not consume the budget; per-row
/// clamping prevents the second line of a two-line status
/// from being chopped off when the first line is long.
fn clamp_multiline(s: &str, max_cols: usize) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for row in s.split('\n') {
        if !first {
            out.push('\n');
        }
        out.push_str(&nmbrs_runtime::activity::truncate_to_width(row, max_cols));
        first = false;
    }
    out
}

struct LogOnlySinkHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    sink_active: Arc<AtomicBool>,
}

impl SinkHandle for LogOnlySinkHandle {
    fn shutdown(mut self: Box<Self>) {
        self.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        // Defensively clear in case the thread exited early
        // before the post-loop store ran (e.g. spawn failure
        // in some future variant).
        self.sink_active.store(false, Ordering::Release);
    }

    fn owns_terminal(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod redraw_tests {
    //! Layout tests for [`redraw_bottom_region`]. Drives the
    //! pure renderer through a sequence of mocked status
    //! updates and asserts the emitted byte sequence — pinning
    //! the absolute-positioning invariants that produced the
    //! "stacked status snapshots" bug.
    use super::*;

    /// 16 dots across two braille cells, column-major bottom-up: empty at 0,
    /// first cell full at 50%, everything at 100%.
    #[test]
    fn braille_meter_fills_column_major() {
        assert_eq!(braille_meter(0.0), ['\u{2800}', '\u{2800}']);
        // 25% = 4 dots = the first column: dots 7,3,2,1 = 0x47.
        assert_eq!(braille_meter(0.25), ['\u{2847}', '\u{2800}']);
        // 50% = first cell solid (all 8 dots = 0xFF).
        assert_eq!(braille_meter(0.5), ['\u{28FF}', '\u{2800}']);
        assert_eq!(braille_meter(1.0), ['\u{28FF}', '\u{28FF}']);
        // Clamped: nonsense inputs stay in range.
        assert_eq!(braille_meter(7.0), ['\u{28FF}', '\u{28FF}']);
        assert_eq!(braille_meter(-1.0), ['\u{2800}', '\u{2800}']);
    }

    /// The ramp the user reads at a glance: bright orange over 90, dim orange
    /// over 80, yellow over 50, bright white over 25, plain below.
    #[test]
    fn util_colors_step_at_the_declared_thresholds() {
        assert_eq!(util_color(0.95), "\x1b[1;38;5;208m");
        assert_eq!(util_color(0.85), "\x1b[38;5;166m");
        assert_eq!(util_color(0.55), "\x1b[33m");
        assert_eq!(util_color(0.30), "\x1b[97m");
        assert_eq!(util_color(0.10), "");
        // Boundaries are exclusive: exactly 90 is the dim-orange tier, etc.
        assert_eq!(util_color(0.90), "\x1b[38;5;166m");
        assert_eq!(util_color(0.80), "\x1b[33m");
        assert_eq!(util_color(0.50), "\x1b[97m");
        assert_eq!(util_color(0.25), "");
    }

    /// The strip's VISIBLE width must line up with every other gutter cell —
    /// per-item color escapes must not count, and each item is exactly one
    /// glyph cell + two meter cells.
    #[test]
    fn sysmon_gutter_is_width_aligned_with_text_gutter() {
        let items = vec![('⛃', 0.97), ('⚙', 0.34), ('▤', 0.61)];
        let w = 27;
        let ours = crate::status_fold::strip_ansi(&sysmon_gutter(&items, w, true));
        let reference = crate::status_fold::strip_ansi(&text_gutter("x", w, true));
        assert_eq!(
            ours.chars().count(),
            reference.chars().count(),
            "sysmon cell must occupy the same columns as a text cell:\n{ours:?}\n{reference:?}"
        );
        // 3 items × 3 cells + 2 separators = 11 visible strip chars.
        assert!(ours.contains('⛃') && ours.contains('⚙') && ours.contains('▤'));
    }

    /// The continuation blank margin is a gutter of the row-0 width
    /// with the `│` in the same column, so a phase's detail rows align
    /// under its count/time triad. Rows with no contextual gutter get
    /// exactly this divider, nothing more.
    #[test]
    fn blank_gutter_is_width_aligned_divider() {
        let row1 = "   12.3s [5/9]  1.2s │ "; // 23 visible cols
        let target = super::visible_width(row1);
        let mut out: Vec<u8> = Vec::new();
        draw_footer_at_cursor(
            &mut out,
            Some("head\ndetail"),
            None,
            80,
            row1,
            target as u16,
            &[],
            &mut GutterState::default(),
        );
        let out = String::from_utf8(out).expect("utf-8");
        let line1 = out.split("\r\n").nth(1).expect("two rows");
        let margin_end = line1.find("detail").expect("detail text present");
        let margin = &line1[..margin_end];
        assert_eq!(
            super::visible_width(margin),
            target,
            "blank margin must be exactly the row-0 width: {margin:?}"
        );
        // Strip SGR escapes, then everything before the divider must
        // be blank.
        let mut plain = String::new();
        let mut in_esc = false;
        for c in margin.chars() {
            if in_esc {
                if c.is_ascii_alphabetic() {
                    in_esc = false;
                }
                continue;
            }
            if c == '\u{1b}' {
                in_esc = true;
                continue;
            }
            if !c.is_ascii_control() {
                plain.push(c);
            }
        }
        let div = plain.find('│').expect("divider present");
        assert!(
            plain[..div].chars().all(|c| c == ' '),
            "blank margin must be blank before the divider: {plain:?}"
        );
    }

    /// One simulated render tick with a left margin: draw the footer
    /// at the cursor and capture the bytes. 80-col terminal; the
    /// follow-the-log footer is relative-positioned so no row count is
    /// needed.
    fn render_tick_with_margin(
        status: Option<&str>,
        prompt: Option<&PromptInput>,
        margin: &str,
    ) -> String {
        let mut out: Vec<u8> = Vec::new();
        let width = super::visible_width(margin) as u16;
        draw_footer_at_cursor(
            &mut out,
            status,
            prompt,
            80,
            margin,
            width,
            &[],
            &mut GutterState::default(),
        );
        String::from_utf8(out).expect("rendered bytes are utf-8")
    }

    fn one_row_prompt(buf: &str) -> PromptInput {
        // Mimic PromptState::render output for a 1-row window.
        let rendered = format!("\x1b[36m❯\x1b[0m {buf}");
        let cursor_col = (2 + buf.chars().count()) as u16;
        PromptInput {
            rendered,
            window_rows: 1,
            cursor_col,
        }
    }

    /// Each footer row is `\r`-homed and erase-to-EOL'd (`\x1b[K`) to
    /// scrub residue from a wider prior tick, rows are joined with
    /// `\r\n`, and the LAST row leaves no trailing newline so the log
    /// stream below isn't pushed down. The returned offset is the
    /// cursor's row distance below the footer top.
    #[test]
    fn footer_rows_home_erase_and_have_no_trailing_newline() {
        let mut out: Vec<u8> = Vec::new();
        let (rows, below) = draw_footer_at_cursor(
            &mut out,
            Some("head row\ntail row"),
            None,
            80,
            "",
            0,
            &[],
            &mut GutterState::default(),
        );
        let s = String::from_utf8(out).expect("utf-8");
        assert_eq!((rows, below), (2, 1), "two rows, cursor 1 below the top");
        assert!(
            s.contains("\r\x1b[Khead row"),
            "head row homed + erased: {s:?}"
        );
        assert!(
            s.contains("\r\x1b[Ktail row"),
            "tail row homed + erased: {s:?}"
        );
        // Exactly one `\r\n` separates the two rows; none trails.
        assert_eq!(s.matches("\r\n").count(), 1, "one separator only: {s:?}");
        assert!(!s.ends_with("\r\n"), "no trailing newline: {s:?}");
    }

    /// An empty footer (no status, no prompt) draws nothing and
    /// reports a zero offset.
    #[test]
    fn empty_footer_draws_nothing() {
        let mut out: Vec<u8> = Vec::new();
        let (rows, below) = draw_footer_at_cursor(
            &mut out,
            None,
            None,
            80,
            "",
            0,
            &[],
            &mut GutterState::default(),
        );
        assert_eq!((rows, below), (0, 0));
        assert!(out.is_empty(), "empty footer emits no bytes: {out:?}");
    }

    /// Status + prompt rows are prefixed with the same margin the log
    /// lines above carry, so content columns line up across the
    /// log/status divide. The cursor lands in the prompt's content
    /// area (past the margin), set with a column-absolute `\x1b[NG`.
    #[test]
    fn footer_rows_carry_log_margin_for_column_alignment() {
        let margin = "12.34s 5/9 │ ";
        let status = "running";
        let prompt = one_row_prompt("hi");
        let out = render_tick_with_margin(Some(status), Some(&prompt), margin);
        assert!(
            out.contains(&format!("\r\x1b[K{margin}running")),
            "status row must carry the margin: {out:?}"
        );
        assert!(
            out.contains(&format!("\r\x1b[K{margin}\x1b[36m❯\x1b[0m hi")),
            "prompt row must carry the margin: {out:?}"
        );
        // Column = margin_width + cursor_col + 1.
        // margin is 13 visible; "❯ hi" puts cursor_col at 4 → 13+4+1=18.
        assert!(
            out.contains("\x1b[18G"),
            "cursor must skip past the margin width: {out:?}"
        );
    }

    /// When the running-phase row-2 margin is supplied (the
    /// bar+ETA+spinner gutter), line 0 of the status inherits the
    /// standard row-1 margin and lines 1+ inherit the row-2 margin.
    #[test]
    fn contextual_gutter_applies_to_detail_row_only() {
        // New contract: row 0 carries the triad margin; a row whose
        // RowGutter is Bar(f) renders the completion-bar cell; Blank
        // rows get the plain divider.
        let row1 = "12.34s 5/9 │ ";
        let status = "running\nstats line";
        let mut out: Vec<u8> = Vec::new();
        let gutters = vec![
            crate::status_fold::RowGutter::Blank,
            crate::status_fold::RowGutter::Bar(0.5),
        ];
        draw_footer_at_cursor(
            &mut out,
            Some(status),
            None,
            80,
            row1,
            super::visible_width(row1) as u16,
            &gutters,
            &mut GutterState::default(),
        );
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains(&format!("\r\x1b[K{row1}running")),
            "line 0 MUST carry the triad margin: {out:?}"
        );
        // Detail row: half-full braille bar (has full cells) + text.
        assert!(
            out.contains('\u{28ff}') && out.contains("stats line"),
            "line 1 MUST carry the bar gutter: {out:?}"
        );
    }

    /// A status row wider than the terminal is TRUNCATED to one
    /// physical row. The climb-back contract (`cursor_below_top`)
    /// counts logical rows — a wrapped row would desync it, making the
    /// next tick's `\x1b[{n}A` land mid-footer and the `\x1b[J` clear
    /// clip footer rows (the "key-metrics line pops in and out"
    /// symptom). Also: a truncation that strands an open SGR must be
    /// closed with a reset so the tint can't bleed into the next row.
    #[test]
    fn overwide_status_row_truncates_to_one_physical_row() {
        let margin = "12.34s 5/9 │ "; // 13 visible cols
        let wide = "x".repeat(200); // ≫ 80-col terminal
        let status = format!("head\n{wide}");
        let mut out: Vec<u8> = Vec::new();
        let (rows, below) = draw_footer_at_cursor(
            &mut out,
            Some(&status),
            None,
            80,
            margin,
            super::visible_width(margin) as u16,
            &[],
            &mut GutterState::default(),
        );
        assert_eq!((rows, below), (2, 1));
        let out = String::from_utf8(out).expect("utf-8");
        for line in out.split("\r\n") {
            assert!(
                super::visible_width(line) <= 80,
                "footer row must fit the terminal width ({}): {line:?}",
                super::visible_width(line)
            );
        }
        assert!(out.contains('…'), "truncation marker expected: {out:?}");

        // Open-SGR guard: a colored row cut before its reset gets one
        // appended so the style can't leak into the following row.
        let colored = format!("\x1b[31m{}\x1b[0m", "y".repeat(200));
        let mut out2: Vec<u8> = Vec::new();
        draw_footer_at_cursor(
            &mut out2,
            Some(&colored),
            None,
            80,
            margin,
            super::visible_width(margin) as u16,
            &[],
            &mut GutterState::default(),
        );
        let out2 = String::from_utf8(out2).expect("utf-8");
        assert!(
            out2.ends_with("\x1b[0m"),
            "truncated colored row must close its SGR: {out2:?}"
        );
    }

    /// Empty gutter list → row 0 carries the triad margin, every
    /// other row the blank-aligned divider (the no-contextual-cell
    /// baseline path).
    #[test]
    fn empty_gutters_fall_back_to_blank_divider() {
        let row1 = "12.34s 5/9 │ ";
        let status = "first\nsecond";
        let mut out: Vec<u8> = Vec::new();
        draw_footer_at_cursor(
            &mut out,
            Some(status),
            None,
            80,
            row1,
            super::visible_width(row1) as u16,
            &[],
            &mut GutterState::default(),
        );
        let out = String::from_utf8(out).expect("utf-8");
        assert!(
            out.contains(&format!("\r\x1b[K{row1}first")),
            "line 0 MUST carry the triad margin: {out:?}"
        );
        assert!(
            out.contains("│\x1b[0m second"),
            "line 1 MUST carry the blank divider margin: {out:?}"
        );
        assert!(
            !out.contains(&format!("{row1}second")),
            "line 1 must NOT repeat the triad margin: {out:?}"
        );
    }

    /// A fresh sink (no shared cursor, or one still holding the
    /// `RESUME_FRESH` sentinel) seeds from the live `log_seq_total`
    /// so it doesn't re-print history the observer already wrote.
    #[test]
    fn seed_last_seen_fresh_uses_current() {
        assert_eq!(super::seed_last_seen(None, 42), 42);
        assert_eq!(super::seed_last_seen(Some(super::RESUME_FRESH), 42), 42);
    }

    /// A swap re-entry (the supervisor's shared cursor holds the
    /// prior sink's final `last_seen`, not the sentinel) resumes
    /// from that cursor so the lines that scrolled under the TUI's
    /// alternate screen are re-emitted into the restored scrollback.
    #[test]
    fn seed_last_seen_resume_uses_stored_cursor() {
        assert_eq!(super::seed_last_seen(Some(10), 42), 10);
        assert_eq!(super::seed_last_seen(Some(0), 42), 0);
    }

    /// `visible_width` strips ANSI SGR sequences so the
    /// margin-offset arithmetic still lines up when the margin
    /// is color-painted.
    #[test]
    fn visible_width_strips_sgr_sequences() {
        assert_eq!(super::visible_width(""), 0);
        assert_eq!(super::visible_width("plain text"), 10);
        // `\x1b[2m...\x1b[0m` carries no columns.
        assert_eq!(super::visible_width("\x1b[2mdim\x1b[0m text"), 8);
        // Nested / multiple escapes.
        assert_eq!(
            super::visible_width("\x1b[1;31mAB\x1b[0m\x1b[32mCD\x1b[0m"),
            4
        );
    }

    /// The phase time must land in the SAME column whichever row carries it.
    ///
    /// A header row renders `body │` and a leaf row renders its duration through
    /// `text_gutter`. Before the shared right margin the leaf's duration sat flush
    /// against the divider while the header's stopped a column short, so the same
    /// quantity appeared in two columns depending on the row:
    ///     0.0s│     ✓ flush  [1/4]
    ///    45.0s │    ✓ [concurrent_query]
    #[test]
    fn phase_time_lands_in_one_column_across_row_kinds() {
        let total = 256usize;
        let body = crate::widgets::margin_body(total, "[65/256]", Some(45.0), Some(569.0));
        let header = super::format_margin_from_body(&body, false);
        let blank_w = crate::widgets::margin_body_width(total) + 1;
        let leaf = super::text_gutter("0.0s", blank_w, false);

        let divider = |cell: &str| cell.chars().position(|c| c == '│').expect("divider");
        assert_eq!(
            divider(&header),
            divider(&leaf),
            "divider column differs:\n  header |{header}|\n  leaf   |{leaf}|"
        );

        // Both durations end on the column immediately before the divider's space.
        let ends_at = |cell: &str, needle: &str| {
            cell.find(needle)
                .map(|b| cell[..b].chars().count() + needle.chars().count())
                .expect("duration present")
        };
        assert_eq!(
            ends_at(&header, "45.0s"),
            ends_at(&leaf, "0.0s"),
            "durations end in different columns:\n  header |{header}|\n  leaf   |{leaf}|"
        );
    }

    /// Timing rows carry a mark so they are not mistaken for the metric cells
    /// that share the column — and it must be single-width, because the width
    /// arithmetic counts characters.
    #[test]
    fn timing_rows_are_marked_with_a_single_cell_glyph() {
        let body = crate::widgets::margin_body(4, "[1/4]", Some(1.0), Some(2.0));
        assert!(body.starts_with(crate::widgets::TIMING_MARK), "got: {body}");
        assert_eq!(crate::widgets::TIMING_MARK.chars().count(), 1);
        // Emoji-presentation clocks render two cells wide and would shift every
        // divider on these rows by one column.
        for wide in ["\u{23F1}", "\u{231A}", "\u{1F550}"] {
            assert_ne!(crate::widgets::TIMING_MARK, wide);
        }
    }

    /// A key metric is framed, not aligned like a timing, so the row announces
    /// what kind of number it carries before the number is read.
    #[test]
    fn key_metric_cell_is_bracketed_and_centred() {
        let cell = super::labeled_gutter("recall", "97.88%", 28, false);
        let plain: String = cell.chars().filter(|c| *c != '\u{1b}').collect();
        assert!(plain.contains("[") && plain.contains("]"), "got: |{cell}|");
        assert!(plain.contains("recall: 97.88%"), "got: |{cell}|");
        // Frame spans the cell: `[` first, `]` against the divider's margin.
        let open = plain.find('[').unwrap();
        assert_eq!(open, 0, "bracket must open at the cell edge: |{cell}|");
        let close = plain.find(']').unwrap();
        let divider = plain.find('│').unwrap();
        assert_eq!(
            divider - close,
            2,
            "close bracket sits one space before the divider: |{cell}|"
        );
        // Centred: padding on both sides differs by at most one.
        let inner = &plain[open + 1..close];
        let lead = inner.len() - inner.trim_start().len();
        let trail = inner.len() - inner.trim_end().len();
        assert!(lead.abs_diff(trail) <= 1, "not centred: |{inner}|");
    }
}

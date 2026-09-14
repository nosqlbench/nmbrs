// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Display-side actor that owns [`RunState`] and publishes
//! immutable snapshots.
//!
//! See `docs/SRD/02_concurrency_model.md` §"Display and
//! Diagnostic Decoupling" for the architectural rationale. In
//! short:
//!
//! - **Downstream (core → UI):** the actor owns a private
//!   mutable `RunState`. After every applied command, it
//!   publishes a fresh `Arc<RunState>` into a shared
//!   [`arc_swap::ArcSwap`]. UI / web / OOB readers do
//!   `snapshot.load()` — a single atomic op, never a wait.
//! - **Upstream (UI → core):** every mutation is a typed
//!   [`RunStateCmd`] variant sent over an [`mpsc::Sender`]. The
//!   actor's `match` is exhaustive, so a new mutation cannot be
//!   added without the actor handling it.
//!
//! There is no shared `RwLock<RunState>` — the principle is that
//! the renderer can never wait on the writer and the writer can
//! never wait on the renderer.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;

use nmbrs_metrics::summaries::binomial_summary::BinomialSummary;
use nmbrs_metrics::summaries::ewma::Ewma;
use nmbrs_metrics::summaries::peak_tracker::PeakTracker;
use nmbrs_runtime::observer::PhaseProgressUpdate;
use nmbrs_runtime::scene_tree::{SceneNodeId, SceneTree};

use crate::state::{ActivePhase, LogEntry, LogSeverity, PhaseSummary, RunState};

/// One log line on the **durable scrollback stream** — the second
/// down-channel from the actor to the active terminal-mode sink.
///
/// The status footer is a latest-wins ArcSwap snapshot (only the
/// newest matters); log scrollback is sequential DATA where every
/// line must be delivered, in order, exactly once. ArcSwap cannot
/// carry deltas, so a renderer that misses a swap loses the
/// intermediate lines — the drop bug this stream removes. The
/// actor feeds one `LogLine` per [`RunStateCmd::Log`] into an
/// **unbounded** channel; the sink drains it fully every tick and
/// emits every line. `seq` matches the ring's
/// [`RunState::log_seq_total`] so the sink can skip lines already
/// emitted on stderr before the display handoff (or by a prior
/// sink before a Ctrl-T swap).
pub(crate) struct LogLine {
    pub seq: u64,
    pub entry: LogEntry,
    /// Uncolored margin body (`session · [n/N] · leaf`) computed by the
    /// actor AT LOG-PROCESSING TIME — i.e. from state that includes every
    /// command that preceded this line in the stream. The sink renders
    /// this stamp (adding color/divider) instead of a render-time
    /// snapshot margin, so a line's gutter can never disagree with the
    /// event the line describes.
    pub margin_body: String,
    /// gutter/final — when this entry is a phase's ✓ outcome block and
    /// the phase published a final gutter spec, the sink renders it as
    /// the margin CELL of the block's DETAIL line (row 1). The header
    /// row keeps the timing triad untouched; every other row gets the
    /// standard stamp.
    pub detail_gutter: Option<nmbrs_runtime::wrappers::gutter::GutterSpec>,
}

/// Single-consumer handle to the durable scrollback stream, held by
/// whichever terminal-mode sink is currently rendering.
///
/// The receiver is single-consumer and must move between sinks
/// across a Ctrl-T swap (terminal → TUI → terminal), so it lives in
/// a shared cell on [`RunStateHandle`]. A sink `take`s it on start
/// (via [`RunStateHandle::take_log_stream`]) and this guard restores
/// it to the cell on drop — so the next sink brought up after a swap
/// resumes draining the SAME stream, and the lines that buffered
/// while the TUI owned the alternate screen flush into the restored
/// scrollback. The cell's `Mutex` is touched only at start / teardown
/// (never per render tick) and only one sink renders at a time, so it
/// is a handoff latch, not shared render state.
pub(crate) struct ScrollbackReceiver {
    cell: Arc<Mutex<Option<mpsc::Receiver<LogLine>>>>,
    rx: Option<mpsc::Receiver<LogLine>>,
}

impl ScrollbackReceiver {
    /// Non-blocking receive of the next buffered log line, or `None`
    /// when the stream is momentarily empty / the receiver wasn't
    /// available (another sink holds it — shouldn't happen, since
    /// sinks are torn down before the next is brought up).
    pub(crate) fn try_next(&self) -> Option<LogLine> {
        self.rx.as_ref().and_then(|rx| rx.try_recv().ok())
    }
}

impl Drop for ScrollbackReceiver {
    fn drop(&mut self) {
        if let Some(rx) = self.rx.take() {
            *self.cell.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
        }
    }
}

/// One mutation of [`RunState`]. Every observer-driven and
/// UI-driven write is one of these variants. The actor's
/// `match` is exhaustive — adding a new mutation requires
/// adding a variant and a handler arm, by design.
#[derive(Debug)]
pub enum RunStateCmd {
    /// Session directory is ready — open `<dir>/transcript.log` and flush any
    /// lines buffered since actor start.
    OpenTranscript(std::path::PathBuf),
    /// Latest sysmon sample window (session-level host utilization).
    Sysmon(nmbrs_runtime::sysmon::SysmonSample),
    /// Replace the scenario tree wholesale (called once after
    /// pre-mapping).
    InstallTree(SceneTree),
    /// A phase has begun. Carries everything needed to seed an
    /// [`ActivePhase`] entry.
    PhaseStarting {
        exec_id: u64,
        /// SRD-100 P1c — dispatch-time scene-tree node id for this
        /// phase. The actor flips THIS node (race-safe under
        /// concurrent same-name dispatch) instead of re-matching by
        /// `(name, status)`.
        scene_node_id: SceneNodeId,
        name: String,
        labels: String,
        /// Count of distinct op definitions in the phase (the
        /// "shape" of the stanza; typically 1 for query workloads).
        op_templates: usize,
        /// Number of times the stanza will iterate (the "amount"
        /// of work).
        total_cycles: u64,
        concurrency: usize,
    },
    /// A phase finished cleanly. The actor builds the
    /// [`PhaseSummary`] from its own active-phase entry before
    /// removing it.
    PhaseCompleted {
        exec_id: u64,
        /// SRD-100 P1c — dispatch-time scene-tree node id (see
        /// [`Self::PhaseStarting`]).
        scene_node_id: SceneNodeId,
        name: String,
        labels: String,
        duration_secs: f64,
    },
    /// A phase failed. The actor removes the active entry and
    /// flips the tree node's status.
    PhaseFailed {
        exec_id: u64,
        /// SRD-100 P1c — dispatch-time scene-tree node id (see
        /// [`Self::PhaseStarting`]).
        scene_node_id: SceneNodeId,
        name: String,
        labels: String,
        error: String,
    },
    /// SRD-63 — an op-level status leaf (`readout: visible`) started, keyed
    /// by its parent phase's scene-node id.
    OpStarting {
        parent: SceneNodeId,
        op_name: String,
    },
    /// An op-level status leaf completed; `duration_secs` is its execution time.
    /// The op's key measurable, arriving just before its completion.
    OpMeasure {
        parent: nmbrs_runtime::scene_tree::SceneNodeId,
        op_name: String,
        text: String,
    },
    OpCompleted {
        parent: SceneNodeId,
        op_name: String,
        duration_secs: f64,
    },
    /// An op-level status leaf failed.
    OpFailed {
        parent: SceneNodeId,
        op_name: String,
        error: String,
    },
    /// Live update from the executor's progress thread.
    PhaseProgress(PhaseProgressUpdate),
    /// SRD-100 P2 — attach the per-phase live render handle to the
    /// matching `ActivePhase`, fired once by the executor after the
    /// activity's metrics/binder exist. The consumer-side status
    /// renderers fold `active_phases` and render each phase's status
    /// off its handle (no producer thread, no `status_render` scalar).
    AttachPhaseRender(nmbrs_runtime::observer::PhaseRenderHandle),
    /// Run is complete; flips `state.finished` so the renderer
    /// drops out of its event loop.
    RunFinished,
    /// Append a log line to the ring buffer.
    Log {
        severity: LogSeverity,
        tag: crate::state::EventTag,
        message: String,
    },
    /// Latency frame ingest from the metrics scheduler. Updates
    /// the live percentile fields, the rolling histories, and
    /// each active phase's peak trackers.
    LatencyFrame {
        min: u64,
        p50: u64,
        p90: u64,
        p99: u64,
        p999: u64,
        max: u64,
    },
    /// Sparkline samples for the throughput / secondary-counter
    /// rows. Driven from the same metrics-frame drain as
    /// `LatencyFrame`.
    SparklineSamples {
        ops: Option<f64>,
        rows: Option<f64>,
        rows_label: Option<String>,
    },
    /// Bookkeeping fields the runner sets before the run starts
    /// (profiler / limit strings shown in the header strip).
    SetMeta {
        profiler: Option<String>,
        limit: Option<String>,
    },
    /// Synchronous render checkpoint. Processed in command order
    /// so its position in the queue marks "all preceding mutations
    /// have been applied". The actor:
    ///
    /// 1. registers `tx` in the shared `pending_acks` queue,
    /// 2. sets the `force_redraw` flag,
    /// 3. publishes a fresh snapshot,
    ///
    /// after which the TUI thread observes `force_redraw`, drops
    /// its poll timeout to zero, draws one frame against the
    /// just-published state, and drains `pending_acks` — at which
    /// point each registered `tx` is signalled and the original
    /// caller's blocking `rx.recv()` returns.
    ///
    /// Used by [`crate::observer::TuiObserver::phase_completed`] /
    /// [`crate::observer::TuiObserver::phase_failed`] to guarantee
    /// at least one fully rendered frame of the just-completed
    /// phase before the executor moves on. The `tick_rate`
    /// throttle is bypassed for this single redraw.
    FrameAck(mpsc::Sender<()>),
    /// Queue barrier: acked by the actor the moment it is
    /// processed, AFTER every command sent before it (the inbox is
    /// ordered). Unlike [`Self::FrameAck`] it does not wait for a
    /// TUI draw, so it works on every surface — the post-run
    /// reporter uses it to guarantee the snapshot reflects all
    /// lifecycle events before counting phases (a fast walk could
    /// otherwise outrun the actor and report bogus not-run counts
    /// and exit codes).
    Barrier(mpsc::Sender<()>),
}

/// Synchronisation surface shared between the actor (which
/// receives [`RunStateCmd::FrameAck`]) and the TUI app (which
/// honours it). Cloned cheaply; held by both sides.
///
/// - `force_redraw` — when set, the TUI app drops its poll
///   timeout to zero so the next iteration draws immediately,
///   bypassing `tick_rate`. Cleared by the app after the draw.
/// - `pending_acks` — `tx` channels handed in via `FrameAck`.
///   Drained and signalled by the app after each `terminal.draw()`.
#[derive(Clone, Default)]
pub struct FrameSync {
    pub force_redraw: Arc<AtomicBool>,
    pub pending_acks: Arc<Mutex<Vec<mpsc::Sender<()>>>>,
}

impl FrameSync {
    /// Drain every queued ack and signal it. Called by the TUI
    /// app immediately after a successful `terminal.draw()`. The
    /// `force_redraw` flag is cleared in the same call so the
    /// app doesn't busy-spin until the next FrameAck arrives.
    pub fn signal_post_draw(&self) {
        self.force_redraw.store(false, Ordering::Release);
        let mut q = self.pending_acks.lock().unwrap_or_else(|e| e.into_inner());
        for tx in q.drain(..) {
            // Receiver may have given up (recv_timeout); drop is
            // fine.
            let _ = tx.send(());
        }
    }
}

/// Handle the rest of the system uses to talk to the actor.
///
/// Cheap to clone — every field is itself `Arc`-style. The
/// snapshot side is read with a single atomic load; the inbox
/// side is fire-and-forget (`send` returns immediately, dropped
/// silently if the actor has exited).
#[derive(Clone)]
pub struct RunStateHandle {
    snapshot: Arc<ArcSwap<RunState>>,
    inbox: mpsc::Sender<RunStateCmd>,
    frame_sync: FrameSync,
    /// Receiver end of the durable scrollback stream (see
    /// [`LogLine`]), parked here between sinks so it survives a
    /// Ctrl-T swap. Taken by the active terminal-mode sink via
    /// [`Self::take_log_stream`] and restored on that sink's
    /// teardown.
    log_rx: Arc<Mutex<Option<mpsc::Receiver<LogLine>>>>,
    /// One-way latch: `false` until a terminal-mode
    /// [`crate::log_only_sink::LogOnlySink`] first claims the stream
    /// (via [`Self::take_log_stream`]), then `true` for the rest of
    /// the run. The actor feeds the scrollback stream ONLY while this
    /// is set, so modes with no `LogOnlySink` (`tui=on` full TUI,
    /// `tui=off` piped/CI, `tui=formatted`) never buffer a stream
    /// nobody drains — the line still lands in the ring and
    /// `session.log`. It is one-way (never cleared) so a Ctrl-T swap
    /// window — when the sink is torn down and no one is draining —
    /// keeps buffering, and the next terminal sink replays those lines
    /// into the restored scrollback.
    log_stream_active: Arc<AtomicBool>,
}

impl RunStateHandle {
    /// Load the current snapshot. Always returns the most
    /// recently published `Arc<RunState>` — the only way for
    /// this to return a stale snapshot is if the actor hasn't
    /// processed a sent command yet, which is the intended
    /// decoupling.
    pub fn load(&self) -> Arc<RunState> {
        self.snapshot.load_full()
    }

    /// Send a command into the actor inbox. Fire-and-forget: if
    /// the actor has exited, the send is dropped silently. The
    /// actor never falls behind in a way that pressures the
    /// caller — the caller doesn't wait for it.
    pub fn send(&self, cmd: RunStateCmd) {
        let _ = self.inbox.send(cmd);
    }

    /// Block until the TUI has rendered at least one frame that
    /// reflects every command sent on this handle prior to the
    /// call. Bypasses the TUI's `tick_rate` throttle so the
    /// caller doesn't pay up to a full tick of latency.
    ///
    /// Returns `true` if the round trip completed inside `timeout`
    /// and `false` if the TUI didn't respond (e.g. the app has
    /// already exited or is wedged). Callers should treat a
    /// `false` as best-effort: the run continues either way.
    pub fn flush_frame(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel::<()>();
        if self.inbox.send(RunStateCmd::FrameAck(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// Block until the actor has PROCESSED every command sent on
    /// this handle prior to the call (ordered-inbox barrier; no TUI
    /// involvement, so it works on every surface). Best-effort:
    /// `false` on timeout or a dead actor.
    pub fn drain_barrier(&self, timeout: Duration) -> bool {
        let (tx, rx) = mpsc::channel::<()>();
        if self.inbox.send(RunStateCmd::Barrier(tx)).is_err() {
            return false;
        }
        rx.recv_timeout(timeout).is_ok()
    }

    /// Access the shared frame-sync surface. The TUI app holds a
    /// clone and consumes it from its draw loop.
    pub fn frame_sync(&self) -> FrameSync {
        self.frame_sync.clone()
    }

    /// Take ownership of the durable scrollback stream receiver for
    /// the duration of a terminal-mode sink's render loop. The
    /// returned [`ScrollbackReceiver`] restores the receiver to the
    /// shared cell when it drops, so the next sink brought up after
    /// a Ctrl-T swap resumes draining the same stream (the lines
    /// that buffered while the TUI owned the alternate screen flush
    /// into the restored scrollback). Only one sink renders at a
    /// time, so at most one live `ScrollbackReceiver` exists.
    ///
    /// Flips the actor's [`Self::log_stream_active`] latch on first
    /// call, so the actor begins buffering scrollback deltas. Callers
    /// must set the latch (i.e. call this) BEFORE seeding their
    /// `last_seen` cursor from `log_seq_total`, so no line sent in the
    /// handoff window is lost: every line the sink expects to emit
    /// (seq greater than the seed) is guaranteed to already be in the
    /// stream.
    pub(crate) fn take_log_stream(&self) -> ScrollbackReceiver {
        self.log_stream_active.store(true, Ordering::Release);
        let rx = self.log_rx.lock().unwrap_or_else(|e| e.into_inner()).take();
        ScrollbackReceiver {
            cell: self.log_rx.clone(),
            rx,
        }
    }
}

/// Spawn the RunState actor on its own OS thread. Returns the
/// handle (used by the observer, the TUI app, the web API, and
/// any OOB introspection surface) and the thread `JoinHandle`
/// (so the runner can join cleanly at shutdown).
///
/// The actor exits when every clone of the inbox sender is
/// dropped — `Receiver::recv` returns `Err`, the loop falls
/// through, and the final state is published one more time so
/// post-shutdown readers see `finished = true`.
/// Durable transcript of the display surface.
///
/// The settled half of the TUI — everything that has scrolled past an active
/// phase — is exactly the [`LogLine`] stream: sequenced, append-only, and
/// already carrying the actor-stamped `margin_body` (the gutter). The ACTIVE
/// half is re-derived from the snapshot every render tick and never appended,
/// so there is nothing to persist there and no boundary to guess at. Teeing
/// here, at the one place margins are stamped, yields a file that is ordered
/// and complete by construction — no second consumer of the scrollback
/// stream, which a sink has exclusively taken.
///
/// The session directory is not known when the actor spawns (the runner
/// creates it later), so lines are BUFFERED until
/// [`RunStateCmd::OpenTranscript`] arrives and then flushed in order. Without
/// that buffer the transcript would silently lose its opening lines — which
/// are the ones naming the session, the workload, and the resolved params.
#[derive(Default)]
pub(crate) struct Transcript {
    file: Option<std::io::BufWriter<std::fs::File>>,
    pending: Vec<String>,
}

impl Transcript {
    /// Render one settled line the way the terminal shows it: the gutter body,
    /// a divider, then the message. Uncolored — a transcript is read later, by
    /// tools as often as by people.
    fn format(line: &LogLine) -> String {
        format!("{:>27} │ {}", line.margin_body, line.entry.message)
    }

    fn record(&mut self, line: &LogLine) {
        let text = Self::format(line);
        match &mut self.file {
            Some(f) => {
                use std::io::Write;
                let _ = writeln!(f, "{text}");
                // Flushed per line: a transcript's whole value is being
                // readable WHILE the run is in flight, and a run that dies is
                // exactly when its tail matters most.
                let _ = f.flush();
            }
            None => self.pending.push(text),
        }
    }

    fn open(&mut self, path: &std::path::Path) {
        let Ok(f) = std::fs::File::create(path) else {
            return;
        };
        let mut w = std::io::BufWriter::new(f);
        use std::io::Write;
        for line in self.pending.drain(..) {
            let _ = writeln!(w, "{line}");
        }
        let _ = w.flush();
        self.file = Some(w);
    }
}

pub fn spawn_run_state_actor(initial: RunState) -> (RunStateHandle, JoinHandle<()>) {
    let snapshot = Arc::new(ArcSwap::new(Arc::new(initial.clone())));
    let (tx, rx) = mpsc::channel::<RunStateCmd>();
    // Durable, ordered, no-drop scrollback stream: the second
    // down-channel (ArcSwap carries the latest-wins status; this
    // carries every log delta). Unbounded so a burst of log lines
    // never blocks the actor (which serves fire-and-forget
    // producer threads) and never evicts an un-emitted line the
    // way the bounded ring did. A slow terminal lets it grow during
    // a burst (bounded by the run's total log volume); the sink
    // drains it fully on the next lull.
    let (log_tx, log_rx) = mpsc::channel::<LogLine>();
    let log_rx = Arc::new(Mutex::new(Some(log_rx)));
    // Latch: the actor feeds the scrollback stream only once a
    // terminal-mode sink has claimed it (see `log_stream_active`).
    let log_stream_active = Arc::new(AtomicBool::new(false));
    let log_stream_active_for_thread = log_stream_active.clone();
    let snapshot_for_thread = snapshot.clone();
    let frame_sync = FrameSync::default();
    let frame_sync_for_thread = frame_sync.clone();

    let handle = std::thread::Builder::new()
        .name("run-state-actor".into())
        .spawn(move || {
            let mut state = initial;
            let mut transcript = Transcript::default();
            // recv() blocks the actor thread when the inbox is
            // empty — fine, this is a dedicated OS thread, not a
            // tokio worker. SRD-02 §"No Blocking Primitives in
            // Async Contexts" only forbids blocking *inside*
            // tokio.
            while let Ok(first) = rx.recv() {
                handle_cmd(
                    &mut state,
                    &frame_sync_for_thread,
                    &log_tx,
                    &log_stream_active_for_thread,
                    &mut transcript,
                    first,
                );
                // Coalesce: drain any further-pending commands
                // before publishing. Cuts publish cost when the
                // executor bursts updates; readers always see
                // the latest published state anyway.
                while let Ok(more) = rx.try_recv() {
                    handle_cmd(
                        &mut state,
                        &frame_sync_for_thread,
                        &log_tx,
                        &log_stream_active_for_thread,
                        &mut transcript,
                        more,
                    );
                }
                snapshot_for_thread.store(Arc::new(state.clone()));
            }
            // Final publish on shutdown — straggler readers see
            // post-shutdown state (finished = true, last logs).
            snapshot_for_thread.store(Arc::new(state));
            // Drop any acks still queued so blocked observers
            // unblock instead of waiting out their full timeout.
            frame_sync_for_thread.signal_post_draw();
            // `log_tx` drops here; the sink's drain sees the
            // stream disconnect only after every buffered line has
            // been delivered (mpsc yields buffered items before
            // `Disconnected`).
        })
        .expect("spawn run-state-actor thread");

    (
        RunStateHandle {
            snapshot,
            inbox: tx,
            frame_sync,
            log_rx,
            log_stream_active,
        },
        handle,
    )
}

/// Top-level command dispatch. Most commands route to `apply`,
/// which mutates the [`RunState`] in place. `FrameAck` is handled
/// here because its target is the [`FrameSync`] surface, not the
/// state.
fn handle_cmd(
    state: &mut RunState,
    frame_sync: &FrameSync,
    log_tx: &mpsc::Sender<LogLine>,
    log_stream_active: &AtomicBool,
    transcript: &mut Transcript,
    cmd: RunStateCmd,
) {
    match cmd {
        RunStateCmd::OpenTranscript(path) => {
            transcript.open(&path);
        }
        RunStateCmd::Barrier(tx) => {
            // All prior commands are already applied (ordered inbox);
            // ack immediately. Receiver may have timed out — ignore.
            let _ = tx.send(());
        }
        RunStateCmd::FrameAck(tx) => {
            // Register the tx so the TUI app can signal it after
            // the next draw. Setting `force_redraw` bypasses the
            // app's tick_rate so that draw fires within ~1 ms
            // rather than waiting up to a full tick.
            frame_sync
                .pending_acks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(tx);
            frame_sync.force_redraw.store(true, Ordering::Release);
        }
        RunStateCmd::Log {
            severity,
            tag,
            message,
        } => {
            // Two sinks for the same line, kept in lock-step:
            //  1. the bounded ring (inspector `log` view, TUI log
            //     panel, post-run failure dump) — last 200 only;
            //  2. the durable scrollback stream — every line, in
            //     order, exactly once, never evicted. The ring's
            //     `seq` tags the stream item so a terminal sink can
            //     skip lines already emitted on stderr before the
            //     display handoff (or by a prior sink pre-swap).
            let entry = LogEntry {
                severity,
                message,
                tag,
                at: std::time::SystemTime::now(),
            };
            // The line is ALWAYS stamped and recorded to the transcript —
            // the durable transcript is a property of the run, not of which
            // display sink happens to be attached, so `tui=off` and
            // `tui=formatted` produce the same file as `tui=terminal`.
            //
            // The in-memory STREAM, by contrast, is fed only once a
            // terminal-mode sink has claimed it: with no drainer it would
            // grow unbounded. So the stamping is unconditional and only the
            // `send` is gated.
            {
                let stream_entry = entry.clone();
                // gutter/final: a ✓ outcome block for a phase whose
                // render handle carries a final gutter spec gets that
                // spec attached — the sink renders it as the detail
                // line's margin cell. The runtime stores the final
                // spec BEFORE logging the outcome, and the phase is
                // still in active_phases at this point (its
                // PhaseCompleted follows this Log in the stream), so
                // the head-line `[name]` match is unambiguous among
                // actives.
                let mut margin_body: Option<String> = None;
                let detail_gutter = if stream_entry.tag.attached
                    == Some(nmbrs_runtime::lifecycle::EventType::PhaseEnd)
                    && stream_entry.tag.category == crate::state::EventCategory::Outcome
                {
                    let plain = crate::status_fold::strip_ansi(&stream_entry.message);
                    let subject = plain
                        .split('[')
                        .nth(1)
                        .map(|rest| rest.split(']').next().unwrap_or("").to_string())
                        .and_then(|name| {
                            state
                                .active_phases
                                .values()
                                .find(|a| a.name == name.as_str())
                        });
                    // SRD-92 R1: the ✓ header wears the completing
                    // NODE'S OWN triad ([n/N] from its plan seq, its
                    // session-clock delta) — the workload-level stamp
                    // can point at a still-running sibling under
                    // concurrent dispatch.
                    if let Some((s, n)) =
                        subject.and_then(|a| a.render.as_ref()).and_then(|h| h.seq)
                    {
                        let sess = state.elapsed_secs();
                        let leaf = subject.map(|a| (sess - a.session_started).max(0.0));
                        margin_body = Some(crate::widgets::margin_body(
                            n,
                            &format!("[{s}/{n}]"),
                            leaf,
                            Some(sess),
                        ));
                    }
                    subject
                        .and_then(|a| a.render.as_ref())
                        .and_then(|h| h.gutter.load_full())
                        .map(|arc| (*arc).clone())
                } else {
                    None
                };
                let seq = state.push_log_entry(entry);
                // Fire-and-forget: a momentarily-parked receiver
                // (Ctrl-T swap window) still buffers; the next sink
                // replays it into the restored scrollback.
                let margin_body = margin_body.unwrap_or_else(|| state.margin_body_stamp());
                let line = LogLine {
                    seq,
                    entry: stream_entry,
                    margin_body,
                    detail_gutter,
                };
                transcript.record(&line);
                if log_stream_active.load(Ordering::Acquire) {
                    let _ = log_tx.send(line);
                }
            }
        }
        RunStateCmd::PhaseCompleted {
            exec_id,
            scene_node_id,
            name,
            labels,
            duration_secs,
        } => {
            // Preserve the per-op timing leaves into the durable
            // scrollback: the footer's op rows (SRD-63 `readout:
            // visible`) vanish with the active-phase block at
            // completion, but their individual timings are exactly the
            // detail the aggregate line forgets. Emit each op's FINAL
            // leaf right under the phase's ✓ outcome block (the
            // outcome Log line precedes this command in the stream, so
            // ordering is inherent). Skipped/elided phases have no
            // leaves recorded and emit nothing.
            if log_stream_active.load(Ordering::Acquire)
                && nmbrs_runtime::observer::completed_phase_display()
                    == nmbrs_runtime::observer::CompletedPhaseDisplay::Full
            {
                let leaves = state.final_op_leaves(&name, &labels);
                for (text, duration_cell) in leaves {
                    let entry = LogEntry {
                        severity: crate::state::LogSeverity::Info,
                        message: text,
                        // SRD-92: op leaves are detail rows of the ✓
                        // block — no triad; their gutter cell carries
                        // the node's execution duration (the default
                        // cell for a visible node with no declared
                        // gutter), the per-step time ledger retained
                        // into scrollback.
                        tag: nmbrs_runtime::observer::EventTag::at(
                            nmbrs_runtime::lifecycle::EventType::PhaseEnd,
                            nmbrs_runtime::observer::EventCategory::Evaluation,
                        ),
                        at: std::time::SystemTime::now(),
                    };
                    let stream_entry = entry.clone();
                    let seq = state.push_log_entry(entry);
                    let margin_body = state.margin_body_stamp();
                    let line = LogLine {
                        seq,
                        entry: stream_entry,
                        margin_body,
                        detail_gutter: Some(nmbrs_runtime::wrappers::gutter::GutterSpec::Text(
                            duration_cell,
                        )),
                    };
                    transcript.record(&line);
                    let _ = log_tx.send(line);
                }
            }
            apply(
                state,
                RunStateCmd::PhaseCompleted {
                    exec_id,
                    scene_node_id,
                    name,
                    labels,
                    duration_secs,
                },
            );
        }
        other => apply(state, other),
    }
}

fn apply(state: &mut RunState, cmd: RunStateCmd) {
    match cmd {
        // Intercepted by `handle_cmd` (acked there); a stray one
        // reaching the state-mutation fold is a harmless no-op.
        RunStateCmd::Barrier(_) => {}
        // Handled in `handle_cmd` (it owns the writer); no snapshot state.
        RunStateCmd::OpenTranscript(_) => {}
        RunStateCmd::Sysmon(sample) => {
            state.sysmon = Some(sample);
        }
        RunStateCmd::InstallTree(tree) => {
            state.install_tree(tree);
        }
        RunStateCmd::PhaseStarting {
            exec_id,
            scene_node_id,
            name,
            labels,
            op_templates,
            total_cycles: _,
            concurrency,
        } => {
            state.set_phase_running(scene_node_id, &name, &labels, op_templates);
            let key = crate::state::ActivePhaseId::new(exec_id, name.clone(), labels.clone());
            // Sparkline capacity = bar width used by
            // latency_detail_lines so the throughput row aligns
            // with the latency rows.
            let throughput_summary = Arc::new(BinomialSummary::new(60));
            // 1 s half-life: short enough to track real
            // throughput changes, long enough to stop the raw
            // value from flickering between frames.
            let rate_ewma = Arc::new(Ewma::new(Duration::from_secs(1)));
            let latency_peak_5s = Arc::new(PeakTracker::max(Duration::from_secs(5)));
            let latency_peak_10s = Arc::new(PeakTracker::max(Duration::from_secs(10)));
            let session_started = state.elapsed_secs();
            state.active_phases.insert(
                key,
                ActivePhase {
                    name,
                    labels,
                    cursor_name: "?".into(),
                    cursor_extent: 0,
                    // Unknown until the first PhaseProgressUpdate carries it.
                    daemon: false,
                    rows_consumed: 0,
                    rows_total: 0,
                    fibers: concurrency,
                    started_at: Instant::now(),
                    session_started,
                    ops_started: 0,
                    ops_finished: 0,
                    ops_ok: 0,
                    skips: 0,
                    errors: 0,
                    retries: 0,
                    ops_per_sec: 0.0,
                    adapter_counters: Vec::new(),
                    rows_per_batch: 0.0,
                    relevancy: Vec::new(),
                    throughput_summary,
                    rate_ewma,
                    latency_peak_5s,
                    latency_peak_10s,
                    render: None,
                },
            );
            // Sparklines reset on every phase boundary so a
            // short ann_query phase doesn't show several seconds
            // of rampup throughput instead of its own.
            state.ops_history.clear();
            state.rows_history.clear();
            state.rows_sparkline_label = None;
        }
        RunStateCmd::PhaseCompleted {
            exec_id,
            scene_node_id,
            name,
            labels,
            duration_secs,
        } => {
            let key = crate::state::ActivePhaseId::new(exec_id, name.clone(), labels.clone());
            let min_ns = state.min_nanos;
            let p50_ns = state.p50_nanos;
            let p99_ns = state.p99_nanos;
            let max_ns = state.max_nanos;
            let summary = state
                .active_phases
                .get(&key)
                .map(|a| PhaseSummary {
                    ops_finished: a.ops_finished,
                    ops_ok: a.ops_ok,
                    skips: a.skips,
                    ops_started: a.ops_started,
                    errors: a.errors,
                    retries: a.retries,
                    fibers: a.fibers,
                    ops_per_sec: a.ops_per_sec,
                    min_nanos: min_ns,
                    p50_nanos: p50_ns,
                    p99_nanos: p99_ns,
                    max_nanos: max_ns,
                    cursor_name: a.cursor_name.clone(),
                    cursor_extent: a.cursor_extent,
                    adapter_counters: a.adapter_counters.clone(),
                    rows_per_batch: a.rows_per_batch,
                    cursors: std::iter::once((a.cursor_name.clone(), a.ops_finished))
                        .chain(a.adapter_counters.iter().map(|(n, t, _)| (n.clone(), *t)))
                        .collect(),
                    relevancy: a.relevancy.clone(),
                    // Freeze the sparkline as a durable artifact —
                    // the live Arc<BinomialSummary> is dropped with
                    // the ActivePhase below.
                    throughput_samples: a.throughput_summary.snapshot(),
                })
                .unwrap_or_default();
            state.set_phase_completed(scene_node_id, &name, &labels, duration_secs, summary);
            state.active_phases.remove(&key);
        }
        RunStateCmd::PhaseFailed {
            exec_id,
            scene_node_id,
            name,
            labels,
            error,
        } => {
            state.set_phase_failed(scene_node_id, &name, &labels, &error);
            state
                .active_phases
                .remove(&crate::state::ActivePhaseId::new(exec_id, name, labels));
        }
        RunStateCmd::OpStarting { parent, op_name } => {
            state.op_starting(parent, &op_name);
        }
        RunStateCmd::OpMeasure {
            parent,
            op_name,
            text,
        } => {
            state.op_measure(parent, &op_name, &text);
        }
        RunStateCmd::OpCompleted {
            parent,
            op_name,
            duration_secs,
        } => {
            state.op_completed(parent, &op_name, duration_secs);
        }
        RunStateCmd::OpFailed {
            parent,
            op_name,
            error,
        } => {
            state.op_failed(parent, &op_name, &error);
        }
        RunStateCmd::PhaseProgress(update) => {
            if let Some(active) =
                state.active_phase_mut(update.exec_id, &update.name, &update.labels)
            {
                active.cursor_name = update.cursor_name.clone();
                active.cursor_extent = update.cursor_extent;
                active.daemon = update.daemon;
                active.rows_consumed = update.rows_consumed;
                active.rows_total = update.rows_total;
                active.fibers = update.fibers;
                active.ops_started = update.ops_started;
                active.ops_finished = update.ops_finished;
                active.ops_ok = update.ops_ok;
                active.skips = update.skips;
                active.errors = update.errors;
                active.retries = update.retries;
                active.ops_per_sec = update.ops_per_sec;
                active.adapter_counters = update
                    .adapter_counters
                    .iter()
                    .map(|(n, t, r)| (n.clone(), *t, *r))
                    .collect();
                active.rows_per_batch = update.rows_per_batch;
                active.relevancy = update
                    .relevancy
                    .iter()
                    .map(|r| {
                        (
                            r.name.clone(),
                            r.window_mean,
                            r.total_mean,
                            r.total_count,
                            r.window_len,
                        )
                    })
                    .collect();
                active.throughput_summary.record(update.ops_per_sec);
                active.rate_ewma.record_now(update.ops_per_sec);
            }
        }
        RunStateCmd::AttachPhaseRender(handle) => {
            // Attach to the live phase slot keyed by (exec_id, name,
            // labels). If `phase_starting` hasn't been processed yet, or
            // the phase already completed, the slot is absent and the
            // handle is dropped — the consumer simply renders no status
            // for that (now-gone) phase, which is correct.
            let key = crate::state::ActivePhaseId::new(
                handle.exec_id,
                handle.name.as_str(),
                handle.labels.as_str(),
            );
            if let Some(active) = state.active_phases.get_mut(&key) {
                active.render = Some(handle);
            }
        }
        RunStateCmd::RunFinished => {
            state.finished = true;
        }
        RunStateCmd::Log { .. } => {
            // Routed to `handle_cmd` before reaching this match (it
            // feeds both the bounded ring and the durable scrollback
            // stream); included here only to keep the match
            // exhaustive.
            unreachable!("Log is handled in handle_cmd before apply")
        }
        RunStateCmd::LatencyFrame {
            min,
            p50,
            p90,
            p99,
            p999,
            max,
        } => {
            state.min_nanos = min;
            state.p50_nanos = p50;
            state.p90_nanos = p90;
            state.p99_nanos = p99;
            state.p999_nanos = p999;
            state.max_nanos = max;

            const HISTORY_CAP: usize = 300; // 5 min at 1 Hz
            state.min_history.push(min);
            state.p50_history.push(p50);
            state.p90_history.push(p90);
            state.p99_history.push(p99);
            state.p999_history.push(p999);
            state.max_history.push(max);
            let trim = |h: &mut Vec<u64>| {
                if h.len() > HISTORY_CAP {
                    h.remove(0);
                }
            };
            trim(&mut state.min_history);
            trim(&mut state.p50_history);
            trim(&mut state.p90_history);
            trim(&mut state.p99_history);
            trim(&mut state.p999_history);
            trim(&mut state.max_history);

            // Each active phase's peak trackers see this frame's
            // max latency. Frames are session-labeled today, so
            // every active phase observes the same max — fine
            // for single-phase scenarios; multi-phase will need
            // per-phase frame demux.
            let now = Instant::now();
            for active in state.active_phases.values() {
                active.latency_peak_5s.record(max, now);
                active.latency_peak_10s.record(max, now);
            }
        }
        RunStateCmd::SparklineSamples {
            ops,
            rows,
            rows_label,
        } => {
            if let Some(o) = ops {
                state.push_ops_sample(o);
            }
            if let Some(r) = rows {
                state.push_rows_sample(r);
            }
            state.rows_sparkline_label = rows_label;
        }
        RunStateCmd::SetMeta { profiler, limit } => {
            if let Some(p) = profiler {
                state.profiler = p;
            }
            if let Some(l) = limit {
                state.limit = l;
            }
        }
        RunStateCmd::FrameAck(_) => {
            // Routed to `handle_cmd` before reaching this match;
            // included here only to keep the match exhaustive.
            unreachable!("FrameAck is handled in handle_cmd before apply")
        }
    }
}

#[cfg(test)]
mod scrollback_stream_tests {
    use super::*;
    use crate::state::{LogSeverity, RunState};

    /// The durable scrollback stream carries EVERY log line, in order,
    /// exactly once — even when far more than the bounded ring's
    /// capacity (200) arrive before anything drains. This is the
    /// no-drop guarantee: the old renderer read log deltas off the
    /// bounded `RunState.log_messages` ring, which evicts its oldest
    /// entries past 200, so a renderer lagging the actor by >200 lines
    /// lost the un-emitted oldest ones. The stream is unbounded, so it
    /// cannot evict.
    #[test]
    fn stream_delivers_all_lines_past_ring_capacity() {
        let (handle, join) = spawn_run_state_actor(RunState::new("t", "default", "stdout"));

        // Claim the stream first (as a real terminal-mode sink does),
        // which flips the actor's feed latch on so every subsequent
        // line is buffered.
        let rx = handle.take_log_stream();

        // 500 > the 200-line ring capacity, all queued before a single
        // drain — the exact overflow condition that dropped lines.
        const N: u64 = 500;
        for i in 0..N {
            handle.send(RunStateCmd::Log {
                severity: LogSeverity::Info,
                tag: crate::state::EventTag::default(),
                message: format!("line-{i}"),
            });
        }

        // Drain the durable stream, waiting for the actor thread to
        // finish enqueuing all N.
        let mut got: Vec<(u64, String)> = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (got.len() as u64) < N && std::time::Instant::now() < deadline {
            match rx.try_next() {
                Some(line) => got.push((line.seq, line.entry.message)),
                None => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }

        assert_eq!(
            got.len() as u64,
            N,
            "every log line must be delivered — none dropped despite \
             {N} lines arriving before any drain (ring cap is 200)"
        );
        // In order, contiguous seq 1..=N, messages line-0..line-(N-1).
        for (i, (seq, msg)) in got.iter().enumerate() {
            assert_eq!(*seq, i as u64 + 1, "seq monotonic + contiguous");
            assert_eq!(msg, &format!("line-{i}"), "delivered in order");
        }

        drop(rx);
        drop(handle);
        let _ = join.join();
    }

    /// The actor does NOT feed the stream until a terminal-mode sink
    /// claims it — so modes with no `LogOnlySink` (`tui=on` full TUI,
    /// `tui=off` piped/CI, `tui=formatted`) never accumulate an
    /// unbounded backlog on a stream nobody drains. Lines pushed before
    /// the claim still reach the ring + `session.log`; they just aren't
    /// buffered on the stream.
    #[test]
    fn stream_not_fed_until_a_sink_claims_it() {
        let (handle, join) = spawn_run_state_actor(RunState::new("t", "default", "stdout"));

        // No sink has claimed the stream yet.
        for i in 0..300u64 {
            handle.send(RunStateCmd::Log {
                severity: LogSeverity::Info,
                tag: crate::state::EventTag::default(),
                message: format!("unfed-{i}"),
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(60));

        // Claim now: nothing that predates the claim is buffered.
        let rx = handle.take_log_stream();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(
            rx.try_next().is_none(),
            "an unclaimed stream must not buffer a backlog — no leak when \
             no LogOnlySink is draining"
        );

        // Lines sent AFTER the claim ARE delivered.
        handle.send(RunStateCmd::Log {
            severity: LogSeverity::Info,
            tag: crate::state::EventTag::default(),
            message: "after-claim".into(),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got = None;
        while got.is_none() && std::time::Instant::now() < deadline {
            match rx.try_next() {
                Some(line) => got = Some(line.entry.message),
                None => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        assert_eq!(
            got.as_deref(),
            Some("after-claim"),
            "lines sent after the claim must flow through the stream"
        );

        drop(rx);
        drop(handle);
        let _ = join.join();
    }

    /// The receiver survives a sink teardown: the `ScrollbackReceiver`
    /// guard restores it to the shared cell on drop, so a subsequent
    /// `take_log_stream` (the next terminal-mode sink after a Ctrl-T
    /// swap) resumes the SAME stream and sees the lines that buffered
    /// while no sink was draining.
    #[test]
    fn stream_receiver_survives_sink_handoff() {
        let (handle, join) = spawn_run_state_actor(RunState::new("t", "default", "stdout"));

        // First "sink" takes the stream, then tears down (guard drops).
        {
            let _rx = handle.take_log_stream();
        }
        // Lines buffered while no sink is draining.
        for i in 0..3u64 {
            handle.send(RunStateCmd::Log {
                severity: LogSeverity::Info,
                tag: crate::state::EventTag::default(),
                message: format!("swap-{i}"),
            });
        }
        // Second "sink" resumes the same stream and drains the backlog.
        let rx = handle.take_log_stream();
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.len() < 3 && std::time::Instant::now() < deadline {
            match rx.try_next() {
                Some(line) => got.push(line.entry.message),
                None => std::thread::sleep(std::time::Duration::from_millis(2)),
            }
        }
        assert_eq!(
            got,
            vec!["swap-0", "swap-1", "swap-2"],
            "the stream receiver must survive the sink handoff and \
             flush the backlog buffered while no sink was draining"
        );

        drop(rx);
        drop(handle);
        let _ = join.join();
    }
}

#[cfg(test)]
mod transcript_tests {
    use super::Transcript;

    /// Lines logged BEFORE the session directory exists — the banner naming
    /// the session, the workload, the resolved params — are exactly the ones
    /// worth keeping, so they buffer and flush in order rather than being
    /// dropped for arriving early.
    #[test]
    fn buffers_until_opened_then_flushes_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "nmbrs-transcript-{}-{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("transcript.log");

        let mut t = Transcript::default();
        for msg in ["first", "second"] {
            t.record(&super::LogLine {
                seq: 0,
                entry: crate::state::LogEntry {
                    severity: crate::state::LogSeverity::Info,
                    message: msg.into(),
                    tag: crate::state::EventTag::default(),
                    at: std::time::SystemTime::now(),
                },
                margin_body: "m".into(),
                detail_gutter: None,
            });
        }
        assert!(!path.exists(), "nothing is written before open");
        t.open(&path);
        t.record(&super::LogLine {
            seq: 2,
            entry: crate::state::LogEntry {
                severity: crate::state::LogSeverity::Info,
                message: "third".into(),
                tag: crate::state::EventTag::default(),
                at: std::time::SystemTime::now(),
            },
            margin_body: "m".into(),
            detail_gutter: None,
        });

        let body = std::fs::read_to_string(&path).unwrap();
        let msgs: Vec<&str> = body
            .lines()
            .map(|l| l.rsplit('│').next().unwrap().trim())
            .collect();
        assert_eq!(
            msgs,
            vec!["first", "second", "third"],
            "buffered lines flush ahead of live ones, in order:\n{body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The transcript carries the GUTTER, which is the whole reason it is not
    /// just session.log: the margin is what ties a line to the phase and
    /// clock position the operator saw it at.
    #[test]
    fn each_line_carries_its_margin_and_divider() {
        let rendered = Transcript::format(&super::LogLine {
            seq: 0,
            entry: crate::state::LogEntry {
                severity: crate::state::LogSeverity::Info,
                message: "phase done".into(),
                tag: crate::state::EventTag::default(),
                at: std::time::SystemTime::now(),
            },
            margin_body: "◷ 12.1s [1/4]".into(),
            detail_gutter: None,
        });
        assert!(rendered.contains("◷ 12.1s [1/4]"), "{rendered}");
        assert!(
            rendered.contains('│'),
            "divider separates gutter from message: {rendered}"
        );
        assert!(rendered.trim_end().ends_with("phase done"), "{rendered}");
    }
}

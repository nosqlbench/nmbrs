// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! SRD-87 Output Channel — the single typed terminal-output conduit.
//!
//! Push 1 lands the trait, the **op-output bucket**, and a test-capture
//! impl. Every adapter op line submits through
//! [`OutputChannel::op_output`]; the installed impl alone decides where
//! the bytes land. The remaining buckets (`log` / `status` / `raster`)
//! and the consolidation of `RunObserver` + `DisplaySink` arrive in
//! later pushes (SRD-87 §11/§13).
//!
//! The op-output channel is **selected once per run** from the run's
//! context (`silent_console`, `is_tty`) — replacing the prior
//! `console_reserved_for_adapter` global flag that `op_output` consulted
//! inline. A console-owning adapter (`silent_console`) and a piped run
//! both own a raw stdout surface; an interactive dashboard routes op
//! output through the live display so it composites without the raw-mode
//! staircase. The console-owning stdout adapter printing nothing on a
//! TTY (the SRD-87 §2 defect) falls out as a consequence: under the raw
//! impl the op line is the adapter's, written to the surface it owns,
//! never suppressed alongside the diagnostics.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwapOption;

use crate::observer::{LogLevel, colorize_log_line};

/// The single conduit for the one user terminal. Exactly one impl is
/// installed per run via [`install`]; producers submit to a bucket and
/// only the impl touches an fd. Every method is non-blocking.
///
/// Push 1 defines the **op-output** bucket only; `log` / `status` /
/// `raster` land in later pushes (SRD-87 §4).
pub trait OutputChannel: Send + Sync {
    /// op-output bucket: one adapter-rendered op line. Reaches the
    /// terminal/file the channel owns — never around it.
    fn op_output(&self, line: &str);

    // SRD-100 P2 — the status bucket is removed. The live phase status is
    // folded at the display consumer from the `active_phases` snapshot
    // (`nmbrs_tui::status_fold`), not submitted as a pre-rendered string to
    // a single channel slot. No keyed submission survives, so per SRD-100
    // §6 the bucket + `RunObserver::set_status_line` are deleted rather than
    // re-keyed.

    /// log bucket: project one diagnostic line to the **live terminal**
    /// (SRD-87 §5). This is the *output* half only — the durable
    /// `session.log` write and the fold-ring append are L1 intake, done by
    /// `observer::log_categorized` before this is ever called, and the
    /// `sink_active`/`min_level` gate (whether a line reaches the live
    /// surface at all) stays with the observer. The default writes the
    /// colorized line to stderr — the fd the channel owns. A sink-active
    /// interactive run never reaches here (the sink renders from the ring);
    /// a console-owning run never reaches here (suppressed upstream); piped
    /// and bootstrap runs land here.
    fn log(&self, level: LogLevel, message: &str) {
        eprintln!("{}", colorize_log_line(level, message));
    }

    /// raster bucket: a fully-rendered, self-contained terminal frame
    /// (braille / ANSI cells) — e.g. the plotter's canvas (SRD-87 §5). The
    /// producer owns layout (cursor controls, line endings); the channel
    /// owns the fd. The default writes the frame bytes raw to the stdout the
    /// channel owns — correct for a console-owning adapter (the plotter owns
    /// the screen) and for a piped plot redirected to a file.
    fn raster(&self, frame: &str) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
    }
}

/// op-output routed **through the live display**: the active
/// `LogOnlySink` / `TuiSink` composites the line into its scrollback,
/// avoiding the raw-mode staircase. Selected for an interactive
/// dashboard that is not console-owning (`tui=terminal` / `on`).
pub struct DisplayRoutedChannel;

impl OutputChannel for DisplayRoutedChannel {
    fn op_output(&self, line: &str) {
        crate::observer::log(crate::observer::LogLevel::Info, line);
    }
}

/// op-output written **raw to the stdout the producer owns**, plus a
/// durable `session.log` capture. Selected for a console-owning adapter
/// on a TTY (it owns the screen) AND for a piped/redirected run (so
/// `nmbrs run | grep` and `> file` keep working). This is the impl that
/// makes a console-owning stdout adapter actually print: the line is the
/// adapter's, written to the surface it owns, never suppressed with the
/// diagnostics.
pub struct RawStdoutChannel;

impl OutputChannel for RawStdoutChannel {
    fn op_output(&self, line: &str) {
        crate::observer::op_output_raw(line);
    }
}

/// Test-capture impl: records every op-output line for assertions and
/// writes no fd. The SRD-87 §12 surface-agreement / no-bypass tests
/// install one of these and inspect [`Self::op_lines`].
#[derive(Default, Clone)]
pub struct CaptureChannel {
    op_lines: Arc<Mutex<Vec<String>>>,
    log_lines: Arc<Mutex<Vec<(LogLevel, String)>>>,
    raster_frames: Arc<Mutex<Vec<String>>>,
}

impl CaptureChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every op-output line submitted so far, in submission order.
    pub fn op_lines(&self) -> Vec<String> {
        self.op_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every log-bucket line submitted so far, in submission order.
    pub fn log_lines(&self) -> Vec<(LogLevel, String)> {
        self.log_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Every raster-bucket frame submitted so far, in submission order.
    pub fn raster_frames(&self) -> Vec<String> {
        self.raster_frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl OutputChannel for CaptureChannel {
    fn op_output(&self, line: &str) {
        self.op_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line.to_string());
    }

    fn log(&self, level: LogLevel, message: &str) {
        self.log_lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((level, message.to_string()));
    }

    fn raster(&self, frame: &str) {
        self.raster_frames
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(frame.to_string());
    }
}

/// Which op-output impl a run's context selects (SRD-87 §10). Factored
/// out so the selection is unit-testable without standing up a surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    /// Raw to the owned stdout + `session.log` — console-owning adapter
    /// or a pipe.
    RawStdout,
    /// Through the live display — an interactive, non-console-owning
    /// dashboard.
    DisplayRouted,
}

/// The op-output routing a run's context implies. A console-owning
/// adapter (`silent_console`) owns a raw surface; a piped run
/// (`!is_tty`) owns the pipe; only an interactive dashboard routes
/// through the display.
pub fn select_kind(silent_console: bool, is_tty: bool) -> ChannelKind {
    if is_tty && !silent_console {
        ChannelKind::DisplayRouted
    } else {
        ChannelKind::RawStdout
    }
}

/// Build the op-output channel a run's context selects.
pub fn select(silent_console: bool, is_tty: bool) -> Arc<dyn OutputChannel> {
    match select_kind(silent_console, is_tty) {
        ChannelKind::DisplayRouted => Arc::new(DisplayRoutedChannel),
        ChannelKind::RawStdout => Arc::new(RawStdoutChannel),
    }
}

/// Sized newtype so the process-global can hold the unsized
/// `Arc<dyn OutputChannel>` inside an `ArcSwapOption` (which stores a
/// `Sized` `Arc<Holder>`).
struct Holder(Arc<dyn OutputChannel>);

/// The installed channel for this process. `None` until a run installs
/// one; bootstrap and unit tests with no run see `None` and the
/// `op_output` free fn falls back to the raw path (SRD-87 §6 carve-out).
/// `ArcSwapOption` so a run installs lock-free and tests can swap/reset.
static CHANNEL: ArcSwapOption<Holder> = ArcSwapOption::const_empty();

/// Install the op-output channel for this run (SRD-87 §10 — chosen once
/// per context). Replaces any prior install.
pub fn install(channel: Arc<dyn OutputChannel>) {
    CHANNEL.store(Some(Arc::new(Holder(channel))));
}

/// Clear the installed channel (end of run; tests).
pub fn clear() {
    CHANNEL.store(None);
}

/// The installed channel, if any. SRD-88 — **task-local-first**: a
/// concurrent in-process execution that scoped its own channel
/// (`ExecutionContext.channel`) resolves to it; everything else (single-run,
/// bootstrap, tests) falls back to the process-global `CHANNEL` (axiom A1, a
/// true no-op until a context scopes one).
pub fn installed() -> Option<Arc<dyn OutputChannel>> {
    if let Some(ch) = crate::execution_context::current_channel() {
        return Some(ch);
    }
    CHANNEL.load_full().map(|h| h.0.clone())
}

/// Project a diagnostic line to the **log bucket** (the live terminal).
/// Called by `observer::log_categorized`/the observer impls *after* the
/// L1 intake (session.log + fold ring) and *after* the
/// `sink_active`/`min_level` gate has decided the line should reach the
/// live surface (SRD-87 §5). Routes to the installed channel — the sole
/// fd owner — falling back to a direct colorized stderr write before any
/// channel is installed (bootstrap), so behavior is unchanged.
pub fn log_to_surface(level: LogLevel, message: &str) {
    if let Some(ch) = installed() {
        ch.log(level, message);
    } else {
        eprintln!("{}", colorize_log_line(level, message));
    }
}

/// Submit a **raster** frame (a self-contained, pre-rendered terminal
/// canvas) to the channel. Producers — the plotter — call this instead of
/// `print!`-ing the frame themselves (SRD-87 §5). Routes to the installed
/// channel (the fd owner), falling back to a raw stdout write before any
/// channel is installed (bootstrap / unit tests), so behavior is unchanged.
pub fn raster(frame: &str) {
    if let Some(ch) = installed() {
        ch.raster(frame);
    } else {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(frame.as_bytes());
        let _ = out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_channel_records_op_output_in_order() {
        let ch = CaptureChannel::new();
        ch.op_output("id-0");
        ch.op_output("id-1");
        assert_eq!(ch.op_lines(), vec!["id-0".to_string(), "id-1".to_string()]);
    }

    #[test]
    fn capture_channel_records_raster_frames() {
        let ch = CaptureChannel::new();
        ch.raster("frame-a");
        ch.raster("frame-b");
        assert_eq!(
            ch.raster_frames(),
            vec!["frame-a".to_string(), "frame-b".to_string()]
        );
    }

    #[test]
    fn capture_channel_records_log_lines() {
        let ch = CaptureChannel::new();
        ch.log(LogLevel::Info, "started");
        ch.log(LogLevel::Warn, "careful");
        assert_eq!(
            ch.log_lines(),
            vec![
                (LogLevel::Info, "started".to_string()),
                (LogLevel::Warn, "careful".to_string()),
            ]
        );
    }

    #[test]
    fn select_kind_console_owning_and_piped_are_raw() {
        // Console-owning adapter on a TTY: it owns the screen → raw, so
        // its output reaches the surface instead of being suppressed with
        // the diagnostics (the SRD-87 §2 defect).
        assert_eq!(select_kind(true, true), ChannelKind::RawStdout);
        // Piped / redirected: raw, so `nmbrs run | grep` keeps working.
        assert_eq!(select_kind(false, false), ChannelKind::RawStdout);
        // Console-owning but not a TTY (e.g. `> file`): still raw.
        assert_eq!(select_kind(true, false), ChannelKind::RawStdout);
        // Interactive dashboard, not console-owning: route through the
        // display so it composites without the raw-mode staircase.
        assert_eq!(select_kind(false, true), ChannelKind::DisplayRouted);
    }
}

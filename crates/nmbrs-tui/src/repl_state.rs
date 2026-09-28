// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! REPL visibility state machine.
//!
//! The interactive REPL — the command prompt at the bottom of
//! the terminal that accepts `set`, `controls`, `metrics …`,
//! and other inspector commands — has three visibility states:
//!
//! - **Hidden** (default): no prompt rendered, no input
//!   accepted, the status block fills the bottom region.
//!   This is the baseline for non-interactive runs.
//! - **Bar**: a single-row prompt at the bottom of the
//!   terminal, accepting commands. The status block remains
//!   visible above it.
//! - **Window**: a full-screen REPL — most of the terminal is
//!   turned over to the command interface with command history
//!   visible. The status block is suppressed in this mode.
//!
//! ## Keys
//!
//! - `~`: cycle Hidden ↔ Bar. From Window, returns to Hidden.
//!   Tilde is the universal "toggle off" key — pressing it
//!   from any visible state collapses to Hidden.
//! - `` ` `` (backtick): open or close Window. From Hidden or
//!   Bar, opens to Window; from Window, returns to Hidden.
//!   Picked over the originally-spec'd `Ctrl-~` because
//!   terminals deliver the bare chord more reliably than the
//!   Ctrl-modified one.
//!
//! ## Coordination
//!
//! State lives in a process-global atomic (`AtomicU8`). The
//! key watcher (which sees `~` presses) stamps the state; the
//! LogOnlySink reads it on every render tick to decide whether
//! to draw the prompt. No locks — every reader picks up the
//! latest write within one render frame.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Discriminant values stored in the global atomic. `u8` so the
/// load/store is single-instruction on every platform we care
/// about, and so the value can be mapped directly to the enum
/// without unsafe transmutes.
const STATE_HIDDEN: u8 = 0;
const STATE_BAR: u8 = 1;
const STATE_WINDOW: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplVisibility {
    /// No REPL surface drawn. The default for non-interactive
    /// runs and the implicit state at session start until the
    /// operator hits `~`.
    Hidden,
    /// Single-row prompt at the bottom of the terminal. Status
    /// block continues to render above.
    Bar,
    /// Full-screen REPL — most of the terminal is the REPL
    /// surface. Status block is suppressed (or compressed into
    /// a single header line, TBD when the renderer lands).
    Window,
}

impl ReplVisibility {
    fn to_u8(self) -> u8 {
        match self {
            Self::Hidden => STATE_HIDDEN,
            Self::Bar => STATE_BAR,
            Self::Window => STATE_WINDOW,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            STATE_BAR => Self::Bar,
            STATE_WINDOW => Self::Window,
            _ => Self::Hidden,
        }
    }
}

static STATE: AtomicU8 = AtomicU8::new(STATE_HIDDEN);

/// Wall-clock nanos of the most recent toggle (either kind).
/// Auto-repeat coming through stdin in raw mode sends a stream
/// of keystrokes while a key is held; without this debounce
/// the operator's single press would strobe Hidden ↔ Bar at
/// 30 Hz. The debounce window is held long enough to absorb
/// the auto-repeat cadence but short enough that a deliberate
/// second tap registers — see `TOGGLE_DEBOUNCE`.
static LAST_TOGGLE_NS: AtomicU64 = AtomicU64::new(0);

/// Minimum interval between honored toggles. Tuned to swallow
/// the typical auto-repeat cadence (30 Hz ≈ 33 ms inter-arrival)
/// plus headroom, while still feeling responsive to a second
/// deliberate press.
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

fn now_nanos() -> u64 {
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    let origin = *ORIGIN.get_or_init(Instant::now);
    origin.elapsed().as_nanos() as u64
}

fn debounce_ok() -> bool {
    let now = now_nanos();
    let last = LAST_TOGGLE_NS.load(Ordering::Acquire);
    if last != 0 && now.saturating_sub(last) < TOGGLE_DEBOUNCE.as_nanos() as u64 {
        return false;
    }
    LAST_TOGGLE_NS.store(now, Ordering::Release);
    true
}

/// Current REPL visibility. Read on every render tick by the
/// LogOnlySink to decide whether to draw the prompt and how
/// to lay out the bottom region.
pub fn current() -> ReplVisibility {
    ReplVisibility::from_u8(STATE.load(Ordering::Acquire))
}

/// Apply the `~` keystroke transition:
///
/// - Hidden → Bar
/// - Bar → Hidden
/// - Window → Hidden (tilde collapses anything visible)
///
/// Idempotent against rapid auto-repeat — the operator
/// holding `~` shouldn't strobe between states. Returns the
/// new state.
pub fn toggle_bar() -> ReplVisibility {
    if !debounce_ok() {
        return current();
    }
    let next = match current() {
        ReplVisibility::Hidden => ReplVisibility::Bar,
        ReplVisibility::Bar | ReplVisibility::Window => ReplVisibility::Hidden,
    };
    STATE.store(next.to_u8(), Ordering::Release);
    next
}

/// Apply the `Ctrl-~` keystroke transition:
///
/// - Hidden, Bar → Window (opens the full-screen REPL)
/// - Window → Hidden (toggle off)
pub fn toggle_window() -> ReplVisibility {
    if !debounce_ok() {
        return current();
    }
    let next = match current() {
        ReplVisibility::Hidden | ReplVisibility::Bar => ReplVisibility::Window,
        ReplVisibility::Window => ReplVisibility::Hidden,
    };
    STATE.store(next.to_u8(), Ordering::Release);
    next
}

/// Capacity of the transcript ring. Caps the in-memory cost of
/// keeping a session-long REPL history; 200 entries is enough
/// to scroll through ~3 screens of typical responses while
/// being a flat ~50 KB at the upper bound.
const TRANSCRIPT_CAPACITY: usize = 200;

/// Per-line REPL transcript. Lines are pre-rendered (the
/// command echo `> cmd` is one entry, each response line is
/// another) so the Window-mode renderer can pick the last N
/// entries directly without re-splitting per tick.
///
/// Mutex (not RwLock): writes (dispatch push) and reads (every
/// render frame) are infrequent relative to the lock-acquire
/// cost; the simpler primitive is the right pick.
static TRANSCRIPT: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

/// Push the operator's command echo + the dispatcher's response
/// onto the transcript ring. Each line lands as its own entry
/// so the renderer's "last N rows" slice maps directly to
/// display rows. Older entries fall off the front when the
/// ring is full.
pub fn push_transcript(command: &str, response: &str) {
    let Ok(mut t) = TRANSCRIPT.lock() else {
        return;
    };
    t.push_back(format!("> {command}"));
    for line in response.split('\n') {
        t.push_back(line.to_string());
    }
    while t.len() > TRANSCRIPT_CAPACITY {
        t.pop_front();
    }
}

/// Push a single pre-rendered line to the transcript ring — e.g.
/// a completion-suggestion row — with no `> ` command echo.
/// Same ring and capacity discipline as [`push_transcript`]. Used
/// to keep ephemeral console output inside the frame instead of
/// scrolling it through the terminal.
pub fn push_transcript_line(line: &str) {
    let Ok(mut t) = TRANSCRIPT.lock() else {
        return;
    };
    t.push_back(line.to_string());
    while t.len() > TRANSCRIPT_CAPACITY {
        t.pop_front();
    }
}

/// Current number of lines in the transcript ring. Cheap change
/// detector for the console renderer — when it differs from the
/// last paint, the console (on the alternate screen) repaints to
/// show new output, regardless of which source pushed it.
pub fn transcript_len() -> usize {
    TRANSCRIPT.lock().map(|t| t.len()).unwrap_or(0)
}

/// Snapshot the most recent `n` transcript lines in display
/// order (oldest first). Returns `<= n` strings — fewer when
/// the transcript is shorter than `n`.
pub fn transcript_tail(n: usize) -> Vec<String> {
    let Ok(t) = TRANSCRIPT.lock() else {
        return Vec::new();
    };
    let start = t.len().saturating_sub(n);
    t.iter().skip(start).cloned().collect()
}

/// Test-only reset to the default state. Avoids cross-test
/// state leakage where one test's `toggle_*` call affects the
/// next test's view of `current()`. Also clears the
/// transcript so transcript-targeted tests see a clean ring.
#[cfg(test)]
pub fn reset() {
    STATE.store(STATE_HIDDEN, Ordering::Release);
    LAST_TOGGLE_NS.store(0, Ordering::Release);
    if let Ok(mut t) = TRANSCRIPT.lock() {
        t.clear();
    }
}

/// Test-only: apply a toggle bypassing the auto-repeat
/// debounce so tests can exercise consecutive toggles without
/// sleeping. Production callers always use the debounced
/// `toggle_*` entry points.
#[cfg(test)]
pub fn toggle_bar_undebounced() -> ReplVisibility {
    LAST_TOGGLE_NS.store(0, Ordering::Release);
    toggle_bar()
}

#[cfg(test)]
pub fn toggle_window_undebounced() -> ReplVisibility {
    LAST_TOGGLE_NS.store(0, Ordering::Release);
    toggle_window()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// All tests touching the global STATE serialize through
    /// this lock — otherwise parallel test execution causes
    /// transitions to interfere across cases.
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tilde_cycles_hidden_to_bar_and_back() {
        let _g = LOCK.lock().unwrap();
        reset();
        assert_eq!(current(), ReplVisibility::Hidden);
        assert_eq!(toggle_bar_undebounced(), ReplVisibility::Bar);
        assert_eq!(current(), ReplVisibility::Bar);
        assert_eq!(toggle_bar_undebounced(), ReplVisibility::Hidden);
        assert_eq!(current(), ReplVisibility::Hidden);
    }

    #[test]
    fn ctrl_tilde_opens_and_closes_window() {
        let _g = LOCK.lock().unwrap();
        reset();
        assert_eq!(toggle_window_undebounced(), ReplVisibility::Window);
        assert_eq!(current(), ReplVisibility::Window);
        assert_eq!(toggle_window_undebounced(), ReplVisibility::Hidden);
    }

    #[test]
    fn ctrl_tilde_from_bar_promotes_to_window() {
        let _g = LOCK.lock().unwrap();
        reset();
        toggle_bar_undebounced();
        assert_eq!(current(), ReplVisibility::Bar);
        assert_eq!(toggle_window_undebounced(), ReplVisibility::Window);
    }

    #[test]
    fn tilde_from_window_collapses_to_hidden() {
        let _g = LOCK.lock().unwrap();
        reset();
        toggle_window_undebounced();
        assert_eq!(current(), ReplVisibility::Window);
        // "tilde also closes it completely" — from any visible
        // state, `~` collapses to Hidden.
        assert_eq!(toggle_bar_undebounced(), ReplVisibility::Hidden);
    }

    /// The debounce swallows the second toggle when it lands
    /// inside the auto-repeat window. The state stays where the
    /// first toggle left it. This is the auto-repeat-hold
    /// safeguard: a single deliberate press of `~` doesn't
    /// strobe through Hidden ↔ Bar at the 30 Hz repeat rate.
    #[test]
    fn debounce_swallows_rapid_toggles() {
        let _g = LOCK.lock().unwrap();
        reset();
        // First call lands.
        assert_eq!(toggle_bar(), ReplVisibility::Bar);
        // Second call within 250ms is swallowed — state stays Bar.
        assert_eq!(toggle_bar(), ReplVisibility::Bar);
        assert_eq!(toggle_bar(), ReplVisibility::Bar);
    }

    /// Transcript stores command + response as separate lines
    /// (per-line entries are what the Window-mode renderer
    /// directly maps to display rows). `transcript_tail` returns
    /// the most-recent slice in chronological order.
    #[test]
    fn transcript_push_and_tail_round_trip() {
        let _g = LOCK.lock().unwrap();
        reset();
        push_transcript("set foo 1", "OK");
        push_transcript("controls", "foo=1\nbar=0");
        let tail = transcript_tail(10);
        assert_eq!(
            tail,
            vec![
                "> set foo 1".to_string(),
                "OK".to_string(),
                "> controls".to_string(),
                "foo=1".to_string(),
                "bar=0".to_string(),
            ]
        );
    }

    /// `push_transcript_line` appends a single raw line (no `> `
    /// echo prefix) — the path console completions / contained
    /// output take into the frame.
    #[test]
    fn push_transcript_line_appends_raw() {
        let _g = LOCK.lock().unwrap();
        reset();
        push_transcript("cmd", "resp");
        push_transcript_line("a  b  c"); // e.g. a completion row
        let tail = transcript_tail(10);
        assert_eq!(
            tail,
            vec![
                "> cmd".to_string(),
                "resp".to_string(),
                "a  b  c".to_string(),
            ]
        );
    }

    /// Tail bounded by request — `transcript_tail(2)` on a
    /// 5-line transcript returns the LAST two entries.
    #[test]
    fn transcript_tail_caps_at_requested_count() {
        let _g = LOCK.lock().unwrap();
        reset();
        push_transcript("a", "1");
        push_transcript("b", "2");
        let tail = transcript_tail(2);
        assert_eq!(tail, vec!["> b".to_string(), "2".to_string()]);
    }

    /// Ring overflow drops the oldest entries first.
    #[test]
    fn transcript_ring_drops_oldest_on_overflow() {
        let _g = LOCK.lock().unwrap();
        reset();
        for i in 0..(TRANSCRIPT_CAPACITY + 5) {
            push_transcript(&format!("c{i}"), "");
        }
        let tail = transcript_tail(usize::MAX);
        // Each `push_transcript` adds 2 entries: `> cmd` + ""
        // (the empty response splits into one empty line).
        // The ring keeps the most recent TRANSCRIPT_CAPACITY.
        assert_eq!(tail.len(), TRANSCRIPT_CAPACITY);
        // The very first command's echo (`> c0`) MUST have
        // fallen off; the most recent one (`> c{N+4}`) MUST
        // still be present.
        assert!(
            !tail.iter().any(|l| l == "> c0"),
            "oldest entry MUST be evicted: {tail:?}"
        );
        assert!(
            tail.iter()
                .any(|l| l == &format!("> c{}", TRANSCRIPT_CAPACITY + 4)),
            "newest entry MUST be retained: {tail:?}"
        );
    }
}

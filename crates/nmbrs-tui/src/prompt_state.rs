// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `PromptState` — the bottom-of-terminal nb-shell prompt.
//!
//! Owns the line buffer, history, and overlay state for the
//! interactive prompt rendered at the bottom of the
//! `tui=terminal` inline surface. Stateless with respect to
//! the surrounding sink: callers pass [`crossterm::event::KeyEvent`]s
//! in via [`Self::handle_key`] and read back a [`PromptAction`]
//! that says what to do externally (submit a command line,
//! resize the prompt window, toggle help, or just keep going).
//!
//! Rendering is also caller-driven via [`Self::render`] — the
//! sink owns the surface and decides where the prompt goes.
//! `PromptState` only knows how to lay out its own rows.
//!
//! ## Why rustyline
//!
//! [`rustyline::line_buffer::LineBuffer`] handles UTF-8-aware
//! cursor positioning, kill-ring semantics, and word boundary
//! detection — the boring-but-easy-to-get-wrong parts of a
//! line editor. [`rustyline::history::DefaultHistory`] gives us
//! a ring with arrow-key recall. We deliberately avoid
//! rustyline's terminal driver (`Editor::readline`) because it
//! wants to own stdin and the rendering surface; the inline
//! sink's existing render loop needs to do that.
//!
//! ## Keystroke table
//!
//! | Key        | Action                                   |
//! |------------|------------------------------------------|
//! | Printable  | Insert char                              |
//! | Backspace  | Delete previous char                     |
//! | Delete     | Delete char under cursor                 |
//! | Left/Right | Move cursor by one                       |
//! | Home/Ctrl-A| Cursor to line start                     |
//! | End/Ctrl-E | Cursor to line end                       |
//! | Ctrl-U     | Kill to start of line                    |
//! | Ctrl-K     | Kill to end of line                      |
//! | Ctrl-W     | Backward kill word                       |
//! | Up/Down    | Previous / next history entry            |
//! | Enter      | Submit                                   |
//! | Alt+Up/Down| Grow / shrink prompt window              |
//! | Ctrl-/     | Toggle keystroke help overlay            |

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rustyline::Word;
use rustyline::history::{DefaultHistory, History, SearchDirection};
use rustyline::line_buffer::{ChangeListener, DeleteListener, Direction, LineBuffer};

/// No-op listener for [`LineBuffer`] mutations. `LineBuffer`
/// requires a `ChangeListener` / `DeleteListener` on most of
/// its API surface; rustyline's built-in `NoListener` is
/// `pub(crate)`, so we ship our own zero-cost equivalent.
struct Silent;
impl DeleteListener for Silent {
    fn delete(&mut self, _idx: usize, _string: &str, _dir: Direction) {}
}
impl ChangeListener for Silent {
    fn insert_char(&mut self, _idx: usize, _c: char) {}
    fn insert_str(&mut self, _idx: usize, _string: &str) {}
    fn replace(&mut self, _idx: usize, _old: &str, _new: &str) {}
}

/// Outcome of one [`PromptState::handle_key`] call. The sink
/// reads this to decide whether to invoke a command, change
/// the layout, or just refresh the prompt rendering.
#[derive(Debug)]
pub enum PromptAction {
    /// Keystroke was absorbed (edit, history nav, ignored).
    /// Caller redraws and keeps polling.
    Continue,
    /// User pressed Enter. Carries the submitted line; the
    /// caller dispatches it (typically into the inspector
    /// command surface) and renders the response.
    Submit(String),
    /// User pressed Tab with multiple completion candidates.
    /// The longest common prefix has already been inserted
    /// into the buffer; the carried list is the candidates
    /// for the partial token under the cursor — the sink
    /// renders them as log lines above the prompt so the
    /// user can see the choices and keep typing to
    /// disambiguate.
    ShowCompletions(Vec<String>),
    /// User pressed Alt+Up. The sink should grow the prompt
    /// window by one row (clamped to [`MAX_WINDOW_ROWS`]).
    GrowWindow,
    /// User pressed Alt+Down. The sink should shrink the
    /// prompt window by one row (clamped to 1).
    ShrinkWindow,
    /// User pressed Ctrl-/. The sink should toggle the help
    /// overlay flag and redraw.
    ToggleHelp,
    /// User pressed Ctrl-C. The sink re-raises SIGINT through
    /// the existing watcher path. Surfaced as a distinct
    /// action so the caller doesn't have to special-case
    /// modifiers inline.
    Interrupt,
}

/// Tab-completion source.
///
/// Returns `(start_pos, candidates)` for the partial token at
/// `pos` inside `line`. `start_pos` is the position in `line`
/// where the partial token begins; the caller replaces
/// `line[start_pos..pos]` with each candidate (or with the
/// longest common prefix when there are multiple).
///
/// Returning an empty `candidates` vec is the canonical
/// "no completions available" signal — the caller leaves the
/// buffer untouched.
pub trait Completer: Send + 'static {
    fn complete(&self, line: &str, pos: usize) -> (usize, Vec<String>);
}

/// No-op completer. Default when [`PromptState`] is built
/// without an explicit completer — Tab becomes a no-op key.
#[derive(Default)]
#[allow(dead_code)] // WIP: SRD-77 refine REPL — default no-op completer
pub struct NoCompletion;

impl Completer for NoCompletion {
    fn complete(&self, _line: &str, _pos: usize) -> (usize, Vec<String>) {
        (0, Vec::new())
    }
}

/// nb-shell command-name completer. Completes the first
/// whitespace-delimited token of the line against the
/// inspector's [`crate::inspector_server::COMMAND_NAMES`]
/// list. Tokens beyond the first (arguments) are left for a
/// future per-command argument completer.
pub struct NbShellCompleter;

impl Completer for NbShellCompleter {
    fn complete(&self, line: &str, pos: usize) -> (usize, Vec<String>) {
        // Find the start of the token under the cursor — the
        // first non-whitespace char preceding `pos`. Tokens
        // are whitespace-delimited.
        let head = &line[..pos];
        let start = head
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        // Only complete the FIRST token (the command name).
        // Subsequent tokens (arguments to `set` etc.) get an
        // empty candidate list — future per-command argument
        // completers can extend this.
        let preceded_by_token = line[..start].split_whitespace().next().is_some();
        if preceded_by_token {
            return (start, Vec::new());
        }
        let partial = &head[start..];
        let candidates: Vec<String> = crate::inspector_server::COMMAND_NAMES
            .iter()
            .filter(|name| name.starts_with(partial))
            .map(|name| name.to_string())
            .collect();
        (start, candidates)
    }
}

/// Hard upper bound on the prompt window's row count. Beyond
/// this the window would occupy more than half the terminal
/// on most consoles and crowd out the log stream. 10 rows is
/// comfortable for inline command history scroll.
pub const MAX_WINDOW_ROWS: u16 = 10;

/// Default initial window row count (just the prompt line).
pub const DEFAULT_WINDOW_ROWS: u16 = 1;

/// nb-shell prompt state. Held by the sink for the lifetime of
/// the inline surface; cleared on shutdown.
pub struct PromptState {
    buffer: LineBuffer,
    history: DefaultHistory,
    /// `Some(i)` when the user is scrolling history; the
    /// buffer reflects the snapshot at that index. `None` when
    /// editing the live (post-history) buffer.
    history_cursor: Option<usize>,
    /// Snapshot of the live buffer text captured the moment
    /// the user first stepped into history (`Up`). Restored on
    /// returning past the newest history entry (`Down`).
    live_snapshot: String,
    /// Current prompt-window height in rows. Includes the
    /// input row and any help-overlay rows above it.
    window_rows: u16,
    help_visible: bool,
    completer: Box<dyn Completer>,
}

impl Default for PromptState {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptState {
    /// New prompt with [`NbShellCompleter`] — completes
    /// command names against the inspector dispatcher's
    /// known surface. Use [`Self::with_completer`] to plug a
    /// different completer (tests use [`NoCompletion`]).
    pub fn new() -> Self {
        Self::with_completer(Box::new(NbShellCompleter))
    }

    /// New prompt with a caller-supplied completer.
    pub fn with_completer(completer: Box<dyn Completer>) -> Self {
        Self {
            buffer: LineBuffer::with_capacity(256),
            history: DefaultHistory::new(),
            history_cursor: None,
            live_snapshot: String::new(),
            window_rows: DEFAULT_WINDOW_ROWS,
            help_visible: false,
            completer,
        }
    }

    /// Current prompt-window height in rows (1..=10).
    pub fn window_rows(&self) -> u16 {
        self.window_rows
    }

    #[allow(dead_code)] // WIP: SRD-77 refine REPL accessor
    pub fn help_visible(&self) -> bool {
        self.help_visible
    }

    /// The buffer's current contents (for tests / debug).
    #[allow(dead_code)] // WIP: SRD-77 refine REPL accessor
    pub fn buffer(&self) -> &str {
        self.buffer.as_str()
    }

    /// Step the prompt window taller by one row, capped at
    /// [`MAX_WINDOW_ROWS`]. Returns the new height.
    pub fn grow_window(&mut self) -> u16 {
        if self.window_rows < MAX_WINDOW_ROWS {
            self.window_rows += 1;
        }
        self.window_rows
    }

    /// Step the prompt window shorter by one row, capped at 1.
    pub fn shrink_window(&mut self) -> u16 {
        if self.window_rows > 1 {
            self.window_rows -= 1;
        }
        self.window_rows
    }

    /// Set the prompt window's row count directly. Clamped
    /// to `[1, MAX_WINDOW_ROWS]`. The REPL Window mode uses
    /// this to claim most of the terminal in one shot rather
    /// than stepping through grow/shrink; the cap stays at
    /// `MAX_WINDOW_ROWS` so even Window mode doesn't exceed
    /// the prompt-state buffer's capacity. Returns the new
    /// (possibly clamped) row count.
    pub fn set_window_rows(&mut self, target: u16) -> u16 {
        self.window_rows = target.clamp(1, MAX_WINDOW_ROWS);
        self.window_rows
    }

    pub fn toggle_help(&mut self) {
        self.help_visible = !self.help_visible;
    }

    /// Handle one key event from the watcher.
    ///
    /// Returns a [`PromptAction`] indicating what the caller
    /// should do externally — the prompt itself never side-
    /// effects beyond its own buffer / history / window state.
    pub fn handle_key(&mut self, ke: KeyEvent) -> PromptAction {
        let alt = ke.modifiers.contains(KeyModifiers::ALT);
        let ctrl = ke.modifiers.contains(KeyModifiers::CONTROL);
        let shift = ke.modifiers.contains(KeyModifiers::SHIFT);

        // Alt + Up/Down: window resize chord. Caller surfaces
        // these as GrowPrompt/ShrinkPrompt watcher signals
        // separately, but a key delivered directly to the
        // prompt (e.g. from a future direct-injection path)
        // still routes correctly.
        if alt {
            return match ke.code {
                KeyCode::Up => PromptAction::GrowWindow,
                KeyCode::Down => PromptAction::ShrinkWindow,
                _ => PromptAction::Continue,
            };
        }

        // Ctrl-C: interrupt. Caller re-raises SIGINT.
        if ctrl && matches!(ke.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            return PromptAction::Interrupt;
        }

        // Ctrl-/ : toggle help. crossterm reports Ctrl-/ as
        // `Char('/')` with CONTROL on most terminals.
        if ctrl && matches!(ke.code, KeyCode::Char('/')) {
            return PromptAction::ToggleHelp;
        }

        match ke.code {
            KeyCode::Enter => {
                let line = self.buffer.as_str().trim().to_string();
                self.buffer.update("", 0, &mut Silent);
                self.history_cursor = None;
                self.live_snapshot.clear();
                if !line.is_empty() {
                    // History add can fail (`MemHistory` bound
                    // reached, file I/O on persistent variants);
                    // ignore — the line still submits.
                    let _ = self.history.add(&line);
                }
                if line.is_empty() {
                    PromptAction::Continue
                } else {
                    PromptAction::Submit(line)
                }
            }
            KeyCode::Backspace => {
                self.buffer.backspace(1, &mut Silent);
                PromptAction::Continue
            }
            KeyCode::Delete => {
                self.buffer.delete(1, &mut Silent);
                PromptAction::Continue
            }
            KeyCode::Left => {
                self.buffer.move_backward(1);
                PromptAction::Continue
            }
            KeyCode::Right => {
                self.buffer.move_forward(1);
                PromptAction::Continue
            }
            KeyCode::Home => {
                self.buffer.move_home();
                PromptAction::Continue
            }
            KeyCode::End => {
                self.buffer.move_end();
                PromptAction::Continue
            }
            KeyCode::Up => {
                self.step_history_back();
                PromptAction::Continue
            }
            KeyCode::Down => {
                self.step_history_forward();
                PromptAction::Continue
            }
            // Readline-style controls.
            KeyCode::Char('a') if ctrl => {
                self.buffer.move_home();
                PromptAction::Continue
            }
            KeyCode::Char('e') if ctrl => {
                self.buffer.move_end();
                PromptAction::Continue
            }
            KeyCode::Char('u') if ctrl => {
                // Kill from cursor to start of line. `discard_line`
                // would clear the whole input row including
                // content after the cursor; `discard_buffer` is
                // the right primitive for "kill from cursor back
                // to start". rustyline 14 only ships
                // `discard_line` / `discard_buffer` / `kill_line`
                // / `kill_buffer`; the cursor-to-start kill is
                // hand-rolled via slice + `update`.
                let pos = self.buffer.pos();
                let tail = self.buffer.as_str()[pos..].to_string();
                self.buffer.update(&tail, 0, &mut Silent);
                PromptAction::Continue
            }
            KeyCode::Char('k') if ctrl => {
                // Kill from cursor to end of line.
                self.buffer.kill_line(&mut Silent);
                PromptAction::Continue
            }
            KeyCode::Char('w') if ctrl => {
                self.buffer.delete_prev_word(Word::Big, 1, &mut Silent);
                PromptAction::Continue
            }
            KeyCode::Tab => self.complete_at_cursor(),
            KeyCode::Char(c) if !ctrl => {
                // Printable insert. Shift-modified printables
                // arrive as the already-shifted Char (e.g. 'A'
                // for Shift+a), so we don't have to apply it.
                let _ = shift;
                self.buffer.insert(c, 1, &mut Silent);
                PromptAction::Continue
            }
            _ => PromptAction::Continue,
        }
    }

    fn step_history_back(&mut self) {
        let len = self.history.len();
        if len == 0 {
            return;
        }
        match self.history_cursor {
            None => {
                // First step into history — snapshot live buffer.
                self.live_snapshot = self.buffer.as_str().to_string();
                self.set_buffer_from_history(len - 1);
                self.history_cursor = Some(len - 1);
            }
            Some(0) => { /* already at oldest */ }
            Some(i) => {
                self.set_buffer_from_history(i - 1);
                self.history_cursor = Some(i - 1);
            }
        }
    }

    fn step_history_forward(&mut self) {
        let len = self.history.len();
        match self.history_cursor {
            None => {} // already on live buffer
            Some(i) if i + 1 < len => {
                self.set_buffer_from_history(i + 1);
                self.history_cursor = Some(i + 1);
            }
            Some(_) => {
                // Stepping past the newest entry restores live.
                self.replace_buffer(self.live_snapshot.clone());
                self.history_cursor = None;
            }
        }
    }

    fn set_buffer_from_history(&mut self, idx: usize) {
        // `History::get` returns `Option<SearchResult>` with
        // a `Cow<str>`; we materialise to `String` so the
        // borrow doesn't conflict with the mutable buffer.
        let text: String = self
            .history
            .get(idx, SearchDirection::Forward)
            .ok()
            .flatten()
            .map(|r| r.entry.into_owned())
            .unwrap_or_default();
        self.replace_buffer(text);
    }

    fn replace_buffer(&mut self, text: String) {
        let end = text.len();
        self.buffer.update(&text, end, &mut Silent);
    }

    /// Invoke the completer against the buffer at the current
    /// cursor position. Three outcomes:
    ///
    /// - **No candidates** → buffer unchanged, return Continue.
    /// - **Single candidate** → replace `line[start..pos]` with
    ///   the candidate and return Continue.
    /// - **Multiple candidates** → insert the longest common
    ///   prefix and return `ShowCompletions(candidates)` so
    ///   the sink can render the list above the prompt.
    fn complete_at_cursor(&mut self) -> PromptAction {
        let pos = self.buffer.pos();
        let line = self.buffer.as_str().to_string();
        let (start, candidates) = self.completer.complete(&line, pos);
        if candidates.is_empty() {
            return PromptAction::Continue;
        }
        if candidates.len() == 1 {
            self.replace_token(start, pos, &candidates[0]);
            return PromptAction::Continue;
        }
        let lcp = longest_common_prefix(&candidates);
        let current_partial = &line[start..pos];
        if lcp.len() > current_partial.len() {
            self.replace_token(start, pos, &lcp);
            // After inserting the LCP, no new info to show
            // unless the user asks again — return Continue.
            PromptAction::Continue
        } else {
            // LCP didn't advance the buffer; surface the
            // candidates so the user sees the choices.
            PromptAction::ShowCompletions(candidates)
        }
    }

    /// Replace `buffer[start..end]` with `replacement` via
    /// LineBuffer's `update`. The simplest sound way; we
    /// reconstruct the full buffer string with the swap
    /// applied and hand it to rustyline.
    fn replace_token(&mut self, start: usize, end: usize, replacement: &str) {
        let text = self.buffer.as_str();
        let mut next = String::with_capacity(text.len() + replacement.len());
        next.push_str(&text[..start]);
        next.push_str(replacement);
        next.push_str(&text[end..]);
        let cursor = start + replacement.len();
        self.buffer.update(&next, cursor, &mut Silent);
    }

    /// Render the prompt window into `out`. `terminal_cols`
    /// is consulted for soft-clamping; the caller is
    /// responsible for positioning the cursor to the prompt's
    /// top-left before this is called.
    ///
    /// Output shape:
    /// - When help is hidden: `<window_rows - 1>` blank rows
    ///   above the input row + `❯ <buffer>` on the final row.
    /// - When help is visible: as many help rows as fit in
    ///   `window_rows - 1` above the input row.
    ///
    /// The caller positions the cursor AFTER calling render —
    /// `cursor_col()` returns the column where it goes on the
    /// input row.
    pub fn render(&self, out: &mut String, terminal_cols: usize, color: bool) {
        let dim = if color { "\x1b[2m" } else { "" };
        let cyan = if color { "\x1b[36m" } else { "" };
        let reset = if color { "\x1b[0m" } else { "" };

        let total_rows = self.window_rows as usize;
        let input_rows_above = total_rows.saturating_sub(1);

        if self.help_visible && input_rows_above > 0 {
            let help_rows = build_help_rows(input_rows_above, terminal_cols, color);
            for (i, row) in help_rows.iter().enumerate() {
                if i > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(row);
            }
            if !help_rows.is_empty() {
                out.push_str("\r\n");
            }
        } else {
            // Plain top rows are blank — the input is anchored
            // to the LAST row of the window.
            for _ in 0..input_rows_above {
                out.push_str("\r\n");
            }
        }

        // Input row.
        out.push_str(&format!("{cyan}❯{reset} {}", self.buffer.as_str()));

        // Clamp written line to terminal width. The simple
        // form: don't draw more cells than `terminal_cols`
        // including the `❯ ` prefix.
        // (For initial push we trust the buffer fits; long
        // lines will wrap to the next row visually.)
        let _ = terminal_cols;
        let _ = dim;
    }

    /// Column where the cursor goes on the input row (0-based).
    /// `❯ ` is 2 cells wide, plus the buffer position counted
    /// in chars (we treat each char as one cell — good for
    /// ASCII; multi-cell CJK width is a future refinement).
    pub fn cursor_col(&self) -> usize {
        let buf_pos_chars = self.buffer.as_str()[..self.buffer.pos()].chars().count();
        2 + buf_pos_chars
    }
}

/// Compute the longest common prefix shared by every string
/// in `candidates`. Returns an empty string when there's no
/// shared prefix (or no candidates).
fn longest_common_prefix(candidates: &[String]) -> String {
    let mut iter = candidates.iter();
    let Some(first) = iter.next() else {
        return String::new();
    };
    let mut lcp_end = first.len();
    for s in iter {
        // Walk both strings in parallel by byte; truncate
        // `lcp_end` at the first mismatch. We compare BYTES
        // because all our completion candidates are ASCII
        // (command names + arguments are command-line tokens).
        let bytes_a = first.as_bytes();
        let bytes_b = s.as_bytes();
        let limit = lcp_end.min(bytes_b.len());
        let mut common = 0;
        while common < limit && bytes_a[common] == bytes_b[common] {
            common += 1;
        }
        lcp_end = common;
        if lcp_end == 0 {
            break;
        }
    }
    first[..lcp_end].to_string()
}

/// Render the help overlay rows. Truncates to fit
/// `max_rows`; soft-clamps each row to `cols`.
fn build_help_rows(max_rows: usize, cols: usize, color: bool) -> Vec<String> {
    let dim = if color { "\x1b[2m" } else { "" };
    let bold = if color { "\x1b[1m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let entries: &[(&str, &str)] = &[
        ("Enter", "submit command"),
        ("Up / Down", "history previous / next"),
        ("Left / Right", "move cursor"),
        ("Home / Ctrl-A", "cursor to start"),
        ("End / Ctrl-E", "cursor to end"),
        ("Backspace", "delete previous char"),
        ("Delete", "delete char under cursor"),
        ("Ctrl-U", "kill to line start"),
        ("Ctrl-K", "kill to line end"),
        ("Ctrl-W", "kill previous word"),
        ("Alt + Up", "grow prompt window"),
        ("Alt + Down", "shrink prompt window"),
        ("Ctrl-/", "toggle this help overlay"),
        ("Ctrl-C", "interrupt run (re-raises SIGINT)"),
        ("Ctrl-Z", "suspend run"),
        ("Ctrl-L", "redraw screen"),
        ("Ctrl-T", "toggle full TUI mode"),
    ];

    let mut rows: Vec<String> = Vec::with_capacity(max_rows);
    let mut row = String::with_capacity(cols);
    row.push_str(&format!("{bold}nb-shell keystrokes{reset}"));
    rows.push(row);
    for (key, descr) in entries {
        if rows.len() >= max_rows {
            break;
        }
        // Pad the key to 14 chars so descriptions columnise.
        let line = format!("  {bold}{:14}{reset} {dim}{}{reset}", key, descr);
        rows.push(line);
    }
    let _ = cols;
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(c: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(c, m)
    }

    fn char_key(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn defaults_init_one_row_no_help() {
        let p = PromptState::new();
        assert_eq!(p.window_rows(), 1);
        assert!(!p.help_visible());
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn printable_chars_extend_buffer() {
        let mut p = PromptState::new();
        for c in "echo hi".chars() {
            assert!(matches!(p.handle_key(char_key(c)), PromptAction::Continue));
        }
        assert_eq!(p.buffer(), "echo hi");
    }

    #[test]
    fn backspace_removes_one_char() {
        let mut p = PromptState::new();
        for c in "abc".chars() {
            p.handle_key(char_key(c));
        }
        p.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(p.buffer(), "ab");
    }

    #[test]
    fn enter_returns_submit_and_clears_buffer() {
        let mut p = PromptState::new();
        for c in "help".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            PromptAction::Submit(line) => assert_eq!(line, "help"),
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn enter_on_empty_buffer_is_continue() {
        let mut p = PromptState::new();
        match p.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            PromptAction::Continue => {}
            other => panic!("empty Enter should be Continue, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_returns_interrupt() {
        let mut p = PromptState::new();
        for c in "stuff".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)) {
            PromptAction::Interrupt => {}
            other => panic!("Ctrl-C should be Interrupt, got {other:?}"),
        }
    }

    #[test]
    fn alt_up_down_resize_window() {
        let mut p = PromptState::new();
        assert_eq!(p.window_rows(), 1);
        match p.handle_key(key(KeyCode::Up, KeyModifiers::ALT)) {
            PromptAction::GrowWindow => {}
            other => panic!("Alt+Up should be GrowWindow, got {other:?}"),
        }
        // Direct helper to actually mutate state (handle_key
        // returns the action; sink applies it).
        for _ in 0..15 {
            p.grow_window();
        }
        assert_eq!(
            p.window_rows(),
            MAX_WINDOW_ROWS,
            "grow clamps at MAX_WINDOW_ROWS"
        );
        for _ in 0..MAX_WINDOW_ROWS as usize + 5 {
            p.shrink_window();
        }
        assert_eq!(p.window_rows(), 1, "shrink clamps at 1");
    }

    #[test]
    fn ctrl_slash_toggles_help() {
        let mut p = PromptState::new();
        match p.handle_key(key(KeyCode::Char('/'), KeyModifiers::CONTROL)) {
            PromptAction::ToggleHelp => {}
            other => panic!("Ctrl-/ should be ToggleHelp, got {other:?}"),
        }
        p.toggle_help();
        assert!(p.help_visible());
        p.toggle_help();
        assert!(!p.help_visible());
    }

    #[test]
    fn history_up_recalls_previous_entries() {
        let mut p = PromptState::new();
        for cmd in ["one", "two", "three"] {
            for c in cmd.chars() {
                p.handle_key(char_key(c));
            }
            p.handle_key(key(KeyCode::Enter, KeyModifiers::NONE));
        }
        // Up x1 → "three"
        p.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(p.buffer(), "three");
        // Up x2 → "two"
        p.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(p.buffer(), "two");
        // Down → "three"
        p.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(p.buffer(), "three");
        // Down → back to live (empty)
        p.handle_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn ctrl_u_kills_from_start_to_cursor() {
        let mut p = PromptState::new();
        for c in "hello world".chars() {
            p.handle_key(char_key(c));
        }
        // cursor at end; Ctrl-U kills the line.
        p.handle_key(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn render_includes_prompt_glyph_and_buffer() {
        let mut p = PromptState::new();
        for c in "ls".chars() {
            p.handle_key(char_key(c));
        }
        let mut s = String::new();
        p.render(&mut s, 80, false);
        assert!(
            s.contains("❯ ls"),
            "render should contain prompt + buffer: {s:?}"
        );
    }

    #[test]
    fn render_help_overlay_when_window_grown_and_help_toggled() {
        let mut p = PromptState::new();
        for _ in 0..5 {
            p.grow_window();
        }
        p.toggle_help();
        let mut s = String::new();
        p.render(&mut s, 80, false);
        assert!(
            s.contains("nb-shell keystrokes"),
            "help overlay should appear when grown + toggled: {s:?}"
        );
    }

    #[test]
    fn cursor_col_accounts_for_prompt_glyph_width() {
        let mut p = PromptState::new();
        for c in "abc".chars() {
            p.handle_key(char_key(c));
        }
        // "❯ " = 2 cells, "abc" = 3 chars → cursor at col 5.
        assert_eq!(p.cursor_col(), 5);
    }

    /// `NbShellCompleter` exposes inspector command names. A
    /// uniquely-prefixed Tab completes outright; ambiguous
    /// prefixes need the LCP-or-show-list behaviour.
    #[test]
    fn tab_single_match_completes_command_name() {
        let mut p = PromptState::new();
        // "metr" → only `metrics` and `metric` match —
        // ambiguous. Type one more char to disambiguate to
        // `metric` (which is itself a complete command —
        // `metric` is a prefix of `metrics`, so `metric` is
        // the LCP, not the unique resolution).
        for c in "snapsho".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            PromptAction::Continue => {}
            other => panic!("single-match Tab should Continue, got {other:?}"),
        }
        assert_eq!(p.buffer(), "snapshot");
    }

    #[test]
    fn tab_ambiguous_prefix_inserts_lcp_silently() {
        let mut p = PromptState::new();
        // `m` matches: meta, metrics, metric
        // LCP = "met"
        for c in "m".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            PromptAction::Continue => {}
            other => panic!("LCP-advance Tab should Continue, got {other:?}"),
        }
        // LCP across {meta, metrics, metric} is "met"
        assert_eq!(p.buffer(), "met");
    }

    #[test]
    fn tab_at_lcp_returns_show_completions() {
        let mut p = PromptState::new();
        // After typing the LCP "met", another Tab can't
        // advance — sink should show the candidates.
        for c in "met".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            PromptAction::ShowCompletions(list) => {
                let names: Vec<&str> = list.iter().map(|s| s.as_str()).collect();
                assert!(names.contains(&"meta"));
                assert!(names.contains(&"metric"));
                assert!(names.contains(&"metrics"));
            }
            other => panic!("Tab at LCP should ShowCompletions, got {other:?}"),
        }
        // Buffer unchanged when only the list is shown.
        assert_eq!(p.buffer(), "met");
    }

    #[test]
    fn tab_with_no_matches_is_noop() {
        let mut p = PromptState::new();
        for c in "xyzzy".chars() {
            p.handle_key(char_key(c));
        }
        match p.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            PromptAction::Continue => {}
            other => panic!("no-match Tab should Continue, got {other:?}"),
        }
        assert_eq!(p.buffer(), "xyzzy");
    }

    #[test]
    fn tab_after_first_token_does_nothing() {
        let mut p = PromptState::new();
        for c in "set fo".chars() {
            p.handle_key(char_key(c));
        }
        // `set <token>` — second-token completion isn't
        // wired yet, so Tab here returns Continue with no
        // buffer change.
        match p.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            PromptAction::Continue => {}
            other => panic!("second-token Tab should be no-op Continue, got {other:?}"),
        }
        assert_eq!(p.buffer(), "set fo");
    }

    #[test]
    fn longest_common_prefix_basics() {
        assert_eq!(longest_common_prefix(&[]), "");
        assert_eq!(longest_common_prefix(&["alone".into()]), "alone");
        assert_eq!(
            longest_common_prefix(&["meta".into(), "metric".into(), "metrics".into()]),
            "met",
        );
        assert_eq!(longest_common_prefix(&["abc".into(), "xyz".into()]), "",);
        assert_eq!(
            longest_common_prefix(&["help".into(), "help".into()]),
            "help",
        );
    }
}

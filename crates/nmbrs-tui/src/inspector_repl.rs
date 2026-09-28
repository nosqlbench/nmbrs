// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `nmbrs --inspector` — interactive REPL TUI for the OOB
//! introspection endpoint (see [`crate::inspector_server`]).
//!
//! Discovers the local socket under
//! `${XDG_RUNTIME_DIR:-/tmp}/nmbrs-*.sock`, connects to it for
//! each command (the protocol is stateless — one connection
//! per request), and renders the responses in a scrollable
//! ratatui buffer with an input line at the bottom.
//!
//! Tab autocompletes against the `commands` list returned by
//! the server. Up/Down recall input history. `:q` or Ctrl+C
//! exits.
//!
//! ## Pinned-metric pane
//!
//! `:pin <selector>` adds a metric selector (same grammar as
//! the server's `metric` command) to a pin pane that renders
//! at the top of the screen. The pane refreshes on a periodic
//! tick (default 1 s, settable via `:watch <secs>`); each tick
//! issues one `metric <selector>` round-trip per pinned entry.
//! `:unpin <n>` removes the n-th pin (1-based) and `:unpin all`
//! clears the pane. `:pins` lists current pins; `:help` lists
//! every REPL-local meta-command.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(windows)]
use uds_windows::UnixStream;

use crossterm::ExecutableCommand;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::prelude::*;
use ratatui::widgets::*;

/// Discovered nmbrs runtime — pid + its socket path. Returned
/// by [`discover_sockets`] so the caller can pick which
/// process to inspect when more than one is running.
#[derive(Clone, Debug)]
pub struct DiscoveredSocket {
    pub pid: u32,
    pub path: PathBuf,
}

/// Scan `${XDG_RUNTIME_DIR:-/tmp}` for `nmbrs-<pid>.sock`
/// entries. Returns each path together with the parsed pid.
/// Sockets whose corresponding process no longer exists are
/// filtered out (best effort — `kill -0` style probe).
pub fn discover_sockets() -> Vec<DiscoveredSocket> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                // Same fallback as the server's `socket_path_for`.
                std::env::temp_dir()
            } else {
                PathBuf::from("/tmp")
            }
        });
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(rest) = name.strip_prefix("nmbrs-") else {
            continue;
        };
        let Some(pid_str) = rest.strip_suffix(".sock") else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if !pid_alive(pid) {
            // Stale socket — owning process is gone. Don't
            // surface it; the user can clean up by hand if
            // they care.
            continue;
        }
        out.push(DiscoveredSocket { pid, path });
    }
    out.sort_by_key(|s| s.pid);
    out
}

#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    // Best-effort: assume alive on non-Linux. The connect
    // attempt later will fail cleanly if it's stale.
    true
}

/// Send one command to the socket and return the response. A
/// fresh connection per request keeps the protocol stateless
/// and trivially robust — the server doesn't carry per-client
/// state, so an interrupted client never leaves anything
/// behind.
pub fn query(path: &Path, command: &str) -> io::Result<String> {
    // Windows AF_UNIX shows rare transient stalls where a sent
    // request never reaches the server's serial accept loop —
    // observed as the read timing out with no response ever
    // arriving. A fresh connection reliably recovers: dropping
    // the stalled one delivers EOF to the server's blocked
    // per-connection read, unsticking the loop. Retry a couple
    // of times before surfacing the error; on Unix this path is
    // effectively never taken.
    let mut last_err = None;
    for _ in 0..3 {
        match query_once(path, command) {
            Ok(s) => return Ok(s),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::Interrupted
                ) =>
            {
                last_err = Some(e)
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop ran at least once"))
}

fn query_once(path: &Path, command: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{}", command.trim_end())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        response.push_str(&line);
        line.clear();
    }
    Ok(response)
}

/// Top-level entry: run the REPL against an already-resolved
/// socket path. The caller is responsible for discovery /
/// disambiguation; this fn just runs the UI loop.
pub fn run_repl(socket: PathBuf) -> io::Result<()> {
    // Pull the command list once at startup so tab-completion
    // works without a round-trip per keystroke.
    let commands = match query(&socket, "commands") {
        Ok(s) => s.lines().map(str::to_string).collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };

    // Initial banner so the user sees something useful before
    // typing anything.
    let banner = match query(&socket, "meta") {
        Ok(s) => format!("connected to {}\n\n{s}", socket.display()),
        Err(e) => format!("connected to {} (meta failed: {e})", socket.display()),
    };

    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend)?;
    let mut app = ReplApp::new(socket, commands, banner);
    let result = app.event_loop(&mut terminal);
    drop(_guard);
    result
}

/// Restores the terminal on Drop. Mirrors the pattern used by
/// the main TUI's `TerminalGuard`.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        io::stderr().execute(EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = io::stderr().execute(LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

// ─── Cached server vocabulary (controls + metrics) ───────────

/// Background-refreshed snapshot of the server's vocabulary
/// — control names and `(family, labels)` instances. Used by
/// the REPL's tab-completion logic to autocomplete `set
/// <name>`, `metric <family>`, label keys, and label values
/// without a per-keystroke round-trip.
///
/// Refreshed every [`VOCAB_REFRESH_INTERVAL`] by a dedicated
/// background thread. The REPL reads through `Arc<Mutex<...>>`
/// — a Mutex is fine here because the contention pattern is
/// "writer once every 5 s vs. reader on Tab keypress."
#[derive(Default, Clone)]
struct ServerVocab {
    /// Sorted, deduplicated list of dynamic controls. We keep
    /// type and current-value alongside the name so value-position
    /// Tab can prefill the current value as an editable starting
    /// point, and so multi-match completions can show type/value
    /// inline (rather than name-only).
    controls: Vec<ControlInfo>,
    /// Sorted, deduplicated list of `(family, labels-as-text)`
    /// pairs returned by `metrics`. Each entry preserves the
    /// raw server form (`family{k=v,...}`) so we can split it
    /// at completion time.
    metrics: Vec<String>,
}

/// Per-control metadata extracted from the `controls` server
/// response. Source columns: `path | name | type | value | …`.
#[derive(Clone, Default, PartialEq, Eq)]
struct ControlInfo {
    name: String,
    /// Type label as the server reports it (e.g. `u64`, `f64`,
    /// `bool`, `str`). Used to drive type-aware value completion.
    value_type: String,
    /// Current value, formatted as the server returned it. Used
    /// as the prefilled value-position completion so the user can
    /// edit-and-submit instead of typing from scratch.
    value: String,
}

const VOCAB_REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Spawn a background thread that periodically refreshes the
/// vocabulary cache. Runs forever — its lifetime ties to the
/// `Arc<Mutex<ServerVocab>>` it writes to. When the REPL drops
/// the Arc clone the thread sees a stale clone and continues
/// harmlessly until the process exits.
fn spawn_vocab_refresh(socket: PathBuf, vocab: Arc<Mutex<ServerVocab>>) {
    std::thread::Builder::new()
        .name("inspector-vocab".into())
        .spawn(move || {
            loop {
                let next = refresh_vocab_once(&socket);
                if let Ok(mut guard) = vocab.lock() {
                    *guard = next;
                }
                std::thread::sleep(VOCAB_REFRESH_INTERVAL);
            }
        })
        .ok();
}

/// One-shot refresh of the vocabulary cache. Used both by the
/// background thread and by the initial seed at REPL start.
fn refresh_vocab_once(socket: &Path) -> ServerVocab {
    let mut vocab = ServerVocab::default();
    if let Ok(resp) = query(socket, "controls") {
        for line in resp.lines() {
            // Each row is `path | name | type | value | rev=… | scope=… | final-marker`.
            let parts: Vec<&str> = line.splitn(7, '|').map(str::trim).collect();
            if parts.len() >= 4 {
                vocab.controls.push(ControlInfo {
                    name: parts[1].to_string(),
                    value_type: parts[2].to_string(),
                    value: parts[3].to_string(),
                });
            }
        }
    }
    vocab.controls.sort_by(|a, b| a.name.cmp(&b.name));
    // Dedup by name — multiple components may declare the same
    // control name (e.g. the activity-level `rate` plus a
    // sub-component's `rate`). Keep the first occurrence; for
    // completion purposes, name uniqueness is what matters.
    vocab.controls.dedup_by(|a, b| a.name == b.name);

    if let Ok(resp) = query(socket, "metrics") {
        for line in resp.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('(') {
                continue;
            }
            vocab.metrics.push(trimmed.to_string());
        }
    }
    vocab.metrics.sort();
    vocab.metrics.dedup();
    vocab
}

/// Split a cached metrics row `family{k=v,...}` into its
/// family name and parsed `(key, value)` label pairs.
fn split_metric_row(row: &str) -> Option<(String, Vec<(String, String)>)> {
    let row = row.trim();
    let (family, body) = match row.find('{') {
        Some(idx) => {
            let (f, rest) = row.split_at(idx);
            if !rest.ends_with('}') {
                return None;
            }
            (f.to_string(), &rest[1..rest.len() - 1])
        }
        None => (row.to_string(), ""),
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    for piece in body.split(',') {
        let p = piece.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((k, v)) = p.split_once('=') {
            // Strip surrounding quotes (server uses Prometheus
            // style: `key="value"`).
            let v = v.trim().trim_matches('"');
            pairs.push((k.trim().to_string(), v.to_string()));
        }
    }
    Some((family, pairs))
}

// ─── Pinned-metric pane state ────────────────────────────────

/// One pinned selector + its most recent rendered output. The
/// pin pane's height is derived at draw time from the line
/// count of every `rendered` field, capped at [`PIN_PANE_MAX_LINES`].
#[derive(Clone)]
struct PinnedEntry {
    selector: String,
    rendered: String,
    /// Last refresh instant — used to suppress duplicate
    /// per-tick refreshes when the user pauses the loop.
    last_refresh: Option<Instant>,
}

const PIN_PANE_MAX_LINES: usize = 12;
const DEFAULT_PIN_REFRESH: Duration = Duration::from_secs(1);
const MIN_PIN_REFRESH: Duration = Duration::from_millis(500);
const MAX_PIN_REFRESH: Duration = Duration::from_secs(60);

// ─── REPL state ──────────────────────────────────────────────

/// State for the REPL.
struct ReplApp {
    socket: PathBuf,
    /// Tab-completion source. Populated once at startup from
    /// the server's `commands` response.
    commands: Vec<String>,
    /// Background-refreshed control + metric vocabulary used by
    /// argument-aware autocompletion.
    vocab: Arc<Mutex<ServerVocab>>,
    /// Scrollback buffer — every command and its response is
    /// appended as one or more `Line`s. Bounded at
    /// [`SCROLLBACK_LIMIT`] so a long session doesn't eat all
    /// memory.
    scrollback: Vec<Line<'static>>,
    /// Currently-typed input line.
    input: String,
    /// Cursor position within `input` (byte index — input is
    /// ASCII-only in practice, all command names are
    /// lower-case English).
    cursor: usize,
    /// Command history (most recent at end). `history_idx`
    /// points one past the end while not browsing; Up/Down
    /// move within bounds.
    history: Vec<String>,
    history_idx: usize,
    /// Vertical scroll offset for the scrollback view. 0 means
    /// auto-follow the tail.
    scroll: u16,
    should_quit: bool,
    /// Pinned-metric selectors + rendered output.
    pinned: Vec<PinnedEntry>,
    pin_refresh_interval: Duration,
    /// Last instant the pin loop swept all entries — gated
    /// against `pin_refresh_interval` so the per-frame draw
    /// doesn't issue a round-trip per keystroke.
    last_pin_sweep: Option<Instant>,
}

const SCROLLBACK_LIMIT: usize = 5000;

impl ReplApp {
    fn new(socket: PathBuf, commands: Vec<String>, banner: String) -> Self {
        let mut scrollback = Vec::new();
        for line in banner.lines() {
            scrollback.push(Line::from(line.to_string()));
        }
        scrollback.push(Line::from(""));
        scrollback.push(Line::from(Span::styled(
            "type a command and press Enter — Tab autocompletes, :help shows pin commands, :q quits",
            Style::default().fg(Color::DarkGray),
        )));
        scrollback.push(Line::from(""));

        // Seed the vocabulary cache synchronously so the first
        // Tab press has data even if the background thread
        // hasn't ticked yet. Then spawn the refresher.
        let initial_vocab = refresh_vocab_once(&socket);
        let vocab = Arc::new(Mutex::new(initial_vocab));
        spawn_vocab_refresh(socket.clone(), vocab.clone());

        Self {
            socket,
            commands,
            vocab,
            scrollback,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: 0,
            scroll: 0,
            should_quit: false,
            pinned: Vec::new(),
            pin_refresh_interval: DEFAULT_PIN_REFRESH,
            last_pin_sweep: None,
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    ) -> io::Result<()> {
        while !self.should_quit {
            // Refresh pinned metrics if the interval elapsed.
            self.maybe_refresh_pins();
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))?
                && let Event::Key(k) = event::read()?
            {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                self.handle_key(k.code, k.modifiers);
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) {
        if mods.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        match code {
            KeyCode::Enter => self.submit(),
            KeyCode::Char(c) => self.insert_char(c),
            KeyCode::Backspace => self.delete_back(),
            KeyCode::Tab => self.autocomplete(),
            KeyCode::Up => self.history_prev(),
            KeyCode::Down => self.history_next(),
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor < self.input.len() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(8),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::Esc => {
                // Esc clears the current input. Helpful when
                // the user is mid-tab-cycle and wants to
                // restart the line.
                self.input.clear();
                self.cursor = 0;
            }
            _ => {}
        }
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    fn delete_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        // Walk back one char (UTF-8 boundary).
        let mut new_cursor = self.cursor - 1;
        while !self.input.is_char_boundary(new_cursor) && new_cursor > 0 {
            new_cursor -= 1;
        }
        self.input.replace_range(new_cursor..self.cursor, "");
        self.cursor = new_cursor;
    }

    /// Replace `self.input` with `new`, leaving the cursor at
    /// the end. Helper used by every autocomplete branch.
    fn set_input(&mut self, new: String) {
        self.input = new;
        self.cursor = self.input.len();
    }

    /// Tab-completion entry point. Picks one of four behaviors
    /// based on the current input shape:
    ///
    /// 1. No whitespace → complete the first token from the
    ///    server's command list.
    /// 2. `set <prefix>` → complete the control name from the
    ///    cached vocabulary.
    /// 3. `metric <family-prefix>` (no `{`) → complete the
    ///    family name.
    /// 4. `metric <family>{k1=v1,…,partial-key>` (no `=` in the
    ///    last token) → complete a label key.
    /// 5. `metric <family>{…,k=<partial-value>` → complete a
    ///    value for that key in instances matching the family.
    /// 6. `readout <prefix>` → complete the readout name from the
    ///    built-in registry.
    fn autocomplete(&mut self) {
        let input = self.input.clone();
        // Only-first-token case (existing behavior).
        if !input.contains(char::is_whitespace) {
            self.complete_command(&input);
            return;
        }
        // Argument completions for `set` and `metric`.
        if let Some(rest) = input.strip_prefix("set ") {
            // The grammar after `set` is `<name> <value> [source=...]`.
            // Position 1 (name) completes from the controls vocab;
            // position 2 (value) completes type-aware candidates
            // (bool → true/false; non-bool empty-prefix → current
            // value prefill; else a hint line).
            match rest.split_once(char::is_whitespace) {
                None => {
                    // Still inside the name token.
                    self.complete_control_name(rest);
                }
                Some((name, after_name)) => {
                    // Reject `set <name> <value> <extra>` —
                    // value is one token, anything past it is
                    // `source=` etc. that isn't completable.
                    let value_prefix = after_name.trim_start();
                    if value_prefix.contains(char::is_whitespace) {
                        return;
                    }
                    self.complete_control_value(name, value_prefix);
                }
            }
            return;
        }
        if let Some(rest) = input.strip_prefix("metric ") {
            self.complete_metric_arg(rest);
            return;
        }
        // `readout <name>` — complete the readout name against the
        // built-in registry (the same set `render_readout` validates
        // against server-side).
        if let Some(rest) = input.strip_prefix("readout ") {
            self.complete_readout_arg(rest);
        }
    }

    /// Tab in `readout <prefix>` argument position: complete the
    /// readout name from the built-in registry
    /// (`nmbrs_runtime::readouts::Registry::all_names()`) — the
    /// authoritative set the server's `render_readout` accepts. The
    /// command takes a single name argument, so once the cursor is
    /// past it (whitespace in `prefix`) there's nothing to complete.
    fn complete_readout_arg(&mut self, prefix: &str) {
        if prefix.contains(char::is_whitespace) {
            return;
        }
        let matches = readout_name_candidates(prefix);
        match matches.as_slice() {
            [] => self.note_no_completion(prefix),
            [single] => self.set_input(format!("readout {single}")),
            many => {
                let names: Vec<String> = many.iter().map(|s| s.to_string()).collect();
                let lcp = longest_common_prefix(&names);
                if lcp.len() > prefix.len() {
                    self.set_input(format!("readout {lcp}"));
                }
                self.scrollback.push(Line::from(Span::styled(
                    names.join("  "),
                    Style::default().fg(Color::DarkGray),
                )));
                self.trim_scrollback();
            }
        }
    }

    fn complete_command(&mut self, prefix: &str) {
        let matches: Vec<&String> = self
            .commands
            .iter()
            .filter(|c| c.starts_with(prefix))
            .collect();
        match matches.as_slice() {
            [] => self.note_no_completion(prefix),
            [single] => self.set_input((*single).clone()),
            many => {
                let names: Vec<String> = many.iter().map(|s| (*s).clone()).collect();
                let lcp = longest_common_prefix(&names);
                if lcp.len() > prefix.len() {
                    self.set_input(lcp);
                }
                self.scrollback.push(Line::from(Span::styled(
                    names.join("  "),
                    Style::default().fg(Color::DarkGray),
                )));
                self.trim_scrollback();
            }
        }
    }

    fn complete_control_name(&mut self, prefix: &str) {
        let controls = {
            let v = self.vocab.lock().ok();
            v.map(|g| g.controls.clone()).unwrap_or_default()
        };
        let matches: Vec<&ControlInfo> = controls
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .collect();
        match matches.as_slice() {
            [] => self.note_no_completion(prefix),
            [single] => {
                // Prefill the current value as the value-position
                // text. This is the "tab my way to setting a
                // value" experience: a single tab puts the user in
                // edit-and-submit posture rather than blank-page
                // posture. To set a brand-new value, the user
                // backspaces the prefilled value first; to keep
                // it (no-op submit), they press Enter.
                self.set_input(format!("set {} {}", single.name, single.value));
                self.scrollback.push(Line::from(Span::styled(
                    format!(
                        "  ({}: type={}, current={})",
                        single.name, single.value_type, single.value
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
                self.trim_scrollback();
            }
            many => {
                let names: Vec<String> = many.iter().map(|c| c.name.clone()).collect();
                let lcp = longest_common_prefix(&names);
                if lcp.len() > prefix.len() {
                    self.set_input(format!("set {lcp}"));
                }
                // Show name + type + current value for each match
                // so the operator can pick by what the controls
                // currently hold, not just by name.
                let pretty: Vec<String> = many
                    .iter()
                    .map(|c| format!("{} ({}={})", c.name, c.value_type, c.value))
                    .collect();
                self.scrollback.push(Line::from(Span::styled(
                    pretty.join("  "),
                    Style::default().fg(Color::DarkGray),
                )));
                self.trim_scrollback();
            }
        }
    }

    /// Tab in value position: `set <known_name> <prefix>` where
    /// `prefix` is what's after the second whitespace boundary.
    /// For boolean controls, complete `true` / `false`. For
    /// numeric controls there is no fixed vocabulary — but the
    /// initial completion already prefilled the current value via
    /// [`complete_control_name`], so a second Tab in value
    /// position re-prefills (idempotent) and shows the type hint.
    /// Returns true if a completion was applied.
    fn complete_control_value(&mut self, name: &str, prefix: &str) -> bool {
        let controls = {
            let v = self.vocab.lock().ok();
            v.map(|g| g.controls.clone()).unwrap_or_default()
        };
        let Some(info) = controls.iter().find(|c| c.name == name) else {
            return false;
        };
        // Type-driven candidates. Loose match against the type
        // label so `bool` and `Bool` both work.
        let ty = info.value_type.to_lowercase();
        if ty == "bool" || ty == "boolean" {
            let candidates: Vec<&'static str> = ["true", "false"]
                .iter()
                .copied()
                .filter(|s| s.starts_with(prefix))
                .collect();
            match candidates.as_slice() {
                [] => return false,
                [single] => {
                    self.set_input(format!("set {} {}", info.name, single));
                    return true;
                }
                many => {
                    let lcp = longest_common_prefix(
                        &many.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                    );
                    if lcp.len() > prefix.len() {
                        self.set_input(format!("set {} {}", info.name, lcp));
                    }
                    self.scrollback.push(Line::from(Span::styled(
                        many.join("  "),
                        Style::default().fg(Color::DarkGray),
                    )));
                    self.trim_scrollback();
                    return true;
                }
            }
        }
        // Non-bool: when the prefix is empty, prefill the current
        // value. This catches the "user backspaced the prefilled
        // value, hit Tab again to recall it" pattern. Otherwise
        // there's nothing to complete from a fixed vocab.
        if prefix.is_empty() {
            self.set_input(format!("set {} {}", info.name, info.value));
            return true;
        }
        // Type hint as a non-completing nudge.
        self.scrollback.push(Line::from(Span::styled(
            format!(
                "  ({}: type={}, current={})",
                info.name, info.value_type, info.value
            ),
            Style::default().fg(Color::DarkGray),
        )));
        self.trim_scrollback();
        true
    }

    fn complete_metric_arg(&mut self, rest: &str) {
        let metrics = {
            let v = self.vocab.lock().ok();
            v.map(|g| g.metrics.clone()).unwrap_or_default()
        };
        // Case A: no `{` yet — completing a family name.
        match rest.find('{') {
            None => {
                let prefix = rest;
                let mut families: Vec<String> = metrics
                    .iter()
                    .filter_map(|row| split_metric_row(row).map(|(f, _)| f))
                    .collect();
                families.sort();
                families.dedup();
                let candidates: Vec<&String> =
                    families.iter().filter(|f| f.starts_with(prefix)).collect();
                match candidates.as_slice() {
                    [] => self.note_no_completion(prefix),
                    [single] => {
                        // Append `{` to invite the next
                        // completion stage.
                        self.set_input(format!("metric {single}{{"));
                    }
                    many => {
                        let names: Vec<String> = many.iter().map(|s| (*s).clone()).collect();
                        let lcp = longest_common_prefix(&names);
                        if lcp.len() > prefix.len() {
                            self.set_input(format!("metric {lcp}"));
                        }
                        self.scrollback.push(Line::from(Span::styled(
                            names.join("  "),
                            Style::default().fg(Color::DarkGray),
                        )));
                        self.trim_scrollback();
                    }
                }
            }
            Some(brace_idx) => {
                let family = &rest[..brace_idx];
                let body = &rest[brace_idx + 1..];
                self.complete_metric_label(family, body, &metrics);
            }
        }
    }

    /// Inside `metric <family>{...|`. Determine whether the
    /// trailing token is a partial key (no `=`) or a partial
    /// value (after `=`), and complete from the cached metrics
    /// instances filtered by family.
    fn complete_metric_label(&mut self, family: &str, body: &str, metrics: &[String]) {
        // Find the trailing partial token after the last `,`.
        let last = body.rsplit(',').next().unwrap_or("").trim_start();
        let mut matching_instances: Vec<Vec<(String, String)>> = Vec::new();
        for row in metrics {
            if let Some((f, pairs)) = split_metric_row(row)
                && (family == "*" || f == family)
            {
                matching_instances.push(pairs);
            }
        }

        // Decide between key-completion and value-completion.
        if let Some((key, partial_val)) = last.split_once('=') {
            // Value completion.
            let key = key.trim();
            let partial_val = partial_val.trim();
            let mut values: HashSet<String> = HashSet::new();
            for inst in &matching_instances {
                for (k, v) in inst {
                    if k == key {
                        values.insert(v.clone());
                    }
                }
            }
            let mut sorted: Vec<String> = values
                .into_iter()
                .filter(|v| v.starts_with(partial_val))
                .collect();
            sorted.sort();
            let prefix_to_replace = format!("{key}={partial_val}");
            match sorted.as_slice() {
                [] => self.note_no_completion(&prefix_to_replace),
                [single] => {
                    let new_input = self
                        .input
                        .replace(&format!("{key}={partial_val}"), &format!("{key}={single}"));
                    self.set_input(new_input);
                }
                many => {
                    let lcp = longest_common_prefix(many);
                    if lcp.len() > partial_val.len() {
                        let new_input = self
                            .input
                            .replace(&format!("{key}={partial_val}"), &format!("{key}={lcp}"));
                        self.set_input(new_input);
                    }
                    self.scrollback.push(Line::from(Span::styled(
                        many.join("  "),
                        Style::default().fg(Color::DarkGray),
                    )));
                    self.trim_scrollback();
                }
            }
        } else {
            // Key completion.
            let partial = last.trim_end_matches('=');
            let mut keys: HashSet<String> = HashSet::new();
            for inst in &matching_instances {
                for (k, _) in inst {
                    keys.insert(k.clone());
                }
            }
            let mut sorted: Vec<String> = keys
                .into_iter()
                .filter(|k| k.starts_with(partial))
                .collect();
            sorted.sort();
            match sorted.as_slice() {
                [] => self.note_no_completion(partial),
                [single] => {
                    // Replace the last token (the partial key)
                    // with `single=` so the user immediately
                    // sees the next completion stage.
                    let new = self.input.trim_end_matches(partial).to_string() + single + "=";
                    self.set_input(new);
                }
                many => {
                    let lcp = longest_common_prefix(many);
                    if lcp.len() > partial.len() {
                        let new = self.input.trim_end_matches(partial).to_string() + &lcp;
                        self.set_input(new);
                    }
                    self.scrollback.push(Line::from(Span::styled(
                        many.join("  "),
                        Style::default().fg(Color::DarkGray),
                    )));
                    self.trim_scrollback();
                }
            }
        }
    }

    fn note_no_completion(&mut self, prefix: &str) {
        self.scrollback.push(Line::from(Span::styled(
            format!("(no completion for '{prefix}')"),
            Style::default().fg(Color::DarkGray),
        )));
        self.trim_scrollback();
    }

    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_idx > 0 {
            self.history_idx -= 1;
            self.input = self.history[self.history_idx].clone();
            self.cursor = self.input.len();
        }
    }

    fn history_next(&mut self) {
        if self.history_idx + 1 < self.history.len() {
            self.history_idx += 1;
            self.input = self.history[self.history_idx].clone();
            self.cursor = self.input.len();
        } else {
            self.history_idx = self.history.len();
            self.input.clear();
            self.cursor = 0;
        }
    }

    fn submit(&mut self) {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }

        // REPL-local meta-commands take priority over server
        // dispatch.
        if let Some(handled) = self.try_handle_meta(trimmed) {
            // Echo + (optionally) printed feedback.
            self.scrollback.push(Line::from(vec![
                Span::styled("> ", Style::default().fg(Color::Cyan)),
                Span::raw(trimmed.to_string()),
            ]));
            for r_line in handled.lines() {
                self.scrollback.push(Line::from(r_line.to_string()));
            }
            self.scrollback.push(Line::from(""));
            self.history.push(trimmed.to_string());
            self.history_idx = self.history.len();
            self.scroll = 0;
            self.trim_scrollback();
            return;
        }
        match trimmed {
            "quit" | "exit" => {
                self.should_quit = true;
                return;
            }
            "clear" => {
                self.scrollback.clear();
                return;
            }
            _ => {}
        }

        // Echo the input, then dispatch and append the
        // response.
        self.scrollback.push(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(trimmed.to_string()),
        ]));
        match query(&self.socket, trimmed) {
            Ok(response) => {
                for r_line in response.lines() {
                    self.scrollback.push(Line::from(r_line.to_string()));
                }
                if !response.ends_with('\n') {
                    self.scrollback.push(Line::from(response.to_string()));
                }
            }
            Err(e) => {
                self.scrollback.push(Line::from(Span::styled(
                    format!("connection error: {e}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        self.scrollback.push(Line::from(""));
        self.history.push(trimmed.to_string());
        self.history_idx = self.history.len();
        self.scroll = 0; // jump to tail on submit
        self.trim_scrollback();
    }

    /// Interpret REPL-internal meta commands (`:pin`, `:unpin`,
    /// `:pins`, `:watch`, `:help`, `:q`/`:quit`/`:clear`).
    /// Returns `Some(feedback)` when the line is a meta command;
    /// `None` lets the caller fall through to the server.
    fn try_handle_meta(&mut self, line: &str) -> Option<String> {
        if line == ":q" || line == ":quit" {
            self.should_quit = true;
            return Some(String::new());
        }
        if line == ":clear" {
            self.scrollback.clear();
            return Some(String::new());
        }
        if line == ":help" {
            return Some(
                "REPL meta-commands:\n  \
                 :pin <selector>     pin a metric selector to the top pane\n  \
                 :unpin <n|all>      remove a pin (1-based) or clear all\n  \
                 :pins               list current pins\n  \
                 :watch <secs>       set pin-refresh interval (0.5..60)\n  \
                 :clear              clear scrollback\n  \
                 :help               this list\n  \
                 :q | :quit          quit"
                    .into(),
            );
        }
        if let Some(rest) = line.strip_prefix(":pin ") {
            return Some(self.cmd_pin(rest.trim()));
        }
        if line == ":pin" {
            return Some("usage: :pin <selector>".into());
        }
        if let Some(rest) = line.strip_prefix(":unpin ") {
            return Some(self.cmd_unpin(rest.trim()));
        }
        if line == ":unpin" {
            return Some("usage: :unpin <n|all>".into());
        }
        if line == ":pins" {
            return Some(self.cmd_pins());
        }
        if let Some(rest) = line.strip_prefix(":watch ") {
            return Some(self.cmd_watch(rest.trim()));
        }
        if line == ":watch" {
            return Some(format!(
                "current refresh interval: {:.2}s — usage: :watch <secs>",
                self.pin_refresh_interval.as_secs_f64(),
            ));
        }
        // Reject unknown `:` commands explicitly so the user
        // doesn't accidentally send a typoed meta-command to
        // the server.
        if line.starts_with(':') {
            return Some(format!("unknown meta-command '{line}'. Try :help"));
        }
        None
    }

    fn cmd_pin(&mut self, selector: &str) -> String {
        if selector.is_empty() {
            return "usage: :pin <selector>".into();
        }
        if self.pinned.iter().any(|p| p.selector == selector) {
            return format!("ERR already pinned: {selector}");
        }
        self.pinned.push(PinnedEntry {
            selector: selector.to_string(),
            rendered: "(pending first refresh…)".to_string(),
            last_refresh: None,
        });
        // Force an immediate refresh so the user sees data
        // right away rather than waiting up to a full interval.
        self.last_pin_sweep = None;
        format!("OK pinned: {selector}")
    }

    fn cmd_unpin(&mut self, arg: &str) -> String {
        if arg == "all" {
            let n = self.pinned.len();
            self.pinned.clear();
            return format!("OK unpinned {n}");
        }
        let Ok(idx_1based): Result<usize, _> = arg.parse() else {
            return format!("ERR parse: '{arg}' is not a positive integer or 'all'");
        };
        if idx_1based == 0 || idx_1based > self.pinned.len() {
            return format!(
                "ERR out_of_range: have {} pin(s), got {idx_1based}",
                self.pinned.len()
            );
        }
        let removed = self.pinned.remove(idx_1based - 1);
        format!("OK unpinned: {}", removed.selector)
    }

    fn cmd_pins(&self) -> String {
        if self.pinned.is_empty() {
            return "(no pins)".into();
        }
        let mut s = String::new();
        for (i, p) in self.pinned.iter().enumerate() {
            s.push_str(&format!("  {}. {}\n", i + 1, p.selector));
        }
        s.pop();
        s
    }

    fn cmd_watch(&mut self, arg: &str) -> String {
        let secs: f64 = match arg.parse() {
            Ok(v) => v,
            Err(_) => return format!("ERR parse: '{arg}' is not a number"),
        };
        let dur = Duration::from_secs_f64(secs);
        if dur < MIN_PIN_REFRESH || dur > MAX_PIN_REFRESH {
            return format!(
                "ERR out_of_range: refresh interval must be between {:.1}s and {:.0}s",
                MIN_PIN_REFRESH.as_secs_f64(),
                MAX_PIN_REFRESH.as_secs_f64(),
            );
        }
        self.pin_refresh_interval = dur;
        format!("OK refresh interval = {secs:.2}s")
    }

    fn maybe_refresh_pins(&mut self) {
        if self.pinned.is_empty() {
            return;
        }
        let now = Instant::now();
        let due = match self.last_pin_sweep {
            None => true,
            Some(t) => now.duration_since(t) >= self.pin_refresh_interval,
        };
        if !due {
            return;
        }
        self.last_pin_sweep = Some(now);
        // Snapshot selectors first so we don't hold a borrow
        // while issuing socket queries.
        let selectors: Vec<String> = self.pinned.iter().map(|p| p.selector.clone()).collect();
        let mut updates: Vec<String> = Vec::with_capacity(selectors.len());
        for sel in &selectors {
            let resp = match query(&self.socket, &format!("metric {sel}")) {
                Ok(s) => s,
                Err(e) => format!("connection error: {e}"),
            };
            updates.push(resp);
        }
        for (entry, rendered) in self.pinned.iter_mut().zip(updates) {
            entry.rendered = rendered;
            entry.last_refresh = Some(now);
        }
    }

    fn trim_scrollback(&mut self) {
        if self.scrollback.len() > SCROLLBACK_LIMIT {
            let drop = self.scrollback.len() - SCROLLBACK_LIMIT;
            self.scrollback.drain(0..drop);
        }
    }

    /// Compute the height (in terminal lines) the pin pane
    /// should occupy at draw time. `0` when there are no pins
    /// (the layout slot collapses), otherwise the sum of
    /// (1 header line + rendered line count) for every pin,
    /// capped at [`PIN_PANE_MAX_LINES`].
    fn pin_pane_height(&self) -> u16 {
        if self.pinned.is_empty() {
            return 0;
        }
        let mut total: usize = 0;
        for entry in &self.pinned {
            // 1 header line per pin, plus the lines of the
            // rendered output.
            total = total.saturating_add(1 + entry.rendered.lines().count());
        }
        // Cap and add 2 for the surrounding border the pane uses.
        let capped = total.min(PIN_PANE_MAX_LINES);
        (capped as u16).saturating_add(2)
    }

    fn pin_pane_lines(&self) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        for (i, entry) in self.pinned.iter().enumerate() {
            out.push(Line::from(vec![
                Span::styled(format!("[{}] ", i + 1), Style::default().fg(Color::Yellow)),
                Span::styled(entry.selector.clone(), Style::default().fg(Color::Cyan)),
            ]));
            for r in entry.rendered.lines() {
                out.push(Line::from(r.to_string()));
            }
        }
        out
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let pin_h = self.pin_pane_height();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(pin_h),
                Constraint::Min(1),
                Constraint::Length(3),
            ])
            .split(area);

        // Pin pane (only rendered when non-empty so the layout
        // hands us a zero-height slot otherwise).
        if pin_h > 0 {
            let pin_lines = self.pin_pane_lines();
            // Trim to the pane's available inner height (border
            // already accounted for in pin_pane_height).
            let inner_h = chunks[0].height.saturating_sub(2) as usize;
            let visible: Vec<Line<'static>> = pin_lines.into_iter().take(inner_h).collect();
            let pinned = Paragraph::new(visible).block(
                Block::default().borders(Borders::ALL).title(format!(
                    " pinned ({} · refresh={:.1}s) ",
                    self.pinned.len(),
                    self.pin_refresh_interval.as_secs_f64(),
                )),
            );
            frame.render_widget(pinned, chunks[0]);
        }

        // Scrollback panel.
        let visible = chunks[1].height.saturating_sub(2) as usize;
        let total = self.scrollback.len();
        let tail_offset = total.saturating_sub(visible);
        let top = tail_offset.saturating_sub(self.scroll as usize);
        let view: Vec<Line<'static>> = self
            .scrollback
            .iter()
            .skip(top)
            .take(visible)
            .cloned()
            .collect();
        let scrollback = Paragraph::new(view).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" nmbrs inspector — {} ", self.socket.display())),
        );
        frame.render_widget(scrollback, chunks[1]);

        // Input line.
        let input_block = Block::default()
            .borders(Borders::ALL)
            .title(" input — Tab=complete · ↑/↓=history · PgUp/PgDn=scroll · :help · :q=quit ");
        let inner = input_block.inner(chunks[2]);
        frame.render_widget(input_block, chunks[2]);
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Yellow)),
            Span::raw(self.input.clone()),
        ]));
        frame.render_widget(prompt, inner);
        // Cursor: start of inner + "> " (2 cells) + cursor index.
        frame.set_cursor_position(Position::new(inner.x + 2 + self.cursor as u16, inner.y));
    }
}

/// Built-in readout names matching `prefix`, for `readout <name>`
/// tab-completion. Pure helper over the authoritative registry
/// (`nmbrs_runtime::readouts::Registry::all_names()`) so it's
/// unit-testable without constructing a `ReplApp`.
fn readout_name_candidates(prefix: &str) -> Vec<&'static str> {
    nmbrs_runtime::readouts::Registry::all_names()
        .iter()
        .copied()
        .filter(|n| n.starts_with(prefix))
        .collect()
}

fn longest_common_prefix(strs: &[String]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    let mut prefix: Vec<u8> = strs[0].as_bytes().to_vec();
    for s in &strs[1..] {
        let bytes = s.as_bytes();
        let len = prefix
            .iter()
            .zip(bytes.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(len);
        if prefix.is_empty() {
            break;
        }
    }
    String::from_utf8_lossy(&prefix).into_owned()
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_metric_row_parses_family_and_labels() {
        let (family, pairs) =
            split_metric_row("cycles_servicetime{phase=\"pvs_query\",table=\"fknn\"}").unwrap();
        assert_eq!(family, "cycles_servicetime");
        assert_eq!(
            pairs,
            vec![
                ("phase".to_string(), "pvs_query".to_string()),
                ("table".to_string(), "fknn".to_string()),
            ]
        );
    }

    #[test]
    fn split_metric_row_handles_no_labels() {
        let (family, pairs) = split_metric_row("ops").unwrap();
        assert_eq!(family, "ops");
        assert!(pairs.is_empty());
    }

    #[test]
    fn split_metric_row_handles_no_quotes() {
        // Some renderers may emit unquoted values; parser
        // should tolerate both forms.
        let (family, pairs) = split_metric_row("ops{phase=load}").unwrap();
        assert_eq!(family, "ops");
        assert_eq!(pairs, vec![("phase".to_string(), "load".to_string())]);
    }

    #[test]
    fn readout_name_candidates_filters_by_prefix() {
        // Unique prefix.
        assert_eq!(readout_name_candidates("error_"), vec!["error_readout"]);
        // Ambiguous prefix → all matches (drives the LCP path).
        assert_eq!(
            readout_name_candidates("phase_st"),
            vec!["phase_starting", "phase_status"],
        );
        // No match.
        assert!(readout_name_candidates("zzz").is_empty());
        // Wired to the real registry — a known name is in the full set.
        assert!(readout_name_candidates("").contains(&"phase_outcome"));
    }

    #[test]
    fn longest_common_prefix_works() {
        assert_eq!(
            longest_common_prefix(&["concurrency".to_string(), "concurrent".to_string(),]),
            "concurren"
        );
        assert_eq!(
            longest_common_prefix(&["a".to_string(), "b".to_string()]),
            ""
        );
        assert_eq!(longest_common_prefix(&[]), "");
    }
}

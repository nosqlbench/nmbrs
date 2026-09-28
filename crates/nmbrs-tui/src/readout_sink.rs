// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! [`TuiReadoutSink`] — `ReadoutSink` implementation that
//! produces ratatui [`Line<'static>`] output.
//!
//! Bridges the readout engine's terminal-mode ANSI emission
//! style to ratatui's typed-style spans. The sink delegates
//! to a [`StringSink`] internally to capture every byte the
//! binder emits, then parses the ANSI SGR stream on
//! [`take`](Self::take) into a `Vec<Line<'static>>` with
//! per-span [`Style`] for fg color, bold, italic, dim, and
//! underline.
//!
//! The parse-on-flush approach (vs. intercepting style at
//! the binder boundary) means readouts emit the same byte
//! stream regardless of terminal vs. TUI surface — the
//! styling escape sequences are the source of truth, and
//! both surfaces consume them. Adding a new surface
//! (HTML, JSON-with-style) is the same pattern: parse the
//! ANSI stream into the surface's native style type.
//!
//! ## ANSI subset supported
//!
//! - `\x1b[0m` reset
//! - `\x1b[1m` bold, `\x1b[2m` dim, `\x1b[3m` italic, `\x1b[4m` underline
//! - `\x1b[3{0..7}m` standard fg colors
//! - `\x1b[9{0..7}m` bright fg colors
//! - `\x1b[38;2;r;g;bm` truecolor RGB fg
//! - `\x1b[38;5;Nm` 256-color palette fg (mapped to nearest standard)
//! - `\x1b[K` clear-line (silently dropped — not meaningful in spans)
//! - `\r` (silently dropped)
//!
//! Codes outside this subset are passed through as plain text
//! so the surface degrades visibly rather than silently.

use nmbrs_runtime::readouts as ro;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// `ReadoutSink` impl that buffers binder output as ANSI
/// bytes and parses to ratatui [`Line<'static>`] on demand.
/// The internal storage is a [`StringSink`] so the trait
/// surface stays identical to the terminal-mode path; only
/// the consumption shape differs.
pub struct TuiReadoutSink {
    inner: ro::StringSink,
}

impl TuiReadoutSink {
    pub fn new() -> Self {
        Self {
            inner: ro::StringSink::with_capacity(192),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            inner: ro::StringSink::with_capacity(cap),
        }
    }

    /// Consume the sink, returning the rendered output as
    /// a list of styled lines. `\n` in the ANSI stream
    /// terminates a line; spans within a line carry per-run
    /// style picked up from SGR escapes.
    pub fn take(self) -> Vec<Line<'static>> {
        let raw = self.inner.take();
        parse_ansi_to_lines(&raw)
    }
}

impl Default for TuiReadoutSink {
    fn default() -> Self {
        Self::new()
    }
}

impl ro::ReadoutSink for TuiReadoutSink {
    fn literal(&mut self, s: &str) {
        self.inner.literal(s);
    }

    fn render(
        &mut self,
        readout: ro::ReadoutHandle,
        ctx: &dyn ro::ReadoutContext,
        lod: ro::Lod,
        mode: ro::ContentMode,
        options: &ro::ReadoutOptions,
        layout: ro::LayoutHint,
    ) {
        // Delegate layout / pending-break bookkeeping to the
        // wrapped StringSink — the bytes it writes match what
        // a terminal-mode emit would produce, and our
        // parse-on-flush builds the typed-style output from
        // those bytes.
        self.inner.render(readout, ctx, lod, mode, options, layout);
    }

    fn line_break(&mut self) {
        self.inner.line_break();
    }
}

// ── ANSI → ratatui parser ──────────────────────────────

/// Parse an ANSI-escaped string into a list of ratatui
/// `Line<'static>` values. Lines are split on `\n`;
/// within each line, SGR escapes update the active
/// [`Style`] for subsequent text runs.
fn parse_ansi_to_lines(src: &str) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut buf = String::new();

    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                flush_buf(&mut buf, &mut current, style);
                lines.push(Line::from(std::mem::take(&mut current)));
            }
            '\r' => { /* drop carriage return */ }
            '\x1b' => {
                // Flush any pending text first so style
                // changes apply to the *next* run.
                flush_buf(&mut buf, &mut current, style);
                // Walk to the terminating letter.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut params = String::new();
                    let mut terminator = None;
                    for ch in chars.by_ref() {
                        if ch.is_ascii_alphabetic() {
                            terminator = Some(ch);
                            break;
                        }
                        params.push(ch);
                    }
                    if matches!(terminator, Some('m')) {
                        style = apply_sgr(style, &params);
                    }
                    // Non-`m` terminators (K, J, etc.) are
                    // silently dropped — they're cursor /
                    // clear ops with no Span analog.
                } else {
                    // Lone `\x1b` without `[` — treat as
                    // a stray byte; drop.
                }
            }
            _ => buf.push(c),
        }
    }
    // Trailing run / line.
    flush_buf(&mut buf, &mut current, style);
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}

fn flush_buf(buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style) {
    if !buf.is_empty() {
        let s = std::mem::take(buf);
        spans.push(Span::styled(s, style));
    }
}

/// Apply an SGR parameter sequence to the current style.
/// Recognized codes match the subset documented at the
/// module level; unknown codes are silently no-op'd so the
/// parser is forward-tolerant of palette extensions.
fn apply_sgr(mut style: Style, params: &str) -> Style {
    let parts: Vec<&str> = if params.is_empty() {
        vec!["0"]
    } else {
        params.split(';').collect()
    };
    let mut i = 0;
    while i < parts.len() {
        let code: u16 = parts[i].parse().unwrap_or(0);
        match code {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            30..=37 => style = style.fg(standard_fg(code - 30)),
            90..=97 => style = style.fg(bright_fg(code - 90)),
            38 => {
                // 38;2;R;G;B (truecolor) or 38;5;N (256-color).
                if i + 1 < parts.len() {
                    let kind: u16 = parts[i + 1].parse().unwrap_or(0);
                    if kind == 2 && i + 4 < parts.len() {
                        let r = parts[i + 2].parse().unwrap_or(0);
                        let g = parts[i + 3].parse().unwrap_or(0);
                        let b = parts[i + 4].parse().unwrap_or(0);
                        style = style.fg(Color::Rgb(r, g, b));
                        i += 4;
                    } else if kind == 5 && i + 2 < parts.len() {
                        let n: u8 = parts[i + 2].parse().unwrap_or(0);
                        style = style.fg(palette_256_to_color(n));
                        i += 2;
                    }
                }
            }
            39 => style = style.fg(Color::Reset),
            // Background colors (40-47, 100-107) — accepted
            // but unused for now; readouts don't emit bg today.
            _ => {}
        }
        i += 1;
    }
    style
}

fn standard_fg(idx: u16) -> Color {
    match idx {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::White,
        _ => Color::Reset,
    }
}

fn bright_fg(idx: u16) -> Color {
    match idx {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        7 => Color::Gray,
        _ => Color::Reset,
    }
}

fn palette_256_to_color(n: u8) -> Color {
    // Convert 256-color palette index to ratatui Color.
    // Indices 0-15 are the 16 base colors; 16-231 are the
    // 6×6×6 RGB cube; 232-255 are grayscale.
    match n {
        0..=7 => standard_fg(n as u16),
        8..=15 => bright_fg(n as u16 - 8),
        16..=231 => {
            let v = n - 16;
            let r = ((v / 36) % 6) * 51;
            let g = ((v / 6) % 6) * 51;
            let b = (v % 6) * 51;
            Color::Rgb(r, g, b)
        }
        232..=255 => {
            let v = (n - 232) * 10 + 8;
            Color::Rgb(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmbrs_runtime::readouts::ReadoutSink;

    fn span_str<'a>(span: &'a Span<'static>) -> &'a str {
        &span.content
    }

    #[test]
    fn plain_text_yields_single_default_styled_span() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("hello world");
        let lines = sink.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(span_str(&lines[0].spans[0]), "hello world");
        assert_eq!(lines[0].spans[0].style, Style::default());
    }

    #[test]
    fn standard_fg_color_picks_up_red() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[31mERROR\x1b[0m: failed");
        let lines = sink.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2);
        assert_eq!(span_str(&lines[0].spans[0]), "ERROR");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(span_str(&lines[0].spans[1]), ": failed");
        assert_eq!(lines[0].spans[1].style, Style::default());
    }

    #[test]
    fn bright_color_picks_up_light_green() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[92mok\x1b[0m");
        let lines = sink.take();
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::LightGreen));
    }

    #[test]
    fn truecolor_rgb_decodes() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[38;2;214;40;40mERROR\x1b[0m");
        let lines = sink.take();
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(214, 40, 40)));
    }

    #[test]
    fn bold_modifier_applied() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[1mbold\x1b[22m normal");
        let lines = sink.take();
        assert_eq!(lines[0].spans.len(), 2);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn italic_dim_underline_modifiers() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[3mit\x1b[0m\x1b[2mdim\x1b[0m\x1b[4mun\x1b[0m");
        let lines = sink.take();
        assert_eq!(lines[0].spans.len(), 3);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(lines[0].spans[1].style.add_modifier.contains(Modifier::DIM));
        assert!(
            lines[0].spans[2]
                .style
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn newline_splits_into_separate_lines() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[36mhead\x1b[0m\nrow1\nrow2");
        let lines = sink.take();
        assert_eq!(lines.len(), 3);
        assert_eq!(span_str(&lines[0].spans[0]), "head");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert_eq!(span_str(&lines[1].spans[0]), "row1");
        assert_eq!(span_str(&lines[2].spans[0]), "row2");
    }

    #[test]
    fn cr_and_clear_line_dropped() {
        // The inline-status thread emits `\r\x1b[K` to
        // overwrite the previous line. In TUI we have proper
        // areas; both should be silently dropped.
        let mut sink = TuiReadoutSink::new();
        sink.literal("\r\x1b[Khello");
        let lines = sink.take();
        assert_eq!(lines.len(), 1);
        assert_eq!(span_str(&lines[0].spans[0]), "hello");
    }

    #[test]
    fn nested_color_changes_split_runs() {
        let mut sink = TuiReadoutSink::new();
        sink.literal("\x1b[31mred\x1b[32mgreen\x1b[0m plain");
        let lines = sink.take();
        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(span_str(&lines[0].spans[0]), "red");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(span_str(&lines[0].spans[1]), "green");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green));
        assert_eq!(span_str(&lines[0].spans[2]), " plain");
        assert_eq!(lines[0].spans[2].style, Style::default());
    }

    #[test]
    fn palette_256_index_decodes() {
        let mut sink = TuiReadoutSink::new();
        // Index 196 = bright red in the 256-color cube
        // (16 + (5*36 + 0*6 + 0) = 196 → r=255,g=0,b=0).
        sink.literal("\x1b[38;5;196mhi\x1b[0m");
        let lines = sink.take();
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(255, 0, 0)));
    }

    #[test]
    fn end_to_end_phase_outcome_through_binder() {
        // Sanity: route a real builtin readout through the
        // sink, confirm the output parses to non-empty
        // styled lines.
        struct Ctx;
        impl ro::ReadoutContext for Ctx {
            fn subject_name(&self) -> &str {
                "setup"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                Some((1, 2))
            }
            fn cycles_completed(&self) -> u64 {
                3
            }
            fn cycles_total(&self) -> u64 {
                3
            }
            fn ops_ok(&self) -> u64 {
                3
            }
            fn errors(&self) -> u64 {
                0
            }
            fn retries(&self) -> u64 {
                0
            }
            fn concurrency(&self) -> usize {
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.01
            }
            fn consumed(&self) -> u64 {
                3
            }
            fn use_color(&self) -> bool {
                true
            }
            fn event(&self) -> nmbrs_runtime::lifecycle::EventType {
                nmbrs_runtime::lifecycle::EventType::PhaseEnd
            }
        }
        let mut sink = TuiReadoutSink::new();
        let phase_outcome = ro::Registry::lookup("phase_outcome").unwrap();
        let body = ro::BakedBody::from_single(phase_outcome, ro::Lod::Labeled);
        body.fire(&Ctx, ro::ContentMode::Value, &mut sink);
        let lines = sink.take();
        assert!(!lines.is_empty(), "expected at least one line");
        // The phase_outcome renderer emits a green ✓ — confirm
        // some span on the first line carries Color::Green
        // (or LightGreen depending on the bright/dim variant).
        let has_green = lines[0]
            .spans
            .iter()
            .any(|s| matches!(s.style.fg, Some(Color::Green) | Some(Color::LightGreen)));
        assert!(has_green, "expected green ✓ span: {:?}", lines[0].spans);
    }
}

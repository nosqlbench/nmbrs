// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Small presentation helpers shared across readouts.
//!
//! Duration formatting, rate auto-scaling, the braille
//! progress bar, and the spinner frame cycle. These were
//! private to `nmbrs-runtime::activity` before Push 2 and
//! now live next to the readouts that consume them.

/// Standard 10-frame braille spinner cycle. Picks a frame
/// deterministically from `tick % 10` so a refresh actor
/// firing at a steady cadence renders smooth animation.
pub fn spinner_frame(tick: u64) -> char {
    static FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(tick as usize) % FRAMES.len()]
}

/// Per-cell completion bar — variant of [`braille_bar`] used
/// when the phase's total extent is small enough that each
/// cell can represent ONE operation rather than a percentage
/// slice. Renders ballot-box glyphs:
///
/// - `☒` (U+2612) — completed with an error
/// - `☑` (U+2611) — completed successfully
/// - `☐` (U+2610) — pending (not yet completed)
///
/// Cells are grouped in `errors → successes → pending` order
/// so a glance reads worst-news-first; with zero errors the
/// bar degenerates cleanly to the conventional
/// `☑☑☑…☐☐☐…` shape.
///
/// `total` is the cell count (= number of operations, capped
/// at 10 to match the visual width of the braille variant).
/// `successes` and `errors` are clamped to `total` so a
/// brief over-count race during refresh doesn't render a
/// malformed bar.
pub fn ballot_bar(total: u64, successes: u64, errors: u64) -> String {
    if total == 0 {
        return String::new();
    }
    let total = (total as usize).min(10);
    let errors = (errors as usize).min(total);
    let successes = (successes as usize).min(total - errors);
    let pending = total - errors - successes;
    let mut s = String::with_capacity(total * 3);
    for _ in 0..errors {
        s.push('\u{2612}');
    } // ☒
    for _ in 0..successes {
        s.push('\u{2611}');
    } // ☑
    for _ in 0..pending {
        s.push('\u{2610}');
    } // ☐
    s
}

/// 10-character braille completion bar. `pct` is clamped to
/// [0, 100]; each char represents 10 percentage points
/// with 8 within-char sub-levels via the standard bottom-up
/// braille fill pattern, so the bar fills smoothly at
/// ~1.25-percent resolution.
pub fn braille_bar(pct: f64, width: usize) -> String {
    static FILL: [char; 9] = [
        '\u{2800}', // ⠀  empty
        '\u{2840}', // ⡀  +dot 7
        '\u{28C0}', // ⣀  +dot 8
        '\u{28C4}', // ⣄  +dot 3
        '\u{28E4}', // ⣤  +dot 6
        '\u{28E6}', // ⣦  +dot 2
        '\u{28F6}', // ⣶  +dot 5
        '\u{28F7}', // ⣷  +dot 1
        '\u{28FF}', // ⣿  full (+dot 4)
    ];
    if width == 0 {
        return String::new();
    }
    let bounded = pct.clamp(0.0, 100.0);
    let total = (bounded / 100.0 * (width as f64) * 8.0).round() as usize;
    let total = total.min(width * 8);
    let full = total / 8;
    let part = total % 8;
    let mut s = String::with_capacity(width * 3);
    for _ in 0..full {
        s.push(FILL[8]);
    }
    if full < width {
        s.push(FILL[part]);
        for _ in (full + 1)..width {
            s.push(FILL[0]);
        }
    }
    s
}

/// Compact ETA ladder: under a minute → `Ns`; under an
/// hour → `NmMMs`; otherwise → `NhMMm`. Returns `—` for
/// non-finite / negative inputs so a stalled rate doesn't
/// produce a misleading number.
pub fn format_eta(remaining_secs: f64) -> String {
    if !remaining_secs.is_finite() || remaining_secs < 0.0 {
        return "—".to_string();
    }
    let secs = remaining_secs.round() as u64;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Compact 8-char session-elapsed clock with a unit suffix
/// (`s`/`m`/`h`) that scales as the run gets longer. The
/// fractional precision shrinks step-by-step so the integer
/// part stays bounded and the total width is constant — log
/// rows column-align cleanly even when the suffix changes
/// underneath them.
///
/// Layout (always 8 visible chars):
///
/// | Magnitude     | Example     | Shape              |
/// |---------------|-------------|--------------------|
/// | 0 – 9.99999s  | `1.23456s`  | 1 int + 5 frac + s |
/// | 10 – 99.9999s | `99.1234s`  | 2 int + 4 frac + s |
/// | 100 – 999.999s| `999.123s`  | 3 int + 3 frac + s |
/// | 16m – 1h      | `1.23456m`  | 1 int + 5 frac + m |
/// | 10m – 100m    | `99.1234m`  | 2 int + 4 frac + m |
/// | 100m – 1000m  | `999.123m`  | 3 int + 3 frac + m |
/// | 1h+           | `1.23456h`  | hours scaled       |
/// | 10h+          | `99.1234h`  |                    |
/// | 100h+         | `999.123h`  |                    |
///
/// Beyond 999.999h (≈ 41 days) the column widens, matching
/// the `format_elapsed_seconds` precedent for runs longer
/// than fit in 7 chars: the integer part grows, the
/// fractional precision stays at 3.
pub fn format_compact_session_elapsed(secs: f64) -> String {
    let s = secs.max(0.0);
    if s < 60.0 * 60.0 * 100.0 {
        // Within hours threshold — pick the unit that gives
        // an integer part of 1-3 digits and use the matching
        // fractional precision.
        if s < 10.0 {
            format!("{s:.5}s")
        } else if s < 100.0 {
            format!("{s:.4}s")
        } else if s < 1000.0 {
            format!("{s:.3}s")
        } else {
            let m = s / 60.0;
            if m < 10.0 {
                format!("{m:.5}m")
            } else if m < 100.0 {
                format!("{m:.4}m")
            } else if m < 1000.0 {
                format!("{m:.3}m")
            } else {
                let h = s / 3600.0;
                if h < 10.0 {
                    format!("{h:.5}h")
                } else {
                    format!("{h:.4}h")
                }
            }
        }
    } else {
        // 100h+ — integer part is ≥3 digits.
        let h = s / 3600.0;
        format!("{h:.3}h")
    }
}

/// ANSI color span surrounding a compact session-elapsed
/// string. Color tracks magnitude so a quick glance reveals
/// how deep into the run the log line was:
///
/// - Sub-minute → dim (faint), the typical bring-up phase
/// - Sub-hour → default (no override), the steady-state
/// - 1h+ → bold (emphasized), long-running attention cue
///
/// Returns `(open, close)`. Both are empty strings when
/// `color` is false so the formatter stays usable in
/// pipelined / NO_COLOR contexts.
pub fn session_elapsed_color(secs: f64, color: bool) -> (&'static str, &'static str) {
    if !color {
        return ("", "");
    }
    if secs < 60.0 {
        ("\x1b[2m", "\x1b[0m") // dim — early-run
    } else if secs < 3600.0 {
        ("", "") // default — mid-run
    } else {
        ("\x1b[1m", "\x1b[0m") // bold — long-run
    }
}

/// Auto-scaled throughput rate.
pub fn format_rate(rate: f64) -> String {
    if rate >= 1_000_000.0 {
        format!("{:.1}M/s", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.1}K/s", rate / 1_000.0)
    } else {
        format!("{:.0}/s", rate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ballot_bar_empty_total_returns_empty_string() {
        assert_eq!(ballot_bar(0, 0, 0), "");
    }

    /// The compact session timer keeps an 8-char-wide field
    /// across the seconds/minutes/hours boundaries so log
    /// rows column-align cleanly regardless of run length.
    #[test]
    fn compact_session_elapsed_seconds_band() {
        assert_eq!(format_compact_session_elapsed(0.0), "0.00000s");
        assert_eq!(format_compact_session_elapsed(1.23456), "1.23456s");
        assert_eq!(format_compact_session_elapsed(9.99999), "9.99999s");
        assert_eq!(format_compact_session_elapsed(10.0), "10.0000s");
        assert_eq!(format_compact_session_elapsed(99.9999), "99.9999s");
        assert_eq!(format_compact_session_elapsed(100.0), "100.000s");
        assert_eq!(format_compact_session_elapsed(999.999), "999.999s");
    }

    #[test]
    fn compact_session_elapsed_minutes_band() {
        // 1000s ≈ 16.67m — into the minutes band.
        assert_eq!(format_compact_session_elapsed(1000.0), "16.6667m");
        // Crossing through 1h.
        assert_eq!(format_compact_session_elapsed(3599.0), "59.9833m");
        assert_eq!(format_compact_session_elapsed(3600.0), "60.0000m");
        // Approaching the hours band.
        assert_eq!(format_compact_session_elapsed(59940.0), "999.000m");
    }

    #[test]
    fn compact_session_elapsed_hours_band() {
        // 60000s = 16.667h — hours band.
        assert_eq!(format_compact_session_elapsed(60000.0), "16.6667h");
        // Long runs widen the integer part but keep 3 frac.
        assert_eq!(format_compact_session_elapsed(360000.0), "100.000h");
    }

    /// Every value in 0..=999h must produce exactly 8 visible
    /// characters so the gutter stays column-aligned.
    #[test]
    fn compact_session_elapsed_fixed_width_under_a_thousand_hours() {
        let samples = [
            0.0, 0.5, 1.0, 9.999, 10.0, 99.99, 100.0, 999.999, 1000.0, 5000.0, 59940.0, 60000.0,
            360000.0, 3500000.0,
        ];
        for s in samples {
            let out = format_compact_session_elapsed(s);
            assert_eq!(
                out.chars().count(),
                8,
                "elapsed={s} produced {out:?} (width != 8)"
            );
        }
    }

    /// Color span tracks magnitude buckets.
    #[test]
    fn session_elapsed_color_buckets() {
        // No color → empty spans regardless of magnitude.
        assert_eq!(session_elapsed_color(0.5, false), ("", ""));
        assert_eq!(session_elapsed_color(3600.0, false), ("", ""));
        // With color, dim under a minute, default under an
        // hour, bold beyond.
        let (open_sub, _) = session_elapsed_color(0.5, true);
        assert_eq!(open_sub, "\x1b[2m");
        let (open_mid, _) = session_elapsed_color(120.0, true);
        assert_eq!(open_mid, "");
        let (open_long, _) = session_elapsed_color(7200.0, true);
        assert_eq!(open_long, "\x1b[1m");
    }

    #[test]
    fn ballot_bar_groups_errors_first_then_successes_then_pending() {
        // 10 ops: 2 errors, 5 successes, 3 pending.
        assert_eq!(ballot_bar(10, 5, 2), "☒☒☑☑☑☑☑☐☐☐");
    }

    #[test]
    fn ballot_bar_all_successful_degenerates_to_check_only_then_pending() {
        assert_eq!(ballot_bar(5, 3, 0), "☑☑☑☐☐");
    }

    #[test]
    fn ballot_bar_clamps_oversize_total_to_ten() {
        // Caller passing total > 10 (caller's threshold logic
        // bugged) clamps so the bar never exceeds the visual
        // width budget.
        assert_eq!(ballot_bar(15, 0, 0).chars().count(), 10);
    }

    #[test]
    fn ballot_bar_clamps_overflowing_counters() {
        // Counter race during refresh — successes + errors
        // exceeds total. Errors take priority, successes get
        // the remainder, no pending.
        assert_eq!(ballot_bar(3, 99, 1), "☒☑☑");
    }
}

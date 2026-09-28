// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! Widget helpers: sparkline rendering, latency bars, color palette.

use ratatui::style::Color;

/// 24-bit color palette for the TUI.
pub mod colors {
    use super::Color;

    pub const BORDER: Color = Color::Rgb(58, 58, 92);
    pub const TEXT: Color = Color::Rgb(224, 224, 224);
    pub const EMPHASIS: Color = Color::Rgb(255, 255, 255);
    pub const DIM: Color = Color::Rgb(96, 96, 96);

    pub const PHASE_ACTIVE: Color = Color::Rgb(122, 193, 66);
    pub const PHASE_PENDING: Color = Color::Rgb(128, 128, 128);
    pub const PHASE_DONE: Color = Color::Rgb(76, 175, 80);
    pub const PHASE_FAILED: Color = Color::Rgb(244, 67, 54);

    // Phase-tint palette for the scenario tree:
    //   RUNNING_TINT  — currently running (yellow)
    //   DONE_CLEAN    — completed with no errors (green — reuse PHASE_DONE)
    //   DONE_WARN     — completed with some errors (orange)
    //   DONE_BAD      — completed with many errors (red-orange)
    //
    // Chosen for colorblind safety: the yellow/orange/red progression
    // preserves hue separation under protanopia/deuteranopia (the
    // common red-green forms), and the bundled glyphs (`▶`, `✓`, `✗`)
    // plus brightness differences reinforce the meaning for users
    // with tritanopia or monochrome terminals.
    pub const PHASE_RUNNING_TINT: Color = Color::Rgb(247, 201, 72);
    pub const PHASE_DONE_WARN: Color = Color::Rgb(255, 140, 0);
    pub const PHASE_DONE_BAD: Color = Color::Rgb(214, 70, 40);

    pub const PROGRESS_HIGH: Color = Color::Rgb(122, 193, 66);

    pub const LAT_P50: Color = Color::Rgb(77, 201, 246);
    pub const LAT_P90: Color = Color::Rgb(247, 201, 72);
    pub const LAT_P99: Color = Color::Rgb(247, 127, 0);
    pub const LAT_MAX: Color = Color::Rgb(214, 40, 40);

    pub const SPARK: Color = Color::Rgb(77, 201, 246);

    pub const LOG_DEBUG: Color = Color::Rgb(96, 96, 96);
    pub const LOG_INFO: Color = Color::Rgb(77, 201, 246);
    pub const LOG_WARN: Color = Color::Rgb(247, 201, 72);
    pub const LOG_ERROR: Color = Color::Rgb(244, 67, 54);
}

/// Reverse a leaf-first scope-coordinate label string into its
/// root-first form for display. The canonical
/// [`format_scope_coordinate_path`](polydat::kernel::scope_coords::format_scope_coordinate_path)
/// emits `(inner_a=…, inner_b=…), (outer=…)` (leaf-first) so it
/// stays stable as the canonical structural identity used for
/// pre-map ↔ runtime matching. Display surfaces (the terminal
/// observer's phase row and the TUI active-phase panel) prefer
/// root-first reading order — outer scopes first — to mirror
/// the scenario tree the user wrote.
///
/// Splits on the unambiguous group boundary `"), ("` (paren-comma-
/// space-paren never appears inside a binding's value), reverses
/// the segment list, and rejoins. Empty input → empty output.
/// Single-group input is returned unchanged.
pub fn coords_root_first(leaf_first: &str) -> String {
    if leaf_first.is_empty() {
        return String::new();
    }
    let inner = leaf_first
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or(leaf_first);
    let parts: Vec<&str> = inner.split("), (").collect();
    if parts.len() <= 1 {
        return leaf_first.to_string();
    }
    let rev: Vec<&str> = parts.into_iter().rev().collect();
    format!("({})", rev.join("), ("))
}

/// Render a sparkline from a slice of values into a string of
/// Unicode block characters: ▁▂▃▄▅▆▇█
///
/// Auto-ranges to the local min/max of the visible window so
/// micro-variations are visible even when throughput is stable.
/// A perfectly flat line renders as mid-height bars.
pub fn sparkline_str(values: &[f64], width: usize) -> String {
    let blocks = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() {
        return " ".repeat(width);
    }
    let start = if values.len() > width {
        values.len() - width
    } else {
        0
    };
    let visible = &values[start..];

    let min = visible.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = visible.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;
    // Degenerate-range guard: min-max normalization amplifies ANY
    // spread to full bar height, so a series that is flat up to
    // floating-point accumulation noise (a constant mean recomputed
    // as sum/n each sample jitters at ~1e-14 relative) would render
    // as a dramatic false trend. Treat a spread below one part per
    // billion of the series' magnitude as flat — far above f64
    // noise, far below anything a ~20-cell spark can meaningfully
    // display.
    let scale = min.abs().max(max.abs());
    let flat = range <= 0.0 || range < scale * 1e-9;

    let mut s = String::with_capacity(width * 3);
    for &v in visible {
        if flat {
            // Flat line — show mid-height
            s.push(blocks[4]);
        } else {
            let normalized = (v - min) / range;
            let idx = (normalized * 7.0).round() as usize;
            s.push(blocks[idx.min(7)]);
        }
    }
    // Pad if fewer values than width
    while s.chars().count() < width {
        s.insert(0, ' ');
    }
    s
}

pub fn format_nanos(nanos: u64) -> String {
    if nanos == 0 {
        return "—".to_string();
    }
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.1}µs", nanos as f64 / 1_000.0)
    } else if nanos < 1_000_000_000 {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", nanos as f64 / 1_000_000_000.0)
    }
}

/// Format elapsed seconds into M:SS or H:MM:SS.
pub fn format_elapsed(secs: f64) -> String {
    let total = secs as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Format a cumulative duration for the status margin with an EXPLICIT
/// unit at every magnitude, so adjacent rows never jump between
/// notation systems: `13.7s` → `2m19s` → `1h05m`. The earlier colon
/// form (`2:19`) sat beside decimal seconds (`13.7s`) and read as a
/// magnitude discontinuity — `2:19` scans as "2.19" unless the reader
/// stops to reparse the notation. Unit-suffixed forms keep the scan
/// monotonic: bigger number of coarser unit == longer, always.
pub fn format_dur_compact(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        let total = secs as u64;
        format!("{}m{:02}s", total / 60, total % 60)
    } else {
        let total = secs as u64;
        format!("{}h{:02}m", total / 3600, (total % 3600) / 60)
    }
}

/// Visible width (chars) of a status-margin body for `total_phases`, matching
/// [`margin_body`]: session-time(8) + space + `[n/total]` count field
/// (`2·digits+3`) + space + phase/leaf-time(8).
// Currently unreferenced: the sinks measure the rendered margin's
// visible width directly (per-line stamps carry their own body), but
// this function IS the width contract `margin_body` promises — kept as
// the executable form of that documentation for the SRD-63 margin work.
#[allow(dead_code)]
pub fn margin_body_width(total_phases: usize) -> usize {
    TIMING_MARK.chars().count()
        + 1
        + 8
        + 1
        + (total_phases.to_string().len().max(1) * 2 + 3)
        + 1
        + 8
}

/// Marks a gutter cell as PHASE TIMING, so timings are identifiable at a glance
/// and are not read as one of the metric cells that share the same column.
///
/// Deliberately a single-cell glyph. The obvious choices — ⏱ U+23F1, ⌚, 🕐 —
/// carry emoji presentation and occupy two terminal cells, while the width
/// arithmetic through this module counts CHARACTERS. Any of them would shift
/// every divider on the timing rows by one column, breaking the alignment the
/// mark exists to serve.
pub const TIMING_MARK: &str = "◷";

/// The agreed status-margin body: a [`TIMING_MARK`] followed by a fixed-width
/// `session-time · count · phase/leaf-time` triad (no `│`, no color) — session time on the LEFT, the
/// phase counter in the MIDDLE, the phase timer on the RIGHT. `count` is padded
/// to the `[n/total]` field width so the flanking right-aligned 8-col time slots
/// line up across every row; `—` fills an absent time. Shared by the managed-TUI
/// gutter and the op leaves so surfaces align identically.
pub fn margin_body(
    total_phases: usize,
    count: &str,
    leaf: Option<f64>,
    sess: Option<f64>,
) -> String {
    let tw = total_phases.to_string().len().max(1);
    let count_w = tw * 2 + 3;
    let leaf_s = leaf
        .map(format_dur_compact)
        .unwrap_or_else(|| "—".to_string());
    let sess_s = sess
        .map(format_dur_compact)
        .unwrap_or_else(|| "—".to_string());
    format!("{TIMING_MARK} {sess_s:>8} {count:<count_w$} {leaf_s:>8}")
}

/// Format a rate value with auto-scaling (K/M suffix).
///
/// Uses a consistent decimal width within each magnitude band so
/// values that oscillate across a boundary don't flip formats
/// frame-to-frame (e.g. 0.99 ↔ 1.00, not 0.99 ↔ 1).
pub fn format_rate(rate: f64) -> String {
    if rate >= 1_000_000.0 {
        format!("{:.1}M", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.1}K", rate / 1_000.0)
    } else {
        format!("{:.2}", rate)
    }
}

/// Format a count with auto-scaling.
pub fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{n}")
    }
}

/// Build a horizontal bar string of given width, filled proportionally.
pub fn bar_str(fraction: f64, width: usize) -> String {
    let fill = (fraction.clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut s = String::with_capacity(width);
    for _ in 0..fill {
        s.push('━');
    }
    for _ in fill..width {
        s.push('╌');
    }
    s
}

/// Build a horizontal Braille-pip progress bar of `width` cells,
/// returning the filled and unfilled halves as separate strings
/// so the caller can color them differently.
///
/// Each cell is a Braille pattern (2 columns × 4 dots = 8 pips).
/// Sub-cell precision: 8 pips per cell × `width` cells. A 100-cell
/// bar resolves to 800 distinct positions.
///
/// Pip ordering within a cell is **bottom-up, left column first
/// then right column** — pips light up like a rising tide:
///
/// ```text
///   .  .       .  .       .  .       .  .       *  .       *  *
///   .  .       .  .       .  .       *  .  →    *  .  →    *  *
///   .  .       .  .       *  .  →    *  .       *  .       *  *
///   .  .  →    *  .  →    *  .       *  .       *  .       *  *
///   1/8        2/8        4/8        5/8        6/8        8/8
/// ```
///
/// The unfilled half uses `⣀` (U+28C0, bottom-row dots in both
/// columns) — a low-key baseline that shows the bar's total
/// width without competing with the filled portion.
///
/// Returns `(filled, unfilled)`. The filled string ends with the
/// boundary cell's partial-pip pattern (or is exactly `width`
/// cells of `⣿` at 100%); the unfilled string is the remaining
/// cells of `⣀`.
pub fn bar_str_braille(fraction: f64, width: usize) -> (String, String) {
    if width == 0 {
        return (String::new(), String::new());
    }
    // 8 pips per cell × `width` cells = total resolution.
    let total_pips = width * 8;
    let lit_pips = (fraction.clamp(0.0, 1.0) * total_pips as f64).round() as usize;
    let full_cells = lit_pips / 8;
    let partial = lit_pips % 8;

    // Unicode 8-dot Braille bit positions:
    //   dot 1: top-left      (0x01)
    //   dot 2: middle-top-l  (0x02)
    //   dot 3: middle-bot-l  (0x04)
    //   dot 4: top-right     (0x08)
    //   dot 5: middle-top-r  (0x10)
    //   dot 6: middle-bot-r  (0x20)
    //   dot 7: bottom-left   (0x40)
    //   dot 8: bottom-right  (0x80)
    //
    // Light pips bottom-up within each column so a partially-
    // filled cell reads as a rising silhouette rather than a
    // hanging-from-the-top one. Left column first (the
    // earlier-progress side), then right column.
    const PIP_ORDER: [u32; 8] = [
        0x40, 0x04, 0x02, 0x01, // left column: bottom → top
        0x80, 0x20, 0x10, 0x08, // right column: bottom → top
    ];

    let mut filled = String::with_capacity(width * 3);
    // Solid block (`█`) for fully-filled cells — crisper and
    // more "settled" than Braille's all-pips glyph (`⣿`).
    // Braille is reserved for the boundary cell where the
    // sub-cell animation actually happens, so the eye is
    // drawn to the moving frontier rather than the static
    // bulk of the bar.
    for _ in 0..full_cells {
        filled.push('\u{2588}');
    }
    if partial > 0 && full_cells < width {
        let mut bits: u32 = 0;
        for &pip in &PIP_ORDER[..partial] {
            bits |= pip;
        }
        // Unicode Braille block starts at U+2800; the low byte
        // is the pip-pattern bitmask.
        let codepoint = 0x2800u32 + bits;
        if let Some(c) = char::from_u32(codepoint) {
            filled.push(c);
        } else {
            filled.push('\u{2800}');
        }
    }

    let drawn = full_cells
        + if partial > 0 && full_cells < width {
            1
        } else {
            0
        };
    let mut unfilled = String::with_capacity((width - drawn) * 3);
    // `⣀` (U+28C0) — dots 7 + 8, the bottom row of both
    // columns. Reads as a low-key baseline that grounds the
    // unfilled portion without grabbing focus from the lit
    // pips above.
    for _ in drawn..width {
        unfilled.push('\u{28C0}');
    }

    (filled, unfilled)
}

/// A lifetime trend buffer that keeps its WHOLE history renderable
/// at character-cell resolution, in two regions:
///
/// - **history** (left): buckets of `stride` samples each (stride is
///   a power of two), re-averaged only when the display fills;
/// - **raw tail** (right): the most recent samples, ONE CELL PER
///   SAMPLE, filling left-to-right into whatever margin the history
///   leaves — rendered in a distinct color by the gutter so "recent,
///   individual" reads apart from "older, averaged".
///
/// Samples stack from the LEFT. When `history + raw` would exceed the
/// capacity, the trend RESAMPLES: history pairs merge (stride
/// doubles) and the raw tail folds into complete stride-sized buckets
/// appended to history, freeing right-margin for individual samples
/// again. Lifetime `min`/`max` are the DISCRETE extrema of every
/// pushed sample, untouched by averaging.
/// The power of two nearest to `n / 2`, clamped so at least one cell remains
/// on each side. This is how the history/tail split is chosen: halfway keeps
/// the two views balanced, and a power of two makes every halving of the
/// history exact.
pub fn nearest_pow2_half(n: usize) -> usize {
    if n < 2 {
        return 1;
    }
    let half = (n as f64) / 2.0;
    let lo_exp = (half.log2().floor().max(0.0) as u32).min(31);
    let (a, b) = (1usize << lo_exp, 1usize << (lo_exp + 1));
    let pick = if (half - a as f64).abs() <= (b as f64 - half).abs() {
        a
    } else {
        b
    };
    pick.clamp(1, n.saturating_sub(1).max(1))
}

pub struct DecimatingTrend {
    /// Averaged history buckets, oldest first, `stride` samples each.
    hist: Vec<f64>,
    /// Recent samples, one per future cell, oldest first.
    raw: Vec<f64>,
    /// Cells reserved for the decimated history: a POWER OF TWO, the one
    /// nearest half the cell width. Fixing this boundary is the point —
    /// previously it drifted, because a halving emptied `raw`, which then
    /// refilled until the next halving, so the two regions were never the same
    /// width twice and could not be compared across ticks. A power of two also
    /// makes each halving exact: `hist` folds pairwise with no remainder.
    hist_cap: usize,
    /// Cells reserved for the un-decimated tail, one sample per cell.
    raw_cap: usize,
    /// Samples per history bucket (power of two).
    stride: u32,
    /// Partial bucket: samples evicted from the tail that do not yet make a
    /// full `stride`-wide history cell.
    pending_sum: f64,
    pending_n: u32,
    /// Discrete lifetime extrema over every pushed sample.
    pub min: f64,
    pub max: f64,
}

impl DecimatingTrend {
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(4);
        let hist_cap = nearest_pow2_half(cap);
        Self {
            hist: Vec::new(),
            raw: Vec::new(),
            hist_cap,
            raw_cap: cap - hist_cap,
            stride: 1,
            pending_sum: 0.0,
            pending_n: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Re-target the trend at a new cell width. Cheap and idempotent, so the
    /// painter may call it every render; a terminal resize re-splits the cell
    /// without discarding history.
    pub fn resize(&mut self, cap: usize) {
        let cap = cap.max(4);
        let hist_cap = nearest_pow2_half(cap);
        if hist_cap == self.hist_cap && cap - hist_cap == self.raw_cap {
            return;
        }
        self.hist_cap = hist_cap;
        self.raw_cap = cap - hist_cap;
        while self.raw.len() > self.raw_cap {
            self.evict_oldest_raw();
        }
        while self.hist.len() > self.hist_cap {
            self.halve_hist();
        }
    }

    /// Halve the history in place: pairwise average, stride doubles. Exact,
    /// because `hist_cap` is a power of two.
    fn halve_hist(&mut self) {
        self.stride = self.stride.saturating_mul(2);
        let mut merged = Vec::with_capacity(self.hist.len().div_ceil(2));
        let mut it = self.hist.chunks_exact(2);
        for pair in &mut it {
            merged.push((pair[0] + pair[1]) / 2.0);
        }
        if let [last] = it.remainder() {
            merged.push(*last);
        }
        self.hist = merged;
    }

    /// Move the oldest per-sample cell into the history bucket, emitting a
    /// history cell once a full `stride` of samples has accumulated.
    fn evict_oldest_raw(&mut self) {
        if self.raw.is_empty() {
            return;
        }
        let v = self.raw.remove(0);
        self.pending_sum += v;
        self.pending_n += 1;
        if self.pending_n >= self.stride.max(1) {
            let n = self.pending_n as f64;
            self.hist.push(self.pending_sum / n);
            self.pending_sum = 0.0;
            self.pending_n = 0;
            if self.hist.len() > self.hist_cap {
                self.halve_hist();
            }
        }
    }

    /// Push one sample onto the raw tail; resample when full.
    ///
    /// The overflow loop GUARANTEES progress on every pass (no
    /// stride-doubling without shrinkage): a ≥2-cell history halves
    /// by pair-merge; a degenerate history (0–1 cells) rebases by
    /// folding the raw tail pairwise instead. Either way the total
    /// strictly decreases, the loop terminates, and the stride can
    /// never overflow to zero (the runaway that produced a
    /// divide-by-zero at 2³² doublings).
    pub fn push(&mut self, v: f64) {
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.raw.push(v);
        // The tail holds exactly `raw_cap` cells once warmed; everything older
        // migrates into the fixed-width history. The split therefore stops
        // moving, which is what makes the two regions comparable tick to tick.
        while self.raw.len() > self.raw_cap {
            self.evict_oldest_raw();
        }
    }

    #[allow(dead_code)]
    pub fn hist_len(&self) -> usize {
        self.hist.len()
    }

    // Inspection surface: the painter draws via `split_view`, so these exist for
    // tests and for callers that want the raw regions rather than the fixed view.
    #[allow(dead_code)]
    /// Cells reserved for the history region.
    pub fn hist_cap(&self) -> usize {
        self.hist_cap
    }

    #[allow(dead_code)]
    /// Cells reserved for the per-sample tail region.
    pub fn raw_cap(&self) -> usize {
        self.raw_cap
    }

    /// Render-ready split at a FIXED boundary: the history resampled to exactly
    /// `hist_cap` cells, and the newest samples one-per-cell up to `raw_cap`.
    ///
    /// The resample is what actually pins the divider. Halving cannot keep the
    /// history at a constant length on its own — folding an odd count leaves a
    /// remainder (9 → 5), so `hist` sawtooths and the boundary walks, which is
    /// the drift this replaced. Stretching whatever history exists across a
    /// constant number of columns makes the two regions comparable on every
    /// tick, at the cost of repeating a column while the buffer is between
    /// foldings — honest, since those columns really do describe the same
    /// averaged span.
    ///
    /// While warming up (`hist` still empty) the history side is empty and the
    /// caller left-pads; the tail simply grows into its own region.
    pub fn split_view(&self) -> (Vec<f64>, Vec<f64>) {
        let hist = if self.hist.is_empty() {
            Vec::new()
        } else if self.hist.len() == self.hist_cap {
            self.hist.clone()
        } else {
            // Nearest-neighbour stretch to the fixed width.
            (0..self.hist_cap)
                .map(|i| {
                    let src = i * self.hist.len() / self.hist_cap;
                    self.hist[src.min(self.hist.len() - 1)]
                })
                .collect()
        };
        let start = self.raw.len().saturating_sub(self.raw_cap);
        (hist, self.raw[start..].to_vec())
    }

    /// The renderable series: averaged history then the raw tail,
    /// oldest to newest, left-stacked.
    #[allow(dead_code)]
    pub fn series(&self) -> Vec<f64> {
        let mut s = self.hist.clone();
        s.extend_from_slice(&self.raw);
        s
    }
}

#[cfg(test)]
mod trend_split_tests {
    use super::*;

    #[test]
    fn nearest_pow2_half_picks_the_closer_power() {
        assert_eq!(nearest_pow2_half(20), 8); // half=10 -> 8 is closer than 16
        assert_eq!(nearest_pow2_half(24), 8); // half=12 ties 8 vs 16; tie goes low,
        // leaving more cells for live detail
        assert_eq!(nearest_pow2_half(16), 8); // exact
        assert_eq!(nearest_pow2_half(32), 16); // exact
        assert_eq!(nearest_pow2_half(5), 2); // half=2.5 -> 2 (tie-break low)
        // Always leaves at least one cell for the per-sample tail.
        for n in 2..64 {
            assert!(nearest_pow2_half(n) < n, "n={n}");
        }
    }

    #[test]
    fn split_stays_fixed_as_samples_accumulate() {
        // The regression: the boundary used to drift, because a halving emptied
        // the tail which then refilled. Once warmed, both regions must hold the
        // same widths on every subsequent push.
        let cap = 20;
        let hist_cap = nearest_pow2_half(cap); // 8
        let raw_cap = cap - hist_cap;
        let mut t = DecimatingTrend::new(cap);
        for i in 0..5_000 {
            t.push(i as f64);
            let (hist, raw) = t.split_view();
            assert!(
                raw.len() <= raw_cap,
                "tail overdrew at push {i}: {}",
                raw.len()
            );
            if i >= 64 {
                // Warmed: the RENDERED split must be constant on every tick.
                assert_eq!(
                    hist.len(),
                    hist_cap,
                    "history view must stay fixed at {hist_cap}, got {} at push {i}",
                    hist.len()
                );
                assert_eq!(
                    raw.len(),
                    raw_cap,
                    "tail view must stay fixed at {raw_cap}, got {} at push {i}",
                    raw.len()
                );
            }
        }
    }

    #[test]
    fn tail_is_one_sample_per_cell_and_newest() {
        // The right-hand region must be raw, un-averaged, most-recent samples.
        let cap = 20;
        let hist_cap = nearest_pow2_half(cap);
        let raw_cap = cap - hist_cap;
        let mut t = DecimatingTrend::new(cap);
        for i in 0..1_000 {
            t.push(i as f64);
        }
        let series = t.series();
        let tail = &series[series.len() - raw_cap..];
        let expected: Vec<f64> = ((1_000 - raw_cap)..1_000).map(|i| i as f64).collect();
        assert_eq!(tail, &expected[..], "tail must be the newest raw samples");
    }

    #[test]
    fn resize_rebalances_without_losing_the_series() {
        let mut t = DecimatingTrend::new(20);
        for i in 0..500 {
            t.push(i as f64);
        }
        t.resize(40);
        for i in 0..500 {
            t.push(i as f64);
        }
        let (h, r) = t.split_view();
        assert_eq!(h.len(), nearest_pow2_half(40));
        assert_eq!(h.len() + r.len(), 40);
        t.resize(12);
        for i in 0..500 {
            t.push(i as f64);
        }
        let (h, r) = t.split_view();
        assert_eq!(h.len(), nearest_pow2_half(12));
        assert_eq!(h.len() + r.len(), 12, "shrinking must not overdraw");
    }

    #[test]
    fn lifetime_extrema_survive_decimation() {
        let mut t = DecimatingTrend::new(16);
        t.push(1000.0);
        for i in 0..500 {
            t.push(i as f64);
        }
        assert_eq!(t.max, 1000.0, "max must be the discrete lifetime max");
        assert_eq!(t.min, 0.0);
    }
}

#[cfg(test)]
mod sparkline_tests {
    use super::sparkline_str;

    #[test]
    fn noise_flat_series_renders_flat() {
        // A constant metric recomputed as sum/n jitters at ~1e-14
        // relative — min-max normalization would stretch that to a
        // full-height false trend. The degenerate-range guard must
        // render it flat (mid-height throughout), same as exactly
        // equal values.
        let base = 200.0 / 3.0; // 66.666… — the shape observed live
        let noisy: Vec<f64> = (0..16).map(|i| base + (i as f64) * 5e-14).collect();
        let s = sparkline_str(&noisy, 16);
        let glyphs: std::collections::HashSet<char> = s.chars().collect();
        assert_eq!(
            glyphs.len(),
            1,
            "noise-flat series must render one glyph, got {s:?}"
        );
        assert!(s.chars().all(|c| c == '▅'), "mid-height flat: {s:?}");

        // A genuine trend still renders as one.
        let real: Vec<f64> = (0..16).map(|i| i as f64).collect();
        let s = sparkline_str(&real, 16);
        assert!(
            s.contains('▁') && s.contains('█'),
            "real spread must span the bar range: {s:?}"
        );
    }
}

#[cfg(test)]
mod decimating_trend_tests {
    use super::{DecimatingTrend, nearest_pow2_half};

    #[test]
    fn tail_is_bounded_from_the_start() {
        // CHANGED with the fixed split: the tail is capped at `raw_cap`
        // immediately rather than consuming the whole width until the cell
        // fills. Bounding it from the start is what lets the history/tail
        // divider sit in one place for the entire run; previously the boundary
        // only existed after the first fold and then walked on every fold.
        let cap = 8;
        let raw_cap = cap - nearest_pow2_half(cap); // 8 - 4 = 4
        let mut t = DecimatingTrend::new(cap);
        for v in 0..cap {
            t.push(v as f64);
        }
        let (_hist, raw) = t.split_view();
        assert_eq!(raw.len(), raw_cap, "tail is bounded at {raw_cap}");
        assert_eq!(
            raw,
            vec![4.0, 5.0, 6.0, 7.0],
            "tail holds the newest samples, unaveraged"
        );
        assert!(
            t.hist_len() > 0,
            "older samples fold into history rather than being dropped"
        );
    }

    #[test]
    fn history_folds_and_keeps_a_fixed_width_view() {
        let cap = 8;
        let hist_cap = nearest_pow2_half(cap); // 4
        let mut t = DecimatingTrend::new(cap);
        for v in 0..64 {
            t.push(v as f64);
        }
        let (hist, raw) = t.split_view();
        assert_eq!(hist.len(), hist_cap, "history view is a constant width");
        assert_eq!(
            hist.len() + raw.len(),
            cap,
            "the two regions tile the cell exactly"
        );
        // History is averaged and ordered oldest -> newest; the tail is raw.
        assert!(
            hist.windows(2).all(|w| w[0] <= w[1]),
            "history stays ordered: {hist:?}"
        );
        assert_eq!(raw, vec![60.0, 61.0, 62.0, 63.0]);
    }

    /// Regression: a degenerate history (0–1 cells) must not spin the
    /// stride without shrinking — the old resample doubled the stride
    /// on every push once nothing could fold, overflowing u32 to zero
    /// after 2³² doublings and dividing by it. Bounded capacity and a
    /// live stride must hold over an arbitrarily long life.
    #[test]
    fn tiny_capacity_never_overflows_or_stalls() {
        let mut t = DecimatingTrend::new(4);
        for i in 0..100_000 {
            t.push((i % 37) as f64);
            assert!(t.series().len() <= 4, "series stays within capacity");
        }
    }

    #[test]
    fn lifetime_extrema_are_discrete_not_averaged() {
        let mut t = DecimatingTrend::new(4);
        for v in [5.0, 100.0, 1.0, 7.0, 9.0, 2.0] {
            t.push(v);
        }
        assert_eq!(t.min, 1.0, "min is the discrete lifetime minimum");
        assert_eq!(
            t.max, 100.0,
            "max survives even after its sample is averaged away"
        );
    }
}

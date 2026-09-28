// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! # nmbrs-adapter-plotter
//!
//! Live-updating terminal-plot adapter. Each cycle's resolved
//! fields are projected onto a braille-canvas plot in the
//! terminal — no external dependencies, no persistent files.
//! Useful for visually verifying that a distribution, function,
//! or phase pattern looks right while prototyping a workload.
//!
//! ## Modes
//!
//! | `mode=` | Shape |
//! |---------|-------|
//! | `plot` (default) | Line plot per numeric field, scrolling left-to-right |
//! | `histogram` (`hist`) | Per field, bin the accumulated values across the width and draw counts as bar height — a distribution view (value × frequency) |
//! | `parametric` | Scatter: first numeric field on X, second on Y |
//! | `polar` | Polar plot: first field as radius, second as theta |
//!
//! ## Configuration
//!
//! - `mode=<name>` — selects the rendering mode (table above).
//! - `lanes=<n>` — caps the number of plotted fields when more
//!   are produced than the canvas can comfortably display.
//! - `fade=<n>` — trail decay for parametric / polar modes;
//!   older points fade out over `n` frames.
//!
//! ## Display preference
//!
//! Plotter declares
//! [`DisplayPreference::Off`](nmbrs_runtime::adapter::DisplayPreference::Off):
//! running this adapter auto-disables the dashboard TUI.
//! Plotter and TUI both want raw terminal control of the same
//! screen real estate; the resolution is "plotter wins" — `nmbrs
//! run adapter=plotter ...` skips the TUI without needing
//! `tui=off`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nmbrs_runtime::adapter::{
    DriverAdapter, ExecutionError, OpDispenser, OpResult, ResolvedFields,
};
use nmbrs_workload::model::ParsedOp;
use polydat::ast::Value;

const PALETTE: [(u8, u8, u8); 10] = [
    (86, 180, 233),
    (230, 159, 0),
    (0, 158, 115),
    (240, 228, 66),
    (0, 114, 178),
    (213, 94, 0),
    (204, 121, 167),
    (100, 100, 100),
    (140, 86, 75),
    (148, 103, 189),
];

pub struct PlotterConfig {
    /// Rendering mode. `auto` (default) infers from the output field
    /// NAMES — `x`/`y` → `parametric`, `r`/`theta` (or `radius`/`angle`,
    /// `rho`/`phi`) → `polar`, otherwise a per-field line `plot`.
    /// Explicit: `plot`, `parametric` (alias `xy`), `polar`.
    pub mode: String,
    pub width: usize,
    pub height: usize,
    pub no_color: bool,
    pub fade: f32,
    /// Lane assignment: each inner Vec is a lane containing field names.
    /// `lanes=x,y;z` → `[["x","y"], ["z"]]`.
    /// Empty means auto (one lane per field).
    pub lanes: Vec<Vec<String>>,
    /// How to drive the canvas. `auto` (default) and `single` both draw the
    /// final plot to the screen exactly once, at the end of the run — the
    /// data is accumulated throughout and rendered in one pass. `live`
    /// animates in place at the default rate (opt-in); `<n>`/`<n>hz` animates
    /// at `n` Hz. Live degrades to a single snapshot off a TTY.
    pub render: RenderRequest,
}

/// Default live refresh rate (Hz) — 10 Hz is the 100 ms tick the
/// adapter has always used, and the practical ceiling for a terminal.
const DEFAULT_HZ: f32 = 10.0;

/// What the caller asked for via `render=`. Parsed and validated for
/// backward compatibility; the plotter now always draws its canvas once at
/// shutdown (SRD-87 push 1's single-writer rewrite), so the live-animation
/// mode no longer resolves to a separate render loop.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum RenderRequest {
    /// Decide from the terminal: TTY → live, non-TTY → single.
    Auto,
    /// One static snapshot to the scrollback when the run ends.
    Single,
    /// Animate at the given refresh rate (Hz).
    Live(f32),
}

impl RenderRequest {
    /// Parse `render=`: `auto` | `single`/`snapshot`/`once` | `live` |
    /// `<hz>` / `<hz>hz`. Rates above 60 Hz are rejected; rates above
    /// 10 Hz warn about terminal-refresh limits.
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(RenderRequest::Auto),
            "single" | "snapshot" | "once" => Ok(RenderRequest::Single),
            "live" => Ok(RenderRequest::Live(DEFAULT_HZ)),
            other => {
                let num = other.strip_suffix("hz").unwrap_or(other);
                let hz: f32 = num.trim().parse().map_err(|_| {
                    format!(
                        "unknown render='{other}' (use: auto, single, live, \
                     or a refresh rate like 5 or 5hz)"
                    )
                })?;
                if hz <= 0.0 {
                    return Err(format!("render={other}: refresh rate must be positive"));
                }
                if hz > 60.0 {
                    return Err(format!(
                        "render={other}: refresh rates above 60hz are disallowed \
                         (no terminal can redraw that fast)"
                    ));
                }
                if hz > 10.0 {
                    // SRD-87 A1: diagnostics go through the log channel (→ the
                    // channel's log bucket + session.log), never raw stderr.
                    nmbrs_runtime::diag!(
                        nmbrs_runtime::observer::LogLevel::Warn,
                        "render={other}: refresh above ~10hz exceeds most \
                         terminals' usable redraw rate"
                    );
                }
                Ok(RenderRequest::Live(hz))
            }
        }
    }
}

// The plotter always draws the final plot exactly once, at shutdown — there
// is no animation loop and so no render-mode to resolve. `render=` values are
// still accepted (and validated) for compatibility but no longer change
// behaviour: a single, deadlock-free final paint is the only mode.

impl Default for PlotterConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            width: 0,
            height: 0,
            no_color: false,
            fade: 0.0,
            lanes: Vec::new(),
            render: RenderRequest::Auto,
        }
    }
}

// ─── Name-driven mode inference ────────────────────────────────

const POLAR_R_NAMES: &[&str] = &["r", "radius", "rho"];
const POLAR_THETA_NAMES: &[&str] = &["theta", "angle", "phi"];

/// Resolve `mode` against the in-scope field names. `auto` inspects
/// the names (polar pair wins over `x`/`y`); any explicit mode is
/// returned unchanged.
fn resolve_mode<'a>(mode: &'a str, ordered: &[String]) -> &'a str {
    if mode != "auto" {
        return mode;
    }
    let has = |set: &[&str]| {
        ordered
            .iter()
            .any(|n| set.iter().any(|w| n.eq_ignore_ascii_case(w)))
    };
    let has1 = |w: &str| ordered.iter().any(|n| n.eq_ignore_ascii_case(w));
    if has(POLAR_R_NAMES) && has(POLAR_THETA_NAMES) {
        "polar"
    } else if has1("x") && has1("y") {
        "parametric"
    } else {
        "plot"
    }
}

/// Pick the field whose name matches any of `wanted` (case-insensitive),
/// else the `fallback_idx`-th field in declaration order.
fn pick_field<'a>(
    numeric: &'a HashMap<String, Vec<f64>>,
    ordered: &[String],
    wanted: &[&str],
    fallback_idx: usize,
) -> Option<&'a Vec<f64>> {
    ordered
        .iter()
        .find(|n| wanted.iter().any(|w| n.eq_ignore_ascii_case(w)))
        .or_else(|| ordered.get(fallback_idx))
        .and_then(|n| numeric.get(n))
}

// ─── Cell & FrameBuffer ────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
struct Cell {
    dots: u8,
    r: u8,
    g: u8,
    b: u8,
    bright: f32,
}

impl Cell {
    fn empty() -> Self {
        Cell {
            dots: 0,
            r: 0,
            g: 0,
            b: 0,
            bright: 0.0,
        }
    }

    fn set_dot(&mut self, dx: usize, dy: usize, r: u8, g: u8, b: u8) {
        let bit = match (dx, dy) {
            (0, 0) => 0,
            (1, 0) => 3,
            (0, 1) => 1,
            (1, 1) => 4,
            (0, 2) => 2,
            (1, 2) => 5,
            (0, 3) => 6,
            (1, 3) => 7,
            _ => return,
        };
        self.dots |= 1 << bit;
        self.r = r;
        self.g = g;
        self.b = b;
        self.bright = 1.0;
    }

    fn to_char(self) -> char {
        if self.bright < 0.01 {
            ' '
        } else {
            char::from_u32(0x2800 + self.dots as u32).unwrap_or(' ')
        }
    }
}

struct FrameBuffer {
    cells: Vec<Vec<Cell>>,
    width: usize,
    height: usize,
}

impl FrameBuffer {
    fn new(w: usize, h: usize) -> Self {
        Self {
            cells: vec![vec![Cell::empty(); w]; h],
            width: w,
            height: h,
        }
    }

    fn set_dot(&mut self, px: usize, py: usize, r: u8, g: u8, b: u8) {
        let (cx, cy) = (px / 2, py / 4);
        if cx < self.width && cy < self.height {
            self.cells[cy][cx].set_dot(px % 2, py % 4, r, g, b);
        }
    }

    fn set_dot_idx(&mut self, px: usize, py: usize, ci: usize) {
        let (r, g, b) = PALETTE[ci % PALETTE.len()];
        self.set_dot(px, py, r, g, b);
    }

    /// Render one row to a string, trimmed to its last non-blank cell.
    /// Trailing blank cells are dropped so a row never pads out to the
    /// full framebuffer width: a line of exactly the terminal width plus
    /// a newline triggers the last-column auto-wrap on many terminals,
    /// inserting a phantom blank line — the "stagger". A fully blank row
    /// renders as the empty string.
    fn render_row(&self, y: usize, use_color: bool) -> String {
        let cells = &self.cells[y];
        let Some(last) = cells.iter().rposition(|c| c.bright > 0.01) else {
            return String::new();
        };
        let mut line = String::new();
        // Emit a colour escape only when the colour CHANGES, not per cell.
        // A per-cell `<colour><glyph><reset>` blows the row up ~10× (≈26 B per
        // cell), and a full plot of that overruns the terminal's PTY write
        // buffer — the write blocks under flow control and can deadlock the
        // run's teardown against a slow/strict reader. Runs of same-coloured
        // braille (a histogram lane) collapse to one escape; blanks are bare
        // spaces (an invisible glyph needs no colour).
        let mut cur: Option<(u8, u8, u8)> = None;
        let mut colored = false;
        for c in &cells[..=last] {
            if c.bright > 0.01 {
                if use_color {
                    let rgb = (
                        (c.r as f32 * c.bright) as u8,
                        (c.g as f32 * c.bright) as u8,
                        (c.b as f32 * c.bright) as u8,
                    );
                    if cur != Some(rgb) {
                        line.push_str(&format!("\x1b[38;2;{};{};{}m", rgb.0, rgb.1, rgb.2));
                        cur = Some(rgb);
                        colored = true;
                    }
                }
                line.push(c.to_char());
            } else {
                line.push(' ');
            }
        }
        if colored {
            line.push_str("\x1b[0m");
        }
        line
    }
}

/// Paint the framebuffer to the terminal once, where the cursor sits — the
/// plot is then simply left in the scrollback. On a TTY the lines use explicit
/// CR + erase-line + CR/LF, so they align at column 0 even in raw mode (where
/// a bare '\n' is line-feed only and would stair-step the rows). Piped output
/// (`!is_tty`) is plain text — no escapes to pollute a captured stream. The
/// colour-on-change `render_row` keeps the whole write small enough to clear
/// the terminal's PTY buffer in one go.
fn paint(fb: &FrameBuffer, title: &str, is_tty: bool, use_color: bool) {
    let (cr, clear, eol): (&str, &str, &str) = if is_tty {
        ("\r", "\x1b[2K", "\r\n")
    } else {
        ("", "", "\n")
    };
    let mut buf = format!("{cr}{clear}─── {title} ───{eol}");
    for y in 0..fb.height {
        buf.push_str(clear);
        buf.push_str(&fb.render_row(y, use_color));
        buf.push_str(eol);
    }
    // SRD-87 §5: submit the rendered canvas to the channel's raster bucket
    // (the channel owns the fd) rather than writing stdout directly. The
    // plotter is console-owning, so this lands on the console it owns.
    nmbrs_runtime::output_channel::raster(&buf);
}

/// Paint a stacked, multi-lane plot with a per-lane heading + divider rule.
/// `title` becomes a top banner (the mode word only — the per-lane headings
/// carry the field names); each `(label, start, height)` section emits a
/// `── <label> ──` rule followed by that lane's rows sliced from `fb`. One
/// write, through the raster bucket (SRD-87 §5).
fn paint_lanes(
    fb: &FrameBuffer,
    title: &str,
    sections: &[(String, usize, usize)],
    is_tty: bool,
    use_color: bool,
) {
    let (cr, clear, eol): (&str, &str, &str) = if is_tty {
        ("\r", "\x1b[2K", "\r\n")
    } else {
        ("", "", "\n")
    };
    // The banner is the mode word (`plot: a, b` → `plot`); the lanes name the
    // fields, so repeating the full field list up top would be redundant.
    let banner = title.split(':').next().unwrap_or(title).trim();
    let mut buf = format!("{cr}{clear}─── {banner} ───{eol}");
    for (label, start, height) in sections {
        buf.push_str(clear);
        buf.push_str(&format!("── {label} {}{eol}", "─".repeat(6)));
        let end = (start + height).min(fb.height);
        for y in *start..end {
            buf.push_str(clear);
            buf.push_str(&fb.render_row(y, use_color));
            buf.push_str(eol);
        }
    }
    nmbrs_runtime::output_channel::raster(&buf);
}

// ─── Data collector ────────────────────────────────────────────

struct PlotData {
    numeric: HashMap<String, Vec<f64>>,
    field_order: Vec<String>,
    new_since_render: bool,
}

impl PlotData {
    fn new() -> Self {
        Self {
            numeric: HashMap::new(),
            field_order: Vec::new(),
            new_since_render: false,
        }
    }
    fn record(&mut self, fields: &ResolvedFields) {
        for (i, name) in fields.names.iter().enumerate() {
            if !self.field_order.contains(name) {
                self.field_order.push(name.clone());
            }
            let f = match &fields.values[i] {
                Value::U64(v) => *v as f64,
                Value::F64(v) => *v,
                Value::Bool(v) => {
                    if *v {
                        1.0
                    } else {
                        0.0
                    }
                }
                _ => continue,
            };
            self.numeric.entry(name.clone()).or_default().push(f);
        }
        self.new_since_render = true;
    }
}

// ─── Adapter ───────────────────────────────────────────────────

pub struct PlotterAdapter {
    data: Arc<Mutex<PlotData>>,
    cfg: RenderCfg,
}

/// Everything the single final paint needs, captured once at construction.
/// There is no background render thread and no shared render state — the only
/// mutable state is `data`, which the run's dispensers fill and the one final
/// paint reads. So the plotter is single-writer by construction: no second
/// writer to interleave with, no thread to join, no flag to poll, nothing to
/// race or deadlock against a slow/strict terminal.
#[derive(Clone)]
struct RenderCfg {
    term_w: usize,
    plot_h: usize,
    mode: String,
    lanes: Vec<Vec<String>>,
    use_color: bool,
    is_tty: bool,
}

impl Default for PlotterAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl PlotterAdapter {
    pub fn new() -> Self {
        Self::with_config(PlotterConfig::default())
    }

    pub fn with_config(config: PlotterConfig) -> Self {
        let data = Arc::new(Mutex::new(PlotData::new()));
        // Reserve the last column when auto-sizing: a row exactly the
        // terminal width plus a newline can trip the last-column auto-wrap on
        // some terminals. An explicit `width=` is honoured verbatim.
        let term_w = if config.width > 0 {
            config.width
        } else {
            terminal_width()
                .map(|w| w.saturating_sub(1))
                .unwrap_or(120)
                .max(1)
        };
        let term_h = if config.height > 0 {
            config.height
        } else {
            terminal_height().unwrap_or(30)
        };
        let is_tty = atty_stdout();
        let cfg = RenderCfg {
            term_w,
            plot_h: term_h.saturating_sub(4),
            mode: config.mode.clone(),
            lanes: config.lanes.clone(),
            use_color: !config.no_color && is_tty,
            is_tty,
        };
        Self { data, cfg }
    }
}

/// Render the accumulated data into a fresh framebuffer and paint it to the
/// terminal ONCE. Runs on a `spawn_blocking` task from `shutdown()`. This is
/// the sole place the plotter writes the plot, so it is the only writer —
/// the lock on `data` is released before the (possibly flow-controlled) write,
/// and there is no other thread to rendezvous with.
fn draw_final_plot(data: &Mutex<PlotData>, cfg: &RenderCfg) {
    let d = data.lock().unwrap();
    let ordered: Vec<String> = d
        .field_order
        .iter()
        .filter(|n| d.numeric.contains_key(*n))
        .cloned()
        .collect();
    let title = frame_title(&cfg.mode, &ordered);
    let mut fb = FrameBuffer::new(cfg.term_w, cfg.plot_h);
    if !ordered.is_empty() {
        draw_frame(&mut fb, &ordered, &d.numeric, &cfg.mode, &cfg.lanes, 0);
    }
    // Per-lane headings + dividers: when the plot stacks multiple lanes
    // (plot / scatter / histogram), give each lane its own labelled rule so
    // the distributions read as separate, identifiable bands without relying
    // on colour. The lane row-spans mirror `draw_frame`'s `li * bh` layout
    // (same `resolve_lane_groups`, same `bh` formula) so headings land exactly
    // on the lane boundaries.
    let sections: Vec<(String, usize, usize)> =
        if !ordered.is_empty() && renders_as_lanes(&cfg.mode, &ordered) {
            let groups = resolve_lane_groups(&ordered, &d.numeric, &cfg.lanes);
            if groups.len() > 1 {
                let bh = (fb.height / groups.len().max(1)).max(3);
                groups
                    .iter()
                    .enumerate()
                    .map(|(li, g)| (g.join(", "), li * bh, bh))
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
    drop(d); // release the lock before the blocking write — no lock held over I/O
    if sections.is_empty() {
        paint(&fb, &title, cfg.is_tty, cfg.use_color);
    } else {
        paint_lanes(&fb, &title, &sections, cfg.is_tty, cfg.use_color);
    }
}

impl DriverAdapter for PlotterAdapter {
    fn name(&self) -> &str {
        "plotter"
    }

    fn map_op<'a>(
        &'a self,
        template: &'a ParsedOp,
        parent: std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Box<dyn OpDispenser>, String>> + Send + 'a>,
    > {
        Box::pin(async move {
            // SRD-68 Push 5: snapshot the op-field templates at map_op.
            // Each entry is resolved through `wires` per cycle.
            let op_fields: Vec<(String, serde_json::Value)> = template
                .op
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            Ok(Box::new(PlotterDispenser {
                data: self.data.clone(),
                canonical_kernel: parent,
                op_fields,
            }) as Box<dyn OpDispenser>)
        })
    }
    fn display_preference(&self) -> nmbrs_runtime::adapter::DisplayPreference {
        nmbrs_runtime::adapter::DisplayPreference::Off
    }

    /// Draw the final plot to the terminal exactly once, HERE, before the run
    /// emits its shutdown diagnostics — this hook runs inside
    /// `resource_pool.shutdown()`, ahead of teardown logging, so the plot
    /// lands as one uninterrupted block. The draw is the only thing this
    /// adapter ever writes, and it happens on a single `spawn_blocking` task
    /// awaited right here: one writer, one write, no background thread, no
    /// shared render state. A flow-controlled (slow/strict) terminal can only
    /// make the awaited write take longer — it cannot deadlock, because no
    /// other thread is waiting on it.
    fn shutdown<'a>(
        &'a self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let data = self.data.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            let _ = tokio::task::spawn_blocking(move || draw_final_plot(&data, &cfg)).await;
        })
    }
}

struct PlotterDispenser {
    data: Arc<Mutex<PlotData>>,
    /// SRD-68 invariant I-3: dispenser-owned canonical Polydat Kernel.
    canonical_kernel: std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>,
    /// Op-field templates snapshotted at `map_op`. Resolved per
    /// cycle via the generic `wires` API; typed `Value`s feed the
    /// numeric plot data store.
    op_fields: Vec<(String, serde_json::Value)>,
}

impl OpDispenser for PlotterDispenser {
    fn canonical_kernel(&self) -> Option<&std::sync::Arc<dyn nmbrs_runtime::adapter::Kernel>> {
        Some(&self.canonical_kernel)
    }

    fn execute<'a>(
        &'a self,
        _cycle: u64,
        ctx: &'a nmbrs_runtime::adapter::ExecCtx<'a>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<OpResult, ExecutionError>> + Send + 'a>,
    > {
        let wires = ctx.wires;
        Box::pin(async move {
            let resolved =
                nmbrs_runtime::wires::resolve_op_fields_via_wires(&self.op_fields, wires).map_err(
                    |msg| {
                        ExecutionError::Op(nmbrs_runtime::adapter::AdapterError {
                            error_name: "BindError".into(),
                            message: msg,
                            retryable: false,
                        })
                    },
                )?;
            self.data.lock().unwrap().record(&resolved);
            Ok(OpResult {
                body: None,
                skipped: false,
            })
        })
    }
}

// ─── Plot helpers ──────────────────────────────────────────────

/// The lane grouping for the stacked plot / scatter / histogram modes: one
/// lane per field by default, or the explicit `lanes=` groups (empties
/// dropped). Shared by the frame drawer and the per-lane heading layout so the
/// two never disagree on lane membership or order.
fn resolve_lane_groups<'a>(
    ordered: &'a [String],
    numeric: &HashMap<String, Vec<f64>>,
    lanes: &'a [Vec<String>],
) -> Vec<Vec<&'a str>> {
    if lanes.is_empty() {
        ordered.iter().map(|n| vec![n.as_str()]).collect()
    } else {
        lanes
            .iter()
            .map(|lane| {
                lane.iter()
                    .filter(|n| numeric.contains_key(*n))
                    .map(|n| n.as_str())
                    .collect()
            })
            .filter(|g: &Vec<&str>| !g.is_empty())
            .collect()
    }
}

/// Whether the mode renders as stacked lanes (plot / scatter / histogram, one
/// labelled lane per field group) vs. a single shared canvas (parametric /
/// polar / xy). Mirrors [`draw_frame`]'s mode branching so the per-lane
/// heading layout matches exactly what was drawn.
fn renders_as_lanes(mode: &str, ordered: &[String]) -> bool {
    let m = resolve_mode(mode, ordered);
    let two = ordered.len() >= 2;
    !((matches!(m, "parametric" | "xy") && two) || (m == "polar" && two))
}

/// Draw one frame of `ordered` fields into `fb` using the resolved
/// mode. `from` is the first sample index to draw (incremental in
/// live mode, `0` for a full redraw). Parametric/polar select their
/// axes by field NAME (`x`/`y`, `r`/`theta`), falling back to the
/// first two fields in declaration order.
fn draw_frame(
    fb: &mut FrameBuffer,
    ordered: &[String],
    numeric: &HashMap<String, Vec<f64>>,
    mode: &str,
    lanes: &[Vec<String>],
    from: usize,
) {
    match resolve_mode(mode, ordered) {
        "parametric" | "xy" if ordered.len() >= 2 => {
            if let (Some(x), Some(y)) = (
                pick_field(numeric, ordered, &["x"], 0),
                pick_field(numeric, ordered, &["y"], 1),
            ) {
                plot_xy(fb, x, y, from, 0);
            }
        }
        "polar" if ordered.len() >= 2 => {
            if let (Some(r), Some(t)) = (
                pick_field(numeric, ordered, POLAR_R_NAMES, 0),
                pick_field(numeric, ordered, POLAR_THETA_NAMES, 1),
            ) {
                plot_polar(fb, r, t, from, 0);
            }
        }
        other => {
            // Line/scatter (`plot`, the default) and `histogram` share the
            // lane layout — one lane per field by default, or the explicit
            // `lanes=` groups (so a histogram can stack one-per-field or
            // overlay several in a lane).
            let is_hist = matches!(other, "histogram" | "hist");
            let lane_groups = resolve_lane_groups(ordered, numeric, lanes);
            let bh = (fb.height / lane_groups.len().max(1)).max(3);
            for (li, group) in lane_groups.iter().enumerate() {
                for (fi, &name) in group.iter().enumerate() {
                    if let Some(vals) = numeric.get(name) {
                        if is_hist {
                            plot_histogram(fb, vals, li * bh, bh, fi);
                        } else {
                            plot_line(fb, vals, li * bh, bh, fi, from);
                        }
                    }
                }
            }
        }
    }
}

/// One-line header describing the resolved mode + fields, printed
/// above the final snapshot.
fn frame_title(mode: &str, ordered: &[String]) -> String {
    match resolve_mode(mode, ordered) {
        "parametric" | "xy" if ordered.len() >= 2 => {
            let pick = |w: &str, i: usize| {
                ordered
                    .iter()
                    .find(|n| n.eq_ignore_ascii_case(w))
                    .map(String::as_str)
                    .unwrap_or(ordered[i].as_str())
                    .to_string()
            };
            format!("parametric: {} × {}", pick("x", 0), pick("y", 1))
        }
        "polar" if ordered.len() >= 2 => "polar (r, θ)".to_string(),
        _ if ordered.is_empty() => "plot (no numeric fields)".to_string(),
        "histogram" | "hist" => format!("histogram: {}", ordered.join(", ")),
        _ => format!("plot: {}", ordered.join(", ")),
    }
}

fn plot_xy(fb: &mut FrameBuffer, xv: &[f64], yv: &[f64], from: usize, ci: usize) {
    let n = xv.len().min(yv.len());
    if n == 0 {
        return;
    }
    let (xmin, xmax) = minmax(&xv[..n]);
    let (ymin, ymax) = minmax(&yv[..n]);
    let xr = safe_range(xmin, xmax);
    let yr = safe_range(ymin, ymax);
    let pw = fb.width * 2;
    let ph = fb.height * 4;
    for i in from..n {
        let px = ((xv[i] - xmin) / xr * (pw - 1) as f64) as usize;
        let py = ((yv[i] - ymin) / yr * (ph - 1) as f64) as usize;
        fb.set_dot_idx(px.min(pw - 1), (ph - 1).saturating_sub(py), ci);
    }
}

fn plot_polar(fb: &mut FrameBuffer, rv: &[f64], tv: &[f64], from: usize, ci: usize) {
    let n = rv.len().min(tv.len());
    if n == 0 {
        return;
    }
    // Convert polar to cartesian, centered in the framebuffer
    let rmax = rv[..n].iter().cloned().fold(0.0f64, f64::max).max(0.001);
    let pw = fb.width * 2;
    let ph = fb.height * 4;
    let cx = pw / 2;
    let cy = ph / 2;
    let scale = cx.min(cy) as f64;
    for i in from..n {
        let r_norm = rv[i] / rmax;
        let x = cx as f64 + r_norm * tv[i].cos() * scale;
        let y = cy as f64 - r_norm * tv[i].sin() * scale;
        let px = (x as usize).min(pw - 1);
        let py = (y as usize).min(ph - 1);
        fb.set_dot_idx(px, py, ci);
    }
}

fn plot_line(fb: &mut FrameBuffer, vals: &[f64], y_off: usize, bh: usize, ci: usize, from: usize) {
    if vals.is_empty() {
        return;
    }
    let (mn, mx) = minmax(vals);
    let range = safe_range(mn, mx);
    let pw = fb.width * 2;
    let ph = bh * 4;
    let n = vals.len();
    for (i, &v) in vals.iter().enumerate().skip(from) {
        let px = (i as f64 / n as f64 * (pw - 1) as f64) as usize;
        let py = ((v - mn) / range * (ph - 1) as f64) as usize;
        fb.set_dot_idx(px.min(pw - 1), y_off * 4 + (ph - 1).saturating_sub(py), ci);
    }
}

/// Histogram mode: bin the accumulated values across the lane width and draw
/// each bin's count as a vertical bar rising from the lane baseline — value on
/// the x-axis, frequency as height. The natural distribution view: a normal
/// field shows a bell, an exponential a decaying ramp, a uniform a flat top.
/// Re-bins the full series each call (not incremental), so it wants a full
/// redraw per frame (the default `fade=0` path clears first).
fn plot_histogram(fb: &mut FrameBuffer, vals: &[f64], y_off: usize, bh: usize, ci: usize) {
    if vals.is_empty() {
        return;
    }
    let (mn, mx) = minmax(vals);
    let range = safe_range(mn, mx);
    let pw = fb.width * 2;
    let ph = bh * 4;
    if pw == 0 || ph == 0 {
        return;
    }
    let mut bins = vec![0u32; pw];
    for &v in vals {
        let b = ((v - mn) / range * (pw - 1) as f64) as usize;
        bins[b.min(pw - 1)] += 1;
    }
    let maxc = bins.iter().copied().max().unwrap_or(1).max(1);
    let base = y_off * 4 + (ph - 1); // bottom pixel-row of this lane
    for (x, &c) in bins.iter().enumerate() {
        if c == 0 {
            continue;
        }
        let h = (c as f64 / maxc as f64 * (ph - 1) as f64).round() as usize;
        for dy in 0..=h {
            fb.set_dot_idx(x, base - dy, ci);
        }
    }
}

fn minmax(v: &[f64]) -> (f64, f64) {
    let mn = v.iter().cloned().fold(f64::MAX, f64::min);
    let mx = v.iter().cloned().fold(f64::MIN, f64::max);
    (mn, mx)
}

fn safe_range(mn: f64, mx: f64) -> f64 {
    if (mx - mn).abs() < 1e-10 {
        1.0
    } else {
        mx - mn
    }
}

#[allow(dead_code)]
fn truecolor_fg(idx: usize) -> String {
    let (r, g, b) = PALETTE[idx % PALETTE.len()];
    format!("\x1b[38;2;{r};{g};{b}m")
}

/// Terminal size via crossterm — portable (Unix ioctl + Windows console
/// API behind one call) and consistent with how the rest of the terminal
/// stack (and the shadow-terminal test harness) reports dimensions.
fn terminal_size() -> Option<(usize, usize)> {
    crossterm::terminal::size()
        .ok()
        .filter(|&(c, r)| c > 0 && r > 0)
        .map(|(c, r)| (c as usize, r as usize))
}

fn terminal_width() -> Option<usize> {
    terminal_size().map(|(c, _)| c)
}

fn terminal_height() -> Option<usize> {
    terminal_size().map(|(_, r)| r)
}

fn atty_stdout() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

// =========================================================================
// Adapter Registration (inventory-based, link-time)
// =========================================================================

inventory::submit! {
    nmbrs_runtime::adapter::AdapterRegistration {
        names: || &["plotter", "plot"],
        known_params: || &["mode", "fade", "lanes", "render", "width", "height", "no_color"],
        display_preference: |_params| nmbrs_runtime::adapter::DisplayPreference::Off,
        supported_controls: || &[],
        create: |params| Box::pin(async move {
            let mode = params.get("mode").cloned().unwrap_or_else(|| "auto".into());
            let fade = params.get("fade")
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(0.0);
            let lanes = params.get("lanes")
                .map(|s| s.split(';')
                    .map(|lane| lane.split(',').map(|f| f.trim().to_string()).collect())
                    .collect())
                .unwrap_or_default();
            let render = match params.get("render") {
                Some(s) => RenderRequest::parse(s)?,
                None => RenderRequest::Auto,
            };
            let width = params.get("width").and_then(|s| s.parse().ok()).unwrap_or(0);
            let height = params.get("height").and_then(|s| s.parse().ok()).unwrap_or(0);
            let no_color = params.get("no_color")
                .map(|s| s == "true" || s == "1" || s == "on")
                .unwrap_or(false);
            Ok(std::sync::Arc::new(PlotterAdapter::with_config(PlotterConfig {
                mode, fade, lanes, render, width, height, no_color,
            })) as std::sync::Arc<dyn nmbrs_runtime::adapter::DriverAdapter>)
        }),
    }
}

// SRD-35 Push C: plotter adapter declares itself
// pool-shareable. The plotter writes to a terminal
// (raw stdout) and is identified by its mode + fade +
// lanes config; phases targeting the same plot config
// share one adapter, avoiding the per-phase plot-state
// reset that would otherwise wipe accumulated history.
inventory::submit! {
    nmbrs_runtime::adapter::SharedDriverRegistration {
        adapter: "plotter",
        driver: nmbrs_runtime::adapter::DEFAULT_DRIVER_NAME,
        share_capability: nmbrs_runtime::resource_pool::ShareCapability::Shared,
        resource_key: |params| {
            let mut k = nmbrs_runtime::resource_pool::ResourceKey::new("plotter");
            for field in ["mode", "fade", "lanes", "render", "width", "height", "no_color"] {
                if let Some(v) = params.get(field) {
                    k = k.with(field, v.clone());
                }
            }
            Ok(k)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_parse_basic_forms() {
        assert_eq!(RenderRequest::parse("auto").unwrap(), RenderRequest::Auto);
        assert_eq!(
            RenderRequest::parse("single").unwrap(),
            RenderRequest::Single
        );
        assert_eq!(
            RenderRequest::parse("snapshot").unwrap(),
            RenderRequest::Single
        );
        assert!(matches!(
            RenderRequest::parse("live").unwrap(),
            RenderRequest::Live(_)
        ));
    }

    #[test]
    fn render_hz_and_hz_suffix_are_equivalent() {
        assert_eq!(RenderRequest::parse("5").unwrap(), RenderRequest::Live(5.0));
        assert_eq!(
            RenderRequest::parse("5hz").unwrap(),
            RenderRequest::Live(5.0)
        );
    }

    #[test]
    fn render_above_60hz_is_disallowed() {
        assert!(RenderRequest::parse("70").is_err());
        assert!(RenderRequest::parse("61hz").is_err());
    }

    #[test]
    fn render_above_10hz_allowed_up_to_60() {
        // Warns to stderr, but accepted.
        assert_eq!(
            RenderRequest::parse("30").unwrap(),
            RenderRequest::Live(30.0)
        );
        assert_eq!(
            RenderRequest::parse("60").unwrap(),
            RenderRequest::Live(60.0)
        );
    }

    #[test]
    fn render_invalid_values_rejected() {
        assert!(RenderRequest::parse("fast").is_err());
        assert!(RenderRequest::parse("0").is_err());
        assert!(RenderRequest::parse("-5").is_err());
    }

    #[test]
    fn mode_inferred_from_field_names() {
        let xy = vec!["x".to_string(), "y".to_string()];
        let rt = vec!["r".to_string(), "theta".to_string()];
        let plain = vec!["latency".to_string()];
        assert_eq!(resolve_mode("auto", &xy), "parametric");
        assert_eq!(resolve_mode("auto", &rt), "polar");
        assert_eq!(resolve_mode("auto", &plain), "plot");
        // Explicit mode passes through unchanged.
        assert_eq!(resolve_mode("polar", &xy), "polar");
    }
}

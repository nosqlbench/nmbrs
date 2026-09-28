// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `phase_outcome` — the phase-end summary line.
//!
//! SRD-76 — single canonical phase-end readout. Renders
//! status / duration / error list / resume cursor from the
//! structured outcome the executor installs on the scene
//! tree. Replaces the legacy `phase_outcome` readout that
//! always rendered as success.
//!
//! Push 1 byte-equivalence target: the line emitted by
//! `nmbrs-runtime::activity`'s end-of-activity block prior
//! to this push. The format string was:
//!
//! ```text
//! {depth_indent}{green}✓{reset} {seq_prefix}{bold}{blue}[{phase_name}]{reset}{coords_part} \
//!  {pct:.0}% {rate_str} ok:{ok_pct:.0}% \
//!  {err_color}e:{errors} r:{retries}{reset} c:{concurrency}{relevancy_str} \
//!  {dim}({elapsed:.2}s){reset}
//! ```
//!
//! Where:
//! - `seq_prefix` = `{dim}[{idx}/{total}]{reset} ` if seq is
//!   `Some`, else `""`.
//! - `coords_part` = ` {bold}{yellow}{labels}{reset}` if
//!   labels are non-empty, else `""`.
//! - `err_color` = yellow when errors or retries > 0, else dim.
//! - `rate_str` = auto-scaled (M/s | K/s | /s) per the
//!   helper logic in the prior implementation.
//!
//! That string is the canonical render at
//! `Lod::Labeled, ContentMode::Value`. Compact and Expanded
//! ship in Push 9g (closes G17) per SRD-63 §3.3
//! monotonicity:
//!
//! - **Compact** — `{depth}✓ [name] {pct}% ({elapsed:.2}s)`
//!   — trained-operator scan form: status glyph + identity
//!   + completion percentage + wallclock. Drops the seq
//!     prefix, rate, ok-pct, error / retry / concurrency
//!     counts, scope coords, and chip tail. Every retained
//!     field also appears in Labeled (monotonicity).
//! - **Expanded** — multi-line labelled block. Same data
//!   as Labeled but split across lines with a label per
//!   field, and the chip stream broken into one chip per
//!   line. Adds nothing new (monotonicity flows the
//!   other way too: every Labeled field is in Expanded).
//!
//! Explanation overlay is per SRD-63 §3.2: same shape as
//! the value at the same LOD, with field labels swapped
//! for descriptors.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::format::ballot_bar;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

/// Process-global record of the last-rendered phase-coords
/// string. Read at render time to diff against the current
/// phase's coords (changed values render highlighted), then
/// overwritten with the new string. Lock contention is
/// negligible because realtime status emission is serial:
/// the executor's per-phase done line fires once per phase
/// activation, not concurrently across fibers.
///
/// Thread-safety is via Mutex (not RwLock) because the
/// contention pattern is exactly one writer per render —
/// no concurrent readers without writers. A Mutex is
/// simpler and matches the once-per-phase write cadence.
static LAST_RENDERED_COORDS: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// One stratum of scope coordinates — a single
/// `(k1=v1, k2=v2, ...)` group as produced by
/// [`polydat::kernel::format_scope_coordinate_path`].
/// Pairs are owned `String`s so the diff can operate on
/// stable references without lifetime gymnastics.
struct Stratum {
    pairs: Vec<(String, String)>,
}

/// Parse the canonical striated-parens form back into the
/// structured per-stratum + per-pair shape. The formatter at
/// [`polydat::kernel::format_scope_coordinate_path`]
/// is the inverse — the round trip is exact for the values
/// that fit the formatter's grammar (no embedded `, ` or
/// `)`, ` (` substrings inside a value).
///
/// Returns an empty Vec when the input is empty. Malformed
/// input degrades gracefully: each unparseable stratum is
/// recorded as a single `(_=raw)` pair so the wrap-aware
/// renderer still has something to print.
fn parse_strata(labels: &str) -> Vec<Stratum> {
    if labels.is_empty() {
        return Vec::new();
    }
    // Split on the canonical `), (` separator. Trim the
    // leading `(` and trailing `)` from the first / last
    // chunks respectively.
    let mut strata = Vec::new();
    for raw in labels.split("), (") {
        let inner = raw.trim_start_matches('(').trim_end_matches(')');
        let mut pairs = Vec::new();
        for kv in inner.split(", ") {
            match kv.split_once('=') {
                Some((k, v)) => pairs.push((k.to_string(), v.to_string())),
                None => pairs.push(("_".into(), kv.to_string())),
            }
        }
        strata.push(Stratum { pairs });
    }
    strata
}

/// Render the parsed strata back into ANSI-styled text with:
/// 1. **Wrap-aware folding**: emit one stratum per line if
///    the joined single-line form would exceed
///    `available_width`, with continuation lines indented by
///    `continuation_indent`.
/// 2. **Change highlight**: any `key=value` pair whose value
///    differs from the same `(stratum_idx, key)` in
///    `prev_strata` renders the value in a different colour
///    so the changed axis pops out visually at a glance.
///
/// Coloration palette:
/// - Surrounding `(`/`)` and `key=` text: bold yellow (the
///   prior render's base style for the whole coords block).
/// - Unchanged values: bold yellow (same as the key,
///   blends in).
/// - Changed values: bold magenta (distinct from the
///   yellow base, doesn't clash with the green ✓ or blue
///   `[name]` of the surrounding line).
/// - `, ` separators within a stratum: bold yellow.
/// - `, ` separator between strata: dim (less visually
///   noisy when there are many strata).
fn render_strata(
    strata: &[Stratum],
    prev_strata: &[Stratum],
    color: bool,
    head_consumed: usize,
    available_width: usize,
    continuation_indent: &str,
) -> String {
    if strata.is_empty() {
        return String::new();
    }
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let magenta = if color { "\x1b[35m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    // Render each stratum twice: once styled (for emission)
    // and once plain (for visible-width measurement). The
    // visible widths drive wrap decisions; the styled forms
    // drive what reaches the terminal.
    let mut rendered: Vec<(String, usize)> = Vec::with_capacity(strata.len());
    for (idx, s) in strata.iter().enumerate() {
        let prior_pairs: Option<&Vec<(String, String)>> = prev_strata.get(idx).map(|p| &p.pairs);
        let mut styled = String::with_capacity(64);
        let mut plain = String::with_capacity(64);
        styled.push_str(bold);
        styled.push_str(yellow);
        styled.push('(');
        plain.push('(');
        for (pi, (k, v)) in s.pairs.iter().enumerate() {
            if pi > 0 {
                styled.push_str(", ");
                plain.push_str(", ");
            }
            // Compare against the prior render's value at the
            // same (stratum, key) coordinate. A missing prior
            // (first phase of the run, or a deeper stratum
            // that didn't exist last time) is treated as
            // "changed" so the operator sees the new context.
            let changed = match prior_pairs {
                Some(prior) => prior
                    .iter()
                    .find(|(pk, _)| pk == k)
                    .map(|(_, pv)| pv != v)
                    .unwrap_or(true),
                None => true,
            };
            write!(styled, "{k}=").ok();
            plain.push_str(k);
            plain.push('=');
            if changed {
                // Drop yellow → magenta for the value, then
                // restore so subsequent text stays styled.
                write!(styled, "{reset}{bold}{magenta}{v}{reset}{bold}{yellow}").ok();
            } else {
                styled.push_str(v);
            }
            plain.push_str(v);
        }
        styled.push(')');
        styled.push_str(reset);
        plain.push(')');
        rendered.push((styled, plain.chars().count()));
    }

    // Greedy line-fill: place strata on the current line as
    // long as each addition fits within `available_width`;
    // when it would overflow, break to a new continuation
    // line. Two wrap branches:
    //
    //   1. *Before* the first stratum, when even with no
    //      separator the head + this stratum's plain width
    //      would overflow → start the coord block on a fresh
    //      continuation line ("block mode" for the coords).
    //      This is the case the active-phase status renderer
    //      hits when the per-cell coord chain has wide
    //      strata (source_model + every CREATE INDEX option
    //      value spelled out) — without this branch the
    //      first stratum would go inline and then get
    //      per-row clamped to `…` by the surface's terminal-
    //      width clamp.
    //   2. *Between* strata, when adding the next stratum's
    //      width to the running line would overflow → fold
    //      the next stratum onto its own continuation line.
    //
    // Strata are NEVER themselves split — a single stratum
    // wider than `available_width` lands on its own line and
    // is allowed to overflow (better than mangling its
    // `(k=v, ...)` shape mid-pair). The status renderer's
    // per-row clamp still applies as a last-resort safety
    // net for the truly-pathological "one stratum is wider
    // than the terminal" case.
    let mut out = String::new();
    let sep_plain_len = 2; // visible chars of `, `
    let cont_indent_width = continuation_indent.chars().count();
    let mut current_visible = head_consumed;
    let mut wrote_any = false;
    for (styled, plain_len) in rendered.iter() {
        let needs_sep = wrote_any;
        let next_width = if needs_sep {
            current_visible + sep_plain_len + plain_len
        } else {
            current_visible + plain_len
        };
        // First-stratum "block-mode" wrap: the head already
        // consumed some columns; if even the first stratum
        // doesn't fit alongside, kick it to a fresh line.
        let first_stratum_overflows = !wrote_any
            && current_visible + plain_len > available_width
            // Don't gratuitously break to a continuation
            // line when the stratum itself is already too
            // wide for the continuation line — the wrap
            // wouldn't help, just produce two truncated
            // lines instead of one.
            && cont_indent_width + plain_len < available_width;
        if first_stratum_overflows {
            out.push('\n');
            out.push_str(continuation_indent);
            out.push_str(styled);
            current_visible = cont_indent_width + plain_len;
        } else if wrote_any && next_width > available_width {
            // Inter-stratum wrap. Comma at line-end signals
            // continuation to the reader; no trailing space
            // (the indent covers visual separation).
            out.push(',');
            out.push('\n');
            out.push_str(continuation_indent);
            out.push_str(styled);
            current_visible = cont_indent_width + plain_len;
        } else {
            if needs_sep {
                out.push_str(dim);
                out.push_str(", ");
                out.push_str(reset);
                current_visible += sep_plain_len;
            }
            out.push_str(styled);
            current_visible += plain_len;
        }
        wrote_any = true;
    }
    out
}

/// Best-effort terminal width with a sensible floor. Reads
/// `terminal_cols()` (TIOCGWINSZ on stderr) and falls back
/// to 120 when stderr isn't a TTY (piped output, CI). The
/// 120-column fallback matches the prior unwrapped-width
/// assumption — wider terminals get a longer single line
/// before wrap fires.
fn current_terminal_cols() -> usize {
    crate::activity::terminal_cols().unwrap_or(120).max(40)
}

/// Filter strata to only the pairs whose values changed
/// against the prior render. SRD-? "summarise completed
/// phases to only the coords that took a NEW value at this
/// phase" — the user said the scope-open lines above the
/// completion line already establish the unchanged context;
/// duplicating it on the `✓` line is just noise.
///
/// Returns an empty Vec when nothing changed (the caller
/// can omit the coords block entirely for the no-op-axis
/// case — every cell in the sweep with literally the same
/// coords as the prior one — common at session start).
fn strata_diff(strata: &[Stratum], prev_strata: &[Stratum]) -> Vec<Stratum> {
    if prev_strata.is_empty() {
        // First-phase render: every coord is "new" relative
        // to the empty prior. Returning the full set
        // preserves the operator's first-look context.
        return strata.to_vec();
    }
    let mut out: Vec<Stratum> = Vec::with_capacity(strata.len());
    for (idx, s) in strata.iter().enumerate() {
        let prior = prev_strata.get(idx);
        let mut changed_pairs: Vec<(String, String)> = Vec::new();
        for (k, v) in &s.pairs {
            let unchanged = match prior {
                Some(p) => p
                    .pairs
                    .iter()
                    .find(|(pk, _)| pk == k)
                    .map(|(_, pv)| pv == v)
                    .unwrap_or(false),
                None => false,
            };
            if !unchanged {
                changed_pairs.push((k.clone(), v.clone()));
            }
        }
        if !changed_pairs.is_empty() {
            out.push(Stratum {
                pairs: changed_pairs,
            });
        }
    }
    out
}

impl Clone for Stratum {
    fn clone(&self) -> Self {
        Self {
            pairs: self.pairs.clone(),
        }
    }
}

/// Format the coords block with wrap-folding + per-pair
/// change highlight. Always returns a string that begins
/// with a leading space (so the caller's format string can
/// concatenate it after `[name]` without conditional logic).
/// Empty labels collapse to "" — caller renders nothing.
///
/// `summarize_changed_only` controls which pairs are
/// rendered:
/// - `false` — emit every stratum / every pair (the
///   "active phase" display where the operator wants the
///   full context).
/// - `true`  — emit ONLY the pairs whose values changed
///   relative to the previous-rendered coords (the
///   "completed phase" summary; the surrounding scope-open
///   lines already establish the unchanged context).
///
/// Visible across the readouts module so the `phase_status`
/// (active-phase) renderer and `phase_outcome` (completed-
/// phase) renderer share one implementation — both
/// surfaces format the SAME canonical coord-path string;
/// only the summarize flag and head-consumed accounting
/// differ.
pub(crate) fn format_coords_block(
    labels: &str,
    color: bool,
    head_consumed: usize,
    continuation_indent: &str,
    summarize_changed_only: bool,
) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let strata = parse_strata(labels);
    let prev = LAST_RENDERED_COORDS
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    let prev_strata = parse_strata(&prev);
    let display_strata: Vec<Stratum> = if summarize_changed_only {
        strata_diff(&strata, &prev_strata)
    } else {
        strata.clone()
    };
    // Only advance the tracker when this is the COMPLETED-
    // phase summary render (`summarize_changed_only ==
    // true`). The ACTIVE-phase status line (`false`) fires
    // at every live-status tick during one phase
    // activation; if it advanced the tracker each tick, the
    // subsequent `phase_outcome` call would diff against the
    // active phase's own labels and the summary would
    // collapse to nothing. The tracker advances at phase
    // boundaries only — `phase_outcome` is the canonical
    // phase-end event, and its single call per phase is
    // where the "what's changed since the last
    // *completed* phase" lens gets fixed.
    if summarize_changed_only && let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
        *g = labels.to_string();
    }
    if display_strata.is_empty() {
        // Nothing changed — completion line elides the
        // coords block entirely. The leading space is
        // suppressed too: callers concatenating
        // `{coords}` get a single-space-separated layout
        // when there ARE coords, and a clean joined layout
        // when there aren't.
        return String::new();
    }
    let width = current_terminal_cols();
    let available = width.saturating_sub(1);
    let body = render_strata(
        &display_strata,
        &prev_strata,
        color,
        head_consumed.saturating_add(1),
        available,
        continuation_indent,
    );
    let mut out = String::with_capacity(body.len() + 1);
    out.push(' ');
    out.push_str(&body);
    out
}

/// SRD-76 — single phase-end readout. Reads the structured
/// outcome (status, duration, error list, resume cursor)
/// from the [`ReadoutContext`] and renders LOD-appropriate
/// detail. Replaces the legacy `phase_outcome` readout that
/// always assumed success.
///
/// Naming: the readout struct is `PhaseOutcomeReadout`
/// rather than `PhaseOutcome` to disambiguate from
/// [`crate::phase_outcome::PhaseOutcome`] (the data type).
/// The string registry key is `"phase_outcome"`.
pub struct PhaseOutcomeReadout;

impl Readout for PhaseOutcomeReadout {
    fn name(&self) -> &'static str {
        "phase_outcome"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Phase]
    }

    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        _opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize {
        match (lod, mode) {
            (Lod::Compact, ContentMode::Value) => render_compact_value(ctx, out),
            (Lod::Compact, ContentMode::Explanation) => render_compact_explanation(ctx, out),
            (Lod::Labeled, ContentMode::Value) => render_labeled_value(ctx, out),
            (Lod::Labeled, ContentMode::Explanation) => render_labeled_explanation(ctx, out),
            (Lod::Expanded, ContentMode::Value) => render_expanded_value(ctx, out),
            (Lod::Expanded, ContentMode::Explanation) => render_expanded_explanation(ctx, out),
        }
    }
}

// ── SRD-76 status helpers ─────────────────────────────────

/// ANSI colour token for a phase status:
/// - `Completed` → green
/// - `Failed` → red
/// - `Skipped` → dim (subdued, not a problem)
/// - `CursorSuspended` → yellow (partial — worth noticing)
///
/// Returns empty strings when colour is off so the same
/// format template covers both colour and no-colour modes.
fn status_color(outcome: &crate::phase_outcome::Outcome, color: bool) -> &'static str {
    if !color {
        return "";
    }
    use crate::phase_outcome::{Disposition, Validity};
    match (outcome.disposition, outcome.validity) {
        (_, Validity::Failed) => "\x1b[31m",    // red
        (Disposition::Skipped, _) => "\x1b[2m", // dim
        (Disposition::Interrupted, Validity::Succeeded) => "\x1b[33m", // yellow (re-usable partial)
        _ => "\x1b[32m",                        // green
    }
}

// ── Compact LOD ───────────────────────────────────────────

/// Compact LOD: `{depth}<glyph> [name] {pct}% ({elapsed:.2}s)`.
/// Glyph + color come from the SRD-76 [`PhaseStatus`]:
/// Completed → ✓ (green), Failed → ✗ (red),
/// Skipped → ~ (dim), CursorSuspended → … (yellow).
/// Trained-operator scan form: status glyph + identity +
/// completion percentage + wallclock. Per §3.3
/// monotonicity, every field is present in Labeled.
/// The completion METER SLOT (SRD-92): ` NN%` from the single
/// fraction source, ` 100%` for a bounded subject with no computable
/// fraction (it finished), and EMPTY for an open-ended subject —
/// there is no "done" to meter on a stopped daemon, same rule the
/// live meter slot follows (the cycles basis printed a meaningless
/// percentage against the daemon's wall-clock ceiling).
fn completion_meter(ctx: &dyn ReadoutContext) -> String {
    if ctx.open_ended() {
        return String::new();
    }
    format!(
        " {:.0}%",
        ctx.progress_fraction().map(|f| f * 100.0).unwrap_or(100.0)
    )
}

fn render_compact_value(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let outcome = ctx.outcome();
    let glyph_color = status_color(&outcome, color);
    let glyph = outcome.glyph();

    // Fraction basis via ctx.progress_fraction(): rows for batched
    // phases (cycles basis showed "1%" for a completed stride-100
    // batch load), cycles otherwise, override first; empty for
    // open-ended subjects.
    let meter = completion_meter(ctx);
    let elapsed = ctx.elapsed_secs();
    let depth_indent = ctx.depth_indent();
    let name = ctx.subject_name();

    let mut tmp = String::with_capacity(64);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{glyph_color}{glyph}{reset} {bold}{blue}[{name}]{reset}\
{meter} {dim}({elapsed:.2}s){reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Compact LOD explanation overlay. Same skeleton as the
/// value form, with field tokens swapped for descriptors.
fn render_compact_explanation(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let depth_indent = ctx.depth_indent();
    let mut tmp = String::with_capacity(96);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{green}done{reset} {bold}{blue}[phase-name]{reset} \
progress% {dim}(elapsed){reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

// ── Expanded LOD ──────────────────────────────────────────

/// Expanded LOD: multi-line labelled block. Same data as
/// Labeled, organised one field per line. Per §3.3
/// monotonicity, every Labeled field is here too.
fn render_expanded_value(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let red = if color { "\x1b[31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let outcome = ctx.outcome();
    let glyph_color = status_color(&outcome, color);
    let glyph = outcome.glyph();
    // SRD-76 — when the phase has recorded errors, append a
    // per-error block at the bottom of the Expanded LOD. This
    // gives operators full per-error detail without leaving
    // the realtime status surface, and matches what `nmbrs
    // replay --errors` shows for the same phase.
    let outcome_errors = ctx.outcome_errors();

    let cycles = ctx.cycles_completed();
    let errors = ctx.errors();
    let retries = ctx.retries();
    let ok = ctx.ops_ok();
    let skips = ctx.skips();
    let concurrency = ctx.concurrency();
    let elapsed = ctx.elapsed_secs();
    let consumed = ctx.consumed();
    let total_extent = ctx.cycles_total();

    // ok% excludes SKIPS — a skipped (`if:`-gated) op is neither a
    // success nor a failure (cycles == result_total + skips).
    // `.max(ok)`: cycles and result_success are read non-atomically and
    // bumped cycles-first, so this can momentarily dip below `ok` — it is
    // never truly < ok. Clamp up so ok% stays <= 100%.
    let result_total = cycles.saturating_sub(skips).max(ok);
    let ok_pct: f64 = if result_total > 0 {
        ok as f64 * 100.0 / result_total as f64
    } else {
        100.0
    };
    // Open-ended subjects have no completion fraction — the progress
    // row shows the bare cycle count instead of a fabricated percent.
    let progress_cell: String = if ctx.open_ended() {
        format!("— (open-ended, {cycles} cycles)")
    } else {
        let pct: f64 = ctx.progress_fraction().map(|f| f * 100.0).unwrap_or(100.0);
        format!("{pct:.0}% ({cycles} of {total_extent})")
    };
    let rate: f64 = if elapsed > 0.0 {
        consumed as f64 / elapsed
    } else {
        0.0
    };
    let rate_str = format_rate(rate);

    let err_color = if errors > 0 || retries > 0 {
        yellow
    } else {
        dim
    };
    let labels = ctx.subject_labels();
    let depth_indent = ctx.depth_indent();
    // Margin owns [n/N] (single-placement rule) — body omits it.
    let seq_part: String = String::new();
    // SRD-? — Expanded LOD: each row already labelled
    // (`coords:`), so the wrap-aware folder gets generous
    // available-width by passing `head_consumed == coords_label_width`.
    // Use the completed-phase summary lens (only-changed
    // strata) — matches the Labeled LOD's contract; the
    // multi-line block here is the operator's deep-dive form
    // for ONE phase, not a cross-phase diff surface.
    // Continuation indent aligns under the value column
    // (matches the other rows' `  <field>: ` prefix).
    let coords_continuation_indent = format!("{depth_indent}               ");
    let coords_head_consumed = depth_indent.chars().count() + 14; // "  coords:      "
    let coords_payload = format_coords_block(
        labels,
        color,
        coords_head_consumed,
        &coords_continuation_indent,
        /* summarize_changed_only */ true,
    );
    let coords_line = if coords_payload.is_empty() {
        String::new()
    } else {
        // `format_coords_block` returned a leading-space
        // payload; place it after the `coords:` label.
        format!("\n{depth_indent}  coords:{coords_payload}")
    };
    let _ = (bold, yellow); // formerly used to wrap the raw labels; helper now owns styling
    // Chip stream: convert `name:value` chips into one per
    // line so the Expanded block reads vertically. Empty
    // chip strings render no metrics block.
    let chips_block = render_chips_block(&ctx.status_metric_chips(), depth_indent, dim, reset);

    let errors_block =
        render_outcome_errors_block(outcome_errors, errors, depth_indent, red, dim, reset);
    let mut tmp = String::with_capacity(384);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{glyph_color}{glyph}{reset} {bold}{blue}[{name}]{reset}{seq}{coords}\n\
{depth_indent}  status:      {glyph_color}{status_label}{reset}\n\
{depth_indent}  progress:    {progress_cell}\n\
{depth_indent}  throughput:  {rate_str}\n\
{depth_indent}  ok:          {ok_pct:.0}%  ({ok} of {result_total})\n\
{depth_indent}  reliability: {err_color}e:{errors} r:{retries}{reset}\n\
{depth_indent}  concurrency: {concurrency}\n\
{chips}\
{depth_indent}  elapsed:     {dim}{elapsed:.2}s{reset}\
{errors_block}",
        name = ctx.subject_name(),
        seq = seq_part,
        coords = coords_line,
        chips = chips_block,
        status_label = outcome.label(),
        errors_block = errors_block,
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// SRD-76 Expanded-LOD per-error block. Empty when the error
/// list is empty. Each error renders as a labelled multi-line
/// stanza:
///   `  errors:`
///   `    [<class>] <message>`
///   `      cycle: <n>`             (when populated)
///   `      op-template: <text>`    (when populated)
///   `      op-resolved: <text>`    (when populated)
fn render_outcome_errors_block(
    errors: &[crate::phase_outcome::PhaseErrorDetail],
    total: u64,
    indent: &str,
    red: &str,
    dim: &str,
    reset: &str,
) -> String {
    use std::fmt::Write as _;
    if errors.is_empty() {
        return String::new();
    }
    // Driver-supplied messages (CQL statement text, etc.) carry
    // embedded newlines; re-indent each continuation line to
    // nest under the surrounding block so the output doesn't
    // break out to column 0.
    let msg_continuation = format!("{indent}      ");
    let detail_continuation = format!("{indent}        ");
    let reindent = |s: &str, prefix: &str| -> String {
        let mut out = String::with_capacity(s.len() + prefix.len() * 2);
        let mut first = true;
        for line in s.split('\n') {
            if first {
                first = false;
            } else {
                out.push('\n');
                out.push_str(prefix);
            }
            out.push_str(line);
        }
        out
    };
    let mut out = String::with_capacity(128);
    let _ = write!(&mut out, "\n{indent}  errors:");
    for e in errors {
        let msg = reindent(&e.message, &msg_continuation);
        let _ = write!(
            &mut out,
            "\n{indent}    {red}[{class}]{reset} {msg}",
            class = e.class
        );
        if let Some(c) = e.cycle {
            let _ = write!(&mut out, "\n{indent}      {dim}cycle:{reset} {c}");
        }
        if let Some(t) = &e.op_template {
            let t = reindent(t, &detail_continuation);
            let _ = write!(&mut out, "\n{indent}      {dim}op-template:{reset} {t}");
        }
        if let Some(r) = &e.op_resolved {
            let r = reindent(r, &detail_continuation);
            let _ = write!(&mut out, "\n{indent}      {dim}op-resolved:{reset} {r}");
        }
    }
    // The capture buffer (PHASE_ERROR_CAPTURE_CAP) can fill before
    // all errors arrive; note the shortfall so the listed set isn't
    // mistaken for the complete record.
    let captured = errors.len() as u64;
    if captured < total {
        let _ = write!(
            &mut out,
            "\n{indent}    {dim}(+{} more occurred — not captured (buffer cap)){reset}",
            total - captured
        );
    }
    out
}

/// Format the chip stream as one chip per line under a
/// `metrics:` header. `chips` follows the convention from
/// `ActivityMetrics::collect_status_values`: leading-space-
/// separated `name:value` tokens. Returns empty when
/// `chips` is empty (the metrics block is skipped).
fn render_chips_block(chips: &str, indent: &str, dim: &str, reset: &str) -> String {
    let entries: Vec<&str> = chips.split_whitespace().collect();
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(64 + entries.len() * 24);
    let _ = writeln!(&mut out, "{indent}  metrics:");
    for chip in &entries {
        // Each chip is `name:value`; align the name in a
        // 16-char field so values columnise.
        let (name, value) = chip.split_once(':').unwrap_or((chip, ""));
        let _ = writeln!(&mut out, "{indent}    {name:<16} {dim}{value}{reset}");
    }
    out
}

/// Expanded LOD explanation overlay. Same multi-line shape
/// as the value form; field labels stay (they're already
/// descriptors), values are replaced with token names.
fn render_expanded_explanation(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let depth_indent = ctx.depth_indent();
    let mut tmp = String::with_capacity(384);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{green}done{reset} {bold}{blue}[phase-name]{reset}\n\
{depth_indent}  progress:    progress% (cycles_completed of cycles_total)\n\
{depth_indent}  throughput:  rate (auto-scaled K/s, M/s)\n\
{depth_indent}  ok:          ok-pct% (ops_ok of cycles_completed)\n\
{depth_indent}  reliability: e:errors r:retries\n\
{depth_indent}  concurrency: fiber count\n\
{depth_indent}  elapsed:     {dim}wallclock seconds{reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Explanation overlay (SRD-63 §3.2 / Push 7). Same
/// shape as the value render, with each glyph / cluster
/// replaced by text describing what it means. Width
/// parity is the readout author's contract — we keep
/// the structural skeleton (`✓`, `[name]`, percentages,
/// `e:`/`r:`/`c:` tail) and rewrite each token's *text*
/// to its meaning.
fn render_labeled_explanation(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let depth_indent = ctx.depth_indent();
    // Margin owns [n/N] (single-placement rule) — body omits it.
    let seq_part: String = String::new();
    let coords_part: String = if ctx.subject_labels().is_empty() {
        String::new()
    } else {
        format!(" {bold}{yellow}(scope-coords){reset}")
    };

    // Width-parity bars: pct → "100%", rate → "rate/s",
    // ok_pct → "ok-pct%", and so on. Each replacement is
    // *short* enough to overlay without wrapping; the
    // user's expectation is "what does this glyph mean?"
    // not "every detail spelled out."
    let mut tmp = String::with_capacity(160);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{green}done{reset} {seq}{bold}{blue}[phase-name]{reset}{coords} \
progress% throughput ok:ok% \
errors retries concurrency \
{dim}(elapsed){reset}",
        seq = seq_part,
        coords = coords_part,
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled_value(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    // SRD-76 — branch on terminal status. Failed phases
    // render the error-flavoured line (glyph + class + first
    // message + elapsed) instead of the success summary.
    // Skipped / interrupted-but-usable fall through the same
    // success-flavoured layout because the throughput /
    // ok-pct / counters telemetry is still meaningful (an
    // interrupted phase ran SOMETHING) and the glyph alone
    // communicates the non-Completed disposition. Only an
    // untrustworthy result gets the failure layout.
    let outcome = ctx.outcome();
    if outcome.is_failure() {
        return render_labeled_value_failed(ctx, out);
    }
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let glyph_color = status_color(&outcome, color);
    let glyph = outcome.glyph();

    let cycles = ctx.cycles_completed();
    let errors = ctx.errors();
    let retries = ctx.retries();
    let ok = ctx.ops_ok();
    let skips = ctx.skips();
    let concurrency = ctx.concurrency();
    let elapsed = ctx.elapsed_secs();
    let consumed = ctx.consumed();
    let total_extent = ctx.cycles_total();

    // ok% excludes SKIPS — a skipped (`if:`-gated) op is neither a
    // success nor a failure (cycles == result_total + skips).
    // `.max(ok)`: cycles and result_success are read non-atomically and
    // bumped cycles-first, so this can momentarily dip below `ok` — it is
    // never truly < ok. Clamp up so ok% stays <= 100%.
    let result_total = cycles.saturating_sub(skips).max(ok);
    // A phase where every op skipped has NO results to be ok about —
    // `ok:—` instead of a fabricated 100%, and the skip count shown
    // so the line reads as "gated off", not "measured clean".
    let ok_str: String = if result_total > 0 {
        format!("{:.0}%", ok as f64 * 100.0 / result_total as f64)
    } else if skips > 0 {
        "—".to_string()
    } else {
        "100%".to_string()
    };
    let skips_chip: String = if skips > 0 {
        format!(" {dim}skip:{skips}{reset}")
    } else {
        String::new()
    };
    // Fraction basis via ctx.progress_fraction(): rows for batched
    // phases (cycles basis showed "1%" for a completed stride-100
    // batch load), cycles otherwise, override first; empty for
    // open-ended subjects.
    let meter = completion_meter(ctx);
    let rate: f64 = if elapsed > 0.0 {
        consumed as f64 / elapsed
    } else {
        0.0
    };
    let rate_str = format_rate(rate);

    let err_color = if errors > 0 || retries > 0 {
        yellow
    } else {
        dim
    };

    let labels = ctx.subject_labels();
    let depth_indent = ctx.depth_indent();
    // Margin owns [n/N] (single-placement rule) — body omits it.
    let seq_part: String = String::new();
    // Per-cell completion bar for small phases — preserves
    // the per-op success/failure visibility from `phase_status`
    // through to the completion line so the operator can see
    // WHICH ops succeeded or failed, not just an aggregate
    // percentage. Width matches phase_status's variant
    // (1 leading space + N glyphs).
    let bar = if total_extent > 0 && total_extent <= 10 {
        let bg = if color { "\x1b[48;2;50;50;50m" } else { "" };
        let fg = if color { "\x1b[97m" } else { "" };
        format!(" {bg}{fg}{}{reset}", ballot_bar(total_extent, ok, errors))
    } else {
        String::new()
    };
    let bar_visible: usize = if total_extent > 0 && total_extent <= 10 {
        1 + total_extent as usize
    } else {
        0
    };

    // Estimate visible columns consumed by the head of the
    // line prior to the coords block — used to drive wrap
    // decisions inside `format_coords_block`. Composition:
    //   depth_indent (variable) + "✓ " (2) + ballot bar
    //   (when ≤10) + seq prefix visible chars ("[N/M] " when
    //   seq is Some) + "[<name>]" length.
    let name = ctx.subject_name();
    let seq_visible: usize = match ctx.subject_seq() {
        Some((s, t)) => format!("[{s}/{t}] ").chars().count(),
        None => 0,
    };
    let head_consumed: usize = depth_indent.chars().count()
        + 2  // ✓ + space
        + bar_visible
        + seq_visible
        + 2  // [ and ]
        + name.chars().count();
    // Wrap continuation lands at the same depth indent +
    // 2 spaces (alignment with the inner-block / chips line
    // already used in this layout).
    let continuation_indent = format!("{depth_indent}  ");
    // SRD-? "completed phase ✓ line shows only the coords
    // that took a new value here" — the scope-open lines
    // above the completion already establish the
    // unchanged context.
    let coords_part = format_coords_block(
        labels,
        color,
        head_consumed,
        &continuation_indent,
        /* summarize_changed_only */ true,
    );
    let chips = ctx.status_metric_chips();

    // Memo row (if any) — see phase_status for rationale.
    // The memo carries the latest published state at phase
    // end; useful when a phase's last activity (e.g. "compacted
    // table_X") is the takeaway the operator needs. SRD-92:
    // header-first composition — the memo renders as a detail
    // row under the ✓/⊘ head line, never as a banner above it.
    let memo = ctx.phase_memo();
    let memo_row = if memo.is_empty() {
        String::new()
    } else {
        let bold_yellow = if color { "\x1b[1;33m" } else { "" };
        format!("{depth_indent}    {bold_yellow}[[ {memo} ]]{reset}\n")
    };

    // FULLY-SKIPPED phase (`skipped_phases=mark`, the default): every
    // cycle was `if:`-gated off — the completion must read as "gated
    // off", not as a measurement. One unmistakable line pair: the ⊘
    // glyph plus the skip count; no rate, no ok%, no pct (nothing was
    // measured). `elide`/`prune` never reach this renderer — the fire
    // site suppresses the readout entirely for those modes.
    let fully_skipped = skips > 0 && cycles > 0 && skips >= cycles && ok == 0 && errors == 0;
    if fully_skipped
        && crate::observer::skipped_phase_display() == crate::observer::SkippedPhaseDisplay::Mark
    {
        let mut tmp = String::with_capacity(160);
        let _ = write!(
            &mut tmp,
            "{depth_indent}{dim}⊘{reset} {seq}{bold}{blue}[{name}]{reset}{coords} {dim}gated off{reset}\n\
{memo_row}\
{depth_indent}    {dim}skip:{skips} c:{concurrency} ({elapsed:.2}s){reset}",
            memo_row = memo_row,
            depth_indent = depth_indent,
            dim = dim,
            reset = reset,
            seq = seq_part,
            bold = bold,
            blue = blue,
            name = ctx.subject_name(),
            coords = coords_part,
            skips = skips,
            concurrency = concurrency,
            elapsed = elapsed,
        );
        let len = tmp.len();
        let _ = out.write_str(&tmp);
        return len;
    }

    // Two-line layout mirroring `phase_status` Labeled:
    //   line 1: {depth}✓ {seq}[{name}]{coords} {pct}%
    //   line 2: {depth}    {rate} ok:{ok}% e:{e} r:{r} c:{c}{chips} (elapsed)
    // Break after the progress percentage keeps the head row
    // narrow (status glyph + identity + completion) and lets
    // the tail carry all the throughput/counter detail
    // without exceeding terminal width.
    let mut tmp = String::with_capacity(256);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{glyph_color}{glyph}{reset}{bar} {seq}{bold}{blue}[{name}]{reset}{coords}{meter}\n\
{memo_row}\
{depth_indent}    {rate_str} ok:{ok_str} \
{err_color}e:{errors} r:{retries}{reset}{skips_chip} c:{concurrency}{chips} \
{dim}({elapsed:.2}s){reset}",
        depth_indent = depth_indent,
        glyph_color = glyph_color,
        glyph = glyph,
        bar = bar,
        reset = reset,
        seq = seq_part,
        bold = bold,
        blue = blue,
        name = ctx.subject_name(),
        coords = coords_part,
        meter = meter,
        rate_str = rate_str,
        ok_str = ok_str,
        skips_chip = skips_chip,
        err_color = err_color,
        errors = errors,
        retries = retries,
        concurrency = concurrency,
        chips = chips,
        dim = dim,
        elapsed = elapsed,
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// SRD-76 — Labeled rendering for `PhaseStatus::Failed`.
/// Two-line layout matching the success variant's shape so
/// surrounding context (depth indent, coord folding, seq
/// prefix) stays consistent; line 1 carries the status glyph
/// + name + coords + first-error class summary, line 2 the
///   first-error message + elapsed.
///
/// Expanded LOD picks up every error in the list; this
/// Labeled form intentionally surfaces only the FIRST error
/// (the one most likely to be the proximate cause) so the
/// status surface stays line-bounded. Operators wanting the
/// full chronology pass `lod=expanded` or use `nmbrs replay
/// --errors`.
fn render_labeled_value_failed(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let red = if color { "\x1b[31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let labels = ctx.subject_labels();
    let depth_indent = ctx.depth_indent();
    let name = ctx.subject_name();
    // Margin owns [n/N] (single-placement rule) — body omits it.
    let seq_part: String = String::new();
    let seq_visible: usize = match ctx.subject_seq() {
        Some((s, t)) => format!("[{s}/{t}] ").chars().count(),
        None => 0,
    };
    // Ballot bar for small phases — same shape as the success
    // path. A failed ≤10-op phase still shows the per-op
    // success/failure breakdown so the operator sees how far
    // the phase got before the failure landed.
    let total_extent = ctx.cycles_total();
    let ok = ctx.ops_ok();
    let err_count = ctx.errors();
    let bar = if total_extent > 0 && total_extent <= 10 {
        let bg = if color { "\x1b[48;2;50;50;50m" } else { "" };
        let fg = if color { "\x1b[97m" } else { "" };
        format!(
            " {bg}{fg}{}{reset}",
            ballot_bar(total_extent, ok, err_count)
        )
    } else {
        String::new()
    };
    let bar_visible: usize = if total_extent > 0 && total_extent <= 10 {
        1 + total_extent as usize
    } else {
        0
    };
    let head_consumed: usize = depth_indent.chars().count()
        + 2  // ✗ + space
        + bar_visible
        + seq_visible
        + 2  // [ and ]
        + name.chars().count();
    let continuation_indent = format!("{depth_indent}  ");
    let coords_part = format_coords_block(
        labels,
        color,
        head_consumed,
        &continuation_indent,
        /* summarize_changed_only */ true,
    );

    let errors = ctx.outcome_errors();
    let first = errors.first();
    let class_label = first.map(|e| e.class.as_str()).unwrap_or("phase_failed");
    // First line only: the footer is a status row, and the full
    // enriched text renders once in the `errors:` block (SRD-82
    // §one full render).
    let message = first
        .map(|e| e.message.lines().next().unwrap_or(e.message.as_str()))
        .unwrap_or("unknown error");
    let elapsed = ctx.elapsed_secs();
    // `+N more` is relative to the TRUE error count (`err_count =
    // ctx.errors()`, the uncapped `errors_total`), not the capped
    // `outcome_errors` buffer length — otherwise a 200-error phase
    // reads "+63 more" when 199 more actually occurred.
    let extra_count = err_count.saturating_sub(1);
    let extra_suffix = if extra_count > 0 {
        format!(" {dim}(+{extra_count} more){reset}")
    } else {
        String::new()
    };

    let mut tmp = String::with_capacity(192);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{red}✗{reset}{bar} {seq_part}{bold}{blue}[{name}]{reset}{coords_part} \
{red}{class_label}{reset}\n\
{depth_indent}    {red}{message}{reset}{extra_suffix} {dim}({elapsed:.2}s){reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn format_rate(rate: f64) -> String {
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
    use crate::readouts::buf::StringBuf;

    // ── coord-stack rendering tests (SRD-? coords stack
    //    folding + change highlight + completed-phase
    //    summarisation) ──────────────────────────────────────

    /// Tests that touch `LAST_RENDERED_COORDS` share a
    /// serial guard so cargo-test's parallel runner doesn't
    /// interleave their state mutations. The production
    /// code path doesn't need this lock — realtime readout
    /// dispatch is single-threaded — but the test
    /// harness's per-#[test] task pool is multi-threaded.
    static SERIAL_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `parse_strata` round-trips the canonical striated-
    /// parens form into per-stratum / per-pair structure.
    #[test]
    fn parse_strata_round_trips_two_strata() {
        let s = parse_strata("(profile=default), (sm=OTHER, mnc=8)");
        assert_eq!(s.len(), 2);
        assert_eq!(
            s[0].pairs,
            vec![("profile".to_string(), "default".to_string())]
        );
        assert_eq!(
            s[1].pairs,
            vec![
                ("sm".to_string(), "OTHER".to_string()),
                ("mnc".to_string(), "8".to_string()),
            ]
        );
    }

    /// `parse_strata` returns an empty Vec for the empty
    /// input — callers can treat "no coords" as the absent
    /// case without an Option.
    #[test]
    fn parse_strata_empty_input_yields_empty() {
        assert_eq!(parse_strata("").len(), 0);
    }

    /// `strata_diff` keeps only the (key, value) pairs whose
    /// value changed against the prior render, dropping
    /// entire strata that became empty after filtering.
    #[test]
    fn strata_diff_keeps_only_changed_pairs() {
        let prev = parse_strata("(profile=default), (sm=OTHER, mnc=8)");
        let curr = parse_strata("(profile=default), (sm=ADA002, mnc=8)");
        let d = strata_diff(&curr, &prev);
        // profile stratum unchanged → dropped entirely.
        // sm changed, mnc unchanged → only sm survives.
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].pairs, vec![("sm".to_string(), "ADA002".to_string())]);
    }

    /// First-phase render (empty prior) is treated as
    /// "everything is new" — preserves the operator's
    /// initial-look context.
    #[test]
    fn strata_diff_empty_prior_treats_all_as_changed() {
        let curr = parse_strata("(profile=default), (sm=OTHER)");
        let d = strata_diff(&curr, &[]);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].pairs.len(), 1);
        assert_eq!(d[1].pairs.len(), 1);
    }

    /// `format_coords_block` with `summarize_changed_only`
    /// drops unchanged strata between sequential renders.
    /// The internal LAST_RENDERED_COORDS tracker advances
    /// after each call so each subsequent render diffs
    /// against the previous one.
    #[test]
    fn format_coords_block_completed_phase_summary_elides_unchanged() {
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Reset the global tracker so this test is
        // order-independent (the global persists between
        // tests if `cargo test` runs them in the same
        // process). Acquiring the lock + clearing matches
        // the production tracker's reset semantics.
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = String::new();
        }
        let labels_a = "(profile=default), (sm=OTHER, mnc=8)";
        let labels_b = "(profile=default), (sm=ADA002, mnc=8)";

        let first = format_coords_block(
            labels_a, /* color */ false, 0, "  ", /* summarize_changed_only */ true,
        );
        // First call diffs against empty prior → every coord
        // appears.
        assert!(first.contains("profile=default"));
        assert!(first.contains("sm=OTHER"));
        assert!(first.contains("mnc=8"));

        let second = format_coords_block(
            labels_b, /* color */ false, 0, "  ", /* summarize_changed_only */ true,
        );
        // Only `sm=ADA002` changed → the second render
        // elides the unchanged ones.
        assert!(
            second.contains("sm=ADA002"),
            "changed pair missing in second render: {second:?}"
        );
        assert!(
            !second.contains("profile=default"),
            "unchanged profile stratum should be elided: {second:?}"
        );
        assert!(
            !second.contains("mnc=8"),
            "unchanged mnc should be elided: {second:?}"
        );
    }

    /// Active-phase render (summarize_changed_only=false)
    /// shows the full coord stack even when nothing has
    /// changed since the prior render.
    #[test]
    fn format_coords_block_active_phase_shows_full_stack() {
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = "(profile=default)".into();
        }
        let labels = "(profile=default)";
        let body = format_coords_block(
            labels, /* color */ false, 0, "  ", /* summarize_changed_only */ false,
        );
        assert!(
            body.contains("profile=default"),
            "active-phase render should show unchanged coords too: {body:?}"
        );
    }

    /// When the head + first stratum would overflow the
    /// available width, the renderer breaks the FIRST
    /// stratum to a continuation line ("block mode" for
    /// coords). Without this, the active-phase status
    /// renderer's per-row terminal-width clamp would
    /// truncate the coord chain to `…` instead of folding.
    #[test]
    fn first_stratum_wraps_to_continuation_when_head_plus_first_overflows() {
        // Direct render_strata call (not through the global
        // tracker) so the test is deterministic regardless
        // of test ordering.
        let strata = parse_strata(
            "(source_model=OTHER, maximum_node_connections=8, construction_beam_width=50)",
        );
        let prev: Vec<Stratum> = Vec::new();
        // head_consumed=40 simulates spinner + bar + seq +
        // `[ensure_compacted]` width. available_width=80
        // (short terminal for the test). The first stratum
        // is ~73 chars; 40 + 73 = 113 > 80 → must wrap.
        let out = render_strata(
            &strata, &prev, /* color */ false, /* head_consumed */ 40,
            /* available_width */ 80, /* continuation_indent */ "  ",
        );
        assert!(
            out.starts_with('\n'),
            "first-stratum overflow should start with a newline; \
             got: {out:?}"
        );
        assert!(
            out.contains("source_model=OTHER"),
            "first stratum content should still appear: {out:?}"
        );
        // No comma-at-end-of-line wrapping marker on the
        // first-stratum-overflow path (the comma is only
        // inserted between strata to signal continuation;
        // when the very first stratum is the overflow, the
        // newline is the only delimiter).
        assert!(
            !out.starts_with(',') && !out.contains(",\n  ("),
            "first-stratum overflow path should not produce a comma-wrap; \
             got: {out:?}"
        );
    }

    /// Repeated active-phase renders during ONE phase
    /// activation must NOT advance the
    /// `LAST_RENDERED_COORDS` tracker. Pinning this
    /// invariant ensures the subsequent completed-phase
    /// summary still diffs against the prior COMPLETED
    /// phase's coords — not against the active phase's
    /// own labels (which would collapse the summary to
    /// nothing).
    #[test]
    fn format_coords_block_active_render_does_not_advance_tracker() {
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Seed the tracker with the previous completed
        // phase's coords.
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = "(profile=default), (sm=OTHER)".into();
        }
        let active_labels = "(profile=default), (sm=ADA002)";
        // Multiple active-phase ticks (tick rate ~1Hz in
        // production) — should all see the same prior
        // and render the same diff highlight without
        // advancing the tracker.
        for _ in 0..5 {
            let _ = format_coords_block(
                active_labels,
                /* color */ false,
                0,
                "  ",
                /* summarize_changed_only */ false,
            );
            let g = LAST_RENDERED_COORDS.lock().expect("lock");
            assert_eq!(
                g.as_str(),
                "(profile=default), (sm=OTHER)",
                "active-phase render must NOT advance the tracker"
            );
        }
        // The completed-phase render with the same labels
        // advances the tracker.
        let _ = format_coords_block(
            active_labels,
            /* color */ false,
            0,
            "  ",
            /* summarize_changed_only */ true,
        );
        let g = LAST_RENDERED_COORDS.lock().expect("lock");
        assert_eq!(
            g.as_str(),
            active_labels,
            "completed-phase render must advance the tracker"
        );
    }

    /// When NO coords changed between two completed phases,
    /// the rendered block is the empty string (no leading
    /// space, no parens, no separator) — the line collapses
    /// to just `✓ [name] 100%` with the chip / latency line
    /// below.
    #[test]
    fn format_coords_block_no_change_collapses_to_empty() {
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = String::new();
        }
        let labels = "(profile=default)";
        // Prime the tracker.
        let _ = format_coords_block(labels, false, 0, "  ", true);
        // Re-render the same coords — should be empty.
        let body = format_coords_block(labels, false, 0, "  ", true);
        assert_eq!(
            body, "",
            "no-change render should collapse to empty: {body:?}"
        );
    }

    /// Tiny in-test context that lets us hand-pick every
    /// field. Lives here so the `phase_outcome` golden can run
    /// without pulling in `nmbrs-runtime`.
    struct TestCtx {
        phase_name: String,
        phase_seq: Option<(usize, usize)>,
        phase_labels: String,
        cycles_completed: u64,
        cycles_total: u64,
        ops_ok: u64,
        skips: u64,
        errors: u64,
        retries: u64,
        concurrency: usize,
        elapsed_secs: f64,
        consumed: u64,
        chips: String,
        depth_indent: String,
        use_color: bool,
        outcome: crate::phase_outcome::Outcome,
        outcome_errors: Vec<crate::phase_outcome::PhaseErrorDetail>,
    }

    impl Default for TestCtx {
        fn default() -> Self {
            Self {
                phase_name: String::new(),
                phase_seq: None,
                phase_labels: String::new(),
                cycles_completed: 0,
                cycles_total: 0,
                ops_ok: 0,
                skips: 0,
                errors: 0,
                retries: 0,
                concurrency: 0,
                elapsed_secs: 0.0,
                consumed: 0,
                chips: String::new(),
                depth_indent: String::new(),
                use_color: false,
                outcome: crate::phase_outcome::Outcome::completed(),
                outcome_errors: Vec::new(),
            }
        }
    }

    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            &self.phase_name
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            self.phase_seq
        }
        fn subject_labels(&self) -> &str {
            &self.phase_labels
        }
        fn cycles_completed(&self) -> u64 {
            self.cycles_completed
        }
        fn cycles_total(&self) -> u64 {
            self.cycles_total
        }
        fn ops_ok(&self) -> u64 {
            self.ops_ok
        }
        fn skips(&self) -> u64 {
            self.skips
        }
        fn errors(&self) -> u64 {
            self.errors
        }
        fn retries(&self) -> u64 {
            self.retries
        }
        fn concurrency(&self) -> usize {
            self.concurrency
        }
        fn elapsed_secs(&self) -> f64 {
            self.elapsed_secs
        }
        fn consumed(&self) -> u64 {
            self.consumed
        }
        fn status_metric_chips(&self) -> String {
            self.chips.clone()
        }
        fn depth_indent(&self) -> &str {
            &self.depth_indent
        }
        fn use_color(&self) -> bool {
            self.use_color
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::PhaseEnd
        }
        fn outcome(&self) -> crate::phase_outcome::Outcome {
            self.outcome.clone()
        }
        fn outcome_errors(&self) -> &[crate::phase_outcome::PhaseErrorDetail] {
            &self.outcome_errors
        }
    }

    /// Helper that renders a `PhaseOutcomeReadout` at the labeled
    /// LOD against `ctx`. Resets the
    /// [`LAST_RENDERED_COORDS`] tracker before invoking so
    /// each test sees the first-phase-of-session lens
    /// (every coord renders as "new"). Tests that need
    /// cross-render diffing call `format_coords_block`
    /// directly and manage the tracker themselves.
    fn render(ctx: &TestCtx) -> String {
        // Test-only serial guard + tracker reset so the
        // existing pre-SRD-? snapshot assertions
        // (e.g. `no_color_with_coords_and_chips` expecting
        // the full coord chain to appear) stay stable
        // regardless of whether a prior test in the same
        // process advanced the tracker.
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = String::new();
        }
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseOutcomeReadout.render(
            ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn no_color_no_coords_no_chips() {
        // cycles_total=3 (≤10) triggers the ballot-bar variant —
        // the completion line shows per-op outcome glyphs so the
        // operator can see which ops succeeded or failed at a
        // glance, preserving the visibility from `phase_status`
        // through to the completion record.
        let ctx = TestCtx {
            phase_name: "setup".into(),
            phase_seq: Some((1, 2)),
            cycles_completed: 3,
            cycles_total: 3,
            ops_ok: 3,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 3,
            ..Default::default()
        };
        assert_eq!(
            render(&ctx),
            "✓ ☑☑☑ [setup] 100%\n    300/s ok:100% e:0 r:0 c:1 (0.01s)"
        );
    }

    #[test]
    fn no_color_with_coords_and_chips() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            phase_seq: Some((1, 8)),
            phase_labels: "(profile=alpha), (bucket=1, kind=READ)".into(),
            cycles_completed: 162,
            cycles_total: 162,
            ops_ok: 162,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 162,
            chips: " recall_at_10:79.62%".into(),
            ..Default::default()
        };
        assert_eq!(
            render(&ctx),
            "✓ [run] (profile=alpha), (bucket=1, kind=READ) 100%\n    16.2K/s ok:100% e:0 r:0 c:1 recall_at_10:79.62% (0.01s)"
        );
    }

    fn render_at(ctx: &TestCtx, lod: Lod, mode: ContentMode) -> String {
        // Isolate every LOD render from the process-global
        // `LAST_RENDERED_COORDS` tracker, exactly as the
        // [`render`] helper does: take the serial guard and reset
        // the tracker so a prior render (in this test or another
        // running concurrently) can't make this one elide its
        // coords as "unchanged". Without this the completed-phase
        // summary path (`summarize_changed_only`) intermittently
        // dropped the coords line under cargo-test's parallel
        // runner. No production code holds this lock — realtime
        // readout dispatch is single-threaded; the guard exists
        // only to serialise the test task pool. (No guard-holding
        // helper calls `render_at`, so re-entrancy can't occur.)
        let _serial = SERIAL_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        if let Ok(mut g) = LAST_RENDERED_COORDS.lock() {
            *g = String::new();
        }
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseOutcomeReadout.render(ctx, lod, mode, &ReadoutOptions::new(), &mut buf);
        s
    }

    /// Regression: two identical completed-phase renders must
    /// BOTH show their scope coords. Before `render_at` reset the
    /// global coord tracker, the second render diffed against the
    /// first and elided the coords (`summarize_changed_only`) —
    /// the intermittent `expanded_value_*` failure under parallel
    /// test execution. With the reset each render is independent.
    #[test]
    fn render_at_isolates_each_render_from_prior_tracker() {
        let ctx = TestCtx {
            phase_name: "ann_query".into(),
            phase_seq: Some((1, 8)),
            phase_labels: "profile=alpha, k=10".into(),
            cycles_completed: 100,
            cycles_total: 100,
            ops_ok: 99,
            errors: 1,
            retries: 0,
            concurrency: 4,
            elapsed_secs: 1.5,
            consumed: 100,
            chips: " recall_at_10:79.62% latency_p99:1.23ms".into(),
            ..Default::default()
        };
        let first = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        let second = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        assert!(
            first.contains("profile=alpha, k=10"),
            "first render should carry coords: {first}"
        );
        assert!(
            second.contains("profile=alpha, k=10"),
            "second render must NOT elide coords (tracker must reset \
             per render_at call): {second}"
        );
    }

    #[test]
    fn compact_value_drops_seq_rate_counts_chips() {
        // Push 9g (G17): Compact form is the trained-
        // operator scan version. Status glyph + name + pct
        // + elapsed only. Seq prefix, rate, ok-pct,
        // errors / retries / concurrency, scope coords, and
        // chips are all dropped. Every retained field
        // appears in Labeled (§3.3 monotonicity).
        let ctx = TestCtx {
            phase_name: "setup".into(),
            phase_seq: Some((1, 2)),
            phase_labels: "(profile=alpha)".into(),
            cycles_completed: 3,
            cycles_total: 3,
            ops_ok: 3,
            errors: 0,
            retries: 0,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 3,
            chips: " recall_at_10:79.62%".into(),
            ..Default::default()
        };
        assert_eq!(
            render_at(&ctx, Lod::Compact, ContentMode::Value),
            "✓ [setup] 100% (0.01s)",
        );
    }

    #[test]
    fn compact_value_pct_zero_when_no_extent() {
        // total_extent == 0 → 100% per the readout's no-
        // extent convention (the phase ran without a
        // declared cycle count and is by definition complete
        // at this fire).
        let ctx = TestCtx {
            phase_name: "x".into(),
            cycles_completed: 0,
            cycles_total: 0,
            elapsed_secs: 0.5,
            ..Default::default()
        };
        assert_eq!(
            render_at(&ctx, Lod::Compact, ContentMode::Value),
            "✓ [x] 100% (0.50s)",
        );
    }

    #[test]
    fn compact_explanation_describes_each_field() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Compact, ContentMode::Explanation);
        assert!(s.contains("done"), "expected 'done': {s}");
        assert!(s.contains("phase-name"), "expected 'phase-name': {s}");
        assert!(s.contains("progress%"), "expected 'progress%': {s}");
        assert!(s.contains("(elapsed)"), "expected '(elapsed)': {s}");
        // Compact's overlay must NOT describe fields it
        // doesn't show (rate, ok-pct, errors, retries,
        // concurrency, coords, chips, seq).
        assert!(
            !s.contains("idx/total"),
            "compact must not describe seq prefix it doesn't render: {s}"
        );
        assert!(
            !s.contains("throughput"),
            "compact must not describe throughput it doesn't render: {s}"
        );
    }

    #[test]
    fn expanded_value_emits_multi_line_block() {
        // Expanded form: same data as Labeled, organised
        // one field per line.
        let ctx = TestCtx {
            phase_name: "ann_query".into(),
            phase_seq: Some((1, 8)),
            phase_labels: "profile=alpha, k=10".into(),
            cycles_completed: 100,
            cycles_total: 100,
            ops_ok: 99,
            errors: 1,
            retries: 0,
            concurrency: 4,
            elapsed_secs: 1.5,
            consumed: 100,
            chips: " recall_at_10:79.62% latency_p99:1.23ms".into(),
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        // Header line carries the same identity as Labeled.
        assert!(s.contains("✓ [ann_query]"));
        assert!(!s.contains("[1/8]"));
        assert!(s.contains("profile=alpha, k=10"));
        // Per-field labelled rows (every Labeled field
        // appears here too — §3.3 monotonicity).
        assert!(s.contains("progress:    100% (100 of 100)"));
        assert!(s.contains("throughput:"));
        assert!(s.contains("ok:          99%  (99 of 100)"));
        assert!(s.contains("reliability: e:1 r:0"));
        assert!(s.contains("concurrency: 4"));
        assert!(s.contains("metrics:"));
        // Chips broken into one-per-line under `metrics:`.
        assert!(s.contains("recall_at_10"));
        assert!(s.contains("latency_p99"));
        assert!(s.contains("elapsed:     1.50s"));
        // Multi-line block — verify line count.
        let line_count = s.lines().count();
        assert!(
            line_count >= 8,
            "expanded should be multi-line (got {line_count}): {s}"
        );
    }

    #[test]
    fn expanded_value_omits_metrics_block_when_no_chips() {
        // metrics: header only renders when there are chips
        // to show under it.
        let ctx = TestCtx {
            phase_name: "setup".into(),
            cycles_completed: 1,
            cycles_total: 1,
            ops_ok: 1,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 1,
            chips: String::new(),
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        assert!(
            !s.contains("metrics:"),
            "expected no metrics: header when chips empty: {s}"
        );
    }

    #[test]
    fn expanded_value_omits_coords_line_when_no_labels() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            phase_labels: String::new(),
            cycles_completed: 1,
            cycles_total: 1,
            ops_ok: 1,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 1,
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        assert!(
            !s.contains("coords:"),
            "expected no coords: line when labels empty: {s}"
        );
    }

    #[test]
    fn expanded_explanation_describes_each_row() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Expanded, ContentMode::Explanation);
        assert!(s.contains("phase-name"));
        assert!(s.contains("progress:"));
        assert!(s.contains("throughput:"));
        assert!(s.contains("ok:"));
        assert!(s.contains("reliability:"));
        assert!(s.contains("concurrency:"));
        assert!(s.contains("elapsed:"));
        // Multi-line.
        assert!(s.lines().count() >= 7);
    }

    #[test]
    fn monotonicity_compact_subset_of_labeled() {
        // §3.3 invariant: every field shown at Compact
        // appears at Labeled too. Verified pragmatically:
        // the compact rendering's stripped form (depth,
        // glyph, name, pct, elapsed, parens) is
        // substring-present in the labeled rendering once
        // we drop the seq / rate / ok / counts / chips
        // additions.
        let ctx = TestCtx {
            phase_name: "setup".into(),
            cycles_completed: 3,
            cycles_total: 3,
            ops_ok: 3,
            concurrency: 1,
            elapsed_secs: 0.01,
            consumed: 3,
            ..Default::default()
        };
        let labeled = render_at(&ctx, Lod::Labeled, ContentMode::Value);
        let compact = render_at(&ctx, Lod::Compact, ContentMode::Value);
        // Identity: "[setup]" appears in both.
        assert!(labeled.contains("[setup]") && compact.contains("[setup]"));
        // Status glyph appears in both.
        assert!(labeled.contains('✓') && compact.contains('✓'));
        // Pct appears in both.
        assert!(labeled.contains("100%") && compact.contains("100%"));
        // Elapsed appears in both.
        assert!(labeled.contains("(0.01s)") && compact.contains("(0.01s)"));
    }

    #[test]
    fn explanation_mode_describes_each_field() {
        // SRD-63 §3.2: explanation overlay describes glyph
        // meaning. Width parity is the author's contract.
        let ctx = TestCtx {
            phase_name: "setup".into(),
            phase_seq: Some((1, 2)),
            phase_labels: "(profile=alpha)".into(),
            ..Default::default()
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = PhaseOutcomeReadout.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(n > 0, "explanation should render");
        // Spot-check semantic descriptors are present —
        // the user reads "phase-name", "progress%", etc.
        // rather than concrete data.
        assert!(s.contains("done"), "expected 'done' descriptor: {s}");
        assert!(s.contains("phase-name"), "expected 'phase-name': {s}");
        assert!(s.contains("scope-coords"), "expected 'scope-coords': {s}");
        // Single-placement: the body no longer carries [n/N] (the
        // margin owns it), so the explanation must not describe it.
        assert!(!s.contains("idx/total"), "seq descriptor must be gone: {s}");
        assert!(s.contains("progress%"), "expected 'progress%': {s}");
        assert!(s.contains("throughput"), "expected 'throughput': {s}");
        assert!(s.contains("ok:ok%"), "expected ok descriptor: {s}");
        assert!(s.contains("(elapsed)"), "expected '(elapsed)': {s}");
    }

    // ── SRD-76 outcome-driven rendering tests ──────────────

    /// Failed status → ✗ glyph + failure-flavoured Labeled
    /// layout (status class on line 1, first-error message
    /// on line 2). Replaces the success-line render that
    /// pre-SRD-76 always emitted regardless of outcome.
    #[test]
    fn labeled_failed_uses_x_glyph_and_first_error_class() {
        let ctx = TestCtx {
            phase_name: "ensure_compacted".into(),
            phase_labels: String::new(),
            elapsed_secs: 14400.0,
            outcome: crate::phase_outcome::Outcome::failed(),
            outcome_errors: vec![crate::phase_outcome::PhaseErrorDetail {
                class: "poll_timeout".into(),
                message: "phase-poll deadline reached after 14441.3s".into(),
                op_name: None,
                cycle: None,
                op_template: None,
                op_resolved: None,
                at_nanos: 0,
                retryable: false,
            }],
            ..Default::default()
        };
        let out = render(&ctx);
        assert!(
            out.starts_with("✗ "),
            "failed render must start with the ✗ glyph: {out:?}"
        );
        assert!(
            out.contains("[ensure_compacted]"),
            "phase name still in line 1: {out:?}"
        );
        assert!(
            out.contains("poll_timeout"),
            "first-error class on line 1: {out:?}"
        );
        assert!(
            out.contains("phase-poll deadline"),
            "first-error message on line 2: {out:?}"
        );
        assert!(out.contains("(14400.00s)"), "elapsed on line 2: {out:?}");
    }

    /// Failed with multiple errors surfaces `(+N more)` so
    /// the operator knows the Labeled LOD is truncating.
    /// Expanded LOD shows the full list.
    #[test]
    fn labeled_failed_shows_more_count_when_multiple_errors() {
        let mk_err = |class: &str, msg: &str| crate::phase_outcome::PhaseErrorDetail {
            class: class.into(),
            message: msg.into(),
            op_name: None,
            cycle: None,
            op_template: None,
            op_resolved: None,
            at_nanos: 0,
            retryable: false,
        };
        let ctx = TestCtx {
            phase_name: "p".into(),
            elapsed_secs: 1.0,
            // True error count == captured (3); `+N more` is now
            // derived from the true `errors()` counter, not the
            // captured-buffer length.
            errors: 3,
            outcome: crate::phase_outcome::Outcome::failed(),
            outcome_errors: vec![
                mk_err("A", "msg-a"),
                mk_err("B", "msg-b"),
                mk_err("C", "msg-c"),
            ],
            ..Default::default()
        };
        let out = render(&ctx);
        assert!(
            out.contains("(+2 more)"),
            "expected `(+2 more)` truncation marker: {out:?}"
        );
    }

    /// Skipped status renders ~ glyph but keeps the
    /// success-flavoured layout (the throughput/counter
    /// telemetry is still meaningful for replay).
    #[test]
    fn labeled_skipped_uses_tilde_glyph() {
        let ctx = TestCtx {
            phase_name: "rampup".into(),
            outcome: crate::phase_outcome::Outcome::skipped(),
            cycles_completed: 0,
            cycles_total: 0,
            elapsed_secs: 0.0,
            ..Default::default()
        };
        let out = render(&ctx);
        assert!(
            out.starts_with("~ "),
            "skipped render uses ~ glyph: {out:?}"
        );
        assert!(out.contains("[rampup]"), "phase name preserved: {out:?}");
    }

    /// Expanded LOD on a Failed phase appends a per-error
    /// stanza below the standard block. Each error renders
    /// its class, message, and (when populated) cycle /
    /// op-template / op-resolved.
    #[test]
    fn expanded_failed_includes_per_error_block() {
        let ctx = TestCtx {
            phase_name: "ann_query".into(),
            cycles_completed: 5,
            cycles_total: 10,
            elapsed_secs: 1.5,
            outcome: crate::phase_outcome::Outcome::failed(),
            outcome_errors: vec![crate::phase_outcome::PhaseErrorDetail {
                class: "Timeout".into(),
                message: "read timed out".into(),
                op_name: Some("read".into()),
                cycle: Some(3),
                op_template: Some("SELECT * FROM ks.t WHERE k = {cycle}".into()),
                op_resolved: Some("SELECT * FROM ks.t WHERE k = 3".into()),
                at_nanos: 0,
                retryable: true,
            }],
            ..Default::default()
        };
        let s = render_at(&ctx, Lod::Expanded, ContentMode::Value);
        assert!(
            s.contains("✗ [ann_query]"),
            "expanded header uses ✗ glyph for Failed: {s}"
        );
        assert!(s.contains("status:"), "status row present: {s}");
        assert!(s.contains("failed"), "status label shows 'failed': {s}");
        assert!(s.contains("errors:"), "errors header present: {s}");
        assert!(
            s.contains("[Timeout] read timed out"),
            "per-error class+message: {s}"
        );
        assert!(
            s.contains("cycle:") && s.contains(" 3"),
            "cycle row present when populated: {s}"
        );
        assert!(s.contains("op-template:"), "op-template row present: {s}");
        assert!(s.contains("op-resolved:"), "op-resolved row present: {s}");
    }

    /// Compact LOD glyph is driven by the two-axis outcome —
    /// untrustworthy → ✗, skipped → ~, re-usable partial → …,
    /// completed-and-trustworthy → ✓.
    #[test]
    fn compact_glyph_tracks_outcome_status() {
        use crate::phase_outcome::Outcome;
        for (outcome, want) in [
            (Outcome::completed(), '✓'),
            (Outcome::failed(), '✗'),
            (Outcome::completed_failed(), '✗'),
            (Outcome::skipped(), '~'),
            (Outcome::interrupted(), '…'),
        ] {
            let ctx = TestCtx {
                phase_name: "x".into(),
                elapsed_secs: 0.1,
                outcome,
                ..Default::default()
            };
            let s = render_at(&ctx, Lod::Compact, ContentMode::Value);
            assert!(
                s.starts_with(want),
                "compact glyph for {want:?} missing: {s:?}"
            );
        }
    }

    /// Failed ≤10-op phase carries the ballot bar so the
    /// operator sees the per-op success/failure pattern that
    /// preceded the failure, not just the error class.
    #[test]
    fn failed_small_phase_renders_ballot_bar_with_errors_first() {
        let ctx = TestCtx {
            phase_name: "tiny".into(),
            cycles_completed: 5,
            cycles_total: 5,
            ops_ok: 3,
            errors: 2,
            elapsed_secs: 0.5,
            outcome: crate::phase_outcome::Outcome::failed(),
            outcome_errors: vec![crate::phase_outcome::PhaseErrorDetail {
                class: "Timeout".into(),
                message: "deadline exceeded".into(),
                op_name: None,
                cycle: None,
                op_template: None,
                op_resolved: None,
                at_nanos: 0,
                retryable: false,
            }],
            ..Default::default()
        };
        let out = render(&ctx);
        // 2 errors → ☒☒, 3 successes → ☑☑☑.
        assert!(
            out.starts_with("✗ ☒☒☑☑☑ "),
            "failed ≤10 phase should lead with bar (errors first): {out:?}"
        );
    }

    #[test]
    fn err_color_promotes_when_errors_or_retries() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 10,
            cycles_total: 10,
            ops_ok: 9,
            errors: 1,
            retries: 0,
            concurrency: 1,
            elapsed_secs: 1.0,
            consumed: 10,
            use_color: true,
            ..Default::default()
        };
        let out = render(&ctx);
        // Yellow used (errors > 0) — confirm the ANSI code
        // sequence appears around the `e:` chunk. Tail line
        // carries the counters under the new two-line layout.
        assert!(
            out.contains("\x1b[33me:1 r:0\x1b[0m"),
            "expected yellow err_color around `e:1 r:0`, got: {out:?}"
        );
        assert!(
            out.contains('\n'),
            "expected two-line break in labeled render: {out:?}"
        );
    }
    /// A phase whose ops ALL `if:`-skipped reads as gated off — under
    /// the default `skipped_phases=mark` mode the completion is the
    /// explicit ⊘ form: skip counter, no rate, no ok%, no fabricated
    /// measurement of any kind.
    #[test]
    fn fully_skipped_phase_shows_skip_chip_not_fake_ok() {
        let ctx = TestCtx {
            phase_name: "recall_postcompact".into(),
            cycles_completed: 10_000,
            cycles_total: 10_000,
            skips: 10_000,
            elapsed_secs: 0.02,
            consumed: 10_000,
            concurrency: 20,
            ..TestCtx::default()
        };
        let out = render(&ctx);
        assert!(out.contains("gated off"), "explicit skip marker: {out:?}");
        assert!(out.contains('⊘'), "skip glyph shown: {out:?}");
        assert!(out.contains("skip:10000"), "skip count shown: {out:?}");
        assert!(
            !out.contains("ok:"),
            "no ok% of any kind on a gated-off phase: {out:?}"
        );
        assert!(!out.contains("/s"), "no rate on a gated-off phase: {out:?}");
    }
}

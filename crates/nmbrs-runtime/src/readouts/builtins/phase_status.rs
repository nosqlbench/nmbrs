// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `phase_status` — the live-status readout.
//!
//! Renders the inline-progress line that the activity's
//! refresh thread emits via `\r\x1b[K…` every 0.5 s.
//! Push 2 byte-equivalence target — the format string the
//! prior implementation used:
//!
//! ```text
//! {depth_indent}{cyan}{spinner}{reset}{bar} {seq_prefix}{activity_name} \
//!   {pct:.0}% {rate_str} ok:{ok_pct:.0}% att:{att_pct:.0}% e:{errors} r:{retries} c:{concurrency}\
//!   {adapter_status}{batch_info}{relevancy_str}{eta}
//! ```
//!
//! `ok:` is the result-level success rate (`result_success /
//! cycles_completed`); `att:` is the attempt-level success rate
//! (`attempt_success / (attempt_success + attempt_failure)`,
//! SRD-91 — resolved attempts only, so in-flight attempts don't
//! skew it). They coincide when no retry fires and diverge under
//! retry pressure.
//!
//! Width clamping (`truncate_to_width`) is the surface's
//! job — see the inline-status driver in `nmbrs-runtime::activity`.
//! Other LODs and the explanation overlay render zero bytes
//! in Push 2; Push 5 (`Lod::Expanded`) and Push 7
//! (`ContentMode::Explanation`) fill them in.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::format::{braille_bar, format_eta, format_rate, spinner_frame};
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct PhaseStatus;

impl Readout for PhaseStatus {
    fn name(&self) -> &'static str {
        "phase_status"
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
            (Lod::Compact, ContentMode::Value) => render_compact(ctx, out),
            (Lod::Labeled, ContentMode::Value) => render_labeled(ctx, out),
            (Lod::Expanded, ContentMode::Value) => render_expanded(ctx, out),
            (Lod::Compact, ContentMode::Explanation) => render_compact_explanation(ctx, out),
            (Lod::Labeled, ContentMode::Explanation) => render_labeled_explanation(ctx, out),
            (Lod::Expanded, ContentMode::Explanation) => render_expanded_explanation(ctx, out),
        }
    }
}

/// Compact LOD explanation overlay. Same shape as
/// `render_compact` (`{spinner} {pct}% {rate}`); each
/// token replaced with a meaning descriptor.
fn render_compact_explanation(_ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let s = "spin progress% rate/s";
    let _ = out.write_str(s);
    s.len()
}

/// Labeled LOD explanation overlay. Same shape as the
/// labeled value form — spinner + bar + name + counters
/// + ETA.
fn render_labeled_explanation(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let coords = if ctx.subject_labels().is_empty() {
        ""
    } else {
        " (scope-coords)"
    };
    let mut tmp = String::with_capacity(160);
    let _ = write!(
        &mut tmp,
        "spin (bar) [phase-name]{coords} \
progress% throughput ok:result-ok% att:attempt-ok% e:errors r:retries c:concurrency \
(metrics) ETA remaining",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Expanded LOD explanation overlay. Multi-line block,
/// same shape as `render_expanded` — one descriptor per
/// row.
fn render_expanded_explanation(_ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let s = "\
spin [phase-name]\n  \
progress:   progress% (bar)  ETA remaining\n  \
throughput: throughput  ok:result-ok%  att:attempt-ok%\n  \
counters:   e:errors r:retries c:concurrency\n  \
adapter:    adapter-counters\n  \
batch:      batch-info\n  \
metrics:    workload-emphasised metrics";
    let _ = out.write_str(s);
    s.len()
}

/// The METER SLOT: `NN%` when the subject has a completion fraction,
/// or a latency summary (`p50:1.2ms p99:9.8ms`) for open-ended
/// subjects (daemon background pollers), which have no "done" to
/// meter — the space carries the signal an operator actually watches
/// on a poller. Empty only when open-ended with no samples yet.
fn meter_slot(_ctx: &dyn ReadoutContext, frac: Option<f64>) -> String {
    // Open-ended subjects render NOTHING here: the header aligns with
    // workload-level tracking; their latency display lives in the
    // detail row's contextual gutter (see the sink's latency_gutter),
    // the space a progress meter would otherwise own.
    match frac {
        Some(f) => format!("{:.0}%", f * 100.0),
        None => String::new(),
    }
}

fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    // Palette per docs/guide/color_style.md:
    //   spinner → cyan (motion cue)
    //   activity name → bold + INFO (sky/blue)
    //   bar / rate / ok% / c: / ETA → MUTED (dim)
    //   pct → default (the headline number; not styled)
    //   e:N r:N → WARN (yellow) when >0, MUTED when 0
    //   memo header → EMPHASIS (bold yellow) — sits above
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let bold = if color { "\x1b[1m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let total_extent = ctx.cycles_total();
    let started = ctx.ops_started();
    let finished = ctx.ops_finished();
    let ops_completed = ctx.cycles_completed();
    let successes = ctx.ops_ok();
    let skips = ctx.skips();
    let errors = ctx.errors();
    let retries = ctx.retries();
    let attempt_ok = ctx.attempt_ok();
    let attempt_failed = ctx.attempt_failed();
    let elapsed = ctx.elapsed_secs();
    let concurrency = ctx.concurrency();

    // "Not yet ready" guard: when a phase has just started
    // and hasn't dispatched its first op, the full counter
    // block reads as all-zeros, which the operator perceives
    // as a stale or broken frame. Render a compact "starting…"
    // placeholder until the phase has either dispatched its
    // first op or accumulated at least 200ms of elapsed time.
    // The spinner keeps moving so motion confirms the
    // renderer is alive.
    if started == 0 && elapsed < 0.2 {
        let depth_indent = ctx.depth_indent();
        let activity_name = ctx.activity_name();
        let spinner = spinner_frame(ctx.refresh_tick());
        // No seq prefix: the margin's [n/N] slot owns the phase
        // counter (single-placement rule — nothing the gutter
        // carries is repeated in body text).
        let mut tmp = String::with_capacity(64);
        let _ = write!(
            &mut tmp,
            "{depth_indent}{cyan}{spinner}{reset} {bold}{blue}[{activity_name}]{reset} {dim}starting…{reset}",
        );
        let len = tmp.len();
        let _ = out.write_str(&tmp);
        return len;
    }

    // Progress percentage uses *completed* cycles, not
    // dispatched ones. The previous `ops_started`-based
    // formula reported 100% the moment the only fiber
    // dispatched its sole op — for long synchronous calls
    // (jolokia_compact, schema migrations) the bar pinned at
    // 100% for the whole wait. `cycles_completed` matches
    // what `phase_outcome` reports and what rate / ETA derive
    // from, so the running bar and the final DONE line agree.
    // A derived-progress override (a producer measuring its own
    // completion, e.g. `poll.progress`) wins over both — the
    // cycle basis pins at 0% for a single long measured op.
    // Single fraction source (override → rows → cycles); `None` for
    // open-ended subjects, whose meter slot renders latency instead.
    let frac = ctx.progress_fraction();
    // ok% excludes SKIPS — a skipped (`if:`-gated) op is neither a
    // success nor a failure, so the basis is result-producing ops only
    // (`cycles_completed - skips == result_total`).
    // `.max(successes)`: cycles_completed and result_success are read
    // non-atomically and bumped in the reverse order (cycles first), so a
    // completing op can make this dip below `successes` momentarily —
    // result_total is never truly < successes. Clamp up so ok% stays <= 100%.
    let result_total = ops_completed.saturating_sub(skips).max(successes);
    let ok_pct: f64 = if result_total > 0 {
        successes as f64 * 100.0 / result_total as f64
    } else {
        100.0
    };
    // All ops skipped so far → no results to be ok about: `ok:—`
    // instead of a fabricated 100%, plus an explicit skip counter so
    // the line reads as "gated off", not "measured clean".
    let ok_str: String = if result_total > 0 {
        format!("{ok_pct:.0}%")
    } else if skips > 0 {
        "—".to_string()
    } else {
        "100%".to_string()
    };
    // Attempt success rate (SRD-91): fraction of RESOLVED
    // adapter invocations that succeeded. Denominator is
    // `attempt_ok + attempt_failed` (both tallied at attempt
    // end) so in-flight attempts don't skew it — the same
    // completed-only basis `ok_pct` uses. Equals `ok_pct` when
    // no retry ever fired; drops below it under retry pressure,
    // where results still land green (`ok%`) only after wasted
    // attempts. Surfaced so an operator sees cluster strain
    // before it propagates to result-level failures.
    let attempt_resolved = attempt_ok + attempt_failed;
    let att_pct: f64 = if attempt_resolved > 0 {
        attempt_ok as f64 * 100.0 / attempt_resolved as f64
    } else {
        100.0
    };
    let rate: f64 = if elapsed > 0.0 {
        finished as f64 / elapsed
    } else {
        0.0
    };
    let rate_str = format_rate(rate);
    let skips_chip: String = if skips > 0 {
        format!(" {dim}skip:{skips}{reset}")
    } else {
        String::new()
    };

    // The spinner moved OUT of this line — it now replaces
    // the `│` divider on the row-2 margin (built by the sink
    // renderer) as a subtle animation indicator that the
    // phase is still ticking. At phase end, the sink reverts
    // to the standard `│` divider.
    let spinner = "";
    let _ = spinner_frame(0);
    // Bar styling: bright-white braille dots on a dark-grey
    // truecolor background. The background makes the empty
    // leading cells visible as a defined region instead of
    // a gap — so an early-phase 5% bar reads as `▮▯▯▯▯▯▯▯▯▯`
    // rather than `▮          ` (where the trailing cells
    // were braille blanks against the terminal default
    // background).
    // The 10-glyph progress bar moved OUT of this line. The
    // outer sink renderer (`log_only_sink`) builds the bar
    // from live metrics and renders it as the row-2 margin
    // replacement, so the running-phase header here is just
    // `<spinner> <name> <pct>%` while the stats row below
    // carries the bar in its left-margin gutter.
    let bar = String::new();
    // Time span: cumulative elapsed / ETA remaining, packed
    // into a single dim parenthesised pair. The slash reads
    // as past→future without needing a label. When ETA can't
    // be computed (no extent / no progress) the span
    // degenerates to just elapsed.
    // ETA only: elapsed lives in the margin's leaf slot (single-
    // placement rule), so the body never re-emits it.
    // Open-ended phases (daemons, elapsed-bounded cursors) have no
    // meaningful completion target — the extent-based fallback must
    // not resurrect an ETA that `eta_secs()` already declined.
    // The chip carries BOTH the remaining time and the total phase
    // estimate (`elapsed + remaining`), so the operator reads the
    // full expected wall time without adding the margin's elapsed
    // to the countdown in their head.
    let eta_chip = |secs: f64| {
        format!(
            " {dim}(~{} left of ~{}){reset}",
            format_eta(secs),
            format_eta(elapsed + secs)
        )
    };
    let eta = match ctx.eta_secs() {
        Some(secs) => eta_chip(secs),
        // Cursor phases (`rows_total > 0`) never take this arm: the
        // extent is row-denominated while `rate`/`finished` are
        // op-denominated (one op strides N rows), so the quotient
        // would overstate the ETA by the stride factor.
        None if !ctx.open_ended() && ctx.rows_total() == 0 && total_extent > 0 && rate > 0.0 => {
            let remaining = total_extent.saturating_sub(finished) as f64;
            eta_chip(remaining / rate)
        }
        None => String::new(),
    };

    // Margin owns [n/N]; body omits it (single-placement rule).
    let seq_prefix: String = String::new();
    let depth_indent = ctx.depth_indent();
    let activity_name = ctx.activity_name();
    let chips = ctx.status_metric_chips();
    let adapter_status = ctx.adapter_counters_text();
    let batch_info = ctx.batch_info_text();

    // Counters tone follows the rule from phase_outcome: yellow
    // when something abnormal (errors/retries > 0), dim when
    // clean. ok% gets the same treatment so a 100% / 99%
    // distinction reads at a glance.
    let err_tone = if errors > 0 || retries > 0 {
        yellow
    } else {
        dim
    };
    let ok_tone = if ok_pct >= 100.0 { dim } else { yellow };
    // Attempt-success chip tone mirrors ok%: dim (quiet) when
    // every attempt lands, yellow (warn) the moment attempts
    // are being burned on retries. Kept adjacent to `ok:` so
    // the result-vs-attempt divergence reads at a glance.
    let att_tone = if att_pct >= 100.0 { dim } else { yellow };

    // Memo row (if any): operator-visible state string published
    // by the `memo:` wrapper, in EMPHASIS color. SRD-92: blocks
    // compose HEADER-FIRST — the memo is a detail row directly
    // under the header, never a banner above it (which broke the
    // header/detail gutter alignment surface-side).
    let memo = ctx.phase_memo();
    let memo_row = if memo.is_empty() {
        String::new()
    } else {
        let bold_yellow = if color { "\x1b[1;33m" } else { "" };
        format!("{depth_indent}    {bold_yellow}[[ {memo} ]]{reset}\n")
    };

    // Two-line layout: break after the progress percentage so
    // the head line stays narrow (spinner/bar/name/pct) and
    // the tail line carries the counters and emphasized
    // metrics. Indentation on the second line aligns roughly
    // under the activity name. The surface sink
    // (`LogOnlySink`) handles multi-line region clearing.
    //
    // The progress chip sits alongside `c:concurrency`.
    //
    // Cursor-driven phase (`rows_total > 0`): the cursor advances in
    // STRIDES — one op consumes N ordinals — so an op-denominated
    // `cycles:{ops}/{extent}` reads ~N× low against the row-denominated
    // extent. Show the authoritative ordinal progress instead,
    // `rows:{consumed}/{extent}`, plus a rows/s rate (consumed / elapsed,
    // the same shape as the throughput rate line) so the fraction and
    // the rate are both row-denominated and agree.
    //
    // Non-cursor phase (`rows_total == 0`, plain `cycles:`): keep the
    // `cycles:N/T` chip — a glance shows both the running cycle count
    // and the total extent the phase is bounded by. With no extent
    // (unbounded sources, when those exist), we elide the `/T` half and
    // show just the running counter.
    // Open-ended phase: neither a rows target nor a cycle extent is
    // a real completion bound — show just the running counter.
    let rows_total = ctx.rows_total();
    let cycles_chip = if ctx.open_ended() {
        if ops_completed > 0 {
            format!(" {dim}cycles:{ops_completed}{reset}")
        } else {
            String::new()
        }
    } else if rows_total > 0 {
        let rows_consumed = ctx.rows_consumed();
        let rows_rate = if elapsed > 0.0 {
            rows_consumed as f64 / elapsed
        } else {
            0.0
        };
        format!(
            " {dim}rows:{rows_consumed}/{rows_total} {}{reset}",
            format_rate(rows_rate)
        )
    } else if total_extent > 0 {
        format!(" {dim}cycles:{ops_completed}/{total_extent}{reset}")
    } else if ops_completed > 0 {
        format!(" {dim}cycles:{ops_completed}{reset}")
    } else {
        String::new()
    };
    // SRD-? — full coord stack on the ACTIVE phase line
    // (`summarize_changed_only=false`). The same helper
    // `phase_outcome` uses for completed phases, called with
    // the inverse flag. Wrap-aware: when the head + coord
    // chain exceeds terminal width, strata fold onto
    // continuation lines indented to match the second-row
    // counters line (`depth_indent + "    "`).
    //
    // Head-consumed accounting for wrap budget:
    //   depth_indent (variable) + spinner (1) + " " (1)
    //   + bar (variable visible width)
    //   + " " (1) + seq_prefix (visible chars when set)
    //   + activity_name length
    let labels = ctx.subject_labels();
    // Bar is no longer inline in line 1 — moved to the
    // row-2 margin built by the sink renderer.
    let bar_visible: usize = 0;
    let seq_visible: usize = match ctx.subject_seq() {
        Some((s, t)) => format!("[{s}/{t}] ").chars().count(),
        None => 0,
    };
    let head_consumed: usize =
        depth_indent.chars().count() + bar_visible + seq_visible + activity_name.chars().count();
    let continuation_indent = format!("{depth_indent}    ");
    let coords_part = super::phase_outcome::format_coords_block(
        labels,
        color,
        head_consumed,
        &continuation_indent,
        /* summarize_changed_only */ false,
    );
    // Third row — the KEY-METRICS line. The domain metrics an operator
    // actually watches (adapter throughput chips like rows/s, the derived
    // rows/batch chip, and the workload's emphasised `status_metrics:` chips
    // such as recall) get their OWN indented line below the operational
    // counters, and ONLY when at least one is present. Packed onto the
    // counters row, a busy phase (CQL batch load + recall) overran the
    // terminal width and wrapped mid-chip; a dedicated line keeps each row
    // scannable. Chips each carry a leading space, so the row is trimmed once
    // and re-indented to align under the counters row above it.
    let key_metrics = format!("{adapter_status}{batch_info}{chips}");
    let key_line = if key_metrics.trim().is_empty() {
        String::new()
    } else {
        // Natural color — `status_metrics:` chips are opt-in EMPHASIS metrics
        // (recall, …), so this row is NOT dimmed like the counters row.
        format!("\n{depth_indent}    {}", key_metrics.trim_start())
    };
    let mut tmp = String::with_capacity(320);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{cyan}{spinner}{reset}{bar} {seq_prefix}{bold}{blue}{activity_name}{reset}{coords_part} {meter}\n\
{memo_row}\
{depth_indent}    {dim}{rate_str}{reset} {ok_tone}ok:{ok_str}{reset} \
{att_tone}att:{att_pct:.0}%{reset}{skips_chip} \
{err_tone}e:{errors} r:{retries}{reset} {dim}c:{concurrency}{reset}{cycles_chip}{eta}\
{key_line}",
        meter = meter_slot(ctx, frac),
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Expanded LOD: each field on its own line. Block-rendered
/// (the binder's layout classification picks `Block` for
/// expanded automatically). SRD-63 §3.3 monotonicity:
/// every field present at Labeled is present here too;
/// new fields are the explicit per-aggregate breakdowns.
fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let dim = if color { "\x1b[2m" } else { "" };
    let cyan = if color { "\x1b[36m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    let total_extent = ctx.cycles_total();
    let _started = ctx.ops_started();
    let finished = ctx.ops_finished();
    let ops_completed = ctx.cycles_completed();
    let successes = ctx.ops_ok();
    let skips = ctx.skips();
    let errors = ctx.errors();
    let retries = ctx.retries();
    let attempt_ok = ctx.attempt_ok();
    let attempt_failed = ctx.attempt_failed();
    let elapsed = ctx.elapsed_secs();
    let concurrency = ctx.concurrency();

    // See `render_labeled` — pct must use completed cycles so
    // dispatched-but-not-yet-returned ops don't pin the bar
    // at 100% during long synchronous waits; a derived-progress
    // override (measured completion) wins over both.
    // Single fraction source (override → rows → cycles); `None` for
    // open-ended subjects, whose meter slot renders latency instead.
    let frac = ctx.progress_fraction();
    let pct: f64 = frac.map(|f| f * 100.0).unwrap_or(0.0);
    // ok% over RESOLVED ops only (`cycles_completed - skips ==
    // result_total`) — a skip is neither a success nor a failure,
    // so it must not dilute the rate. Matches `render_labeled`.
    // `.max(successes)`: cycles_completed and result_success are read
    // non-atomically and bumped in the reverse order (cycles first), so a
    // completing op can make this dip below `successes` momentarily —
    // result_total is never truly < successes. Clamp up so ok% stays <= 100%.
    let result_total = ops_completed.saturating_sub(skips).max(successes);
    let ok_pct: f64 = if result_total > 0 {
        successes as f64 * 100.0 / result_total as f64
    } else {
        100.0
    };
    // Attempt success rate over resolved attempts — see
    // `render_labeled`.
    let attempt_resolved = attempt_ok + attempt_failed;
    let att_pct: f64 = if attempt_resolved > 0 {
        attempt_ok as f64 * 100.0 / attempt_resolved as f64
    } else {
        100.0
    };
    let rate: f64 = if elapsed > 0.0 {
        finished as f64 / elapsed
    } else {
        0.0
    };
    let rate_str = format_rate(rate);
    let bar = if total_extent > 0 {
        braille_bar(pct, 20)
    } else {
        String::new()
    };
    // Push 9f: prefer `ctx.eta_secs()`; fall back to the
    // inline derivation when the context doesn't supply one.
    let eta = match ctx.eta_secs() {
        Some(secs) => format!("ETA {}", format_eta(secs)),
        None if total_extent > 0 && rate > 0.0 => {
            let remaining = total_extent.saturating_sub(finished) as f64;
            format!("ETA {}", format_eta(remaining / rate))
        }
        None => String::from("ETA —"),
    };

    let activity_name = ctx.activity_name();
    let chips = ctx.status_metric_chips();
    let adapter_status = ctx.adapter_counters_text();
    let batch_info = ctx.batch_info_text();
    // Margin owns [n/N]; body omits it (single-placement rule).
    let seq_prefix: String = String::new();

    let mut tmp = String::with_capacity(384);
    let _ = write!(
        &mut tmp,
        "{cyan}{spinner}{reset} {seq_prefix}{activity_name}\n  \
         progress:   {meter} {dim}{bar}{reset}  {eta}\n  \
         throughput: {rate_str}  ok:{ok_pct:.0}%  att:{att_pct:.0}%\n  \
         counters:   e:{errors} r:{retries} c:{concurrency}",
        meter = meter_slot(ctx, frac),
        spinner = spinner_frame(ctx.refresh_tick()),
    );
    if !adapter_status.is_empty() {
        let _ = write!(&mut tmp, "\n  adapter:   {adapter_status}");
    }
    if !batch_info.is_empty() {
        let _ = write!(&mut tmp, "\n  batch:     {batch_info}");
    }
    if !chips.is_empty() {
        let _ = write!(&mut tmp, "\n  metrics:   {chips}");
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Compact LOD: a stripped-down one-token cluster. Per
/// SRD-63 §3.3's monotonicity invariant this is a strict
/// subset of `Labeled`. Used by the TUI tree row at the
/// default LOD setting (see Push 5).
fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let finished = ctx.ops_finished();
    let elapsed = ctx.elapsed_secs();
    // Pct from completed cycles — see `render_labeled`; a
    // derived-progress override (measured completion) wins.
    // Single fraction source (override → rows → cycles); `None` for
    // open-ended subjects, whose meter slot renders latency instead.
    let frac = ctx.progress_fraction();
    let _pct: f64 = frac.map(|f| f * 100.0).unwrap_or(0.0);
    let rate: f64 = if elapsed > 0.0 {
        finished as f64 / elapsed
    } else {
        0.0
    };
    let mut tmp = String::with_capacity(32);
    let _ = write!(
        &mut tmp,
        "{spin} {meter} {rate}",
        meter = meter_slot(ctx, frac),
        spin = spinner_frame(ctx.refresh_tick()),
        rate = format_rate(rate),
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::EventType;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        phase_name: String,
        activity_name: String,
        phase_seq: Option<(usize, usize)>,
        cycles_completed: u64,
        cycles_total: u64,
        ops_started: u64,
        ops_finished: u64,
        ops_ok: u64,
        skips: u64,
        errors: u64,
        retries: u64,
        attempt_ok: u64,
        attempt_failed: u64,
        concurrency: usize,
        elapsed_secs: f64,
        consumed: u64,
        rows_consumed: u64,
        rows_total: u64,
        chips: String,
        adapter: String,
        batch: String,
        depth_indent: String,
        refresh_tick: u64,
        use_color: bool,
    }

    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            &self.phase_name
        }
        fn activity_name(&self) -> &str {
            if self.activity_name.is_empty() {
                &self.phase_name
            } else {
                &self.activity_name
            }
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            self.phase_seq
        }
        fn subject_labels(&self) -> &str {
            ""
        }
        fn cycles_completed(&self) -> u64 {
            self.cycles_completed
        }
        fn cycles_total(&self) -> u64 {
            self.cycles_total
        }
        fn ops_started(&self) -> u64 {
            self.ops_started
        }
        fn ops_finished(&self) -> u64 {
            self.ops_finished
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
        fn attempt_ok(&self) -> u64 {
            self.attempt_ok
        }
        fn attempt_failed(&self) -> u64 {
            self.attempt_failed
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
        fn rows_consumed(&self) -> u64 {
            self.rows_consumed
        }
        fn rows_total(&self) -> u64 {
            self.rows_total
        }
        fn status_metric_chips(&self) -> String {
            self.chips.clone()
        }
        fn adapter_counters_text(&self) -> String {
            self.adapter.clone()
        }
        fn batch_info_text(&self) -> String {
            self.batch.clone()
        }
        fn depth_indent(&self) -> &str {
            &self.depth_indent
        }
        fn use_color(&self) -> bool {
            self.use_color
        }
        fn event(&self) -> EventType {
            EventType::Update
        }
        fn refresh_tick(&self) -> u64 {
            self.refresh_tick
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseStatus.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_no_color_minimal() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 50,
            cycles_total: 100,
            ops_started: 50,
            ops_finished: 50,
            ops_ok: 50,
            errors: 0,
            retries: 0,
            attempt_ok: 50,
            attempt_failed: 0,
            concurrency: 1,
            elapsed_secs: 1.0,
            consumed: 50,
            refresh_tick: 0,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // The spinner and 10-glyph progress bar moved OUT of
        // this row — the sink renderer builds them as the
        // row-2 margin replacement so the readout body is
        // just `<name> <coord> <pct>%` on row 1 and
        // `<rate> ok:.. e:.. r:.. c:.. cycles:..` on row 2.
        assert!(
            !out.contains("⠋"),
            "spinner MUST NOT appear in phase_status body: {out}"
        );
        // Two-line layout: head ends with " 50%\n",
        // tail begins with the indented counters.
        assert!(
            out.contains(" 50%\n"),
            "two-line break after pct missing: {out:?}"
        );
        assert!(
            out.contains("50/s ok:100% att:100% e:0 r:0 c:1"),
            "labeled body wrong: {out:?}"
        );
        // Time chip is ETA-ONLY (single-placement rule: elapsed is
        // the margin leaf slot's datum). remaining=cycles_total/rate
        // = 50/50 = 1s.
        assert!(
            out.contains("(~1s left of ~2s)"),
            "ETA chip missing for finite-rate phase: {out:?}"
        );
        assert!(
            !out.contains("(1s/1s)"),
            "elapsed must not be re-emitted in the body: {out:?}"
        );
    }

    #[test]
    fn cursor_phase_renders_rows_chip_not_cycles() {
        // A DATA-DRIVEN phase (rows_total > 0) consumes its cursor in
        // strides, so the progress chip must be row-denominated:
        // `rows:{consumed}/{extent}` plus a rows/s rate — NOT the
        // op-denominated `cycles:` chip which would read N× low.
        // Here 7 ops @ stride 100 = 700 ordinals of a 1000-row cursor.
        let ctx = TestCtx {
            phase_name: "ann".into(),
            activity_name: "ann".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 7,
            cycles_total: 1000,
            ops_started: 7,
            ops_finished: 7,
            ops_ok: 7,
            attempt_ok: 7,
            concurrency: 1,
            elapsed_secs: 1.0,
            consumed: 7,
            rows_consumed: 700,
            rows_total: 1000,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("rows:700/1000"),
            "cursor phase must show row-denominated progress: {out:?}"
        );
        // rows/s = consumed/elapsed = 700/1.0, format_rate → "700/s".
        assert!(
            out.contains("rows:700/1000 700/s"),
            "cursor phase must show a rows/s rate beside the fraction: {out:?}"
        );
        // The op-denominated cycles chip must be GONE for cursor phases.
        assert!(
            !out.contains("cycles:"),
            "cursor phase must NOT emit the cycles: chip: {out:?}"
        );
    }

    #[test]
    fn non_cursor_phase_keeps_cycles_chip() {
        // A plain `cycles:` phase (rows_total == 0) has no declared
        // cursor — ops advance the cycle counter one-for-one — so it
        // keeps the op-denominated `cycles:{completed}/{total}` chip
        // and emits no `rows:` chip. Byte-identical to pre-change output.
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 50,
            cycles_total: 100,
            ops_started: 50,
            ops_finished: 50,
            ops_ok: 50,
            attempt_ok: 50,
            concurrency: 1,
            elapsed_secs: 1.0,
            consumed: 50,
            // rows_consumed / rows_total default to 0 → non-cursor.
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("cycles:50/100"),
            "non-cursor phase must keep the cycles: chip: {out:?}"
        );
        assert!(
            !out.contains("rows:"),
            "non-cursor phase must NOT emit a rows: chip: {out:?}"
        );
    }

    #[test]
    fn skips_excluded_from_ok_rate() {
        // 3 ops completed: 2 succeeded, 1 was an `if:`-gated SKIP with
        // NO error. ok% must read 100% (2 of 2 result-producing ops),
        // NOT 67% (2 of 3) — a skip is neither a success nor a failure,
        // so it must not sit in the ok% denominator.
        let ctx = TestCtx {
            phase_name: "run".into(),
            cycles_completed: 3,
            cycles_total: 3,
            ops_started: 3,
            ops_finished: 3,
            ops_ok: 2,
            skips: 1,
            concurrency: 1,
            elapsed_secs: 1.0,
            consumed: 3,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("ok:100%"),
            "a skip (0 errors) must not drag ok% below 100%: {out:?}"
        );
        assert!(
            !out.contains("ok:67%"),
            "ok% wrongly counts the skip in its denominator: {out:?}"
        );
    }

    #[test]
    fn attempt_success_rate_diverges_from_ok_under_retry() {
        // SRD-91: results all land green (ok:100%) but 50 of the
        // 150 resolved attempts failed and were retried —
        // attempt success is 100/(100+50) = 67%. The status line
        // must surface BOTH: `ok:100% att:67%` is the
        // retry-pressure tell.
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 100,
            cycles_total: 100,
            ops_started: 100,
            ops_finished: 100,
            ops_ok: 100,
            errors: 50,
            retries: 50,
            attempt_ok: 100,
            attempt_failed: 50,
            concurrency: 4,
            elapsed_secs: 1.0,
            consumed: 100,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("ok:100% att:67%"),
            "attempt success rate must sit beside ok%, diverging under retry: {out:?}"
        );
        // Same divergence must show at the Expanded LOD
        // (monotonicity — the field can't vanish when detail
        // grows).
        let expanded = render(&ctx, Lod::Expanded);
        assert!(
            expanded.contains("att:67%"),
            "expanded throughput row missing attempt rate: {expanded:?}"
        );
    }

    #[test]
    fn memo_header_renders_above_status_when_non_empty() {
        // Memo wrapper publishes "compacting tableX"; the
        // status readout must surface it as
        // `[[ compacting tableX ]]` on its own line above the
        // regular two-line body. Empty memo (default) renders
        // nothing extra (the other tests guard that path).
        struct MemoCtx;
        impl ReadoutContext for MemoCtx {
            fn subject_name(&self) -> &str {
                "x"
            }
            fn activity_name(&self) -> &str {
                "x"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                None
            }
            fn subject_labels(&self) -> &str {
                ""
            }
            fn cycles_completed(&self) -> u64 {
                1
            }
            fn cycles_total(&self) -> u64 {
                1
            }
            fn ops_started(&self) -> u64 {
                1
            }
            fn ops_finished(&self) -> u64 {
                1
            }
            fn ops_ok(&self) -> u64 {
                1
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
                1.0
            }
            fn consumed(&self) -> u64 {
                1
            }
            fn status_metric_chips(&self) -> String {
                String::new()
            }
            fn depth_indent(&self) -> &str {
                ""
            }
            fn use_color(&self) -> bool {
                false
            }
            fn event(&self) -> EventType {
                EventType::Update
            }
            fn refresh_tick(&self) -> u64 {
                0
            }
            fn phase_memo(&self) -> &str {
                "compacting tableX"
            }
        }
        let ctx = MemoCtx;
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseStatus.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        // SRD-92: blocks compose HEADER-FIRST — the memo is a
        // detail row directly under the header, not a banner above.
        let lines: Vec<&str> = s.lines().collect();
        assert!(
            lines[0].contains("x") && lines[0].contains("100%"),
            "header row must lead the output, got: {s:?}"
        );
        assert_eq!(
            lines[1].trim_start(),
            "[[ compacting tableX ]]",
            "memo must be the first detail row under the header: {s:?}"
        );
        // Counters row still present below the memo.
        assert!(
            lines[2].contains("ok:"),
            "regular counters row missing: {s:?}"
        );
    }

    #[test]
    fn labeled_no_eta_when_no_extent() {
        // When cycles_total=0 there's no `/ETA` half of the
        // span; the time pair degenerates to elapsed-only —
        // `(0s)` here. The slash MUST NOT appear in this
        // branch. Bypass the "not-yet-ready" guard with
        // ops_started > 0 + measurable elapsed so the full
        // labeled render fires.
        let ctx = TestCtx {
            phase_name: "x".into(),
            activity_name: "x".into(),
            cycles_total: 0,
            ops_started: 1,
            elapsed_secs: 0.5,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // No extent and no override → no ETA is computable, and
        // elapsed belongs to the margin — so the body carries NO
        // time chip at all (single-placement rule).
        assert!(
            !out.contains(" left)") && !out.contains("(0s") && !out.contains("(1s"),
            "no time chip expected when ETA is not computable: {out}"
        );
    }

    #[test]
    fn labeled_chips_and_adapter_and_batch() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 100,
            cycles_total: 100,
            ops_started: 100,
            ops_finished: 100,
            ops_ok: 100,
            concurrency: 4,
            elapsed_secs: 1.0,
            consumed: 100,
            chips: " recall_at_10:80.00%".into(),
            adapter: " rows/s=12.5K".into(),
            batch: " r/b=12.5".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // Ordering preserved: adapter → batch → chips.
        assert!(
            out.contains("rows/s=12.5K r/b=12.5 recall_at_10:80.00%"),
            "adapter / batch / chips ordering wrong: {out}"
        );
        // …but on their OWN indented line below the counters row, not packed
        // onto it (which overran the width and wrapped on a busy phase).
        assert!(
            out.contains("\n    rows/s=12.5K r/b=12.5 recall_at_10:80.00%"),
            "key metrics should sit on a dedicated indented line: {out:?}"
        );
        // The counters row (e:/r:/c:) stays a separate line ABOVE the key row.
        let counters_line_idx = out.find("c:4").expect("counters row present");
        let key_line_idx = out.find("rows/s=12.5K").expect("key row present");
        assert!(
            counters_line_idx < key_line_idx,
            "counters row must precede the key-metrics row: {out:?}"
        );
    }

    #[test]
    fn labeled_omits_key_line_when_no_key_metrics() {
        // A plain phase (no adapter chips, no batch info, no status_metrics)
        // must NOT grow a third line — the key-metrics row is conditional.
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            cycles_completed: 10,
            cycles_total: 10,
            ops_started: 10,
            ops_finished: 10,
            ops_ok: 10,
            concurrency: 4,
            elapsed_secs: 1.0,
            consumed: 10,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // Exactly two lines: header + counters (one embedded '\n').
        assert_eq!(
            out.matches('\n').count(),
            1,
            "plain phase must stay two lines (no empty key row): {out:?}"
        );
    }

    #[test]
    fn compact_is_short_and_starts_with_spinner() {
        // Pct is driven by cycles_completed (completed cycles),
        // NOT ops_started — dispatched-but-not-returned ops
        // don't count toward the displayed percentage.
        let ctx = TestCtx {
            phase_name: "x".into(),
            cycles_total: 10,
            cycles_completed: 5,
            ops_started: 5,
            ops_finished: 5,
            elapsed_secs: 1.0,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Compact);
        assert!(out.starts_with("⠋"), "compact missing spinner: {out}");
        assert_eq!(out, "⠋ 50% 5/s");
    }

    #[test]
    fn pct_uses_completed_not_started() {
        // Regression guard for the off-by-one bug: a single
        // long-running op is dispatched (ops_started=1) but
        // hasn't returned yet (ops_finished=0,
        // cycles_completed=0). Pct must read 0%, not 100%.
        let ctx = TestCtx {
            phase_name: "x".into(),
            cycles_total: 1,
            cycles_completed: 0,
            ops_started: 1,
            ops_finished: 0,
            elapsed_secs: 5.0,
            ..Default::default()
        };
        let labeled = render(&ctx, Lod::Labeled);
        assert!(
            labeled.contains(" 0%\n"),
            "in-flight op should read 0%, not 100%: {labeled:?}"
        );
        let compact = render(&ctx, Lod::Compact);
        assert!(
            compact.contains(" 0% "),
            "compact in-flight should also read 0%: {compact:?}"
        );
    }

    #[test]
    fn explanation_mode_renders_descriptors_at_every_lod() {
        // SRD-63 §3.2 / Push 7: Explanation overlay has a
        // descriptor for every LOD. Width-parity with the
        // value render is the author's contract.
        let ctx = TestCtx {
            phase_name: "x".into(),
            cycles_total: 100,
            ops_started: 50,
            ops_finished: 50,
            elapsed_secs: 1.0,
            ..Default::default()
        };
        for lod in [Lod::Compact, Lod::Labeled, Lod::Expanded] {
            let mut s = String::new();
            let mut buf = StringBuf::new(&mut s);
            let n = PhaseStatus.render(
                &ctx,
                lod,
                ContentMode::Explanation,
                &ReadoutOptions::new(),
                &mut buf,
            );
            assert!(n > 0, "{lod:?}/Explanation should render");
            assert!(
                s.contains("progress"),
                "{lod:?}/Explanation missing 'progress' descriptor: {s}"
            );
        }
    }

    #[test]
    fn expanded_renders_multi_line_block() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            activity_name: "run".into(),
            phase_seq: Some((1, 1)),
            cycles_completed: 100,
            cycles_total: 200,
            ops_started: 100,
            ops_finished: 100,
            ops_ok: 100,
            concurrency: 4,
            elapsed_secs: 1.0,
            consumed: 100,
            chips: " recall_at_10:80.00%".into(),
            adapter: " rows/s=12.5K".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        // Expanded renders multi-line: progress, throughput,
        // counters at minimum. Adapter / metrics tails when
        // present.
        assert!(
            out.contains("progress:"),
            "expanded missing 'progress:': {out}"
        );
        assert!(
            out.contains("throughput:"),
            "expanded missing 'throughput:': {out}"
        );
        assert!(
            out.contains("counters:"),
            "expanded missing 'counters:': {out}"
        );
        assert!(
            out.contains("adapter:"),
            "expanded missing 'adapter:' tail: {out}"
        );
        assert!(
            out.contains("metrics:"),
            "expanded missing 'metrics:' tail: {out}"
        );
        assert!(
            out.lines().count() >= 5,
            "expanded should be multi-line: {out}"
        );
    }

    #[test]
    fn refresh_tick_advances_spinner_frame() {
        let mut ctx = TestCtx {
            phase_name: "x".into(),
            cycles_total: 10,
            ops_started: 1,
            ops_finished: 1,
            elapsed_secs: 1.0,
            ..Default::default()
        };
        let mut frames = std::collections::HashSet::new();
        for tick in 0..10 {
            ctx.refresh_tick = tick;
            let out = render(&ctx, Lod::Compact);
            // First non-empty char is the spinner.
            let first = out.chars().next().unwrap();
            frames.insert(first);
        }
        assert_eq!(frames.len(), 10, "spinner cycle not 10 distinct frames");
    }
}

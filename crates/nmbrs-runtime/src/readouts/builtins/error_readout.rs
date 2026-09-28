// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `error_readout` — phase-end error block.
//!
//! Renders the structured error list the executor collected
//! on `PhaseOutcome.errors` during the phase. Designed to
//! sit AFTER `phase_outcome` in the `on_phase_end` slot so
//! the operator sees the normative status line first
//! (✓/✗/~/…) and then the error detail block when the phase
//! failed.
//!
//! Renders nothing when there's nothing that ULTIMATELY FAILED:
//! empty error list, OR a phase where every error was retried and
//! recovered (`failed_ops = errors - retries == 0` and status is not
//! `Failed`). Recovered/transient errors are surfaced as the
//! `e:/r:` counters on the phase summary line, not as an error block
//! — an error block on a *successful* phase reads as a failure.
//! Their full detail is still in the persisted `phase_errors` rows
//! (`nmbrs replay`). So binding this to every `on_phase_end` is safe:
//! successful phases (even retry-heavy ones) produce no extra output.
//!
//! Per-LOD shape:
//! - **Compact** — single-line summary chip: `(errors:N)`,
//!   or empty when no errors.
//! - **Labeled** — header line + first error line:
//!   `errors: [class] message`
//!   `  (+M more)` when more than one.
//! - **Expanded** — full per-error block with class, message,
//!   cycle, op-template, op-resolved. Mirrors the existing
//!   Expanded LOD of `phase_outcome` but stands alone so it
//!   can be bound separately.
//!
//! Subject kind: Phase (the error list lives on the phase
//! outcome).

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct ErrorReadout;

impl Readout for ErrorReadout {
    fn name(&self) -> &'static str {
        "error_readout"
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
        let errors = ctx.outcome_errors();
        if errors.is_empty() {
            return 0;
        }
        // Explanation (help text) always renders.
        if matches!(mode, ContentMode::Explanation) {
            return render_explanation(out);
        }
        // Only surface the error-detail block for ops that ULTIMATELY
        // FAILED. Retried-and-recovered errors are transient: their
        // counts already appear in the phase summary's `e:/r:` tail, so
        // rendering them here would make a *successful* phase look
        // failed. `failed_ops = errors - retries` mirrors the activity's
        // own derivation (`activity.rs` end-of-phase); a stopped phase
        // is `Failed` even if that subtraction happens to be zero.
        // Recovered-error detail still lives in the persisted
        // `phase_errors` rows (`nmbrs replay`).
        let failed_ops = ctx.errors().saturating_sub(ctx.retries());
        let phase_failed = failed_ops > 0 || ctx.outcome().is_failure();
        if !phase_failed {
            return 0;
        }
        match (lod, mode) {
            (Lod::Compact, ContentMode::Value) => render_compact(ctx, out),
            (Lod::Labeled, ContentMode::Value) => render_labeled(ctx, out),
            (Lod::Expanded, ContentMode::Value) => render_expanded(ctx, out),
            // Explanation handled above; kept for match exhaustiveness.
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

/// True total error count for the phase. `ctx.errors()` is the
/// uncapped `errors_total` counter (every failed attempt); the
/// `outcome_errors()` buffer is capped (`PHASE_ERROR_CAPTURE_CAP`),
/// so the headline count must come from the counter, not the
/// buffer length — otherwise a phase with 200 errors reads "64".
/// `.max(captured)` guards the (impossible-in-practice) case of a
/// context whose counter lags the captured list.
fn total_and_captured(ctx: &dyn ReadoutContext) -> (u64, u64) {
    let captured = ctx.outcome_errors().len() as u64;
    let total = ctx.errors().max(captured);
    (total, captured)
}

fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let red = if color { "\x1b[31m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let (total, _) = total_and_captured(ctx);
    let mut tmp = String::with_capacity(32);
    let _ = write!(
        &mut tmp,
        "{dim}({reset}{red}errors:{total}{reset}{dim}){reset}"
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Re-indent every line after the first to `prefix` so an
/// embedded newline in a driver-supplied message doesn't break
/// out of the surrounding readout block's indent.
///
/// CQL `cassandra-cpp` driver errors carry the offending
/// statement inline as a multi-line string (e.g. an INSERT
/// formatted across rows); without re-indentation those rows
/// land at column 0 instead of nesting under the `errors:`
/// header.
fn indent_continuations(s: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(s.len() + prefix.len() * 4);
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
}

fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    // No leading newline — the readout binder's Block layout
    // inserts the separator between bound readouts itself. A
    // leading `\n` here would produce a doubled separator
    // (`\n\n` = visible blank row above the error block) when
    // bound after `phase_outcome` in the `on_phase_end` slot.
    let errors = ctx.outcome_errors();
    let color = ctx.use_color();
    let red = if color { "\x1b[31m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let indent = ctx.depth_indent();
    let first = errors.first().expect("checked non-empty by caller");
    let (total, captured) = total_and_captured(ctx);
    let continuation = format!("{indent}    ");
    let msg = indent_continuations(&first.message, &continuation);
    let mut tmp = String::with_capacity(96 + msg.len());
    let _ = write!(
        &mut tmp,
        "{indent}  {red}errors:{reset} {red}[{class}]{reset} {msg}",
        class = first.class,
    );
    if total > 1 {
        // `+N more` is relative to the TRUE total, not the capped
        // buffer. When the buffer was capped (captured < total), say
        // so — and point only at sources that actually hold the
        // detail: the Expanded LOD (the in-memory captured set) and
        // `nmbrs replay` (the persisted phase_errors rows). Per-cycle
        // errors are NOT written to session.log, so the old
        // "see session.log" pointer was a dead end.
        let more = total - 1;
        let cap_note = if captured < total {
            format!("; {captured} captured")
        } else {
            String::new()
        };
        let _ = write!(
            &mut tmp,
            "\n{indent}  {dim}(+{more} more{cap_note} — see Expanded LOD or `nmbrs replay`){reset}"
        );
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let errors = ctx.outcome_errors();
    let color = ctx.use_color();
    let red = if color { "\x1b[31m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let indent = ctx.depth_indent();
    let msg_continuation = format!("{indent}    ");
    let detail_continuation = format!("{indent}      ");
    let (total, captured) = total_and_captured(ctx);
    let mut tmp = String::with_capacity(256);
    // No leading newline — the binder's Block layout inserts
    // the separator. See `render_labeled` for the rationale.
    // Header shows the TRUE total; when the capture buffer was
    // capped, note how many of the total are listed below.
    let header = if captured < total {
        format!("{captured} of {total} captured")
    } else {
        format!("{total}")
    };
    let _ = write!(
        &mut tmp,
        "{indent}  {red}errors{reset} {dim}({header}){reset}:"
    );
    for e in errors {
        let msg = indent_continuations(&e.message, &msg_continuation);
        let _ = write!(
            &mut tmp,
            "\n{indent}  {red}[{class}]{reset} {msg}",
            class = e.class
        );
        if let Some(c) = e.cycle {
            let _ = write!(&mut tmp, "\n{indent}    {dim}cycle:{reset} {c}");
        }
        if let Some(op) = &e.op_name {
            let _ = write!(&mut tmp, "\n{indent}    {dim}op:{reset} {op}");
        }
        if let Some(t) = &e.op_template {
            let t = indent_continuations(t, &detail_continuation);
            let _ = write!(&mut tmp, "\n{indent}    {dim}op-template:{reset} {t}");
        }
        if let Some(r) = &e.op_resolved {
            let r = indent_continuations(r, &detail_continuation);
            let _ = write!(&mut tmp, "\n{indent}    {dim}op-resolved:{reset} {r}");
        }
    }
    if captured < total {
        // The capture buffer (PHASE_ERROR_CAPTURE_CAP) filled before
        // all errors arrived — surface the shortfall so the listed
        // set isn't mistaken for the whole story.
        let _ = write!(
            &mut tmp,
            "\n{indent}  {dim}(+{} more occurred — not captured (buffer cap)){reset}",
            total - captured
        );
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "error_readout — per-phase error block; renders only when the phase recorded \
             at least one structured error. Compact = count summary; Labeled = first error \
             line with extra-count tail; Expanded = full per-error block with cycle / \
             op-template / op-resolved detail";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_outcome::PhaseErrorDetail;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        errors: Vec<PhaseErrorDetail>,
        /// True `errors_total` count. 0 (default) → `errors()`
        /// falls back to the captured-list length, preserving the
        /// pre-cap behaviour for tests that don't exercise capping.
        total_errors: u64,
        /// Derived retry count (`errors - failed_ops`). With
        /// `total_errors`, drives `failed_ops = errors - retries` —
        /// the gate for whether the block renders at all.
        retries: u64,
        /// Marks the phase as failed (stopped) regardless of the
        /// count math.
        failed: bool,
        indent: String,
        color: bool,
    }

    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            ""
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            None
        }
        fn subject_labels(&self) -> &str {
            ""
        }
        fn cycles_completed(&self) -> u64 {
            0
        }
        fn cycles_total(&self) -> u64 {
            0
        }
        fn ops_ok(&self) -> u64 {
            0
        }
        fn errors(&self) -> u64 {
            self.total_errors
        }
        fn retries(&self) -> u64 {
            self.retries
        }
        fn concurrency(&self) -> usize {
            0
        }
        fn elapsed_secs(&self) -> f64 {
            0.0
        }
        fn consumed(&self) -> u64 {
            0
        }
        fn status_metric_chips(&self) -> String {
            String::new()
        }
        fn depth_indent(&self) -> &str {
            &self.indent
        }
        fn use_color(&self) -> bool {
            self.color
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::PhaseEnd
        }
        fn outcome_errors(&self) -> &[PhaseErrorDetail] {
            &self.errors
        }
        fn outcome(&self) -> crate::phase_outcome::Outcome {
            if self.failed {
                crate::phase_outcome::Outcome::failed()
            } else {
                crate::phase_outcome::Outcome::completed()
            }
        }
    }

    fn err(class: &str, msg: &str, cycle: Option<u64>) -> PhaseErrorDetail {
        PhaseErrorDetail {
            class: class.into(),
            message: msg.into(),
            op_name: None,
            cycle,
            op_template: None,
            op_resolved: None,
            at_nanos: 0,
            retryable: false,
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        ErrorReadout.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn empty_errors_renders_nothing() {
        let ctx = TestCtx::default();
        assert_eq!(render(&ctx, Lod::Labeled), "");
        assert_eq!(render(&ctx, Lod::Compact), "");
        assert_eq!(render(&ctx, Lod::Expanded), "");
    }

    #[test]
    fn recovered_errors_render_nothing() {
        // A successful phase whose errors were all retried-and-
        // recovered (failed_ops = errors - retries == 0, not Failed)
        // must produce NO error block in any value LOD — the e:/r:
        // counters on the summary line carry the info, and an error
        // block would make a successful phase look failed.
        let captured: Vec<_> = (0..64)
            .map(|i| err("WriteTimeout", "timed out", Some(i)))
            .collect();
        let ctx = TestCtx {
            errors: captured,
            total_errors: 200,
            retries: 200,  // failed_ops = 200 - 200 = 0
            failed: false, // phase completed
            ..Default::default()
        };
        assert_eq!(
            render(&ctx, Lod::Compact),
            "",
            "no chip for fully-recovered errors"
        );
        assert_eq!(
            render(&ctx, Lod::Labeled),
            "",
            "no block for fully-recovered errors"
        );
        assert_eq!(
            render(&ctx, Lod::Expanded),
            "",
            "no block for fully-recovered errors"
        );
    }

    #[test]
    fn partial_failure_still_renders() {
        // Some recovered, some terminal: failed_ops = 200 - 195 = 5 > 0
        // → the block renders (real failures occurred).
        let captured: Vec<_> = (0..64).map(|i| err("X", "e", Some(i))).collect();
        let ctx = TestCtx {
            errors: captured,
            total_errors: 200,
            retries: 195,
            failed: false,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("errors:"),
            "5 ops failed → block must render: {out:?}"
        );
    }

    #[test]
    fn failed_status_renders_even_when_count_math_zero() {
        // Belt-and-suspenders: a stopped phase is Failed even if
        // failed_ops math happens to be 0 (e.g. a stop on a
        // non-op-counted error) — the block must still render.
        let ctx = TestCtx {
            errors: vec![err("StopErr", "fatal", Some(0))],
            total_errors: 1,
            retries: 1,   // failed_ops = 0
            failed: true, // but the phase stopped
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("StopErr"),
            "failed phase must render the block regardless of count math: {out:?}"
        );
    }

    #[test]
    fn compact_shows_count_only() {
        let ctx = TestCtx {
            errors: vec![err("X", "one", None), err("Y", "two", None)],
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Compact);
        assert!(out.contains("errors:2"));
        assert!(!out.contains("one"));
        assert!(!out.contains("two"));
    }

    #[test]
    fn labeled_shows_first_error_with_more_count() {
        let ctx = TestCtx {
            errors: vec![
                err("CqlParseError", "syntax", Some(7)),
                err("CqlParseError", "syntax", Some(8)),
                err("CqlParseError", "syntax", Some(9)),
            ],
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // No leading newline — the binder's Block-layout
        // dispatcher inserts the separator between bound
        // readouts. A leading `\n` here would double up to
        // `\n\n` (visible blank row) above the error block.
        assert!(
            !out.starts_with('\n'),
            "no leading newline — binder owns the separator: {out:?}"
        );
        assert!(out.contains("[CqlParseError]"));
        assert!(out.contains("syntax"));
        assert!(out.contains("+2 more"));
    }

    #[test]
    fn labeled_reports_true_total_and_capped_count() {
        // 64 captured, 200 actually occurred. The tail must report
        // the TRUE remainder (199), note the captured count (64),
        // point at a real source, and NOT claim session.log.
        let captured: Vec<_> = (0..64)
            .map(|i| err("WriteTimeout", "timed out", Some(i)))
            .collect();
        let ctx = TestCtx {
            errors: captured,
            total_errors: 200,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(
            out.contains("+199 more"),
            "tail must report the true remainder (200-1): {out:?}"
        );
        assert!(
            out.contains("64 captured"),
            "tail must note how many were captured: {out:?}"
        );
        assert!(
            out.contains("nmbrs replay") || out.contains("Expanded LOD"),
            "tail must point at a source that actually holds the detail: {out:?}"
        );
        assert!(
            !out.contains("session.log"),
            "tail must not point at session.log — errors aren't recorded there: {out:?}"
        );
    }

    #[test]
    fn compact_uses_true_total_not_capped_buffer() {
        let captured: Vec<_> = (0..64).map(|i| err("X", "e", Some(i))).collect();
        let ctx = TestCtx {
            errors: captured,
            total_errors: 200,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Compact);
        assert!(
            out.contains("errors:200"),
            "compact chip must show the true total, not the capped buffer length: {out:?}"
        );
    }

    #[test]
    fn expanded_header_shows_captured_of_total_when_capped() {
        let captured: Vec<_> = (0..64).map(|i| err("X", "e", Some(i))).collect();
        let ctx = TestCtx {
            errors: captured,
            total_errors: 200,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(
            out.contains("64 of 200 captured"),
            "expanded header must show captured/total when capped: {out:?}"
        );
        assert!(
            out.contains("not captured"),
            "expanded must note the uncaptured remainder: {out:?}"
        );
    }

    #[test]
    fn labeled_all_captured_omits_cap_note() {
        // total == captured → no "; N captured" note, just "+N more".
        let captured: Vec<_> = vec![
            err("X", "a", Some(0)),
            err("X", "b", Some(1)),
            err("X", "c", Some(2)),
        ];
        let ctx = TestCtx {
            errors: captured,
            total_errors: 3,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(out.contains("+2 more"), "true remainder: {out:?}");
        assert!(
            !out.contains("captured"),
            "no cap note when everything was captured: {out:?}"
        );
    }

    #[test]
    fn labeled_single_error_omits_more_suffix() {
        let ctx = TestCtx {
            errors: vec![err("X", "only", None)],
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(out.contains("[X]"));
        assert!(out.contains("only"));
        assert!(!out.contains("more"));
    }

    #[test]
    fn expanded_lists_every_error_with_cycle() {
        let ctx = TestCtx {
            errors: vec![err("A", "first", Some(0)), err("B", "second", Some(5))],
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("[A]"));
        assert!(out.contains("first"));
        assert!(out.contains("cycle:"));
        assert!(out.contains("[B]"));
        assert!(out.contains("second"));
    }

    #[test]
    fn ansi_emitted_when_color_enabled() {
        let ctx = TestCtx {
            errors: vec![err("X", "msg", None)],
            color: true,
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(out.contains("\x1b[31m"));
    }

    /// CQL driver errors carry the offending statement inline
    /// as a multi-line string. Every line after the first must
    /// inherit the surrounding readout's indent so the output
    /// doesn't break out to column 0.
    #[test]
    fn labeled_multiline_message_keeps_continuation_indent() {
        let multi = "Cassandra error: timeout\n\
                     statement: INSERT INTO ks.t\n\
                     (a, b, c) VALUES\n\
                     (?, ?, ?)";
        let ctx = TestCtx {
            errors: vec![err("cql_error", multi, None)],
            indent: "                ".into(), // 16-space depth indent
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        // Every line that ISN'T the leading newline or the
        // first error line must start with at least the
        // depth indent — never column 0, never < indent.
        let depth = "                "; // matches ctx.indent
        for (i, line) in out.split('\n').enumerate() {
            if i == 0 || line.is_empty() {
                continue;
            }
            assert!(
                line.starts_with(depth),
                "line {i} broke indent at column 0: {line:?}"
            );
        }
    }

    #[test]
    fn expanded_multiline_message_keeps_continuation_indent() {
        let multi = "first line\nsecond line\nthird line";
        let ctx = TestCtx {
            errors: vec![err("X", multi, None)],
            indent: "      ".into(),
            failed: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        let depth = "      ";
        for (i, line) in out.split('\n').enumerate() {
            if i == 0 || line.is_empty() {
                continue;
            }
            assert!(
                line.starts_with(depth),
                "line {i} broke indent at column 0: {line:?}"
            );
        }
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `session_summary` — the session-level rollup line:
//! `phases:  X completed, Y failed, Z not run (of N total)`.
//!
//! Reads session-scope totals from
//! [`ReadoutContext::session_phases_*`]. Default slot is
//! `on_session_end`; workloads can also bind it elsewhere
//! (e.g. mid-run snapshot via `on_update`) but the totals
//! only make sense at session-scope contexts.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct SessionSummary;

impl Readout for SessionSummary {
    fn name(&self) -> &'static str {
        "session_summary"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Session]
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
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

/// Compact: single-line tallies, no labels — matches the
/// existing observer's bracket form.
fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let dim = if color { "\x1b[2m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let red = if color { "\x1b[1;31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let f = ctx.session_phases_failed();
    let p = ctx.session_phases_pending();
    let fail_color = if f > 0 { red } else { dim };
    // Pending phases render dim regardless of count (no emphasis colour
    // is defined for "not yet run").
    let pending_color = dim;
    let mut tmp = String::with_capacity(64);
    let _ = write!(
        &mut tmp,
        "{green}{c}{reset}/{fail_color}{f}{reset}/{pending_color}{p}{reset}/{dim}{t}{reset}",
        c = ctx.session_phases_completed(),
        t = ctx.session_phases_total(),
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// Labeled: full-prose form matching the observer's
/// pre-engine rollup `phases:  X completed, Y failed,
/// Z not run (of N total)`.
fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let tmp = labeled_phase_rollup(
        ctx.session_phases_completed(),
        ctx.session_phases_failed(),
        ctx.session_phases_pending(),
        ctx.session_phases_total(),
        ctx.use_color(),
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

/// The canonical labeled phase-rollup line —
/// `phases:  C completed, F failed, P not run (of T total)` — built
/// from raw counts. Shared by the `session_summary` readout (which
/// pulls the counts from session-scope totals) and any caller that
/// needs the identical text from its own tallies, e.g. the in-process
/// example walker synthesising a **per-execution** rollup for rule
/// matching (its session-scope summary spans every concurrent
/// execution, so it counts its own [`PhaseRecord`](crate::concurrent)s
/// instead). One formatter ⇒ no drift between the two.
///
/// Per `docs/guide/color_style.md`: `phases:` is HEADER (bold), counts
/// colored per status (OK/ERROR/MUTED), total MUTED, `of N total`
/// parenthetical dim. `color=false` yields plain text.
pub fn labeled_phase_rollup(
    completed: usize,
    failed: usize,
    pending: usize,
    total: usize,
    color: bool,
) -> String {
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let red = if color { "\x1b[1;31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let fail_color = if failed > 0 { red } else { dim };
    let mut tmp = String::with_capacity(128);
    let _ = write!(
        &mut tmp,
        "{bold}phases:{reset}  \
         {green}{completed}{reset} completed, \
         {fail_color}{failed}{reset} failed, \
         {dim}{pending}{reset} not run \
         {dim}(of {total} total){reset}",
    );
    tmp
}

/// Expanded: per-line breakdown — same data, friendlier
/// to scan for debugging / scrollback.
fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let red = if color { "\x1b[1;31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let f = ctx.session_phases_failed();
    let fail_color = if f > 0 { red } else { dim };
    let mut tmp = String::with_capacity(192);
    let _ = write!(
        &mut tmp,
        "{bold}session totals{reset}\n  \
         {dim}completed:{reset}  {green}{c}{reset}\n  \
         {dim}failed:{reset}     {fail_color}{f}{reset}\n  \
         {dim}not run:{reset}    {dim}{p}{reset}\n  \
         {dim}total:{reset}      {dim}{t}{reset}",
        c = ctx.session_phases_completed(),
        p = ctx.session_phases_pending(),
        t = ctx.session_phases_total(),
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "phases:  <completed-count> completed, <failed-count> failed, \
             <pending-count> not run (of <total> total)";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        completed: usize,
        failed: usize,
        pending: usize,
        total: usize,
    }
    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            "session"
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
            0
        }
        fn retries(&self) -> u64 {
            0
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
            ""
        }
        fn use_color(&self) -> bool {
            false
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::SessionEnd
        }
        fn session_phases_completed(&self) -> usize {
            self.completed
        }
        fn session_phases_failed(&self) -> usize {
            self.failed
        }
        fn session_phases_pending(&self) -> usize {
            self.pending
        }
        fn session_phases_total(&self) -> usize {
            self.total
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        SessionSummary.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_matches_pre_engine_format() {
        let ctx = TestCtx {
            completed: 7,
            failed: 1,
            pending: 0,
            total: 8,
        };
        assert_eq!(
            render(&ctx, Lod::Labeled),
            "phases:  7 completed, 1 failed, 0 not run (of 8 total)",
        );
    }

    #[test]
    fn compact_packs_into_slash_form() {
        let ctx = TestCtx {
            completed: 5,
            failed: 2,
            pending: 1,
            total: 8,
        };
        assert_eq!(render(&ctx, Lod::Compact), "5/2/1/8");
    }

    #[test]
    fn expanded_breaks_onto_multiple_lines() {
        let ctx = TestCtx {
            completed: 1,
            failed: 0,
            pending: 0,
            total: 1,
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.lines().count() >= 4);
        assert!(out.contains("completed:  1"));
        assert!(out.contains("total:      1"));
    }

    #[test]
    fn explanation_shows_field_descriptors() {
        let ctx = TestCtx::default();
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        SessionSummary.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(s.contains("completed-count"));
        assert!(s.contains("failed-count"));
        assert!(s.contains("pending-count"));
    }
}

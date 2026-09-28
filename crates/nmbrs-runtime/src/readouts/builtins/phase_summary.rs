// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `phase_summary` — the post-run summary line readout.
//!
//! Renders the `[ok] [N/total] name duration` form the
//! TUI observer's post-run roll-up emits today. Push 8
//! ships the readout itself; the live observer keeps its
//! direct format for now (Push 8b wires it through the
//! readout engine alongside the rest of the post-run
//! integration).
//!
//! Available so workloads can bind it via the `readouts:`
//! block — e.g. as an alternate `on_phase_end` body that
//! emits the bracket form instead of the ✓ form:
//!
//! ```yaml
//! readouts:
//!   on_phase_end: phase_summary
//! ```

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::{LifecycleState, ReadoutContext};
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct PhaseSummary;

impl Readout for PhaseSummary {
    fn name(&self) -> &'static str {
        "phase_summary"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Phase]
    }

    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        lod: Lod,
        mode: ContentMode,
        opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize {
        // Push 8 surface: Labeled / Value only. The
        // explanation overlay falls through to a
        // descriptor; other LODs stub to zero bytes.
        match (lod, mode) {
            (Lod::Labeled, ContentMode::Value) => render_labeled_value(ctx, opts, out),
            (Lod::Labeled, ContentMode::Explanation) => render_labeled_explanation(ctx, out),
            _ => 0,
        }
    }
}

fn render_labeled_value(
    ctx: &dyn ReadoutContext,
    opts: &ReadoutOptions,
    out: &mut dyn ReadoutBuf,
) -> usize {
    // Color palette per docs/guide/color_style.md:
    //   [ok] → OK (green), [!!] → ERROR (red),
    //   [..] → INFO (sky), [  ] → MUTED (dim),
    //   phase name → bold INFO, duration → MUTED.
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let yellow = if color { "\x1b[33m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let green = if color { "\x1b[32m" } else { "" };
    let red = if color { "\x1b[1;31m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };

    // Status marker mirrors the TUI observer's existing
    // bracket vocabulary — same characters so the
    // post-run summary reads the same whether it routed
    // through the legacy direct emit or through the
    // engine. (Push 8b's whole point.)
    let (marker, suffix) = match ctx.subject_state() {
        LifecycleState::Completed => (format!("{green}[ok]{reset}"), String::new()),
        LifecycleState::Running => (
            format!("{blue}[..]{reset}"),
            format!(" {dim}(still running){reset}"),
        ),
        LifecycleState::Pending => (
            format!("{dim}[  ]{reset}"),
            format!(" {dim}(not run){reset}"),
        ),
        LifecycleState::Failed(err) => (format!("{red}[!!]{reset}"), format!(" ({err})")),
    };
    let seq_part: String = match ctx.subject_seq() {
        Some((s, t)) => format!("{dim}[{s}/{t}]{reset} "),
        None => String::new(),
    };
    let depth_indent = ctx.depth_indent();
    let phase_name = ctx.subject_name();
    let labels = ctx.subject_labels();
    // Push 9e: `show_labels=true` opts into rendering the
    // iter-tuple coord-path inline as ` ({labels})` between
    // the name and the duration / suffix. Default off — the
    // post-run summary's primary row hides them since the
    // scope-header rows above already carry the coords.
    // The failed-phase inset sets it on so each failure
    // line is self-contained without scrolling.
    let show_labels = opts.get_bool("show_labels").unwrap_or(false);
    let labels_part = if labels.is_empty() || !show_labels {
        String::new()
    } else {
        format!(" {bold}{yellow}({labels}){reset}")
    };
    let elapsed = ctx.elapsed_secs();
    let dur_part = if elapsed > 0.0 {
        format!(" {dim}{elapsed:.2}s{reset}")
    } else {
        String::new()
    };

    // Memo row: phase displays surface the latest published memo
    // as `[[ <memo> ]]` — a detail row BELOW the bracket-form
    // header (SRD-92 header-first composition). Empty memo → no
    // row. Bold yellow per the style guide's EMPHASIS tier.
    let memo = ctx.phase_memo();
    let memo_row = if memo.is_empty() {
        String::new()
    } else {
        let bold_yellow = if color { "\x1b[1;33m" } else { "" };
        format!("\n{depth_indent}    {bold_yellow}[[ {memo} ]]{reset}")
    };

    let mut tmp = String::with_capacity(160);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{marker} {seq_part}{bold}{blue}{phase_name}{reset}{labels_part}{dur_part}{suffix}{memo_row}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled_explanation(_ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let s = "[ok|!!] [idx/total] phase-name (elapsed)";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::OptionValue;
    use crate::readouts::buf::StringBuf;

    struct TestCtx {
        phase_name: String,
        phase_seq: Option<(usize, usize)>,
        phase_labels: String,
        elapsed_secs: f64,
        depth_indent: String,
        state: LifecycleState,
    }
    impl TestCtx {
        // Replaces #[derive(Default)] now that LifecycleState
        // requires explicit construction.
        fn defaults() -> Self {
            Self {
                phase_name: String::new(),
                phase_seq: None,
                phase_labels: String::new(),
                elapsed_secs: 0.0,
                depth_indent: String::new(),
                state: LifecycleState::Running,
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
            1
        }
        fn elapsed_secs(&self) -> f64 {
            self.elapsed_secs
        }
        fn consumed(&self) -> u64 {
            0
        }
        fn status_metric_chips(&self) -> String {
            String::new()
        }
        fn depth_indent(&self) -> &str {
            &self.depth_indent
        }
        fn use_color(&self) -> bool {
            false
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::PhaseEnd
        }
        fn subject_state(&self) -> LifecycleState {
            self.state.clone()
        }
    }

    fn render(ctx: &TestCtx) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseSummary.render(
            ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn completed_with_seq_and_duration() {
        let ctx = TestCtx {
            phase_name: "setup".into(),
            phase_seq: Some((1, 2)),
            elapsed_secs: 0.02,
            state: LifecycleState::Completed,
            ..TestCtx::defaults()
        };
        assert_eq!(render(&ctx), "[ok] [1/2] setup 0.02s");
    }

    #[test]
    fn completed_no_seq_no_duration() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            state: LifecycleState::Completed,
            ..TestCtx::defaults()
        };
        assert_eq!(render(&ctx), "[ok] x");
    }

    #[test]
    fn failed_state_emits_double_bang_and_error() {
        let ctx = TestCtx {
            phase_name: "load".into(),
            phase_seq: Some((2, 3)),
            elapsed_secs: 1.0,
            state: LifecycleState::Failed("boom".into()),
            ..TestCtx::defaults()
        };
        assert_eq!(render(&ctx), "[!!] [2/3] load 1.00s (boom)");
    }

    #[test]
    fn pending_state_emits_blank_marker() {
        let ctx = TestCtx {
            phase_name: "verify".into(),
            phase_seq: Some((3, 3)),
            state: LifecycleState::Pending,
            ..TestCtx::defaults()
        };
        assert_eq!(render(&ctx), "[  ] [3/3] verify (not run)");
    }

    #[test]
    fn running_state_emits_double_dot_marker() {
        let ctx = TestCtx {
            phase_name: "run".into(),
            phase_seq: Some((1, 1)),
            elapsed_secs: 0.5,
            state: LifecycleState::Running,
            ..TestCtx::defaults()
        };
        assert_eq!(render(&ctx), "[..] [1/1] run 0.50s (still running)");
    }

    #[test]
    fn explanation_describes_each_field() {
        let ctx = TestCtx::defaults();
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = PhaseSummary.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(n > 0);
        assert!(s.contains("phase-name"));
        assert!(s.contains("idx/total"));
        assert!(s.contains("elapsed"));
    }

    #[test]
    fn other_lods_are_zero_bytes() {
        let ctx = TestCtx {
            phase_name: "x".into(),
            ..TestCtx::defaults()
        };
        for lod in [Lod::Compact, Lod::Expanded] {
            let mut s = String::new();
            let mut buf = StringBuf::new(&mut s);
            let n = PhaseSummary.render(
                &ctx,
                lod,
                ContentMode::Value,
                &ReadoutOptions::new(),
                &mut buf,
            );
            assert_eq!(n, 0, "{lod:?} should render zero bytes");
        }
    }

    #[test]
    fn colorized_emits_ansi_for_status_and_name() {
        // When use_color is on, the bracket marker, phase
        // name, and duration MUST carry ANSI codes per
        // docs/guide/color_style.md. This is a regression
        // guard — pre-Push the post-run summary went to the
        // surface as plain text even on a TTY.
        struct TestCtxColor;
        impl ReadoutContext for TestCtxColor {
            fn subject_name(&self) -> &str {
                "setup"
            }
            fn subject_seq(&self) -> Option<(usize, usize)> {
                Some((1, 2))
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
                1
            }
            fn elapsed_secs(&self) -> f64 {
                0.5
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
                true
            }
            fn event(&self) -> crate::lifecycle::EventType {
                crate::lifecycle::EventType::PhaseEnd
            }
            fn subject_state(&self) -> LifecycleState {
                LifecycleState::Completed
            }
        }
        let ctx = TestCtxColor;
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseSummary.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        // Green wrapper around the [ok] marker.
        assert!(
            s.contains("\x1b[32m[ok]\x1b[0m"),
            "expected green [ok] marker, got: {s:?}"
        );
        // Bold + blue around the phase name.
        assert!(
            s.contains("\x1b[1m\x1b[34msetup\x1b[0m"),
            "expected bold-blue phase name, got: {s:?}"
        );
        // Dim duration tail.
        assert!(
            s.contains("\x1b[2m0.50s\x1b[0m"),
            "expected dim duration, got: {s:?}"
        );
    }

    #[test]
    fn show_labels_inserts_iter_tuple_between_name_and_dur() {
        // Push 9e: failed-phase inset uses this option to
        // surface the iter-tuple coord-path inline so the
        // failure block in `failures:` is self-contained.
        let ctx = TestCtx {
            phase_name: "ann_query".into(),
            phase_labels: "profile=alpha, k=10".into(),
            elapsed_secs: 0.0,
            state: LifecycleState::Failed("connection lost".into()),
            ..TestCtx::defaults()
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let mut opts = ReadoutOptions::new();
        opts.set("show_labels", OptionValue::Bool(true));
        PhaseSummary.render(&ctx, Lod::Labeled, ContentMode::Value, &opts, &mut buf);
        assert_eq!(s, "[!!] ann_query (profile=alpha, k=10) (connection lost)",);
    }

    #[test]
    fn default_omits_labels_even_when_present() {
        // Without `show_labels=true`, labels are dropped —
        // the post-run summary's primary tree carries them
        // in scope-header rows above the phase row, so
        // duplicating them would be noisy.
        let ctx = TestCtx {
            phase_name: "ann_query".into(),
            phase_labels: "profile=alpha".into(),
            state: LifecycleState::Completed,
            ..TestCtx::defaults()
        };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseSummary.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert_eq!(s, "[ok] ann_query");
    }
}

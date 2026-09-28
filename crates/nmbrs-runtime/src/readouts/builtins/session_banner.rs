// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `session_banner` — the opening session row, default-bound
//! to `on_session_start` (and re-used by the post-run
//! summary's first line, `session: <name> (<workload>)`).
//!
//! Reads
//! [`ReadoutContext::session_scenario_name`] and
//! [`ReadoutContext::session_workload_file`]. When the
//! workload file is empty (inline-CLI / shell-pipeline runs
//! without a yaml on disk), the parenthesised tail is
//! omitted.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct SessionBanner;

impl Readout for SessionBanner {
    fn name(&self) -> &'static str {
        "session_banner"
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

fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let name = ctx.session_scenario_name();
    if name.is_empty() {
        return 0;
    }
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut tmp = String::with_capacity(name.len() + 8);
    let _ = write!(&mut tmp, "{bold}{name}{reset}");
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let name = ctx.session_scenario_name();
    let workload = ctx.session_workload_file();
    if name.is_empty() && workload.is_empty() {
        return 0;
    }
    // Per docs/guide/color_style.md: `session:` is HEADER
    // (bold white), name is bold (identity), file path is
    // MUTED (dim).
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut tmp = String::with_capacity(128);
    if workload.is_empty() {
        let _ = write!(&mut tmp, "{bold}session:{reset} {bold}{name}{reset}");
    } else {
        let _ = write!(
            &mut tmp,
            "{bold}session:{reset} {bold}{name}{reset} {dim}({workload}){reset}",
        );
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let name = ctx.session_scenario_name();
    let workload = ctx.session_workload_file();
    if name.is_empty() && workload.is_empty() {
        return 0;
    }
    let color = ctx.use_color();
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut tmp = String::with_capacity(192);
    let _ = write!(
        &mut tmp,
        "{bold}session{reset}\n  {dim}scenario:{reset} {n}\n  {dim}workload:{reset} {w}",
        n = if name.is_empty() { "<unnamed>" } else { name },
        w = if workload.is_empty() {
            "<inline>"
        } else {
            workload
        },
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "session: <scenario-name> (<workload-file>)";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        scenario: String,
        workload: String,
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
            crate::lifecycle::EventType::SessionStart
        }
        fn session_scenario_name(&self) -> &str {
            &self.scenario
        }
        fn session_workload_file(&self) -> &str {
            &self.workload
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        SessionBanner.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_renders_name_and_workload() {
        let ctx = TestCtx {
            scenario: "smoke".into(),
            workload: "workloads/smoke.yaml".into(),
        };
        assert_eq!(
            render(&ctx, Lod::Labeled),
            "session: smoke (workloads/smoke.yaml)",
        );
    }

    #[test]
    fn labeled_drops_workload_tail_when_inline() {
        let ctx = TestCtx {
            scenario: "ad_hoc".into(),
            workload: String::new(),
        };
        assert_eq!(render(&ctx, Lod::Labeled), "session: ad_hoc");
    }

    #[test]
    fn compact_emits_just_name() {
        let ctx = TestCtx {
            scenario: "smoke".into(),
            workload: "x.yaml".into(),
        };
        assert_eq!(render(&ctx, Lod::Compact), "smoke");
    }

    #[test]
    fn expanded_breaks_onto_lines() {
        let ctx = TestCtx {
            scenario: "smoke".into(),
            workload: "x.yaml".into(),
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("scenario: smoke"));
        assert!(out.contains("workload: x.yaml"));
        assert!(out.lines().count() >= 3);
    }

    #[test]
    fn empty_inputs_emit_zero() {
        let ctx = TestCtx::default();
        assert_eq!(render(&ctx, Lod::Labeled), "");
    }
}

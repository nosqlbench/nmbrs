// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `scope_header` — the `· {scope_name}` row that appears
//! above each iteration of a `for_each` /
//! `for_combinations` / `do_while` scope group.
//!
//! Two emit sites today, both bypassing the engine:
//!
//! 1. `nmbrs-tui::observer`'s post-run summary tree walk
//!    (the `· for_each profile=label_00` rows that nest
//!    above each phase row).
//! 2. `nmbrs-tui::log_only_observer`'s `phase_starting`
//!    scope walker (the live mid-run scope-ancestor
//!    headers that fire when the run enters a fresh
//!    iteration).
//!
//! Push 8c lands the readout; the observers route through
//! it in the same push so the two emit sites share one
//! formatter.
//!
//! The readout's renderer reads only the scope name
//! (carried via `phase_name` on the row's
//! `ReadoutContext`) — depth-indent chrome stays the
//! surface's job per SRD-63 §10's layout vs. content
//! split.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct ScopeHeader;

impl Readout for ScopeHeader {
    fn name(&self) -> &'static str {
        "scope_header"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Iteration]
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
            (Lod::Compact, ContentMode::Value) => render_value(ctx, out, false),
            (Lod::Labeled, ContentMode::Value) => render_value(ctx, out, false),
            (Lod::Expanded, ContentMode::Value) => render_value(ctx, out, true),
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

fn render_value(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf, expanded: bool) -> usize {
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let italic = if color { "\x1b[3m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let labels = ctx.subject_labels();

    let mut tmp = String::with_capacity(64);
    if expanded && !labels.is_empty() {
        // Expanded: show both name and the iteration tuple
        // labels on a second indented line. Used by the
        // active-phase panel's expanded row when the user
        // wants the full breakdown.
        let _ = write!(&mut tmp, "{cyan}·{reset} {italic}{name}{reset}\n  {labels}");
    } else {
        // Compact / Labeled: single-row form. The TUI
        // observer's existing scope row matches this byte
        // for byte (cyan bullet, italic name).
        let _ = write!(&mut tmp, "{cyan}·{reset} {italic}{name}{reset}");
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "· {scope-name} — header for the surrounding for_each / do_while scope iteration";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        name: String,
        labels: String,
        use_color: bool,
    }
    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            &self.name
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            None
        }
        fn subject_labels(&self) -> &str {
            &self.labels
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
            self.use_color
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::EachStart
        }
    }

    fn render(ctx: &TestCtx, lod: Lod, mode: ContentMode) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        ScopeHeader.render(ctx, lod, mode, &ReadoutOptions::new(), &mut buf);
        s
    }

    #[test]
    fn labeled_no_color() {
        let ctx = TestCtx {
            name: "for_each profile=label_00".into(),
            ..Default::default()
        };
        assert_eq!(
            render(&ctx, Lod::Labeled, ContentMode::Value),
            "· for_each profile=label_00",
        );
    }

    #[test]
    fn compact_matches_labeled_for_simple_form() {
        // SRD-63 §3.3 monotonicity invariant — Compact's
        // info is a strict subset of Labeled's. For
        // scope_header the two are identical at the value
        // level (no extra fields to drop).
        let ctx = TestCtx {
            name: "for_each k=10".into(),
            ..Default::default()
        };
        assert_eq!(
            render(&ctx, Lod::Compact, ContentMode::Value),
            render(&ctx, Lod::Labeled, ContentMode::Value),
        );
    }

    #[test]
    fn expanded_with_labels_shows_iteration_tuple() {
        let ctx = TestCtx {
            name: "for_combinations".into(),
            labels: "(profile=alpha), (k=10, limit=100)".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded, ContentMode::Value);
        assert!(out.contains("for_combinations"));
        assert!(out.contains("(profile=alpha), (k=10, limit=100)"));
        // Expanded splits onto two lines.
        assert!(
            out.lines().count() >= 2,
            "expanded should be multi-line: {out}"
        );
    }

    #[test]
    fn explanation_describes_the_row() {
        let ctx = TestCtx {
            name: "for_each".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled, ContentMode::Explanation);
        assert!(
            out.contains("scope-name"),
            "expected 'scope-name' descriptor: {out}"
        );
    }

    #[test]
    fn ansi_emitted_when_color_enabled() {
        let ctx = TestCtx {
            name: "for_each k".into(),
            use_color: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled, ContentMode::Value);
        // Cyan bullet + italic name + reset bytes.
        assert!(out.contains("\x1b[36m"), "missing cyan: {out:?}");
        assert!(out.contains("\x1b[3m"), "missing italic: {out:?}");
        assert!(out.contains("\x1b[0m"), "missing reset: {out:?}");
    }
}

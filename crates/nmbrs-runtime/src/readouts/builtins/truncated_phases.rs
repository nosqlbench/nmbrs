// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `truncated_phases` — the `(… and N more phases not
//! listed)` rollup that follows the post-run summary's
//! per-phase rows when the tail was truncated past a failure.
//!
//! When a scenario fails early in a long phase list, the
//! post-run summary trims the pending tail to a small
//! window so the operator's terminal isn't drowned in
//! "[  ] not run" rows. This readout renders the message
//! summarising what was dropped, plus a pointer at
//! `dryrun=phase` for the full plan.
//!
//! Reads
//! [`ReadoutContext::session_phases_truncated`]. Renders
//! zero bytes when the count is 0 — callers fire it
//! unconditionally and rely on the readout's own
//! "is there anything to say?" check.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct TruncatedPhases;

impl Readout for TruncatedPhases {
    fn name(&self) -> &'static str {
        "truncated_phases"
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
        let count = ctx.session_phases_truncated();
        if count == 0 {
            return 0;
        }
        let color = ctx.use_color();
        match (lod, mode) {
            (Lod::Compact, ContentMode::Value) => render_compact(count, color, out),
            (Lod::Labeled, ContentMode::Value) => render_labeled(count, color, out),
            (Lod::Expanded, ContentMode::Value) => render_expanded(count, color, out),
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

fn render_compact(count: usize, color: bool, out: &mut dyn ReadoutBuf) -> usize {
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut tmp = String::with_capacity(48);
    let _ = write!(&mut tmp, "{dim}(… {count} more){reset}");
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled(count: usize, color: bool, out: &mut dyn ReadoutBuf) -> usize {
    // Two-line form: rollup + tip. Both rendered as MUTED
    // (dim) per docs/guide/color_style.md — this is
    // informational tail, not primary signal.
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let suffix = if count == 1 { "" } else { "s" };
    let mut tmp = String::with_capacity(128);
    let _ = write!(
        &mut tmp,
        "{dim}(… and {count} more phase{suffix} not listed){reset}\n\
{dim}tip: run with dryrun=phase to see the full plan{reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(count: usize, color: bool, out: &mut dyn ReadoutBuf) -> usize {
    // Expanded adds one extra contextualising line above
    // the rollup explaining *why* the tail was truncated.
    // Same data; just spelled out for the operator who's
    // reading the post-run summary cold.
    let dim = if color { "\x1b[2m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let suffix = if count == 1 { "" } else { "s" };
    let mut tmp = String::with_capacity(192);
    let _ = write!(
        &mut tmp,
        "{dim}post-failure tail truncated to keep the summary readable{reset}\n\
{dim}(… and {count} more phase{suffix} not listed){reset}\n\
{dim}tip: run with dryrun=phase to see the full plan{reset}",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "(… and <count> more phases not listed) — \
             tail of the phase list trimmed past the last \
             failure; tip points at dryrun=phase for the \
             full plan";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    struct TestCtx {
        truncated: usize,
    }
    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            "session"
        }
        fn session_phases_truncated(&self) -> usize {
            self.truncated
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::SessionEnd
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        TruncatedPhases.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn zero_count_renders_no_bytes() {
        let ctx = TestCtx { truncated: 0 };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = TruncatedPhases.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert_eq!(n, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn labeled_matches_pre_engine_format() {
        // Byte-equivalent to the prior observer.rs:596
        // direct eprintln pair (the two lines `(... and N
        // more phase[s] not listed)` + `tip: …`).
        let ctx = TestCtx { truncated: 7 };
        assert_eq!(
            render(&ctx, Lod::Labeled),
            "(… and 7 more phases not listed)\n\
tip: run with dryrun=phase to see the full plan",
        );
    }

    #[test]
    fn singular_one_phase_drops_plural_suffix() {
        let ctx = TestCtx { truncated: 1 };
        assert!(render(&ctx, Lod::Labeled).contains("1 more phase not listed"));
    }

    #[test]
    fn compact_packs_into_one_line() {
        let ctx = TestCtx { truncated: 12 };
        assert_eq!(render(&ctx, Lod::Compact), "(… 12 more)");
    }

    #[test]
    fn expanded_adds_context_line() {
        let ctx = TestCtx { truncated: 3 };
        let s = render(&ctx, Lod::Expanded);
        assert!(s.contains("post-failure tail truncated"));
        assert!(s.contains("3 more phases"));
        assert!(s.contains("dryrun=phase"));
        // Multi-line.
        assert_eq!(s.lines().count(), 3);
    }

    #[test]
    fn explanation_describes_the_rollup() {
        let ctx = TestCtx { truncated: 5 };
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        let n = TruncatedPhases.render(
            &ctx,
            Lod::Labeled,
            ContentMode::Explanation,
            &ReadoutOptions::new(),
            &mut buf,
        );
        assert!(n > 0);
        assert!(s.contains("<count>"));
        assert!(s.contains("dryrun=phase"));
    }
}

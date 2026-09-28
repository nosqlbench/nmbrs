// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `scope_open` — opening row for non-iteration scopes
//! (`do_while`, `do_until`, plain group blocks). Bound by
//! default to `on_scope_start`.
//!
//! Distinct from `scope_header`, which opens iterations
//! inside `for_each` / `for_combinations`. The non-iteration
//! variants don't carry a tuple label, so this readout is
//! thinner — just the scope name and an opening glyph.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct ScopeOpen;

impl Readout for ScopeOpen {
    fn name(&self) -> &'static str {
        "scope_open"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Scope]
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
    if name.is_empty() {
        return 0;
    }

    let mut tmp = String::with_capacity(48);
    if expanded {
        let _ = write!(
            &mut tmp,
            "{cyan}┌─{reset} {italic}{name}{reset}\n  scope opens"
        );
    } else {
        let _ = write!(&mut tmp, "{cyan}┌─{reset} {italic}{name}{reset}");
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "┌─ {scope-name} — opening marker for a do_while / do_until / group scope";
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
            self.use_color
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::ScopeStart
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        ScopeOpen.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_renders_open_glyph_and_name() {
        let ctx = TestCtx {
            name: "do_while".into(),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Labeled), "┌─ do_while");
    }

    #[test]
    fn compact_matches_labeled() {
        let ctx = TestCtx {
            name: "do_until".into(),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Compact), render(&ctx, Lod::Labeled),);
    }

    #[test]
    fn expanded_breaks_onto_lines() {
        let ctx = TestCtx {
            name: "do_while".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("do_while"));
        assert!(out.lines().count() >= 2);
    }

    #[test]
    fn ansi_when_color_enabled() {
        let ctx = TestCtx {
            name: "x".into(),
            use_color: true,
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(out.contains("\x1b[36m"));
    }
}

// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `scope_close` — closing row for non-iteration scopes.
//! Mirror of [`scope_open`](super::scope_open::ScopeOpen);
//! bound by default to `on_scope_end`.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct ScopeClose;

impl Readout for ScopeClose {
    fn name(&self) -> &'static str {
        "scope_close"
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
            (Lod::Compact, ContentMode::Value) => render_compact(ctx, out),
            (Lod::Labeled, ContentMode::Value) => render_labeled(ctx, out),
            (Lod::Expanded, ContentMode::Value) => render_expanded(ctx, out),
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let mut tmp = String::with_capacity(16);
    let _ = write!(&mut tmp, "{cyan}└{reset}");
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let italic = if color { "\x1b[3m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let mut tmp = String::with_capacity(48);
    if name.is_empty() {
        let _ = write!(&mut tmp, "{cyan}└─{reset} {italic}end{reset}");
    } else {
        let _ = write!(&mut tmp, "{cyan}└─{reset} {italic}end {name}{reset}");
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let italic = if color { "\x1b[3m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let elapsed = ctx.elapsed_secs();
    let mut tmp = String::with_capacity(80);
    if name.is_empty() {
        let _ = write!(
            &mut tmp,
            "{cyan}└─{reset} {italic}end{reset}\n  elapsed: {elapsed:.2}s"
        );
    } else {
        let _ = write!(
            &mut tmp,
            "{cyan}└─{reset} {italic}end {name}{reset}\n  elapsed: {elapsed:.2}s"
        );
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "└─ end {scope-name} — closing marker for a do_while / do_until / group scope";
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
        elapsed: f64,
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
            self.elapsed
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
            crate::lifecycle::EventType::ScopeEnd
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        ScopeClose.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_renders_close_glyph_and_name() {
        let ctx = TestCtx {
            name: "do_while".into(),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Labeled), "└─ end do_while");
    }

    #[test]
    fn compact_thinner_than_labeled() {
        let ctx = TestCtx {
            name: "do_while".into(),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Compact), "└");
    }

    #[test]
    fn expanded_includes_elapsed() {
        let ctx = TestCtx {
            name: "do_until".into(),
            elapsed: 12.345,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("do_until"));
        assert!(out.contains("elapsed: 12.35s"));
    }
}

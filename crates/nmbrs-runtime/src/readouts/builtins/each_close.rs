// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `each_close` — companion to `scope_header` for the
//! `on_each_end` slot. Where `scope_header` opens an
//! iteration with `· {scope_name}`, `each_close` closes it
//! with the matching trailing marker.
//!
//! Compact / Labeled use a thin glyph (`└─ end`); Expanded
//! also surfaces the iteration tuple so post-run scrollback
//! can identify which iteration finished without scanning
//! upward to the opener.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct EachClose;

impl Readout for EachClose {
    fn name(&self) -> &'static str {
        "each_close"
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
    let mut tmp = String::with_capacity(32);
    let _ = write!(&mut tmp, "{cyan}└─{reset} {italic}end{reset}");
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let cyan = if color { "\x1b[36m" } else { "" };
    let italic = if color { "\x1b[3m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let labels = ctx.subject_labels();
    let mut tmp = String::with_capacity(64);
    if labels.is_empty() {
        let _ = write!(&mut tmp, "{cyan}└─{reset} {italic}end{reset}");
    } else {
        let _ = write!(&mut tmp, "{cyan}└─{reset} {italic}end{reset} {labels}");
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "└─ end — closing marker for the surrounding for_each / for_combinations iteration";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        labels: String,
        use_color: bool,
    }
    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            ""
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
            crate::lifecycle::EventType::EachEnd
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        EachClose.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_no_color() {
        let ctx = TestCtx::default();
        assert_eq!(render(&ctx, Lod::Labeled), "└─ end");
    }

    #[test]
    fn compact_thinner_than_labeled() {
        let ctx = TestCtx::default();
        assert_eq!(render(&ctx, Lod::Compact), "└");
    }

    #[test]
    fn expanded_includes_iter_labels() {
        let ctx = TestCtx {
            labels: "(profile=alpha, k=10)".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("end"));
        assert!(out.contains("(profile=alpha, k=10)"));
    }

    #[test]
    fn ansi_emitted_when_color_enabled() {
        let ctx = TestCtx {
            use_color: true,
            ..Default::default()
        };
        let out = render(&ctx, Lod::Labeled);
        assert!(out.contains("\x1b[36m"));
        assert!(out.contains("\x1b[3m"));
    }
}

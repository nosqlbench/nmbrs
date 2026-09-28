// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `phase_starting` — opt-in pre-phase header. Push 2
//! removed the unconditional `▶ phase 'X' starting` row
//! from the live output; this readout brings it back as a
//! workload-bound option, default-targetting `on_phase_start`.
//!
//! Compact / Labeled / Expanded show progressively more
//! context (name only → name + seq → name + seq + iter
//! tuple labels). Explanation describes the row's purpose.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct PhaseStarting;

impl Readout for PhaseStarting {
    fn name(&self) -> &'static str {
        "phase_starting"
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
            (_, ContentMode::Explanation) => render_explanation(out),
        }
    }
}

fn render_compact(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    let color = ctx.use_color();
    let green = if color { "\x1b[32m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let depth_indent = ctx.depth_indent();
    let mut tmp = String::with_capacity(64);
    let _ = write!(&mut tmp, "{depth_indent}{green}▶{reset} {name}");
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_labeled(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    // Same prefix shape as `phase_outcome` so the START / DONE
    // pair line up vertically:
    //   {indent}▶ [idx/total] [name] (coords) starting
    //   {indent}✓ [idx/total] [name] (coords) 100%
    let color = ctx.use_color();
    let green = if color { "\x1b[32m" } else { "" };
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let labels = ctx.subject_labels();
    let depth_indent = ctx.depth_indent();
    let seq_part: String = match ctx.subject_seq() {
        Some((s, t)) => format!("{dim}[{s}/{t}]{reset} "),
        None => String::new(),
    };
    let seq_visible: usize = match ctx.subject_seq() {
        Some((s, t)) => format!("[{s}/{t}] ").chars().count(),
        None => 0,
    };
    // SRD-? — phase_starting is the entry into the
    // ACTIVE phase. Use the full-stack lens
    // (`summarize_changed_only: false`) so the operator
    // sees the complete coord context as the phase begins;
    // changed values relative to the prior completed
    // phase remain highlighted. This call only READS the
    // tracker; the corresponding `phase_outcome` at end
    // advances it.
    let head_consumed: usize = depth_indent.chars().count()
        + 2  // ▶ + space
        + seq_visible
        + 2  // [ and ]
        + name.chars().count();
    let coords_part = super::phase_outcome::format_coords_block(
        labels,
        color,
        head_consumed,
        &format!("{depth_indent}  "),
        /* summarize_changed_only */ false,
    );
    let mut tmp = String::with_capacity(160);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{green}▶{reset} {seq_part}{bold}{blue}[{name}]{reset}{coords_part} starting",
    );
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_expanded(ctx: &dyn ReadoutContext, out: &mut dyn ReadoutBuf) -> usize {
    // Same prefix as Labeled; iter-tuple coords on their own
    // line below (matching phase_outcome's Expanded shape).
    let color = ctx.use_color();
    let green = if color { "\x1b[32m" } else { "" };
    let bold = if color { "\x1b[1m" } else { "" };
    let dim = if color { "\x1b[2m" } else { "" };
    let blue = if color { "\x1b[34m" } else { "" };
    let reset = if color { "\x1b[0m" } else { "" };
    let name = ctx.subject_name();
    let labels = ctx.subject_labels();
    let depth_indent = ctx.depth_indent();
    let seq_part: String = match ctx.subject_seq() {
        Some((s, t)) => format!("{dim}[{s}/{t}]{reset} "),
        None => String::new(),
    };
    let mut tmp = String::with_capacity(192);
    let _ = write!(
        &mut tmp,
        "{depth_indent}{green}▶{reset} {seq_part}{bold}{blue}[{name}]{reset} starting",
    );
    if !labels.is_empty() {
        // SRD-? — full coord stack on its own line for the
        // expanded form, with change-highlight against the
        // prior completed phase. Read-only against the
        // tracker (phase_outcome is the advancement event).
        let coords_continuation_indent = format!("{depth_indent}  ");
        let coords_head_consumed = depth_indent.chars().count() + 2;
        let coords_payload = super::phase_outcome::format_coords_block(
            labels,
            color,
            coords_head_consumed,
            &coords_continuation_indent,
            /* summarize_changed_only */ false,
        );
        // The helper returns a leading-space payload;
        // strip it because we already have the continuation
        // indent providing the visual offset.
        let payload = coords_payload.strip_prefix(' ').unwrap_or(&coords_payload);
        if !payload.is_empty() {
            let _ = write!(&mut tmp, "\n{depth_indent}  {payload}");
        }
    }
    let len = tmp.len();
    let _ = out.write_str(&tmp);
    len
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "▶ {phase-name} ({idx}/{total}) starting — opt-in pre-phase header";
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
        seq: Option<(usize, usize)>,
        labels: String,
        use_color: bool,
    }
    impl ReadoutContext for TestCtx {
        fn subject_name(&self) -> &str {
            &self.name
        }
        fn subject_seq(&self) -> Option<(usize, usize)> {
            self.seq
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
            crate::lifecycle::EventType::PhaseStart
        }
    }

    fn render(ctx: &TestCtx, lod: Lod) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        PhaseStarting.render(
            ctx,
            lod,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn labeled_with_seq() {
        let ctx = TestCtx {
            name: "run".into(),
            seq: Some((3, 8)),
            ..Default::default()
        };
        // Prefix shape matches phase_outcome so the start/done
        // pair line up: ▶ [seq] [name] [(coords)] starting.
        assert_eq!(render(&ctx, Lod::Labeled), "▶ [3/8] [run] starting");
    }

    #[test]
    fn labeled_without_seq() {
        let ctx = TestCtx {
            name: "setup".into(),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Labeled), "▶ [setup] starting");
    }

    #[test]
    fn compact_drops_seq_and_starting_word() {
        let ctx = TestCtx {
            name: "run".into(),
            seq: Some((3, 8)),
            ..Default::default()
        };
        assert_eq!(render(&ctx, Lod::Compact), "▶ run");
    }

    #[test]
    fn expanded_includes_iter_labels() {
        let ctx = TestCtx {
            name: "run".into(),
            seq: Some((3, 8)),
            labels: "(profile=alpha, k=10)".into(),
            ..Default::default()
        };
        let out = render(&ctx, Lod::Expanded);
        assert!(out.contains("[3/8]"), "got: {out:?}");
        assert!(out.contains("[run]"), "got: {out:?}");
        assert!(out.contains("(profile=alpha, k=10)"), "got: {out:?}");
        assert!(out.lines().count() >= 2);
    }
}

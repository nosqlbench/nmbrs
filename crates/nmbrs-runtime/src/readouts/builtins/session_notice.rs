// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! `session_notice` — the SRD-106 sticky-session announcement,
//! seeded onto `on_session_start` by the runner when the
//! `stick_session` rung re-attached the run to `sessions/latest`.
//!
//! Reads [`ReadoutContext::stick_reattached_session`]; renders
//! nothing when it is empty (the readout may be bound in a
//! workload's `readouts:` block, and stays quiet on runs where
//! stick did not engage). The line renders in the palette's
//! ACCENT — bold bright magenta — on color surfaces, and
//! undecorated on monochrome sinks. It names the copy-pasteable
//! override (`--session new`) so the escape hatch travels with
//! the announcement.

use std::fmt::Write as _;

use crate::lifecycle::SubjectKind;
use crate::readouts::buf::ReadoutBuf;
use crate::readouts::context::ReadoutContext;
use crate::readouts::readout::{ContentMode, Lod, Readout, ReadoutOptions};

pub struct SessionNotice;

impl Readout for SessionNotice {
    fn name(&self) -> &'static str {
        "session_notice"
    }
    fn accepts(&self) -> &'static [SubjectKind] {
        &[SubjectKind::Session]
    }

    fn render(
        &self,
        ctx: &dyn ReadoutContext,
        _lod: Lod,
        mode: ContentMode,
        _opts: &ReadoutOptions,
        out: &mut dyn ReadoutBuf,
    ) -> usize {
        if mode == ContentMode::Explanation {
            return render_explanation(out);
        }
        let id = ctx.stick_reattached_session();
        if id.is_empty() {
            return 0;
        }
        let color = ctx.use_color();
        let accent = if color { "\x1b[1;95m" } else { "" };
        let reset = if color { "\x1b[0m" } else { "" };
        let mut tmp = String::with_capacity(id.len() + 96);
        let _ = write!(
            &mut tmp,
            "{accent}\u{25cf} sticky session: re-attached to {id} \
             (stick_session: true) — pass --session new to start fresh{reset}",
        );
        let len = tmp.len();
        let _ = out.write_str(&tmp);
        len
    }
}

fn render_explanation(out: &mut dyn ReadoutBuf) -> usize {
    let s = "sticky session: re-attached to <session-id> \
             (stick_session: true) — pass --session new to start fresh";
    let _ = out.write_str(s);
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readouts::buf::StringBuf;

    #[derive(Default)]
    struct TestCtx {
        reattached: String,
        color: bool,
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
            self.color
        }
        fn event(&self) -> crate::lifecycle::EventType {
            crate::lifecycle::EventType::SessionStart
        }
        fn stick_reattached_session(&self) -> &str {
            &self.reattached
        }
    }

    fn render(ctx: &TestCtx) -> String {
        let mut s = String::new();
        let mut buf = StringBuf::new(&mut s);
        SessionNotice.render(
            ctx,
            Lod::Labeled,
            ContentMode::Value,
            &ReadoutOptions::new(),
            &mut buf,
        );
        s
    }

    #[test]
    fn names_the_session_and_the_override() {
        let ctx = TestCtx {
            reattached: "vsuite_20260805_1200".into(),
            color: false,
        };
        let out = render(&ctx);
        assert!(out.contains("re-attached to vsuite_20260805_1200"));
        assert!(out.contains("--session new"));
        assert!(out.contains("stick_session: true"));
    }

    #[test]
    fn accent_wraps_the_whole_line_when_colored() {
        let ctx = TestCtx {
            reattached: "s1".into(),
            color: true,
        };
        let out = render(&ctx);
        assert!(
            out.starts_with("\x1b[1;95m"),
            "ACCENT opens the line: {out:?}"
        );
        assert!(out.ends_with("\x1b[0m"), "reset closes the line: {out:?}");
    }

    #[test]
    fn silent_when_stick_did_not_engage() {
        let ctx = TestCtx::default();
        assert_eq!(render(&ctx), "");
    }
}
